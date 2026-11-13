#![allow(clippy::unwrap_used)]

//! Runs that outlive their process: a process killed in the middle of a run leaves a session
//! that says so, and `--continue-run` and the RPC command `continue_run` go on with it.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const LIMIT: Duration = Duration::from_secs(60);

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        Sandbox { dir }
    }

    /// `azazel` answering from `script`, in this sandbox's home and project.
    fn azazel(&self, script: &Value, args: &[&str]) -> Command {
        let script_path = self.dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let _ = std::fs::remove_file(self.dir.path().join("requests.jsonl"));
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("azazel"));
        command
            .env("AZAZEL_HOME", self.dir.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", &script_path)
            .env("AZAZEL_MOCK_REQUESTS", self.dir.path().join("requests.jsonl"))
            .env("AZAZEL_MOCK_TOOLS", "1")
            .args(["--set", "session.auto_name=false", "--no-memory"])
            .arg("--cwd")
            .arg(self.dir.path().join("project"))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// The requests the scripted provider received since the last `azazel`.
    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
            .collect()
    }

    fn session_file(&self) -> Option<PathBuf> {
        let projects = std::fs::read_dir(self.dir.path().join("home/projects")).ok()?;
        projects
            .flatten()
            .flat_map(|project| {
                std::fs::read_dir(project.path().join("sessions"))
                    .into_iter()
                    .flatten()
            })
            .flatten()
            .map(|file| file.path())
            .find(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
    }

    /// The entries of the session file, by their `type`.
    fn entries(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(self.session_file().unwrap()).unwrap();
        text.lines()
            .skip(1)
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Starts a run that will not end by itself and kills its process once the session file
    /// holds an entry of `kind`.
    fn killed_after(&self, script: &Value, kind: &str) {
        let mut child = self
            .azazel(script, &["--mode", "auto", "-p", "do the work"])
            .spawn()
            .unwrap();
        let started = Instant::now();
        loop {
            let written = self
                .session_file()
                .and_then(|file| std::fs::read_to_string(file).ok())
                .unwrap_or_default();
            if written.contains(&format!("\"type\":\"{kind}\"")) {
                break;
            }
            assert!(started.elapsed() < LIMIT, "no `{kind}` entry in: {written}");
            std::thread::sleep(Duration::from_millis(25));
        }
        kill(&mut child);
    }
}

/// Ends a process the hard way: nothing of it gets to clean up.
fn kill(child: &mut Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}

fn text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn kinds(entries: &[Value]) -> Vec<&str> {
    entries
        .iter()
        .filter_map(|entry| entry["type"].as_str())
        .collect()
}

/// A model that never answers in the time a test takes.
fn silent() -> Value {
    json!({ "lead": [{ "delay_ms": 600_000, "reply": { "text": "too late" } }] })
}

#[test]
fn a_run_killed_before_the_model_answered_is_gone_on_with_in_print_mode() {
    let sandbox = Sandbox::new();
    sandbox.killed_after(&silent(), "request_started");
    let entries = sandbox.entries();
    // A run with a start and no end.
    assert!(
        kinds(&entries).ends_with(&["message", "run_started", "request_started"]),
        "{:?}",
        kinds(&entries)
    );

    // A prompt is not what goes on with it, and neither flag stands alone.
    let wrong = sandbox
        .azazel(&json!({ "lead": [] }), &["--continue-run", "-p"])
        .output()
        .unwrap();
    assert_eq!(wrong.status.code(), Some(2), "{}", text(&wrong).1);

    let recovered = json!({ "lead": [{ "reply": { "text": "Recovered." } }] });
    let output = sandbox
        .azazel(&recovered, &["-c", "-p", "--continue-run"])
        .output()
        .unwrap();
    let (out, err) = text(&output);
    assert!(output.status.success(), "{err}");
    assert_eq!(out, "Recovered.\n");
    // The request that went unanswered was sent again as it was: the prompt is there once.
    let requests = sandbox.requests();
    assert_eq!(requests.len(), 1);
    let messages = requests[0]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0]["content"][0]["text"], "do the work");
    // It is the run that was cut short that ended.
    let entries = sandbox.entries();
    let started: Vec<&Value> = entries
        .iter()
        .filter(|entry| entry["type"] == "run_started")
        .collect();
    let ended: Vec<&Value> = entries
        .iter()
        .filter(|entry| entry["type"] == "run_ended")
        .collect();
    assert_eq!((started.len(), ended.len()), (1, 1), "{:?}", kinds(&entries));
    assert_eq!(ended[0]["runId"], started[0]["runId"]);
    assert_eq!(ended[0]["stop"], "stop");

    // Nothing is left to go on with.
    let again = sandbox
        .azazel(&recovered, &["-c", "-p", "--continue-run"])
        .output()
        .unwrap();
    assert_eq!(again.status.code(), Some(1));
    assert!(
        text(&again).1.contains("no run of this session was interrupted"),
        "{}",
        text(&again).1
    );
}

