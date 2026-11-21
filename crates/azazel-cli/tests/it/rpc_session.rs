//! The session commands of RPC mode: moving between sessions and within one, compaction, shell
//! lines, export, side questions and registered commands.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use azazel_protocol::{Frame, SessionRow, State, TreeRow};
use serde_json::{Value, json};

/// An `azazel rpc` process answering from a script, and the lines it writes.
struct Client {
    dir: tempfile::TempDir,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
}

impl Client {
    fn start(script: &Value, args: &[&str]) -> Client {
        Client::start_in(script, args, |_| {})
    }

    /// Like [`Client::start`], after `prepare` has put what the test needs into the azazel home.
    fn start_in(script: &Value, args: &[&str], prepare: impl FnOnce(&Path)) -> Client {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        prepare(&dir.path().join("home"));
        let script_path = dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let mut child = Command::new(assert_cmd::cargo::cargo_bin("azazel"))
            .env("AZAZEL_HOME", dir.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", &script_path)
            .env("AZAZEL_MOCK_REQUESTS", dir.path().join("requests.jsonl"))
            // The namer would take a reply of the script for itself.
            .args(["--set", "session.auto_name=false"])
            .args(args)
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
            next_id: 0,
        }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    /// Every request the scripted provider received, in order.
    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
            .collect()
    }

    /// The next line, which must be a frame of the protocol.
    fn next(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(60))
            .expect("a protocol line within a minute");
        let value: Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("not a protocol line ({e}): {line}"));
        let frame: Frame = serde_json::from_value(value.clone()).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert_ne!(frame, Frame::Unknown, "{line}");
        value
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

    /// Sends a command; returns the lines that came before its response, and the response.
    fn ask(&mut self, mut command: Value) -> (Vec<Value>, Value) {
        self.next_id += 1;
        let id = self.next_id;
        command["id"] = id.into();
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{command}").unwrap();
        stdin.flush().unwrap();
        self.until(|line| line["type"] == "response" && line["id"] == id)
    }

    /// Sends a command without waiting for its response; returns its id.
    fn send(&mut self, mut command: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        command["id"] = id.into();
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{command}").unwrap();
        stdin.flush().unwrap();
        id
    }

    /// The response to a command sent with [`Client::send`].
    fn response(&self, id: u64) -> Value {
        self.until(|line| line["type"] == "response" && line["id"] == id)
            .1
    }

    /// The data of a command that must succeed.
    fn ok(&mut self, command: Value) -> Value {
        let (_, response) = self.ask(command.clone());
        assert_eq!(response["success"], true, "{command}: {response}");
        response["data"].clone()
    }

    /// The error of a command that must fail.
    fn error(&mut self, command: Value) -> String {
        let (_, response) = self.ask(command.clone());
        assert_eq!(response["success"], false, "{command}: {response}");
        response["error"].as_str().unwrap().to_string()
    }

    fn state(&mut self, command: Value) -> State {
        serde_json::from_value(self.ok(command)).unwrap()
    }

    /// Sends a prompt and reads to the end of its run.
    fn prompt(&mut self, text: &str) -> Vec<Value> {
        self.ok(json!({ "type": "prompt", "text": text }));
        let (events, idle) = self.until(|line| line["type"] == "idle" || line["type"] == "failed");
        assert_eq!(idle["type"], "idle", "{idle}");
        events
    }

    /// The branch's messages as `(role, text)`.
    fn messages(&mut self) -> Vec<(String, String)> {
        self.ok(json!({ "type": "get_messages" }))
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let message = &row["message"];
                let text: String = message["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .collect();
                (message["role"].as_str().unwrap().to_string(), text)
            })
            .collect()
    }

    fn tree(&mut self) -> Vec<TreeRow> {
        serde_json::from_value(self.ok(json!({ "type": "get_tree" }))).unwrap()
    }

    /// Closes standard input and returns the exit code.
    fn finish(mut self) -> Option<i32> {
        drop(self.stdin.take());
        self.child.wait().unwrap().code()
    }
}

fn replies(texts: &[&str]) -> Value {
    let steps: Vec<Value> = texts
        .iter()
        .map(|text| json!({ "reply": { "text": text } }))
        .collect();
    json!({ "lead": steps })
}

fn pair(role: &str, text: &str) -> (String, String) {
    (role.to_string(), text.to_string())
}

