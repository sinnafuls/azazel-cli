//! `/remote`: the session of this terminal, shared with browsers.
//!
//! The terminal stays the master. The agents, the tools and the permissions are this process's;
//! a browser is a client that reads the frames in [`Feed`] and sends commands the terminal's loop
//! runs as if they had been typed. Requests to the user (an approval, a question) go to the
//! terminal and to the browsers at once and the first answer wins ([`SharedBridge`]).

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{AgentHandle, Services};
use azazel_ai::Block;
use azazel_core::config::Config;
use azazel_core::{
    ApprovalDecision, ApprovalRequest, Level, PlanDecision, Question, SessionStore, Ui, UiBridge, UiError,
};
use azazel_protocol::{
    Attached, Command, Decision, Frame, MessageRow, Request, Response, Snapshot, UiRequest,
};
use azazel_remote::{
    Access, Conn, Folders, Kind, Link, NewSession, Options, SavedSession, Server, SessionInfo, Sessions,
    StartError,
};
use azazel_team::Team;
use azazel_tui::Glyphs;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::background::BackgroundSessions;
use super::start::{Ask, Rules};
use super::tunnel::{self, Tunnel};
use crate::app::App;
use crate::print::event_json;
use crate::rpc::browser::{self as live, Browser};
use crate::rpc::session::{agent_messages_of, messages_of, roster_of, state_of, tree_of};
use crate::rpc::{
    self, Fenced, Pending, Waiting, answered, approval_frame, asked, level_name, locked, team_frame,
};

/// The id of the one session a terminal offers.
pub(crate) const ID: &str = "terminal";
/// Frames kept for a browser that reconnects.
const RING: usize = 4096;
/// Frames that may wait for one browser; the one that lets this many pile up is dropped.
const QUEUE: usize = 8192;
/// Messages of the conversation an attaching browser is told.
const TAIL: usize = 20;

// ---------------------------------------------------------------------------------- the feed

struct Sub {
    conn: u64,
    tx: mpsc::Sender<String>,
}

#[derive(Default)]
struct Inner {
    live: crate::host::Live,
    /// The `at` the next frame gets.
    next: u64,
    lines: VecDeque<(u64, String)>,
    /// Requests to the user that nobody has answered: `(at, request_id, line)`.
    asks: Vec<(u64, String, String)>,
    subs: Vec<Sub>,
}

/// What a browser that attaches is given.
pub(crate) struct Attachment {
    pub snapshot: Snapshot,
    pub cursor: u64,
    /// The frames after `since`; empty when there was no `since` or it is unknown.
    pub replay: Vec<String>,
    /// `since` named a frame the ring no longer holds.
    pub gap: bool,
    /// The requests that still wait, for a browser that has not seen them.
    pub asks: Vec<(String, String)>,
    pub live: mpsc::Receiver<String>,
    pub clients: u32,
}

/// The frames of the session, each stamped with its `at`, kept for a browser that reconnects and
/// handed to the ones that are attached.
#[derive(Default)]
pub(crate) struct Feed {
    inner: Mutex<Inner>,
    busy: AtomicBool,
}

impl Feed {
    pub(crate) fn busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::Relaxed);
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stamps and keeps `frame`, and sends it to every browser but `except`.
    pub(crate) fn publish(&self, mut frame: Value, except: Option<u64>) {
        let mut inner = self.guard();
        let at = inner.next;
        inner.next += 1;
        frame["at"] = json!(at);
        let kind = frame["type"].as_str().unwrap_or_default().to_string();
        inner.live.on_frame(&kind, &frame);
        let request_id = frame["request_id"].as_str().map(str::to_string);
        let line = frame.to_string();
        match (kind.as_str(), request_id) {
            ("approval_request", Some(id)) => inner.asks.push((at, id, line.clone())),
            ("ui_request", Some(id)) if frame["kind"] != "notify" => inner.asks.push((at, id, line.clone())),
            ("request_closed", Some(id)) => inner.asks.retain(|(_, known, _)| *known != id),
            _ => {}
        }
        inner.lines.push_back((at, line.clone()));
        if inner.lines.len() > RING {
            inner.lines.pop_front();
        }
        // A browser that cannot keep up is let go; it reconnects and resumes with `since`.
        inner.subs.retain(|sub| {
            if Some(sub.conn) == except {
                return !sub.tx.is_closed();
            }
            sub.tx.try_send(line.clone()).is_ok()
        });
    }

    /// [`Feed::publish`] for a frame that is already text.
    pub(crate) fn publish_text(&self, line: &str, except: Option<u64>) {
        if let Ok(frame) = serde_json::from_str::<Value>(line) {
            self.publish(frame, except);
        }
    }

    /// What the user of the terminal (or a browser, `from`) sent the lead.
    pub(crate) fn user_input(&self, kind: &str, text: &str, from: Option<u64>) {
        let frame = Frame::UserInput {
            kind: kind.to_string(),
            text: text.to_string(),
            agent: None,
        };
        if let Ok(frame) = serde_json::to_value(frame) {
            self.publish(frame, from);
        }
    }

    /// The frames after `at`, or `None` when the ring no longer reaches back to it.
    fn since(inner: &Inner, at: u64) -> Option<Vec<String>> {
        let first = inner.lines.front().map_or(inner.next, |(first, _)| *first);
        if at >= inner.next || at + 1 < first {
            return None;
        }
        Some(
            inner
                .lines
                .iter()
                .filter(|(stamp, _)| *stamp > at)
                .map(|(_, line)| line.clone())
                .collect(),
        )
    }

    /// Makes `conn` a subscriber and says what it has missed, in one step so that no frame falls
    /// between the replay and the live ones.
    pub(crate) fn attach(&self, conn: u64, since: Option<u64>) -> Attachment {
        let mut inner = self.guard();
        let (tx, live) = mpsc::channel(QUEUE);
        inner.subs.retain(|sub| !sub.tx.is_closed() && sub.conn != conn);
        inner.subs.push(Sub { conn, tx });
        let resumed = since.and_then(|at| Self::since(&inner, at));
        let gap = since.is_some() && resumed.is_none();
        let asks = if resumed.is_some() {
            Vec::new()
        } else {
            inner
                .asks
                .iter()
                .map(|(_, id, line)| (id.clone(), line.clone()))
                .collect()
        };
        Attachment {
            snapshot: Snapshot {
                busy: inner.live.busy,
                last_error: inner.live.last_error.clone(),
                streaming: inner.live.streaming.clone(),
                tools: inner
                    .live
                    .tools
                    .iter()
                    .map(|(tool, since)| azazel_protocol::RunningTool {
                        elapsed_ms: since.elapsed().as_millis() as u64,
                        ..tool.clone()
                    })
                    .collect(),
                queue: inner.live.queue.iter().cloned().collect(),
                ..Snapshot::default()
            },
            cursor: inner.next,
            replay: resumed.unwrap_or_default(),
            gap,
            asks,
            live,
            clients: inner.subs.len() as u32,
        }
    }

    /// How many browsers are attached.
    pub(crate) fn clients(&self) -> usize {
        let mut inner = self.guard();
        inner.subs.retain(|sub| !sub.tx.is_closed());
        inner.subs.len()
    }
}

