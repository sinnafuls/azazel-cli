//! `azazel acp`: azazel as an agent of the Agent Client Protocol, which editors speak to coding
//! agents: JSON-RPC on standard input and output.
//!
//! Each ACP session is an azazel session with its own [`App`], built for the directory the
//! client names, so several sessions in different projects can be open at once. What the lead
//! does is reported as `session/update` notifications; what agents ask of the user becomes
//! `session/request_permission`. azazel reads files and runs commands itself: the client's
//! `fs/*` and `terminal/*` methods are not used.

mod convert;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema as acp;
use agent_client_protocol::{Agent, ByteStreams, Client, Error};
use async_trait::async_trait;
use azazel_agent::AgentEvent;
use azazel_ai::{Block, Role, StopReason, StreamEvent};
use azazel_core::names::Namer;
use azazel_core::{
    ApprovalDecision, ApprovalRequest, AskKind, EntryKind, Event, EventHandler, EventKind, ExtCtx, Extension,
    Level, McpServers, Outcome, PermissionMode, PlanDecision, Question, StartSource, Ui, UiBridge, UiError,
};
use azazel_team::LEAD;
use azazel_tools::{TodoStatus, TodoStore};
use serde_json::Value;
use tokio::sync::watch;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

use crate::app::{Active, App};
use crate::headless::{Ended, Work, build_app, run_work};
use crate::print::{EXIT_FAILED, EXIT_OK};

type Connection = ConnectionTo<Client>;

/// The one way to authenticate: there is nothing to do, azazel uses what it already has.
const AUTH_METHOD: &str = "azazel";
/// How long a permission request waits for the client to be told of the call it is about. The
/// call was announced before permission was asked, so this passes only for calls that are not
/// announced at all (those a script makes for the lead).
const ANNOUNCE_WAIT: Duration = Duration::from_millis(500);

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn internal(why: impl std::fmt::Display) -> Error {
    Error::internal_error().data(Value::String(why.to_string()))
}

/// A tool call of the lead on its way through the permission rules.
struct OpenCall {
    call_id: String,
    tool: String,
    /// As it will be run: after the hooks that may rewrite it.
    input: Value,
}

/// The lead's tool calls that have not ended.
#[derive(Default)]
struct Calls {
    /// Those a permission request may be about.
    checked: Vec<OpenCall>,
    /// Ids of those the client has been told about.
    announced: HashSet<String>,
}

/// What a session's bridge and its prompt runs share.
struct Shared {
    session_id: String,
    connection: Connection,
    calls: Mutex<Calls>,
    /// Wakes permission requests that wait for their call to be announced.
    announced: tokio::sync::Notify,
    /// Numbers the questions that belong to no tool call the client knows.
    questions: AtomicU64,
}

/// Notes each call of the lead as it reaches the permission rules, with its id: a permission
/// request names only the tool and its input.
struct Recorder(Arc<Shared>);

#[async_trait]
impl EventHandler for Recorder {
    async fn handle(&self, event: &mut Event, _: &ExtCtx) -> Outcome {
        if let Event::ToolCall {
            agent,
            call_id,
            tool,
            input,
            ..
        } = event
            && agent.id == LEAD
        {
            locked(&self.0.calls).checked.push(OpenCall {
                call_id: call_id.clone(),
                tool: tool.clone(),
                input: input.clone(),
            });
        }
        Outcome::Continue
    }
}

impl Shared {
    /// The id of the lead's call a permission request is about, once the client knows the call.
    async fn call_of(&self, request: &ApprovalRequest) -> Option<String> {
        if request.agent.id != LEAD {
            return None;
        }
        let call_id = locked(&self.calls)
            .checked
            .iter()
            .rev()
            .find(|call| call.tool == request.tool && call.input == request.input)
            .map(|call| call.call_id.clone())?;
        let told = async {
            loop {
                let announced = self.announced.notified();
                if locked(&self.calls).announced.contains(&call_id) {
                    return;
                }
                announced.await;
            }
        };
        let _ = tokio::time::timeout(ANNOUNCE_WAIT, told).await;
        Some(call_id)
    }

    fn update(&self, update: acp::SessionUpdate) {
        let notification = acp::SessionNotification::new(self.session_id.clone(), update);
        if let Err(e) = self.connection.send_notification(notification) {
            tracing::debug!("a session update was not sent: {e}");
        }
    }

