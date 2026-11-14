//! Checks on write as a session meets them: a checker found on `PATH` runs on what the lead
//! writes, the lead is told the rest when it stops, and `doctor` says what is installed.

use std::path::PathBuf;
use std::process::Output;
use std::sync::LazyLock;

use crate::common::{fixture_program, stderr, stdout};
use assert_cmd::Command;
use serde_json::{Value, json};

static FIXTURE: LazyLock<PathBuf> = LazyLock::new(|| fixture_program("fake-check"));

/// What the stand-in `shellcheck` says about the file it is given: a warning and an error.
const FOUND: &str = "--line\n{file}:2:6: warning: Double quote to prevent globbing. [SC2086]\n\
                     --line\n{file}:3:1: error: This is not a command. [SC1000]\n\
                     --exit\n1";

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    /// A project and a home, and a directory of programs put first on `PATH`: the fixture under
    /// the names `shellcheck`, `gitleaks`, `cargo` and `cargo-clippy`.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["project/.git", "home", "bin"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        for name in ["shellcheck", "gitleaks", "cargo", "cargo-clippy"] {
            let program = dir
                .path()
                .join("bin")
                .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
            std::fs::copy(&*FIXTURE, program).unwrap();
        }
        Sandbox { dir }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    /// `azazel` in this sandbox. `found` is what the stand-in checkers do when they are run.
    fn command(&self, found: &str) -> Command {
        let mut path = vec![self.path("bin")];
        path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.path("home"))
            .env("PATH", std::env::join_paths(path).unwrap())
            .env("FAKE_CHECK_ARGS", found)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .arg("--cwd")
            .arg(self.path("project"));
        command
    }

    /// `azazel -p go` with the lead following `steps`, every request recorded.
    fn run(&self, steps: &Value, args: &[&str]) -> Output {
        self.run_finding(FOUND, steps, args)
    }

    fn run_finding(&self, found: &str, steps: &Value, args: &[&str]) -> Output {
        std::fs::write(self.path("script.json"), json!({ "lead": steps }).to_string()).unwrap();
        let _ = std::fs::remove_file(self.path("requests.jsonl"));
        self.command(found)
            .env("AZAZEL_MOCK_SCRIPT", self.path("script.json"))
            .env("AZAZEL_MOCK_REQUESTS", self.path("requests.jsonl"))
            .args(["--set", "memory.auto_extract=false"])
            .args(["--set", "memory.auto_index=false"])
            .args(["--set", "session.auto_name=false"])
            // These runs end where the report of the checks ends them.
            .args(["--set", "guards.done_check=false"])
            // What is checked is what the lead wrote, whatever formatter is installed.
            .args(["--set", "tools.format_on_write=false"])
            .args(["--mode", "auto"])
            .args(args)
            .args(["-p", "go"])
            .write_stdin("")
            .output()
            .unwrap()
    }

    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
            .collect()
    }
}