/// Feeds `feed` with what the lead and the team do, and releases the request lines of the bridge
/// in their place among the lead's events: a request never reaches a browser before the
/// `tool_start` that caused it.
pub(crate) fn pump(
    lead: AgentHandle,
    team: Option<Arc<Team>>,
    mut requests: mpsc::UnboundedReceiver<Fenced>,
    feed: Arc<Feed>,
) -> JoinHandle<()> {
    let (mut events, emitted) = lead.subscribe_counted();
    let mut team_events = team.as_ref().map(|team| team.subscribe());
    tokio::spawn(async move {
        let mut pending = Pending {
            written: emitted,
            lines: VecDeque::new(),
        };
        let team = team.as_deref();
        let roster = || {
            serde_json::to_value(Frame::Roster {
                members: roster_of(team),
            })
            .unwrap_or(Value::Null)
        };
        loop {
            tokio::select! {
                biased;
                event = events.recv() => {
                    let Some(event) = event else { break };
                    while let Some(earlier) = team_events.as_mut().map(|events| events.try_recv()) {
                        match earlier {
                            Ok(earlier) => feed.publish(team_frame(team, earlier), None),
                            Err(TryRecvError::Lagged(_)) => feed.publish(roster(), None),
                            Err(TryRecvError::Closed) => team_events = None,
                            Err(TryRecvError::Empty) => break,
                        }
                    }
                    while let Ok(fenced) = requests.try_recv() {
                        pending.lines.push_back(fenced);
                    }
                    pending.flush_to(|line| feed.publish_text(&line, None));
                    if matches!(event, azazel_agent::AgentEvent::AgentStart) {
                        feed.set_busy(true);
                    } else if event.is_terminal() {
                        feed.set_busy(false);
                    }
                    feed.publish(event_json(&lead.id, &event), None);
                    pending.written += 1;
                    pending.flush_to(|line| feed.publish_text(&line, None));
                }
                Some(event) = async { Some(team_events.as_mut()?.recv().await) } => match event {
                    Ok(event) => feed.publish(team_frame(team, event), None),
                    Err(RecvError::Lagged(_)) => feed.publish(roster(), None),
                    Err(RecvError::Closed) => team_events = None,
                },
                Some(fenced) = requests.recv() => {
                    pending.lines.push_back(fenced);
                    pending.flush_to(|line| feed.publish_text(&line, None));
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------- the shared bridge

/// Request lines on their way to the feed, each with the number of lead events that precede it.
#[derive(Clone)]
pub(crate) struct Requests {
    lines: mpsc::UnboundedSender<Fenced>,
    emitted: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Requests {
    pub(crate) fn new(
        lines: mpsc::UnboundedSender<Fenced>,
        emitted: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        Requests { lines, emitted }
    }

    fn line(&self, frame: &Frame) {
        if let Ok(text) = serde_json::to_string(frame) {
            let _ = self.lines.send(((self.emitted)(), text));
        }
    }
}

/// What agents ask the user through while the session is shared: the terminal's own bridge and
/// the browsers both get each request, and whoever answers first decides.
pub(crate) struct SharedBridge {
    tui: Arc<dyn UiBridge>,
    out: Requests,
    waiting: Arc<Waiting>,
}

impl SharedBridge {
    pub(crate) fn new(tui: Arc<dyn UiBridge>, out: Requests, waiting: Arc<Waiting>) -> Self {
        SharedBridge { tui, out, waiting }
    }

    fn closed(&self, request_id: &str) {
        self.out.line(&Frame::RequestClosed {
            request_id: request_id.to_string(),
        });
    }

    /// Asks the browsers `request` and the terminal `tui`. A browser's `value` is turned into the
    /// answer by `web`; `None` from it, or `null`, is a dismissal.
    async fn race<T>(
        &self,
        request: UiRequest,
        tui: impl Future<Output = Result<T, UiError>>,
        web: impl FnOnce(Value) -> Option<T>,
    ) -> Result<T, UiError> {
        let id = self.waiting.id();
        let (reply, mut answer) = oneshot::channel();
        locked(&self.waiting.answers).insert(id.clone(), reply);
        self.out.line(&Frame::UiRequest {
            request_id: id.clone(),
            request,
        });
        let mut tui = std::pin::pin!(tui);
        let outcome = tokio::select! {
            got = &mut answer => match got {
                Ok(value) => Ok(value).and_then(|value: Value| {
                    Some(value).filter(|value| !value.is_null()).and_then(web).ok_or(UiError::Cancelled)
                }),
                // Sharing ended: the terminal goes on alone.
                Err(_) => tui.await,
            },
            done = &mut tui => {
                locked(&self.waiting.answers).remove(&id);
                done
            }
        };
        self.closed(&id);
        outcome
    }
}

#[async_trait]
impl UiBridge for SharedBridge {
    async fn ask(&self, question: &Question) -> Result<Vec<String>, UiError> {
        let request = UiRequest::Ask(asked(question));
        self.race(request, self.tui.ask(question), |value| {
            answered(question, Some(value))
        })
        .await
    }

    async fn ask_form(&self, questions: &[Question]) -> Result<Vec<Vec<String>>, UiError> {
        let request = UiRequest::Form {
            questions: questions.iter().map(asked).collect(),
        };
        self.race(request, self.tui.ask_form(questions), |value| {
            let Value::Array(mut values) = value else {
                return None;
            };
            values.resize(questions.len(), Value::Null);
            Some(
                questions
                    .iter()
                    .zip(values)
                    .map(|(question, value)| answered(question, Some(value)).unwrap_or_default())
                    .collect(),
            )
        })
        .await
    }

    async fn confirm(&self, prompt: &str) -> Result<bool, UiError> {
        let request = UiRequest::Confirm {
            prompt: prompt.to_string(),
        };
        self.race(request, self.tui.confirm(prompt), |value| value.as_bool())
            .await
    }

    async fn select(&self, prompt: &str, options: &[String]) -> Result<usize, UiError> {
        let request = UiRequest::Select {
            prompt: prompt.to_string(),
            options: options.to_vec(),
        };
        self.race(request, self.tui.select(prompt, options), |value| {
            let chosen = match value {
                Value::Number(index) => index.as_u64().and_then(|index| usize::try_from(index).ok()),
                Value::String(option) => options.iter().position(|known| *known == option),
                _ => None,
            };
            chosen.filter(|index| *index < options.len())
        })
        .await
    }

    async fn input(&self, prompt: &str) -> Result<String, UiError> {
        let request = UiRequest::Input {
            prompt: prompt.to_string(),
            secret: false,
        };
        self.race(request, self.tui.input(prompt), |value| {
            value.as_str().map(str::to_string)
        })
        .await
    }

    async fn secret(&self, prompt: &str) -> Result<String, UiError> {
        // A secret is typed at the terminal: it is never asked of a browser.
        self.tui.secret(prompt).await
    }

    fn notify(&self, level: Level, text: &str) {
        self.out.line(&Frame::UiRequest {
            request_id: self.waiting.id(),
            request: UiRequest::Notify {
                level: level_name(level).to_string(),
                text: text.to_string(),
            },
        });
        self.tui.notify(level, text);
    }

    async fn approval(&self, request: ApprovalRequest) -> Option<ApprovalDecision> {
        let id = self.waiting.id();
        let (reply, mut answer) = oneshot::channel();
        locked(&self.waiting.approvals).insert(id.clone(), reply);
        self.out.line(&approval_frame(id.clone(), request.clone()));
        let mut tui = std::pin::pin!(self.tui.approval(request));
        let decision = tokio::select! {
            got = &mut answer => match got {
                Ok(decision) => Some(decision),
                Err(_) => tui.await,
            },
            decision = &mut tui => {
                locked(&self.waiting.approvals).remove(&id);
                decision
            }
        };
        self.closed(&id);
        decision
    }

    async fn review_plan(&self, path: &std::path::Path, plan: &str) -> Result<PlanDecision, UiError> {
        let request = UiRequest::Plan {
            path: path.display().to_string(),
            plan: plan.to_string(),
        };
        self.race(request, self.tui.review_plan(path, plan), |value| match value {
            Value::Bool(true) => Some(PlanDecision::Approve),
            Value::String(feedback) => Some(PlanDecision::Revise(feedback)),
            _ => None,
        })
        .await
    }
}

// ------------------------------------------------------------------------------ the sessions

/// What a browser asked the terminal to do, with the way back to its answer.
pub(crate) struct Web {
    pub what: Asked,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

pub(crate) enum Asked {
    Prompt { text: String, images: Vec<Block> },
    Steer { text: String, images: Vec<Block> },
    FollowUp { text: String },
    Abort,
    SetMode(String),
    SetModel(Vec<String>),
    SetThinking(String),
}

/// What the browsers read of the session.
pub(crate) trait Source: Send + Sync + 'static {
    fn state(&self, busy: bool) -> Result<Value, String>;
    fn messages(&self, leaf: Option<&str>, before: Option<&str>, limit: Option<u32>)
    -> Result<Value, String>;
    fn agent_messages(&self, agent: &str, before: Option<&str>, limit: Option<u32>) -> Result<Value, String>;
    fn tree(&self) -> Result<Value, String>;
    fn tail(&self) -> Vec<MessageRow>;
    fn about(&self) -> About;
    /// What a browser lists: models, roles, commands, files and usage windows.
    fn listing(&self, command: &Command) -> Option<Result<Value, String>>;
}

pub(crate) struct About {
    pub session_id: String,
    pub session_file: String,
    pub name: Option<String>,
    pub cwd: String,
    pub mode: String,
    pub cost_usd: Option<f64>,
}

/// The session of this process.
pub(crate) struct LocalSource {
    pub app: Arc<App>,
    pub lead: AgentHandle,
    pub session: Arc<SessionStore>,
    pub team: Option<Arc<Team>>,
    /// The model lists providers gave while shared.
    pub listed: crate::models::Listed,
}

impl Source for LocalSource {
    fn state(&self, busy: bool) -> Result<Value, String> {
        let state = state_of(&self.app, &self.lead, &self.session, self.team.as_deref(), busy);
        serde_json::to_value(state).map_err(|e| e.to_string())
    }

    fn messages(
        &self,
        leaf: Option<&str>,
        before: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Value, String> {
        messages_of(&self.session, leaf, before, limit)
    }

    fn agent_messages(&self, agent: &str, before: Option<&str>, limit: Option<u32>) -> Result<Value, String> {
        agent_messages_of(self.team.as_deref(), agent, before, limit)
    }

    fn tree(&self) -> Result<Value, String> {
        serde_json::to_value(tree_of(&self.session)).map_err(|e| e.to_string())
    }

    fn tail(&self) -> Vec<MessageRow> {
        let mut rows: Vec<MessageRow> = self
            .session
            .branch(None)
            .into_iter()
            .rev()
            .filter_map(|entry| match entry.kind {
                azazel_core::EntryKind::Message { message } => Some(MessageRow {
                    entry_id: entry.id,
                    timestamp: entry.timestamp,
                    message: serde_json::to_value(message).ok()?,
                }),
                _ => None,
            })
            .take(TAIL)
            .collect();
        rows.reverse();
        rows
    }

    fn listing(&self, command: &Command) -> Option<Result<Value, String>> {
        let to_value = |v: Result<Value, serde_json::Error>| v.map_err(|e| e.to_string());
        Some(match command {
            Command::ListModels { all } => azazel_ai::providers::presets::resolve(&self.app.config.providers)
                .and_then(|providers| {
                    to_value(serde_json::to_value(crate::models::rows(
                        &self.app.router,
                        &self.listed,
                        &providers,
                        &self.app.credentials,
                        self.app.provider.as_deref(),
                        *all,
                    )))
                }),
            Command::ListProviders => azazel_ai::providers::presets::resolve(&self.app.config.providers)
                .and_then(|providers| {
                    to_value(serde_json::to_value(crate::models::provider_rows(
                        &self.app.router,
                        &self.listed,
                        &providers,
                        &self.app.credentials,
                        self.app.provider.as_deref(),
                    )))
                }),
            Command::ListRoles => {
                let place = crate::roles::Place::of(&self.app, Some((&self.lead, Some(&*self.session))));
                to_value(serde_json::to_value(crate::roles::list(&place)))
            }
            Command::ListCommands => {
                to_value(serde_json::to_value(crate::rpc::session::commands_of(&self.app)))
            }
            Command::ListFiles => to_value(serde_json::to_value(crate::tui::project_files(
                &self.app.paths.cwd,
                self.app.config.tui.complete_ignored,
            ))),
            Command::GetUsage => to_value(serde_json::to_value(self.app.router.limits().snapshot())),
            Command::GetSettings => crate::rpc::admin::get_settings(&self.app)
                .and_then(|list| to_value(serde_json::to_value(list))),
            Command::GetTheme { light } => crate::rpc::admin::get_theme(&self.app, *light)
                .and_then(|theme| to_value(serde_json::to_value(theme))),
            Command::GetContext => to_value(serde_json::to_value(crate::rpc::admin::get_context(
                &self.lead,
                &self.session,
            ))),
            Command::GetCost => to_value(serde_json::to_value(crate::rpc::admin::get_cost(
                &self.app,
                &self.lead,
                &self.session,
            ))),
            _ => return None,
        })
    }

    fn about(&self) -> About {
        About {
            session_id: self.session.id(),
            session_file: self.session.path().display().to_string(),
            name: self.session.name(),
            cwd: self.app.paths.cwd.display().to_string(),
            mode: self.app.permissions.mode().to_string(),
            cost_usd: self.lead.status().usage.cost_usd,
        }
    }
}

/// The one session a terminal offers a server.
pub(crate) struct TerminalSessions {
    source: Arc<dyn Source>,
    feed: Arc<Feed>,
    waiting: Arc<Waiting>,
    web: mpsc::UnboundedSender<Web>,
    redact: bool,
    started: String,
    next_conn: AtomicU64,
    /// The agents' browser, which the browsers of the server may watch and take over. Its pictures
    /// go to the one that asked, not through the [`Feed`]: they are never kept for a later one.
    browser: Option<Arc<Browser>>,
    /// The background sessions of this machine, which a browser may start, list and open beside
    /// the terminal's own session, under the rules `azazel remote` has for them.
    background: Option<Arc<BackgroundSessions>>,
}

impl TerminalSessions {
    pub(crate) fn new(
        source: Arc<dyn Source>,
        feed: Arc<Feed>,
        waiting: Arc<Waiting>,
        web: mpsc::UnboundedSender<Web>,
        redact: bool,
        browser: Option<Arc<Browser>>,
        background: Option<Arc<BackgroundSessions>>,
    ) -> Arc<TerminalSessions> {
        Arc::new(TerminalSessions {
            source,
            feed,
            waiting,
            web,
            redact,
            started: jiff::Timestamp::now().to_string(),
            next_conn: AtomicU64::new(1),
            browser,
            background,
        })
    }

    fn line(&self, text: String) -> String {
        if self.redact {
            super::background::redacted(&text)
        } else {
            text
        }
    }

    fn respond(&self, id: Value, outcome: Result<Value, String>) -> String {
        let response = match outcome {
            Ok(data) => Response::ok(id, data),
            Err(error) => Response::err(id, error),
        };
        self.line(serde_json::to_string(&Frame::Response(response)).unwrap_or_default())
    }

    /// The ids of the requests that still wait for an answer.
    fn open_requests(&self) -> Vec<String> {
        let mut ids: Vec<String> = locked(&self.waiting.approvals)
            .iter()
            .filter(|(_, reply)| !reply.is_closed())
            .map(|(id, _)| id.clone())
            .collect();
        ids.extend(
            locked(&self.waiting.answers)
                .iter()
                .filter(|(_, reply)| !reply.is_closed())
                .map(|(id, _)| id.clone()),
        );
        ids
    }

    /// Hands the terminal's loop a command and waits for what it says.
    async fn ask_terminal(&self, what: Asked) -> Result<Value, String> {
        let (reply, answer) = oneshot::channel();
        self.web
            .send(Web { what, reply })
            .map_err(|_| "the terminal is no longer sharing this session".to_string())?;
        answer
            .await
            .map_err(|_| "the terminal did not answer".to_string())?
    }

    async fn run(&self, command: Command) -> Result<Value, String> {
        let blocks = |images: Vec<azazel_protocol::Image>| -> Vec<Block> {
            images
                .into_iter()
                .map(|image| Block::Image {
                    mime: image.mime,
                    data: image.data,
                })
                .collect()
        };
        match command {
            Command::GetState => self.source.state(self.feed.busy()),
            Command::GetMessages { leaf, before, limit } => {
                self.source.messages(leaf.as_deref(), before.as_deref(), limit)
            }
            Command::GetAgentMessages { agent, before, limit } => {
                self.source.agent_messages(&agent, before.as_deref(), limit)
            }
            Command::GetTree => self.source.tree(),
            Command::ListModels { .. }
            | Command::ListProviders
            | Command::ListRoles
            | Command::ListCommands
            | Command::ListFiles
            | Command::GetUsage
            | Command::GetSettings
            | Command::GetTheme { .. }
            | Command::GetContext
            | Command::GetCost => {
                let source = self.source.clone();
                let command = command.clone();
                tokio::task::spawn_blocking(move || source.listing(&command))
                    .await
                    .map_err(|e| e.to_string())?
                    .unwrap_or_else(|| Err("not available from a shared terminal session".to_string()))
            }
            Command::ApprovalResponse {
                request_id,
                decision,
                feedback,
            } => {
                let decision = match decision {
                    Decision::Allow => ApprovalDecision::AllowOnce,
                    Decision::Session => ApprovalDecision::AllowSession,
                    Decision::Always => ApprovalDecision::Always,
                    Decision::Deny => ApprovalDecision::Deny { feedback },
                };
                let reply = locked(&self.waiting.approvals).remove(&request_id);
                reply
                    .ok_or_else(|| format!("no approval is waiting as `{request_id}`"))?
                    .send(decision)
                    .map_err(|_| "the agent is no longer waiting for this approval".to_string())?;
                Ok(Value::Null)
            }
            Command::UiResponse { request_id, value } => {
                let reply = locked(&self.waiting.answers).remove(&request_id);
                reply
                    .ok_or_else(|| format!("no request is waiting as `{request_id}`"))?
                    .send(value)
                    .map_err(|_| "the agent is no longer waiting for this answer".to_string())?;
                Ok(Value::Null)
            }
            Command::Prompt { text, images, .. } => {
                self.ask_terminal(Asked::Prompt {
                    text,
                    images: blocks(images),
                })
                .await
            }
            Command::Steer { text, images } => {
                self.ask_terminal(Asked::Steer {
                    text,
                    images: blocks(images),
                })
                .await
            }
            Command::FollowUp { text } => self.ask_terminal(Asked::FollowUp { text }).await,
            Command::Abort => self.ask_terminal(Asked::Abort).await,
            Command::SetMode { mode } => self.ask_terminal(Asked::SetMode(mode)).await,
            Command::SetModel { chain } => self.ask_terminal(Asked::SetModel(chain)).await,
            Command::SetThinking { level } => self.ask_terminal(Asked::SetThinking(level)).await,
            _ => Err("not available from a shared terminal session".to_string()),
        }
    }

    /// Serves one browser until it goes.
    async fn serve(
        self: Arc<Self>,
        conn: u64,
        mut commands: mpsc::Receiver<String>,
        out: mpsc::Sender<String>,
    ) {
        let mut forward: Option<JoinHandle<()>> = None;
        let mut browser_pump: Option<JoinHandle<()>> = None;
        let holder = live::holder_of(conn);
        while let Some(line) = commands.recv().await {
            let (id, command) = Request::parse(&line);
            let reply = match command {
                Err(error) => self.respond(id, Err(error)),
                Ok(Command::Attach { since, .. }) => {
                    let attachment = self.feed.attach(conn, since);
                    let about = self.source.about();
                    let attached = Attached {
                        host: ID.to_string(),
                        pid: std::process::id(),
                        session_id: about.session_id,
                        session_file: about.session_file,
                        clients: attachment.clients,
                        snapshot: Snapshot {
                            busy: self.feed.busy(),
                            tail: self.source.tail(),
                            pending: self.open_requests(),
                            ..attachment.snapshot
                        },
                        cursor: attachment.cursor,
                        gap: attachment.gap,
                    };
                    let response =
                        self.respond(id, serde_json::to_value(attached).map_err(|e| e.to_string()));
                    let mut lines = vec![response];
                    lines.extend(attachment.replay.into_iter().map(|line| self.line(line)));
                    lines.extend(attachment.asks.into_iter().map(|(_, line)| self.line(line)));
                    for line in lines {
                        if out.send(line).await.is_err() {
                            return;
                        }
                    }
                    if let Some(old) = forward.take() {
                        old.abort();
                    }
                    let (this, out) = (self.clone(), out.clone());
                    let mut live = attachment.live;
                    forward = Some(tokio::spawn(async move {
                        while let Some(line) = live.recv().await {
                            if out.send(this.line(line)).await.is_err() {
                                break;
                            }
                        }
                    }));
                    continue;
                }
                Ok(command) if live::is_browser(&command) => {
                    if browser_pump.is_none()
                        && let Some(browser) = &self.browser
                    {
                        browser_pump = Some(browser.pump_to(holder.clone(), tell(&out, self.redact)));
                    }
                    let (answer, answered) = oneshot::channel();
                    live::dispatch(
                        self.browser.as_ref(),
                        holder.clone(),
                        command,
                        Box::new(move |outcome| {
                            let _ = answer.send(outcome);
                        }),
                    );
                    let outcome = answered
                        .await
                        .unwrap_or_else(|_| Err("the browser did not answer".to_string()));
                    self.respond(id, outcome)
                }
                Ok(command) => {
                    // What a browser sent is echoed to the others, not to itself.
                    let echo = match &command {
                        Command::Prompt { text, .. } => Some(("prompt", text.clone())),
                        Command::Steer { text, .. } => Some(("steer", text.clone())),
                        Command::FollowUp { text } => Some(("follow_up", text.clone())),
                        _ => None,
                    };
                    let outcome = self.run(command).await;
                    if outcome.is_ok()
                        && let Some((kind, text)) = echo
                    {
                        self.feed.user_input(kind, &text, Some(conn));
                    }
                    self.respond(id, outcome)
                }
            };
            if out.send(reply).await.is_err() {
                break;
            }
        }
        if let Some(forward) = forward {
            forward.abort();
        }
        if let Some(pump) = browser_pump {
            pump.abort();
        }
        // A browser that held the agents' browser and went must not leave them waiting.
        if let Some(browser) = &self.browser {
            browser.leave(&holder);
        }
    }
}

/// What the pump of the agents' browser writes to the browser on `out`, with the address and title
/// of the page hidden when `redact` is on. Lines wait in order for `out`, which a slow device
/// reads late: a picture is skipped while one still waits, so that it sees fewer of them and is
/// never let go for them (the newest skipped one is sent when the waiting one is out), but a
/// state is never dropped.
fn tell(out: &mpsc::Sender<String>, redact: bool) -> impl Fn(String, bool) + Send + Sync + 'static {
    let (lines, mut waiting) = mpsc::unbounded_channel::<String>();
    let pictures = Arc::new(live::Pictures::default());
    tokio::spawn({
        let (out, pictures) = (out.clone(), pictures.clone());
        async move {
            while let Some(line) = waiting.recv().await {
                let picture = line.starts_with(live::PICTURE);
                let sent = out.send(line).await;
                // Sends the newest picture that was skipped meanwhile, so that a page that has
                // stopped changing is not left on an old one.
                live::written(&pictures, picture);
                if sent.is_err() {
                    return;
                }
            }
        }
    });
    let send = live::one_picture_in_flight(pictures, move |line| {
        let _ = lines.send(line);
    });
    move |line, picture| {
        let line = if redact && !picture {
            super::background::redacted(&line)
        } else {
            line
        };
        send(line, picture);
    }
}

#[async_trait]
impl Sessions for TerminalSessions {
    /// The terminal's session first, then the background sessions.
    async fn list(&self) -> Vec<SessionInfo> {
        let about = self.source.about();
        let mut all = vec![SessionInfo {
            id: ID.to_string(),
            name: about.name,
            kind: Kind::Terminal,
            state: if self.feed.busy() { "working" } else { "idle" }.to_string(),
            cwd: about.cwd,
            cost_usd: about.cost_usd,
            mode: Some(about.mode),
            started: self.started.clone(),
        }];
        if let Some(background) = &self.background {
            all.extend(background.list().await);
        }
        all
    }

    async fn open(&self, id: &str) -> Result<Conn, String> {
        if id != ID {
            return match &self.background {
                Some(background) => background.open(id).await,
                None => Err(format!(
                    "no session {id}: this terminal shares one session, `{ID}`"
                )),
            };
        }
        let (frames_tx, frames) = mpsc::channel::<String>(QUEUE);
        let (commands, commands_rx) = mpsc::channel::<String>(64);
        let hello = serde_json::to_string(&rpc::hello()).map_err(|e| e.to_string())?;
        frames_tx
            .send(hello)
            .await
            .map_err(|_| "the connection closed".to_string())?;
        let conn = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let this = Arc::new(TerminalSessions {
            source: self.source.clone(),
            feed: self.feed.clone(),
            waiting: self.waiting.clone(),
            web: self.web.clone(),
            redact: self.redact,
            started: self.started.clone(),
            next_conn: AtomicU64::new(0),
            browser: self.browser.clone(),
            background: None,
        });
        tokio::spawn(this.serve(conn, commands_rx, frames_tx));
        Ok(Conn { frames, commands })
    }

    fn can_start(&self) -> bool {
        self.background.is_some()
    }

    async fn folders(&self, under: Option<&str>) -> Result<Folders, StartError> {
        match &self.background {
            Some(background) => background.folders(under).await,
            None => Err(StartError::Refused(
                "new sessions cannot be started from here".to_string(),
            )),
        }
    }

    async fn history(&self, query: Option<&str>) -> Result<Vec<SavedSession>, StartError> {
        match &self.background {
            Some(background) => background.history(query).await,
            None => Err(StartError::Refused(
                "earlier sessions cannot be listed from here".to_string(),
            )),
        }
    }

    async fn start(&self, new: NewSession) -> Result<String, StartError> {
        match &self.background {
            Some(background) => background.start(new).await,
            None => Err(StartError::Refused(
                "new sessions cannot be started from here".to_string(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------------- sharing

/// What happens to a session that is shared, as the terminal's loop is told.
pub(crate) enum Event {
    /// A browser asked for something.
    Web(Web),
    /// The tunnel asked for with `/remote tunnel` or `/remote tailscale` is up, or is not.
    Tunnel(Result<Box<Tunnel>, String>, Access),
}

/// A session shared from this terminal: the server, what it serves, and what to undo.
pub(crate) struct Sharing {
    server: Server,
    pub(crate) tunnel: Option<Tunnel>,
    pub(crate) feed: Arc<Feed>,
    waiting: Arc<Waiting>,
    tui_bridge: Arc<dyn UiBridge>,
    services: Arc<Services>,
    pump: JoinHandle<()>,
    /// Puts a browser's request for a folder nobody trusted to the person at the terminal.
    questions: JoinHandle<()>,
}

impl Sharing {
    /// Starts the server on this machine and puts the shared bridge in place of the terminal's.
    pub(crate) async fn start(
        app: &Arc<App>,
        lead: &AgentHandle,
        session: &Arc<SessionStore>,
        web: mpsc::UnboundedSender<Web>,
    ) -> Result<Sharing, String> {
        let services = app.registry.services.clone();
        let tui_bridge = services
            .get::<Ui>()
            .map(|ui| ui.0.clone())
            .ok_or("this session has no terminal to share")?;
        let team = services.get::<Team>();
        let feed = Arc::new(Feed::default());
        let waiting: Arc<Waiting> = Arc::default();
        let remote = &app.config.remote;
        let source = Arc::new(LocalSource {
            app: app.clone(),
            lead: lead.clone(),
            session: session.clone(),
            team: team.clone(),
            listed: crate::models::Listed::default(),
        });
        // The same rules as `azazel remote`; a folder nobody trusted is asked about at this terminal.
        let (asks, mut asked) = mpsc::channel::<Ask>(8);
        let ask_bridge = tui_bridge.clone();
        let questions = tokio::spawn(async move {
            while let Some(ask) = asked.recv().await {
                let yes = ask_bridge.confirm(ask.question.trim()).await.unwrap_or(false);
                let _ = ask.answer.send(yes);
            }
        });
        // `--trust` trusts the folder this terminal was started in, not every folder a browser names.
        let carried = services
            .get::<crate::detach::Carried>()
            .map(|carried| {
                carried
                    .0
                    .iter()
                    .filter(|arg| *arg != "--trust")
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let background = BackgroundSessions::new(
            app.paths.clone(),
            remote.redact,
            carried,
            Rules::of(app.paths.home.clone(), std::env::home_dir(), remote),
            Some(asks),
        );
        let sessions = TerminalSessions::new(
            source,
            feed.clone(),
            waiting.clone(),
            web,
            remote.redact,
            Browser::of(&services),
            Some(background),
        );
        let server = Server::start(options(&app.config), sessions)
            .await
            .map_err(|e| format!("could not start the server: {e}"))?;
        let (lines, requests) = mpsc::unbounded_channel();
        let counter = lead.clone();
        let out = Requests::new(lines, Arc::new(move || counter.emitted()));
        services.insert(Arc::new(Ui(Arc::new(SharedBridge::new(
            tui_bridge.clone(),
            out,
            waiting.clone(),
        )))));
        let pump = pump(lead.clone(), team, requests, feed.clone());
        Ok(Sharing {
            server,
            tunnel: None,
            feed,
            waiting,
            tui_bridge,
            services,
            pump,
            questions,
        })
    }

    /// The terminal's own bridge, for questions only the person at the terminal answers.
    pub(crate) fn tui_bridge(&self) -> Arc<dyn UiBridge> {
        self.tui_bridge.clone()
    }

    pub(crate) fn port(&self) -> u16 {
        self.server.port()
    }

    pub(crate) fn urls(&self) -> Vec<String> {
        self.server.urls()
    }

    pub(crate) fn link(&self, access: Access) -> Link {
        self.server.link(access)
    }

    /// Makes `url` the address links point to.
    pub(crate) fn set_public_url(&self, url: &str) -> Result<(), String> {
        self.server.set_public_url(url)
    }

    /// How many browsers are attached.
    pub(crate) fn clients(&self) -> usize {
        self.feed.clients()
    }

    /// Ends the sharing: the terminal's own bridge is put back, the server and the tunnel stop.
    pub(crate) async fn stop(self) {
        self.services.insert(Arc::new(Ui(self.tui_bridge.clone())));
        self.pump.abort();
        self.questions.abort();
        // Requests the browsers were still to answer are the terminal's alone from here on.
        locked(&self.waiting.approvals).clear();
        locked(&self.waiting.answers).clear();
        self.server.shutdown().await;
        if let Some(tunnel) = self.tunnel {
            tunnel.stop().await;
        }
    }
}

fn options(config: &Config) -> Options {
    let remote = &config.remote;
    Options {
        bind: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        port: remote.port,
        public_url: Some(remote.public_url.clone()).filter(|url| !url.trim().is_empty()),
        link_ttl: Duration::from_secs(remote.link_minutes.max(1) * 60),
        device_ttl: Duration::from_secs(remote.device_hours.max(1) * 3600),
        redact: remote.redact,
        allow_bypass: remote.allow_bypass,
        full_access: remote.full_access,
        view_browser: remote.view_browser,
        gated: azazel_core::config::is_trust_gated,
    }
}

/// Which way in `/remote` takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Way {
    Local,
    Cloudflare,
    Tailscale,
}

/// Opens the tunnel of `way` to `port`. A missing `cloudflared` is offered through `ask`, the
/// terminal's own popups.
pub(crate) async fn open_tunnel(
    paths: azazel_core::Paths,
    way: Way,
    port: u16,
    ask: Arc<dyn UiBridge>,
) -> Result<Tunnel, String> {
    match way {
        Way::Local => Err("no tunnel was asked for".to_string()),
        Way::Cloudflare => {
            let program: PathBuf = tunnel::ensure_cloudflared(&paths, Some(&ask)).await?;
            tunnel::cloudflare(&program, port).await
        }
        Way::Tailscale => tunnel::tailscale(&paths, port).await,
    }
}

/// The link as lines of a QR code.
pub(crate) fn qr_rows(url: &str, glyphs: Glyphs) -> Vec<String> {
    super::qr::qr_lines(&azazel_remote::qr_matrix(url), glyphs)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use azazel_agent::{Access as ToolAccess, AgentInfo};

    use super::*;

    fn approval() -> ApprovalRequest {
        ApprovalRequest {
            agent: AgentInfo {
                id: "lead".into(),
                def: "lead".into(),
                parent: None,
                depth: 0,
                session_id: "s".into(),
            },
            tool: "shell".into(),
            access: ToolAccess::Exec,
            subjects: vec!["ls".into()],
            input: Value::Null,
            rule: None,
            note: None,
            destructive: false,
            secret: false,
        }
    }

    /// The terminal's side of a request: it answers when told to, and says when it was given up.
    struct Terminal {
        answer: Mutex<Option<oneshot::Receiver<ApprovalDecision>>>,
        dropped: Arc<AtomicBool>,
    }

    struct Gone(Arc<AtomicBool>);

    impl Drop for Gone {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl UiBridge for Terminal {
        async fn ask(&self, _: &Question) -> Result<Vec<String>, UiError> {
            Err(UiError::NoUser)
        }
        async fn ask_form(&self, _: &[Question]) -> Result<Vec<Vec<String>>, UiError> {
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
        async fn secret(&self, _: &str) -> Result<String, UiError> {
            Err(UiError::NoUser)
        }
        fn notify(&self, _: Level, _: &str) {}
        async fn approval(&self, _: ApprovalRequest) -> Option<ApprovalDecision> {
            let _gone = Gone(self.dropped.clone());
            let answer = self.answer.lock().unwrap().take()?;
            answer.await.ok()
        }
        async fn review_plan(&self, _: &std::path::Path, _: &str) -> Result<PlanDecision, UiError> {
            Err(UiError::NoUser)
        }
    }

    struct Rig {
        bridge: Arc<SharedBridge>,
        waiting: Arc<Waiting>,
        lines: mpsc::UnboundedReceiver<Fenced>,
        terminal_answers: Option<oneshot::Sender<ApprovalDecision>>,
        dropped: Arc<AtomicBool>,
        emitted: Arc<AtomicU64>,
    }

    fn rig() -> Rig {
        let (terminal_answers, answer) = oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let tui = Arc::new(Terminal {
            answer: Mutex::new(Some(answer)),
            dropped: dropped.clone(),
        });
        let (lines_tx, lines) = mpsc::unbounded_channel();
        let emitted = Arc::new(AtomicU64::new(0));
        let counter = emitted.clone();
        let waiting: Arc<Waiting> = Arc::default();
        let out = Requests::new(lines_tx, Arc::new(move || counter.load(Ordering::SeqCst)));
        Rig {
            bridge: Arc::new(SharedBridge::new(tui, out, waiting.clone())),
            waiting,
            lines,
            terminal_answers: Some(terminal_answers),
            dropped,
            emitted,
        }
    }

    async fn line(rig: &mut Rig) -> (u64, Value) {
        let (after, text) = rig.lines.recv().await.unwrap();
        (after, serde_json::from_str(&text).unwrap())
    }

    #[tokio::test]
    async fn an_answer_at_the_terminal_closes_the_request_in_the_browsers() {
        let mut rig = rig();
        let bridge = rig.bridge.clone();
        let asking = tokio::spawn(async move { bridge.approval(approval()).await });
        let (_, request) = line(&mut rig).await;
        assert_eq!(request["type"], "approval_request");
        let id = request["request_id"].as_str().unwrap().to_string();
        rig.terminal_answers
            .take()
            .unwrap()
            .send(ApprovalDecision::AllowOnce)
            .unwrap();
        assert_eq!(asking.await.unwrap(), Some(ApprovalDecision::AllowOnce));
        let (_, closed) = line(&mut rig).await;
        assert_eq!(
            (closed["type"].as_str(), closed["request_id"].as_str()),
            (Some("request_closed"), Some(id.as_str()))
        );
        assert!(locked(&rig.waiting.approvals).is_empty());
    }

    #[tokio::test]
    async fn an_answer_in_a_browser_closes_the_request_at_the_terminal() {
        let mut rig = rig();
        let bridge = rig.bridge.clone();
        let asking = tokio::spawn(async move { bridge.approval(approval()).await });
        let (_, request) = line(&mut rig).await;
        let id = request["request_id"].as_str().unwrap().to_string();
        let reply = locked(&rig.waiting.approvals).remove(&id).unwrap();
        reply.send(ApprovalDecision::Always).unwrap();
        assert_eq!(asking.await.unwrap(), Some(ApprovalDecision::Always));
        assert!(
            rig.dropped.load(Ordering::SeqCst),
            "the terminal's request was given up"
        );
        let (_, closed) = line(&mut rig).await;
        assert_eq!(closed["type"], "request_closed");
    }

    #[tokio::test]
    async fn a_request_waits_for_the_events_the_lead_had_emitted_before_it() {
        let mut rig = rig();
        rig.emitted.store(3, Ordering::SeqCst);
        let bridge = rig.bridge.clone();
        tokio::spawn(async move { bridge.approval(approval()).await });
        let (after, request) = line(&mut rig).await;
        assert_eq!(after, 3);
        // The feed's loop writes it only once three events of the lead went out.
        let mut pending = Pending {
            written: 2,
            lines: VecDeque::from([(after, request.to_string())]),
        };
        let mut out = Vec::new();
        pending.flush_to(|line| out.push(line));
        assert!(out.is_empty());
        pending.written = 3;
        pending.flush_to(|line| out.push(line));
        assert_eq!(out.len(), 1);
    }

    fn replay_since(feed: &Feed, at: u64) -> Option<Vec<String>> {
        let attached = feed.attach(99, Some(at));
        (!attached.gap).then_some(attached.replay)
    }

    #[test]
    fn a_late_browser_sees_the_partial_reply_and_the_last_failure() {
        let feed = Feed::default();
        feed.publish(json!({"type": "agent_start", "agent": "lead"}), None);
        feed.publish(json!({"type": "message_start", "agent": "lead"}), None);
        feed.publish(
            json!({"type": "stream", "agent": "lead", "event": {
                "type": "text_delta", "index": 0, "delta": "Already written"
            }}),
            None,
        );
        let snapshot = feed.attach(1, None).snapshot;
        assert!(snapshot.busy);
        assert_eq!(snapshot.streaming[0].text, "Already written");
        feed.publish(
            json!({"type": "failed", "agent": "lead", "error": "quota exhausted"}),
            None,
        );
        let snapshot = feed.attach(2, None).snapshot;
        assert!(!snapshot.busy);
        assert!(snapshot.streaming.is_empty());
        assert_eq!(snapshot.last_error.as_deref(), Some("quota exhausted"));
    }

    #[test]
    fn a_browser_resumes_after_the_frame_it_saw_and_learns_of_a_gap() {
        let feed = Feed::default();
        for n in 0..3 {
            feed.publish(json!({ "type": "idle", "n": n }), None);
        }
        let after_first = replay_since(&feed, 0).unwrap();
        assert_eq!(after_first.len(), 2);
        assert!(after_first[0].contains("\"at\":1"));
        assert_eq!(replay_since(&feed, 2).unwrap().len(), 0);
        assert!(replay_since(&feed, 9).is_none());
        for _ in 0..RING + 10 {
            feed.publish(json!({ "type": "idle" }), None);
        }
        assert!(replay_since(&feed, 0).is_none());
    }

    struct Stub;

    impl Source for Stub {
        fn state(&self, busy: bool) -> Result<Value, String> {
            Ok(json!({ "busy": busy }))
        }
        fn messages(&self, _: Option<&str>, _: Option<&str>, _: Option<u32>) -> Result<Value, String> {
            Ok(json!([]))
        }
        fn agent_messages(&self, _: &str, _: Option<&str>, _: Option<u32>) -> Result<Value, String> {
            Err("none".into())
        }
        fn tree(&self) -> Result<Value, String> {
            Ok(json!([]))
        }
        fn tail(&self) -> Vec<MessageRow> {
            Vec::new()
        }
        fn listing(&self, command: &Command) -> Option<Result<Value, String>> {
            matches!(command, Command::ListModels { .. }).then(|| {
                Ok(json!([{ "model": "mock/lead", "reasoning": true, "thinking_levels": ["low", "high"] }]))
            })
        }
        fn about(&self) -> About {
            About {
                session_id: "s1".into(),
                session_file: "s1.jsonl".into(),
                name: None,
                cwd: ".".into(),
                mode: "manual".into(),
                cost_usd: None,
            }
        }
    }

    async fn frame(conn: &mut Conn) -> Value {
        serde_json::from_str(&conn.frames.recv().await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_prompt_from_a_browser_reaches_the_terminals_loop_and_is_echoed_to_the_others() {
        let feed = Arc::new(Feed::default());
        let (web, mut loop_rx) = mpsc::unbounded_channel();
        let sessions = TerminalSessions::new(
            Arc::new(Stub),
            feed.clone(),
            Arc::default(),
            web,
            false,
            None,
            None,
        );
        assert_eq!(sessions.list().await[0].id, ID);
        assert!(sessions.open("nope").await.is_err());
        let mut one = sessions.open(ID).await.unwrap();
        let mut two = sessions.open(ID).await.unwrap();
        for conn in [&mut one, &mut two] {
            assert_eq!(frame(conn).await["type"], "hello");
            conn.commands
                .send(json!({ "id": 1, "type": "attach" }).to_string())
                .await
                .unwrap();
            let attached = frame(conn).await;
            assert_eq!(attached["data"]["session_id"], "s1");
        }
        one.commands
            .send(json!({ "id": 2, "type": "prompt", "text": "hello" }).to_string())
            .await
            .unwrap();
        let web = loop_rx.recv().await.unwrap();
        let Asked::Prompt { text, .. } = web.what else {
            panic!("a prompt reaches the loop as a prompt");
        };
        assert_eq!(text, "hello");
        web.reply.send(Ok(Value::Null)).unwrap();
        let response = frame(&mut one).await;
        assert_eq!(
            (response["type"].as_str(), response["success"].as_bool()),
            (Some("response"), Some(true))
        );
        let echoed = frame(&mut two).await;
        assert_eq!(
            (echoed["type"].as_str(), echoed["text"].as_str()),
            (Some("user_input"), Some("hello"))
        );
        // What the sender typed is not sent back to it.
        one.commands
            .send(json!({ "id": 3, "type": "shell", "command": "ls" }).to_string())
            .await
            .unwrap();
        let refused = frame(&mut one).await;
        assert_eq!(refused["id"], 3);
        assert_eq!(refused["success"], false);
    }

    #[tokio::test]
    async fn a_shared_terminal_answers_what_it_cannot_do_instead_of_waiting() {
        let (web, mut loop_rx) = mpsc::unbounded_channel();
        let sessions = TerminalSessions::new(
            Arc::new(Stub),
            Arc::default(),
            Arc::default(),
            web,
            false,
            None,
            None,
        );
        let mut conn = sessions.open(ID).await.unwrap();
        assert_eq!(frame(&mut conn).await["type"], "hello");
        let commands = [
            json!({ "type": "set_setting", "path": "tui.theme", "value": "auto" }),
            json!({ "type": "trust_project", "trusted": true }),
            json!({ "type": "add_dir", "path": "." }),
            json!({ "type": "forget_approvals" }),
        ];
        for (n, mut command) in commands.into_iter().enumerate() {
            command["id"] = json!(n + 1);
            conn.commands.send(command.to_string()).await.unwrap();
            let answer = tokio::time::timeout(std::time::Duration::from_secs(5), frame(&mut conn))
                .await
                .expect("an answer, not a wait");
            assert_eq!(answer["success"], false, "{answer}");
            assert_eq!(answer["error"], "not available from a shared terminal session");
        }
        // Nothing reached the terminal's loop.
        assert!(loop_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_browser_on_a_shared_terminal_can_read_the_model_list() {
        let (web, _loop_rx) = mpsc::unbounded_channel();
        let sessions = TerminalSessions::new(
            Arc::new(Stub),
            Arc::default(),
            Arc::default(),
            web,
            false,
            None,
            None,
        );
        let mut conn = sessions.open(ID).await.unwrap();
        assert_eq!(frame(&mut conn).await["type"], "hello");
        conn.commands
            .send(json!({ "id": 1, "type": "list_models" }).to_string())
            .await
            .unwrap();
        let models = frame(&mut conn).await;
        assert_eq!(models["success"], true, "{models}");
        assert_eq!(models["data"][0]["thinking_levels"], json!(["low", "high"]));
    }

    /// A background session that is running as far as the list can tell: its entry, and its lock held.
    fn running_entry(home: &std::path::Path, id: &str, cwd: &std::path::Path) -> crate::running::Lock {
        crate::running::create_run_dir(home).unwrap();
        crate::running::Entry {
            id: id.into(),
            pid: std::process::id(),
            session_id: format!("session-{id}"),
            session_file: home.join("none.jsonl"),
            cwd: cwd.to_path_buf(),
            name: None,
            started: "2026-11-20T10:00:00Z".into(),
            socket: String::new(),
            version: "0".into(),
            state: "idle".into(),
            cost_usd: None,
            mode: None,
            args: Vec::new(),
            interrupted_at: None,
        }
        .write(home)
        .unwrap();
        crate::running::Lock::acquire(&crate::running::host_lock(home, id))
            .unwrap()
            .unwrap()
    }

    /// A terminal share that also reaches the background sessions of `home`, under `rules`.
    fn shared(home: &std::path::Path, rules: Rules) -> Arc<TerminalSessions> {
        let (web, _loop_rx) = mpsc::unbounded_channel();
        let paths = azazel_core::Paths::discover(home, Some(&home.to_string_lossy()));
        let background = BackgroundSessions::new(paths, false, Vec::new(), rules, None);
        TerminalSessions::new(
            Arc::new(Stub),
            Arc::default(),
            Arc::default(),
            web,
            false,
            None,
            Some(background),
        )
    }

    #[tokio::test]
    async fn a_shared_terminal_lists_its_own_session_first_and_the_background_sessions_after_it() {
        let scratch = tempfile::tempdir().unwrap();
        let home = scratch.path().join("azazel");
        let work = scratch.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let _held = running_entry(&home, "bg01", &work);
        let rules = Rules {
            home: home.clone(),
            user_home: Some(scratch.path().to_path_buf()),
            roots: vec![work.display().to_string()],
            anywhere: false,
        };
        let sessions = shared(&home, rules);
        let listed = sessions.list().await;
        let ids: Vec<(&str, Kind)> = listed.iter().map(|info| (info.id.as_str(), info.kind)).collect();
        assert_eq!(ids, [(ID, Kind::Terminal), ("bg01", Kind::Background)]);
        assert!(sessions.can_start());
        // Other ids are the background sessions' to answer, and none is there.
        let missing = sessions.open("nope").await.err().unwrap();
        assert!(missing.contains("no background session nope"), "{missing}");
        // A share without them stays the one session it was.
        let (web, _loop_rx) = mpsc::unbounded_channel();
        let alone = TerminalSessions::new(
            Arc::new(Stub),
            Arc::default(),
            Arc::default(),
            web,
            false,
            None,
            None,
        );
        assert!(!alone.can_start());
        assert_eq!(alone.list().await.len(), 1);
        assert!(matches!(
            alone.start(NewSession::default()).await,
            Err(StartError::Refused(_))
        ));
    }

    #[tokio::test]
    async fn a_shared_terminal_starts_only_where_azazel_remote_would() {
        let scratch = tempfile::tempdir().unwrap();
        let home = scratch.path().join("azazel");
        let work = scratch.path().join("work");
        let elsewhere = scratch.path().join("elsewhere");
        for dir in [&home, &work, &elsewhere] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let rules = Rules {
            home: home.clone(),
            user_home: Some(scratch.path().to_path_buf()),
            roots: vec![work.display().to_string()],
            anywhere: false,
        };
        let sessions = shared(&home, rules);
        let refused = sessions
            .start(NewSession {
                cwd: elsewhere.display().to_string(),
                ..NewSession::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(refused, StartError::Refused(_)), "{refused:?}");
        let folders = sessions.folders(None).await.unwrap();
        assert!(!folders.anywhere);
        assert_eq!(folders.folders.len(), 1);

        // A saved session outside the folders is answered as an id that is not there.
        let paths = azazel_core::Paths::discover(&home, Some(&home.to_string_lossy()));
        let sessions_dir = paths.home.join("projects").join("p").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let saved = azazel_core::SessionStore::create(&sessions_dir, &elsewhere, None).unwrap();
        let (known, unknown) = (saved.id(), "00000000-0000-0000-0000-000000000000".to_string());
        let mut answers = Vec::new();
        for wanted in [&known, &unknown] {
            let error = sessions
                .start(NewSession {
                    resume: Some(wanted.clone()),
                    ..NewSession::default()
                })
                .await
                .unwrap_err();
            let StartError::Invalid(text) = error else {
                panic!("{error:?}");
            };
            answers.push(text.replace(wanted.as_str(), "<id>"));
        }
        assert_eq!(answers[0], answers[1]);
    }
}
