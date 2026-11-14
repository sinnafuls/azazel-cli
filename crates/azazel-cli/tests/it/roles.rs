//! `azazel roles` and `azazel models` in a sandbox home, and the thinking level a role's
//! `:level` gives a request against one chosen for the run.

use std::path::PathBuf;
use std::process::Output;

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

    fn project_config(&self) -> String {
        std::fs::read_to_string(self.project().join(".azazel/config.toml")).unwrap_or_default()
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn lines(output: &Output) -> Vec<Value> {
    stdout(output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn roles_are_listed_with_their_model_level_and_source() {
    let sandbox = Sandbox::new("[roles]\ndefault = [\"anthropic/claude-fable-5:high\"]\n");
    let default = sandbox.role("default");
    assert_eq!(default["chain"], json!(["anthropic/claude-fable-5:high"]));
    assert_eq!(default["source"], "home");
    assert_eq!(default["thinking"], "high");
    // A role that falls back to `default` answers with its model.
    let smol = sandbox.role("smol");
    assert_eq!(smol["source"], "builtin");
    assert_eq!(smol["resolved"][0], "anthropic/claude-fable-5:high");

    let text = stdout(&sandbox.azazel(&["roles"]));
    let row = text.lines().find(|line| line.starts_with("smol ")).unwrap();
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(
        cells,
        ["smol", "@default", "anthropic/claude-fable-5", "high", "built-in"]
    );
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

#[test]
fn the_project_config_is_written_only_in_a_trusted_project() {
    let sandbox = Sandbox::new("");
    let refused = sandbox.azazel(&["roles", "set", "slow", "anthropic/claude-fable-5", "--project"]);
    assert_eq!(refused.status.code(), Some(2), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("not trusted"), "{}", stderr(&refused));
    assert!(!sandbox.project_config().contains("slow"));

    let trusted = sandbox.azazel(&[
        "--trust",
        "roles",
        "set",
        "slow",
        "anthropic/claude-fable-5",
        "--project",
    ]);
    assert!(trusted.status.success(), "{}", stderr(&trusted));
    assert!(
        sandbox
            .project_config()
            .contains("slow = [\"anthropic/claude-fable-5\"]")
    );
    assert!(!sandbox.home_config().contains("slow"));
}

#[test]
fn a_preset_is_saved_and_loaded_back() {
    let sandbox = Sandbox::new("[roles]\ndefault = [\"anthropic/claude-fable-5\"]\n");
    let saved = sandbox.azazel(&["roles", "preset", "save", "deep", "--json"]);
    assert!(saved.status.success(), "{}", stderr(&saved));
    assert!(
        lines(&saved)[0]["roles"]
            .as_array()
            .unwrap()
            .contains(&json!("default"))
    );

    let output = sandbox.azazel(&["roles", "set", "default", "anthropic/claude-haiku-4-5"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(sandbox.role("default")["chain"][0], "anthropic/claude-haiku-4-5");

    let loaded = sandbox.azazel(&["roles", "preset", "load", "deep", "--json"]);
    assert!(loaded.status.success(), "{}", stderr(&loaded));
    assert!(
        lines(&loaded)
            .iter()
            .any(|row| row["role"] == "default" && row["chain"][0] == "anthropic/claude-fable-5")
    );
    assert_eq!(sandbox.role("default")["chain"][0], "anthropic/claude-fable-5");
}

#[test]
fn mistakes_exit_2_and_a_failed_write_exits_1() {
    let sandbox = Sandbox::new("");
    for args in [
        &["roles", "set", "bad name", "anthropic/claude-fable-5"][..],
        &["roles", "set", "smol", "@smol"],
        &[
            "roles",
            "set",
            "smol",
            "anthropic/claude-fable-5",
            "--thinking",
            "loud",
        ],
        &["roles", "set", "smol", "@default", "--thinking", "high"],
        &["roles", "preset", "load", "nothing"],
        &["roles", "preset", "save", "bad name"],
        &["roles", "set", "smol"],
    ] {
        let output = sandbox.azazel(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {}", stderr(&output));
    }
    assert_eq!(sandbox.home_config(), "");

    // The home config cannot be replaced while it is a read-only file.
    let file = sandbox.dir.path().join("home/config.toml");
    let mut permissions = std::fs::metadata(&file).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&file, permissions.clone()).unwrap();
    let output = sandbox.azazel(&["roles", "set", "smol", "anthropic/claude-fable-5"]);
    #[expect(
        clippy::permissions_set_readonly_false,
        reason = "the temporary directory is removed"
    )]
    permissions.set_readonly(false);
    std::fs::set_permissions(&file, permissions).unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
}

#[test]
fn models_are_listed_per_provider_with_the_roles_that_use_them() {
    let sandbox = Sandbox::new("[roles]\ndefault = [\"anthropic/claude-fable-5:high\"]\n");
    let text = sandbox.azazel(&["models", "--all", "--provider", "anthropic"]);
    assert!(text.status.success(), "{}", stderr(&text));
    let text = stdout(&text);
    let mut rows = text.lines();
    assert_eq!(rows.next(), Some("anthropic · no credential"));
    let head: Vec<&str> = rows.next().unwrap().split_whitespace().collect();
    assert_eq!(
        head,
        [
            "model", "context", "out", "price", "thinking", "vision", "used", "by"
        ]
    );
    let fable = text
        .lines()
        .find(|line| line.trim_start().starts_with("anthropic/claude-fable-5 "))
        .unwrap();
    // The last column, after the widest gap: the roles that answer with it, `default` first.
    let used_by = fable.trim_end().rsplit("  ").next().unwrap();
    assert!(used_by.starts_with("default, "), "{fable}");

    let json = lines(&sandbox.azazel(&["models", "--all", "--provider", "anthropic", "--json"]));
    let fable = json
        .iter()
        .find(|row| row["model"] == "anthropic/claude-fable-5")
        .unwrap();
    assert_eq!(fable["roles"][0], "default");
    assert_eq!(fable["login"], false);
    assert_eq!(fable["usable"], false);
    assert!(
        fable["name"].is_string() && fable["thinking_levels"].is_array(),
        "{fable}"
    );
    assert!(
        json.iter()
            .all(|row| row["model"] == fable["model"] || row["roles"] == json!([]))
    );

    // Without --all, a provider with no credential is not listed.
    let none = sandbox.azazel(&["models", "--provider", "anthropic", "--json"]);
    assert!(lines(&none).is_empty());
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
