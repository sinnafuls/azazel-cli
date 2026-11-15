//! Rule files and stream rules, the newer hook options and events, the destructive-command
//! gate, keys from `.env`, and the approval and import of MCP servers.

use std::path::PathBuf;
use std::process::Output;

use crate::common::{call, stderr, stdout, system, text};
use assert_cmd::Command;
use azazel_core::TrustStore;
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

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn write(&self, relative: &str, text: &str) {
        let file = self.path(relative);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }

    fn read(&self, relative: &str) -> Option<String> {
        std::fs::read_to_string(self.path(relative)).ok()
    }

    /// Records the user's decision to trust the project, as the trust prompt would.
    fn trust(&self) {
        TrustStore::new(self.path("home/trust.json"))
            .set(&self.path("project"), true)
            .unwrap();
    }

    /// `azazel` in this sandbox, with no key in the environment and the user's own directories
    /// pointed into the sandbox.
    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.path("home"))
            .env("HOME", self.path("user"))
            .env("USERPROFILE", self.path("user"))
            .env("APPDATA", self.path("user/appdata"))
            .env("XDG_CONFIG_HOME", self.path("user/config"))
            .env_remove("AZAZEL_MOCK_SCRIPT")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .arg("--cwd")
            .arg(self.path("project"));
        command
    }

    /// A print-mode run answering from `script`, with every request recorded.
    fn run(&self, script: &Value, args: &[&str]) -> Output {
        self.write("script.json", &script.to_string());
        let _ = std::fs::remove_file(self.path("requests.jsonl"));
        self.command()
            .env("AZAZEL_MOCK_SCRIPT", self.path("script.json"))
            .env("AZAZEL_MOCK_REQUESTS", self.path("requests.jsonl"))
            .args(args)
            .write_stdin("")
            .output()
            .unwrap()
    }

    /// The lead's requests, in order. What runs after the answer (memory extraction) asks the
    /// model too; those requests offer no tools and are left out.
    fn requests(&self) -> Vec<Value> {
        self.read("requests.jsonl")
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
            .filter(|request| request["tools"].as_array().is_some_and(|tools| !tools.is_empty()))
            .collect()
    }

    /// The text of every session file of the sandbox.
    fn transcripts(&self) -> String {
        let mut text = String::new();
        for project in std::fs::read_dir(self.path("home/projects")).unwrap().flatten() {
            for file in std::fs::read_dir(project.path().join("sessions"))
                .unwrap()
                .flatten()
            {
                if file.path().extension().is_some_and(|e| e == "jsonl") {
                    text.push_str(&std::fs::read_to_string(file.path()).unwrap());
                }
            }
        }
        text
    }
}

