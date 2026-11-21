//! The `azazel` command-line harness: argument parsing, run modes and the embedding [`Builder`].

pub mod accounts;
#[cfg(feature = "acp")]
pub mod acp;
pub mod app;
mod attention;
mod branch;
#[cfg(feature = "browser")]
mod browser_cmd;
pub mod bug;
pub mod changelog;
pub mod cli;
mod config_cli;
pub mod context;
#[cfg(feature = "detach")]
pub mod detach;
#[cfg(feature = "diagrams")]
mod diagram;
mod digest;
pub mod doctor;
pub mod export;
mod fix;
pub mod git;
pub mod github;
pub mod handoff;
#[cfg(any(feature = "mcp", feature = "acp"))]
pub mod headless;
pub mod history;
pub mod hooks;
#[cfg(feature = "detach")]
pub mod host;
pub mod init;
pub mod inventory;
pub mod login;
pub mod loops;
#[cfg(feature = "lsp")]
mod lsp_cli;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "memory")]
pub mod memory;
pub mod migrate;
pub mod models;
pub mod orchestration;
pub mod pkg;
pub mod print;
pub mod profiles;
#[cfg(feature = "remote")]
pub mod remote;
pub mod reports;
pub mod resets;
pub mod resources;
mod restart;
pub mod retry;
pub mod roles;
mod roles_cli;
pub mod routing;
pub mod rpc;
pub mod rules;
pub mod running;
#[cfg(feature = "detach")]
pub mod schedule;
pub mod scripts_cli;
pub mod secrets;
pub mod sessions;
pub mod settings;
pub mod share;
pub mod side;
pub mod ssh;
pub mod stats;
pub mod telemetry;
mod tui;
pub mod update;
pub mod usage;
#[cfg(feature = "voice")]
pub mod voice;
pub mod worktree;

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use azazel_core::trust::needs_trust;
use azazel_core::{Config, Extension, LoadOptions, Paths, TrustStore};
use azazel_team::goal::goal_prompt;
use azazel_team::{GoalStatus, Goals};

use crate::app::{App, discover_paths};
use crate::cli::{Cli, Sub};

/// The stack of the thread a run happens on.
const RUN_STACK: usize = 8 * 1024 * 1024;

/// Entry point for the `azazel` binary and for custom distributions that add their own extensions.
#[derive(Default)]
pub struct Builder {
    extensions: Vec<Box<dyn Extension>>,
}

impl Builder {
    /// Adds an extension, registered after the built-in ones.
    pub fn extension(mut self, extension: impl Extension) -> Self {
        self.extensions.push(Box::new(extension));
        self
    }

    /// Parses the process arguments and runs the selected mode to completion.
    pub fn run(self) -> ExitCode {
        let extensions = self.extensions;
        restart::remember_binary();
        // The run gets a thread of its own, with the stack a main thread has on Linux. The main
        // thread of a Windows process has one megabyte, which a debug build does not fit in:
        // not the run, and not reading a command line with this many subcommands.
        let running = std::thread::Builder::new()
            .name("azazel".to_string())
            .stack_size(RUN_STACK)
            .spawn(move || {
                // Before anything can build an HTTPS client: also those inside libraries, which do
                // not go through `azazel_ai::tls`.
                azazel_ai::tls::install();
                let cli = Cli::parse_args();
                let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        eprintln!("azazel: cannot start the async runtime: {e}");
                        return ExitCode::FAILURE;
                    }
                };
                let code = runtime.block_on(run(cli, extensions));
                // Background work (jobs being killed, log flushes) gets a moment, not forever.
                runtime.shutdown_timeout(Duration::from_secs(2));
                ExitCode::from(code)
            });
        match running.map(|run| run.join()) {
            // `/restart`: the run is over and everything it held is closed.
            Ok(Ok(code)) => match restart::take() {
                Some(request) => restart::start(request),
                None => code,
            },
            // The run panicked; the panic has been reported.
            Ok(Err(_)) => ExitCode::FAILURE,
            Err(e) => {
                eprintln!("azazel: cannot start: {e}");
                ExitCode::FAILURE
            }
        }
    }
}

