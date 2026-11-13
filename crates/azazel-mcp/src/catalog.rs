//! What a server offered the last time it was connected, kept on disk so that a server that
//! starts on demand can be offered, searched and listed without being started.

use std::path::{Path, PathBuf};

use rmcp::model::Prompt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::ServerConfig;

/// One tool as the server describes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CachedTool {
    /// The server's own name for it.
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub schema: Value,
    #[serde(default)]
    pub read_only: bool,
}

/// The file of one server: `<dir>/<server>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Cached {
    pub server: String,
    /// The fingerprint of the entry the list was fetched for. Another one means the entry
    /// changed, and with it possibly the program behind it.
    pub fingerprint: String,
    pub tools: Vec<CachedTool>,
    #[serde(default)]
    pub prompts: Vec<Prompt>,
    #[serde(default)]
    pub instructions: Option<String>,
}

fn file(dir: &Path, server: &str) -> PathBuf {
    dir.join(format!("{}.json", crate::name_segment(server)))
}

/// What is known about `config`'s server. `None` when nothing was kept, the file cannot be read,
/// or it describes another server or another version of this one's entry.
pub(crate) fn read(dir: &Path, config: &ServerConfig) -> Option<Cached> {
    let text = std::fs::read_to_string(file(dir, &config.name)).ok()?;
    let cached: Cached = serde_json::from_str(&text).ok()?;
    (cached.server == config.name && cached.fingerprint == config.fingerprint).then_some(cached)
}

/// Keeps `cached`; a file that already says the same is left alone. Not being able to write it
/// costs the next session a start of the server, nothing else.
pub(crate) fn write(dir: &Path, cached: &Cached) {
    let path = file(dir, &cached.server);
    let Ok(text) = serde_json::to_string(cached) else {
        return;
    };
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == text) {
        return;
    }
    let staged = path.with_extension("json.new");
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&staged, text))
        .and_then(|()| std::fs::rename(&staged, &path));
    if let Err(e) = written {
        tracing::warn!(
            "could not keep the tool list of MCP server {}: {e}",
            cached.server
        );
    }
}
