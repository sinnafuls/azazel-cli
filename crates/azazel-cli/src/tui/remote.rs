//! A session in another process as the UI sees it: a background session on this machine, or
//! azazel on another machine reached over SSH. Everything goes through the RPC protocol.
//!
//! What the UI draws on every frame (the lead's status, the mode, the roster) is kept here as
//! the session last reported it, and brought up to date as frames arrive. What the UI reads of
//! the session's contents is a copy, fetched again once the session has changed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_agent::{Access, AgentEvent, AgentInfo, AgentStatus, ToolOutput};
use azazel_ai::{Block, ModelRef, ModelSpec, StreamEvent, ThinkingLevel, Usage};
use azazel_client::{Client, Error};
use azazel_core::session::{Entry, Header, SessionSummary};
use azazel_core::side::SideAnswer;
use azazel_core::{
    ApprovalDecision, ApprovalRequest, AskKind, CommandOutcome, EntryKind, Excerpt, Level, PermissionMode,
    PlanDecision, Question, SessionStore, UiError,
};
use azazel_protocol::{
    self as protocol, Attached, BtwAnswer, Command, CommandResult, CommandRow, Decision, Exported, Frame,
    Member, ModelRow, ProviderRow, Replay, RewindWhat, Roles, SaveTo, SessionCopy, SessionRow, ShellResult,
    Snapshot, State,
};
use azazel_team::{LEAD, MemberInfo, MemberState, Mentioned, TeamEvent};
use azazel_tools::{TodoItem, TodoStatus};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::backend::{Backend, Billing, Facts, Go, Incoming, Moved};
use super::bridge::UiRequest;
use super::overlay::RewindPoint;
use super::{Internal, btw, slash};
use crate::retry::Again;
use crate::side::Exchange;

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the pump and the UI both look at.
struct Shared {
    state: Mutex<State>,
    /// The roster as it last arrived, and when: the clocks of working agents run on from then.
    roster: Mutex<(Vec<Member>, Instant)>,
    /// The session has changed since its copy was fetched.
    stale: AtomicBool,
    /// A `get_state` is on its way.
    refreshing: AtomicBool,
    /// Where the text of the side question being answered goes.
    btw: Mutex<Option<(u64, mpsc::UnboundedSender<Internal>)>>,
    /// The requests of the session the UI shows and has not answered: cancelling one takes
    /// it off the screen without an answer (another client gave it).
    asked: Mutex<HashMap<String, CancellationToken>>,
}

/// Where a session that is not in this process runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Place {
    /// A background session on this machine.
    #[cfg(feature = "detach")]
    Background { id: String },
    /// azazel on another machine, started for this connection.
    Ssh { host: String },
    /// A background session on another machine.
    SshBackground { host: String, id: String },
}

impl Place {
    fn name(&self) -> String {
        match self {
            #[cfg(feature = "detach")]
            Place::Background { id } => format!("background session {id}"),
            Place::Ssh { host } => format!("on {host}"),
            Place::SshBackground { host, id } => format!("background session {id} on {host}"),
        }
    }

    /// The id of the background session, when it is one.
    fn background(&self) -> Option<&str> {
        match self {
            #[cfg(feature = "detach")]
            Place::Background { id } => Some(id),
            Place::SshBackground { id, .. } => Some(id),
            Place::Ssh { .. } => None,
        }
    }

    fn farewell(&self) -> String {
        match self {
            #[cfg(feature = "detach")]
            Place::Background { id } => {
                format!("background session {id} goes on; `azazel attach {id}` returns to it")
            }
            Place::SshBackground { host, id } => format!(
                "background session {id} goes on on {host}; `azazel --ssh {host} --attach {id}` returns to it"
            ),
            Place::Ssh { host } => {
                format!("the session is saved on {host}; `azazel --ssh {host} -c` continues it")
            }
        }
    }

    /// The arguments that open the session again, after those that say where it runs: what
    /// [`Place::farewell`] tells the user to type.
    fn reopen(&self) -> Vec<&str> {
        match self {
            #[cfg(feature = "detach")]
            Place::Background { id } => vec!["attach", id],
            Place::SshBackground { id, .. } => vec!["--attach", id],
            Place::Ssh { .. } => vec!["-c"],
        }
    }
}

pub(crate) struct Remote {
    client: Arc<Client>,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    shared: Arc<Shared>,
    place: Place,
    /// The copy of the session the UI reads.
    session: Arc<SessionStore>,
    commands: Vec<(String, String)>,
}

fn parsed<T: DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|e| format!("the session answered something unexpected: {e}"))
}

async fn ask<T: DeserializeOwned>(client: &Client, command: Command) -> Result<T, String> {
    parsed(client.send(command).await.map_err(|e| e.to_string())?)
}

/// The session's own name for an image block.
fn images(blocks: Vec<Block>) -> Vec<protocol::Image> {
    blocks
        .into_iter()
        .filter_map(|block| match block {
            Block::Image { mime, data } => Some(protocol::Image { mime, data }),
            _ => None,
        })
        .collect()
}

