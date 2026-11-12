//! The extension surface: what an extension can register, and the event bus its handlers hang on.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};

use async_trait::async_trait;
use azazel_agent::{AgentHooks, AgentInfo, Services, Tool, ToolOutput, ToolSource};
use azazel_ai::{Block, Message, ModelSpec, Provider, Request, SystemPart};
use serde_json::Value;

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

/// Every tool available to agents: registered tools plus dynamic sources, minus disabled names.
/// The first tool registered under a name wins.
#[derive(Default)]
pub struct ToolRegistry {
    tools: RwLock<Vec<(String, Arc<dyn Tool>)>>,
    sources: RwLock<Vec<(String, Arc<dyn ToolSource>)>>,
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
    /// Id of the extension that registered it.
    pub owner: String,
    /// False when `[tools] disabled` or the tool flags leave it out.
    pub offered: bool,
}

fn glob_set(patterns: &[String]) -> Result<globset::GlobSet, String> {
    let mut set = globset::GlobSetBuilder::new();
    for pattern in patterns {
        set.add(globset::Glob::new(pattern).map_err(|e| format!("tool pattern `{pattern}`: {e}"))?);
    }
    set.build().map_err(|e| e.to_string())
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

    /// Offers only the tools `allowed` matches (all when `None`), minus those `disabled` matches.
    pub fn select(&self, allowed: Option<&[String]>, disabled: &[String]) -> Result<(), String> {
        let selection = Selection {
            allowed: allowed.map(glob_set).transpose()?,
            disabled: glob_set(disabled)?,
        };
        *self.selection.write().unwrap_or_else(PoisonError::into_inner) = selection;
        Ok(())
    }

    /// Names of every registered tool and every tool a source offers now, before selection.
    pub fn all_names(&self, agent: &AgentInfo) -> Vec<String> {
        let mut names: Vec<String> = self
            .tools
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(_, tool)| tool.name().to_string())
            .collect();
        let sources: Vec<Arc<dyn ToolSource>> = self
            .sources
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(_, s)| s.clone())
            .collect();
        for source in sources {
            names.extend(source.tools(agent).iter().map(|tool| tool.name().to_string()));
        }
        names.sort();
        names.dedup();
        names
    }

    /// Every tool agents could be offered, in registration order, with the extension it comes
    /// from and whether the selection offers it. A name registered twice is listed once, under
    /// the extension that wins.
    pub fn listing(&self, agent: &AgentInfo) -> Vec<ToolListing> {
        let selection = self.selection.read().unwrap_or_else(PoisonError::into_inner);
        let mut out: Vec<ToolListing> = Vec::new();
        let mut add = |owner: &str, name: &str| {
            if !out.iter().any(|listed| listed.name == name) {
                out.push(ToolListing {
                    name: name.to_string(),
                    owner: owner.to_string(),
                    offered: selection.offers(name),
                });
            }
        };
        for (owner, tool) in self.tools.read().unwrap_or_else(PoisonError::into_inner).iter() {
            add(owner, tool.name());
        }
        let sources: Vec<(String, Arc<dyn ToolSource>)> = self
            .sources
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for (owner, source) in sources {
            for tool in source.tools(agent) {
                add(&owner, tool.name());
            }
        }
        out
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
    }

    pub fn get(&self, agent: &AgentInfo, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools(agent).into_iter().find(|t| t.name() == name)
    }
}

impl ToolSource for ToolRegistry {
    fn tools(&self, agent: &AgentInfo) -> Vec<Arc<dyn Tool>> {
        let selection = self.selection.read().unwrap_or_else(PoisonError::into_inner);
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        let mut add = |tool: Arc<dyn Tool>| {
            let name = tool.name();
            if selection.offers(name) && !out.iter().any(|t| t.name() == name) {
                out.push(tool);
            }
        };
        for (_, tool) in self.tools.read().unwrap_or_else(PoisonError::into_inner).iter() {
            add(tool.clone());
        }
        let sources: Vec<Arc<dyn ToolSource>> = self
            .sources
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(_, s)| s.clone())
            .collect();
        for source in sources {
            for tool in source.tools(agent) {
                add(tool);
            }
        }
        out
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
    /// Ids of the extensions loaded so far, in load order.
    loaded: RwLock<Vec<String>>,
}

/// The registry as a service, for extensions that load or unload others while the process runs
/// (a script host reloading its scripts).
pub struct RegistryHandle(pub std::sync::Weak<Registry>);

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
        request.system = self.prompt.render(agent).await;
        let mut event = Event::BeforeRequest {
            agent: agent.clone(),
            request: std::mem::take(request),
        };
        self.bus.emit(&mut event).await;
        if let Event::BeforeRequest { request: changed, .. } = event {
            *request = changed;
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
}
