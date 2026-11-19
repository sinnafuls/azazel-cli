//! Image generation through the ChatGPT (Codex) login: `POST {base}/images/generations`, or
//! `POST {base}/images/edits` when the request has reference images, where `base` is the
//! `openai-codex` provider's (`https://chatgpt.com/backend-api/codex`).
//!
//! OpenAI documents image generation for ChatGPT plans but not these endpoints; the request is
//! the OpenAI Images API's, as the Codex client sends it. So the backend checks what it is told:
//! a reply that says the endpoint or the plan is not there switches it off for that account for
//! the rest of the session, with a message that says so, and no later call of that account makes
//! a request. A refusal that may be passing (a 403 that is not the backend's own, such as a
//! proxy's page, or one that does not name the plan) is reported and switches nothing off.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use azazel_ai::Auth;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::backend::{
    MediaBackend, MediaError, MediaFile, MediaKind, MediaRequest, REQUEST_TIMEOUT, clip, data_url,
    decode_file, json_reply, send, until_cancelled,
};
use crate::openai::size;

const ID: &str = "openai-codex";

/// The most reference images one edit takes, as in the Codex client.
const MAX_INPUTS: usize = 5;

/// What the user is told when the backend has no such endpoint.
const NOT_OFFERED: &str =
    "the ChatGPT backend did not offer image generation to azazel: use `openai:` or `google:` models";

/// What the user is told when the plan has no image generation.
const NOT_ON_PLAN: &str =
    "image generation is not available to this ChatGPT plan (it needs a paid plan; Free has none)";

/// Why the backend is off, by ChatGPT account id. Shared by the backends made for one provider,
/// because the service builds a new one for each call (a login's token is renewed in between);
/// keyed by account, so that signing in to another account gives the backend a new chance.
#[derive(Clone, Default)]
pub struct CodexOff(Arc<Mutex<HashMap<String, String>>>);

impl CodexOff {
    fn get(&self, account: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(account)
            .cloned()
    }

    /// Records `why` for `account` unless an answer got there first; returns what is recorded.
    fn set(&self, account: &str, why: &str) -> String {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(account.to_string())
            .or_insert_with(|| why.to_string())
            .clone()
    }
}

pub struct CodexImages {
    base_url: String,
    auth: Auth,
    headers: Vec<(String, String)>,
    client: reqwest::Client,
    disabled: CodexOff,
}

impl CodexImages {
    /// `base_url` is the `openai-codex` provider's, `auth` its login and `headers` what
    /// `Auth::login_headers` gives for it (`originator`, `chatgpt-account-id`).
    pub fn new(base_url: impl Into<String>, auth: Auth, headers: Vec<(String, String)>) -> Self {
        CodexImages {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth,
            headers,
            client: azazel_ai::tls::client(),
            disabled: CodexOff::default(),
        }
    }

    /// Makes this backend share its off switch with others made for the same provider.
    pub(crate) fn sharing(mut self, disabled: CodexOff) -> Self {
        self.disabled = disabled;
        self
    }

