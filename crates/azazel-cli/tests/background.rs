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

    // One client at a time.
    let second = sandbox.run(&["attach", &id]);
    let (_, why) = text(&second);
    assert_eq!(second.status.code(), Some(1), "{why}");
    assert!(
        why.contains(&format!(
            "another client is attached to this session (process {})",
            first.id()
        )),
        "{why}"
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
