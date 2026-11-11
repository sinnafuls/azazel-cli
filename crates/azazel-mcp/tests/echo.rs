#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use azazel_agent::{
    Agent, AgentConfig, AgentEvent, AgentHandle, AgentInfo, AllowAll, CompactionSettings, Limits, NullSink,
    Services, ToolSource, UserInput,
};
use azazel_ai::testing::{MockProvider, MockStep};
use azazel_ai::{Block, Catalog, Credentials, Provider, Request, RetryPolicy, Role, Router, ThinkingLevel};
use azazel_core::process::Shell;
use azazel_core::{CommandOutcome, Event, ExtCtx, Headless, Registry, StartSource, Ui};
use azazel_mcp::{McpExt, McpManager, Options, State, config};
use azazel_tools::CoreToolsExt;
use serde_json::json;

/// The `mcp-echo` fixture server, built on first use into the directory this test runs from.
static FIXTURE: LazyLock<PathBuf> = LazyLock::new(|| {
    // <target>/<profile>/deps/<this test>
    let test = std::env::current_exe().unwrap();
    let profile_dir = test.parent().unwrap().parent().unwrap();
    let mut build = Command::new(env!("CARGO"));
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

struct Bench {
    dir: tempfile::TempDir,
    manager: McpManager,
    registry: Arc<Registry>,
    mock: Arc<MockProvider>,
    agent: AgentHandle,
    events: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
}

fn lead() -> AgentInfo {
    AgentInfo {
        id: "lead".into(),
        def: "lead".into(),
        parent: None,
        depth: 0,
        session_id: "s1".into(),
    }
}

impl Bench {
    /// A session whose only MCP server is the echo fixture.
    async fn new(defer_threshold: usize) -> Bench {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        let entry = json!({ "mcpServers": { "echo": {
            "command": FIXTURE.as_path(),
            "env": { "MCP_ECHO_PID_FILE": dir.path().join("pid") }
        } } });
        std::fs::write(&file, entry.to_string()).unwrap();
        let loaded = config::load(&[file], dir.path(), &|_| None);
        assert_eq!(loaded.problems, []);

        let services = Arc::new(Services::new());
        services.insert(Arc::new(Ui(Arc::new(Headless))));
        let registry = Arc::new(Registry::new(ExtCtx {
            services: services.clone(),
            cwd: dir.path().to_path_buf(),
            session_id: "s1".into(),
        }));
        registry
            .load(&CoreToolsExt {
                shell: Shell::resolve(""),
                shell_timeout: Duration::from_secs(5),
                artifacts_dir: dir.path().join("artifacts"),
            })
            .unwrap();
        let options = Options {
            startup_wait: Duration::from_secs(30),
            defer_threshold,
        };
        let credentials = Arc::new(Credentials::new(dir.path().join("auth.json")));
        let manager = McpManager::new(
            loaded,
            services.clone(),
            vec![dir.path().to_path_buf()],
            credentials,
            options,
        );
        registry
            .load(&McpExt {
                manager: manager.clone(),
            })
            .unwrap();
        registry
            .bus
            .emit(&mut Event::SessionStart {
                source: StartSource::Startup,
            })
            .await;

        let mock = Arc::new(MockProvider::new());
        let providers: Vec<Arc<dyn Provider>> = vec![mock.clone()];
        let router = Arc::new(Router::new(
            providers,
            Catalog::default(),
            BTreeMap::new(),
            RetryPolicy::default(),
        ));
        let agent = Agent::spawn(AgentConfig {
            info: lead(),
            system: Vec::new(),
            tools: registry.tools.clone(),
            chain: vec!["mock/a".parse().unwrap()],
            thinking: ThinkingLevel::Off,
            router,
            gate: Arc::new(AllowAll),
            bus: registry.agent_hooks(),
            services,
            cwd: dir.path().to_path_buf(),
            initial_messages: Vec::new(),
            sink: Arc::new(NullSink::default()),
            artifacts: None,
            limits: Limits::default(),
            compaction: CompactionSettings::default(),
        });
        let events = agent.subscribe();
        Bench {
            dir,
            manager,
            registry,
            mock,
            agent,
            events,
        }
    }

    /// Runs one prompt to the end and returns the text of every tool result, in order.
    async fn run(&mut self, prompt: &str) -> Vec<String> {
        assert!(self.agent.prompt(UserInput::text(prompt)));
        let mut results = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(60), self.events.recv())
                .await
                .unwrap()
                .unwrap();
            match event {
                AgentEvent::ToolEnd { output, .. } => results.push(output.text_content()),
                AgentEvent::Idle { .. } => return results,
                AgentEvent::Failed { error } => panic!("the agent failed: {error}"),
                _ => {}
            }
        }
    }

    fn pid(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join("pid"))
            .ok()
            .filter(|pid| !pid.is_empty())
    }
}

fn tool_names(request: &Request) -> Vec<&str> {
    request.tools.iter().map(|tool| tool.name.as_str()).collect()
}

