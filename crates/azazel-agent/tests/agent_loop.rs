#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_agent::agent::{ABORTED_TEXT, TURN_LIMIT_TEXT};
use azazel_agent::{
    Access, Agent, AgentCmd, AgentConfig, AgentEvent, AgentHandle, AgentHooks, AgentInfo, AllowAll,
    ArtifactSink, CompactionSettings, ContextEntry, Gate, GateDecision, Limits, NoHooks, Services, Tool,
    ToolCtx, ToolOutput, TranscriptSink, UserInput,
};
use azazel_ai::testing::{MockProvider, MockStep};
use azazel_ai::{
    AiError, Block, Catalog, Message, ModelOverride, ModelSpec, Request, RetryPolicy, Role, Router,
    StopReason, StreamEvent, ThinkingLevel,
};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

// ---------------------------------------------------------------- test tools

struct WriteFile(PathBuf);

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "write a file"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn access(&self, _: &Value) -> Access {
        Access::Write
    }
    fn subjects(&self, input: &Value) -> Vec<String> {
        vec![input["path"].as_str().unwrap_or_default().to_string()]
    }
    async fn call(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let path = self.0.join(input["path"].as_str().unwrap_or_default());
        match std::fs::write(&path, input["content"].as_str().unwrap_or_default()) {
            Ok(()) => ToolOutput::text("written"),
            Err(e) => ToolOutput::error(e.to_string()),
        }
    }
}

struct ReadFile(PathBuf);

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "read a file"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn access(&self, _: &Value) -> Access {
        Access::Read
    }
    fn subjects(&self, input: &Value) -> Vec<String> {
        vec![input["path"].as_str().unwrap_or_default().to_string()]
    }
    async fn call(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        match std::fs::read_to_string(self.0.join(input["path"].as_str().unwrap_or_default())) {
            Ok(text) => ToolOutput::text(text),
            Err(e) => ToolOutput::error(e.to_string()),
        }
    }
}

/// Sleeps `ms` (stopping early on cancellation), reports progress, returns `bytes` bytes of text.
struct Slow {
    name: &'static str,
    access: Access,
    ms: u64,
    bytes: usize,
}

#[async_trait]
impl Tool for Slow {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "sleep"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn access(&self, _: &Value) -> Access {
        self.access
    }
    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }
    async fn call(&self, _: Value, ctx: &ToolCtx) -> ToolOutput {
        ctx.update.send("working");
        tokio::select! {
            _ = ctx.cancel.cancelled() => ToolOutput::error("tool saw the cancel"),
            _ = tokio::time::sleep(Duration::from_millis(self.ms)) => ToolOutput::text("x".repeat(self.bytes)),
        }
    }
}

struct Boom;

#[async_trait]
impl Tool for Boom {
    fn name(&self) -> &str {
        "boom"
    }
    fn description(&self) -> &str {
        "panics"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn access(&self, _: &Value) -> Access {
        Access::Exec
    }
    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }
    async fn call(&self, _: Value, _: &ToolCtx) -> ToolOutput {
        panic!("tool bug")
    }
}

// ---------------------------------------------------------------- recording seams

#[derive(Default)]
struct Recorder {
    messages: Mutex<Vec<Message>>,
    edits: Mutex<Vec<(String, String)>>,
    compactions: Mutex<Vec<(String, String, u64)>>,
    spills: Mutex<Vec<(String, usize)>>,
}

impl TranscriptSink for Recorder {
    fn message(&self, message: &Message) -> String {
        let mut messages = self.messages.lock().unwrap();
        messages.push(message.clone());
        format!("m{}", messages.len())
    }
    fn context_edit(&self, target: &str, replacement: &str) {
        self.edits
            .lock()
            .unwrap()
            .push((target.to_string(), replacement.to_string()));
    }
    fn compaction(&self, summary: &str, first_kept: &str, tokens_before: u64) -> String {
        self.compactions
            .lock()
            .unwrap()
            .push((summary.to_string(), first_kept.to_string(), tokens_before));
        "compaction-1".to_string()
    }
}

impl ArtifactSink for Recorder {
    fn spill(&self, call_id: &str, text: &str) -> Option<String> {
        self.spills
            .lock()
            .unwrap()
            .push((call_id.to_string(), text.len()));
        Some(format!("artifact://{call_id}"))
    }
}

