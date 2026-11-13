#![cfg(feature = "lsp")]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, ToolOutput, ToolSource, UpdateSink};
use azazel_core::config::LspServerConfig;
use azazel_core::process::Shell;
use azazel_core::{CommandOutcome, Event, ExtCtx, Registry};
use azazel_tools::CoreToolsExt;
use azazel_tools::lsp::{LspExt, LspManager};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// The `fake-lsp` fixture server, built on first use into the directory this test runs from.
static FIXTURE: LazyLock<PathBuf> = LazyLock::new(|| {
    // <target>/<profile>/deps/<this test>
    let test = std::env::current_exe().unwrap();
    let profile_dir = test.parent().unwrap().parent().unwrap();
    let mut build = Command::new(env!("CARGO"));
    build.args(["build", "--quiet", "--package", "fake-lsp", "--target-dir"]);
    build.arg(profile_dir.parent().unwrap());
    if profile_dir.file_name().unwrap() == "release" {
        build.arg("--release");
    }
    build.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    assert!(
        build.status().unwrap().success(),
        "could not build the fake-lsp fixture"
    );
    let binary = profile_dir.join(format!("fake-lsp{}", std::env::consts::EXE_SUFFIX));
    assert!(binary.is_file(), "{} is missing", binary.display());
    binary
});

const SOURCE: &str = "foo bar\nsecond\n    name = 1\n";

struct Bench {
    dir: tempfile::TempDir,
    registry: Registry,
}

/// A project whose `.fk` files belong to the fixture server and whose `.gone` files belong to a
/// server that is not installed.
fn bench() -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let registry = Registry::new(ExtCtx {
        services: Arc::new(Services::new()),
        cwd: root.clone(),
        session_id: "s".into(),
    });
    registry
        .load(&CoreToolsExt {
            shell: Shell::resolve(""),
            shell_timeout: Duration::from_secs(30),
            artifacts_dir: root.join(".artifacts"),
        })
        .unwrap();
    let server = |command: &str, extension: &str| LspServerConfig {
        command: command.to_string(),
        args: Vec::new(),
        extensions: vec![extension.to_string()],
        warm_files: Vec::new(),
    };
    let servers = BTreeMap::from([
        ("fake".to_string(), server(&FIXTURE.to_string_lossy(), "fk")),
        ("ghost".to_string(), server("no-such-language-server", ".GONE")),
    ]);
    registry.load(&LspExt { root, servers }).unwrap();
    Bench { dir, registry }
}

impl Bench {
    fn root(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.root().join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn servers(&self) -> Arc<LspManager> {
        self.registry.services.get::<LspManager>().unwrap()
    }

    async fn call(&self, tool: &str, input: Value) -> ToolOutput {
        let info = AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id: "s".into(),
        };
        let ctx = ToolCtx {
            agent_id: info.id.clone(),
            session_id: info.session_id.clone(),
            agent: info,
            call_id: "c1".into(),
            cwd: self.root(),
            cancel: CancellationToken::new(),
            update: UpdateSink::default(),
            services: self.registry.services.clone(),
        };
        let tool: Arc<dyn Tool> = self.registry.tools.get(&ctx.agent, tool).unwrap();
        tool.call(input, &ctx).await
    }

    async fn ok(&self, tool: &str, input: Value) -> String {
        let output = self.call(tool, input).await;
        assert!(!output.is_error, "{}", output.text_content());
        output.text_content()
    }

    async fn err(&self, tool: &str, input: Value) -> String {
        let output = self.call(tool, input).await;
        assert!(output.is_error, "{}", output.text_content());
        output.text_content()
    }

    async fn lsp(&self, input: Value) -> String {
        self.ok("lsp", input).await
    }

