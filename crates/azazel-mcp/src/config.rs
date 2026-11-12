//! `mcp.json` files: reading, merging, `${VAR}` expansion and editing.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use azazel_core::trust::fingerprint;
use azazel_core::{Paths, TrustStore};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Seconds a request to a server may take when the entry sets no `timeout`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OAuthSettings {
    /// Empty means register a client dynamically.
    pub client_id: String,
    pub scope: String,
    /// 0 means any free port.
    pub callback_port: u16,
}

/// One `mcpServers` entry as written in a file. Keys this version does not know are ignored, so
/// files shared with other tools load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawServer {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: PathBuf,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        /// `type: "sse"`: fall back to the legacy SSE transport when streamable HTTP is refused.
        legacy_sse: bool,
    },
}

/// Whether a server may start. A server that a project defines needs the user's approval, once
/// for each version of its definition.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Approval {
    /// Not a project's server, or approved.
    #[default]
    Granted,
    /// New, or changed since it was approved; nobody has decided yet.
    Waiting,
    /// The user said no.
    Rejected,
}

/// A server entry after merging and variable expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub name: String,
    pub transport: Transport,
    pub timeout: Duration,
    pub oauth: OAuthSettings,
    pub enabled: bool,
    /// The file the entry came from.
    pub source: PathBuf,
    /// A hash of the entry as written; an approval holds for one fingerprint.
    pub fingerprint: String,
    pub approval: Approval,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    /// Usable entries, sorted by name. Entries with `enabled: false` are included.
    pub servers: Vec<ServerConfig>,
    /// `(server name or file, what is wrong)` for everything that was left out.
    pub problems: Vec<(String, String)>,
}

/// The files servers are read from, lowest precedence first. Project files count only when the
/// project is trusted: an entry is a command to run.
pub fn config_files(paths: &Paths, trusted: bool) -> Vec<PathBuf> {
    let mut files = vec![paths.home_mcp()];
    if trusted {
        files.push(paths.project_root.join(".mcp.json"));
        files.push(paths.project_dir().join("mcp.json"));
    }
    files
}

/// The files in which a project defines servers.
pub fn project_files(paths: &Paths) -> [PathBuf; 2] {
    [
        paths.project_root.join(".mcp.json"),
        paths.project_dir().join("mcp.json"),
    ]
}

/// Marks the servers the project defines as approved, waiting or rejected, as the trust file
/// says; `approve_all` (a `--trust` run) approves them all for this run.
pub fn apply_approvals(loaded: &mut Loaded, paths: &Paths, store: &TrustStore, approve_all: bool) {
    let project = project_files(paths);
    for server in &mut loaded.servers {
        if approve_all || !project.contains(&server.source) {
            continue;
        }
        server.approval = match store.server(&paths.project_root, &server.name, &server.fingerprint) {
            Some(true) => Approval::Granted,
            Some(false) => Approval::Rejected,
            None => Approval::Waiting,
        };
    }
}

static VARIABLE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}").ok());

/// Replaces `${VAR}` and `${VAR:-default}`. `Err` names the first variable that is unset and has
/// no default.
pub fn expand(text: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let Some(pattern) = VARIABLE.as_ref() else {
        return Ok(text.to_string());
    };
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for found in pattern.captures_iter(text) {
        let (Some(whole), Some(name)) = (found.get(0), found.get(1)) else {
            continue;
        };
        out.push_str(&text[last..whole.start()]);
        match env(name.as_str()).or_else(|| found.get(2).map(|d| d.as_str().to_string())) {
            Some(value) => out.push_str(&value),
            None => return Err(name.as_str().to_string()),
        }
        last = whole.end();
    }
    out.push_str(&text[last..]);
    Ok(out)
}

fn read_servers(file: &Path) -> Result<Map<String, Value>, String> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(e.to_string()),
    };
    let root: Value = serde_json::from_str(&text).map_err(|e| format!("not valid JSON: {e}"))?;
    match root.get("mcpServers") {
        None => Ok(Map::new()),
        Some(Value::Object(servers)) => Ok(servers.clone()),
        Some(_) => Err("`mcpServers` is not an object".to_string()),
    }
}