// ---------------------------------------------------------------- harness

struct Setup {
    tools: Vec<Arc<dyn Tool>>,
    gate: Arc<dyn Gate>,
    hooks: Arc<dyn AgentHooks>,
    limits: Limits,
    compaction: CompactionSettings,
    context_window: u64,
    initial: Vec<ContextEntry>,
    roles: BTreeMap<String, Vec<ModelSpec>>,
}

impl Default for Setup {
    fn default() -> Self {
        Setup {
            tools: Vec::new(),
            gate: Arc::new(AllowAll),
            hooks: Arc::new(NoHooks),
            limits: Limits::default(),
            compaction: CompactionSettings::default(),
            context_window: 128_000,
            initial: Vec::new(),
            roles: BTreeMap::new(),
        }
    }
}

struct Session {
    mock: Arc<MockProvider>,
    agent: AgentHandle,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    recorder: Arc<Recorder>,
}

fn start(setup: Setup) -> Session {
    let mock = Arc::new(MockProvider::new());
    let mut catalog = Catalog::default();
    let window = ModelOverride {
        context_window: Some(setup.context_window),
        ..ModelOverride::default()
    };
    catalog.apply_overrides(&BTreeMap::from([
        ("mock/a".to_string(), window.clone()),
        ("mock/b".to_string(), window.clone()),
        ("mock/s".to_string(), window),
    ]));
    let policy = RetryPolicy {
        max_attempts: 3,
        cooldown: Duration::from_secs(300),
        base_delay: Duration::from_millis(1),
        max_jitter: Duration::ZERO,
    };
    let router = Arc::new(Router::new(vec![mock.clone()], catalog, setup.roles, policy));
    let recorder = Arc::new(Recorder::default());
    let agent = Agent::spawn(AgentConfig {
        info: AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id: "s1".into(),
        },
        system: vec![azazel_ai::SystemPart {
            text: "You are a test agent.".into(),
            cache_boundary: true,
        }],
        tools: Arc::new(setup.tools),
        chain: vec!["mock/a".parse().unwrap()],
        thinking: ThinkingLevel::Off,
        router,
        gate: setup.gate,
        bus: setup.hooks,
        services: Arc::new(Services::new()),
        cwd: std::env::temp_dir(),
        initial_messages: setup.initial,
        sink: recorder.clone(),
        artifacts: Some(recorder.clone()),
        limits: setup.limits,
        compaction: setup.compaction,
    });
    let events = agent.subscribe();
    Session {
        mock,
        agent,
        events,
        recorder,
    }
}

impl Session {
    /// Events up to and including the next `Idle` / `Failed`.
    async fn until_terminal(&mut self) -> Vec<AgentEvent> {
        let mut seen = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(20), self.events.recv())
                .await
                .expect("agent went quiet")
                .expect("agent dropped its event stream");
            let terminal = event.is_terminal();
            seen.push(event);
            if terminal {
                return seen;
            }
        }
    }

    async fn run(&mut self, prompt: &str) -> Vec<AgentEvent> {
        assert!(self.agent.prompt(UserInput::text(prompt)));
        self.until_terminal().await
    }

    async fn wait_for(&mut self, what: impl Fn(&AgentEvent) -> bool) {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(20), self.events.recv())
                .await
                .expect("agent went quiet")
                .expect("agent dropped its event stream");
            if what(&event) {
                return;
            }
        }
    }
}

fn kinds(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|e| !matches!(e, AgentEvent::Stream(_)))
        .map(|e| {
            let value = serde_json::to_value(e).unwrap();
            match e {
                AgentEvent::TurnStart { turn } => format!("turn_start {turn}"),
                _ => value["type"].as_str().unwrap().to_string(),
            }
        })
        .collect()
}

fn tool_outputs(events: &[AgentEvent]) -> Vec<(String, bool, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolEnd { call_id, output, .. } => {
                Some((call_id.clone(), output.is_error, output.text_content()))
            }
            _ => None,
        })
        .collect()
}

fn last_user_text(request: &Request) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(Message::text)
        .unwrap_or_default()
}

