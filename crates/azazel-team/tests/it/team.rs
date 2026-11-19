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
use azazel_ai::{AiError, Catalog, Message, Provider, RetryPolicy, Router};
use azazel_core::agents::{DefSource, discover};
use azazel_core::config::ModelRouting;
use azazel_core::permissions::plan::PermissionsExt;
use azazel_core::process::Shell;
use azazel_core::side::{Side, SideCosts};
use azazel_core::{
    CommandOutcome, Config, ExtCtx, Extension, Headless, LoadOptions, Paths, PermissionGate, PermissionState,
    Post, Registrar, Registry, SessionStore, Ui,
};
use azazel_team::{
    BUILTIN_AGENTS, Delivery, MemberState, Mentioned, RoutingSettings, Team, TeamDeps, TeamEvent, TeamExt,
};
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
/// Its result is typed by the definition itself.
const COUNTER: &str = "---\nname: counter\ndescription: Counts things.\ntools: nap\noutput-schema:\n  type: object\n  \
                       properties:\n    count:\n      type: integer\n  required: [count]\n---\nYou count.\n";
const FRAGILE: &str = "---\nname: fragile\ndescription: Has a fallback model.\nmodel: [\"mock/a\", \"mock/b\"]\n\
                       tools: nap\n---\nYou answer.\n";