fn resolve(
    name: &str,
    entry: Value,
    source: &Path,
    base: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ServerConfig, String> {
    let fingerprint = fingerprint(&entry);
    let raw: RawServer = serde_json::from_value(entry).map_err(|e| e.to_string())?;
    let unset = |variable: String| format!("${{{variable}}} is not set");
    let one = |text: &str| expand(text, env).map_err(unset);
    let map = |pairs: &BTreeMap<String, String>| -> Result<BTreeMap<String, String>, String> {
        pairs
            .iter()
            .map(|(key, value)| Ok((key.clone(), one(value)?)))
            .collect()
    };
    let kind = match (raw.kind.as_deref(), &raw.command, &raw.url) {
        (Some("stdio"), ..) | (None, Some(_), _) => "stdio",
        (Some("http" | "streamable-http" | "streamableHttp"), ..) | (None, None, Some(_)) => "http",
        (Some("sse"), ..) => "sse",
        (Some(other), ..) => return Err(format!("unknown type `{other}` (expected stdio, http or sse)")),
        (None, None, None) => return Err("needs a `command` or a `url`".to_string()),
    };
    let transport = if kind == "stdio" {
        let command = raw.command.as_deref().ok_or("type stdio needs a `command`")?;
        let cwd = match raw.cwd.as_deref() {
            Some(cwd) => base.join(one(cwd)?),
            None => base.to_path_buf(),
        };
        Transport::Stdio {
            command: one(command)?,
            args: raw.args.iter().map(|arg| one(arg)).collect::<Result<_, _>>()?,
            env: map(&raw.env)?,
            cwd,
        }
    } else {
        let url = raw
            .url
            .as_deref()
            .ok_or_else(|| format!("type {kind} needs a `url`"))?;
        Transport::Http {
            url: one(url)?,
            headers: map(&raw.headers)?,
            legacy_sse: kind == "sse",
        }
    };
    let mut oauth = raw.oauth.unwrap_or_default();
    oauth.client_id = one(&oauth.client_id)?;
    oauth.scope = one(&oauth.scope)?;
    Ok(ServerConfig {
        name: name.to_string(),
        transport,
        timeout: Duration::from_secs(raw.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1)),
        oauth,
        enabled: raw.enabled.unwrap_or(true),
        source: source.to_path_buf(),
        fingerprint,
        approval: Approval::Granted,
    })
}

/// Where a server added by an extension is said to come from.
pub const EXTENSION_SOURCE: &str = "an extension";

/// A server from an entry shaped like those of `mcp.json`, handed over by an extension.
pub fn server_from_entry(
    name: &str,
    entry: Value,
    base: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ServerConfig, String> {
    resolve(name, entry, Path::new(EXTENSION_SOURCE), base, env)
}

/// Reads `files` in order; an entry in a later file replaces one of the same name. Relative `cwd`
/// values are taken from `base`.
pub fn load(files: &[PathBuf], base: &Path, env: &dyn Fn(&str) -> Option<String>) -> Loaded {
    let mut loaded = Loaded::default();
    let mut merged: BTreeMap<String, (Value, &Path)> = BTreeMap::new();
    for file in files {
        match read_servers(file) {
            Ok(servers) => {
                for (name, entry) in servers {
                    merged.insert(name, (entry, file));
                }
            }
            Err(problem) => loaded.problems.push((file.display().to_string(), problem)),
        }
    }
    for (name, (entry, source)) in merged {
        match resolve(&name, entry, source, base, env) {
            Ok(server) => loaded.servers.push(server),
            Err(problem) => loaded.problems.push((name, problem)),
        }
    }
    loaded
}

/// The names of the servers `file` defines; none when it is missing or unreadable.
pub fn defined(file: &Path) -> Vec<String> {
    read_servers(file)
        .map(|servers| servers.keys().cloned().collect())
        .unwrap_or_default()
}

/// Rewrites the `mcpServers` object of `file` with `change`, keeping the file's other keys.
fn edit(file: &Path, change: impl FnOnce(&mut Map<String, Value>) -> io::Result<()>) -> io::Result<()> {
    let invalid = |problem: String| io::Error::new(io::ErrorKind::InvalidData, problem);
    let mut root: Value = match std::fs::read_to_string(file) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| invalid(format!("not valid JSON: {e}")))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => return Err(e),
    };
    let object = root
        .as_object_mut()
        .ok_or_else(|| invalid("the file is not a JSON object".to_string()))?;
    let servers = object
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| invalid("`mcpServers` is not an object".to_string()))?;
    change(servers)?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(&root)?;
    text.push('\n');
    std::fs::write(file, text)
}

/// Adds or replaces the entry `name` in `file`.
pub fn add_server(file: &Path, name: &str, server: &RawServer) -> io::Result<()> {
    let entry = serde_json::to_value(server)?;
    edit(file, |servers| {
        servers.insert(name.to_string(), entry);
        Ok(())
    })
}

/// Removes the entry `name` from `file`. False when the file has no such entry.
pub fn remove_server(file: &Path, name: &str) -> io::Result<bool> {
    if !read_servers(file).is_ok_and(|servers| servers.contains_key(name)) {
        return Ok(false);
    }
    edit(file, |servers| {
        servers.remove(name);
        Ok(())
    })?;
    Ok(true)
}