/// The event a frame of an agent carries, with the agent's id. `None` for the frames that are
/// not events of an agent.
pub(crate) fn agent_event(frame: Frame) -> Option<(String, AgentEvent)> {
    let model = |text: &str| text.parse::<ModelRef>().ok();
    Some(match frame {
        Frame::AgentStart { agent } => (agent, AgentEvent::AgentStart),
        Frame::TurnStart { agent, turn } => (agent, AgentEvent::TurnStart { turn }),
        Frame::MessageStart { agent } => (agent, AgentEvent::MessageStart),
        Frame::Stream { agent, event } => {
            let event = serde_json::to_value(event).ok()?;
            (agent, AgentEvent::Stream(serde_json::from_value(event).ok()?))
        }
        Frame::MessageEnd { agent, message } => (
            agent,
            AgentEvent::MessageEnd {
                message: serde_json::from_value(message).ok()?,
            },
        ),
        Frame::ToolStart {
            agent,
            call_id,
            name,
            input,
        } => (agent, AgentEvent::ToolStart { call_id, name, input }),
        Frame::ToolUpdate { agent, call_id, text } => (agent, AgentEvent::ToolUpdate { call_id, text }),
        Frame::ToolEnd {
            agent,
            call_id,
            output,
            duration_ms,
        } => {
            let output = ToolOutput {
                content: output
                    .content
                    .into_iter()
                    .filter_map(|block| serde_json::from_value(block).ok())
                    .collect(),
                is_error: output.is_error,
                details: output.details,
            };
            (
                agent,
                AgentEvent::ToolEnd {
                    call_id,
                    output,
                    duration_ms,
                },
            )
        }
        Frame::TurnEnd { agent } => (agent, AgentEvent::TurnEnd),
        Frame::Retry {
            agent,
            attempt,
            model: asked,
            stalled_secs,
        } => (
            agent,
            AgentEvent::Retry {
                attempt,
                model: model(&asked)?,
                stalled_secs,
            },
        ),
        Frame::Fallback { agent, from, to } => (
            agent,
            AgentEvent::Fallback {
                from: model(&from)?,
                to: model(&to)?,
            },
        ),
        Frame::Account {
            agent,
            provider,
            from,
            to,
            until,
            limit,
        } => (
            agent,
            AgentEvent::Account {
                provider,
                from,
                to,
                until,
                limit,
            },
        ),
        Frame::TierDropped {
            agent,
            model: asked,
            tier,
        } => (
            agent,
            AgentEvent::TierDropped {
                model: model(&asked)?,
                tier: tier.parse().ok()?,
            },
        ),
        Frame::Rule { agent, name } => (agent, AgentEvent::Rule { name }),
        Frame::Compaction {
            agent,
            tokens_before,
            tokens_after,
        } => (
            agent,
            AgentEvent::Compaction {
                tokens_before,
                tokens_after,
            },
        ),
        Frame::Idle { agent, stop } => (
            agent,
            AgentEvent::Idle {
                stop: serde_json::from_value(Value::String(stop)).ok()?,
            },
        ),
        Frame::Failed { agent, error } => (agent, AgentEvent::Failed { error }),
        _ => return None,
    })
}

/// The lead's events that stand for what was in flight when the UI attached: the reply being
/// written so far, and the tool calls that run.
fn in_flight(snapshot: &Snapshot) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    if let Some(reply) = snapshot.streaming.iter().find(|reply| reply.agent == LEAD) {
        events.push(AgentEvent::MessageStart);
        if !reply.thinking.is_empty() {
            events.push(AgentEvent::Stream(StreamEvent::ThinkingDelta {
                index: 0,
                delta: reply.thinking.clone(),
            }));
        }
        if !reply.text.is_empty() {
            events.push(AgentEvent::Stream(StreamEvent::TextDelta {
                index: 0,
                delta: reply.text.clone(),
            }));
        }
    }
    for tool in snapshot.tools.iter().filter(|tool| tool.agent == LEAD) {
        events.push(AgentEvent::ToolStart {
            call_id: tool.call_id.clone(),
            name: tool.name.clone(),
            input: tool.input.clone(),
        });
    }
    events
}

/// A member of the roster as the session's own process would list it. `since` is how long ago
/// the roster arrived.
fn member_info(member: &Member, since: Duration) -> MemberInfo {
    let state = match member.state.as_str() {
        "queued" => MemberState::Queued,
        "running" => MemberState::Running {
            activity: member.activity.clone().unwrap_or_else(|| "working".to_string()),
        },
        "waiting for approval" => MemberState::WaitingApproval,
        "done" => MemberState::Done,
        "failed" => MemberState::Failed(String::new()),
        "stopped" => MemberState::Stopped,
        _ => MemberState::Idle,
    };
    let running = state.is_active();
    MemberInfo {
        id: member.id.clone(),
        def: member.def.clone(),
        parent: member.parent.clone(),
        depth: member.depth,
        elapsed: Duration::from_millis(member.elapsed_ms) + if running { since } else { Duration::ZERO },
        tokens: member.tokens,
        cost_usd: member.cost_usd,
        state,
    }
}

fn summary(row: SessionRow) -> (SessionSummary, Vec<String>) {
    let summary = SessionSummary {
        path: PathBuf::from(row.path),
        id: row.id,
        timestamp: row.created,
        updated: row.updated,
        name: row.name,
        first_prompt: row.first_prompt,
        messages: row.messages as usize,
        pinned: row.pinned,
        usage: Usage {
            input: row.usage.input,
            output: row.usage.output,
            cache_read: row.usage.cache_read,
            cache_write: row.usage.cache_write,
            reasoning: row.usage.reasoning,
            cost_usd: row.usage.cost_usd,
            cache_ttl_secs: None,
        },
        prompts: row.prompts as usize,
        models: Default::default(),
        tools: Default::default(),
        cwd: String::new(),
        archived: false,
    };
    (summary, row.matches)
}

/// A copy of the session to read.
async fn copy_of(client: &Client) -> Result<SessionStore, String> {
    let copy: SessionCopy = ask(client, Command::GetSession).await?;
    let header: Header = parsed(copy.header)?;
    let entries: Vec<Entry> = copy
        .entries
        .into_iter()
        .filter_map(|entry| serde_json::from_value(entry).ok())
        .collect();
    Ok(SessionStore::mirror(
        header,
        entries,
        copy.leaf,
        PathBuf::from(copy.path),
    ))
}