    /// Asks the client's user to pick one of `options` for `call`. `None` when the user
    /// dismissed the question or the client has gone.
    async fn choose(&self, call: acp::ToolCallUpdate, options: Vec<acp::PermissionOption>) -> Option<String> {
        let request = acp::RequestPermissionRequest::new(self.session_id.clone(), call, options);
        match self.connection.send_request(request).block_task().await {
            Ok(response) => match response.outcome {
                acp::RequestPermissionOutcome::Selected(selected) => Some(selected.option_id.to_string()),
                _ => None,
            },
            Err(e) => {
                tracing::debug!("the client did not answer a permission request: {e}");
                None
            }
        }
    }

    /// A question that is not about a tool call the client knows: shown as a call of its own.
    fn question(&self, title: &str, text: Option<&str>) -> acp::ToolCallUpdate {
        let id = format!("question-{}", self.questions.fetch_add(1, Ordering::Relaxed) + 1);
        let mut fields = acp::ToolCallUpdateFields::new()
            .title(title.to_string())
            .kind(acp::ToolKind::Other);
        if let Some(text) = text {
            fields = fields.content(vec![acp::ToolCallContent::from(acp::ContentBlock::Text(
                acp::TextContent::new(text),
            ))]);
        }
        acp::ToolCallUpdate::new(id, fields)
    }

    /// Asks the user to pick one of `options`; the index of the one picked.
    async fn pick(&self, title: &str, text: Option<&str>, options: &[String]) -> Result<usize, UiError> {
        let choices = options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                acp::PermissionOption::new(
                    format!("choice-{index}"),
                    option.clone(),
                    acp::PermissionOptionKind::AllowOnce,
                )
            })
            .collect();
        let picked = self
            .choose(self.question(title, text), choices)
            .await
            .ok_or(UiError::Cancelled)?;
        picked
            .strip_prefix("choice-")
            .and_then(|index| index.parse::<usize>().ok())
            .filter(|index| *index < options.len())
            .ok_or(UiError::Cancelled)
    }
}

/// Agents reach the client's user through this.
struct Bridge {
    shared: Arc<Shared>,
    cwd: PathBuf,
}

#[async_trait]
impl UiBridge for Bridge {
    async fn ask(&self, question: &Question) -> Result<Vec<String>, UiError> {
        let context = question.context.as_deref();
        match question.kind {
            // The protocol has a way to choose, not a way to type an answer.
            AskKind::Text => Err(UiError::NoUser),
            AskKind::Confirm => {
                let options = ["Yes".to_string(), "No".to_string()];
                let said = match self.shared.pick(&question.text, context, &options).await? {
                    0 => "yes",
                    _ => "no",
                };
                Ok(vec![said.to_string()])
            }
            AskKind::Select | AskKind::Multiselect => {
                let index = self
                    .shared
                    .pick(&question.text, context, &question.options)
                    .await?;
                Ok(vec![question.options[index].clone()])
            }
        }
    }

    async fn confirm(&self, prompt: &str) -> Result<bool, UiError> {
        let options = ["Yes".to_string(), "No".to_string()];
        Ok(self.shared.pick(prompt, None, &options).await? == 0)
    }

    async fn select(&self, prompt: &str, options: &[String]) -> Result<usize, UiError> {
        if options.is_empty() {
            return Err(UiError::Cancelled);
        }
        self.shared.pick(prompt, None, options).await
    }

    async fn input(&self, _: &str) -> Result<String, UiError> {
        Err(UiError::NoUser)
    }

    fn notify(&self, level: Level, text: &str) {
        match level {
            Level::Info => tracing::info!("{text}"),
            Level::Warn => tracing::warn!("{text}"),
            Level::Error => tracing::error!("{text}"),
        }
    }