#[test]
fn a_run_killed_in_a_tool_call_is_gone_on_with_over_rpc() {
    let sandbox = Sandbox::new();
    std::fs::write(
        sandbox.dir.path().join("project/notes.txt"),
        "remember the milk\n",
    )
    .unwrap();
    // A read, then a call that outlives the test: the process dies while it runs.
    let stuck = json!({ "lead": [{ "reply": { "tool_calls": [
        { "id": "c1", "name": "read", "input": { "path": "notes.txt" } },
        { "id": "c2", "name": "test_wait", "input": { "ms": 600_000 } },
    ] } }] });
    sandbox.killed_after(&stuck, "tool_started\",\"callId\":\"c2");

    let recovered = json!({ "lead": [{ "reply": { "text": "Picked up where it stopped." } }] });
    let mut rpc = sandbox
        .azazel(&recovered, &["--mode", "auto", "-c", "rpc"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut commands = rpc.stdin.take().unwrap();
    let mut lines = BufReader::new(rpc.stdout.take().unwrap()).lines();
    let mut next = || -> Value { serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap() };
    let mut until = |wanted: &dyn Fn(&Value) -> bool| -> Vec<Value> {
        let mut read = Vec::new();
        loop {
            let line = next();
            let found = wanted(&line);
            read.push(line);
            if found {
                return read;
            }
        }
    };
    let mut send = |command: Value| writeln!(commands, "{command}").unwrap();

    // The session says where it stopped.
    send(json!({ "id": 1, "type": "get_state" }));
    let state = until(&|line| line["id"] == 1).pop().unwrap();
    assert_eq!(
        state["data"]["interrupted"], "stopped while read, test_wait ran",
        "{state}"
    );
    assert_eq!(state["data"]["busy"], false);

    send(json!({ "id": 2, "type": "continue_run" }));
    let frames = until(&|line| line["type"] == "idle" || line["type"] == "failed");
    let answered = frames.iter().find(|line| line["id"] == 2).unwrap();
    assert_eq!(answered["data"]["state"], "running", "{answered}");
    let results: Vec<(&str, bool, String)> = frames
        .iter()
        .filter(|line| line["type"] == "tool_end")
        .map(|line| {
            (
                line["call_id"].as_str().unwrap(),
                line["output"]["is_error"].as_bool().unwrap(),
                line["output"]["content"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect();
    assert_eq!(results.len(), 2, "{frames:?}");
    // The read only reads: it ran again. The other call may have done anything: it did not.
    assert_eq!((results[0].0, results[0].1), ("c1", false));
    assert!(results[0].2.contains("remember the milk"), "{results:?}");
    assert_eq!((results[1].0, results[1].1), ("c2", true));
    assert!(
        results[1]
            .2
            .starts_with("interrupted: the process stopped before this call finished"),
        "{results:?}"
    );
    assert_eq!(frames.last().unwrap()["type"], "idle");

    // With the run at its end, the session has nothing to go on with.
    send(json!({ "id": 3, "type": "get_state" }));
    let state = until(&|line| line["id"] == 3).pop().unwrap();
    assert_eq!(state["data"]["interrupted"], Value::Null, "{state}");
    send(json!({ "id": 4, "type": "continue_run" }));
    let refused = until(&|line| line["id"] == 4).pop().unwrap();
    assert_eq!(refused["success"], false);
    drop(commands);
    assert!(rpc.wait().unwrap().success());

    // The model was told what became of each call.
    let requests = sandbox.requests();
    assert_eq!(requests.len(), 1);
    let results = &requests[0]["messages"].as_array().unwrap().last().unwrap()["content"];
    assert_eq!(results.as_array().unwrap().len(), 2, "{results}");
}
