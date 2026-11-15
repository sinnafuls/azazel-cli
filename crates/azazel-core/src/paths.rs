//! Where things live on disk: the azazel home, the project root and the per-project data directory.

use std::path::{Path, PathBuf};

/// The folder, inside a session's own folder, that holds the transcripts of its sub-agents.
pub(crate) const AGENT_TRANSCRIPTS: &str = "agents";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `~/.azazel`, or `AZAZEL_HOME`.
    pub home: PathBuf,
    /// Nearest ancestor of `cwd` holding `.azazel/` or `.git`, else `cwd`.
    pub project_root: PathBuf,
    pub cwd: PathBuf,
    /// `<home>/projects/<slug>`.
    pub project_data: PathBuf,
    /// Where sessions are kept instead of `<project_data>/sessions` (`--session-dir`, `--no-session`).
    pub sessions_override: Option<PathBuf>,
}

impl Paths {
    /// `home_override` is the value of `AZAZEL_HOME`, if set.
    pub fn discover(cwd: &Path, home_override: Option<&str>) -> Self {
        let cwd = canonical(cwd);
        let default_home = std::env::home_dir().map(|home| home.join(".azazel"));
        let home = match home_override.filter(|h| !h.is_empty()) {
            Some(home) => absolute(Path::new(home)),
            None => default_home.clone().unwrap_or_else(|| cwd.clone()),
        };
        let project_root = find_project_root(&cwd, &home, default_home.as_deref());
        let project_data = home.join("projects").join(project_slug(&project_root));
        Paths {
            home,
            project_root,
            cwd,
            project_data,
            sessions_override: None,
        }
    }

    pub fn home_config(&self) -> PathBuf {
        self.home.join("config.toml")
    }
    pub fn auth_file(&self) -> PathBuf {
        self.home.join("auth.json")
    }
    /// Where `azazel secrets` keeps the credentials agents use without seeing them.
    pub fn secrets_file(&self) -> PathBuf {
        self.home.join("secrets.json")
    }
    pub fn home_mcp(&self) -> PathBuf {
        self.home.join("mcp.json")
    }
    pub fn trust_file(&self) -> PathBuf {
        self.home.join("trust.json")
    }
    pub fn packages_file(&self) -> PathBuf {
        self.home.join("packages.toml")
    }
    pub fn packages_dir(&self) -> PathBuf {
        self.home.join("packages")
    }
    pub fn keybindings_file(&self) -> PathBuf {
        self.home.join("keybindings.toml")
    }
    /// Where azazel installs the language servers it can install itself.
    pub fn lsp_dir(&self) -> PathBuf {
        self.home.join("lsp")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.home.join("logs")
    }
    /// Where bug reports are written.
    pub fn reports_dir(&self) -> PathBuf {
        self.home.join("reports")
    }
    /// The user data directory of the browser agents drive.
    pub fn browser_profile_dir(&self) -> PathBuf {
        self.home.join("browser-profile")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.home.join("cache")
    }
    pub fn models_cache(&self) -> PathBuf {
        self.cache_dir().join("models.json")
    }
    pub fn global_memory_db(&self) -> PathBuf {
        self.home.join("memory.db")
    }

    /// `<project>/.azazel`.
    pub fn project_dir(&self) -> PathBuf {
        self.project_root.join(".azazel")
    }
    pub fn project_config(&self) -> PathBuf {
        self.project_dir().join("config.toml")
    }
    pub fn project_local_config(&self) -> PathBuf {
        self.project_dir().join("config.local.toml")
    }
    pub fn plans_dir(&self) -> PathBuf {
        self.project_dir().join("plans")
    }
    pub fn worktrees_dir(&self) -> PathBuf {
        self.project_dir().join("worktrees")
    }
    /// The project this one is a worktree of, when it is one azazel made: the directory that
    /// holds `.azazel/worktrees/<name>`. Otherwise this project's root.
    pub fn main_project_root(&self) -> PathBuf {
        let root = &self.project_root;
        let parent = root.parent();
        let azazel = parent.and_then(Path::parent);
        let named = |dir: Option<&Path>, name: &str| dir.and_then(Path::file_name).is_some_and(|n| n == name);
        match azazel.and_then(Path::parent) {
            Some(main) if named(parent, "worktrees") && named(azazel, ".azazel") => main.to_path_buf(),
            _ => root.clone(),
        }
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.sessions_override
            .clone()
            .unwrap_or_else(|| self.project_data.join("sessions"))
    }
    /// Transcripts of the sub-agents of one session.
    pub fn agent_transcripts_dir(&self, session_id: &str) -> PathBuf {
        self.sessions_dir().join(session_id).join(AGENT_TRANSCRIPTS)
    }
    pub fn artifacts_dir(&self) -> PathBuf {
        self.project_data.join("artifacts")
    }
    pub fn shadow_git(&self) -> PathBuf {
        self.project_data.join("shadow.git")
    }
    pub fn memory_db(&self) -> PathBuf {
        self.project_data.join("memory.db")
    }
    pub fn index_db(&self) -> PathBuf {
        self.project_data.join("index.db")
    }
}

