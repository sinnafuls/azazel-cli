//! Helpers shared by the file tools.

use std::io;
use std::path::{Component, Path, PathBuf};

use azazel_agent::ToolOutput;
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

    #[test]
    fn write_atomic_replaces_content_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep/nested/a.txt");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.txt"]);
    }

    #[test]
    fn unified_diff_has_headers_and_hunks() {
        let diff = unified_diff("a\nb\nc\n", "a\nB\nc\n", "src/x.rs");
        assert!(
            diff.starts_with("--- a/src/x.rs\n+++ b/src/x.rs\n@@ -1,3 +1,3 @@\n"),
            "{diff}"
        );
        assert!(diff.contains("-b\n+B\n"));
    }
}
