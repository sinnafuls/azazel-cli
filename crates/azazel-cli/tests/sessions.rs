#![allow(clippy::unwrap_used)]

//! Session names, `azazel sessions ...` and `azazel export` against a sandbox home.

use std::path::PathBuf;
use std::process::Output;

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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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
