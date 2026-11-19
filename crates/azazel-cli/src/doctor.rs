//! `azazel doctor` and `/doctor`: what the harness found on this machine and in this project.

use std::time::Duration;

use azazel_ai::providers::presets;
use azazel_ai::{Catalog, ModelSpec, RetryPolicy, Router};
use azazel_core::checkpoint::Checkpoints;
use azazel_core::config::AdvisorWhen;
use azazel_core::packages::Packages;
use azazel_core::process::Shell;
use azazel_core::skills::{discover, standard_roots};
use azazel_core::trust::needs_trust;
use azazel_core::{Config, LoadOptions, Paths, TrustStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
}

impl Report {
    fn add(&mut self, severity: Severity, text: impl Into<String>) {
        self.findings.push(Finding {
            severity,
            text: text.into(),
        });
    }

    fn ok(&mut self, text: impl Into<String>) {
        self.add(Severity::Ok, text);
    }

    fn warn(&mut self, text: impl Into<String>) {
        self.add(Severity::Warn, text);
    }

    fn error(&mut self, text: impl Into<String>) {
        self.add(Severity::Error, text);
    }

    pub fn has_errors(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Error)
    }

    /// One finding per line, each led by `ok`, `warn` or `FAIL`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for finding in &self.findings {
            let mark = match finding.severity {
                Severity::Ok => "ok  ",
                Severity::Warn => "warn",
                Severity::Error => "FAIL",
            };
            out.push_str(&format!("{mark}  {}\n", finding.text));
        }
        out
    }
}