fn result_texts(request: &Request) -> Vec<String> {
    request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            Block::ToolResult { content, .. } => Some(
                content
                    .iter()
                    .map(|c| match c {
                        Block::Text { text } => text.clone(),
                        _ => String::new(),
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

fn slow(name: &'static str, access: Access, ms: u64, bytes: usize) -> Arc<dyn Tool> {
    Arc::new(Slow {
        name,
        access,
        ms,
        bytes,
    })
}

// ---------------------------------------------------------------- tests

/// Holds the first `write` call at the gate until released, so the test can act mid-batch.
struct HoldWrite {
    entered: mpsc::UnboundedSender<()>,
    release: Arc<Notify>,
}

#[async_trait]
impl Gate for HoldWrite {
    async fn before(&self, _: &AgentInfo, tool: &dyn Tool, _: &str, _: &Value) -> GateDecision {
        if tool.name() == "write" {
            let _ = self.entered.send(());
            self.release.notified().await;
        }
        GateDecision::Allow { input: None }
    }
    async fn after(&self, _: &AgentInfo, _: &dyn Tool, _: &str, _: &mut ToolOutput) {}
}

#[tokio::test]
async fn write_then_read_then_answer_with_a_steer_during_the_tool_batch() {
    let dir = tempfile::tempdir().unwrap();
    let (entered, mut entered_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());
    let mut session = start(Setup {
        tools: vec![
            Arc::new(WriteFile(dir.path().into())),
            Arc::new(ReadFile(dir.path().into())),
        ],
        gate: Arc::new(HoldWrite {
            entered,
            release: release.clone(),
        }),
        ..Setup::default()
    });
    session.mock.push(
        "a",
        MockStep::tools(
            "",
            &[("c1", "write", json!({"path": "out.txt", "content": "hello"}))],
        ),
    );
    session.mock.push(
        "a",
        MockStep::tools("", &[("c2", "read", json!({"path": "out.txt"}))]),
    );
    session.mock.push("a", MockStep::text("done"));

    assert!(session.agent.prompt(UserInput::text("go")));
    entered_rx.recv().await.unwrap();
    assert!(session.agent.steer(UserInput::text("also keep it short")));
    release.notify_one();
    let events = session.until_terminal().await;

    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "hello"
    );
    assert_eq!(
        kinds(&events),
        [
            "agent_start",
            "turn_start 1",
            "message_start",
            "message_end",
            "tool_start",
            "tool_end",
            "turn_end",
            "turn_start 2",
            "message_start",
            "message_end",
            "tool_start",
            "tool_end",
            "turn_end",
            "turn_start 3",
            "message_start",
            "message_end",
            "turn_end",
            "idle",
        ]
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Idle {
            stop: StopReason::Stop
        })
    ));
    assert_eq!(
        tool_outputs(&events),
        [
            ("c1".to_string(), false, "written".to_string()),
            ("c2".to_string(), false, "hello".to_string())
        ]
    );

    let requests = session.mock.requests_for("a");
    assert_eq!(requests.len(), 3);
    assert_eq!(last_user_text(&requests[0]), "go");
    // The steer lands after the first tool batch and before the second request.
    let second: Vec<Role> = requests[1].messages.iter().map(|m| m.role).collect();
    assert_eq!(second, [Role::User, Role::Assistant, Role::Tool, Role::User]);
    assert_eq!(last_user_text(&requests[1]), "also keep it short");
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        ["write", "read"]
    );
    // Everything the agent said and saw went to the transcript, in order.
    let recorded: Vec<Role> = session
        .recorder
        .messages
        .lock()
        .unwrap()
        .iter()
        .map(|m| m.role)
        .collect();
    assert_eq!(
        recorded,
        [
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::Assistant
        ]
    );
}

#[tokio::test]
async fn abort_cancels_the_running_tool_and_records_an_aborted_result() {
    let mut session = start(Setup {
        tools: vec![slow("wait", Access::Exec, 60_000, 1)],
        ..Setup::default()
    });
    session.mock.push(
        "a",
        MockStep::tools("", &[("c1", "wait", json!({})), ("c2", "wait", json!({}))]),
    );
    session.mock.push("a", MockStep::text("after"));

    let started = Instant::now();
    assert!(session.agent.prompt(UserInput::text("go")));
    session
        .wait_for(|e| matches!(e, AgentEvent::ToolUpdate { text, .. } if text == "working"))
        .await;
    assert!(session.agent.abort());
    let events = session.until_terminal().await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Idle {
            stop: StopReason::Aborted
        })
    ));
    // The running call and the one that never started both end as aborted.
    assert_eq!(
        tool_outputs(&events),
        [
            ("c1".to_string(), true, ABORTED_TEXT.to_string()),
            ("c2".to_string(), true, ABORTED_TEXT.to_string())
        ]
    );

    session.run("continue").await;
    let requests = session.mock.requests_for("a");
    assert_eq!(result_texts(&requests[1]), [ABORTED_TEXT, ABORTED_TEXT]);
    assert_eq!(last_user_text(&requests[1]), "continue");
}

