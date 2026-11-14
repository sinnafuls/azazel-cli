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
    _dir: tempfile::TempDir,
    pid_file: PathBuf,
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

/// How a [`Bench`] differs from a session whose echo server is connected from the start.
struct Setup {
    defer_threshold: usize,
    /// Keys of the server's entry beside its command.
    entry: serde_json::Value,
    idle_timeout: Option<Duration>,
    /// Where what servers offer is kept between sessions.
    cache_dir: Option<PathBuf>,
    /// Where the server writes its process id; a file of the session's own when `None`. Sessions
    /// that are to share what is kept of the server need the same entry, so the same file.
    pid_file: Option<PathBuf>,
}

impl Setup {
    /// A server that starts on demand, as a local program does unless its entry says otherwise.
    fn on_demand(shared: &Path) -> Setup {
        Setup {
            defer_threshold: 40,
            entry: json!({}),
            idle_timeout: None,
            cache_dir: Some(shared.join("cache")),
            pid_file: Some(shared.join("pid")),
        }
    }
}

impl Bench {
    /// A session whose only MCP server is the echo fixture, connected when the session starts.
    async fn new(defer_threshold: usize) -> Bench {
        Bench::with(Setup {
            defer_threshold,
            entry: json!({ "lifecycle": "eager" }),
            idle_timeout: None,
            cache_dir: None,
            pid_file: None,
        })
        .await
    }

    async fn with(setup: Setup) -> Bench {
        let Setup {
            defer_threshold,
            entry: extra,
            idle_timeout,
            cache_dir,
            pid_file,
        } = setup;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        let pid_file = pid_file.unwrap_or_else(|| dir.path().join("pid"));
        // Whatever an earlier session started is none of this one's.
        let _ = std::fs::remove_file(&pid_file);
        let mut server = json!({
            "command": FIXTURE.as_path(),
            "env": { "MCP_ECHO_PID_FILE": &pid_file }
        });
        for (key, value) in extra.as_object().unwrap() {
            server[key] = value.clone();
        }
        let entry = json!({ "mcpServers": { "echo": server } });
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
                read_guard: Default::default(),
                patch: Default::default(),
            })
            .unwrap();
        let options = Options {
            startup_wait: Duration::from_secs(30),
            defer_threshold,
            idle_timeout,
            cache_dir,
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
            _dir: dir,
            pid_file,
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
        std::fs::read_to_string(&self.pid_file)
            .ok()
            .filter(|pid| !pid.is_empty())
    }
}

impl Bench {
    fn state(&self) -> (State, String) {
        let status = self.manager.status().remove(0);
        (status.state.clone(), status.state_text())
    }

