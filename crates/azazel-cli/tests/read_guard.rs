#![allow(clippy::unwrap_used)]

//! The read guard and `ast_grep` as a session meets them: the lead must read the lines it
//! edits, and with ast-grep on `PATH` it can search and rewrite by syntax tree, its writes
//! passing the permission rules like any other.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::LazyLock;

use assert_cmd::Command;
use serde_json::{Value, json};

/// The `fake-sg` fixture program, built on first use into the directory this test runs from.
static FIXTURE: LazyLock<PathBuf> = LazyLock::new(|| {
    // <target>/<profile>/deps/<this test>
    let test = std::env::current_exe().unwrap();
    let profile_dir = test.parent().unwrap().parent().unwrap();
    let mut build = std::process::Command::new(env!("CARGO"));
    build.args(["build", "--quiet", "--package", "fake-sg", "--target-dir"]);
    build.arg(profile_dir.parent().unwrap());
    if profile_dir.file_name().unwrap() == "release" {
        build.arg("--release");
    }
    build.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    assert!(
        build.status().unwrap().success(),
        "could not build the fake-sg fixture"
    );
    let binary = profile_dir.join(format!("fake-sg{}", std::env::consts::EXE_SUFFIX));
    assert!(binary.is_file(), "{} is missing", binary.display());
    binary
});

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    /// A project and a home, and a directory put first on `PATH` that holds the fixture under
    /// the name `ast-grep`.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["project/.git", "project/src", "home", "bin"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        let program = dir
            .path()
            .join("bin")
            .join(format!("ast-grep{}", std::env::consts::EXE_SUFFIX));
        std::fs::copy(&*FIXTURE, program).unwrap();
        Sandbox { dir }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn text(&self, relative: &str) -> String {
        std::fs::read_to_string(self.path(relative)).unwrap()
    }

    fn command(&self) -> Command {
        let mut path = vec![self.path("bin")];
        path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.path("home"))
            .env("PATH", std::env::join_paths(path).unwrap())
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .arg("--cwd")
            .arg(self.path("project"));
        command
    }

    /// `azazel -p go` with the lead making `calls` one after another and then saying it is done.
    /// Returns what each call returned.
    fn run(&self, calls: &[(&str, Value)], args: &[&str]) -> Vec<String> {
        let mut steps: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(index, (tool, input))| {
                json!({ "reply": { "tool_calls": [{ "id": format!("t{index}"), "name": tool, "input": input }] } })
            })
            .collect();
        steps.push(json!({ "reply": { "text": "done" } }));
        std::fs::write(self.path("script.json"), json!({ "lead": steps }).to_string()).unwrap();
        let _ = std::fs::remove_file(self.path("requests.jsonl"));
        let output: Output = self
            .command()
            .env("AZAZEL_MOCK_SCRIPT", self.path("script.json"))
            .env("AZAZEL_MOCK_REQUESTS", self.path("requests.jsonl"))
            .args(["--set", "memory.auto_extract=false"])
            .args(["--set", "memory.auto_index=false"])
            .args(["--set", "session.auto_name=false"])
            .args(["--set", "tools.check_on_write=false"])
            // The files are written as the lead wrote them, whatever formatter is installed.
            .args(["--set", "tools.format_on_write=false"])
            // The lead says it is done after its last call and is not sent back to prove it.
            .args(["--set", "guards.done_check=false"])
            .args(args)
            .args(["-p", "go"])
            .write_stdin("")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let requests = std::fs::read_to_string(self.path("requests.jsonl")).unwrap();
        let last: Value = serde_json::from_str(requests.lines().last().unwrap()).unwrap();
        let mut results = Vec::new();
        for message in last["request"]["messages"].as_array().unwrap() {
            for block in message["content"].as_array().unwrap() {
                if block["type"] == "tool_result" {
                    let parts: Vec<&str> = block["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|inner| inner["text"].as_str())
                        .collect();
                    results.push(parts.join("\n"));
                }
            }
        }
        assert_eq!(results.len(), calls.len(), "{last:#}");
        results
    }
}

fn edit(path: &str, old: &str, new: &str) -> (&'static str, Value) {
    (
        "edit",
        json!({ "path": path, "edits": [{ "old": old, "new": new }] }),
    )
}

