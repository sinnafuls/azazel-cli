//! A session in another process as the UI sees it: a background session on this machine, or
//! azazel on another machine reached over SSH. Everything goes through the RPC protocol.
//!
//! What the UI draws on every frame (the lead's status, the mode, the roster) is kept here as
//! the session last reported it, and brought up to date as frames arrive. What the UI reads of
//! the session's contents is a copy, fetched again once the session has changed.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_agent::{Access, AgentEvent, AgentInfo, AgentStatus, ToolOutput};
use azazel_ai::{Block, ModelRef, ModelSpec, ThinkingLevel, Usage};
use azazel_client::{Client, Error};
use azazel_core::session::{Entry, Header, SessionSummary};
use azazel_core::side::SideAnswer;
use azazel_core::{
    ApprovalDecision, ApprovalRequest, CommandOutcome, EntryKind, Level, PermissionMode, PlanDecision,
    SessionStore, UiError,
};
use azazel_protocol::{
    self as protocol, Attached, BtwAnswer, Command, CommandResult, CommandRow, Decision, Exported, Frame,
    Member, ModelRow, Replay, RewindWhat, SessionCopy, SessionRow, ShellResult, State,
};
use azazel_team::{LEAD, MemberInfo, MemberState, TeamEvent};
use azazel_tools::{TodoItem, TodoStatus};
use azazel_tui::components::format_tokens;
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
        } => (
            agent,
            AgentEvent::Retry {
                attempt,
                model: model(&asked)?,
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
        },
        prompts: row.prompts as usize,
        models: Default::default(),
        tools: Default::default(),
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

/// Answers a request once the UI has: `value` turns what the user chose into the `value` of a
/// `ui_response`; a request that was dismissed is answered with `null`.
fn answer_later<T: Send + 'static>(
    client: &Arc<Client>,
    request_id: String,
    reply: oneshot::Receiver<T>,
    value: impl FnOnce(T) -> Value + Send + 'static,
) {
    let client = client.clone();
    tokio::spawn(async move {
        let value = reply.await.map(value).unwrap_or(Value::Null);
        let _ = client.send(Command::UiResponse { request_id, value }).await;
    });
}

/// Hands a request of the session to the UI, and its answer back to the session.
fn request(client: &Arc<Client>, request_id: String, asked: protocol::UiRequest) -> UiRequest {
    match asked {
        protocol::UiRequest::Notify { level, text } => UiRequest::Notify {
            level: match level.as_str() {
                "warn" => Level::Warn,
                "error" => Level::Error,
                _ => Level::Info,
            },
            text,
        },
        protocol::UiRequest::Ask {
            question,
            options,
            multi,
        } => {
            let (reply, answer) = oneshot::channel();
            let free = options.is_empty();
            answer_later(
                client,
                request_id,
                answer,
                move |chosen: Result<Vec<String>, UiError>| match chosen {
                    Ok(chosen) if free => json!(chosen.first().cloned().unwrap_or_default()),
                    Ok(chosen) => json!(chosen),
                    Err(_) => Value::Null,
                },
            );
            UiRequest::Ask {
                question,
                options,
                multi,
                reply,
            }
        }
        protocol::UiRequest::Confirm { prompt } => {
            let (reply, answer) = oneshot::channel();
            answer_later(
                client,
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
                };
                let (reply, answer) = oneshot::channel::<Option<ApprovalDecision>>();
                let answering = client.clone();
                tokio::spawn(async move {
                    // An approval nobody gave is a refusal, as it is with nobody there.
                    let (decision, feedback) = match answer.await.ok().flatten() {
                        Some(ApprovalDecision::AllowOnce) => (Decision::Allow, None),
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
            } => incoming.send(Incoming::Request(request(&client, request_id, asked))),
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
        let ended = if place.background().is_some() {
            let attached: Attached = ask(
                &client,
                Command::Attach {
                    pid: Some(std::process::id()),
                    // The UI shows where the session stands from the session itself.
                    replay: Replay::None,
                },
            )
            .await?;
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
        });
        let (sender, incoming) = mpsc::unbounded_channel();
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
        *locked(&self.shared.state) = state;
        self.shared.stale.store(true, Ordering::SeqCst);
        Moved {
            resumed,
            notice: None,
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
        }
    }

    fn status(&self) -> AgentStatus {
        let state = self.state();
        AgentStatus {
            busy: state.busy,
            turn: state.turn,
            model: state.model.as_deref().and_then(|model| model.parse().ok()),
            chain: state
                .chain
                .iter()
                .filter_map(|spec| spec.parse::<ModelSpec>().ok())
                .collect(),
            thinking: state.thinking.parse().unwrap_or(ThinkingLevel::Off),
            context_tokens: state.context_tokens,
            context_window: state.context_window,
            usage: Usage {
                input: state.usage.input,
                output: state.usage.output,
                cache_read: state.usage.cache_read,
                cache_write: state.usage.cache_write,
                reasoning: state.usage.reasoning,
                cost_usd: state.usage.cost_usd,
            },
            requests: state.requests,
        }
    }

    fn model_label(&self) -> String {
        let state = self.state();
        if let Some(model) = state.model {
            return model;
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
        self.state().mode.parse().unwrap_or(PermissionMode::Ask)
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
        let mut rows: Vec<(String, String)> = self
            .state()
            .roles
            .into_iter()
            .map(|(role, model)| (format!("@{role}"), model))
            .collect();
        let models: Vec<ModelRow> = ask(&self.client, Command::ListModels).await.unwrap_or_default();
        rows.extend(models.into_iter().map(|model| {
            let detail = format!("{} context", format_tokens(model.context_window));
            (model.model, detail)
        }));
        rows
    }

    async fn prompt(&mut self, text: String, attached: Vec<Block>) -> Result<(), String> {
        let command = Command::Prompt {
            text,
            images: images(attached),
        };
        self.client
            .send(command)
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

    async fn export(&mut self, path: &str) -> Result<String, String> {
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
