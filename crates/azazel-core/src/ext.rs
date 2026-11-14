//! The extension surface: what an extension can register, and the event bus its handlers hang on.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};

use async_trait::async_trait;
use azazel_agent::{Access, AgentHooks, AgentInfo, Services, Tool, ToolCtx, ToolOutput, ToolSource};
use azazel_ai::{Block, Message, ModelSpec, Provider, Request, SystemPart, VirtualModel, VirtualModels};
use serde_json::Value;

use crate::cache::CacheLedger;
use crate::prompt::{PromptBuilder, SystemPartProvider};
use crate::scheme::{Scheme, Schemes};
use crate::skills::{SkillRegistry, SkillRoot, SkillSource};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ExtError(pub String);

/// A unit of functionality added to the harness. Built-in features, config hooks, the Luau host
/// and MCP are all extensions.
pub trait Extension: Send + Sync + 'static {
    fn id(&self) -> &str;
    fn register(&self, r: &mut Registrar) -> Result<(), ExtError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartSource {
    Startup,
    Resume,
    Clear,
    Compact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    SessionStart,
    SessionEnd,
    UserPrompt,
    BeforeAgentStart,
    TurnStart,
    TurnEnd,
    ToolCall,
    ToolResult,
    Context,
    BeforeRequest,
    AgentSpawn,
    AgentEnd,
    PreCompact,
    PostCompact,
    Stop,
    PermissionRequest,
    Notification,
    FileChanged,
    ConfigChange,
    WorktreeCreate,
    WorktreeRemove,
    Setup,
}

/// Handlers receive the event mutably and may change the fields documented as replaceable.
#[derive(Debug, Clone)]
pub enum Event {
    SessionStart {
        source: StartSource,
    },
    SessionEnd,
    /// `text` and `images` may be replaced; blocking rejects the prompt.
    UserPrompt {
        text: String,
        images: Vec<Block>,
    },
    /// `system` may be extended.
    BeforeAgentStart {
        agent: AgentInfo,
        system: Vec<SystemPart>,
    },
    TurnStart {
        agent: AgentInfo,
    },
    TurnEnd {
        agent: AgentInfo,
    },
    /// `input` may be rewritten; `skip_prompt` removes the approval prompt; blocking denies the call.
    ToolCall {
        agent: AgentInfo,
        call_id: String,
        tool: String,
        input: Value,
        skip_prompt: bool,
    },
    /// `output` may be changed.
    ToolResult {
        agent: AgentInfo,
        call_id: String,
        tool: String,
        output: ToolOutput,
    },
    /// `messages` may be rewritten for this request only.
    Context {
        agent: AgentInfo,
        messages: Vec<Message>,
    },
    BeforeRequest {
        agent: AgentInfo,
        request: Request,
    },
    /// `prompt` may be replaced; blocking refuses the spawn.
    AgentSpawn {
        parent: AgentInfo,
        def: String,
        prompt: String,
    },
    AgentEnd {
        agent: AgentInfo,
        status: String,
    },
    /// Blocking cancels a manual compaction.
    PreCompact {
        agent: AgentInfo,
        manual: bool,
    },
    PostCompact {
        agent: AgentInfo,
    },
    /// Blocking keeps the agent going with the reason as a user message.
    Stop {
        agent: AgentInfo,
        stop_hook_active: bool,
    },
    /// Setting `decision` settles the request without asking the user.
    PermissionRequest {
        agent: AgentInfo,
        tool: String,
        input: Value,
        subjects: Vec<String>,
        decision: Option<bool>,
    },
    Notification {
        level: Level,
        text: String,
    },
    /// A `write` or `edit` succeeded. `path` is absolute.
    FileChanged {
        agent: AgentInfo,
        path: PathBuf,
        tool: String,
    },
    /// The settings menu (`source` is `settings`) or a reload (`reload`) changed the
    /// configuration.
    ConfigChange {
        source: String,
    },
    /// The worktree of an isolated agent was made.
    WorktreeCreate {
        agent_id: String,
        path: PathBuf,
    },
    /// The worktree of an isolated agent was removed.
    WorktreeRemove {
        agent_id: String,
        path: PathBuf,
    },
    /// `azazel init` finished (`trigger` is `init`), or the project runs trusted for the first
    /// time (`trust`).
    Setup {
        trigger: String,
    },
}

