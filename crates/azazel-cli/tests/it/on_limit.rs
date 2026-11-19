//! `[profiles] on_limit`: a usage limit moves the lead to another profile once and the request
//! goes on there, as the binary does it with the scripted provider.

use std::path::PathBuf;
use std::process::Output;

use crate::common::{stderr, stdout};
use assert_cmd::Command;
use serde_json::{Value, json};

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new(config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.azazel")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::write(dir.path().join("home/config.toml"), config).unwrap();
        Sandbox { dir }
    }

    fn requests(&self) -> PathBuf {
        self.dir.path().join("requests.jsonl")
    }

    fn run(&self, script: Value) -> Output {
        let path = self.dir.path().join("script.json");
        std::fs::write(&path, script.to_string()).unwrap();
        Command::cargo_bin("azazel")
            .unwrap()
            .env("AZAZEL_HOME", self.dir.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", path)
            .env("AZAZEL_MOCK_REQUESTS", self.requests())
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .args([
                "-p",
                "go",
                "--mode",
                "auto",
                "--no-memory",
                "--set",
                "session.auto_name=false",
                "--cwd",
            ])
            .arg(self.dir.path().join("project"))
            .write_stdin("")
            .output()
            .unwrap()
    }

    /// The model of every request the scripted provider received, in order.
    fn models(&self) -> Vec<String> {
        std::fs::read_to_string(self.requests())
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let row: Value = serde_json::from_str(line).unwrap();
                row["model"].as_str().unwrap_or_default().to_string()
            })
            .collect()
    }
}

const PROFILE: &str = "[profiles.fallback.roles]\ndefault = [\"mock/scout\"]\n";

fn limit() -> Value {
    json!({ "usage_limit": { "resets_in_secs": 7200 } })
}

fn reply(text: &str) -> Value {
    json!({ "reply": { "text": text } })
}

#[test]
fn a_usage_limit_switches_once_and_the_request_goes_to_the_new_model() {
    let sandbox = Sandbox::new(&format!("[profiles]\non_limit = \"fallback\"\n{PROFILE}"));
    let output = sandbox.run(json!({ "lead": [limit()], "scout": [reply("on the fallback")] }));
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("on the fallback"), "{}", stdout(&output));
    assert_eq!(sandbox.models(), ["lead", "scout"]);
    let error = stderr(&output);
    assert!(
        error.contains("usage limit reached") && error.contains("costs more"),
        "{error}"
    );
}

#[test]
fn nothing_switches_without_an_on_limit() {
    let sandbox = Sandbox::new(PROFILE);
    let output = sandbox.run(json!({ "lead": [limit()], "scout": [reply("never")] }));
    assert!(!stdout(&output).contains("never"), "{}", stdout(&output));
    assert_eq!(sandbox.models(), ["lead"]);
}

#[test]
fn a_fallback_that_hits_a_limit_too_ends_the_run_with_the_error() {
    let sandbox = Sandbox::new(&format!("[profiles]\non_limit = \"fallback\"\n{PROFILE}"));
    let output = sandbox.run(json!({ "lead": [limit()], "scout": [limit(), reply("never")] }));
    assert!(!stdout(&output).contains("never"), "{}", stdout(&output));
    assert_eq!(sandbox.models(), ["lead", "scout"]);
}

#[test]
fn transient_overload_is_retried_in_place_and_switches_nothing() {
    let sandbox = Sandbox::new(&format!("[profiles]\non_limit = \"fallback\"\n{PROFILE}"));
    let overloaded = json!({ "events": [{ "type": "error", "error": { "kind": "overloaded" } }] });
    let output = sandbox.run(json!({
        "lead": [overloaded, reply("recovered")],
        "scout": [reply("never")],
    }));
    assert!(
        stdout(&output).contains("recovered"),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(sandbox.models(), ["lead", "lead"]);
}

/// An `azazel rpc` process in a sandbox, and the lines it writes.
struct Rpc {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    lines: std::sync::mpsc::Receiver<String>,
    next_id: u64,
}

impl Rpc {
    fn start(sandbox: &Sandbox, script: Value) -> Rpc {
        use std::io::{BufRead, BufReader};
        let path = sandbox.dir.path().join("script.json");
        std::fs::write(&path, script.to_string()).unwrap();
        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("azazel"))
            .env("AZAZEL_HOME", sandbox.dir.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", path)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .args(["--no-memory", "--set", "session.auto_name=false", "rpc", "--cwd"])
            .arg(sandbox.dir.path().join("project"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Rpc {
            stdin: child.stdin.take(),
            child,
            lines,
            next_id: 0,
        }
    }

    /// Reads up to the first line `wanted` accepts; returns the lines before it and that line.
    fn until(&self, wanted: impl Fn(&Value) -> bool) -> (Vec<Value>, Value) {
        let mut before = Vec::new();
        loop {
            let line = self
                .lines
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("a protocol line within a minute");
            let line: Value = serde_json::from_str(&line).unwrap();
            if wanted(&line) {
                return (before, line);
            }
            before.push(line);
        }
    }

    fn ask(&mut self, mut command: Value) -> (Vec<Value>, Value) {
        use std::io::Write;
        self.next_id += 1;
        let id = self.next_id;
        command["id"] = id.into();
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{command}").unwrap();
        stdin.flush().unwrap();
        self.until(|line| line["type"] == "response" && line["id"] == id)
    }

    fn finish(mut self) {
        drop(self.stdin.take());
        self.child.wait().unwrap();
    }
}

#[test]
fn an_rpc_client_is_told_of_the_switch_with_one_profile_changed_frame() {
    let sandbox = Sandbox::new(&format!("[profiles]\non_limit = \"fallback\"\n{PROFILE}"));
    let mut client = Rpc::start(
        &sandbox,
        json!({ "lead": [limit()], "scout": [reply("on the fallback")] }),
    );
    let (_, state) = client.ask(json!({ "type": "get_state" }));
    assert_eq!(state["data"]["profile"], Value::Null);

    let (_, accepted) = client.ask(json!({ "type": "prompt", "text": "go" }));
    assert_eq!(accepted["success"], true, "{accepted}");
    let (events, idle) = client.until(|line| line["type"] == "idle" || line["type"] == "failed");
    assert_eq!(idle["type"], "idle", "{idle}");

    let frames: Vec<&Value> = events
        .iter()
        .filter(|line| line["type"] == "profile_changed")
        .collect();
    assert_eq!(frames.len(), 1, "{events:?}");
    let frame = frames[0];
    assert_eq!(
        (&frame["to"], &frame["reason"]),
        (&json!("fallback"), &json!("limit"))
    );
    assert!(frame["until"].as_u64().unwrap() > 0, "{frame}");
    assert!(
        frame["limit"]
            .as_str()
            .is_some_and(|limit| limit.contains("limit")),
        "{frame}"
    );
    // The switch is told by the frame alone: no second message about it.
    assert!(
        !events
            .iter()
            .any(|line| line.to_string().contains("usage limit reached")),
        "{events:?}"
    );

    let (_, state) = client.ask(json!({ "type": "get_state" }));
    assert_eq!(state["data"]["profile"], "fallback");
    client.finish();
}
