#![cfg(feature = "lsp")]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, ToolOutput, ToolSource, UpdateSink};
use azazel_core::config::{LspServerConfig, WriteSeverity};
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

struct Setup {
    /// `lsp.idle_secs`, as a duration.
    idle: Duration,
    write_severity: WriteSeverity,
    /// The fixture server's `warm_files`.
    warm_files: Vec<String>,
}

impl Default for Setup {
    fn default() -> Self {
        Setup {
            idle: Duration::from_secs(240),
            write_severity: WriteSeverity::Error,
            warm_files: Vec::new(),
        }
    }
}

fn bench() -> Bench {
    bench_with(Setup::default())
}

/// A project whose `.fk` files belong to the fixture server and whose `.gone` files belong to a
/// server that is not installed. The fixture server writes what it receives to `lsp.log`.
fn bench_with(setup: Setup) -> Bench {
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
            read_guard: Default::default(),
            patch: Default::default(),
        })
        .unwrap();
    let server = |command: &str, extension: &str| LspServerConfig {
        command: command.to_string(),
        args: Vec::new(),
        extensions: vec![extension.to_string()],
        warm_files: Vec::new(),
    };
    let mut fake = server(&FIXTURE.to_string_lossy(), "fk");
    fake.args = vec![
        "--log".to_string(),
        root.join("lsp.log").to_string_lossy().into_owned(),
    ];
    fake.warm_files = setup.warm_files;
    let servers = BTreeMap::from([
        ("fake".to_string(), fake),
        ("ghost".to_string(), server("no-such-language-server", ".GONE")),
    ]);
    registry
        .load(&LspExt {
            root,
            servers,
            idle: setup.idle,
            write_severity: setup.write_severity,
        })
        .unwrap();
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

    /// What the fixture server has received: one `<method> <params>` line per message.
    fn received(&self) -> Vec<String> {
        std::fs::read_to_string(self.root().join("lsp.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// How many messages of `method` the fixture server has received.
    fn count(&self, method: &str) -> usize {
        self.received()
            .iter()
            .filter(|line| line.split(' ').next() == Some(method))
            .count()
    }

    /// Waits until the fixture server has received `count` messages of `method`.
    async fn until_received(&self, method: &str, count: usize) {
        let begun = Instant::now();
        while self.count(method) < count {
            assert!(
                begun.elapsed() < Duration::from_secs(20),
                "never {count} of `{method}`:\n{}",
                self.received().join("\n")
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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

/// A document with a class of two methods, a function and a variable, as the fixture reads them.
const OUTLINED: &str = "\
use helper
class Shape
    fn area
        let width
        compute
    end
    fn name
    end
end

fn main
    Shape.area
end
let total
";

#[tokio::test]
async fn the_outline_lists_every_symbol_with_its_lines_and_declaration() {
    let bench = bench();
    bench.file("shapes.fk", OUTLINED);
    let outline = bench
        .lsp(json!({ "action": "outline", "path": "shapes.fk" }))
        .await;
    assert_eq!(
        outline,
        "\
class Shape  2-9  class Shape
  method area  3-6  fn area
    variable width  4-4  let width
  method name  7-8  fn name
function main  11-13  fn main
variable total  14-14  let total"
    );
    // The plain list of symbols is as it was.
    let symbols = bench
        .lsp(json!({ "action": "symbols", "path": "shapes.fk" }))
        .await;
    assert!(
        symbols.starts_with("2: class Shape\n  3: method area\n"),
        "{symbols}"
    );
}

#[tokio::test]
async fn read_symbol_returns_the_source_and_counts_as_reading_it() {
    let bench = bench();
    let mut text = OUTLINED.to_string();
    // A second `name`, outside the class.
    text.push_str("fn name\n    other\nend\n");
    bench.file("shapes.fk", &text);
    let read = |symbol: &str| json!({ "action": "read_symbol", "path": "shapes.fk", "symbol": symbol });

    assert_eq!(
        bench.lsp(read("area")).await,
        "method area  shapes.fk:3-6\n    fn area\n        let width\n        compute\n    end"
    );
    // The lines of the symbol are read: an edit inside them goes through, one outside does not.
    let inside = json!({ "path": "shapes.fk", "edits": [{ "old": "compute", "new": "width * 2" }] });
    assert!(
        bench
            .ok("edit", inside)
            .await
            .starts_with("edited shapes.fk (1 replacement)")
    );
    let outside = json!({ "path": "shapes.fk", "edits": [{ "old": "Shape.area", "new": "Shape.name" }] });
    assert_eq!(
        bench.err("edit", outside).await,
        "edit targets line 12 of shapes.fk, which you have not read; read it first"
    );

    // A name that several symbols have is asked about; a qualified name or a number settles it.
    assert_eq!(
        bench.err("lsp", read("name")).await,
        "`name` names 2 symbols in shapes.fk; say which with `occurrence`:\n\
         1: method name  7-8  fn name\n\
         2: function name  15-17  fn name"
    );
    assert_eq!(
        bench.lsp(read("Shape.name")).await,
        "method name  shapes.fk:7-8\n    fn name\n    end"
    );
    assert_eq!(
        bench.lsp(read("Shape::name")).await,
        bench.lsp(read("Shape.name")).await
    );
    let second = json!({ "action": "read_symbol", "path": "shapes.fk", "symbol": "name", "occurrence": 2 });
    assert_eq!(
        bench.lsp(second).await,
        "function name  shapes.fk:15-17\nfn name\n    other\nend"
    );
    let third = json!({ "action": "read_symbol", "path": "shapes.fk", "symbol": "name", "occurrence": 3 });
    assert_eq!(
        bench.err("lsp", third).await,
        "`occurrence` 3 is not one of the 2 symbols named `name` in shapes.fk"
    );
    assert_eq!(
        bench.err("lsp", read("missing")).await,
        "no symbol `missing` in shapes.fk; `outline` lists its symbols"
    );
    let unnamed = json!({ "action": "read_symbol", "path": "shapes.fk" });
    assert_eq!(bench.err("lsp", unnamed).await, "`read_symbol` needs `symbol`");
}

#[tokio::test]
async fn a_long_symbol_comes_in_part_and_only_that_part_is_read() {
    let bench = bench();
    let body: String = (1..=2100).map(|n| format!("    step {n}\n")).collect();
    bench.file("long.fk", &format!("fn long\n{body}end\n"));
    let read = bench
        .lsp(json!({ "action": "read_symbol", "path": "long.fk", "symbol": "long" }))
        .await;
    assert!(
        read.starts_with("function long  long.fk:1-2102\nfn long\n    step 1\n"),
        "{}",
        &read[..80]
    );
    assert!(
        read.ends_with("    step 1999\n[lines 1-2000 of 1-2102; continue with read offset=2001]"),
        "{}",
        &read[read.len() - 120..]
    );
    let seen = json!({ "path": "long.fk", "edits": [{ "old": "step 1999\n", "new": "step x\n" }] });
    assert!(bench.ok("edit", seen).await.starts_with("edited long.fk"));
    let unseen = json!({ "path": "long.fk", "edits": [{ "old": "step 2050\n", "new": "step y\n" }] });
    assert_eq!(
        bench.err("edit", unseen).await,
        "edit targets line 2051 of long.fk, which you have not read; read it first"
    );
}

#[tokio::test]
async fn warnings_follow_a_write_when_the_configuration_asks_for_them() {
    let source = "one\nWARN two\nthree WARN\n";
    let errors_only = bench();
    errors_only.file("a.fk", "x");
    errors_only.ok("read", json!({ "path": "a.fk" })).await;
    let written = errors_only
        .ok("write", json!({ "path": "a.fk", "content": source }))
        .await;
    assert_eq!(
        written,
        format!(
            "replaced a.fk ({0} bytes)\ndiagnostics:\na.fk:1:1 error fake error: {0} bytes",
            source.len()
        )
    );

    let with_warnings = bench_with(Setup {
        write_severity: WriteSeverity::Warning,
        ..Setup::default()
    });
    with_warnings.file("a.fk", "x");
    with_warnings.ok("read", json!({ "path": "a.fk" })).await;
    let written = with_warnings
        .ok("write", json!({ "path": "a.fk", "content": source }))
        .await;
    assert_eq!(
        written,
        format!(
            "replaced a.fk ({0} bytes)\ndiagnostics:\na.fk:1:1 error fake error: {0} bytes\n\
             a.fk:2:1 warning fake warning\na.fk:3:1 warning fake warning",
            source.len()
        )
    );
}

#[tokio::test]
async fn a_server_nobody_touches_is_stopped_and_starts_again_on_the_next_touch() {
    let idle = Duration::from_millis(800);
    let bench = bench_with(Setup {
        idle,
        ..Setup::default()
    });
    bench.file("a.fk", SOURCE);
    bench.ok("read", json!({ "path": "a.fk" })).await;
    bench.until("running, 1 files open").await;

    // Touched again and again, it stays: for twice the time it may sit idle, with never more
    // than half of that between two touches (a machine too busy to keep that pace proves
    // nothing either way).
    let begun = Instant::now();
    let mut longest = Duration::ZERO;
    let mut last = begun;
    while begun.elapsed() < idle * 2 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        bench.ok("read", json!({ "path": "a.fk" })).await;
        longest = longest.max(last.elapsed());
        last = Instant::now();
    }
    if longest < idle / 2 {
        assert!(bench.servers().report().contains("fake  running, 1 files open"));
        assert_eq!(bench.count("initialize"), 1);
    }
    let starts = bench.count("initialize");

    let report = bench.until("stopped, idle").await;
    assert!(report.contains("fake  stopped, idle  "), "{report}");
    bench.until_received("shutdown", starts).await;

    // The next write starts it without waiting for it; the one after hears from it.
    let first = bench
        .ok(
            "edit",
            json!({ "path": "a.fk", "edits": [{ "old": "second", "new": "2nd" }] }),
        )
        .await;
    assert_eq!(first, "edited a.fk (1 replacement)");
    bench.until("running, 1 files open").await;
    assert_eq!(bench.count("initialize"), starts + 1);
    let second = bench
        .ok(
            "edit",
            json!({ "path": "a.fk", "edits": [{ "old": "2nd", "new": "two" }] }),
        )
        .await;
    assert!(
        second.contains("\ndiagnostics:\na.fk:1:1 error fake error: "),
        "{second}"
    );

    // However often it is stopped for idleness, it starts again: that is no failure.
    for round in 2..=4 {
        bench.until("stopped, idle").await;
        bench.ok("read", json!({ "path": "a.fk" })).await;
        bench.until("running, 1 files open").await;
        assert_eq!(bench.count("initialize"), starts + round);
    }
}

#[tokio::test]
async fn warm_files_are_opened_when_the_server_starts() {
    let bench = bench_with(Setup {
        warm_files: vec!["lib/core.fk".to_string(), "missing.fk".to_string()],
        ..Setup::default()
    });
    std::fs::create_dir_all(bench.root().join("lib")).unwrap();
    bench.file("lib/core.fk", "fn core\nend\n");
    bench.file("a.fk", SOURCE);
    assert_eq!(
        bench.count("initialize"),
        0,
        "nothing starts before a file is touched"
    );

    bench.ok("read", json!({ "path": "a.fk" })).await;
    bench.until("running, 2 files open").await;
    let opened: Vec<String> = bench
        .received()
        .into_iter()
        .filter(|line| line.starts_with("textDocument/didOpen "))
        .collect();
    assert_eq!(opened.len(), 2, "{opened:?}");
    // The warm file first, then the file that was touched; the missing one is passed over.
    assert!(opened[0].contains("/lib/core.fk\""), "{}", opened[0]);
    assert!(opened[1].contains("/a.fk\""), "{}", opened[1]);
}

#[tokio::test]
async fn moving_a_file_asks_the_server_first_and_tells_it_after() {
    let bench = bench();
    let helper = bench.file("helper.fk", "fn help\nend\n");
    let user = bench.file("user.fk", "first\nuse helper\nlast\n");
    // The server knows the file that refers to the one being moved.
    bench.ok("read", json!({ "path": "user.fk", "limit": 1 })).await;
    bench.ok("read", json!({ "path": "helper.fk" })).await;
    bench.until("running, 2 files open").await;

    let moved = bench
        .lsp(json!({ "action": "rename_file", "path": "helper.fk", "new_path": "lib/tools.fk" }))
        .await;
    assert_eq!(
        moved,
        "moved helper.fk to lib/tools.fk; 1 edits in 1 files\nuser.fk (1)"
    );
    let target = bench.root().join("lib").join("tools.fk");
    assert!(!helper.exists());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "fn help\nend\n");
    assert_eq!(
        std::fs::read_to_string(&user).unwrap(),
        "first\nuse tools\nlast\n"
    );

    // Asked before the move, told after it, with both names; the old document is closed and
    // the new one opened.
    bench.until_received("workspace/didRenameFiles", 1).await;
    bench.until("running, 2 files open").await;
    let received = bench.received();
    let at = |method: &str| {
        received
            .iter()
            .position(|line| line.split(' ').next() == Some(method))
            .unwrap_or_else(|| panic!("no {method} in\n{}", received.join("\n")))
    };
    let (will, did) = (at("workspace/willRenameFiles"), at("workspace/didRenameFiles"));
    assert!(will < did);
    for line in [&received[will], &received[did]] {
        let params: Value = serde_json::from_str(line.split_once(' ').unwrap().1).unwrap();
        let file = &params["files"][0];
        assert!(file["oldUri"].as_str().unwrap().ends_with("/helper.fk"), "{line}");
        assert!(
            file["newUri"].as_str().unwrap().ends_with("/lib/tools.fk"),
            "{line}"
        );
    }
    let closed = at("textDocument/didClose");
    assert!(
        will < closed && received[closed].contains("/helper.fk\""),
        "{}",
        received[closed]
    );

    // What the agent had read of the file it has read of it where it is now, and the line the
    // server changed in the other file it has seen; the rest of that file it has not.
    let edited = bench
        .ok(
            "edit",
            json!({ "path": "lib/tools.fk", "edits": [{ "old": "help", "new": "assist" }] }),
        )
        .await;
    assert!(
        edited.starts_with("edited lib/tools.fk (1 replacement)"),
        "{edited}"
    );
    let seen = json!({ "path": "user.fk", "edits": [{ "old": "use tools", "new": "use lib.tools" }] });
    assert!(bench.ok("edit", seen).await.starts_with("edited user.fk"));
    let unseen = json!({ "path": "user.fk", "edits": [{ "old": "last", "new": "end" }] });
    assert_eq!(
        bench.err("edit", unseen).await,
        "edit targets line 3 of user.fk, which you have not read; read it first"
    );

    // A move onto a file that is there, or out of the project, is refused.
    bench.file("taken.fk", "x");
    let onto = json!({ "action": "rename_file", "path": "user.fk", "new_path": "taken.fk" });
    assert!(bench.err("lsp", onto).await.ends_with("taken.fk already exists"));
    let out = json!({ "action": "rename_file", "path": "user.fk", "new_path": "../elsewhere.fk" });
    assert!(bench.err("lsp", out).await.ends_with("is outside the project"));
    let nowhere = json!({ "action": "rename_file", "path": "user.fk" });
    assert_eq!(bench.err("lsp", nowhere).await, "`rename_file` needs `new_path`");
    assert!(user.exists());
}
