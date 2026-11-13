//! The agent actor: one task that owns a conversation and drives model requests and tool calls.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use azazel_ai::router::{RouteEvent, RouteState};
use azazel_ai::{
    AiError, Block, INVALID_JSON_KEY, Message, ModelRef, ModelSpec, Request, Router, StopReason, StreamEvent,
    SystemPart, ThinkingLevel, Usage,
};
use futures::StreamExt;
use futures::future::join_all;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::compaction::{self, CompactionSettings, ContextEntry};
use crate::event::{AgentCmd, AgentEvent, AgentId, AgentInfo, UserInput};
use crate::gate::{Gate, GateDecision};
use crate::hooks::{AgentHooks, TranscriptSink};
use crate::inspect::RequestRecord;
use crate::tool::{ArtifactSink, Services, Tool, ToolCtx, ToolOutput, ToolSource, UpdateSink, tool_spec};

pub const TURN_LIMIT_TEXT: &str = "Turn limit reached. Reply with your final report now.";
pub const ABORTED_TEXT: &str = "aborted by user";

/// Replies a rule may withdraw per prompt; after that the reply stands, so two rules that
/// contradict each other cannot keep an agent going forever.
const MAX_RULE_STOPS: u32 = 2;

/// What one request produced.
enum Reply {
    Message(Message),
    /// A stream rule matched; the note to add before asking again.
    Withdrawn(String),
}

/// The `name` attribute of the tag a rule note opens with (`<rule name="no-todo" …>`).
fn rule_name(note: &str) -> String {
    note.lines()
        .next()
        .and_then(|line| line.split_once("name=\""))
        .and_then(|(_, rest)| rest.split_once('"'))
        .map_or_else(|| "stream rule".to_string(), |(name, _)| name.to_string())
}

/// How long an aborted tool gets to clean up before its task is dropped.
const ABORT_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Requests per prompt before the agent is told to wrap up.
    pub max_turns: Option<u32>,
    /// Tool results larger than this are cut and spilled to an artifact.
    pub max_result_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_turns: None,
            max_result_bytes: 50_000,
        }
    }
}

pub struct AgentConfig {
    pub info: AgentInfo,
    pub system: Vec<SystemPart>,
    pub tools: Arc<dyn ToolSource>,
    pub chain: Vec<ModelSpec>,
    pub thinking: ThinkingLevel,
    pub router: Arc<Router>,
    pub gate: Arc<dyn Gate>,
    pub bus: Arc<dyn AgentHooks>,
    pub services: Arc<Services>,
    pub cwd: PathBuf,
    pub initial_messages: Vec<ContextEntry>,
    pub sink: Arc<dyn TranscriptSink>,
    pub artifacts: Option<Arc<dyn ArtifactSink>>,
    pub limits: Limits,
    pub compaction: CompactionSettings,
}

#[derive(Debug, Clone, Default)]
pub struct AgentStatus {
    pub busy: bool,
    /// Request number within the current prompt.
    pub turn: u32,
    /// Model that answered most recently.
    pub model: Option<ModelRef>,
    pub chain: Vec<ModelSpec>,
    pub thinking: ThinkingLevel,
    pub context_tokens: u64,
    pub context_window: u64,
    /// Totals over the agent's lifetime, including summarisation requests.
    pub usage: Usage,
    pub requests: u32,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Lossless fan-out of events to any number of subscribers.
#[derive(Default)]
struct EventHub {
    subscribers: Mutex<Vec<mpsc::UnboundedSender<AgentEvent>>>,
}

impl EventHub {
    fn emit(&self, event: AgentEvent) {
        let mut subscribers = locked(&self.subscribers);
        match subscribers.len() {
            0 => {}
            1 => {
                if subscribers[0].send(event).is_err() {
                    subscribers.clear();
                }
            }
            _ => subscribers.retain(|s| s.send(event.clone()).is_ok()),
        }
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<AgentEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        locked(&self.subscribers).push(tx);
        rx
    }
}

#[derive(Clone)]
pub struct AgentHandle {
    pub id: AgentId,
    info: AgentInfo,
    commands: mpsc::UnboundedSender<AgentCmd>,
    hub: Arc<EventHub>,
    status: Arc<Mutex<AgentStatus>>,
    last_request: Arc<Mutex<Option<Arc<RequestRecord>>>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl AgentHandle {
    pub fn info(&self) -> &AgentInfo {
        &self.info
    }

    /// False when the agent has shut down.
    pub fn send(&self, command: AgentCmd) -> bool {
        self.commands.send(command).is_ok()
    }

    pub fn prompt(&self, input: UserInput) -> bool {
        self.send(AgentCmd::Prompt(input))
    }

    pub fn steer(&self, input: UserInput) -> bool {
        self.send(AgentCmd::Steer(input))
    }

    pub fn follow_up(&self, input: UserInput) -> bool {
        self.send(AgentCmd::FollowUp(input))
    }

    pub fn abort(&self) -> bool {
        self.send(AgentCmd::Abort)
    }

    /// Events from now on. Subscribe before sending the prompt whose events you want.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<AgentEvent> {
        self.hub.subscribe()
    }