impl Event {
    pub fn kind(&self) -> EventKind {
        match self {
            Event::SessionStart { .. } => EventKind::SessionStart,
            Event::SessionEnd => EventKind::SessionEnd,
            Event::UserPrompt { .. } => EventKind::UserPrompt,
            Event::BeforeAgentStart { .. } => EventKind::BeforeAgentStart,
            Event::TurnStart { .. } => EventKind::TurnStart,
            Event::TurnEnd { .. } => EventKind::TurnEnd,
            Event::ToolCall { .. } => EventKind::ToolCall,
            Event::ToolResult { .. } => EventKind::ToolResult,
            Event::Context { .. } => EventKind::Context,
            Event::BeforeRequest { .. } => EventKind::BeforeRequest,
            Event::AgentSpawn { .. } => EventKind::AgentSpawn,
            Event::AgentEnd { .. } => EventKind::AgentEnd,
            Event::PreCompact { .. } => EventKind::PreCompact,
            Event::PostCompact { .. } => EventKind::PostCompact,
            Event::Stop { .. } => EventKind::Stop,
            Event::PermissionRequest { .. } => EventKind::PermissionRequest,
            Event::Notification { .. } => EventKind::Notification,
            Event::FileChanged { .. } => EventKind::FileChanged,
            Event::ConfigChange { .. } => EventKind::ConfigChange,
            Event::WorktreeCreate { .. } => EventKind::WorktreeCreate,
            Event::WorktreeRemove { .. } => EventKind::WorktreeRemove,
            Event::Setup { .. } => EventKind::Setup,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Block { reason: String },
}

/// What a handler or command can reach.
#[derive(Clone)]
pub struct ExtCtx {
    pub services: Arc<Services>,
    pub cwd: PathBuf,
    pub session_id: String,
}

#[async_trait]
pub trait EventHandler: Send + Sync {
    async fn handle(&self, ev: &mut Event, ctx: &ExtCtx) -> Outcome;
}

struct Registered {
    owner: String,
    handler: Arc<dyn EventHandler>,
}

/// Dispatches events to handlers in registration order, one at a time; the first `Block` stops
/// the chain.
pub struct EventBus {
    handlers: RwLock<HashMap<EventKind, Vec<Registered>>>,
    ctx: RwLock<ExtCtx>,
}

impl EventBus {
    pub fn new(ctx: ExtCtx) -> Self {
        EventBus {
            handlers: RwLock::default(),
            ctx: RwLock::new(ctx),
        }
    }

    pub fn ctx(&self) -> ExtCtx {
        self.ctx.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Call when a session starts or is switched so handlers see the right id.
    pub fn set_session(&self, session_id: &str) {
        self.ctx
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .session_id = session_id.to_string();
    }

    pub fn on(&self, kind: EventKind, owner: &str, handler: Arc<dyn EventHandler>) {
        self.handlers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(kind)
            .or_default()
            .push(Registered {
                owner: owner.to_string(),
                handler,
            });
    }

    /// Drops every handler `owner` registered (extension reload).
    pub fn remove_owner(&self, owner: &str) {
        let mut handlers = self.handlers.write().unwrap_or_else(PoisonError::into_inner);
        for list in handlers.values_mut() {
            list.retain(|r| r.owner != owner);
        }
    }

    pub async fn emit(&self, ev: &mut Event) -> Outcome {
        let handlers: Vec<Arc<dyn EventHandler>> = {
            let map = self.handlers.read().unwrap_or_else(PoisonError::into_inner);
            match map.get(&ev.kind()) {
                Some(list) => list.iter().map(|r| r.handler.clone()).collect(),
                None => return Outcome::Continue,
            }
        };
        let ctx = self.ctx();
        for handler in handlers {
            if let Outcome::Block { reason } = handler.handle(ev, &ctx).await {
                return Outcome::Block { reason };
            }
        }
        Outcome::Continue
    }
}

/// A slash command.
#[derive(Clone)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub handler: Arc<dyn CommandHandler>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Nothing further to do.
    Done,
    /// Show this text to the user.
    Show(String),
    /// Send this text to the lead agent as the user, on `model` for this prompt when it is set.
    Prompt {
        text: String,
        model: Option<Vec<ModelSpec>>,
    },
    Error(String),
}

#[async_trait]
pub trait CommandHandler: Send + Sync {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome;
}

/// Commands that come and go while the session runs (a connected server's prompts, for example).
pub trait CommandSource: Send + Sync {
    fn commands(&self) -> Vec<Command>;
}

/// Code that runs around every call of one tool, whichever implementation of the tool is in use.
#[async_trait]
pub trait ToolWrap: Send + Sync {
    /// Runs before the tool, after the call was permitted. `input` may be rewritten; `Err` answers
    /// the call with that output and the tool does not run.
    async fn before(&self, _input: &mut Value, _ctx: &ToolCtx) -> Result<(), ToolOutput> {
        Ok(())
    }

    /// Runs after the tool with the input it was given; `output` may be changed.
    async fn after(&self, _input: &Value, _output: &mut ToolOutput, _ctx: &ToolCtx) {}
}

/// A tool with the wraps registered for its name around it.
struct Wrapped {
    inner: Arc<dyn Tool>,
    /// In registration order: the first is the outermost.
    wraps: Vec<Arc<dyn ToolWrap>>,
}

#[async_trait]
impl Tool for Wrapped {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn schema(&self) -> Value {
        self.inner.schema()
    }

    fn access(&self, input: &Value) -> Access {
        self.inner.access(input)
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        self.inner.subjects(input)
    }

    fn concurrency_safe(&self, input: &Value) -> bool {
        self.inner.concurrency_safe(input)
    }

    fn replay_safe(&self, input: &Value) -> bool {
        self.inner.replay_safe(input)
    }

    fn approval_note(&self, input: &Value) -> Option<String> {
        self.inner.approval_note(input)
    }

    fn offered_to(&self, model: &azazel_ai::ModelRef) -> bool {
        self.inner.offered_to(model)
    }

    async fn call(&self, mut input: Value, ctx: &ToolCtx) -> ToolOutput {
        let mut entered = 0;
        let mut answered = None;
        for wrap in &self.wraps {
            match wrap.before(&mut input, ctx).await {
                Ok(()) => entered += 1,
                Err(output) => {
                    answered = Some(output);
                    break;
                }
            }
        }
        let mut output = match answered {
            Some(output) => output,
            None => self.inner.call(input.clone(), ctx).await,
        };
        // Only the wraps that let the call in see it come out, innermost first.
        for wrap in self.wraps[..entered].iter().rev() {
            wrap.after(&input, &mut output, ctx).await;
        }
        output
    }
}

/// Tools registered in place of others, with the extension they are removed with and the name
/// listings give for it (the extension's id unless the extension gave another).
struct Replacement {
    owner: String,
    label: String,
    source: Arc<dyn ToolSource>,
}

/// A wrap registered around the tool `tool`.
struct Wrapping {
    owner: String,
    label: String,
    tool: String,
    wrap: Arc<dyn ToolWrap>,
}

/// Every tool available to agents: registered tools plus dynamic sources, minus disabled names.
/// The first tool registered under a name wins, unless a replacement was registered for the name:
/// then the replacement registered last does. Wraps go around whichever won.
#[derive(Default)]
pub struct ToolRegistry {
    tools: RwLock<Vec<(String, Arc<dyn Tool>)>>,
    sources: RwLock<Vec<(String, Arc<dyn ToolSource>)>>,
    replacements: RwLock<Vec<Replacement>>,
    wraps: RwLock<Vec<Wrapping>>,
    selection: RwLock<Selection>,
}

/// Which tool names are offered: those the allowed patterns match (every name when there are
/// none), minus those a disabled pattern matches. Patterns are globs (`mcp__github__*`).
#[derive(Default)]
struct Selection {
    allowed: Option<globset::GlobSet>,
    disabled: globset::GlobSet,
}

impl Selection {
    fn offers(&self, name: &str) -> bool {
        self.allowed.as_ref().is_none_or(|allowed| allowed.is_match(name)) && !self.disabled.is_match(name)
    }
}

/// One tool as [`ToolRegistry::listing`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolListing {
    pub name: String,
    /// Who registered the implementation in use: the id of an extension, or the name it gave
    /// for a part of itself (`lua:<script>`).
    pub owner: String,
    /// False when `[tools] disabled` or the tool flags leave it out.
    pub offered: bool,
    /// The implementation in use was registered as a replacement.
    pub replacement: bool,
    /// Who registered the implementation the replacement stands in for, when there was one.
    pub replaces: Option<String>,
    /// Who wraps the tool, outermost first.
    pub wrapped_by: Vec<String>,
}

fn glob_set(patterns: &[String]) -> Result<globset::GlobSet, String> {
    let mut set = globset::GlobSetBuilder::new();
    for pattern in patterns {
        set.add(globset::Glob::new(pattern).map_err(|e| format!("tool pattern `{pattern}`: {e}"))?);
    }
    set.build().map_err(|e| e.to_string())
}

/// A tool as the registry settled it, before selection and wrapping.
struct Settled {
    tool: Arc<dyn Tool>,
    owner: String,
    replacement: bool,
    replaces: Option<String>,
}

impl ToolRegistry {
    pub fn add(&self, owner: &str, tool: Arc<dyn Tool>) {
        self.tools
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push((owner.to_string(), tool));
    }

