#![allow(clippy::unwrap_used)]

//! What a session can say about its context: `azazel context` and the `context` report take the
//! lead's last request apart by category, and `session_recall` brings back what eliding and
//! compaction took out of it.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Output, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

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

    fn azazel(&self) -> Command {
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.dir.path().join("home"))
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .arg("--cwd")
            .arg(self.project());
        command
    }

    /// `azazel -p <prompt>` answering from `script`, every request recorded.
    fn print(&self, script: &Value, prompt: &str) -> Output {
        let script_path = self.dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let _ = std::fs::remove_file(self.requests_file());
        self.azazel()
            .env("AZAZEL_MOCK_SCRIPT", &script_path)
            .env("AZAZEL_MOCK_REQUESTS", self.requests_file())
            .args(["-p", "--no-memory", "--set", "session.auto_name=false", prompt])
            .write_stdin("")
            .output()
            .unwrap()
    }

    /// The requests the scripted provider received, in order.
    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.requests_file())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
            .collect()
    }

    /// Every entry of every session file of the sandbox.
    fn entries(&self) -> Vec<Value> {
        let mut entries = Vec::new();
        for project in std::fs::read_dir(self.dir.path().join("home/projects"))
            .unwrap()
            .flatten()
        {
            for file in std::fs::read_dir(project.path().join("sessions"))
                .into_iter()
                .flatten()
                .flatten()
            {
                if file.path().extension().is_some_and(|e| e == "jsonl") {
                    let text = std::fs::read_to_string(file.path()).unwrap();
                    entries.extend(
                        text.lines()
                            .skip(1)
                            .map(|line| serde_json::from_str::<Value>(line).unwrap()),
                    );
                }
            }
        }
        entries
    }

    /// Writes `<home>/config.toml`.
    fn configure(&self, toml: &str) {
        std::fs::write(self.dir.path().join("home/config.toml"), toml).unwrap();
    }

    /// Tokens of the lead's system prompt as compaction counts them, found by asking once.
    fn system_tokens(&self) -> u64 {
        let output = self.print(&json!({ "lead": [text("hi")] }), "hi");
        assert!(output.status.success(), "{}", stderr(&output));
        self.requests()[0]["system"]
            .as_array()
            .unwrap()
            .iter()
            .map(|part| (part["text"].as_str().unwrap().len() as u64).div_ceil(4))
            .sum()
    }

    /// A file of about 2000 tokens with one line that says `marker`.
    fn big_file(&self, name: &str, marker: &str) {
        let mut body: String = (0..99)
            .map(|n| format!("{name} line {n:02} {}\n", "x".repeat(64)))
            .collect();
        body.push_str(&format!("the {marker} lives in {name}\n"));
        std::fs::write(self.project().join(name), body).unwrap();
    }
}

fn text(text: &str) -> Value {
    json!({ "reply": { "text": text } })
}