    /// The ChatGPT account the login belongs to: the key of the off switch. A login that names
    /// none counts as one account.
    fn account(&self) -> &str {
        self.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("chatgpt-account-id"))
            .map_or("", |(_, value)| value.as_str())
    }

    fn refuse_when_disabled(&self) -> Result<(), MediaError> {
        match self.disabled.get(self.account()) {
            Some(message) => Err(MediaError::Unsupported(message)),
            None => Ok(()),
        }
    }

    async fn run(&self, req: &MediaRequest) -> Result<Vec<MediaFile>, MediaError> {
        self.refuse_when_disabled()?;
        if req.inputs.len() > MAX_INPUTS {
            return Err(MediaError::Input(format!(
                "an edit takes at most {MAX_INPUTS} input images, not {}",
                req.inputs.len()
            )));
        }
        let mut body = Map::new();
        body.insert("model".to_string(), req.model.as_str().into());
        body.insert("prompt".to_string(), req.prompt.as_str().into());
        body.insert("n".to_string(), req.count.max(1).into());
        if let Some(size) = size(req) {
            body.insert("size".to_string(), size.into());
        }
        if req.transparent() {
            body.insert("background".to_string(), "transparent".into());
            body.insert("output_format".to_string(), "png".into());
        }
        let mut images = Vec::with_capacity(req.inputs.len());
        for path in &req.inputs {
            let url = data_url(path).await?;
            images.push(json!({"image_url": url}));
        }
        let endpoint = if images.is_empty() {
            "images/generations"
        } else {
            body.insert("images".to_string(), Value::Array(images));
            "images/edits"
        };

        let mut request = self
            .client
            .post(format!("{}/{endpoint}", self.base_url))
            .timeout(REQUEST_TIMEOUT)
            .json(&body);
        if let Auth::ApiKey(token) | Auth::OAuth { access: token, .. } = &self.auth {
            request = request.bearer_auth(token);
        }
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        let (status, body) = send(ID, request).await?;
        if let Some(why) = self.switch_off(status, &body) {
            return Err(MediaError::Unsupported(why));
        }
        if status == 403 && !is_json(&body) {
            // Not the backend's own answer, so it says nothing about the endpoint or the plan.
            return Err(MediaError::Api {
                backend: ID.to_string(),
                status,
                message: format!(
                    "the answer is not from the ChatGPT backend's API (a proxy or network filter?); \
                     image generation stays on, so try again: {}",
                    clip(body.trim(), 200)
                ),
            });
        }
        let reply = json_reply(ID, status, &body)?;

        let stated = reply["output_format"]
            .as_str()
            .map(|format| format!("image/{format}"));
        let mut files = Vec::new();
        for item in reply["data"].as_array().map(Vec::as_slice).unwrap_or_default() {
            if let Some(encoded) = item["b64_json"].as_str() {
                files.push(decode_file(ID, encoded, stated.as_deref())?);
            }
        }
        if files.is_empty() {
            return Err(MediaError::Failed(
                "the ChatGPT backend answered without an image".to_string(),
            ));
        }
        Ok(files)
    }

    /// Switches the backend off for this account when `status` and `body` say it is not
    /// offered, and returns the message to give. A 429 is a usage limit, and a 403 that is not
    /// JSON or does not name the plan may pass, so none of them switches anything off.
    fn switch_off(&self, status: u16, body: &str) -> Option<String> {
        let why = match status {
            400 | 403 if names_plan(body) => NOT_ON_PLAN,
            404 | 405 | 501 => NOT_OFFERED,
            _ => return None,
        };
        tracing::warn!(status, body = %clip(body, 200), "the ChatGPT backend refused image generation; switching it off");
        // The first answer wins when two calls race.
        Some(self.disabled.set(self.account(), why))
    }
}

fn is_json(body: &str) -> bool {
    serde_json::from_str::<Value>(body).is_ok_and(|value| value.is_object())
}

/// Whether an error body in JSON says the plan has no image generation: the error's code, type
/// or message has a word of that meaning. Whole words only, so `explanation` and `planet` do
/// not count; a body that is not JSON (a proxy's page) says nothing about the plan.
fn names_plan(body: &str) -> bool {
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    let error = root.get("error").unwrap_or(&Value::Null);
    let fields = [
        &error["code"],
        &error["type"],
        &error["message"],
        root.get("detail").unwrap_or(&Value::Null),
        root.get("message").unwrap_or(&Value::Null),
    ];
    fields.into_iter().filter_map(Value::as_str).any(|text| {
        let words: Vec<String> = text
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        let has = |wanted: &[&str]| words.iter().any(|word| wanted.contains(&word.as_str()));
        has(&[
            "plan",
            "plans",
            "entitled",
            "entitlement",
            "entitlements",
            "subscription",
        ]) || words
            .windows(2)
            .any(|pair| pair[0] == "free" && pair[1] == "tier")
    })
}

#[async_trait]
impl MediaBackend for CodexImages {
    fn id(&self) -> &str {
        ID
    }

    fn kinds(&self) -> &[MediaKind] {
        &[MediaKind::Image]
    }