    /// A source is asked for its tools before every request (MCP servers come and go).
    pub fn add_source(&self, owner: &str, source: Arc<dyn ToolSource>) {
        self.sources
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push((owner.to_string(), source));
    }

    /// The tools of `source` take the place of whatever else is registered under their names,
    /// earlier replacements included. `label` is what listings call the one who replaced them.
    pub fn replace(&self, owner: &str, label: &str, source: Arc<dyn ToolSource>) {
        self.replacements
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Replacement {
                owner: owner.to_string(),
                label: label.to_string(),
                source,
            });
    }

    /// Puts `wrap` around the tool `name`, inside the wraps registered for it before.
    pub fn wrap(&self, owner: &str, label: &str, name: &str, wrap: Arc<dyn ToolWrap>) {
        self.wraps
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Wrapping {
                owner: owner.to_string(),
                label: label.to_string(),
                tool: name.to_string(),
                wrap,
            });
    }

    /// Offers only the tools `allowed` matches (all when `None`), minus those `disabled` matches.
    pub fn select(&self, allowed: Option<&[String]>, disabled: &[String]) -> Result<(), String> {
        let selection = Selection {
            allowed: allowed.map(glob_set).transpose()?,
            disabled: glob_set(disabled)?,
        };
        *self.selection.write().unwrap_or_else(PoisonError::into_inner) = selection;
        Ok(())
    }

    /// One tool per name, in registration order; a replacement stands where what it replaced
    /// stood, or at the end when it replaced nothing.
    fn settled(&self, agent: &AgentInfo) -> Vec<Settled> {
        let mut out: Vec<Settled> = Vec::new();
        let mut add = |owner: &str, tool: Arc<dyn Tool>| {
            if !out.iter().any(|settled| settled.tool.name() == tool.name()) {
                out.push(Settled {
                    tool,
                    owner: owner.to_string(),
                    replacement: false,
                    replaces: None,
                });
            }
        };
        for (owner, tool) in self.tools.read().unwrap_or_else(PoisonError::into_inner).iter() {
            add(owner, tool.clone());
        }
        // The sources are asked outside the locks: one may register something while it answers.
        let sources: Vec<(String, Arc<dyn ToolSource>)> = self
            .sources
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for (owner, source) in sources {
            for tool in source.tools(agent) {
                add(&owner, tool);
            }
        }
        let replacements: Vec<(String, Arc<dyn ToolSource>)> = self
            .replacements
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .rev()
            .map(|entry| (entry.label.clone(), entry.source.clone()))
            .collect();
        for (label, source) in replacements {
            for tool in source.tools(agent) {
                match out.iter_mut().find(|settled| settled.tool.name() == tool.name()) {
                    // A replacement registered later has the name already.
                    Some(settled) if settled.replacement => {}
                    Some(settled) => {
                        settled.replaces = Some(std::mem::replace(&mut settled.owner, label.clone()));
                        settled.replacement = true;
                        settled.tool = tool;
                    }
                    None => out.push(Settled {
                        tool,
                        owner: label.clone(),
                        replacement: true,
                        replaces: None,
                    }),
                }
            }
        }
        out
    }

    /// Names of every registered tool and every tool a source offers now, before selection.
    pub fn all_names(&self, agent: &AgentInfo) -> Vec<String> {
        let mut names: Vec<String> = self
            .settled(agent)
            .iter()
            .map(|settled| settled.tool.name().to_string())
            .collect();
        names.sort();
        names
    }

    /// Every tool agents could be offered, in registration order, with who registered the
    /// implementation in use, whether the selection offers it, and who replaced or wraps it. A
    /// name registered twice is listed once.
    pub fn listing(&self, agent: &AgentInfo) -> Vec<ToolListing> {
        let settled = self.settled(agent);
        let selection = self.selection.read().unwrap_or_else(PoisonError::into_inner);
        let wraps = self.wraps.read().unwrap_or_else(PoisonError::into_inner);
        settled
            .into_iter()
            .map(|settled| {
                let name = settled.tool.name();
                ToolListing {
                    offered: selection.offers(name),
                    wrapped_by: wraps
                        .iter()
                        .filter(|entry| entry.tool == name)
                        .map(|entry| entry.label.clone())
                        .collect(),
                    name: name.to_string(),
                    owner: settled.owner,
                    replacement: settled.replacement,
                    replaces: settled.replaces,
                }
            })
            .collect()
    }

    /// The tools registered as replacements, as `(who registered it, tool name)`, in
    /// registration order.
    pub fn replacements(&self, agent: &AgentInfo) -> Vec<(String, String)> {
        let sources: Vec<(String, Arc<dyn ToolSource>)> = self
            .replacements
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|entry| (entry.label.clone(), entry.source.clone()))
            .collect();
        let mut out = Vec::new();
        for (label, source) in sources {
            for tool in source.tools(agent) {
                out.push((label.clone(), tool.name().to_string()));
            }
        }
        out
    }

    /// The wraps there are, as `(who registered it, tool name)`, in registration order.
    pub fn wraps(&self) -> Vec<(String, String)> {
        self.wraps
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|entry| (entry.label.clone(), entry.tool.clone()))
            .collect()
    }

    pub fn remove_owner(&self, owner: &str) {
        self.tools
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(o, _)| o != owner);
        self.sources
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(o, _)| o != owner);
        self.replacements
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|entry| entry.owner != owner);
        self.wraps
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|entry| entry.owner != owner);
    }

    pub fn get(&self, agent: &AgentInfo, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools(agent).into_iter().find(|t| t.name() == name)
    }
}