fn call(id: &str, name: &str, input: Value) -> Value {
    json!({ "reply": { "tool_calls": [{ "id": id, "name": name, "input": input }] } })
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The text of every tool result of a request, in order.
fn result_texts(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap())
        .filter(|block| block["type"] == "tool_result")
        .map(|block| {
            block["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|inner| inner["text"].as_str())
                .collect::<String>()
        })
        .collect()
}

/// The entry id an elision note or a summary's note names after `before`.
fn id_after<'a>(text: &'a str, before: &str) -> &'a str {
    let rest = &text[text
        .find(before)
        .unwrap_or_else(|| panic!("no `{before}` in {text}"))
        + before.len()..];
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn the_last_request_of_a_session_is_reported_by_category_from_the_shell() {
    let sandbox = Sandbox::new();
    std::fs::write(
        sandbox.project().join("AGENTS.md"),
        "# Build\nRun `make` before anything else.\n",
    )
    .unwrap();
    std::fs::write(sandbox.project().join("notes.txt"), "remember the milk\n").unwrap();
    // Nothing to report before a session exists.
    let none = sandbox.azazel().args(["context", "--json"]).output().unwrap();
    assert!(!none.status.success());
    assert!(
        stderr(&none).contains("this project has no session yet"),
        "{}",
        stderr(&none)
    );

    let script = json!({ "lead": [
        call("c1", "read", json!({ "path": "notes.txt" })),
        { "reply": { "text": "done", "usage": { "input": 3000, "cache_read": 1000, "output": 20 } } },
    ] });
    let output = sandbox.print(&script, "read notes.txt");
    assert!(output.status.success(), "{}", stderr(&output));
    let sent = sandbox.requests().pop().unwrap();

    let shown = sandbox.azazel().args(["context", "--json"]).output().unwrap();
    assert!(shown.status.success(), "{}", stderr(&shown));
    let report: Value = serde_json::from_str(stdout(&shown).trim()).unwrap();
    assert_eq!(report["agent"], "lead");
    assert_eq!(report["request"], 2);
    assert_eq!(report["model"], "mock/lead");
    // Cached input is input; the categories are scaled to add up to what was counted.
    assert_eq!(report["reported"], 4000);
    assert_eq!(report["tokens"], 4000);
    let categories = report["categories"].as_array().unwrap();
    assert_eq!(
        categories
            .iter()
            .map(|c| c["tokens"].as_u64().unwrap())
            .sum::<u64>(),
        4000
    );
    assert_eq!(
        categories
            .iter()
            .map(|c| c["estimate"].as_u64().unwrap())
            .sum::<u64>(),
        report["estimate"].as_u64().unwrap()
    );

    // Every part of the system prompt that was sent is a category under its own name, counted
    // as compaction counts text.
    let find = |kind: &str, name: &str| {
        categories
            .iter()
            .find(|c| c["kind"] == kind && c["name"] == name)
            .unwrap_or_else(|| panic!("no {kind} category `{name}` in {categories:?}"))
    };
    let parts = sent["system"].as_array().unwrap();
    assert!(parts.len() >= 3, "{parts:?}");
    for part in parts {
        let name = part["name"]
            .as_str()
            .expect("every part the prompt builder made has a name");
        let tokens = (part["text"].as_str().unwrap().len() as u64).div_ceil(4);
        assert_eq!(find("system", name)["estimate"], tokens, "{name}");
    }
    let agents = find("system", "context_files");
    assert_eq!(agents["label"], "AGENTS and context files");
    assert!(
        parts
            .iter()
            .any(|part| part["name"] == "context_files"
                && part["text"].as_str().unwrap().contains("Run `make`"))
    );
    assert_eq!(find("system", "identity")["label"], "system prompt: identity");
    assert_eq!(
        find("tools", "")["items"].as_u64().unwrap() as usize,
        sent["tools"].as_array().unwrap().len()
    );
    assert_eq!(find("messages", "user")["items"], 1);
    assert_eq!(find("messages", "tool_calls")["items"], 1);
    let results = find("tool_results", "");
    assert_eq!(results["items"], 1);
    assert_eq!(
        results["estimate"].as_u64().unwrap(),
        (result_texts(&sent)[0].len() as u64).div_ceil(4)
    );

    // The same as text, and for a session named by the start of its id.
    let id = report["session"].as_str().unwrap();
    let plain = sandbox.azazel().args(["context", &id[..8]]).output().unwrap();
    let plain = stdout(&plain);
    let lines: Vec<&str> = plain.lines().collect();
    assert_eq!(lines[0], format!("session {id}"));
    assert_eq!(lines[1], "request 2 of lead · mock/lead");
    assert!(lines[2].starts_with("4.0k of "), "{}", lines[2]);
    assert_eq!(lines.len(), 3 + categories.len());
    assert!(plain.contains("\nAGENTS and context files "));

    let unknown = sandbox.azazel().args(["context", "nope"]).output().unwrap();
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("no session with id `nope`"));
}