    pub fn status(&self) -> AgentStatus {
        locked(&self.status).clone()
    }

    /// The request the agent sent last, exactly as it was sent; `None` before its first.
    pub fn last_request(&self) -> Option<Arc<RequestRecord>> {
        locked(&self.last_request).clone()
    }

    /// Asks the agent to stop and waits for its task to end.
    pub async fn shutdown(&self) {
        self.send(AgentCmd::Shutdown);
        let task = locked(&self.task).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

pub struct Agent;

impl Agent {
    pub fn spawn(config: AgentConfig) -> AgentHandle {
        let (commands, rx) = mpsc::unbounded_channel();
        let hub = Arc::new(EventHub::default());
        let status = Arc::new(Mutex::new(AgentStatus {
            chain: config.chain.clone(),
            thinking: config.thinking,
            ..AgentStatus::default()
        }));
        let info = config.info.clone();
        let last_request = Arc::new(Mutex::new(None));
        let actor = Actor {
            inbox: Inbox {
                rx,
                closed: false,
                steer: VecDeque::new(),
                follow_up: VecDeque::new(),
                chain: config.chain,
                thinking: config.thinking,
                compact: None,
                shutdown: false,
                turn_cancel: CancellationToken::new(),
            },
            core: Core {
                info: config.info,
                system: config.system,
                tools: config.tools,
                router: config.router,
                gate: config.gate,
                hooks: config.bus,
                services: config.services,
                cwd: config.cwd,
                sink: config.sink,
                artifacts: config.artifacts,
                limits: config.limits,
                compaction: config.compaction,
                entries: config.initial_messages,
                anchor: None,
                hub: hub.clone(),
                status: status.clone(),
                requests: 0,
                last_request: last_request.clone(),
            },
        };
        let task = tokio::spawn(actor.run());
        AgentHandle {
            id: info.id.clone(),
            info,
            commands,
            hub,
            status,
            last_request,
            task: Arc::new(Mutex::new(Some(task))),
        }
    }
}

/// Command intake. Kept apart from [`Core`] so commands can be handled while a future that borrows
/// the core is in flight.
struct Inbox {
    rx: mpsc::UnboundedReceiver<AgentCmd>,
    closed: bool,
    steer: VecDeque<UserInput>,
    follow_up: VecDeque<UserInput>,
    chain: Vec<ModelSpec>,
    thinking: ThinkingLevel,
    /// A compaction requested while busy; runs when the prompt ends.
    compact: Option<Option<String>>,
    shutdown: bool,
    turn_cancel: CancellationToken,
}

impl Inbox {
    fn while_busy(&mut self, command: AgentCmd) {
        match command {
            AgentCmd::Prompt(input) | AgentCmd::FollowUp(input) => self.follow_up.push_back(input),
            AgentCmd::Steer(input) | AgentCmd::Append(input) => self.steer.push_back(input),
            AgentCmd::Abort => self.turn_cancel.cancel(),
            AgentCmd::SetModel(chain) => self.chain = chain,
            AgentCmd::SetThinking(level) => self.thinking = level,
            AgentCmd::Compact { instructions } => self.compact = Some(instructions),
            AgentCmd::Shutdown => self.stop(),
        }
    }

    fn stop(&mut self) {
        self.shutdown = true;
        self.turn_cancel.cancel();
    }

    /// Runs `future` to completion while still reacting to commands.
    async fn drive<T>(&mut self, future: impl Future<Output = T>) -> T {
        tokio::pin!(future);
        loop {
            tokio::select! {
                biased;
                output = &mut future => return output,
                command = self.rx.recv(), if !self.closed => match command {
                    Some(command) => self.while_busy(command),
                    None => {
                        self.closed = true;
                        self.stop();
                    }
                },
            }
        }
    }

    /// All queued steering inputs as one user message.
    fn take_steer(&mut self) -> Option<Message> {
        if self.steer.is_empty() {
            return None;
        }
        let content: Vec<Block> = self.steer.drain(..).flat_map(|input| input.content).collect();
        Some(Message::user_blocks(content))
    }
}

struct Core {
    info: AgentInfo,
    system: Vec<SystemPart>,
    tools: Arc<dyn ToolSource>,
    router: Arc<Router>,
    gate: Arc<dyn Gate>,
    hooks: Arc<dyn AgentHooks>,
    services: Arc<Services>,
    cwd: PathBuf,
    sink: Arc<dyn TranscriptSink>,
    artifacts: Option<Arc<dyn ArtifactSink>>,
    limits: Limits,
    compaction: CompactionSettings,
    entries: Vec<ContextEntry>,
    /// `(entries.len(), context tokens)` as of the last response that reported usage.
    anchor: Option<(usize, u64)>,
    hub: Arc<EventHub>,
    status: Arc<Mutex<AgentStatus>>,
    /// Requests of its own the agent has built.
    requests: u32,
    last_request: Arc<Mutex<Option<Arc<RequestRecord>>>>,
}

struct Actor {
    inbox: Inbox,
    core: Core,
}

type Call = (String, String, Value);

impl Actor {
    async fn run(mut self) {
        loop {
            let command = match self.inbox.follow_up.pop_front() {
                Some(input) => AgentCmd::Prompt(input),
                None => match self.inbox.rx.recv().await {
                    Some(command) => command,
                    None => break,
                },
            };
            match command {
                AgentCmd::Prompt(input) | AgentCmd::Steer(input) | AgentCmd::FollowUp(input) => {
                    self.run_prompt(input).await
                }
                AgentCmd::Append(input) => self.core.push(input.into_message()),
                AgentCmd::Abort => {}
                AgentCmd::SetModel(chain) => self.inbox.chain = chain,
                AgentCmd::SetThinking(level) => self.inbox.thinking = level,
                AgentCmd::Compact { instructions } => self.inbox.compact = Some(instructions),
                AgentCmd::Shutdown => break,
            }
            if self.inbox.shutdown {
                break;
            }
            {
                let mut status = locked(&self.core.status);
                status.chain = self.inbox.chain.clone();
                status.thinking = self.inbox.thinking;
            }
            if let Some(instructions) = self.inbox.compact.take() {
                self.manual_compact(instructions).await;
            }
        }
    }

    async fn run_prompt(&mut self, input: UserInput) {
        let cancel = CancellationToken::new();
        self.inbox.turn_cancel = cancel.clone();
        locked(&self.core.status).busy = true;
        self.core.emit(AgentEvent::AgentStart);
        self.core.push(input.into_message());
        let outcome = self.turns(&cancel).await;
        {
            let mut status = locked(&self.core.status);
            status.busy = false;
            status.context_tokens = self.core.estimate();
        }
        self.core.emit(match outcome {
            Ok(stop) => AgentEvent::Idle { stop },
            Err(error) => AgentEvent::Failed { error },
        });
    }

    async fn manual_compact(&mut self, instructions: Option<String>) {
        let cancel = CancellationToken::new();
        self.inbox.turn_cancel = cancel.clone();
        let chain = self.inbox.chain.clone();
        let result = self
            .inbox
            .drive(
                self.core
                    .compact(&chain, &cancel, true, true, instructions.as_deref()),
            )
            .await;
        if let Err(error) = result {
            self.core.emit(AgentEvent::Failed {
                error: format!("compaction failed: {error}"),
            });
        }
    }

    async fn turns(&mut self, cancel: &CancellationToken) -> Result<StopReason, String> {
        let mut turn = 0u32;
        let mut stop_hook_active = false;
        let mut overflowed = false;
        let mut final_report = false;
        let mut rule_stops = 0u32;
        loop {
            if cancel.is_cancelled() {
                return Ok(StopReason::Aborted);
            }
            turn += 1;
            if !final_report && self.core.limits.max_turns.is_some_and(|max| turn > max) {
                self.core.push(Message::user(TURN_LIMIT_TEXT));
                final_report = true;
            }
            locked(&self.core.status).turn = turn;
            self.core.emit(AgentEvent::TurnStart { turn });
            if let Err(reason) = self
                .inbox
                .drive(self.core.hooks.turn_start(&self.core.info))
                .await
            {
                self.core.emit(AgentEvent::TurnEnd);
                return Err(reason);
            }

            let chain = self.inbox.chain.clone();
            let thinking = self.inbox.thinking;
            if let Err(error) = self.inbox.drive(self.core.maintain_context(&chain, cancel)).await {
                tracing::warn!("compaction skipped: {error}");
            }
            let (request, tools) = self
                .inbox
                .drive(self.core.build_request(thinking, !final_report))
                .await;
            let request = Arc::new(request);
            self.core.record_request(&request);

            let watched = rule_stops < MAX_RULE_STOPS;
            let message = match self.stream_message(&chain, &request, cancel, watched).await {
                Ok(Reply::Message(message)) => message,
                Ok(Reply::Withdrawn(note)) => {
                    rule_stops += 1;
                    self.core.emit(AgentEvent::Rule {
                        name: rule_name(&note),
                    });
                    self.core.push(Message::user(note));
                    self.core.emit(AgentEvent::TurnEnd);
                    continue;
                }
                Err(AiError::Aborted) => {
                    self.core.emit(AgentEvent::TurnEnd);
                    return Ok(StopReason::Aborted);
                }
                Err(AiError::ContextOverflow) if !overflowed => {
                    overflowed = true;
                    self.core.emit(AgentEvent::TurnEnd);
                    let compacted = self
                        .inbox
                        .drive(self.core.compact(&chain, cancel, true, false, None))
                        .await;
                    match compacted {
                        Ok(true) => continue,
                        Ok(false) => {
                            return Err(format!("{}; nothing left to compact", AiError::ContextOverflow));
                        }
                        Err(error) => {
                            return Err(format!(
                                "{}; compaction failed: {error}",
                                AiError::ContextOverflow
                            ));
                        }
                    }
                }
                Err(error) => {
                    self.core.emit(AgentEvent::TurnEnd);
                    return Err(error.to_string());
                }
            };
            overflowed = false;

            let stop = message.meta.as_ref().map_or(StopReason::Stop, |m| m.stop);
            let calls: Vec<Call> = message
                .tool_calls()
                .map(|(id, name, input)| (id.to_string(), name.to_string(), input.clone()))
                .collect();
            self.core.record_response(&message);
            self.core.push(message.clone());
            self.core.emit(AgentEvent::MessageEnd { message });

            if calls.is_empty() || final_report {
                self.inbox.drive(self.core.hooks.turn_end(&self.core.info)).await;
                self.core.emit(AgentEvent::TurnEnd);
                if final_report {
                    return Ok(stop);
                }
                if let Some(steer) = self.inbox.take_steer() {
                    self.core.push(steer);
                    continue;
                }
                let keep_going = self
                    .inbox
                    .drive(self.core.hooks.stop(&self.core.info, stop_hook_active))
                    .await;
                if let Some(reason) = keep_going {
                    self.core.push(Message::user(reason));
                    stop_hook_active = true;
                    continue;
                }
                return Ok(stop);
            }

            let results = self.run_tools(calls, &tools, cancel).await;
            self.core.push(Message::tool(results));
            self.inbox.drive(self.core.hooks.turn_end(&self.core.info)).await;
            self.core.emit(AgentEvent::TurnEnd);
            if cancel.is_cancelled() {
                return Ok(StopReason::Aborted);
            }
            if let Some(steer) = self.inbox.take_steer() {
                self.core.push(steer);
            }
        }
    }

    /// One assistant message, re-entering the router when a stream fails part-way. With `watched`
    /// the hooks see the reply as it grows and may withdraw it.
    async fn stream_message(
        &mut self,
        chain: &[ModelSpec],
        request: &Request,
        cancel: &CancellationToken,
        watched: bool,
    ) -> Result<Reply, AiError> {
        let (route_tx, mut route_rx) = mpsc::unbounded_channel();
        let mut route = RouteState::with_events(route_tx);
        let router = self.core.router.clone();
        loop {
            // Its own token, so a withdrawn reply ends this request without ending the prompt.
            let request_cancel = cancel.child_token();
            let started = self
                .inbox
                .drive(router.stream_with(chain, request, request_cancel.clone(), &mut route))
                .await;
            self.core.forward_route_events(&mut route_rx);
            let (model, mut stream) = started?;
            locked(&self.core.status).model = Some(model.clone());
            self.core.emit(AgentEvent::MessageStart);

            let (mut text, mut thinking) = (String::new(), String::new());
            let (error, partial) = loop {
                match self.inbox.drive(stream.next()).await {
                    Some(StreamEvent::Done { message }) => return Ok(Reply::Message(message)),
                    Some(StreamEvent::Error { error, partial }) => break (error, partial),
                    Some(event) => {
                        let note = match &event {
                            StreamEvent::TextDelta { delta, .. } if watched => {
                                text.push_str(delta);
                                self.core.hooks.stream_text(&self.core.info, &text)
                            }
                            StreamEvent::ThinkingDelta { delta, .. } if watched => {
                                thinking.push_str(delta);
                                self.core.hooks.stream_thinking(&self.core.info, &thinking)
                            }
                            _ => None,
                        };
                        self.core.emit(AgentEvent::Stream(event));
                        if let Some(note) = note {
                            request_cancel.cancel();
                            return Ok(Reply::Withdrawn(note));
                        }
                    }
                    None => {
                        break (
                            AiError::Network("stream ended without a terminal event".to_string()),
                            None,
                        );
                    }
                }
            };
            match error {
                AiError::Aborted => {
                    // Keep what the model had already said, marked as cut short.
                    if let Some(mut message) = partial.filter(|m| !m.content.is_empty()) {
                        if let Some(meta) = &mut message.meta {
                            meta.stop = StopReason::Aborted;
                        }
                        self.core.push(message.clone());
                        self.core.emit(AgentEvent::MessageEnd { message });
                    }
                    return Err(AiError::Aborted);
                }
                AiError::ContextOverflow | AiError::InvalidRequest(_) => return Err(error),
                error => {
                    router.report_stream_failure(&model, &error, &mut route);
                    self.core.forward_route_events(&mut route_rx);
                }
            }
        }
    }

    /// Runs the calls of one assistant message: concurrently when every call allows it, otherwise
    /// one after another in order. Always returns one result per call.
    async fn run_tools(
        &mut self,
        calls: Vec<Call>,
        tools: &[Arc<dyn Tool>],
        cancel: &CancellationToken,
    ) -> Vec<Block> {
        let resolved: Vec<Option<Arc<dyn Tool>>> = calls
            .iter()
            .map(|(_, name, _)| tools.iter().find(|t| t.name() == name).cloned())
            .collect();
        let concurrent = calls.len() > 1
            && calls
                .iter()
                .zip(&resolved)
                .all(|((_, _, input), tool)| tool.as_ref().is_none_or(|t| t.concurrency_safe(input)));
        let core = &self.core;
        if concurrent {
            let running = calls
                .into_iter()
                .zip(resolved)
                .map(|(call, tool)| core.run_tool(call, tool, cancel));
            return self.inbox.drive(join_all(running)).await;
        }
        let mut results = Vec::with_capacity(calls.len());
        for (call, tool) in calls.into_iter().zip(resolved) {
            results.push(self.inbox.drive(core.run_tool(call, tool, cancel)).await);
        }
        results
    }
}

impl Core {
    fn emit(&self, event: AgentEvent) {
        self.hub.emit(event);
    }

    fn push(&mut self, message: Message) {
        let id = self.sink.message(&message);
        self.entries.push(ContextEntry {
            id,
            message,
            summary: false,
        });
    }

    /// Books the usage of a response and anchors the context estimate to it. Call before `push`.
    fn record_response(&mut self, message: &Message) {
        let Some(meta) = &message.meta else { return };
        self.add_usage(&meta.usage);
        let tokens = meta.usage.context_tokens();
        if tokens > 0 {
            self.anchor = Some((self.entries.len() + 1, tokens));
        }
        // The request this answers now has a model and, when the provider counts, a size.
        let answered = locked(&self.last_request).as_ref().map(|sent| {
            let input = meta.usage.input + meta.usage.cache_read + meta.usage.cache_write;
            Arc::new(RequestRecord {
                model: Some(format!("{}/{}", meta.provider, meta.model)),
                reported: (input > 0).then_some(input),
                ..RequestRecord::clone(sent)
            })
        });
        if let Some(record) = answered {
            self.keep_request(record);
        }
    }

    /// Keeps `request` as the one sent last, so it can be taken apart without building it again.
    fn record_request(&mut self, request: &Arc<Request>) {
        self.requests += 1;
        let record = Arc::new(RequestRecord {
            agent: self.info.id.clone(),
            seq: self.requests,
            request: request.clone(),
            window: locked(&self.status).context_window,
            model: None,
            reported: None,
        });
        self.keep_request(record);
    }

    fn keep_request(&self, record: Arc<RequestRecord>) {
        // Only the lead's is written down: it is the one a session is asked about afterwards.
        if self.info.depth == 0 {
            self.sink.request(&record.report());
        }
        *locked(&self.last_request) = Some(record);
    }

    fn add_usage(&self, usage: &Usage) {
        let mut status = locked(&self.status);
        status.usage.add(usage);
        status.requests += 1;
    }

    /// Tokens the next request will carry: the last reported usage plus an estimate of everything
    /// appended since, or a pure estimate when no usage is available.
    fn estimate(&self) -> u64 {
        let rest = |from: usize| -> u64 {
            self.entries[from..]
                .iter()
                .map(|e| compaction::estimate_message(&e.message))
                .sum()
        };
        match self.anchor {
            Some((len, tokens)) if len <= self.entries.len() => tokens + rest(len),
            _ => {
                self.system
                    .iter()
                    .map(|p| compaction::estimate_text(&p.text))
                    .sum::<u64>()
                    + rest(0)
            }
        }
    }

    fn forward_route_events(&self, events: &mut mpsc::UnboundedReceiver<RouteEvent>) {
        while let Ok(event) = events.try_recv() {
            self.emit(match event {
                RouteEvent::Retry {
                    attempt,
                    model,
                    error,
                    ..
                } => AgentEvent::Retry {
                    attempt,
                    model,
                    stalled_secs: match error {
                        AiError::Stalled { secs } => Some(secs),
                        _ => None,
                    },
                },
                RouteEvent::Fallback { from, to, .. } => AgentEvent::Fallback { from, to },
                RouteEvent::Account {
                    provider,
                    from,
                    to,
                    until,
                    limit,
                } => AgentEvent::Account {
                    provider,
                    from,
                    to,
                    until,
                    limit,
                },
                RouteEvent::TierDropped { model, tier } => AgentEvent::TierDropped { model, tier },
            });
        }
    }

    /// Elides old tool results and compacts when the context is close to the model's window.
    async fn maintain_context(
        &mut self,
        chain: &[ModelSpec],
        cancel: &CancellationToken,
    ) -> Result<(), AiError> {
        let window = match self.router.resolve(chain).first() {
            Some(model) => self.router.model_info(model).context_window,
            None => return Ok(()),
        };
        let mut estimate = self.estimate();
        {
            let mut status = locked(&self.status);
            status.context_tokens = estimate;
            status.context_window = window;
        }
        if !self.compaction.enabled {
            return Ok(());
        }
        if estimate as f64 > self.compaction.micro_threshold * window as f64 {
            let mut changed = false;
            let recall = self.recall_offered();
            for i in compaction::micro_targets(&self.entries, self.compaction.keep_tool_results) {
                if let Some(note) = compaction::elide(&mut self.entries[i], recall) {
                    self.sink.context_edit(&self.entries[i].id, &note);
                    changed = true;
                }
            }
            if changed {
                self.anchor = None;
                estimate = self.estimate();
            }
        }
        if estimate > window.saturating_sub(self.compaction.reserve_tokens) {
            self.compact(chain, cancel, false, false, None).await?;
        }
        Ok(())
    }

    /// Whether the agent is offered the tool that reads back what was taken out of its context.
    fn recall_offered(&self) -> bool {
        self.tools
            .tools(&self.info)
            .iter()
            .any(|tool| tool.name() == compaction::RECALL_TOOL)
    }

    /// Replaces the older part of the context with a summary. `Ok(false)` when there was nothing
    /// to summarise or a hook cancelled a manual compaction.
    async fn compact(
        &mut self,
        chain: &[ModelSpec],
        cancel: &CancellationToken,
        forced: bool,
        manual: bool,
        instructions: Option<&str>,
    ) -> Result<bool, AiError> {
        if let Some(reason) = self.hooks.pre_compact(&self.info, manual).await
            && manual
        {
            tracing::info!("compaction cancelled by hook: {reason}");
            return Ok(false);
        }
        let Some(cut) = compaction::cut_point(&self.entries, self.compaction.keep_recent_tokens, forced)
        else {
            return Ok(false);
        };
        let tokens_before = self.estimate();

        let smol = [ModelSpec::Role("smol".to_string())];
        let summariser: &[ModelSpec] = if self.router.resolve(&smol).is_empty() {
            chain
        } else {
            &smol
        };
        let window = match self.router.resolve(summariser).first() {
            Some(model) => self.router.model_info(model).context_window,
            None => return Err(AiError::InvalidRequest("no model configured".to_string())),
        };
        let span = &self.entries[..cut];
        let (previous, span) = match span.split_first() {
            Some((first, rest)) if first.summary => (Some(compaction::summary_text(&first.message)), rest),
            _ => (None, span),
        };
        // Roughly three characters per token, leaving room for the prompt and the summary itself.
        let max_chars = (window.saturating_sub(8192).max(4096) * 3) as usize;
        let mut prompt = String::new();
        if let Some(previous) = previous {
            prompt.push_str(&format!(
                "<previous-summary>\n{previous}\n</previous-summary>\n\n"
            ));
        }
        // With the recall tool the summary can say which entry holds a detail it leaves out.
        let recall = self.recall_offered();
        let last_summarised = span.last().map(|entry| entry.id.clone());
        let rendered = if recall {
            compaction::render_span_ids(span, max_chars)
        } else {
            compaction::render_span(span, max_chars)
        };
        prompt.push_str(&format!("<conversation>\n{rendered}</conversation>"));
        if let Some(instructions) = instructions.filter(|i| !i.trim().is_empty()) {
            prompt.push_str(&format!("\n\nAdditional instructions: {instructions}"));
        }
        let request = Request {
            system: vec![SystemPart {
                text: if recall {
                    format!("{}{}", compaction::SUMMARY_SYSTEM, compaction::SUMMARY_IDS)
                } else {
                    compaction::SUMMARY_SYSTEM.to_string()
                },
                name: String::new(),
                cache_boundary: false,
            }],
            messages: vec![Message::user(prompt)],
            ..Request::default()
        };
        let (_, mut stream) = self.router.stream(summariser, &request, cancel.clone()).await?;
        let reply = loop {
            match stream.next().await {
                Some(StreamEvent::Done { message }) => break message,
                Some(StreamEvent::Error { error, .. }) => return Err(error),
                Some(_) => {}
                None => {
                    return Err(AiError::Network(
                        "stream ended without a terminal event".to_string(),
                    ));
                }
            }
        };
        if let Some(meta) = &reply.meta {
            self.add_usage(&meta.usage);
        }
        let mut summary = reply.text();
        if summary.trim().is_empty() {
            return Err(AiError::InvalidRequest(
                "the summariser returned no text".to_string(),
            ));
        }
        if let Some(last) = last_summarised.filter(|_| recall) {
            summary = format!("{}{}", summary.trim_end(), compaction::recall_note(&last));
        }

        let id = self
            .sink
            .compaction(&summary, &self.entries[cut].id, tokens_before);
        let kept = self.entries.split_off(cut);
        self.entries.clear();
        self.entries.push(ContextEntry {
            id,
            message: compaction::summary_message(&summary),
            summary: true,
        });
        self.entries.extend(kept);
        self.anchor = None;
        let tokens_after = self.estimate();
        locked(&self.status).context_tokens = tokens_after;
        self.emit(AgentEvent::Compaction {
            tokens_before,
            tokens_after,
        });
        self.hooks.post_compact(&self.info).await;
        Ok(true)
    }

    /// The request for the next turn and the tools that may be called in its response.
    async fn build_request(
        &mut self,
        thinking: ThinkingLevel,
        with_tools: bool,
    ) -> (Request, Vec<Arc<dyn Tool>>) {
        let mut messages: Vec<Message> = self.entries.iter().map(|e| e.message.clone()).collect();
        self.hooks.context(&self.info, &mut messages).await;
        let mut tools = if with_tools {
            self.tools.tools(&self.info)
        } else {
            Vec::new()
        };
        let mut request = Request {
            system: self.system.clone(),
            messages,
            tools: tools.iter().map(|t| tool_spec(t.as_ref())).collect(),
            thinking,
            max_output: None,
            temperature: None,
            cache_key: Some(format!("{}:{}", self.info.session_id, self.info.id)),
            // The tier is the session's, so every agent's own requests carry it; summaries and
            // other side requests stay at the provider's default.
            tier: self.router.tier(),
        };
        self.hooks.before_request(&self.info, &mut request).await;
        // A hook may have withdrawn tools; only what was offered can be called.
        tools.retain(|t| request.tools.iter().any(|spec| spec.name == t.name()));
        (request, tools)
    }

    async fn run_tool(&self, call: Call, tool: Option<Arc<dyn Tool>>, cancel: &CancellationToken) -> Block {
        let (call_id, name, input) = call;
        self.emit(AgentEvent::ToolStart {
            call_id: call_id.clone(),
            name: name.clone(),
            input: input.clone(),
        });
        let started = Instant::now();
        let output = self.execute(&call_id, &name, input, tool, cancel).await;
        let result = Block::ToolResult {
            call_id: call_id.clone(),
            content: output.content.clone(),
            is_error: output.is_error,
        };
        self.emit(AgentEvent::ToolEnd {
            call_id,
            output,
            duration_ms: started.elapsed().as_millis() as u64,
        });
        result
    }

    async fn execute(
        &self,
        call_id: &str,
        name: &str,
        input: Value,
        tool: Option<Arc<dyn Tool>>,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let Some(tool) = tool else {
            return ToolOutput::error(format!("unknown tool `{name}`"));
        };
        if input.get(INVALID_JSON_KEY).is_some() {
            return ToolOutput::error(
                "the tool arguments were not valid JSON; send one JSON object that matches the tool's schema",
            );
        }
        if cancel.is_cancelled() {
            return ToolOutput::error(ABORTED_TEXT);
        }
        let decision = tokio::select! {
            _ = cancel.cancelled() => return ToolOutput::error(ABORTED_TEXT),
            decision = self.gate.before(&self.info, tool.as_ref(), call_id, &input) => decision,
        };
        let input = match decision {
            GateDecision::Deny { reason } => return ToolOutput::error(reason),
            GateDecision::Allow {
                input: Some(replacement),
            } => replacement,
            GateDecision::Allow { input: None } => input,
        };

        let hub = self.hub.clone();
        let update_id = call_id.to_string();
        let ctx = ToolCtx {
            agent: self.info.clone(),
            agent_id: self.info.id.clone(),
            session_id: self.info.session_id.clone(),
            call_id: call_id.to_string(),
            cwd: self.cwd.clone(),
            cancel: cancel.clone(),
            update: UpdateSink::new(move |text| {
                hub.emit(AgentEvent::ToolUpdate {
                    call_id: update_id.clone(),
                    text,
                })
            }),
            services: self.services.clone(),
        };
        // A task of its own, so a panicking tool fails this call and nothing else.
        let runner = tool.clone();
        let mut task = tokio::spawn(async move { runner.call(input, &ctx).await });
        let mut output = tokio::select! {
            joined = &mut task => match joined {
                Ok(output) => output,
                Err(e) if e.is_panic() => ToolOutput::error(format!("tool `{name}` panicked")),
                Err(_) => ToolOutput::error(ABORTED_TEXT),
            },
            _ = cancel.cancelled() => {
                if tokio::time::timeout(ABORT_GRACE, &mut task).await.is_err() {
                    task.abort();
                }
                return ToolOutput::error(ABORTED_TEXT);
            }
        };
        self.gate
            .after(&self.info, tool.as_ref(), call_id, &mut output)
            .await;
        self.limit_output(call_id, &mut output);
        output
    }

    /// Cuts text beyond `max_result_bytes`, storing the full text as an artifact.
    fn limit_output(&self, call_id: &str, output: &mut ToolOutput) {
        let max = self.limits.max_result_bytes;
        let total: usize = output
            .content
            .iter()
            .map(|b| match b {
                Block::Text { text } => text.len(),
                _ => 0,
            })
            .sum();
        if total <= max {
            return;
        }
        let uri = self
            .artifacts
            .as_ref()
            .and_then(|a| a.spill(call_id, &output.text_content()));
        let mut budget = max;
        output.content.retain_mut(|block| match block {
            Block::Text { text } => {
                if budget == 0 {
                    return false;
                }
                if text.len() > budget {
                    let mut end = budget;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                }
                budget -= text.len().min(budget);
                true
            }
            _ => true,
        });
        let mut note = format!("[output truncated: {total} bytes in total]");
        if let Some(uri) = uri {
            note.push_str(&format!(" full output: {uri}"));
        }
        output.content.push(Block::text(note));
    }
}
