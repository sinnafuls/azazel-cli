#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/scripts")
        .join(name)
}

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project")).unwrap();
        Sandbox { dir }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    /// `azazel` pointed at this sandbox, with no ambient configuration or credentials.
    fn azazel(&self) -> Command {
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.dir.path().join("home"))
            .env_remove("AZAZEL_MOCK_SCRIPT")
            .arg("--cwd")
            .arg(self.project());
        command
    }

    fn script(&self, json: &str) -> PathBuf {
        let path = self.dir.path().join("script.json");
        std::fs::write(&path, json).unwrap();
        path
    }

    fn session_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let projects = self.dir.path().join("home/projects");
        for project in std::fs::read_dir(projects).into_iter().flatten().flatten() {
            for file in std::fs::read_dir(project.path().join("sessions"))
                .into_iter()
                .flatten()
                .flatten()
            {
                if file.path().extension().is_some_and(|e| e == "jsonl") {
                    files.push(file.path());
                }
            }
        }
        files
    }
}

fn json_lines(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn json_output_streams_events_from_agent_start_to_idle() {
    let sandbox = Sandbox::new();
    let output = sandbox
        .azazel()
        .env("AZAZEL_MOCK_SCRIPT", fixture("write_read.json"))
        .args(["-p", "go", "--output", "json", "--mode", "auto"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = json_lines(&output.stdout);
    assert_eq!(events.first().unwrap()["type"], "agent_start");
    assert_eq!(events.last().unwrap()["type"], "idle");
    assert_eq!(events.last().unwrap()["stop"], "stop");
    assert!(events.iter().all(|e| e["agent"] == "lead"));
    let tools: Vec<&str> = events
        .iter()
        .filter(|e| e["type"] == "tool_start")
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["write", "read"]);
    let read_result = events.iter().rfind(|e| e["type"] == "tool_end").unwrap();
    assert_eq!(
        read_result["output"]["content"][0]["text"],
        "1\thello from azazel"
    );

    assert_eq!(
        std::fs::read_to_string(sandbox.project().join("hello.txt")).unwrap(),
        "hello from azazel\n"
    );
    let sessions = sandbox.session_files();
    assert_eq!(sessions.len(), 1);
    let entries = json_lines(std::fs::read(&sessions[0]).unwrap().as_slice());
    let kinds: Vec<&str> = entries.iter().map(|e| e["type"].as_str().unwrap()).collect();
    // The tree is checkpointed before the prompt; then user, (assistant, tool) twice, assistant,
    // with the journal of the run between them: each request and each call as it starts.
    assert_eq!(
        kinds,
        [
            "session",
            "checkpoint",
            "message",
            "run_started",
            "request_started",
            "message",
            "tool_started",
            "message",
            "request_started",
            "message",
            "tool_started",
            "message",
            "request_started",
            "message",
            "run_ended"
        ]
    );
    assert_eq!(entries[1]["commit"].as_str().unwrap().len(), 40);
}

#[test]
fn text_output_is_the_final_answer_and_piped_stdin_joins_the_prompt() {
    let sandbox = Sandbox::new();
    let output = sandbox
        .azazel()
        .env(
            "AZAZEL_MOCK_SCRIPT",
            sandbox.script(r#"{"lead": [{"reply": {"text": "All done."}}]}"#),
        )
        .args(["-p", "summarise this"])
        .write_stdin("piped context")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim_end(), "All done.");

    let entries = json_lines(std::fs::read(&sandbox.session_files()[0]).unwrap().as_slice());
    let prompt = entries.iter().find(|e| e["type"] == "message").unwrap();
    assert_eq!(
        prompt["message"]["content"][0]["text"],
        "summarise this\n\npiped context"
    );
}

#[test]
fn continue_reuses_the_latest_session() {
    let sandbox = Sandbox::new();
    let first = sandbox.script(r#"{"lead": [{"reply": {"text": "first answer"}}]}"#);
    sandbox
        .azazel()
        .env("AZAZEL_MOCK_SCRIPT", &first)
        .args(["-p", "one"])
        .assert()
        .success();
    let second = sandbox.script(r#"{"lead": [{"reply": {"text": "second answer"}}]}"#);
    sandbox
        .azazel()
        .env("AZAZEL_MOCK_SCRIPT", &second)
        .args(["-p", "two", "-c"])
        .assert()
        .success();

    let sessions = sandbox.session_files();
    assert_eq!(
        sessions.len(),
        1,
        "the second run must append to the first session"
    );
    let transcript = std::fs::read_to_string(&sessions[0]).unwrap();
    for text in ["one", "first answer", "two", "second answer"] {
        assert!(transcript.contains(text), "missing `{text}`");
    }
}

#[test]
fn failures_exit_non_zero_with_a_message() {
    let sandbox = Sandbox::new();
    let no_model = sandbox.azazel().args(["-p", "hi"]).output().unwrap();
    assert_eq!(no_model.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&no_model.stderr)
            .contains("no model configured: run \"azazel init\" or pass --model provider/id")
    );

    let exhausted = sandbox
        .azazel()
        .env("AZAZEL_MOCK_SCRIPT", sandbox.script(r#"{"lead": []}"#))
        .args(["-p", "hi"])
        .output()
        .unwrap();
    assert_eq!(exhausted.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&exhausted.stderr).contains("mock script exhausted for lead"));

    let bad_mode = sandbox
        .azazel()
        .args(["-p", "hi", "--mode", "yolo"])
        .output()
        .unwrap();
    assert_eq!(bad_mode.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad_mode.stderr).contains("unknown mode `yolo`"));
}
