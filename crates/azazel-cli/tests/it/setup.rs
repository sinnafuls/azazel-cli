use std::path::PathBuf;
use std::process::Output;

use assert_cmd::Command;
use azazel_ai::providers::presets::presets;

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        Sandbox { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.dir.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// `azazel` pointed at this sandbox, with no ambient configuration or credentials.
    fn azazel(&self) -> Command {
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.home())
            .env_remove("AZAZEL_MOCK_SCRIPT")
            .arg("--cwd")
            .arg(self.project());
        for provider in presets() {
            for variable in provider.api_key_env {
                command.env_remove(variable);
            }
        }
        command
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn doctor_fails_when_nothing_can_run_a_model() {
    let sandbox = Sandbox::new();
    let output = sandbox.azazel().arg("doctor").output().unwrap();
    let report = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert!(report.contains("FAIL  role default: not set"), "{report}");
    assert!(
        report.contains("ok    config: no files, defaults in use"),
        "{report}"
    );
    assert!(report.contains("warn  providers: no API key found"), "{report}");
    assert!(
        report.contains("ok    trust: nothing in this project needs it"),
        "{report}"
    );
}

#[test]
fn doctor_lists_what_an_untrusted_project_asked_for_and_what_was_skipped() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "home/config.toml",
        "[roles]\ndefault = [\"anthropic/some-model\"]\n",
    );
    sandbox.write("home/skills/broken/SKILL.md", "no frontmatter here\n");
    sandbox.write("project/.mcp.json", "{\"mcpServers\": {}}\n");
    sandbox.write(
        "project/.azazel/config.toml",
        "[[hooks.session_start]]\ncommand = \"echo hooked\"\n",
    );

    let output = sandbox
        .azazel()
        .env("ANTHROPIC_API_KEY", "k")
        .arg("doctor")
        .output()
        .unwrap();
    let report = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert!(
        report.contains("ok    provider anthropic: credential from env ANTHROPIC_API_KEY"),
        "{report}"
    );
    assert!(
        report.contains("ok    role default: anthropic/some-model"),
        "{report}"
    );
    assert!(
        report.contains("ok    role plan: anthropic/some-model"),
        "{report}"
    );
    assert!(
        report.contains("warn  trust: not decided; the project has MCP servers in .mcp.json"),
        "{report}"
    );
    assert!(report.contains("`hooks` in .azazel/config.toml"), "{report}");
    assert!(
        report.contains("warn  config: skipped because the project is not trusted:"),
        "{report}"
    );
    assert!(report.contains("warn  skill skipped:"), "{report}");

    let output = sandbox
        .azazel()
        .env("ANTHROPIC_API_KEY", "k")
        .args(["--trust", "doctor"])
        .output()
        .unwrap();
    let report = stdout(&output);
    assert!(
        report.contains("ok    trust: this project is trusted"),
        "{report}"
    );
    assert!(
        !report.contains("skipped because the project is not trusted"),
        "{report}"
    );
}

#[test]
fn init_keeps_an_existing_default_model_and_writes_the_project_config_once() {
    let sandbox = Sandbox::new();
    sandbox.write("home/config.toml", "[roles]\ndefault = [\"mock/a\"]\n");
    let output = sandbox.azazel().arg("init").output().unwrap();
    let said = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{said}");
    assert!(said.contains("already sets roles.default"), "{said}");
    let config = sandbox.project().join(".azazel/config.toml");
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "[verify]\ncommands = []\n"
    );
    assert_eq!(
        std::fs::read_to_string(sandbox.home().join("config.toml")).unwrap(),
        "[roles]\ndefault = [\"mock/a\"]\n"
    );

    std::fs::write(&config, "[verify]\ncommands = [\"make test\"]\n").unwrap();
    assert!(sandbox.azazel().arg("init").output().unwrap().status.success());
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "[verify]\ncommands = [\"make test\"]\n"
    );
}

#[test]
fn the_interactive_ui_refuses_to_start_without_a_terminal() {
    let sandbox = Sandbox::new();
    let output = sandbox.azazel().output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("pass -p"));
}
