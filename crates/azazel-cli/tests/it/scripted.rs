//! Scripted orchestration through the binary: `eval` scripts in Luau that start agents, run calls
//! at the same time, keep values in the session and ask the `judge` role; and virtual models
//! an extension registers.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Output, Stdio};

use crate::common::{stderr, stdout, text};
use assert_cmd::Command;
use serde_json::{Value, json};

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("home/extensions")).unwrap();
        Sandbox { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn requests_file(&self) -> PathBuf {
        self.dir.path().join("requests.jsonl")
    }

    fn script(&self, script: &Value) -> PathBuf {
        let path = self.dir.path().join("script.json");
        std::fs::write(&path, script.to_string()).unwrap();
        path
    }

    /// `azazel -p` in this sandbox answering from `script`, with every request recorded.
    fn print(&self, script: &Value, args: &[&str]) -> Output {
        let _ = std::fs::remove_file(self.requests_file());
        Command::cargo_bin("azazel")
            .unwrap()
            .env("AZAZEL_HOME", self.home())
            .env("AZAZEL_MOCK_SCRIPT", self.script(script))
            .env("AZAZEL_MOCK_REQUESTS", self.requests_file())
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .arg("--cwd")
            .arg(self.dir.path().join("project"))
            .args(["-p", "--no-memory", "--set", "session.auto_name=false"])
            .args(args)
            .write_stdin("")
            .output()
            .unwrap()
    }

    /// The models the scripted provider was asked, in order, each with the text of the last
    /// message it was sent.
    fn asked(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(self.requests_file())
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let recorded: Value = serde_json::from_str(line).unwrap();
                let last = recorded["request"]["messages"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()
                    .clone();
                let text = last["content"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                (recorded["model"].as_str().unwrap().to_string(), text)
            })
            .collect()
    }

    /// The entries of the one session file of this sandbox.
    fn session(&self) -> Vec<Value> {
        let projects = self.home().join("projects");
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
        std::fs::read_to_string(&files[0])
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn extension(&self, name: &str, source: &str) {
        std::fs::write(self.home().join("extensions").join(name), source).unwrap();
    }
}

fn run_code(id: &str, code: &str) -> Value {
    let input = json!({ "language": "luau", "code": code });
    json!({ "reply": { "tool_calls": [{ "id": id, "name": "eval", "input": input }] } })
}

/// What the tool call `id` returned, from the events of `--output json`.
fn result_of(output: &Output, id: &str) -> String {
    let events: Vec<Value> = stdout(output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let end = events
        .iter()
        .find(|event| event["type"] == "tool_end" && event["call_id"] == id)
        .unwrap_or_else(|| panic!("no result of {id}: {}\n{}", stdout(output), stderr(output)));
    end["output"]["content"][0]["text"].as_str().unwrap().to_string()
}

#[test]
fn a_script_starts_agents_side_by_side_and_what_it_stores_is_there_when_the_session_goes_on() {
    let sandbox = Sandbox::new();
    let orchestrate = r#"
        local got = parallel({
            function() return agents.run("scout", "count the TODOs in alpha.rs", { model = "mock/a" }) end,
            function() return agents.run("scout", "count the TODOs in beta.rs", { model = "mock/b" }) end,
        })
        store("findings", { count = #got, reports = got })
        return table.concat(got, "\n")
    "#;
    let script = json!({
        "lead": [run_code("c1", orchestrate), text("Both counted.")],
        "a": [text("alpha.rs has 3 TODOs")],
        "b": [text("beta.rs has 1 TODO")],
    });
    let output = sandbox.print(&script, &["--output", "json", "--mode", "auto", "count them"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        result_of(&output, "c1"),
        "alpha.rs has 3 TODOs\nbeta.rs has 1 TODO"
    );
    // Each agent got its own prompt on the model the script named.
    let asked = sandbox.asked();
    for (model, prompt) in [
        ("a", "count the TODOs in alpha.rs"),
        ("b", "count the TODOs in beta.rs"),
    ] {
        assert!(
            asked.iter().any(|(m, text)| m == model && text == prompt),
            "{asked:?}"
        );
    }
    // The value is an entry of the session.
    let stored: Vec<Value> = sandbox
        .session()
        .into_iter()
        .filter(|entry| entry["type"] == "custom" && entry["customType"] == "run_code_store")
        .map(|entry| entry["data"].clone())
        .collect();
    assert_eq!(
        stored,
        [json!({
            "key": "findings",
            "value": { "count": 2, "reports": ["alpha.rs has 3 TODOs", "beta.rs has 1 TODO"] }
        })]
    );

    // A later run of the same session reads it back.
    let later = json!({
        "lead": [
            run_code("c2", r#"local f = load("findings") return f.count .. ": " .. f.reports[2]"#),
            text("Still there."),
        ],
    });
    let output = sandbox.print(&later, &["-c", "--output", "json", "what did we find?"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(result_of(&output, "c2"), "2: beta.rs has 1 TODO");
}

#[test]
fn a_script_asks_the_judge_role_to_classify() {
    let sandbox = Sandbox::new();
    let sort = r#"
        local answer = classify("What is this report about?\nThe app crashes when I click save.", { "bug", "question" })
        return answer.choice .. " " .. answer.confidence .. " by " .. answer.model
    "#;
    let script = json!({
        "lead": [run_code("c1", sort), text("A bug.")],
        "judge": [text("{\"choice\": 1, \"confidence\": 0.85, \"reason\": \"it crashes\"}")],
    });
    let output = sandbox.print(
        &script,
        &[
            "--set",
            "roles.judge=[\"mock/judge\"]",
            "--output",
            "json",
            "sort it",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(result_of(&output, "c1"), "bug 0.85 by mock/judge");
    // The judge saw the question and the choices, and nothing of the conversation.
    let asked = sandbox.asked();
    let (_, question) = asked.iter().find(|(model, _)| model == "judge").unwrap();
    assert!(
        question.contains("The app crashes when I click save.") && question.contains("1. bug\n2. question"),
        "{question}"
    );
}

const ROUTER: &str = r#"
azazel.router("auto", function(request)
    if request.reason ~= "user" and request.previous then
        return request.previous
    end
    if string.find(request.text, "hard", 1, true) then
        return "mock/big"
    end
    return "mock/small"
end)
"#;

/// `(model, via)` of every request the session records.
fn dispatched(session: &[Value]) -> Vec<(String, String)> {
    session
        .iter()
        .filter(|entry| entry["type"] == "request_started")
        .map(|entry| {
            (
                entry["model"].as_str().unwrap().to_string(),
                entry["via"].as_str().unwrap_or("-").to_string(),
            )
        })
        .collect()
}

#[test]
fn a_virtual_model_routes_each_request_and_a_session_comes_back_on_it() {
    let sandbox = Sandbox::new();
    sandbox.extension("router.luau", ROUTER);
    let script = json!({
        "big": [
            { "reply": { "tool_calls": [{ "id": "c1", "name": "glob", "input": { "pattern": "*.rs" } }] } },
            text("Solved the hard one."),
        ],
        "small": [text("Easy.")],
        "lead": [text("On the default model.")],
    });

    // A virtual model nobody defines is a mistake on the command line.
    let unknown = sandbox.print(&script, &["--model", "virtual/nobody", "x"]);
    assert_eq!(unknown.status.code(), Some(2), "{}", stderr(&unknown));
    assert!(
        stderr(&unknown).contains("--model virtual/nobody: the virtual models are virtual/auto"),
        "{}",
        stderr(&unknown)
    );

    // A virtual model picks among every provider's models: not with `--provider`.
    let limited = sandbox.print(&script, &["--provider", "mock", "--model", "virtual/auto", "x"]);
    assert_eq!(limited.status.code(), Some(2), "{}", stderr(&limited));
    assert!(
        stderr(&limited).contains(
            "--model virtual/auto: a virtual model picks among every provider's models and cannot be used \
             with --provider mock"
        ),
        "{}",
        stderr(&limited)
    );

    let output = sandbox.print(&script, &["--model", "virtual/auto", "a hard one"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "Solved the hard one.");
    let asked: Vec<String> = sandbox.asked().into_iter().map(|(model, _)| model).collect();
    assert_eq!(asked, ["big", "big"]);

    // The session goes on with the virtual model it was left on; this prompt is an easy one.
    let output = sandbox.print(&script, &["-c", "an easy one"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "Easy.");
    let session = sandbox.session();
    assert_eq!(
        dispatched(&session),
        [
            ("mock/big".to_string(), "virtual/auto".to_string()),
            ("mock/big".to_string(), "virtual/auto".to_string()),
            ("mock/small".to_string(), "virtual/auto".to_string()),
        ]
    );
    // What was chosen is recorded apart from where each request went.
    let chosen: Vec<&Value> = session
        .iter()
        .filter(|entry| entry["type"] == "model_change")
        .map(|entry| &entry["chain"])
        .collect();
    assert_eq!(chosen, [&json!(["virtual/auto"])]);

    // Without the extension the virtual model is gone: the session goes on with the default.
    std::fs::remove_file(sandbox.home().join("extensions/router.luau")).unwrap();
    let output = sandbox.print(&script, &["-c", "and now?"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "On the default model.");
    assert_eq!(
        dispatched(&sandbox.session()).last().unwrap(),
        &("mock/lead".to_string(), "-".to_string())
    );
}

#[test]
fn the_state_over_rpc_names_the_virtual_model_and_the_model_it_picked() {
    let sandbox = Sandbox::new();
    sandbox.extension("router.luau", ROUTER);
    let script = sandbox.script(&json!({ "small": [text("Easy.")] }));
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("azazel"))
        .env("AZAZEL_HOME", sandbox.home())
        .env("AZAZEL_MOCK_SCRIPT", script)
        .args(["--set", "session.auto_name=false", "--no-memory", "rpc", "--cwd"])
        .arg(sandbox.dir.path().join("project"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut until = |wanted: &dyn Fn(&Value) -> bool| loop {
        let line = lines.next().expect("the process ended early").unwrap();
        let line: Value = serde_json::from_str(&line).unwrap();
        if wanted(&line) {
            return line;
        }
    };
    let response = |id: u64| move |line: &Value| line["type"] == "response" && line["id"] == id;

    writeln!(
        stdin,
        "{}",
        json!({ "id": 1, "type": "set_model", "chain": ["virtual/auto"] })
    )
    .unwrap();
    let set = until(&response(1));
    assert_eq!(set["success"], true, "{set}");
    writeln!(stdin, "{}", json!({ "id": 2, "type": "get_state" })).unwrap();
    let before = until(&response(2));
    assert_eq!(before["data"]["chain"], json!(["virtual/auto"]));
    assert_eq!(before["data"]["virtual_models"], json!(["virtual/auto"]));
    assert_eq!(before["data"]["via"], Value::Null);

    writeln!(
        stdin,
        "{}",
        json!({ "id": 3, "type": "prompt", "text": "an easy one" })
    )
    .unwrap();
    until(&|line| line["type"] == "idle");
    writeln!(stdin, "{}", json!({ "id": 4, "type": "get_state" })).unwrap();
    let after = until(&response(4));
    assert_eq!(
        (&after["data"]["model"], &after["data"]["via"]),
        (&json!("mock/small"), &json!("virtual/auto"))
    );

    // A virtual model nobody defines is refused.
    writeln!(
        stdin,
        "{}",
        json!({ "id": 5, "type": "set_model", "chain": ["virtual/nobody"] })
    )
    .unwrap();
    let refused = until(&response(5));
    assert_eq!(refused["success"], false, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("the virtual models are virtual/auto"),
        "{refused}"
    );
    drop(stdin);
    child.wait().unwrap();
}