#[tokio::test]
async fn read_only_calls_run_concurrently_and_others_in_order() {
    let mut session = start(Setup {
        tools: vec![
            slow("peek", Access::Read, 300, 1),
            slow("poke", Access::Exec, 300, 1),
        ],
        ..Setup::default()
    });
    session.mock.push(
        "a",
        MockStep::tools("", &[("c1", "peek", json!({})), ("c2", "peek", json!({}))]),
    );
    session.mock.push(
        "a",
        MockStep::tools("", &[("c3", "peek", json!({})), ("c4", "poke", json!({}))]),
    );
    session.mock.push("a", MockStep::text("done"));

    assert!(session.agent.prompt(UserInput::text("go")));
    let mut batch_start = Instant::now();
    let mut batches = Vec::new();
    let mut ends = 0;
    loop {
        let event = session.events.recv().await.unwrap();
        match &event {
            AgentEvent::TurnStart { .. } => batch_start = Instant::now(),
            AgentEvent::ToolEnd { .. } => {
                ends += 1;
                if ends % 2 == 0 {
                    batches.push(batch_start.elapsed());
                }
            }
            _ => {}
        }
        if event.is_terminal() {
            break;
        }
    }
    assert!(
        batches[0] < Duration::from_millis(550),
        "two reads should overlap: {:?}",
        batches[0]
    );
    assert!(
        batches[1] >= Duration::from_millis(600),
        "a mixed batch runs in order: {:?}",
        batches[1]
    );
}

struct Policy;

#[async_trait]
impl Gate for Policy {
    async fn before(&self, _: &AgentInfo, tool: &dyn Tool, _: &str, input: &Value) -> GateDecision {
        match tool.name() {
            "write" => GateDecision::Deny {
                reason: "writes are not allowed here".to_string(),
            },
            "read" if input["path"] == "secret.txt" => GateDecision::Allow {
                input: Some(json!({"path": "public.txt"})),
            },
            _ => GateDecision::Allow { input: None },
        }
    }
    async fn after(&self, _: &AgentInfo, tool: &dyn Tool, _: &str, output: &mut ToolOutput) {
        if tool.name() == "read" {
            output.content.push(Block::text("(checked)"));
        }
    }
}

#[tokio::test]
async fn gate_can_deny_rewrite_input_and_amend_output() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("public.txt"), "public").unwrap();
    std::fs::write(dir.path().join("secret.txt"), "secret").unwrap();
    let mut session = start(Setup {
        tools: vec![
            Arc::new(WriteFile(dir.path().into())),
            Arc::new(ReadFile(dir.path().into())),
        ],
        gate: Arc::new(Policy),
        ..Setup::default()
    });
    session.mock.push(
        "a",
        MockStep::tools(
            "",
            &[
                ("c1", "write", json!({"path": "new.txt", "content": "x"})),
                ("c2", "read", json!({"path": "secret.txt"})),
            ],
        ),
    );
    session.mock.push("a", MockStep::text("done"));
    let events = session.run("go").await;

    assert_eq!(
        tool_outputs(&events),
        [
            ("c1".to_string(), true, "writes are not allowed here".to_string()),
            ("c2".to_string(), false, "public\n(checked)".to_string())
        ]
    );
    assert!(!dir.path().join("new.txt").exists());
    assert_eq!(
        result_texts(&session.mock.requests_for("a")[1]),
        ["writes are not allowed here", "public(checked)"]
    );
}

