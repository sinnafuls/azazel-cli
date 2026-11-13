#![allow(clippy::unwrap_used)]
#![cfg(feature = "detach")]

//! Background sessions: `azazel bg`, `ps`, `logs`, `attach`, `kill`, the lock on their session
//! and `/bg`, against the real binary and the scripted provider.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

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

/// The lines a process writes, as they come.
fn lines_of(reader: impl std::io::Read + Send + 'static) -> Receiver<String> {
    let (sender, lines) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    lines
}

/// Reads lines up to the first that contains `wanted`; returns all of them.
fn read_until(lines: &Receiver<String>, wanted: &str) -> Vec<String> {
    let mut read = Vec::new();
    loop {
        let line = lines
            .recv_timeout(LIMIT)
            .unwrap_or_else(|_| panic!("no line with `{wanted}` after {read:?}"));
        let found = line.contains(wanted);
        read.push(line);
        if found {
            return read;
        }
    }
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

/// Ends a process the hard way, with whatever it started: nothing of it gets to clean up.
fn kill_process(pid: u64) {
    #[cfg(windows)]
    let killed = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .output();
    #[cfg(unix)]
    let killed = Command::new("kill").args(["-9", &pid.to_string()]).output();
    assert!(killed.unwrap().status.success(), "could not kill {pid}");
}

#[test]
fn a_background_session_killed_in_a_tool_call_is_listed_and_goes_on_when_attached() {
    // A read, then a command that outlives the test.
    let sandbox = Sandbox::new(&json!({ "lead": [{ "reply": { "tool_calls": [
        { "id": "c1", "name": "read", "input": { "path": "notes.txt" } },
        { "id": "c2", "name": "shell", "input": { "command": "sleep 45" } },
    ] } }] }));
    std::fs::write(sandbox.project().join("notes.txt"), "remember the milk\n").unwrap();
    let id = sandbox.bg(&["--mode", "auto", "--name", "night shift"], "do the work");
    let row = sandbox.wait_for_state(&id, "working");
    // The process dies while the command runs.
    let started = Instant::now();
    while !sandbox.session_files().iter().any(|file| {
        std::fs::read_to_string(file)
            .unwrap_or_default()
            .contains("\"type\":\"tool_started\",\"callId\":\"c2\"")
    }) {
        assert!(started.elapsed() < LIMIT, "the command never started");
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
        text(&sandbox.run(&["logs", &id])).0.contains("→ shell sleep 45"),
        "its log is still there"
    );

    // Attaching starts it again on the same session, in the mode it was in, and it goes on:
    // the read is run again, the command is not, and the model is told so.
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
            "→ shell sleep 45",
            "  ✗ shell: interrupted: the process stopped before this call finished; check its effects \
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
        bridge.send(json!({ "id": "attach", "type": "attach", "replay": replay }));
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
