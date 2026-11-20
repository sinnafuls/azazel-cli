//! Orchestration: typed results in print mode (`--json-schema`), coordinator mode and the advisor.

use std::path::PathBuf;
use std::process::Output;

use crate::common::{call, stderr, stdout, system, text, tool_names};
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

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn requests_file(&self) -> PathBuf {
        self.dir.path().join("requests.jsonl")
    }

    /// `azazel -p` in this sandbox answering from `script`, with every request recorded. The
    /// session is not named and nothing is remembered from it, so the only requests are the
    /// lead's and the advisor's.
    fn print(&self, script: &Value, args: &[&str]) -> Output {
        let script_path = self.dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let _ = std::fs::remove_file(self.requests_file());
        Command::cargo_bin("azazel")
            .unwrap()
            .env("AZAZEL_HOME", self.dir.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", &script_path)
            .env("AZAZEL_MOCK_REQUESTS", self.requests_file())
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .arg("--cwd")
            .arg(self.project())
            .args(["-p", "--no-memory", "--set", "session.auto_name=false"])
            .args(args)
            .write_stdin("")
            .output()
            .unwrap()
    }

    /// Every request the scripted provider received, as `(model, request)`.
    fn requests(&self) -> Vec<(String, Value)> {
        std::fs::read_to_string(self.requests_file())
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let recorded: Value = serde_json::from_str(line).unwrap();
                (
                    recorded["model"].as_str().unwrap().to_string(),
                    recorded["request"].clone(),
                )
            })
            .collect()
    }

    /// The text of the one session file of this sandbox.
    fn session(&self) -> String {
        let projects = self.dir.path().join("home/projects");
        let mut files = Vec::new();
        for project in std::fs::read_dir(projects).unwrap().flatten() {
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
        assert_eq!(files.len(), 1, "{files:?}");
        std::fs::read_to_string(&files[0]).unwrap()
    }
}

