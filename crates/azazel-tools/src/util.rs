//! Helpers shared by the file tools.

use std::io;
use std::path::{Component, Path, PathBuf};

use azazel_agent::{ToolCtx, ToolOutput};
use azazel_core::SessionStore;
use azazel_core::scheme::{LOCAL, local_dir, local_path, split_url};
use serde::de::DeserializeOwned;
use serde_json::Value;
use similar::TextDiff;

/// Typed tool arguments, or the error result to hand back to the model.
pub fn parse_input<T: DeserializeOwned>(input: Value) -> Result<T, ToolOutput> {
    serde_json::from_value(input).map_err(|e| ToolOutput::error(format!("invalid arguments: {e}")))
}

/// `path` as the model wrote it, made absolute against `cwd`: `~/` expands to the home directory
/// and `.`/`..` components are folded without touching the filesystem.
pub fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let path = path.trim();
    let expanded = match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => std::env::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    };
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// The file a file tool's `path` names: a path made absolute against the agent's directory (see
/// [`resolve_path`]), or for `local://<name>` that file in the session's scratch directory. An
/// address of any other scheme is refused, naming `tool`.
pub fn resolve_target(ctx: &ToolCtx, path: &str, tool: &str) -> Result<PathBuf, String> {
    match split_url(path.trim()) {
        Some((LOCAL, rest)) => local_path(&local_dir(&ctx.services)?, rest),
        Some((scheme, _)) => Err(format!(
            "{tool} does not take {scheme}:// addresses; only local:// and file paths"
        )),
        None => Ok(resolve_path(&ctx.cwd, path)),
    }
}

/// `path` as a result shows it: `local://<name>` for a file in the session's scratch directory,
/// otherwise as [`display_path`] shows it against the agent's directory.
pub fn shown_path(ctx: &ToolCtx, path: &Path) -> String {
    if let Some(session) = ctx.services.get::<SessionStore>()
        && let Ok(name) = path.strip_prefix(session.local_dir())
    {
        return format!("{LOCAL}://{}", name.to_string_lossy().replace('\\', "/"));
    }
    display_path(&ctx.cwd, path)
}

/// How a tool that lists files shows them: relative to a directory, after a prefix (`local://`
/// for the session's scratch directory).
pub struct Shown {
    root: PathBuf,
    prefix: &'static str,
}

impl Shown {
    pub fn relative_to(root: &Path) -> Self {
        Shown {
            root: root.to_path_buf(),
            prefix: "",
        }
    }

    pub fn show(&self, path: &Path) -> String {
        format!("{}{}", self.prefix, display_path(&self.root, path))
    }
}

/// Where a search tool starts, `path` or the agent's directory, and how it shows what it finds:
/// relative to the agent's directory, or as `local://<name>` in the session's scratch directory.
pub fn search_base(ctx: &ToolCtx, path: Option<&str>, tool: &str) -> Result<(PathBuf, Shown), String> {
    let Some(path) = path else {
        return Ok((ctx.cwd.clone(), Shown::relative_to(&ctx.cwd)));
    };
    match split_url(path.trim()) {
        Some((LOCAL, rest)) => {
            let dir = local_dir(&ctx.services)?;
            let base = local_path(&dir, rest)?;
            let shown = Shown {
                root: dir,
                prefix: "local://",
            };
            Ok((base, shown))
        }
        Some((scheme, _)) => Err(format!(
            "{tool} does not take {scheme}:// addresses; only local:// and file paths"
        )),
        None => Ok((resolve_path(&ctx.cwd, path), Shown::relative_to(&ctx.cwd))),
    }
}

/// `path` relative to `base` with forward slashes, or absolute when it is outside `base`.
pub fn display_path(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Writes through a temporary file in the same directory and renames it into place, keeping the
/// permissions of the file being replaced.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.azazel-tmp"));
    std::fs::write(&tmp, bytes)?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Unified diff with three lines of context.
pub fn unified_diff(old: &str, new: &str, path: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_joins_and_folds_dots() {
        let cwd = if cfg!(windows) {
            Path::new("C:\\work\\proj")
        } else {
            Path::new("/work/proj")
        };
        let expect = |parts: &[&str]| {
            parts
                .iter()
                .fold(cwd.parent().unwrap().to_path_buf(), |p, part| p.join(part))
        };
        assert_eq!(resolve_path(cwd, "src/./a.rs"), expect(&["proj", "src", "a.rs"]));
        assert_eq!(resolve_path(cwd, "../other/b.rs"), expect(&["other", "b.rs"]));
        assert_eq!(
            resolve_path(cwd, " src/x/../y.rs "),
            expect(&["proj", "src", "y.rs"])
        );
        let absolute = cwd.join("z.txt");
        assert_eq!(
            resolve_path(Path::new("elsewhere"), &absolute.to_string_lossy()),
            absolute
        );
    }
}