#[test]
fn sessions_are_started_listed_resumed_and_forked() {
    let mut client = Client::start(&replies(&["one", "two", "three"]), &[]);
    let first = client.state(json!({ "type": "get_state" }));
    client.prompt("first");
    client.ok(json!({ "type": "set_name", "name": " alpha " }));

    let second = client.state(json!({ "type": "new_session", "name": "beta" }));
    assert_ne!(second.session_id, first.session_id);
    assert_eq!((second.name.as_deref(), second.busy), (Some("beta"), false));
    assert_eq!(client.messages(), []);
    client.prompt("second");

    let rows: Vec<SessionRow> =
        serde_json::from_value(client.ok(json!({ "type": "list_sessions" }))).unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    let alpha = rows.iter().find(|row| row.id == first.session_id).unwrap();
    assert_eq!(
        (
            alpha.name.as_deref(),
            alpha.first_prompt.as_deref(),
            alpha.prompts
        ),
        (Some("alpha"), Some("first"), 1)
    );
    assert!(std::path::Path::new(&alpha.path).is_file());
    let found: Vec<SessionRow> =
        serde_json::from_value(client.ok(json!({ "type": "list_sessions", "query": "TWO" }))).unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        (found[0].id.as_str(), found[0].matches.as_slice()),
        (second.session_id.as_str(), &["two".to_string()][..])
    );

    // Back to the first session, by a prefix of its id, then by its file.
    let resumed = client.state(json!({ "type": "resume_session", "session_id": &first.session_id[..13] }));
    assert_eq!(
        (resumed.session_id.as_str(), resumed.name.as_deref()),
        (first.session_id.as_str(), Some("alpha"))
    );
    assert_eq!(
        client.messages(),
        [pair("user", "first"), pair("assistant", "one")]
    );
    let by_path = client.state(json!({ "type": "resume_session", "path": alpha.path }));
    assert_eq!(by_path.session_id, first.session_id);
    assert!(
        client
            .error(json!({ "type": "resume_session", "session_id": "ffffffff" }))
            .starts_with("no session with id `ffffffff`")
    );
    assert_eq!(
        client.error(json!({ "type": "resume_session" })),
        "`session_id` or `path` must name the session to resume"
    );

    // A fork is a copy that goes its own way.
    let fork = client.state(json!({ "type": "fork_session" }));
    assert!(fork.session_id != first.session_id && fork.session_id != second.session_id);
    assert_eq!(
        client.messages(),
        [pair("user", "first"), pair("assistant", "one")]
    );
    client.prompt("third");
    assert_eq!(client.messages().len(), 4);
    let last = client.requests().pop().unwrap();
    let sent: Vec<&str> = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"][0]["text"].as_str().unwrap())
        .collect();
    assert_eq!(sent, ["first", "one", "third"]);
    client.state(json!({ "type": "resume_session", "session_id": first.session_id }));
    assert_eq!(client.messages().len(), 2);
    assert_eq!(
        client.error(json!({ "type": "fork_session", "entry_id": "nope" })),
        "no entry `nope` in this session"
    );
    let rows = client.ok(json!({ "type": "list_sessions" }));
    assert_eq!(rows.as_array().unwrap().len(), 3);

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn the_tree_is_read_moved_in_and_the_last_prompt_sent_again() {
    let mut client = Client::start(&replies(&["one", "two", "two again"]), &[]);
    assert_eq!(
        client.error(json!({ "type": "retry" })),
        "there is no prompt to send again"
    );
    client.prompt("first");
    client.prompt("second");
    let before = client.tree();
    assert!(before.iter().all(|row| row.current), "{before:?}");
    let shown: Vec<(&str, &str)> = before
        .iter()
        .filter(|row| row.kind == "message")
        .map(|row| (row.role.as_deref().unwrap(), row.text.as_deref().unwrap()))
        .collect();
    assert_eq!(
        shown,
        [
            ("user", "first"),
            ("assistant", "one"),
            ("user", "second"),
            ("assistant", "two")
        ]
    );

    // The earlier attempt stays in the file as a branch.
    client.ok(json!({ "type": "retry" }));
    client.until(|line| line["type"] == "idle");
    assert_eq!(
        client.messages(),
        [
            pair("user", "first"),
            pair("assistant", "one"),
            pair("user", "second"),
            pair("assistant", "two again")
        ]
    );
    let after = client.tree();
    let left_behind: Vec<&str> = after
        .iter()
        .filter(|row| !row.current && row.kind == "message")
        .map(|row| row.text.as_deref().unwrap())
        .collect();
    assert_eq!(left_behind, ["second", "two"]);

    // Continuing from an earlier reply.
    let one = after
        .iter()
        .find(|row| row.text.as_deref() == Some("one"))
        .unwrap();
    let state = client.state(json!({ "type": "set_leaf", "entry_id": one.id }));
    assert!(!state.busy);
    assert_eq!(
        client.messages(),
        [pair("user", "first"), pair("assistant", "one")]
    );
    assert_eq!(
        client.error(json!({ "type": "set_leaf", "entry_id": "nope" })),
        "no entry `nope` in this session"
    );
    let upto = client.ok(json!({ "type": "get_messages", "leaf": after.last().unwrap().id }));
    assert_eq!(upto.as_array().unwrap().len(), 4);

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn messages_are_read_in_pages_from_the_end_of_the_branch() {
    let mut client = Client::start(&replies(&["one", "two", "three"]), &[]);
    for prompt in ["a", "b", "c"] {
        client.prompt(prompt);
    }
    let rows = client.ok(json!({ "type": "get_messages" }));
    let ids: Vec<String> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["entry_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 6);
    let texts = |rows: &Value| -> Vec<String> {
        rows.as_array()
            .unwrap()
            .iter()
            .map(|row| row["message"]["content"][0]["text"].as_str().unwrap().to_string())
            .collect()
    };

    // The last page, oldest first; then the one before it, found through its first row.
    let last = client.ok(json!({ "type": "get_messages", "limit": 2 }));
    assert_eq!(texts(&last), ["c", "three"]);
    let before = last[0]["entry_id"].as_str().unwrap();
    let middle = client.ok(json!({ "type": "get_messages", "before": before, "limit": 2 }));
    assert_eq!(texts(&middle), ["b", "two"]);
    // A page that reaches past the start is what is left.
    let first = client.ok(json!({ "type": "get_messages", "before": middle[0]["entry_id"], "limit": 5 }));
    assert_eq!(texts(&first), ["a", "one"]);
    // Without a limit, `before` gives everything up to it.
    let upto = client.ok(json!({ "type": "get_messages", "before": ids[3] }));
    assert_eq!(texts(&upto), ["a", "one", "b"]);
    assert_eq!(
        client.error(json!({ "type": "get_messages", "before": "nope" })),
        "no message `nope` on this branch"
    );
    assert_eq!(client.finish(), Some(0));
}

#[test]
fn the_messages_of_a_sub_agent_are_read_from_its_transcript() {
    let script: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture("mention.json")).unwrap()).unwrap();
    let mut client = Client::start(&script, &["--set", "roles.smol=[\"mock/scout\"]"]);
    assert_eq!(
        client.error(json!({ "type": "get_agent_messages", "agent": "scout-1" })),
        "no agent `scout-1` in this session"
    );
    client.ok(json!({ "type": "agent_mention", "name": "scout", "text": "where is the retry policy?" }));
    client.until(|line| line["type"] == "idle" && line["agent"] == "lead");

    let rows = client.ok(json!({ "type": "get_agent_messages", "agent": "scout-1" }));
    let said: Vec<(&str, &str)> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["message"]["role"].as_str().unwrap(),
                row["message"]["content"][0]["text"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        said,
        [
            ("user", "where is the retry policy?"),
            ("assistant", "The retry policy is in src/retry.rs.")
        ]
    );
    let last = client.ok(json!({ "type": "get_agent_messages", "agent": "scout-1", "limit": 1 }));
    assert_eq!(last.as_array().unwrap().len(), 1);
    assert_eq!(last[0]["message"]["role"], "assistant");
    let first = client.ok(json!({
        "type": "get_agent_messages", "agent": "scout-1", "before": last[0]["entry_id"], "limit": 1
    }));
    assert_eq!(first[0]["message"]["role"], "user");
    assert_eq!(client.finish(), Some(0));
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/scripts")
        .join(name)
}