/// The text of every block of `kind` the lead had been sent by `request`, in order: what its
/// tool calls returned (`tool_result`) or what it was told as the user (`text`).
fn sent(request: &Value, kind: &str) -> Vec<String> {
    let mut texts = Vec::new();
    for message in request["messages"].as_array().unwrap() {
        if message["role"] != "user" && kind == "text" {
            continue;
        }
        for block in message["content"].as_array().unwrap() {
            if block["type"] != kind {
                continue;
            }
            match kind {
                "tool_result" => {
                    let parts: Vec<&str> = block["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|inner| inner["text"].as_str())
                        .collect();
                    texts.push(parts.join("\n"));
                }
                _ => texts.push(block["text"].as_str().unwrap_or_default().to_string()),
            }
        }
    }
    texts
}

/// The lead writes a shell script to `path`, says it is done, and answers whatever it is told
/// next.
fn steps(path: &str) -> Value {
    writes(path, "#!/bin/sh\necho $1\nnope\n")
}

/// The lead writes `content` to `path`, says it is done, and answers whatever it is told next.
fn writes(path: &str, content: &str) -> Value {
    json!([
        { "reply": { "tool_calls": [{ "id": "t1", "name": "write", "input": {
            "path": path, "content": content } }] } },
        { "reply": { "text": "done" } },
        { "reply": { "text": "the script is a sketch; left as it is" } }
    ])
}

#[test]
fn a_checker_on_path_checks_what_the_lead_writes_and_the_lead_is_told_the_rest_when_it_stops() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&steps("run.sh"), &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let requests = sandbox.requests();
    // The write, the stop that was answered with the report, and the stop after it.
    assert_eq!(requests.len(), 3, "{requests:#?}");
    assert_eq!(
        sent(&requests[2], "tool_result"),
        ["created run.sh (23 bytes)\nchecks:\nrun.sh:3:1 error This is not a command. [SC1000] [shellcheck]"]
    );
    let told = sent(&requests[2], "text");
    assert_eq!(
        told.last().unwrap(),
        "<checks>\n\
         The checkers found 1 error, 1 warning in 1 file written in this turn:\n\
         run.sh: 1 error, 1 warning\n\
         On the lines changed in this turn:\n\
         run.sh:2:6 warning Double quote to prevent globbing. [SC2086] [shellcheck]\n\
         run.sh:3:1 error This is not a command. [SC1000] [shellcheck]\n\
         </checks>\n\
         This is an automatic report, not a message from the user. Fix what your changes caused, say in \
         a line what you leave and why, then finish."
    );
    assert!(
        stdout(&output).contains("the script is a sketch; left as it is"),
        "{}",
        stdout(&output)
    );

    // Without the report the error is still in the result of the write, and the first stop ends
    // the run.
    let output = sandbox.run(&steps("quiet.sh"), &["--set", "tools.check_report=\"off\""]);
    assert!(output.status.success(), "{}", stderr(&output));
    let requests = sandbox.requests();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    assert!(
        sent(&requests[1], "tool_result")[0].contains("\nchecks:\nquiet.sh:3:1 error "),
        "{requests:#?}"
    );

    // With checks on write off the checker is not run.
    let output = sandbox.run(&steps("unchecked.sh"), &["--set", "tools.check_on_write=false"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let requests = sandbox.requests();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    assert_eq!(
        sent(&requests[1], "tool_result"),
        ["created unchecked.sh (23 bytes)"]
    );
}

#[test]
fn doctor_says_which_checkers_are_installed_and_whether_secrets_are_scanned_for() {
    let sandbox = Sandbox::new();
    let doctor = |args: &[&str]| {
        let output = sandbox.command(FOUND).args(args).arg("doctor").output();
        stdout(&output.unwrap())
    };
    let line = |report: &str, start: &str| {
        report
            .lines()
            .find(|line| line.contains(start))
            .unwrap_or_else(|| panic!("no `{start}` line in:\n{report}"))
            .to_string()
    };

    let report = doctor(&[]);
    let checks = line(&report, "checks on write:");
    assert!(
        checks.starts_with("ok    ") && checks.contains("shellcheck (sh, bash)"),
        "{checks}"
    );
    assert_eq!(
        line(&report, "secret scan:"),
        "ok    secret scan: off, the project is not trusted"
    );
    let trusted = line(&doctor(&["--trust"]), "secret scan:");
    let gitleaks = sandbox.path("bin").join("gitleaks");
    assert!(
        trusted.starts_with(&format!("ok    secret scan: {}", gitleaks.display())),
        "{trusted}"
    );

    std::fs::write(
        sandbox.path("home/config.toml"),
        "[tools]\ncheck_report = \"off\"\n[tools.check]\nzz = \"azazel-no-such-checker --strict\"\n",
    )
    .unwrap();
    let report = doctor(&["--trust"]);
    assert_eq!(
        line(&report, "checker for"),
        "warn  checker for `zz` files: `azazel-no-such-checker` was not found"
    );
    assert_eq!(
        line(&report, "secret scan:"),
        "ok    secret scan: off, because `tools.check_report` is off and nothing would show what it found"
    );

    std::fs::write(
        sandbox.path("home/config.toml"),
        "[tools]\ncheck_on_write = false\n[tools.scan]\nsecrets = false\n",
    )
    .unwrap();
    let report = doctor(&["--trust"]);
    assert_eq!(line(&report, "checks on write:"), "ok    checks on write: off");
    assert_eq!(line(&report, "secret scan:"), "ok    secret scan: off");
}

#[test]
fn a_checker_that_runs_the_project_s_code_runs_only_when_the_project_is_trusted() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.path("project/src")).unwrap();
    std::fs::write(
        sandbox.path("project/Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    // The stand-in `cargo` notes that it was run and warns about `src/b.rs`.
    let calls = sandbox.path("cargo-calls.log");
    let found = format!(
        "--log\n{}\n--line\nsrc/b.rs:1:8: warning: unused variable: `x`",
        calls.display()
    );
    let code = "pub fn f() {\n    let x = 1;\n}\n";

    // Not trusted: clippy would build and run the project's build scripts, so it is not run,
    // and nothing is said about the file.
    let output = sandbox.run_finding(&found, &writes("src/a.rs", code), &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let requests = sandbox.requests();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    assert_eq!(sent(&requests[1], "tool_result"), ["created src/a.rs (30 bytes)"]);
    assert!(!calls.exists(), "cargo was run in a project that is not trusted");

    // Trusted: it runs, and what it found reaches the lead when it stops.
    let output = sandbox.run_finding(&found, &writes("src/b.rs", code), &["--trust"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);
    let requests = sandbox.requests();
    assert_eq!(requests.len(), 3, "{requests:#?}");
    let told = sent(&requests[2], "text");
    assert!(
        told.last().unwrap().contains(
            "On the lines changed in this turn:\n\
             src/b.rs:1:8 warning unused variable: `x` [clippy]\n\
             </checks>"
        ),
        "{told:#?}"
    );

    // `doctor` says which of the two it is, and that the project has something to trust.
    let doctor = |args: &[&str]| {
        let output = sandbox.command(&found).args(args).arg("doctor").output();
        stdout(&output.unwrap())
    };
    let report = doctor(&[]);
    assert!(
        report.contains("ok    checks on write: clippy (rs): off, the project is not trusted\n"),
        "{report}"
    );
    assert!(
        report.contains(
            "warn  trust: not decided; the project has Rust code that checks on write would build with \
             cargo clippy, running its build scripts and macros\n"
        ),
        "{report}"
    );
    let report = doctor(&["--trust"]);
    let ready = report
        .lines()
        .find(|line| line.contains("checks on write:"))
        .unwrap();
    assert!(
        ready.contains("clippy (rs)") && !report.contains("clippy (rs): off"),
        "{report}"
    );
}