    async fn approval(&self, request: ApprovalRequest) -> Option<ApprovalDecision> {
        // The call the lead announced, when this is about one; a sub-agent's call is shown as a
        // call of its own.
        let known = self.shared.call_of(&request).await;
        let mut title = convert::title(&request.tool, &request.input);
        if request.agent.id != LEAD {
            title = format!("{}: {title}", request.agent.id);
        }
        let mut fields = acp::ToolCallUpdateFields::new()
            .title(title)
            .kind(convert::tool_kind(&request.tool, Some(request.access)))
            .raw_input(request.input.clone());
        if let Some(locations) = convert::locations(&self.cwd, &request.input) {
            fields = fields.locations(locations);
        }
        if let Some(note) = &request.note {
            fields = fields.content(vec![acp::ToolCallContent::from(acp::ContentBlock::Text(
                acp::TextContent::new(note.clone()),
            ))]);
        }
        let call_id = known.unwrap_or_else(|| {
            format!(
                "question-{}",
                self.shared.questions.fetch_add(1, Ordering::Relaxed) + 1
            )
        });
        let mut options = vec![acp::PermissionOption::new(
            "allow_once",
            "Allow",
            acp::PermissionOptionKind::AllowOnce,
        )];
        if let Some(rule) = request.rule.as_ref().filter(|_| !request.destructive) {
            options.push(acp::PermissionOption::new(
                "allow_session",
                "Allow for this session",
                acp::PermissionOptionKind::AllowAlways,
            ));
            options.push(acp::PermissionOption::new(
                "allow_always",
                format!("Always allow {rule}"),
                acp::PermissionOptionKind::AllowAlways,
            ));
        }
        options.push(acp::PermissionOption::new(
            "reject_once",
            "Reject",
            acp::PermissionOptionKind::RejectOnce,
        ));
        let chosen = self
            .shared
            .choose(acp::ToolCallUpdate::new(call_id, fields), options)
            .await;
        Some(match chosen.as_deref() {
            Some("allow_once") => ApprovalDecision::AllowOnce,
            Some("allow_session") => ApprovalDecision::AllowSession,
            Some("allow_always") => ApprovalDecision::Always,
            _ => ApprovalDecision::Deny { feedback: None },
        })
    }

    async fn review_plan(&self, path: &Path, plan: &str) -> Result<PlanDecision, UiError> {
        let options = ["Approve the plan".to_string(), "Keep planning".to_string()];
        let title = format!("Plan: {}", path.display());
        match self.shared.pick(&title, Some(plan), &options).await? {
            0 => Ok(PlanDecision::Approve),
            // The protocol carries no feedback; the agent asks for it.
            _ => Err(UiError::Cancelled),
        }
    }
}

struct Session {
    app: App,
    active: Active,
    shared: Arc<Shared>,
    /// Held while a prompt runs, so that the next one starts after it has ended.
    turn: tokio::sync::Mutex<()>,
    /// Cancels the prompt that runs now, or the one that was just received.
    cancel: Mutex<CancellationToken>,
    /// The name the client was last told.
    title: Mutex<Option<String>>,
    /// Ids of the running calls that change the task list.
    todo_calls: Mutex<HashSet<String>>,
    /// The prompts the client named (`_meta.requestId`) that this process ran or runs, each
    /// with how it ended once it has.
    named: Mutex<HashMap<String, watch::Receiver<Option<Stopped>>>>,
}

/// How a prompt ended: the reason it stopped, or why it failed.
type Stopped = Result<acp::StopReason, String>;

/// What the session knows of a prompt the client named.
enum Seen {
    /// It was never sent before.
    New,
    /// It runs, or ran, in this process.
    Here(watch::Receiver<Option<Stopped>>),
    /// It ran to its end in an earlier process.
    Ended(Stopped),
    /// The process that ran it ended first, and nothing was sent since.
    Interrupted,
}

fn stop_reason(stop: StopReason) -> acp::StopReason {
    match stop {
        StopReason::Stop => acp::StopReason::EndTurn,
        StopReason::Length => acp::StopReason::MaxTokens,
        StopReason::ToolUse => acp::StopReason::MaxTurnRequests,
        StopReason::Aborted => acp::StopReason::Cancelled,
        StopReason::Error => acp::StopReason::Refusal,
    }
}

fn answer(outcome: Stopped) -> Result<acp::PromptResponse, Error> {
    outcome.map(acp::PromptResponse::new).map_err(internal)
}

