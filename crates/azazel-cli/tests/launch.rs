#![allow(clippy::unwrap_used)]

//! Launch flags: the positional prompt, `@file`, tool selection, system prompt flags, discovery
//! switches, session flags, the cost cap, model names and the listing commands.

use std::path::{Path, PathBuf};
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
        Sandbox { dir }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn requests_file(&self) -> PathBuf {
        self.dir.path().join("requests.jsonl")
    }

    /// `azazel` in this sandbox answering from `script`, with every request recorded.
    fn azazel(&self, script: &Value, args: &[&str]) -> Output {
        let script_path = self.dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let _ = std::fs::remove_file(self.requests_file());
        Command::cargo_bin("azazel")
            .unwrap()
            .env("AZAZEL_HOME", self.dir.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", &script_path)
            .env("AZAZEL_MOCK_REQUESTS", self.requests_file())
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .arg("--cwd")
            .arg(self.project())
            .args(args)
            .write_stdin("")
            .output()
            .unwrap()
    }

    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.requests_file())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
            .collect()
    }

    fn session_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let projects = self.dir.path().join("home/projects");
        for project in std::fs::read_dir(projects).into_iter().flatten().flatten() {
            for file in std::fs::read_dir(project.path().join("sessions"))
                .into_iter()
                .flatten()
                .flatten()
            {
                if file.path().extension().is_some_and(|e| e == "jsonl") {
                    files.push(file.path());
                }
            }
        }
        files.sort();
        files
    }
}

