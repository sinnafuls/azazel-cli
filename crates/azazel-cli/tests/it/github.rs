//! GitHub through `gh` (here `fake-gh`): `#<n>` in a prompt attaches the issue or pull request
//! it names, when it resolves.

use std::path::PathBuf;
use std::sync::LazyLock;

use crate::common::fixture_program;
use assert_cmd::Command;
use serde_json::json;

static FIXTURE: LazyLock<PathBuf> = LazyLock::new(|| fixture_program("fake-gh"));

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["project/.git", "home", "bin/gh-data"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        std::fs::copy(
            &*FIXTURE,
            dir.path()
                .join("bin")
                .join(format!("gh{}", std::env::consts::EXE_SUFFIX)),
        )
        .unwrap();
        Sandbox { dir }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn command(&self) -> Command {
        let mut path = vec![self.path("bin")];
        path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.path("home"))
            .env("PATH", std::env::join_paths(path).unwrap())
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .arg("--cwd")
            .arg(self.path("project"));
        command
    }

    /// The text of the first request the scripted lead got for `prompt`.
    fn request_for(&self, prompt: &str) -> String {
        let script = json!({ "lead": [{ "reply": { "text": "ok" } }] });
        std::fs::write(self.path("script.json"), script.to_string()).unwrap();
        let output = self
            .command()
            .env("AZAZEL_MOCK_SCRIPT", self.path("script.json"))
            .env("AZAZEL_MOCK_REQUESTS", self.path("requests.jsonl"))
            .args(["--model", "mock/lead", "--mode", "auto", "--no-memory"])
            .args(["--set", "session.auto_name=false"])
            .args(["-p", prompt])
            .write_stdin("")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        std::fs::read_to_string(self.path("requests.jsonl")).unwrap()
    }
}

#[test]
fn a_number_in_a_prompt_attaches_the_issue_or_pull_request_only_when_it_resolves() {
    let sandbox = Sandbox::new();
    let data = sandbox.path("bin/gh-data");
    std::fs::write(
        data.join("issue-7.json"),
        r#"{"number":7,"title":"Crash on start","state":"OPEN","author":{"login":"ann"},"labels":[],
        "body":"It crashes.","createdAt":"2026-11-02T10:00:00Z","comments":[]}"#,
    )
    .unwrap();
    std::fs::write(
        data.join("pr-12.json"),
        r#"{"number":12,"title":"Fix start","state":"OPEN","author":{"login":"bob"},"labels":[],
        "body":"Fixes it.","createdAt":"2026-11-03T10:00:00Z","headRefName":"fix","baseRefName":"main",
        "statusCheckRollup":[],"files":[],"comments":[]}"#,
    )
    .unwrap();
    let request = sandbox.request_for("look at #7 and #12, not #99 or C#1x");
    assert!(
        request.contains("issue://7") && request.contains("Crash on start"),
        "{request}"
    );
    assert!(
        request.contains("pr://12") && request.contains("Fix start"),
        "{request}"
    );
    assert!(
        !request.contains("://99") && !request.contains("://1x"),
        "{request}"
    );
    assert!(request.contains("not #99 or C#1x"), "the prompt stays as written");
}
