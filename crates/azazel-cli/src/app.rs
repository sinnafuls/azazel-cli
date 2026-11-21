//! Assembles the process-wide pieces (config, providers, extensions, permissions) and opens
//! sessions on top of them. Every mode starts here.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use azazel_agent::{
    Agent, AgentCmd, AgentConfig, AgentHandle, AgentInfo, ArtifactSink, Limits, Resume, Services, UserInput,
};
use azazel_ai::providers::http::{HttpProvider, http_client};
use azazel_ai::providers::presets;
use azazel_ai::{
    Block, Catalog, Credentials, ModelSpec, Provider, RetryPolicy, Router, StallLimits, ThinkingLevel,
};
use azazel_core::agents::{self, AgentDef, LeadAgent};
use azazel_core::cache::{CacheLedger, Warmer};
use azazel_core::checkpoint::Checkpoints;
use azazel_core::commands::{CustomCommandsExt, expand_mentions};
use azazel_core::config::{SessionResume, set_override};
use azazel_core::context::ContextExt;
use azazel_core::hooks::HooksExt;
use azazel_core::limits::{LimitsExt, Spend};
use azazel_core::media::{MAX_IMAGE_SIDE, image_block};
use azazel_core::names::{Namer, SessionNamesExt};
use azazel_core::packages::Packages;
use azazel_core::permissions::plan::PermissionsExt;
use azazel_core::permissions::shell::guard_env;
use azazel_core::process::{AgentEnv, Shell};
use azazel_core::prompt::BasePromptExt;
use azazel_core::rules::{self, Rules, RulesExt};
use azazel_core::session::InterruptedRun;
use azazel_core::side::{Side, SideCosts};
use azazel_core::skills::{SkillGrantsExt, SkillsExt, standard_roots};
use azazel_core::{
    ArtifactStore, Command, CommandHandler, CommandOutcome, Config, EntryKind, Event, ExtCtx, ExtError,
    Extension, FileTracker, Headless, Level, LoadOptions, Outcome, OutputRedactor, Paths, PermissionGate,
    PermissionMode, PermissionState, Registrar, Registry, SessionStore, StartSource, ToolCaller, Ui,
};
use azazel_team::{
    Advisor, Checker, Coordinator, GoalExt, GoalSettings, Goals, LeadTools, Team, TeamDeps, TeamExt, Typed,
};
use azazel_tools::CoreToolsExt;

use crate::cli::{Cli, Discovery};
use crate::resources::RunResources;
use crate::running::{self, Lock};
use crate::telemetry::{Telemetry, Watch};

pub const NO_MODEL: &str = "no model configured: run \"azazel init\" or pass --model provider/id";

/// Everything that outlives a session.
pub struct App {
    pub paths: Paths,
    pub config: Config,
    /// Whether the project's configuration may run code and widen permissions.
    pub trusted: bool,
    /// Project settings that were skipped because the project is not trusted.
    pub ignored_settings: Vec<String>,
    pub registry: Arc<Registry>,
    pub router: Arc<Router>,
    pub credentials: Arc<Credentials>,
    pub permissions: Arc<PermissionState>,
    pub shell: Shell,
    /// What a new session's lead starts with.
    pub chain: Vec<ModelSpec>,
    /// The thinking level `--thinking` chose; `None` leaves it to the model's `:level`.
    pub thinking: Option<ThinkingLevel>,
    /// How the configuration was loaded, for reading it again (`/reload`).
    pub load_options: LoadOptions,
    /// What the command line added for this run (`--extension`, `--skill`, `--mcp-config`).
    pub run: RunResources,
    /// The only provider model names resolve to in this run (`--provider`).
    pub provider: Option<String>,
    /// Hooks are off for this run (`--no-hooks`, `AZAZEL_HOOKS_DISABLED`).
    pub hooks_disabled: bool,
    /// Whether the lead only plans and delegates (`/coordinator`).
    pub coordinator: Arc<Coordinator>,
    /// The typed result this run owes (`--json-schema`).
    pub report: Option<Arc<Typed>>,
    /// What the lead is offered: every tool, fewer in coordinator mode, `report` for a typed run.
    lead_tools: Arc<LeadTools>,
    /// `--coordinator` was given and no session has been opened yet.
    coordinator_forced: AtomicBool,
    /// The primary agent the lead is: its identity, model, limits and colour.
    pub lead_agent: Arc<LeadAgent>,
    /// `--agent`, until the first session has been opened.
    agent_flag: Mutex<Option<String>>,
    /// What the lead was on before its primary agent's own model or thinking level.
    before: Arc<Mutex<Before>>,
    /// A `--no-session` run: its transcripts are deleted when it ends.
    ephemeral: bool,
    /// `--model` was given: it wins over what a reopened session last used, as `--thinking`
    /// (`thinking`) does.
    model_flag: bool,
    /// The session mode changes are recorded in.
    current: Arc<RwLock<Option<Arc<SessionStore>>>>,
    /// This process is a background session (`azazel host`): it locks the session it has open.
    background: bool,
    /// The lock on the open session, by its id, for when the same session is opened again.
    session_lock: Mutex<Option<(String, Weak<Lock>)>>,
    /// Trace export, when `[telemetry]` asks for it.
    telemetry: Option<Arc<Telemetry>>,
}

/// One open session and its lead agent.
pub struct Active {
    pub session: Arc<SessionStore>,
    pub lead: AgentHandle,
    pub checkpoints: Arc<Checkpoints>,
    /// Whether the session existed before it was opened.
    pub resumed: bool,
    /// Something about how it was opened that the user should be told.
    pub notice: Option<String>,
    /// Keeps other processes out of the session while a background session has it open.
    _lock: Option<Arc<Lock>>,
    /// The spans of this session on their way to a tracing backend.
    trace: Option<Watch>,
}

pub enum Open {
    New,
    /// An existing session file.
    File(PathBuf),
    /// A store already in memory (a fork, or a session whose leaf was moved).
    Store(Arc<SessionStore>),
}

/// What the lead was on before a primary agent with a model or a thinking level of its own took
/// over: switching to an agent without one gives it back.
#[derive(Default)]
struct Before {
    chain: Option<Vec<ModelSpec>>,
    thinking: Option<Option<ThinkingLevel>>,
}

/// What the lead takes up as it becomes a primary agent: a model chain and a thinking level
/// where they change, and what the user should know.
#[derive(Debug, Default, PartialEq)]
struct Follow {
    chain: Option<Vec<ModelSpec>>,
    /// The level to set, where it changes; `Some(None)` goes back to the model's own.
    thinking: Option<Option<ThinkingLevel>>,
    notice: Option<String>,
}

/// The model chain and thinking level of a lead on `chain` and `thinking` that becomes `def`:
/// the agent's own, with what the lead was on kept in `before`; that again when the agent has
/// none. What the command line chose (`pinned`: `--model`, `--thinking`) stays.
fn follow(
    def: &AgentDef,
    chain: &[ModelSpec],
    thinking: Option<ThinkingLevel>,
    before: &mut Before,
    router: &Router,
    pinned: (bool, bool),
) -> Follow {
    let mut notice = None;
    let own = def.model.clone().filter(|own| {
        let usable = !router.resolve(own).is_empty();
        if !usable {
            let names: Vec<String> = own.iter().map(ToString::to_string).collect();
            notice = Some(format!(
                "agent {} names no model that can be used ({}); the lead stays on its model",
                def.name,
                names.join(", ")
            ));
        }
        usable
    });
    let chain = match own {
        _ if pinned.0 => None,
        Some(own) => {
            before.chain.get_or_insert_with(|| chain.to_vec());
            (own != chain).then_some(own)
        }
        None => before.chain.take().filter(|back| back != chain),
    };
    let thinking = match def.thinking {
        _ if pinned.1 => None,
        Some(own) => {
            before.thinking.get_or_insert(thinking);
            (Some(own) != thinking).then_some(Some(own))
        }
        None => before.thinking.take().filter(|back| *back != thinking),
    };
    Follow {
        chain,
        thinking,
        notice,
    }
}

/// The limits of `def` on the lead: kept in plan mode or not, and the tools it grants.
fn lead_limits(permissions: &PermissionState, def: &AgentDef) {
    permissions.set_limits(azazel_team::LEAD, def.plan_mode, def.tools.clone());
}

/// What a switch of the lead's primary agent changes on the running lead.
struct Switched {
    permissions: Arc<PermissionState>,
    services: Weak<Services>,
    router: Arc<Router>,
    before: Arc<Mutex<Before>>,
    pinned: (bool, bool),
}

impl Switched {
    fn apply(&self, def: &AgentDef) {
        lead_limits(&self.permissions, def);
        let Some(services) = self.services.upgrade() else {
            return;
        };
        let Some(lead) = services.get::<Team>().and_then(|team| team.lead()) else {
            return;
        };
        let status = lead.status();
        let follow = follow(
            def,
            &status.chain,
            status.thinking_set,
            &mut self.before.lock().unwrap_or_else(PoisonError::into_inner),
            &self.router,
            self.pinned,
        );
        if let Some(chain) = follow.chain {
            lead.send(AgentCmd::SetModel(chain));
        }
        if let Some(level) = follow.thinking {
            lead.send(AgentCmd::SetThinking(level));
        }
        if let (Some(notice), Some(ui)) = (follow.notice, services.get::<Ui>()) {
            ui.0.notify(Level::Warn, &notice);
        }
    }
}

/// The most recent session started in `folder` (the launch folder; any of the project's when
/// `None`), for `-c`, or for the interactive UI when `[session] resume = "last"` and neither
/// `--new` nor `--no-session` is given. `None` when neither asks for it, or the setting does and
/// there is none: a new session starts then.
fn latest_session(
    dir: &Path,
    folder: Option<&Path>,
    cli: &Cli,
    resume: SessionResume,
) -> Result<Option<Open>> {
    let by_default = resume == SessionResume::Last && cli.interactive() && !cli.new && !cli.no_session;
    if !(cli.resume_latest || by_default) {
        return Ok(None);
    }
    let mut sessions = SessionStore::list(dir);
    if let Some(folder) = folder {
        sessions = azazel_core::session::in_folder(sessions, folder);
    }
    // An archived session (`-p --archived`) is not one to go on with.
    match sessions.into_iter().find(|s| !s.archived) {
        Some(latest) => Ok(Some(Open::File(latest.path))),
        None if cli.resume_latest && folder.is_some() => {
            bail!(
                "no session was started in this folder to continue; `--session <id>` opens one of the project's"
            )
        }
        None if cli.resume_latest => bail!("this project has no session to continue"),
        None => Ok(None),
    }
}