/// Logs go to `<home>/logs/azazel.log` only; stdout and stderr belong to the user and to protocols.
fn init_logging(paths: &Paths, debug: bool) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let log = App::log_file(paths);
    std::fs::create_dir_all(log.parent()?).ok()?;
    let file = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&log)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let level = if debug {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_max_level(level)
        .try_init()
        .ok()?;
    Some(guard)
}

/// Whether this run may execute what the project's configuration asks for. A project that has
/// such configuration and no recorded decision is asked about once, when a person is present.
pub(crate) fn project_trust(cli: &Cli, paths: &Paths, interactive: bool) -> bool {
    if cli.trust {
        return true;
    }
    let store = TrustStore::new(paths.trust_file());
    if let Some(decided) = store.decision(&paths.project_root) {
        return decided;
    }
    let options = LoadOptions {
        trusted: false,
        overrides: toml::Table::new(),
        env: std::env::vars().collect(),
    };
    let (compat, tools) = match Config::load(paths, &options) {
        Ok(loaded) => (loaded.config.compat.claude, loaded.config.tools),
        Err(_) => (true, Default::default()),
    };
    let reasons = needs_trust(paths, compat, &tools);
    if reasons.is_empty() || !interactive {
        return false;
    }
    println!("{} has:", paths.project_root.display());
    for reason in &reasons {
        println!("  - {reason}");
    }
    print!("Trust this project? Its config can run commands. [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    let trusted = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if let Err(e) = store.set(&paths.project_root, trusted) {
        eprintln!("azazel: could not save the decision: {e}");
    }
    trusted
}

/// `azazel ext types`: writes the declarations of the `azazel` global where editors can be
/// pointed at them.
#[cfg(feature = "lua")]
fn write_extension_types(paths: &Paths) -> u8 {
    let file = paths.home.join("types").join("azazel.d.luau");
    let written = file
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&file, azazel_lua::type_declarations()));
    match written {
        Ok(()) => {
            println!("{}", file.display());
            print::EXIT_OK
        }
        Err(e) => {
            eprintln!("azazel: {}: {e}", file.display());
            print::EXIT_FAILED
        }
    }
}

/// `azazel attach --take <id>`: ends the background session and turns this run into an
/// ordinary interactive one that opens its session.
#[cfg(feature = "detach")]
async fn taken(mut cli: Cli) -> Result<Cli, u8> {
    let Some(Sub::Attach {
        id, take: true, now, ..
    }) = &cli.command
    else {
        return Ok(cli);
    };
    // The session goes on in the interactive UI, or with -p answers what is piped in. Without
    // either the background session is better left running.
    if !cli.print && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        eprintln!(
            "azazel: attach --take continues the session in the interactive UI, which needs a terminal"
        );
        return Err(print::EXIT_USAGE);
    }
    let entry = match discover_paths(&cli) {
        Ok(paths) => detach::take(&paths, id, *now).await,
        Err(e) => Err(format!("{e:#}")),
    };
    match entry {
        Ok(entry) => {
            cli.command = None;
            cli.cwd = Some(entry.cwd);
            cli.session = Some(entry.session_file.to_string_lossy().into_owned());
            Ok(cli)
        }
        Err(e) => {
            eprintln!("azazel: {e}");
            Err(print::EXIT_FAILED)
        }
    }
}

/// Why the options of this command line cannot be applied to the background session `host` that
/// plain `azazel` is about to attach to, and what to do instead.
#[cfg(feature = "detach")]
fn cannot_attach(cli: &Cli, open_picker: bool, host: &str) -> Option<String> {
    let (flag, command) = if !cli.model.is_empty() {
        ("--model", "/model")
    } else if cli.thinking.is_some() {
        ("--thinking", "/thinking")
    } else if cli.mode.is_some() {
        ("--mode", "/mode")
    } else if cli.name.is_some() {
        ("--name", "/name")
    } else if cli.continue_run {
        ("--continue-run", "/continue")
    } else if open_picker {
        ("`azazel open`", "/open")
    } else {
        return None;
    };
    Some(format!(
        "{flag} cannot change a session that is already running in the background ({host}).          Run `azazel attach {host}` and use {command} there, or `azazel kill {host}` to end it first"
    ))
}

