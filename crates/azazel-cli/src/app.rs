//! Assembles the process-wide pieces (config, providers, extensions, permissions) and opens
//! sessions on top of them. Every mode starts here.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use azazel_agent::{Agent, AgentConfig, AgentHandle, AgentInfo, ArtifactSink, Limits, Services, UserInput};
use azazel_ai::providers::http::{HttpProvider, http_client};
use azazel_ai::providers::presets;
use azazel_ai::{Block, Catalog, Credentials, ModelSpec, Provider, RetryPolicy, Router, ThinkingLevel};
use azazel_core::agents;
use azazel_core::checkpoint::Checkpoints;
use azazel_core::commands::{CustomCommandsExt, command_roots, expand_mentions};
use azazel_core::config::set_override;
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
use azazel_core::side::{Side, SideCosts};
use azazel_core::skills::{SkillGrantsExt, SkillsExt, standard_roots};
use azazel_core::{
    ArtifactStore, Config, EntryKind, Event, ExtCtx, Extension, FileTracker, Headless, Level, LoadOptions,
    Outcome, Paths, PermissionGate, PermissionMode, PermissionState, Registry, SessionStore, StartSource,
    ToolCaller, Ui,
};
use azazel_team::{
    Advisor, Checker, Coordinator, GoalExt, GoalSettings, Goals, LeadTools, Team, TeamDeps, TeamExt, Typed,
};
use azazel_tools::CoreToolsExt;

use crate::cli::{Cli, Discovery};

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
    pub thinking: ThinkingLevel,
    /// How the configuration was loaded, for reading it again (`/reload`).
    pub load_options: LoadOptions,
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
    /// A `--no-session` run: its transcripts are deleted when it ends.
    ephemeral: bool,
    /// `--model` and `--thinking` were given: they win over what a reopened session last used.
    model_flag: bool,
    thinking_flag: bool,
    /// The session mode changes are recorded in.
    current: Arc<RwLock<Option<Arc<SessionStore>>>>,
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
}