/// Asks the session where it stands, unless that is being asked already.
fn refresh(client: &Arc<Client>, shared: &Arc<Shared>) {
    if shared.refreshing.swap(true, Ordering::SeqCst) {
        return;
    }
    let (client, shared) = (client.clone(), shared.clone());
    tokio::spawn(async move {
        let state = ask::<State>(&client, Command::GetState).await;
        shared.refreshing.store(false, Ordering::SeqCst);
        if let Ok(state) = state {
            *locked(&shared.roster) = (state.agents.clone(), Instant::now());
            *locked(&shared.state) = state;
        }
    });
}

/// Notes a request the UI is about to show. The token is cancelled when the session says the
/// request no longer waits.
fn asked(shared: &Shared, request_id: &str) -> CancellationToken {
    let closed = CancellationToken::new();
    locked(&shared.asked).insert(request_id.to_string(), closed.clone());
    closed
}

/// Waits for what the UI answers a request with. `None` when the session said meanwhile that
/// the request no longer waits: giving the answer up here takes the request off the screen.
async fn answered<T>(
    shared: &Shared,
    request_id: &str,
    closed: CancellationToken,
    reply: oneshot::Receiver<T>,
) -> Option<Result<T, ()>> {
    let answer = tokio::select! {
        answer = reply => Some(answer.map_err(|_| ())),
        _ = closed.cancelled() => None,
    };
    locked(&shared.asked).remove(request_id);
    answer
}

/// Answers a request once the UI has: `value` turns what the user chose into the `value` of a
/// `ui_response`; a request that was dismissed is answered with `null`.
fn answer_later<T: Send + 'static>(
    client: &Arc<Client>,
    shared: &Arc<Shared>,
    request_id: String,
    reply: oneshot::Receiver<T>,
    value: impl FnOnce(T) -> Value + Send + 'static,
) {
    let (client, shared) = (client.clone(), shared.clone());
    let closed = asked(&shared, &request_id);
    tokio::spawn(async move {
        let Some(answer) = answered(&shared, &request_id, closed, reply).await else {
            return;
        };
        let value = answer.map(value).unwrap_or(Value::Null);
        let _ = client.send(Command::UiResponse { request_id, value }).await;
    });
}

/// A question of the session as the UI asks it.
fn question(asked: protocol::Question) -> Question {
    let kind = match asked.kind() {
        protocol::AskKind::Select => AskKind::Select,
        protocol::AskKind::Multiselect => AskKind::Multiselect,
        protocol::AskKind::Text => AskKind::Text,
        protocol::AskKind::Confirm => AskKind::Confirm,
    };
    Question {
        text: asked.question,
        header: asked.header,
        context: asked.context,
        excerpt: asked.code.map(Excerpt::Code).or(asked.diff.map(Excerpt::Diff)),
        kind,
        options: asked.options,
        recommended: asked.recommended,
        allow_freeform: asked.allow_freeform,
    }
}

/// What the user answered to a question of `kind`, as the `value` the session expects.
fn answer_value(kind: AskKind, chosen: Vec<String>) -> Value {
    match kind {
        AskKind::Text => json!(chosen.first().cloned().unwrap_or_default()),
        AskKind::Confirm => json!(chosen.first().is_some_and(|said| said == "yes")),
        AskKind::Select | AskKind::Multiselect => json!(chosen),
    }
}

/// Hands a request of the session to the UI, and its answer back to the session.
fn request(
    client: &Arc<Client>,
    shared: &Arc<Shared>,
    request_id: String,
    asked: protocol::UiRequest,
) -> UiRequest {
    match asked {
        protocol::UiRequest::Notify { level, text } => UiRequest::Notify {
            level: match level.as_str() {
                "warn" => Level::Warn,
                "error" => Level::Error,
                _ => Level::Info,
            },
            text,
        },
        protocol::UiRequest::Ask(asked) => {
            let (reply, answer) = oneshot::channel();
            let question = question(asked);
            let kind = question.kind;
            answer_later(
                client,
                shared,
                request_id,
                answer,
                move |chosen: Result<Vec<String>, UiError>| match chosen {
                    Ok(chosen) => answer_value(kind, chosen),
                    Err(_) => Value::Null,
                },
            );
            UiRequest::Ask { question, reply }
        }
        protocol::UiRequest::Form { questions } => {
            let (reply, answer) = oneshot::channel();
            let questions: Vec<Question> = questions.into_iter().map(question).collect();
            let kinds: Vec<AskKind> = questions.iter().map(|question| question.kind).collect();
            answer_later(
                client,
                shared,
                request_id,
                answer,
                move |answers: Result<Vec<Vec<String>>, UiError>| match answers {
                    // A question left open has no value of its kind.
                    Ok(answers) => Value::Array(
                        kinds
                            .iter()
                            .zip(answers)
                            .map(|(kind, chosen)| match chosen.is_empty() {
                                true => Value::Null,
                                false => answer_value(*kind, chosen),
                            })
                            .collect(),
                    ),
                    Err(_) => Value::Null,
                },
            );
            UiRequest::Form { questions, reply }
        }
        protocol::UiRequest::Confirm { prompt } => {
            let (reply, answer) = oneshot::channel();
            answer_later(
                client,
                shared,
                request_id,
                answer,
                |confirmed: Result<bool, UiError>| match confirmed {
                    Ok(confirmed) => json!(confirmed),
                    Err(_) => Value::Null,
                },
            );
            UiRequest::Confirm { prompt, reply }
        }
        protocol::UiRequest::Select { prompt, options } => {
            let (reply, answer) = oneshot::channel();
            answer_later(
                client,
                shared,
                request_id,
                answer,
                |chosen: Result<usize, UiError>| match chosen {
                    Ok(index) => json!(index),
                    Err(_) => Value::Null,
                },
            );
            UiRequest::Select {
                prompt,
                options,
                reply,
            }
        }
        protocol::UiRequest::Input { prompt, secret } => {
            let (reply, answer) = oneshot::channel();
            answer_later(
                client,
                shared,
                request_id,
                answer,
                |typed: Result<String, UiError>| match typed {
                    Ok(typed) => json!(typed),
                    Err(_) => Value::Null,
                },
            );
            UiRequest::Input {
                prompt,
                secret,
                reply,
            }
        }
        protocol::UiRequest::Plan { path, plan } => {
            let (reply, answer) = oneshot::channel();
            answer_later(
                client,
                shared,
                request_id,
                answer,
                |decision: Result<PlanDecision, UiError>| match decision {
                    Ok(PlanDecision::Approve) => json!(true),
                    Ok(PlanDecision::Revise(feedback)) => json!(feedback),
                    Err(_) => Value::Null,
                },
            );
            UiRequest::Plan {
                path: PathBuf::from(path),
                plan,
                reply,
            }
        }
    }
}