impl ToolSource for ToolRegistry {
    fn tools(&self, agent: &AgentInfo) -> Vec<Arc<dyn Tool>> {
        let settled = self.settled(agent);
        let selection = self.selection.read().unwrap_or_else(PoisonError::into_inner);
        let wraps = self.wraps.read().unwrap_or_else(PoisonError::into_inner);
        settled
            .into_iter()
            .filter(|settled| selection.offers(settled.tool.name()))
            .map(|settled| {
                let around: Vec<Arc<dyn ToolWrap>> = wraps
                    .iter()
                    .filter(|entry| entry.tool == settled.tool.name())
                    .map(|entry| entry.wrap.clone())
                    .collect();
                if around.is_empty() {
                    settled.tool
                } else {
                    Arc::new(Wrapped {
                        inner: settled.tool,
                        wraps: around,
                    })
                }
            })
            .collect()
    }
}

/// Everything extensions have contributed. Shared by the whole process.
pub struct Registry {
    pub tools: Arc<ToolRegistry>,
    pub bus: Arc<EventBus>,
    pub schemes: Arc<Schemes>,
    pub prompt: Arc<PromptBuilder>,
    pub services: Arc<Services>,
    pub skills: Arc<SkillRegistry>,
    commands: RwLock<Vec<(String, Command)>>,
    command_sources: RwLock<Vec<(String, Arc<dyn CommandSource>)>>,
    providers: RwLock<Vec<Arc<dyn Provider>>>,
    /// The virtual models extensions define. The router reads the same set, so one registered
    /// while the process runs is routed through at once.
    pub virtual_models: Arc<VirtualModels>,
    /// Ids of the extensions loaded so far, in load order.
    loaded: RwLock<Vec<String>>,
}

/// The registry as a service, for extensions that load or unload others while the process runs
/// (a script host reloading its scripts).
pub struct RegistryHandle(pub std::sync::Weak<Registry>);

/// Tells extensions that the configuration changed: `source` is `settings` for the settings
/// menu and `reload` for a reload. Does nothing where the registry is not published.
pub async fn config_changed(services: &Services, source: &str) {
    let Some(registry) = services
        .get::<RegistryHandle>()
        .and_then(|handle| handle.0.upgrade())
    else {
        return;
    };
    let mut event = Event::ConfigChange {
        source: source.to_string(),
    };
    registry.bus.emit(&mut event).await;
}

/// Lets an extension add MCP servers without depending on the MCP client.
pub trait McpRegistrar: Send + Sync {
    /// Adds a server and starts connecting to it. `entry` has the shape of an `mcp.json` entry.
    fn add_server(&self, name: &str, entry: Value) -> Result<(), String>;
}

/// The service under which the MCP client, when there is one, accepts servers from extensions.
pub struct McpServers(pub Arc<dyn McpRegistrar>);

impl Registry {
    pub fn new(ctx: ExtCtx) -> Self {
        let services = ctx.services.clone();
        let schemes = Arc::new(Schemes::default());
        let skills = Arc::new(SkillRegistry::default());
        services.insert(schemes.clone());
        services.insert(skills.clone());
        services.insert(Arc::new(crate::ui::UiExtras::default()));
        Registry {
            tools: Arc::new(ToolRegistry::default()),
            bus: Arc::new(EventBus::new(ctx)),
            schemes,
            prompt: Arc::new(PromptBuilder::default()),
            services,
            skills,
            commands: RwLock::default(),
            command_sources: RwLock::default(),
            providers: RwLock::default(),
            virtual_models: Arc::default(),
            loaded: RwLock::default(),
        }
    }

