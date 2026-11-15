#![cfg(feature = "debug")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, ToolOutput, UpdateSink};
use azazel_core::config::DebugAdapterConfig;
use azazel_core::{Event, ExtCtx, Registry};
use azazel_tools::debug::{DebugExt, DebugManager};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

static FIXTURE: LazyLock<PathBuf> = LazyLock::new(|| crate::common::fixture_program("fake-dap"));

struct Bench {
    dir: tempfile::TempDir,
    registry: Registry,
}

/// A project whose `rust` programs run under the fixture adapter and whose `go` programs under an
/// adapter that is not installed.
fn bench() -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(ExtCtx {
        services: Arc::new(Services::new()),
        cwd: dir.path().to_path_buf(),
        session_id: "s".into(),
    });
    let adapter = |command: String| DebugAdapterConfig {
        command,
        args: Vec::new(),
        listens: None,
    };
    let adapters = BTreeMap::from([
        (
            "rust".to_string(),
            adapter(FIXTURE.to_string_lossy().into_owned()),
        ),
        ("go".to_string(), adapter("no-such-debug-adapter".to_string())),
    ]);
    registry
        .load(&DebugExt {
            adapters,
            timeout: Duration::from_secs(30),
        })
        .unwrap();
    Bench { dir, registry }
}

impl Bench {
    async fn call(&self, input: Value) -> ToolOutput {
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
            cwd: self.dir.path().to_path_buf(),
            cancel: CancellationToken::new(),
            update: UpdateSink::default(),
            services: self.registry.services.clone(),
        };
        let tool: Arc<dyn Tool> = self.registry.tools.get(&ctx.agent, "debug").unwrap();
        tool.call(input, &ctx).await
    }

    async fn ok(&self, input: Value) -> String {
        let output = self.call(input).await;
        assert!(!output.is_error, "{}", output.text_content());
        output.text_content()
    }

    async fn err(&self, input: Value) -> String {
        let output = self.call(input).await;
        assert!(output.is_error, "{}", output.text_content());
        output.text_content()
    }
}

#[tokio::test]
async fn a_session_runs_from_start_to_stop() {
    let bench = bench();
    let started = bench
        .ok(json!({
            "action": "start", "program": "app", "language": "rust",
            "breakpoints": [{ "file": "main.fk", "line": 5 }]
        }))
        .await;
    assert!(started.contains("breakpoint main.fk:5"), "{started}");
    assert!(started.contains("stopped: breakpoint"), "{started}");
    assert!(started.contains("#0 inner at main.fk:5"), "{started}");
    assert!(started.contains("#1 main at main.fk:3"), "{started}");
    assert!(started.contains("x: int = 42"), "{started}");
    assert!(started.contains("output:\nhello"), "{started}");
    // Only the top frame's first scope, and long values are cut.
    assert!(!started.contains("GLOBAL"), "{started}");
    assert!(started.contains("more characters"), "{started}");

    let set = bench
        .ok(json!({ "action": "break", "file": "main.fk", "line": 9, "condition": "x > 1" }))
        .await;
    assert!(
        set.contains("main.fk:5") && set.contains("main.fk:9 if x > 1"),
        "{set}"
    );

    let stepped = bench.ok(json!({ "action": "step", "kind": "in" })).await;
    assert!(
        stepped.contains("stopped: step") && stepped.contains("#0 inner at main.fk:6"),
        "{stepped}"
    );

    let resumed = bench.ok(json!({ "action": "continue" })).await;
    assert!(resumed.contains("condition x > 1"), "{resumed}");
    assert!(resumed.contains("#0 inner at main.fk:9"), "{resumed}");
    assert!(resumed.contains("output:\ncontinued"), "{resumed}");

    let state = bench.ok(json!({ "action": "state" })).await;
    assert!(
        state.contains("#0 inner at main.fk:9") && state.contains("line: int = 9"),
        "{state}"
    );

    assert_eq!(
        bench.ok(json!({ "action": "eval", "expression": "x + 1" })).await,
        "x + 1: int = 43"
    );
    let failed = bench.err(json!({ "action": "eval", "expression": "nope" })).await;
    assert!(failed.contains("cannot evaluate `nope`"), "{failed}");
    let missing = bench
        .err(json!({ "action": "eval", "expression": "x", "frame": 5 }))
        .await;
    assert!(missing.contains("frame 5 does not exist"), "{missing}");

    let output = bench.ok(json!({ "action": "output" })).await;
    assert_eq!(output, "hello\ncontinued");

    let last = bench.ok(json!({ "action": "continue" })).await;
    assert!(last.contains("exited with code 0"), "{last}");
    bench.ok(json!({ "action": "stop" })).await;
    let gone = bench.err(json!({ "action": "state" })).await;
    assert!(gone.contains("no debug session"), "{gone}");
}

