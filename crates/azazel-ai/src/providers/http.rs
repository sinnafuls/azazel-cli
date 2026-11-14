//! A provider that speaks one of the HTTP wire APIs.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use reqwest::header::HeaderMap;
use tokio_util::sync::CancellationToken;

use crate::catalog::{Cost, ModelInfo};
use crate::credentials::{Auth, Credentials, DEFAULT_ACCOUNT, account_key, now_ms};
use crate::error::AiError;
use crate::limits::Window;
use crate::provider::Provider;
use crate::providers::presets::{Discovery, ProviderConfig};
use crate::providers::{cloud_code, discovery};
use crate::sse::{JsonLines, SseParser, SseRecord};
use crate::stall::{StallLimits, Watchdog};
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

/// What a failed response means beyond its status: `None` leaves it to the generic mapping.
pub(crate) type Classify = fn(u16, &HeaderMap, &str) -> Option<AiError>;

fn no_limits(_: &HeaderMap) -> Vec<Window> {
    Vec::new()
}

/// The function set of one wire API.
pub(super) struct Wire {
    build_request:
        fn(&reqwest::Client, &ProviderConfig, &ModelInfo, &Request, &Auth) -> reqwest::RequestBuilder,
    decoder: fn(&ProviderConfig, &ModelInfo) -> Box<dyn Decoder>,
    framing: Framing,
    pub(super) classify_error: Classify,
    /// The usage windows of the account, read from the headers of a response.
    limits: fn(&HeaderMap) -> Vec<Window>,
    pub(super) models_request: fn(&reqwest::Client, &ProviderConfig, &Auth) -> reqwest::RequestBuilder,
    pub(super) parse_models: fn(&ProviderConfig, &serde_json::Value) -> Vec<ModelInfo>,
}

macro_rules! wire_table {
    ($module:ident) => {
        Wire {
            build_request: wire::$module::build_request,
            decoder: wire::$module::decoder,
            framing: Framing::Sse,
            classify_error: |status, _, body| wire::$module::classify_error(status, body),
            limits: no_limits,
            models_request: wire::$module::models_request,
            parse_models: wire::$module::parse_models,
        }
    };
}