/// The id of the background session that has the session file `how` names open, when one does.
fn background_host(home: &Path, how: &Open) -> Option<String> {
    let Open::File(path) = how else { return None };
    let id = SessionStore::open(path).ok()?.id();
    running::host_of(home, &id)
}

/// A mistake in how azazel was called; the run exits with status 2.
#[derive(Debug)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// What `--theme` names, as `tui.theme` takes it: the name of a built-in theme or of one in
/// `~/.azazel/themes`, or the full path of a theme file.
fn theme_flag(value: &str, paths: &Paths) -> Result<String, UsageError> {
    let file = paths.cwd.join(value);
    let name = match azazel_tui::Theme::builtin(value) {
        None if file.is_file() => azazel_core::paths::canonical(&file).display().to_string(),
        _ => value.to_string(),
    };
    match crate::tui::named_theme(paths, &name, None).1 {
        Some(problem) => Err(UsageError(format!("--theme: {problem}"))),
        None => Ok(name),
    }
}

pub(crate) fn config_overrides(cli: &Cli, paths: &Paths) -> Result<toml::Table> {
    let mut overrides = toml::Table::new();
    for assignment in &cli.set {
        set_override(&mut overrides, assignment).map_err(|e| UsageError(format!("--set {e}")))?;
    }
    if let Some(theme) = &cli.theme {
        let theme = toml::Value::String(theme_flag(theme, paths)?);
        let tui = overrides
            .entry("tui")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        match tui.as_table_mut() {
            Some(tui) => tui.insert("theme".to_string(), theme),
            None => return Err(UsageError("--set tui: `tui` is a table".to_string()).into()),
        };
    }
    let mut assign = |text: String| set_override(&mut overrides, &text).map_err(|e| anyhow!(e));
    if let Some(mode) = &cli.mode {
        let mode: PermissionMode = mode
            .parse()
            .map_err(|e: String| UsageError(format!("--mode: {e}")))?;
        assign(format!("permissions.mode=\"{mode}\""))?;
    }
    if let Some(turns) = cli.max_turns {
        assign(format!("limits.max_turns={turns}"))?;
    }
    if let Some(cost) = cli.max_cost {
        if !cost.is_finite() || cost < 0.0 {
            return Err(UsageError("--max-cost must be a positive amount".to_string()).into());
        }
        assign(format!("limits.max_cost_usd={cost:?}"))?;
    }
    Ok(overrides)
}

/// Provider ids `--model` may name, and the ones it may pick from when the name is partial:
/// those that need no key or have one. Providers that are not HTTP endpoints (cloud SDKs, scripts,
/// extensions) find their own credentials and count as usable.
pub fn known_and_usable(
    provider_ids: Vec<String>,
    endpoints: &[presets::ProviderConfig],
    credentials: &Credentials,
    everyone: bool,
) -> (Vec<String>, Vec<String>) {
    let usable = provider_ids
        .iter()
        .filter(|id| {
            everyone
                || endpoints
                    .iter()
                    .find(|p| p.id == **id)
                    .is_none_or(|p| crate::models::usable(credentials, p))
        })
        .cloned()
        .collect();
    (provider_ids, usable)
}

/// The provider lists of [`known_and_usable`] cut down to `only` (`--provider`), when it is given.
fn only_provider(
    (mut known, mut usable): (Vec<String>, Vec<String>),
    only: Option<&str>,
) -> (Vec<String>, Vec<String>) {
    if let Some(only) = only {
        known.retain(|id| id == only);
        usable.retain(|id| id == only);
    }
    (known, usable)
}

/// Why a model name did not resolve, with the reminder that `--provider` narrowed the search.
fn provider_note(why: String, only: Option<&str>) -> String {
    match only {
        Some(only) => format!("{why} (--provider {only} leaves only that provider's models)"),
        None => why,
    }
}

/// `--tools`, `--no-tools` and `--disallowed-tools` on top of `[tools] disabled`. A plain name
/// (not a glob) that no tool has is a usage error; MCP tools (`mcp__...`) connect later and are
/// not checked.
fn select_tools(cli: &Cli, config: &Config, registry: &Registry) -> Result<()> {
    let allowed = if cli.no_tools {
        Some(Vec::new())
    } else {
        cli.tools.clone()
    };
    let mut disabled = config.tools.disabled.clone();
    disabled.extend(cli.disallowed_tools.iter().cloned());
    let lead = AgentInfo {
        id: "lead".into(),
        def: "lead".into(),
        parent: None,
        depth: 0,
        session_id: String::new(),
    };
    let known = registry.tools.all_names(&lead);
    for name in allowed.iter().flatten().chain(&cli.disallowed_tools) {
        let plain = !name.contains(['*', '?', '[', '{']);
        if plain && !name.starts_with("mcp__") && !known.contains(name) {
            return Err(UsageError(format!(
                "no tool named `{name}`; the tools are: {}",
                known.join(", ")
            ))
            .into());
        }
    }
    registry
        .tools
        .select(allowed.as_deref(), &disabled)
        .map_err(|e| UsageError(e).into())
}

/// Whether `eval` offers Luau: `[tools.eval] luau` and a build that has the Luau engine.
pub(crate) fn eval_luau(eval: &azazel_core::config::EvalConfig) -> bool {
    eval.luau && cfg!(feature = "lua")
}

/// Whether `name` keeps its full schema in code-first mode: the core tools, `patch` where it takes
/// the place of `edit`, and the tools that end a goal, a plan or a typed run, which an agent must
/// be able to call itself.
fn code_first_core(name: &str) -> bool {
    azazel_core::config::CODE_FIRST_CORE.contains(&name)
        || name == "patch"
        || name == "plan_submit"
        || name == azazel_team::REPORT_TOOL
        || azazel_team::goal::GOAL_TOOLS.contains(&name)
}

/// The tools `[tools] code_first` hides: for the lead and for sub-agents whose definition has no
/// `tools:` list, everything but the core; an explicit list is its author's choice and stays.
fn code_first_hiding(
    defs: Arc<azazel_core::agents::AgentDefs>,
    lead: Arc<LeadAgent>,
) -> azazel_agent::CodeFirst {
    azazel_agent::CodeFirst(Arc::new(move |agent: &AgentInfo, name: &str| {
        let def = if agent.depth == 0 {
            lead.def()
        } else {
            defs.get(&agent.def)
        };
        def.is_none_or(|def| def.tools.is_none()) && !code_first_core(name)
    }))
}

/// Text given inline, or read from the file named after an `@`.
fn text_or_file(flag: &str, value: &str, cwd: &std::path::Path) -> Result<String> {
    match value.strip_prefix('@') {
        Some(file) => std::fs::read_to_string(cwd.join(file))
            .map_err(|e| UsageError(format!("{flag} @{file}: {e}")).into()),
        None => Ok(value.to_string()),
    }
}

/// The scripted provider, when this is a test build and a script is given.
#[cfg(feature = "test-support")]
fn mock_provider(config: &mut Config) -> Result<Option<Arc<dyn Provider>>> {
    let Some(script) = std::env::var_os("AZAZEL_MOCK_SCRIPT") else {
        return Ok(None);
    };
    let mut provider = azazel_ai::testing::MockProvider::from_script_file(std::path::Path::new(&script))
        .map_err(|e| anyhow!("AZAZEL_MOCK_SCRIPT: {e}"))?;
    if let Some(record) = std::env::var_os("AZAZEL_MOCK_REQUESTS") {
        provider = provider.recording_to(PathBuf::from(record));
    }
    // Scripted runs need no configuration: the lead answers from the `lead` queue.
    config
        .roles
        .entry("default".to_string())
        .or_insert_with(|| vec![ModelSpec::Model(azazel_ai::ModelRef::new("mock", "lead"))]);
    Ok(Some(Arc::new(provider)))
}

#[cfg(not(feature = "test-support"))]
fn mock_provider(_: &mut Config) -> Result<Option<Arc<dyn Provider>>> {
    Ok(None)
}

/// A tool for tests that does nothing for as long as it is told to (`test_wait {"ms": 500}`):
/// a call that is still running when a test kills the process. Offered in a test build when
/// `AZAZEL_MOCK_TOOLS` is set.
#[cfg(feature = "test-support")]
mod mock_tools {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
    use azazel_core::{ExtError, Extension, Registrar};
    use serde_json::{Value, json};

    struct Wait;

    #[async_trait]
    impl Tool for Wait {
        fn name(&self) -> &str {
            "test_wait"
        }

        fn description(&self) -> &str {
            "Waits for `ms` milliseconds."
        }

        fn schema(&self) -> Value {
            json!({ "type": "object", "properties": { "ms": { "type": "integer" } }, "required": ["ms"] })
        }

        fn access(&self, _: &Value) -> Access {
            Access::Exec
        }

        fn subjects(&self, _: &Value) -> Vec<String> {
            Vec::new()
        }

        async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
            let wait = Duration::from_millis(input["ms"].as_u64().unwrap_or(0));
            tokio::select! {
                _ = ctx.cancel.cancelled() => ToolOutput::error("the wait was cut short"),
                _ = tokio::time::sleep(wait) => ToolOutput::text("waited"),
            }
        }
    }

    pub struct MockToolsExt;

    impl Extension for MockToolsExt {
        fn id(&self) -> &str {
            "mock-tools"
        }

        fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
            r.tool(Arc::new(Wait));
            Ok(())
        }
    }
}

/// Stands in for `/memory` in a session that runs without memory.
struct MemoryOffExt {
    why: String,
}

impl Extension for MemoryOffExt {
    fn id(&self) -> &str {
        "memory-off"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.command(Command {
            name: "memory".to_string(),
            description: "What is remembered (memory is turned off in this session)".to_string(),
            argument_hint: None,
            handler: Arc::new(MemoryOff(self.why.clone())),
        });
        Ok(())
    }
}

