#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_agent::{AgentInfo, Services, ToolOutput};
use azazel_ai::testing::{MockProvider, MockStep};
use azazel_ai::{Block, Catalog, ModelSpec, Provider, RetryPolicy, Router};
use azazel_core::config::{HookConfig, HookKind};
use azazel_core::hooks::HooksExt;
use azazel_core::process::Shell;
use azazel_core::{
    ApprovalDecision, ApprovalRequest, Config, Event, EventBus, ExtCtx, Level, LoadOptions, Outcome, Paths,
    PermissionState, Registry, SessionStore, Skill, SkillRoot, SkillSource, StartSource, SubagentRunner,
    Subagents, Ui, UiBridge, UiError,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SESSION: &str = "session-1";

/// A skill whose hooks stop the lead from finishing and from using the shell.
const GUARD_SKILL: [&str; 17] = [
    "---",
    "name: guard",
    "description: Keeps the agent honest in tests.",
    "hooks:",
    "  Stop:",
    "    - hooks:",
    "        - type: command",
    "          command: |",
    "            printf '%s' \"$CLAUDE_SKILL_DIR\" > skill_dir.txt; echo 'the skill is not finished' >&2; exit 2",
    "  PreToolUse:",
    "    - matcher: Bash",
    "      hooks:",
    "        - type: command",
    "          command: |",
    "            cat > skill_pre.json; echo 'no shell while guarded' >&2; exit 2",
    "---",
    "Guard the work.",
];

fn hook(kind: HookKind) -> HookConfig {
    HookConfig {
        matcher: None,
        when: None,
        kind,
        command: None,
        url: None,
        headers: BTreeMap::new(),
        prompt: None,
        model: None,
        timeout: 60,
        detached: false,
        once: false,
        wake: false,
    }
}

fn command(script: &str) -> HookConfig {
    HookConfig {
        command: Some(script.to_string()),
        ..hook(HookKind::Command)
    }
}

fn matching(matcher: &str, mut hook: HookConfig) -> HookConfig {
    hook.matcher = Some(matcher.to_string());
    hook
}

fn lead() -> AgentInfo {
    AgentInfo {
        id: "lead".into(),
        def: "lead".into(),
        parent: None,
        depth: 0,
        session_id: SESSION.into(),
    }
}

fn tool_call(tool: &str, input: Value) -> Event {
    Event::ToolCall {
        agent: lead(),
        call_id: "call-1".into(),
        tool: tool.into(),
        input,
        skip_prompt: false,
    }
}

fn stop(agent: AgentInfo, stop_hook_active: bool) -> Event {
    Event::Stop {
        agent,
        stop_hook_active,
    }
}

fn blocked(reason: &str) -> Outcome {
    Outcome::Block {
        reason: reason.to_string(),
    }
}

/// A project directory in which hooks run; they leave their evidence in files there.
struct Bench {
    _dir: tempfile::TempDir,
    paths: Paths,
    services: Arc<Services>,
}

impl Bench {
    fn new() -> Bench {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".azazel")).unwrap();
        let paths = Paths::discover(&project, dir.path().join("home").to_str());
        Bench {
            _dir: dir,
            paths,
            services: Arc::new(Services::new()),
        }
    }

    fn ext(&self, hooks: Vec<(&str, HookConfig)>) -> HooksExt {
        let mut by_event: BTreeMap<String, Vec<HookConfig>> = BTreeMap::new();
        for (event, hook) in hooks {
            by_event.entry(event.to_string()).or_default().push(hook);
        }
        HooksExt {
            hooks: by_event,
            shell: Shell::resolve(""),
            paths: self.paths.clone(),
            claude_compat: true,
            disabled: false,
            trusted: true,
            sources: BTreeMap::new(),
        }
    }

    fn registry(&self, ext: &HooksExt) -> Registry {
        let registry = Registry::new(ExtCtx {
            services: self.services.clone(),
            cwd: self.paths.cwd.clone(),
            session_id: SESSION.into(),
        });
        registry.load(ext).unwrap();
        registry
    }

    fn load(&self, hooks: Vec<(&str, HookConfig)>) -> Registry {
        self.registry(&self.ext(hooks))
    }

    /// What a hook wrote to `name` in its working directory; `None` when it never did.
    fn written(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.paths.cwd.join(name)).ok()
    }

    fn json(&self, name: &str) -> Value {
        serde_json::from_str(&self.written(name).unwrap()).unwrap()
    }

    /// Puts the guard skill into the project and returns it as the registry sees it.
    fn guard_skill(&self, registry: &Registry) -> Skill {
        let root = self.paths.project_dir().join("skills");
        std::fs::create_dir_all(root.join("guard")).unwrap();
        std::fs::write(root.join("guard").join("SKILL.md"), GUARD_SKILL.join("\n")).unwrap();
        registry
            .skills
            .add_root(SkillRoot::new(root, SkillSource::Project));
        registry.skills.get("guard").unwrap()
    }
}