/// The text of every message of the user's side, in order.
fn user_texts(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| {
            message["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .collect()
}

const ANSWER_SCHEMA: &str = r#"{"type":"object","properties":{"answer":{"type":"integer"}},"required":["answer"],"additionalProperties":false}"#;

#[test]
fn a_typed_run_prints_only_the_reported_json() {
    let sandbox = Sandbox::new();
    // The first report does not match; the lead corrects it.
    let script = json!({ "lead": [
        call("r1", "report", json!({ "result": { "answer": "forty-two" } })),
        call("r2", "report", json!({ "result": { "answer": 42 } })),
        text("The answer is 42."),
    ] });
    let output = sandbox.print(&script, &["--json-schema", ANSWER_SCHEMA, "what is the answer?"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "{\"answer\":42}\n");

    let requests = sandbox.requests();
    assert_eq!(requests.len(), 3);
    let first = &requests[0].1;
    assert!(tool_names(first).contains(&"report".to_string()));
    assert!(system(first).contains("finish by calling `report`"));
    let refused = requests[1].1["messages"].to_string();
    assert!(
        refused.contains("the result does not match the schema"),
        "{refused}"
    );

    // The schema may be a file, and a run without the flag has no `report` tool.
    std::fs::write(sandbox.project().join("schema.json"), ANSWER_SCHEMA).unwrap();
    let script = json!({ "lead": [call("r1", "report", json!({ "result": { "answer": 7 } })), text("7")] });
    let output = sandbox.print(&script, &["--json-schema", "schema.json", "and now?"]);
    assert_eq!(stdout(&output), "{\"answer\":7}\n", "{}", stderr(&output));
    let output = sandbox.print(&json!({ "lead": [text("plain")] }), &["hello"]);
    assert_eq!(stdout(&output), "plain\n");
    assert!(!tool_names(&sandbox.requests()[0].1).contains(&"report".to_string()));
}

#[test]
fn a_typed_run_that_never_reports_fails_and_a_bad_schema_is_a_usage_error() {
    let sandbox = Sandbox::new();
    // The lead is reminded three times and still only talks.
    let script = json!({ "lead": [text("42"), text("42"), text("42"), text("It is 42.")] });
    let output = sandbox.print(&script, &["--json-schema", ANSWER_SCHEMA, "what is the answer?"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("the run ended without a result that matches --json-schema"),
        "{}",
        stderr(&output)
    );
    let requests = sandbox.requests();
    assert_eq!(requests.len(), 4);
    let reminders = user_texts(&requests[3].1);
    assert_eq!(reminders.len(), 4, "{reminders:?}");
    assert!(reminders[3].ends_with("(Reminder 3 of 3.)"), "{reminders:?}");

    for (schema, why) in [
        (r#"{"type":"nonsense"}"#, "--json-schema: "),
        ("{not json", "--json-schema: the schema is not valid JSON"),
        ("missing.json", "--json-schema missing.json: "),
    ] {
        let output = sandbox.print(
            &json!({ "lead": [text("unused")] }),
            &["--json-schema", schema, "x"],
        );
        assert_eq!(output.status.code(), Some(2), "{schema}: {}", stderr(&output));
        assert!(stderr(&output).contains(why), "{schema}: {}", stderr(&output));
        assert!(sandbox.requests().is_empty());
    }
}

#[test]
fn a_coordinating_lead_has_no_tool_that_edits_or_runs_commands_and_stays_so_after_resume() {
    let sandbox = Sandbox::new();
    let output = sandbox.print(
        &json!({ "lead": [text("planned")] }),
        &["--coordinator", "plan it"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let request = sandbox.requests().remove(0).1;
    let tools = tool_names(&request);
    for withheld in ["write", "edit", "shell", "job", "pty", "eval"] {
        assert!(!tools.contains(&withheld.to_string()), "{withheld} in {tools:?}");
    }
    for kept in ["task", "agent_wait", "read", "glob", "grep", "todo", "ask_user"] {
        assert!(tools.contains(&kept.to_string()), "{kept} not in {tools:?}");
    }
    assert!(system(&request).contains("# Coordinator mode"));

    // Resumed without the flag, the session is still a coordinator's.
    let output = sandbox.print(&json!({ "lead": [text("planned more")] }), &["-c", "go on"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let request = sandbox.requests().remove(0).1;
    let tools = tool_names(&request);
    for withheld in ["write", "edit", "shell"] {
        assert!(!tools.contains(&withheld.to_string()), "{withheld} in {tools:?}");
    }
    assert!(system(&request).contains("# Coordinator mode"));
    // The mode was written down once, when the session began.
    assert_eq!(sandbox.session().matches("coordinator:on").count(), 1);

    // A session of its own, without the mode, has every tool and no coordinator prompt.
    let plain = Sandbox::new();
    let output = plain.print(&json!({ "lead": [text("done")] }), &["do it"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let request = plain.requests().remove(0).1;
    let tools = tool_names(&request);
    assert!(tools.contains(&"write".to_string()) && tools.contains(&"shell".to_string()));
    assert!(!system(&request).contains("# Coordinator mode"));
    assert!(!plain.session().contains("coordinator:"));

    // The setting does what the flag does.
    let configured = Sandbox::new();
    let output = configured.print(
        &json!({ "lead": [text("planned")] }),
        &["--set", "team.coordinator=true", "plan it"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!tool_names(&configured.requests()[0].1).contains(&"write".to_string()));
}

const ADVISOR: [&str; 4] = [
    "--set",
    "advisor.when=\"stop\"",
    "--set",
    "advisor.model=[\"mock/advisor\"]",
];

#[test]
fn advice_from_the_advisor_gives_the_lead_one_more_round() {
    let sandbox = Sandbox::new();
    let script = json!({
        "lead": [
            call("w1", "write", json!({ "path": "note.txt", "content": "hello\n" })),
            text("I wrote the note."),
            text("I checked it again; the note is right."),
        ],
        "advisor": [text("note.txt: the request was for a greeting in French.")],
    });
    let mut args = ADVISOR.to_vec();
    args.extend(["--mode", "auto", "write a note"]);
    let output = sandbox.print(&script, &args);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "I checked it again; the note is right.\n");

    let requests = sandbox.requests();
    let models: Vec<&str> = requests.iter().map(|(model, _)| model.as_str()).collect();
    assert_eq!(models, ["lead", "lead", "advisor", "lead"]);

    // The advisor saw the prompt, the lead's last message and what changed, and nothing else.
    let asked = &requests[2].1;
    assert_eq!(asked["system"], json!([]));
    assert_eq!(asked["tools"], json!([]));
    let brief = user_texts(asked).join("");
    assert!(brief.contains("<prompt>\nwrite a note\n</prompt>"), "{brief}");
    assert!(
        brief.contains("<final-message>\nI wrote the note.\n</final-message>"),
        "{brief}"
    );
    assert!(brief.contains("note.txt") && brief.contains("+hello"), "{brief}");

    // The lead's extra round begins with the advice, and the transcript keeps it.
    let advice = "<advisor>\nnote.txt: the request was for a greeting in French.\n</advisor>";
    let last = user_texts(&requests[3].1).pop().unwrap();
    assert!(last.starts_with(advice), "{last}");
    assert!(last.contains("A second model reviewed your work"), "{last}");
    assert_eq!(sandbox.session().matches("<advisor>").count(), 1);
}

#[test]
fn an_advisor_that_says_ok_adds_no_round_and_a_silent_one_does_not_hold_the_run_up() {
    let sandbox = Sandbox::new();
    // One reply of the lead only: another round would find the script used up and fail.
    let script = json!({ "lead": [text("All done.")], "advisor": [text("OK")] });
    let mut args = ADVISOR.to_vec();
    args.push("do it");
    let output = sandbox.print(&script, &args);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "All done.\n");
    let models: Vec<String> = sandbox.requests().into_iter().map(|(model, _)| model).collect();
    assert_eq!(models, ["lead", "advisor"]);
    assert!(!sandbox.session().contains("<advisor>"));

    // An advisor that cannot be reached is skipped; with `when = "off"` it is never asked.
    let unreachable = Sandbox::new();
    let output = unreachable.print(&json!({ "lead": [text("All done.")] }), &args);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "All done.\n");

    let off = Sandbox::new();
    let output = off.print(&script, &["--set", "advisor.model=[\"mock/advisor\"]", "do it"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let models: Vec<String> = off.requests().into_iter().map(|(model, _)| model).collect();
    assert_eq!(models, ["lead"]);
}

#[test]
fn a_coordinator_delegates_to_typed_scouts_and_reports_what_they_found() {
    let sandbox = Sandbox::new();
    let files = json!({
        "type": "object",
        "properties": { "files": { "type": "array", "items": { "type": "string" } } },
        "required": ["files"]
    });
    let scout = |n: usize, dir: &str| {
        json!({
            "agent": "scout",
            "prompt": format!("list the files under {dir}"),
            "model": format!("mock/s{n}"),
            "schema": files,
        })
    };
    let found = |file: &str| {
        json!([
            call("r", "report", json!({ "result": { "files": [file] } })),
            text("reported"),
        ])
    };
    let script = json!({
        "lead": [
            call("t1", "task", json!({ "agents": [scout(1, "src"), scout(2, "tests")] })),
            call("r1", "report", json!({ "result": { "files": ["src/lib.rs", "tests/it.rs"] } })),
            text("Both listed."),
        ],
        "s1": found("src/lib.rs"),
        "s2": found("tests/it.rs"),
    });
    let output = sandbox.print(
        &script,
        &[
            "--coordinator",
            "--json-schema",
            &files.to_string(),
            "list the files",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "{\"files\":[\"src/lib.rs\",\"tests/it.rs\"]}\n");

    let requests = sandbox.requests();
    let lead: Vec<&Value> = requests
        .iter()
        .filter(|(model, _)| model == "lead")
        .map(|(_, request)| request)
        .collect();
    let tools = tool_names(lead[0]);
    for withheld in ["write", "edit", "shell"] {
        assert!(!tools.contains(&withheld.to_string()), "{withheld} in {tools:?}");
    }
    assert!(tools.contains(&"task".to_string()) && tools.contains(&"report".to_string()));
    // Both scouts' values came back to the lead as JSON.
    let results = lead[1]["messages"].to_string();
    for file in ["src/lib.rs", "tests/it.rs"] {
        assert!(results.contains(&format!("\\\"{file}\\\"")), "{results}");
    }
    assert!(results.contains("## scout-1 (done, ") && results.contains("## scout-2 (done, "));
    // The scouts keep their own tools: the mode limits the lead only.
    let scout = &requests.iter().find(|(model, _)| model == "s1").unwrap().1;
    assert!(tool_names(scout).contains(&"report".to_string()));
}
