//! `azazel roles` and `azazel models` in a sandbox home, and the thinking level a role's
//! `:level` gives a request against one chosen for the run.

use std::path::PathBuf;
use std::process::Output;

use crate::common::{stderr, stdout};
use assert_cmd::Command;
use serde_json::{Value, json};

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new(home_config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.azazel")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::write(dir.path().join("home/config.toml"), home_config).unwrap();
        Sandbox { dir }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn home_config(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("home/config.toml")).unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.dir.path().join("home"))
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("AZAZEL_MOCK_SCRIPT")
            .arg("--cwd")
            .arg(self.project());
        command
    }

    fn azazel(&self, args: &[&str]) -> Output {
        self.command().args(args).write_stdin("").output().unwrap()
    }

    /// `azazel roles --json`, by role.
    fn roles(&self) -> Vec<Value> {
        let output = self.azazel(&["roles", "--json"]);
        assert!(output.status.success(), "{}", stderr(&output));
        lines(&output)
    }

    fn role(&self, role: &str) -> Value {
        self.roles()
            .into_iter()
            .find(|row| row["role"] == role)
            .unwrap_or(Value::Null)
    }
}

fn lines(output: &Output) -> Vec<Value> {
    stdout(output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn a_role_is_set_with_fallbacks_and_a_level_then_cleared() {
    let sandbox = Sandbox::new("# mine\n");
    let output = sandbox.azazel(&[
        "roles",
        "set",
        "smol",
        "anthropic/claude-haiku-4-5",
        "@slow",
        "--thinking",
        "low",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        sandbox
            .home_config()
            .contains("smol = [\"anthropic/claude-haiku-4-5:low\", \"@slow\"]"),
        "{}",
        sandbox.home_config()
    );
    assert!(sandbox.home_config().contains("# mine"));
    let smol = sandbox.role("smol");
    assert_eq!(
        (smol["source"].clone(), smol["thinking"].clone()),
        (json!("home"), json!("low"))
    );

    let output = sandbox.azazel(&["roles", "clear", "smol", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(lines(&output)[0]["source"], "builtin");
    assert!(!sandbox.home_config().contains("smol"));
}

/// The level a role's `:level` gives the lead's requests, and the one `--thinking` chooses.
#[test]
fn a_level_chosen_for_the_run_beats_the_level_of_the_role() {
    let sandbox = Sandbox::new("");
    let script = sandbox.dir.path().join("script.json");
    let requests = sandbox.dir.path().join("requests.jsonl");
    std::fs::write(
        &script,
        json!({ "lead": [{ "reply": { "text": "ok" } }] }).to_string(),
    )
    .unwrap();
    let level = |extra: &[&str]| {
        let _ = std::fs::remove_file(&requests);
        let output = sandbox
            .command()
            .env("AZAZEL_MOCK_SCRIPT", &script)
            .env("AZAZEL_MOCK_REQUESTS", &requests)
            .args(["-p", "--no-memory", "--set", "session.auto_name=false"])
            .args(["--set", "roles.default=[\"mock/lead:high\"]"])
            .args(extra)
            .arg("hi")
            .write_stdin("")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        let first = std::fs::read_to_string(&requests).unwrap();
        let request: Value = serde_json::from_str(first.lines().next().unwrap()).unwrap();
        request["request"]["thinking"].as_str().unwrap().to_string()
    };
    assert_eq!(level(&[]), "high");
    assert_eq!(level(&["--thinking", "low"]), "low");
    assert_eq!(level(&["--thinking", "off"]), "off");
}

/// `--profile` before a subcommand used to be ignored; `--preset` is its old name.
#[test]
fn a_profile_named_before_a_subcommand_is_applied_or_refused() {
    let sandbox = Sandbox::new("[profiles.mine.roles]\nsmol = [\"acme/tiny\"]\n");
    let chain = |args: &[&str], role: &str| {
        let output = sandbox.azazel(args);
        assert!(output.status.success(), "{}", stderr(&output));
        lines(&output)
            .into_iter()
            .find(|row| row["role"] == role)
            .map(|row| row["chain"].clone())
            .unwrap_or(Value::Null)
    };
    assert_eq!(
        chain(&["--profile", "cheap", "roles", "--json"], "default"),
        json!(["anthropic/claude-sonnet-5-5"])
    );
    assert_eq!(
        chain(&["--preset", "mine", "roles", "--json"], "smol"),
        json!(["acme/tiny"])
    );
    // Without it the roles are the configured ones.
    assert_ne!(chain(&["roles", "--json"], "smol"), json!(["acme/tiny"]));

    for flag in ["--profile", "--preset"] {
        let output = sandbox.azazel(&[flag, "nope", "roles"]);
        assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
        let error = stderr(&output);
        assert!(
            error.contains("unknown profile `nope`") && error.contains("claude, gpt, cheap"),
            "{error}"
        );
    }
}

#[test]
fn an_on_limit_naming_no_profile_stops_the_start_and_a_built_in_is_accepted() {
    let sandbox = Sandbox::new("[profiles]\non_limit = \"nope\"\n");
    let output = sandbox.azazel(&["roles"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("profiles.on_limit: unknown profile `nope`"),
        "{}",
        stderr(&output)
    );

    let sandbox = Sandbox::new("[profiles]\non_limit = \"gpt\"\non_reset = \"auto\"\n");
    let output = sandbox.azazel(&["roles"]);
    assert!(output.status.success(), "{}", stderr(&output));
}