pub(super) fn wire_for(api: WireApi) -> Result<Wire, AiError> {
    Ok(match api {
        WireApi::AnthropicMessages => Wire {
            classify_error: wire::anthropic::classify_response,
            limits: wire::anthropic::limits,
            ..wire_table!(anthropic)
        },
        WireApi::OpenAiResponses => Wire {
            limits: wire::openai_responses::limits,
            ..wire_table!(openai_responses)
        },
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
/// response that goes silent is given up by the provider's stall watchdog ([`StallLimits`]); the
/// read timeout is what is left when that is turned off.
pub fn http_client() -> reqwest::Client {
    client_builder().build().unwrap_or_default()
}

/// What every provider's client is built with; a provider with its own TLS settings adds them.
pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    crate::tls::client_builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(300))
        .user_agent(concat!("azazel/", env!("CARGO_PKG_VERSION")))
}

/// What a configured header value holds where the conversation's id goes: OpenCode's gateways
/// refuse a request that names no session.
pub const SESSION_ID: &str = "{session_id}";

pub struct HttpProvider {
    config: ProviderConfig,
    client: reqwest::Client,
    credentials: Arc<Credentials>,
    /// Models the configuration names for this provider; listed whatever the endpoint reports.
    declared: Vec<ModelInfo>,
    /// The Cloud project of each Cloud Code Assist login, looked up once per account.
    projects: tokio::sync::Mutex<HashMap<String, String>>,
    /// How long a response may stay silent, or take.
    stall: StallLimits,
    /// The configured headers that carry [`SESSION_ID`], as written. `config.headers` has them
    /// filled with an id of this provider's own, for requests that belong to no conversation.
    session_headers: Vec<(String, String)>,
}

/// A request to an account that answers 429 with a wait this long is out of allowance, not
/// merely asked too often.
pub(crate) const LONG_WAIT: Duration = Duration::from_secs(60);
/// How long an account that ran out of allowance is left alone when the provider did not say
/// when it resets.
pub(crate) const UNKNOWN_RESET: Duration = Duration::from_secs(15 * 60);

/// Until when (Unix seconds) the account that got `error` is out of allowance; `None` for an
/// error that says nothing about the account's allowance.
pub(crate) fn blocked_until(error: &AiError) -> Option<u64> {
    let now = now_ms() / 1000;
    match error {
        AiError::Quota { resets_at, .. } => Some(
            resets_at
                .filter(|at| *at > now)
                .unwrap_or(now + UNKNOWN_RESET.as_secs()),
        ),
        AiError::RateLimit {
            retry_after: Some(wait),
        } if *wait > LONG_WAIT => Some(now + wait.as_secs()),
        _ => None,
    }
}

/// A refusal for lack of allowance, with what is known about the account added to it: which
/// window is full and when it resets. Anthropic's "out of extra usage" says neither.
fn name_full_window(error: AiError, full: Option<Window>) -> AiError {
    match (error, full) {
        (AiError::Quota { resets_at, message }, Some(window)) if !message.contains(&window.label()) => {
            let now = now_ms() / 1000;
            AiError::Quota {
                resets_at: resets_at.or(window.resets_at.filter(|at| *at > now)),
                message: format!("{message}\n{}", window.full_line(now)),
            }
        }
        (error, _) => error,
    }
}

impl HttpProvider {
    /// A provider with `config`. Its requests go out through `client` unless the configuration
    /// gives it a client of its own (`tls`).
    pub fn new(mut config: ProviderConfig, client: reqwest::Client, credentials: Arc<Credentials>) -> Self {
        let client = config.tls.as_ref().map_or(client, |tls| tls.client().clone());
        let session_headers: Vec<(String, String)> = config
            .headers
            .iter()
            .filter(|(_, value)| value.contains(SESSION_ID))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        if !session_headers.is_empty() {
            let own = format!("{:032x}", rand::random::<u128>());
            for (name, template) in &session_headers {
                config
                    .headers
                    .insert(name.clone(), template.replace(SESSION_ID, &own));
            }
        }
        let stall = config.timeouts.over(StallLimits::default());
        HttpProvider {
            config,
            client,
            credentials,
            declared: Vec::new(),
            projects: tokio::sync::Mutex::default(),
            stall,
            session_headers,
        }
    }

    /// Gives up a response that stays silent for longer than `limits` allow, with
    /// [`AiError::Stalled`]. Without this a response is waited for as long as the connection
    /// lives. The provider's own `timeout_secs`, `first_event_secs` and `stall_secs` replace
    /// the ones of `limits`.
    pub fn with_stall(mut self, limits: StallLimits) -> Self {
        self.stall = self.config.timeouts.over(limits);
        self
    }

    /// `config` with the headers that carry [`SESSION_ID`] naming the conversation `key`.
    fn for_conversation<'a>(
        &self,
        config: Cow<'a, ProviderConfig>,
        key: Option<&str>,
    ) -> Cow<'a, ProviderConfig> {
        let Some(key) = key.filter(|_| !self.session_headers.is_empty()) else {
            return config;
        };
        let mut config = config.into_owned();
        for (name, template) in &self.session_headers {
            config
                .headers
                .insert(name.clone(), template.replace(SESSION_ID, key));
        }
        Cow::Owned(config)
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
        self.send_as(wire, request, cancel, None).await
    }

    /// Sends `request`. When it asks model `model` with stored account `account` (given as
    /// `(account, model)`), what the answer's headers say about that account's usage windows is
    /// kept, and an answer that means the account is out of allowance marks it so: for the
    /// models the window that ran out counts, or for all of them.
    async fn send_as(
        &self,
        wire: &Wire,
        request: reqwest::RequestBuilder,
        cancel: &CancellationToken,
        account: Option<(&str, &str)>,
    ) -> Result<reqwest::Response, AiError> {
        let limits = self.credentials.limits();
        let id = &self.config.id;
        match send_request(request, wire.classify_error, cancel).await {
            Ok(response) => {
                if let Some((account, model)) = account {
                    limits.record(id, account, (wire.limits)(response.headers()));
                    limits.unblock_for(id, account, model);
                }
                Ok(response)
            }
            Err(failed) => {
                if failed.status == Some(401) {
                    self.credentials.invalidate(id);
                }
                let Some((account, model)) = account else {
                    return Err(failed.error);
                };
                limits.record(id, account, (wire.limits)(&failed.headers));
                let full = limits.full_for(id, account, model);
                let error = name_full_window(failed.error, full.clone());
                if let Some(until) = blocked_until(&error) {
                    limits.block_on(id, account, until, full.as_ref());
                }
                Err(error)
            }
        }
    }

    /// Before a request to `model` goes out on stored account `account`: when that account is
    /// known to be out of allowance for the model (blocked, or a usage report shows a window
    /// the provider refuses for) and another stored account is not, the refusal the provider
    /// would give, so that the router moves the request there without asking. With nowhere
    /// else to go, or no router to move it, the provider is asked, and its answer is the last
    /// word.
    fn known_refusal(&self, account: &str, model: &str) -> Option<AiError> {
        if !self.credentials.moved() {
            return None;
        }
        let limits = self.credentials.limits();
        let id = &self.config.id;
        let now = now_ms() / 1000;
        let (until, limit) = match limits.blocked_for(id, account, model) {
            Some(block) => (block.until, block.limit()),
            None => {
                let window = limits.spent(id, account, model)?;
                (
                    window
                        .resets_at
                        .filter(|at| *at > now)
                        .unwrap_or(now + UNKNOWN_RESET.as_secs()),
                    window.label(),
                )
            }
        };
        let elsewhere = self
            .credentials
            .rotation(id)
            .iter()
            .any(|other| other.label != account && limits.open(id, &other.label, model));
        elsewhere.then(|| AiError::Quota {
            resets_at: Some(until),
            message: format!("the {limit} of account {account} is used up"),
        })
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

    /// The credential requests are made with, and the stored account it belongs to. Fails before
    /// anything is sent when the provider's address still waits for a variable the login does
    /// not answer, and finds the Cloud project a Cloud Code Assist login works in.
    async fn authorise(&self) -> Result<(Auth, Option<String>), AiError> {
        let resolved = self.credentials.resolve_account(&self.config).await;
        if let Some(variable) = self.config.unset_variable()
            && !resolved
                .as_ref()
                .is_ok_and(|(auth, _)| login_base_url(auth).is_some())
        {
            return Err(AiError::Auth(format!(
                "provider {} is not set up: set {variable} (its address is {})",
                self.config.id, self.config.base_url
            )));
        }
        let (auth, account) = resolved?;
        if self.config.api != WireApi::GoogleGeminiCli {
            return Ok((auth, account));
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
            let label = account.as_deref().unwrap_or(DEFAULT_ACCOUNT);
            // Held across the lookup: requests that start together wait for one answer.
            let mut projects = self.projects.lock().await;
            let project = match projects.get(label) {
                Some(project) => project.clone(),
                None => {
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
                    if let Err(e) = self.credentials.set_login_field(
                        &account_key(&self.config.id, label),
                        PROJECT_FIELD,
                        &project,
                    ) {
                        tracing::debug!("could not keep the project of {}: {e}", self.config.id);
                    }
                    projects.insert(label.to_string(), project.clone());
                    project
                }
            };
            fields.insert(PROJECT_FIELD.to_string(), project);
        }
        Ok((Auth::OAuth { access, fields }, account))
    }

    /// The configuration requests made with `auth` go out with: the provider's, at the address
    /// the login keeps when it keeps one.
    pub(super) fn config_for(&self, auth: &Auth) -> Cow<'_, ProviderConfig> {
        for_login(Cow::Borrowed(&self.config), auth)
    }

    /// The wire API's own model list.
    pub(super) async fn wire_models(&self, wire: &Wire, auth: &Auth) -> Result<Vec<ModelInfo>, AiError> {
        let config = self.config_for(auth);
        let request = (wire.models_request)(&self.client, &config, auth);
        let body = self.fetch_json(wire, request).await?;
        Ok((wire.parse_models)(&config, &body))
    }

    pub(super) fn credentials(&self) -> &Credentials {
        &self.credentials
    }
}