#[test]
fn an_elided_result_names_its_entry_and_session_recall_returns_what_it_held() {
    let sandbox = Sandbox::new();
    let system = sandbox.system_tokens();
    // Three reads of about 2000 tokens each. Eliding starts above half the window, which the
    // third read passes; the newest result is kept.
    sandbox.configure(&format!(
        "[models.\"mock/lead\"]\ncontext_window = {}\n\n[compaction]\nreserve_tokens = 100\n\
         keep_tool_results = 1\nmicro_threshold = 0.5\n",
        (system + 5300) * 2
    ));
    for (name, marker) in [("a.txt", "quokka"), ("b.txt", "numbat"), ("c.txt", "bilby")] {
        sandbox.big_file(name, marker);
    }
    let script = json!({ "lead": [
        call("c1", "read", json!({ "path": "a.txt" })),
        call("c2", "read", json!({ "path": "b.txt" })),
        call("c3", "read", json!({ "path": "c.txt" })),
        call("c4", "session_recall", json!({ "query": "quokka" })),
        call("c5", "session_recall", json!({ "expand": ["FIRST"], "touched": true })),
        text("done"),
    ] });
    // The script cannot know the id: the second recall names a placeholder and the test reads
    // the entry itself below.
    let output = sandbox.print(&script, "read the three files");
    assert!(output.status.success(), "{}", stderr(&output));
    let requests = sandbox.requests();
    assert_eq!(requests.len(), 6);

    // Request four carries the first two results as notes that name their entries.
    let elided = result_texts(&requests[3]);
    assert_eq!(elided.len(), 3);
    assert!(
        elided[0].starts_with("[result elided: ") && elided[0].contains("; session_recall expand="),
        "{}",
        elided[0]
    );
    assert!(elided[2].contains("the bilby lives in c.txt"));
    let first = id_after(&elided[0], "expand=").to_string();
    let second = id_after(&elided[1], "expand=").to_string();
    assert_ne!(first, second);

    // The search found the entry the note names, and that entry is the read of a.txt.
    let found = result_texts(&requests[4]).pop().unwrap();
    assert_eq!(
        found,
        format!(
            "1 matching entry:\n{first}  result of read a.txt: 100\tthe quokka lives in a.txt\n\
             Read an entry in full with expand=[\"<id>\"]."
        )
    );
    let entries = sandbox.entries();
    let held = entries
        .iter()
        .find(|entry| entry["id"] == first.as_str())
        .unwrap();
    assert!(
        held["message"]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("the quokka lives in a.txt")
    );
    // The files read, each with the entry that holds what was read.
    let touched = result_texts(&requests[5]).pop().unwrap();
    assert!(
        touched.starts_with(&format!("read:\n  a.txt  {first}\n  b.txt  {second}\n  c.txt  ")),
        "{touched}"
    );
    assert!(touched.contains("no entry `FIRST` holds a message"));
}