#[tokio::test]
async fn bad_calls_become_error_results_and_the_agent_carries_on() {
    let mut session = start(Setup {
        tools: vec![Arc::new(Boom)],
        ..Setup::default()
    });
    session.mock.push(
        "a",
        MockStep::tools(
            "",
            &[
                ("c1", "missing", json!({})),
                ("c2", "boom", json!({"_invalid_json": "{oops"})),
                ("c3", "boom", json!({})),
            ],
        ),
    );
    session.mock.push("a", MockStep::text("recovered"));
    let events = session.run("go").await;

    let outputs = tool_outputs(&events);
    assert_eq!(
        outputs[0],
        ("c1".to_string(), true, "unknown tool `missing`".to_string())
    );
    assert!(
        outputs[1].1 && outputs[1].2.contains("not valid JSON"),
        "{outputs:?}"
    );
    assert_eq!(
        outputs[2],
        ("c3".to_string(), true, "tool `boom` panicked".to_string())
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Idle {
            stop: StopReason::Stop
        })
    ));
}

#[tokio::test]
async fn oversized_results_are_cut_and_spilled() {
    let mut session = start(Setup {
        tools: vec![slow("dump", Access::Read, 0, 10_000)],
        limits: Limits {
            max_turns: None,
            max_result_bytes: 1000,
        },
        ..Setup::default()
    });
    session
        .mock
        .push("a", MockStep::tools("", &[("c1", "dump", json!({}))]));
    session.mock.push("a", MockStep::text("done"));
    session.run("go").await;

    assert_eq!(
        *session.recorder.spills.lock().unwrap(),
        [("c1".to_string(), 10_000)]
    );
    let results = result_texts(&session.mock.requests_for("a")[1]);
    assert_eq!(
        results[0],
        format!(
            "{}[output truncated: 10000 bytes in total] full output: artifact://c1",
            "x".repeat(1000)
        )
    );
}

fn big_rounds(session: &Session, rounds: usize) {
    for i in 0..rounds {
        let id = format!("c{}", i + 1);
        session
            .mock
            .push("a", MockStep::tools("", &[(id.as_str(), "dump", json!({}))]));
    }
}

#[tokio::test]
async fn old_tool_results_are_elided_once_the_context_passes_the_micro_threshold() {
    // 1600-byte results are ~400 tokens each; the window is 2000, so eliding starts above 1000.
    let mut session = start(Setup {
        tools: vec![slow("dump", Access::Read, 0, 1600)],
        context_window: 2000,
        compaction: CompactionSettings {
            enabled: true,
            reserve_tokens: 100,
            keep_recent_tokens: 300,
            keep_tool_results: 1,
            micro_threshold: 0.5,
        },
        ..Setup::default()
    });
    big_rounds(&session, 3);
    session.mock.push("a", MockStep::text("done"));
    let events = session.run("go").await;
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Compaction { .. })));

    let note = "[result elided: 1600 bytes; re-run the tool if needed]";
    let requests = session.mock.requests_for("a");
    assert_eq!(
        result_texts(&requests[2]),
        ["x".repeat(1600), "x".repeat(1600)],
        "below the threshold"
    );
    assert_eq!(
        result_texts(&requests[3]),
        [note.to_string(), note.to_string(), "x".repeat(1600)]
    );
    // The two edits were recorded against the transcript ids of the first two tool messages.
    assert_eq!(
        *session.recorder.edits.lock().unwrap(),
        [
            ("m3".to_string(), note.to_string()),
            ("m5".to_string(), note.to_string())
        ]
    );
}

#[tokio::test]
async fn full_compaction_summarises_the_old_span_and_keeps_the_recent_tail() {
    let mut session = start(Setup {
        tools: vec![slow("dump", Access::Read, 0, 1600)],
        context_window: 2000,
        compaction: CompactionSettings {
            enabled: true,
            reserve_tokens: 500,
            keep_recent_tokens: 500,
            keep_tool_results: 100,
            micro_threshold: 1.0,
        },
        ..Setup::default()
    });
    big_rounds(&session, 4);
    session.mock.push("a", MockStep::text("## Goal\nship it"));
    session.mock.push("a", MockStep::text("done"));
    let events = session.run("go").await;

    let (before, after) = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Compaction {
                tokens_before,
                tokens_after,
            } => Some((*tokens_before, *tokens_after)),
            _ => None,
        })
        .expect("a compaction event");
    assert!(before > 1500 && after < before, "{before} -> {after}");

    let requests = session.mock.requests_for("a");
    // Request 5 is the summariser: fixed headings in the system prompt, no tools, the old span inside.
    let summariser = &requests[4];
    assert!(summariser.tools.is_empty());
    assert!(summariser.system[0].text.contains("## Files modified"));
    assert!(
        summariser.messages[0]
            .text()
            .starts_with("<conversation>\nUser: go\n\nAssistant called dump")
    );
    // Transcript ids: m1 user, m2/m3 round 1, m4/m5 round 2, m6/m7 round 3, m8/m9 round 4.
    // Keeping ~500 tokens covers rounds 3-4; the cut backs up from tool m7 to its assistant m6.
    assert_eq!(
        *session.recorder.compactions.lock().unwrap(),
        [("## Goal\nship it".to_string(), "m6".to_string(), before)]
    );
    let resumed = &requests[5];
    assert_eq!(
        resumed.messages[0].text(),
        "Summary of the conversation so far:\n\n## Goal\nship it"
    );
    assert_eq!(
        resumed.messages.iter().map(|m| m.role).collect::<Vec<_>>(),
        [
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::Assistant,
            Role::Tool
        ]
    );
    assert_eq!(session.agent.status().requests, 6);
}

