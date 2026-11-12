//! `azazel mcp …`: list, add, remove, approve and import MCP servers, log in to remote ones, and
//! serve azazel as one.

pub mod serve;

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use azazel_agent::Services;
use azazel_core::{Config, Headless, LoadOptions, Paths, Ui};
use azazel_mcp::config::{RawServer, add_server, defined, remove_server};
use azazel_mcp::import::{self, Client, Dirs, Found};
use azazel_mcp::{McpManager, status_report};

use crate::app::mcp_manager;
use crate::cli::McpAction;

/// How long `azazel mcp list` gives servers to connect before it reports their state.
const LIST_WAIT: Duration = Duration::from_secs(10);

fn manager(paths: &Paths, trusted: bool, approve_all: bool) -> McpManager {
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
        Arc::new(crate::app::credentials(paths, trusted)),
        &azazel_core::packages::Packages::new(paths).load().mcp_files,
        approve_all,
    )
}

/// Which of `count` listed servers an answer names: `all`, nothing, or numbers and ranges such
/// as `1,3` or `2-4`, counted from 1. The result counts from 0.
fn parse_choice(answer: &str, count: usize) -> Result<Vec<usize>, String> {
    let answer = answer.trim().to_ascii_lowercase();
    match answer.as_str() {
        "" | "n" | "none" => return Ok(Vec::new()),
        "a" | "all" | "y" | "yes" => return Ok((0..count).collect()),
        _ => {}
    }
    let mut chosen = Vec::new();
    for part in answer.split([',', ' ']).filter(|part| !part.is_empty()) {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        let number = |text: &str| {
            text.parse::<usize>()
                .ok()
                .filter(|n| (1..=count).contains(n))
                .ok_or_else(|| format!("`{part}` is not a number from 1 to {count}"))
        };
        for index in number(first)?..=number(last)? {
            if !chosen.contains(&(index - 1)) {
                chosen.push(index - 1);
            }
        }
    }
    Ok(chosen)
}