/// Sets `enabled` on the entry `name` in `file`, leaving the rest of the entry as written.
pub fn set_enabled(file: &Path, name: &str, enabled: bool) -> io::Result<()> {
    edit(file, |servers| {
        let entry = servers
            .get_mut(name)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no server named `{name}`")))?;
        entry.insert("enabled".to_string(), Value::Bool(enabled));
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "TOKEN" => Some("s3cret".to_string()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn variables_expand_with_defaults_and_name_the_missing_one() {
        assert_eq!(expand("Bearer ${TOKEN}", &env).unwrap(), "Bearer s3cret");
        assert_eq!(
            expand("${MISSING:-fallback}/${TOKEN:-x}", &env).unwrap(),
            "fallback/s3cret"
        );
        assert_eq!(expand("${MISSING:-}", &env).unwrap(), "");
        assert_eq!(
            expand("a${EMPTY}b $HOME ${not a var}", &env).unwrap(),
            "ab $HOME ${not a var}"
        );
        assert_eq!(
            expand("${TOKEN} ${MISSING} ${ALSO_MISSING}", &env).unwrap_err(),
            "MISSING"
        );
    }

    #[test]
    fn later_files_win_and_bad_entries_are_reported_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home.json");
        let project = dir.path().join("project.json");
        std::fs::write(
            &home,
            r#"{ "mcpServers": {
                "fs": { "command": "npx", "args": ["-y", "server", "${ROOT:-.}"], "env": { "K": "${TOKEN}" }, "cwd": "sub" },
                "docs": { "type": "http", "url": "https://example.com/mcp", "timeout": 5,
                          "headers": { "Authorization": "Bearer ${TOKEN}" },
                          "oauth": { "clientId": "abc", "callbackPort": 8123 } },
                "shadowed": { "command": "old" },
                "needs": { "url": "https://${MISSING}/mcp" },
                "odd": { "type": "carrier-pigeon", "url": "x" },
                "empty": { "futureKey": true }
            } }"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"{ "mcpServers": {
                "shadowed": { "type": "sse", "url": "https://example.com/sse", "enabled": false }
            }, "other": 1 }"#,
        )
        .unwrap();
        let missing = dir.path().join("absent.json");
        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "{").unwrap();

        let base = dir.path().join("base");
        let loaded = load(
            &[home.clone(), missing, project.clone(), broken.clone()],
            &base,
            &env,
        );
        let names: Vec<&str> = loaded.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["docs", "fs", "shadowed"]);

        let docs = &loaded.servers[0];
        assert_eq!(docs.timeout, Duration::from_secs(5));
        assert_eq!(
            (docs.oauth.client_id.as_str(), docs.oauth.callback_port),
            ("abc", 8123)
        );
        assert_eq!(
            docs.transport,
            Transport::Http {
                url: "https://example.com/mcp".into(),
                headers: BTreeMap::from([("Authorization".to_string(), "Bearer s3cret".to_string())]),
                legacy_sse: false,
            }
        );
        let fs = &loaded.servers[1];
        assert_eq!(
            (fs.enabled, fs.timeout, &fs.source),
            (true, Duration::from_secs(DEFAULT_TIMEOUT_SECS), &home)
        );
        assert_eq!(
            fs.transport,
            Transport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "server".into(), ".".into()],
                env: BTreeMap::from([("K".to_string(), "s3cret".to_string())]),
                cwd: base.join("sub"),
            }
        );
        let shadowed = &loaded.servers[2];
        assert_eq!((shadowed.enabled, &shadowed.source), (false, &project));
        assert!(matches!(
            &shadowed.transport,
            Transport::Http { legacy_sse: true, .. }
        ));

        let problems: BTreeMap<&str, &str> = loaded
            .problems
            .iter()
            .map(|(what, why)| (what.as_str(), why.as_str()))
            .collect();
        assert_eq!(problems["needs"], "${MISSING} is not set");
        assert!(problems["odd"].starts_with("unknown type `carrier-pigeon`"));
        assert_eq!(problems["empty"], "needs a `command` or a `url`");
        assert!(problems[broken.display().to_string().as_str()].starts_with("not valid JSON"));
        assert_eq!(loaded.problems.len(), 4);
    }

    #[test]
    fn editing_keeps_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("mcp.json");
        let echo = RawServer {
            command: Some("echo-server".into()),
            args: vec!["--stdio".into()],
            ..RawServer::default()
        };
        add_server(&file, "echo", &echo).unwrap();
        let written = std::fs::read_to_string(&file).unwrap();
        assert!(written.ends_with("}\n"));
        assert_eq!(
            serde_json::from_str::<Value>(&written).unwrap(),
            serde_json::json!({ "mcpServers": { "echo": { "command": "echo-server", "args": ["--stdio"] } } })
        );

        std::fs::write(
            &file,
            r#"{ "theirs": [1], "mcpServers": { "echo": { "command": "x", "futureKey": 7 } } }"#,
        )
        .unwrap();
        set_enabled(&file, "echo", false).unwrap();
        let remote = RawServer {
            url: Some("https://example.com/mcp".into()),
            ..RawServer::default()
        };
        add_server(&file, "remote", &remote).unwrap();
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(root["theirs"], serde_json::json!([1]));
        assert_eq!(
            root["mcpServers"]["echo"],
            serde_json::json!({ "command": "x", "futureKey": 7, "enabled": false })
        );
        assert_eq!(
            root["mcpServers"]["remote"],
            serde_json::json!({ "url": "https://example.com/mcp" })
        );

        assert_eq!(
            set_enabled(&file, "ghost", true).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(remove_server(&file, "echo").unwrap());
        assert!(!remove_server(&file, "echo").unwrap());
        assert!(!remove_server(&dir.path().join("absent.json"), "echo").unwrap());
        assert!(!dir.path().join("absent.json").exists());
    }
}
