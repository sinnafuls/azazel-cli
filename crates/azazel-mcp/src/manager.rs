//! The servers of a session: their connections, what they offer, and the calls into them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_agent::{Access, Services, Tool, ToolCtx, ToolOutput, UpdateSink};
use azazel_ai::{Block, Credentials};
use azazel_core::{AskKind, Level, Question, TrustStore, Ui};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotificationParam, ClientRequest,
    GetPromptRequestParams, JsonObject, ProgressToken, Prompt, ReadResourceRequestParams, ServerResult,
};
use rmcp::service::{Peer, PeerRequestOptions, RoleClient, ServiceError};
use serde_json::Value;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::catalog::{self, Cached, CachedTool};
use crate::client::{self, ConnectError};
use crate::config::{self, Approval, Lifecycle, Loaded, ServerConfig, Transport};
use crate::handler::{self, Handler};
use crate::{convert, oauth, search};

/// Seconds between attempts to reach a server that failed or went away.
const RETRY_DELAYS: [u64; 5] = [1, 2, 4, 8, 16];
/// A connection that lasted this long counts as healthy: its loss starts the delays over.
const STABLE_AFTER: Duration = Duration::from_secs(30);
/// How long a call waits for a server that is (re)connecting.
const READY_WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(100);
/// Characters of a server's instructions that go into the system prompt.
pub const INSTRUCTIONS_LIMIT: usize = 2000;
/// How long the user has to finish logging in in the browser.
const LOGIN_WAIT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// `enabled: false` in its entry.
    Disabled,
    /// A project's server that is new or has changed; it starts once the user approves it.
    WaitingApproval,
    /// A project's server the user decided not to run.
    Rejected,
    /// Not started yet.
    Idle,
    Connecting,
    Ready,
    /// The server answered 401; the user has to log in.
    NeedsAuth,
    Failed(String),
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            State::Disabled => f.write_str("disabled"),
            State::WaitingApproval => f.write_str("waiting for approval"),
            State::Rejected => f.write_str("rejected"),
            State::Idle => f.write_str("not started"),
            State::Connecting => f.write_str("connecting"),
            State::Ready => f.write_str("ready"),
            State::NeedsAuth => f.write_str("needs login"),
            State::Failed(why) => write!(f, "failed: {why}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub name: String,
    pub state: State,
    pub tools: usize,
    pub prompts: usize,
    /// The command line or URL.
    pub target: String,
    /// The file the entry is in.
    pub source: PathBuf,
}