/// One line read from the terminal after `question`.
fn ask(question: &str) -> String {
    print!("{question} ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    answer
}

/// Copies `chosen` of the found servers into `file`. A name the file already has is replaced
/// only when `replace` says so; a name that was just imported is not imported twice.
fn import_into(
    file: &std::path::Path,
    found: &[Found],
    chosen: &[usize],
    replace: &dyn Fn(&str) -> bool,
) -> Result<Vec<String>, String> {
    let mut existing = defined(file);
    let mut imported: Vec<String> = Vec::new();
    let mut lines = Vec::new();
    for server in chosen.iter().filter_map(|index| found.get(*index)) {
        let name = &server.name;
        if imported.contains(name) {
            lines.push(format!(
                "skipped `{name}` from {}: a server of that name was just imported",
                server.client.name()
            ));
            continue;
        }
        if existing.contains(name) && !replace(name) {
            lines.push(format!("kept `{name}` as it is: already in {}", file.display()));
            continue;
        }
        add_server(file, name, &server.server).map_err(|e| format!("{}: {e}", file.display()))?;
        lines.push(format!("imported `{name}` from {}", server.client.name()));
        imported.push(name.clone());
        existing.push(name.clone());
    }
    if imported.is_empty() {
        lines.push("nothing was imported".to_string());
    } else {
        lines.push(format!(
            "{} added to {}",
            if imported.len() == 1 {
                "1 server".to_string()
            } else {
                format!("{} servers", imported.len())
            },
            file.display()
        ));
    }
    Ok(lines)
}

/// `azazel mcp import`: lists the servers other clients are configured with and copies the
/// chosen ones.
fn run_import(paths: &Paths, client: Option<&str>, yes: bool) -> Result<String, String> {
    let clients = match client {
        Some(name) => vec![Client::parse(name)?],
        None => Client::ALL.to_vec(),
    };
    let dirs = Dirs::detect(&paths.project_root, &|name| std::env::var(name).ok())
        .ok_or("cannot find your home directory")?;
    let scan = import::scan(&clients, &dirs);
    for (file, problem) in &scan.problems {
        eprintln!("azazel: {}: {problem}", file.display());
    }
    if scan.found.is_empty() {
        let looked: Vec<String> = scan
            .looked
            .iter()
            .map(|file| format!("  {}", file.display()))
            .collect();
        return Ok(format!("no MCP servers found. Looked in:\n{}", looked.join("\n")));
    }
    let target = paths.home_mcp();
    let existing = defined(&target);
    for (index, server) in scan.found.iter().enumerate() {
        println!(
            "{:>3}. {}  {}  ({}, {}){}",
            index + 1,
            server.name,
            server.target(),
            server.client.name(),
            server.file.display(),
            if existing.contains(&server.name) {
                "  · already configured"
            } else {
                ""
            }
        );
        if let Some(note) = &server.note {
            println!("     {note}");
        }
    }
    let interactive = std::io::stdin().is_terminal();
    let chosen = if yes {
        (0..scan.found.len()).collect()
    } else if interactive {
        parse_choice(
            &ask("Import which? `all`, numbers such as 1,3, or Enter for none:"),
            scan.found.len(),
        )?
    } else {
        return Ok("pass --yes to import them all, or run this in a terminal to choose".to_string());
    };
    // Without a person to ask, a name that is already configured is never replaced.
    let replace = |name: &str| {
        !yes && matches!(
            ask(&format!("`{name}` is already configured. Replace it? [y/N]"))
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "y" | "yes"
        )
    };
    Ok(import_into(&target, &scan.found, &chosen, &replace)?.join("\n"))
}

/// `azazel mcp approve|reject`: records the decision for the project's servers.
fn decide(manager: &McpManager, names: &[String], approved: bool, trusted: bool) -> Result<String, String> {
    if names.is_empty() {
        return Ok("no server of this project waits for approval".to_string());
    }
    let decisions: Vec<(String, bool)> = names.iter().map(|name| (name.clone(), approved)).collect();
    let saved = manager.decide(&decisions).map_err(|why| {
        if trusted {
            why
        } else {
            format!("{why} (this project is not trusted, so the servers it defines are not read)")
        }
    })?;
    let mut done = format!(
        "{} {}",
        if approved { "approved" } else { "rejected" },
        names.join(", ")
    );
    if !saved {
        done.push_str(
            "\nnot saved: the project has no stored trust decision (it runs with --trust, which approves \
             every server for that run anyway)",
        );
    }
    Ok(done)
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

/// `approve_all` is `--trust`: every server the project defines counts as approved for this run.
pub async fn run(action: &McpAction, paths: &Paths, trusted: bool, approve_all: bool) -> u8 {
    let manager = |paths: &Paths, trusted: bool| manager(paths, trusted, approve_all);
    let outcome = match action {
        McpAction::Approve { names, all } => {
            let manager = manager(paths, trusted);
            let names = if *all {
                manager.waiting().into_iter().map(|server| server.name).collect()
            } else if names.is_empty() {
                return usage("give the servers to approve, or --all");
            } else {
                names.clone()
            };
            decide(&manager, &names, true, trusted)
        }
        McpAction::Reject { name } => decide(
            &manager(paths, trusted),
            std::slice::from_ref(name),
            false,
            trusted,
        ),
        McpAction::Import { client, yes } => run_import(paths, client.as_deref(), *yes),
        // Handled before this is called: it needs more than the paths.
        McpAction::Serve => Err("`mcp serve` is not available here".to_string()),
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
                } else if *project {
                    done.push_str(&format!(
                        "\nit starts once you approve it: azazel mcp approve {name}"
                    ));
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

fn usage(what: &str) -> u8 {
    eprintln!("azazel: {what}");
    crate::print::EXIT_USAGE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_choice_is_all_none_or_numbers_and_ranges() {
        assert_eq!(parse_choice("", 3), Ok(vec![]));
        assert_eq!(parse_choice(" none ", 3), Ok(vec![]));
        assert_eq!(parse_choice("All", 3), Ok(vec![0, 1, 2]));
        assert_eq!(parse_choice("3, 1", 3), Ok(vec![2, 0]));
        assert_eq!(parse_choice("2-3 2", 3), Ok(vec![1, 2]));
        assert!(parse_choice("4", 3).is_err());
        assert!(parse_choice("0", 3).is_err());
        assert!(parse_choice("x", 3).is_err());
    }

    #[test]
    fn an_import_never_replaces_a_name_unasked_and_imports_a_name_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        std::fs::write(&file, r#"{ "mcpServers": { "files": { "command": "mine" } } }"#).unwrap();
        let found = |name: &str, command: &str, client: Client| Found {
            name: name.to_string(),
            client,
            file: dir.path().join("other.json"),
            server: RawServer {
                command: Some(command.to_string()),
                ..RawServer::default()
            },
            note: None,
        };
        let servers = [
            found("files", "theirs", Client::Cursor),
            found("gh", "gh-mcp", Client::Cursor),
            found("gh", "gh-other", Client::VsCode),
        ];
        let lines = import_into(&file, &servers, &[0, 1, 2], &|_| false).unwrap();
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(lines[0].starts_with("kept `files` as it is"));
        assert_eq!(lines[1], "imported `gh` from cursor");
        assert!(lines[2].starts_with("skipped `gh` from vscode"));
        assert!(lines[3].starts_with("1 server added to"));
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"mine\"") && text.contains("gh-mcp") && !text.contains("gh-other"));

        // Asked and answered yes, the entry is replaced.
        let lines = import_into(&file, &servers, &[0], &|_| true).unwrap();
        assert_eq!(lines[0], "imported `files` from cursor");
        assert!(std::fs::read_to_string(&file).unwrap().contains("theirs"));
        assert_eq!(
            import_into(&file, &servers, &[], &|_| true).unwrap(),
            ["nothing was imported"]
        );
    }

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