async fn run(cli: Cli, extensions: Vec<Box<dyn Extension>>) -> u8 {
    #[cfg(feature = "detach")]
    let cli = match taken(cli).await {
        Ok(cli) => cli,
        Err(code) => return code,
    };
    // `azazel open` is the interactive UI with the picker of `/open` up.
    let mut cli = cli;
    let open_picker = matches!(cli.command, Some(Sub::Open));
    if open_picker {
        cli.command = None;
    }
    let cli = cli;
    let paths = match discover_paths(&cli) {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("azazel: {e:#}");
            return print::EXIT_USAGE;
        }
    };
    let _log_guard = init_logging(&paths, cli.debug);
    if cfg!(windows) {
        update::remove_retired();
    }
    // The session is on another machine: nothing of this one's project is opened.
    if let Some(host) = &cli.ssh {
        return ssh::run(&cli, paths, host).await;
    }
    let interactive = cli.interactive();
    if cli.continue_run && (!cli.prompt.is_empty() || !(cli.resume_latest || cli.session.is_some())) {
        eprintln!(
            "azazel: --continue-run goes on with the run a session's last process did not finish: it takes -c or \
             --session, and no prompt"
        );
        return print::EXIT_USAGE;
    }
    if cli.print && !cli.continue_run && cli.prompt.is_empty() && std::io::stdin().is_terminal() {
        eprintln!("azazel: -p needs a prompt: azazel -p \"what to do\", or text piped on standard input");
        return print::EXIT_USAGE;
    }
    if interactive && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        eprintln!("azazel: the interactive UI needs a terminal; pass -p \"prompt\" to run without one");
        return print::EXIT_USAGE;
    }
    let trusted = project_trust(&cli, &paths, interactive);
    // The files of `-f` are named from where azazel was started, also when the run moves to
    // a worktree.
    let files = match print::files(&cli.files, &paths.cwd) {
        Ok(files) => files,
        Err(e) => {
            eprintln!("azazel: {e}");
            return print::EXIT_USAGE;
        }
    };
    let (paths, worktree) = match &cli.worktree {
        Some(name) => match into_worktree(&paths, name).await {
            Ok((moved, created)) => (moved, Some(created)),
            Err(e) => {
                eprintln!("azazel: --worktree: {e}");
                return print::EXIT_FAILED;
            }
        },
        None => (paths, None),
    };
    // `--profile` before a subcommand is checked too, instead of being ignored.
    if let Some(name) = cli.profile.as_deref().filter(|_| cli.command.is_some())
        && let Some(config) = quiet_config(&cli, &paths, trusted)
        && let Err(e) = profiles::resolve(&config, name)
    {
        eprintln!("azazel: --profile: {e}");
        return print::EXIT_USAGE;
    }
    let goal = match &cli.command {
        Some(Sub::Init) => {
            let code = init::run(&paths).await;
            if code == print::EXIT_OK {
                hooks::run_setup(&paths, trusted, "init").await;
            }
            return code;
        }
        Some(Sub::Rules { action }) => return rules::run(action, &paths, trusted),
        Some(Sub::Scripts { action }) => return scripts_cli::run(action, &paths, trusted),
        #[cfg(feature = "lsp")]
        Some(Sub::Lsp { action }) => return lsp_cli::run(action, &paths, trusted).await,
        #[cfg(feature = "browser")]
        Some(Sub::Browser { action }) => {
            let Some(config) = load_config(&cli, &paths, trusted) else {
                return print::EXIT_FAILED;
            };
            return browser_cmd::run(action, &paths, &config).await;
        }
        Some(Sub::Doctor) => return doctor::run(&paths, trusted).await,
        Some(Sub::Bug {
            description,
            transcript,
            session,
            issue,
        }) => {
            let description = description.join(" ");
            let args = bug::Args {
                description: &description,
                transcript: *transcript,
                session: session.as_deref(),
                issue: *issue,
            };
            return bug::run(&paths, trusted, args).await;
        }
        Some(Sub::Login {
            provider,
            key,
            device,
            enterprise,
            label,
        }) => {
            let flags = login::LoginFlags {
                key: *key,
                device: *device,
                enterprise: enterprise.as_deref(),
            };
            return login::login(provider.as_deref(), flags, label.as_deref(), &paths).await;
        }
        Some(Sub::Logout { provider, account }) => {
            return login::logout(provider, account.as_deref(), &paths);
        }
        Some(Sub::Config { action }) => return config_cli::run(action, &cli, &paths, trusted),
        Some(Sub::Secrets { action }) => return secrets::run(action, &paths),
        Some(Sub::Sessions { action }) => return sessions::run(action, &paths).await,
        Some(Sub::Changelog { since }) => return changelog::run(since.as_deref()),
        Some(Sub::Stats {
            days,
            period,
            by,
            since,
            json,
            rebuild,
        }) => {
            let args = stats::Args {
                days: *days,
                period: *period,
                by: *by,
                since: since.clone(),
                json: *json,
                rebuild: *rebuild,
            };
            return stats::run(&paths, &args);
        }
        Some(Sub::Context { session, json }) => return context::run(&paths, session.as_deref(), *json),
        Some(Sub::Export {
            session,
            format,
            file,
            sanitize,
        }) => {
            return sessions::export(&paths, session.as_deref(), *format, file.as_deref(), *sanitize);
        }
        // Taken for the interactive UI above.
        Some(Sub::Open) => None,
        Some(Sub::Completions { shell }) => {
            let mut command = <Cli as clap::CommandFactory>::command();
            clap_complete::generate(*shell, &mut command, "azazel", &mut std::io::stdout());
            return print::EXIT_OK;
        }
        Some(Sub::Models {
            query,
            provider,
            all,
            json,
        }) => {
            let setup = match roles_cli::Setup::new(&cli, paths.clone(), trusted) {
                Ok(setup) => setup,
                Err(e) => {
                    eprintln!("azazel: {e}");
                    return print::EXIT_FAILED;
                }
            };
            let catalog = azazel_ai::Catalog::load(Some(&paths.models_cache()), &setup.config.models);
            return models::run_models(
                &catalog,
                &setup.providers,
                &setup.credentials,
                &setup.roles(),
                query.as_deref(),
                provider.as_deref(),
                *all,
                *json,
            );
        }
        Some(Sub::Roles { action, json }) => {
            return match roles_cli::Setup::new(&cli, paths.clone(), trusted) {
                Ok(setup) => roles_cli::run(&setup, action.as_ref(), *json),
                Err(e) => {
                    eprintln!("azazel: {e}");
                    print::EXIT_FAILED
                }
            };
        }
        Some(Sub::Auth {
            action: cli::AuthAction::Status { json },
        }) => {
            let Some((_, providers, credentials)) = provider_setup(&cli, &paths, trusted) else {
                return print::EXIT_FAILED;
            };
            return models::run_auth_status(&providers, &credentials, *json);
        }
        Some(Sub::Usage { json }) => {
            let Some((config, _, credentials)) = provider_setup(&cli, &paths, trusted) else {
                return print::EXIT_FAILED;
            };
            credentials.limits().persist(usage::cache_file(&paths));
            let source = match usage::Source::of(&config, Arc::new(credentials), &paths) {
                Ok(source) => source,
                Err(e) => {
                    eprintln!("azazel: {e}");
                    return print::EXIT_FAILED;
                }
            };
            return usage::run(&source, &config, &paths, *json).await;
        }
        Some(Sub::Migrate {
            from,
            only,
            dry_run,
            overwrite,
            project,
            fix_chats,
        }) => {
            if *fix_chats {
                return migrate::fix_chats(&paths);
            }
            let args = migrate::Args {
                from,
                only,
                dry_run: *dry_run,
                overwrite: *overwrite,
                project: project.as_deref(),
            };
            return migrate::run(&args, &paths).await;
        }
        Some(Sub::Update { check }) => return update::run(&paths, *check).await,
        Some(Sub::Pkg { action }) => return pkg::run(action, &paths),
        Some(Sub::Skills { action }) => return pkg::run_skills(action, &paths),
        #[cfg(feature = "mcp")]
        Some(Sub::Mcp {
            action: cli::McpAction::Serve,
        }) => return mcp::serve::run(extensions).await,
        #[cfg(feature = "mcp")]
        Some(Sub::Mcp { action }) => return mcp::run(action, &paths, trusted, cli.trust).await,
        #[cfg(feature = "memory")]
        Some(Sub::Memory { action }) => {
            return memory::run(memory::Command::Memory(action), &paths, trusted).await;
        }
        #[cfg(feature = "memory")]
        Some(Sub::Index) => return memory::run(memory::Command::Index, &paths, trusted).await,
        #[cfg(feature = "memory")]
        Some(Sub::Rag { action }) => return memory::run(memory::Command::Rag(action), &paths, trusted).await,
        #[cfg(feature = "lua")]
        Some(Sub::Ext {
            action: cli::ExtAction::Types,
        }) => return write_extension_types(&paths),
        Some(Sub::Goal {
            objective,
            max_iterations,
            max_cost,
            max_minutes,
        }) => Some((objective.clone(), *max_iterations, *max_cost, *max_minutes)),
        #[cfg(feature = "acp")]
        Some(Sub::Acp) => return acp::run(extensions).await,
        #[cfg(feature = "detach")]
        Some(Sub::Bg { request_id, .. }) => return detach::bg(&paths, request_id.as_deref()).await,
        #[cfg(feature = "detach")]
        Some(Sub::Ps { json }) => return detach::ps(&paths, *json),
        #[cfg(feature = "detach")]
        Some(Sub::Logs { id, follow, json }) => return detach::logs(&paths, id, *follow, *json).await,
        #[cfg(feature = "detach")]
        Some(Sub::Kill { id }) => return detach::kill(&paths, id).await,
        #[cfg(feature = "detach")]
        Some(Sub::Attach { id, .. }) => return detach::attach(&cli, paths, id).await,
        #[cfg(feature = "remote")]
        Some(Sub::Remote(args)) => {
            let Some(config) = load_config(&cli, &paths, trusted) else {
                return print::EXIT_FAILED;
            };
            return remote::run(&cli, &paths, &config, args).await;
        }
        #[cfg(feature = "detach")]
        Some(Sub::Schedule { action }) => {
            let Some(config) = load_config(&cli, &paths, trusted) else {
                return print::EXIT_FAILED;
            };
            return schedule::run(&cli, &paths, &config, action).await;
        }
        #[cfg(feature = "detach")]
        Some(Sub::Host { .. }) => None,
        #[cfg(feature = "detach")]
        Some(Sub::Rpc { attach: Some(wanted) }) => return detach::bridge(&cli, &paths, wanted).await,
        #[cfg(not(feature = "detach"))]
        Some(Sub::Rpc { attach: Some(_) }) => {
            eprintln!("azazel: this build has no background sessions");
            return print::EXIT_FAILED;
        }
        Some(Sub::Rpc { attach: None }) | None => None,
    };

    // A background session that cannot start leaves the reason for `azazel bg` to print.
    #[cfg(feature = "detach")]
    let background = match &cli.command {
        Some(Sub::Host { id, .. }) => Some((paths.home.clone(), id.clone())),
        _ => None,
    };
    let complain = move |why: String| {
        eprintln!("azazel: {why}");
        #[cfg(feature = "detach")]
        if let Some((home, id)) = &background {
            running::note_failure(home, id, &why);
        }
    };
    let app = match App::build(&cli, paths, trusted, &extensions).await {
        Ok(app) => app,
        Err(e) => {
            complain(format!("{e:#}"));
            return if e.downcast_ref::<app::UsageError>().is_some() {
                print::EXIT_USAGE
            } else {
                print::EXIT_FAILED
            };
        }
    };
    let requested = app.requested_session(&cli);
    // The session this folder continues is open in a background session (for instance one a
    // browser started through `azazel remote`): the interactive UI goes to it, as `azazel attach`
    // does, instead of refusing. Print mode and the other modes still say where it runs.
    #[cfg(feature = "detach")]
    if cli.interactive()
        && let Ok(how) = &requested
        && let Some(host) = app.background_host(how)
    {
        // The session is already running: what only shapes a new one cannot apply to it.
        if let Some(refusal) = cannot_attach(&cli, open_picker, &host) {
            eprintln!("azazel: {refusal}");
            return print::EXIT_USAGE;
        }
        let first = Some(cli.prompt_text()).filter(|prompt| !prompt.trim().is_empty());
        if let Some(code) = detach::attach_if_running(&cli, app.paths.clone(), &host, first).await {
            return code;
        }
        // The host ended meanwhile: the session is free, and opens as usual.
    }
    let opened = match requested {
        Ok(how) => app.open(how, app.chain.clone(), app.thinking).await,
        Err(e) => Err(e),
    };
    let active = match opened {
        Ok(active) => active,
        Err(e) => {
            complain(format!("{e:#}"));
            return print::EXIT_FAILED;
        }
    };
    if !cli.no_session {
        sessions::prune_in_background(&app.paths, app.config.session.retention_days, active.session.id());
        sessions::prune_agents_in_background(&app.paths, app.config.team.agent_retention_days);
        // The first start imports the usage history of the session files, off the UI thread.
        if app.config.usage.ledger {
            let home = app.paths.home.clone();
            std::thread::spawn(move || azazel_core::ledger::ensure_backfilled(&home));
        }
    }
    // A project that runs trusted for the first time gets its `setup` hooks.
    if !cli.trust
        && trusted
        && azazel_core::TrustStore::new(app.paths.trust_file()).take_setup(&app.paths.project_root)
    {
        let mut setup = azazel_core::Event::Setup {
            trigger: "trust".to_string(),
        };
        app.registry.bus.emit(&mut setup).await;
    }
    app.record_flags(&active.session);
    if cli.print
        && let Some(notice) = &active.notice
    {
        eprintln!("azazel: {notice}");
    }
    if let Some(name) = cli.name.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
        active.session.append(azazel_core::EntryKind::SessionInfo {
            name: name.to_string(),
        });
    }
    if cli.archived {
        active
            .session
            .append(azazel_core::EntryKind::Archive { archived: true });
    }
    if let Some((objective, max_iterations, max_cost, max_minutes)) = goal {
        return goal_mode(
            &app,
            &active,
            &objective,
            max_iterations,
            max_cost,
            max_minutes,
            cli.output,
        )
        .await;
    }
    if matches!(cli.command, Some(Sub::Rpc { .. })) {
        return rpc::run(Arc::new(app), active).await;
    }
    #[cfg(feature = "detach")]
    if let Some(Sub::Host {
        id,
        idle_secs,
        request_id,
        prompt,
    }) = &cli.command
    {
        let prompt = prompt.join(" ");
        let options = host::Options {
            id: id.clone(),
            idle: Duration::from_secs(*idle_secs),
            prompt: (!prompt.trim().is_empty()).then_some(prompt),
            request_id: request_id.clone(),
            carried: host_carried(&cli),
        };
        return host::run(Arc::new(app), active, options).await;
    }
    let prompt = cli.prompt_text();
    if cli.print {
        let what = match cli.continue_run {
            true => print::What::ContinueRun,
            false => print::What::Prompt(&prompt, files),
        };
        let code = print::run(&app, &active, what, cli.output).await;
        if let Some(created) = worktree {
            print::worktree_line(&created, cli.output);
        }
        code
    } else {
        // Going on with an interrupted run is a command of the UI, sent as if typed.
        let first = match (cli.continue_run, open_picker) {
            (true, _) => Some("/continue".to_string()),
            (false, true) => Some("/open".to_string()),
            (false, false) => (!prompt.trim().is_empty()).then_some(prompt),
        };
        tui::run(Arc::new(app), active, first).await
    }
}