/// Canonical, never a `\\?\` path on Windows. A path that does not exist (yet, or any more) keeps
/// its missing tail on the canonical form of its nearest existing ancestor, so it names the same
/// file before and after it exists even under a symlinked directory (macOS `/var` is one).
pub fn canonical(path: &Path) -> PathBuf {
    if let Ok(found) = dunce::canonicalize(path) {
        return found;
    }
    let full = absolute(path);
    let mut missing = Vec::new();
    let mut existing = full.as_path();
    while let Some(parent) = existing.parent() {
        // A `..` in the missing part cannot be resolved against a real directory.
        let Some(name) = existing.file_name() else {
            return full;
        };
        missing.push(name.to_os_string());
        existing = parent;
        if let Ok(found) = dunce::canonicalize(existing) {
            return missing.iter().rev().fold(found, |path, part| path.join(part));
        }
    }
    full
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The azazel home is itself a `.azazel` directory, so it never marks its parent as a project.
fn find_project_root(cwd: &Path, home: &Path, default_home: Option<&Path>) -> PathBuf {
    for dir in cwd.ancestors() {
        let marker = dir.join(".azazel");
        // `~/.azazel` is azazel's own home; treating it as a marker would make the user's home
        // directory the project of every path outside a repository on the machine.
        let own = marker == home || default_home.is_some_and(|home| marker == home);
        if (marker.is_dir() && !own) || dir.join(".git").exists() {
            return dir.to_path_buf();
        }
    }
    cwd.to_path_buf()
}

/// The path with every run of characters outside `[A-Za-z0-9]` turned into one `-`, then `-` and
/// the first 8 hex digits of the path's BLAKE3 hash.
pub fn project_slug(root: &Path) -> String {
    let text = root.to_string_lossy();
    let mut slug = String::with_capacity(text.len() + 9);
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    format!("{slug}-{}", &blake3::hash(text.as_bytes()).to_hex()[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_root_is_nearest_marker_and_home_is_not_a_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let base = canonical(tmp.path());
        let home = base.join("user").join(".azazel");
        let repo = base.join("user").join("repo");
        let nested = repo.join("crates").join("x");
        let inner = repo.join("sub");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(inner.join(".azazel")).unwrap();
        let loose = base.join("user").join("notes");
        std::fs::create_dir_all(&loose).unwrap();

        let home_str = home.to_string_lossy().into_owned();
        assert_eq!(Paths::discover(&nested, Some(&home_str)).project_root, repo);
        assert_eq!(Paths::discover(&inner, Some(&home_str)).project_root, inner);
        // `user/` holds the azazel home, which must not turn `user/` into a project.
        let paths = Paths::discover(&loose, Some(&home_str));
        assert_eq!(paths.project_root, loose);
        assert_eq!(paths.home, home);
        assert!(paths.sessions_dir().starts_with(home.join("projects")));
    }

    #[test]
    fn a_worktree_azazel_made_belongs_to_the_project_that_holds_it() {
        let tmp = tempfile::tempdir().unwrap();
        let base = canonical(tmp.path());
        let main = base.join("repo");
        let worktree = main.join(".azazel").join("worktrees").join("feature");
        std::fs::create_dir_all(main.join(".git")).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        // A worktree has a `.git` file of its own, which makes it a project.
        std::fs::write(worktree.join(".git"), "gitdir: elsewhere").unwrap();
        let home = base.join("home");
        let inside = Paths::discover(&worktree, home.to_str());
        assert_eq!(inside.project_root, worktree);
        assert_eq!(inside.main_project_root(), main);
        let outside = Paths::discover(&main, home.to_str());
        assert_eq!(outside.main_project_root(), main);
    }
}
