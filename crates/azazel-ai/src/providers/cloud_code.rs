//! Finding the Google Cloud project a Cloud Code Assist login makes its requests in.
//!
//! A Gemini CLI sign-in does not say which project it belongs to. The service is asked
//! (`loadCodeAssist`); an account it has never seen is first enrolled in the tier the service
//! offers it (`onboardUser`, a long-running operation that is polled until it is done).

use std::time::Duration;

use serde_json::{Value, json};

use crate::error::AiError;

/// How long one request of the lookup may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How often and how many times an enrolment is asked whether it has finished.
pub(super) const POLL_EVERY: Duration = Duration::from_secs(5);
const POLL_ATTEMPTS: u32 = 24;
/// The variables a user names their own project in; paid tiers need one.
pub(super) const PROJECT_ENV: [&str; 2] = ["GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_PROJECT_ID"];
/// The tier an account is enrolled in when the service marks none as its default.
const FALLBACK_TIER: &str = "legacy-tier";
const FREE_TIER: &str = "free-tier";

pub(super) struct Lookup<'a> {
    pub client: &'a reqwest::Client,
    pub base_url: &'a str,
    /// The login's access token.
    pub access: &'a str,
    /// The project the environment names, if any.
    pub configured: Option<String>,
    pub poll_every: Duration,
}

fn metadata(project: Option<&str>) -> Value {
    let mut metadata = json!({
        "ideType": "IDE_UNSPECIFIED",
        "platform": "PLATFORM_UNSPECIFIED",
        "pluginType": "GEMINI",
    });
    if let Some(project) = project {
        metadata["duetProject"] = json!(project);
    }
    metadata
}

/// The project named by `cloudaicompanionProject`, which is an id or an object with one.
fn project_of(answer: &Value) -> Option<String> {
    let found = answer.get("cloudaicompanionProject")?;
    found
        .as_str()
        .or_else(|| found.get("id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

impl Lookup<'_> {
    async fn call(&self, request: reqwest::RequestBuilder) -> Result<Value, AiError> {
        let response = request
            .bearer_auth(self.access)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| AiError::Network(format!("Cloud Code Assist did not answer: {e}")))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let text: String = text.chars().take(500).collect();
            return Err(match status.as_u16() {
                401 | 403 => AiError::Auth(format!("Cloud Code Assist refused the login: {text}")),
                code => AiError::Server {
                    status: code,
                    body: text,
                },
            });
        }
        serde_json::from_str(&text).map_err(|e| {
            AiError::Network(format!(
                "Cloud Code Assist answered with something unreadable: {e}"
            ))
        })
    }

    fn post(&self, method: &str, body: &Value) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}/v1internal:{method}", self.base_url))
            .json(body)
    }

    /// The project of the signed-in account, enrolling the account first when it is new.
    pub async fn project(&self) -> Result<String, AiError> {
        let configured = self.configured.as_deref();
        let mut load = json!({ "metadata": metadata(configured) });
        if let Some(project) = configured {
            load["cloudaicompanionProject"] = json!(project);
        }
        let loaded = self.call(self.post("loadCodeAssist", &load)).await?;
        if let Some(project) = project_of(&loaded) {
            return Ok(project);
        }
        let missing = || {
            AiError::Auth(format!(
                "this Google account has no Cloud Code Assist project: set {} to one of yours",
                PROJECT_ENV[0]
            ))
        };
        // An account that already has a tier but no managed project uses the user's own.
        if loaded.get("currentTier").is_some_and(|tier| !tier.is_null()) {
            return configured.map(str::to_string).ok_or_else(missing);
        }

        let tier = loaded
            .get("allowedTiers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|tier| tier.get("isDefault").and_then(Value::as_bool) == Some(true))
            .and_then(|tier| tier.get("id").and_then(Value::as_str))
            .unwrap_or(FALLBACK_TIER);
        // The free tier runs in a project Google manages; the others in the user's.
        let own = configured.filter(|_| tier != FREE_TIER);
        let mut onboard = json!({ "tierId": tier, "metadata": metadata(own) });
        if let Some(project) = own {
            onboard["cloudaicompanionProject"] = json!(project);
        }
        let mut operation = self.call(self.post("onboardUser", &onboard)).await?;
        for _ in 0..POLL_ATTEMPTS {
            if operation.get("done").and_then(Value::as_bool) == Some(true) {
                break;
            }
            let Some(name) = operation.get("name").and_then(Value::as_str) else {
                break;
            };
            tokio::time::sleep(self.poll_every).await;
            let url = format!("{}/v1internal/{name}", self.base_url);
            operation = self.call(self.client.get(url)).await?;
        }
        operation
            .get("response")
            .and_then(project_of)
            .or_else(|| configured.map(str::to_string))
            .ok_or_else(missing)
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn lookup<'a>(client: &'a reqwest::Client, server: &'a str, configured: Option<&str>) -> Lookup<'a> {
        Lookup {
            client,
            base_url: server,
            access: "at-1",
            configured: configured.map(str::to_string),
            poll_every: Duration::from_millis(1),
        }
    }

    #[tokio::test]
    async fn a_known_account_names_its_project() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .and(header("authorization", "Bearer at-1"))
            .and(body_partial_json(json!({"metadata": {"pluginType": "GEMINI"}})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "currentTier": {"id": "free-tier"},
                "cloudaicompanionProject": "managed-123",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = crate::tls::client();
        let uri = server.uri();
        assert_eq!(
            lookup(&client, &uri, None).project().await.unwrap(),
            "managed-123"
        );
    }

    #[tokio::test]
    async fn a_new_account_is_enrolled_and_the_enrolment_polled() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "allowedTiers": [{"id": "standard-tier"}, {"id": "free-tier", "isDefault": true}],
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1internal:onboardUser"))
            .and(body_partial_json(json!({"tierId": "free-tier"})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"name": "operations/op-7", "done": false})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1internal/operations/op-7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "done": true,
                "response": {"cloudaicompanionProject": {"id": "fresh-9"}},
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = crate::tls::client();
        let uri = server.uri();
        assert_eq!(lookup(&client, &uri, None).project().await.unwrap(), "fresh-9");
    }

    #[tokio::test]
    async fn a_paid_account_uses_the_project_the_environment_names_or_says_what_to_set() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"currentTier": {"id": "standard-tier"}})),
            )
            .mount(&server)
            .await;
        let client = crate::tls::client();
        let uri = server.uri();
        assert_eq!(
            lookup(&client, &uri, Some("mine")).project().await.unwrap(),
            "mine"
        );
        let error = lookup(&client, &uri, None).project().await.unwrap_err();
        assert!(
            matches!(&error, AiError::Auth(why) if why.contains("GOOGLE_CLOUD_PROJECT")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_login_is_an_authentication_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("expired"))
            .mount(&server)
            .await;
        let client = crate::tls::client();
        let uri = server.uri();
        let error = lookup(&client, &uri, None).project().await.unwrap_err();
        assert!(matches!(error, AiError::Auth(_)), "{error:?}");
    }
}
