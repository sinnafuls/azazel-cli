//! Keeps the paths a script reads and writes inside the directories it may use.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

/// The directories the `azazel.fs` functions of one script may touch.
pub(crate) struct Roots {
    /// The project root. Relative paths start here.
    pub project: PathBuf,
    /// The configured additional directories and the script's data directory.
    pub others: Vec<PathBuf>,
}

impl Roots {
    /// Where `path` really is, with `..` and symlinks resolved, when that lies inside one of the
    /// directories. The result is the path to open: it cannot lead anywhere else.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        if path.is_empty() {
            return Err("the path is empty".to_string());
        }
        let target = real(&self.project.join(path)).map_err(|e| format!("cannot resolve `{path}`: {e}"))?;
        let inside = std::iter::once(&self.project)
            .chain(&self.others)
            .any(|root| real(root).is_ok_and(|root| target.starts_with(root)));
        if inside {
            Ok(target)
        } else {
            Err(format!(
                "`{path}` is outside the directories an extension may access (the project, the additional \
                 directories and its data directory)"
            ))
        }
    }
}

/// `path` without `.` and `..`, with the part of it that exists replaced by its canonical form. A
/// symlink whose target is missing is an error rather than a name to create.
fn real(path: &Path) -> io::Result<PathBuf> {
    let path = lexical(path);
    let mut existing = path.clone();
    let mut missing: Vec<OsString> = Vec::new();
    loop {
        if existing.symlink_metadata().is_ok() {
            let mut real = existing.canonicalize()?;
            real.extend(missing.iter().rev());
            return Ok(real);
        }
        let Some(name) = existing.file_name().map(OsString::from) else {
            return Ok(path);
        };
        missing.push(name);
        existing.pop();
    }
}

/// `path` with `.` dropped and each `..` cancelling the component before it.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(dir: &Path) -> Roots {
        let project = dir.join("project");
        let data = dir.join("data");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("src/a.txt"), "a").unwrap();
        std::fs::write(dir.join("secret.txt"), "s").unwrap();
        Roots {
            project,
            others: vec![data],
        }
    }

    #[test]
    fn paths_inside_a_root_resolve_whether_or_not_they_exist() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots(dir.path());
        let project = roots.project.canonicalize().unwrap();
        assert_eq!(
            roots.resolve("src/a.txt").unwrap(),
            project.join("src").join("a.txt")
        );
        assert_eq!(
            roots.resolve("src/../src/./a.txt").unwrap(),
            project.join("src").join("a.txt")
        );
        assert_eq!(
            roots.resolve("new/deep/file.txt").unwrap(),
            project.join("new").join("deep").join("file.txt")
        );
        // The data directory does not exist yet and is still a place to write to.
        let in_data = dir.path().join("data").join("cache.json");
        assert!(roots.resolve(in_data.to_str().unwrap()).is_ok());
    }

    #[test]
    fn paths_that_leave_every_root_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots(dir.path());
        for path in ["../secret.txt", "src/../../secret.txt", "..", ""] {
            assert!(roots.resolve(path).is_err(), "{path}");
        }
        let absolute = dir.path().join("secret.txt");
        let refused = roots.resolve(absolute.to_str().unwrap()).unwrap_err();
        assert!(refused.contains("outside"), "{refused}");
        // A sibling whose name merely starts like the project is not inside it.
        std::fs::create_dir_all(dir.path().join("project-two")).unwrap();
        assert!(roots.resolve("../project-two/x").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cannot_carry_a_path_outside() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots(dir.path());
        std::os::unix::fs::symlink(dir.path(), roots.project.join("up")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("not-there"), roots.project.join("dangling")).unwrap();
        std::os::unix::fs::symlink(roots.project.join("src"), roots.project.join("alias")).unwrap();
        assert!(roots.resolve("up/secret.txt").is_err());
        assert!(roots.resolve("up/new.txt").is_err());
        assert!(roots.resolve("dangling").is_err());
        assert!(roots.resolve("alias/a.txt").is_ok());
    }
}
