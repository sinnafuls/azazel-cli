//! `azazel lsp list` and `azazel lsp install`.

use azazel_core::{Config, LoadOptions, Paths};
use azazel_tools::lsp::Known;
use serde_json::json;

use crate::cli::LspAction;
use crate::print::{EXIT_FAILED, EXIT_OK};

fn config(paths: &Paths, trusted: bool) -> Result<Config, String> {
    let options = LoadOptions {
        trusted,
        overrides: toml::Table::new(),
        env: std::env::vars().collect(),
    };
    Config::load(paths, &options)
        .map(|loaded| loaded.config)
        .map_err(|e| e.to_string())
}

/// Where a server's program comes from, for `azazel lsp list`.
fn origin(server: &Known) -> String {
    match &server.found {
        Some(path) if server.managed => format!("installed by azazel: {}", path.display()),
        Some(path) => format!("installed: {}", path.display()),
        None => match (&server.install, server.installable) {
            (Some(how), true) => format!("not installed; `azazel lsp install {}` runs `{how}`", server.name),
            (Some(how), false) => format!("not installed; {how}"),
            (None, _) => format!("not installed; `{}` was not found", server.command),
        },
    }
}

/// Runs `azazel lsp …`.
pub async fn run(action: &LspAction, paths: &Paths, trusted: bool) -> u8 {
    let config = match config(paths, trusted) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("azazel: {e}");
            return EXIT_FAILED;
        }
    };
    let places = azazel_tools::lsp::Places::system(paths.lsp_dir());
    match action {
        LspAction::List { json } => {
            let servers = azazel_tools::lsp::known(&config.lsp.servers, &places);
            if *json {
                let rows: Vec<_> = servers
                    .iter()
                    .map(|server| {
                        json!({
                            "name": server.name,
                            "command": server.command,
                            "extensions": server.extensions,
                            "installed": server.found.is_some(),
                            "path": server.found,
                            "managed": server.managed,
                            "install": server.install,
                        })
                    })
                    .collect();
                println!("{}", serde_json::Value::Array(rows));
            } else {
                for server in &servers {
                    let extensions: Vec<String> = server.extensions.iter().map(|e| format!(".{e}")).collect();
                    println!("{}  {}  {}", server.name, extensions.join(" "), origin(server));
                }
            }
            EXIT_OK
        }
        LspAction::Install { server } => match azazel_tools::lsp::install(server, &places).await {
            Ok(path) => {
                println!("{server}: {}", path.display());
                EXIT_OK
            }
            Err(e) => {
                eprintln!("azazel: {e}");
                EXIT_FAILED
            }
        },
    }
}