pub enum Open {
    New,
    /// An existing session file.
    File(PathBuf),
    /// A store already in memory (a fork, or a session whose leaf was moved).
    Store(Arc<SessionStore>),
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

pub(crate) fn config_overrides(cli: &Cli) -> Result<toml::Table> {
    let mut overrides = toml::Table::new();
    for assignment in &cli.set {
        set_override(&mut overrides, assignment).map_err(|e| UsageError(format!("--set {e}")))?;
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

/// The MCP servers configured for this run: those of installed packages, then the user's.
/// Project files count only for a trusted project.
#[cfg(feature = "mcp")]
pub fn mcp_manager(
    paths: &Paths,
    trusted: bool,
    config: &Config,
    services: Arc<Services>,
    credentials: Arc<Credentials>,
    package_files: &[PathBuf],
    approve_all: bool,
) -> azazel_mcp::McpManager {
    let mut files = package_files.to_vec();
    files.extend(azazel_mcp::config::config_files(paths, trusted));
    let mut loaded = azazel_mcp::config::load(&files, &paths.project_root, &|name| std::env::var(name).ok());
    // Each server the project defines waits for the user's word; `--trust` gives it for this run.
    let trust = azazel_core::TrustStore::new(paths.trust_file());
    azazel_mcp::config::apply_approvals(&mut loaded, paths, &trust, approve_all);
    let mut roots = vec![paths.project_root.clone()];
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
    let mut paths = Paths::discover(&cwd, std::env::var("AZAZEL_HOME").ok().as_deref());
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
            overrides: config_overrides(cli)?,
            env: std::env::vars().collect(),
        };
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
        let permissions = Arc::new(PermissionState::new(&paths, &loaded).map_err(|e| anyhow!(e))?);
        let hook_sources = azazel_core::hooks::hook_sources(&loaded.layers);
        let ignored_settings = loaded.ignored;
        let mut config = loaded.config;

        if let Some(name) = &cli.preset {
            crate::routing::launch_preset(&mut config, name)
                .map_err(|e| UsageError(format!("--preset: {e}")))?;
        }
        let tier = match &cli.tier {
            Some(tier) => crate::routing::parse_tier(tier).map_err(|e| UsageError(format!("--tier: {e}")))?,
            None => None,
        };

        let credentials = Arc::new(credentials(&paths, trusted).configured(&config.providers));
        let thinking = match &cli.thinking {
            Some(level) => level
                .parse::<ThinkingLevel>()
                .map_err(|e| anyhow!("--thinking: {e}"))?,
            None => ThinkingLevel::Off,
        };

        let services = Arc::new(Services::new());
        services.insert(Arc::new(Ui(Arc::new(Headless))));
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
        registry.load(&BasePromptExt {
            cwd: paths.cwd.clone(),
            project_root: paths.project_root.clone(),
            shell: shell.name().to_string(),
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
        registry.load(&CoreToolsExt {
            shell: shell.clone(),
            shell_timeout: Duration::from_secs(config.tools.shell_timeout_secs),
            artifacts_dir: paths.artifacts_dir(),
        })?;
        #[cfg(feature = "lsp")]
        registry.load(&azazel_tools::lsp::LspExt {
            root: paths.project_root.clone(),
            servers: config.lsp.servers.clone(),
        })?;
        registry.load(&azazel_tools::web::WebExt {
            search: config.web.search.clone(),
            searxng_url: config.web.searxng_url.clone(),
        })?;
        registry.load(&crate::login::LoginExt {
            providers: crate::login::key_providers(&config).map_err(|e| anyhow!(e))?,
            credentials: credentials.clone(),
        })?;
        registry.load(&crate::settings::SettingsExt {
            providers: presets::resolve(&config.providers).map_err(|e| anyhow!(e))?,
            credentials: credentials.clone(),
            home_config: paths.home_config(),
            config: config.clone(),
        })?;
        registry.load(&crate::accounts::AccountsExt {
            providers: crate::login::key_providers(&config).map_err(|e| anyhow!(e))?,
            credentials: credentials.clone(),
            home_config: paths.home_config(),
        })?;
        registry.load(&crate::routing::RoutingExt {
            presets: config.presets.clone(),
            home_config: paths.home_config(),
        })?;
        registry.load(&PermissionsExt {
            state: permissions.clone(),
        })?;
        // Rule files count as context files; `[[stream_rules]]` are settings and always apply.
        let rules = Arc::new(Rules::new(rules::Sources {
            paths: paths.clone(),
            files: !cli.skips(Discovery::ContextFiles),
            claude_compat: compat,
            compat: config.rules.compat,
            stream_rules: config.stream_rules.clone(),
        }));
        registry.load(&RulesExt { rules: rules.clone() })?;
        if !cli.skips(Discovery::ContextFiles) {
            registry.load(&ContextExt {
                paths: paths.clone(),
                claude_compat: compat,
                rules: Some(rules),
            })?;
        }
        if !cli.skips(Discovery::Skills) {
            registry.load(&SkillsExt {
                roots: standard_roots(&paths, compat, &packages),
                trusted,
            })?;
        }
        registry.load(&CustomCommandsExt {
            roots: command_roots(&paths, compat, &packages),
            shell: shell.clone(),
            cwd: paths.cwd.clone(),
            trusted,
        })?;
        let mut agent_defs = agents::discover(
            azazel_team::BUILTIN_AGENTS,
            &agents::def_roots(&paths, compat, &packages),
        );
        agent_defs.apply_overrides(&config.agents);
        let agent_defs = Arc::new(agent_defs);
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
        })?;
        #[cfg(feature = "screen")]
        registry.load(&azazel_browser::screen::ScreenExt)?;
        #[cfg(feature = "media")]
        registry.load(&azazel_media::MediaExt {
            media: Arc::new(azazel_media::Media::from_config(&config, &paths, credentials.clone()).await),
        })?;
        #[cfg(feature = "memory")]
        if config.memory.enabled && !cli.skips(Discovery::Memory) {
            let memory = azazel_memory::MemoryExt::open(&paths, &config, credentials.clone())
                .map_err(|e| anyhow!("memory: {e} (set `[memory] enabled = false` to start without it)"))?;
            registry.load(&memory)?;
        }
        #[cfg(feature = "mcp")]
        if !cli.skips(Discovery::Mcp) {
            registry.load(&azazel_mcp::McpExt {
                manager: mcp_manager(
                    &paths,
                    trusted,
                    &config,
                    services.clone(),
                    credentials.clone(),
                    &packages.mcp_files,
                    cli.trust,
                ),
            })?;
        }
        // After MCP so scripts can add servers, and before the router so they can add providers.
        #[cfg(feature = "lua")]
        if !cli.skips(Discovery::Extensions) {
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
                packages.scripts.clone(),
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
            )));
        }
        select_tools(cli, &config, &registry)?;
        // Every process an agent starts inherits the push guard.
        if let Some(env) = services.get::<AgentEnv>() {
            env.set(guard_env(&|name| std::env::var(name).ok()));
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
                let provider =
                    HttpProvider::new(p, client.clone(), credentials.clone()).with_declared(declared);
                Arc::new(provider) as Arc<dyn Provider>
            })
            .collect();
        #[cfg(feature = "bedrock")]
        {
            let settings = config.providers.get(presets::BEDROCK);
            providers.push(Arc::new(azazel_ai::providers::bedrock::BedrockProvider::new(
                settings.and_then(|s| s.region.clone()),
                settings.and_then(|s| s.profile.clone()),
            )));
        }
        #[cfg(feature = "vertex")]
        {
            let settings = config.providers.get(presets::VERTEX);
            providers.push(Arc::new(azazel_ai::providers::vertex::VertexProvider::new(
                settings.and_then(|s| s.project.clone()),
                settings.and_then(|s| s.location.clone()),
                client.clone(),
            )));
        }
        providers.extend(registry.providers());
        providers.extend(mock_provider(&mut config)?);
        let provider_ids: Vec<String> = providers.iter().map(|p| p.id().to_string()).collect();
        let policy = RetryPolicy {
            max_attempts: config.retry.max_attempts.max(1),
            cooldown: Duration::from_secs(config.retry.cooldown_secs),
            ..RetryPolicy::default()
        };
        let router = Arc::new(
            Router::new(providers, catalog, config.roles.clone(), policy).with_accounts(credentials.clone()),
        );
        router.set_tier(tier);
        // Models named on the command line are this run's `default` role, so that the roles
        // built on it (`smol`, `verify`, ...) and the agents using them have a model too.
        let chain = if cli.model.is_empty() {
            vec![ModelSpec::Role("lead".to_string())]
        } else {
            let (known, usable) =
                known_and_usable(provider_ids, &endpoints, &credentials, cli.api_key.is_some());
            let chain = crate::models::resolve_specs(&cli.model, router.catalog(), &known, &usable)
                .map_err(UsageError)?;
            if chain.iter().all(|spec| matches!(spec, ModelSpec::Model(_))) {
                config.roles.insert("default".to_string(), chain.clone());
                router.set_role("default", chain.clone());
            }
            chain
        };
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
            defs: agent_defs,
            max_parallel: config.team.max_parallel,
            max_depth: config.team.max_depth,
            max_result_bytes: config.tools.max_result_bytes,
            compaction: (&config.compaction).into(),
            posix_shell: shell.is_posix(),
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
        let lead_tools = Arc::new(LeadTools::new(
            registry.tools.clone(),
            coordinator.clone(),
            report.clone(),
        ));
        // A script run for the lead reaches what the lead is offered, and no more.
        if let Some(caller) = services.get::<ToolCaller>() {
            caller.scope_agent("lead", lead_tools.clone());
        }
        let side_costs = Arc::new(SideCosts::default());
        services.insert(Arc::new(Side {
            router: router.clone(),
            prompt: registry.prompt.clone(),
            tools: registry.tools.clone(),
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
            hooks_disabled,
            coordinator,
            report,
            lead_tools,
            coordinator_forced: AtomicBool::new(cli.coordinator),
            ephemeral: cli.no_session,
            model_flag: !cli.model.is_empty(),
            thinking_flag: cli.thinking.is_some(),
            current,
        })
    }

    /// The session the command line asks for: `--resume <id>`, `--continue`, or a new one.
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
        if cli.resume_latest {
            return match SessionStore::list(&dir).first() {
                Some(latest) => Ok(Open::File(latest.path.clone())),
                None => bail!("this project has no session to continue"),
            };
        }
        Ok(Open::New)
    }

    /// Model names as typed (`fable`, `@smol`, `anthropic/claude-fable-5:high`) turned into a chain,
    /// the way `--model` resolves them.
    pub fn model_specs(&self, texts: &[String]) -> Result<Vec<ModelSpec>, String> {
        let endpoints = presets::resolve(&self.config.providers)?;
        let (known, usable) = known_and_usable(
            self.router
                .provider_ids()
                .into_iter()
                .map(str::to_string)
                .collect(),
            &endpoints,
            &self.credentials,
            false,
        );
        crate::models::resolve_specs(texts, self.router.catalog(), &known, &usable)
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
        let matches: Vec<_> = SessionStore::list(&self.paths.sessions_dir())
            .into_iter()
            .filter(|s| s.id.starts_with(wanted))
            .collect();
        match matches.as_slice() {
            [one] => Ok(one.path.clone()),
            [] => bail!("no session with id `{wanted}` in this project"),
            many => bail!("`{wanted}` matches {} sessions; give more of the id", many.len()),
        }
    }

    /// Whether `chain` names a model this process can use: a configured one whose provider needs
    /// no key or has a credential (the cloud and script providers find their own).
    pub fn has_model(&self, chain: &[ModelSpec]) -> bool {
        let Ok(endpoints) = presets::resolve(&self.config.providers) else {
            return false;
        };
        self.router.resolve(chain).iter().any(|model| {
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
        let level = self.thinking.to_string();
        if self.thinking_flag && session.last_thinking(None).as_ref() != Some(&level) {
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
        thinking: ThinkingLevel,
    ) -> (Vec<ModelSpec>, ThinkingLevel, Option<String>) {
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
            .filter(|_| !self.thinking_flag)
            .and_then(|level| level.parse().ok())
            .unwrap_or(thinking);
        (chain, thinking, notice)
    }

    /// Opens a session and starts its lead agent on `chain`. A session read from its file comes
    /// back on the model and thinking level it last used instead (see [`App::as_left`]).
    pub async fn open(&self, how: Open, chain: Vec<ModelSpec>, thinking: ThinkingLevel) -> Result<Active> {
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
        let (chain, thinking, notice) = if from_file {
            self.as_left(&session, chain, thinking)
        } else {
            (chain, thinking, None)
        };
        if resumed {
            crate::accounts::restore(&session, &self.credentials);
        }
        let session_id = session.id();
        crate::orchestration::open_session(
            &self.coordinator,
            &session,
            self.coordinator_forced.swap(false, Ordering::SeqCst),
        );
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Some(session.clone());
        // Extensions that need the transcript (hooks) find the open session here.
        self.registry.services.insert(session.clone());
        self.registry.bus.set_session(&session_id);
        let checkpoints = Arc::new(Checkpoints::new(
            &self.paths,
            &session_id,
            &self.config.checkpoints,
        ));

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
        if let Some(goals) = services.get::<Goals>() {
            goals.attach(lead.clone(), session.clone(), checkpoints.clone());
        }
        if let Some(advisor) = services.get::<Advisor>() {
            advisor.attach(&lead, session.clone(), checkpoints.clone());
        }
        Ok(Active {
            session,
            lead,
            checkpoints,
            resumed,
            notice,
        })
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

    /// Sends a user prompt to the lead: extensions may rewrite or reject it, mentioned files are
    /// attached, the project tree is checkpointed, then the lead starts. `Err` carries the reason
    /// an extension rejected the prompt.
    pub async fn submit(&self, active: &Active, text: String, images: Vec<Block>) -> Result<(), String> {
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
        let text = mentions.text;
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
        active.lead.prompt(UserInput { content });
        Ok(())
    }

    /// Stops a session's lead without ending the process-wide session state.
    pub async fn close(&self, active: &Active) {
        if let Some(team) = self.registry.services.get::<Team>() {
            team.detach().await;
        }
        active.lead.shutdown().await;
    }

    /// Ends the last session: tells extensions, stops the lead and background jobs.
    pub async fn finish(&self, active: &Active) {
        // A name being chosen in the background is written before the session closes.
        if let Some(namer) = self.registry.services.get::<Namer>() {
            namer.settle().await;
        }
        self.registry.bus.emit(&mut Event::SessionEnd).await;
        self.close(active).await;
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
    }

    pub fn log_file(paths: &Paths) -> PathBuf {
        paths.logs_dir().join("azazel.log")
    }
}