/// The text of every block of every message of a request, tool results included.
fn message_texts(request: &Value) -> Vec<String> {
    fn collect(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(fields) => {
                if let Some(Value::String(text)) = fields.get("text") {
                    out.push(text.clone());
                }
                fields.values().for_each(|inner| collect(inner, out));
            }
            Value::Array(items) => items.iter().for_each(|inner| collect(inner, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    collect(&request["messages"], &mut out);
    out
}

#[test]
fn a_stream_rule_cuts_the_reply_once_and_the_retry_carries_the_rule() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "project/.azazel/rules/no-todo.md",
        "---\ntrigger: TODO\n---\nNever leave a TODO: finish the work or say what is missing.\n",
    );
    sandbox.write(
        "home/config.toml",
        "[[stream_rules]]\npattern = \"(?i)as an ai\"\nmessage = \"Do not talk about being an AI.\"\n",
    );
    let script = json!({ "lead": [
        text("I left a TODO in main.rs"),
        text("As an AI, I finished main.rs"),
        text("main.rs is finished, with one more TODO"),
    ] });
    let output = sandbox.run(&script, &["-p", "finish main.rs", "--output", "json"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let requests = sandbox.requests();
    assert_eq!(requests.len(), 3, "{}", stdout(&output));
    let second = message_texts(&requests[1]).join("\n");
    assert!(
        second.contains("<rule name=\"no-todo\" reason=\"matched\">\nNever leave a TODO"),
        "{second}"
    );
    assert!(second.contains("not a prompt injection"));
    assert!(!second.contains("I left a TODO"), "{second}");
    let third = message_texts(&requests[2]).join("\n");
    assert!(
        third.contains("<rule name=\"stream-rule-1\"") && third.contains("Do not talk about being an AI.")
    );
    // The third reply says TODO again, but each rule fires once and two replies were withdrawn.
    let events = stdout(&output);
    assert_eq!(events.matches("\"type\":\"rule\"").count(), 2, "{events}");
    assert!(events.contains("\"name\":\"no-todo\""), "{events}");
    // Neither withdrawn reply was stored.
    let stored = sandbox.transcripts();
    assert!(!stored.contains("I left a TODO in main.rs") && !stored.contains("As an AI, I finished"));
    assert!(stored.contains("with one more TODO"));
}

#[test]
fn rules_reach_the_agent_always_by_path_and_on_request() {
    let sandbox = Sandbox::new();
    sandbox.write("project/.azazel/rules/tone.md", "Answer in short sentences.\n");
    sandbox.write(
        "project/.azazel/rules/notes.md",
        "---\npaths: [\"**/*.txt\"]\n---\nNotes are plain text: no Markdown.\n",
    );
    sandbox.write(
        "project/.azazel/rules/release.md",
        "---\ndescription: how to cut a release\n---\nTag, then build.\n",
    );
    sandbox.write(".cursorrules-elsewhere", "not read");
    sandbox.write("project/.cursorrules", "Cursor says: prefer early returns.\n");
    sandbox.write("project/notes.txt", "remember the milk\n");
    let script = json!({ "lead": [
        call("c1", "read", json!({ "path": "notes.txt" })),
        call("c2", "read", json!({ "path": "notes.txt" })),
        call("c3", "read", json!({ "path": "rule://release" })),
        text("done"),
    ] });
    let output = sandbox.run(&script, &["-p", "read the notes"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let requests = sandbox.requests();
    assert_eq!(requests.len(), 4);
    let prompt = system(&requests[0]);
    assert!(prompt.contains("Answer in short sentences."), "{prompt}");
    assert!(prompt.contains("Cursor says: prefer early returns."));
    assert!(prompt.contains("# Rules on request") && prompt.contains("- release: how to cut a release"));
    assert!(!prompt.contains("no Markdown") && !prompt.contains("Tag, then build."));
    // The path rule comes with the first matching read, and only with the first.
    let after_first = message_texts(&requests[1]).join("\n");
    assert_eq!(
        after_first.matches("Notes are plain text: no Markdown.").count(),
        1
    );
    assert!(after_first.contains("<rule file="));
    let after_second = message_texts(&requests[2]).join("\n");
    assert_eq!(
        after_second.matches("Notes are plain text: no Markdown.").count(),
        1
    );
    let after_third = message_texts(&requests[3]).join("\n");
    assert!(after_third.contains("Tag, then build."));

    // Rule files are context files: the switch that turns those off turns rules off too.
    let bare = sandbox.run(
        &json!({ "lead": [text("done")] }),
        &["-p", "hi", "--no-context-files"],
    );
    assert!(bare.status.success(), "{}", stderr(&bare));
    let prompt = system(&sandbox.requests()[0]);
    assert!(!prompt.contains("Answer in short sentences.") && !prompt.contains("Rules on request"));

    let listed = sandbox.command().args(["rules", "list"]).output().unwrap();
    let list = stdout(&listed);
    assert!(listed.status.success(), "{}", stderr(&listed));
    for line in [
        "tone  (project)  always",
        "notes  (project)  when a file matching **/*.txt is touched",
        "release  (project)  on request (rule://release)",
        "cursorrules  (cursor)  always",
    ] {
        assert!(list.contains(line), "missing `{line}` in:\n{list}");
    }

    let rule = ".azazel/rules/notes.md";
    let applies = sandbox
        .command()
        .args(["rules", "test", rule, "--path", "docs/a.txt"])
        .output()
        .unwrap();
    assert!(applies.status.success(), "{}", stderr(&applies));
    assert!(stdout(&applies).contains("path docs/a.txt: applies"));
    let not = sandbox
        .command()
        .args(["rules", "test", rule, "--path", "src/main.rs"])
        .output()
        .unwrap();
    assert_eq!(not.status.code(), Some(1));
    assert!(stdout(&not).contains("path src/main.rs: does not apply"));
}

#[test]
fn a_wake_hook_reaches_the_lead_and_a_changed_file_fires_its_event() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "home/config.toml",
        r#"
[[hooks.post_tool_use]]
matcher = "write"
command = "echo 'the linter found 2 warnings'"
async = true
wake = true

[[hooks.file_changed]]
command = "cat > changed.json"

[[hooks.pre_tool_use]]
command = "echo ran >> once.txt"
once = true
"#,
    );
    // The second reply takes long enough for the hook's result to arrive while the lead works.
    let script = json!({ "lead": [
        call("c1", "write", json!({ "path": "hello.txt", "content": "hello\n" })),
        { "delay_ms": 6000, "reply": { "tool_calls": [
            { "id": "c2", "name": "read", "input": { "path": "hello.txt" } }
        ] } },
        text("wrote hello.txt and saw the linter's warnings"),
    ] });
    let output = sandbox.run(&script, &["-p", "write hello.txt", "--mode", "auto"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let requests = sandbox.requests();
    assert_eq!(requests.len(), 3);
    let last = message_texts(&requests[2]).join("\n");
    assert!(
        last.contains("<hook-result event=\"post_tool_use\">\nthe linter found 2 warnings\n</hook-result>"),
        "{last}"
    );
    let changed: Value = serde_json::from_str(&sandbox.read("project/changed.json").unwrap()).unwrap();
    assert_eq!(changed["hook_event_name"], "FileChanged");
    assert_eq!(changed["tool_name"], "write");
    assert!(
        changed["file_path"]
            .as_str()
            .unwrap()
            .replace('\\', "/")
            .ends_with("project/hello.txt"),
        "{changed}"
    );
    // Two tool calls, one run of the `once` hook.
    assert_eq!(sandbox.read("project/once.txt").unwrap().lines().count(), 1);
}

#[test]
fn a_destructive_command_is_not_run_unattended_even_in_auto_mode() {
    let sandbox = Sandbox::new();
    sandbox.write("project/keep.txt", "precious\n");
    let script = json!({ "lead": [
        call("c1", "shell", json!({ "command": "git clean -fdx" })),
        text("I could not clean the tree"),
    ] });
    let output = sandbox.run(&script, &["-p", "clean up", "--mode", "auto"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let result = message_texts(&sandbox.requests()[1]).join("\n");
    assert!(
        result.contains(
            "this command deletes untracked files (git clean -f); it needs the user's confirmation"
        ),
        "{result}"
    );
    assert_eq!(sandbox.read("project/keep.txt").as_deref(), Some("precious\n"));

    // The setting can let such commands through.
    let script = json!({ "lead": [
        call("c1", "shell", json!({ "command": "rm -rf ../scratch-that-is-not-there" })),
        text("cleared"),
    ] });
    let allowed = sandbox.run(
        &script,
        &[
            "-p",
            "clear the stash",
            "--mode",
            "auto",
            "--set",
            "permissions.destructive=\"allow\"",
        ],
    );
    assert!(allowed.status.success(), "{}", stderr(&allowed));
    let result = message_texts(&sandbox.requests()[1]).join("\n");
    assert!(!result.contains("needs the user's confirmation"), "{result}");
}

#[test]
fn a_key_in_the_projects_env_file_counts_only_once_the_project_is_trusted() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "project/.env",
        "# local keys\nOPENAI_API_KEY=sk-from-project\nDATABASE_URL=postgres://x\n",
    );
    let status = |args: &[&str]| {
        let output = sandbox
            .command()
            .args(args)
            .args(["auth", "status", "--json"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        stdout(&output)
    };
    let untrusted = status(&[]);
    assert!(!untrusted.contains("\"openai\""), "{untrusted}");

    let trusted = status(&["--trust"]);
    let row: Value = trusted
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|row| row["provider"] == "openai")
        .unwrap_or_else(|| panic!("no openai row in: {trusted}"));
    let source = row["source"].as_str().unwrap();
    assert!(
        source.starts_with("OPENAI_API_KEY in ") && source.ends_with(".env"),
        "{source}"
    );
    assert!(!trusted.contains("sk-from-project"));

    // The home file needs no trust, and a real variable still wins over both.
    sandbox.write("home/.env", "export ANTHROPIC_API_KEY=\"sk-from-home\"\n");
    let home = status(&[]);
    assert!(
        home.contains("ANTHROPIC_API_KEY in ") && !home.contains("sk-from-home"),
        "{home}"
    );
    let output = sandbox
        .command()
        .env("ANTHROPIC_API_KEY", "sk-real")
        .args(["auth", "status"])
        .output()
        .unwrap();
    assert!(
        stdout(&output).contains("env ANTHROPIC_API_KEY"),
        "{}",
        stdout(&output)
    );
}

#[cfg(feature = "mcp")]
#[test]
fn a_projects_mcp_server_waits_for_approval_and_again_after_it_changes() {
    let sandbox = Sandbox::new();
    sandbox.trust();
    let define = |arg: &str| {
        sandbox.write(
            "project/.azazel/mcp.json",
            &json!({ "mcpServers": { "local": { "command": "no-such-mcp-binary", "args": [arg] } } })
                .to_string(),
        );
    };
    sandbox.write(
        "home/mcp.json",
        &json!({ "mcpServers": { "mine": { "command": "no-such-mcp-binary" } } }).to_string(),
    );
    let mcp = |args: &[&str]| {
        let output = sandbox.command().arg("mcp").args(args).output().unwrap();
        (output.status.code(), stdout(&output), stderr(&output))
    };
    define("one");

    let (_, listed, _) = mcp(&["list"]);
    assert!(listed.contains("local  waiting for approval\n"), "{listed}");
    // The user's own servers never wait.
    assert!(
        listed.contains("mine  failed: `no-such-mcp-binary` was not found on PATH"),
        "{listed}"
    );
    assert!(listed.contains("azazel mcp approve <name>"));
    let doctor = sandbox.command().arg("doctor").output().unwrap();
    assert!(
        stdout(&doctor).contains("warn  mcp server local: no-such-mcp-binary (waiting for approval"),
        "{}",
        stdout(&doctor)
    );

    let (code, said, err) = mcp(&["approve", "local"]);
    assert_eq!((code, said.trim()), (Some(0), "approved local"), "{err}");
    let (_, listed, _) = mcp(&["list"]);
    assert!(
        listed.contains("local  failed: `no-such-mcp-binary` was not found on PATH"),
        "{listed}"
    );

    // The approval was for that definition: a changed one waits again.
    define("two");
    let (_, listed, _) = mcp(&["list"]);
    assert!(listed.contains("local  waiting for approval\n"), "{listed}");
    let (code, _, _) = mcp(&["approve", "--all"]);
    assert_eq!(code, Some(0));
    let (code, said, _) = mcp(&["reject", "local"]);
    assert_eq!((code, said.trim()), (Some(0), "rejected local"));
    let (_, listed, _) = mcp(&["list"]);
    assert!(listed.contains("local  rejected\n"), "{listed}");

    // Only the project's servers are anyone's to approve.
    let (code, _, err) = mcp(&["approve", "mine"]);
    assert_eq!(code, Some(1));
    assert!(err.contains("`mine` is not a server of this project"), "{err}");
    // `--trust` approves every one for that run.
    let trusted = sandbox
        .command()
        .args(["--trust", "mcp", "list"])
        .output()
        .unwrap();
    assert!(
        stdout(&trusted).contains("local  failed:"),
        "{}",
        stdout(&trusted)
    );
}

#[cfg(feature = "mcp")]
#[test]
fn servers_of_another_client_are_imported_without_replacing_a_name() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "user/.cursor/mcp.json",
        r#"{ "mcpServers": {
            "search": { "url": "https://search.example/mcp", "headers": { "Authorization": "Bearer t0ken" } },
            "files": { "command": "cursor-files", "args": ["--root", "."] } } }"#,
    );
    sandbox.write(
        "home/mcp.json",
        r#"{ "mcpServers": { "files": { "command": "my-files" } } }"#,
    );
    // Without `--yes` and without a terminal nothing is written.
    let listed = sandbox
        .command()
        .args(["mcp", "import", "cursor"])
        .write_stdin("")
        .output()
        .unwrap();
    let said = stdout(&listed);
    assert!(
        said.contains("1. search  https://search.example/mcp  (cursor, "),
        "{said}"
    );
    assert!(said.contains("2. files  cursor-files --root .") && said.contains("already configured"));
    assert!(said.contains("pass --yes to import them all"));
    assert!(!said.contains("t0ken"));

    let imported = sandbox
        .command()
        .args(["mcp", "import", "--yes"])
        .write_stdin("")
        .output()
        .unwrap();
    assert!(imported.status.success(), "{}", stderr(&imported));
    let said = stdout(&imported);
    assert!(said.contains("imported `search` from cursor"), "{said}");
    assert!(said.contains("kept `files` as it is"), "{said}");
    let written: Value = serde_json::from_str(&sandbox.read("home/mcp.json").unwrap()).unwrap();
    assert_eq!(written["mcpServers"]["files"]["command"], "my-files");
    assert_eq!(
        written["mcpServers"]["search"]["url"],
        "https://search.example/mcp"
    );
    assert_eq!(
        written["mcpServers"]["search"]["headers"]["Authorization"],
        "Bearer t0ken"
    );

    let unknown = sandbox
        .command()
        .args(["mcp", "import", "emacs"])
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(1));
    assert!(stderr(&unknown).contains("unknown client `emacs`"));
}

#[test]
fn setup_hooks_run_the_first_time_a_project_runs_trusted() {
    let sandbox = Sandbox::new();
    sandbox.trust();
    sandbox.write(
        "project/.azazel/config.toml",
        "[[hooks.setup]]\ncommand = \"cat >> setup.jsonl; echo >> setup.jsonl\"\n",
    );
    for _ in 0..2 {
        let output = sandbox.run(&json!({ "lead": [text("hi")] }), &["-p", "hello"]);
        assert!(output.status.success(), "{}", stderr(&output));
    }
    let runs = sandbox.read("project/setup.jsonl").unwrap();
    let lines: Vec<&str> = runs.lines().filter(|line| !line.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "{runs}");
    let payload: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(payload["hook_event_name"], "Setup");
    assert_eq!(payload["trigger"], "trust");

    // `azazel init` runs them too, and says so in the trigger.
    let fresh = Sandbox::new();
    fresh.write(
        "home/config.toml",
        "[roles]\ndefault = [\"mock/a\"]\n\n[[hooks.setup]]\ncommand = \"cat > init-setup.json\"\n",
    );
    let init = fresh.command().arg("init").write_stdin("").output().unwrap();
    assert!(init.status.success(), "{}", stderr(&init));
    let payload: Value = serde_json::from_str(&fresh.read("project/init-setup.json").unwrap()).unwrap();
    assert_eq!(payload["trigger"], "init");
}
