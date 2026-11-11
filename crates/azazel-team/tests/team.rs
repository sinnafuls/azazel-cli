#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_agent::{
    Access, Agent, AgentConfig, AgentEvent, AgentHandle, AgentInfo, CompactionSettings, Limits, Services,
    Tool, ToolCtx, ToolOutput, UserInput,
};
use azazel_ai::testing::{MockProvider, MockStep};
use azazel_ai::{AiError, Catalog, Message, Provider, RetryPolicy, Router, ThinkingLevel};
use azazel_core::agents::{DefSource, discover};
use azazel_core::permissions::plan::PermissionsExt;
use azazel_core::process::Shell;
use azazel_core::{
    Config, ExtCtx, Extension, Headless, LoadOptions, Paths, PermissionGate, PermissionState, Registrar,
    Registry, SessionStore, Ui,
};
use azazel_team::{BUILTIN_AGENTS, MemberState, Team, TeamDeps, TeamEvent, TeamExt};
use azazel_tools::CoreToolsExt;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

/// Sleeps `ms` milliseconds unless cancelled first, and remembers whether it was.
struct Nap {
    cancelled: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for Nap {
    fn name(&self) -> &str {
        "nap"
    }
    fn description(&self) -> &str {
        "sleeps"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn access(&self, _: &Value) -> Access {
        Access::Read
    }
    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }
    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let ms = input["ms"].as_u64().unwrap_or(300);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(ms)) => ToolOutput::text("rested"),
            _ = ctx.cancel.cancelled() => {
                self.cancelled.store(true, Ordering::SeqCst);
                ToolOutput::error("woken by a cancel")
            }
        }
    }
}

struct NapExt(Arc<AtomicBool>);

impl Extension for NapExt {
    fn id(&self) -> &str {
        "nap"
    }
    fn register(&self, r: &mut Registrar) -> Result<(), azazel_core::ExtError> {
        r.tool(Arc::new(Nap {
            cancelled: self.0.clone(),
        }));
        Ok(())
    }
}

/// Definitions the tests start agents from; each `task` entry picks its own mock model.
const WORKER: &str =
    "---\nname: worker\ndescription: Naps on request.\ntools: nap\n---\nYou nap when told to.\n";
const EDITOR: &str =
    "---\nname: editor\ndescription: Edits files.\ntools: read, edit, write, nap\n---\nYou edit.\n";
const FRAGILE: &str = "---\nname: fragile\ndescription: Has a fallback model.\nmodel: [\"mock/a\", \"mock/b\"]\n\
                       tools: nap\n---\nYou answer.\n";

