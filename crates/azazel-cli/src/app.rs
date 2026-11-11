//! Assembles the process-wide pieces (config, providers, extensions, permissions) and opens
//! sessions on top of them. Every mode starts here.

use std::path::PathBuf;
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
use azazel_core::context::ContextExt;
use azazel_core::hooks::HooksExt;
use azazel_core::packages::Packages;
use azazel_core::permissions::plan::PermissionsExt;
use azazel_core::permissions::shell::guard_env;
use azazel_core::process::{AgentEnv, Shell};
use azazel_core::prompt::BasePromptExt;
use azazel_core::skills::{SkillsExt, standard_roots};
use azazel_core::{
    ArtifactStore, Config, EntryKind, Event, ExtCtx, Extension, FileTracker, Headless, Level, LoadOptions,
    Outcome, Paths, PermissionGate, PermissionMode, PermissionState, Registry, SessionStore, StartSource, Ui,
};
use azazel_team::{Checker, GoalExt, GoalSettings, Goals, Team, TeamDeps, TeamExt};
use azazel_tools::CoreToolsExt;

use crate::cli::Cli;

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
}

pub enum Open {
    New,
    /// An existing session file.
    File(PathBuf),
    /// A store already in memory (a fork, or a session whose leaf was moved).
    Store(Arc<SessionStore>),
}

/// The chain the lead starts with: `--model` flags, else the `default` role.
fn lead_chain(cli: &Cli) -> Result<Vec<ModelSpec>> {
    if cli.model.is_empty() {
        return Ok(vec![ModelSpec::Role("default".to_string())]);
    }
    cli.model
        .iter()
        .map(|spec| {
            spec.parse::<ModelSpec>()
                .map_err(|e| anyhow!("--model {spec}: {e}"))
        })
        .collect()
}

fn config_overrides(cli: &Cli) -> Result<toml::Table> {
    let mut overrides = toml::Table::new();
    if let Some(mode) = &cli.mode {
        let mode: PermissionMode = mode.parse().map_err(|e: String| anyhow!("--mode: {e}"))?;
        let mut permissions = toml::Table::new();
        permissions.insert("mode".to_string(), toml::Value::String(mode.to_string()));
        overrides.insert("permissions".to_string(), toml::Value::Table(permissions));
    }
    Ok(overrides)
}