    /// Waits until `/lsp` shows `state` for the fixture server.
    async fn until(&self, state: &str) -> String {
        let begun = Instant::now();
        loop {
            let report = self.servers().report();
            let fake = report
                .lines()
                .find(|line| line.starts_with("fake "))
                .unwrap()
                .to_string();
            if fake.contains(state) {
                return report;
            }
            assert!(
                begun.elapsed() < Duration::from_secs(20),
                "never `{state}`:\n{report}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[tokio::test]
async fn a_write_reports_the_errors_of_the_new_content() {
    let bench = bench();
    bench.file("a.fk", SOURCE);
    bench.ok("read", json!({ "path": "a.fk" })).await;

    // The read started the server, so the edit waits for what it says about the new text.
    let edited = bench
        .ok(
            "edit",
            json!({ "path": "a.fk", "edits": [{ "old": "second", "new": "2nd" }] }),
        )
        .await;
    let bytes = SOURCE.len() - 3;
    assert_eq!(
        edited,
        format!("edited a.fk (1 replacement)\ndiagnostics:\na.fk:1:1 error fake error: {bytes} bytes")
    );

    let created = bench
        .ok("write", json!({ "path": "sub/b.fk", "content": "12345" }))
        .await;
    assert_eq!(
        created,
        "created sub/b.fk (5 bytes)\ndiagnostics:\nsub/b.fk:1:1 error fake error: 5 bytes"
    );
    // Files no server covers are left alone.
    let plain = bench
        .ok("write", json!({ "path": "notes.txt", "content": "x" }))
        .await;
    assert_eq!(plain, "created notes.txt (1 bytes)");
}

#[tokio::test]
async fn a_server_nobody_used_yet_is_started_without_holding_up_the_write() {
    let bench = bench();
    assert!(bench.servers().report().contains("fake  not started  "));
    let first = bench
        .ok("write", json!({ "path": "a.fk", "content": "one" }))
        .await;
    assert_eq!(first, "created a.fk (3 bytes)");
    let report = bench.until("running, 1 files open").await;
    assert!(
        report.contains("ghost  not installed  `no-such-language-server`  .gone"),
        "{report}"
    );

    let second = bench
        .ok("write", json!({ "path": "a.fk", "content": "three" }))
        .await;
    assert_eq!(
        second,
        "replaced a.fk (5 bytes)\ndiagnostics:\na.fk:1:1 error fake error: 5 bytes"
    );
}

#[tokio::test]
async fn the_tool_answers_in_lines_and_character_columns() {
    let bench = bench();
    bench.file("a.fk", SOURCE);
    let ask = |action: &str| json!({ "action": action, "path": "a.fk", "line": 1, "column": 1 });

    assert_eq!(bench.lsp(ask("definition")).await, "a.fk:3:5  name = 1");
    assert_eq!(
        bench.lsp(ask("references")).await,
        "2 references\na.fk:1:1  foo bar\na.fk:3:5  name = 1"
    );
    assert_eq!(
        bench.lsp(ask("symbols")).await,
        "1: function main\n  3: variable inner"
    );
    assert_eq!(
        bench.lsp(ask("diagnostics")).await,
        format!("a.fk:1:1 error fake error: {} bytes", SOURCE.len())
    );

    // Column 4 comes after `é` (one UTF-16 unit), `𝄞` (two) and a space.
    bench.file("wide.fk", "é𝄞 x\n");
    let hover = json!({ "action": "hover", "path": "wide.fk", "line": 1, "column": 4 });
    assert_eq!(bench.lsp(hover).await, "fake hover at 0:4");
}

#[tokio::test]
async fn rename_writes_the_servers_edit_and_keeps_the_agent_up_to_date() {
    let bench = bench();
    let path = bench.file("a.fk", SOURCE);
    let rename = json!({ "action": "rename", "path": "a.fk", "line": 3, "column": 5, "new_name": "total" });
    assert_eq!(
        bench.lsp(rename).await,
        "renamed to `total`: 1 edits in 1 files\na.fk (1)"
    );
    let renamed = "foo bar\nsecond\n    totale = 1\n";
    assert_eq!(std::fs::read_to_string(&path).unwrap(), renamed);

    // The agent made this change, so it may go on editing without reading the file again, and
    // the server has the renamed text.
    let edited = bench
        .ok(
            "edit",
            json!({ "path": "a.fk", "edits": [{ "old": "totale", "new": "total" }] }),
        )
        .await;
    assert_eq!(
        edited,
        format!(
            "edited a.fk (1 replacement)\ndiagnostics:\na.fk:1:1 error fake error: {} bytes",
            renamed.len() - 1
        )
    );
}

#[tokio::test]
async fn what_cannot_be_asked_is_explained() {
    let bench = bench();
    bench.file("a.fk", SOURCE);
    bench.file("notes.txt", "x");
    bench.file("old.gone", "x");

    let input = |action: &str, path: &str| json!({ "action": action, "path": path });
    assert_eq!(
        bench.err("lsp", input("hover", "notes.txt")).await,
        "no language server is available for `.txt` files; /lsp lists the servers"
    );
    assert_eq!(
        bench.err("lsp", input("hover", "a.fk")).await,
        "`hover` needs `line` (and `column`, 1 when left out)"
    );
    assert_eq!(
        bench
            .err("lsp", json!({ "action": "hover", "path": "a.fk", "line": 9 }))
            .await,
        "a.fk has no line 9; it has 4"
    );
    assert!(
        bench
            .err("lsp", input("format", "a.fk"))
            .await
            .starts_with("unknown action `format`; actions: ")
    );
    assert_eq!(
        bench
            .err("lsp", json!({ "action": "rename", "path": "a.fk", "line": 1 }))
            .await,
        "`rename` needs `new_name`"
    );
    assert_eq!(
        bench.err("lsp", input("diagnostics", "old.gone")).await,
        "language server `ghost`: `no-such-language-server` was not found"
    );
    // A server that cannot run does not get in the way of the file tools.
    bench.ok("read", json!({ "path": "old.gone" })).await;
    let written = bench
        .ok("write", json!({ "path": "old.gone", "content": "yz" }))
        .await;
    assert_eq!(written, "replaced old.gone (2 bytes)");

    let tool = bench.registry.tools.tools(&AgentInfo {
        id: "lead".into(),
        def: "lead".into(),
        parent: None,
        depth: 0,
        session_id: "s".into(),
    });
    let lsp = tool.iter().find(|tool| tool.name() == "lsp").unwrap();
    assert_eq!(lsp.access(&input("rename", "a.fk")), azazel_agent::Access::Write);
    assert_eq!(lsp.access(&input("hover", "a.fk")), azazel_agent::Access::Read);
}

#[tokio::test]
async fn servers_stop_when_the_session_ends_and_start_again_on_demand() {
    let bench = bench();
    bench.file("a.fk", SOURCE);
    bench.ok("read", json!({ "path": "a.fk" })).await;
    bench.until("running, 1 files open").await;

    let command = bench.registry.command("lsp").unwrap();
    let CommandOutcome::Show(shown) = command.handler.run("", &bench.registry.bus.ctx()).await else {
        panic!("/lsp shows a report");
    };
    assert_eq!(shown, bench.servers().report());

    bench.registry.bus.emit(&mut Event::SessionEnd).await;
    assert!(bench.servers().report().contains("fake  not started  "));

    let again = json!({ "action": "definition", "path": "a.fk", "line": 1 });
    assert_eq!(bench.lsp(again).await, "a.fk:3:5  name = 1");
}
