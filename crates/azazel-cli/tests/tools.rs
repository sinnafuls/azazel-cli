#![allow(clippy::unwrap_used)]

//! The tools of the binary as an agent meets them: `run_code`, PDFs, the addresses `read` takes,
//! format on write, and what a loaded skill allows.

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

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn requests_file(&self) -> PathBuf {
        self.dir.path().join("requests.jsonl")
    }

    /// `azazel -p go` in this sandbox answering from `script`, with every request recorded.
    /// Memory is kept from doing anything on its own.
    fn azazel(&self, script: &Value, args: &[&str]) -> Output {
        let script_path = self.dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let _ = std::fs::remove_file(self.requests_file());
        Command::cargo_bin("azazel")
            .unwrap()
            .env("AZAZEL_HOME", self.home())
            .env("AZAZEL_MOCK_SCRIPT", &script_path)
            .env("AZAZEL_MOCK_REQUESTS", self.requests_file())
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .arg("--cwd")
            .arg(self.project())
            .args(["--set", "memory.auto_extract=false"])
            .args(["--set", "memory.auto_index=false"])
            .args(args)
            .args(["-p", "go"])
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

    /// The text of every tool result the lead had been given by its last request, in order.
    fn results(&self) -> Vec<String> {
        let requests = self.requests();
        let last = requests.last().unwrap();
        let mut results = Vec::new();
        for message in last["messages"].as_array().unwrap() {
            for block in message["content"].as_array().unwrap() {
                if block["type"] == "tool_result" {
                    let text: Vec<&str> = block["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|inner| inner["text"].as_str())
                        .collect();
                    results.push(text.join("\n"));
                }
            }
        }
        results
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A script in which the lead makes `calls` one after another and then says `done`.
fn calls(calls: &[(&str, Value)]) -> Value {
    let mut steps: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, input))| {
            json!({ "reply": { "tool_calls": [{ "id": format!("t{}", i + 1), "name": name, "input": input }] } })
        })
        .collect();
    steps.push(json!({ "reply": { "text": "done" } }));
    json!({ "lead": steps })
}

fn tool<'a>(request: &'a Value, name: &str) -> Option<&'a Value> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == name)
}

#[cfg(feature = "lua")]
#[test]
fn a_script_does_several_steps_in_one_call_and_its_calls_pass_the_permission_rules() {
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.project().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(sandbox.project().join("b.txt"), "only\n").unwrap();
    let code = r#"
        local counts = {}
        for _, path in ipairs({ "a.txt", "b.txt" }) do
            local lines = 0
            for _ in string.gmatch(tools.read({ path = path }), "[^\n]+") do lines += 1 end
            table.insert(counts, path .. ": " .. lines)
        end
        local ran, why = pcall(tools.shell, { command = "echo hi" })
        return table.concat(counts, ", ") .. "; shell ran: " .. tostring(ran) .. " (" .. why .. ")"
    "#;
    let script = calls(&[("run_code", json!({ "code": code }))]);
    let output = sandbox.azazel(&script, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        sandbox.results(),
        [
            "a.txt: 3, b.txt: 1; shell ran: false (shell: permission required for shell: re-run with --mode \
          auto or add an allow rule)"
        ]
    );

    // The model is told which tools a script can call, and the tool does not depend on the
    // Luau extensions being loaded.
    let requests = sandbox.requests();
    let description = tool(&requests[0], "run_code").unwrap()["description"]
        .as_str()
        .unwrap();
    assert!(description.contains("\n- read{ path: string, "), "{description}");
    assert!(description.contains("\n- shell{ "), "{description}");
    assert!(!description.contains("\n- task{"), "{description}");
    assert_eq!(
        tool(&requests[1], "run_code").unwrap()["description"],
        description
    );

    let bare = sandbox.azazel(&script, &["--no-extensions"]);
    assert!(bare.status.success(), "{}", stderr(&bare));
    assert!(tool(&sandbox.requests()[0], "run_code").is_some());
}

#[cfg(feature = "pdf")]
#[test]
fn a_pdf_is_read_as_text_page_by_page() {
    let sandbox = Sandbox::new();
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../azazel-tools/tests/fixtures/read/three-pages.pdf");
    std::fs::copy(fixture, sandbox.project().join("doc.pdf")).unwrap();
    let script = calls(&[
        ("read", json!({ "path": "doc.pdf" })),
        ("read", json!({ "path": "doc.pdf", "pages": "2" })),
    ]);
    let output = sandbox.azazel(&script, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        sandbox.results(),
        [
            "--- page 1 ---\nAlpha one\n\n--- page 2 ---\nBeta two\n\n--- page 3 ---\nGamma three",
            "--- page 2 ---\nBeta two\n[pages 2-2 of 3]",
        ]
    );
    let read = sandbox.requests()[0].clone();
    assert!(tool(&read, "read").unwrap()["schema"]["properties"]["pages"].is_object());
}