/// `-p --worktree[=NAME]`: makes the worktree the run happens in, and the paths of the run
/// there. A worktree of a trusted project is trusted.
async fn into_worktree(paths: &Paths, name: &str) -> Result<(Paths, worktree::Created), String> {
    let dir = worktree::dir(paths);
    let name = match name.trim() {
        "" => worktree::new_name(&dir),
        given => given.to_string(),
    };
    let created = worktree::create(&paths.cwd, &dir, &name, false).await?;
    worktree::inherit_trust(paths, &created.path);
    let mut moved = Paths::discover(&created.cwd, Some(&paths.home.to_string_lossy()));
    moved.sessions_override = paths.sessions_override.clone();
    Ok((moved, created))
}

/// The options a background session was started with that a process started in its place
/// (`azazel attach` on one that died) is to have. A key given on the command line is not
/// among them: the entry they are kept in is a file.
#[cfg(feature = "detach")]
pub(crate) fn host_carried(cli: &Cli) -> Vec<String> {
    let mut carried = Vec::new();
    let mut args = cli.carried().into_iter();
    while let Some(arg) = args.next() {
        if arg == "--api-key" {
            args.next();
            continue;
        }
        carried.push(arg.to_string_lossy().into_owned());
    }
    carried
}

/// The configuration, or nothing when it cannot be read (the command that needs it says so).
fn quiet_config(cli: &Cli, paths: &Paths, trusted: bool) -> Option<Config> {
    let overrides = app::config_overrides(cli, paths).ok()?;
    let options = LoadOptions {
        trusted,
        overrides,
        env: std::env::vars().collect(),
    };
    Config::load(paths, &options).ok().map(|loaded| loaded.config)
}