fn kill(pid: &str) {
    let status = if cfg!(windows) {
        Command::new("taskkill")
            .args(["/PID", pid, "/F"])
            .output()
            .unwrap()
            .status
    } else {
        Command::new("kill").args(["-9", pid]).output().unwrap().status
    };
    assert!(status.success(), "could not kill process {pid}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_tool_is_listed_called_and_called_again_after_the_server_was_killed() {
    let mut bench = Bench::new(40).await;
    bench.mock.push(
        "a",
        MockStep::tools("", &[("c1", "mcp__echo__echo", json!({ "text": "hi" }))]),
    );
    bench.mock.push("a", MockStep::text("done"));
    assert_eq!(bench.run("say hi").await, ["hi"]);

    let requests = bench.mock.requests_for("a");
    let offered = tool_names(&requests[0]);
    for expected in ["mcp__echo__echo", "mcp__echo__add", "list_mcp_resources", "read"] {
        assert!(
            offered.contains(&expected),
            "{expected} is missing from {offered:?}"
        );
    }
    assert!(!offered.contains(&"tool_search"));
    let echo = requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "mcp__echo__echo")
        .unwrap();
    assert_eq!(echo.description, "Returns the text it was given");
    assert_eq!(echo.schema["required"], json!(["text"]));
    // Only the tool that says so is read-only.
    let tools = bench.registry.tools.tools(&lead());
    let access = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.name() == name)
            .unwrap()
            .access(&json!({}))
    };
    assert_eq!(access("mcp__echo__add"), azazel_agent::Access::Read);
    assert_eq!(access("mcp__echo__echo"), azazel_agent::Access::Exec);

    let first = bench.pid().unwrap();
    kill(&first);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reconnected = bench.pid().is_some_and(|pid| pid != first)
            && bench
                .manager
                .status()
                .iter()
                .all(|server| server.state == State::Ready);
        if reconnected {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the server did not come back: {:?}",
            bench.manager.status()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bench.mock.push(
        "a",
        MockStep::tools("", &[("c2", "mcp__echo__add", json!({ "a": 2, "b": 40 }))]),
    );
    bench.mock.push("a", MockStep::text("done"));
    assert_eq!(bench.run("add").await, ["42"]);
    bench.manager.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn with_too_many_tools_they_are_found_through_tool_search() {
    let mut bench = Bench::new(1).await;
    bench.mock.push(
        "a",
        MockStep::tools("", &[("c1", "tool_search", json!({ "query": "echo" }))]),
    );
    bench.mock.push(
        "a",
        MockStep::tools("", &[("c2", "mcp__echo__echo", json!({ "text": "hi" }))]),
    );
    bench.mock.push("a", MockStep::text("done"));
    let results = bench.run("find and use echo").await;
    assert!(
        results[0].starts_with("## mcp__echo__echo\nReturns the text it was given\ninput schema: {"),
        "{}",
        results[0]
    );
    assert_eq!(results[1], "hi");

    let requests = bench.mock.requests_for("a");
    let before = tool_names(&requests[0]);
    assert!(before.contains(&"tool_search"));
    assert!(!before.iter().any(|name| name.starts_with("mcp__")), "{before:?}");
    let after = tool_names(&requests[1]);
    assert!(
        after.contains(&"mcp__echo__echo") && after.contains(&"tool_search"),
        "{after:?}"
    );
    bench.manager.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn resources_prompts_and_instructions_reach_the_session() {
    let mut bench = Bench::new(40).await;
    bench.mock.push(
        "a",
        MockStep::tools(
            "",
            &[("c1", "read", json!({ "path": "mcp://echo/mem://greeting" }))],
        ),
    );
    bench.mock.push(
        "a",
        MockStep::tools("", &[("c2", "list_mcp_resources", json!({}))]),
    );
    bench.mock.push(
        "a",
        MockStep::tools(
            "",
            &[("c3", "read", json!({ "path": "mcp://echo/mem://nothing" }))],
        ),
    );
    bench.mock.push("a", MockStep::text("done"));
    let results = bench.run("read the greeting").await;
    assert_eq!(results[0], "hello from the echo fixture");
    assert_eq!(results[1], "mcp://echo/mem://greeting  greeting\n");
    assert!(results[2].contains("MCP server `echo`"), "{}", results[2]);

    // The second request carries the first tool result back to the model.
    let requests = bench.mock.requests_for("a");
    let returned = requests[1]
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Tool)
        .unwrap();
    assert!(matches!(
        &returned.content[0],
        Block::ToolResult { is_error: false, .. }
    ));

    let status = bench.manager.status();
    assert_eq!(
        (status[0].name.as_str(), status[0].tools, status[0].prompts),
        ("echo", 2, 1)
    );
    let system = bench.registry.prompt.render(&lead()).await;
    assert!(system.iter().any(|part| {
        part.text
            .contains("## echo\nEcho fixture: `echo` returns its text")
    }));

    let greet = bench.registry.command("mcp:echo:greet").unwrap();
    assert_eq!(greet.argument_hint.as_deref(), Some("<name>"));
    let context = bench.registry.bus.ctx();
    assert_eq!(
        greet.handler.run("Ada", &context).await,
        CommandOutcome::Prompt {
            text: "Say hello to Ada.".into(),
            model: None
        }
    );
    assert_eq!(
        greet.handler.run("", &context).await,
        CommandOutcome::Error("missing: name".into())
    );

    let report = bench
        .registry
        .command("mcp")
        .unwrap()
        .handler
        .run("", &context)
        .await;
    let CommandOutcome::Show(report) = report else {
        panic!("{report:?}")
    };
    assert!(
        report.starts_with("echo  ready · 2 tools · 1 prompts\n"),
        "{report}"
    );
    bench.manager.shutdown();
}