/// A user interface that only remembers what it was told.
#[derive(Default)]
struct Notes(Mutex<Vec<(Level, String)>>);

#[async_trait]
impl UiBridge for Notes {
    async fn ask(&self, _: &str, _: &[String], _: bool) -> Result<Vec<String>, UiError> {
        Err(UiError::NoUser)
    }
    async fn confirm(&self, _: &str) -> Result<bool, UiError> {
        Err(UiError::NoUser)
    }
    async fn select(&self, _: &str, _: &[String]) -> Result<usize, UiError> {
        Err(UiError::NoUser)
    }
    async fn input(&self, _: &str) -> Result<String, UiError> {
        Err(UiError::NoUser)
    }
    fn notify(&self, level: Level, text: &str) {
        self.0.lock().unwrap().push((level, text.to_string()));
    }
    async fn approval(&self, _: ApprovalRequest) -> Option<ApprovalDecision> {
        None
    }
}

#[tokio::test]
async fn a_command_exiting_2_blocks_the_call_with_its_stderr_and_the_hooks_after_it_do_not_run() {
    let bench = Bench::new();
    let registry = bench.load(vec![
        ("pre_tool_use", command("echo first >> order.txt")),
        ("pre_tool_use", command("echo 'not on my watch' >&2; exit 2")),
        ("pre_tool_use", command("echo third >> order.txt")),
    ]);
    let mut event = tool_call("shell", json!({"command": "rm -rf build"}));
    assert_eq!(registry.bus.emit(&mut event).await, blocked("not on my watch"));
    assert_eq!(bench.written("order.txt").unwrap().trim(), "first");
}

#[tokio::test]
async fn a_failing_or_slow_hook_warns_the_user_and_lets_the_call_through() {
    let bench = Bench::new();
    let notes = Arc::new(Notes::default());
    bench.services.insert(Arc::new(Ui(notes.clone())));
    let mut slow = matching("read", command("sleep 30"));
    slow.timeout = 1;
    let registry = bench.load(vec![
        (
            "pre_tool_use",
            matching("shell", command("echo 'policy script is broken' >&2; exit 1")),
        ),
        ("pre_tool_use", slow),
    ]);

    let mut event = tool_call("shell", json!({"command": "ls"}));
    assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);
    let started = Instant::now();
    let mut event = tool_call("read", json!({"path": "a.txt"}));
    assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );

    let notes = notes.0.lock().unwrap();
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(notes.iter().all(|(level, _)| *level == Level::Warn));
    assert!(notes[0].1.contains("policy script is broken"), "{notes:?}");
    assert!(notes[1].1.contains("timed out"), "{notes:?}");
}

#[tokio::test]
async fn json_output_rewrites_the_input_for_later_hooks_and_waives_the_approval_prompt() {
    let bench = Bench::new();
    let rewrite = r#"printf '%s' '{"hookSpecificOutput":{"permissionDecision":"allow","updatedInput":{"command":"echo safe"}}}'"#;
    let registry = bench.load(vec![
        ("pre_tool_use", command(rewrite)),
        ("pre_tool_use", command("cat > second.json")),
    ]);
    let mut event = tool_call("shell", json!({"command": "echo risky"}));
    assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);
    let Event::ToolCall {
        input, skip_prompt, ..
    } = event
    else {
        panic!("the event changed kind")
    };
    assert_eq!(input, json!({"command": "echo safe"}));
    assert!(skip_prompt);
    assert_eq!(bench.json("second.json")["tool_input"]["command"], "echo safe");
}