#[test]
fn a_summary_says_where_it_ends_and_session_recall_finds_what_it_replaced() {
    let sandbox = Sandbox::new();
    let system = sandbox.system_tokens();
    // Nothing is elided; the third read takes the context past the window less the reserve,
    // so the fourth request is preceded by a summary of everything before that read.
    sandbox.configure(&format!(
        "[models.\"mock/lead\"]\ncontext_window = {}\n\n[compaction]\nreserve_tokens = 1000\n\
         keep_recent_tokens = 1500\nkeep_tool_results = 100\nmicro_threshold = 1.0\n",
        system + 6300
    ));
    for (name, marker) in [("a.txt", "quokka"), ("b.txt", "numbat"), ("c.txt", "bilby")] {
        sandbox.big_file(name, marker);
    }
    let script = json!({ "lead": [
        call("c1", "read", json!({ "path": "a.txt" })),
        call("c2", "read", json!({ "path": "b.txt" })),
        call("c3", "read", json!({ "path": "c.txt" })),
        text("## Goal\nRead three files.\n## Files read\na.txt\nb.txt"),
        call("c4", "session_recall", json!({ "regex": "the (quokka|numbat) lives" })),
        text("done"),
    ] });
    let output = sandbox.print(&script, "read the three files");
    assert!(output.status.success(), "{}", stderr(&output));
    let requests = sandbox.requests();
    assert_eq!(requests.len(), 6, "three reads, the summary, the recall, the end");

    // The summariser saw the span with entry ids and was told what they are for.
    let summariser = &requests[3];
    let span = summariser["messages"][0]["content"][0]["text"].as_str().unwrap();
    assert!(span.starts_with("<conversation>\nUser ["), "{}", &span[..80]);
    assert!(span.contains("Assistant called read [") && span.contains("Tool result ["));
    assert!(summariser["tools"].as_array().unwrap().is_empty());

    // The lead's next request starts from the summary, which names the newest entry it covers.
    let resumed = &requests[4];
    let summary = resumed["messages"][0]["content"][0]["text"].as_str().unwrap();
    assert!(
        summary.starts_with("Summary of the conversation so far:\n\n## Goal\nRead three files."),
        "{summary}"
    );
    let last = id_after(summary, "up to entry ");
    assert!(summary.contains("session_recall"));
    assert!(!summary.contains("quokka") && result_texts(resumed).iter().all(|r| !r.contains("quokka")));
    let entries = sandbox.entries();
    let position = |id: &str| {
        entries
            .iter()
            .position(|entry| entry["id"] == id)
            .unwrap_or_else(|| panic!("no entry {id}"))
    };
    // That entry is the result of the second read: the last thing before what was kept.
    assert!(
        entries[position(last)]["message"]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("the numbat lives in b.txt")
    );

    // Both reads the summary replaced are found, oldest first, by entries before its end.
    let found = result_texts(&requests[5]).pop().unwrap();
    let lines: Vec<&str> = found.lines().collect();
    assert_eq!(lines[0], "2 matching entries, oldest first:");
    assert!(
        lines[1].ends_with("  result of read a.txt: 100\tthe quokka lives in a.txt"),
        "{found}"
    );
    assert!(
        lines[2].ends_with("  result of read b.txt: 100\tthe numbat lives in b.txt"),
        "{found}"
    );
    let ids: Vec<&str> = lines[1..3]
        .iter()
        .map(|line| line.split_once("  ").unwrap().0)
        .collect();
    assert!(position(ids[0]) < position(ids[1]) && ids[1] == last);
}

/// An `azazel rpc` process in a sandbox, answering from a script.
struct Rpc {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
}

impl Rpc {
    fn start(sandbox: &Sandbox, script: &Value, args: &[&str]) -> Rpc {
        let script_path = sandbox.dir.path().join("rpc-script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let _ = std::fs::remove_file(sandbox.requests_file());
        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("azazel"))
            .env("AZAZEL_HOME", sandbox.dir.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", &script_path)
            .env("AZAZEL_MOCK_REQUESTS", sandbox.requests_file())
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .args(["--set", "session.auto_name=false", "--no-memory"])
            .args(args)
            .args(["rpc", "--cwd"])
            .arg(sandbox.project())
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
        Rpc {
            stdin: child.stdin.take(),
            child,
            lines,
            next_id: 0,
        }
    }

    fn next(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(60))
            .expect("a protocol line within a minute");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not a protocol line ({e}): {line}"))
    }

    /// Sends a command and reads up to its response.
    fn ask(&mut self, mut command: Value) -> Value {
        self.next_id += 1;
        command["id"] = self.next_id.into();
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{command}").unwrap();
        stdin.flush().unwrap();
        loop {
            let line = self.next();
            if line["type"] == "response" && line["id"] == self.next_id {
                return line;
            }
        }
    }

    /// The response to `/context <args>`.
    fn context(&mut self, args: &str) -> Value {
        self.ask(json!({ "type": "run_command", "name": "context", "args": args }))
    }

    /// What `/context <args>` shows; it must succeed.
    fn shown(&mut self, args: &str) -> String {
        let response = self.context(args);
        assert_eq!(response["success"], true, "/context {args}: {response}");
        response["data"]["text"].as_str().unwrap().to_string()
    }

    /// Sends a prompt and reads to the end of its run.
    fn prompt(&mut self, text: &str) {
        let response = self.ask(json!({ "type": "prompt", "text": text }));
        assert_eq!(response["success"], true, "{response}");
        loop {
            let line = self.next();
            assert_ne!(line["type"], "failed", "{line}");
            if line["type"] == "idle" {
                return;
            }
        }
    }

    fn finish(mut self) -> Option<i32> {
        drop(self.stdin.take());
        self.child.wait().unwrap().code()
    }
}

