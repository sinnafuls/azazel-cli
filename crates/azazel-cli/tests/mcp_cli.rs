#![cfg(feature = "mcp")]
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::LazyLock;

use assert_cmd::Command;
use serde_json::{Value, json};

/// The `mcp-echo` fixture server, built on first use into the directory this test runs from.
static FIXTURE: LazyLock<PathBuf> = LazyLock::new(|| {
    // <target>/<profile>/deps/<this test>
    let test = std::env::current_exe().unwrap();
    let profile_dir = test.parent().unwrap().parent().unwrap();
    let mut build = std::process::Command::new(env!("CARGO"));
    build.args(["build", "--quiet", "--package", "mcp-echo", "--target-dir"]);
    build.arg(profile_dir.parent().unwrap());
    if profile_dir.file_name().unwrap() == "release" {
        build.arg("--release");
    }
    build.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    assert!(
        build.status().unwrap().success(),
        "could not build the mcp-echo fixture"
    );
    let binary = profile_dir.join(format!("mcp-echo{}", std::env::consts::EXE_SUFFIX));
    assert!(binary.is_file(), "{} is missing", binary.display());
    binary
});

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        Sandbox { dir }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn azazel(&self) -> Command {
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.path("home"))
            .env_remove("AZAZEL_MOCK_SCRIPT")
            .arg("--cwd")
            .arg(self.path("project"));
        command
    }

    fn json(&self, relative: &str) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.path(relative)).unwrap()).unwrap()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_server_is_added_listed_used_by_an_agent_and_removed() {
    let sandbox = Sandbox::new();
    let added = sandbox
        .azazel()
        .args(["mcp", "add", "echo", "--"])
        .arg(FIXTURE.as_path())
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    assert_eq!(
        sandbox.json("home/mcp.json"),
        json!({ "mcpServers": { "echo": { "command": FIXTURE.as_path() } } })
    );

    let listed = sandbox.azazel().args(["mcp", "list"]).output().unwrap();
    assert!(
        stdout(&listed).starts_with("echo  ready · 2 tools · 1 prompts\n"),
        "{}",
        stdout(&listed)
    );

    let script = sandbox.path("script.json");
    let steps = json!({ "lead": [
        { "reply": { "tool_calls": [{ "id": "c1", "name": "mcp__echo__echo", "input": { "text": "hi" } }] } },
        { "reply": { "text": "The server said hi." } }
    ] });
    std::fs::write(&script, steps.to_string()).unwrap();
    let ran = sandbox
        .azazel()
        .env("AZAZEL_MOCK_SCRIPT", &script)
        .args(["--mode", "auto", "--output", "json", "-p", "say hi"])
        .output()
        .unwrap();
    assert!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr));
    let events: Vec<Value> = stdout(&ran)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let result = events.iter().find(|event| event["type"] == "tool_end").unwrap();
    assert_eq!(
        result["output"]["content"],
        json!([{ "type": "text", "text": "hi" }])
    );
    assert_eq!(result["output"]["is_error"], false);

    let removed = sandbox.azazel().args(["mcp", "remove", "echo"]).output().unwrap();
    assert!(
        stdout(&removed).starts_with("removed `echo` from "),
        "{}",
        stdout(&removed)
    );
    let empty = sandbox.azazel().args(["mcp", "list"]).output().unwrap();
    assert!(
        stdout(&empty).starts_with("no MCP servers are configured"),
        "{}",
        stdout(&empty)
    );
    let again = sandbox.azazel().args(["mcp", "remove", "echo"]).output().unwrap();
    assert_eq!(again.status.code(), Some(1));
}

#[test]
fn project_servers_need_trust_and_a_missing_command_is_reported() {
    let sandbox = Sandbox::new();
    let added = sandbox
        .azazel()
        .args([
            "mcp",
            "add",
            "local",
            "--project",
            "--",
            "no-such-mcp-binary",
            "--flag",
        ])
        .output()
        .unwrap();
    assert!(
        stdout(&added).contains("project servers start only once the project is trusted"),
        "{}",
        stdout(&added)
    );
    assert_eq!(
        sandbox.json("project/.azazel/mcp.json"),
        json!({ "mcpServers": { "local": { "command": "no-such-mcp-binary", "args": ["--flag"] } } })
    );

    let untrusted = sandbox.azazel().args(["mcp", "list"]).output().unwrap();
    assert!(
        stdout(&untrusted).starts_with("no MCP servers are configured"),
        "{}",
        stdout(&untrusted)
    );
    let trusted = sandbox
        .azazel()
        .args(["--trust", "mcp", "list"])
        .output()
        .unwrap();
    assert!(
        stdout(&trusted).starts_with("local  failed: `no-such-mcp-binary` was not found on PATH\n"),
        "{}",
        stdout(&trusted)
    );

    let remote = sandbox
        .azazel()
        .args([
            "mcp",
            "add",
            "docs",
            "--url",
            "https://example.invalid/mcp",
            "--header",
            "X-Key=${DOCS_KEY}",
        ])
        .output()
        .unwrap();
    assert!(remote.status.success());
    assert_eq!(
        sandbox.json("home/mcp.json")["mcpServers"]["docs"],
        json!({ "url": "https://example.invalid/mcp", "headers": { "X-Key": "${DOCS_KEY}" } })
    );
    // The variable is not set, so the entry is reported instead of started.
    let listed = sandbox
        .azazel()
        .env_remove("DOCS_KEY")
        .args(["mcp", "list"])
        .output()
        .unwrap();
    assert!(
        stdout(&listed).contains("docs  not loaded: ${DOCS_KEY} is not set"),
        "{}",
        stdout(&listed)
    );

    let both = sandbox
        .azazel()
        .args(["mcp", "add", "x", "--url", "https://x.invalid", "--", "cmd"])
        .output()
        .unwrap();
    assert_eq!(both.status.code(), Some(1));
    let local_login = sandbox
        .azazel()
        .args(["--trust", "mcp", "login", "local"])
        .output()
        .unwrap();
    assert_eq!(local_login.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&local_login.stderr).contains("nothing to log in to"));
}
