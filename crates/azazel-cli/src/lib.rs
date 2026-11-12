//! The `azazel` command-line harness: argument parsing, run modes and the embedding [`Builder`].

pub mod app;
pub mod cli;
pub mod doctor;
pub mod init;
pub mod login;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "memory")]
pub mod memory;
pub mod models;
pub mod pkg;
pub mod print;
pub mod rpc;
pub mod settings;
mod tui;
pub mod update;

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use azazel_core::trust::needs_trust;
use azazel_core::{Config, Extension, LoadOptions, Paths, TrustStore};
use azazel_team::goal::goal_prompt;
use azazel_team::{GoalStatus, Goals};
use clap::Parser;

use crate::app::{App, discover_paths};
use crate::cli::{Cli, Sub};

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
        let cli = Cli::parse();
        let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(e) => {
                eprintln!("azazel: cannot start the async runtime: {e}");
                return ExitCode::FAILURE;
            }
        };
        let code = runtime.block_on(run(cli, self.extensions));
        // Background work (jobs being killed, log flushes) gets a moment, not forever.
        runtime.shutdown_timeout(Duration::from_secs(2));
        ExitCode::from(code)
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
fn project_trust(cli: &Cli, paths: &Paths, interactive: bool) -> bool {
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
    let compat = Config::load(paths, &options).map_or(true, |loaded| loaded.config.compat.claude);
    let reasons = needs_trust(paths, compat);
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

async fn run(cli: Cli, extensions: Vec<Box<dyn Extension>>) -> u8 {
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
    let interactive = cli.interactive();
    if cli.print && cli.prompt.is_empty() && std::io::stdin().is_terminal() {
        eprintln!("azazel: -p needs a prompt: azazel -p \"what to do\", or text piped on standard input");
        return print::EXIT_USAGE;
    }
    if interactive && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        eprintln!("azazel: the interactive UI needs a terminal; pass -p \"prompt\" to run without one");
        return print::EXIT_USAGE;
    }
    let trusted = project_trust(&cli, &paths, interactive);
    let goal = match &cli.command {
        Some(Sub::Init) => return init::run(&paths).await,
        Some(Sub::Doctor) => return doctor::run(&paths, trusted).await,
        Some(Sub::Login { provider, key }) => {
            return login::login(provider.as_deref(), *key, &paths).await;
        }
        Some(Sub::Logout { provider }) => return login::logout(provider, &paths),
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
            let Some((config, providers, credentials)) = provider_setup(&cli, &paths, trusted) else {
                return print::EXIT_FAILED;
            };
            let catalog = azazel_ai::Catalog::load(Some(&paths.models_cache()), &config.models);
            return models::run_models(
                &catalog,
                &providers,
                &credentials,
                query.as_deref(),
                provider.as_deref(),
                *all,
                *json,
            );
        }
        Some(Sub::Auth {
            action: cli::AuthAction::Status { json },
        }) => {
            let Some((_, providers, credentials)) = provider_setup(&cli, &paths, trusted) else {
                return print::EXIT_FAILED;
            };
            return models::run_auth_status(&providers, &credentials, *json);
        }
        Some(Sub::Update { check }) => return update::run(&paths, *check).await,
        Some(Sub::Pkg { action }) => return pkg::run(action, &paths),
        Some(Sub::Skills { action }) => return pkg::run_skills(action, &paths),
        #[cfg(feature = "mcp")]
        Some(Sub::Mcp { action }) => return mcp::run(action, &paths, trusted).await,
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
        Some(Sub::Rpc) | None => None,
    };

    let app = match App::build(&cli, paths, trusted, &extensions).await {
        Ok(app) => app,
        Err(e) => {
            eprintln!("azazel: {e:#}");
            return if e.downcast_ref::<app::UsageError>().is_some() {
                print::EXIT_USAGE
            } else {
                print::EXIT_FAILED
            };
        }
    };
    let opened = match app.requested_session(&cli) {
        Ok(how) => app.open(how, app.chain.clone(), app.thinking).await,
        Err(e) => Err(e),
    };
    let active = match opened {
        Ok(active) => active,
        Err(e) => {
            eprintln!("azazel: {e:#}");
            return print::EXIT_FAILED;
        }
    };
    if let Some(name) = cli.name.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
        active.session.append(azazel_core::EntryKind::SessionInfo {
            name: name.to_string(),
        });
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
    if cli.command == Some(Sub::Rpc) {
        return rpc::run(Arc::new(app), active).await;
    }
    let prompt = cli.prompt_text();
    if cli.print {
        print::run(&app, &active, &prompt, cli.output).await
    } else {
        tui::run(
            Arc::new(app),
            active,
            (!prompt.trim().is_empty()).then_some(prompt),
        )
        .await
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
    let loaded = app::config_overrides(cli).and_then(|overrides| {
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
    let config = match loaded {
        Ok(loaded) => loaded.config,
        Err(e) => {
            eprintln!("azazel: {e:#}");
            return None;
        }
    };
    let providers = match models::providers(&config) {
        Ok(providers) => providers,
        Err(e) => {
            eprintln!("azazel: {e}");
            return None;
        }
    };
    Some((config, providers, azazel_ai::Credentials::new(paths.auth_file())))
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
    let code = print::run(app, active, &goal_prompt(objective), output).await;
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
