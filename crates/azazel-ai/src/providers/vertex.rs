//! Google Cloud's model endpoints, reached with the machine's Google credentials: Gemini models
//! through the `google-generative` wire and Claude models through the `anthropic-messages` wire,
//! each at its publisher's address.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::catalog::ModelInfo;
use crate::credentials::Auth;
use crate::error::AiError;
use crate::provider::Provider;
use crate::providers::http::{Classify, event_stream, prepare, send_request};
use crate::providers::presets::{ProviderConfig, VERTEX};
use crate::stall::{StallLimits, Watchdog};
use crate::stream::EventStream;
use crate::types::{Request, WireApi};
use crate::wire;
use crate::wire::normalize::Target;

const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// The location used when none is configured; it routes to wherever there is capacity.
pub const DEFAULT_LOCATION: &str = "global";
/// Claude requests name the API version in the body here, not in a header.
const ANTHROPIC_VERSION: &str = "2026-10-21";
const PROJECT_ENV: [&str; 2] = ["GOOGLE_CLOUD_PROJECT", "GCLOUD_PROJECT"];
/// Longest the search for credentials may take. Its last resort is the metadata server of a
/// Google Cloud machine, which answers at once there and not at all anywhere else.
const LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Where tokens and the default project come from.
enum Identity {
    /// Application default credentials, found on first use.
    Google(OnceCell<Arc<dyn gcp_auth::TokenProvider>>),
    #[cfg(test)]
    Fixed(&'static str),
}

pub struct VertexProvider {
    project: Option<String>,
    location: String,
    /// Replaces `https://<location>-aiplatform.googleapis.com` (tests).
    host: Option<String>,
    client: reqwest::Client,
    identity: Identity,
    /// How long a response may stay silent.
    stall: StallLimits,
}

fn credentials_error(why: impl std::fmt::Display) -> AiError {
    AiError::Auth(format!(
        "no usable Google Cloud credentials ({why}): run `gcloud auth application-default login` or set \
         GOOGLE_APPLICATION_CREDENTIALS"
    ))
}

/// Whether `model` is one of Anthropic's, which live under their own publisher.
fn is_claude(model: &str) -> bool {
    model.to_ascii_lowercase().contains("claude")
}

impl VertexProvider {
    /// `project` falls back to `GOOGLE_CLOUD_PROJECT` and then to the project of the credentials;
    /// `location` to [`DEFAULT_LOCATION`].
    pub fn new(project: Option<String>, location: Option<String>, client: reqwest::Client) -> Self {
        let chosen = |value: Option<String>| value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        VertexProvider {
            project: chosen(project),
            location: chosen(location).unwrap_or_else(|| DEFAULT_LOCATION.to_string()),
            host: None,
            client,
            identity: Identity::Google(OnceCell::new()),
            stall: StallLimits::default(),
        }
    }

    /// Gives up a response that stays silent for longer than `limits` allow, with
    /// [`AiError::Stalled`].
    pub fn with_stall(mut self, limits: StallLimits) -> Self {
        self.stall = limits;
        self
    }

    async fn google(
        cell: &OnceCell<Arc<dyn gcp_auth::TokenProvider>>,
    ) -> Result<&Arc<dyn gcp_auth::TokenProvider>, AiError> {
        cell.get_or_try_init(|| async {
            match tokio::time::timeout(LOOKUP_TIMEOUT, gcp_auth::provider()).await {
                Ok(found) => found.map_err(credentials_error),
                Err(_) => Err(credentials_error("none were found in time")),
            }
        })
        .await
    }

    async fn token(&self) -> Result<String, AiError> {
        match &self.identity {
            Identity::Google(cell) => {
                let token = Self::google(cell)
                    .await?
                    .token(&[SCOPE])
                    .await
                    .map_err(credentials_error)?;
                Ok(token.as_str().to_string())
            }
            #[cfg(test)]
            Identity::Fixed(token) => Ok(token.to_string()),
        }
    }