fn history() -> Vec<ContextEntry> {
    let reply = |text: &str| {
        let mut message = Message::user(text);
        message.role = Role::Assistant;
        message
    };
    [
        Message::user("first task"),
        reply("did it"),
        Message::user("second task"),
        reply("did that too"),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, message)| ContextEntry {
        id: format!("h{i}"),
        message,
        summary: false,
    })
    .collect()
}

#[tokio::test]
async fn context_overflow_forces_one_compaction_then_fails_if_it_overflows_again() {
    let mut session = start(Setup {
        initial: history(),
        ..Setup::default()
    });
    session.mock.push("a", MockStep::fail(AiError::ContextOverflow));
    session.mock.push("a", MockStep::text("## Goal\nboth tasks done"));
    session.mock.push("a", MockStep::text("third done"));
    let events = session.run("third task").await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Idle {
            stop: StopReason::Stop
        })
    ));
    let retried = &session.mock.requests_for("a")[2];
    assert!(retried.messages[0].text().contains("both tasks done"));
    assert_eq!(last_user_text(retried), "third task");

    let mut session = start(Setup {
        initial: history(),
        ..Setup::default()
    });
    session.mock.push("a", MockStep::fail(AiError::ContextOverflow));
    session.mock.push("a", MockStep::text("## Goal\nsummary"));
    session.mock.push("a", MockStep::fail(AiError::ContextOverflow));
    let events = session.run("third task").await;
    match events.last() {
        Some(AgentEvent::Failed { error }) => assert!(error.contains("context window"), "{error}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn turn_limit_asks_for_a_final_report_without_tools() {
    let mut session = start(Setup {
        tools: vec![slow("dump", Access::Read, 0, 4)],
        limits: Limits {
            max_turns: Some(1),
            ..Limits::default()
        },
        ..Setup::default()
    });
    session
        .mock
        .push("a", MockStep::tools("", &[("c1", "dump", json!({}))]));
    session.mock.push("a", MockStep::text("final report"));
    let events = session.run("go").await;

    let requests = session.mock.requests_for("a");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools.len(), 1);
    assert!(requests[1].tools.is_empty());
    assert_eq!(last_user_text(&requests[1]), TURN_LIMIT_TEXT);
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Idle {
            stop: StopReason::Stop
        })
    ));
}

#[derive(Default)]
struct Hooks {
    stop_calls: Mutex<Vec<bool>>,
}

#[async_trait]
impl AgentHooks for Hooks {
    async fn before_request(&self, _: &AgentInfo, request: &mut Request) {
        request.tools.retain(|t| t.name != "write");
    }
    async fn stop(&self, _: &AgentInfo, stop_hook_active: bool) -> Option<String> {
        self.stop_calls.lock().unwrap().push(stop_hook_active);
        (!stop_hook_active).then(|| "run the tests first".to_string())
    }
}