    async fn generate(
        &self,
        req: &MediaRequest,
        cancel: CancellationToken,
    ) -> Result<Vec<MediaFile>, MediaError> {
        until_cancelled(&cancel, self.run(req)).await
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use base64::Engine;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn png() -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(8, 4, image::Rgba([200, 30, 30, 255]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    fn login() -> (Auth, Vec<(String, String)>) {
        let auth = Auth::OAuth {
            access: "at-1".to_string(),
            fields: Default::default(),
        };
        let headers = vec![
            ("originator".to_string(), "azazel".to_string()),
            ("chatgpt-account-id".to_string(), "acct-9".to_string()),
        ];
        (auth, headers)
    }

    fn backend(server: &MockServer) -> CodexImages {
        let (auth, headers) = login();
        CodexImages::new(format!("{}/backend-api/codex", server.uri()), auth, headers)
    }

    fn request(inputs: Vec<PathBuf>) -> MediaRequest {
        MediaRequest {
            kind: MediaKind::Image,
            model: "gpt-image-2".to_string(),
            prompt: "a red cube".to_string(),
            inputs,
            params: Map::new(),
            count: 1,
        }
    }

    fn image_reply() -> ResponseTemplate {
        let encoded = base64::engine::general_purpose::STANDARD.encode(png());
        ResponseTemplate::new(200).set_body_json(json!({ "data": [{ "b64_json": encoded }] }))
    }

    async fn run(backend: &CodexImages, req: &MediaRequest) -> Result<Vec<MediaFile>, MediaError> {
        backend.generate(req, CancellationToken::new()).await
    }

    #[tokio::test]
    async fn generation_sends_the_login_and_saves_the_image() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/images/generations"))
            .and(header("authorization", "Bearer at-1"))
            .and(header("originator", "azazel"))
            .and(header("chatgpt-account-id", "acct-9"))
            .respond_with(image_reply())
            .expect(1)
            .mount(&server)
            .await;

        let files = run(&backend(&server), &request(Vec::new())).await.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].mime, "image/png");
        assert_eq!(files[0].bytes.to_vec(), png());
        let sent: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(
            (&sent["model"], &sent["prompt"], &sent["n"]),
            (&json!("gpt-image-2"), &json!("a red cube"), &json!(1))
        );
        assert!(sent.get("images").is_none());
    }

    #[tokio::test]
    async fn an_edit_goes_to_the_edits_endpoint_with_the_input_as_a_data_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/images/edits"))
            .respond_with(image_reply())
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.png");
        std::fs::write(&input, png()).unwrap();

        run(&backend(&server), &request(vec![input])).await.unwrap();
        let sent: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        let url = sent["images"][0]["image_url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
    }

    #[tokio::test]
    async fn more_than_five_inputs_are_refused_without_a_request() {
        let server = MockServer::start().await;
        let inputs = vec![Path::new("a.png").to_path_buf(); 6];
        let error = run(&backend(&server), &request(inputs)).await.unwrap_err();
        assert!(matches!(error, MediaError::Input(_)), "{error}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_endpoint_switches_the_backend_off_for_good() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .expect(1)
            .mount(&server)
            .await;
        let backend = backend(&server);

        let first = run(&backend, &request(Vec::new())).await.unwrap_err();
        assert_eq!(first, MediaError::Unsupported(NOT_OFFERED.to_string()));
        let second = run(&backend, &request(Vec::new())).await.unwrap_err();
        assert_eq!(second, first);
        // `.expect(1)` fails when the server drops with a second request.
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_off_switch_is_shared_by_backends_made_for_one_login() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(405))
            .expect(1)
            .mount(&server)
            .await;
        let state = CodexOff::default();
        let first = backend(&server).sharing(state.clone());
        run(&first, &request(Vec::new())).await.unwrap_err();
        let later = backend(&server).sharing(state);
        let error = run(&later, &request(Vec::new())).await.unwrap_err();
        assert_eq!(error, MediaError::Unsupported(NOT_OFFERED.to_string()));
    }