/// The API address a login keeps (a GitHub Enterprise host, a Snowflake account), when it keeps
/// one.
pub(crate) fn login_base_url(auth: &Auth) -> Option<&str> {
    match auth {
        Auth::OAuth { fields, .. } => fields
            .get(crate::oauth::BASE_URL_FIELD)
            .map(String::as_str)
            .filter(|url| !url.is_empty()),
        _ => None,
    }
}

/// `config` at the address the login of `auth` keeps, when it keeps one.
fn for_login<'a>(config: Cow<'a, ProviderConfig>, auth: &Auth) -> Cow<'a, ProviderConfig> {
    match login_base_url(auth) {
        Some(url) if url != config.base_url => {
            let mut config = config.into_owned();
            config.base_url = url.trim_end_matches('/').to_string();
            Cow::Owned(config)
        }
        _ => config,
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
        thinking_explicit: req.thinking_explicit,
        max_output: req.max_output,
        temperature: req.temperature,
        cache_key: req.cache_key.clone(),
        tier: req.tier,
        hosted: req.hosted.clone(),
        tail: req.tail.clone(),
    }
}

/// A request that got no usable answer.
pub(crate) struct Failed {
    /// `None` when no response arrived.
    pub(crate) status: Option<u16>,
    /// The headers of the failure response; empty when there was none.
    pub(crate) headers: HeaderMap,
    pub(crate) error: AiError,
}