    /// Makes the registry reachable through the services as a [`RegistryHandle`].
    pub fn publish(self: &Arc<Self>) {
        self.services
            .insert(Arc::new(RegistryHandle(Arc::downgrade(self))));
    }

    /// Ids of the loaded extensions, in load order.
    pub fn extensions(&self) -> Vec<String> {
        self.loaded.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Runs `extension.register`, attributing what it registers to its id.
    pub fn load(&self, extension: &dyn Extension) -> Result<(), ExtError> {
        let mut registrar = Registrar {
            owner: extension.id().to_string(),
            registry: self,
        };
        extension.register(&mut registrar)?;
        let mut loaded = self.loaded.write().unwrap_or_else(PoisonError::into_inner);
        if !loaded.iter().any(|id| id == extension.id()) {
            loaded.push(extension.id().to_string());
        }
        Ok(())
    }

    /// Removes everything registered under `owner`.
    pub fn unload(&self, owner: &str) {
        self.tools.remove_owner(owner);
        self.bus.remove_owner(owner);
        self.prompt.remove_owner(owner);
        self.schemes.remove_owner(owner);
        self.virtual_models.remove_owner(owner);
        self.commands
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(o, _)| o != owner);
        self.command_sources
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(o, _)| o != owner);
        if let Some(extras) = self.services.get::<crate::ui::UiExtras>() {
            extras.remove_owner(owner);
        }
        self.loaded
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|id| id != owner);
    }

    /// Commands sorted by name; the first registration of a name wins, and registered commands
    /// come before those of dynamic sources.
    pub fn commands(&self) -> Vec<Command> {
        let sources: Vec<Arc<dyn CommandSource>> = self
            .command_sources
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(_, source)| source.clone())
            .collect();
        let registered: Vec<Command> = self
            .commands
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(_, command)| command.clone())
            .collect();
        let mut out: Vec<Command> = Vec::new();
        for command in registered
            .into_iter()
            .chain(sources.iter().flat_map(|source| source.commands()))
        {
            if !out.iter().any(|c| c.name == command.name) {
                out.push(command);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn command(&self, name: &str) -> Option<Command> {
        self.commands().into_iter().find(|c| c.name == name)
    }

    /// The names of the commands extensions registered outright, sorted: those of dynamic
    /// sources (skills, custom commands, MCP prompts) depend on the machine and are left out.
    pub fn registered_commands(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .commands
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(_, command)| command.name.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Providers contributed by extensions, in registration order.
    pub fn providers(&self) -> Vec<Arc<dyn Provider>> {
        self.providers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The hooks an agent runs with: the event bus plus per-request system prompt rendering.
    pub fn agent_hooks(&self) -> Arc<dyn AgentHooks> {
        Arc::new(BusHooks {
            bus: self.bus.clone(),
            prompt: self.prompt.clone(),
            rules: self.services.get::<crate::rules::Rules>(),
            runaway: self.services.get::<crate::runaway::Runaway>(),
            services: self.services.clone(),
        })
    }
}

/// Handed to [`Extension::register`].
pub struct Registrar<'a> {
    owner: String,
    registry: &'a Registry,
}

impl Registrar<'_> {
    pub fn tool(&mut self, t: Arc<dyn Tool>) {
        self.registry.tools.add(&self.owner, t);
    }

    pub fn tool_source(&mut self, s: Arc<dyn ToolSource>) {
        self.registry.tools.add_source(&self.owner, s);
    }

    /// Registers `t` in place of whatever else has its name: a built-in tool, another
    /// extension's, or an earlier replacement.
    pub fn replace_tool(&mut self, t: Arc<dyn Tool>) {
        self.registry
            .tools
            .replace(&self.owner, &self.owner, Arc::new(vec![t]));
    }

    /// Like [`Registrar::replace_tool`] for tools that come and go; listings name `label` as the
    /// one who replaced them.
    pub fn replace_tool_source(&mut self, label: &str, s: Arc<dyn ToolSource>) {
        self.registry.tools.replace(&self.owner, label, s);
    }

    /// Runs `w` around every call of the tool `name`, whoever registered the tool.
    pub fn wrap_tool(&mut self, name: &str, w: Arc<dyn ToolWrap>) {
        self.registry.tools.wrap(&self.owner, &self.owner, name, w);
    }

    /// Like [`Registrar::wrap_tool`]; listings name `label` as the one who wraps the tool.
    pub fn wrap_tool_as(&mut self, label: &str, name: &str, w: Arc<dyn ToolWrap>) {
        self.registry.tools.wrap(&self.owner, label, name, w);
    }

    pub fn command_source(&mut self, s: Arc<dyn CommandSource>) {
        self.registry
            .command_sources
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push((self.owner.clone(), s));
    }

    pub fn command(&mut self, c: Command) {
        self.registry
            .commands
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push((self.owner.clone(), c));
    }

    pub fn on(&mut self, kind: EventKind, h: Arc<dyn EventHandler>) {
        self.registry.bus.on(kind, &self.owner, h);
    }

    pub fn provider(&mut self, p: Arc<dyn Provider>) {
        self.registry
            .providers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(p);
    }

    /// Makes `virtual/<name>` a model that can be chosen; `m` picks the model of each request
    /// made with it. A later registration of the same name takes its place.
    pub fn virtual_model(&mut self, name: &str, m: Arc<dyn VirtualModel>) {
        self.registry.virtual_models.add(&self.owner, name, m);
    }

    pub fn scheme(&mut self, s: Arc<dyn Scheme>) {
        self.registry.schemes.add(&self.owner, s);
    }

    /// Adds a directory of skills, taking precedence over the roots added before it.
    pub fn skill_root(&mut self, path: PathBuf, source: SkillSource) {
        self.registry.skills.add_root(SkillRoot::new(path, source));
    }

    /// Contributes one named part of the system prompt.
    pub fn system_part(&mut self, name: &str, p: Arc<dyn SystemPartProvider>) {
        self.registry.prompt.add(&self.owner, name, p);
    }

    pub fn service<T: Send + Sync + 'static>(&mut self, svc: Arc<T>) {
        self.registry.services.insert(svc);
    }

    pub fn services(&self) -> &Arc<Services> {
        &self.registry.services
    }
}