#[test]
fn the_context_report_gives_numbers_then_the_exact_text_of_a_category() {
    let sandbox = Sandbox::new();
    let mut rpc = Rpc::start(&sandbox, &json!({ "lead": [text("one"), text("two")] }), &[]);

    // Before any request: said so, and nothing to take apart.
    let early = rpc.shown("");
    assert!(
        early.starts_with("no request has been sent in this session yet"),
        "{early}"
    );
    assert_eq!(rpc.context("json")["success"], false);

    rpc.prompt("a question about heron nests");
    let numbers: Value = serde_json::from_str(&rpc.shown("json")).unwrap();
    assert_eq!(numbers["request"], 1);
    let categories = numbers["categories"].as_array().unwrap();
    let sent = sandbox.requests().remove(0);

    // The text of a category is the text that was sent: a system part word for word, the
    // prompt under the message it is.
    let rules = categories
        .iter()
        .position(|c| c["kind"] == "system" && c["name"] == "rules")
        .unwrap();
    let rules_sent = sent["system"]
        .as_array()
        .unwrap()
        .iter()
        .find(|part| part["name"] == "rules")
        .unwrap();
    assert_eq!(
        rpc.shown(&format!("text 1 {rules}")),
        rules_sent["text"].as_str().unwrap()
    );
    let user = categories
        .iter()
        .position(|c| c["kind"] == "messages" && c["name"] == "user")
        .unwrap();
    assert_eq!(
        rpc.shown(&format!("text 1 {user}")),
        "--- message 1 ---\na question about heron nests"
    );
    assert!(rpc.shown("").starts_with("request 1 of lead · mock/lead\n"));

    // A category that does not exist, and a request the agent has moved on from.
    assert_eq!(
        rpc.context(&format!("text 1 {}", categories.len()))["success"],
        false
    );
    rpc.prompt("and another");
    let stale = rpc.context(&format!("text 1 {user}"));
    assert_eq!(stale["success"], false);
    assert!(
        stale["error"]
            .as_str()
            .unwrap()
            .starts_with("request 1 is gone: the agent has sent request 2 since"),
        "{stale}"
    );
    assert_eq!(
        rpc.shown(&format!("text 2 {user}")),
        "--- message 1 ---\na question about heron nests\n\n--- message 3 ---\nand another"
    );
    assert_eq!(rpc.finish(), Some(0));
}

#[test]
fn a_reopened_session_still_says_what_its_last_request_was_made_of() {
    let sandbox = Sandbox::new();
    let script = json!({ "lead": [
        { "reply": { "text": "first answer", "usage": { "input": 2500 } } },
    ] });
    let output = sandbox.print(&script, "a question about heron nests");
    assert!(output.status.success(), "{}", stderr(&output));
    let from_shell = sandbox.azazel().args(["context", "--json"]).output().unwrap();
    let mut from_shell: Value = serde_json::from_str(stdout(&from_shell).trim()).unwrap();
    assert_eq!(from_shell["reported"], 2500);

    // The same session in a new process, before it has sent anything: the numbers come from
    // the session's folder; the text of that request went with the process that sent it.
    let mut rpc = Rpc::start(&sandbox, &json!({ "lead": [text("second answer")] }), &["-c"]);
    let reopened: Value = serde_json::from_str(&rpc.shown("json")).unwrap();
    from_shell.as_object_mut().unwrap().remove("session");
    assert_eq!(reopened, from_shell);
    let text = rpc.context("text 1 0");
    assert_eq!(text["success"], false);
    assert!(
        text["error"]
            .as_str()
            .unwrap()
            .contains("only while the session that sent it runs"),
        "{text}"
    );

    // Its next request is its own first, with the earlier exchange in it.
    rpc.prompt("and where do they nest?");
    let next: Value = serde_json::from_str(&rpc.shown("json")).unwrap();
    assert_eq!(next["request"], 1);
    let user = next["categories"]
        .as_array()
        .unwrap()
        .iter()
        .position(|c| c["kind"] == "messages" && c["name"] == "user")
        .unwrap();
    assert_eq!(
        rpc.shown(&format!("text 1 {user}")),
        "--- message 1 ---\na question about heron nests\n\n--- message 3 ---\nand where do they nest?"
    );
    assert_eq!(rpc.finish(), Some(0));
}