struct Bench {
    dir: tempfile::TempDir,
    team: Arc<Team>,
    registry: Arc<Registry>,
    mock: Arc<MockProvider>,
    lead: AgentHandle,
    session: Arc<SessionStore>,
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
        Bench::build(max_parallel, repository, None).await
    }

    /// A bench whose team routes models with `routing`: `@default` is `mock/mid`, `@smol` (the
    /// rater) is `mock/smol`.
    async fn routed(routing: RoutingSettings) -> Bench {
        Bench::build(4, false, Some(routing)).await
    }

    async fn build(max_parallel: usize, repository: bool, routing: Option<RoutingSettings>) -> Bench {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let agents = project.join(".azazel/agents");
        std::fs::create_dir_all(&agents).unwrap();
        for (name, text) in [
            ("worker", WORKER),
            ("editor", EDITOR),
            ("fragile", FRAGILE),
            ("counter", COUNTER),
        ] {
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
                read_guard: Default::default(),
                patch: Default::default(),
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
        // The built-in agents the commands start run on roles of their own.
        let mut roles = ["reviewer", "scout"]
            .into_iter()
            .map(|role| (role.to_string(), vec![format!("mock/{role}").parse().unwrap()]))
            .collect::<BTreeMap<_, _>>();
        if routing.is_some() {
            roles.insert("default".to_string(), vec!["mock/mid".parse().unwrap()]);
            roles.insert("smol".to_string(), vec!["mock/smol".parse().unwrap()]);
            roles.insert("slow".to_string(), vec!["mock/big".parse().unwrap()]);
        }
        let mut catalog = Catalog::default();
        if routing.is_some() {
            // Priced, so that a move has a saving: `mid` costs 3 per million input tokens, `fast` 1.
            catalog.merge(["mid", "fast"].map(|id| {
                let mut info = azazel_ai::ModelInfo::unknown("mock", id);
                info.cost = Some(azazel_ai::Cost {
                    input: if id == "mid" { 3.0 } else { 1.0 },
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                });
                info
            }));
        }
        let router = Arc::new(Router::new(providers, catalog, roles, policy));
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
            max_turns: 0,
            max_result_bytes: 50_000,
            compaction: CompactionSettings::default(),
            posix_shell: shell.is_posix(),
            routing: routing.unwrap_or_default(),
        }));
        services.insert(team.clone());
        services.insert(Arc::new(Side {
            router: router.clone(),
            prompt: registry.prompt.clone(),
            tools: registry.tools.clone(),
            costs: Arc::new(SideCosts::default()),
        }));

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
            thinking: None,
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
        team.attach(lead.clone(), session.clone());
        let events = lead.subscribe();
        Bench {
            dir,
            team,
            registry,
            mock,
            lead,
            session,
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

    /// Runs a slash command the way the UI does and returns what it says.
    async fn command(&self, name: &str, args: &str) -> Result<String, String> {
        let command = self.registry.command(name).unwrap();
        match command.handler.run(args, &self.registry.bus.ctx()).await {
            CommandOutcome::Show(text) => Ok(text),
            CommandOutcome::Error(why) => Err(why),
            _ => panic!("/{name} neither showed nor failed"),
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
    assert_eq!(calls[1].2, "worker-1: delivered at its next step");
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

#[tokio::test(flavor = "multi_thread")]
async fn the_user_starts_a_reviewer_or_any_agent_and_the_lead_gets_the_report() {
    let mut bench = Bench::new(4, true).await;
    // Nothing changed yet, and a ref that is none: no agent is started for either.
    let clean = bench.command("review", "").await.unwrap();
    assert!(clean.contains("no uncommitted changes"), "{clean}");
    let unknown = bench.command("review", "no-such-branch").await.unwrap_err();
    assert!(unknown.contains("is not a commit, branch or tag"), "{unknown}");
    assert!(
        bench
            .command("review", "--output=x")
            .await
            .unwrap_err()
            .starts_with("usage: /review")
    );
    assert_eq!(bench.team.list().len(), 1);

    std::fs::write(bench.project().join("f.txt"), "one\ntwo\n").unwrap();
    std::fs::write(bench.project().join("new.txt"), "fresh\n").unwrap();
    bench
        .mock
        .push("reviewer", MockStep::text("approve: nothing found"));
    bench.mock.push("lead", MockStep::text("the review is in"));
    let started = bench.command("review", "").await.unwrap();
    assert_eq!(
        started,
        "started reviewer-1; its report goes to the lead when it is done"
    );
    bench.until_idle().await;
    let task = user_texts(&bench.mock.requests_for("reviewer")[0].messages).join("\n");
    assert!(task.contains("Run `git diff HEAD`"), "{task}");
    assert!(task.contains("- new.txt\n"), "{task}");
    assert!(task.contains("Do not edit, create or delete any file"), "{task}");
    let told = user_texts(&bench.mock.requests_for("lead")[0].messages).join("\n");
    assert!(told.contains("approve: nothing found"), "{told}");

    // The commits since a ref are reviewed with the three-dot diff.
    bench.mock.push("reviewer", MockStep::text("approve"));
    bench.mock.push("lead", MockStep::text("noted"));
    bench.command("review", "HEAD").await.unwrap();
    bench.until_idle().await;
    let task = user_texts(&bench.mock.requests_for("reviewer")[1].messages).join("\n");
    assert!(task.contains("Run `git diff HEAD...HEAD`"), "{task}");

    // `/task` starts any definition, under a name when one is given.
    let listing = bench.command("task", "").await.unwrap();
    assert!(
        listing.contains("scout: ") && listing.contains("worker: Naps on request."),
        "{listing}"
    );
    bench.mock.push("scout", MockStep::text("f.txt holds two lines"));
    bench.mock.push("lead", MockStep::text("thanks"));
    let started = bench
        .command("task", "scout name=finder what is in f.txt?")
        .await
        .unwrap();
    assert!(started.starts_with("started finder;"), "{started}");
    bench.until_idle().await;
    let task = user_texts(&bench.mock.requests_for("scout")[0].messages).join("\n");
    assert!(task.contains("what is in f.txt?"), "{task}");
    assert_eq!(bench.state("finder"), MemberState::Done);
    let told = user_texts(&bench.mock.requests_for("lead")[2].messages).join("\n");
    assert!(told.contains("f.txt holds two lines"), "{told}");

    let missing = bench.command("task", "ghost do it").await.unwrap_err();
    assert!(
        missing.starts_with("no agent definition named `ghost`"),
        "{missing}"
    );
    assert!(
        bench
            .command("task", "scout")
            .await
            .unwrap_err()
            .starts_with("usage: /task")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mention_starts_an_agent_from_a_definition_or_reaches_the_agent_of_that_id() {
    let mut bench = Bench::new(4, false).await;
    let mut team_events = bench.team.subscribe();
    // Neither an agent nor a definition: not a mention, and nothing is started.
    assert!(bench.team.mention("ghost", "do it").await.is_none());
    assert!(bench.team.mention("f.txt", "explain").await.is_none());
    assert_eq!(bench.team.list().len(), 1);

    bench.mock.push("scout", MockStep::text("f.txt holds one line"));
    bench.mock.push("lead", MockStep::text("thanks"));
    let started = bench
        .team
        .mention("scout", "what is in f.txt?")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        started,
        Mentioned {
            agent: "scout-1".to_string(),
            started: true
        }
    );
    bench.until_idle().await;
    let told = user_texts(&bench.mock.requests_for("lead")[0].messages).join("\n");
    assert!(told.contains("f.txt holds one line"), "{told}");

    // The id of that agent names it from now on: the text is a message, which starts it again.
    bench.mock.push("scout", MockStep::text("it says one"));
    bench.mock.push("lead", MockStep::text("noted"));
    let sent = bench
        .team
        .mention("scout-1", "what does it say?")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        sent,
        Mentioned {
            agent: "scout-1".to_string(),
            started: false
        }
    );
    bench.until_idle().await;
    assert_eq!(bench.team.list().len(), 2, "no second agent was started");
    let asked = user_texts(&bench.mock.requests_for("scout")[1].messages);
    assert_eq!(asked.last().map(String::as_str), Some("what does it say?"));

    // What is posted to an agent from outside its turn (the answers to a question it asked
    // without waiting) reaches it the same way; an agent that is not there is not reached.
    let post = bench.registry.services.get::<Post>().unwrap();
    assert!(!(post.0)("ghost", "hello".to_string()));
    bench.mock.push("scout", MockStep::text("understood"));
    bench.mock.push("lead", MockStep::text("fine"));
    assert!((post.0)(
        "scout-1",
        "<user-answer>\nWhich?\nblue\n</user-answer>".to_string()
    ));
    bench.until_idle().await;
    let answered = user_texts(&bench.mock.requests_for("scout")[2].messages);
    assert!(
        answered.last().is_some_and(|text| text.contains("Which?\nblue")),
        "{answered:?}"
    );

    // Whoever watches the team is told what the agent was handed: its task, then each message.
    let mut handed = Vec::new();
    while let Ok(event) = team_events.try_recv() {
        if let TeamEvent::Input { id, text } = event {
            handed.push((id, text));
        }
    }
    let handed: Vec<(&str, &str)> = handed
        .iter()
        .map(|(id, text)| (id.as_str(), text.as_str()))
        .collect();
    assert_eq!(
        handed,
        [
            ("scout-1", "what is in f.txt?"),
            ("scout-1", "what does it say?"),
            ("scout-1", "<user-answer>\nWhich?\nblue\n</user-answer>"),
        ]
    );
}

fn tool_names(request: &azazel_ai::Request) -> Vec<String> {
    let mut names: Vec<String> = request.tools.iter().map(|tool| tool.name.clone()).collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread")]
async fn a_typed_agent_corrects_a_refused_report_and_its_result_is_json() {
    let mut bench = Bench::new(4, false).await;
    let schema = json!({
        "type": "object",
        "properties": { "count": { "type": "integer" } },
        "required": ["count"],
        "additionalProperties": false
    });
    bench.lead_calls(
        "t1",
        "task",
        json!({ "agents": [{ "agent": "worker", "prompt": "count the files", "model": "mock/w", "schema": schema }] }),
    );
    bench.lead_calls("t2", "read", json!({ "path": "agent://worker-1" }));
    bench.mock.push("lead", MockStep::text("three files"));
    bench.mock.push(
        "w",
        MockStep::tools("", &[("r1", "report", json!({ "result": { "count": "three" } }))]),
    );
    bench.mock.push(
        "w",
        MockStep::tools("", &[("r2", "report", json!({ "result": { "count": 3 } }))]),
    );
    bench.mock.push("w", MockStep::text("reported"));
    let calls = bench.run("how many files?").await;

    // The caller gets the value in a fenced block; `agent://` gives the value alone.
    assert!(!calls[0].1, "{}", calls[0].2);
    assert!(calls[0].2.starts_with("## worker-1 (done, "), "{}", calls[0].2);
    assert!(
        calls[0].2.ends_with("```json\n{\n  \"count\": 3\n}\n```"),
        "{}",
        calls[0].2
    );
    assert!(calls[1].2.contains("\"count\": 3"), "{}", calls[1].2);
    assert!(!calls[1].2.contains("```"), "{}", calls[1].2);
    let result = bench.team.result("worker-1").unwrap();
    assert_eq!(result.state, MemberState::Done);
    assert_eq!(result.json.as_deref(), Some("{\n  \"count\": 3\n}"));

    // Only the typed agent is offered `report`, whatever its definition grants, and it is told
    // to finish with it.
    let requests = bench.mock.requests_for("w");
    assert_eq!(requests.len(), 3);
    assert_eq!(tool_names(&requests[0]), ["agent_send", "nap", "report"]);
    let report = requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "report")
        .unwrap();
    assert_eq!(report.schema["properties"]["result"], schema);
    let identity = &requests[0].system[0].text;
    assert!(identity.contains("finish by calling `report`"), "{identity}");
    assert!(
        !identity.contains("Your last message is your report"),
        "{identity}"
    );
    assert!(!tool_names(&bench.mock.requests_for("lead")[0]).contains(&"report".to_string()));

    // The refused report came back with what was wrong.
    let refused = serde_json::to_string(requests[1].messages.last().unwrap()).unwrap();
    assert!(
        refused.contains("the result does not match the schema"),
        "{refused}"
    );
    assert!(refused.contains("- /count: "), "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_typed_agent_that_never_reports_fails_after_three_reminders() {
    let mut bench = Bench::new(4, false).await;
    // `counter` owes a typed result by its definition.
    bench.lead_calls(
        "t1",
        "task",
        json!({ "agents": [{ "agent": "counter", "prompt": "count the files", "model": "mock/w" }] }),
    );
    bench.mock.push("lead", MockStep::text("it did not report"));
    for _ in 0..4 {
        bench.mock.push("w", MockStep::text("there are three files"));
    }
    let calls = bench.run("how many files?").await;
    assert!(calls[0].2.starts_with("## counter-1 (failed, "), "{}", calls[0].2);
    assert!(
        calls[0]
            .2
            .ends_with("The agent ended without a report. Its last message:\nthere are three files"),
        "{}",
        calls[0].2
    );
    assert_eq!(
        bench.state("counter-1"),
        MemberState::Failed("ended without a report".to_string())
    );
    assert_eq!(bench.team.result("counter-1").unwrap().json, None);

    let requests = bench.mock.requests_for("w");
    assert_eq!(requests.len(), 4);
    assert_eq!(bench.mock.remaining("w"), 0);
    let texts = user_texts(&requests[3].messages);
    assert_eq!(texts.len(), 4, "{texts:?}");
    for (n, reminder) in texts[1..].iter().enumerate() {
        assert!(
            reminder.starts_with("You have not handed in your result. Call `report`"),
            "{reminder}"
        );
        assert!(
            reminder.ends_with(&format!("(Reminder {} of 3.)", n + 1)),
            "{reminder}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_schema_that_cannot_be_used_starts_no_agent() {
    let mut bench = Bench::new(4, false).await;
    bench.lead_calls(
        "t1",
        "task",
        json!({ "agents": [{ "agent": "worker", "prompt": "count", "schema": { "type": "nonsense" } }] }),
    );
    bench.lead_calls(
        "t2",
        "task",
        json!({ "agents": [{ "agent": "worker", "prompt": "count", "schema": "an object" }] }),
    );
    bench.mock.push("lead", MockStep::text("no luck"));
    let calls = bench.run("count").await;
    assert!(calls[0].1 && calls[1].1);
    assert!(
        calls[0]
            .2
            .starts_with("the result schema for `worker` cannot be used: "),
        "{}",
        calls[0].2
    );
    assert_eq!(calls[1].2, "the schema for `worker` must be a JSON Schema object");
    assert_eq!(bench.team.list().len(), 1);
}

/// Starts `def` for the lead on `mock/<model>`, its result left for a wait to collect.
async fn spawn_for_lead(bench: &Bench, def: &str, prompt: &str, model: &str) -> String {
    let request = azazel_team::SpawnRequest {
        def: def.to_string(),
        prompt: prompt.to_string(),
        name: None,
        model: Some(vec![format!("mock/{model}").parse().unwrap()]),
        isolation: None,
        tools: None,
        schema: None,
    };
    bench.team.spawn(bench.lead.info(), request, false).await.unwrap()
}

/// Waits until agent `id` exists and `check` holds for its state.
async fn until_state(bench: &Bench, id: &str, check: impl Fn(&MemberState) -> bool) {
    let started = Instant::now();
    let state = || {
        bench
            .team
            .list()
            .into_iter()
            .find(|member| member.id == id)
            .map(|member| member.state)
    };
    while !state().is_some_and(|state| check(&state)) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{id} is still {:?}",
            state()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn napping(state: &MemberState) -> bool {
    matches!(state, MemberState::Running { activity } if activity == "nap")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_status_of_a_working_agent_shows_its_tool_its_words_and_its_files() {
    let bench = Bench::new(4, false).await;
    bench.mock.push(
        "e",
        MockStep::tools(
            "",
            &[("c1", "write", json!({ "path": "notes.txt", "content": "hi\n" }))],
        ),
    );
    bench.mock.push(
        "e",
        MockStep::tools(
            "Wrote the notes.\nNow resting.",
            &[("c2", "nap", json!({ "ms": 20_000 }))],
        ),
    );
    let id = spawn_for_lead(&bench, "editor", "write the notes", "e").await;
    until_state(&bench, &id, napping).await;

    let status = bench.team.status(&id).await.unwrap();
    let status = &status[0];
    assert!(status.state.is_active(), "{:?}", status.state);
    assert_eq!(status.tool.as_ref().map(|tool| tool.name.as_str()), Some("nap"));
    assert_eq!(status.text, ["Wrote the notes.", "Now resting."]);
    assert_eq!(status.files, ["notes.txt"]);
    // `all` is every agent, the lead first.
    let all = bench.team.status("all").await.unwrap();
    let ids: Vec<&str> = all.iter().map(|status| status.id.as_str()).collect();
    assert_eq!(ids, ["lead", id.as_str()]);
    assert!(bench.team.status("nobody").await.is_err());
    bench.team.stop(&id).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_agent_keeps_its_models_when_roles_change_and_a_new_one_uses_them() {
    let bench = Bench::new(4, false).await;
    let on_role = |prompt: &str| azazel_team::SpawnRequest {
        def: "worker".to_string(),
        prompt: prompt.to_string(),
        name: None,
        model: Some(vec!["@scout".parse().unwrap()]),
        isolation: None,
        tools: None,
        schema: None,
    };
    bench.mock.push(
        "scout",
        MockStep::tools("", &[("n1", "nap", json!({ "ms": 400 }))]),
    );
    bench
        .mock
        .push("scout", MockStep::text("finished on the old model"));
    let first = bench
        .team
        .spawn(bench.lead.info(), on_role("one"), false)
        .await
        .unwrap();
    until_state(&bench, &first, napping).await;

    // What a switch of profile does: pin the agents that run, then move the role.
    assert_eq!(bench.team.pin_running(), 1);
    let router = bench.team.router();
    router.set_role("scout", vec!["mock/other".parse().unwrap()]);

    bench
        .mock
        .push("other", MockStep::text("started on the new model"));
    let second = bench
        .team
        .spawn(bench.lead.info(), on_role("two"), false)
        .await
        .unwrap();
    until_state(&bench, &first, |state| *state == MemberState::Done).await;
    until_state(&bench, &second, |state| *state == MemberState::Done).await;
    // The first agent's second request still went to the model its role named when it started.
    assert_eq!(bench.mock.requests_for("scout").len(), 2);
    assert_eq!(bench.mock.requests_for("other").len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_asked_on_the_side_is_answered_while_the_agent_keeps_working() {
    let mut bench = Bench::new(4, false).await;
    bench.lead_calls(
        "t1",
        "task",
        json!({ "agents": [{ "agent": "worker", "prompt": "check A", "model": "mock/w" }], "wait": false }),
    );
    // Asked a moment later, while the worker naps.
    bench.mock.push(
        "lead",
        MockStep::tools(
            "",
            &[(
                "t2",
                "agent_send",
                json!({ "to": "worker-1", "message": "what are you on?", "ask": true }),
            )],
        )
        .delayed(Duration::from_millis(300)),
    );
    bench.mock.push("lead", MockStep::text("done"));
    bench.mock.push("lead", MockStep::text("noted"));
    bench.mock.push(
        "w",
        MockStep::tools("Looking at A.", &[("n", "nap", json!({ "ms": 1500 }))]),
    );
    // The side question is the worker's next request on its model; its own comes after the nap.
    bench.mock.push(
        "w",
        MockStep::text("I am on A.").with_usage(azazel_ai::Usage {
            input: 10,
            output: 5,
            ..Default::default()
        }),
    );
    bench.mock.push("w", MockStep::text("finished A"));
    let calls = bench.run("go").await;
    assert!(!calls[1].1, "{}", calls[1].2);
    assert_eq!(calls[1].2, "I am on A.");
    until_state(&bench, "worker-1", |state| *state == MemberState::Done).await;
    bench.until_idle().await;

    let requests = bench.mock.requests_for("w");
    assert_eq!(requests.len(), 3);
    // The question saw the worker's conversation, and the worker never saw the question.
    let asked = user_texts(&requests[1].messages);
    assert_eq!(asked.first().map(String::as_str), Some("check A"));
    assert!(asked.last().unwrap().ends_with("what are you on?"));
    assert!(
        user_texts(&requests[2].messages)
            .iter()
            .all(|text| !text.contains("what are you on?"))
    );
    let transcript = std::fs::read_to_string(bench.team.transcript_path("worker-1").unwrap()).unwrap();
    assert!(!transcript.contains("what are you on?"));
    assert!(!transcript.contains("I am on A."));
    // Counted to the lead, which asked.
    let lead = bench.team.status("lead").await.unwrap();
    let asked = lead[0].asked.unwrap();
    assert_eq!(asked.input + asked.output, 15);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_says_how_it_reached_a_queued_a_busy_and_an_idle_agent() {
    let bench = Bench::new(1, false).await;
    let lead = bench.lead.info().clone();
    bench
        .mock
        .push("w1", MockStep::tools("", &[("n", "nap", json!({ "ms": 800 }))]));
    bench.mock.push("w1", MockStep::text("did A and B"));
    bench.mock.push("w1", MockStep::text("woken"));
    bench.mock.push("w2", MockStep::text("two done"));
    let first = spawn_for_lead(&bench, "worker", "do A", "w1").await;
    until_state(&bench, &first, napping).await;
    // The only slot is taken, so the second waits for it.
    let second = spawn_for_lead(&bench, "worker", "do two", "w2").await;
    assert_eq!(
        bench.team.send(&lead, &second, "before you start").unwrap(),
        [(second.clone(), Delivery::Queued)]
    );
    assert_eq!(
        bench.team.send(&lead, &first, "also B").unwrap(),
        [(first.clone(), Delivery::NextStep)]
    );
    until_state(&bench, &first, |state| *state == MemberState::Done).await;
    assert_eq!(
        bench.team.send(&lead, &first, "once more").unwrap(),
        [(first.clone(), Delivery::Woke)]
    );
    // Woken with a turn of its own, which it answers.
    let started = Instant::now();
    while bench.team.result(&first).map(|done| done.report) != Some("woken".to_string()) {
        assert!(started.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    until_state(&bench, &second, |state| *state == MemberState::Done).await;
    let seconds = bench.mock.requests_for("w2");
    assert!(
        user_texts(&seconds[0].messages)
            .iter()
            .any(|text| text.contains("before you start")),
        "{:?}",
        user_texts(&seconds[0].messages)
    );
    let firsts = bench.mock.requests_for("w1");
    assert!(user_texts(&firsts[1].messages).last().unwrap().contains("also B"));
}

#[tokio::test(flavor = "multi_thread")]
async fn what_the_user_types_ends_a_wait_and_the_agent_reports_later() {
    let mut bench = Bench::new(4, false).await;
    bench.lead_calls(
        "t1",
        "task",
        json!({ "agents": [{ "agent": "worker", "prompt": "slow work", "model": "mock/w" }] }),
    );
    bench.mock.push("lead", MockStep::text("I am here"));
    bench.mock.push("lead", MockStep::text("noted"));
    bench
        .mock
        .push("w", MockStep::tools("", &[("n", "nap", json!({ "ms": 2000 }))]));
    bench.mock.push("w", MockStep::text("slow report"));
    assert!(bench.lead.prompt(UserInput::text("go")));
    until_state(&bench, "worker-1", napping).await;
    assert!(bench.lead.steer(UserInput::text("are you there?")));

    let calls = bench.until_idle().await;
    assert!(!calls[0].1, "{}", calls[0].2);
    assert!(calls[0].2.contains("worker-1"), "{}", calls[0].2);
    assert!(
        calls[0].3 < Duration::from_millis(1500),
        "waited {:?}",
        calls[0].3
    );
    assert!(bench.state("worker-1").is_active());
    let answered = bench.mock.requests_for("lead");
    assert_eq!(
        user_texts(&answered[1].messages).last().map(String::as_str),
        Some("are you there?")
    );

    // The worker went on and its report reaches the lead as a message.
    bench.until_idle().await;
    let last = bench.mock.requests_for("lead").pop().unwrap();
    let delivered = user_texts(&last.messages).pop().unwrap();
    assert!(
        delivered.starts_with("<agent-result id=\"worker-1\" status=\"done\""),
        "{delivered}"
    );
    assert!(delivered.contains("slow report"));
}

/// Routing settings whose fast model is a model of its own, so the rater (`@smol`) and the agent
/// it moves never share a queue.
fn routing(mode: ModelRouting, allow_deep: bool) -> RoutingSettings {
    RoutingSettings {
        mode,
        fast: "mock/fast".parse().unwrap(),
        allow_deep,
        ..RoutingSettings::default()
    }
}

/// Starts one `worker` (or with `model`, on that model) and lets it finish; `rating` is what the
/// rater answers, when it is asked. Returns the bench to look at what went where.
async fn routed_worker(settings: RoutingSettings, rating: Option<MockStep>, model: Option<&str>) -> Bench {
    let mut bench = Bench::routed(settings).await;
    let mut agent = json!({ "agent": "worker", "prompt": "rename `a` to `b` in f.txt" });
    if let Some(model) = model {
        agent["model"] = json!(model);
    }
    bench.lead_calls("t1", "task", json!({ "agents": [agent] }));
    bench.mock.push("lead", MockStep::text("ok"));
    if let Some(rating) = rating {
        bench.mock.push("smol", rating);
    }
    for queue in ["fast", "mid", "big", "w"] {
        bench
            .mock
            .push(queue, MockStep::text(format!("report from {queue}")));
    }
    let calls = bench.run("go").await;
    assert!(!calls[0].1, "{}", calls[0].2);
    bench
}

/// The model queues the worker was asked on.
fn asked(bench: &Bench) -> Vec<&'static str> {
    ["fast", "mid", "big", "w"]
        .into_iter()
        .filter(|queue| !bench.mock.requests_for(queue).is_empty())
        .collect()
}

fn rating(tier: &str, confidence: f64) -> MockStep {
    MockStep::text(format!(
        "{{\"tier\": \"{tier}\", \"confidence\": {confidence}, \"risky\": 0}}"
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_routing_moves_down_at_the_downgrade_confidence() {
    let bench = routed_worker(
        routing(ModelRouting::Auto, false),
        Some(rating("fast", 0.6)),
        None,
    )
    .await;
    assert_eq!(asked(&bench), ["fast"]);
    // The rating is a one-off: the task as data, no tools, no system prompt to cache.
    let rated = bench.mock.requests_for("smol");
    assert_eq!(rated.len(), 1);
    assert_eq!(rated[0].messages.len(), 1);
    assert!(rated[0].tools.is_empty() && rated[0].system.is_empty());
    assert!(
        rated[0].messages[0]
            .text()
            .contains("<task>\nrename `a` to `b` in f.txt\n</task>")
    );
    let (id, routed) = bench.team.routing().pop().unwrap();
    assert_eq!(id, "worker-1");
    assert!(routed.applied);
    assert_eq!(routed.to.unwrap().to_string(), "mock/fast");
}

#[tokio::test(flavor = "multi_thread")]
async fn below_the_downgrade_confidence_the_model_stays() {
    let bench = routed_worker(
        routing(ModelRouting::Auto, false),
        Some(rating("fast", 0.59)),
        None,
    )
    .await;
    assert_eq!(asked(&bench), ["mid"]);
    let (_, routed) = bench.team.routing().pop().unwrap();
    assert!(routed.to.is_none() && !routed.applied);
}

#[tokio::test(flavor = "multi_thread")]
async fn moving_up_needs_the_upgrade_confidence_and_allow_deep() {
    let up = routed_worker(
        routing(ModelRouting::Auto, true),
        Some(rating("deep", 0.85)),
        None,
    )
    .await;
    assert_eq!(asked(&up), ["big"]);
    let short = routed_worker(
        routing(ModelRouting::Auto, true),
        Some(rating("deep", 0.84)),
        None,
    )
    .await;
    assert_eq!(asked(&short), ["mid"]);
    let capped = routed_worker(
        routing(ModelRouting::Auto, false),
        Some(rating("deep", 0.99)),
        None,
    )
    .await;
    assert_eq!(asked(&capped), ["mid"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn suggest_records_the_move_without_making_it() {
    let bench = routed_worker(
        routing(ModelRouting::Suggest, false),
        Some(rating("fast", 0.9)),
        None,
    )
    .await;
    assert_eq!(asked(&bench), ["mid"]);
    let status = bench.team.status("worker-1").await.unwrap().pop().unwrap();
    let line = azazel_team::status_text(&status);
    assert!(line.contains("would move to mock/fast"), "{line}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_model_is_not_rated() {
    let bench = routed_worker(routing(ModelRouting::Auto, true), None, Some("mock/w")).await;
    assert_eq!(asked(&bench), ["w"]);
    assert!(bench.mock.requests_for("smol").is_empty());
    assert!(bench.team.routing().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rating_that_comes_too_late_changes_nothing() {
    let settings = RoutingSettings {
        timeout: Duration::from_millis(200),
        ..routing(ModelRouting::Auto, false)
    };
    let late = rating("fast", 0.99).delayed(Duration::from_secs(5));
    let started = Instant::now();
    let bench = routed_worker(settings, Some(late), None).await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(asked(&bench), ["mid"]);
    let (_, routed) = bench.team.routing().pop().unwrap();
    assert!(routed.rating.is_none() && routed.to.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn with_routing_off_no_rating_is_asked_for() {
    let bench = routed_worker(routing(ModelRouting::Off, true), None, None).await;
    assert_eq!(asked(&bench), ["mid"]);
    assert!(bench.mock.requests_for("smol").is_empty());
    assert!(bench.team.routing().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_that_runs_twice_is_counted_once_in_the_savings() {
    let used = || azazel_ai::Usage {
        input: 1_000_000,
        ..azazel_ai::Usage::default()
    };
    let mut bench = Bench::routed(routing(ModelRouting::Auto, false)).await;
    bench.lead_calls(
        "t1",
        "task",
        json!({ "agents": [{ "agent": "worker", "prompt": "rename `a` to `b` in f.txt" }] }),
    );
    bench.mock.push("lead", MockStep::text("ok"));
    bench.mock.push("smol", rating("fast", 0.9));
    bench
        .mock
        .push("fast", MockStep::text("first report").with_usage(used()));
    bench
        .mock
        .push("fast", MockStep::text("second report").with_usage(used()));
    let calls = bench.run("go").await;
    assert!(!calls[0].1, "{}", calls[0].2);

    let lead = bench.lead.info().clone();
    bench.team.send(&lead, "worker-1", "once more").unwrap();
    let started = Instant::now();
    while bench.team.result("worker-1").map(|done| done.report) != Some("second report".to_string()) {
        assert!(started.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The end of the second run is recorded a moment after its report is there.
    let entries = || -> Vec<f64> {
        bench
            .session
            .entries()
            .into_iter()
            .filter_map(|entry| match entry.kind {
                azazel_core::EntryKind::Custom { custom_type, data }
                    if custom_type == "routing" && data["phase"] == "saving" =>
                {
                    data["saving_usd"].as_f64()
                }
                _ => None,
            })
            .collect()
    };
    let started = Instant::now();
    while entries().len() < 2 {
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", entries());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // 1M input tokens a run, 3 against 1 per million: each run saved 2, and the two entries add
    // up to what /cost shows for the agent.
    let total: f64 = entries().iter().sum();
    assert!((total - 4.0).abs() < 1e-9, "{:?}", entries());
    let (_, routed) = bench.team.routing().pop().unwrap();
    assert!((routed.saving_usd.unwrap() - 4.0).abs() < 1e-9, "{routed:?}");
}
