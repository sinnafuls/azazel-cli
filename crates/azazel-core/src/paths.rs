//! Where things live on disk: the azazel home, the project root and the per-project data directory.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `~/.azazel`, or `AZAZEL_HOME`.
    pub home: PathBuf,
    /// Nearest ancestor of `cwd` holding `.azazel/` or `.git`, else `cwd`.
    pub project_root: PathBuf,
    pub cwd: PathBuf,
    /// `<home>/projects/<slug>`.
    pub project_data: PathBuf,
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
        }
    }

    pub fn home_config(&self) -> PathBuf {
        self.home.join("config.toml")
    }
    pub fn auth_file(&self) -> PathBuf {
        self.home.join("auth.json")
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
    pub fn logs_dir(&self) -> PathBuf {
        self.home.join("logs")
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

    pub fn sessions_dir(&self) -> PathBuf {
        self.project_data.join("sessions")
    }
    /// Transcripts of the sub-agents of one session.
    pub fn agent_transcripts_dir(&self, session_id: &str) -> PathBuf {
        self.sessions_dir().join(session_id).join("agents")
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
    fn slug_collapses_separators_and_appends_hash() {
        let slug = project_slug(Path::new("/home/me/my  project!"));
        let (name, hash) = slug.rsplit_once('-').unwrap();
        assert_eq!(name, "home-me-my-project");
        assert_eq!(hash.len(), 8);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        // Paths that clean to the same text still get different slugs.
        assert_ne!(project_slug(Path::new("/a/b")), project_slug(Path::new("/a-b")));
    }

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
    fn the_default_home_in_the_users_home_is_not_a_project_marker_either() {
        // A machine that has run azazel has `~/.azazel`; work below it, outside any repository,
        // is still not a project.
        let tmp = tempfile::tempdir().unwrap();
        let base = canonical(tmp.path());
        let loose = base.join("notes");
        std::fs::create_dir_all(&loose).unwrap();
        let home = base.join("elsewhere");
        let paths = Paths::discover(&loose, home.to_str());
        assert_eq!(paths.project_root, loose);
    }
}
