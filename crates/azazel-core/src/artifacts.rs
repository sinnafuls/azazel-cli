//! Files the harness writes on the agent's behalf: spilled tool output, patches, screenshots.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use azazel_agent::{ArtifactSink, ToolCtx};
use azazel_ai::Block;

use crate::scheme::Scheme;

pub struct ArtifactStore {
    dir: PathBuf,
}

impl ArtifactStore {
    pub fn new(dir: PathBuf) -> Self {
        ArtifactStore { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes `bytes` as `<dir>/<name>` and returns the path. `name` is reduced to its file name.
    pub fn save(&self, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(file_name(name));
        std::fs::write(&path, bytes)?;
        Ok(path)
    }

    fn resolve(&self, id: &str) -> Option<PathBuf> {
        let name = file_name(id);
        [self.dir.join(&name), self.dir.join(format!("{name}.txt"))]
            .into_iter()
            .find(|p| p.is_file())
    }
}

/// The last path component with anything outside `[A-Za-z0-9._-]` replaced, so an id can never
/// address a file outside the artifact directory.
fn file_name(id: &str) -> String {
    let last = id.rsplit(['/', '\\']).next().unwrap_or(id);
    let clean: String = last
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.trim_matches('.').is_empty() {
        "artifact".to_string()
    } else {
        clean
    }
}

impl ArtifactSink for ArtifactStore {
    fn spill(&self, call_id: &str, text: &str) -> Option<String> {
        let name = file_name(call_id);
        match self.save(&format!("{name}.txt"), text.as_bytes()) {
            Ok(_) => Some(format!("artifact://{name}")),
            Err(e) => {
                tracing::warn!("could not store artifact {name}: {e}");
                None
            }
        }
    }
}

/// `artifact://<id>` for the `read` tool.
pub struct ArtifactScheme(pub std::sync::Arc<ArtifactStore>);

#[async_trait]
impl Scheme for ArtifactScheme {
    fn scheme(&self) -> &str {
        "artifact"
    }

    async fn read(&self, rest: &str, _: &ToolCtx) -> Result<Vec<Block>, String> {
        let path = self
            .0
            .resolve(rest)
            .ok_or_else(|| format!("no artifact named `{rest}`"))?;
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(vec![Block::text(text)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spill_then_resolve_round_trips_and_ids_cannot_escape_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(dir.path().join("artifacts"));
        assert_eq!(
            store.spill("call_1", "full output").as_deref(),
            Some("artifact://call_1")
        );
        let path = store.resolve("call_1").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "full output");
        assert_eq!(path.parent(), Some(store.dir()));

        std::fs::write(dir.path().join("secret.txt"), "outside").unwrap();
        assert_eq!(store.resolve("../secret"), None);
        assert_eq!(
            store.spill("../../evil id", "x").as_deref(),
            Some("artifact://evil_id")
        );
        assert!(store.dir().join("evil_id.txt").is_file());
        assert_eq!(file_name(".."), "artifact");
    }
}