    #[tokio::test]
    async fn a_plan_refusal_says_the_plan_has_no_image_generation() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "error": { "message": "Image generation is not included in your plan." }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let backend = backend(&server);
        let error = run(&backend, &request(Vec::new())).await.unwrap_err();
        assert_eq!(error, MediaError::Unsupported(NOT_ON_PLAN.to_string()));
        assert!(run(&backend, &request(Vec::new())).await.is_err());
    }

    #[tokio::test]
    async fn a_usage_limit_does_not_switch_the_backend_off() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({
                "error": { "message": "You have hit your usage limit." }
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(image_reply())
            .mount(&server)
            .await;
        let backend = backend(&server);

        let limited = run(&backend, &request(Vec::new())).await.unwrap_err();
        assert!(
            matches!(&limited, MediaError::Api { status: 429, message, .. } if message.contains("usage limit")),
            "{limited}"
        );
        assert_eq!(run(&backend, &request(Vec::new())).await.unwrap().len(), 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    /// A backend of the account `id`, over the shared off switch `off`.
    fn account(server: &MockServer, id: &str, off: &CodexOff) -> CodexImages {
        let (auth, _) = login();
        let headers = vec![("chatgpt-account-id".to_string(), id.to_string())];
        CodexImages::new(format!("{}/backend-api/codex", server.uri()), auth, headers).sharing(off.clone())
    }

    #[tokio::test]
    async fn signing_in_to_another_account_gives_the_backend_a_new_chance() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("chatgpt-account-id", "free"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(header("chatgpt-account-id", "paid"))
            .respond_with(image_reply())
            .mount(&server)
            .await;
        let off = CodexOff::default();
        let error = run(&account(&server, "free", &off), &request(Vec::new()))
            .await
            .unwrap_err();
        assert_eq!(error, MediaError::Unsupported(NOT_OFFERED.to_string()));
        // The account that was refused stays off, and the other is asked.
        assert!(
            run(&account(&server, "free", &off), &request(Vec::new()))
                .await
                .is_err()
        );
        assert_eq!(
            run(&account(&server, "paid", &off), &request(Vec::new()))
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_403_page_from_a_proxy_is_reported_and_the_next_call_is_made() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("content-type", "text/html")
                    .set_body_string(
                        "<html><body>Access denied: upgrade your plan of the firewall</body></html>",
                    ),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(image_reply())
            .mount(&server)
            .await;
        let backend = backend(&server);

        let first = run(&backend, &request(Vec::new())).await.unwrap_err();
        assert!(
            matches!(&first, MediaError::Api { status: 403, message, .. } if message.contains("try again")),
            "{first}"
        );
        assert_eq!(run(&backend, &request(Vec::new())).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_json_403_that_does_not_name_the_plan_does_not_switch_the_backend_off() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "error": { "message": "Request blocked, try again later." }
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(image_reply())
            .mount(&server)
            .await;
        let backend = backend(&server);

        let first = run(&backend, &request(Vec::new())).await.unwrap_err();
        assert!(matches!(first, MediaError::Api { status: 403, .. }), "{first}");
        assert_eq!(run(&backend, &request(Vec::new())).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_plan_is_recognised_by_its_error_code_or_by_a_whole_word() {
        for body in [
            json!({ "error": { "code": "plan_not_supported", "message": "Not available." } }),
            json!({ "error": { "message": "Your subscription does not include images." } }),
            json!({ "error": { "message": "Not on the Free tier." } }),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(403).set_body_json(body.clone()))
                .mount(&server)
                .await;
            let error = run(&backend(&server), &request(Vec::new())).await.unwrap_err();
            assert_eq!(error, MediaError::Unsupported(NOT_ON_PLAN.to_string()), "{body}");
        }
    }

    #[tokio::test]
    async fn words_that_only_contain_plan_do_not_say_the_plan_lacks_images() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": { "message": "No explanation given: the planet in the prompt cannot be drawn; upgrade-insecure." }
            })))
            .expect(2)
            .mount(&server)
            .await;
        let backend = backend(&server);
        for _ in 0..2 {
            let error = run(&backend, &request(Vec::new())).await.unwrap_err();
            assert!(matches!(error, MediaError::Api { status: 400, .. }), "{error}");
        }
    }
}
