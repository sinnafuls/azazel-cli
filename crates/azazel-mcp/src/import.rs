//! Finding the MCP servers other clients are configured with, so they can be copied into
//! `~/.azazel/mcp.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::config::RawServer;

/// A client whose servers can be imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    ClaudeDesktop,
    ClaudeCode,
    Cursor,
    VsCode,
    Windsurf,
    Jcode,
}

impl Client {
    pub const ALL: [Client; 6] = [
        Client::ClaudeDesktop,
        Client::ClaudeCode,
        Client::Cursor,
        Client::VsCode,
        Client::Windsurf,
        Client::Jcode,
    ];

    /// The name on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Client::ClaudeDesktop => "claude-desktop",
            Client::ClaudeCode => "claude-code",
            Client::Cursor => "cursor",
            Client::VsCode => "vscode",
            Client::Windsurf => "windsurf",
            Client::Jcode => "jcode",
        }
    }

    pub fn parse(name: &str) -> Result<Client, String> {
        Client::ALL
            .into_iter()
            .find(|client| client.name() == name)
            .ok_or_else(|| {
                let names: Vec<&str> = Client::ALL.iter().map(|client| client.name()).collect();
                format!("unknown client `{name}`: expected {}", names.join(", "))
            })
    }
}

/// The directories the clients keep their configuration under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    /// The user's home directory.
    pub home: PathBuf,
    /// Where applications keep per-user configuration: `%APPDATA%` on Windows,
    /// `~/Library/Application Support` on macOS, `$XDG_CONFIG_HOME` or `~/.config` elsewhere.
    pub config: PathBuf,
    /// The project root, for the files clients keep in a project.
    pub project: PathBuf,
    /// jcode's own directory: `$JCODE_HOME`, else `~/.jcode`.
    pub jcode: PathBuf,
}

impl Dirs {
    /// `None` when the home directory is not known.
    pub fn detect(project: &Path, env: &dyn Fn(&str) -> Option<String>) -> Option<Dirs> {
        let home = std::env::home_dir()?;
        let config = if cfg!(windows) {
            env("APPDATA").map_or_else(|| home.join("AppData").join("Roaming"), PathBuf::from)
        } else if cfg!(target_os = "macos") {
            home.join("Library").join("Application Support")
        } else {
            env("XDG_CONFIG_HOME")
                .filter(|dir| !dir.is_empty())
                .map_or_else(|| home.join(".config"), PathBuf::from)
        };
        let jcode = env("JCODE_HOME")
            .filter(|dir| !dir.is_empty())
            .map_or_else(|| home.join(".jcode"), PathBuf::from);
        Some(Dirs {
            home,
            config,
            project: project.to_path_buf(),
            jcode,
        })
    }
}

/// The files `client` keeps servers in, user-wide ones first.
pub fn config_files(client: Client, dirs: &Dirs) -> Vec<PathBuf> {
    match client {
        Client::ClaudeDesktop => vec![dirs.config.join("Claude").join("claude_desktop_config.json")],
        // The project's own `.mcp.json` is not listed: azazel reads it where it is.
        Client::ClaudeCode => vec![dirs.home.join(".claude.json")],
        Client::Cursor => vec![
            dirs.home.join(".cursor").join("mcp.json"),
            dirs.project.join(".cursor").join("mcp.json"),
        ],
        Client::VsCode => vec![
            dirs.config.join("Code").join("User").join("mcp.json"),
            dirs.project.join(".vscode").join("mcp.json"),
        ],
        Client::Windsurf => vec![
            dirs.home
                .join(".codeium")
                .join("windsurf")
                .join("mcp_config.json"),
        ],
        // jcode also reads the project's `.mcp.json`, which azazel reads where it is.
        Client::Jcode => vec![
            dirs.jcode.join("mcp.json"),
            dirs.project.join(".jcode").join("mcp.json"),
        ],
    }
}

/// One server another client is configured with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub client: Client,
    pub file: PathBuf,
    /// The entry as azazel would write it.
    pub server: RawServer,
    /// Something in the entry that azazel cannot carry over as it is.
    pub note: Option<String>,
}