impl Session {
    /// What became of the prompt the client named `request_id`.
    fn seen(&self, request_id: &str) -> Seen {
        if let Some(ended) = locked(&self.named).get(request_id) {
            return Seen::Here(ended.clone());
        }
        let Some(run) = self.active.session.request_run(request_id) else {
            return Seen::New;
        };
        match run.stop.as_deref() {
            Some("failed") => Seen::Ended(Err("the run this prompt started failed".to_string())),
            Some(stop) => Seen::Ended(Ok(serde_json::from_value::<StopReason>(Value::String(
                stop.to_string(),
            ))
            .map_or(acp::StopReason::EndTurn, stop_reason))),
            None => {
                let last = self.active.session.interrupted_run(None);
                if last.is_some_and(|last| last.run_id == run.run_id) {
                    Seen::Interrupted
                } else {
                    Seen::Ended(Err(
                        "the run this prompt started was cut short, and the session has gone on since"
                            .to_string(),
                    ))
                }
            }
        }
    }

    /// Notes a named prompt that is about to run; how it ends is sent through what is returned.
    fn name(&self, request_id: String) -> watch::Sender<Option<Stopped>> {
        let (ended, seen) = watch::channel(None);
        locked(&self.named).insert(request_id, seen);
        ended
    }

    /// Cancels the prompt that runs and returns the token of the one that comes next.
    fn next_prompt(&self) -> CancellationToken {
        let mut cancel = locked(&self.cancel);
        cancel.cancel();
        *cancel = CancellationToken::new();
        cancel.clone()
    }

    fn modes(&self) -> acp::SessionModeState {
        let modes = PermissionMode::ALL
            .iter()
            .map(|mode| {
                let (name, description) = match mode {
                    PermissionMode::Plan => ("Plan", "Reads and plans; changes nothing"),
                    PermissionMode::Ask => ("Ask", "Asks before it changes files or runs commands"),
                    PermissionMode::AcceptEdits => (
                        "Accept edits",
                        "Edits files without asking; asks before it runs commands",
                    ),
                    PermissionMode::Auto => ("Auto", "Works without asking"),
                };
                acp::SessionMode::new(mode.to_string(), name).description(description.to_string())
            })
            .collect();
        acp::SessionModeState::new(self.app.permissions.mode().to_string(), modes)
    }