#[tokio::test]
async fn matcher_and_if_decide_which_calls_a_hook_sees() {
    let bench = Bench::new();
    let mut commits = matching("shell", command("echo 'no commits' >&2; exit 2"));
    commits.when = Some("shell(git commit:*)".to_string());
    let mut sources = matching("write|edit", command("echo 'src is frozen' >&2; exit 2"));
    sources.when = Some("edit(src/**)".to_string());
    let registry = bench.load(vec![("pre_tool_use", commits), ("pre_tool_use", sources)]);

    let cases = [
        ("read", json!({"path": "src/a.rs"}), None),
        ("shell", json!({"command": "git status"}), None),
        (
            "shell",
            json!({"command": "git commit -m wip"}),
            Some("no commits"),
        ),
        (
            "shell",
            json!({"command": "cargo fmt && git commit -m wip"}),
            Some("no commits"),
        ),
        ("shell_extra", json!({"command": "git commit -m wip"}), None),
        ("write", json!({"path": "src/a.rs", "content": ""}), None),
        ("edit", json!({"path": "docs/a.md", "edits": []}), None),
        (
            "edit",
            json!({"path": "src/a.rs", "edits": []}),
            Some("src is frozen"),
        ),
    ];
    for (tool, input, reason) in cases {
        let mut event = tool_call(tool, input.clone());
        let expected = reason.map_or(Outcome::Continue, blocked);
        assert_eq!(registry.bus.emit(&mut event).await, expected, "{tool} {input}");
    }
}

#[tokio::test]
async fn a_stop_hook_keeps_the_lead_going_once_and_sub_agents_have_their_own_event() {
    let bench = Bench::new();
    let once = "input=$(cat); printf '%s\\n' \"$input\" >> stop.log; \
                case \"$input\" in *'\"stop_hook_active\":true'*) exit 0;; esac; \
                echo 'run the tests first' >&2; exit 2";
    let registry = bench.load(vec![
        ("stop", command(once)),
        ("subagent_stop", command("cat > sub.json")),
    ]);

    assert_eq!(
        registry.bus.emit(&mut stop(lead(), false)).await,
        blocked("run the tests first")
    );
    assert_eq!(
        registry.bus.emit(&mut stop(lead(), true)).await,
        Outcome::Continue
    );
    let log = bench.written("stop.log").unwrap();
    let seen: Vec<&str> = log.lines().collect();
    assert_eq!(seen.len(), 2, "{log}");
    assert!(seen[0].contains("\"hook_event_name\":\"Stop\""), "{log}");
    assert!(seen[0].contains("\"stop_hook_active\":false"), "{log}");
    assert!(seen[1].contains("\"stop_hook_active\":true"), "{log}");

    let scout = AgentInfo {
        id: "scout-1".into(),
        def: "scout".into(),
        parent: Some("lead".into()),
        depth: 1,
        session_id: SESSION.into(),
    };
    assert_eq!(
        registry.bus.emit(&mut stop(scout, false)).await,
        Outcome::Continue
    );
    assert_eq!(bench.written("stop.log").unwrap().lines().count(), 2);
    let sub = bench.json("sub.json");
    assert_eq!(sub["hook_event_name"], "SubagentStop");
    assert_eq!(sub["agent_id"], "scout-1");
    assert_eq!(sub["stop_hook_active"], false);
    let transcript = sub["agent_transcript_path"].as_str().unwrap().replace('\\', "/");
    assert!(
        transcript.ends_with(&format!("sessions/{SESSION}/agents/scout-1.jsonl")),
        "{transcript}"
    );
}

#[tokio::test]
async fn hook_output_becomes_prompt_context_and_session_start_output_is_added_once() {
    let bench = Bench::new();
    let registry = bench.load(vec![
        (
            "session_start",
            command("cat > start.json; echo 'the build is red'"),
        ),
        ("user_prompt_submit", command("echo 'branch: main'")),
    ]);
    let outcome = registry
        .bus
        .emit(&mut Event::SessionStart {
            source: StartSource::Startup,
        })
        .await;
    assert_eq!(outcome, Outcome::Continue);
    let start = bench.json("start.json");
    assert_eq!(start["hook_event_name"], "SessionStart");
    assert_eq!(start["source"], "startup");

    let mut prompts = Vec::new();
    for text in ["hi", "again"] {
        let mut event = Event::UserPrompt {
            text: text.to_string(),
            images: Vec::new(),
        };
        assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);
        let Event::UserPrompt { text, .. } = event else {
            panic!("the event changed kind")
        };
        prompts.push(text);
    }
    assert_eq!(
        prompts,
        [
            "hi\n\n<hook-context>\nthe build is red\nbranch: main\n</hook-context>",
            "again\n\n<hook-context>\nbranch: main\n</hook-context>",
        ]
    );
}