/// Turns the session's frames into what the UI takes in, until the connection ends.
async fn pump(
    client: Arc<Client>,
    shared: Arc<Shared>,
    incoming: mpsc::UnboundedSender<Incoming>,
    ended: String,
) {
    let mut frames = client.frames();
    while let Some(frame) = frames.next().await {
        let sent = match frame {
            Frame::Roster { members } => {
                *locked(&shared.roster) = (members, Instant::now());
                refresh(&client, &shared);
                incoming.send(Incoming::Team(TeamEvent::Roster))
            }
            Frame::ApprovalRequest {
                request_id,
                agent,
                tool,
                access,
                subjects,
                input,
                rule,
                note,
                destructive,
                secret,
            } => {
                let asked = ApprovalRequest {
                    agent: AgentInfo {
                        def: agent.clone(),
                        id: agent,
                        parent: None,
                        depth: 0,
                        session_id: String::new(),
                    },
                    tool,
                    access: match access.as_str() {
                        "read" => Access::Read,
                        "write" => Access::Write,
                        "net" => Access::Net,
                        _ => Access::Exec,
                    },
                    subjects,
                    input,
                    rule,
                    note,
                    destructive,
                    secret,
                };
                let (reply, answer) = oneshot::channel::<Option<ApprovalDecision>>();
                let (answering, asked_of) = (client.clone(), shared.clone());
                let closed = self::asked(&shared, &request_id);
                tokio::spawn(async move {
                    let Some(answer) = answered(&asked_of, &request_id, closed, answer).await else {
                        return;
                    };
                    // An approval nobody gave is a refusal, as it is with nobody there.
                    let (decision, feedback) = match answer.ok().flatten() {
                        Some(ApprovalDecision::AllowOnce) => (Decision::Allow, None),
                        Some(ApprovalDecision::AllowSession) => (Decision::Session, None),
                        Some(ApprovalDecision::Always) => (Decision::Always, None),
                        Some(ApprovalDecision::Deny { feedback }) => (Decision::Deny, feedback),
                        None => (Decision::Deny, None),
                    };
                    let _ = answering
                        .send(Command::ApprovalResponse {
                            request_id,
                            decision,
                            feedback,
                        })
                        .await;
                });
                refresh(&client, &shared);
                incoming.send(Incoming::Request(UiRequest::Approval {
                    request: Box::new(asked),
                    reply,
                }))
            }
            Frame::UiRequest {
                request_id,
                request: asked,
            } => incoming.send(Incoming::Request(request(&client, &shared, request_id, asked))),
            Frame::RequestClosed { request_id } => {
                if let Some(closed) = locked(&shared.asked).get(&request_id) {
                    closed.cancel();
                }
                Ok(())
            }
            // What a sub-agent was handed belongs to its part of the fleet view.
            Frame::UserInput {
                text,
                agent: Some(id),
                ..
            } => incoming.send(Incoming::Team(TeamEvent::Input { id, text })),
            Frame::UserInput {
                kind,
                text,
                agent: None,
            } => {
                shared.stale.store(true, Ordering::SeqCst);
                incoming.send(Incoming::UserInput { kind, text })
            }
            Frame::BtwDelta { delta, .. } => {
                if let Some((id, updates)) = locked(&shared.btw).as_ref() {
                    let _ = updates.send(Internal::Btw(btw::Update::Text { id: *id, delta }));
                }
                Ok(())
            }
            frame => {
                let Some((agent, event)) = agent_event(frame) else {
                    continue;
                };
                // Text as it is written changes nothing that is kept.
                if !matches!(event, AgentEvent::Stream(_) | AgentEvent::ToolUpdate { .. }) {
                    shared.stale.store(true, Ordering::SeqCst);
                }
                let turning_point = matches!(
                    event,
                    AgentEvent::AgentStart
                        | AgentEvent::MessageEnd { .. }
                        | AgentEvent::ToolEnd { .. }
                        | AgentEvent::Compaction { .. }
                        | AgentEvent::Idle { .. }
                        | AgentEvent::Failed { .. }
                );
                if turning_point {
                    refresh(&client, &shared);
                }
                incoming.send(if agent == LEAD {
                    Incoming::Lead(event)
                } else {
                    Incoming::Team(TeamEvent::Agent {
                        id: agent,
                        event: Box::new(event),
                    })
                })
            }
        };
        if sent.is_err() {
            return;
        }
    }
    let _ = incoming.send(Incoming::Closed(ended));
}