/// The configuration as this run has it, for commands that open no session: `--profile` applied.
/// A configuration that cannot be read is complained about.
fn load_config(cli: &Cli, paths: &Paths, trusted: bool) -> Option<Config> {
    let loaded = app::config_overrides(cli, paths).and_then(|overrides| {
        Config::load(
            paths,
            &LoadOptions {
                trusted,
                overrides,
                env: std::env::vars().collect(),
            },
        )
        .map_err(Into::into)
    });
    match loaded {
        Ok(loaded) => {
            let mut config = loaded.config;
            if let Some(name) = &cli.profile
                && let Err(e) = profiles::apply(&mut config, name)
            {
                eprintln!("azazel: --profile: {e}");
                return None;
            }
            Some(config)
        }
        Err(e) => {
            eprintln!("azazel: {e:#}");
            None
        }
    }
}

/// The configuration, providers and credentials, for commands that only look at providers.
fn provider_setup(
    cli: &Cli,
    paths: &Paths,
    trusted: bool,
) -> Option<(
    Config,
    Vec<azazel_ai::providers::presets::ProviderConfig>,
    azazel_ai::Credentials,
)> {
    let config = load_config(cli, paths, trusted)?;
    let providers = match models::providers(&config) {
        Ok(providers) => providers,
        Err(e) => {
            eprintln!("azazel: {e}");
            return None;
        }
    };
    let credentials = app::credentials(paths, trusted).configured(&config.providers);
    Some((config, providers, credentials))
}

