//! URL schemes the `read` tool can resolve (`artifact://`, `skill://`, `agent://`, `mcp://`, …),
//! and `local://`, the session's scratch files, which the file tools also write.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use async_trait::async_trait;
use azazel_agent::{Access, Services, ToolCtx};
use azazel_ai::Block;

use crate::session::SessionStore;

/// The scheme of the session's scratch files.
pub const LOCAL: &str = "local";

/// The lines a `read` call asks for with `offset` and `limit`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Lines {
    /// The first line, 1-based.
    pub offset: Option<usize>,
    /// The most lines.
    pub limit: Option<usize>,
}

#[async_trait]
pub trait Scheme: Send + Sync {
    /// The part before `://`.
    fn scheme(&self) -> &str;
    /// `rest` is everything after `scheme://`.
    async fn read(&self, rest: &str, ctx: &ToolCtx) -> Result<Vec<Block>, String>;
    /// [`Scheme::read`] for a call that may name the lines it wants. A scheme whose content is
    /// not paged by line returns all of it.
    async fn read_lines(&self, rest: &str, _lines: Lines, ctx: &ToolCtx) -> Result<Vec<Block>, String> {
        self.read(rest, ctx).await
    }
    /// What reading `rest` touches, and the subject permission rules are matched against; `None`
    /// leaves the address itself as the subject. A scheme that reaches the network answers
    /// `(Access::Net, Some("domain:<host>"))`, and is then asked about like `web_fetch`.
    fn access(&self, _rest: &str) -> (Access, Option<String>) {
        (Access::Read, None)
    }
}

/// Registered schemes; available to tools as a service.
#[derive(Default)]
pub struct Schemes {
    schemes: RwLock<Vec<(String, Arc<dyn Scheme>)>>,
}

impl Schemes {
    pub fn add(&self, owner: &str, scheme: Arc<dyn Scheme>) {
        self.schemes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push((owner.to_string(), scheme));
    }

    pub fn remove_owner(&self, owner: &str) {
        self.schemes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(o, _)| o != owner);
    }

    pub fn get(&self, scheme: &str) -> Option<Arc<dyn Scheme>> {
        let schemes = self.schemes.read().unwrap_or_else(PoisonError::into_inner);
        schemes
            .iter()
            .find(|(_, s)| s.scheme() == scheme)
            .map(|(_, s)| s.clone())
    }

    /// Scheme names, sorted.
    pub fn names(&self) -> Vec<String> {
        let schemes = self.schemes.read().unwrap_or_else(PoisonError::into_inner);
        let mut names: Vec<String> = schemes.iter().map(|(_, s)| s.scheme().to_string()).collect();
        names.sort();
        names.dedup();
        names
    }
}

/// Splits `scheme://rest`. A Windows drive path (`C:\x`, `C:/x`) is not a URL.
pub fn split_url(path: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = path.split_once("://")?;
    let valid = scheme.len() >= 2
        && scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some((scheme, rest))
}

/// The parts of the name in `local://<name>`, or why it names nothing inside the directory: a
/// name is relative, and has no `..` and no drive.
pub fn local_parts(rest: &str) -> Result<Vec<&str>, String> {
    let rest = rest.trim();
    if rest.starts_with(['/', '\\']) {
        return Err(format!(
            "`local://{rest}` is an absolute path; a local:// name is relative to the session's scratch directory"
        ));
    }
    let mut parts = Vec::new();
    for part in rest.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                return Err(format!(
                    "`local://{rest}` leads out of the session's scratch directory; `..` is not allowed there"
                ));
            }
            part if part.contains(':') => {
                return Err(format!(
                    "`local://{rest}`: `{part}` is not a usable name; a local:// name has no drive or `:`"
                ));
            }
            part => parts.push(part),
        }
    }
    Ok(parts)
}

/// The file `local://<rest>` names in `dir`, the session's scratch directory.
pub fn local_path(dir: &Path, rest: &str) -> Result<PathBuf, String> {
    Ok(local_parts(rest)?
        .into_iter()
        .fold(dir.to_path_buf(), |path, part| path.join(part)))
}

/// The session's scratch directory, created when it is not there yet.
pub fn local_dir(services: &Services) -> Result<PathBuf, String> {
    let session = services
        .get::<SessionStore>()
        .ok_or_else(|| "no session is open, so there are no local:// files".to_string())?;
    let dir = session.local_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_names_map_into_the_directory_and_never_out_of_it() {
        let dir = Path::new("scratch");
        assert_eq!(local_path(dir, "report.md").unwrap(), dir.join("report.md"));
        assert_eq!(
            local_path(dir, "runs/2/./out.txt").unwrap(),
            dir.join("runs").join("2").join("out.txt")
        );
        assert_eq!(local_path(dir, "").unwrap(), dir);
        for escape in [
            "../x",
            "a/../../x",
            "a\\..\\x",
            "/etc/passwd",
            "\\x",
            "C:/x",
            "C:x",
        ] {
            assert!(local_path(dir, escape).is_err(), "{escape}");
        }
    }

    #[test]
    fn split_url_accepts_schemes_and_rejects_paths() {
        assert_eq!(split_url("artifact://call_1"), Some(("artifact", "call_1")));
        assert_eq!(
            split_url("mcp://server/mem://greeting"),
            Some(("mcp", "server/mem://greeting"))
        );
        assert_eq!(
            split_url("https://example.com/a?b=c"),
            Some(("https", "example.com/a?b=c"))
        );
        for not_url in [
            "C://Users/me",
            "src/main.rs",
            "/abs/path",
            "a b://x",
            "1x://y",
            "://x",
        ] {
            assert_eq!(split_url(not_url), None, "{not_url}");
        }
    }
}