#[tokio::test]
async fn stop_hook_keeps_the_agent_going_and_withdrawn_tools_cannot_be_called() {
    let dir = tempfile::tempdir().unwrap();
    let hooks = Arc::new(Hooks::default());
    let mut session = start(Setup {
        tools: vec![
            Arc::new(WriteFile(dir.path().into())),
            Arc::new(ReadFile(dir.path().into())),
        ],
        hooks: hooks.clone(),
        ..Setup::default()
    });
    session.mock.push(
        "a",
        MockStep::tools("", &[("c1", "write", json!({"path": "x.txt", "content": "x"}))]),
    );
    session.mock.push("a", MockStep::text("done"));
    session.mock.push("a", MockStep::text("tests pass, really done"));
    let events = session.run("go").await;

    assert_eq!(
        tool_outputs(&events)[0],
        ("c1".to_string(), true, "unknown tool `write`".to_string())
    );
    assert!(!dir.path().join("x.txt").exists());
    let requests = session.mock.requests_for("a");
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        ["read"]
    );
    assert_eq!(last_user_text(&requests[2]), "run the tests first");
    assert_eq!(*hooks.stop_calls.lock().unwrap(), [false, true]);
    assert_eq!(kinds(&events).iter().filter(|k| *k == "idle").count(), 1);
}

#[tokio::test]
async fn a_stream_that_fails_part_way_is_discarded_and_retried() {
    let mut session = start(Setup::default());
    session.mock.push(
        "a",
        MockStep {
            events: vec![
                StreamEvent::Start,
                StreamEvent::TextDelta {
                    index: 0,
                    delta: "par".into(),
                },
                StreamEvent::Error {
                    error: AiError::Overloaded,
                    partial: None,
                },
            ],
            ..MockStep::default()
        },
    );
    session.mock.push("a", MockStep::text("complete"));
    let events = session.run("go").await;

    assert_eq!(
        kinds(&events),
        [
            "agent_start",
            "turn_start 1",
            "message_start",
            "retry",
            "message_start",
            "message_end",
            "turn_end",
            "idle"
        ]
    );
    let finals: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageEnd { message } => Some(message.text()),
            _ => None,
        })
        .collect();
    assert_eq!(finals, ["complete"]);
    // The discarded partial never reached the transcript.
    assert_eq!(session.recorder.messages.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn follow_ups_run_after_idle_and_model_changes_apply_to_the_next_request() {
    let mut session = start(Setup {
        tools: vec![slow("wait", Access::Exec, 150, 1)],
        ..Setup::default()
    });
    session
        .mock
        .push("a", MockStep::tools("", &[("c1", "wait", json!({}))]));
    session.mock.push("b", MockStep::text("first done"));
    session.mock.push("b", MockStep::text("second done"));

    assert!(session.agent.prompt(UserInput::text("first")));
    session
        .wait_for(|e| matches!(e, AgentEvent::ToolStart { .. }))
        .await;
    assert!(session.agent.follow_up(UserInput::text("second")));
    assert!(
        session
            .agent
            .send(AgentCmd::SetModel(vec!["mock/b".parse().unwrap()]))
    );

    let first = session.until_terminal().await;
    let second = session.until_terminal().await;
    assert!(matches!(first.last(), Some(AgentEvent::Idle { .. })));
    assert_eq!(kinds(&second).first().map(String::as_str), Some("agent_start"));
    // The model switch arrived mid-tool, so the very next request already used it.
    assert_eq!(session.mock.requests_for("a").len(), 1);
    let on_b = session.mock.requests_for("b");
    assert_eq!(on_b.len(), 2);
    assert_eq!(last_user_text(&on_b[1]), "second");
    assert_eq!(
        session.agent.status().model.map(|m| m.to_string()).as_deref(),
        Some("mock/b")
    );

    session.agent.shutdown().await;
    assert!(!session.agent.prompt(UserInput::text("anyone there?")));
}

#[tokio::test]
async fn append_adds_context_without_starting_a_turn() {
    let mut session = start(Setup::default());
    session.mock.push("a", MockStep::text("seen"));
    assert!(
        session
            .agent
            .send(AgentCmd::Append(UserInput::text("ran `ls`:\nsrc\ntests")))
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        session.mock.requests().is_empty(),
        "appending must not call the model"
    );
    assert!(session.events.try_recv().is_err(), "appending emits no events");

    session.run("what did ls show?").await;
    let request = &session.mock.requests_for("a")[0];
    let texts: Vec<String> = request.messages.iter().map(Message::text).collect();
    assert_eq!(texts, ["ran `ls`:\nsrc\ntests", "what did ls show?"]);
    assert_eq!(session.recorder.messages.lock().unwrap().len(), 3);
}
