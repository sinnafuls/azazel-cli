//! Primary agents from the outside: `--agent` and `[agents] default`, what the built-in `plan`
//! may write, the limits of a project's own primary agent on the lead, and the switch kept in the
//! session.

use std::path::PathBuf;
use std::process::Output;

use assert_cmd::Command;
use serde_json::{Value, json};

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("project/.azazel")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        Sandbox { dir }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn write(&self, relative: &str, text: &str) {
        let file = self.path(relative);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }

    /// A print-mode run answering from `script`, with every request recorded.
    fn run(&self, script: &Value, args: &[&str]) -> Output {
        self.write("script.json", &script.to_string());
        let _ = std::fs::remove_file(self.path("requests.jsonl"));
        Command::cargo_bin("azazel")
            .unwrap()
            .env("AZAZEL_HOME", self.path("home"))
            .env("HOME", self.path("user"))
            .env("USERPROFILE", self.path("user"))
            .env("AZAZEL_MOCK_SCRIPT", self.path("script.json"))
            .env("AZAZEL_MOCK_REQUESTS", self.path("requests.jsonl"))
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .arg("--cwd")
            .arg(self.path("project"))
            .args([
                "--no-memory",
                "--set",
                "session.auto_name=false",
                "--set",
                "guards.done_check=false",
            ])
            .args(args)
            .write_stdin("")
            .output()
            .unwrap()
    }

    /// The lead's requests, as the provider was sent them.
    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
            .filter(|request| request["tools"].as_array().is_some_and(|tools| !tools.is_empty()))
            .collect()
    }

    /// The text of the last message of each request: the results of the calls before it.
    fn results(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|request| {
                request["messages"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    /// The text of every session file of the sandbox.
    fn transcripts(&self) -> String {
        let mut text = String::new();
        for project in std::fs::read_dir(self.path("home/projects")).unwrap().flatten() {
            for file in std::fs::read_dir(project.path().join("sessions"))
                .unwrap()
                .flatten()
            {
                if file.path().extension().is_some_and(|e| e == "jsonl") {
                    text.push_str(&std::fs::read_to_string(file.path()).unwrap());
                }
            }
        }
        text
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn text(reply: &str) -> Value {
    json!({ "reply": { "text": reply } })
}

fn call(id: &str, tool: &str, input: Value) -> Value {
    json!({ "reply": { "tool_calls": [{ "id": id, "name": tool, "input": input }] } })
}

/// The identity at the top of a request's system prompt.
fn identity(request: &Value) -> String {
    let first = &request["system"][0];
    assert_eq!(first["name"], "identity", "{request}");
    first["text"].as_str().unwrap().to_string()
}

fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn an_agent_that_is_no_primary_agent_is_refused() {
    let sandbox = Sandbox::new();
    let script = json!({ "lead": [text("hi")] });
    let ghost = sandbox.run(&script, &["-p", "hi", "--agent", "ghost"]);
    assert_eq!(ghost.status.code(), Some(2), "{}", stderr(&ghost));
    assert!(
        stderr(&ghost).contains("--agent: there is no agent `ghost`; the primary agents are: build, plan"),
        "{}",
        stderr(&ghost)
    );
    let scout = sandbox.run(&script, &["-p", "hi", "--agent", "scout"]);
    assert_eq!(scout.status.code(), Some(2));
    assert!(
        stderr(&scout).contains("`scout` is a sub-agent, not a primary agent"),
        "{}",
        stderr(&scout)
    );

    sandbox.write("home/config.toml", "[agents]\ndefault = \"scout\"\n");
    let config = sandbox.run(&script, &["-p", "hi"]);
    assert!(!config.status.success());
    assert!(
        stderr(&config).contains("[agents] default: `scout` is a sub-agent"),
        "{}",
        stderr(&config)
    );
}

#[test]
fn the_plan_agent_writes_only_plans_and_a_resumed_session_is_the_agent_it_was() {
    let sandbox = Sandbox::new();
    let script = json!({ "lead": [
        call("c1", "write", json!({ "path": "src.txt", "content": "code\n" })),
        call("c2", "write", json!({ "path": ".azazel/plans/p.md", "content": "# Plan\n" })),
        text("planned"),
    ] });
    let plan = sandbox.run(&script, &["-p", "plan it", "--agent", "plan", "--mode", "auto"]);
    assert!(plan.status.success(), "{}", stderr(&plan));
    assert!(!sandbox.path("project/src.txt").exists());
    assert_eq!(
        std::fs::read_to_string(sandbox.path("project/.azazel/plans/p.md")).unwrap(),
        "# Plan\n"
    );
    let results = sandbox.results();
    assert!(
        results[1].contains("plan mode: only read-only tools are available"),
        "{}",
        results[1]
    );
    let requests = sandbox.requests();
    assert!(
        identity(&requests[0]).contains("you are planning"),
        "{}",
        identity(&requests[0])
    );
    assert!(
        sandbox
            .transcripts()
            .contains(r#""type":"agent_change","agent":"plan""#)
    );

    // Resumed without the flag, the session is still `plan`.
    let again = sandbox.run(
        &json!({ "lead": [text("still planning")] }),
        &["-p", "go on", "--continue"],
    );
    assert!(again.status.success(), "{}", stderr(&again));
    assert!(identity(&sandbox.requests()[0]).contains("you are planning"));

    // `--agent build` takes it back to the lead it was before, with every tool.
    let script = json!({ "lead": [
        call("c1", "write", json!({ "path": "src.txt", "content": "code\n" })),
        text("built"),
    ] });
    let build = sandbox.run(
        &script,
        &[
            "-p",
            "build it",
            "--continue",
            "--agent",
            "build",
            "--mode",
            "auto",
        ],
    );
    assert!(build.status.success(), "{}", stderr(&build));
    let requests = sandbox.requests();
    assert!(
        identity(&requests[0])
            .starts_with("You are azazel, a coding agent working in the user's terminal. You lead a team")
    );
    assert_eq!(
        std::fs::read_to_string(sandbox.path("project/src.txt")).unwrap(),
        "code\n"
    );
    assert!(
        sandbox
            .transcripts()
            .contains(r#""type":"agent_change","agent":"build""#)
    );
}

#[test]
fn a_primary_agent_of_the_project_is_the_lead_s_identity_and_limits() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "project/.azazel/agents/notes.md",
        "---\nname: notes\ndescription: keeps notes\nmode: primary\ntools: read, grep, write(notes/**)\n\
         color: green\n---\nYou keep notes and nothing else.\n",
    );
    let script = json!({ "lead": [
        call("c1", "write", json!({ "path": "notes/today.md", "content": "a note\n" })),
        call("c2", "write", json!({ "path": "src.txt", "content": "code\n" })),
        text("noted"),
    ] });
    let run = sandbox.run(&script, &["-p", "note it", "--agent", "notes", "--mode", "auto"]);
    assert!(run.status.success(), "{}", stderr(&run));
    let requests = sandbox.requests();
    assert_eq!(identity(&requests[0]), "You keep notes and nothing else.");
    // The rules and the rest of the prompt stay.
    let parts: Vec<&str> = requests[0]["system"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| part["name"].as_str().unwrap())
        .collect();
    assert!(
        parts.contains(&"rules") && parts.contains(&"environment"),
        "{parts:?}"
    );
    let mut offered = tool_names(&requests[0]);
    offered.sort();
    assert_eq!(offered, ["grep", "read", "write"]);
    assert_eq!(
        std::fs::read_to_string(sandbox.path("project/notes/today.md")).unwrap(),
        "a note\n"
    );
    assert!(!sandbox.path("project/src.txt").exists());
    let results = sandbox.results();
    assert!(
        results[2].contains("agent `lead` may use `write` only as: write(notes/**)"),
        "{}",
        results[2]
    );
}
