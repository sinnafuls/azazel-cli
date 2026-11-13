#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use serde_json::Value;

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        match entry.file_type().unwrap().is_dir() {
            true => copy_dir(&entry.path(), &target),
            false => drop(std::fs::copy(entry.path(), target).unwrap()),
        }
    }
}

/// A user's home made of the fixture homes of the named harnesses, a project, and an azazel
/// home with nothing in it.
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new(sources: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/migrate");
        std::fs::create_dir_all(dir.path().join("user")).unwrap();
        for source in sources {
            copy_dir(&fixtures.join(source).join("home"), &dir.path().join("user"));
        }
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        Sandbox { dir }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    /// `azazel migrate` as the user whose home the sandbox is.
    fn migrate(&self, args: &[&str]) -> Output {
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.path("azazel"))
            .env("HOME", self.path("user"))
            .env("USERPROFILE", self.path("user"))
            .env_remove("AZAZEL_MOCK_SCRIPT");
        // Nothing of the machine's own harnesses is looked at.
        for variable in [
            "PI_CODING_AGENT_DIR",
            "PI_CODING_AGENT_SESSION_DIR",
            "PI_CONFIG_DIR",
            "OMP_PROFILE",
            "PI_PROFILE",
            "JCODE_HOME",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
        ] {
            command.env_remove(variable);
        }
        command
            .arg("--cwd")
            .arg(self.path("project"))
            .arg("migrate")
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, relative: &str) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.path(relative)).unwrap()).unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_dry_run_prints_the_plan_and_a_run_brings_it_over() {
    let sandbox = Sandbox::new(&["pi", "codex"]);

    let plan = sandbox.migrate(&["--dry-run"]);
    assert!(plan.status.success(), "{}", text(&plan.stderr));
    let stdout = text(&plan.stdout);
    assert!(
        stdout.contains("would import  role default = team-gateway/big-model:high"),
        "{stdout}"
    );
    assert!(stdout.contains("Codex CLI ("), "{stdout}");
    assert!(stdout.contains("nothing was written"), "{stdout}");
    // The home holds the log of the run and nothing a migration writes.
    for written in [
        "config.toml",
        "models.yml",
        "auth.json",
        "mcp.json",
        "skills",
        "commands",
        "projects",
    ] {
        assert!(
            !sandbox.path("azazel").join(written).exists(),
            "the dry run wrote {written}"
        );
    }
    // What was found is said before anything is done.
    let stderr = text(&plan.stderr);
    assert!(
        stderr.contains(
            "pi: 3 models and roles, 3 keys and sign-ins, 1 skill, 1 command, 2 MCP servers, 1 rule file"
        ),
        "{stderr}"
    );

    let run = sandbox.migrate(&[]);
    assert!(run.status.success(), "{}", text(&run.stderr));
    let stdout = text(&run.stdout);
    // Keys are stored and never shown; a sign-in is named with what to run.
    let keys = sandbox.json("azazel/auth.json");
    assert_eq!(keys["openrouter"]["key"], "test-key-openrouter");
    assert!(keys.get("anthropic").is_none() && keys.get("openai-codex").is_none());
    assert!(
        !stdout.contains("test-key-") && !text(&run.stderr).contains("test-key-"),
        "{stdout}"
    );
    assert!(stdout.contains("run `azazel login anthropic`"), "{stdout}");
    assert!(stdout.contains("run `azazel login openai-codex`"), "{stdout}");
    // The first source to set a role keeps it.
    assert!(
        stdout.contains("roles.default is already [\"team-gateway/big-model:high\"]"),
        "{stdout}"
    );
    assert!(sandbox.path("azazel/skills/changelog/SKILL.md").is_file());
    assert!(sandbox.path("azazel/commands/draftpr.md").is_file());

    // The configuration it wrote is one azazel starts with, and the conversations are sessions.
    let mut list = Command::cargo_bin("azazel").unwrap();
    let listing = list
        .env("AZAZEL_HOME", sandbox.path("azazel"))
        .arg("--cwd")
        .arg(sandbox.path("project"))
        .args(["sessions", "list", "--all-projects"])
        .output()
        .unwrap();
    let listed = text(&listing.stdout);
    assert!(listing.status.success(), "{}", text(&listing.stderr));
    assert!(listed.contains("imported from pi: Notes review"), "{listed}");
    assert!(
        listed.contains("imported from Codex CLI: Fix the parser"),
        "{listed}"
    );

    let again = sandbox.migrate(&["--only", "chats,keys"]);
    let stdout = text(&again.stdout);
    assert!(stdout.starts_with("pi ("), "{stdout}");
    assert!(stdout.contains("\n0 imported, "), "{stdout}");
    assert!(!stdout.contains("  models\n"), "{stdout}");
}

#[test]
fn only_the_sources_and_kinds_named_are_touched() {
    let sandbox = Sandbox::new(&["pi", "codex"]);
    let run = sandbox.migrate(&["--from", "codex", "--only", "commands"]);
    assert!(run.status.success(), "{}", text(&run.stderr));
    let stdout = text(&run.stdout);
    assert!(stdout.starts_with("Codex CLI ("), "{stdout}");
    assert!(!stdout.contains("pi ("), "{stdout}");
    assert!(sandbox.path("azazel/commands/draftpr.md").is_file());
    assert!(!sandbox.path("azazel/config.toml").exists());
    assert!(!sandbox.path("azazel/auth.json").exists());
    assert!(!sandbox.path("azazel/projects").exists());
}

#[test]
fn a_harness_that_is_not_there_and_a_name_that_is_none_are_said() {
    let sandbox = Sandbox::new(&[]);
    let nothing = sandbox.migrate(&[]);
    assert!(nothing.status.success());
    let stdout = text(&nothing.stdout);
    assert!(stdout.starts_with("nothing to migrate. Looked in:"), "{stdout}");
    assert!(
        stdout.contains(&sandbox.path("user").join(".codex").display().to_string()),
        "{stdout}"
    );

    // Asked for by name, a harness that is missing is a failure.
    let missing = sandbox.migrate(&["--from", "jcode"]);
    assert_eq!(missing.status.code(), Some(1));

    let unknown = sandbox.migrate(&["--from", "cursor"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(text(&unknown.stderr).contains("unknown source `cursor`"));
    let unknown = sandbox.migrate(&["--only", "sessions"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(text(&unknown.stderr).contains("unknown kind `sessions`"));
}
