//! A provider that speaks one of the HTTP wire APIs.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::catalog::{Cost, ModelInfo};
use crate::credentials::{Auth, Credentials};
use crate::error::AiError;
use crate::provider::Provider;
use crate::providers::presets::{Discovery, ProviderConfig};
use crate::providers::{cloud_code, discovery};
use crate::sse::{JsonLines, SseParser, SseRecord};
use crate::stream::{EventStream, StreamEvent};
use crate::types::{Request, WireApi};
use crate::wire::gemini_cli::PROJECT_FIELD;
use crate::wire::normalize::{Target, normalize};
use crate::wire::{self, Decoder};

/// How a response body is cut into the records a decoder reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    /// Server-sent events.
    Sse,
    /// One JSON object per line.
    JsonLines,
}

/// The function set of one wire API.
pub(super) struct Wire {
    build_request:
        fn(&reqwest::Client, &ProviderConfig, &ModelInfo, &Request, &Auth) -> reqwest::RequestBuilder,
    decoder: fn(&ProviderConfig, &ModelInfo) -> Box<dyn Decoder>,
    framing: Framing,
    pub(super) classify_error: fn(u16, &str) -> Option<AiError>,
    pub(super) models_request: fn(&reqwest::Client, &ProviderConfig, &Auth) -> reqwest::RequestBuilder,
    pub(super) parse_models: fn(&ProviderConfig, &serde_json::Value) -> Vec<ModelInfo>,
}

macro_rules! wire_table {
    ($module:ident) => {
        Wire {
            build_request: wire::$module::build_request,
            decoder: wire::$module::decoder,
            framing: Framing::Sse,
            classify_error: wire::$module::classify_error,
            models_request: wire::$module::models_request,
            parse_models: wire::$module::parse_models,
        }
    };
}

pub(super) fn wire_for(api: WireApi) -> Result<Wire, AiError> {
    Ok(match api {
        WireApi::AnthropicMessages => wire_table!(anthropic),
        WireApi::OpenAiResponses => wire_table!(openai_responses),
        // OpenRouter is the chat format with its own request members, chosen inside the module.
        WireApi::OpenAiChat | WireApi::OpenRouter => wire_table!(openai_chat),
        WireApi::GoogleGenerative => wire_table!(google),
        WireApi::OllamaChat => Wire {
            framing: Framing::JsonLines,
            ..wire_table!(ollama)
        },
        WireApi::AzureOpenAiResponses => Wire {
            build_request: wire::azure::build_responses_request,
            models_request: wire::azure::models_request,
            ..wire_table!(openai_responses)
        },
        WireApi::AzureOpenAiChat => Wire {
            build_request: wire::azure::build_chat_request,
            models_request: wire::azure::models_request,
            ..wire_table!(openai_chat)
        },
        WireApi::GoogleGeminiCli => Wire {
            build_request: wire::gemini_cli::build_request,
            models_request: wire::gemini_cli::models_request,
            parse_models: wire::gemini_cli::parse_models,
            ..wire_table!(google)
        },
        WireApi::BedrockConverse => {
            return Err(AiError::InvalidRequest(
                "bedrock-converse is served by the `bedrock` provider, not by an HTTP endpoint".to_string(),
            ));
        }
    })
}