struct BusHooks {
    bus: Arc<EventBus>,
    prompt: Arc<PromptBuilder>,
    /// The stream rules, when the rules extension was loaded before these hooks were made.
    rules: Option<Arc<crate::rules::Rules>>,
    /// The runaway guard, when it was registered before these hooks were made.
    runaway: Option<Arc<crate::runaway::Runaway>>,
    services: Arc<Services>,
}

#[async_trait]
impl AgentHooks for BusHooks {
    async fn turn_start(&self, agent: &AgentInfo) -> Result<(), String> {
        match self
            .bus
            .emit(&mut Event::TurnStart { agent: agent.clone() })
            .await
        {
            Outcome::Block { reason } => Err(reason),
            Outcome::Continue => Ok(()),
        }
    }

    async fn turn_end(&self, agent: &AgentInfo) {
        self.bus.emit(&mut Event::TurnEnd { agent: agent.clone() }).await;
    }

    async fn context(&self, agent: &AgentInfo, messages: &mut Vec<Message>) {
        let mut event = Event::Context {
            agent: agent.clone(),
            messages: std::mem::take(messages),
        };
        self.bus.emit(&mut event).await;
        if let Event::Context {
            messages: rewritten, ..
        } = event
        {
            *messages = rewritten;
        }
    }

    async fn before_request(&self, agent: &AgentInfo, request: &mut Request) {
        let prompt = self.prompt.prompt(agent).await;
        request.system = prompt.system;
        request.tail = prompt.tail;
        let mut event = Event::BeforeRequest {
            agent: agent.clone(),
            request: std::mem::take(request),
        };
        self.bus.emit(&mut event).await;
        if let Event::BeforeRequest { request: changed, .. } = event {
            *request = changed;
        }
        if let Some(ledger) = self.services.get::<CacheLedger>() {
            ledger.sent(agent);
        }
    }

    fn response(&self, agent: &AgentInfo, request: &Arc<Request>, chain: &[ModelSpec], message: &Message) {
        if let Some(ledger) = self.services.get::<CacheLedger>()
            && let Some(miss) = ledger.answered(agent, request, chain, message)
        {
            crate::cache::report_miss(&ledger, &self.services, &miss);
        }
    }

    async fn stop(&self, agent: &AgentInfo, stop_hook_active: bool) -> Option<String> {
        match self
            .bus
            .emit(&mut Event::Stop {
                agent: agent.clone(),
                stop_hook_active,
            })
            .await
        {
            Outcome::Block { reason } => Some(reason),
            Outcome::Continue => None,
        }
    }

    async fn pre_compact(&self, agent: &AgentInfo, manual: bool) -> Option<String> {
        match self
            .bus
            .emit(&mut Event::PreCompact {
                agent: agent.clone(),
                manual,
            })
            .await
        {
            Outcome::Block { reason } => Some(reason),
            Outcome::Continue => None,
        }
    }

    async fn post_compact(&self, agent: &AgentInfo) {
        self.bus
            .emit(&mut Event::PostCompact { agent: agent.clone() })
            .await;
    }

    fn stream_text(&self, agent: &AgentInfo, so_far: &str) -> Option<String> {
        let rule = self
            .rules
            .as_ref()
            .and_then(|rules| rules.stream_text(agent, so_far));
        rule.or_else(|| self.runaway.as_ref()?.note(so_far, false))
    }

    fn stream_thinking(&self, agent: &AgentInfo, so_far: &str) -> Option<String> {
        let rule = self
            .rules
            .as_ref()
            .and_then(|rules| rules.stream_thinking(agent, so_far));
        rule.or_else(|| self.runaway.as_ref()?.note(so_far, true))
    }