#[tokio::test]
async fn a_prompt_hook_can_reject_the_prompt_with_a_reason() {
    let bench = Bench::new();
    let screen = r#"case "$(cat)" in *password*) printf '%s' '{"decision":"block","reason":"no secrets in prompts"}';; esac"#;
    let registry = bench.load(vec![("user_prompt_submit", command(screen))]);

    let mut event = Event::UserPrompt {
        text: "my password is hunter2".into(),
        images: Vec::new(),
    };
    assert_eq!(
        registry.bus.emit(&mut event).await,
        blocked("no secrets in prompts")
    );

    let mut event = Event::UserPrompt {
        text: "hello".into(),
        images: Vec::new(),
    };
    assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);
    let Event::UserPrompt { text, .. } = event else {
        panic!("the event changed kind")
    };
    assert_eq!(text, "hello");
}

#[tokio::test]
async fn a_post_tool_use_hook_appends_its_reason_to_a_successful_result_only() {
    let bench = Bench::new();
    let registry = bench.load(vec![(
        "post_tool_use",
        command("cat > post.json; echo 'run the formatter' >&2; exit 2"),
    )]);
    let mut call = tool_call("write", json!({"path": "a.rs", "content": "fn main(){}"}));
    assert_eq!(registry.bus.emit(&mut call).await, Outcome::Continue);

    let result = |output: ToolOutput| Event::ToolResult {
        agent: lead(),
        call_id: "call-1".into(),
        tool: "write".into(),
        output,
    };
    let mut event = result(ToolOutput::text("wrote a.rs"));
    assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);
    let Event::ToolResult { output, .. } = event else {
        panic!("the event changed kind")
    };
    assert_eq!(
        output.content,
        [Block::text("wrote a.rs"), Block::text("run the formatter")]
    );
    assert!(!output.is_error);
    let stdin = bench.json("post.json");
    assert_eq!(stdin["hook_event_name"], "PostToolUse");
    assert_eq!(stdin["tool_use_id"], "call-1");
    assert_eq!(stdin["tool_input"]["path"], "a.rs");
    assert_eq!(stdin["tool_response"]["content"], "wrote a.rs");

    let mut event = result(ToolOutput::error("disk full"));
    assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);
    let Event::ToolResult { output, .. } = event else {
        panic!("the event changed kind")
    };
    assert_eq!(output, ToolOutput::error("disk full"));
}

#[tokio::test]
async fn a_permission_hook_settles_the_request_from_its_output() {
    let bench = Bench::new();
    let deny = r#"printf '%s' '{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"policy says no"}}'"#;
    let allow = r#"printf '%s' '{"hookSpecificOutput":{"permissionDecision":"allow"}}'"#;
    let registry = bench.load(vec![
        ("permission_request", matching("shell", command(deny))),
        ("permission_request", matching("read", command(allow))),
    ]);

    let cases = [
        ("shell", blocked("policy says no"), Some(false)),
        ("read", Outcome::Continue, Some(true)),
        ("glob", Outcome::Continue, None),
    ];
    for (tool, outcome, settled) in cases {
        let mut event = Event::PermissionRequest {
            agent: lead(),
            tool: tool.into(),
            input: json!({}),
            subjects: Vec::new(),
            decision: None,
        };
        assert_eq!(registry.bus.emit(&mut event).await, outcome, "{tool}");
        let Event::PermissionRequest { decision, .. } = event else {
            panic!("the event changed kind")
        };
        assert_eq!(decision, settled, "{tool}");
    }
}

