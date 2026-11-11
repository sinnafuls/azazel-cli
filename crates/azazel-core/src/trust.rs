//! Project trust: whether a project's configuration may run code and widen permissions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::frontmatter;
use crate::paths::{Paths, canonical};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Decision {
    trusted: bool,
    at: String,
}

/// `~/.azazel/trust.json`: canonical project path to the user's decision.
pub struct TrustStore {
    file: PathBuf,
}

fn key(project: &Path) -> String {
    canonical(project).to_string_lossy().into_owned()
}

impl TrustStore {
    pub fn new(file: PathBuf) -> Self {
        TrustStore { file }
    }

    fn load(&self) -> BTreeMap<String, Decision> {
        std::fs::read_to_string(&self.file)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// `None` when the user has not decided about this project.
    pub fn decision(&self, project: &Path) -> Option<bool> {
        self.load().get(&key(project)).map(|d| d.trusted)
    }

    pub fn set(&self, project: &Path, trusted: bool) -> std::io::Result<()> {
        let mut decisions = self.load();
        decisions.insert(
            key(project),
            Decision {
                trusted,
                at: jiff::Timestamp::now().to_string(),
            },
        );
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.file, serde_json::to_vec_pretty(&decisions)?)
    }
}

/// Project skill roots, lowest precedence first.
pub fn project_skill_roots(paths: &Paths, claude_compat: bool) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if claude_compat {
        roots.push(paths.project_root.join(".claude").join("skills"));
        roots.push(paths.project_root.join(".agents").join("skills"));
    }
    roots.push(paths.project_dir().join("skills"));
    roots
}

/// What in this project could run code or widen permissions. Empty means no trust decision is
/// needed. Each entry is a short description for the trust prompt and for `doctor`.
pub fn needs_trust(paths: &Paths, claude_compat: bool) -> Vec<String> {
    let mut reasons = Vec::new();
    let project = paths.project_dir();
    if has_entries(&project.join("extensions")) {
        reasons.push("extensions in .azazel/extensions/".to_string());
    }
    for (file, label) in [
        (project.join("mcp.json"), ".azazel/mcp.json"),
        (paths.project_root.join(".mcp.json"), ".mcp.json"),
    ] {
        if file.is_file() {
            reasons.push(format!("MCP servers in {label}"));
        }
    }
    for (file, label) in [
        (paths.project_config(), ".azazel/config.toml"),
        (paths.project_local_config(), ".azazel/config.local.toml"),
    ] {
        let Some(table) = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| t.parse::<toml::Table>().ok())
        else {
            continue;
        };
        for setting in crate::config::gated_settings(&table) {
            reasons.push(format!("`{setting}` in {label}"));
        }
    }
    for root in project_skill_roots(paths, claude_compat) {
        for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let file = entry.path().join("SKILL.md");
            let has_hooks = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| frontmatter::split(&text).map(|(yaml, _)| yaml.to_string()))
                .and_then(|yaml| frontmatter::parse(&yaml).ok())
                .is_some_and(|map| map.contains_key("hooks"));
            if has_hooks {
                reasons.push(format!("hooks in skill {}", entry.file_name().to_string_lossy()));
            }
        }
    }
    reasons
}

fn has_entries(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join(".azazel")).unwrap();
        let paths = Paths::discover(&root, Some(&tmp.path().join("home").to_string_lossy()));
        (tmp, paths)
    }

    #[test]
    fn decisions_are_per_project_and_persist() {
        let (tmp, paths) = project();
        let store = TrustStore::new(paths.trust_file());
        assert_eq!(store.decision(&paths.project_root), None);
        store.set(&paths.project_root, true).unwrap();
        store.set(&tmp.path().join("other"), false).unwrap();

        let reopened = TrustStore::new(paths.trust_file());
        assert_eq!(reopened.decision(&paths.project_root), Some(true));
        assert_eq!(reopened.decision(&tmp.path().join("other")), Some(false));
        assert_eq!(reopened.decision(&tmp.path().join("third")), None);
    }

    #[test]
    fn plain_projects_need_no_trust() {
        let (_tmp, paths) = project();
        std::fs::write(paths.project_root.join("AGENTS.md"), "be nice").unwrap();
        std::fs::write(
            paths.project_config(),
            "[team]\nmax_parallel = 2\n[permissions]\ndeny = [\"shell(rm:*)\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(paths.project_dir().join("skills/notes")).unwrap();
        std::fs::write(
            paths.project_dir().join("skills/notes/SKILL.md"),
            "---\nname: notes\ndescription: d\n---\nx",
        )
        .unwrap();
        assert_eq!(needs_trust(&paths, true), Vec::<String>::new());
    }

    #[test]
    fn everything_that_can_run_code_is_reported() {
        let (_tmp, paths) = project();
        let root = &paths.project_root;
        std::fs::create_dir_all(paths.project_dir().join("extensions")).unwrap();
        std::fs::write(paths.project_dir().join("extensions/x.luau"), "").unwrap();
        std::fs::write(root.join(".mcp.json"), "{}").unwrap();
        std::fs::write(paths.project_dir().join("mcp.json"), "{}").unwrap();
        std::fs::write(
            paths.project_config(),
            "[permissions]\nallow = [\"shell\"]\n[providers.x]\napi_key_command = \"cat key\"\n[[hooks.stop]]\ncommand = \"x\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".claude/skills/gate")).unwrap();
        std::fs::write(
            root.join(".claude/skills/gate/SKILL.md"),
            "---\nname: gate\ndescription: d\nhooks:\n  Stop: []\n---\nbody",
        )
        .unwrap();

        let reasons = needs_trust(&paths, true);
        for expected in [
            "extensions in .azazel/extensions/",
            "MCP servers in .azazel/mcp.json",
            "MCP servers in .mcp.json",
            "`hooks` in .azazel/config.toml",
            "`providers` in .azazel/config.toml",
            "`permissions.allow` in .azazel/config.toml",
            "hooks in skill gate",
        ] {
            assert!(
                reasons.iter().any(|r| r == expected),
                "missing `{expected}` in {reasons:?}"
            );
        }
        // Without compatibility mode the .claude skill root is not read at all.
        assert!(
            !needs_trust(&paths, false)
                .iter()
                .any(|r| r.contains("skill gate"))
        );
    }
}