impl ServerStatus {
    /// The state in words. A server that starts on demand and has not been started says how many
    /// tools are known from the last time it ran.
    pub fn state_text(&self) -> String {
        match (&self.state, self.tools) {
            (State::Idle, 0) => self.state.to_string(),
            (State::Idle, 1) => "not started (cached 1 tool)".to_string(),
            (State::Idle, tools) => format!("not started (cached {tools} tools)"),
            (state, _) => state.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// How long the first request of a session waits for servers that are still connecting.
    pub startup_wait: Duration,
    /// With more MCP tools than this, they are found through `tool_search` instead of being listed.
    pub defer_threshold: usize,
    /// A server started on demand (`lifecycle: lazy`) is stopped after this long without a
    /// call; `None` leaves it running.
    pub idle_timeout: Option<Duration>,
    /// Where the lists of what servers offer are kept between sessions. Without it a server
    /// that starts on demand is started once per session, to ask it.
    pub cache_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// A server's tool as agents call it.
pub(crate) struct ProxyTool {
    name: String,
    description: String,
    schema: Value,
    read_only: bool,
    server: String,
    tool: String,
    inner: Weak<Inner>,
}

impl ProxyTool {
    fn new(inner: Weak<Inner>, server: &str, tool: CachedTool) -> Self {
        ProxyTool {
            name: crate::tool_name(server, &tool.name),
            description: tool.description,
            schema: tool.schema,
            read_only: tool.read_only,
            server: server.to_string(),
            tool: tool.name,
            inner,
        }
    }

    /// The tool as it is kept between sessions.
    fn cached(&self) -> CachedTool {
        CachedTool {
            name: self.tool.clone(),
            description: self.description.clone(),
            schema: self.schema.clone(),
            read_only: self.read_only,
        }
    }
}

fn described(tool: &rmcp::model::Tool) -> CachedTool {
    CachedTool {
        name: tool.name.to_string(),
        description: tool.description.as_deref().unwrap_or_default().to_string(),
        schema: Value::Object((*tool.input_schema).clone()),
        read_only: tool
            .annotations
            .as_ref()
            .and_then(|a| a.read_only_hint)
            .unwrap_or(false),
    }
}

#[async_trait]
impl Tool for ProxyTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    fn access(&self, _: &Value) -> Access {
        if self.read_only {
            Access::Read
        } else {
            Access::Exec
        }
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        match self.inner.upgrade() {
            Some(inner) => inner.call_tool(&self.server, &self.tool, input, ctx).await,
            None => ToolOutput::error("MCP servers are shut down"),
        }
    }
}

/// What a server offers: asked of it when it connected, or for one that starts on demand, read
/// from what was kept the last time it ran.
struct Catalog {
    tools: Vec<Arc<ProxyTool>>,
    prompts: Vec<Prompt>,
    instructions: Option<String>,
}

impl Catalog {
    fn of(inner: &Weak<Inner>, cached: Cached) -> Catalog {
        Catalog {
            tools: cached
                .tools
                .into_iter()
                .map(|tool| Arc::new(ProxyTool::new(inner.clone(), &cached.server, tool)))
                .collect(),
            prompts: cached.prompts,
            instructions: cached.instructions,
        }
    }

    fn cached(&self, config: &ServerConfig) -> Cached {
        Cached {
            server: config.name.clone(),
            fingerprint: config.fingerprint.clone(),
            tools: self.tools.iter().map(|tool| tool.cached()).collect(),
            prompts: self.prompts.clone(),
            instructions: self.instructions.clone(),
        }
    }
}

/// The calls in flight on a server and when the last one ended: what decides whether a server
/// started on demand has been idle long enough to stop.
struct Activity {
    busy: AtomicUsize,
    used: Mutex<Instant>,
}

impl Activity {
    fn new() -> Arc<Activity> {
        Arc::new(Activity {
            busy: AtomicUsize::new(0),
            used: Mutex::new(Instant::now()),
        })
    }

    fn touch(&self) {
        *self.used.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    /// How long nothing has used the server; zero while a call is in flight.
    fn idle_for(&self) -> Duration {
        if self.busy.load(Ordering::SeqCst) > 0 {
            return Duration::ZERO;
        }
        self.used.lock().unwrap_or_else(PoisonError::into_inner).elapsed()
    }
}

/// Held while a request to a server is in flight.
struct InUse(Arc<Activity>);

impl InUse {
    fn new(activity: &Arc<Activity>) -> InUse {
        activity.busy.fetch_add(1, Ordering::SeqCst);
        InUse(activity.clone())
    }
}

impl Drop for InUse {
    fn drop(&mut self) {
        self.0.touch();
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Entry {
    config: ServerConfig,
    state: State,
    /// The connection, while there is one.
    peer: Option<Peer<RoleClient>>,
    catalog: Option<Catalog>,
    task: Option<JoinHandle<()>>,
    /// How many times the server has been connected; a call that lost its connection waits for
    /// this to move on.
    connections: u64,
    activity: Arc<Activity>,
}

impl Entry {
    fn new(config: ServerConfig, catalog: Option<Catalog>) -> Entry {
        Entry {
            state: resting(&config),
            config,
            peer: None,
            catalog,
            task: None,
            connections: 0,
            activity: Activity::new(),
        }
    }

    /// Whether the first call into the server starts it.
    fn on_demand(&self) -> bool {
        self.config.lifecycle != Lifecycle::Eager
    }

    /// What agents are offered of this server right now: what a connected server said it has,
    /// and for one that starts on demand also while it is not running.
    fn offers(&self) -> Option<&Catalog> {
        match self.state {
            State::Ready => self.catalog.as_ref(),
            State::Idle | State::Connecting if self.on_demand() => self.catalog.as_ref(),
            _ => None,
        }
    }
}

pub(crate) struct Inner {
    servers: RwLock<BTreeMap<String, Entry>>,
    problems: Vec<(String, String)>,
    pub(crate) services: Arc<Services>,
    pub(crate) roots: Vec<PathBuf>,
    credentials: Arc<Credentials>,
    options: Options,
    /// Tools found through `tool_search`; they stay listed for the rest of the session.
    activated: RwLock<BTreeSet<String>>,
    /// Where progress notices of calls in flight go.
    progress: Mutex<Vec<(ProgressToken, UpdateSink)>>,
    /// The user's answer, per server, to whether it may use the model.
    sampling: tokio::sync::Mutex<HashMap<String, bool>>,
    changed: Notify,
    started: AtomicBool,
    waited: AtomicBool,
    /// Where decisions about the project's servers are kept, and for which project.
    approvals: std::sync::OnceLock<(TrustStore, PathBuf, Vec<PathBuf>)>,
    /// Whether the user was asked about the servers that wait for approval.
    asked: AtomicBool,
}

/// What is known from earlier sessions of what `config`'s server offers. Only a server that
/// starts on demand is offered from that; one that is kept connected is asked.
fn known(inner: &Weak<Inner>, options: &Options, config: &ServerConfig) -> Option<Catalog> {
    if config.lifecycle == Lifecycle::Eager {
        return None;
    }
    let cached = catalog::read(options.cache_dir.as_deref()?, config)?;
    Some(Catalog::of(inner, cached))
}

/// The state a server is in before anything connects.
fn resting(config: &ServerConfig) -> State {
    match (config.enabled, config.approval) {
        (false, _) => State::Disabled,
        (true, Approval::Granted) => State::Idle,
        (true, Approval::Waiting) => State::WaitingApproval,
        (true, Approval::Rejected) => State::Rejected,
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let servers = self.servers.get_mut().unwrap_or_else(PoisonError::into_inner);
        for entry in servers.values() {
            if let Some(task) = &entry.task {
                task.abort();
            }
        }
    }
}

fn target(config: &ServerConfig) -> String {
    match &config.transport {
        Transport::Stdio { command, args, .. } => std::iter::once(command.as_str())
            .chain(args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        Transport::Http { url, .. } => url.clone(),
    }
}

/// Lists what a freshly connected (or changed) server offers, without the tools its entry
/// leaves out. A list that cannot be fetched is logged and left empty.
async fn fetch(
    inner: &Weak<Inner>,
    config: &ServerConfig,
    peer: &Peer<RoleClient>,
) -> (Vec<Arc<ProxyTool>>, Vec<Prompt>) {
    let (server, limit) = (config.name.as_str(), config.timeout);
    let capabilities = peer.peer_info().map(|info| info.capabilities.clone());
    let mut tools = Vec::new();
    if capabilities.as_ref().is_none_or(|c| c.tools.is_some()) {
        match tokio::time::timeout(limit, peer.list_all_tools()).await {
            Ok(Ok(listed)) => {
                let filter = config.tool_filter();
                tools = listed
                    .iter()
                    .filter(|tool| filter.allows(&tool.name))
                    .map(|tool| Arc::new(ProxyTool::new(inner.clone(), server, described(tool))))
                    .collect();
            }
            Ok(Err(e)) => tracing::warn!("mcp server {server}: could not list tools: {e}"),
            Err(_) => tracing::warn!("mcp server {server}: listing tools timed out"),
        }
    }
    let mut prompts = Vec::new();
    if capabilities.as_ref().is_some_and(|c| c.prompts.is_some()) {
        match tokio::time::timeout(limit, peer.list_all_prompts()).await {
            Ok(Ok(listed)) => prompts = listed,
            Ok(Err(e)) => tracing::warn!("mcp server {server}: could not list prompts: {e}"),
            Err(_) => tracing::warn!("mcp server {server}: listing prompts timed out"),
        }
    }
    (tools, prompts)
}

/// Ends when the server `name` has had no call for `after`, having marked it as not started.
/// Never ends for a server that is not stopped when idle.
async fn idle(weak: &Weak<Inner>, name: &str, after: Option<Duration>) {
    let Some(after) = after else {
        return std::future::pending().await;
    };
    loop {
        let Some(inner) = weak.upgrade() else { return };
        let idle_for = {
            // Decided with the entry locked: a call that found the server ready has counted
            // itself in before this can run.
            let mut servers = inner.write();
            let Some(entry) = servers.get_mut(name) else {
                return;
            };
            let idle_for = entry.activity.idle_for();
            if idle_for >= after {
                entry.peer = None;
                entry.state = State::Idle;
            }
            idle_for
        };
        if idle_for >= after {
            inner.changed.notify_waiters();
            return;
        }
        drop(inner);
        tokio::time::sleep(after - idle_for).await;
    }
}

/// Keeps one server connected: connects, waits for the connection to end, and tries again with
/// growing delays until it gives up. A server that starts on demand is not connected again when
/// its connection ends, and is stopped when it has been idle for long enough.
async fn supervise(weak: Weak<Inner>, name: String) {
    let mut failures = 0usize;
    loop {
        let Some(inner) = weak.upgrade() else { return };
        let Some(config) = inner.config(&name) else { return };
        inner.update(&name, |entry| entry.state = State::Connecting);
        let handler = Handler {
            server: name.clone(),
            inner: weak.clone(),
        };
        let problem = match client::connect(&config, handler, &inner.credentials).await {
            Ok(running) => {
                let connected = Instant::now();
                let peer = running.peer().clone();
                let (tools, prompts) = fetch(&weak, &config, &peer).await;
                let instructions = peer.peer_info().and_then(|info| info.instructions.clone());
                let catalog = Catalog {
                    tools,
                    prompts,
                    instructions,
                };
                inner.keep(&config, &catalog);
                inner.update(&name, |entry| {
                    entry.peer = Some(peer);
                    entry.catalog = Some(catalog);
                    entry.state = State::Ready;
                    entry.connections += 1;
                    // The time to stop counts from now, not from the call that woke it.
                    entry.activity.touch();
                });
                let idle_after = match config.lifecycle {
                    Lifecycle::Lazy => inner.options.idle_timeout,
                    Lifecycle::Eager | Lifecycle::LazyKeepAlive => None,
                };
                drop(inner);
                let stop = running.cancellation_token();
                let mut ended = std::pin::pin!(running.waiting());
                tokio::select! {
                    ended = &mut ended => tracing::warn!("mcp server {name} disconnected: {ended:?}"),
                    () = idle(&weak, &name, idle_after) => {
                        tracing::info!("mcp server {name}: stopped, nothing has called it for a while");
                        stop.cancel();
                        let _ = ended.await;
                        return;
                    }
                }
                if connected.elapsed() >= STABLE_AFTER {
                    failures = 0;
                }
                let Some(inner) = weak.upgrade() else { return };
                if config.lifecycle == Lifecycle::Lazy {
                    // The next call starts it again.
                    inner.update(&name, |entry| {
                        entry.peer = None;
                        entry.state = State::Idle;
                    });
                    return;
                }
                inner.update(&name, |entry| {
                    entry.peer = None;
                    entry.state = State::Connecting;
                });
                "the server keeps disconnecting".to_string()
            }
            Err(ConnectError::NeedsAuth) => {
                inner.update(&name, |entry| entry.state = State::NeedsAuth);
                handler::notify(
                    &inner,
                    Level::Warn,
                    &format!("MCP server `{name}` needs a login: /mcp login {name}"),
                );
                return;
            }
            Err(ConnectError::Fatal(why)) => {
                inner.update(&name, |entry| entry.state = State::Failed(why));
                return;
            }
            Err(ConnectError::Retry(why)) => {
                tracing::warn!("mcp server {name}: {why}");
                why
            }
        };
        failures += 1;
        let Some(delay) = RETRY_DELAYS.get(failures - 1) else {
            if let Some(inner) = weak.upgrade() {
                inner.update(&name, |entry| entry.state = State::Failed(problem));
            }
            return;
        };
        tokio::time::sleep(Duration::from_secs(*delay)).await;
    }
}

impl Inner {
    fn read(&self) -> RwLockReadGuard<'_, BTreeMap<String, Entry>> {
        self.servers.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, BTreeMap<String, Entry>> {
        self.servers.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn update(&self, name: &str, change: impl FnOnce(&mut Entry)) {
        if let Some(entry) = self.write().get_mut(name) {
            change(entry);
        }
        self.changed.notify_waiters();
    }

    fn config(&self, name: &str) -> Option<ServerConfig> {
        self.read().get(name).map(|entry| entry.config.clone())
    }

    /// Starts (or restarts) the task that keeps `name` connected. The task is started while the
    /// entry is locked and already `Connecting`, so its first change of state (a server that fails
    /// at once) cannot be overwritten.
    fn spawn(self: &Arc<Self>, name: &str) {
        let me = Arc::downgrade(self);
        self.update(name, |entry| {
            if let Some(previous) = entry.task.take() {
                previous.abort();
            }
            entry.peer = None;
            entry.state = State::Connecting;
            entry.task = Some(tokio::spawn(supervise(me, name.to_string())));
        });
    }

    /// Starts a server that waits for its first call. Whoever calls first starts it; the others
    /// find it connecting.
    fn wake(self: &Arc<Self>, name: &str) {
        let me = Arc::downgrade(self);
        self.update(name, |entry| {
            if entry.state == State::Idle && entry.on_demand() {
                entry.state = State::Connecting;
                entry.task = Some(tokio::spawn(supervise(me, name.to_string())));
            }
        });
    }

    /// Brings a server that may run to where it belongs once the session's servers are up:
    /// connected, or, for one that starts on demand and whose tools are known, waiting for its
    /// first call.
    fn bring_up(self: &Arc<Self>, name: &str) {
        let waits = self
            .read()
            .get(name)
            .is_some_and(|entry| entry.on_demand() && entry.catalog.is_some());
        if waits {
            self.stop(name, State::Idle);
        } else {
            self.spawn(name);
        }
    }

    /// Keeps what a server that starts on demand offers for the sessions to come.
    fn keep(&self, config: &ServerConfig, catalog: &Catalog) {
        if config.lifecycle != Lifecycle::Eager
            && let Some(dir) = &self.options.cache_dir
        {
            catalog::write(dir, &catalog.cached(config));
        }
    }

    /// Drops the connection to `name` and leaves it in `state`.
    fn stop(&self, name: &str, state: State) {
        self.update(name, |entry| {
            if let Some(task) = entry.task.take() {
                task.abort();
            }
            entry.peer = None;
            entry.state = state;
        });
    }

    pub(crate) fn progress_sink(&self, token: &ProgressToken) -> Option<UpdateSink> {
        let progress = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
        progress
            .iter()
            .find(|(known, _)| known == token)
            .map(|(_, sink)| sink.clone())
    }

    /// Whether `server` may use the model. The user is asked the first time; nobody to ask
    /// means no.
    pub(crate) async fn sampling_allowed(&self, server: &str) -> bool {
        let mut answers = self.sampling.lock().await;
        if let Some(answer) = answers.get(server) {
            return *answer;
        }
        let question = format!(
            "MCP server `{server}` asks to use your model to generate text. Allow it for this session?"
        );
        let allowed = match self.services.get::<Ui>() {
            Some(ui) => ui.0.confirm(&question).await.unwrap_or(false),
            None => false,
        };
        answers.insert(server.to_string(), allowed);
        allowed
    }

    /// Fetches the tool and prompt lists of a connected server again.
    pub(crate) async fn reload(self: &Arc<Self>, server: &str) {
        let found = {
            let servers = self.read();
            servers
                .get(server)
                .and_then(|entry| Some((entry.peer.clone()?, entry.config.clone())))
        };
        let Some((peer, config)) = found else { return };
        let (tools, prompts) = fetch(&Arc::downgrade(self), &config, &peer).await;
        self.update(server, |entry| {
            if entry.peer.is_some()
                && let Some(catalog) = &mut entry.catalog
            {
                catalog.tools = tools;
                catalog.prompts = prompts;
                self.keep(&config, catalog);
            }
        });
    }

    /// The connection to `server`, waiting while it is being (re)established; a server that
    /// waits for its first call is started. `after` is the connection count a caller already
    /// saw fail. The server counts as in use until the last value returned is dropped.
    async fn ready_peer(
        self: &Arc<Self>,
        server: &str,
        after: u64,
    ) -> Result<(Peer<RoleClient>, Duration, u64, InUse), String> {
        let deadline = Instant::now() + READY_WAIT;
        loop {
            let asleep = {
                let servers = self.read();
                let Some(entry) = servers.get(server) else {
                    return Err(format!("no MCP server named `{server}`"));
                };
                match (&entry.state, &entry.peer) {
                    (State::Ready, Some(peer)) if entry.connections > after => {
                        return Ok((
                            peer.clone(),
                            entry.config.timeout,
                            entry.connections,
                            InUse::new(&entry.activity),
                        ));
                    }
                    (State::Ready | State::Connecting, _) => false,
                    (State::Idle, _) if entry.on_demand() => true,
                    (State::NeedsAuth, _) => {
                        return Err(format!(
                            "MCP server `{server}` needs a login: the user can run /mcp login {server}"
                        ));
                    }
                    (state, _) => return Err(format!("MCP server `{server}` is {state}")),
                }
            };
            if asleep {
                self.wake(server);
            }
            if Instant::now() >= deadline {
                return Err(format!("MCP server `{server}` is still connecting"));
            }
            let _ = tokio::time::timeout(POLL, self.changed.notified()).await;
        }
    }

    /// Runs one request against `server` within its timeout.
    async fn request<T, F>(
        self: &Arc<Self>,
        server: &str,
        send: impl FnOnce(Peer<RoleClient>) -> F,
    ) -> Result<T, String>
    where
        F: Future<Output = Result<T, ServiceError>>,
    {
        let (peer, limit, _, _in_use) = self.ready_peer(server, 0).await?;
        match tokio::time::timeout(limit, send(peer)).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => Err(format!("MCP server `{server}`: {e}")),
            Err(_) => Err(format!(
                "MCP server `{server}` did not answer within {} s",
                limit.as_secs()
            )),
        }
    }

    async fn call_tool(
        self: &Arc<Self>,
        server: &str,
        tool: &str,
        input: Value,
        ctx: &ToolCtx,
    ) -> ToolOutput {
        let arguments: Option<JsonObject> = match input {
            Value::Object(map) => Some(map),
            Value::Null => None,
            _ => return ToolOutput::error("the arguments must be a JSON object"),
        };
        let mut seen = 0;
        // A connection found dead before the request left is replaced once.
        for attempt in 0..2 {
            let (peer, limit, connection, _in_use) = match self.ready_peer(server, seen).await {
                Ok(ready) => ready,
                Err(why) => return ToolOutput::error(why),
            };
            let mut params = CallToolRequestParams::new(tool.to_string());
            if let Some(arguments) = &arguments {
                params = params.with_arguments(arguments.clone());
            }
            let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
            let handle = match peer
                .send_cancellable_request(request, PeerRequestOptions::no_options())
                .await
            {
                Ok(handle) => handle,
                Err(ServiceError::TransportClosed) if attempt == 0 => {
                    seen = connection;
                    continue;
                }
                Err(e) => return ToolOutput::error(format!("MCP server `{server}`: {e}")),
            };
            let (token, id) = (handle.progress_token.clone(), handle.id.clone());
            self.progress
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((token.clone(), ctx.update.clone()));
            let outcome = tokio::select! {
                answer = tokio::time::timeout(limit, handle.await_response()) => Some(answer),
                _ = ctx.cancel.cancelled() => None,
            };
            self.progress
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|(known, _)| *known != token);
            let give_up = |reason: &str| {
                let notice = CancelledNotificationParam::new(Some(id.clone()), Some(reason.to_string()));
                let peer = peer.clone();
                async move {
                    let _ = peer.notify_cancelled(notice).await;
                }
            };
            return match outcome {
                Some(Ok(Ok(ServerResult::CallToolResult(result)))) => convert::tool_output(&result, ctx),
                Some(Ok(Ok(_))) => ToolOutput::error(format!(
                    "MCP server `{server}` answered `{tool}` with something that is not a tool result"
                )),
                Some(Ok(Err(e))) => ToolOutput::error(format!("MCP server `{server}`: {e}")),
                Some(Err(_)) => {
                    give_up("timed out").await;
                    ToolOutput::error(format!(
                        "`{tool}` on MCP server `{server}` did not finish within {} s",
                        limit.as_secs()
                    ))
                }
                None => {
                    give_up("cancelled by the user").await;
                    ToolOutput::error("cancelled")
                }
            };
        }
        ToolOutput::error(format!("MCP server `{server}` is not reachable"))
    }
}

/// The MCP servers of this process. Cheap to clone.
#[derive(Clone)]
pub struct McpManager(Arc<Inner>);

impl McpManager {
    /// `roots` are the directories servers are told about: the project root and extra dirs.
    pub fn new(
        loaded: Loaded,
        services: Arc<Services>,
        roots: Vec<PathBuf>,
        credentials: Arc<Credentials>,
        options: Options,
    ) -> Self {
        McpManager(Arc::new_cyclic(|inner| Inner {
            servers: RwLock::new(
                loaded
                    .servers
                    .into_iter()
                    .map(|config| {
                        let catalog = known(inner, &options, &config);
                        (config.name.clone(), Entry::new(config, catalog))
                    })
                    .collect(),
            ),
            problems: loaded.problems,
            services,
            roots,
            credentials,
            options,
            activated: RwLock::default(),
            progress: Mutex::default(),
            sampling: tokio::sync::Mutex::default(),
            changed: Notify::new(),
            started: AtomicBool::new(false),
            waited: AtomicBool::new(false),
            approvals: std::sync::OnceLock::new(),
            asked: AtomicBool::new(false),
        }))
    }

    /// Decisions about the servers `project` defines (in `files`) are kept in `store` from now on.
    pub fn record_approvals(&self, store: TrustStore, project: PathBuf, files: Vec<PathBuf>) {
        let _ = self.0.approvals.set((store, project, files));
    }

    /// The project's servers that wait for the user's approval.
    pub fn waiting(&self) -> Vec<ServerStatus> {
        self.status()
            .into_iter()
            .filter(|server| server.state == State::WaitingApproval)
            .collect()
    }

    /// Approves or rejects servers the project defines. The decisions are kept with the project's
    /// trust, for the definitions as they are now; an approved server starts connecting when the
    /// session's servers have been started. `Err` names a server that is not known or not the
    /// project's to decide about; `Ok(false)` that the decisions hold for this run only, because
    /// the project has no stored trust decision to keep them with.
    pub fn decide(&self, decisions: &[(String, bool)]) -> Result<bool, String> {
        let mut records = Vec::new();
        for (name, approved) in decisions {
            let config = self.known(name)?;
            let ours = self
                .0
                .approvals
                .get()
                .is_some_and(|(_, _, files)| files.contains(&config.source));
            if !ours && config.approval == Approval::Granted {
                return Err(format!(
                    "`{name}` is not a server of this project; only those need approval"
                ));
            }
            records.push((name.clone(), config.fingerprint.clone(), *approved));
        }
        let saved = match self.0.approvals.get() {
            Some((store, project, _)) => store.set_servers(project, &records).unwrap_or_else(|e| {
                tracing::warn!("could not save the decision about MCP servers: {e}");
                false
            }),
            None => false,
        };
        for (name, _, approved) in records {
            let approval = if approved {
                Approval::Granted
            } else {
                Approval::Rejected
            };
            self.0.update(&name, |entry| entry.config.approval = approval);
            let state = self.0.config(&name).map(|config| resting(&config));
            match state {
                Some(State::Idle) if self.0.started.load(Ordering::SeqCst) => self.0.bring_up(&name),
                Some(state) => self.0.stop(&name, state),
                None => {}
            }
        }
        Ok(saved)
    }

    /// Asks the user, once per process, which of the waiting servers may start. Nobody to ask
    /// leaves them waiting.
    pub(crate) async fn ask_approvals(&self) {
        let waiting = self.waiting();
        if waiting.is_empty() || self.0.asked.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(ui) = self.0.services.get::<Ui>() else {
            return;
        };
        let options: Vec<String> = waiting
            .iter()
            .map(|server| format!("{}: {}", server.name, server.target))
            .collect();
        let question = Question {
            text: "This project defines MCP servers that are new or have changed. Each one is a program \
                   or an address the project chose. Which may start?"
                .to_string(),
            header: None,
            context: None,
            excerpt: None,
            kind: AskKind::Multiselect,
            options: options.clone(),
            recommended: None,
            allow_freeform: false,
        };
        let Ok(chosen) = ui.0.ask(&question).await else {
            return;
        };
        let decisions: Vec<(String, bool)> = waiting
            .iter()
            .zip(&options)
            .map(|(server, option)| (server.name.clone(), chosen.contains(option)))
            .collect();
        if let Err(why) = self.decide(&decisions) {
            tracing::warn!("MCP server approval: {why}");
        }
    }

    /// The directory relative `cwd` values of servers are taken from: the project root.
    pub(crate) fn base_dir(&self) -> PathBuf {
        self.0.roots.first().cloned().unwrap_or_default()
    }

    /// Adds a server while the process runs, replacing one an extension added before under the
    /// same name. It starts connecting at once when the session's servers have been started.
    pub fn add(&self, config: ServerConfig) -> Result<(), String> {
        let name = config.name.clone();
        let state = resting(&config);
        let catalog = known(&Arc::downgrade(&self.0), &self.0.options, &config);
        {
            let mut servers = self.0.write();
            if let Some(existing) = servers.get(&name) {
                if existing.config.source != std::path::Path::new(config::EXTENSION_SOURCE) {
                    return Err(format!(
                        "an MCP server named `{name}` is already configured in {}",
                        existing.config.source.display()
                    ));
                }
                if let Some(task) = &existing.task {
                    task.abort();
                }
            }
            servers.insert(name.clone(), Entry::new(config, catalog));
        }
        if state == State::Idle && self.0.started.load(Ordering::SeqCst) {
            self.0.bring_up(&name);
        }
        self.0.changed.notify_waiters();
        Ok(())
    }

    /// Begins connecting the servers that are kept connected, in the background, and those that
    /// start on demand when nothing is known yet of what they offer. Later calls do nothing.
    pub fn start(&self) {
        if self.0.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let names: Vec<String> = self
            .0
            .read()
            .iter()
            .filter(|(_, entry)| resting(&entry.config) == State::Idle)
            .map(|(name, _)| name.clone())
            .collect();
        for name in names {
            self.0.bring_up(&name);
        }
    }

    /// Waits until no server is connecting, at most `limit`.
    pub async fn settle(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        loop {
            let connecting = self
                .0
                .read()
                .values()
                .any(|entry| entry.state == State::Connecting);
            if !connecting || Instant::now() >= deadline {
                return;
            }
            let _ = tokio::time::timeout(POLL.min(limit), self.0.changed.notified()).await;
        }
    }

    /// The pause before a session's first request: servers get `startup_wait` to come up. Later
    /// calls return at once; slower servers contribute their tools to a later request.
    pub async fn wait_startup(&self) {
        if !self.0.waited.swap(true, Ordering::SeqCst) {
            self.settle(self.0.options.startup_wait).await;
        }
    }

    pub fn status(&self) -> Vec<ServerStatus> {
        self.0
            .read()
            .values()
            .map(|entry| ServerStatus {
                name: entry.config.name.clone(),
                state: entry.state.clone(),
                tools: entry.offers().map_or(0, |catalog| catalog.tools.len()),
                prompts: entry.offers().map_or(0, |catalog| catalog.prompts.len()),
                target: target(&entry.config),
                source: entry.config.source.clone(),
            })
            .collect()
    }

    /// Entries and files that could not be used, as `(what, why)`.
    pub fn problems(&self) -> &[(String, String)] {
        &self.0.problems
    }

    fn known(&self, name: &str) -> Result<ServerConfig, String> {
        self.0
            .config(name)
            .ok_or_else(|| format!("no MCP server named `{name}`"))
    }

    /// Drops the connection to `name`, if any, and connects again from scratch.
    pub fn reconnect(&self, name: &str) -> Result<(), String> {
        match resting(&self.known(name)?) {
            State::Idle => {
                self.0.spawn(name);
                Ok(())
            }
            state => Err(format!("MCP server `{name}` is {state}")),
        }
    }

    /// Enables or disables `name`, in its config file and in this session.
    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<(), String> {
        let source = self.known(name)?.source;
        config::set_enabled(&source, name, enabled).map_err(|e| format!("{}: {e}", source.display()))?;
        self.0.update(name, |entry| entry.config.enabled = enabled);
        // Enabling does not approve: a project's server still waits for that.
        match self.0.config(name).map(|config| resting(&config)) {
            Some(State::Idle) => self.0.bring_up(name),
            Some(state) => self.0.stop(name, state),
            None => {}
        }
        Ok(())
    }

    /// Closes every connection.
    pub fn shutdown(&self) {
        let names: Vec<String> = self.0.read().keys().cloned().collect();
        for name in names {
            let state = self
                .0
                .config(&name)
                .map_or(State::Disabled, |config| resting(&config));
            self.0.stop(&name, state);
        }
    }

    fn proxies(&self) -> Vec<Arc<ProxyTool>> {
        self.0
            .read()
            .values()
            .filter_map(Entry::offers)
            .flat_map(|catalog| catalog.tools.iter().cloned())
            .collect()
    }

    /// Whether there are too many MCP tools to list them all in every request.
    pub fn deferring(&self) -> bool {
        self.proxies().len() > self.0.options.defer_threshold
    }

    /// The MCP tools to offer right now: all of them, or, when there are too many, only those
    /// found through `tool_search` so far.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let proxies = self.proxies();
        let deferring = proxies.len() > self.0.options.defer_threshold;
        let activated = self.0.activated.read().unwrap_or_else(PoisonError::into_inner);
        proxies
            .into_iter()
            .filter(|tool| !deferring || activated.contains(&tool.name))
            .map(|tool| tool as Arc<dyn Tool>)
            .collect()
    }

    /// Every MCP tool by name with whether agents are offered it right now; a tool that waits to
    /// be found through `tool_search` is not.
    pub fn tool_states(&self) -> Vec<(String, bool)> {
        let proxies = self.proxies();
        let deferring = proxies.len() > self.0.options.defer_threshold;
        let activated = self.0.activated.read().unwrap_or_else(PoisonError::into_inner);
        proxies
            .into_iter()
            .map(|tool| {
                let offered = !deferring || activated.contains(&tool.name);
                (tool.name.clone(), offered)
            })
            .collect()
    }

    /// Ranks every MCP tool against `query` and makes the hits available to agents.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        let proxies = self.proxies();
        let documents: Vec<(String, String)> = proxies
            .iter()
            .map(|tool| (tool.name.clone(), tool.description.clone()))
            .collect();
        let hits: Vec<SearchHit> = search::rank(&documents, query, limit)
            .into_iter()
            .filter_map(|index| proxies.get(index))
            .map(|tool| SearchHit {
                name: tool.name.clone(),
                description: tool.description.clone(),
                schema: tool.schema.clone(),
            })
            .collect();
        let mut activated = self.0.activated.write().unwrap_or_else(PoisonError::into_inner);
        activated.extend(hits.iter().map(|hit| hit.name.clone()));
        hits
    }

    /// The `mcp` part of the system prompt: the instructions of each server agents can use.
    pub fn instructions(&self) -> Option<String> {
        let mut out = String::new();
        for (name, entry) in self.0.read().iter() {
            let Some(text) = entry.offers().and_then(|catalog| catalog.instructions.as_deref()) else {
                continue;
            };
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            let shown: String = text.chars().take(INSTRUCTIONS_LIMIT).collect();
            out.push_str(&format!("## {name}\n{shown}\n\n"));
        }
        if out.is_empty() {
            return None;
        }
        Some(format!(
            "# MCP servers\nInstructions from the servers.\n\n{}",
            out.trim_end()
        ))
    }

    /// Every prompt of every server agents can use, as `(server, prompt)`.
    pub fn prompts(&self) -> Vec<(String, Prompt)> {
        let servers = self.0.read();
        servers
            .iter()
            .filter_map(|(name, entry)| Some((name, entry.offers()?)))
            .flat_map(|(name, catalog)| {
                catalog
                    .prompts
                    .iter()
                    .map(move |prompt| (name.clone(), prompt.clone()))
            })
            .collect()
    }

    /// Fetches a prompt with its arguments filled in and returns its text.
    pub async fn prompt_text(
        &self,
        server: &str,
        prompt: &str,
        arguments: JsonObject,
    ) -> Result<String, String> {
        let mut params = GetPromptRequestParams::new(prompt);
        if !arguments.is_empty() {
            params = params.with_arguments(arguments);
        }
        let result = self
            .0
            .request(server, |peer| async move { peer.get_prompt(params).await })
            .await?;
        let parts: Vec<String> = result
            .messages
            .iter()
            .filter_map(|message| convert::content_text(&message.content))
            .collect();
        if parts.is_empty() {
            return Err(format!("prompt `{prompt}` of MCP server `{server}` has no text"));
        }
        Ok(parts.join("\n\n"))
    }

    pub async fn read_resource(&self, server: &str, uri: &str, ctx: &ToolCtx) -> Result<Vec<Block>, String> {
        let params = ReadResourceRequestParams::new(uri);
        let result = self
            .0
            .request(server, |peer| async move { peer.read_resource(params).await })
            .await?;
        let blocks = convert::resource_blocks(&result.contents, ctx);
        if blocks.is_empty() {
            return Err(format!("resource `{uri}` of MCP server `{server}` is empty"));
        }
        Ok(blocks)
    }

    /// One line per resource and resource template of `server`, or of every connected server.
    pub async fn list_resources(&self, server: Option<&str>) -> Result<String, String> {
        let names: Vec<String> = match server {
            Some(name) => vec![self.known(name)?.name],
            None => self
                .0
                .read()
                .iter()
                .filter(|(_, entry)| entry.state == State::Ready)
                .map(|(name, _)| name.clone())
                .collect(),
        };
        let mut out = String::new();
        for name in &names {
            let offers =
                self.0.read().get(name).and_then(|entry| {
                    Some(entry.peer.as_ref()?.peer_info()?.capabilities.resources.is_some())
                });
            if offers == Some(false) {
                continue;
            }
            let resources = self
                .0
                .request(name, |peer| async move { peer.list_all_resources().await })
                .await?;
            for resource in resources {
                out.push_str(&format!("mcp://{name}/{}  {}", resource.uri, resource.name));
                if let Some(description) = &resource.description {
                    out.push_str(&format!(": {description}"));
                }
                out.push('\n');
            }
            let templates = self
                .0
                .request(
                    name,
                    |peer| async move { peer.list_all_resource_templates().await },
                )
                .await;
            for template in templates.unwrap_or_default() {
                out.push_str(&format!(
                    "mcp://{name}/{}  {} (template)\n",
                    template.uri_template, template.name
                ));
            }
        }
        if out.is_empty() {
            out.push_str("no resources");
        }
        Ok(out)
    }

    /// Logs in to the HTTP server `name` in the browser and connects once that is done. `show_url`
    /// receives the address the user has to open.
    pub async fn login(&self, name: &str, show_url: &(dyn Fn(&str) + Send + Sync)) -> Result<(), String> {
        let config = self.known(name)?;
        let Transport::Http { url, .. } = &config.transport else {
            return Err(format!(
                "MCP server `{name}` runs as a local command; there is nothing to log in to"
            ));
        };
        let http = client::http_client(&config)?;
        oauth::login(
            url,
            &config.oauth,
            self.0.credentials.clone(),
            http,
            show_url,
            LOGIN_WAIT,
        )
        .await?;
        if config.enabled {
            self.0.spawn(name);
        }
        Ok(())
    }

    /// Forgets the stored login of `name` and drops its connection. False when nothing was stored.
    pub fn logout(&self, name: &str) -> Result<bool, String> {
        let config = self.known(name)?;
        let Transport::Http { url, .. } = &config.transport else {
            return Ok(false);
        };
        let removed = oauth::logout(url, &self.0.credentials).map_err(|e| e.to_string())?;
        if removed && config.enabled {
            self.0.spawn(name);
        }
        Ok(removed)
    }
}