    /// Tells the client what the lead does. The work of sub-agents shows as the lead's `task`
    /// call, not event by event.
    fn on_event(&self, agent: &str, event: &AgentEvent) {
        if agent != LEAD {
            return;
        }
        let text =
            |delta: &str| acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(delta)));
        match event {
            AgentEvent::Stream(StreamEvent::TextDelta { delta, .. }) => {
                self.shared
                    .update(acp::SessionUpdate::AgentMessageChunk(text(delta)));
            }
            AgentEvent::Stream(StreamEvent::ThinkingDelta { delta, .. }) => {
                self.shared
                    .update(acp::SessionUpdate::AgentThoughtChunk(text(delta)));
            }
            AgentEvent::ToolStart { call_id, name, input } => {
                let call = convert::tool_call(
                    &self.app.paths.cwd,
                    call_id,
                    name,
                    input,
                    acp::ToolCallStatus::InProgress,
                );
                self.shared.update(acp::SessionUpdate::ToolCall(call));
                locked(&self.shared.calls).announced.insert(call_id.clone());
                self.shared.announced.notify_waiters();
                if name == "todo" {
                    locked(&self.todo_calls).insert(call_id.clone());
                }
            }
            AgentEvent::ToolEnd { call_id, output, .. } => {
                {
                    let mut calls = locked(&self.shared.calls);
                    calls.checked.retain(|call| call.call_id != *call_id);
                    calls.announced.remove(call_id);
                }
                let update = convert::tool_result(&self.app.paths.cwd, call_id, output);
                self.shared.update(acp::SessionUpdate::ToolCallUpdate(update));
                if locked(&self.todo_calls).remove(call_id) {
                    self.plan();
                }
            }
            _ => {}
        }
    }

    /// The lead's task list, as the client's plan.
    fn plan(&self) {
        let Some(todos) = self.app.registry.services.get::<TodoStore>() else {
            return;
        };
        let entries = todos
            .get(LEAD)
            .into_iter()
            .map(|item| {
                let status = match item.status {
                    TodoStatus::Pending => acp::PlanEntryStatus::Pending,
                    TodoStatus::InProgress => acp::PlanEntryStatus::InProgress,
                    TodoStatus::Done => acp::PlanEntryStatus::Completed,
                };
                acp::PlanEntry::new(item.text, acp::PlanEntryPriority::Medium, status)
            })
            .collect();
        self.shared
            .update(acp::SessionUpdate::Plan(acp::Plan::new(entries)));
    }

    /// Tells the client the session's name when it has one the client does not know yet.
    fn announce_name(&self) {
        let name = self.active.session.name();
        let mut told = locked(&self.title);
        if name.is_some() && *told != name {
            *told = name.clone();
            self.shared.update(acp::SessionUpdate::SessionInfoUpdate(
                acp::SessionInfoUpdate::new().title(name),
            ));
        }
    }

    /// Sends the stored conversation, the way it would have arrived while it happened.
    fn replay(&self) {
        let chunk = |block: acp::ContentBlock| acp::ContentChunk::new(block);
        let text = |text: &str| acp::ContentBlock::Text(acp::TextContent::new(text));
        for entry in self.active.session.branch(None) {
            let EntryKind::Message { message } = entry.kind else {
                continue;
            };
            for block in message.content {
                let update = match (message.role, block) {
                    (Role::User, Block::Text { text: said }) => {
                        acp::SessionUpdate::UserMessageChunk(chunk(text(&said)))
                    }
                    (Role::User, Block::Image { mime, data }) => acp::SessionUpdate::UserMessageChunk(chunk(
                        acp::ContentBlock::Image(acp::ImageContent::new(data, mime)),
                    )),
                    (Role::Assistant, Block::Text { text: said }) => {
                        acp::SessionUpdate::AgentMessageChunk(chunk(text(&said)))
                    }
                    (Role::Assistant, Block::Thinking { text: thought, .. }) => {
                        acp::SessionUpdate::AgentThoughtChunk(chunk(text(&thought)))
                    }
                    (Role::Assistant, Block::ToolCall { id, name, input, .. }) => {
                        acp::SessionUpdate::ToolCall(convert::tool_call(
                            &self.app.paths.cwd,
                            &id,
                            &name,
                            &input,
                            acp::ToolCallStatus::Pending,
                        ))
                    }
                    (
                        Role::Tool,
                        Block::ToolResult {
                            call_id,
                            content,
                            is_error,
                        },
                    ) => {
                        let output = azazel_agent::ToolOutput {
                            content,
                            is_error,
                            details: None,
                        };
                        acp::SessionUpdate::ToolCallUpdate(convert::tool_result(
                            &self.app.paths.cwd,
                            &call_id,
                            &output,
                        ))
                    }
                    _ => continue,
                };
                self.shared.update(update);
            }
        }
    }
}