struct MemoryOff(String);

#[async_trait::async_trait]
impl CommandHandler for MemoryOff {
    async fn run(&self, _: &str, _: &ExtCtx) -> CommandOutcome {
        CommandOutcome::Show(self.0.clone())
    }
}

/// Why this session has no memory, and what turns it on.
fn memory_off_reason(cli: &Cli, config: &Config) -> String {
    if cli.bare {
        "memory is turned off: --bare was given. Start without --bare to turn it on.".to_string()
    } else if cli.no_memory {
        "memory is turned off: --no-memory was given. Start without --no-memory to turn it on.".to_string()
    } else if !cfg!(feature = "memory") {
        "memory is not in this build of azazel.".to_string()
    } else if !config.memory.enabled {
        "memory is turned off: `[memory] enabled = false`. Set it to true (or remove it) and start a new session to turn it on."
            .to_string()
    } else {
        "memory is turned off in this session.".to_string()
    }
}

/// The `mcp.json` files read in the usual places: those of installed packages, then the user's,
/// then, for a trusted project, the project's.
#[cfg(feature = "mcp")]
pub fn mcp_files(paths: &Paths, trusted: bool, package_files: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = package_files.to_vec();
    files.extend(azazel_mcp::config::config_files(paths, trusted));
    files
}

/// The MCP servers `files` define; a later file wins where two define a server of one name.
#[cfg(feature = "mcp")]
pub fn mcp_manager(
    paths: &Paths,
    config: &Config,
    services: Arc<Services>,
    credentials: Arc<Credentials>,
    files: &[PathBuf],
    approve_all: bool,
) -> azazel_mcp::McpManager {
    let mut loaded = azazel_mcp::config::load(files, &paths.project_root, &|name| std::env::var(name).ok());
    // Each server the project defines waits for the user's word; `--trust` gives it for this run.
    let trust = azazel_core::TrustStore::new(paths.trust_file());
    azazel_mcp::config::apply_approvals(&mut loaded, paths, &trust, approve_all);
    let mut roots = vec![paths.cwd.clone()];
    roots.extend(
        config
            .permissions
            .additional_dirs
            .iter()
            .map(|dir| paths.project_root.join(dir)),
    );
    let options = azazel_mcp::Options {
        startup_wait: Duration::from_millis(config.mcp.startup_wait_ms),
        defer_threshold: config.mcp.defer_threshold,
        idle_timeout: (config.mcp.idle_timeout_secs > 0)
            .then(|| Duration::from_secs(config.mcp.idle_timeout_secs)),
        cache_dir: Some(paths.cache_dir().join("mcp")),
    };
    let manager = azazel_mcp::McpManager::new(loaded, services, roots, credentials, options);
    manager.record_approvals(
        trust,
        paths.project_root.clone(),
        azazel_mcp::config::project_files(paths).to_vec(),
    );
    manager
}

/// The credential store of this run: `auth.json`, plus keys from `~/.azazel/.env` and, in a
/// trusted project, from the project's `.env`.
pub fn credentials(paths: &Paths, trusted: bool) -> Credentials {
    let mut files = vec![paths.home.join(".env")];
    if trusted {
        files.push(paths.project_root.join(".env"));
    }
    Credentials::new(paths.auth_file()).with_env_files(files)
}

/// Where the command line says to work.
pub fn discover_paths(cli: &Cli) -> Result<Paths> {
    let cwd = match &cli.cwd {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().context("cannot read the current directory")?,
    };
    if !cwd.is_dir() {
        bail!("{} is not a directory", cwd.display());
    }
    // `--home` is how a process azazel starts for itself is told what `AZAZEL_HOME` told this one.
    let home = match &cli.home {
        Some(home) => Some(home.to_string_lossy().into_owned()),
        None => std::env::var("AZAZEL_HOME").ok(),
    };
    let mut paths = Paths::discover(&cwd, home.as_deref());
    paths.sessions_override = if cli.no_session {
        Some(ephemeral_dir(&paths))
    } else {
        cli.session_dir.as_ref().map(|dir| paths.cwd.join(dir))
    };
    Ok(paths)
}

/// Where a `--no-session` run keeps its transcripts until it ends.
fn ephemeral_dir(paths: &Paths) -> PathBuf {
    paths
        .cache_dir()
        .join("ephemeral")
        .join(std::process::id().to_string())
}