impl Failed {
    fn unsent(error: AiError) -> Box<Self> {
        Box::new(Failed {
            status: None,
            headers: HeaderMap::new(),
            error,
        })
    }
}

/// Sends `request`. A failure status becomes the error `classify` reads from the headers and the
/// body, or the one the status stands for; it is returned with the status and the headers.
pub(crate) async fn send_request(
    request: reqwest::RequestBuilder,
    classify: Classify,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, Box<Failed>> {
    let response = tokio::select! {
        _ = cancel.cancelled() => return Err(Failed::unsent(AiError::Aborted)),
        r = request.send() => r.map_err(|e| Failed::unsent(AiError::Network(describe(&e))))?,
    };
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let headers = response.headers().clone();
    let body = response.text().await.unwrap_or_default();
    let status = status.as_u16();
    let error = http_error(classify, status, &headers, &body);
    Err(Box::new(Failed {
        status: Some(status),
        headers,
        error,
    }))
}

/// The events of a streaming SSE response body, decoded by `decoder`; see [`framed_stream`].
#[cfg(feature = "vertex")]
pub(crate) fn event_stream(
    response: reqwest::Response,
    decoder: Box<dyn Decoder>,
    cancel: CancellationToken,
    started: Instant,
    cost: Option<Cost>,
    watchdog: Watchdog,
) -> EventStream {
    framed_stream(response, Framing::Sse, decoder, cancel, started, cost, watchdog)
}

/// The events of a streaming response body, cut into records by `framing` and decoded by
/// `decoder`. The final message gets the time since `started` and, when the provider did not
/// price it itself and `cost` is known, its price. `watchdog` was started when the request was
/// sent: a body that stays silent for too long ends the stream with [`AiError::Stalled`].
fn framed_stream(
    response: reqwest::Response,
    framing: Framing,
    decoder: Box<dyn Decoder>,
    cancel: CancellationToken,
    started: Instant,
    cost: Option<Cost>,
    watchdog: Watchdog,
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
        watchdog,
    };
    Box::pin(futures::stream::unfold(live, Live::next))
}

#[async_trait]
impl Provider for HttpProvider {
    fn id(&self) -> &str {
        &self.config.id
    }

    fn hosts_web_search(&self) -> bool {
        wire::hosted_api(&self.config).is_some()
    }

    async fn stream(
        &self,
        model: &ModelInfo,
        req: &Request,
        cancel: CancellationToken,
    ) -> Result<EventStream, AiError> {
        // A request with hosted tools goes out in the format the provider serves them in; any
        // other in the format the model is served on.
        let api = match req.hosted.is_empty() {
            true => self.config.api_for(&model.id),
            false => wire::hosted_api(&self.config).ok_or_else(|| {
                AiError::InvalidRequest(format!("provider {} runs no tools of its own", self.config.id))
            })?,
        };
        let config = match api == self.config.api {
            true => Cow::Borrowed(&self.config),
            false => Cow::Owned(ProviderConfig {
                api,
                ..self.config.clone()
            }),
        };
        let wire = wire_for(config.api)?;
        let config = self.for_conversation(config, req.cache_key.as_deref());
        let (auth, account) = self.authorise().await?;
        let config = for_login(config, &auth);
        if let Some(refusal) = account
            .as_deref()
            .and_then(|account| self.known_refusal(account, &model.id))
        {
            return Err(refusal);
        }
        let target = Target {
            provider: &config.id,
            model,
            api: config.api,
        };
        let prepared = prepare(req, &target);

        let started = Instant::now();
        let watchdog = Watchdog::start(self.stall);
        let request = (wire.build_request)(&self.client, &config, model, &prepared, &auth);
        let response = tokio::select! {
            biased;
            sent = self.send_as(&wire, request, &cancel, account.as_deref().map(|account| (account, model.id.as_str()))) => sent?,
            stalled = watchdog.expired() => return Err(stalled),
        };
        let decoder = (wire.decoder)(&config, model);
        Ok(framed_stream(
            response,
            wire.framing,
            decoder,
            cancel,
            started,
            model.cost,
            watchdog,
        ))
    }