    async fn project(&self) -> Result<String, AiError> {
        if let Some(project) = &self.project {
            return Ok(project.clone());
        }
        let from_env = PROJECT_ENV
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|value| !value.trim().is_empty()));
        if let Some(project) = from_env {
            return Ok(project);
        }
        let unknown = || {
            AiError::InvalidRequest(
                "no Google Cloud project: set `[providers.vertex] project` or GOOGLE_CLOUD_PROJECT"
                    .to_string(),
            )
        };
        match &self.identity {
            Identity::Google(cell) => match Self::google(cell).await?.project_id().await {
                Ok(project) => Ok(project.to_string()),
                Err(_) => Err(unknown()),
            },
            #[cfg(test)]
            Identity::Fixed(_) => Err(unknown()),
        }
    }

    /// The address of one publisher's models in `project`.
    fn publisher_url(&self, project: &str, publisher: &str) -> String {
        let host = match &self.host {
            Some(host) => host.clone(),
            None if self.location == DEFAULT_LOCATION => "https://aiplatform.googleapis.com".to_string(),
            None => format!("https://{}-aiplatform.googleapis.com", self.location),
        };
        format!(
            "{host}/v1/projects/{project}/locations/{}/publishers/{publisher}",
            self.location
        )
    }
}

#[async_trait]
impl Provider for VertexProvider {
    fn id(&self) -> &str {
        VERTEX
    }

    async fn stream(
        &self,
        model: &ModelInfo,
        req: &Request,
        cancel: CancellationToken,
    ) -> Result<EventStream, AiError> {
        let token = self.token().await?;
        let project = self.project().await?;
        let claude = is_claude(&model.id);
        let (api, publisher) = if claude {
            (WireApi::AnthropicMessages, "anthropic")
        } else {
            (WireApi::GoogleGenerative, "google")
        };
        let config = ProviderConfig {
            requires_key: true,
            ..ProviderConfig::new(VERTEX, api, self.publisher_url(&project, publisher))
        };
        let target = Target {
            provider: VERTEX,
            model,
            api,
        };
        let prepared = prepare(req, &target);
        let started = Instant::now();
        let watchdog = Watchdog::start(self.stall);

        let (request, decoder, classify): (_, _, Classify) = if claude {
            // The address names the model, so the body must not.
            let mut body = wire::anthropic::build_body(&config, model, &prepared);
            if let Value::Object(members) = &mut body {
                members.remove("model");
                members.insert("anthropic_version".to_string(), json!(ANTHROPIC_VERSION));
            }
            let url = format!("{}/models/{}:streamRawPredict", config.base_url, model.id);
            (
                self.client.post(url).bearer_auth(&token).json(&body),
                wire::anthropic::decoder(&config, model),
                |status, _, body| wire::anthropic::classify_error(status, body),
            )
        } else {
            let auth = Auth::OAuth {
                access: token,
                fields: BTreeMap::new(),
            };
            (
                wire::google::build_request(&self.client, &config, model, &prepared, &auth),
                wire::google::decoder(&config, model),
                |status, _, body| wire::google::classify_error(status, body),
            )
        };
        let response = tokio::select! {
            biased;
            sent = send_request(request, classify, &cancel) => sent.map_err(|failed| failed.error)?,
            stalled = watchdog.expired() => return Err(stalled),
        };
        Ok(event_stream(
            response, decoder, cancel, started, model.cost, watchdog,
        ))
    }

    /// The models come from the catalogue: the listing endpoint is not part of the stable API.
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AiError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::stream::StreamEvent;
    use crate::types::{Message, SystemPart};