/// Looks at the installation and the project `paths` points at. `trusted` is whether this run
/// treats the project as trusted.
pub async fn examine(paths: &Paths, trusted: bool) -> Report {
    let mut report = Report::default();
    report.ok(format!("azazel {}", env!("CARGO_PKG_VERSION")));
    report.ok(format!("home: {}", paths.home.display()));
    report.ok(format!("project root: {}", paths.project_root.display()));

    let options = LoadOptions {
        trusted,
        overrides: toml::Table::new(),
        env: std::env::vars().collect(),
    };
    let config = match Config::load(paths, &options) {
        Ok(loaded) => {
            if loaded.layers.is_empty() {
                report.ok("config: no files, defaults in use");
            }
            for layer in &loaded.layers {
                report.ok(format!("config: loaded {}", layer.path.display()));
            }
            for file in &loaded.models_files {
                report.ok(format!(
                    "models: loaded {} ({} providers, {} models)",
                    file.path.display(),
                    file.providers,
                    file.models
                ));
            }
            for skipped in &loaded.ignored {
                report.warn(format!(
                    "config: skipped because the project is not trusted: {skipped}"
                ));
            }
            loaded.config
        }
        Err(e) => {
            report.error(format!("config: {e}"));
            Config::default()
        }
    };

    let reasons = needs_trust(paths, config.compat.claude, &config.tools);
    match (
        TrustStore::new(paths.trust_file()).decision(&paths.project_root),
        trusted,
    ) {
        (_, true) => report.ok("trust: this project is trusted"),
        (Some(false), _) => report.warn("trust: you declined to trust this project"),
        _ if reasons.is_empty() => report.ok("trust: nothing in this project needs it"),
        _ => report.warn(format!(
            "trust: not decided; the project has {}",
            reasons.join(", ")
        )),
    }

    let credentials = crate::app::credentials(paths, trusted).configured(&config.providers);
    match presets::resolve(&config.providers) {
        Ok(providers) => {
            let (mut with_key, mut local, mut without) = (0, Vec::new(), Vec::new());
            for provider in &providers {
                let id = provider.id.as_str();
                let endpoint = format!("{} at {}", provider.api, provider.base_url);
                // A provider the configuration adds is always shown: it is there on purpose.
                let added = !presets::is_builtin(id);
                match (credentials.source(provider), provider.unset_variable()) {
                    (Some(source), Some(variable)) => {
                        with_key += 1;
                        report.warn(format!(
                            "provider {id}: credential from {source}, but its address needs {variable} set"
                        ));
                    }
                    (Some(source), None) => {
                        with_key += 1;
                        report.ok(format!("provider {id}: credential from {source} ({endpoint})"));
                    }
                    (None, _) if !provider.requires_key && added => {
                        report.ok(format!("provider {id}: no key needed ({endpoint})"));
                    }
                    (None, _) if !provider.requires_key => local.push(id),
                    (None, _) if added => {
                        let how = match provider.api_key_env.first() {
                            Some(variable) => format!("set {variable}"),
                            None => format!("run `azazel login {id}`"),
                        };
                        report.warn(format!("provider {id}: no credential, {how} ({endpoint})"));
                    }
                    (None, _) => without.push(id),
                }
            }
            if !local.is_empty() {
                report.ok(format!(
                    "providers needing no key (local servers): {}",
                    local.join(", ")
                ));
            }
            if with_key == 0 {
                report.warn("providers: no API key found; only local servers can be used");
            }
            if !without.is_empty() {
                // Hundreds of names bury the report; the models listing and /login name them.
                report.ok(format!(
                    "providers without a credential: {} (`azazel login` signs in to one; `azazel models --all` lists the models of every provider)",
                    without.len()
                ));
            }
        }
        Err(e) => report.error(format!("providers: {e}")),
    }
    // The cloud providers use the platform's own credentials, looked up when a model is first used.
    #[cfg(feature = "bedrock")]
    {
        let settings = config.providers.get(presets::BEDROCK);
        let region = settings
            .and_then(|s| s.region.as_deref())
            .unwrap_or("from the AWS configuration");
        report.ok(format!(
            "provider bedrock: AWS default credential chain, region {region}"
        ));
    }
    #[cfg(feature = "vertex")]
    {
        let settings = config.providers.get(presets::VERTEX);
        let project = settings
            .and_then(|s| s.project.as_deref())
            .unwrap_or("from the environment or the credentials");
        let location = settings
            .and_then(|s| s.location.as_deref())
            .unwrap_or(azazel_ai::providers::vertex::DEFAULT_LOCATION);
        report.ok(format!(
            "provider vertex: Google application default credentials, project {project}, location {location}"
        ));
    }

    let router = Router::new(
        Vec::new(),
        Catalog::default(),
        config.roles.clone(),
        RetryPolicy::default(),
    );
    if !config.roles.contains_key("default") {
        report.error("role default: not set; run `azazel init` or pass --model provider/id");
    }
    for role in config.roles.keys() {
        let chain: Vec<String> = router
            .resolve(&[ModelSpec::Role(role.clone())])
            .iter()
            .map(ToString::to_string)
            .collect();
        if chain.is_empty() {
            if role != "default" && config.roles.contains_key("default") {
                report.warn(format!("role {role}: resolves to no model"));
            }
        } else {
            report.ok(format!("role {role}: {}", chain.join(", ")));
        }
    }
    if config.limits.max_cost_usd > 0.0 {
        let catalog = Catalog::load(Some(&paths.models_cache()), &config.models);
        let unpriced: Vec<String> = router
            .resolve(&[ModelSpec::Role("default".to_string())])
            .iter()
            .filter(|model| catalog.get(&model.provider, &model.model).cost.is_none())
            .map(ToString::to_string)
            .collect();
        if unpriced.is_empty() {
            report.ok(format!("cost limit: ${:.2} per run", config.limits.max_cost_usd));
        } else {
            report.warn(format!(
                "cost limit: ${:.2} is set, but {} has no known price and never counts towards it",
                config.limits.max_cost_usd,
                unpriced.join(", ")
            ));
        }
    }

    let advisor: Vec<String> = router
        .resolve(&config.advisor.model)
        .iter()
        .map(ToString::to_string)
        .collect();
    match (config.advisor.when, advisor.is_empty()) {
        (AdvisorWhen::Stop, true) => report.warn(
            "advisor: `when = \"stop\"` is set, but [advisor] model names no model; nothing is reviewed",
        ),
        (AdvisorWhen::Stop, false) => report.ok(format!(
            "advisor: {} reviews the lead's work each time it is about to stop",
            advisor.join(", ")
        )),
        (AdvisorWhen::Off, false) => report.ok(format!("advisor: {} answers /advise", advisor.join(", "))),
        (AdvisorWhen::Off, true) => {}
    }
    if config.team.coordinator {
        report.ok("coordinator mode: on for new sessions (the lead plans and delegates)");
    }

    let shell = Shell::resolve(&config.tools.shell);
    report.ok(format!("shell: {} ({})", shell.name(), shell.program.display()));

    match which::which("git") {
        Ok(git) => {
            report.ok(format!("git: {}", git.display()));
            match Checkpoints::new(paths, "doctor", &config.checkpoints)
                .disabled()
                .await
            {
                None => report.ok("checkpoints: on"),
                Some(reason) => report.warn(format!("checkpoints: off, {reason}")),
            }
        }
        Err(_) => report.warn("git: not found on PATH; checkpoints and rewind are off"),
    }
    match azazel_tools::schemes::gh_program() {
        Some(gh) => report.ok(format!(
            "gh: {}; `read` takes pr:// and issue:// addresses, GitHub URLs are read with its login when it is \
             signed in, and /share and /bug upload through it",
            gh.display()
        )),
        None => report.ok(
            "gh: not found on PATH; pr:// and issue:// addresses, /share and the issue of /bug are off, and \
             GitHub URLs are read without a login",
        ),
    }

    let store = Packages::new(paths);
    let packages = store.load();
    match (&packages.problem, store.list()) {
        (Some(why), _) => report.warn(format!("packages: not loaded, {why}")),
        (None, Ok(installed)) if !installed.is_empty() => {
            let names: Vec<String> = installed
                .iter()
                .map(|package| match package.code_enabled {
                    true => format!("{} (code enabled)", package.name),
                    false => package.name.clone(),
                })
                .collect();
            report.ok(format!("packages: {}", names.join(", ")));
        }
        _ => {}
    }
    #[cfg(feature = "lua")]
    for line in tool_overrides(paths, trusted, &config, &packages) {
        report.ok(line);
    }
    let skills = discover(&standard_roots(paths, config.compat.claude, &packages));
    report.ok(format!("skills: {} found", skills.skills.len()));
    for invalid in &skills.invalid {
        report.warn(format!(
            "skill skipped: {}: {}",
            invalid.path.display(),
            invalid.error
        ));
    }
    {
        use azazel_core::rules::{Sources, load};
        let rules = load(&Sources {
            paths: paths.clone(),
            files: true,
            claude_compat: config.compat.claude,
            compat: config.rules.compat,
            stream_rules: config.stream_rules.clone(),
            include_depth: config.context.include_depth,
        });
        report.ok(format!("rules: {} found", rules.rules.len()));
        for (file, problem) in &rules.problems {
            report.warn(format!("rule skipped: {file}: {problem}"));
        }
    }
    #[cfg(feature = "mcp")]
    {
        use azazel_mcp::config::{Approval, Lifecycle, Transport, apply_approvals, config_files, load};
        let mut files = packages.mcp_files.clone();
        files.extend(config_files(paths, trusted));
        let mut loaded = load(&files, &paths.project_root, &|name| std::env::var(name).ok());
        apply_approvals(&mut loaded, paths, &TrustStore::new(paths.trust_file()), false);
        for server in &loaded.servers {
            let target = match &server.transport {
                Transport::Stdio { command, .. } => command,
                Transport::Http { url, .. } => url,
            };
            let state = match (server.enabled, server.lifecycle) {
                (false, _) => "disabled",
                (true, Lifecycle::Eager) => "enabled",
                (true, Lifecycle::Lazy | Lifecycle::LazyKeepAlive) => "enabled, starts on first use",
            };
            match server.approval {
                Approval::Granted => report.ok(format!("mcp server {}: {target} ({state})", server.name)),
                Approval::Waiting => report.warn(format!(
                    "mcp server {}: {target} (waiting for approval: azazel mcp approve {})",
                    server.name, server.name
                )),
                Approval::Rejected => report.ok(format!("mcp server {}: {target} (rejected)", server.name)),
            }
        }
        for (what, why) in &loaded.problems {
            report.warn(format!("mcp {what}: not loaded, {why}"));
        }
    }
    #[cfg(feature = "browser")]
    match azazel_browser::find_binary(&config.browser.path, &|name| std::env::var(name).ok()) {
        Some(binary) => report.ok(format!("browser: {}", binary.display())),
        None => report.warn(format!("browser: {}", azazel_browser::NOT_FOUND)),
    }
    // What `azazel secrets` stored: named, never shown.
    let stored = azazel_ai::ServiceStore::new(paths.secrets_file()).services();
    if !stored.is_empty() {
        let named = |sites: bool| -> Vec<&str> {
            let of_kind = stored
                .iter()
                .filter(|(_, service)| matches!(service, azazel_ai::Service::Site { .. }) == sites);
            of_kind.map(|(name, _)| name.as_str()).collect()
        };
        for (what, names) in [
            ("request credentials", named(false)),
            ("site logins", named(true)),
        ] {
            if !names.is_empty() {
                report.ok(format!("{what} for agents: {}", names.join(", ")));
            }
        }
    }
    let backend = azazel_tools::web::active_backend(&config.web.search, &config.web.searxng_url, &|name| {
        std::env::var(name).ok()
    });
    // The lead's provider answers first when it has a search tool of its own.
    let lead = router
        .resolve(&[ModelSpec::Role("default".to_string())])
        .into_iter()
        .next();
    let searches = |model: &azazel_ai::ModelRef| {
        presets::resolve(&config.providers)
            .unwrap_or_default()
            .iter()
            .any(|provider| provider.id == model.provider && azazel_ai::wire::hosted_api(provider).is_some())
    };
    let hosted = lead
        .as_ref()
        .filter(|model| searches(model))
        .map(azazel_ai::ModelRef::key);
    let provider =
        azazel_tools::web::ProviderSearch::from_config(&config.web.search, config.web.provider_search);
    match (provider, hosted, backend) {
        (azazel_tools::web::ProviderSearch::Only, Some(model), _) => {
            report.ok(format!("web search: provider ({model})"));
        }
        (azazel_tools::web::ProviderSearch::Only, None, _) => report.warn(format!(
            "web search: `provider` is set, but {} has no web search of its own, so every search fails",
            lead.map_or_else(|| "no model".to_string(), |model| model.key()),
        )),
        (azazel_tools::web::ProviderSearch::First, Some(model), name) => {
            report.ok(format!("web search: provider ({model}), then {name}"));
        }
        (_, _, "none") => report.warn(format!(
            "web search: `{}` is not a search backend",
            config.web.search
        )),
        (_, _, name) => report.ok(format!("web search: {name}")),
    }
    #[cfg(feature = "lsp")]
    {
        let places = azazel_tools::lsp::Places::system(paths.lsp_dir());
        let in_project = azazel_tools::lsp::extensions_in(&paths.project_root);
        let mut ready = Vec::new();
        for server in azazel_tools::lsp::known(&config.lsp.servers, &places) {
            let used = server
                .extensions
                .iter()
                .any(|extension| in_project.contains(extension));
            match (server.found.is_some(), server.builtin, used) {
                (true, _, _) => ready.push(server.name),
                // The defaults are optional, unless the project has files for them.
                (false, true, false) => {}
                (false, true, true) => {
                    let how = server.install.unwrap_or_default();
                    let how = if server.installable {
                        format!("`azazel lsp install {}` runs `{how}`", server.name)
                    } else {
                        how
                    };
                    report.warn(format!(
                        "lsp server `{}` is not installed, and this project has {} files: {how}",
                        server.name,
                        server
                            .extensions
                            .iter()
                            .map(|e| format!(".{e}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ));
                }
                (false, false, _) => {
                    report.warn(format!("lsp server `{}`: its command was not found", server.name))
                }
            }
        }
        if ready.is_empty() {
            report.ok("lsp: no language server is installed");
        } else {
            report.ok(format!("lsp: {}", ready.join(", ")));
        }
    }
    if config.tools.check_on_write {
        let installed = azazel_tools::check::installed(trusted);
        let named = |used: bool| -> Vec<&str> {
            let of_kind = installed.iter().filter(|(_, is_used)| *is_used == used);
            of_kind.map(|(checker, _)| checker.as_str()).collect()
        };
        match named(true) {
            ready if !ready.is_empty() => report.ok(format!("checks on write: {}", ready.join(", "))),
            _ if installed.is_empty() => {
                report.ok("checks on write: none of the built-in checkers is installed");
            }
            _ => {}
        }
        // These run the project's code (build scripts, lint configuration) to check a file.
        for checker in named(false) {
            report.ok(format!(
                "checks on write: {checker}: off, the project is not trusted"
            ));
        }
        for (kind, program) in azazel_tools::check::missing(&config.tools.check) {
            report.warn(format!("checker for `{kind}` files: `{program}` was not found"));
        }
    } else {
        report.ok("checks on write: off");
    }
    if config.tools.format_on_write {
        use azazel_tools::format::{Formatters, Planned};
        let formatters = Formatters::new(config.tools.format.clone(), paths.project_root.clone(), trusted);
        let mut used = Vec::new();
        for (extension, command) in config
            .tools
            .format
            .iter()
            .filter(|(_, command)| !command.trim().is_empty())
        {
            used.push(format!("{command} ({extension})"));
        }
        let mut untrusted = Vec::new();
        for (name, extensions, planned) in formatters.planned() {
            let kinds = match extensions.as_slice() {
                few if few.len() <= 4 => few.join(", "),
                many => format!("{}, … {} kinds", many[..3].join(", "), many.len()),
            };
            match planned {
                Planned::Used { .. } => used.push(format!("{name} ({kinds})")),
                // These run the project's code (a config file in JavaScript, its own copy of
                // the program) to format a file.
                Planned::Untrusted => untrusted.push(name),
            }
        }
        if used.is_empty() {
            report.ok("format on write: no formatter is installed that this project uses");
        } else {
            report.ok(format!("format on write: {}", used.join(", ")));
        }
        for name in untrusted {
            report.ok(format!(
                "format on write: {name}: off, the project is not trusted"
            ));
        }
        for (extension, program) in formatters.missing() {
            report.warn(format!(
                "formatter for `{extension}` files: `{program}` was not found"
            ));
        }
    } else {
        report.ok("format on write: off");
    }
    match azazel_tools::scan::scanner(config.tools.scan.secrets, config.tools.check_report, trusted) {
        Ok(gitleaks) => report.ok(format!("secret scan: {}", gitleaks.display())),
        Err(why) => report.ok(format!("secret scan: {why}")),
    }
    match azazel_tools::ast_grep::program() {
        Some(program) => report.ok(format!("ast_grep: {}", program.display())),
        None => report.ok("ast_grep: off, ast-grep is not installed"),
    }
    #[cfg(feature = "detach")]
    {
        use crate::schedule::{Store, os};
        let schedules = Store::of(paths).list();
        let (triggers, timed): (Vec<_>, Vec<_>) =
            schedules.iter().partition(|schedule| schedule.when.is_trigger());
        match (config.schedule.register, os::scheduler()) {
            (false, _) => report.ok(format!(
                "schedules: {} here, not handed to {} (`schedule.register` is off)",
                timed.len(),
                os::Kind::HERE.name()
            )),
            (true, Ok(_)) => report.ok(format!(
                "schedules: {} here, started by {}",
                timed.len(),
                os::Kind::HERE.name()
            )),
            (true, Err(why)) => report.warn(format!("schedules: {why}")),
        }
        let ci = triggers
            .iter()
            .any(|schedule| schedule.when == crate::schedule::when::When::CiFailure);
        let comments = triggers
            .iter()
            .any(|schedule| schedule.when == crate::schedule::when::When::PrComment);
        if ci && which::which("gh").is_err() {
            report.warn("trigger on ci-failure: `gh` is not on PATH, so CI runs cannot be looked at");
        } else if comments && which::which("gh").is_err() {
            report.warn("trigger on pr-comment: `gh` is not on PATH, so pull requests cannot be looked at");
        } else if !triggers.is_empty() {
            report.ok(format!(
                "triggers: {} here, watched while a session is open",
                triggers.len()
            ));
        }
    }
    let variable = |name: &str| std::env::var(name).ok();
    match crate::telemetry::Wire::from_config(&config.telemetry, &variable) {
        None => report.ok("trace export: off"),
        Some(wire) => {
            report.ok(format!(
                "trace export: {}, {} prompt and reply text",
                wire.describe(),
                if config.telemetry.content {
                    "with"
                } else {
                    "without"
                }
            ));
            let named = &config.telemetry.api_key_env;
            let keyless = wire.headers().is_empty();
            if !named.is_empty() && variable(named).is_none_or(|key| key.trim().is_empty()) {
                report.warn(format!(
                    "trace export: {named} (telemetry.api_key_env) is not set; spans go out without a key"
                ));
            } else if keyless && matches!(wire, crate::telemetry::Wire::Langsmith { .. }) {
                report.warn("trace export: no LangSmith key; set LANGSMITH_API_KEY or telemetry.api_key_env");
            }
        }
    }
    #[cfg(feature = "memory")]
    match &config.embeddings {
        _ if !config.memory.enabled => report.ok("memory: off"),
        Some(embeddings) => report.ok(format!(
            "memory: on, embeddings from {}/{}",
            embeddings.provider, embeddings.model
        )),
        None => report.ok("memory: on, word search only (no [embeddings] configured)"),
    }
    #[cfg(feature = "voice")]
    report
        .findings
        .extend(crate::voice::findings(&config, &credentials));
    report
}

/// The built-in tools the Luau scripts of this project and of the user replace or wrap, one line
/// each. The scripts are loaded the way a session loads them, into a registry of their own.
#[cfg(feature = "lua")]
fn tool_overrides(
    paths: &Paths,
    trusted: bool,
    config: &Config,
    packages: &azazel_core::packages::Loaded,
) -> Vec<String> {
    use std::sync::Arc;

    use azazel_agent::{AgentInfo, Services};
    use azazel_core::{ExtCtx, Headless, McpRegistrar, McpServers, Registry, Ui};

    /// Scripts may add MCP servers as they load; here nothing is started.
    struct NoServers;
    impl McpRegistrar for NoServers {
        fn add_server(&self, _: &str, _: serde_json::Value) -> Result<(), String> {
            Ok(())
        }
    }

    let services = Arc::new(Services::new());
    services.insert(Arc::new(Ui(Arc::new(Headless))));
    services.insert(Arc::new(McpServers(Arc::new(NoServers))));
    let registry = Registry::new(ExtCtx {
        services,
        cwd: paths.cwd.clone(),
        session_id: String::new(),
    });
    let scripts = azazel_lua::LuaExt::new(
        paths.clone(),
        trusted,
        Arc::new(crate::app::credentials(paths, trusted)),
        config
            .permissions
            .additional_dirs
            .iter()
            .map(|dir| paths.project_root.join(dir))
            .collect(),
        azazel_lua::Scripts {
            packaged: packages.scripts.clone(),
            ..azazel_lua::Scripts::default()
        },
    );
    if let Err(e) = registry.load(&scripts) {
        return vec![format!("extensions: not loaded, {e}")];
    }
    let lead = AgentInfo {
        id: "lead".into(),
        def: "lead".into(),
        parent: None,
        depth: 0,
        session_id: String::new(),
    };
    let who = crate::reports::overrider;
    let replaced = registry.tools.replacements(&lead).into_iter();
    let wrapped = registry.tools.wraps().into_iter();
    replaced
        .map(|(by, tool)| format!("tool `{tool}`: replaced by {}", who(&by)))
        .chain(wrapped.map(|(by, tool)| format!("tool `{tool}`: wrapped by {}", who(&by))))
        .collect()
}

/// Runs `azazel doctor`: prints the report; the exit code is 1 when a finding is an error.
pub async fn run(paths: &Paths, trusted: bool) -> u8 {
    // Reading credentials through a key command can take a moment; nothing else here blocks.
    let report = match tokio::time::timeout(Duration::from_secs(30), examine(paths, trusted)).await {
        Ok(report) => report,
        Err(_) => {
            eprintln!("azazel: doctor timed out");
            return 1;
        }
    };
    print!("{}", report.render());
    u8::from(report.has_errors())
}
