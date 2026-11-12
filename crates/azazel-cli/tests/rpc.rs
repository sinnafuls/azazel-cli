#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use serde_json::{Value, json};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/scripts")
        .join(name)
}

/// An `azazel rpc` process and the lines it writes.
struct Client {
    dir: tempfile::TempDir,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Client {
    fn start(script: &str) -> Client {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut child = Command::new(assert_cmd::cargo::cargo_bin("azazel"))
            .env("AZAZEL_HOME", dir.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", fixture(script))
            .args(["rpc", "--cwd"])
            .arg(&project)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Client {
            stdin: child.stdin.take(),
            dir,
            child,
            lines,
        }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn send(&mut self, command: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{command}").unwrap();
        stdin.flush().unwrap();
    }

    /// The next line, which must be JSON: nothing else may appear on standard output.
    fn next(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(60))
            .expect("a protocol line within a minute");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not a protocol line ({e}): {line}"))
    }

    /// Reads up to the first line `wanted` accepts; returns the lines before it and that line.
    fn until(&self, wanted: impl Fn(&Value) -> bool) -> (Vec<Value>, Value) {
        let mut before = Vec::new();
        loop {
            let line = self.next();
            if wanted(&line) {
                return (before, line);
            }
            before.push(line);
        }
    }

    /// Sends a command and returns its response, with the events that came first.
    fn ask(&mut self, id: u64, mut command: Value) -> (Vec<Value>, Value) {
        command["id"] = id.into();
        self.send(command);
        self.until(|line| line["type"] == "response" && line["id"] == id)
    }

    /// Closes standard input and returns the exit code.
    fn finish(mut self) -> Option<i32> {
        drop(self.stdin.take());
        self.child.wait().unwrap().code()
    }
}

fn kinds(lines: &[Value]) -> Vec<&str> {
    lines.iter().map(|line| line["type"].as_str().unwrap()).collect()
}

#[test]
fn a_prompt_runs_through_an_approval_to_idle() {
    let mut client = Client::start("write_read.json");

    let (_, state) = client.ask(1, json!({ "type": "get_state" }));
    assert_eq!(state["success"], true, "{state}");
    let data = &state["data"];
    assert_eq!(
        (&data["mode"], &data["busy"], &data["turn"]),
        (&json!("ask"), &json!(false), &json!(0))
    );
    assert_eq!(data["chain"], json!(["@lead"]));

    // The prompt is accepted at once; the work shows up as events.
    let (_, accepted) = client.ask(2, json!({ "type": "prompt", "text": "create hello" }));
    assert_eq!(
        accepted,
        json!({ "id": 2, "type": "response", "success": true, "data": null })
    );

    // Writing a file needs the user's word in `ask` mode: the agent waits for the answer.
    let (before, request) = client.until(|line| line["type"] == "approval_request");
    assert!(kinds(&before).contains(&"tool_start"), "{before:?}");
    assert!(!kinds(&before).contains(&"tool_end"));
    assert_eq!(request["agent"], "lead");
    assert_eq!(request["tool"], "write");
    assert_eq!(request["access"], "write");
    assert_eq!(request["subjects"], json!(["hello.txt"]));
    assert_eq!(request["input"]["content"], "hello from azazel\n");
    assert!(!client.project().join("hello.txt").exists());
    let (_, busy) = client.ask(3, json!({ "type": "get_state" }));
    assert_eq!(busy["data"]["busy"], true);

    // An answer for a request that does not exist changes nothing.
    let (_, stray) = client.ask(
        4,
        json!({ "type": "approval_response", "request_id": "r999", "decision": "allow" }),
    );
    assert_eq!(stray["error"], "no approval is waiting as `r999`");
    let request_id = request["request_id"].clone();
    let (_, unknown) = client.ask(
        5,
        json!({ "type": "approval_response", "request_id": request_id, "decision": "maybe" }),
    );
    assert_eq!(unknown["success"], false);

    let (_, answered) = client.ask(
        6,
        json!({ "type": "approval_response", "request_id": request_id, "decision": "allow" }),
    );
    assert_eq!(answered["success"], true, "{answered}");
    let (after, idle) = client.until(|line| line["type"] == "idle");
    assert_eq!((&idle["agent"], &idle["stop"]), (&json!("lead"), &json!("stop")));
    let finished: Vec<&Value> = after.iter().filter(|line| line["type"] == "tool_end").collect();
    assert_eq!(finished.len(), 2, "{after:?}");
    assert!(finished.iter().all(|line| line["output"]["is_error"] == false));
    assert_eq!(
        std::fs::read_to_string(client.project().join("hello.txt")).unwrap(),
        "hello from azazel\n"
    );
    // The same answer cannot be given twice.
    let (_, again) = client.ask(
        7,
        json!({ "type": "approval_response", "request_id": request_id, "decision": "allow" }),
    );
    assert_eq!(again["success"], false);

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn commands_change_the_session_and_mistakes_are_answered() {
    let mut client = Client::start("write_read.json");

    let (_, mode) = client.ask(1, json!({ "type": "set_mode", "mode": "auto" }));
    assert_eq!(mode["data"], json!({ "mode": "auto" }));
    let (_, bad_mode) = client.ask(2, json!({ "type": "set_mode", "mode": "yolo" }));
    assert!(bad_mode["error"].as_str().unwrap().contains("yolo"), "{bad_mode}");
    let (_, model) = client.ask(
        3,
        json!({ "type": "set_model", "chain": ["mock/other", "@default"] }),
    );
    assert_eq!(model["data"]["models"], json!(["mock/other", "mock/lead"]));
    let (_, no_model) = client.ask(4, json!({ "type": "set_model", "chain": ["nowhere/m"] }));
    // A name of no known provider is looked up loosely and matches nothing.
    assert!(
        no_model["error"]
            .as_str()
            .unwrap()
            .starts_with("nowhere/m: no model"),
        "{no_model}"
    );
    let (_, state) = client.ask(5, json!({ "type": "get_state" }));
    assert_eq!(state["data"]["mode"], "auto");
    assert_eq!(state["data"]["chain"], json!(["mock/other", "@default"]));
    // Back to the model the script answers as.
    let (_, model) = client.ask(50, json!({ "type": "set_model", "chain": ["@default"] }));
    assert_eq!(model["data"]["models"], json!(["mock/lead"]));

    for (id, command, error) in [
        (6, json!({ "type": "dance" }), "unknown command `dance`"),
        (7, json!({ "text": "hi" }), "the command has no `type`"),
        (8, json!({ "type": "prompt" }), "`text` must be a string"),
        (
            9,
            json!({ "type": "agent_stop", "agent": "ghost-1" }),
            "no agent named `ghost-1`",
        ),
        (
            10,
            json!({ "type": "ui_response", "request_id": "r1", "value": true }),
            "no request is waiting as `r1`",
        ),
    ] {
        let (_, response) = client.ask(id, command);
        assert_eq!(
            (&response["success"], &response["error"]),
            (&json!(false), &json!(error))
        );
    }
    client.send(json!([1, 2]));
    let (_, not_object) = client.until(|line| line["type"] == "response");
    assert_eq!(
        not_object,
        json!({ "id": null, "type": "response", "success": false, "error": "a command is a JSON object" })
    );
    client.stdin.as_mut().unwrap().write_all(b"\n{oops\n").unwrap();
    let (_, not_json) = client.until(|line| line["type"] == "response");
    assert!(
        not_json["error"].as_str().unwrap().starts_with("not JSON: "),
        "{not_json}"
    );

    // In `auto` nothing is asked: the prompt runs to the end on its own.
    let (_, accepted) = client.ask(11, json!({ "type": "prompt", "text": "create hello" }));
    assert_eq!(accepted["success"], true);
    let (events, _) = client.until(|line| line["type"] == "idle");
    assert!(!kinds(&events).contains(&"approval_request"), "{events:?}");
    assert!(client.project().join("hello.txt").is_file());

    assert_eq!(client.finish(), Some(0));
}