impl Remote {
    /// Takes the session at the other end of `client` over for the UI. `place` says where it
    /// runs; a background session is attached to first.
    pub(crate) async fn open(client: Client, place: Place) -> Result<Remote, String> {
        let client = Arc::new(client);
        let mut snapshot = None;
        let ended = if place.background().is_some() {
            let attached: Attached = ask(
                &client,
                Command::Attach {
                    pid: Some(std::process::id()),
                    // The UI shows where the session stands from the session itself.
                    replay: Replay::None,
                    since: None,
                },
            )
            .await?;
            snapshot = Some(attached.snapshot);
            format!("the background session {} has ended", attached.host)
        } else {
            format!("the session {} has ended", place.name())
        };
        let state: State = ask(&client, Command::GetState).await?;
        let commands: Vec<CommandRow> = ask(&client, Command::ListCommands).await?;
        let session = Arc::new(copy_of(&client).await?);
        let shared = Arc::new(Shared {
            roster: Mutex::new((state.agents.clone(), Instant::now())),
            state: Mutex::new(state),
            stale: AtomicBool::new(false),
            refreshing: AtomicBool::new(false),
            btw: Mutex::new(None),
            asked: Mutex::new(HashMap::new()),
        });
        let (sender, incoming) = mpsc::unbounded_channel();
        // What was in flight when the UI attached is shown as if it had been seen to start.
        for event in snapshot.iter().flat_map(in_flight) {
            let _ = sender.send(Incoming::Lead(event));
        }
        tokio::spawn(pump(client.clone(), shared.clone(), sender, ended));
        Ok(Remote {
            client,
            incoming,
            shared,
            place,
            session,
            commands: commands
                .into_iter()
                .map(|row| (row.name, row.description))
                .collect(),
        })
    }

    fn state(&self) -> State {
        locked(&self.shared.state).clone()
    }

    /// Sends a command nobody waits for.
    fn tell(&self, command: Command) {
        let client = self.client.clone();
        tokio::spawn(async move {
            let _ = client.send(command).await;
        });
    }

    /// Takes in the state a session command answered with: the conversation is elsewhere now.
    fn moved_to(&mut self, state: State, resumed: bool) -> Moved {
        *locked(&self.shared.roster) = (state.agents.clone(), Instant::now());
        let interrupted = state.interrupted.clone();
        *locked(&self.shared.state) = state;
        self.shared.stale.store(true, Ordering::SeqCst);
        Moved {
            resumed,
            notice: None,
            interrupted,
        }
    }
}