impl Found {
    /// The command line or the address, without the environment or headers (they hold secrets).
    pub fn target(&self) -> String {
        match (&self.server.command, &self.server.url) {
            (Some(command), _) => std::iter::once(command.as_str())
                .chain(self.server.args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            (None, Some(url)) => url.clone(),
            (None, None) => String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// In the order the clients and their files were read; a name may appear more than once.
    pub found: Vec<Found>,
    /// `(file, what is wrong)` for files and entries that could not be read.
    pub problems: Vec<(PathBuf, String)>,
    /// The files that were looked for, whether they exist or not.
    pub looked: Vec<PathBuf>,
}

/// JSON with `//` and `/* */` comments and trailing commas removed (VS Code writes both).
fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for skipped in chars.by_ref() {
                    if skipped == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut previous = ' ';
                for skipped in chars.by_ref() {
                    if previous == '*' && skipped == '/' {
                        break;
                    }
                    previous = skipped;
                }
            }
            (',', _) => {
                // A comma followed only by white space and a closing bracket is a trailing one.
                let mut ahead = chars.clone();
                let next = ahead.find(|c| !c.is_whitespace());
                if !matches!(next, Some('}' | ']')) {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// `${env:NAME}` as azazel writes it, `${NAME}`; and what else in `text` azazel does not fill in.
fn variables(text: &str, unfilled: &mut Vec<String>) -> String {
    let text = text.replace("${env:", "${");
    let mut rest = text.as_str();
    while let Some(at) = rest.find("${") {
        let Some(end) = rest[at..].find('}') else { break };
        let name = &rest[at + 2..at + end];
        if name.contains(':') && !name.contains(":-") || name == "workspaceFolder" || name == "userHome" {
            let variable = format!("${{{name}}}");
            if !unfilled.contains(&variable) {
                unfilled.push(variable);
            }
        }
        rest = &rest[at + end..];
    }
    text
}

/// An entry of another client as an `mcp.json` entry, and what in it azazel cannot carry over as
/// it is. `Err` says why it cannot be an entry at all.
pub fn convert(entry: &Value) -> Result<(RawServer, Option<String>), String> {
    let fields = entry.as_object().ok_or("the entry is not an object")?;
    // Shared by the closures below, which each note what they could not carry over.
    let unfilled = std::cell::RefCell::new(Vec::new());
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| fields.get(*key).and_then(Value::as_str))
            .filter(|value| !value.trim().is_empty())
            .map(|value| variables(value, &mut unfilled.borrow_mut()))
    };
    let command = text(&["command"]);
    let url = text(&["url", "serverUrl", "httpUrl"]);
    let cwd = text(&["cwd"]);
    let kind = text(&["type", "transport"]);
    let pairs = |key: &str| -> BTreeMap<String, String> {
        fields
            .get(key)
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(name, value)| {
                let value = match value {
                    Value::String(text) => text.clone(),
                    Value::Number(number) => number.to_string(),
                    Value::Bool(flag) => flag.to_string(),
                    _ => return None,
                };
                Some((name.clone(), variables(&value, &mut unfilled.borrow_mut())))
            })
            .collect()
    };
    let env = pairs("env");
    let headers = pairs("headers");
    let args: Vec<String> = fields
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|arg| variables(arg, &mut unfilled.borrow_mut()))
        .collect();
    if command.is_none() && url.is_none() {
        return Err("it has neither a `command` nor a `url`".to_string());
    }
    let kind = match kind.as_deref() {
        Some("sse") => Some("sse".to_string()),
        Some("http" | "streamable-http" | "streamableHttp" | "streamable_http" | "streamablehttp") => {
            Some("http".to_string())
        }
        _ => None,
    };
    let disabled = fields.get("disabled").and_then(Value::as_bool) == Some(true)
        || fields.get("enabled").and_then(Value::as_bool) == Some(false);
    // jcode writes the time limit of a call in seconds.
    let timeout = fields
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .filter(|secs| *secs > 0);
    let server = RawServer {
        kind: kind.filter(|_| command.is_none()),
        command,
        args,
        env,
        cwd,
        url,
        headers,
        timeout,
        oauth: None,
        enabled: disabled.then_some(false),
        ..RawServer::default()
    };
    let unfilled = unfilled.into_inner();
    let note = (!unfilled.is_empty()).then(|| {
        format!(
            "uses {}, which azazel does not fill in; edit the entry after importing",
            unfilled.join(", ")
        )
    });
    Ok((server, note))
}

/// The objects of `root` that hold servers by name.
fn server_maps<'a>(client: Client, root: &'a Value, project: &Path) -> Vec<&'a Map<String, Value>> {
    let object = |value: Option<&'a Value>| value.and_then(Value::as_object);
    let mut maps = Vec::new();
    maps.extend(object(root.get("mcpServers")));
    match client {
        // jcode's own name for the key, beside `mcpServers`.
        Client::Jcode => maps.extend(object(root.get("servers"))),
        // VS Code names the key `servers`; older versions kept it under `mcp` in the settings.
        Client::VsCode => {
            maps.extend(object(root.get("servers")));
            maps.extend(object(root.get("mcp").and_then(|mcp| mcp.get("servers"))));
        }
        // Claude Code keeps the servers added for one project under that project's path.
        Client::ClaudeCode => {
            let wanted = project.to_string_lossy().replace('\\', "/");
            let projects = object(root.get("projects")).into_iter().flatten();
            for (path, settings) in projects {
                let written = path.replace('\\', "/");
                let (written, wanted) = (written.trim_end_matches('/'), wanted.trim_end_matches('/'));
                let same = written == wanted || (cfg!(windows) && written.eq_ignore_ascii_case(wanted));
                if same {
                    maps.extend(object(settings.get("mcpServers")));
                }
            }
        }
        _ => {}
    }
    maps
}