fn ok(text: &str) -> Value {
    json!({ "lead": [{ "reply": { "text": text } }] })
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn first_user_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap()["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("")
}

fn system(request: &Value) -> Vec<String> {
    request["system"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| part["text"].as_str().unwrap().to_string())
        .collect()
}

fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn the_prompt_is_the_words_after_the_flags_quoted_or_not() {
    let sandbox = Sandbox::new();
    let joined = sandbox.azazel(&ok("a"), &["-p", "hello", "world", "--mode", "auto"]);
    assert!(joined.status.success(), "{}", stderr(&joined));
    let first = first_user_text(&sandbox.requests()[0]);

    let quoted = sandbox.azazel(&ok("b"), &["-p", "hello world"]);
    assert!(quoted.status.success(), "{}", stderr(&quoted));
    assert_eq!(first_user_text(&sandbox.requests()[0]), first);
    assert_eq!(first, "hello world");

    // `--` makes a subcommand's name part of the prompt.
    let forced = sandbox.azazel(&ok("c"), &["-p", "--", "init", "x"]);
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert_eq!(first_user_text(&sandbox.requests()[0]), "init x");

    let empty = sandbox.azazel(&ok("d"), &["-p"]);
    assert_eq!(empty.status.code(), Some(2));
    assert!(stderr(&empty).contains("-p needs a prompt"));
}

#[test]
fn a_mentioned_image_is_attached_to_the_prompt() {
    let sandbox = Sandbox::new();
    let mut png = Vec::new();
    image::RgbaImage::new(3, 2)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    std::fs::write(sandbox.project().join("tiny.png"), &png).unwrap();

    let output = sandbox.azazel(&ok("seen"), &["-p", "what is in @tiny.png"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let request = &sandbox.requests()[0];
    let user = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap();
    let images: Vec<&Value> = user["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == "image")
        .collect();
    assert_eq!(images.len(), 1, "{user}");
    assert_eq!(images[0]["mime"], "image/png");
}

#[test]
fn tools_can_be_chosen_removed_or_all_turned_off() {
    let sandbox = Sandbox::new();
    let output = sandbox.azazel(&ok("x"), &["-p", "hi", "--tools", "read,grep"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(tool_names(&sandbox.requests()[0]), ["read", "grep"]);

    let output = sandbox.azazel(
        &ok("x"),
        &["-p", "hi", "--tools", "w*", "--disallowed-tools", "web_*"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let names = tool_names(&sandbox.requests()[0]);
    assert!(names.contains(&"write".to_string()), "{names:?}");
    assert!(
        names.iter().all(|n| n.starts_with('w') && !n.starts_with("web_")),
        "{names:?}"
    );

    let output = sandbox.azazel(&ok("x"), &["-p", "hi", "--no-tools"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(tool_names(&sandbox.requests()[0]).is_empty());

    let unknown = sandbox.azazel(&ok("x"), &["-p", "hi", "--tools", "nope"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(
        stderr(&unknown).contains("no tool named `nope`"),
        "{}",
        stderr(&unknown)
    );
    assert!(sandbox.requests().is_empty());
}

#[test]
fn the_system_prompt_can_be_replaced_or_extended() {
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.project().join("AGENTS.md"), "Use tabs.\n").unwrap();
    std::fs::write(sandbox.project().join("extra.md"), "Answer in French.\n").unwrap();

    let output = sandbox.azazel(
        &ok("x"),
        &["-p", "hi", "--system-prompt", "You are a test robot."],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let parts = system(&sandbox.requests()[0]);
    assert_eq!(parts[0], "You are a test robot.");
    assert!(!parts.iter().any(|p| p.contains("You are azazel")), "{parts:?}");
    assert!(!parts.iter().any(|p| p.contains("# Rules")), "{parts:?}");

    let output = sandbox.azazel(&ok("x"), &["-p", "hi", "--append-system-prompt", "@extra.md"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let parts = system(&sandbox.requests()[0]);
    assert!(parts[0].contains("You are azazel"), "{parts:?}");
    let context = parts.iter().position(|p| p.contains("Use tabs.")).unwrap();
    let appended = parts
        .iter()
        .position(|p| p.contains("Answer in French."))
        .unwrap();
    assert!(appended > context, "{parts:?}");

    let missing = sandbox.azazel(&ok("x"), &["-p", "hi", "--system-prompt", "@nowhere.md"]);
    assert_eq!(missing.status.code(), Some(2));
}

#[test]
fn bare_leaves_out_skills_and_context_files() {
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.project().join("AGENTS.md"), "Use tabs.\n").unwrap();
    let full = sandbox.azazel(&ok("x"), &["-p", "hi"]);
    assert!(full.status.success(), "{}", stderr(&full));
    let parts = system(&sandbox.requests()[0]).join("\n");
    assert!(
        parts.contains("Use tabs.") && parts.contains("# Skills"),
        "{parts}"
    );
    assert!(tool_names(&sandbox.requests()[0]).contains(&"skill".to_string()));

    let bare = sandbox.azazel(&ok("x"), &["-p", "hi", "--bare"]);
    assert!(bare.status.success(), "{}", stderr(&bare));
    let request = &sandbox.requests()[0];
    let parts = system(request).join("\n");
    assert!(
        !parts.contains("Use tabs.") && !parts.contains("# Skills"),
        "{parts}"
    );
    let names = tool_names(request);
    assert!(!names.contains(&"skill".to_string()), "{names:?}");
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with("memory_") || n.starts_with("mcp")),
        "{names:?}"
    );
}

#[test]
fn sessions_can_be_skipped_named_reopened_by_path_and_forked() {
    let sandbox = Sandbox::new();
    let unsaved = sandbox.azazel(&ok("x"), &["-p", "hi", "--no-session"]);
    assert!(unsaved.status.success(), "{}", stderr(&unsaved));
    assert!(sandbox.session_files().is_empty());
    assert!(
        !sandbox
            .dir
            .path()
            .join("home/cache/ephemeral")
            .read_dir()
            .unwrap()
            .any(|_| true)
    );

    let named = sandbox.azazel(&ok("first"), &["-p", "one", "--name", "my work"]);
    assert!(named.status.success(), "{}", stderr(&named));
    let [original] = sandbox.session_files().try_into().unwrap();
    assert!(
        std::fs::read_to_string(&original)
            .unwrap()
            .contains("\"my work\"")
    );

    let script = json!({ "lead": [{ "reply": { "text": "second" } }] });
    let path = original.to_string_lossy().into_owned();
    let reopened = sandbox.azazel(&script, &["-p", "two", "--session", &path]);
    assert!(reopened.status.success(), "{}", stderr(&reopened));
    assert_eq!(sandbox.session_files().len(), 1);
    // The reopened session carries the first exchange into the request.
    assert_eq!(sandbox.requests()[0]["messages"].as_array().unwrap().len(), 3);

    let forked = sandbox.azazel(&script, &["-p", "three", "--fork", &path]);
    assert!(forked.status.success(), "{}", stderr(&forked));
    let files = sandbox.session_files();
    assert_eq!(files.len(), 2);
    let copy = files.iter().find(|f| **f != original).unwrap();
    let header: Value =
        serde_json::from_str(std::fs::read_to_string(copy).unwrap().lines().next().unwrap()).unwrap();
    assert_eq!(Path::new(header["parentSession"].as_str().unwrap()), original);

    let elsewhere = sandbox.dir.path().join("kept");
    let moved = sandbox.azazel(
        &ok("x"),
        &["-p", "hi", "--session-dir", elsewhere.to_str().unwrap()],
    );
    assert!(moved.status.success(), "{}", stderr(&moved));
    // One session file; its folder beside it holds what its last request was made of.
    let kept: Vec<PathBuf> = std::fs::read_dir(&elsewhere)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    let files: Vec<&PathBuf> = kept.iter().filter(|path| path.is_file()).collect();
    assert_eq!(files.len(), 1, "{kept:?}");
    assert_eq!(files[0].extension().unwrap(), "jsonl");
    assert_eq!(sandbox.session_files().len(), 2);
}

#[test]
fn the_cost_cap_stops_the_run_after_the_turn_that_reached_it() {
    let sandbox = Sandbox::new();
    let script = json!({ "lead": [
        { "reply": {
            "tool_calls": [{ "id": "c1", "name": "read", "input": { "path": "." } }],
            "usage": { "input": 10, "output": 5, "cost_usd": 0.02 }
        } },
        { "reply": { "text": "never sent" } }
    ] });
    let output = sandbox.azazel(&script, &["-p", "look", "--max-cost", "0.01", "--mode", "auto"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("the cost limit of $0.01 is reached ($0.02 spent)"),
        "{}",
        stderr(&output)
    );
    assert_eq!(sandbox.requests().len(), 1);
}

#[test]
fn settings_can_be_changed_for_one_run() {
    let sandbox = Sandbox::new();
    let output = sandbox.azazel(&ok("x"), &["-p", "hi", "--set", "tools.disabled=[\"shell\"]"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!tool_names(&sandbox.requests()[0]).contains(&"shell".to_string()));

    let wrong = sandbox.azazel(&ok("x"), &["-p", "hi", "--set", "nonsense"]);
    assert_eq!(wrong.status.code(), Some(2));
    assert!(stderr(&wrong).contains("--set"), "{}", stderr(&wrong));
}

#[test]
fn model_names_may_be_partial() {
    let sandbox = Sandbox::new();
    std::fs::write(
        sandbox.dir.path().join("home/config.toml"),
        "[models.\"mock/lead-one\"]\n[models.\"mock/other\"]\n",
    )
    .unwrap();
    let script = json!({ "lead-one": [{ "reply": { "text": "from one" } }] });
    let output = sandbox.azazel(&script, &["-p", "hi", "--model", "lead-o", "--output", "json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("from one"));

    std::fs::write(
        sandbox.dir.path().join("home/config.toml"),
        "[models.\"mock/lead-one\"]\n[models.\"mock/lead-two\"]\n",
    )
    .unwrap();
    let tied = sandbox.azazel(&script, &["-p", "hi", "--model", "lead-"]);
    assert_eq!(tied.status.code(), Some(2));
    let error = stderr(&tied);
    assert!(
        error.contains("mock/lead-one") && error.contains("mock/lead-two"),
        "{error}"
    );
}

#[test]
fn models_and_completions_print_for_scripts() {
    let sandbox = Sandbox::new();
    let output = sandbox.azazel(
        &ok("x"),
        &["models", "--all", "--provider", "anthropic", "--json"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let lines: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(!lines.is_empty());
    assert!(
        lines
            .iter()
            .all(|m| m["model"].as_str().unwrap().starts_with("anthropic/"))
    );

    let completions = sandbox.azazel(&ok("x"), &["completions", "bash"]);
    assert!(completions.status.success());
    assert!(String::from_utf8_lossy(&completions.stdout).contains("_azazel"));
}