    async fn out_of_allowance(
        &self,
        agent: &AgentInfo,
        chain: &[ModelSpec],
        error: &azazel_ai::AiError,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> azazel_agent::Recovery {
        // Looked up when it happens: whoever runs the agents registers it after these hooks
        // were made.
        match self.services.get::<azazel_agent::OnLimit>() {
            Some(handler) => handler.0.out_of_allowance(agent, chain, error, cancel).await,
            None => azazel_agent::Recovery::Fail,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use azazel_agent::{Access, ToolCtx};
    use serde_json::json;

    use super::*;

    fn ctx() -> ExtCtx {
        ExtCtx {
            services: Arc::new(Services::new()),
            cwd: PathBuf::from("."),
            session_id: "s".into(),
        }
    }

    fn agent() -> AgentInfo {
        AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id: "s".into(),
        }
    }

    struct Named(&'static str);

    #[async_trait]
    impl Tool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "test"
        }
        fn schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn access(&self, _: &Value) -> Access {
            Access::Read
        }
        fn subjects(&self, _: &Value) -> Vec<String> {
            Vec::new()
        }
        async fn call(&self, _: Value, _: &ToolCtx) -> ToolOutput {
            ToolOutput::text(self.0)
        }
    }

    /// Appends its tag to the tool input, and blocks when told to.
    struct Tagger {
        tag: &'static str,
        block: bool,
        seen: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl EventHandler for Tagger {
        async fn handle(&self, ev: &mut Event, ctx: &ExtCtx) -> Outcome {
            self.seen.lock().unwrap().push(self.tag);
            assert_eq!(ctx.session_id, "s");
            if let Event::ToolCall { input, .. } = ev {
                input["tags"].as_array_mut().unwrap().push(json!(self.tag));
            }
            if self.block {
                Outcome::Block {
                    reason: format!("{} says no", self.tag),
                }
            } else {
                Outcome::Continue
            }
        }
    }

    struct Ext {
        id: &'static str,
        tool: &'static str,
        block: bool,
        seen: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Extension for Ext {
        fn id(&self) -> &str {
            self.id
        }
        fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
            r.tool(Arc::new(Named(self.tool)));
            r.tool(Arc::new(Named("shared")));
            r.on(
                EventKind::ToolCall,
                Arc::new(Tagger {
                    tag: self.id,
                    block: self.block,
                    seen: self.seen.clone(),
                }),
            );
            Ok(())
        }
    }

    fn tool_call() -> Event {
        Event::ToolCall {
            agent: agent(),
            call_id: "c".into(),
            tool: "x".into(),
            input: json!({"tags": []}),
            skip_prompt: false,
        }
    }

    #[tokio::test]
    async fn handlers_run_in_registration_order_mutate_the_event_and_first_block_stops_the_chain() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let registry = Registry::new(ctx());
        for (id, block) in [("one", false), ("two", true), ("three", false)] {
            registry
                .load(&Ext {
                    id,
                    tool: id,
                    block,
                    seen: seen.clone(),
                })
                .unwrap();
        }
        let mut event = tool_call();
        assert_eq!(
            registry.bus.emit(&mut event).await,
            Outcome::Block {
                reason: "two says no".into()
            }
        );
        assert_eq!(*seen.lock().unwrap(), ["one", "two"]);
        let Event::ToolCall { input, .. } = event else {
            panic!()
        };
        assert_eq!(input, json!({"tags": ["one", "two"]}));
        assert_eq!(registry.bus.emit(&mut Event::SessionEnd).await, Outcome::Continue);
    }

    #[tokio::test]
    async fn unload_removes_an_extensions_tools_and_handlers_only() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let registry = Registry::new(ctx());
        registry
            .load(&Ext {
                id: "one",
                tool: "alpha",
                block: true,
                seen: seen.clone(),
            })
            .unwrap();
        registry
            .load(&Ext {
                id: "two",
                tool: "beta",
                block: false,
                seen: seen.clone(),
            })
            .unwrap();
        let names = |r: &Registry| {
            r.tools
                .tools(&agent())
                .iter()
                .map(|t| t.name().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&registry), ["alpha", "shared", "beta"]);

        registry.unload("one");
        assert_eq!(names(&registry), ["beta", "shared"]);
        assert_eq!(registry.bus.emit(&mut tool_call()).await, Outcome::Continue);
        assert_eq!(*seen.lock().unwrap(), ["two"]);

        registry.tools.select(None, &["shared".into()]).unwrap();
        assert_eq!(names(&registry), ["beta"]);
        assert!(registry.tools.get(&agent(), "shared").is_none());
        registry.tools.select(Some(&["s*".into()]), &[]).unwrap();
        assert_eq!(names(&registry), ["shared"]);
        assert_eq!(registry.tools.all_names(&agent()), ["beta", "shared"]);
        assert!(registry.tools.select(Some(&["[".into()]), &[]).is_err());
    }

    fn tool_ctx() -> ToolCtx {
        ToolCtx {
            agent: agent(),
            agent_id: "lead".into(),
            session_id: "s".into(),
            call_id: "c".into(),
            cwd: PathBuf::from("."),
            cancel: tokio_util::sync::CancellationToken::new(),
            update: azazel_agent::UpdateSink::default(),
            services: Arc::new(Services::new()),
        }
    }

    /// Answers with the input it was called with.
    struct Echo(&'static str);

    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn access(&self, _: &Value) -> Access {
            Access::Write
        }
        fn subjects(&self, input: &Value) -> Vec<String> {
            vec![input["path"].as_str().unwrap_or_default().to_string()]
        }
        async fn call(&self, input: Value, _: &ToolCtx) -> ToolOutput {
            ToolOutput::text(input.to_string())
        }
    }

    /// Adds its tag to the input's `trail` on the way in and to the output on the way out;
    /// refuses the call when the input says `refuse: <tag>`.
    struct Trail(&'static str);

    #[async_trait]
    impl ToolWrap for Trail {
        async fn before(&self, input: &mut Value, _: &ToolCtx) -> Result<(), ToolOutput> {
            if input["refuse"] == self.0 {
                return Err(ToolOutput::error(format!("{} refused", self.0)));
            }
            input["trail"].as_array_mut().unwrap().push(json!(self.0));
            Ok(())
        }

        async fn after(&self, _: &Value, output: &mut ToolOutput, _: &ToolCtx) {
            *output = ToolOutput {
                content: vec![azazel_ai::Block::text(format!(
                    "{} <{}",
                    output.text_content(),
                    self.0
                ))],
                ..output.clone()
            };
        }
    }

    /// Registers a replacement for `shared`, or a wrap around it.
    struct Override {
        id: &'static str,
        replace: bool,
    }

    impl Extension for Override {
        fn id(&self) -> &str {
            self.id
        }
        fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
            if self.replace {
                r.replace_tool(Arc::new(Echo("shared")));
                r.replace_tool(Arc::new(Echo("extra")));
            } else {
                r.wrap_tool("shared", Arc::new(Trail(self.id)));
            }
            Ok(())
        }
    }