#[tokio::test]
async fn a_command_hook_receives_the_event_on_stdin_and_the_session_in_its_environment() {
    let bench = Bench::new();
    let session =
        Arc::new(SessionStore::create(&bench.paths.sessions_dir(), &bench.paths.cwd, None).unwrap());
    bench.services.insert(session.clone());
    let options = LoadOptions {
        trusted: true,
        ..LoadOptions::default()
    };
    let loaded = Config::load(&bench.paths, &options).unwrap();
    bench
        .services
        .insert(Arc::new(PermissionState::new(&bench.paths, &loaded).unwrap()));
    let record = "cat > stdin.json; printf '%s\\n' \"$AZAZEL_PROJECT_DIR\" \"$AZAZEL_SESSION_ID\" \
                  \"$AZAZEL_HOOK_EVENT\" \"$AZAZEL_HOOKS_DISABLED\" \"$CLAUDE_PROJECT_DIR\" > env.txt";
    let env = |bench: &Bench| -> Vec<String> {
        let text = bench.written("env.txt").unwrap();
        text.lines().map(|line| line.replace('\\', "/")).collect()
    };

    let registry = bench.load(vec![("pre_tool_use", command(record))]);
    let mut event = tool_call("shell", json!({"command": "ls"}));
    assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);

    let stdin = bench.json("stdin.json");
    assert_eq!(stdin["hook_event_name"], "PreToolUse");
    assert_eq!(stdin["session_id"], SESSION);
    assert_eq!(stdin["transcript_path"], session.path().display().to_string());
    assert_eq!(stdin["cwd"], bench.paths.cwd.display().to_string());
    assert_eq!(stdin["agent_id"], "lead");
    assert_eq!(stdin["permission_mode"], "ask");
    assert_eq!(stdin["tool_name"], "shell");
    assert_eq!(stdin["tool_input"], json!({"command": "ls"}));
    assert_eq!(stdin["tool_use_id"], "call-1");

    let lines = env(&bench);
    assert!(lines[0].ends_with("/project"), "{lines:?}");
    assert_eq!(lines[1..4], [SESSION, "pre_tool_use", "1"]);
    assert_eq!(lines[4], lines[0]);

    let mut ext = bench.ext(vec![("pre_tool_use", command(record))]);
    ext.claude_compat = false;
    let registry = bench.registry(&ext);
    let mut event = tool_call("shell", json!({"command": "ls"}));
    assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);
    let lines = env(&bench);
    assert!(lines[0].ends_with("/project"), "{lines:?}");
    assert_eq!(lines[4], "");
}

#[tokio::test]
async fn disabled_hooks_register_nothing() {
    let bench = Bench::new();
    let mut ext = bench.ext(vec![("pre_tool_use", command("echo 'never' >&2; exit 2"))]);
    ext.disabled = true;
    let registry = bench.registry(&ext);
    let mut event = tool_call("shell", json!({"command": "ls"}));
    assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);
}

#[tokio::test]
async fn an_http_hook_blocks_with_the_servers_answer_and_a_failing_server_does_not() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/gate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"decision": "block", "reason": "no"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/down"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let http = |route: &str| HookConfig {
        url: Some(format!("{}{route}", server.uri())),
        headers: BTreeMap::from([("x-team".to_string(), "blue".to_string())]),
        ..hook(HookKind::Http)
    };
    let bench = Bench::new();
    let registry = bench.load(vec![
        ("pre_tool_use", matching("shell", http("/gate"))),
        ("pre_tool_use", matching("read", http("/down"))),
    ]);

    let mut event = tool_call("shell", json!({"command": "ls"}));
    assert_eq!(registry.bus.emit(&mut event).await, blocked("no"));
    let mut event = tool_call("read", json!({"path": "a.txt"}));
    assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);

    let requests = server.received_requests().await.unwrap();
    let gate = requests
        .iter()
        .find(|request| request.url.path() == "/gate")
        .unwrap();
    assert_eq!(gate.headers.get("x-team").unwrap().to_str().unwrap(), "blue");
    let body: Value = gate.body_json().unwrap();
    assert_eq!(body["hook_event_name"], "PreToolUse");
    assert_eq!(body["tool_name"], "shell");
}

#[tokio::test]
async fn an_async_hook_is_not_waited_for_and_its_result_is_ignored() {
    let bench = Bench::new();
    let mut slow = command("sleep 5; echo 'too late' >&2; exit 2");
    slow.detached = true;
    let registry = bench.load(vec![("pre_tool_use", slow)]);
    let started = Instant::now();
    let mut event = tool_call("shell", json!({"command": "ls"}));
    assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_prompt_hook_asks_the_small_model_and_blocks_on_its_verdict() {
    let bench = Bench::new();
    let mock = Arc::new(MockProvider::new());
    mock.push(
        "judge",
        MockStep::text("```json\n{\"ok\": false, \"reason\": \"too risky\"}\n```"),
    );
    mock.push("judge", MockStep::text("Looks fine to me."));
    let providers: Vec<Arc<dyn Provider>> = vec![mock.clone()];
    let roles = BTreeMap::from([(
        "smol".to_string(),
        vec!["mock/judge".parse::<ModelSpec>().unwrap()],
    )]);
    let policy = RetryPolicy {
        max_attempts: 1,
        base_delay: Duration::from_millis(1),
        ..RetryPolicy::default()
    };
    bench.services.insert(Arc::new(Router::new(
        providers,
        Catalog::default(),
        roles,
        policy,
    )));
    let registry = bench.load(vec![(
        "pre_tool_use",
        HookConfig {
            prompt: Some("Refuse anything that deletes files.".to_string()),
            ..hook(HookKind::Prompt)
        },
    )]);

    let mut event = tool_call("shell", json!({"command": "rm -rf build"}));
    assert_eq!(registry.bus.emit(&mut event).await, blocked("too risky"));
    // An answer without a verdict does not block.
    let mut event = tool_call("shell", json!({"command": "rm -rf build"}));
    assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);

    let requests = mock.requests_for("judge");
    assert_eq!(requests.len(), 2);
    let asked = requests[0].messages[0].text();
    assert!(asked.contains("Refuse anything that deletes files."), "{asked}");
    assert!(asked.contains("\"tool_name\":\"shell\""), "{asked}");
    assert!(asked.contains("rm -rf build"), "{asked}");
}

