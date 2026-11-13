//! Project trust: whether a project's configuration may run code and widen permissions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::ToolsConfig;
use crate::frontmatter;
use crate::paths::{Paths, canonical};

/// A file of one of these names means a project uses eslint, which loads it as code.
pub const ESLINT_CONFIGS: [&str; 12] = [
    "eslint.config.js",
    "eslint.config.mjs",
    "eslint.config.cjs",
    "eslint.config.ts",
    "eslint.config.mts",
    "eslint.config.cts",
    ".eslintrc",
    ".eslintrc.js",
    ".eslintrc.cjs",
    ".eslintrc.json",
    ".eslintrc.yml",
    ".eslintrc.yaml",
];

/// The user's word on one MCP server a project defines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ServerDecision {
    /// [`fingerprint`] of the definition the decision was made about.
    hash: String,
    approved: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Decision {
    trusted: bool,
    at: String,
    /// The project's MCP servers the user has decided about, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    servers: BTreeMap<String, ServerDecision>,
    /// Whether the `setup` event has fired for this project since it was trusted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    setup: bool,
}

/// A stable hash of a server definition as written: the same whatever the order of its keys, and
/// different as soon as any value in it changes.
pub fn fingerprint(definition: &serde_json::Value) -> String {
    fn canonical(value: &serde_json::Value, out: &mut String) {
        match value {
            serde_json::Value::Object(fields) => {
                let mut keys: Vec<&String> = fields.keys().collect();
                keys.sort();
                out.push('{');
                for key in keys {
                    out.push_str(&serde_json::Value::String(key.clone()).to_string());
                    out.push(':');
                    canonical(&fields[key], out);
                    out.push(',');
                }
                out.push('}');
            }
            serde_json::Value::Array(items) => {
                out.push('[');
                for item in items {
                    canonical(item, out);
                    out.push(',');
                }
                out.push(']');
            }
            scalar => out.push_str(&scalar.to_string()),
        }
    }
    let mut text = String::new();
    canonical(definition, &mut text);
    blake3::hash(text.as_bytes()).to_hex().to_string()
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
        // A project that is trusted again keeps what was decided about its servers.
        let servers = decisions
            .remove(&key(project))
            .map(|decision| decision.servers)
            .unwrap_or_default();
        decisions.insert(
            key(project),
            Decision {
                trusted,
                at: jiff::Timestamp::now().to_string(),
                servers,
                setup: false,
            },
        );
        self.save(&decisions)
    }

    fn save(&self, decisions: &BTreeMap<String, Decision>) -> std::io::Result<()> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.file, serde_json::to_vec_pretty(decisions)?)
    }

    /// What the user decided about the project's MCP server `name` as it is defined now (`hash`
    /// is its [`fingerprint`]). `None` for a server that is new or has changed since.
    pub fn server(&self, project: &Path, name: &str, hash: &str) -> Option<bool> {
        let decisions = self.load();
        let decision = decisions.get(&key(project))?.servers.get(name)?;
        (decision.hash == hash).then_some(decision.approved)
    }

    /// Records decisions about the project's MCP servers as `(name, fingerprint, approved)`.
    /// False when the project has no trust decision to keep them with (a `--trust` run).
    pub fn set_servers(&self, project: &Path, servers: &[(String, String, bool)]) -> std::io::Result<bool> {
        let mut decisions = self.load();
        let Some(decision) = decisions.get_mut(&key(project)) else {
            return Ok(false);
        };
        for (name, hash, approved) in servers {
            decision.servers.insert(
                name.clone(),
                ServerDecision {
                    hash: hash.clone(),
                    approved: *approved,
                },
            );
        }
        self.save(&decisions).map(|()| true)
    }

    /// True the first time it is asked about a project that is trusted: the moment for the
    /// `setup` event. The answer is remembered.
    pub fn take_setup(&self, project: &Path) -> bool {
        let mut decisions = self.load();
        let Some(decision) = decisions.get_mut(&key(project)) else {
            return false;
        };
        if !decision.trusted || decision.setup {
            return false;
        }
        decision.setup = true;
        self.save(&decisions).is_ok()
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

/// The checkers that would run on this project's files after a write and that run code of the
/// project while they check: each as what the trust prompt says of it. A checker counts when its
/// program is installed (`installed` looks one up on `PATH`), the project root has the file that
/// makes it apply, and `[tools.check]` does not replace it.
fn checks_that_run_code(root: &Path, tools: &ToolsConfig, installed: &dyn Fn(&str) -> bool) -> Vec<String> {
    if !tools.check_on_write {
        return Vec::new();
    }
    let mut reasons = Vec::new();
    let replaced = |kind: &str| tools.check.contains_key(kind);
    if root.join("Cargo.toml").is_file() && !replaced("rs") && installed("cargo-clippy") && installed("cargo")
    {
        reasons.push(
            "Rust code that checks on write would build with cargo clippy, running its build scripts and macros"
                .to_string(),
        );
    }
    if ESLINT_CONFIGS.iter().any(|name| root.join(name).is_file()) && installed("eslint") {
        reasons.push("an eslint config that checks on write would have eslint load and run".to_string());
    }
    if root.join("go.mod").is_file() && !replaced("go") && installed("go") {
        reasons.push("Go code that checks on write would build with go vet".to_string());
    }
    reasons
}

/// What in this project could run code or widen permissions. Empty means no trust decision is
/// needed. Each entry is a short description for the trust prompt and for `doctor`. `tools` is
/// the `[tools]` table as it is without the project's own gated settings.
pub fn needs_trust(paths: &Paths, claude_compat: bool, tools: &ToolsConfig) -> Vec<String> {
    let mut reasons = Vec::new();
    let project = paths.project_dir();
    if has_entries(&project.join("extensions")) {
        reasons.push("extensions in .azazel/extensions/".to_string());
    }
    // It can add providers, base URLs and key commands.
    if project.join("models.yml").is_file() {
        reasons.push("providers and models in .azazel/models.yml".to_string());
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
    reasons.extend(checks_that_run_code(&paths.project_root, tools, &|program| {
        which::which(program).is_ok()
    }));
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
    fn server_decisions_hold_for_one_definition_and_setup_fires_once() {
        let (tmp, paths) = project();
        let project = &paths.project_root;
        let store = TrustStore::new(paths.trust_file());
        let definition = serde_json::json!({"command": "npx", "args": ["-y", "server"], "env": {"A": "1"}});
        let hash = fingerprint(&definition);
        let reordered: serde_json::Value =
            serde_json::from_str(r#"{"env": {"A": "1"}, "args": ["-y", "server"], "command": "npx"}"#)
                .unwrap();
        assert_eq!(fingerprint(&reordered), hash);
        let changed = fingerprint(&serde_json::json!({"command": "npx", "args": ["-y", "other"]}));
        assert_ne!(changed, hash);

        // Nothing is kept for a project the user has not decided about.
        let decisions = [("files".to_string(), hash.clone(), true)];
        assert!(!store.set_servers(project, &decisions).unwrap());
        assert_eq!(store.server(project, "files", &hash), None);
        assert!(!store.take_setup(project));

        store.set(project, true).unwrap();
        assert!(store.set_servers(project, &decisions).unwrap());
        store
            .set_servers(project, &[("web".to_string(), "h2".to_string(), false)])
            .unwrap();
        assert_eq!(store.server(project, "files", &hash), Some(true));
        assert_eq!(store.server(project, "files", &changed), None);
        assert_eq!(store.server(project, "web", "h2"), Some(false));
        assert_eq!(store.server(project, "unknown", &hash), None);
        assert_eq!(store.server(&tmp.path().join("other"), "files", &hash), None);

        assert!(store.take_setup(project));
        assert!(!store.take_setup(project));
        // Trusting again keeps the server decisions and lets setup run again.
        store.set(project, true).unwrap();
        assert_eq!(store.server(project, "files", &hash), Some(true));
        assert!(store.take_setup(project));
        store.set(project, false).unwrap();
        assert!(!store.take_setup(project));

        // Files written before servers were recorded still load.
        std::fs::write(
            paths.trust_file(),
            format!(
                "{{{:?}: {{\"trusted\": true, \"at\": \"2026-01-01T00:00:00Z\"}}}}",
                key(project)
            ),
        )
        .unwrap();
        assert_eq!(store.decision(project), Some(true));
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
        assert_eq!(
            needs_trust(&paths, true, &ToolsConfig::default()),
            Vec::<String>::new()
        );
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

        let reasons = needs_trust(&paths, true, &ToolsConfig::default());
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
            !needs_trust(&paths, false, &ToolsConfig::default())
                .iter()
                .any(|r| r.contains("skill gate"))
        );
    }

    #[test]
    fn a_checker_that_runs_the_project_s_code_is_a_reason_when_it_would_apply() {
        let (_tmp, paths) = project();
        let root = &paths.project_root;
        let all = |_: &str| true;
        let mut tools = ToolsConfig::default();
        let reasons = |tools: &ToolsConfig, installed: &dyn Fn(&str) -> bool| {
            checks_that_run_code(root, tools, installed)
        };
        // Nothing in the project for such a checker to run on.
        std::fs::write(root.join("pyproject.toml"), "").unwrap();
        assert_eq!(reasons(&tools, &all), Vec::<String>::new());

        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.join("eslint.config.mjs"), "export default [];\n").unwrap();
        std::fs::write(root.join("go.mod"), "module example.com/demo\n").unwrap();
        assert_eq!(
            reasons(&tools, &all),
            [
                "Rust code that checks on write would build with cargo clippy, running its build scripts and macros",
                "an eslint config that checks on write would have eslint load and run",
                "Go code that checks on write would build with go vet",
            ]
        );
        // Only what is installed: clippy needs cargo and its own program.
        assert_eq!(
            reasons(&tools, &|program| program == "cargo" || program == "go"),
            ["Go code that checks on write would build with go vet"]
        );
        assert_eq!(reasons(&tools, &|_| false), Vec::<String>::new());
        // A checker of the user's own for the kind, or none at all, takes the built-in one away.
        tools.check.insert("rs".to_string(), String::new());
        tools.check.insert("go".to_string(), "staticcheck".to_string());
        assert_eq!(
            reasons(&tools, &all),
            ["an eslint config that checks on write would have eslint load and run"]
        );
        tools.check_on_write = false;
        assert_eq!(reasons(&tools, &all), Vec::<String>::new());
    }
}