/// The scripted provider, when this is a test build and a script is given.
#[cfg(feature = "test-support")]
fn mock_provider(config: &mut Config) -> Result<Option<Arc<dyn Provider>>> {
    let Some(script) = std::env::var_os("AZAZEL_MOCK_SCRIPT") else {
        return Ok(None);
    };
    let provider = azazel_ai::testing::MockProvider::from_script_file(std::path::Path::new(&script))
        .map_err(|e| anyhow!("AZAZEL_MOCK_SCRIPT: {e}"))?;
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
) -> azazel_mcp::McpManager {
    let mut files = package_files.to_vec();
    files.extend(azazel_mcp::config::config_files(paths, trusted));
    let loaded = azazel_mcp::config::load(&files, &paths.project_root, &|name| std::env::var(name).ok());
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
    azazel_mcp::McpManager::new(loaded, services, roots, credentials, options)
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
    Ok(Paths::discover(
        &cwd,
        std::env::var("AZAZEL_HOME").ok().as_deref(),
    ))
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
        let loaded = Config::load(&paths, &options)?;
        let permissions = Arc::new(PermissionState::new(&paths, &loaded).map_err(|e| anyhow!(e))?);
        let ignored_settings = loaded.ignored;
        let mut config = loaded.config;

        let credentials = Arc::new(Credentials::new(paths.auth_file()));
        let chain = lead_chain(cli)?;
        // Models named on the command line are this run's `default` role, so that the roles
        // built on it (`smol`, `verify`, …) and the agents using them have a model too.
        if !cli.model.is_empty() && chain.iter().all(|spec| matches!(spec, ModelSpec::Model(_))) {
            config.roles.insert("default".to_string(), chain.clone());
        }
        if let (Some(key), Some(ModelSpec::Model(model))) = (&cli.api_key, chain.first()) {
            credentials.set_flag_key(&model.provider, key.clone());
        }
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
        })?;
        registry.load(&PermissionsExt {
            state: permissions.clone(),
        })?;
        registry.load(&ContextExt {
            paths: paths.clone(),
            claude_compat: compat,
        })?;
        registry.load(&SkillsExt {
            roots: standard_roots(&paths, compat, &packages),
        })?;
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
        registry.load(&HooksExt {
            hooks: config.hooks.clone(),
            shell: shell.clone(),
            paths: paths.clone(),
            claude_compat: compat,
            disabled: std::env::var_os("AZAZEL_HOOKS_DISABLED").is_some(),
            trusted,
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
        if config.memory.enabled {
            let memory = azazel_memory::MemoryExt::open(&paths, &config, credentials.clone())
                .map_err(|e| anyhow!("memory: {e} (set `[memory] enabled = false` to start without it)"))?;
            registry.load(&memory)?;
        }
        #[cfg(feature = "mcp")]
        registry.load(&azazel_mcp::McpExt {
            manager: mcp_manager(
                &paths,
                trusted,
                &config,
                services.clone(),
                credentials.clone(),
                &packages.mcp_files,
            ),
        })?;
        // After MCP so scripts can add servers, and before the router so they can add providers.
        #[cfg(feature = "lua")]
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
        for extension in extensions {
            registry
                .load(extension.as_ref())
                .with_context(|| format!("extension `{}` failed to register", extension.id()))?;
        }
        registry.tools.set_disabled(config.tools.disabled.clone());
        // Every process an agent starts inherits the push guard.
        if let Some(env) = services.get::<AgentEnv>() {
            env.set(guard_env(&|name| std::env::var(name).ok()));
        }

        let client = http_client();
        let resolved = presets::resolve(&config.providers).map_err(|e| anyhow!(e))?;
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
        let mut providers: Vec<Arc<dyn Provider>> = resolved
            .into_iter()
            .map(|p| Arc::new(HttpProvider::new(p, client.clone(), credentials.clone())) as Arc<dyn Provider>)
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
        let catalog = Catalog::load(Some(&paths.models_cache()), &config.models);
        let policy = RetryPolicy {
            max_attempts: config.retry.max_attempts.max(1),
            cooldown: Duration::from_secs(config.retry.cooldown_secs),
            ..RetryPolicy::default()
        };
        let router = Arc::new(Router::new(providers, catalog, config.roles.clone(), policy));
        // Extensions that need a model of their own (MCP sampling) find the router here.
        services.insert(router.clone());
        services.insert(Arc::new(Team::new(TeamDeps {
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
        })));
        // The interactive UI starts anyway and walks the user through choosing a model; every
        // other mode needs one before it can do anything.
        let interactive = cli.print.is_none() && cli.command.is_none();
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
            current,
        })
    }

    /// The session the command line asks for: `--resume <id>`, `--continue`, or a new one.
    pub fn requested_session(&self, cli: &Cli) -> Result<Open> {
        let dir = self.paths.sessions_dir();
        if let Some(wanted) = &cli.resume {
            let matches: Vec<_> = SessionStore::list(&dir)
                .into_iter()
                .filter(|s| s.id.starts_with(wanted))
                .collect();
            return match matches.as_slice() {
                [one] => Ok(Open::File(one.path.clone())),
                [] => bail!("no session with id `{wanted}` in this project"),
                many => bail!("`{wanted}` matches {} sessions; give more of the id", many.len()),
            };
        }
        if cli.resume_latest {
            return match SessionStore::list(&dir).first() {
                Some(latest) => Ok(Open::File(latest.path.clone())),
                None => bail!("this project has no session to continue"),
            };
        }
        Ok(Open::New)
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

    /// Makes `model` the default for this process and for every later session: written to the
    /// home config as `roles.default`, and given to the router so the roles built on it resolve.
    pub fn set_default_model(&self, model: &str) -> Result<()> {
        let spec: ModelSpec = model.parse().map_err(|e| anyhow!("{model}: {e}"))?;
        crate::init::write_default_role(&self.paths.home_config(), model)?;
        self.router.set_role("default", vec![spec]);
        Ok(())
    }

    /// Opens a session and starts its lead agent on `chain`.
    pub async fn open(&self, how: Open, chain: Vec<ModelSpec>, thinking: ThinkingLevel) -> Result<Active> {
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
        let session_id = session.id();
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
            tools: self.registry.tools.clone(),
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
                max_turns: None,
                max_result_bytes: self.config.tools.max_result_bytes,
            },
            compaction: (&self.config.compaction).into(),
        });
        if let Some(team) = services.get::<Team>() {
            team.attach(lead.clone(), session.clone());
        }
        if let Some(goals) = services.get::<Goals>() {
            goals.attach(lead.clone(), session.clone(), checkpoints.clone());
        }
        Ok(Active {
            session,
            lead,
            checkpoints,
            resumed,
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
        let (text, mentioned) = expand_mentions(&text, &self.paths.cwd);
        if let Some(tracker) = self.registry.services.get::<FileTracker>() {
            for (path, bytes) in &mentioned {
                tracker.record(&active.lead.id, path, bytes);
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
        self.registry.bus.emit(&mut Event::SessionEnd).await;
        self.close(active).await;
        if let Some(jobs) = self.registry.services.get::<azazel_tools::Jobs>() {
            jobs.kill_all();
        }
        #[cfg(feature = "mcp")]
        if let Some(mcp) = self.registry.services.get::<azazel_mcp::McpManager>() {
            mcp.shutdown();
        }
    }

    pub fn log_file(paths: &Paths) -> PathBuf {
        paths.logs_dir().join("azazel.log")
    }
}