    /// Waits until the echo server is in `state`.
    async fn until(&self, state: State) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.state().0 != state {
            assert!(
                Instant::now() < deadline,
                "the server is {:?}, not {state:?}",
                self.state()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Runs a prompt in which the lead calls `tool` once, and returns the result.
    async fn call(&mut self, tool: &str, input: serde_json::Value) -> String {
        self.mock.push("a", MockStep::tools("", &[("c", tool, input)]));
        self.mock.push("a", MockStep::text("done"));
        self.run("go").await.remove(0)
    }
}

/// Whether the process `pid` still runs.
fn alive(pid: &str) -> bool {
    if cfg!(windows) {
        let listed = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&listed.stdout).contains(&format!("\"{pid}\""))
    } else {
        Command::new("kill")
            .args(["-0", pid])
            .output()
            .unwrap()
            .status
            .success()
    }
}

/// Waits until the process `pid` has ended.
async fn gone(pid: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} still runs");
        tokio::time::sleep(Duration::from_millis(50)).await;
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

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_starts_on_demand_is_asked_once_and_then_started_by_its_first_call() {
    let cache = tempfile::tempdir().unwrap();

    // Nothing is known of the server yet: the first session starts it to ask what it has, and
    // keeps the answer.
    let mut first = Bench::with(Setup::on_demand(cache.path())).await;
    assert_eq!(first.call("mcp__echo__echo", json!({ "text": "hi" })).await, "hi");
    assert!(first.pid().is_some());
    let kept = std::fs::read_to_string(cache.path().join("cache").join("echo.json")).unwrap();
    assert!(
        kept.contains("Returns the text it was given") && kept.contains("Echo fixture"),
        "{kept}"
    );
    let started = first.pid().unwrap();
    first.manager.shutdown();
    gone(&started).await;

    // The next session offers the tools, the prompt and the instructions from what was kept,
    // and starts nothing.
    let mut second = Bench::with(Setup::on_demand(cache.path())).await;
    second.mock.push("a", MockStep::text("nothing to do"));
    second.run("hello").await;
    let offered = second.mock.requests_for("a");
    let names = tool_names(&offered[0]);
    assert!(
        names.contains(&"mcp__echo__echo") && names.contains(&"mcp__echo__add"),
        "{names:?}"
    );
    let add = second
        .registry
        .tools
        .tools(&lead())
        .into_iter()
        .find(|tool| tool.name() == "mcp__echo__add")
        .unwrap();
    assert_eq!(add.access(&json!({})), azazel_agent::Access::Read);
    assert_eq!(
        second.state(),
        (State::Idle, "not started (cached 2 tools)".to_string())
    );
    let system = second.registry.prompt.render(&lead()).await;
    assert!(
        system
            .iter()
            .any(|part| part.text.contains("## echo\nEcho fixture"))
    );
    let greet = second.registry.command("mcp:echo:greet").unwrap();
    assert_eq!(second.pid(), None, "nothing has called the server yet");

    // The first call starts it; the session goes on with it connected.
    assert_eq!(
        second.call("mcp__echo__add", json!({ "a": 2, "b": 40 })).await,
        "42"
    );
    assert!(second.pid().is_some());
    assert_eq!(second.state().0, State::Ready);
    second.manager.shutdown();

    // A prompt of a server that is not running starts it as a call does.
    let third = Bench::with(Setup::on_demand(cache.path())).await;
    assert_eq!(third.pid(), None);
    let greet_again = third.registry.command("mcp:echo:greet").unwrap();
    assert_eq!(greet_again.argument_hint, greet.argument_hint);
    assert_eq!(
        greet_again.handler.run("Ada", &third.registry.bus.ctx()).await,
        CommandOutcome::Prompt {
            text: "Say hello to Ada.".into(),
            model: None
        }
    );
    assert!(third.pid().is_some());
    third.manager.shutdown();

    // An entry that changed is another server for all the cache knows: it is asked again.
    let mut changed = Setup::on_demand(cache.path());
    changed.entry = json!({ "timeout": 30 });
    let changed = Bench::with(changed).await;
    changed.until(State::Ready).await;
    assert!(changed.pid().is_some());
    changed.manager.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_of_a_server_that_is_not_running_are_found_through_tool_search() {
    let cache = tempfile::tempdir().unwrap();
    let first = Bench::with(Setup::on_demand(cache.path())).await;
    first.until(State::Ready).await;
    first.manager.shutdown();

    let mut setup = Setup::on_demand(cache.path());
    setup.defer_threshold = 1;
    let mut bench = Bench::with(setup).await;
    let found = bench
        .call("tool_search", json!({ "query": "sum two integers" }))
        .await;
    assert!(
        found.starts_with("## mcp__echo__add\nAdds two integers\n"),
        "{found}"
    );
    assert_eq!(bench.pid(), None, "searching starts nothing");
    assert_eq!(bench.call("mcp__echo__add", json!({ "a": 1, "b": 2 })).await, "3");
    assert!(bench.pid().is_some());
    bench.manager.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_started_on_demand_is_stopped_when_idle_and_started_again_by_the_next_call() {
    let cache = tempfile::tempdir().unwrap();
    let mut setup = Setup::on_demand(cache.path());
    setup.idle_timeout = Some(Duration::from_millis(300));
    let mut bench = Bench::with(setup).await;
    assert_eq!(
        bench.call("mcp__echo__echo", json!({ "text": "one" })).await,
        "one"
    );
    let first = bench.pid().unwrap();

    bench.until(State::Idle).await;
    gone(&first).await;
    assert_eq!(bench.state().1, "not started (cached 2 tools)");

    assert_eq!(
        bench.call("mcp__echo__echo", json!({ "text": "two" })).await,
        "two"
    );
    let second = bench.pid().unwrap();
    assert_ne!(second, first);
    bench.until(State::Idle).await;
    gone(&second).await;
    bench.manager.shutdown();

    // One that is kept alive once started stays, however long nothing calls it.
    let mut setup = Setup::on_demand(cache.path());
    setup.entry = json!({ "lifecycle": "lazy-keep-alive" });
    setup.idle_timeout = Some(Duration::from_millis(100));
    let mut kept = Bench::with(setup).await;
    assert_eq!(
        kept.call("mcp__echo__echo", json!({ "text": "three" })).await,
        "three"
    );
    let pid = kept.pid().unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(kept.state().0, State::Ready);
    assert!(alive(&pid));
    kept.manager.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_entry_chooses_which_of_its_servers_tools_are_offered() {
    let cache = tempfile::tempdir().unwrap();
    for (entry, expected) in [
        (json!({ "excludeTools": ["a*"] }), vec!["mcp__echo__echo"]),
        (
            json!({ "includeTools": ["add", "nothing"] }),
            vec!["mcp__echo__add"],
        ),
        (
            json!({ "includeTools": ["*"], "excludeTools": ["ech?"], "lifecycle": "eager" }),
            vec!["mcp__echo__add"],
        ),
    ] {
        let mut setup = Setup::on_demand(cache.path());
        setup.entry = entry.clone();
        let mut bench = Bench::with(setup).await;
        bench.mock.push("a", MockStep::text("nothing to do"));
        bench.run("hello").await;
        let requests = bench.mock.requests_for("a");
        let offered: Vec<&str> = tool_names(&requests[0])
            .into_iter()
            .filter(|name| name.starts_with("mcp__"))
            .collect();
        assert_eq!(offered, expected, "{entry}");
        assert_eq!(bench.manager.status()[0].tools, 1, "{entry}");
        // What is left out cannot be called either.
        let hidden = if expected[0].ends_with("add") {
            "mcp__echo__echo"
        } else {
            "mcp__echo__add"
        };
        let refused = bench.call(hidden, json!({ "text": "x", "a": 1, "b": 1 })).await;
        assert!(refused.contains(hidden), "{refused}");
        bench.manager.shutdown();
    }
}