impl App {
    pub async fn build(
        cli: &Cli,
        paths: Paths,
        trusted: bool,
        extensions: &[Box<dyn Extension>],
    ) -> Result<App> {
        let options = LoadOptions {
            trusted,
            overrides: config_overrides(cli, &paths)?,
            env: std::env::vars().collect(),
        };
        let run = RunResources::from_cli(cli, &paths.cwd).map_err(UsageError)?;
        let mut loaded = Config::load(&paths, &options)?;
        // Directories from the command line add to those of the configuration.
        loaded
            .config
            .permissions
            .additional_dirs
            .extend(cli.add_dir.iter().map(|dir| paths.cwd.join(dir)));
        if cli.no_session {
            loaded.config.checkpoints.enabled = false;
        }
        let permission_state = PermissionState::new(&paths, &loaded).map_err(|e| anyhow!(e))?;
        permission_state.set_never_push(cli.never_push);
        let permissions = Arc::new(permission_state);
        let hook_sources = azazel_core::hooks::hook_sources(&loaded.layers);
        let ignored_settings = loaded.ignored;
        let mut config = loaded.config;

        if let Some(name) = &cli.profile {
            crate::profiles::apply(&mut config, name).map_err(|e| UsageError(format!("--profile: {e}")))?;
        }
        let tier = match &cli.tier {
            Some(tier) => crate::routing::parse_tier(tier).map_err(|e| UsageError(format!("--tier: {e}")))?,
            None => None,
        };

        let credentials = Arc::new(
            credentials(&paths, trusted)
                .configured(&config.providers)
                .ordered(&config.accounts.order),
        );
        // Accounts that ran out of allowance in an earlier run are still out in this one.
        credentials.limits().persist(crate::usage::cache_file(&paths));
        let thinking = match &cli.thinking {
            Some(level) => Some(
                level
                    .parse::<ThinkingLevel>()
                    .map_err(|e| UsageError(format!("--thinking: {e}")))?,
            ),
            None => None,
        };

        let services = Arc::new(Services::new());
        services.insert(Arc::new(Ui(Arc::new(Headless))));
        services.insert(Arc::new(crate::rpc::admin::SessionSettings::default()));
        services.insert(Arc::new(azazel_agent::ToolSettings {
            cache_reads: config.tools.cache_reads,
            retries: config.tools.retries,
            call_timeout: (config.tools.call_timeout_secs > 0)
                .then(|| Duration::from_secs(config.tools.call_timeout_secs)),
            ..azazel_agent::ToolSettings::default()
        }));
        services.insert(Arc::new(azazel_agent::Deferral(config.tools.defer.clone())));
        services.insert(Arc::new(config.tools.code_first_listing));
        // Large JSON results lose their noise and, with `tools.digest`, large results become notes;
        // decided once, when the result is produced.
        if let Some(digester) = crate::digest::digester(&config, paths.cache_dir().join("digests")) {
            services.insert(Arc::new(azazel_agent::Shaper(digester)));
        }
        // How long a provider may stay silent; the providers extensions add read it from here.
        let stall = StallLimits::from_secs(config.retry.first_event_secs, config.retry.stall_secs);
        services.insert(Arc::new(stall));
        let registry = Arc::new(Registry::new(ExtCtx {
            services: services.clone(),
            cwd: paths.cwd.clone(),
            session_id: String::new(),
        }));
        registry.publish();
        let shell = Shell::resolve(&config.tools.shell);
        let compat = config.compat.claude;
        let packages = Packages::new(&paths).load();
        if let Some(why) = &packages.problem {
            tracing::warn!("installed packages are not loaded: {why}");
        }
        let mut agent_defs = agents::discover(
            azazel_team::BUILTIN_AGENTS,
            &run.def_roots(&paths, compat, &packages),
        );
        agent_defs.apply_overrides(&config.agents.overrides);
        let agent_defs = Arc::new(agent_defs);
        let lead_agent = Arc::new(
            LeadAgent::new(agent_defs.clone(), &config.agents.default)
                .map_err(|why| anyhow!("[agents] default: {why}"))?,
        );
        let agent_flag = match &cli.agent {
            Some(name) if agent_defs.primary(name).is_none() => {
                return Err(
                    UsageError(format!("--agent: {}", agents::not_primary(&agent_defs, name))).into(),
                );
            }
            other => other.clone(),
        };
        services.insert(lead_agent.clone());
        registry.load(&BasePromptExt {
            cwd: paths.cwd.clone(),
            project_root: paths.project_root.clone(),
            shell: shell.name().to_string(),
            lead: lead_agent.clone(),
            system: cli
                .system_prompt
                .as_deref()
                .map(|value| text_or_file("--system-prompt", value, &paths.cwd))
                .transpose()?,
            append: cli
                .append_system_prompt
                .as_deref()
                .map(|value| text_or_file("--append-system-prompt", value, &paths.cwd))
                .transpose()?,
        })?;
        registry.load(&SessionNamesExt {
            auto_name: config.session.auto_name,
        })?;
        registry.load(&LimitsExt {
            max_cost_usd: config.limits.max_cost_usd,
            warn_cost_usd: config.limits.warn_cost_usd,
        })?;
        registry.services.insert(Arc::new(
            azazel_tools::eval::EvalSettings::from_switches(
                config.tools.eval.python,
                config.tools.eval.javascript,
                eval_luau(&config.tools.eval),
            )
            .with_background_max_secs(config.tools.eval.background_max_secs),
        ));
        registry.load(&CoreToolsExt {
            shell: shell.clone(),
            shell_timeout: Duration::from_secs(config.tools.shell_timeout_secs),
            artifacts_dir: paths.artifacts_dir(),
            read_guard: config.tools.read_guard,
            patch: config.tools.patch,
        })?;
        #[cfg(feature = "test-support")]
        if std::env::var_os("AZAZEL_MOCK_TOOLS").is_some() {
            registry.load(&mock_tools::MockToolsExt)?;
        }
        #[cfg(feature = "lsp")]
        registry.load(&azazel_tools::lsp::LspExt {
            root: paths.project_root.clone(),
            places: azazel_tools::lsp::Places::system(paths.lsp_dir()),
            install: config.lsp.install,
            servers: config.lsp.servers.clone(),
            idle: Duration::from_secs(config.lsp.idle_secs),
            write_severity: config.lsp.write_severity,
        })?;
        #[cfg(feature = "debug")]
        registry.load(&azazel_tools::debug::DebugExt {
            adapters: config.debug.adapters.clone(),
            timeout: Duration::from_secs(config.debug.timeout_secs),
        })?;
        registry.load(&azazel_tools::web::WebExt {
            search: config.web.search.clone(),
            searxng_url: config.web.searxng_url.clone(),
            provider_search: config.web.provider_search,
        })?;
        // What `azazel secrets` stored: the `http` tool and the browser use it, agents never see it.
        let stored = Arc::new(azazel_ai::ServiceStore::new(paths.secrets_file()));
        registry.load(&azazel_tools::web::HttpExt {
            stored: stored.clone(),
            redact: azazel_tui::text::redact,
        })?;
        registry.load(&crate::login::LoginExt {
            providers: crate::login::key_providers(&config).map_err(|e| anyhow!(e))?,
            credentials: credentials.clone(),
        })?;
        registry.load(&crate::settings::SettingsExt {
            providers: presets::resolve(&config.providers).map_err(|e| anyhow!(e))?,
            credentials: credentials.clone(),
            paths: paths.clone(),
            options: options.clone(),
        })?;
        registry.load(&crate::accounts::AccountsExt {
            providers: crate::login::key_providers(&config).map_err(|e| anyhow!(e))?,
            credentials: credentials.clone(),
            home_config: paths.home_config(),
            redact_email: config.usage.redact_email,
        })?;
        registry.load(&crate::usage::UsageExt {
            source: crate::usage::Source::of(&config, credentials.clone(), &paths).map_err(|e| anyhow!(e))?,
            redact_email: config.usage.redact_email,
            paths: paths.clone(),
        })?;
        registry.load(&crate::resets::ResetsExt {
            resets: crate::resets::Resets {
                source: crate::usage::Source::of(&config, credentials.clone(), &paths)
                    .map_err(|e| anyhow!(e))?,
                redact_email: config.usage.redact_email,
            },
            auto_reset: config.usage.auto_reset,
            unattended: cli.background(),
            only: cli.provider.clone(),
            providers: presets::resolve(&config.providers).map_err(|e| anyhow!(e))?,
        })?;
        let endpoints_now = presets::resolve(&config.providers).map_err(|e| anyhow!(e))?;
        let access_credentials = credentials.clone();
        // The router is built after the extensions, so the check asks for it when it is used.
        let access_services = Arc::downgrade(&services);
        registry.load(&crate::routing::RoutingExt {
            book: Arc::new(
                crate::profiles::Book::new(
                    crate::profiles::every_in(&config.profiles.entries, &loaded.layers),
                    config.profiles.disabled.clone(),
                    Arc::new(crate::profiles::access(
                        endpoints_now,
                        access_credentials,
                        move |id| {
                            let router = access_services.upgrade()?.get::<Router>()?;
                            Some(router.provider(id).is_some())
                        },
                    )),
                    cli.profile.clone(),
                )
                .with_on_limit(&config.profiles.on_limit)
                .with_on_limit_now({
                    let (paths, options) = (paths.clone(), options.clone());
                    Arc::new(move || {
                        Config::load(&paths, &options)
                            .ok()
                            .map(|loaded| loaded.config.profiles.on_limit)
                    })
                })
                .with_project_disables(crate::profiles::project_sets_disabled(&loaded.layers)),
            ),
            home_config: paths.home_config(),
            on_reset: config.profiles.on_reset,
            started_on: cli.profile.clone(),
        })?;
        registry.load(&PermissionsExt {
            state: permissions.clone(),
        })?;
        #[cfg(feature = "detach")]
        registry.load(&crate::detach::BgExt {
            paths: paths.clone(),
            permissions: permissions.clone(),
            carried: cli.carried(),
            ephemeral: cli.no_session,
        })?;
        #[cfg(feature = "detach")]
        registry.load(&crate::schedule::tool::ScheduleExt {
            scheduler: crate::schedule::Scheduler::new(cli, &paths, &config),
            permissions: permissions.clone(),
            carried: crate::host_carried(cli),
            ephemeral: cli.no_session,
        })?;
        // Rule files count as context files; `[[stream_rules]]` are settings and always apply.
        let rules = Arc::new(Rules::new(rules::Sources {
            paths: paths.clone(),
            files: !cli.skips(Discovery::ContextFiles),
            claude_compat: compat,
            compat: config.rules.compat,
            stream_rules: config.stream_rules.clone(),
            include_depth: config.context.include_depth,
        }));
        registry.load(&RulesExt { rules: rules.clone() })?;
        if !cli.skips(Discovery::ContextFiles) {
            registry.load(&ContextExt {
                paths: paths.clone(),
                claude_compat: compat,
                rules: Some(rules),
                nested: config.context.nested,
                include_depth: config.context.include_depth,
            })?;
        }
        // What the command line names loads whatever was switched off.
        let mut skill_roots = match cli.skips(Discovery::Skills) {
            true => Vec::new(),
            false => standard_roots(&paths, compat, &packages),
        };
        skill_roots.extend(run.skill_roots.iter().cloned());
        if !cli.skips(Discovery::Skills) || !skill_roots.is_empty() {
            registry.load(&SkillsExt {
                roots: skill_roots,
                trusted,
            })?;
        }
        registry.load(&CustomCommandsExt {
            roots: run.command_roots(&paths, compat, &packages),
            shell: shell.clone(),
            cwd: paths.cwd.clone(),
            trusted,
        })?;
        let coordinator = crate::orchestration::load(&registry, &config, cli)?;
        registry.load(&TeamExt {
            defs: agent_defs.clone(),
            max_depth: config.team.max_depth,
        })?;
        let checker = Checker {
            commands: config.verify.commands.clone(),
            use_agent: config.verify.agent,
            timeout: Duration::from_secs(config.verify.timeout_secs),
            shell: shell.clone(),
            project_root: paths.project_root.clone(),
            services: services.clone(),
        };
        let goal_settings = GoalSettings {
            max_iterations: config.goal.max_iterations,
            max_nudges: config.goal.max_nudges,
            on_stop: config.verify.on_stop,
        };
        // After the advisor, which reviews only a stop that nothing has answered yet, and before
        // the goal, whose own check has the last word.
        registry.load(&azazel_tools::check::ChecksExt {
            root: paths.project_root.clone(),
            on_write: config.tools.check_on_write,
            configured: config.tools.check.clone(),
            trusted,
            report: config.tools.check_report,
            timeout: azazel_tools::check::TIMEOUT,
            limit: azazel_tools::check::LIMIT,
            gitleaks: azazel_tools::scan::scanner(
                config.tools.scan.secrets,
                config.tools.check_report,
                trusted,
            )
            .ok(),
        })?;
        // Behind the report of the checks, which comes first. With `verify.on_stop` the
        // project's checks run at every stop anyway, and there is nothing to send back for.
        let verified_on_stop = config.verify.on_stop && !config.verify.commands.is_empty();
        registry.load(&azazel_tools::guards::GuardsExt {
            done_check: config.guards.done_check && !verified_on_stop,
            stuck_after: config.guards.stuck_after,
            runaway: config.guards.runaway,
            verify: config.verify.commands.clone(),
        })?;
        registry.load(&GoalExt {
            goals: Arc::new(Goals::new(checker, goal_settings)),
        })?;
        let hooks_disabled =
            cli.skips(Discovery::Hooks) || std::env::var_os("AZAZEL_HOOKS_DISABLED").is_some();
        registry.load(&HooksExt {
            hooks: config.hooks.clone(),
            shell: shell.clone(),
            paths: paths.clone(),
            claude_compat: compat,
            disabled: hooks_disabled,
            trusted,
            sources: hook_sources,
        })?;
        #[cfg(feature = "browser")]
        registry.load(&azazel_browser::BrowserExt {
            path: config.browser.path.clone(),
            headless: config.browser.headless,
            profile: paths.browser_profile_dir(),
            screenshot_scale: config.browser.screenshot_scale,
            snapshot_diff: config.browser.snapshot_diff,
            live: azazel_browser::LiveSettings {
                fps: config.browser.live_fps,
                quality: config.browser.live_quality,
                max_width: config.browser.live_max_width,
            }
            .clamped(),
            logins: stored.clone(),
        })?;
        #[cfg(feature = "screen")]
        registry.load(&azazel_browser::screen::ScreenExt)?;
        #[cfg(feature = "media")]
        registry.load(&azazel_media::MediaExt {
            media: Arc::new(azazel_media::Media::from_config(&config, &paths, credentials.clone()).await),
        })?;
        #[cfg(feature = "memory")]
        let memory_on = config.memory.enabled && !cli.skips(Discovery::Memory);
        #[cfg(not(feature = "memory"))]
        let memory_on = false;
        #[cfg(feature = "memory")]
        if memory_on {
            let memory = azazel_memory::MemoryExt::open(&paths, &config, credentials.clone())
                .map_err(|e| anyhow!("memory: {e} (set `[memory] enabled = false` to start without it)"))?;
            registry.load(&memory)?;
        }
        if !memory_on {
            // `/memory` stays a known command, so that it can say why it does nothing.
            registry.load(&MemoryOffExt {
                why: memory_off_reason(cli, &config),
            })?;
        }
        #[cfg(feature = "repomap")]
        registry.load(&azazel_repomap::RepoMapExt::new(
            &paths,
            config.context.repo_map_tokens as usize,
        ))?;
        #[cfg(feature = "mcp")]
        if !cli.skips(Discovery::Mcp) || !run.mcp_files.is_empty() {
            let mut files = match cli.skips(Discovery::Mcp) {
                true => Vec::new(),
                false => mcp_files(&paths, trusted, &packages.mcp_files),
            };
            files.extend(run.mcp_files.iter().cloned());
            registry.load(&azazel_mcp::McpExt {
                manager: mcp_manager(
                    &paths,
                    &config,
                    services.clone(),
                    credentials.clone(),
                    &files,
                    cli.trust,
                ),
            })?;
        }
        // After MCP so scripts can add servers, and before the router so they can add providers.
        #[cfg(feature = "lua")]
        if !cli.skips(Discovery::Extensions) || !run.scripts.is_empty() {
            registry.load(&azazel_lua::LuaExt::new(
                paths.clone(),
                trusted,
                credentials.clone(),
                config
                    .permissions
                    .additional_dirs
                    .iter()
                    .map(|dir| paths.project_root.join(dir))
                    .collect(),
                azazel_lua::Scripts {
                    packaged: packages.scripts.clone(),
                    run: run.scripts.clone(),
                    only_run: cli.skips(Discovery::Extensions),
                },
            ))?;
        }
        // A tool like any other, so it is there with `--no-extensions` too.
        #[cfg(feature = "lua")]
        registry.load(&azazel_lua::CodeExt::default())?;
        for extension in extensions {
            registry
                .load(extension.as_ref())
                .with_context(|| format!("extension `{}` failed to register", extension.id()))?;
        }
        // Last, so that it sees only the stops no other extension turned into another round.
        registry.load(&SkillGrantsExt)?;
        // Calls a script makes for an agent pass the same gate as the agent's own.
        services.insert(Arc::new(ToolCaller::new(
            registry.tools.clone(),
            Arc::new(PermissionGate::new(
                permissions.clone(),
                registry.bus.clone(),
                services.clone(),
                shell.is_posix(),
            )),
        )));
        if config.tools.format_on_write {
            services.insert(Arc::new(azazel_tools::format::Formatters::new(
                config.tools.format.clone(),
                paths.project_root.clone(),
                trusted,
            )));
        }
        // Results lose their secrets before the model and the session file get them.
        if config.tools.redact_output {
            services.insert(Arc::new(OutputRedactor(azazel_tui::text::redact)));
        }
        select_tools(cli, &config, &registry)?;
        // Every process a sub-agent starts inherits the push guard; the lead's do not.
        if let Some(env) = services.get::<AgentEnv>() {
            env.set_guard(guard_env(&|name| std::env::var(name).ok()));
            // Builds the agents start must not take every core, nor start from scratch in each
            // worktree; see `[tools] priority`, `[tools] build_jobs` and `[team] worktree_target`.
            let cores = std::thread::available_parallelism().map_or(2, usize::from);
            let mut vars = azazel_core::process::no_prompts_env();
            vars.extend(azazel_core::process::build_jobs_env(
                &config.tools.build_jobs,
                cores,
                &|name| std::env::var_os(name).is_some(),
            ));
            env.set(vars);
            env.set_priority(config.tools.priority);
            env.set_worktree_target(config.team.worktree_target, which::which("sccache").is_ok());
        }

        let client = http_client();
        let resolved = presets::resolve(&config.providers).map_err(|e| anyhow!(e))?;
        let endpoints = resolved.clone();
        let catalog = Catalog::load(Some(&paths.models_cache()), &config.models);
        // A provider that only a login can reach has no entry in the embedded catalogue; it is
        // asked for its models in the background, and the answer is kept for the next session.
        for login in resolved
            .iter()
            .filter(|p| p.oauth.is_some() && credentials.source(p).is_some_and(|s| s.contains("login")))
        {
            let (provider, client, credentials) = (login.clone(), client.clone(), credentials.clone());
            let cache = paths.models_cache();
            tokio::spawn(async move {
                match HttpProvider::new(provider.clone(), client, credentials)
                    .list_models()
                    .await
                {
                    Ok(models) if !models.is_empty() => {
                        if let Err(e) = Catalog::refresh_cache(&cache, &provider.id, &models) {
                            tracing::warn!("could not record the models of {}: {e}", provider.id);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!("the model list of {} was not refreshed: {e}", provider.id),
                }
            });
        }
        // A provider lists what its endpoint reports plus the models the configuration
        // (`models.yml`, `[models]`) names for it.
        let mut providers: Vec<Arc<dyn Provider>> = resolved
            .into_iter()
            .map(|p| {
                let declared = catalog.declared(&config.models, &p.id);
                let provider = HttpProvider::new(p, client.clone(), credentials.clone())
                    .with_declared(declared)
                    .with_stall(stall);
                Arc::new(provider) as Arc<dyn Provider>
            })
            .collect();
        #[cfg(feature = "bedrock")]
        {
            let settings = config.providers.get(presets::BEDROCK);
            providers.push(Arc::new(
                azazel_ai::providers::bedrock::BedrockProvider::new(
                    settings.and_then(|s| s.region.clone()),
                    settings.and_then(|s| s.profile.clone()),
                )
                .with_stall(settings.map_or(stall, |s| s.timeouts().over(stall))),
            ));
        }
        #[cfg(feature = "vertex")]
        {
            let settings = config.providers.get(presets::VERTEX);
            providers.push(Arc::new(
                azazel_ai::providers::vertex::VertexProvider::new(
                    settings.and_then(|s| s.project.clone()),
                    settings.and_then(|s| s.location.clone()),
                    client.clone(),
                )
                .with_stall(settings.map_or(stall, |s| s.timeouts().over(stall))),
            ));
        }
        providers.extend(registry.providers());
        providers.extend(mock_provider(&mut config)?);
        let provider_ids: Vec<String> = providers.iter().map(|p| p.id().to_string()).collect();
        if let Some(only) = &cli.provider
            && !provider_ids.contains(only)
        {
            let mut known = provider_ids.clone();
            known.sort();
            return Err(UsageError(format!(
                "--provider: there is no provider `{only}`; the providers are: {}",
                known.join(", ")
            ))
            .into());
        }
        let policy = RetryPolicy {
            max_attempts: config.retry.max_attempts.max(1),
            cooldown: Duration::from_secs(config.retry.cooldown_secs),
            max_stalls: config.retry.max_stalls,
            ..RetryPolicy::default()
        };
        let mut router = Router::new(providers, catalog, config.roles.clone(), policy)
            .with_accounts(credentials.clone())
            .with_virtual(registry.virtual_models.clone());
        // The lead gives up a long rate limit at once only when a profile can take it over.
        if !config.profiles.on_limit.trim().is_empty() {
            router = router.with_limit_hand_over();
        }
        {
            let (accounts, configs) = (credentials.clone(), endpoints.clone());
            router = router.with_logins(Arc::new(move |provider: &str| {
                configs
                    .iter()
                    .find(|p| p.id == provider)
                    .is_some_and(|p| crate::models::login(&accounts, p))
            }));
        }
        if config.usage.ledger {
            let accounts = credentials.clone();
            let configs = endpoints.clone();
            let slug = paths
                .project_data
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            router = router.with_usage_sink(Arc::new(azazel_core::ledger::Ledger::new(
                &paths.home,
                &slug,
                move |provider| {
                    configs
                        .iter()
                        .find(|p| p.id == provider)
                        .is_some_and(|p| crate::models::login(&accounts, p))
                },
            )));
        }
        let router = Arc::new(router);
        router.set_tier(tier);
        // Models named on the command line are this run's `default` role, so that the roles
        // built on it (`smol`, `verify`, ...) and the agents using them have a model too.
        let lead_role = vec![ModelSpec::Role("lead".to_string())];
        let chain = match (&cli.provider, cli.model.is_empty()) {
            (None, true) => lead_role,
            // The lead's configured models, without those of other providers.
            (Some(only), true) => {
                let configured = router.resolve(&lead_role);
                let chain: Vec<ModelSpec> = configured
                    .iter()
                    .filter(|model| model.provider == *only)
                    .cloned()
                    .map(ModelSpec::Model)
                    .collect();
                if chain.is_empty() {
                    let models: Vec<String> = configured.iter().map(ToString::to_string).collect();
                    return Err(UsageError(match models.as_slice() {
                        [] => format!("--provider {only}: no model is configured; name one with --model"),
                        models => format!(
                            "--provider {only}: none of the configured models ({}) is that provider's; name one \
                             with --model",
                            models.join(", ")
                        ),
                    })
                    .into());
                }
                chain
            }
            (only, false) => {
                let lists = known_and_usable(
                    provider_ids.clone(),
                    &endpoints,
                    &credentials,
                    cli.api_key.is_some(),
                );
                let (known, usable) = only_provider(lists, only.as_deref());
                let chain = crate::models::resolve_specs(&cli.model, router.catalog(), &known, &usable)
                    .map_err(|why| UsageError(provider_note(why, only.as_deref())))?;
                crate::models::check_virtual(&chain, &router, only.as_deref()).map_err(UsageError)?;
                chain
            }
        };
        // `--models` names what the model-cycling keys step through in this run, in the words
        // `--model` takes.
        if !cli.models.is_empty() {
            let only = cli.provider.as_deref();
            let lists = known_and_usable(provider_ids, &endpoints, &credentials, cli.api_key.is_some());
            let (known, usable) = only_provider(lists, only);
            config.tui.favourites =
                crate::models::resolve_specs(&cli.models, router.catalog(), &known, &usable).map_err(
                    |why| UsageError(provider_note(why.replacen("--model ", "--models ", 1), only)),
                )?;
        }
        let chosen = !cli.model.is_empty() || cli.provider.is_some();
        if chosen && chain.iter().all(|spec| matches!(spec, ModelSpec::Model(_))) {
            config.roles.insert("default".to_string(), chain.clone());
            router.set_role("default", chain.clone());
        }
        if let (Some(key), Some(ModelSpec::Model(model))) = (&cli.api_key, chain.first()) {
            credentials.set_flag_key(&model.provider, key.clone());
        }
        // Extensions that need a model of their own (MCP sampling) find the router here.
        services.insert(router.clone());
        let team = Arc::new(Team::new(TeamDeps {
            tools: registry.tools.clone(),
            hooks: registry.agent_hooks(),
            prompt: registry.prompt.clone(),
            bus: registry.bus.clone(),
            services: services.clone(),
            router: router.clone(),
            permissions: permissions.clone(),
            paths: paths.clone(),
            defs: agent_defs.clone(),
            max_parallel: config.team.max_parallel,
            max_depth: config.team.max_depth,
            max_turns: config.team.max_turns,
            max_result_bytes: config.tools.max_result_bytes,
            compaction: (&config.compaction).into(),
            posix_shell: shell.is_posix(),
            routing: (&config.team).into(),
        }));
        services.insert(team.clone());
        let report = cli
            .json_schema
            .as_deref()
            .map(|value| crate::orchestration::result_schema(value, &paths.cwd))
            .transpose()?;
        if let Some(typed) = &report {
            team.expect_lead_report(typed.clone());
        }
        // Code-first mode needs Luau, which hidden tools are reached through.
        let code_first = if !config.tools.code_first {
            None
        } else if !eval_luau(&config.tools.eval) {
            tracing::warn!(
                "tools.code_first is ignored: it needs Luau in eval (tools.eval.luau = true, in a build with the `lua` feature)"
            );
            None
        } else {
            Some(Arc::new(code_first_hiding(agent_defs, lead_agent.clone())))
        };
        if let Some(code_first) = &code_first {
            services.insert(code_first.clone());
        }
        let lead_tools = Arc::new(LeadTools::new(
            registry.tools.clone(),
            coordinator.clone(),
            report.clone(),
            Some(lead_agent.clone()),
        ));
        // A script run for the lead reaches what the lead is offered, and no more.
        if let Some(caller) = services.get::<ToolCaller>() {
            caller.scope_agent("lead", lead_tools.clone());
        }
        let side_costs = Arc::new(SideCosts::default());
        // What the prompt cache did for each request; with `cache.warming` it is also kept alive
        // while the session waits for its user.
        let cache = {
            let (accounts, configs) = (credentials.clone(), endpoints.clone());
            Arc::new(
                CacheLedger::new(&config.cache).with_logins(Arc::new(move |provider: &str| {
                    configs
                        .iter()
                        .find(|p| p.id == provider)
                        .is_some_and(|p| crate::models::login(&accounts, p))
                })),
            )
        };
        services.insert(cache.clone());
        if cache.warms() {
            tokio::spawn(
                Warmer {
                    ledger: cache,
                    router: router.clone(),
                    costs: side_costs.clone(),
                    services: services.clone(),
                }
                .run(),
            );
        }
        services.insert(Arc::new(Side {
            router: router.clone(),
            prompt: registry.prompt.clone(),
            // The request of a question asked on the side carries the tools the lead's own do.
            tools: match code_first {
                Some(code_first) => Arc::new(azazel_agent::CodeFirstVisible {
                    inner: lead_tools.clone(),
                    code_first,
                }),
                None => lead_tools.clone(),
            },
            costs: side_costs.clone(),
        }));
        // What the run has spent, for the cost cap: every agent of the team, the lead included,
        // and the requests made on the side.
        let members = Arc::downgrade(&team);
        services.insert(Arc::new(Spend(Arc::new(move || {
            let agents: f64 = members
                .upgrade()
                .map(|team| team.list().iter().filter_map(|m| m.cost_usd).sum())
                .unwrap_or(0.0);
            agents + side_costs.total_cost()
        }))));
        // The interactive UI starts anyway and walks the user through choosing a model; every
        // other mode needs one before it can do anything.
        let interactive = cli.interactive();
        if !interactive && router.resolve(&chain).is_empty() {
            bail!(NO_MODEL);
        }

        let current: Arc<RwLock<Option<Arc<SessionStore>>>> = Arc::default();
        let recorder = current.clone();
        permissions.on_mode_change(Arc::new(move |mode| {
            if let Some(session) = recorder.read().unwrap_or_else(PoisonError::into_inner).as_ref() {
                session.append(EntryKind::ModeChange {
                    mode: mode.to_string(),
                });
            }
        }));
        let recorder = current.clone();
        coordinator.on_change(Arc::new(move |on| {
            if let Some(session) = recorder.read().unwrap_or_else(PoisonError::into_inner).as_ref() {
                session.append(EntryKind::ModeChange {
                    mode: azazel_team::coordinator::entry(on),
                });
            }
        }));
        // A switch of the primary agent is recorded, and the lead takes up its limits, its model
        // and its thinking level at once.
        let switched = Switched {
            permissions: permissions.clone(),
            services: Arc::downgrade(&services),
            router: router.clone(),
            before: Arc::default(),
            pinned: (chosen, cli.thinking.is_some()),
        };
        let before = switched.before.clone();
        let recorder = current.clone();
        lead_agent.on_change(Arc::new(move |def| {
            if let Some(session) = recorder.read().unwrap_or_else(PoisonError::into_inner).as_ref() {
                session.append(EntryKind::AgentChange {
                    agent: def.name.clone(),
                });
            }
            switched.apply(def);
        }));

        let telemetry = Telemetry::start(&config.telemetry);
        Ok(App {
            paths,
            config,
            trusted,
            ignored_settings,
            registry,
            router,
            credentials,
            permissions,
            shell,
            chain,
            thinking,
            load_options: options,
            run,
            provider: cli.provider.clone(),
            hooks_disabled,
            coordinator,
            report,
            lead_tools,
            coordinator_forced: AtomicBool::new(cli.coordinator),
            lead_agent,
            agent_flag: Mutex::new(agent_flag),
            before,
            ephemeral: cli.no_session,
            model_flag: chosen,
            current,
            background: cli.background(),
            session_lock: Mutex::default(),
            telemetry,
        })
    }

    /// The session the command line asks for: `--resume <id>`, `--continue`, or a new one; the
    /// interactive UI continues the latest one when `[session] resume` says so and `--new` is
    /// not given.
    pub fn requested_session(&self, cli: &Cli) -> Result<Open> {
        let dir = self.paths.sessions_dir();
        if let Some(wanted) = &cli.session {
            return Ok(Open::File(self.find_session(wanted)?));
        }
        if let Some(wanted) = &cli.fork {
            let original = SessionStore::open(&self.find_session(wanted)?)?;
            // Read back from its file, like any session that was not open in this process.
            return Ok(Open::File(original.fork(&dir, None)?.path()));
        }
        let folder = self.paths.session_folder();
        Ok(latest_session(&dir, folder, cli, self.config.session.resume)?.unwrap_or(Open::New))
    }

    /// The id of the background session that has the session `how` opens, when one does: such a
    /// session is not opened a second time, it is attached to.
    pub fn background_host(&self, how: &Open) -> Option<String> {
        background_host(&self.paths.home, how)
    }

    /// Model names as typed (`fable`, `@smol`, `anthropic/claude-fable-5:high`) turned into a chain,
    /// the way `--model` resolves them.
    pub fn model_specs(&self, texts: &[String]) -> Result<Vec<ModelSpec>, String> {
        let endpoints = presets::resolve(&self.config.providers)?;
        let lists = known_and_usable(
            self.router
                .provider_ids()
                .into_iter()
                .map(str::to_string)
                .collect(),
            &endpoints,
            &self.credentials,
            false,
        );
        let (known, usable) = only_provider(lists, self.provider.as_deref());
        let chain = crate::models::resolve_specs(texts, self.router.catalog(), &known, &usable)
            .map_err(|why| provider_note(why, self.provider.as_deref()))?;
        crate::models::check_virtual(&chain, &self.router, self.provider.as_deref())?;
        Ok(chain)
    }

    /// A session given as a path to its file, or as its id (a unique prefix) in this project.
    pub fn find_session(&self, wanted: &str) -> Result<PathBuf> {
        let path = self.paths.cwd.join(wanted);
        if wanted.ends_with(".jsonl") || path.is_file() {
            if !path.is_file() {
                bail!("no session file {}", path.display());
            }
            return Ok(path);
        }
        let matches: Vec<_> = SessionStore::list_all(&self.paths.sessions_dir())
            .into_iter()
            .filter(|s| s.id.starts_with(wanted))
            .collect();
        match matches.as_slice() {
            [one] => Ok(one.path.clone()),
            [] => bail!("no session with id `{wanted}` in this project"),
            many => bail!("`{wanted}` matches {} sessions; give more of the id", many.len()),
        }
    }

    /// The virtual models that can be chosen in this run, as `virtual/<name>`: those extensions
    /// define, and none while `--provider` keeps the run to one provider's models.
    pub fn virtual_models(&self) -> Vec<String> {
        match self.provider {
            Some(_) => Vec::new(),
            None => crate::models::virtual_names(&self.router),
        }
    }

    /// Whether `chain` names a model this process can use: a configured one whose provider needs
    /// no key or has a credential (the cloud and script providers find their own), or a virtual
    /// model an extension that is loaded defines.
    pub fn has_model(&self, chain: &[ModelSpec]) -> bool {
        let Ok(endpoints) = presets::resolve(&self.config.providers) else {
            return false;
        };
        let defined = self.virtual_models();
        self.router.resolve(chain).iter().any(|model| {
            if azazel_ai::is_virtual(model) {
                return defined.contains(&model.key());
            }
            match endpoints.iter().find(|p| p.id == model.provider) {
                Some(p) => !p.requires_key || self.credentials.source(p).is_some(),
                None => true,
            }
        })
    }

    /// Whether what this run does is kept on disk (it is not with `--no-session`).
    pub fn keeps_sessions(&self) -> bool {
        !self.ephemeral
    }

    /// Writes the model and thinking level chosen on the command line into the session, so that
    /// reopening it later comes back on them.
    pub fn record_flags(&self, session: &SessionStore) {
        if self.model_flag && session.last_chain(None).as_ref() != Some(&self.chain) {
            session.append(EntryKind::ModelChange {
                chain: self.chain.clone(),
            });
        }
        if let Some(level) = self.thinking.map(|level| level.to_string())
            && session.last_thinking(None).as_ref() != Some(&level)
        {
            session.append(EntryKind::ThinkingChange { level });
        }
    }

    /// What a session read from its file starts on: the model chain and thinking level it last
    /// switched to, unless the command line chose them or `[session] restore_model` is off. A
    /// chain that can no longer be used gives way to `chain`, with a notice.
    fn as_left(
        &self,
        session: &SessionStore,
        chain: Vec<ModelSpec>,
        thinking: Option<ThinkingLevel>,
    ) -> (Vec<ModelSpec>, Option<ThinkingLevel>, Option<String>) {
        if !self.config.session.restore_model {
            return (chain, thinking, None);
        }
        let names = |chain: &[ModelSpec]| {
            chain
                .iter()
                .map(ModelSpec::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let last = session.last_chain(None).filter(|_| !self.model_flag);
        let (chain, notice) = match last {
            Some(last) if self.has_model(&last) => (last, None),
            Some(last) => {
                let notice = format!(
                    "this session last used {}, which cannot be used now; it goes on with {}",
                    names(&last),
                    names(&chain)
                );
                (chain, Some(notice))
            }
            None => (chain, None),
        };
        let thinking = session
            .last_thinking(None)
            .filter(|_| self.thinking.is_none())
            .and_then(|level| level.parse().ok())
            .or(thinking);
        (chain, thinking, notice)
    }

    /// Puts the lead to the primary agent `session` is on: the one `--agent` names, else the one
    /// its branch last switched to, else the one the lead is now. A session that is opened as
    /// another agent than it records gets the new one written down. Returns the model chain and
    /// thinking level the lead starts on as that agent, and what the user should be told.
    fn open_agent(
        &self,
        session: &SessionStore,
        chain: Vec<ModelSpec>,
        thinking: Option<ThinkingLevel>,
        mut notice: Option<String>,
    ) -> (Vec<ModelSpec>, Option<ThinkingLevel>, Option<String>) {
        let lead = &self.lead_agent;
        let default = lead.default_name();
        let recorded = session.last_agent(None);
        let forced = self
            .agent_flag
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let wanted = forced.or_else(|| recorded.clone()).unwrap_or_else(|| lead.name());
        let def = match lead.restore(&wanted) {
            Ok(def) => def,
            Err(_) => {
                let gone = format!(
                    "this session was last the agent {wanted}, which is gone; it goes on as {default}"
                );
                notice = Some(notice.map_or(gone.clone(), |first| format!("{first}; {gone}")));
                // The default is a primary agent: `LeadAgent::new` made sure of it.
                match lead.restore(default) {
                    Ok(def) => def,
                    Err(_) => return (chain, thinking, notice),
                }
            }
        };
        if recorded.as_deref().unwrap_or(default) != def.name {
            session.append(EntryKind::AgentChange {
                agent: def.name.clone(),
            });
        }
        lead_limits(&self.permissions, &def);
        let mut before = self.before.lock().unwrap_or_else(PoisonError::into_inner);
        *before = Before::default();
        let follow = follow(
            &def,
            &chain,
            thinking,
            &mut before,
            &self.router,
            (self.model_flag, self.thinking.is_some()),
        );
        if let Some(more) = follow.notice {
            notice = Some(notice.map_or(more.clone(), |first| format!("{first}; {more}")));
        }
        (
            follow.chain.unwrap_or(chain),
            follow.thinking.unwrap_or(thinking),
            notice,
        )
    }

    /// Makes the lead the primary agent `name`: it is recorded in the session, and the lead takes
    /// up the agent's identity, limits, model and thinking level. Returns whether that changed
    /// anything.
    pub fn set_agent(&self, name: &str) -> Result<bool, String> {
        self.lead_agent.set(name)
    }

    /// Opens a session and starts its lead agent on `chain`. A session read from its file comes
    /// back on the model and thinking level it last used instead (see [`App::as_left`]).
    pub async fn open(
        &self,
        how: Open,
        chain: Vec<ModelSpec>,
        thinking: Option<ThinkingLevel>,
    ) -> Result<Active> {
        let from_file = matches!(how, Open::File(_));
        let (session, resumed) = match how {
            Open::New => (
                Arc::new(SessionStore::create(
                    &self.paths.sessions_dir(),
                    &self.paths.cwd,
                    None,
                )?),
                false,
            ),
            Open::File(path) => (Arc::new(SessionStore::open(&path)?), true),
            Open::Store(store) => (store, true),
        };
        let lock = self.lock_session(&session.id(), from_file)?;
        let (chain, thinking, notice) = if from_file {
            self.as_left(&session, chain, thinking)
        } else {
            (chain, thinking, None)
        };
        if resumed {
            crate::accounts::restore(&session, &self.credentials);
        }
        // From the first session on, the usage of every subscription is asked for in the
        // background, so that a request does not start on an account that has nothing left.
        let services = &self.registry.services;
        if self.config.usage.poll_secs > 0
            && services.get::<crate::usage::Poller>().is_none()
            && let Ok(source) = crate::usage::Source::of(&self.config, self.credentials.clone(), &self.paths)
        {
            services.insert(Arc::new(crate::usage::poll(source, self.config.usage.poll_secs)));
        }
        let session_id = session.id();
        crate::orchestration::open_session(
            &self.coordinator,
            &session,
            self.coordinator_forced.swap(false, Ordering::SeqCst),
        );
        let (chain, thinking, notice) = self.open_agent(&session, chain, thinking, notice);
        // The task list is the one this branch of the session ended with.
        if let Some(todos) = self.registry.services.get::<azazel_tools::TodoStore>() {
            todos.rehydrate("lead", &session.branch(None));
        }
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Some(session.clone());
        // Extensions that need the transcript (hooks) find the open session here.
        self.registry.services.insert(session.clone());
        self.registry.bus.set_session(&session_id);
        // A fork holds its parent's entries: its own history of snapshots starts at the last
        // one they name.
        let seed = session
            .branch(None)
            .into_iter()
            .rev()
            .find_map(|entry| match entry.kind {
                EntryKind::Checkpoint { commit, .. } => Some(commit),
                _ => None,
            });
        let checkpoints =
            Arc::new(Checkpoints::new(&self.paths, &session_id, &self.config.checkpoints).seeded(seed));

        let services = self.registry.services.clone();
        let info = AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id,
        };
        let gate = PermissionGate::new(
            self.permissions.clone(),
            self.registry.bus.clone(),
            services.clone(),
            self.shell.is_posix(),
        );
        let lead = Agent::spawn(AgentConfig {
            system: self.registry.prompt.render(&info).await,
            info,
            tools: self.lead_tools.clone(),
            chain,
            thinking,
            router: self.router.clone(),
            gate: Arc::new(gate),
            bus: self.registry.agent_hooks(),
            services: services.clone(),
            cwd: self.paths.cwd.clone(),
            initial_messages: session.build_context(None),
            sink: session.clone(),
            artifacts: services
                .get::<ArtifactStore>()
                .map(|store| store as Arc<dyn ArtifactSink>),
            limits: Limits {
                max_turns: (self.config.limits.max_turns > 0).then_some(self.config.limits.max_turns),
                max_result_bytes: self.config.tools.max_result_bytes,
            },
            compaction: (&self.config.compaction).into(),
        });
        if let Some(team) = services.get::<Team>() {
            team.attach(lead.clone(), session.clone());
        }
        // A `wake` hook's output reaches this session's lead: a steer message while it works,
        // a prompt of its own when it is idle.
        let woken = lead.clone();
        services.insert(Arc::new(azazel_core::hooks::Wake(Arc::new(move |text| {
            woken.steer(UserInput::text(text));
        }))));
        // Requests made on the lead's model from outside its conversation (a web search through
        // its provider) follow the model it is on now.
        let asked = lead.clone();
        services.insert(Arc::new(azazel_core::side::LeadModel(Arc::new(move || {
            asked.status().chain
        }))));
        if let Some(goals) = services.get::<Goals>() {
            goals.attach(lead.clone(), session.clone(), checkpoints.clone());
        }
        if let Some(advisor) = services.get::<Advisor>() {
            advisor.attach(&lead, session.clone(), checkpoints.clone());
        }
        let trace = self
            .telemetry
            .as_ref()
            .map(|telemetry| telemetry.watch(&session, lead.clone(), services.get::<Team>()));
        Ok(Active {
            session,
            lead,
            checkpoints,
            resumed,
            notice,
            _lock: lock,
            trace,
        })
    }

    /// A background session locks the session it opens. Any other process only makes sure that
    /// no background session has the file it is about to read and write.
    fn lock_session(&self, session_id: &str, from_file: bool) -> Result<Option<Arc<Lock>>> {
        if !self.background {
            if from_file && running::host_of(&self.paths.home, session_id).is_some() {
                bail!(running::in_background(&self.paths.home, session_id));
            }
            return Ok(None);
        }
        let mut held = self.session_lock.lock().unwrap_or_else(PoisonError::into_inner);
        // The same session opened again (its leaf moved): the lock it has stays.
        if let Some((id, lock)) = held.as_ref()
            && id == session_id
            && let Some(lock) = lock.upgrade()
        {
            return Ok(Some(lock));
        }
        let lock = Arc::new(running::lock_session(&self.paths.home, session_id).map_err(|e| anyhow!(e))?);
        *held = Some((session_id.to_string(), Arc::downgrade(&lock)));
        Ok(Some(lock))
    }

    /// The session that is open.
    pub fn session(&self) -> Option<Arc<SessionStore>> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Announces a session to extensions. Call once per opened session, before its first prompt.
    pub async fn start(&self, source: StartSource) {
        self.registry.bus.emit(&mut Event::SessionStart { source }).await;
    }

    pub fn notify(&self, level: Level, text: &str) {
        if let Some(ui) = self.registry.services.get::<Ui>() {
            ui.0.notify(level, text);
        }
    }

    /// The issues and pull requests a prompt names with `#<n>`, as `<file path="issue://<n>">`
    /// blocks to append to it. Only numbers `gh` resolves in the repository of the working
    /// directory are attached; without `gh`, or outside a GitHub repository, nothing is.
    async fn github_references(&self, prompt: &str) -> String {
        let numbers = azazel_tools::gh::references(prompt);
        if numbers.is_empty() {
            return String::new();
        }
        let Some(program) = azazel_tools::schemes::gh_program() else {
            return String::new();
        };
        let env = self
            .registry
            .services
            .get::<AgentEnv>()
            .map(|env| env.vars())
            .unwrap_or_default();
        let mut out = String::new();
        for number in numbers.into_iter().take(azazel_tools::gh::MAX_REFERENCES) {
            let Some((kind, shown)) =
                azazel_tools::gh::resolve(&program, &self.paths.cwd, number, &env).await
            else {
                continue;
            };
            self.notify(Level::Info, &format!("attached {} #{number}", kind.label()));
            out.push_str(&format!(
                "

<file path=\"{}://{number}\">
{shown}
</file>",
                kind.noun()
            ));
        }
        out
    }

    /// Sends a user prompt to the lead: extensions may rewrite or reject it, mentioned files are
    /// attached, the project tree is checkpointed, then the lead starts. `Err` carries the reason
    /// an extension rejected the prompt.
    pub async fn submit(&self, active: &Active, text: String, images: Vec<Block>) -> Result<(), String> {
        self.submit_as(active, text, images, None).await
    }

    /// [`App::submit`] of a prompt its sender named: the name is recorded with the run the
    /// prompt starts, so that the same prompt sent twice can be told from a new one.
    pub async fn submit_as(
        &self,
        active: &Active,
        text: String,
        images: Vec<Block>,
        request_id: Option<String>,
    ) -> Result<(), String> {
        let mut event = Event::UserPrompt { text, images };
        if let Outcome::Block { reason } = self.registry.bus.emit(&mut event).await {
            return Err(reason);
        }
        let Event::UserPrompt { text, mut images } = event else {
            return Err("an extension replaced the prompt event".to_string());
        };
        let mentions = expand_mentions(&text, &self.paths.cwd);
        if let Some(tracker) = self.registry.services.get::<FileTracker>() {
            for (path, bytes) in &mentions.files {
                tracker.record(&active.lead.id, path, bytes);
            }
        }
        let mut text = mentions.text;
        text.push_str(&self.github_references(&text.clone()).await);
        for (name, bytes) in mentions.images {
            match tokio::task::spawn_blocking(move || image_block(&bytes, MAX_IMAGE_SIDE)).await {
                Ok(Ok(block)) => images.push(block),
                Ok(Err(why)) => self.notify(Level::Warn, &format!("@{name} is not attached: {why}")),
                Err(e) => self.notify(Level::Warn, &format!("@{name} is not attached: {e}")),
            }
        }
        let follows = active.session.leaf().unwrap_or_default();
        match active.checkpoints.snapshot("prompt").await {
            Ok(Some(commit)) => {
                active.session.append(EntryKind::Checkpoint {
                    turn_entry_id: follows,
                    commit,
                });
            }
            Ok(None) => {}
            Err(reason) => self.notify(Level::Warn, &format!("checkpoints are off: {reason}")),
        }
        let mut content = vec![Block::text(text)];
        content.append(&mut images);
        active.lead.prompt(UserInput { content, request_id });
        Ok(())
    }

    /// Goes on with the run of the open session that the end of a process cut short: calls
    /// without a result get one and the request nobody answered is sent again. `Err` when no
    /// run was interrupted, or the lead is at work.
    pub fn continue_run(&self, active: &Active) -> Result<InterruptedRun, String> {
        if active.lead.status().busy {
            return Err("the lead is working; there is no interrupted run to go on with".to_string());
        }
        let run = active
            .session
            .interrupted_run(None)
            .ok_or("no run of this session was interrupted")?;
        active.lead.send(AgentCmd::Resume(Resume {
            run_id: run.run_id.clone(),
            started: run.started.clone(),
        }));
        Ok(run)
    }

    /// Stops a session's lead without ending the process-wide session state.
    pub async fn close(&self, active: &Active) {
        if let Some(team) = self.registry.services.get::<Team>() {
            team.detach().await;
        }
        active.lead.shutdown().await;
        if let Some(trace) = &active.trace {
            trace.close().await;
        }
    }

    /// Ends the last session: tells extensions, stops the lead and background jobs.
    pub async fn finish(&self, active: &Active) {
        let ended = std::time::Instant::now();
        let mut at = ended;
        let mut took = |step: &str| {
            tracing::debug!("session end: {step} took {} ms", at.elapsed().as_millis());
            at = std::time::Instant::now();
        };
        // A name being chosen in the background is written before the session closes.
        if let Some(namer) = self.registry.services.get::<Namer>() {
            namer.settle().await;
        }
        took("naming");
        self.registry.bus.emit(&mut Event::SessionEnd).await;
        took("session_end handlers");
        self.close(active).await;
        took("closing the lead");
        // A start that sent nothing leaves no file to pile up in `/resume`.
        active.session.discard_if_empty();
        if let Some(telemetry) = &self.telemetry {
            telemetry.flush().await;
        }
        took("telemetry");
        active.checkpoints.tidy().await;
        took("checkpoints");
        if let Some(jobs) = self.registry.services.get::<azazel_tools::Jobs>() {
            jobs.kill_all();
        }
        #[cfg(feature = "mcp")]
        if let Some(mcp) = self.registry.services.get::<azazel_mcp::McpManager>() {
            mcp.shutdown();
        }
        if self.ephemeral {
            let _ = std::fs::remove_dir_all(ephemeral_dir(&self.paths));
        }
        took("jobs and servers");
        tracing::debug!("session end: {} ms in all", ended.elapsed().as_millis());
    }

    pub fn log_file(paths: &Paths) -> PathBuf {
        paths.logs_dir().join("azazel.log")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use clap::Parser;

    use super::*;

    /// What a start with `args` opens in a project whose sessions are in `dir`: the file of a
    /// session, `None` for a new one.
    fn opened(dir: &Path, args: &[&str], resume: SessionResume) -> Option<PathBuf> {
        opened_in(dir, None, args, resume)
    }

    /// The same for a start in `folder` of the project.
    fn opened_in(dir: &Path, folder: Option<&Path>, args: &[&str], resume: SessionResume) -> Option<PathBuf> {
        let cli = Cli::try_parse_from(std::iter::once("azazel").chain(args.iter().copied())).unwrap();
        match latest_session(dir, folder, &cli, resume).unwrap() {
            Some(Open::File(path)) => Some(path),
            Some(_) => panic!("not a session file"),
            None => None,
        }
    }

    /// The text `/memory` answers in a session started with `args`.
    async fn memory_command_says(args: &[&str]) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        let (home, project) = (dir.path().join("home"), dir.path().join("project"));
        std::fs::create_dir_all(project.join(".azazel")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let mut argv = vec![
            "azazel".as_ref(),
            "--home".as_ref(),
            home.as_os_str(),
            "--cwd".as_ref(),
        ];
        argv.push(project.as_os_str());
        argv.extend(args.iter().map(std::ffi::OsStr::new));
        let cli = Cli::try_parse_from(argv).unwrap();
        let paths = discover_paths(&cli).unwrap();
        let app = App::build(&cli, paths, true, &[]).await.unwrap();
        let command = app.registry.command("memory")?;
        match command.handler.run("list", &app.registry.bus.ctx()).await {
            CommandOutcome::Show(text) => Some(text),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn memory_stays_a_known_command_and_says_why_it_is_off() {
        let said = memory_command_says(&["--no-memory"]).await.unwrap();
        assert!(
            said.contains("turned off") && said.contains("--no-memory"),
            "{said}"
        );
        let said = memory_command_says(&["--set", "memory.enabled=false"])
            .await
            .unwrap();
        assert!(
            said.contains("turned off") && said.contains("[memory] enabled"),
            "{said}"
        );
    }

    #[cfg(feature = "memory")]
    #[tokio::test]
    async fn memory_command_is_the_real_one_when_memory_is_on() {
        let said = memory_command_says(&[]).await.unwrap();
        assert!(!said.contains("turned off"), "{said}");
    }

    #[test]
    fn the_interactive_ui_continues_the_latest_session_when_the_setting_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        // No session yet: a new one, without complaint; `-c` does complain.
        assert_eq!(opened(&sessions, &[], SessionResume::Last), None);
        let cli = Cli::try_parse_from(["azazel", "-c"]).unwrap();
        assert!(latest_session(&sessions, None, &cli, SessionResume::Last).is_err());

        let store = SessionStore::create(&sessions, dir.path(), None).unwrap();
        store.append(EntryKind::SessionInfo {
            name: "earlier".to_string(),
        });
        let latest = Some(store.path());
        assert_eq!(opened(&sessions, &[], SessionResume::Last), latest);
        assert_eq!(opened(&sessions, &[], SessionResume::New), None);
        assert_eq!(opened(&sessions, &["--new"], SessionResume::Last), None);
        assert_eq!(opened(&sessions, &["--no-session"], SessionResume::Last), None);
        // Print mode keeps starting new sessions unless it is told otherwise.
        assert_eq!(opened(&sessions, &["-p", "hi"], SessionResume::Last), None);
        assert_eq!(opened(&sessions, &["-p", "-c", "hi"], SessionResume::New), latest);
        // `--new` with a flag that names a session is a mistake.
        assert!(Cli::try_parse_from(["azazel", "--new", "-c"]).is_err());
    }

    #[test]
    fn a_session_open_in_the_background_is_attached_to_instead_of_opened() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let store = SessionStore::create(&dir.path().join("sessions"), dir.path(), None).unwrap();
        let latest = Open::File(store.path());
        assert_eq!(background_host(&home, &latest), None);
        assert_eq!(background_host(&home, &Open::New), None);

        // A background session took it (as one a browser starts through `azazel remote` does).
        let lock = running::lock_session(&home, &store.id()).unwrap();
        let _alive = Lock::acquire(&running::host_lock(&home, "ab12"))
            .unwrap()
            .unwrap();
        running::Entry {
            id: "ab12".into(),
            pid: 1,
            session_id: store.id(),
            session_file: store.path(),
            cwd: dir.path().to_path_buf(),
            name: None,
            started: "2026-01-01T00:00:00Z".into(),
            socket: "sock".into(),
            version: "0".into(),
            state: "idle".into(),
            cost_usd: None,
            mode: None,
            args: Vec::new(),
            interrupted_at: None,
        }
        .write(&home)
        .unwrap();
        assert_eq!(background_host(&home, &latest).as_deref(), Some("ab12"));
        // Once it has ended, the session is opened here again.
        drop(lock);
        assert_eq!(background_host(&home, &latest), None);
    }
}