    /// What the provider's discovery finds, then the models the configuration names that it did
    /// not report. A provider that does not answer within the first-event limit has stalled.
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AiError> {
        let mut models = match self.config.discovery {
            Discovery::Off => Vec::new(),
            kind => {
                let wire = wire_for(self.config.api)?;
                let watchdog = Watchdog::start(self.stall);
                let listing = async {
                    let (auth, _) = self.authorise().await?;
                    discovery::list(self, &wire, &auth, kind).await
                };
                tokio::select! {
                    biased;
                    listed = listing => listed?,
                    stalled = watchdog.expired() => return Err(stalled),
                }
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
    watchdog: Watchdog,
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
                        // Anything the provider sends is a sign of life, a keep-alive too.
                        self.watchdog.heard();
                        let records = self.framer.push(&bytes);
                        self.feed(records);
                    }
                    Some(Err(e)) => self.end(Some(AiError::Network(describe(&e)))),
                    None => {
                        let last = self.framer.finish();
                        self.feed(last);
                        self.end(None);
                    }
                },
                // After the body: what has arrived is read before the silence is judged.
                stalled = self.watchdog.expired() => self.end(Some(stalled)),
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

    /// The body ended, cleanly or because of `failure`: the connection broke, or it went silent.
    /// A reply the decoder already has in full stands either way.
    fn end(&mut self, failure: Option<AiError>) {
        if self.finished {
            return;
        }
        let mut events = self.decoder.finish();
        if let Some(failure) = failure
            && let Some(StreamEvent::Error { error, .. }) = events.last_mut()
        {
            *error = failure;
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

fn http_error(classify: Classify, status: u16, headers: &HeaderMap, body: &str) -> AiError {
    let retry_after = retry_after(headers);
    match classify(status, headers, body) {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn a_refusal_for_lack_of_allowance_names_the_full_window() {
        let now = now_ms() / 1000;
        let full = Window::new("5h", 1.0, Some(now + 2 * 3600 + 10 * 60 + 30), "allowed");
        let refusal = || AiError::Quota {
            resets_at: None,
            message: "You're out of extra usage. Add more at claude.ai/settings/usage and keep going."
                .to_string(),
        };
        // What the provider said stays first; the window and its reset follow on a second line,
        // and the reset becomes the time the account is left alone until.
        let named = name_full_window(refusal(), Some(full.clone()));
        assert_eq!(
            named,
            AiError::Quota {
                resets_at: full.resets_at,
                message: "You're out of extra usage. Add more at claude.ai/settings/usage and keep going.\n\
                          the 5-hour limit of this account is full; it resets in 2 h 10 min"
                    .to_string(),
            }
        );
        assert_eq!(blocked_until(&named), full.resets_at);
        // Nothing known about the account, or a message that names the window already: unchanged.
        assert_eq!(name_full_window(refusal(), None), refusal());
        let said = AiError::Quota {
            resets_at: Some(now + 60),
            message: "the 5-hour limit of this account is used up".to_string(),
        };
        assert_eq!(name_full_window(said.clone(), Some(full.clone())), said);
        // Other errors are not about allowance.
        assert_eq!(
            name_full_window(AiError::Overloaded, Some(full)),
            AiError::Overloaded
        );
    }

    const MODELS: &str = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 33\r\n\
                          connection: close\r\n\r\n{\"data\":[{\"id\":\"gateway-model\"}]}";

    #[tokio::test]
    async fn a_provider_with_a_client_certificate_reaches_a_server_that_requires_one() {
        use crate::providers::presets::{ProviderSettings, resolve_with};
        use crate::tls::TlsSettings;

        let (server, identity) = crate::tls::local::serve_mutual(MODELS).await;
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = (dir.path().join("client.pem"), dir.path().join("client.key"));
        std::fs::write(&cert, &identity.cert).unwrap();
        std::fs::write(&key, &identity.key).unwrap();
        let provider = |tls: TlsSettings| {
            let settings = ProviderSettings {
                api: Some(WireApi::OpenAiChat),
                base_url: Some(server.url()),
                tls: Some(tls),
                ..ProviderSettings::default()
            };
            let config = resolve_with(&BTreeMap::from([("gateway".to_string(), settings)]), &|_| None)
                .unwrap()
                .into_iter()
                .find(|p| p.id == "gateway")
                .unwrap();
            let credentials = Credentials::with_env(dir.path().join("auth.json"), Arc::new(|_| None));
            HttpProvider::new(config, http_client(), Arc::new(credentials))
        };

        let with = provider(TlsSettings {
            cert: Some(cert.to_string_lossy().into_owned()),
            key: Some(key.to_string_lossy().into_owned()),
            ca: Some(server.certificate.clone()),
        });
        let models = with.list_models().await.unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["gateway-model"]
        );

        // The same server, trusted but without the certificate: the handshake is refused.
        let without = provider(TlsSettings {
            ca: Some(server.certificate.clone()),
            ..TlsSettings::default()
        });
        let error = without.list_models().await.unwrap_err();
        assert!(matches!(error, AiError::Network(_)), "{error:?}");
    }
}
