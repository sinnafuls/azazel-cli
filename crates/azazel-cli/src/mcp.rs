//! `azazel mcp …`: list, add and remove MCP servers, and log in to remote ones.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use azazel_agent::Services;
use azazel_ai::Credentials;
use azazel_core::{Config, Headless, LoadOptions, Paths, Ui};
use azazel_mcp::config::{RawServer, add_server, remove_server};
use azazel_mcp::{McpManager, status_report};

use crate::app::mcp_manager;
use crate::cli::McpAction;

/// How long `azazel mcp list` gives servers to connect before it reports their state.
const LIST_WAIT: Duration = Duration::from_secs(10);

fn manager(paths: &Paths, trusted: bool) -> McpManager {
    let options = LoadOptions {
        trusted,
        overrides: toml::Table::new(),
        env: std::env::vars().collect(),
    };
    let config = Config::load(paths, &options)
        .map(|loaded| loaded.config)
        .unwrap_or_default();
    let services = Arc::new(Services::new());
    services.insert(Arc::new(Ui(Arc::new(Headless))));
    mcp_manager(
        paths,
        trusted,
        &config,
        services,
        Arc::new(Credentials::new(paths.auth_file())),
        &azazel_core::packages::Packages::new(paths).load().mcp_files,
    )
}

/// The entry `azazel mcp add` writes: a command line, or a URL with headers.
fn new_server(url: Option<&str>, headers: &[String], command: &[String]) -> Result<RawServer, String> {
    match (url, command.split_first()) {
        (Some(_), Some(_)) => Err("give either --url or a command after `--`, not both".to_string()),
        (None, None) => Err("give a command after `--`, or --url <url>".to_string()),
        (Some(url), None) => {
            let mut parsed = BTreeMap::new();
            for header in headers {
                let (name, value) = header
                    .split_once('=')
                    .filter(|(name, _)| !name.trim().is_empty())
                    .ok_or_else(|| format!("--header {header}: expected NAME=VALUE"))?;
                parsed.insert(name.trim().to_string(), value.trim().to_string());
            }
            Ok(RawServer {
                url: Some(url.to_string()),
                headers: parsed,
                ..RawServer::default()
            })
        }
        (None, Some((program, args))) => {
            if !headers.is_empty() {
                return Err("--header only applies to --url servers".to_string());
            }
            Ok(RawServer {
                command: Some(program.clone()),
                args: args.to_vec(),
                ..RawServer::default()
            })
        }
    }
}

/// The files a server can be removed from, highest precedence first.
fn editable_files(paths: &Paths) -> Vec<PathBuf> {
    vec![
        paths.project_dir().join("mcp.json"),
        paths.project_root.join(".mcp.json"),
        paths.home_mcp(),
    ]
}

pub async fn run(action: &McpAction, paths: &Paths, trusted: bool) -> u8 {
    let outcome = match action {
        McpAction::List => {
            let manager = manager(paths, trusted);
            manager.start();
            manager.settle(LIST_WAIT).await;
            let report = status_report(&manager);
            manager.shutdown();
            Ok(report.trim_end().to_string())
        }
        McpAction::Add {
            name,
            project,
            url,
            headers,
            command,
        } => {
            let file = if *project {
                paths.project_dir().join("mcp.json")
            } else {
                paths.home_mcp()
            };
            new_server(url.as_deref(), headers, command).and_then(|server| {
                add_server(&file, name, &server).map_err(|e| format!("{}: {e}", file.display()))?;
                let mut done = format!("added `{name}` to {}", file.display());
                if *project && !trusted {
                    done.push_str("\nproject servers start only once the project is trusted");
                }
                Ok(done)
            })
        }
        McpAction::Remove { name } => {
            let mut removed = Err(format!("no MCP server named `{name}` is configured"));
            for file in editable_files(paths) {
                match remove_server(&file, name) {
                    Ok(true) => {
                        removed = Ok(format!("removed `{name}` from {}", file.display()));
                        break;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        removed = Err(format!("{}: {e}", file.display()));
                        break;
                    }
                }
            }
            removed
        }
        McpAction::Login { name } => {
            let show = |url: &str| println!("Open this address to log in:\n{url}");
            manager(paths, trusted)
                .login(name, &show)
                .await
                .map(|()| format!("logged in to `{name}`"))
        }
        McpAction::Logout { name } => manager(paths, trusted).logout(name).map(|removed| {
            if removed {
                format!("logged out of `{name}`")
            } else {
                format!("no login was stored for `{name}`")
            }
        }),
    };
    match outcome {
        Ok(done) => {
            println!("{done}");
            0
        }
        Err(why) => {
            eprintln!("azazel: {why}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_added_server_is_a_command_line_or_a_url_with_headers() {
        let words = |list: &[&str]| list.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        let local = new_server(None, &[], &words(&["npx", "-y", "server"])).unwrap();
        assert_eq!(
            (local.command.as_deref(), local.args.len(), local.url),
            (Some("npx"), 2, None)
        );

        let remote = new_server(
            Some("https://example.com/mcp"),
            &words(&["Authorization=Bearer a=b"]),
            &[],
        )
        .unwrap();
        assert_eq!(remote.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(remote.headers["Authorization"], "Bearer a=b");

        assert!(new_server(None, &[], &[]).is_err());
        assert!(new_server(Some("https://x"), &[], &words(&["cmd"])).is_err());
        assert!(new_server(Some("https://x"), &words(&["nonsense"]), &[]).is_err());
        assert!(new_server(None, &words(&["A=b"]), &words(&["cmd"])).is_err());
    }
}