/// Stands in for the team: records what it is asked to run, and lets the "verifier" make a tool
/// call of its own before reporting.
struct Verifier {
    bus: Mutex<Option<Arc<EventBus>>>,
    runs: Mutex<Vec<(String, String)>>,
    own_call: Mutex<Option<Outcome>>,
}

#[async_trait]
impl SubagentRunner for Verifier {
    async fn run(&self, def: &str, prompt: String, parent: &AgentInfo) -> Result<String, String> {
        self.runs.lock().unwrap().push((def.to_string(), prompt));
        let bus = self.bus.lock().unwrap().clone().unwrap();
        let mut call = Event::ToolCall {
            agent: AgentInfo {
                id: "verifier-1".into(),
                def: def.to_string(),
                parent: Some(parent.id.clone()),
                depth: parent.depth + 1,
                session_id: parent.session_id.clone(),
            },
            call_id: "call-v".into(),
            tool: "read".into(),
            input: json!({"path": "Cargo.toml"}),
            skip_prompt: false,
        };
        let outcome = bus.emit(&mut call).await;
        *self.own_call.lock().unwrap() = Some(outcome);
        Ok(
            "I looked at the change.\n```json\n{\"ok\": false, \"reason\": \"two tests fail\"}\n```"
                .to_string(),
        )
    }
}

#[tokio::test]
async fn an_agent_hook_runs_a_verifier_whose_own_calls_do_not_start_it_again() {
    let bench = Bench::new();
    let registry = bench.load(vec![(
        "pre_tool_use",
        HookConfig {
            prompt: Some("Check that the tests pass.".to_string()),
            ..hook(HookKind::Agent)
        },
    )]);

    // Nobody to run agents: the hook cannot answer, so the call goes through.
    let mut event = tool_call("shell", json!({"command": "git commit -m done"}));
    assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);

    let verifier = Arc::new(Verifier {
        bus: Mutex::new(Some(registry.bus.clone())),
        runs: Mutex::default(),
        own_call: Mutex::default(),
    });
    bench.services.insert(Arc::new(Subagents(verifier.clone())));
    let mut event = tool_call("shell", json!({"command": "git commit -m done"}));
    assert_eq!(registry.bus.emit(&mut event).await, blocked("two tests fail"));

    let runs = verifier.runs.lock().unwrap();
    assert_eq!(
        runs.len(),
        1,
        "the verifier's own tool call started another verifier"
    );
    assert_eq!(runs[0].0, "verifier");
    assert!(runs[0].1.contains("Check that the tests pass."), "{}", runs[0].1);
    assert!(
        runs[0].1.contains("\"hook_event_name\":\"PreToolUse\""),
        "{}",
        runs[0].1
    );
    assert_eq!(*verifier.own_call.lock().unwrap(), Some(Outcome::Continue));
}

#[tokio::test]
async fn skill_hooks_run_from_the_skills_first_use_until_the_session_ends() {
    let bench = Bench::new();
    let registry = bench.load(Vec::new());
    let skill = bench.guard_skill(&registry);

    assert_eq!(
        registry.bus.emit(&mut stop(lead(), false)).await,
        Outcome::Continue
    );
    assert_eq!(bench.written("skill_dir.txt"), None);

    registry.skills.mark_used(&skill);
    assert_eq!(
        registry.bus.emit(&mut stop(lead(), false)).await,
        blocked("the skill is not finished")
    );
    let dir = bench.written("skill_dir.txt").unwrap().replace('\\', "/");
    assert!(dir.ends_with(".azazel/skills/guard"), "{dir}");

    // The skill's matcher names the shell tool by its alias, and the hook is told that name.
    let mut event = tool_call("shell", json!({"command": "ls"}));
    assert_eq!(
        registry.bus.emit(&mut event).await,
        blocked("no shell while guarded")
    );
    assert_eq!(bench.json("skill_pre.json")["tool_name"], "Bash");
    let mut event = tool_call("read", json!({"path": "a.txt"}));
    assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);

    assert_eq!(registry.bus.emit(&mut Event::SessionEnd).await, Outcome::Continue);
    assert_eq!(
        registry.bus.emit(&mut stop(lead(), false)).await,
        Outcome::Continue
    );
}

