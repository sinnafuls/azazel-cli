#![allow(clippy::unwrap_used)]

//! The prompt cache and the context files, from outside: what changes between two requests of a
//! session stays out of what a provider caches, `/cost` says how much was read from the cache and
//! why a request missed it, and instruction files reach the agent from the directories it works
//! in and through `@path` includes.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

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
    /// Starts in a project that `prepare` has filled.
    fn start(script: &Value, args: &[&str], prepare: impl FnOnce(&std::path::Path)) -> Client {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        prepare(&project);
        let script_path = dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let mut child = Command::new(assert_cmd::cargo::cargo_bin("azazel"))
            .env("AZAZEL_HOME", dir.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", &script_path)
            .env("AZAZEL_MOCK_REQUESTS", dir.path().join("requests.jsonl"))
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            // The namer would take a reply of the script for itself.
            .args(["--no-memory", "--set", "session.auto_name=false"])
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

    /// The data of a command that must succeed.
    fn ok(&mut self, mut command: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        command["id"] = id.into();
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{command}").unwrap();
        stdin.flush().unwrap();
        let (_, response) = self.until(|line| line["type"] == "response" && line["id"] == id);
        assert_eq!(response["success"], true, "{command}: {response}");
        response["data"].clone()
    }

    /// Sends a prompt and reads to the end of its run.
    fn prompt(&mut self, text: &str) -> Vec<Value> {
        self.ok(json!({ "type": "prompt", "text": text }));
        let (events, idle) = self.until(|line| line["type"] == "idle" || line["type"] == "failed");
        assert_eq!(idle["type"], "idle", "{idle}");
        events
    }

    /// What `/cost` says.
    fn cost(&mut self) -> String {
        let shown = self.ok(json!({ "type": "run_command", "name": "cost" }));
        shown["text"].as_str().unwrap().to_string()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

fn reply(text: &str, usage: Value) -> Value {
    json!({ "reply": { "text": text, "usage": usage } })
}

fn call(id: &str, name: &str, input: Value) -> Value {
    json!({ "reply": { "tool_calls": [{ "id": id, "name": name, "input": input }] } })
}

/// The parts of `list` (a request's `system` or `tail`) as `(name, text)`.
fn parts(list: &Value) -> Vec<(String, String)> {
    list.as_array()
        .into_iter()
        .flatten()
        .map(|part| {
            (
                part["name"].as_str().unwrap_or_default().to_string(),
                part["text"].as_str().unwrap().to_string(),
            )
        })
        .collect()
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
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect()
}

#[test]
fn what_changes_between_requests_follows_the_conversation_and_the_system_prompt_stays_as_it_was() {
    let script = json!({ "lead": [
        reply("one", json!({})),
        reply("two", json!({})),
        reply("three", json!({})),
    ] });
    let mut client = Client::start(&script, &[], |project| {
        std::fs::write(project.join("AGENTS.md"), "Keep it short.\n").unwrap();
    });
    client.prompt("first");
    // A mode switch tells the agent something new.
    client.ok(json!({ "type": "set_mode", "mode": "plan" }));
    client.prompt("second");
    client.ok(json!({ "type": "set_mode", "mode": "manual" }));
    client.prompt("third");

    let requests = client.requests();
    assert_eq!(requests.len(), 3);
    // Byte for byte the same system prompt and tools: everything a provider cached is read again.
    assert_eq!(requests[0]["system"], requests[1]["system"]);
    assert_eq!(requests[0]["system"], requests[2]["system"]);
    assert_eq!(requests[0]["tools"], requests[2]["tools"]);
    let names: Vec<String> = parts(&requests[0]["system"]).into_iter().map(|p| p.0).collect();
    assert!(names.contains(&"environment".to_string()), "{names:?}");
    assert!(names.contains(&"context_files".to_string()), "{names:?}");
    for volatile in ["now", "mode", "memory"] {
        assert!(!names.contains(&volatile.to_string()), "{names:?}");
    }
    let system: String = parts(&requests[1]["system"]).into_iter().map(|p| p.1).collect();
    assert!(
        !system.contains("Date:") && !system.contains("Plan mode"),
        "{system}"
    );

    // The conversation only grows: each request starts with the one before it.
    for (before, after) in [(0, 1), (1, 2)] {
        let was = requests[before]["messages"].as_array().unwrap();
        let is = requests[after]["messages"].as_array().unwrap();
        assert_eq!(
            &is[..was.len()],
            &was[..],
            "request {after} repeats request {before}"
        );
    }

    // What is true for one request only comes after it, by name.
    let tails: Vec<Vec<(String, String)>> = requests.iter().map(|r| parts(&r["tail"])).collect();
    let today = jiff::Zoned::now().date().to_string();
    for tail in &tails {
        assert_eq!(tail[0].0, "now");
        assert!(tail[0].1.starts_with(&format!("Date: {today}")), "{tail:?}");
    }
    assert_eq!(tails[0].len(), 1, "{:?}", tails[0]);
    assert_eq!(tails[1][1].0, "mode");
    assert!(tails[1][1].1.starts_with("# Plan mode"), "{:?}", tails[1]);
    assert_eq!(tails[2].len(), 1, "{:?}", tails[2]);
}

#[test]
fn cost_reports_the_hit_rate_and_names_the_part_that_made_a_request_miss_the_cache() {
    let script = json!({ "lead": [
        reply("one", json!({ "input": 100, "cache_write": 9000, "output": 5 })),
        reply("two", json!({ "input": 50, "cache_read": 9000, "cache_write": 500, "output": 5 })),
        reply("three", json!({ "input": 60, "cache_read": 2000, "cache_write": 7600, "output": 5 })),
    ] });
    let mut client = Client::start(
        &script,
        &["--set", "cache.report_misses=true", "--set", "cache.warming=idle"],
        |project| std::fs::write(project.join("AGENTS.md"), "Keep it short.\n").unwrap(),
    );
    assert!(!client.cost().contains("cache:"), "nothing was counted yet");

    let notices = |events: &[Value]| -> Vec<String> {
        events
            .iter()
            .filter(|event| event["type"] == "ui_request" && event["kind"] == "notify")
            .map(|event| event["text"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(notices(&client.prompt("first")), Vec::<String>::new());
    assert_eq!(notices(&client.prompt("second")), Vec::<String>::new());
    let cost = client.cost();
    assert!(
        cost.contains("cache: 48% of this session's input was read from the prompt cache"),
        "{cost}"
    );
    assert!(!cost.contains("cache misses:"), "{cost}");

    // The user edits the instructions: everything after them in the request is new to the cache.
    std::fs::write(client.project().join("AGENTS.md"), "Keep it long.\n").unwrap();
    let miss = "lead (mock/lead) read 2.0k tokens from the prompt cache where the request before read 9.0k: \
                the system prompt part `context_files` changed";
    assert_eq!(notices(&client.prompt("third")), [format!("cache miss: {miss}")]);
    let cost = client.cost();
    let lines: Vec<&str> = cost
        .lines()
        .skip_while(|line| !line.starts_with("cache:"))
        .collect();
    assert_eq!(
        lines[..3],
        [
            "cache: 38% of this session's input was read from the prompt cache",
            "  mock/lead: 38% · 11.0k read · 17.1k written · 210 not cached · 3 requests",
            "cache misses:",
        ],
        "{cost}"
    );
    assert!(lines[3].ends_with(miss), "{cost}");
    // Warming is on, and says why it has nothing to do for this provider.
    assert_eq!(
        lines[4],
        "cache warming: on, no request sent yet · mock/lead has no cache that a request can keep alive"
    );
    assert_eq!(lines.len(), 5, "{cost}");
}

#[test]
fn the_instructions_of_a_directory_come_with_the_first_call_there_and_includes_are_part_of_a_file() {
    let script = || {
        json!({ "lead": [
            call("c1", "read", json!({ "path": "pkg/api/a.txt" })),
            call("c2", "read", json!({ "path": "pkg/api/b.txt" })),
            call("c3", "grep", json!({ "pattern": "alpha", "path": "tools" })),
            reply("done", json!({})),
        ] })
    };
    let fill = |project: &std::path::Path| {
        let put = |file: &str, text: &str| {
            let path = project.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        put("AGENTS.md", "Root rules.\n@docs/style.md\n");
        put("docs/style.md", "Tabs, never spaces.\n");
        put("pkg/AGENTS.md", "Package rules: see @api/naming.md\n");
        put("pkg/api/naming.md", "snake_case everywhere.\n");
        put("pkg/api/CLAUDE.md", "Api rules.\n");
        put("pkg/api/a.txt", "alpha\n");
        put("pkg/api/b.txt", "beta\n");
        put("tools/AGENTS.md", "Tool rules.\n");
        put("tools/run.txt", "alpha\n");
    };
    let mut client = Client::start(&script(), &[], fill);
    client.prompt("look around");
    let requests = client.requests();
    assert_eq!(requests.len(), 4);

    // The root file is in the system prompt, with what it includes.
    let context = parts(&requests[0]["system"])
        .into_iter()
        .find(|(name, _)| name == "context_files")
        .unwrap()
        .1;
    assert!(
        context.contains("Root rules.\n<include file=\"docs/style.md\">\nTabs, never spaces.\n</include>"),
        "{context}"
    );
    assert!(!context.contains("Package rules"), "{context}");

    // The first read below `pkg/api` brings both files above it, the nearest first.
    let first = &result_texts(&requests[1])[0];
    let api = first.find("<context file=\"pkg/api/CLAUDE.md\">\nApi rules.\n</context>");
    let pkg = first.find(
        "<context file=\"pkg/AGENTS.md\">\nPackage rules: see @api/naming.md\n\
         <include file=\"pkg/api/naming.md\">\nsnake_case everywhere.\n</include>\n</context>",
    );
    assert!(api.is_some() && pkg.is_some() && api < pkg, "{first}");
    assert!(!first.contains("Root rules"), "{first}");
    // The second read there brings nothing new; a search in another directory brings that one's.
    let results = result_texts(&requests[3]);
    assert!(!results[1].contains("<context"), "{}", results[1]);
    assert!(
        results[2].contains("<context file=\"tools/AGENTS.md\">\nTool rules.\n</context>"),
        "{}",
        results[2]
    );
    drop(client);

    // Switched off, results are what the tools returned and `@path` is text.
    let mut plain = Client::start(
        &script(),
        &[
            "--set",
            "context.nested=false",
            "--set",
            "context.include_depth=0",
        ],
        fill,
    );
    plain.prompt("look around");
    let requests = plain.requests();
    assert!(
        result_texts(&requests[3])
            .iter()
            .all(|text| !text.contains("<context"))
    );
    let context = parts(&requests[0]["system"])
        .into_iter()
        .find(|(name, _)| name == "context_files")
        .unwrap()
        .1;
    assert!(
        context.contains("Root rules.\n@docs/style.md") && !context.contains("Tabs"),
        "{context}"
    );
}
