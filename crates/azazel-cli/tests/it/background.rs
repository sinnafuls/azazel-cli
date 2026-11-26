#![cfg(feature = "detach")]

//! Background sessions: `azazel bg`, `ps`, `logs`, `attach`, `kill`, the lock on their session
//! and `/bg`, against the real binary and the scripted provider.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::common::lines_of;
use azazel_protocol::Frame;
use serde_json::{Value, json};

const LIMIT: Duration = Duration::from_secs(60);

/// A home and a project of its own. Background sessions still running when it goes are
/// stopped.
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new(script: &Value) -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::write(dir.path().join("script.json"), script.to_string()).unwrap();
        Sandbox { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    /// `azazel` with this sandbox's home, script and project, and `args` after them.
    fn azazel(&self, args: &[&str]) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("azazel"));
        command
            .env("AZAZEL_HOME", self.home())
            .env("AZAZEL_MOCK_SCRIPT", self.dir.path().join("script.json"))
            .env("AZAZEL_MOCK_REQUESTS", self.dir.path().join("requests.jsonl"))
            .env("AZAZEL_MOCK_TOOLS", "1")
            .args(["--set", "session.auto_name=false", "--no-memory"])
            // No toast on the machine that runs the tests.
            .args(["--set", "notify.desktop=\"off\""])
            .arg("--cwd")
            .arg(self.project())
            .args(args)
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.azazel(args).output().unwrap()
    }

    /// The standard output of a command that must succeed.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// Starts a background session and returns its id.
    fn bg(&self, args: &[&str], prompt: &str) -> String {
        let mut all = args.to_vec();
        all.extend(["bg", prompt]);
        let id = self.ok(&all).trim().to_string();
        assert!(
            id.len() == 8 && id.chars().all(|c| c.is_ascii_hexdigit()),
            "an id: {id:?}"
        );
        id
    }

    fn ps(&self) -> Vec<Value> {
        self.ok(&["ps", "--json"])
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Waits until `azazel ps` says the session `id` is in `state`.
    fn wait_for_state(&self, id: &str, state: &str) -> Value {
        let started = Instant::now();
        loop {
            let rows = self.ps();
            if let Some(row) = rows.iter().find(|row| row["id"] == id && row["state"] == state) {
                return row.clone();
            }
            assert!(
                started.elapsed() < LIMIT,
                "{id} never was `{state}`: {rows:?}\n{}",
                self.errors(id)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// What the background process wrote to standard error.
    fn errors(&self, id: &str) -> String {
        std::fs::read_to_string(self.home().join("run").join(format!("{id}.err"))).unwrap_or_default()
    }

    fn run_file(&self, name: &str) -> PathBuf {
        self.home().join("run").join(name)
    }

    /// The requests the scripted provider received, over every process of the sandbox.
    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
            .collect()
    }

    fn session_files(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for project in std::fs::read_dir(self.home().join("projects")).unwrap().flatten() {
            let sessions = project.path().join("sessions");
            for file in std::fs::read_dir(sessions).into_iter().flatten().flatten() {
                if file.path().extension().is_some_and(|e| e == "jsonl") {
                    found.push(file.path());
                }
            }
        }
        found.sort();
        found
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let listed = self.azazel(&["ps", "--json"]).output();
        let rows = listed
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        for row in rows
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        {
            if let Some(id) = row["id"].as_str() {
                let _ = self.azazel(&["kill", id]).output();
            }
        }
    }
}

/// Reads lines up to the first that contains `wanted`; returns all of them.
fn read_until(lines: &Receiver<String>, wanted: &str) -> Vec<String> {
    crate::common::read_until(lines, wanted, LIMIT)
}

fn text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn a_prompt_runs_in_the_background_and_is_listed_logged_and_killed() {
    let sandbox =
        Sandbox::new(&json!({ "lead": [{ "delay_ms": 2500, "reply": { "text": "The work is done." } }] }));
    let id = sandbox.bg(&["--name", "night shift"], "do the work");

    // `bg` came back while the lead was still waiting for its reply.
    let rows = sandbox.ps();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(
        (&row["id"], &row["state"], &row["name"]),
        (&json!(id), &json!("working"), &json!("night shift")),
        "{row}"
    );
    assert_eq!(
        dunce::canonicalize(row["cwd"].as_str().unwrap()).unwrap(),
        dunce::canonicalize(sandbox.project()).unwrap()
    );
    let table = sandbox.ok(&["ps"]);
    assert!(table.starts_with("ID "), "{table}");
    assert!(
        table.contains(&id) && table.contains("night shift") && table.contains("working"),
        "{table}"
    );

    // The session is this process's alone for as long as it runs.
    let session_id = row["session_id"].as_str().unwrap().to_string();
    let refused = sandbox.run(&["-p", "--session", &session_id, "me too"]);
    let (_, why) = text(&refused);
    assert_eq!(refused.status.code(), Some(1), "{why}");
    assert!(
        why.contains(&format!(
            "this session is running in the background: azazel attach {id}"
        )),
        "{why}"
    );

    sandbox.wait_for_state(&id, "idle");
    // By name and by the start of the id alike.
    let log = sandbox.ok(&["logs", "night shift"]);
    assert_eq!(log, "\n❯ do the work\nThe work is done.\n· done\n");
    let frames = sandbox.ok(&["logs", &id[..4], "--json"]);
    let kinds: Vec<String> = frames
        .lines()
        .map(|line| {
            let frame: Frame = serde_json::from_str(line).unwrap();
            assert_ne!(frame, Frame::Unknown, "{line}");
            serde_json::from_str::<Value>(line).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(kinds.first().map(String::as_str), Some("user_input"), "{kinds:?}");
    assert!(
        kinds.contains(&"message_end".to_string()) && kinds.contains(&"idle".to_string()),
        "{kinds:?}"
    );

    assert_eq!(sandbox.ok(&["kill", &id]).trim(), format!("stopped {id}"));
    assert!(sandbox.ps().is_empty());
    assert!(!sandbox.run_file(&format!("{id}.json")).exists());
    assert!(!sandbox.run_file(&format!("{id}.sock")).exists());
    assert!(!sandbox.run_file(&format!("{id}.lock")).exists());
    assert!(!sandbox.run_file(&format!("session-{session_id}.lock")).exists());
    // The log outlives the session, and the session can be opened again.
    assert!(sandbox.ok(&["logs", &id]).contains("The work is done."));
    let gone = sandbox.run(&["kill", &id]);
    assert_eq!(gone.status.code(), Some(1));
    assert!(
        text(&gone).1.contains("no background session"),
        "{}",
        text(&gone).1
    );
}

#[test]
fn an_approval_raised_with_nobody_attached_is_answered_after_attach() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/scripts/write_read.json");
    let script: Value = serde_json::from_str(&std::fs::read_to_string(script).unwrap()).unwrap();
    let sandbox = Sandbox::new(&script);
    let id = sandbox.bg(&[], "create hello");
    sandbox.wait_for_state(&id, "waiting for you");
    assert!(!sandbox.project().join("hello.txt").exists());

    let mut first = sandbox
        .azazel(&["attach", &id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut typing = first.stdin.take().unwrap();
    let shown = lines_of(first.stdout.take().unwrap());
    // What happened before is replayed, then the request that still waits.
    let replayed = read_until(&shown, "/approve");
    assert_eq!(
        replayed,
        [
            "",
            "❯ create hello",
            "→ write hello.txt",
            "? lead wants to use write: hello.txt",
            "  /approve, /session, /always, or /deny [what to do instead]"
        ]
    );

    writeln!(typing, "/approve").unwrap();
    let rest = read_until(&shown, "· done");
    assert!(
        rest.contains(&"Wrote and verified hello.txt.".to_string()),
        "{rest:?}"
    );
    assert_eq!(
        std::fs::read_to_string(sandbox.project().join("hello.txt")).unwrap(),
        "hello from azazel\n"
    );
    writeln!(typing, "/detach").unwrap();
    let last = read_until(&shown, "detached");
    assert!(last.last().unwrap().contains(&id), "{last:?}");
    assert!(first.wait().unwrap().success());

    // Detaching left it running; a client that attaches now is told nothing twice.
    sandbox.wait_for_state(&id, "idle");
    let mut again = sandbox
        .azazel(&["attach", &id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut typing = again.stdin.take().unwrap();
    let shown = lines_of(again.stdout.take().unwrap());
    writeln!(typing, "/kill").unwrap();
    let end = read_until(&shown, "the background session has ended");
    assert_eq!(end, ["the background session has ended"]);
    assert!(again.wait().unwrap().success());
    assert!(sandbox.ps().is_empty());
}

#[test]
fn a_taken_session_leaves_no_background_process_and_goes_on_here() {
    let sandbox = Sandbox::new(&json!({ "lead": [
        { "reply": { "text": "First answer." } },
        { "delay_ms": 30000, "reply": { "text": "never said" } },
    ] }));
    let id = sandbox.bg(&[], "first question");
    sandbox.wait_for_state(&id, "idle");

    // The interactive UI needs a terminal; without one the background session is left alone.
    let no_terminal = sandbox.run(&["attach", "--take", &id]);
    assert_eq!(no_terminal.status.code(), Some(2), "{}", text(&no_terminal).1);
    assert_eq!(sandbox.ps().len(), 1);

    // A second prompt keeps the lead busy; `--now` does not wait for it.
    let mut client = sandbox
        .azazel(&["attach", &id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut typing = client.stdin.take().unwrap();
    let shown = lines_of(client.stdout.take().unwrap());
    writeln!(typing, "second question").unwrap();
    sandbox.wait_for_state(&id, "working");
    writeln!(typing, "/detach").unwrap();
    read_until(&shown, "detached");
    assert!(client.wait().unwrap().success());

    // Print mode reads its prompt from standard input; the taken session answers it here. The
    // script starts over in this process, so its first reply is said again.
    let mut taking = sandbox
        .azazel(&["-p", "attach", "--take", "--now", &id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    taking.stdin.take().unwrap().write_all(b"third question").unwrap();
    let taken = taking.wait_with_output().unwrap();
    let (said, why) = text(&taken);
    assert!(taken.status.success(), "{why}");
    assert_eq!(said.trim(), "First answer.");
    assert!(sandbox.ps().is_empty());
    assert!(!sandbox.run_file(&format!("{id}.json")).exists());

    // One session file, and the last request carries what the background session did.
    assert_eq!(sandbox.session_files().len(), 1);
    let requests = sandbox.requests();
    let last = serde_json::to_string(requests.last().unwrap()).unwrap();
    for part in [
        "first question",
        "First answer.",
        "second question",
        "third question",
    ] {
        assert!(last.contains(part), "`{part}` in {last}");
    }
}

#[test]
fn another_program_drives_a_background_session_through_the_bridge() {
    let sandbox = Sandbox::new(&json!({ "lead": [
        { "reply": { "text": "First." } },
        { "reply": { "text": "Second." } },
    ] }));
    let id = sandbox.bg(&["--mode", "auto"], "one");
    sandbox.wait_for_state(&id, "idle");

    let mut bridge = sandbox
        .azazel(&["rpc", "--attach", &id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut commands = bridge.stdin.take().unwrap();
    let frames = lines_of(bridge.stdout.take().unwrap());
    let next = || -> Value {
        let line = frames.recv_timeout(LIMIT).expect("a frame within a minute");
        let frame: Frame = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert_ne!(frame, Frame::Unknown, "{line}");
        serde_json::from_str(&line).unwrap()
    };
    assert_eq!(next()["type"], "hello");

    // Nothing but `attach` and `stop` is taken from a connection that is not the client.
    writeln!(commands, "{}", json!({ "id": 1, "type": "get_state" })).unwrap();
    let refused = next();
    assert_eq!((&refused["id"], &refused["success"]), (&json!(1), &json!(false)));
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .starts_with("send `attach` first"),
        "{refused}"
    );

    // Attaching answers, then replays what was asked for: here, everything.
    writeln!(
        commands,
        "{}",
        json!({ "id": 2, "type": "attach", "replay": "all" })
    )
    .unwrap();
    let attached = next();
    assert_eq!(attached["success"], true, "{attached}");
    assert_eq!(attached["data"]["host"], json!(id));
    let mut replayed = Vec::new();
    loop {
        let frame = next();
        let done = frame["type"] == "idle";
        replayed.push(frame["type"].as_str().unwrap().to_string());
        if done {
            break;
        }
    }
    assert_eq!(
        replayed.first().map(String::as_str),
        Some("user_input"),
        "{replayed:?}"
    );
    assert!(replayed.contains(&"message_end".to_string()), "{replayed:?}");

    // From here on it is the protocol of `azazel rpc`.
    writeln!(
        commands,
        "{}",
        json!({ "id": 3, "type": "prompt", "text": "two" })
    )
    .unwrap();
    let mut said = String::new();
    loop {
        let frame = next();
        if frame["type"] == "message_end" {
            said = frame["message"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string();
        }
        if frame["type"] == "idle" {
            break;
        }
    }
    assert_eq!(said, "Second.");
    writeln!(commands, "{}", json!({ "id": 4, "type": "get_state" })).unwrap();
    let state = loop {
        let frame = next();
        if frame["id"] == 4 {
            break frame;
        }
    };
    assert_eq!(state["data"]["busy"], false, "{state}");

    // Closing the bridge detaches; the session goes on and the log has both prompts.
    drop(commands);
    assert!(bridge.wait().unwrap().success());
    sandbox.wait_for_state(&id, "idle");
    let log = sandbox.ok(&["logs", &id]);
    assert_eq!(log, "\n❯ one\nFirst.\n· done\n\n❯ two\nSecond.\n· done\n");
    assert_eq!(sandbox.ok(&["kill", &id]).trim(), format!("stopped {id}"));
}

#[test]
fn a_background_session_with_nothing_to_do_and_nobody_attached_ends() {
    let sandbox = Sandbox::new(&json!({ "lead": [{ "reply": { "text": "Done." } }] }));
    let host = sandbox
        .azazel(&["host", "--id", "feedc0de", "--idle-secs", "1", "say done"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let ended = host.wait_with_output().unwrap();
    assert!(ended.status.success(), "{}", text(&ended).1);
    assert!(sandbox.ps().is_empty());
    assert!(!sandbox.run_file("feedc0de.json").exists());
    assert_eq!(sandbox.ok(&["logs", "feedc0de"]), "\n❯ say done\nDone.\n· done\n");
}

#[test]
fn bg_as_a_command_continues_a_copy_of_the_session_in_the_background() {
    let sandbox = Sandbox::new(&json!({ "lead": [{ "reply": { "text": "An answer." } }] }));
    // A session with one exchange to copy.
    assert_eq!(sandbox.ok(&["-p", "the first question"]).trim(), "An answer.");
    let mut rpc = sandbox
        .azazel(&["-c", "rpc"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut commands = rpc.stdin.take().unwrap();
    let frames = lines_of(rpc.stdout.take().unwrap());
    writeln!(
        commands,
        "{}",
        json!({ "id": 1, "type": "run_command", "name": "bg", "args": "and a second one" })
    )
    .unwrap();
    let answered = read_until(&frames, "\"id\":1");
    let response: Value = serde_json::from_str(answered.last().unwrap()).unwrap();
    assert_eq!(response["success"], true, "{response}");
    let shown = response["data"]["text"].as_str().unwrap();
    let id = shown
        .strip_prefix("started background session ")
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("{shown}"))
        .to_string();
    drop(commands);
    assert!(rpc.wait().unwrap().success());

    let row = sandbox.wait_for_state(&id, "idle");
    // The copy is a session of its own that names the one it came from.
    let files = sandbox.session_files();
    assert_eq!(files.len(), 2, "{files:?}");
    let copy = PathBuf::from(row["session_file"].as_str().unwrap());
    let header: Value =
        serde_json::from_str(std::fs::read_to_string(&copy).unwrap().lines().next().unwrap()).unwrap();
    let original = files
        .iter()
        .find(|file| file.file_name() != copy.file_name())
        .unwrap();
    assert_eq!(
        Path::new(header["parentSession"].as_str().unwrap()).file_name(),
        original.file_name()
    );
    let log = sandbox.ok(&["logs", &id]);
    assert_eq!(log, "\n❯ and a second one\nAn answer.\n· done\n");
    // The background run was asked with the first exchange in its context.
    let last = serde_json::to_string(sandbox.requests().last().unwrap()).unwrap();
    assert!(
        last.contains("the first question") && last.contains("and a second one"),
        "{last}"
    );
    assert_eq!(sandbox.ok(&["kill", &id]).trim(), format!("stopped {id}"));
}

/// Ends a process the hard way: nothing of it gets to clean up.
fn kill_process(pid: u64) {
    #[cfg(windows)]
    let killed = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output();
    #[cfg(unix)]
    let killed = Command::new("kill").args(["-9", &pid.to_string()]).output();
    assert!(killed.unwrap().status.success(), "could not kill {pid}");
}

#[test]
fn a_background_session_killed_in_a_tool_call_is_listed_and_goes_on_when_attached() {
    // A read, then a call that outlives the test.
    let sandbox = Sandbox::new(&json!({ "lead": [{ "reply": { "tool_calls": [
        { "id": "c1", "name": "read", "input": { "path": "notes.txt" } },
        { "id": "c2", "name": "test_wait", "input": { "ms": 600_000 } },
    ] } }] }));
    std::fs::write(sandbox.project().join("notes.txt"), "remember the milk\n").unwrap();
    let id = sandbox.bg(&["--mode", "auto", "--name", "night shift"], "do the work");
    let row = sandbox.wait_for_state(&id, "working");
    // The process dies while the call runs.
    let started = Instant::now();
    while !sandbox.session_files().iter().any(|file| {
        std::fs::read_to_string(file)
            .unwrap_or_default()
            .contains("\"type\":\"tool_started\",\"callId\":\"c2\"")
    }) {
        assert!(started.elapsed() < LIMIT, "the call never started");
        std::thread::sleep(Duration::from_millis(25));
    }
    // The log is written beside the session file, a moment later on a busy machine.
    while !text(&sandbox.run(&["logs", &id])).0.contains("→ test_wait") {
        assert!(started.elapsed() < LIMIT, "the call never reached the log");
        std::thread::sleep(Duration::from_millis(25));
    }
    kill_process(row["pid"].as_u64().unwrap());

    // It is still listed, as what it is; the session can be told from one that runs.
    let row = sandbox.wait_for_state(&id, "interrupted");
    assert_eq!(row["name"], "night shift");
    let listed = sandbox.ok(&["ps"]);
    assert!(
        listed.contains("interrupted") && listed.contains("azazel attach <id>"),
        "{listed}"
    );
    assert!(
        text(&sandbox.run(&["logs", &id])).0.contains("→ test_wait"),
        "its log is still there"
    );

    // Attaching starts it again on the same session, in the mode it was in, and it goes on:
    // the read is run again, the other call is not, and the model is told so.
    std::fs::write(
        sandbox.dir.path().join("script.json"),
        json!({ "lead": [{ "reply": { "text": "Picked up where it stopped." } }] }).to_string(),
    )
    .unwrap();
    let mut client = sandbox
        .azazel(&["attach", &id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut typing = client.stdin.take().unwrap();
    let shown = lines_of(client.stdout.take().unwrap());
    let went_on = read_until(&shown, "· done");
    assert_eq!(
        went_on,
        [
            "→ read notes.txt",
            "→ test_wait {\"ms\":600000}",
            "  ✗ test_wait: interrupted: the process stopped before this call finished; check its effects \
             before retrying",
            "Picked up where it stopped.",
            "· done"
        ]
    );
    let row = sandbox.wait_for_state(&id, "idle");
    assert_eq!(row["mode"], "auto", "{row}");
    assert_eq!(sandbox.session_files().len(), 1, "the same session, not another");
    let requests = sandbox.requests();
    let results = &requests.last().unwrap()["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["content"];
    assert_eq!(results[0]["is_error"], false, "{results}");
    assert!(
        results[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("remember the milk"),
        "{results}"
    );
    assert_eq!(results[1]["is_error"], true, "{results}");
    writeln!(typing, "/kill").unwrap();
    read_until(&shown, "the background session has ended");
    assert!(client.wait().unwrap().success());
    assert!(sandbox.ps().is_empty());
}

#[test]
fn a_session_that_died_in_a_run_waits_for_continue_when_it_is_not_to_resume_by_itself() {
    let sandbox =
        Sandbox::new(&json!({ "lead": [{ "delay_ms": 600_000, "reply": { "text": "too late" } }] }));
    let quiet = ["--set", "session.resume_runs=false"];
    let id = sandbox.bg(&quiet, "do the work");
    let other = sandbox.bg(&quiet, "more work");
    let started = Instant::now();
    let asked = |file: &PathBuf| {
        std::fs::read_to_string(file)
            .unwrap_or_default()
            .contains("\"type\":\"request_started\"")
    };
    while !(sandbox.session_files().len() == 2 && sandbox.session_files().iter().all(asked)) {
        assert!(started.elapsed() < LIMIT, "the requests never went out");
        std::thread::sleep(Duration::from_millis(25));
    }
    for row in sandbox.ps() {
        kill_process(row["pid"].as_u64().unwrap());
    }
    sandbox.wait_for_state(&id, "interrupted");
    sandbox.wait_for_state(&other, "interrupted");

    // One of them is forgotten: it leaves the list, and its session file stays.
    assert_eq!(sandbox.ok(&["kill", &other]).trim(), format!("stopped {other}"));
    let listed: Vec<Value> = sandbox.ps();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(sandbox.session_files().len(), 2);

    std::fs::write(
        sandbox.dir.path().join("script.json"),
        json!({ "lead": [{ "reply": { "text": "Here now." } }] }).to_string(),
    )
    .unwrap();
    let mut client = sandbox
        .azazel(&["attach", &id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut typing = client.stdin.take().unwrap();
    let shown = lines_of(client.stdout.take().unwrap());
    // It was started with the options of the one that died: it does nothing by itself.
    sandbox.wait_for_state(&id, "idle");
    assert_eq!(sandbox.requests().len(), 2, "nothing was sent again yet");
    writeln!(typing, "/continue").unwrap();
    let went_on = read_until(&shown, "· done");
    assert_eq!(went_on, ["Here now.", "· done"]);
    assert_eq!(sandbox.requests().len(), 3);
    writeln!(typing, "/kill").unwrap();
    read_until(&shown, "the background session has ended");
    assert!(client.wait().unwrap().success());
    assert!(sandbox.ps().is_empty());
}

/// A program on a background session's socket, speaking the protocol.
struct Bridge {
    process: std::process::Child,
    commands: Option<std::process::ChildStdin>,
    frames: Receiver<String>,
}

impl Bridge {
    /// Connects and attaches; returns the bridge and the data of `attach`.
    fn attach(sandbox: &Sandbox, id: &str, replay: &str) -> (Bridge, Value) {
        Bridge::attach_with(sandbox, id, json!({ "replay": replay }))
    }

    /// Connects and attaches with the members `extra` adds to the command; returns the
    /// bridge and the data of `attach`.
    fn attach_with(sandbox: &Sandbox, id: &str, extra: Value) -> (Bridge, Value) {
        let mut process = sandbox
            .azazel(&["rpc", "--attach", id])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut bridge = Bridge {
            commands: process.stdin.take(),
            frames: lines_of(process.stdout.take().unwrap()),
            process,
        };
        assert_eq!(bridge.next()["type"], "hello");
        let mut attach = json!({ "id": "attach", "type": "attach" });
        for (key, value) in extra.as_object().unwrap() {
            attach[key] = value.clone();
        }
        bridge.send(attach);
        let attached = bridge.until(|frame| frame["id"] == "attach").pop().unwrap();
        assert_eq!(attached["success"], true, "{attached}");
        (bridge, attached["data"].clone())
    }

    fn send(&mut self, command: Value) {
        writeln!(self.commands.as_mut().unwrap(), "{command}").unwrap();
    }

    fn next(&self) -> Value {
        let line = self.frames.recv_timeout(LIMIT).expect("a frame within a minute");
        let frame: Frame = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert_ne!(frame, Frame::Unknown, "{line}");
        serde_json::from_str(&line).unwrap()
    }

    /// Reads up to the first frame `wanted` accepts; returns all of them.
    fn until(&self, wanted: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut read = Vec::new();
        loop {
            let frame = self.next();
            let found = wanted(&frame);
            read.push(frame);
            if found {
                return read;
            }
        }
    }

    fn detach(mut self) {
        drop(self.commands.take());
        assert!(self.process.wait().unwrap().success());
    }
}

/// The events among `frames`, as `type` and what tells two of a type apart: what every client
/// of a session is sent alike. Responses and what answers one client only are left out.
fn events(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .filter(|frame| {
            !matches!(
                frame["type"].as_str(),
                Some("response" | "request_closed" | "user_input" | "roster")
            )
        })
        .map(|frame| {
            let what = frame["call_id"]
                .as_str()
                .or(frame["event"]["delta"].as_str())
                .or(frame["stop"].as_str())
                .unwrap_or_default();
            format!("{} {what}", frame["type"].as_str().unwrap())
                .trim()
                .to_string()
        })
        .collect()
}

#[test]
fn two_clients_of_one_session_see_the_same_and_either_answers_an_approval() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/scripts/write_read.json");
    let mut script: Value = serde_json::from_str(&std::fs::read_to_string(script).unwrap()).unwrap();
    script["lead"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "reply": { "text": "And once more." } }));
    let sandbox = Sandbox::new(&script);
    let id = sandbox.bg(&[], "create hello");
    sandbox.wait_for_state(&id, "waiting for you");

    // Both are told where the session stands: the lead works, a call runs, a request waits.
    let (mut one, first) = Bridge::attach(&sandbox, &id, "none");
    let (mut two, second) = Bridge::attach(&sandbox, &id, "none");
    assert_eq!((&first["clients"], &second["clients"]), (&json!(1), &json!(2)));
    for attached in [&first, &second] {
        let snapshot = &attached["snapshot"];
        assert_eq!(snapshot["busy"], true, "{snapshot}");
        assert_eq!(snapshot["tools"][0]["name"], "write", "{snapshot}");
        assert_eq!(snapshot["tools"][0]["call_id"], "c1", "{snapshot}");
        assert_eq!(snapshot["pending"].as_array().unwrap().len(), 1, "{snapshot}");
        // The conversation so far: the prompt, and the reply that asks for the call.
        let tail = snapshot["tail"].as_array().unwrap();
        assert_eq!(tail.len(), 2, "{snapshot}");
        assert_eq!(tail[0]["message"]["content"][0]["text"], "create hello");
    }
    // And both are shown the request.
    let asked_one = one
        .until(|frame| frame["type"] == "approval_request")
        .pop()
        .unwrap();
    let asked_two = two
        .until(|frame| frame["type"] == "approval_request")
        .pop()
        .unwrap();
    assert_eq!(asked_one, asked_two);
    let request_id = asked_one["request_id"].as_str().unwrap().to_string();
    assert_eq!(first["snapshot"]["pending"][0], json!(request_id));

    // The second answers. Both number their commands alike; each gets its own answer.
    two.send(json!({ "id": 1, "type": "approval_response", "request_id": request_id, "decision": "allow" }));
    one.send(json!({ "id": 1, "type": "get_state" }));
    let seen_one = one.until(|frame| frame["type"] == "idle");
    let seen_two = two.until(|frame| frame["type"] == "idle");
    let answer_one = seen_one.iter().find(|frame| frame["id"] == 1).unwrap();
    let answer_two = seen_two.iter().find(|frame| frame["id"] == 1).unwrap();
    assert!(answer_one["data"]["session_id"].is_string(), "{answer_one}");
    assert_eq!(answer_two["data"], Value::Null, "{answer_two}");
    // The request closed on the client that did not answer it, and only there.
    let closed = |frames: &[Value]| {
        frames
            .iter()
            .filter(|frame| frame["type"] == "request_closed")
            .map(|frame| frame["request_id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(closed(&seen_one), std::slice::from_ref(&request_id));
    assert!(closed(&seen_two).is_empty());
    // From there to the end of the run both were sent the same.
    assert_eq!(events(&seen_one), events(&seen_two));
    assert!(
        events(&seen_one).contains(&"tool_end c1".to_string()),
        "{seen_one:?}"
    );
    assert_eq!(
        std::fs::read_to_string(sandbox.project().join("hello.txt")).unwrap(),
        "hello from azazel\n"
    );
    // An answer that comes late is told so.
    one.send(json!({ "id": 2, "type": "approval_response", "request_id": request_id, "decision": "deny" }));
    let late = one.until(|frame| frame["id"] == 2).pop().unwrap();
    assert_eq!(late["success"], false, "{late}");

    // Either may prompt: the other reads what was typed, the one that typed does not.
    one.send(json!({ "id": 3, "type": "prompt", "text": "once more" }));
    let seen_one = one.until(|frame| frame["type"] == "idle");
    let seen_two = two.until(|frame| frame["type"] == "idle");
    let typed = |frames: &[Value]| {
        frames
            .iter()
            .filter(|frame| frame["type"] == "user_input")
            .map(|frame| frame["text"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert!(typed(&seen_one).is_empty(), "{seen_one:?}");
    assert_eq!(typed(&seen_two), ["once more"]);
    assert_eq!(events(&seen_one), events(&seen_two));

    // A third client that comes while two are there is replayed nothing it need not see.
    let (three, third) = Bridge::attach(&sandbox, &id, "missed");
    assert_eq!(third["clients"], 3);
    assert_eq!(third["snapshot"]["busy"], false);
    one.detach();
    two.detach();
    three.detach();
    sandbox.wait_for_state(&id, "idle");
    assert_eq!(sandbox.ok(&["kill", &id]).trim(), format!("stopped {id}"));
}

/// The `at` of every frame that has one.
fn cursors(frames: &[Value]) -> Vec<u64> {
    frames.iter().filter_map(|frame| frame["at"].as_u64()).collect()
}

/// A background session whose run waits for an approval, and the script that finishes it.
fn session_waiting_for_approval() -> (Sandbox, String) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/scripts/write_read.json");
    let script: Value = serde_json::from_str(&std::fs::read_to_string(script).unwrap()).unwrap();
    let sandbox = Sandbox::new(&script);
    let id = sandbox.bg(&[], "create hello");
    sandbox.wait_for_state(&id, "waiting for you");
    (sandbox, id)
}

#[test]
fn a_client_that_dropped_is_sent_the_frames_after_its_last_one_and_no_other() {
    let (sandbox, id) = session_waiting_for_approval();
    let (mut stays, attached) = Bridge::attach(&sandbox, &id, "all");
    let (drops, _) = Bridge::attach(&sandbox, &id, "all");
    let begin = attached["cursor"].as_u64().unwrap();
    let seen = drops.until(|frame| frame["type"] == "approval_request");
    let request = seen.last().unwrap().clone();
    stays.until(|frame| frame["type"] == "approval_request");
    // Every frame of the log says where it is; the order is the log's.
    let first = cursors(&seen);
    assert!(
        first.len() > 2 && first.windows(2).all(|pair| pair[0] < pair[1]),
        "{seen:?}"
    );
    assert!(first.iter().all(|at| *at < begin), "{first:?} before {begin}");
    let last_seen = *first.last().unwrap();
    drops.detach();

    // The run goes on while it is away.
    let request_id = request["request_id"].as_str().unwrap();
    stays
        .send(json!({ "id": 1, "type": "approval_response", "request_id": request_id, "decision": "allow" }));
    let live = stays.until(|frame| frame["type"] == "idle");
    let missed: Vec<Value> = live
        .iter()
        .filter(|frame| frame["at"].is_u64())
        .cloned()
        .collect();
    assert!(missed.len() > 2, "{live:?}");

    let (back, resumed) = Bridge::attach_with(&sandbox, &id, json!({ "replay": "none", "since": last_seen }));
    assert_eq!(resumed["gap"], Value::Null, "{resumed}");
    assert!(resumed["cursor"].as_u64().unwrap() > *cursors(&missed).last().unwrap());
    let replayed = back.until(|frame| frame["type"] == "idle");
    // Exactly what came after `last_seen`: nothing before it, nothing twice, nothing missing.
    assert_eq!(cursors(&replayed), cursors(&missed), "{replayed:?}");
    assert_eq!(events(&replayed), events(&missed));
    assert!(
        replayed.iter().all(|frame| frame["type"] != "approval_request"),
        "the request was answered: {replayed:?}"
    );
    back.detach();
    stays.detach();
    sandbox.wait_for_state(&id, "idle");
}

#[test]
fn a_since_the_log_does_not_hold_is_a_gap() {
    let (sandbox, id) = session_waiting_for_approval();
    let (first, attached) = Bridge::attach(&sandbox, &id, "all");
    let cursor = attached["cursor"].as_u64().unwrap();
    first.detach();
    // Past the end of the log, and in the middle of a line.
    for since in [cursor + 1000, 1] {
        let (bridge, attached) = Bridge::attach_with(&sandbox, &id, json!({ "since": since }));
        assert_eq!(attached["gap"], true, "since {since}: {attached}");
        assert_eq!(attached["cursor"], cursor, "{attached}");
        // The client is still told what waits for an answer.
        bridge.until(|frame| frame["type"] == "approval_request");
        bridge.detach();
    }
    // Resuming at a frame the log holds is no gap.
    let (bridge, _) = Bridge::attach(&sandbox, &id, "all");
    let last = cursors(&bridge.until(|frame| frame["type"] == "approval_request"));
    bridge.detach();
    let (bridge, attached) = Bridge::attach_with(&sandbox, &id, json!({ "since": last[last.len() - 2] }));
    assert_eq!(attached["gap"], Value::Null, "{attached}");
    bridge.until(|frame| frame["type"] == "approval_request");
    bridge.detach();
    assert_eq!(sandbox.ok(&["kill", &id]).trim(), format!("stopped {id}"));
}

#[test]
fn a_named_bg_sent_again_starts_no_second_session() {
    let sandbox = Sandbox::new(&json!({ "lead": [{ "reply": { "text": "Done." } }] }));
    let named = ["bg", "--request-id", "nightly-1", "do the nightly work"];
    let first = sandbox.ok(&named).trim().to_string();
    let again = sandbox.run(&named);
    let (out, err) = text(&again);
    assert!(again.status.success(), "{err}");
    assert_eq!(out.trim(), first);
    assert!(err.contains("nothing was started"), "{err}");
    let ids: Vec<String> = sandbox
        .ps()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, std::slice::from_ref(&first));
    // Another name is another session.
    let other = sandbox.ok(&["bg", "--request-id", "nightly-2", "do the nightly work"]);
    assert_ne!(other.trim(), first);
    // The name went with the prompt into the session: the run it started can be asked for.
    sandbox.wait_for_state(&first, "idle");
    let (mut bridge, _) = Bridge::attach(&sandbox, &first, "none");
    bridge
        .send(json!({ "id": 1, "type": "prompt", "text": "do the nightly work", "request_id": "nightly-1" }));
    let seen = bridge.until(|frame| frame["id"] == 1).pop().unwrap();
    assert_eq!(
        (
            &seen["data"]["duplicate"],
            &seen["data"]["state"],
            &seen["data"]["text"]
        ),
        (&json!(true), &json!("ended"), &json!("Done.")),
        "{seen}"
    );
    bridge.detach();
}

/// Runs `azazel bg` from a parent that ends right after it, the way a shell command does, and
/// returns the id it printed. On Windows the parent runs in a job that kills what is left in it
/// when it ends, as a terminal or a CI runner may run its commands, and allows leaving it.
fn bg_from_a_short_lived_parent(sandbox: &Sandbox, prompt: &str) -> String {
    let started = sandbox.azazel(&["bg", prompt]);
    let program = started.get_program().to_owned();
    let args: Vec<_> = started.get_args().map(|a| a.to_owned()).collect();
    #[cfg(windows)]
    let mut parent = {
        let script = sandbox.dir.path().join("parent.ps1");
        std::fs::write(&script, PARENT_IN_A_JOB).unwrap();
        let mut parent = Command::new("powershell");
        parent
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(script);
        parent
    };
    #[cfg(not(windows))]
    let mut parent = {
        let mut parent = Command::new("sh");
        parent.args(["-c", "\"$@\""]).arg("sh");
        parent
    };
    parent.arg(&program).args(&args).stdin(Stdio::null());
    for (name, value) in started.get_envs() {
        if let Some(value) = value {
            parent.env(name, value);
        }
    }
    let output = parent.output().unwrap();
    let (out, err) = text(&output);
    assert!(output.status.success(), "{out}\n{err}");
    let id = out.lines().next().unwrap_or_default().trim().to_string();
    assert!(
        id.len() == 8 && id.chars().all(|c| c.is_ascii_hexdigit()),
        "an id: {out:?} {err:?}"
    );
    id
}

/// Puts this process in a job that ends what is in it when the job is closed and that lets a
/// process created with `CREATE_BREAKAWAY_FROM_JOB` leave; runs the command; closes the job.
#[cfg(windows)]
const PARENT_IN_A_JOB: &str = r#"
Add-Type @"
using System; using System.Runtime.InteropServices;
public class Job {
  [DllImport("kernel32")] static extern IntPtr CreateJobObjectW(IntPtr a, string n);
  [DllImport("kernel32")] static extern bool SetInformationJobObject(IntPtr j, int c, IntPtr i, uint l);
  [DllImport("kernel32")] static extern bool AssignProcessToJobObject(IntPtr j, IntPtr p);
  [DllImport("kernel32")] static extern bool CloseHandle(IntPtr h);
  static IntPtr job;
  public static void Enter() {
    job = CreateJobObjectW(IntPtr.Zero, null);
    IntPtr info = Marshal.AllocHGlobal(144);
    for (int i = 0; i < 144; i++) Marshal.WriteByte(info, i, 0);
    // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK
    Marshal.WriteInt32(info, 16, 0x2800);
    if (!SetInformationJobObject(job, 9, info, 144)) throw new Exception("SetInformationJobObject");
    if (!AssignProcessToJobObject(job, System.Diagnostics.Process.GetCurrentProcess().Handle)) throw new Exception("AssignProcessToJobObject");
  }
  public static void Leave() { CloseHandle(job); }
}
"@
[Job]::Enter()
& $args[0] $args[1..($args.Length - 1)]
[Job]::Leave()
"#;

#[test]
fn a_background_session_outlives_the_command_that_started_it() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/scripts/write_read.json");
    let script: Value = serde_json::from_str(&std::fs::read_to_string(script).unwrap()).unwrap();
    let sandbox = Sandbox::new(&script);
    let id = bg_from_a_short_lived_parent(&sandbox, "create hello");

    // The parent has ended; the session is still there and still waits at the approval.
    sandbox.wait_for_state(&id, "waiting for you");
    let mut client = sandbox
        .azazel(&["attach", &id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut typing = client.stdin.take().unwrap();
    let shown = lines_of(client.stdout.take().unwrap());
    read_until(&shown, "/approve");
    writeln!(typing, "/detach").unwrap();
    read_until(&shown, "detached");
    assert!(client.wait().unwrap().success());
    assert_eq!(sandbox.ps()[0]["state"], "waiting for you");
}

/// Plain `azazel` with `args` in a terminal, as the interactive UI runs. Returns what it showed up
/// to the first output that contains `wanted`, and the process, still running.
fn interactive(
    sandbox: &Sandbox,
    args: &[&str],
    wanted: &str,
) -> (String, Box<dyn portable_pty::Child + Send + Sync>) {
    interactive_in(sandbox, &sandbox.project(), args, wanted)
}

/// The same started in `cwd`, a folder of the project or the project itself.
fn interactive_in(
    sandbox: &Sandbox,
    cwd: &Path,
    args: &[&str],
    wanted: &str,
) -> (String, Box<dyn portable_pty::Child + Send + Sync>) {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    // The interactive UI needs a terminal.
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(assert_cmd::cargo::cargo_bin("azazel"));
    command.env("AZAZEL_HOME", sandbox.home());
    command.env("AZAZEL_MOCK_SCRIPT", sandbox.dir.path().join("script.json"));
    command.args(["--set", "session.auto_name=false", "--no-memory"]);
    command.args(["--set", "notify.desktop=\"off\""]);
    command.args(["--set", "session.resume=\"last\""]);
    command.arg("--cwd");
    command.arg(cwd);
    command.args(args);
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    // Kept open: a terminal whose input closes ends the program on some systems.
    let writer = pair.master.take_writer().unwrap();
    let writer = std::sync::Arc::new(std::sync::Mutex::new((writer, pair.master)));
    let (sender, received) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        while let Ok(read) = std::io::Read::read(&mut reader, &mut buffer) {
            if read == 0
                || sender
                    .send(String::from_utf8_lossy(&buffer[..read]).into_owned())
                    .is_err()
            {
                break;
            }
        }
    });
    let started = Instant::now();
    let mut shown = String::new();
    while !shown.contains(wanted) {
        let left = LIMIT.saturating_sub(started.elapsed());
        match received.recv_timeout(left) {
            Ok(more) => {
                // The terminal is asked where its cursor is (Windows' console asks first) and
                // answers as a real one would.
                for _ in more.matches("[6n") {
                    let mut held = writer.lock().unwrap();
                    held.0.write_all(b"[1;1R").unwrap();
                    held.0.flush().unwrap();
                }
                shown.push_str(&more);
            }
            Err(_) => {
                let _ = child.kill();
                panic!("azazel never said `{wanted}`: {shown:?}");
            }
        }
    }
    // The terminal stays open for as long as the process does.
    std::mem::forget(writer);
    (shown, child)
}

/// Plain `azazel` in a folder whose latest session runs in the background (one a browser started
/// through `azazel remote`, say) goes to it as `azazel attach` does, instead of refusing; the
/// prompt on the command line is sent to it.
#[test]
fn the_interactive_ui_attaches_to_the_latest_session_when_it_runs_in_the_background() {
    let sandbox = Sandbox::new(&json!({ "lead": [
        { "reply": { "text": "Done." } },
        { "reply": { "text": "Fixed." } }
    ] }));
    let id = sandbox.bg(&[], "do it");
    sandbox.wait_for_state(&id, "idle");

    // The full UI on the background session: its header names it. The console may put its own
    // sequences between the words of a line, so the notice is matched on its own.
    let wanted = format!("background session {id}");
    let (shown, mut child) = interactive(&sandbox, &["fix it"], &wanted);
    assert!(shown.contains("attaching to"), "{shown:?}");
    assert!(
        !shown.contains("this session is running in the background: azazel attach"),
        "{shown:?}"
    );
    // The prompt of the command line reached the session that runs.
    let started = Instant::now();
    loop {
        let log = sandbox.ok(&["logs", &id]);
        if log.contains("fix it") && log.contains("Fixed.") {
            break;
        }
        assert!(started.elapsed() < LIMIT, "the prompt never ran: {log}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    // Attaching left the session where it runs, and opened no other.
    assert_eq!(sandbox.ps().len(), 1);
    assert_eq!(sandbox.session_files().len(), 1);
}

/// A session belongs to the folder it was started in: plain `azazel` in a folder of the project
/// does not attach to the background session of the project root, and starts a session of its own.
#[test]
fn the_interactive_ui_in_a_sub_folder_does_not_attach_to_the_roots_background_session() {
    let sandbox = Sandbox::new(&json!({ "lead": [{ "reply": { "text": "Done." } }] }));
    let id = sandbox.bg(&[], "do it");
    sandbox.wait_for_state(&id, "idle");
    let web = sandbox.project().join("web");
    std::fs::create_dir_all(&web).unwrap();

    // A new session opens, as the status line's model shows; `attaching to` never does.
    let (shown, mut child) = interactive_in(&sandbox, &web, &[], "mock/lead");
    assert!(!shown.contains("attaching to"), "{shown:?}");
    assert!(!shown.contains(&format!("background session {id}")), "{shown:?}");
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(sandbox.ps().len(), 1);
}

/// What only shapes a new session cannot apply to one that is running: each flag is refused with
/// its name and what to do instead, not dropped.
#[test]
fn options_that_cannot_apply_to_a_running_session_are_refused_when_attaching() {
    let sandbox = Sandbox::new(&json!({ "lead": [{ "reply": { "text": "Done." } }] }));
    let id = sandbox.bg(&[], "do it");
    sandbox.wait_for_state(&id, "idle");

    for (args, flag, instead) in [
        (&["-c", "--model", "mock/lead"][..], "--model", "/model"),
        (&["-c", "--thinking", "high"][..], "--thinking", "/thinking"),
        (&["-c", "--mode", "auto"][..], "--mode", "/mode"),
        (&["-c", "--name", "x"][..], "--name", "/name"),
        (&["-c", "--continue-run"][..], "--continue-run", "/continue"),
        (&["open"][..], "azazel open", "/open"),
    ] {
        // The terminal wraps long lines: the end of the message is what is waited for.
        let (shown, mut child) = interactive(&sandbox, args, "to end it first");
        let status = child.wait().unwrap();
        assert_eq!(status.exit_code(), 2, "{args:?}: {shown:?}");
        let flat: String = shown
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '')
            .collect();
        for part in [
            flag,
            &format!("azazel attach {id}"),
            &format!("azazel kill {id}"),
            instead,
        ] {
            let part: String = part.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(flat.contains(&part), "{args:?}: missing {part}: {shown:?}");
        }
    }
    // Nothing was started or changed.
    assert_eq!(sandbox.ps().len(), 1);
    assert_eq!(sandbox.session_files().len(), 1);
}

/// A client that never used the browser is not told what it does: its address and title are not
/// for a device that may not watch (the server lets a browser command through only to one that
/// may).
#[cfg(feature = "browser")]
#[test]
fn a_client_that_never_asked_for_the_browser_is_told_nothing_of_it() {
    let (sandbox, id) = session_waiting_for_approval();
    let (mut bystander, _) = Bridge::attach(&sandbox, &id, "none");
    let (mut user, _) = Bridge::attach(&sandbox, &id, "none");
    user.send(json!({ "id": "w", "type": "browser_watch", "on": true }));
    user.until(|frame| frame["type"] == "browser_state");
    bystander.send(json!({ "id": "g", "type": "get_state" }));
    let seen = bystander.until(|frame| frame["id"] == "g");
    assert!(
        seen.iter().all(|frame| frame["type"] != "browser_state"),
        "{seen:?}"
    );
    bystander.detach();
    user.detach();
}

/// The agents' browser is shared by the clients of a session, but there is nothing to take over
/// until the agent has opened a page: every client is refused, and what the clients send is never
/// logged or replayed.
#[cfg(feature = "browser")]
#[test]
fn the_browser_cannot_be_taken_before_the_agent_opens_a_page_and_is_never_logged() {
    let (sandbox, id) = session_waiting_for_approval();
    let (mut one, _) = Bridge::attach(&sandbox, &id, "none");
    let (mut two, _) = Bridge::attach(&sandbox, &id, "none");

    one.send(json!({ "id": "w", "type": "browser_watch", "on": true }));
    let watching = one.until(|frame| frame["id"] == "w").pop().unwrap();
    assert_eq!(watching["success"], true, "{watching}");
    assert_eq!(watching["data"]["running"], false, "{watching}");
    two.send(json!({ "id": "w", "type": "browser_watch", "on": true }));
    let watching = two.until(|frame| frame["id"] == "w").pop().unwrap();
    assert_eq!(watching["success"], true, "{watching}");

    for client in [&mut one, &mut two] {
        client.send(json!({ "id": "t", "type": "browser_take", "on": true, "force": true }));
        let refused = client.until(|frame| frame["id"] == "t").pop().unwrap();
        assert_eq!(
            refused["error"], "no browser is running; the agent has not opened a page yet",
            "{refused}"
        );
        client.send(json!({ "id": "i", "type": "browser_input", "input": { "kind": "back" } }));
        let refused = client.until(|frame| frame["id"] == "i").pop().unwrap();
        assert_eq!(refused["error"], "take the browser over first", "{refused}");
    }
    one.detach();

    // A client that comes late is replayed the whole log, and finds no browser frame in it.
    let (late, _) = Bridge::attach(&sandbox, &id, "all");
    let replayed = late.until(|frame| frame["type"] == "approval_request");
    assert!(
        replayed
            .iter()
            .all(|frame| !frame["type"].as_str().unwrap().starts_with("browser_")),
        "{replayed:?}"
    );
    let log = std::fs::read_to_string(sandbox.run_file(&format!("{id}.log"))).unwrap();
    assert!(!log.contains("browser_"), "the log holds browser frames");
    late.detach();
    two.detach();
}
