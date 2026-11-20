//! The tool abstraction and what a tool receives when it runs.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use azazel_ai::{Block, ModelRef, ToolSpec};
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::event::{AgentId, AgentInfo};

/// What a call touches; permission modes key off this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Read,
    Write,
    Exec,
    Net,
}

/// How long one call of a tool may take.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Timeout {
    /// The tool keeps its own time (a shell command, a question to the user, a sub-agent); the
    /// agent adds none.
    #[default]
    Own,
    /// `tools.call_timeout_secs`, for a tool that has no limit of its own.
    Configured,
    After(Duration),
}

/// How a tool is put in front of the model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Offer {
    /// In every request (kept out of the prompt until searched for when `[tools] defer` names it
    /// and the provider searches for tools itself).
    #[default]
    Always,
    /// Waits to be found: where the provider searches for tools itself it is always sent kept out
    /// of the prompt (so finding it changes nothing the provider cached); elsewhere it is offered
    /// once a search tool of the harness (MCP's `tool_search`) has `found` it.
    Searchable { found: bool },
    /// A search tool of the harness, offered only where the provider does not search itself.
    Search,
}

/// The tools `[tools] defer` names, kept as a service: where the provider searches for tools
/// itself, they are kept out of the prompt until the model looks for them. A name ending in `*`
/// covers every tool it starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deferral(pub Vec<String>);

impl Deferral {
    pub fn covers(&self, name: &str) -> bool {
        self.0.iter().any(|entry| match entry.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => entry == name,
        })
    }
}

/// Code-first mode (`[tools] code_first`), kept as a service: the tools its predicate names for
/// an agent are left out of that agent's requests and are called from a Luau `eval` script. The
/// agent can still call one directly, and scripts reach it through `ToolCaller`. The answer must
/// not change during a session, or the provider's prompt cache is lost with every change.
#[derive(Clone)]
pub struct CodeFirst(pub Arc<HidesTool>);

/// Whether a tool, by name, is kept out of an agent's requests.
pub type HidesTool = dyn Fn(&AgentInfo, &str) -> bool + Send + Sync;

impl CodeFirst {
    /// Whether tool `name` is kept out of the requests of `agent`.
    pub fn hides(&self, agent: &AgentInfo, name: &str) -> bool {
        (self.0)(agent, name)
    }

    /// Whether the mode holds for an agent offered `tools`: only with `eval` among them, since
    /// hidden tools are reached through it. An agent without `eval` (an explicit `tools:` list,
    /// `--tools`, `tools.disabled`) keeps what it is offered.
    pub fn applies(&self, tools: &[Arc<dyn Tool>]) -> bool {
        tools.iter().any(|tool| tool.name() == "eval")
    }

    /// `tools` without those hidden from `agent`; all of them when the mode does not apply.
    pub fn visible(&self, agent: &AgentInfo, mut tools: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
        if self.applies(&tools) {
            tools.retain(|tool| !self.hides(agent, tool.name()));
        }
        tools
    }
}

/// A tool source that leaves out what code-first mode hides from the agent, for requests that must
/// carry the tool list the agent's own requests carry (questions asked on the side).
pub struct CodeFirstVisible {
    pub inner: Arc<dyn ToolSource>,
    pub code_first: Arc<CodeFirst>,
}

impl ToolSource for CodeFirstVisible {
    fn tools(&self, agent: &AgentInfo) -> Vec<Arc<dyn Tool>> {
        self.code_first.visible(agent, self.inner.tools(agent))
    }
}

/// What `[tools]` asks of every agent, kept as a service: `cache_reads`, `retries`,
/// `call_timeout_secs`. Without it nothing is cached, retried or limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSettings {
    /// A read-only call that gives the result it gave before, while that result is still in the
    /// context, is answered with a short note instead of the result again.
    pub cache_reads: bool,
    /// How many times an idempotent call is run again after a transient failure.
    pub retries: u32,
    /// The limit of a call whose tool has none of its own.
    pub call_timeout: Option<Duration>,
    /// The wait before the first retry; it doubles for each one after.
    pub backoff: Duration,
}

impl Default for ToolSettings {
    fn default() -> Self {
        ToolSettings {
            cache_reads: false,
            retries: 0,
            call_timeout: None,
            backoff: Duration::from_millis(500),
        }
    }
}