#[tokio::test]
async fn hooks_of_a_project_skill_do_not_run_in_an_untrusted_project() {
    let bench = Bench::new();
    let mut ext = bench.ext(Vec::new());
    ext.trusted = false;
    let registry = bench.registry(&ext);
    let skill = bench.guard_skill(&registry);
    registry.skills.mark_used(&skill);

    assert_eq!(
        registry.bus.emit(&mut stop(lead(), false)).await,
        Outcome::Continue
    );
    assert_eq!(bench.written("skill_dir.txt"), None);
}

#[tokio::test]
async fn a_once_hook_runs_one_time_per_session() {
    let bench = Bench::new();
    let mut once = command("echo ran >> once.txt");
    once.once = true;
    let registry = bench.load(vec![
        ("pre_tool_use", once),
        ("pre_tool_use", command("echo ran >> every.txt")),
    ]);
    for _ in 0..3 {
        let mut event = tool_call("read", json!({"path": "a.txt"}));
        assert_eq!(registry.bus.emit(&mut event).await, Outcome::Continue);
    }
    assert_eq!(bench.written("once.txt").unwrap().lines().count(), 1);
    assert_eq!(bench.written("every.txt").unwrap().lines().count(), 3);

    // Another session is another first time.
    registry.bus.set_session("session-2");
    for _ in 0..2 {
        let mut event = tool_call("read", json!({"path": "a.txt"}));
        registry.bus.emit(&mut event).await;
    }
    assert_eq!(bench.written("once.txt").unwrap().lines().count(), 2);
}

#[tokio::test]
async fn a_wake_hook_hands_its_output_to_the_lead_when_it_finishes() {
    let bench = Bench::new();
    let delivered: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = delivered.clone();
    bench
        .services
        .insert(Arc::new(azazel_core::hooks::Wake(Arc::new(move |text| {
            sink.lock().unwrap().push(text);
        }))));
    let mut lint = matching("edit", command("sleep 0.3; echo 'lint: 2 warnings in a.rs'"));
    lint.detached = true;
    lint.wake = true;
    // Detached without `wake`: its output goes nowhere. `wake` without `async`: not a valid hook.
    let mut silent = command("echo 'nobody hears this'");
    silent.detached = true;
    let mut invalid = command("echo invalid >> invalid.txt");
    invalid.wake = true;
    let registry = bench.load(vec![
        ("post_tool_use", lint),
        ("post_tool_use", silent),
        ("post_tool_use", invalid),
    ]);

    let started = Instant::now();
    let mut event = tool_call("edit", json!({"path": "a.rs", "edits": []}));
    registry.bus.emit(&mut event).await;
    let mut result = Event::ToolResult {
        agent: lead(),
        call_id: "call-1".into(),
        tool: "edit".into(),
        output: ToolOutput::text("edited"),
    };
    registry.bus.emit(&mut result).await;
    // The tool result does not wait for the hook.
    assert!(delivered.lock().unwrap().is_empty());
    while delivered.lock().unwrap().is_empty() && started.elapsed() < Duration::from_secs(20) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        *delivered.lock().unwrap(),
        ["<hook-result event=\"post_tool_use\">\nlint: 2 warnings in a.rs\n</hook-result>"]
    );
    assert_eq!(bench.written("invalid.txt"), None);
}