#[test]
fn read_takes_the_addresses_of_sessions_jobs_and_memories() {
    let sandbox = Sandbox::new();
    let mut steps = vec![
        ("read", json!({ "path": "session://" })),
        ("read", json!({ "path": "job://" })),
    ];
    if cfg!(feature = "memory") {
        steps.push((
            "memory_save",
            json!({ "text": "Releases are cut on Fridays", "kind": "fact" }),
        ));
        steps.push(("read", json!({ "path": "memory://" })));
        steps.push(("read", json!({ "path": "memory://search/releases" })));
    }
    let output = sandbox.azazel(&calls(&steps), &["--mode", "auto"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let results = sandbox.results();
    assert!(results[0].starts_with("id: "), "{}", results[0]);
    assert!(
        results[0].contains("messages: 1 from the user, 1 from the agent, 0 tool results"),
        "{}",
        results[0]
    );
    assert!(
        results[0].ends_with("This project has no other saved session."),
        "{}",
        results[0]
    );
    assert_eq!(results[1], "no background jobs");
    if cfg!(feature = "memory") {
        let id = results[2]
            .strip_prefix("saved as ")
            .and_then(|rest| rest.split(' ').next())
            .unwrap();
        let line = format!("{id} [fact, project] Releases are cut on Fridays");
        assert_eq!(results[3], line);
        assert_eq!(results[4], line);
    }

    // A second session of the project can read the first one's transcript by its id.
    let first_id = results[0]
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("id: "))
        .unwrap()
        .to_string();
    let again = calls(&[
        ("read", json!({ "path": "session://" })),
        ("read", json!({ "path": format!("session://{first_id}") })),
    ]);
    let output = sandbox.azazel(&again, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let results = sandbox.results();
    assert!(results[0].contains(&format!("\n{first_id}  ")), "{}", results[0]);
    assert!(
        results[1].starts_with("[prompt]\ngo\n\n→ read session://\n→ read job://"),
        "{}",
        results[1]
    );
}

/// A program that replaces the content of the file it is given with `formatted`.
fn fake_formatter(dir: &Path) -> String {
    let (name, body) = if cfg!(windows) {
        ("fmt.cmd", "@echo off\r\necho formatted> %1\r\n")
    } else {
        ("fmt.sh", "#!/bin/sh\nprintf 'formatted\\n' > \"$1\"\n")
    };
    let program = dir.join(name);
    std::fs::write(&program, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    program.display().to_string().replace('\\', "/")
}

#[test]
fn format_on_write_runs_the_formatter_and_the_next_edit_is_not_stale() {
    let sandbox = Sandbox::new();
    let formatter = fake_formatter(sandbox.dir.path());
    let script = calls(&[
        (
            "write",
            json!({ "path": "notes.txt", "content": "messy   text\n" }),
        ),
        (
            "edit",
            json!({ "path": "notes.txt", "edits": [{ "old": "formatted", "new": "by hand" }] }),
        ),
    ]);
    let setting = format!("tools.format.txt=\"{formatter}\"");
    let output = sandbox.azazel(
        &script,
        &[
            "--mode",
            "auto",
            "--set",
            "tools.format_on_write=true",
            "--set",
            &setting,
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let results = sandbox.results();
    assert!(
        results[0].starts_with("created notes.txt (") && results[0].ends_with("; formatted with fmt"),
        "{}",
        results[0]
    );
    assert_eq!(results[1], "edited notes.txt (1 replacement); formatted with fmt");

    // Off by default: the same write is left as the agent wrote it.
    let plain = sandbox.azazel(
        &calls(&[(
            "write",
            json!({ "path": "other.txt", "content": "messy   text\n" }),
        )]),
        &["--mode", "auto", "--set", &setting],
    );
    assert!(plain.status.success(), "{}", stderr(&plain));
    assert_eq!(sandbox.results(), ["created other.txt (13 bytes)"]);
}

#[test]
fn what_a_loaded_skill_allows_needs_no_approval_while_the_run_lasts() {
    let sandbox = Sandbox::new();
    let skill = sandbox.home().join("skills/greet");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: greet\ndescription: Say hello from the shell.\nallowed-tools: Bash(echo:*)\n---\nRun `echo hello`.\n",
    )
    .unwrap();
    let script = calls(&[
        ("shell", json!({ "command": "echo before" })),
        ("skill", json!({ "name": "greet" })),
        ("shell", json!({ "command": "echo hello" })),
        ("shell", json!({ "command": "ls" })),
    ]);
    let output = sandbox.azazel(&script, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let results = sandbox.results();
    let refused = "permission required for shell: re-run with --mode auto or add an allow rule";
    assert_eq!(results[0], refused);
    assert!(results[1].ends_with("Run `echo hello`.\n") || results[1].ends_with("Run `echo hello`."));
    assert!(
        results[2].starts_with("hello") && results[2].contains("exit code: 0"),
        "{}",
        results[2]
    );
    // Only what the skill lists is allowed.
    assert_eq!(results[3], refused);
}