#[tokio::test]
async fn a_program_without_breakpoints_runs_to_its_end() {
    let bench = bench();
    let started = bench
        .ok(json!({ "action": "start", "program": "app", "language": "rust" }))
        .await;
    assert!(started.contains("exited with code 3"), "{started}");
    assert!(started.contains("output:\nhello"), "{started}");
}

#[tokio::test]
async fn an_adapter_that_is_not_installed_is_named() {
    let bench = bench();
    let error = bench
        .err(json!({ "action": "start", "program": "app", "language": "go" }))
        .await;
    assert!(
        error.contains("no-such-debug-adapter") && error.contains("debug.adapters.go"),
        "{error}"
    );
    let unknown = bench
        .err(json!({ "action": "start", "program": "app", "language": "cobol" }))
        .await;
    assert!(unknown.contains("cobol"), "{unknown}");
}

#[tokio::test]
async fn a_program_that_does_not_stop_is_reported_after_the_time_limit_and_can_be_paused() {
    let bench = bench();
    let started = bench
        .ok(json!({ "action": "start", "program": "hang", "language": "rust", "timeout_secs": 1 }))
        .await;
    assert!(started.contains("still running after 1 s"), "{started}");
    let state = bench.ok(json!({ "action": "state" })).await;
    assert!(state.starts_with("the program is running"), "{state}");
    let busy = bench.err(json!({ "action": "step" })).await;
    assert!(busy.contains("`pause`"), "{busy}");
    let paused = bench.ok(json!({ "action": "pause" })).await;
    assert!(paused.contains("stopped: pause"), "{paused}");
}

#[tokio::test]
async fn a_cancelled_wait_returns_at_once() {
    let bench = bench();
    let info = AgentInfo {
        id: "lead".into(),
        def: "lead".into(),
        parent: None,
        depth: 0,
        session_id: "s".into(),
    };
    let cancel = CancellationToken::new();
    let ctx = ToolCtx {
        agent_id: info.id.clone(),
        session_id: "s".into(),
        agent: info,
        call_id: "c1".into(),
        cwd: bench.dir.path().to_path_buf(),
        cancel: cancel.clone(),
        update: UpdateSink::default(),
        services: bench.registry.services.clone(),
    };
    let tool: Arc<dyn Tool> = bench.registry.tools.get(&ctx.agent, "debug").unwrap();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
    });
    let output = tool
        .call(
            json!({ "action": "start", "program": "hang", "language": "rust", "timeout_secs": 60 }),
            &ctx,
        )
        .await;
    canceller.await.unwrap();
    assert!(
        output.is_error && output.text_content().contains("aborted"),
        "{}",
        output.text_content()
    );
}

#[tokio::test]
async fn the_session_ends_with_its_agent() {
    let bench = bench();
    bench
        .ok(json!({
            "action": "start", "program": "app", "language": "rust",
            "breakpoints": [{ "file": "a.fk", "line": 2 }]
        }))
        .await;
    let manager = bench.registry.services.get::<DebugManager>().unwrap();
    assert!(manager.has_session("lead"));
    let agent = AgentInfo {
        id: "lead".into(),
        def: "lead".into(),
        parent: None,
        depth: 0,
        session_id: "s".into(),
    };
    bench
        .registry
        .bus
        .emit(&mut Event::AgentEnd {
            agent,
            status: "done".into(),
        })
        .await;
    assert!(!manager.has_session("lead"));
}