#[test]
fn the_lead_edits_only_the_lines_it_has_read() {
    let sandbox = Sandbox::new();
    let text: String = (1..=40).map(|n| format!("line {n}\n")).collect();
    std::fs::write(sandbox.path("project/code.rs"), &text).unwrap();
    let calls = [
        ("read", json!({ "path": "code.rs", "limit": 5 })),
        edit("code.rs", "line 30\n", "thirty\n"),
        ("read", json!({ "path": "code.rs", "offset": 28, "limit": 5 })),
        edit("code.rs", "line 30\n", "thirty\n"),
        ("write", json!({ "path": "code.rs", "content": "all new\n" })),
    ];
    let results = sandbox.run(&calls, &["--mode", "auto"]);
    assert!(
        results[0].ends_with("[lines 1-5 of 40; continue with offset=6]"),
        "{}",
        results[0]
    );
    assert_eq!(
        results[1],
        "edit targets line 30 of code.rs, which you have not read; read it first"
    );
    assert_eq!(results[3], "edited code.rs (1 replacement)");
    assert_eq!(
        results[4],
        "you have read only part of code.rs; read the rest before overwriting it, or change it with edit"
    );
    assert_eq!(
        sandbox.text("project/code.rs"),
        text.replace("line 30\n", "thirty\n")
    );

    // `tools.read_guard = "file"`: a read of any part of the file is enough.
    std::fs::write(sandbox.path("project/code.rs"), &text).unwrap();
    let results = sandbox.run(
        &calls[..2],
        &["--mode", "auto", "--set", "tools.read_guard=\"file\""],
    );
    assert_eq!(results[1], "edited code.rs (1 replacement)");
}

fn replace(apply: Option<&[&str]>) -> (&'static str, Value) {
    let mut input = json!({ "action": "replace", "pattern": "log($MSG)", "rewrite": "print($MSG)" });
    if let Some(paths) = apply {
        input["apply"] = true.into();
        input["paths"] = json!(paths);
    }
    ("ast_grep", input)
}

#[test]
fn with_ast_grep_installed_the_lead_searches_and_rewrites_and_its_writes_pass_the_rules() {
    let sandbox = Sandbox::new();
    let (a, b) = ("start()\nlog(first)\nend()\n", "one\nlog(second)\n");
    std::fs::write(sandbox.path("project/src/a.ts"), a).unwrap();
    std::fs::write(sandbox.path("project/src/b.ts"), b).unwrap();

    let doctor = sandbox.command().arg("doctor").output().unwrap();
    let report = String::from_utf8_lossy(&doctor.stdout).into_owned();
    let line = report
        .lines()
        .find(|line| line.contains("ast_grep:"))
        .unwrap_or_else(|| panic!("no ast_grep line in:\n{report}"));
    let program = sandbox.path("bin").join("ast-grep");
    assert!(
        line.starts_with(&format!("ok    ast_grep: {}", program.display())),
        "{line}"
    );

    let calls = [
        ("ast_grep", json!({ "action": "search", "pattern": "log($MSG)" })),
        replace(None),
        replace(Some(&["src/a.ts", "src/b.ts"])),
    ];
    let results = sandbox.run(&calls, &["--mode", "auto"]);
    assert_eq!(
        results[0],
        "src/a.ts:2: log(first)\n  $MSG = first\nsrc/b.ts:2: log(second)\n  $MSG = second"
    );
    assert!(
        results[1].starts_with("preview: 2 replacements in 2 files, nothing written."),
        "{}",
        results[1]
    );
    assert_eq!(
        results[2],
        "replaced: 2 replacements in 2 files\nsrc/a.ts (1 replacement)\nsrc/b.ts (1 replacement)"
    );
    assert_eq!(sandbox.text("project/src/a.ts"), "start()\nprint(first)\nend()\n");
    assert_eq!(sandbox.text("project/src/b.ts"), "one\nprint(second)\n");

    // A rule that denies one of the files stops the write of both, and only the write: the
    // search and the preview are reads of other paths.
    std::fs::write(sandbox.path("project/src/a.ts"), a).unwrap();
    std::fs::write(sandbox.path("project/src/b.ts"), b).unwrap();
    let denied = [
        "--mode",
        "auto",
        "--set",
        "permissions.deny=[\"ast_grep(src/b.ts)\"]",
    ];
    let mut preview = replace(None);
    preview.1["paths"] = json!(["src/a.ts"]);
    let calls = [preview, replace(Some(&["src/a.ts", "src/b.ts"]))];
    let results = sandbox.run(&calls, &denied);
    assert!(
        results[0].starts_with("preview: 1 replacement in 1 file, "),
        "{}",
        results[0]
    );
    assert!(!results[1].starts_with("replaced"), "{}", results[1]);
    assert_eq!(sandbox.text("project/src/a.ts"), a);
    assert_eq!(sandbox.text("project/src/b.ts"), b);

    // Asked about in a mode that asks, with nobody to ask: the write does not happen, the
    // preview does.
    let results = sandbox.run(
        &[replace(None), replace(Some(&["src/a.ts", "src/b.ts"]))],
        &["--mode", "manual"],
    );
    assert!(
        results[0].starts_with("preview: 2 replacements in 2 files, "),
        "{}",
        results[0]
    );
    assert!(!results[1].starts_with("replaced"), "{}", results[1]);
    assert_eq!(sandbox.text("project/src/a.ts"), a);
}