struct Bench {
    dir: tempfile::TempDir,
    team: Arc<Team>,
    mock: Arc<MockProvider>,
    lead: AgentHandle,
    events: UnboundedReceiver<AgentEvent>,
    nap_cancelled: Arc<AtomicBool>,
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

impl Bench {
    async fn new(max_parallel: usize, repository: bool) -> Bench {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let agents = project.join(".azazel/agents");
        std::fs::create_dir_all(&agents).unwrap();
        for (name, text) in [("worker", WORKER), ("editor", EDITOR), ("fragile", FRAGILE)] {
            std::fs::write(agents.join(format!("{name}.md")), text).unwrap();
        }
        std::fs::write(project.join("f.txt"), "one\n").unwrap();
        if repository {
            std::fs::write(project.join(".gitignore"), ".azazel/\n").unwrap();
            git(&project, &["init", "-q"]);
            git(&project, &["config", "user.name", "t"]);
            git(&project, &["config", "user.email", "t@example.invalid"]);
            git(&project, &["config", "core.autocrlf", "false"]);
            git(&project, &["add", "-A"]);
            git(&project, &["commit", "-q", "-m", "first"]);
        }
        let paths = Paths::discover(&project, dir.path().join("home").to_str());

        let mut overrides = toml::Table::new();
        overrides.insert(
            "permissions".into(),
            toml::Value::Table(toml::Table::from_iter([("mode".to_string(), "auto".into())])),
        );
        let loaded = Config::load(
            &paths,
            &LoadOptions {
                trusted: true,
                overrides,
                env: Vec::new(),
            },
        )
        .unwrap();
        let permissions = Arc::new(PermissionState::new(&paths, &loaded).unwrap());

        let services = Arc::new(Services::new());
        services.insert(Arc::new(Ui(Arc::new(Headless))));
        let registry = Arc::new(Registry::new(ExtCtx {
            services: services.clone(),
            cwd: paths.cwd.clone(),
            session_id: String::new(),
        }));
        let shell = Shell::resolve("");
        registry
            .load(&CoreToolsExt {
                shell: shell.clone(),
                shell_timeout: Duration::from_secs(30),
                artifacts_dir: paths.artifacts_dir(),
            })
            .unwrap();
        registry
            .load(&PermissionsExt {
                state: permissions.clone(),
            })
            .unwrap();
        let nap_cancelled = Arc::new(AtomicBool::new(false));
        registry.load(&NapExt(nap_cancelled.clone())).unwrap();
        let defs = Arc::new(discover(BUILTIN_AGENTS, &[(agents, DefSource::Project)]));
        assert_eq!(defs.invalid(), []);
        registry
            .load(&TeamExt {
                defs: defs.clone(),
                max_depth: 2,
            })
            .unwrap();

        let mock = Arc::new(MockProvider::new());
        let providers: Vec<Arc<dyn Provider>> = vec![mock.clone()];
        let policy = RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            ..RetryPolicy::default()
        };
        let router = Arc::new(Router::new(
            providers,
            Catalog::default(),
            BTreeMap::new(),
            policy,
        ));
        let team = Arc::new(Team::new(TeamDeps {
            tools: registry.tools.clone(),
            hooks: registry.agent_hooks(),
            prompt: registry.prompt.clone(),
            bus: registry.bus.clone(),
            services: services.clone(),
            router: router.clone(),
            permissions: permissions.clone(),
            paths: paths.clone(),
            defs,
            max_parallel,
            max_depth: 2,
            max_result_bytes: 50_000,
            compaction: CompactionSettings::default(),
            posix_shell: shell.is_posix(),
        }));
        services.insert(team.clone());

        let session = Arc::new(SessionStore::create(&paths.sessions_dir(), &paths.cwd, None).unwrap());
        registry.bus.set_session(&session.id());
        let info = AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id: session.id(),
        };
        let gate = PermissionGate::new(
            permissions,
            registry.bus.clone(),
            services.clone(),
            shell.is_posix(),
        );
        let lead = Agent::spawn(AgentConfig {
            system: registry.prompt.render(&info).await,
            info,
            tools: registry.tools.clone(),
            chain: vec!["mock/lead".parse().unwrap()],
            thinking: ThinkingLevel::Off,
            router,
            gate: Arc::new(gate),
            bus: registry.agent_hooks(),
            services,
            cwd: paths.cwd.clone(),
            initial_messages: Vec::new(),
            sink: session.clone(),
            artifacts: None,
            limits: Limits::default(),
            compaction: CompactionSettings::default(),
        });
        team.attach(lead.clone(), session);
        let events = lead.subscribe();
        Bench {
            dir,
            team,
            mock,
            lead,
            events,
            nap_cancelled,
        }
    }

    fn project(&self) -> std::path::PathBuf {
        self.dir.path().join("project")
    }

    /// Queues one tool call of the lead.
    fn lead_calls(&self, id: &str, tool: &str, input: Value) {
        self.mock.push("lead", MockStep::tools("", &[(id, tool, input)]));
    }

    /// Runs the lead until it goes idle; returns `(call id, error?, text, duration)` of its tool calls.
    async fn run(&mut self, prompt: &str) -> Vec<(String, bool, String, Duration)> {
        assert!(self.lead.prompt(UserInput::text(prompt)));
        self.until_idle().await
    }

    async fn until_idle(&mut self) -> Vec<(String, bool, String, Duration)> {
        let mut calls = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(30), self.events.recv())
                .await
                .unwrap()
                .unwrap();
            match event {
                AgentEvent::ToolEnd {
                    call_id,
                    output,
                    duration_ms,
                } => calls.push((
                    call_id,
                    output.is_error,
                    output.text_content(),
                    Duration::from_millis(duration_ms),
                )),
                AgentEvent::Idle { .. } => return calls,
                AgentEvent::Failed { error } => panic!("the lead failed: {error}"),
                _ => {}
            }
        }
    }

    fn state(&self, id: &str) -> MemberState {
        self.team
            .list()
            .into_iter()
            .find(|member| member.id == id)
            .unwrap()
            .state
    }
}

fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m.role == azazel_ai::Role::User)
        .map(Message::text)
        .collect()
}

/// Three agents that each nap 300 ms, started by one `task` call.
async fn three_nappers(max_parallel: usize) -> (Duration, String) {
    let mut bench = Bench::new(max_parallel, false).await;
    let agents: Vec<Value> = (1..=3)
        .map(|n| json!({ "agent": "worker", "prompt": format!("nap {n}"), "model": format!("mock/w{n}") }))
        .collect();
    bench.lead_calls("t1", "task", json!({ "agents": agents }));
    bench.mock.push("lead", MockStep::text("all rested"));
    for n in 1..=3 {
        bench.mock.push(
            &format!("w{n}"),
            MockStep::tools("", &[("n", "nap", json!({ "ms": 300 }))]),
        );
        bench
            .mock
            .push(&format!("w{n}"), MockStep::text(format!("report {n}")));
    }
    let calls = bench.run("rest").await;
    assert!(!calls[0].1, "{}", calls[0].2);
    (calls[0].3, calls[0].2.clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_of_one_task_call_run_in_parallel_up_to_the_limit() {
    let (parallel, reports) = three_nappers(4).await;
    assert!(parallel < Duration::from_millis(700), "took {parallel:?}");
    for n in 1..=3 {
        assert!(
            reports.contains(&format!("## worker-{n} (done, 0s)\nreport {n}")),
            "{reports}"
        );
    }
    let (serial, _) = three_nappers(1).await;
    assert!(serial > Duration::from_millis(800), "took {serial:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_reaches_a_working_agent_before_its_next_request() {
    let mut bench = Bench::new(4, false).await;
    bench.lead_calls(
        "t1",
        "task",
        json!({ "agents": [{ "agent": "worker", "prompt": "check A", "model": "mock/w" }], "wait": false }),
    );
    // The message waits a moment, so the worker has started (and is inside its nap) when it lands.
    bench.mock.push(
        "lead",
        MockStep::tools(
            "",
            &[(
                "t2",
                "agent_send",
                json!({ "to": "worker-1", "message": "also check B" }),
            )],
        )
        .delayed(Duration::from_millis(250)),
    );
    bench.lead_calls("t3", "agent_wait", json!({}));
    bench.mock.push("lead", MockStep::text("done"));
    bench
        .mock
        .push("w", MockStep::tools("", &[("n", "nap", json!({ "ms": 600 }))]));
    bench.mock.push("w", MockStep::text("checked A and B"));
    let calls = bench.run("go").await;
    assert!(calls[0].2.starts_with("started: worker-1."), "{}", calls[0].2);
    assert_eq!(calls[1].2, "delivered to: worker-1");
    assert!(calls[2].2.starts_with("## worker-1 (done, "), "{}", calls[2].2);
    assert!(calls[2].2.ends_with("checked A and B"), "{}", calls[2].2);

    let requests = bench.mock.requests_for("w");
    assert_eq!(user_texts(&requests[0].messages), ["check A"]);
    let second = user_texts(&requests[1].messages);
    assert_eq!(
        second.last().unwrap(),
        "<agent-message from=\"lead\">\nalso check B\n</agent-message>"
    );

    // A sub-agent can write to its parent only.
    let worker = bench.team.member("worker-1").unwrap().0;
    assert!(bench.team.send(&worker, "lead", "hello").is_ok());
    assert!(
        bench
            .team
            .send(&worker, "worker-2", "hello")
            .unwrap_err()
            .contains("parent only")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_an_agent_cancels_what_it_is_doing() {
    let mut bench = Bench::new(4, false).await;
    bench.lead_calls(
        "t1",
        "task",
        json!({ "agents": [{ "agent": "worker", "prompt": "long nap", "model": "mock/w" }], "wait": false }),
    );
    // The stop waits a moment, so the worker is inside its nap when it arrives.
    bench.mock.push(
        "lead",
        MockStep::tools(
            "",
            &[(
                "t2",
                "agent_stop",
                json!({ "id": "worker-1", "reason": "not needed" }),
            )],
        )
        .delayed(Duration::from_millis(1500)),
    );
    bench.mock.push("lead", MockStep::text("stopped it"));
    // The result of the stopped agent reaches the lead as a message, which it answers.
    bench.mock.push("lead", MockStep::text("noted"));
    bench
        .mock
        .push("w", MockStep::tools("", &[("n", "nap", json!({ "ms": 20_000 }))]));
    let started = Instant::now();
    let calls = bench.run("go").await;
    assert_eq!(calls[1].2, "stopped: worker-1");

    while bench.state("worker-1") != MemberState::Stopped {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "still {:?}",
            bench.state("worker-1")
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(bench.nap_cancelled.load(Ordering::SeqCst));
    bench.until_idle().await;
    let delivered = bench
        .mock
        .requests_for("lead")
        .last()
        .map(|request| user_texts(&request.messages))
        .unwrap();
    assert!(
        delivered
            .last()
            .unwrap()
            .starts_with("<agent-result id=\"worker-1\" status=\"stopped\""),
        "{delivered:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(10));

    // Only the lead, or the agent that started it, may stop an agent.
    let worker = bench.team.member("worker-1").unwrap().0;
    assert!(!bench.team.manages(&worker, "worker-1"));
    assert!(bench.team.stop("nobody").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_falls_back_along_its_own_model_chain() {
    let mut bench = Bench::new(4, false).await;
    bench.lead_calls(
        "t1",
        "task",
        json!({ "agents": [{ "agent": "fragile", "prompt": "answer" }] }),
    );
    bench.mock.push("lead", MockStep::text("done"));
    bench
        .mock
        .push("a", MockStep::fail(AiError::Auth("revoked".into())));
    bench.mock.push("b", MockStep::text("from b"));
    let calls = bench.run("go").await;
    assert!(calls[0].2.ends_with("from b"), "{}", calls[0].2);
    assert_eq!(bench.mock.requests_for("a").len(), 1);
    assert_eq!(bench.mock.requests_for("b").len(), 1);
    assert_eq!(bench.mock.requests_for("lead").len(), 2);

    // A definition that does not exist, and a model nobody provides, are reported to the caller.
    bench.lead_calls(
        "t2",
        "task",
        json!({ "agents": [{ "agent": "ghost", "prompt": "x" }] }),
    );
    bench.lead_calls(
        "t3",
        "task",
        json!({ "agents": [{ "agent": "worker", "prompt": "x", "model": "nowhere/m" }] }),
    );
    bench.mock.push("lead", MockStep::text("done"));
    let calls = bench.run("again").await;
    assert!(
        calls[0].1
            && calls[0]
                .2
                .starts_with("no agent definition named `ghost`; there are: "),
        "{}",
        calls[0].2
    );
    assert!(calls[1].2.contains("(failed, "), "{}", calls[1].2);
    assert!(
        calls[1].2.contains("unknown provider `nowhere`"),
        "{}",
        calls[1].2
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_second_of_two_agents_editing_a_file_must_read_it_again() {
    let mut bench = Bench::new(4, false).await;
    let mut team_events = bench.team.subscribe();
    bench.lead_calls(
        "t1",
        "task",
        json!({ "agents": [
            { "agent": "editor", "name": "first", "prompt": "make it two", "model": "mock/e1" },
            { "agent": "editor", "name": "second", "prompt": "make it three", "model": "mock/e2" }
        ] }),
    );
    bench.mock.push("lead", MockStep::text("done"));
    bench.mock.push(
        "e1",
        MockStep::tools("", &[("r", "read", json!({ "path": "f.txt" }))]),
    );
    bench.mock.push(
        "e1",
        MockStep::tools(
            "",
            &[(
                "e",
                "edit",
                json!({ "path": "f.txt", "edits": [{ "old": "one", "new": "two" }] }),
            )],
        ),
    );
    bench.mock.push("e1", MockStep::text("it is two"));
    bench.mock.push(
        "e2",
        MockStep::tools("", &[("r", "read", json!({ "path": "f.txt" }))]),
    );
    bench
        .mock
        .push("e2", MockStep::tools("", &[("n", "nap", json!({ "ms": 500 }))]));
    bench.mock.push(
        "e2",
        MockStep::tools(
            "",
            &[(
                "e",
                "edit",
                json!({ "path": "f.txt", "edits": [{ "old": "one", "new": "three" }] }),
            )],
        ),
    );
    bench.mock.push("e2", MockStep::text("could not"));
    bench.run("go").await;
    assert_eq!(
        std::fs::read_to_string(bench.project().join("f.txt")).unwrap(),
        "two\n"
    );

    let mut edits = Vec::new();
    while let Ok(event) = team_events.try_recv() {
        if let TeamEvent::Agent { id, event } = event
            && let AgentEvent::ToolEnd { call_id, output, .. } = *event
            && call_id == "e"
        {
            edits.push((id, output.is_error, output.text_content()));
        }
    }
    edits.sort();
    assert_eq!(edits[0].0, "first");
    assert!(!edits[0].1, "{}", edits[0].2);
    assert_eq!((edits[1].0.as_str(), edits[1].1), ("second", true));
    assert_eq!(edits[1].2, "file changed since you last read it; read it again");

    // The second agent was told who changed the file before its request after the nap.
    let told = bench.mock.requests_for("e2").iter().any(|request| {
        user_texts(&request.messages)
            .iter()
            .any(|text| text.ends_with("f.txt was modified by first; read it again before editing"))
    });
    assert!(told);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_isolated_agents_work_lands_in_the_main_tree_when_applied() {
    let mut bench = Bench::new(4, true).await;
    bench.lead_calls("t1", "task", json!({ "agents": [{ "agent": "editor", "prompt": "add a file", "model": "mock/e", "isolation": "worktree" }] }));
    bench.lead_calls("t2", "worktree_apply", json!({ "agent": "editor-1" }));
    bench.mock.push("lead", MockStep::text("done"));
    bench.mock.push(
        "e",
        MockStep::tools(
            "",
            &[(
                "w",
                "write",
                json!({ "path": "new file.txt", "content": "made apart\n" }),
            )],
        ),
    );
    bench.mock.push("e", MockStep::text("added it"));
    let calls = bench.run("go").await;

    let report = &calls[0].2;
    assert!(
        report.contains("added it\n\nChanges made in the worktree"),
        "{report}"
    );
    assert!(report.contains("new file.txt"), "{report}");
    assert!(!calls[1].1, "{}", calls[1].2);
    assert_eq!(
        calls[1].2,
        "applied the changes of `editor-1` to the main tree and removed its worktree"
    );
    let project = bench.project();
    assert_eq!(
        std::fs::read_to_string(project.join("new file.txt")).unwrap(),
        "made apart\n"
    );
    assert!(!project.join(".azazel/worktrees/editor-1").exists());
    // The agent's system prompt told it where it works.
    let system = &bench.mock.requests_for("e")[0].system;
    assert!(system.iter().any(|part| {
        part.text
            .contains("You work in your own checkout of the project at")
    }));

    // Outside a repository an isolated agent cannot be started.
    let mut plain = Bench::new(4, false).await;
    plain.lead_calls(
        "t1",
        "task",
        json!({ "agents": [{ "agent": "editor", "prompt": "x", "isolation": "worktree" }] }),
    );
    plain.mock.push("lead", MockStep::text("done"));
    let calls = plain.run("go").await;
    assert_eq!(
        (calls[0].1, calls[0].2.as_str()),
        (true, "isolation: worktree requires a git repository")
    );
}