    const GEMINI: &str = concat!(
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"from gemini\"}]},",
        "\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":2}}\n\n",
    );
    const CLAUDE: &str = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"usage\":",
        "{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\"from claude\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
        "\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );

    fn provider(server: &MockServer, location: &str) -> VertexProvider {
        VertexProvider {
            project: Some("proj-1".into()),
            location: location.into(),
            host: Some(server.uri()),
            client: reqwest::Client::new(),
            identity: Identity::Fixed("gcp-token"),
            stall: StallLimits::default(),
        }
    }

    fn request() -> Request {
        Request {
            system: vec![SystemPart {
                text: "be brief".into(),
                name: String::new(),
                cache_boundary: false,
            }],
            messages: vec![Message::user("hi")],
            ..Request::default()
        }
    }

    async fn answer(provider: &VertexProvider, model: &str) -> String {
        let model = ModelInfo::unknown(VERTEX, model);
        let stream = provider
            .stream(&model, &request(), CancellationToken::new())
            .await
            .unwrap();
        let events: Vec<StreamEvent> = stream.collect().await;
        let Some(StreamEvent::Done { message }) = events.last() else {
            panic!("{events:?}");
        };
        assert_eq!(message.meta.as_ref().unwrap().provider, VERTEX);
        message.text()
    }

    #[tokio::test]
    async fn gemini_and_claude_models_go_to_their_publishers_with_the_google_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1/projects/proj-1/locations/europe-west1/publishers/google/models/gemini-pro:streamGenerateContent",
            ))
            .and(query_param("alt", "sse"))
            .and(header("authorization", "Bearer gcp-token"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(GEMINI, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(
                "/v1/projects/proj-1/locations/europe-west1/publishers/anthropic/models/claude-sonnet-4-5:streamRawPredict",
            ))
            .and(header("authorization", "Bearer gcp-token"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(CLAUDE, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        let provider = provider(&server, "europe-west1");

        assert_eq!(answer(&provider, "gemini-pro").await, "from gemini");
        assert_eq!(answer(&provider, "claude-sonnet-4-5").await, "from claude");

        let requests = server.received_requests().await.unwrap();
        let claude: Value = serde_json::from_slice(&requests[1].body).unwrap();
        // The address names the model; the version travels in the body.
        assert_eq!(claude.get("model"), None);
        assert_eq!(claude["anthropic_version"], ANTHROPIC_VERSION);
        assert_eq!(claude["stream"], true);
        assert_eq!(claude["messages"][0]["role"], "user");
        assert!(!requests[1].headers.contains_key("x-api-key"));
        assert!(!requests[1].headers.contains_key("anthropic-version"));
    }

    #[tokio::test]
    async fn a_publisher_that_does_not_begin_its_answer_in_time_has_stalled() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(60))
                    .set_body_raw(GEMINI, "text/event-stream"),
            )
            .mount(&server)
            .await;
        let limits = StallLimits {
            first_event: Some(std::time::Duration::from_millis(300)),
            stall: None,
        };
        let provider = provider(&server, "europe-west1").with_stall(limits);
        let model = ModelInfo::unknown(VERTEX, "gemini-pro");
        let began = Instant::now();
        let Err(error) = provider
            .stream(&model, &request(), CancellationToken::new())
            .await
        else {
            panic!("a response began");
        };
        assert_eq!(error, AiError::Stalled { secs: 1 });
        assert!(began.elapsed() < std::time::Duration::from_secs(30));
    }

    #[tokio::test]
    async fn a_refused_request_is_reported_the_way_its_wire_reports_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({
                "error": { "code": 429, "status": "RESOURCE_EXHAUSTED", "message": "quota" }
            })))
            .mount(&server)
            .await;
        let provider = provider(&server, "us-central1");
        let model = ModelInfo::unknown(VERTEX, "gemini-pro");
        let failed = provider
            .stream(&model, &request(), CancellationToken::new())
            .await;
        assert!(
            matches!(failed, Err(AiError::RateLimit { .. })),
            "{:?}",
            failed.err()
        );
    }

    #[test]
    fn addresses_follow_the_location() {
        let at = |location: Option<&str>| {
            VertexProvider::new(
                Some(" p ".into()),
                location.map(str::to_string),
                reqwest::Client::new(),
            )
            .publisher_url("p", "google")
        };
        assert_eq!(
            at(None),
            "https://aiplatform.googleapis.com/v1/projects/p/locations/global/publishers/google"
        );
        assert_eq!(
            at(Some("us-east5")),
            "https://us-east5-aiplatform.googleapis.com/v1/projects/p/locations/us-east5/publishers/google"
        );
        assert_eq!(at(Some("  ")), at(None));
        assert!(is_claude("Claude-Opus-4-5@20251101") && !is_claude("gemini-2.5-pro"));
    }

    #[tokio::test]
    async fn a_missing_project_is_named_before_anything_is_sent() {
        let server = MockServer::start().await;
        let mut provider = provider(&server, "global");
        provider.project = None;
        let model = ModelInfo::unknown(VERTEX, "gemini-pro");
        let outcome = provider
            .stream(&model, &request(), CancellationToken::new())
            .await;
        // An ambient project variable would stand in for the setting.
        if PROJECT_ENV.iter().all(|name| std::env::var(name).is_err()) {
            assert!(
                matches!(&outcome, Err(AiError::InvalidRequest(why)) if why.starts_with("no Google Cloud project")),
                "{:?}",
                outcome.err()
            );
        }
    }
}