struct Server {
    extensions: Vec<Box<dyn Extension>>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl Server {
    fn session(&self, id: &acp::SessionId) -> Result<Arc<Session>, Error> {
        locked(&self.sessions)
            .get(&id.to_string())
            .cloned()
            .ok_or_else(|| Error::invalid_params().data(Value::String(format!("no session `{id}` is open"))))
    }

    /// Ends a session: its prompt is cancelled, then its agents and servers are stopped.
    async fn close(session: Arc<Session>) {
        locked(&session.cancel).cancel();
        let _turn = session.turn.lock().await;
        session.app.finish(&session.active).await;
    }

    /// Builds an azazel session for `cwd`: a new one, or the stored session `resume`.
    async fn open(
        &self,
        connection: &Connection,
        cwd: PathBuf,
        servers: Vec<acp::McpServer>,
        resume: Option<String>,
    ) -> Result<Arc<Session>, Error> {
        // A session that is loaded again while it is open is closed first: two agents must not
        // write one session file.
        let open = resume.as_ref().and_then(|id| locked(&self.sessions).remove(id));
        if let Some(open) = open {
            Server::close(open).await;
        }
        let built = build_app(&self.extensions, |cli| {
            cli.cwd = Some(cwd);
            cli.resume_latest = false;
            cli.fork = None;
            cli.session = resume;
        })
        .await;
        let (app, cli) = built.map_err(|e| internal(format!("{e:#}")))?;
        // The servers the client brings belong to this session only.
        if let Some(mcp) = app.registry.services.get::<McpServers>() {
            for server in servers {
                let Some((name, entry)) = convert::mcp_entry(server) else {
                    continue;
                };
                if let Err(why) = mcp.0.add_server(&name, entry) {
                    tracing::warn!("the client's MCP server `{name}` is not used: {why}");
                }
            }
        }
        let how = app
            .requested_session(&cli)
            .map_err(|e| internal(format!("{e:#}")))?;
        let active = app
            .open(how, app.chain.clone(), app.thinking)
            .await
            .map_err(|e| internal(format!("{e:#}")))?;
        let shared = Arc::new(Shared {
            session_id: active.session.id(),
            connection: connection.clone(),
            calls: Mutex::default(),
            announced: tokio::sync::Notify::new(),
            questions: AtomicU64::new(0),
        });
        // Registered last, so it sees a call as the hooks before it left it.
        app.registry
            .bus
            .on(EventKind::ToolCall, "acp", Arc::new(Recorder(shared.clone())));
        app.registry.services.insert(Arc::new(Ui(Arc::new(Bridge {
            shared: shared.clone(),
            cwd: app.paths.cwd.clone(),
        }))));
        // The mode also changes without the client asking: an approved plan leaves plan mode.
        let watching = shared.clone();
        app.permissions.on_mode_change(Arc::new(move |mode| {
            watching.update(acp::SessionUpdate::CurrentModeUpdate(
                acp::CurrentModeUpdate::new(mode.to_string()),
            ));
        }));
        app.start(if active.resumed {
            StartSource::Resume
        } else {
            StartSource::Startup
        })
        .await;
        let session = Arc::new(Session {
            title: Mutex::new(active.session.name()),
            app,
            active,
            shared,
            turn: tokio::sync::Mutex::new(()),
            cancel: Mutex::default(),
            todo_calls: Mutex::default(),
            named: Mutex::default(),
        });
        locked(&self.sessions).insert(session.shared.session_id.clone(), session.clone());
        Ok(session)
    }

    /// Runs one prompt of a session to its end.
    async fn prompt(&self, session: Arc<Session>, work: Work, cancel: CancellationToken) -> Stopped {
        let _turn = session.turn.lock().await;
        if cancel.is_cancelled() {
            return Ok(acp::StopReason::Cancelled);
        }
        let ended = run_work(
            &session.app,
            &session.active,
            work,
            &cancel,
            &mut |agent, event| session.on_event(agent, event),
        )
        .await;
        // Calls that never ended are over all the same.
        *locked(&session.shared.calls) = Calls::default();
        locked(&session.todo_calls).clear();
        // The name is chosen in the background after the first reply; the client hears of it
        // when it is there, without the prompt waiting for it.
        let naming = session.clone();
        tokio::spawn(async move {
            if let Some(namer) = naming.app.registry.services.get::<Namer>() {
                namer.settle().await;
            }
            naming.announce_name();
        });
        match ended {
            Ok(Ended::Idle { stop, .. }) => Ok(stop_reason(stop)),
            Ok(Ended::Failed(error)) => Err(error),
            Err(rejected) => Err(format!("the prompt was rejected: {rejected}")),
        }
    }

    /// Ends every session, when the client has gone.
    async fn finish(&self) {
        let sessions: Vec<Arc<Session>> = locked(&self.sessions)
            .drain()
            .map(|(_, session)| session)
            .collect();
        for session in sessions {
            Server::close(session).await;
        }
    }
}

/// Serves the protocol on standard input and output until the client closes the connection.
pub async fn run(extensions: Vec<Box<dyn Extension>>) -> u8 {
    let server = Arc::new(Server {
        extensions,
        sessions: Mutex::default(),
    });
    let transport = ByteStreams::new(tokio::io::stdout().compat_write(), tokio::io::stdin().compat());
    let (new, load, mode, prompt, cancel) = (
        server.clone(),
        server.clone(),
        server.clone(),
        server.clone(),
        server.clone(),
    );
    let served = Agent
        .v1()
        .name("azazel")
        .on_receive_request(
            async move |_: acp::InitializeRequest, responder, _connection| {
                let capabilities = acp::AgentCapabilities::new()
                    .load_session(true)
                    .prompt_capabilities(acp::PromptCapabilities::new().image(true).embedded_context(true))
                    .mcp_capabilities(acp::McpCapabilities::new().http(true).sse(true));
                let auth = acp::AuthMethodAgent::new(AUTH_METHOD, "azazel's own credentials").description(
                    "Use the logins and keys already in ~/.azazel (set up with `azazel login` or `azazel init`)"
                        .to_string(),
                );
                responder.respond(
                    acp::InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(capabilities)
                        .auth_methods(vec![acp::AuthMethod::Agent(auth)])
                        .agent_info(acp::Implementation::new("azazel", env!("CARGO_PKG_VERSION")).title("Azazel".to_string())),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_: acp::AuthenticateRequest, responder, _connection| {
                responder.respond(acp::AuthenticateResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::NewSessionRequest, responder, connection: Connection| {
                // Building a session takes a while; the connection goes on meanwhile.
                let server = new.clone();
                tokio::spawn(async move {
                    let opened = server
                        .open(&connection, request.cwd, request.mcp_servers, None)
                        .await
                        .map(|session| {
                            acp::NewSessionResponse::new(session.shared.session_id.clone()).modes(session.modes())
                        });
                    let _ = responder.respond_with_result(opened);
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::LoadSessionRequest, responder, connection: Connection| {
                let server = load.clone();
                tokio::spawn(async move {
                    let wanted = request.session_id.to_string();
                    let opened = server
                        .open(&connection, request.cwd, request.mcp_servers, Some(wanted))
                        .await
                        .map(|session| {
                            session.replay();
                            acp::LoadSessionResponse::new().modes(session.modes())
                        });
                    let _ = responder.respond_with_result(opened);
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::SetSessionModeRequest, responder, _connection| {
                let changed = mode.session(&request.session_id).and_then(|session| {
                    let wanted: PermissionMode = request
                        .mode_id
                        .to_string()
                        .parse()
                        .map_err(|e: String| Error::invalid_params().data(Value::String(e)))?;
                    session.app.permissions.set_mode(wanted);
                    Ok(acp::SetSessionModeResponse::new())
                });
                responder.respond_with_result(changed)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::PromptRequest, responder, _connection| {
                let session = match prompt.session(&request.session_id) {
                    Ok(session) => session,
                    Err(e) => return responder.respond_with_error(e),
                };
                // A prompt the client named (`_meta.requestId`) and sent before starts nothing:
                // it is answered as the first one is, or was.
                let request_id = request
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get("requestId"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let seen = request_id.as_deref().map_or(Seen::New, |id| session.seen(id));
                let work = match seen {
                    Seen::Here(mut ended) => {
                        tokio::spawn(async move {
                            let outcome = match ended.wait_for(Option::is_some).await {
                                Ok(outcome) => outcome.clone(),
                                Err(_) => None,
                            };
                            let outcome =
                                outcome.unwrap_or_else(|| Err("the prompt ended without an answer".to_string()));
                            let _ = responder.respond_with_result(answer(outcome));
                        });
                        return Ok(());
                    }
                    Seen::Ended(outcome) => return responder.respond_with_result(answer(outcome)),
                    // The process that ran it ended in the middle: the run is gone on with.
                    Seen::Interrupted => Work::ContinueRun,
                    Seen::New => {
                        let (text, images) = convert::user_input(request.prompt);
                        Work::Prompt {
                            text,
                            images,
                            request_id: request_id.clone(),
                        }
                    }
                };
                // Decided here, in the order the messages arrived: this prompt cancels the one
                // before it, and a `session/cancel` that follows cancels this one.
                let cancel = session.next_prompt();
                let ended = request_id.map(|request_id| session.name(request_id));
                let server = prompt.clone();
                tokio::spawn(async move {
                    let outcome = server.prompt(session, work, cancel).await;
                    if let Some(ended) = ended {
                        let _ = ended.send(Some(outcome.clone()));
                    }
                    let _ = responder.respond_with_result(answer(outcome));
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: acp::CancelNotification, _connection| {
                if let Ok(session) = cancel.session(&notification.session_id) {
                    locked(&session.cancel).cancel();
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(transport)
        .await;
    server.finish().await;
    match served {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("azazel: the ACP connection failed: {e}");
            EXIT_FAILED
        }
    }
}