/// Reads the configuration of each of `clients` and lists the servers in them.
pub fn scan(clients: &[Client], dirs: &Dirs) -> Scan {
    let mut scan = Scan::default();
    for client in clients {
        for file in config_files(*client, dirs) {
            scan.looked.push(file.clone());
            let text = match std::fs::read_to_string(&file) {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    scan.problems.push((file, e.to_string()));
                    continue;
                }
            };
            let root: Value = match serde_json::from_str(&strip_jsonc(&text)) {
                Ok(root) => root,
                Err(e) => {
                    scan.problems.push((file, format!("not valid JSON: {e}")));
                    continue;
                }
            };
            for servers in server_maps(*client, &root, &dirs.project) {
                for (name, entry) in servers {
                    match convert(entry) {
                        Ok((server, note)) => scan.found.push(Found {
                            name: name.clone(),
                            client: *client,
                            file: file.clone(),
                            server,
                            note,
                        }),
                        Err(problem) => scan
                            .problems
                            .push((file.clone(), format!("server `{name}` is skipped: {problem}"))),
                    }
                }
            }
        }
    }
    scan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(root: &Path) -> Dirs {
        Dirs {
            home: root.join("home"),
            config: root.join("config"),
            project: root.join("project"),
            jcode: root.join("jcode-home"),
        }
    }

    fn by_name_timeout(scan: &Scan, name: &str) -> Option<u64> {
        scan.found.iter().find(|found| found.name == name)?.server.timeout
    }

    fn write(file: &Path, text: &str) {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }

    #[test]
    fn each_clients_servers_are_found_and_converted() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs(tmp.path());
        write(
            &dirs.config.join("Claude/claude_desktop_config.json"),
            r#"{ "mcpServers": { "files": { "command": "npx", "args": ["-y", "fs-server", "/data"],
                                             "env": { "TOKEN": "t0p" } } }, "theme": "dark" }"#,
        );
        let project = dirs.project.to_string_lossy().replace('\\', "/");
        write(
            &dirs.home.join(".claude.json"),
            &format!(
                r#"{{ "mcpServers": {{ "docs": {{ "type": "http", "url": "https://docs.example/mcp",
                                                   "headers": {{ "Authorization": "Bearer x" }} }} }},
                      "projects": {{ "{project}": {{ "mcpServers": {{ "here": {{ "command": "here-server" }} }} }},
                                     "/somewhere/else": {{ "mcpServers": {{ "there": {{ "command": "x" }} }} }} }} }}"#
            ),
        );
        write(
            &dirs.home.join(".cursor/mcp.json"),
            r#"{ "mcpServers": { "search": { "url": "https://search.example/sse", "type": "sse" },
                                 "off": { "command": "off-server", "disabled": true },
                                 "broken": { "args": ["x"] } } }"#,
        );
        write(
            &dirs.project.join(".cursor/mcp.json"),
            r#"{ "mcpServers": { "files": { "command": "other-fs" } } }"#,
        );
        write(
            &dirs.config.join("Code/User/mcp.json"),
            "{\n  // user servers\n  \"servers\": {\n    \"gh\": { \"type\": \"stdio\", \"command\": \"gh-mcp\",\n \
             \"args\": [\"--root\", \"${workspaceFolder}\"], \"env\": { \"KEY\": \"${env:GH_KEY}\", \"PIN\": \
             \"${input:pin}\" }, },\n  },\n  /* inputs are not imported */ \"inputs\": []\n}",
        );
        write(
            &dirs.home.join(".codeium/windsurf/mcp_config.json"),
            r#"{ "mcpServers": { "remote": { "serverUrl": "https://wind.example/mcp" } } }"#,
        );
        write(
            &dirs.jcode.join("mcp.json"),
            r#"{ "servers": { "notes": { "command": "notes-mcp", "env": { "TOKEN": "${NOTES_TOKEN}" },
                                         "timeout_secs": 60, "shared": true },
                              "live": { "type": "streamablehttp", "url": "https://live.example/mcp" } } }"#,
        );
        write(
            &dirs.project.join(".jcode/mcp.json"),
            r#"{ "mcpServers": { "local": { "command": "local-mcp", "disabled": true } } }"#,
        );

        let scan = scan(&Client::ALL, &dirs);
        assert_eq!(scan.looked.len(), 9);
        let listed: Vec<(&str, &str, String)> = scan
            .found
            .iter()
            .map(|found| (found.name.as_str(), found.client.name(), found.target()))
            .collect();
        assert_eq!(
            listed,
            [
                ("files", "claude-desktop", "npx -y fs-server /data".to_string()),
                ("docs", "claude-code", "https://docs.example/mcp".to_string()),
                ("here", "claude-code", "here-server".to_string()),
                ("search", "cursor", "https://search.example/sse".to_string()),
                ("off", "cursor", "off-server".to_string()),
                ("files", "cursor", "other-fs".to_string()),
                ("gh", "vscode", "gh-mcp --root ${workspaceFolder}".to_string()),
                ("remote", "windsurf", "https://wind.example/mcp".to_string()),
                ("notes", "jcode", "notes-mcp".to_string()),
                ("live", "jcode", "https://live.example/mcp".to_string()),
                ("local", "jcode", "local-mcp".to_string()),
            ]
        );
        assert_eq!(by_name_timeout(&scan, "notes"), Some(60));
        assert_eq!(
            scan.found.iter().find(|f| f.name == "notes").unwrap().server.env["TOKEN"],
            "${NOTES_TOKEN}"
        );
        assert_eq!(
            scan.found
                .iter()
                .find(|f| f.name == "live")
                .unwrap()
                .server
                .kind
                .as_deref(),
            Some("http")
        );
        assert_eq!(
            scan.found
                .iter()
                .find(|f| f.name == "local")
                .unwrap()
                .server
                .enabled,
            Some(false)
        );
        let by_name = |name: &str| scan.found.iter().find(|found| found.name == name).unwrap();
        assert_eq!(by_name("files").server.env["TOKEN"], "t0p");
        assert_eq!(by_name("docs").server.kind.as_deref(), Some("http"));
        assert_eq!(by_name("docs").server.headers["Authorization"], "Bearer x");
        assert_eq!(by_name("search").server.kind.as_deref(), Some("sse"));
        assert_eq!(by_name("off").server.enabled, Some(false));
        assert_eq!(
            by_name("remote").server.url.as_deref(),
            Some("https://wind.example/mcp")
        );
        let gh = by_name("gh");
        assert_eq!(gh.server.kind, None);
        assert_eq!(gh.server.env["KEY"], "${GH_KEY}");
        assert_eq!(
            gh.note.as_deref(),
            Some(
                "uses ${input:pin}, ${workspaceFolder}, which azazel does not fill in; edit the entry after \
                 importing"
            )
        );
        assert!(by_name("files").note.is_none());
        assert_eq!(scan.problems.len(), 1);
        assert!(
            scan.problems[0].1.contains("`broken` is skipped"),
            "{:?}",
            scan.problems
        );

        // One client, and a file that is not JSON.
        write(&dirs.home.join(".cursor/mcp.json"), "{ nope");
        let cursor = super::scan(&[Client::Cursor], &dirs);
        assert_eq!(cursor.found.len(), 1);
        assert!(cursor.problems[0].1.starts_with("not valid JSON"));
        assert_eq!(Client::parse("vscode"), Ok(Client::VsCode));
        assert_eq!(Client::parse("jcode"), Ok(Client::Jcode));
        assert!(Client::parse("emacs").unwrap_err().contains("claude-desktop"));
    }

    #[test]
    fn comments_and_trailing_commas_are_removed_but_strings_are_kept() {
        let text =
            "{ \"a\": \"// not a comment, \", // gone\n \"b\": [1, 2, ], /* gone */ \"c\": \"x\\\"/*y\", }";
        let value: Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(value["a"], "// not a comment, ");
        assert_eq!(value["b"], serde_json::json!([1, 2]));
        assert_eq!(value["c"], "x\"/*y");
    }
}