/// `azazel goal`: sets the objective, lets the lead work until the goal is settled, and reports
/// through the exit code whether the checker verified it.
async fn goal_mode(
    app: &App,
    active: &app::Active,
    objective: &str,
    max_iterations: Option<u32>,
    max_cost: Option<f64>,
    max_minutes: Option<u64>,
    output: cli::OutputFormat,
) -> u8 {
    let Some(goals) = app.registry.services.get::<Goals>() else {
        eprintln!("azazel: goals are not available");
        return print::EXIT_FAILED;
    };
    let mut budget = goals.default_budget();
    if let Some(limit) = max_iterations {
        budget.max_iterations = limit;
    }
    budget.max_cost_usd = max_cost;
    budget.max_minutes = max_minutes;
    if let Err(why) = goals.start(objective, budget).await {
        eprintln!("azazel: {why}");
        app.finish(active).await;
        return print::EXIT_FAILED;
    }
    let what = print::What::Prompt(&goal_prompt(objective), print::Files::default());
    let code = print::run(app, active, what, output).await;
    if code != print::EXIT_OK {
        return code;
    }
    match goals.current() {
        Some(goal) if goal.status == GoalStatus::Done => print::EXIT_OK,
        Some(goal) => {
            eprintln!("azazel: {}", goal.describe());
            print::EXIT_GOAL_UNMET
        }
        None => print::EXIT_GOAL_UNMET,
    }
}