impl Remote {
    /// Reads the session's state again, after a change that may have moved the lead.
    async fn refresh_state(&mut self) {
        if let Ok(state) = ask::<State>(&self.client, Command::GetState).await {
            *locked(&self.shared.state) = state;
            self.shared.stale.store(true, Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl Backend for Remote {
    fn place(&self) -> Option<String> {
        Some(self.place.name())
    }

    fn farewell(&self) -> Option<String> {
        Some(self.place.farewell())
    }

    fn reopen(&self) -> Result<Vec<std::ffi::OsString>, String> {
        Ok(self.place.reopen().iter().map(Into::into).collect())
    }

    async fn next(&mut self) -> Incoming {
        match self.incoming.recv().await {
            Some(incoming) => incoming,
            None => Incoming::Closed(format!("the session {} has ended", self.place.name())),
        }
    }

    fn opened(&mut self) -> Moved {
        let has_messages = self
            .session
            .entries()
            .iter()
            .any(|entry| matches!(entry.kind, EntryKind::Message { .. }));
        Moved {
            resumed: has_messages,
            notice: None,
            interrupted: self.state().interrupted,
        }
    }

    fn status(&self) -> AgentStatus {
        let state = self.state();
        AgentStatus {
            busy: state.busy,
            turn: state.turn,
            model: state.model.as_deref().and_then(|model| model.parse().ok()),
            via: state.via.as_deref().and_then(|via| via.parse().ok()),
            chain: state
                .chain
                .iter()
                .filter_map(|spec| spec.parse::<ModelSpec>().ok())
                .collect(),
            thinking: state.thinking.parse().unwrap_or(ThinkingLevel::Off),
            // What the attached session chose is its own to keep.
            thinking_set: None,
            context_tokens: state.context_tokens,
            context_window: state.context_window,
            usage: Usage {
                input: state.usage.input,
                output: state.usage.output,
                cache_read: state.usage.cache_read,
                cache_write: state.usage.cache_write,
                reasoning: state.usage.reasoning,
                cost_usd: state.usage.cost_usd,
                cache_ttl_secs: None,
            },
            requests: state.requests,
        }
    }

    fn model_label(&self) -> String {
        let state = self.state();
        if let Some(model) = state.model {
            return crate::reports::routed_label(state.via.as_deref(), &model);
        }
        // Nothing has answered yet: what the chain starts with, a role as what it stands for.
        match state.chain.first() {
            Some(first) => match first.strip_prefix('@') {
                Some(role) => state.roles.get(role).cloned().unwrap_or_else(|| first.clone()),
                None => first.clone(),
            },
            None => String::new(),
        }
    }

    fn has_model(&self) -> bool {
        // A session without one does not start where there is nobody to set one up.
        true
    }

    fn mode(&self) -> PermissionMode {
        self.state().mode.parse().unwrap_or(PermissionMode::Manual)
    }

    fn cwd(&self) -> PathBuf {
        PathBuf::from(self.state().cwd)
    }

    fn branch(&self) -> Option<String> {
        self.state().branch
    }

    fn facts(&self) -> Facts {
        let state = self.state();
        Facts {
            coordinator: state.coordinator,
            tier: state
                .tier
                .as_deref()
                .and_then(|tier| crate::routing::parse_tier(tier).ok().flatten()),
            goal: state.goal,
            todo: state
                .todo
                .into_iter()
                .map(|item| TodoItem {
                    text: item.text,
                    status: match item.status.as_str() {
                        "in_progress" => TodoStatus::InProgress,
                        "done" => TodoStatus::Done,
                        _ => TodoStatus::Pending,
                    },
                })
                .collect(),
            // How the other process pays is not part of its state: what it reports as cost
            // is shown, and nothing when it reports none.
            billing: Billing::Metered,
            // The other process says so in a notice of its own.
            waiting: None,
            account: None,
            account_email: None,
        }
    }

    fn roster(&self) -> Vec<MemberInfo> {
        let roster = locked(&self.shared.roster);
        let since = roster.1.elapsed();
        roster.0.iter().map(|member| member_info(member, since)).collect()
    }

    fn commands(&self) -> Vec<(String, String)> {
        self.commands.clone()
    }

    fn thinking_levels(&self) -> Vec<ThinkingLevel> {
        Vec::new()
    }

    fn resolves(&self, _spec: &str) -> bool {
        // The session says so when it is asked to use it.
        true
    }

    fn cycle_roles(&self) -> Vec<String> {
        let roles = self.state().roles.into_iter();
        // A role is known here by the model it answers with first.
        let models = |model: String| Vec::from_iter((!model.is_empty()).then_some(model));
        super::backend::cycle_of(roles.map(|(role, model)| (role, models(model))).collect())
    }

    async fn session(&mut self) -> Arc<SessionStore> {
        if self.shared.stale.swap(false, Ordering::SeqCst) {
            match copy_of(&self.client).await {
                Ok(copy) => self.session = Arc::new(copy),
                // What was read before is better than nothing; the next look asks again.
                Err(_) => self.shared.stale.store(true, Ordering::SeqCst),
            }
        }
        self.session.clone()
    }

    async fn sessions(&mut self, query: Option<&str>) -> Result<Vec<(SessionSummary, Vec<String>)>, String> {
        let command = Command::ListSessions {
            query: query.map(str::to_string),
        };
        let rows: Vec<SessionRow> = ask(&self.client, command).await?;
        Ok(rows.into_iter().map(summary).collect())
    }

    async fn models(&mut self) -> Vec<(String, String)> {
        let state = self.state();
        let mut rows: Vec<(String, String)> = state
            .roles
            .into_iter()
            .map(|(role, model)| (format!("@{role}"), model))
            .collect();
        rows.extend(
            state
                .virtual_models
                .into_iter()
                .map(|name| (name, crate::models::VIRTUAL_DETAIL.to_string())),
        );
        let models: Vec<ModelRow> = ask(&self.client, Command::ListModels { all: false })
            .await
            .unwrap_or_default();
        rows.extend(models.into_iter().map(|model| {
            let cost = model.cost.map(|cost| azazel_ai::Cost {
                input: cost.input,
                output: cost.output,
                cache_read: 0.0,
                cache_write: 0.0,
            });
            let detail = crate::settings::model_detail(model.context_window, cost.as_ref(), model.login);
            (model.model, detail)
        }));
        rows
    }

    fn current_model(&self) -> Option<String> {
        let state = self.state();
        // What the chain begins with, else the model that answered last.
        let first = match state.chain.first()?.parse::<ModelSpec>().ok()? {
            ModelSpec::Model(model) => Some(format!("{}/{}", model.provider, model.model)),
            ModelSpec::Role(role) => state.roles.get(&role).cloned(),
        };
        first.or(state.model)
    }

    async fn model_rows(&mut self) -> Result<Vec<ModelRow>, String> {
        ask(&self.client, Command::ListModels { all: true }).await
    }

    async fn providers(&mut self) -> Result<Vec<ProviderRow>, String> {
        ask(&self.client, Command::ListProviders).await
    }

    fn refresh_models(&mut self, provider: String, done: mpsc::UnboundedSender<Internal>) {
        let client = self.client.clone();
        tokio::spawn(async move {
            let command = Command::RefreshModels {
                provider: provider.clone(),
            };
            let result = ask::<Vec<ModelRow>>(&client, command).await;
            let _ = done.send(Internal::Models(super::models::Update::Refreshed {
                provider,
                result,
            }));
        });
    }

    async fn prompt(&mut self, text: String, attached: Vec<Block>) -> Result<(), String> {
        let command = Command::Prompt {
            text,
            images: images(attached),
            request_id: None,
        };
        self.client
            .send(command)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn continue_run(&mut self) -> Result<(), String> {
        self.client
            .send(Command::ContinueRun)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn steer(&mut self, text: String, attached: Vec<Block>) {
        self.tell(Command::Steer {
            text,
            images: images(attached),
        });
    }

    fn abort(&mut self) {
        self.tell(Command::Abort);
    }

    fn compact(&mut self, instructions: Option<String>) {
        self.tell(Command::Compact { instructions });
    }

    fn set_chain(&mut self, chain: Vec<ModelSpec>) {
        self.tell(Command::SetModel {
            chain: chain.iter().map(ToString::to_string).collect(),
        });
    }

    async fn set_model(&mut self, specs: &[String]) -> Result<Vec<ModelSpec>, String> {
        let command = Command::SetModel {
            chain: specs.to_vec(),
        };
        self.client.send(command).await.map_err(|e| e.to_string())?;
        // The session says what the names it was given stand for.
        let state: State = ask(&self.client, Command::GetState).await?;
        let chain = state
            .chain
            .iter()
            .filter_map(|spec| spec.parse::<ModelSpec>().ok())
            .collect();
        *locked(&self.shared.state) = state;
        self.shared.stale.store(true, Ordering::SeqCst);
        Ok(chain)
    }

    async fn roles(&mut self) -> Result<Roles, String> {
        ask(&self.client, Command::ListRoles).await
    }

    async fn set_role(
        &mut self,
        role: &str,
        chain: Option<Vec<String>>,
        save: SaveTo,
    ) -> Result<Roles, String> {
        let command = Command::SetRole {
            role: role.to_string(),
            chain,
            save,
        };
        let roles = ask(&self.client, command).await?;
        // The lead may have gone back to following its role.
        self.refresh_state().await;
        Ok(roles)
    }

    async fn save_preset(&mut self, name: &str) -> Result<Roles, String> {
        ask(
            &self.client,
            Command::SavePreset {
                name: name.to_string(),
            },
        )
        .await
    }

    async fn apply_preset(&mut self, name: &str) -> Result<Roles, String> {
        let roles = ask(
            &self.client,
            Command::ApplyPreset {
                name: name.to_string(),
            },
        )
        .await?;
        self.refresh_state().await;
        Ok(roles)
    }

    async fn set_thinking(&mut self, level: ThinkingLevel) -> Result<(), String> {
        let command = Command::SetThinking {
            level: level.to_string(),
        };
        self.client.send(command).await.map_err(|e| e.to_string())?;
        locked(&self.shared.state).thinking = level.to_string();
        self.shared.stale.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn set_mode(&mut self, mode: PermissionMode) -> Result<(), String> {
        let command = Command::SetMode {
            mode: mode.to_string(),
        };
        self.client.send(command).await.map_err(|e| e.to_string())?;
        locked(&self.shared.state).mode = mode.to_string();
        Ok(())
    }

    fn agents(&self) -> (Vec<azazel_protocol::PrimaryAgent>, String) {
        let state = self.state();
        (state.primary_agents, state.agent)
    }

    async fn set_agent(&mut self, name: &str) -> Result<(), String> {
        let command = Command::SetAgent {
            agent: name.to_string(),
        };
        self.client.send(command).await.map_err(|e| e.to_string())?;
        locked(&self.shared.state).agent = name.to_string();
        Ok(())
    }

    async fn set_name(&mut self, name: String) -> Result<(), String> {
        self.client
            .send(Command::SetName { name: name.clone() })
            .await
            .map_err(|e| e.to_string())?;
        locked(&self.shared.state).name = Some(name);
        self.shared.stale.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn set_label(&mut self, entry: &str, label: &str) -> Result<(), String> {
        let command = Command::SetLabel {
            entry_id: entry.to_string(),
            label: label.to_string(),
        };
        self.client.send(command).await.map_err(|e| e.to_string())?;
        self.shared.stale.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn go(&mut self, to: Go) -> Result<Moved, String> {
        let (command, resumed) = match to {
            Go::New => (Command::NewSession { name: None }, false),
            Go::File(path) => (
                Command::ResumeSession {
                    session_id: None,
                    path: Some(path.to_string_lossy().into_owned()),
                },
                true,
            ),
            Go::Fork => (Command::ForkSession { entry_id: None }, true),
            Go::Leaf { entry, before } => (
                Command::SetLeaf {
                    entry_id: entry,
                    before,
                },
                true,
            ),
        };
        let state: State = ask(&self.client, command).await?;
        Ok(self.moved_to(state, resumed))
    }

    async fn rewind_points(&mut self) -> Result<Vec<(RewindPoint, String)>, String> {
        let session = self.session().await;
        let points = slash::rewind_points(&session);
        if points.is_empty() {
            return Err("nothing to rewind to: this session has no checkpoint".to_string());
        }
        // What changed in the files since each is known where the files are.
        Ok(points.into_iter().map(|point| (point, String::new())).collect())
    }

    async fn rewind(
        &mut self,
        point: &RewindPoint,
        code: bool,
        conversation: bool,
    ) -> Result<Option<Moved>, String> {
        let command = Command::Rewind {
            entry_id: point.entry_id.clone(),
            what: match (code, conversation) {
                (true, true) => RewindWhat::Both,
                (true, false) => RewindWhat::Code,
                _ => RewindWhat::Conversation,
            },
        };
        self.client.send(command).await.map_err(|e| e.to_string())?;
        let state: State = ask(&self.client, Command::GetState).await?;
        let moved = self.moved_to(state, true);
        Ok(conversation.then_some(moved))
    }

    async fn retry(&mut self, _again: Again) -> Result<Moved, String> {
        // The session finds its last prompt itself.
        self.client
            .send(Command::Retry)
            .await
            .map_err(|e| e.to_string())?;
        let state: State = ask(&self.client, Command::GetState).await?;
        Ok(self.moved_to(state, true))
    }

    /// The machine the session runs on writes the file, with its own `tui.redact`.
    async fn export(&mut self, path: &str, _redact: bool) -> Result<String, String> {
        let command = Command::Export {
            format: None,
            path: Some(path.to_string()).filter(|path| !path.is_empty()),
        };
        let exported: Exported = ask(&self.client, command).await?;
        Ok(exported.path)
    }

    fn shell(&mut self, command: String, share: bool, done: mpsc::UnboundedSender<Internal>) {
        let client = self.client.clone();
        tokio::spawn(async move {
            let ran: Result<ShellResult, String> = ask(&client, Command::Shell { command, share }).await;
            let (output, status) = match ran {
                Ok(ran) => (ran.output, ran.status),
                Err(why) => (String::new(), why),
            };
            let _ = done.send(Internal::Shell { output, status });
        });
    }

    fn command(&mut self, name: &str, args: &str, done: mpsc::UnboundedSender<Internal>) {
        let client = self.client.clone();
        let command = Command::RunCommand {
            name: name.to_string(),
            args: args.to_string(),
            only: None,
        };
        tokio::spawn(async move {
            let outcome = match ask::<CommandResult>(&client, command).await {
                Ok(CommandResult::Show { text }) => CommandOutcome::Show(text),
                // A prompt the command made is on its way to the lead already.
                Ok(CommandResult::Done | CommandResult::Prompt { .. }) => CommandOutcome::Done,
                Err(why) => CommandOutcome::Error(why),
            };
            let _ = done.send(Internal::Command(outcome));
        });
    }

    async fn report(&mut self, name: &str, args: &str) -> Option<Result<String, String>> {
        crate::reports::REPORTS
            .iter()
            .any(|(report, _)| *report == name)
            .then_some(())?;
        let command = Command::RunCommand {
            name: name.to_string(),
            args: args.to_string(),
            only: None,
        };
        Some(match ask::<CommandResult>(&self.client, command).await {
            Ok(CommandResult::Show { text }) => Ok(text),
            Ok(_) => Ok(String::new()),
            Err(why) => Err(why),
        })
    }

    fn btw(
        &mut self,
        id: u64,
        question: String,
        earlier: Vec<Exchange>,
        updates: mpsc::UnboundedSender<Internal>,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        // The protocol's side question stands alone: what was asked before goes along in it.
        let mut asked = String::new();
        for exchange in &earlier {
            asked.push_str(&format!(
                "Earlier I asked on the side: {}\nYou answered: {}\n\n",
                exchange.question.trim(),
                exchange.answer.trim()
            ));
        }
        asked.push_str(&question);
        *locked(&self.shared.btw) = Some((id, updates.clone()));
        let (client, shared) = (self.client.clone(), self.shared.clone());
        tokio::spawn(async move {
            let answered = tokio::select! {
                answered = ask::<BtwAnswer>(&client, Command::Btw { question: asked }) => answered,
                _ = cancel.cancelled() => Err("cancelled".to_string()),
            };
            let result = answered.map(|answer| SideAnswer {
                text: answer.answer,
                model: answer
                    .model
                    .parse()
                    .unwrap_or_else(|_| ModelRef::new("remote", &answer.model)),
                usage: Usage::default(),
                dropped_tools: answer.dropped_tools,
            });
            let mut sink = locked(&shared.btw);
            if sink.as_ref().is_some_and(|(current, _)| *current == id) {
                *sink = None;
            }
            drop(sink);
            let _ = updates.send(Internal::Btw(btw::Update::Done { id, result }));
        });
        Ok(())
    }

    fn list_files(&mut self, done: mpsc::UnboundedSender<Internal>) {
        let client = self.client.clone();
        tokio::spawn(async move {
            if let Ok(files) = ask::<Vec<String>>(&client, Command::ListFiles).await {
                let _ = done.send(Internal::Files(files));
            }
        });
    }

    async fn stop_agent(&mut self, id: &str) -> Result<Vec<String>, String> {
        let command = Command::AgentStop {
            agent: id.to_string(),
        };
        let stopped: Value = ask(&self.client, command).await?;
        parsed(stopped["agents"].clone())
    }

    async fn message_agent(&mut self, id: &str, message: &str) -> Result<Vec<String>, String> {
        let command = Command::AgentSend {
            to: id.to_string(),
            message: message.to_string(),
        };
        let reached: Value = ask(&self.client, command).await?;
        parsed(reached["agents"].clone())
    }

    async fn mention(&mut self, name: &str, text: &str) -> Option<Result<Mentioned, String>> {
        let command = Command::AgentMention {
            name: name.to_string(),
            text: text.to_string(),
        };
        match ask::<Option<protocol::Mentioned>>(&self.client, command).await {
            Ok(mentioned) => mentioned.map(|mentioned| {
                Ok(Mentioned {
                    agent: mentioned.agent,
                    started: mentioned.started,
                })
            }),
            Err(why) => Some(Err(why)),
        }
    }

    async fn finish(&mut self) {
        // Closing the connection leaves a background session running and ends one that was
        // started for this connection.
        match self.client.close().await {
            Ok(_) | Err(Error::Closed) => {}
            Err(e) => tracing::debug!("closing the connection to the session {}: {e}", self.place.name()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_roster_member_reads_as_the_session_lists_it() {
        let member = Member {
            id: "scout-1".into(),
            def: "scout".into(),
            parent: Some("lead".into()),
            depth: 1,
            state: "running".into(),
            elapsed_ms: 2_000,
            tokens: 40,
            cost_usd: None,
            activity: Some("reading src/lib.rs".into()),
        };
        let info = member_info(&member, Duration::from_secs(3));
        assert_eq!(
            info.state,
            MemberState::Running {
                activity: "reading src/lib.rs".into()
            }
        );
        // The clock of an agent at work runs on between two rosters; a finished one's stands.
        assert_eq!(info.elapsed, Duration::from_secs(5));
        let done = Member {
            state: "done".into(),
            activity: None,
            ..member
        };
        let info = member_info(&done, Duration::from_secs(3));
        assert_eq!(
            (info.state, info.elapsed),
            (MemberState::Done, Duration::from_secs(2))
        );
    }
}