/// What the agent may do around one call of a tool: stop it when it takes too long, and run it
/// again when it failed for a reason that passes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolPolicy {
    /// The call can be run again without harm (it only reads), so a transient failure (a
    /// timeout, a reset connection, HTTP 429 or 5xx) is retried up to `tools.retries` times.
    pub idempotent: bool,
    pub timeout: Timeout,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema of the input object.
    fn schema(&self) -> Value;
    fn access(&self, input: &Value) -> Access;
    /// The strings permission rules are matched against: command lines, paths, URLs.
    fn subjects(&self, input: &Value) -> Vec<String>;
    /// Whether this call may run at the same time as other calls of the same assistant message.
    fn concurrency_safe(&self, input: &Value) -> bool {
        matches!(self.access(input), Access::Read)
    }
    /// Whether this call can be run a second time without harm: it only reads. A call that was
    /// running when the process ended is run again when the run is resumed if so; any other is
    /// reported as interrupted.
    fn replay_safe(&self, _input: &Value) -> bool {
        false
    }
    /// The time limit and retries of this call; none unless the tool says so.
    fn policy(&self, _input: &Value) -> ToolPolicy {
        ToolPolicy::default()
    }
    /// Whether the result of this call is the same when the call is made again and nothing it
    /// reads has changed. The agent then shows the model a short note in place of a result it
    /// already has in its context (`tools.cache_reads`); the call itself still runs.
    fn repeatable(&self, _input: &Value) -> bool {
        false
    }
    /// Something the user should know before approving this call (that it may cost money, for
    /// example); shown with the approval prompt.
    fn approval_note(&self, _input: &Value) -> Option<String> {
        None
    }
    /// Command lines this call has run later, with nobody there to ask (the checks of a
    /// schedule). The permission gate holds each to what a `shell` call of it would need.
    fn commands(&self, _input: &Value) -> Vec<String> {
        Vec::new()
    }
    /// Whether the tool is offered in a request to `model`, the first model the agent asks. A
    /// tool meant for some model families says no to the others (`patch` and `edit`).
    fn offered_to(&self, _model: &ModelRef) -> bool {
        true
    }
    /// How the tool is put in front of the model; see [`Offer`].
    fn offer(&self) -> Offer {
        Offer::Always
    }
    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput;
    /// Called once with the result of a call that succeeded, before it is stored in the context.
    /// A tool may shrink its own result here, knowing which earlier results the model still has.
    /// What it leaves is what every later request repeats, so it is never undone later.
    fn shape(&self, _input: &Value, _output: &mut ToolOutput, _ctx: &ShapeCtx<'_>) {}
}

/// What [`Tool::shape`] is told about the call.
pub struct ShapeCtx<'a> {
    pub agent_id: &'a str,
    pub call_id: &'a str,
    /// Whether the result of an earlier call is still in the context with its content.
    pub in_context: &'a (dyn Fn(&str) -> bool + Send + Sync),
}

pub fn tool_spec(tool: &dyn Tool) -> ToolSpec {
    ToolSpec {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        schema: tool.schema(),
        defer: false,
    }
}

/// The tools an agent may use, asked again before every request so the set can change mid-session.
pub trait ToolSource: Send + Sync {
    fn tools(&self, agent: &AgentInfo) -> Vec<Arc<dyn Tool>>;
}

impl ToolSource for Vec<Arc<dyn Tool>> {
    fn tools(&self, _agent: &AgentInfo) -> Vec<Arc<dyn Tool>> {
        self.clone()
    }
}

#[derive(Clone)]
pub struct ToolCtx {
    pub agent: AgentInfo,
    pub agent_id: AgentId,
    pub session_id: String,
    pub call_id: String,
    pub cwd: PathBuf,
    /// Fires when the user aborts; long-running tools must return promptly once it does.
    pub cancel: CancellationToken,
    pub update: UpdateSink,
    pub services: Arc<Services>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ToolOutput {
    /// `Text` and `Image` blocks.
    pub content: Vec<Block>,
    pub is_error: bool,
    /// Structured data for the UI (diffs, exit codes); never sent to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        ToolOutput {
            content: vec![Block::text(text)],
            is_error: false,
            details: None,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        ToolOutput {
            content: vec![Block::text(text)],
            is_error: true,
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// The text blocks joined with newlines.
    pub fn text_content(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let Block::Text { text } = block {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }
}

/// Progress text from a running tool, shown live and never sent to the model.
#[derive(Clone, Default)]
pub struct UpdateSink {
    sink: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

impl UpdateSink {
    pub fn new(sink: impl Fn(String) + Send + Sync + 'static) -> Self {
        UpdateSink {
            sink: Some(Arc::new(sink)),
        }
    }

    pub fn send(&self, text: impl Into<String>) {
        if let Some(sink) = &self.sink {
            sink(text.into());
        }
    }
}

/// What a [`ResultShaper`] is told about the call whose result it may shrink.
pub struct ShapeRequest<'a> {
    pub agent: &'a AgentInfo,
    pub call_id: &'a str,
    pub name: &'a str,
    pub input: &'a Value,
    /// The last thing the user asked, for notes that answer it.
    pub last_user: Option<&'a str>,
    pub cwd: &'a std::path::Path,
    pub services: &'a Arc<Services>,
    pub cancel: &'a CancellationToken,
}

/// Shrinks a tool result once, when the result is produced and before it is stored in the
/// context. What it returns is what the model sees from then on and what every later request
/// repeats, so it must leave the output as it is whenever it is unsure. Registered as a service
/// behind [`Shaper`].
#[async_trait]
pub trait ResultShaper: Send + Sync {
    async fn shape(&self, request: ShapeRequest<'_>, output: &mut ToolOutput);
}

/// The [`ResultShaper`] of the application, as a service.
#[derive(Clone)]
pub struct Shaper(pub Arc<dyn ResultShaper>);

/// Stores tool output that is too large to return and gives back a URI the model can read.
pub trait ArtifactSink: Send + Sync {
    fn spill(&self, call_id: &str, text: &str) -> Option<String>;
}

/// Application services keyed by type, so tools can reach them without this crate knowing them.
/// Trait-object services are registered behind a concrete wrapper type.
#[derive(Default)]
pub struct Services {
    map: RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl Services {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces any service of the same type.
    pub fn insert<T: Send + Sync + 'static>(&self, service: Arc<T>) {
        self.map
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(TypeId::of::<T>(), service);
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        let service = self
            .map
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&TypeId::of::<T>())
            .cloned()?;
        service.downcast::<T>().ok()
    }
}