    fn listed(registry: &Registry, name: &str) -> ToolListing {
        let listing = registry.tools.listing(&agent());
        listing.into_iter().find(|tool| tool.name == name).unwrap()
    }

    #[tokio::test]
    async fn a_replacement_takes_the_place_of_the_tool_and_leaves_with_its_extension() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let registry = Registry::new(ctx());
        let first = Ext {
            id: "one",
            tool: "alpha",
            block: false,
            seen,
        };
        registry.load(&first).unwrap();
        registry
            .load(&Override {
                id: "over",
                replace: true,
            })
            .unwrap();

        // The replacement stands where the tool stood; one that replaced nothing comes last.
        let tools = registry.tools.tools(&agent());
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(names, ["alpha", "shared", "extra"]);
        let shared = registry.tools.get(&agent(), "shared").unwrap();
        assert_eq!(shared.description(), "echo");
        assert_eq!(
            shared
                .call(json!({"path": "a"}), &tool_ctx())
                .await
                .text_content(),
            r#"{"path":"a"}"#
        );
        let listing = listed(&registry, "shared");
        assert_eq!(listing.owner, "over");
        assert!(listing.replacement);
        assert_eq!(listing.replaces.as_deref(), Some("one"));
        assert_eq!(listed(&registry, "extra").replaces, None);
        assert!(!listed(&registry, "alpha").replacement);
        assert_eq!(registry.tools.all_names(&agent()), ["alpha", "extra", "shared"]);
        assert_eq!(
            registry.tools.replacements(&agent()),
            [
                ("over".to_string(), "shared".to_string()),
                ("over".to_string(), "extra".to_string())
            ]
        );

        // The selection goes by name, whoever has it.
        registry.tools.select(None, &["shared".into()]).unwrap();
        assert!(registry.tools.get(&agent(), "shared").is_none());
        registry.tools.select(None, &[]).unwrap();

        // A replacement registered later wins over an earlier one.
        registry.tools.replace(
            "late",
            "late",
            Arc::new(vec![Arc::new(Named("shared")) as Arc<dyn Tool>]),
        );
        assert_eq!(listed(&registry, "shared").owner, "late");
        assert_eq!(listed(&registry, "shared").replaces.as_deref(), Some("one"));
        registry.tools.remove_owner("late");

        registry.unload("over");
        let shared = registry.tools.get(&agent(), "shared").unwrap();
        assert_eq!(shared.description(), "test");
        assert_eq!(listed(&registry, "shared").owner, "one");
        assert!(registry.tools.get(&agent(), "extra").is_none());
    }

    #[tokio::test]
    async fn wraps_run_around_whichever_tool_has_the_name_outermost_first() {
        let registry = Registry::new(ctx());
        registry.tools.add("core", Arc::new(Echo("shared")));
        for id in ["outer", "inner"] {
            registry.load(&Override { id, replace: false }).unwrap();
        }
        let shared = registry.tools.get(&agent(), "shared").unwrap();
        // The wrapped tool is still the tool as far as permissions go.
        assert_eq!(shared.access(&json!({})), Access::Write);
        assert_eq!(shared.subjects(&json!({"path": "a.txt"})), ["a.txt"]);

        let output = shared.call(json!({"trail": []}), &tool_ctx()).await;
        assert_eq!(
            output.text_content(),
            r#"{"trail":["outer","inner"]} <inner <outer"#
        );

        // A wrap that answers the call keeps the tool and the wraps inside it from running.
        let refused = shared
            .call(json!({"trail": [], "refuse": "inner"}), &tool_ctx())
            .await;
        assert!(refused.is_error);
        assert_eq!(refused.text_content(), "inner refused <outer");

        assert_eq!(listed(&registry, "shared").wrapped_by, ["outer", "inner"]);
        assert_eq!(
            registry.tools.wraps(),
            [
                ("outer".to_string(), "shared".to_string()),
                ("inner".to_string(), "shared".to_string())
            ]
        );
        // The wraps stay around a replacement.
        registry.tools.replace(
            "late",
            "late",
            Arc::new(vec![Arc::new(Named("shared")) as Arc<dyn Tool>]),
        );
        let replaced = registry.tools.get(&agent(), "shared").unwrap();
        let output = replaced.call(json!({"trail": []}), &tool_ctx()).await;
        assert_eq!(output.text_content(), "shared <inner <outer");

        registry.unload("inner");
        assert_eq!(listed(&registry, "shared").wrapped_by, ["outer"]);
        registry.unload("outer");
        let output = registry
            .tools
            .get(&agent(), "shared")
            .unwrap()
            .call(json!({"trail": []}), &tool_ctx())
            .await;
        assert_eq!(output.text_content(), "shared");
    }

    #[test]
    fn the_agent_hooks_watch_a_reply_for_loops_only_when_the_guard_is_registered() {
        let registry = Registry::new(ctx());
        let looped = "The same sentence about the same thing, said again and again without end. ".repeat(12);
        let unwatched = registry.agent_hooks();
        assert_eq!(unwatched.stream_text(&agent(), &looped), None);

        registry.services.insert(Arc::new(crate::runaway::Runaway));
        let watched = registry.agent_hooks();
        let note = watched.stream_text(&agent(), &looped).unwrap();
        // Named like a rule, so the stop is reported as one.
        assert!(
            note.starts_with("<rule name=\"runaway\" reason=\"repetition\">"),
            "{note}"
        );
        let thought = watched.stream_thinking(&agent(), &looped).unwrap();
        assert!(thought.contains("Your thinking repeated"), "{thought}");
        assert_eq!(watched.stream_text(&agent(), "A short and ordinary reply."), None);
    }
}
