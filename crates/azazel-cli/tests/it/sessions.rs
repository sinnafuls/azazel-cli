//! Session names, `azazel sessions ...` and `azazel export` against a sandbox home.

use std::path::PathBuf;
use std::process::Output;

use crate::common::{stderr, stdout};
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
        // Session names come from the `smol` role; a queue of its own keeps the lead's apart.
        std::fs::write(
            dir.path().join("home/config.toml"),
            "[roles]\nsmol = [\"mock/namer\"]\n",
        )
        .unwrap();
        Sandbox { dir }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn azazel(&self, script: &Value, args: &[&str]) -> Output {
        let script_path = self.dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let _ = std::fs::remove_file(self.dir.path().join("requests.jsonl"));
        Command::cargo_bin("azazel")
            .unwrap()
            .env("AZAZEL_HOME", self.dir.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", &script_path)
            .env("AZAZEL_MOCK_REQUESTS", self.dir.path().join("requests.jsonl"))
            .arg("--cwd")
            .arg(self.project())
            .args(args)
            .write_stdin("")
            .output()
            .unwrap()
    }

    /// The thinking level of each request the scripted provider received.
    fn thinking_asked(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["request"]["thinking"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    /// Runs `azazel rpc` with `commands` on standard input and returns the lines it wrote.
    fn rpc(&self, script: &Value, args: &[&str], commands: &[Value]) -> Vec<Value> {
        let script_path = self.dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let input: String = commands.iter().map(|command| format!("{command}\n")).collect();
        let output = Command::cargo_bin("azazel")
            .unwrap()
            .env("AZAZEL_HOME", self.dir.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", &script_path)
            .arg("--cwd")
            .arg(self.project())
            .args(args)
            .arg("rpc")
            .write_stdin(input)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        stdout(&output)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn models_asked(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("requests.jsonl"))
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

    fn session_file(&self) -> PathBuf {
        let projects = self.dir.path().join("home/projects");
        let project = std::fs::read_dir(projects)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::read_dir(project.join("sessions"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .unwrap()
    }
}

fn entries(path: &PathBuf) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn a_new_session_is_named_after_its_first_reply_unless_named_by_hand() {
    let sandbox = Sandbox::new();
    let script = json!({
        "lead": [{ "reply": { "text": "The parser now handles empty files." } }],
        "namer": [{ "reply": { "text": "\"Empty file parsing fix.\"" } }]
    });
    let output = sandbox.azazel(
        &script,
        &["-p", "make the parser accept empty files", "--no-memory"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let names: Vec<Value> = entries(&sandbox.session_file())
        .into_iter()
        .filter(|e| e["type"] == "session_info")
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    assert_eq!(names[0]["name"], "Empty file parsing fix");
    assert_eq!(sandbox.models_asked(), ["lead", "namer"]);

    let named = Sandbox::new();
    let output = named.azazel(&script, &["-p", "same again", "--no-memory", "--name", "mine"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(named.models_asked(), ["lead"]);
    let names: Vec<Value> = entries(&named.session_file())
        .into_iter()
        .filter(|e| e["type"] == "session_info")
        .collect();
    assert_eq!(names.len(), 1);
    assert_eq!(names[0]["name"], "mine");
}

#[test]
fn a_reopened_session_comes_back_on_its_model_and_thinking_level() {
    let sandbox = Sandbox::new();
    let reply = |text: &str| json!({ "reply": { "text": text } });
    // Every run reads the script from its start: one reply per model is enough.
    let script = json!({
        "lead": [reply("from the lead")],
        "other": [reply("from the other")],
    });
    let quiet = ["--no-memory", "--set", "session.auto_name=false"];
    let run = |args: &[&str]| {
        let args: Vec<&str> = quiet.iter().chain(args).copied().collect();
        let output = sandbox.azazel(&script, &args);
        assert!(output.status.success(), "{}", stderr(&output));
        (stdout(&output).trim().to_string(), stderr(&output))
    };

    // The model and thinking level chosen at launch are written into the session.
    let (said, _) = run(&["--model", "mock/other", "--thinking", "high", "-p", "start"]);
    assert_eq!(said, "from the other");
    let file = sandbox.session_file();
    let kinds: Vec<String> = entries(&file)[1..]
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds[..2], ["model_change", "thinking_change"], "{kinds:?}");

    // Continued without flags, it is on both again.
    let (said, _) = run(&["-c", "-p", "go on"]);
    assert_eq!(said, "from the other");
    assert_eq!(
        (sandbox.models_asked(), sandbox.thinking_asked()),
        (vec!["other".to_string()], vec!["high".to_string()])
    );
    // Nothing changed, so nothing was written again.
    let changes = |file: &PathBuf| {
        entries(file)
            .iter()
            .filter(|e| e["type"] == "model_change" || e["type"] == "thinking_change")
            .count()
    };
    assert_eq!(changes(&file), 2);

    // RPC reports the restored state before any prompt.
    let lines = sandbox.rpc(&script, &["-c"], &[json!({ "id": 1, "type": "get_state" })]);
    let state = lines
        .iter()
        .find(|line| line["type"] == "response" && line["id"] == 1)
        .unwrap();
    assert_eq!(state["data"]["thinking"], "high", "{state}");

    // A flag wins over what the session last used, and `restore_model = false` switches it off.
    let (said, _) = run(&["-c", "--thinking", "low", "-p", "quick one"]);
    assert_eq!(said, "from the other");
    assert_eq!(sandbox.thinking_asked(), ["low"]);
    let (said, _) = run(&["-c", "--set", "session.restore_model=false", "-p", "plain"]);
    assert_eq!(said, "from the lead");
    assert_eq!(sandbox.thinking_asked(), ["off"]);

    // A model that is gone gives way to the default one, and says so.
    let last = entries(&file).last().unwrap()["id"].as_str().unwrap().to_string();
    let gone = json!({
        "id": "feedbeef", "parentId": last, "timestamp": "2026-11-12T10:00:00Z",
        "type": "model_change", "chain": ["@retired"],
    });
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str(&format!("{gone}\n"));
    std::fs::write(&file, text).unwrap();
    let (said, warned) = run(&["-c", "-p", "still there?"]);
    assert_eq!(said, "from the lead");
    assert!(
        warned.contains("last used @retired, which cannot be used now"),
        "{warned}"
    );
    // The thinking level it last switched to still applies.
    assert_eq!(sandbox.thinking_asked(), ["low"]);
}

#[test]
fn the_task_list_is_back_when_a_session_is_continued_and_the_agent_is_reminded_of_it() {
    let sandbox = Sandbox::new();
    let quiet = ["--no-memory", "--set", "session.auto_name=false"];
    let run = |script: &Value, args: &[&str]| {
        let args: Vec<&str> = quiet.iter().chain(args).copied().collect();
        let output = sandbox.azazel(script, &args);
        assert!(output.status.success(), "{}", stderr(&output));
    };
    // What each request the scripted provider received ended with: the note about the plan that
    // follows the conversation, or without one the last message.
    let last_sent = || -> Vec<String> {
        std::fs::read_to_string(sandbox.dir.path().join("requests.jsonl"))
            .unwrap()
            .lines()
            .map(|line| {
                let request: Value = serde_json::from_str(line).unwrap();
                let request = &request["request"];
                let note = request["tail"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|note| note["name"] == "plan");
                let last = request["messages"].as_array().unwrap().last().unwrap();
                note.map_or(&last["content"][0]["text"], |note| &note["text"])
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    };
    let todo = |id: &str, items: Value| json!({ "reply": { "tool_calls": [{ "id": id, "name": "todo", "input": { "items": items } }] } });
    let reply = |text: &str| json!({ "reply": { "text": text } });
    let plan = json!([
        { "text": "find the bug", "status": "done" },
        { "text": "fix it", "status": "in_progress" },
        { "text": "add a test", "status": "pending" }
    ]);
    let reminder = "[Plan] 1/3 done (your todo list; keep it current with `todo`)\n\
                    [x] find the bug\n[~] fix it\n[ ] add a test";

    run(
        &json!({ "lead": [todo("c1", plan.clone()), reply("planned")] }),
        &["-p", "plan the fix"],
    );
    // Before there is a list nothing is added; once there is, it follows what was sent.
    assert_eq!(last_sent(), ["plan the fix", reminder]);

    // Another process, the same session: the list is there before the agent has done anything.
    let lines = sandbox.rpc(
        &json!({ "lead": [] }),
        &["-c"],
        &[json!({ "id": 1, "type": "get_state" })],
    );
    let state = lines
        .iter()
        .find(|line| line["type"] == "response" && line["id"] == 1)
        .unwrap();
    assert_eq!(state["data"]["todo"], plan, "{state}");

    run(&json!({ "lead": [reply("going on")] }), &["-c", "-p", "go on"]);
    assert_eq!(last_sent(), [reminder]);
    // The reminder is sent, never kept.
    let file = sandbox.session_file();
    assert!(!std::fs::read_to_string(&file).unwrap().contains("[Plan]"));

    // With everything done the agent is left alone, in this run and in the next.
    let done = json!([
        { "text": "find the bug", "status": "done" },
        { "text": "fix it", "status": "done" },
        { "text": "add a test", "status": "done" }
    ]);
    run(
        &json!({ "lead": [todo("c2", done), reply("all done")] }),
        &["-c", "-p", "finish"],
    );
    assert_eq!(last_sent()[0], reminder);
    assert!(!last_sent()[1].contains("[Plan]"), "{:?}", last_sent());
    run(
        &json!({ "lead": [reply("nothing left")] }),
        &["-c", "-p", "anything else?"],
    );
    assert_eq!(last_sent(), ["anything else?"]);

    // A new session starts without a list.
    let lines = sandbox.rpc(
        &json!({ "lead": [] }),
        &[],
        &[json!({ "id": 1, "type": "get_state" })],
    );
    let state = lines
        .iter()
        .find(|line| line["type"] == "response" && line["id"] == 1)
        .unwrap();
    assert_eq!(state["data"]["todo"], json!([]), "{state}");
}

#[test]
fn sessions_are_listed_searched_exported_pruned_and_deleted_from_the_shell() {
    let sandbox = Sandbox::new();
    let script = json!({ "lead": [{ "reply": { "text": "Answer about <tokio>." } }] });
    let output = sandbox.azazel(
        &script,
        &["-p", "explain the tokio runtime", "--no-memory", "--name", "rt"],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let list = sandbox.azazel(&script, &["sessions", "list", "--json"]);
    assert!(list.status.success(), "{}", stderr(&list));
    let rows: Vec<Value> = stdout(&list)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "rt");
    let id = rows[0]["id"].as_str().unwrap().to_string();

    let search = sandbox.azazel(&script, &["sessions", "search", "TOKIO RUNTIME"]);
    assert!(
        stdout(&search).contains("explain the tokio runtime"),
        "{}",
        stdout(&search)
    );
    assert!(stdout(&search).contains(" · rt · "), "{}", stdout(&search));

    let html = sandbox.azazel(&script, &["export", &id[..8], "-o", "out.html"]);
    assert!(html.status.success(), "{}", stderr(&html));
    let page = std::fs::read_to_string(sandbox.project().join("out.html")).unwrap();
    assert!(!page.contains("<script") && page.contains("Answer about &lt;tokio&gt;."));
    let lines = sandbox.azazel(&script, &["sessions", "export", "--format", "jsonl", "-o", "-"]);
    assert!(stdout(&lines).lines().count() >= 3, "{}", stdout(&lines));

    let dry = sandbox.azazel(&script, &["sessions", "prune", "--older-than", "0d", "--dry-run"]);
    assert!(
        stdout(&dry).contains("1 session(s) would be deleted"),
        "{}",
        stdout(&dry)
    );
    assert!(sandbox.session_file().is_file());

    let refused = sandbox.azazel(&script, &["sessions", "delete", &id]);
    assert_eq!(refused.status.code(), Some(2), "{}", stderr(&refused));
    let deleted = sandbox.azazel(&script, &["sessions", "delete", &id, "--yes"]);
    assert!(deleted.status.success(), "{}", stderr(&deleted));
    let list = sandbox.azazel(&script, &["sessions", "list", "--json"]);
    assert!(stdout(&list).trim().is_empty());
}