/// The HTTP client shared by all providers. No overall timeout: responses stream for minutes. A
/// stalled connection is caught by the read timeout.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(300))
        .user_agent(concat!("azazel/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_default()
}

pub struct HttpProvider {
    config: ProviderConfig,
    client: reqwest::Client,
    credentials: Arc<Credentials>,
    /// Models the configuration names for this provider; listed whatever the endpoint reports.
    declared: Vec<ModelInfo>,
    /// The Cloud project of a Cloud Code Assist login, looked up once.
    project: OnceCell<String>,
}

impl HttpProvider {
    pub fn new(config: ProviderConfig, client: reqwest::Client, credentials: Arc<Credentials>) -> Self {
        HttpProvider {
            config,
            client,
            credentials,
            declared: Vec::new(),
            project: OnceCell::new(),
        }
    }

    /// Adds the models the configuration names for this provider to what it lists, for the ones
    /// its endpoint does not report.
    pub fn with_declared(mut self, models: Vec<ModelInfo>) -> Self {
        self.declared = models;
        self
    }

    pub fn config(&self) -> &ProviderConfig {
        &self.config
    }

    pub(super) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub(super) async fn send(
        &self,
        wire: &Wire,
        request: reqwest::RequestBuilder,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response, AiError> {
        match send_request(request, wire.classify_error, cancel).await {
            Ok(response) => Ok(response),
            Err((status, error)) => {
                if status == Some(401) {
                    self.credentials.invalidate(&self.config.id);
                }
                Err(error)
            }
        }
    }

    /// Sends `request` and reads the answer as JSON.
    pub(super) async fn fetch_json(
        &self,
        wire: &Wire,
        request: reqwest::RequestBuilder,
    ) -> Result<serde_json::Value, AiError> {
        let response = self.send(wire, request, &CancellationToken::new()).await?;
        response.json().await.map_err(|e| AiError::Network(describe(&e)))
    }

    /// The credential requests are made with. Fails before anything is sent when the provider's
    /// address still waits for a variable, and finds the Cloud project a Cloud Code Assist login
    /// works in.
    async fn authorise(&self) -> Result<Auth, AiError> {
        if let Some(variable) = self.config.unset_variable() {
            return Err(AiError::Auth(format!(
                "provider {} is not set up: set {variable} (its address is {})",
                self.config.id, self.config.base_url
            )));
        }
        let auth = self.credentials.resolve(&self.config).await?;
        if self.config.api != WireApi::GoogleGeminiCli {
            return Ok(auth);
        }
        let (access, mut fields) = match auth {
            Auth::OAuth { access, fields } => (access, fields),
            // A token handed over as a key is a login without its account fields.
            Auth::ApiKey(access) => (access, Default::default()),
            Auth::None => {
                return Err(AiError::Auth(format!(
                    "no credentials for {id}: run `azazel login {id}`",
                    id = self.config.id
                )));
            }
        };
        if !fields.contains_key(PROJECT_FIELD) {
            let project = self
                .project
                .get_or_try_init(|| async {
                    let configured = cloud_code::PROJECT_ENV
                        .iter()
                        .find_map(|name| std::env::var(name).ok().filter(|value| !value.trim().is_empty()));
                    let lookup = cloud_code::Lookup {
                        client: &self.client,
                        base_url: &self.config.base_url,
                        access: &access,
                        configured,
                        poll_every: cloud_code::POLL_EVERY,
                    };
                    let project = lookup.project().await?;
                    // Kept with the login, so the next process does not ask again.
                    if let Err(e) = self
                        .credentials
                        .set_login_field(&self.config.id, PROJECT_FIELD, &project)
                    {
                        tracing::debug!("could not keep the project of {}: {e}", self.config.id);
                    }
                    Ok::<_, AiError>(project)
                })
                .await?;
            fields.insert(PROJECT_FIELD.to_string(), project.clone());
        }
        Ok(Auth::OAuth { access, fields })
    }

    /// The wire API's own model list.
    pub(super) async fn wire_models(&self, wire: &Wire, auth: &Auth) -> Result<Vec<ModelInfo>, AiError> {
        let request = (wire.models_request)(&self.client, &self.config, auth);
        let body = self.fetch_json(wire, request).await?;
        Ok((wire.parse_models)(&self.config, &body))
    }
}

/// `req` as the model behind `target` accepts it: messages normalised, tools in name order so the
/// cached prefix does not depend on registration order, and thinking at a level the model takes.
pub(crate) fn prepare(req: &Request, target: &Target<'_>) -> Request {
    let mut tools = req.tools.clone();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    Request {
        system: req.system.clone(),
        messages: normalize(&req.messages, target),
        tools,
        thinking: target.model.thinking_level(req.thinking),
        max_output: req.max_output,
        temperature: req.temperature,
        cache_key: req.cache_key.clone(),
    }
}

/// Sends `request`. A failure status becomes the error `classify` reads from the body, or the one
/// the status stands for; it is returned with the status.
pub(crate) async fn send_request(
    request: reqwest::RequestBuilder,
    classify: fn(u16, &str) -> Option<AiError>,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, (Option<u16>, AiError)> {
    let response = tokio::select! {
        _ = cancel.cancelled() => return Err((None, AiError::Aborted)),
        r = request.send() => r.map_err(|e| (None, AiError::Network(describe(&e))))?,
    };
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    let status = status.as_u16();
    Err((Some(status), http_error(classify, status, retry_after, &body)))
}

/// The events of a streaming SSE response body, decoded by `decoder`; see [`framed_stream`].
#[cfg(feature = "vertex")]
pub(crate) fn event_stream(
    response: reqwest::Response,
    decoder: Box<dyn Decoder>,
    cancel: CancellationToken,
    started: Instant,
    cost: Option<Cost>,
) -> EventStream {
    framed_stream(response, Framing::Sse, decoder, cancel, started, cost)
}

/// The events of a streaming response body, cut into records by `framing` and decoded by
/// `decoder`. The final message gets the time since `started` and, when the provider did not
/// price it itself and `cost` is known, its price.
fn framed_stream(
    response: reqwest::Response,
    framing: Framing,
    decoder: Box<dyn Decoder>,
    cancel: CancellationToken,
    started: Instant,
    cost: Option<Cost>,
) -> EventStream {
    let live = Live {
        body: Box::pin(response.bytes_stream()),
        framer: match framing {
            Framing::Sse => Framer::Sse(SseParser::new()),
            Framing::JsonLines => Framer::Lines(JsonLines::new()),
        },
        decoder,
        queue: VecDeque::new(),
        finished: false,
        cancel,
        started,
        cost,
    };
    Box::pin(futures::stream::unfold(live, Live::next))
}

#[async_trait]
impl Provider for HttpProvider {
    fn id(&self) -> &str {
        &self.config.id
    }

    async fn stream(
        &self,
        model: &ModelInfo,
        req: &Request,
        cancel: CancellationToken,
    ) -> Result<EventStream, AiError> {
        let wire = wire_for(self.config.api)?;
        let auth = self.authorise().await?;
        let target = Target {
            provider: &self.config.id,
            model,
            api: self.config.api,
        };
        let prepared = prepare(req, &target);

        let started = Instant::now();
        let request = (wire.build_request)(&self.client, &self.config, model, &prepared, &auth);
        let response = self.send(&wire, request, &cancel).await?;
        let decoder = (wire.decoder)(&self.config, model);
        Ok(framed_stream(
            response,
            wire.framing,
            decoder,
            cancel,
            started,
            model.cost,
        ))
    }

    /// What the provider's discovery finds, then the models the configuration names that it did
    /// not report.
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AiError> {
        let mut models = match self.config.discovery {
            Discovery::Off => Vec::new(),
            kind => {
                let wire = wire_for(self.config.api)?;
                let auth = self.authorise().await?;
                discovery::list(self, &wire, &auth, kind).await?
            }
        };
        for declared in &self.declared {
            if models.iter().all(|listed| listed.id != declared.id) {
                models.push(declared.clone());
            }
        }
        Ok(models)
    }
}

/// Cuts a response body into records.
enum Framer {
    Sse(SseParser),
    Lines(JsonLines),
}

impl Framer {
    fn push(&mut self, chunk: &[u8]) -> Vec<SseRecord> {
        match self {
            Framer::Sse(parser) => parser.push(chunk),
            Framer::Lines(lines) => lines.push(chunk),
        }
    }

    fn finish(&mut self) -> Option<SseRecord> {
        match self {
            Framer::Sse(parser) => parser.finish(),
            Framer::Lines(lines) => lines.finish(),
        }
    }
}

/// One in-flight response body.
struct Live {
    body: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    framer: Framer,
    decoder: Box<dyn Decoder>,
    queue: VecDeque<StreamEvent>,
    finished: bool,
    cancel: CancellationToken,
    started: Instant,
    cost: Option<Cost>,
}

impl Live {
    async fn next(mut self) -> Option<(StreamEvent, Live)> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Some((event, self));
            }
            if self.finished {
                return None;
            }
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {
                    self.finished = true;
                    self.queue.push_back(StreamEvent::Error { error: AiError::Aborted, partial: None });
                }
                chunk = self.body.next() => match chunk {
                    Some(Ok(bytes)) => {
                        let records = self.framer.push(&bytes);
                        self.feed(records);
                    }
                    Some(Err(e)) => self.end(Some(describe(&e))),
                    None => {
                        let last = self.framer.finish();
                        self.feed(last);
                        self.end(None);
                    }
                },
            }
        }
    }

    fn feed(&mut self, records: impl IntoIterator<Item = SseRecord>) {
        for record in records {
            if self.finished {
                return;
            }
            let events = self.decoder.feed(record.event.as_deref(), &record.data);
            self.accept(events);
        }
    }

    /// The body ended, cleanly or with `transport_error`.
    fn end(&mut self, transport_error: Option<String>) {
        if self.finished {
            return;
        }
        let mut events = self.decoder.finish();
        if let Some(message) = transport_error
            && let Some(StreamEvent::Error { error, .. }) = events.last_mut()
        {
            *error = AiError::Network(message);
        }
        self.accept(events);
        self.finished = true;
    }

    fn accept(&mut self, events: Vec<StreamEvent>) {
        for mut event in events {
            if let StreamEvent::Done { message } = &mut event
                && let Some(meta) = &mut message.meta
            {
                meta.duration_ms = self.started.elapsed().as_millis() as u64;
                // A price the provider reported is what was charged; otherwise the tokens are
                // priced from the catalogue.
                if meta.usage.cost_usd.is_none()
                    && let Some(cost) = &self.cost
                {
                    meta.usage.price(cost);
                }
            }
            let terminal = event.is_terminal();
            self.queue.push_back(event);
            if terminal {
                self.finished = true;
                return;
            }
        }
    }
}

/// `reqwest` hides the cause behind "error sending request"; include the source chain.
fn describe(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<f64>().ok();
    if let Some(ms) = value("retry-after-ms") {
        return Some(Duration::from_secs_f64((ms / 1000.0).max(0.0)));
    }
    value("retry-after").map(|secs| Duration::from_secs_f64(secs.max(0.0)))
}

const MAX_ERROR_BODY: usize = 2000;

fn http_error(
    classify: fn(u16, &str) -> Option<AiError>,
    status: u16,
    retry_after: Option<Duration>,
    body: &str,
) -> AiError {
    match classify(status, body) {
        Some(AiError::RateLimit { retry_after: None }) => return AiError::RateLimit { retry_after },
        Some(error) => return error,
        None => {}
    }
    let body: String = body.chars().take(MAX_ERROR_BODY).collect();
    match status {
        401 | 403 => AiError::Auth(body),
        408 => AiError::Network(format!("HTTP 408: {body}")),
        429 => AiError::RateLimit { retry_after },
        503 | 529 => AiError::Overloaded,
        500.. => AiError::Server { status, body },
        _ => AiError::InvalidRequest(format!("HTTP {status}: {body}")),
    }
}