/// The texts of the messages of `request` a user wrote, in order.
fn user_texts(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| message["content"][0]["text"].as_str().unwrap().to_string())
        .collect()
}

/// The model each request went to, in order.
fn models_asked(client: &Client) -> Vec<String> {
    std::fs::read_to_string(client.dir.path().join("requests.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["model"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[test]
fn the_agent_is_told_what_happened_on_a_branch_that_is_left() {
    let script = json!({
        "lead": [
            { "reply": { "text": "one" } },
            { "reply": { "text": "two" } },
            { "reply": { "text": "three" } },
        ],
        "smol": [{ "reply": { "text": "A second thing was asked for.\nThe agent answered two." } }],
    });
    let mut client = Client::start(&script, &["--set", "roles.smol=[\"mock/smol\"]"]);
    client.prompt("first");
    client.prompt("second");
    let tree = client.tree();
    let entry = |text: &str| {
        tree.iter()
            .find(|row| row.text.as_deref() == Some(text))
            .unwrap()
            .id
            .clone()
    };

    // Back to the first reply: the second exchange is written up, and the user is told.
    client.state(json!({ "type": "set_leaf", "entry_id": entry("one") }));
    let (_, told) = client.until(|line| line["type"] == "ui_request" && line["kind"] == "notify");
    assert_eq!(
        told["text"],
        "the agent was told what happened on the branch you left:\nA second thing was asked for.\nThe agent \
         answered two."
    );
    // The small model read the branch that was left, and nothing of what the conversation
    // goes on from.
    let asked = client.requests().pop().unwrap();
    assert_eq!(models_asked(&client), ["lead", "lead", "smol"]);
    let read = user_texts(&asked).join("\n");
    assert!(read.contains("User: second\nAgent: two"), "{read}");
    assert!(!read.contains("first"), "{read}");

    // The note asked for no reply; it is in front of the next prompt.
    client.prompt("third");
    let sent = user_texts(&client.requests().pop().unwrap());
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert_eq!(sent[0], "first");
    assert!(
        sent[1].starts_with("I went back to an earlier point of this conversation.")
            && sent[1].ends_with("A second thing was asked for.\nThe agent answered two."),
        "{sent:?}"
    );
    assert_eq!(sent[2], "third");

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn thinking_name_and_tier_are_set_and_the_context_is_compacted() {
    let mut client = Client::start(&replies(&["one", "## Goal\nsummarised"]), &[]);
    client.ok(json!({ "type": "set_thinking", "level": "high" }));
    assert!(
        client
            .error(json!({ "type": "set_thinking", "level": "extreme" }))
            .contains("extreme")
    );
    client.ok(json!({ "type": "set_tier", "tier": "priority" }));
    let state = client.state(json!({ "type": "get_state" }));
    assert_eq!(
        (state.thinking.as_str(), state.tier.as_deref(), state.name),
        ("high", Some("priority"), None)
    );
    client.ok(json!({ "type": "set_tier", "tier": "standard" }));
    assert!(
        client
            .error(json!({ "type": "set_tier", "tier": "warp" }))
            .contains("warp")
    );
    assert_eq!(
        client.error(json!({ "type": "set_name", "name": "  " })),
        "`name` must not be empty"
    );
    client.ok(json!({ "type": "set_name", "name": "demo" }));
    let state = client.state(json!({ "type": "get_state" }));
    assert_eq!((state.tier, state.name.as_deref()), (None, Some("demo")));
    // Both are recorded, so that a resumed session comes back as it was left.
    let kinds: Vec<String> = client.tree().into_iter().map(|row| row.kind).collect();
    assert_eq!(kinds, ["thinking_change", "session_info"]);

    client.prompt("first");
    assert_eq!(client.requests()[0]["thinking"], "high");
    assert_eq!(
        client.ok(json!({ "type": "compact", "instructions": "keep the file names" })),
        Value::Null
    );
    let (_, compaction) = client.until(|line| line["type"] == "compaction");
    assert_eq!(compaction["agent"], "lead");
    let summarise = client.requests().pop().unwrap();
    assert!(
        summarise["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("Additional instructions: keep the file names")
    );
    assert!(client.tree().iter().any(|row| row.kind == "compaction"));

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn a_shell_line_runs_and_the_session_is_exported() {
    let mut client = Client::start(&replies(&["one"]), &[]);
    client.prompt("first");

    let shared = client.ok(json!({ "type": "shell", "command": "echo hi-from-the-shell" }));
    assert_eq!(shared["status"], "exit code: 0");
    assert!(
        shared["output"].as_str().unwrap().contains("hi-from-the-shell"),
        "{shared}"
    );
    let private = client.ok(json!({ "type": "shell", "command": "echo for-me-only", "share": false }));
    assert!(private["output"].as_str().unwrap().contains("for-me-only"));
    // The lead is told about the first and not about the second.
    let notes: Vec<String> = client
        .messages()
        .into_iter()
        .skip(2)
        .map(|(_, text)| text)
        .collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].starts_with("I ran `echo hi-from-the-shell` myself:") && notes[0].contains("exit code: 0")
    );

    let html = client.ok(json!({ "type": "export", "format": "html" }));
    let html_path = PathBuf::from(html["path"].as_str().unwrap());
    // Canonical on both sides: the temporary directory may sit behind a symlink (macOS `/var`).
    let canonical = |path: &std::path::Path| dunce::canonicalize(path).unwrap();
    assert_eq!(
        canonical(html_path.parent().unwrap()),
        canonical(&client.project())
    );
    assert_eq!(html_path.extension().unwrap(), "html");
    let page = std::fs::read_to_string(&html_path).unwrap();
    assert!(page.contains("first") && page.contains("one") && !page.contains("<script"));
    let lines = client.ok(json!({ "type": "export", "path": "saved.jsonl" }));
    assert_eq!(
        canonical(&PathBuf::from(lines["path"].as_str().unwrap())),
        canonical(&client.project().join("saved.jsonl"))
    );
    let saved = std::fs::read_to_string(client.project().join("saved.jsonl")).unwrap();
    assert!(saved.lines().count() >= 3);
    assert!(
        saved
            .lines()
            .all(|line| serde_json::from_str::<Value>(line).is_ok())
    );
    let markdown = client.ok(json!({ "type": "export" }));
    assert!(markdown["path"].as_str().unwrap().ends_with(".md"));
    assert!(
        client
            .error(json!({ "type": "export", "path": "no-such-dir/x.md" }))
            .starts_with("could not write ")
    );

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn a_side_question_is_answered_while_the_lead_works() {
    let script = json!({ "lead": [
        { "delay_ms": 3000, "reply": { "text": "the slow answer" } },
        { "reply": { "text": "It is 42." } }
    ] });
    let mut client = Client::start(&script, &[]);
    client.ok(json!({ "type": "prompt", "text": "think hard" }));
    // The lead's request is on its way; the side question takes the next reply of the script.
    client.until(|line| line["type"] == "message_start");
    let (before, answer) = client.ask(json!({ "type": "btw", "question": "what is the answer?" }));
    assert_eq!(answer["success"], true, "{answer}");
    assert_eq!(answer["data"]["answer"], "It is 42.");
    assert_eq!(answer["data"]["model"], "mock/lead");
    let deltas: String = before
        .iter()
        .filter(|line| line["type"] == "btw_delta" && line["id"] == answer["id"])
        .map(|line| line["delta"].as_str().unwrap())
        .collect();
    assert_eq!(deltas, "It is 42.");
    // It came while the lead was still working.
    assert!(before.iter().all(|line| line["type"] != "idle"), "{before:?}");
    assert_eq!(client.ok(json!({ "type": "get_state" }))["busy"], true);
    client.until(|line| line["type"] == "idle");

    // The side request carries the lead's instructions and tools; the session gains nothing.
    let requests = client.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["system"], requests[0]["system"]);
    assert_eq!(requests[1]["tools"], requests[0]["tools"]);
    let asked = requests[1]["messages"].as_array().unwrap().last().unwrap()["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        asked.starts_with("<side-question>") && asked.ends_with("what is the answer?"),
        "{asked}"
    );
    assert_eq!(
        client.messages(),
        [pair("user", "think hard"), pair("assistant", "the slow answer")]
    );

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn a_rewind_takes_back_files_and_conversation() {
    let script = json!({ "lead": [
        { "reply": { "tool_calls": [
            { "id": "c1", "name": "write", "input": { "path": "made.txt", "content": "made\n" } }
        ] } },
        { "reply": { "text": "made it" } },
        { "reply": { "tool_calls": [
            { "id": "c2", "name": "write", "input": { "path": "more.txt", "content": "more\n" } }
        ] } },
        { "reply": { "text": "made more" } }
    ] });
    let mut client = Client::start(&script, &["--mode", "auto"]);
    std::fs::write(client.project().join("kept.txt"), "kept\n").unwrap();
    client.prompt("make a file");
    client.prompt("make another");
    let (made, more) = (
        client.project().join("made.txt"),
        client.project().join("more.txt"),
    );
    assert!(made.is_file() && more.is_file());

    let tree = client.tree();
    let checkpoints: Vec<&TreeRow> = tree.iter().filter(|row| row.kind == "checkpoint").collect();
    assert_eq!(checkpoints.len(), 2, "{tree:?}");
    let second_prompt = tree
        .iter()
        .find(|row| row.text.as_deref() == Some("make another"))
        .unwrap();
    let reply = tree.iter().rfind(|row| row.kind == "message").unwrap();
    assert_eq!(
        client.error(json!({ "type": "rewind", "entry_id": reply.id })),
        format!("`{}` is neither a checkpoint nor a prompt", reply.id)
    );

    // The files only, named by the prompt: the conversation stays, and the lead is told.
    client.ok(json!({ "type": "rewind", "entry_id": second_prompt.id, "what": "code" }));
    assert!(made.is_file() && !more.exists());
    let messages = client.messages();
    assert!(
        messages
            .last()
            .unwrap()
            .1
            .starts_with("I rewound the project files to how they were before this prompt: make another"),
        "{messages:?}"
    );

    // Everything, named by the first checkpoint: back to an empty conversation.
    client.ok(json!({ "type": "rewind", "entry_id": checkpoints[0].id }));
    assert!(!made.exists() && client.project().join("kept.txt").is_file());
    assert_eq!(client.messages(), []);
    assert_eq!(client.ok(json!({ "type": "get_state" }))["busy"], false);

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn roles_providers_and_models_are_listed_and_changed() {
    let mut client = Client::start(&replies(&[]), &["--trust"]);
    let home_config = client.dir.path().join("home").join("config.toml");
    let config = || std::fs::read_to_string(&home_config).unwrap_or_default();
    let role = |roles: &Value, name: &str| {
        roles["roles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["role"] == name)
            .unwrap()
            .clone()
    };

    // Every provider, with what it takes; the scripted one needs no credential.
    let providers = client.ok(json!({ "type": "list_providers" }));
    let providers = providers.as_array().unwrap();
    let mock = providers.iter().find(|p| p["id"] == "mock").unwrap();
    assert_eq!(
        (mock["usable"].clone(), mock["login"].clone()),
        (json!(true), json!(false))
    );
    let anthropic = providers.iter().find(|p| p["id"] == "anthropic").unwrap();
    assert_eq!(anthropic["oauth"], true);
    assert!(
        anthropic["env"]
            .as_array()
            .unwrap()
            .contains(&json!("ANTHROPIC_API_KEY"))
    );

    // Without `all` only what can be used; with it, every provider's models, each saying so.
    let usable = client.ok(json!({ "type": "list_models" }));
    assert!(usable.as_array().unwrap().iter().all(|row| row["usable"] == true));
    let all = client.ok(json!({ "type": "list_models", "all": true }));
    let all = all.as_array().unwrap();
    assert!(all.len() >= usable.as_array().unwrap().len());
    let row = all.iter().find(|row| row["provider"] == "anthropic").unwrap();
    assert_eq!(row["model"], format!("anthropic/{}", row["id"].as_str().unwrap()));

    // The scripted provider lists a model per queue of its script.
    let fresh = client.ok(json!({ "type": "refresh_models", "provider": "mock" }));
    assert_eq!(fresh[0]["model"], "mock/lead");
    let listed = client.ok(json!({ "type": "list_models" }));
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["model"] == "mock/lead")
    );
    assert!(
        client
            .error(json!({ "type": "refresh_models", "provider": "nope" }))
            .contains("nope")
    );

    // A role is pointed at a model with a thinking level, saved, then cleared.
    let roles = client.ok(json!({
        "type": "set_role", "role": "smol", "chain": ["mock/lead:high", "@default"], "save": "home"
    }));
    let smol = role(&roles, "smol");
    assert_eq!(
        (smol["source"].clone(), smol["thinking"].clone()),
        (json!("home"), json!("high"))
    );
    assert!(
        config().contains("smol = [\"mock/lead:high\", \"@default\"]"),
        "{}",
        config()
    );
    let roles = client.ok(json!({ "type": "list_roles" }));
    assert_eq!(role(&roles, "smol")["resolved"][0], "mock/lead:high");
    let roles = client.ok(json!({ "type": "set_role", "role": "smol", "save": "home" }));
    assert_eq!(role(&roles, "smol")["source"], "builtin");
    assert!(!config().contains("smol"), "{}", config());

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn what_a_ui_elsewhere_needs_is_answered() {
    let mut client = Client::start(&replies(&["one", "two"]), &[]);
    std::fs::create_dir_all(client.project().join("src")).unwrap();
    std::fs::write(client.project().join("src/lib.rs"), "").unwrap();
    client.prompt("first");
    client.prompt("second");

    // The session as its file holds it: the header, every entry, and where it continues.
    let copy = client.ok(json!({ "type": "get_session" }));
    let state = client.state(json!({ "type": "get_state" }));
    assert_eq!(copy["header"]["id"], json!(state.session_id));
    assert_eq!(copy["path"], json!(state.session_file));
    let entries = copy["entries"].as_array().unwrap();
    let messages: Vec<&Value> = entries
        .iter()
        .filter(|entry| entry["type"] == "message")
        .collect();
    assert_eq!(messages.len(), 4, "{entries:?}");
    // The conversation continues from what was said last; what the run noted hangs beside it.
    assert_eq!(copy["leaf"], messages.last().unwrap()["id"]);
    assert_eq!(entries.last().unwrap()["type"], "run_ended");
    assert!(std::path::Path::new(copy["path"].as_str().unwrap()).is_file());

    let cost = client.ok(json!({ "type": "run_command", "name": "cost" }));
    assert_eq!(cost["outcome"], "show");
    assert!(
        cost["text"].as_str().unwrap().starts_with("requests: 2"),
        "{cost}"
    );
    assert_eq!(state.requests, 2);

    // A label on an entry, and going back to before a prompt.
    let second = messages[2]["id"].as_str().unwrap();
    client.ok(json!({ "type": "set_label", "entry_id": second, "label": "try again" }));
    assert_eq!(
        client.error(json!({ "type": "set_label", "entry_id": "nope", "label": "x" })),
        "no entry `nope` in this session"
    );
    let labels: Vec<TreeRow> = client
        .tree()
        .into_iter()
        .filter(|row| row.kind == "label")
        .collect();
    assert_eq!(
        labels
            .iter()
            .map(|row| (row.text.as_deref(), row.target_id.as_deref()))
            .collect::<Vec<_>>(),
        [(Some("try again"), Some(second))]
    );
    client.state(json!({ "type": "set_leaf", "entry_id": second, "before": true }));
    assert_eq!(
        client.messages(),
        [pair("user", "first"), pair("assistant", "one")]
    );

    // The reports of the session are commands like any other.
    let names: Vec<String> = client
        .ok(json!({ "type": "list_commands" }))
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect();
    for name in ["cost", "session", "tools", "fast"] {
        assert!(names.contains(&name.to_string()), "{name} in {names:?}");
    }
    let todo = client.ok(json!({ "type": "run_command", "name": "todo", "args": "add write the tests" }));
    assert_eq!(todo["text"], " 1. [ ] write the tests");
    assert_eq!(
        client.error(json!({ "type": "run_command", "name": "todo", "args": "done 7" })),
        "usage: /todo done <n>, with n from 1 to 1"
    );

    // What the status line of a UI shows comes with the state.
    let state = client.state(json!({ "type": "get_state" }));
    assert_eq!(
        state
            .todo
            .iter()
            .map(|item| (item.text.as_str(), item.status.as_str()))
            .collect::<Vec<_>>(),
        [("write the tests", "pending")]
    );
    assert_eq!(state.roles.get("default").map(String::as_str), Some("mock/lead"));
    assert!(!state.coordinator && state.goal.is_none());

    let files = client.ok(json!({ "type": "list_files" }));
    assert_eq!(files, json!(["src/lib.rs"]));

    // A background session's commands are not for a session with one client of its own.
    let refused = client.error(json!({ "type": "attach" }));
    assert!(
        refused.starts_with("this is not a background session"),
        "{refused}"
    );

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn a_command_says_where_it_comes_from_and_a_confined_run_refuses_the_project_s() {
    let mut client = Client::start_in(&json!({ "lead": [] }), &["--no-memory"], |home| {
        let commands = home.join("commands");
        std::fs::create_dir_all(commands.join("mcp")).unwrap();
        std::fs::write(commands.join("deploy.md"), "Deploy $ARGUMENTS").unwrap();
        // Named like a command of the memory feature, which this run does not have.
        std::fs::write(commands.join("memory.md"), "Not the memory command").unwrap();
        // `mcp:x` is a name for MCP prompts; a file cannot take it.
        std::fs::write(commands.join("mcp").join("x.md"), "Pretend").unwrap();
    });
    let rows = client.ok(json!({ "type": "list_commands" }));
    let source = |name: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .map(|row| row["source"].as_str().unwrap().to_string())
    };
    assert_eq!(source("deploy").as_deref(), Some("custom"));
    assert_eq!(source("memory").as_deref(), Some("custom"));
    assert_eq!(source("fast").as_deref(), Some("builtin"));
    assert_eq!(source("cost").as_deref(), Some("builtin"));
    assert_eq!(source("mcp:x"), None);

    let only = json!(["builtin", "skill", "mcp"]);
    for name in ["deploy", "memory", "/memory"] {
        let refused = client.error(json!({ "type": "run_command", "name": name, "only": only }));
        assert!(refused.contains("project or of a script"), "{name}: {refused}");
    }
    // The program's own commands and the reports run under the same limit.
    let cost = client.ok(json!({ "type": "run_command", "name": "cost", "only": only }));
    assert_eq!(cost["outcome"], "show");
    // Without the limit a project's command is a prompt like any other.
    assert_eq!(
        client.error(json!({ "type": "run_command", "name": "mcp:x" })),
        "no command named `mcp:x`"
    );
    assert_eq!(client.finish(), Some(0));
}

#[test]
fn moving_between_sessions_waits_for_the_lead() {
    let script = json!({ "lead": [{ "delay_ms": 2000, "reply": { "text": "slow" } }] });
    let mut client = Client::start(&script, &[]);
    client.ok(json!({ "type": "prompt", "text": "go" }));
    // Refused from the moment the prompt is accepted, not only once the lead has picked it up.
    for (command, what) in [
        (json!({ "type": "new_session" }), "start a new session"),
        (json!({ "type": "fork_session" }), "fork the session"),
        (json!({ "type": "retry" }), "send the last prompt again"),
        (
            json!({ "type": "set_leaf", "entry_id": "x" }),
            "move within the session",
        ),
    ] {
        assert_eq!(
            client.error(command),
            format!("the lead is working; send `abort` first to {what}")
        );
    }
    client.ok(json!({ "type": "abort" }));
    let (_, idle) = client.until(|line| line["type"] == "idle");
    assert_eq!(idle["stop"], "aborted");
    let state = client.state(json!({ "type": "new_session" }));
    assert!(!state.busy);

    assert_eq!(client.finish(), Some(0));
}

#[cfg(feature = "lua")]
#[test]
fn a_script_adds_a_prompt_part_and_acts_on_the_session() {
    const SCRIPT: &str = r#"
        azazel.prompt("house_rules", function(ctx)
            return "House rule: tabs, for " .. ctx.agent_id .. "."
        end)
        azazel.on("session_start", function()
            azazel.session.set_name("named by a script")
            azazel.session.set_thinking("high")
        end)
        azazel.command({ name = "said", run = function()
            local lines = {}
            for _, message in ipairs(azazel.session.messages()) do
                table.insert(lines, message.role .. ": " .. message.text)
            end
            return azazel.session.name() .. " | " .. table.concat(lines, ", ")
        end })
        azazel.command({ name = "switch", run = function(args)
            azazel.session.set_model({ args, "@default" })
            azazel.session.compact("keep it short")
        end })
    "#;
    let mut client = Client::start_in(&replies(&["one", "## Goal\nsummarised"]), &[], |home| {
        std::fs::create_dir_all(home.join("extensions")).unwrap();
        std::fs::write(home.join("extensions/house.luau"), SCRIPT).unwrap();
    });
    // What the script did when the session started is in the session file. The thinking level
    // reaches the lead through its command queue, so the state may show it a moment later.
    let mut state = client.state(json!({ "type": "get_state" }));
    let asked = std::time::Instant::now();
    while state.thinking != "high" && asked.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(50));
        state = client.state(json!({ "type": "get_state" }));
    }
    assert_eq!(
        (state.name.as_deref(), state.thinking.as_str()),
        (Some("named by a script"), "high")
    );
    let tree = client.tree();
    let named = tree.iter().find(|row| row.kind == "session_info").unwrap();
    assert_eq!(named.text.as_deref(), Some("named by a script"));
    assert!(tree.iter().any(|row| row.kind == "thinking_change"));

    client.prompt("first");
    let request = &client.requests()[0];
    let system: Vec<&str> = request["system"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| part["text"].as_str().unwrap())
        .collect();
    assert!(system.contains(&"House rule: tabs, for lead."), "{system:?}");
    assert_eq!(request["thinking"], "high");

    let said = client.ok(json!({ "type": "run_command", "name": "said" }));
    assert_eq!(
        said,
        json!({ "outcome": "show", "text": "named by a script | user: first, assistant: one" })
    );
    // The model changes for the lead and the change is recorded; the compaction follows.
    let switched = client.ok(json!({ "type": "run_command", "name": "switch", "args": "mock/other" }));
    assert_eq!(switched, json!({ "outcome": "done" }));
    client.until(|line| line["type"] == "compaction");
    let state = client.state(json!({ "type": "get_state" }));
    assert_eq!(state.chain, ["mock/other", "@default"]);
    assert!(client.tree().iter().any(|row| row.kind == "model_change"));
    let mistake = client.error(json!({ "type": "run_command", "name": "switch", "args": "not a model" }));
    assert!(
        mistake.starts_with("/switch: azazel.session.set_model: "),
        "{mistake}"
    );

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn a_named_prompt_sent_again_starts_nothing_and_says_where_its_run_stands() {
    let script = json!({ "lead": [
        { "delay_ms": 1500, "reply": { "text": "First." } },
        { "reply": { "text": "Second." } },
    ] });
    let mut client = Client::start(&script, &[]);
    let named = json!({ "type": "prompt", "text": "one", "request_id": "job-1" });
    let first = client.ok(named.clone());
    assert_eq!(first, json!({ "request_id": "job-1", "state": "queued" }));
    // The same line again, while the run it started goes on.
    let again = client.ok(named.clone());
    assert_eq!(
        (&again["duplicate"], &again["request_id"]),
        (&json!(true), &json!("job-1")),
        "{again}"
    );
    assert!(
        again["state"] == "queued" || again["state"] == "running",
        "{again}"
    );
    let (_, idle) = client.until(|line| line["type"] == "idle" || line["type"] == "failed");
    assert_eq!(idle["type"], "idle", "{idle}");

    // And once it has ended: how it ended and what was said, read from the session file.
    let ended = client.ok(named);
    let run_id = ended["run_id"].as_str().unwrap().to_string();
    assert_eq!(
        ended,
        json!({ "request_id": "job-1", "run_id": run_id, "state": "ended", "duplicate": true,
                "stop": "stop", "text": "First." })
    );
    assert_eq!(client.requests().len(), 1, "one run, one request");
    assert_eq!(
        client.messages(),
        [pair("user", "one"), pair("assistant", "First.")]
    );

    // Another name is another prompt; none at all answers as it always did.
    client.ok(json!({ "type": "prompt", "text": "two", "request_id": "job-2" }));
    let (_, idle) = client.until(|line| line["type"] == "idle" || line["type"] == "failed");
    assert_eq!(idle["type"], "idle", "{idle}");
    assert_eq!(client.requests().len(), 2);
    let second = client.ok(json!({ "type": "prompt", "text": "two", "request_id": "job-2" }));
    assert_eq!(
        (&second["state"], &second["text"]),
        (&json!("ended"), &json!("Second."))
    );
    // A new session has not seen the name.
    client.ok(json!({ "type": "new_session" }));
    let fresh = client.ok(json!({ "type": "prompt", "text": "one", "request_id": "job-1" }));
    assert_eq!(fresh["state"], "queued", "{fresh}");
    assert_eq!(fresh.get("duplicate"), None, "{fresh}");
    client.until(|line| line["type"] == "idle" || line["type"] == "failed");
    assert_eq!(client.finish(), Some(0));
}

#[test]
fn a_handoff_continues_in_a_new_session_that_starts_from_the_masked_brief() {
    let brief = "## Requests\n- add the clock (done)\n\n## Next steps\nship it, key sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789";
    let mut client = Client::start(&replies(&["the clock is in", brief]), &[]);
    let first = client.state(json!({ "type": "get_state" }));
    assert!(
        client
            .error(json!({ "type": "handoff" }))
            .contains("nothing to hand off")
    );
    client.prompt("add the clock");

    let handed = client.ok(json!({ "type": "handoff", "focus": "the clock" }));
    assert_eq!(handed["from"], first.session_id);
    let now = client.state(json!({ "type": "get_state" }));
    assert_eq!(handed["session_id"], now.session_id);
    assert_ne!(now.session_id, first.session_id);

    // The model was asked once, with the structure of the brief and the focus.
    let requests = client.requests();
    assert_eq!(requests.len(), 2);
    let asked = requests[1]["messages"].as_array().unwrap().last().unwrap()["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        asked.contains("Errors already solved") && asked.ends_with("Put the weight on: the clock"),
        "{asked}"
    );

    // The new session holds the brief, with the secret masked, and no reply.
    let messages = client.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0].0, "user");
    assert!(messages[0].1.contains("## Next steps"), "{}", messages[0].1);
    assert!(
        !messages[0].1.contains("abcdefghijklmnopqrstuvwxyz0123456789"),
        "{}",
        messages[0].1
    );
    // The draft made first is only for a request that fails.
    assert!(!find_draft(client.dir.path(), &first.session_id));

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn a_handoff_is_refused_while_the_lead_works() {
    let script = json!({ "lead": [
        { "delay_ms": 3000, "reply": { "text": "the slow answer" } }
    ] });
    let mut client = Client::start(&script, &[]);
    client.ok(json!({ "type": "prompt", "text": "think hard" }));
    client.until(|line| line["type"] == "message_start");
    let refused = client.error(json!({ "type": "handoff" }));
    assert!(refused.contains("the lead is working"), "{refused}");
    client.ok(json!({ "type": "abort" }));
    client.until(|line| line["type"] == "idle" || line["type"] == "failed");
    assert_eq!(client.finish(), Some(0));
}

#[test]
fn nothing_changes_the_session_while_a_brief_is_written_and_abort_cancels_it() {
    let script = json!({ "lead": [
        { "reply": { "text": "the clock is in" } },
        { "delay_ms": 30000, "reply": { "text": "a brief that comes too late" } }
    ] });
    let mut client = Client::start(&script, &[]);
    let first = client.state(json!({ "type": "get_state" }));
    client.prompt("add the clock");

    let handoff = client.send(json!({ "type": "handoff" }));
    // The brief is being written: the request to the model is on its way.
    let state = loop {
        let state = client.state(json!({ "type": "get_state" }));
        if state.busy && client.requests().len() == 2 {
            break state;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(state.session_id, first.session_id);
    for command in [
        json!({ "type": "prompt", "text": "one more thing" }),
        json!({ "type": "steer", "text": "and this" }),
        json!({ "type": "follow_up", "text": "and that" }),
        json!({ "type": "handoff" }),
    ] {
        let refused = client.error(command.clone());
        assert!(
            refused.contains("hand-off") || refused.contains("hand the work off"),
            "{command}: {refused}"
        );
    }

    client.ok(json!({ "type": "abort" }));
    let cancelled = client.response(handoff);
    assert_eq!(cancelled["success"], false, "{cancelled}");
    // Nothing was handed off: the same session is open, idle, and takes a prompt again.
    let after = client.state(json!({ "type": "get_state" }));
    assert_eq!(after.session_id, first.session_id);
    assert!(!after.busy);
    assert_eq!(client.messages().len(), 2);
    assert_eq!(client.finish(), Some(0));
}

#[test]
fn a_handoff_whose_request_fails_leaves_the_draft_and_says_what_the_model_tried() {
    let script = json!({ "lead": [
        { "reply": { "text": "the clock is in" } },
        { "events": [{ "type": "error", "error": { "kind": "invalid_request", "detail": "boom" } }] },
        { "reply": { "tool_calls": [{ "id": "c1", "name": "read", "input": { "path": "a.txt" } }] } }
    ] });
    let mut client = Client::start(&script, &[]);
    let first = client.state(json!({ "type": "get_state" }));
    client.prompt("add the clock");

    let failed = client.error(json!({ "type": "handoff" }));
    assert!(
        failed.contains("boom") && failed.contains("a draft brief is in"),
        "{failed}"
    );
    assert!(find_draft(client.dir.path(), &first.session_id));

    let tried = client.error(json!({ "type": "handoff" }));
    assert!(tried.contains("tried to use read"), "{tried}");
    // The session is as it was.
    assert_eq!(
        client.state(json!({ "type": "get_state" })).session_id,
        first.session_id
    );
    assert_eq!(client.finish(), Some(0));
}

fn find_draft(root: &Path, id: &str) -> bool {
    let name = format!("handoff-{id}.md");
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| *n == *name) {
                return true;
            }
        }
    }
    false
}

#[test]
fn security_check_sends_the_dependency_audit_skill_as_the_prompt() {
    let mut client = Client::start(&replies(&["audited"]), &[]);
    let rows = client.ok(json!({ "type": "list_commands" }));
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "security-check"),
        "{rows}"
    );
    let sent = client.ok(json!({ "type": "run_command", "name": "security-check", "args": "web/" }));
    assert_eq!(sent["outcome"], "prompt");
    let text = sent["text"].as_str().unwrap();
    assert!(
        text.contains("# Dependency audit") && text.contains("api.osv.dev"),
        "{text}"
    );
    assert!(
        text.ends_with("Arguments: web/") || text.contains("web/"),
        "{text}"
    );
    assert!(!text.contains("$ARGUMENTS"), "{text}");
    client.until(|line| line["type"] == "idle");
    let first = &client.requests()[0];
    assert_eq!(user_texts(first), [text.to_string()]);
}
