//! Side questions: asked while the lead works, answered from the lead's prompt and tools, and
//! written to no session.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::common::json_lines;
use assert_cmd::Command;
use serde_json::{Value, json};

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        Sandbox { dir }
    }

    fn requests_file(&self) -> PathBuf {
        self.dir.path().join("requests.jsonl")
    }

    fn azazel(&self, script: &Value) -> Command {
        let script_path = self.dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.dir.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", &script_path)
            .env("AZAZEL_MOCK_REQUESTS", self.requests_file())
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .arg("--cwd")
            .arg(self.dir.path().join("project"))
            .write_stdin("");
        command
    }

    fn requests(&self) -> Vec<Value> {
        json_lines(&std::fs::read(self.requests_file()).unwrap())
            .into_iter()
            .map(|line| line["request"].clone())
            .collect()
    }

    fn session_file(&self) -> PathBuf {
        let projects = self.dir.path().join("home/projects");
        let mut files = Vec::new();
        for project in std::fs::read_dir(projects).unwrap().flatten() {
            for file in std::fs::read_dir(project.path().join("sessions"))
                .unwrap()
                .flatten()
            {
                if file.path().extension().is_some_and(|e| e == "jsonl") {
                    files.push(file.path());
                }
            }
        }
        assert_eq!(files.len(), 1, "{files:?}");
        files.remove(0)
    }
}

fn texts(message: &Value) -> String {
    message["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("")
}

#[test]
fn a_side_question_is_answered_while_the_lead_works_and_leaves_no_trace() {
    let sandbox = Sandbox::new();
    // The lead's turn takes two seconds; the side question is answered from the same queue at once,
    // and its attempt to use a tool is dropped.
    let script = json!({ "lead": [
        { "delay_ms": 2000, "reply": { "text": "the migration is written" } },
        { "reply": {
            "text": "Two tables change.",
            "tool_calls": [{ "id": "c1", "name": "read", "input": { "path": "schema.sql" } }],
            "usage": { "input": 40, "output": 5 }
        } }
    ] });
    let started = Instant::now();
    let output = sandbox
        .azazel(&script)
        .env("AZAZEL_TEST_BTW", "how many tables change?")
        .args(["-p", "write the migration", "--output", "json", "--mode", "auto"])
        // Neither the namer nor the memory extractor adds a request of its own.
        .args(["--set", "session.auto_name=false", "--no-memory"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(started.elapsed() >= Duration::from_secs(2));

    // The answer is there before the lead's reply is.
    let lines = json_lines(&output.stdout);
    let answered = lines.iter().position(|line| line["type"] == "btw").unwrap();
    let replied = lines
        .iter()
        .position(|line| line["type"] == "message_end")
        .unwrap();
    assert!(answered < replied, "{answered} >= {replied}");
    let answer = &lines[answered];
    assert_eq!(answer["answer"], "Two tables change.");
    assert_eq!(answer["streamed"], "Two tables change.");
    assert_eq!(
        answer["note"],
        "the model tried to use read; ask in the main conversation"
    );

    // It was asked with the lead's system prompt and tools, after the conversation so far.
    let requests = sandbox.requests();
    assert_eq!(requests.len(), 2);
    let (lead, side) = (&requests[0], &requests[1]);
    assert!(!lead["system"].as_array().unwrap().is_empty());
    assert!(!lead["tools"].as_array().unwrap().is_empty());
    assert_eq!(side["system"], lead["system"]);
    assert_eq!(side["tools"], lead["tools"]);
    let messages = side["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(texts(&messages[0]), "write the migration");
    let question = texts(&messages[1]);
    assert!(question.starts_with("<side-question>"), "{question}");
    assert!(question.ends_with("how many tables change?"), "{question}");

    // The session holds the prompt and the reply, and nothing of the side question.
    let session = std::fs::read_to_string(sandbox.session_file()).unwrap();
    assert!(!session.contains("tables change"), "{session}");
    assert!(!session.contains("side-question"), "{session}");
    let entries = json_lines(session.as_bytes());
    let said: Vec<String> = entries
        .iter()
        .filter(|entry| entry["type"] == "message")
        .map(|entry| texts(&entry["message"]))
        .collect();
    assert_eq!(said, ["write the migration", "the migration is written"]);
}

#[test]
fn in_coordinator_mode_a_side_question_carries_the_tools_the_lead_is_offered() {
    let sandbox = Sandbox::new();
    let script = json!({ "lead": [
        { "delay_ms": 1500, "reply": { "text": "planned" } },
        { "reply": { "text": "Nothing yet." } }
    ] });
    let output = sandbox
        .azazel(&script)
        .env("AZAZEL_TEST_BTW", "what is done?")
        .args(["-p", "plan the work", "--output", "json", "--coordinator"])
        .args(["--set", "session.auto_name=false", "--no-memory"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = sandbox.requests();
    assert_eq!(requests.len(), 2);
    let names = |request: &Value| -> Vec<String> {
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect()
    };
    let lead = names(&requests[0]);
    assert!(lead.contains(&"task".to_string()), "{lead:?}");
    assert!(!lead.contains(&"write".to_string()), "{lead:?}");
    // Same tools as the lead's request, so the side request reuses its cached prefix.
    assert_eq!(names(&requests[1]), lead);
}