#[tokio::test]
async fn the_newer_events_reach_their_hooks_with_their_fields() {
    let bench = Bench::new();
    let mut rust_only = matching("write|edit", command("cat > changed.json"));
    rust_only.when = Some("edit(src/**)".to_string());
    let mut any_tool = command("cat > changed-any.json");
    any_tool.when = Some("*(src/**)".to_string());
    let registry = bench.load(vec![
        ("file_changed", rust_only),
        ("file_changed", any_tool),
        ("config_change", command("cat > config.json")),
        ("worktree_create", command("cat > created.json")),
        ("worktree_remove", command("cat > removed.json")),
        ("setup", command("cat > setup.json")),
    ]);
    let changed = |path: &str, tool: &str| Event::FileChanged {
        agent: lead(),
        path: bench.paths.cwd.join(path),
        tool: tool.to_string(),
    };
    registry.bus.emit(&mut changed("docs/a.md", "edit")).await;
    assert_eq!(bench.written("changed.json"), None);
    // `if` names a tool: a write is not an edit, but `*` covers both.
    registry.bus.emit(&mut changed("src/new.rs", "write")).await;
    assert_eq!(bench.written("changed.json"), None);
    assert_eq!(bench.json("changed-any.json")["tool_name"], "write");
    registry.bus.emit(&mut changed("src/a.rs", "edit")).await;
    let payload = bench.json("changed.json");
    assert_eq!(payload["hook_event_name"], "FileChanged");
    assert_eq!(payload["tool_name"], "edit");
    assert_eq!(
        payload["file_path"],
        bench.paths.cwd.join("src/a.rs").display().to_string()
    );

    registry
        .bus
        .emit(&mut Event::ConfigChange {
            source: "settings".into(),
        })
        .await;
    assert_eq!(bench.json("config.json")["source"], "settings");

    let worktree = bench.paths.worktrees_dir().join("coder-1");
    registry
        .bus
        .emit(&mut Event::WorktreeCreate {
            agent_id: "coder-1".into(),
            path: worktree.clone(),
        })
        .await;
    registry
        .bus
        .emit(&mut Event::WorktreeRemove {
            agent_id: "coder-1".into(),
            path: worktree.clone(),
        })
        .await;
    for file in ["created.json", "removed.json"] {
        let payload = bench.json(file);
        assert_eq!(payload["agent_id"], "coder-1", "{file}");
        assert_eq!(payload["worktree_path"], worktree.display().to_string(), "{file}");
    }
    assert_eq!(bench.json("removed.json")["hook_event_name"], "WorktreeRemove");

    registry
        .bus
        .emit(&mut Event::Setup {
            trigger: "init".into(),
        })
        .await;
    assert_eq!(bench.json("setup.json")["trigger"], "init");
}

#[tokio::test]
async fn the_hook_list_names_each_hook_its_source_and_how_it_last_ran() {
    let bench = Bench::new();
    let mut gate = matching("shell", command("echo 'no' >&2; exit 2"));
    gate.once = true;
    gate.timeout = 5;
    let mut ext = bench.ext(vec![
        ("stop", command("true")),
        ("pre_tool_use", gate),
        ("pre_tool_use", command("exit 1")),
    ]);
    ext.sources = BTreeMap::from([("pre_tool_use".to_string(), vec!["/home/config.toml".to_string()])]);
    let registry = bench.registry(&ext);
    let hooks = bench.services.get::<azazel_core::hooks::Hooks>().unwrap();
    assert!(hooks.enabled());
    let before = hooks.list();
    let shown: Vec<(&str, &str, Option<&str>)> = before
        .iter()
        .map(|hook| (hook.event, hook.source.as_str(), hook.matcher.as_deref()))
        .collect();
    assert_eq!(
        shown,
        [
            ("pre_tool_use", "/home/config.toml", Some("shell")),
            ("pre_tool_use", "environment or command line", None),
            ("stop", "environment or command line", None),
        ]
    );
    assert_eq!(before[0].options, ["once", "timeout 5 s"]);
    assert_eq!(before[0].kind, "command");
    assert!(before.iter().all(|hook| hook.last.is_none()));

    let mut event = tool_call("shell", json!({"command": "ls"}));
    assert_eq!(registry.bus.emit(&mut event).await, blocked("no"));
    let mut event = tool_call("read", json!({"path": "a"}));
    registry.bus.emit(&mut event).await;
    let after = hooks.list();
    assert_eq!(after[0].last.as_ref().unwrap().result, "blocked: no");
    assert_eq!(
        after[1].last.as_ref().unwrap().result,
        "failed: exited with code 1"
    );
    assert!(after[2].last.is_none());

    // Switched off, the hooks are still listed but none runs.
    let off = Bench::new();
    let mut ext = off.ext(vec![("pre_tool_use", command("echo ran > ran.txt"))]);
    ext.disabled = true;
    let registry = off.registry(&ext);
    let hooks = off.services.get::<azazel_core::hooks::Hooks>().unwrap();
    assert!(!hooks.enabled());
    assert_eq!(hooks.list().len(), 1);
    let mut event = tool_call("read", json!({"path": "a"}));
    registry.bus.emit(&mut event).await;
    assert_eq!(off.written("ran.txt"), None);
}
