//! RPC mode (`azazel rpc`): a session driven by another program over standard input and output.
//!
//! Both directions carry one JSON object per line; standard output carries nothing else. The
//! lines are the types of the `azazel-protocol` crate: the client writes [`Command`]s, each with
//! an `id` that its response repeats, and reads [`Frame`]s. `docs/rpc.md` describes them for
//! people and `docs/rpc.schema.json` for programs.
//!
//! The session ends when the input closes.

pub(crate) mod admin;
pub(crate) mod session;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use azazel_agent::{AgentCmd, AgentEvent, AgentHandle, UserInput};
use azazel_ai::{Block, ModelSpec, ThinkingLevel};
use azazel_core::process::{RunOptions, run as run_command};
use azazel_core::side::{Side, SideQuestion};
use azazel_core::{
    ApprovalDecision, ApprovalRequest, AskKind, CommandOutcome, EntryKind, Excerpt, Level, PermissionMode,
    PlanDecision, Question, StartSource, Ui, UiBridge, UiError,
};
use azazel_protocol::{
    BtwAnswer, Command, CommandOrigin, CommandResult, Decision, Frame, Models, PROTOCOL, Request, Response,
    RunState, ShellResult, UiRequest,
};
use azazel_team::{Team, TeamEvent};
use futures::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::app::{Active, App};
use crate::handoff::settled;
use crate::print::{EXIT_OK, event_json};
use crate::tui::SHELL_NOTE;

/// Protocol lines on their way to the client; `None` ends the output.
#[derive(Clone)]
struct Out(mpsc::UnboundedSender<Option<String>>);

impl Out {
    fn line(&self, line: &impl Serialize) {
        if let Ok(text) = serde_json::to_string(line) {
            let _ = self.0.send(Some(text));
        }
    }

    fn respond(&self, id: Value, outcome: Result<Value, String>) {
        self.line(&Frame::Response(match outcome {
            Ok(data) => Response::ok(id, data),
            Err(error) => Response::err(id, error),
        }));
    }
}

pub(crate) fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The data of a response.
fn data(value: impl Serialize) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|e| e.to_string())
}

/// Requests the client has been sent and not yet answered.
#[derive(Default)]
pub(crate) struct Waiting {
    next: AtomicU64,
    pub(crate) approvals: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
    pub(crate) answers: Mutex<HashMap<String, oneshot::Sender<Value>>>,
}

impl Waiting {
    pub(crate) fn id(&self) -> String {
        format!("r{}", self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Whether the request is still waiting: nobody has answered it, and whoever asked has not
    /// given up.
    #[cfg(feature = "detach")]
    pub(crate) fn is_open(&self, request_id: &str) -> bool {
        let approval = locked(&self.approvals)
            .get(request_id)
            .is_some_and(|reply| !reply.is_closed());
        approval
            || locked(&self.answers)
                .get(request_id)
                .is_some_and(|reply| !reply.is_closed())
    }
}

/// Agents reach the client through this: each call becomes a request line and waits for the
/// command that answers it. When the client has gone, the call ends as it does with nobody there.
struct RpcBridge {
    out: Requests,
    waiting: Arc<Waiting>,
}

/// The lines of requests to the user, each with the number of events the lead had emitted when
/// it was made. They reach the client through the loop of [`serve_lines`], which writes each
/// one after those events of the lead and before the later ones: a `tool_start` comes before
/// its `approval_request`, and a notice given during a run before the run's `idle`.
struct Requests {
    lead: AgentHandle,
    lines: mpsc::UnboundedSender<Fenced>,
}

/// A request line and the count of lead events that go out before it.
pub(crate) type Fenced = (u64, String);

impl Requests {
    fn line(&self, frame: &Frame) {
        if let Ok(text) = serde_json::to_string(frame) {
            let _ = self.lines.send((self.lead.emitted(), text));
        }
    }
}

impl RpcBridge {
    /// Sends a `ui_request` and returns the client's `value`; `None` when the client dismissed
    /// it or went away.
    async fn request(&self, request: UiRequest) -> Option<Value> {
        let id = self.waiting.id();
        let (reply, answer) = oneshot::channel();
        locked(&self.waiting.answers).insert(id.clone(), reply);
        self.out.line(&Frame::UiRequest {
            request_id: id,
            request,
        });
        answer.await.ok().filter(|value| !value.is_null())
    }
}

/// The frame that asks the client to decide on `request`.
pub(crate) fn approval_frame(request_id: String, request: ApprovalRequest) -> Frame {
    let access = serde_json::to_value(request.access)
        .ok()
        .and_then(|access| access.as_str().map(str::to_string))
        .unwrap_or_default();
    Frame::ApprovalRequest {
        request_id,
        agent: request.agent.id,
        tool: request.tool,
        access,
        subjects: request.subjects,
        input: request.input,
        rule: request.rule,
        note: request.note,
        destructive: request.destructive,
        secret: request.secret,
    }
}

/// A question as the protocol carries it.
pub(crate) fn asked(question: &Question) -> azazel_protocol::Question {
    let (code, diff) = match &question.excerpt {
        Some(Excerpt::Code(code)) => (Some(code.clone()), None),
        Some(Excerpt::Diff(diff)) => (None, Some(diff.clone())),
        None => (None, None),
    };
    azazel_protocol::Question {
        question: question.text.clone(),
        header: question.header.clone(),
        context: question.context.clone(),
        code,
        diff,
        expects: Some(match question.kind {
            AskKind::Select => azazel_protocol::AskKind::Select,
            AskKind::Multiselect => azazel_protocol::AskKind::Multiselect,
            AskKind::Text => azazel_protocol::AskKind::Text,
            AskKind::Confirm => azazel_protocol::AskKind::Confirm,
        }),
        options: question.options.clone(),
        multi: question.kind == AskKind::Multiselect,
        recommended: question.recommended.clone(),
        allow_freeform: question.allow_freeform,
    }
}

/// What the client's `value` answers to `question`; `None` when it is no answer to it.
pub(crate) fn answered(question: &Question, value: Option<Value>) -> Option<Vec<String>> {
    if question.kind == AskKind::Confirm {
        // A client that knows no `confirm` shows a text box: the words are taken too.
        let said = match &value {
            Some(Value::Bool(yes)) => Some(*yes),
            Some(Value::String(word)) => match word.trim().to_ascii_lowercase().as_str() {
                "yes" | "y" | "true" => Some(true),
                "no" | "n" | "false" => Some(false),
                _ => None,
            },
            _ => None,
        };
        return said.map(|yes| vec![if yes { "yes" } else { "no" }.to_string()]);
    }
    match value {
        Some(Value::String(one)) => Some(vec![one]),
        Some(Value::Array(many)) => Some(
            many.into_iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect(),
        ),
        _ => None,
    }
}

pub(crate) fn level_name(level: Level) -> &'static str {
    match level {
        Level::Info => "info",
        Level::Warn => "warn",
        Level::Error => "error",
    }
}

#[async_trait]
impl UiBridge for RpcBridge {
    async fn ask(&self, question: &Question) -> Result<Vec<String>, UiError> {
        let answer = self.request(UiRequest::Ask(asked(question))).await;
        answered(question, answer).ok_or(UiError::Cancelled)
    }

    async fn ask_form(&self, questions: &[Question]) -> Result<Vec<Vec<String>>, UiError> {
        let request = UiRequest::Form {
            questions: questions.iter().map(asked).collect(),
        };
        let Some(Value::Array(mut values)) = self.request(request).await else {
            return Err(UiError::Cancelled);
        };
        // A question the client left open, or answered with what is no answer to it, has none.
        values.resize(questions.len(), Value::Null);
        Ok(questions
            .iter()
            .zip(values)
            .map(|(question, value)| answered(question, Some(value)).unwrap_or_default())
            .collect())
    }

    async fn confirm(&self, prompt: &str) -> Result<bool, UiError> {
        let request = UiRequest::Confirm {
            prompt: prompt.to_string(),
        };
        match self.request(request).await {
            Some(Value::Bool(answer)) => Ok(answer),
            _ => Err(UiError::Cancelled),
        }
    }

    async fn select(&self, prompt: &str, options: &[String]) -> Result<usize, UiError> {
        let request = UiRequest::Select {
            prompt: prompt.to_string(),
            options: options.to_vec(),
        };
        let chosen = match self.request(request).await {
            Some(Value::Number(index)) => index.as_u64().and_then(|index| usize::try_from(index).ok()),
            Some(Value::String(option)) => options.iter().position(|known| *known == option),
            _ => None,
        };
        chosen
            .filter(|index| *index < options.len())
            .ok_or(UiError::Cancelled)
    }

    async fn input(&self, prompt: &str) -> Result<String, UiError> {
        let request = UiRequest::Input {
            prompt: prompt.to_string(),
            secret: false,
        };
        match self.request(request).await {
            Some(Value::String(text)) => Ok(text),
            _ => Err(UiError::Cancelled),
        }
    }

    async fn secret(&self, prompt: &str) -> Result<String, UiError> {
        let request = UiRequest::Input {
            prompt: prompt.to_string(),
            secret: true,
        };
        match self.request(request).await {
            Some(Value::String(text)) => Ok(text),
            _ => Err(UiError::Cancelled),
        }
    }

    fn notify(&self, level: Level, text: &str) {
        self.out.line(&Frame::UiRequest {
            request_id: self.waiting.id(),
            request: UiRequest::Notify {
                level: level_name(level).to_string(),
                text: text.to_string(),
            },
        });
    }

    async fn approval(&self, request: ApprovalRequest) -> Option<ApprovalDecision> {
        let id = self.waiting.id();
        let (reply, answer) = oneshot::channel();
        locked(&self.waiting.approvals).insert(id.clone(), reply);
        self.out.line(&approval_frame(id, request));
        answer.await.ok()
    }

    async fn review_plan(&self, path: &Path, plan: &str) -> Result<PlanDecision, UiError> {
        let request = UiRequest::Plan {
            path: path.display().to_string(),
            plan: plan.to_string(),
        };
        match self.request(request).await {
            Some(Value::Bool(true)) => Ok(PlanDecision::Approve),
            Some(Value::String(feedback)) => Ok(PlanDecision::Revise(feedback)),
            _ => Err(UiError::Cancelled),
        }
    }
}

/// What a command that ran beside the loop came back with. The loop finishes it, because the
/// rest needs the open session, which may have changed meanwhile.
enum Finished {
    Shell {
        id: Value,
        command: String,
        output: String,
        status: String,
        share: bool,
    },
    Command {
        id: Value,
        outcome: CommandOutcome,
    },
    /// The model's brief for `handoff`, or why there is none.
    Handoff {
        id: Value,
        /// The session the brief is about, which must still be the open one.
        from: String,
        /// Which hand-off this is, so that one cancelled earlier does not end a later one.
        seq: u64,
        /// The draft made without a model, removed once the hand-off is done.
        draft: Option<std::path::PathBuf>,
        brief: Result<String, String>,
    },
}

struct Session {
    app: Arc<App>,
    active: Active,
    /// The events of the open session's lead.
    events: mpsc::UnboundedReceiver<AgentEvent>,
    team: Option<Arc<Team>>,
    waiting: Arc<Waiting>,
    out: Out,
    finished: mpsc::UnboundedSender<Finished>,
    /// A prompt was handed to the lead and its run has not ended. The lead's own status says so
    /// only once it has picked the prompt up.
    working: bool,
    /// The chain to go back to after a prompt that ran on a command's own model.
    restore_chain: Option<Vec<ModelSpec>>,
    /// The named prompts this process took, as `(session id, request id)`: one the lead has
    /// not taken up yet is in no session file.
    named: HashSet<(String, String)>,
    /// Ends side questions when the session ends.
    cancel: CancellationToken,
    /// The model lists providers gave this session.
    listed: crate::models::Listed,
    /// The session in which `[session] handoff_at` was told of.
    handoff_offered: Option<String>,
    /// The hand-off whose brief is being written, by number, and what cancels it.
    handing_off: Option<(u64, CancellationToken)>,
    /// Numbers the hand-offs.
    handoffs: u64,
}

impl Session {
    /// The lead works, or a hand-off brief is being written: either changes the session.
    fn busy(&self) -> bool {
        self.working || self.handing_off.is_some() || self.active.lead.status().busy
    }

    /// Refuses what would change the conversation while a hand-off brief is being written: the
    /// brief would leave it out.
    fn not_handing_off(&self) -> Result<(), String> {
        if self.handing_off.is_some() {
            return Err(
                "a hand-off brief is being written; wait for it, or send `abort` to cancel it".to_string(),
            );
        }
        Ok(())
    }

    /// Refuses what cannot be done while the lead works.
    fn idle_to(&self, what: &str) -> Result<(), String> {
        self.not_handing_off()?;
        if self.busy() {
            return Err(format!("the lead is working; send `abort` first to {what}"));
        }
        Ok(())
    }

    fn on_event(&mut self, event: AgentEvent) {
        self.out.line(&event_json(&self.active.lead.id, &event));
        if event.is_terminal() {
            self.working = false;
            if let Some(chain) = self.restore_chain.take() {
                self.active.lead.send(AgentCmd::SetModel(chain));
            }
            self.offer_handoff();
        }
    }

    /// Past `[session] handoff_at` context tokens, tells once per session (a notification) that
    /// `handoff` can start a fresh one from a brief. Makes no request.
    fn offer_handoff(&mut self) {
        let limit = self.app.config.session.handoff_at;
        let tokens = self.active.lead.status().context_tokens;
        let id = self.active.session.id();
        if limit == 0 || tokens < limit || self.handoff_offered.as_deref() == Some(id.as_str()) {
            return;
        }
        self.handoff_offered = Some(id);
        self.out.line(&Frame::UiRequest {
            request_id: self.waiting.id(),
            request: UiRequest::Notify {
                level: "info".to_string(),
                text: format!(
                    "The context holds {tokens} tokens. /handoff starts a fresh session from a written brief; it costs least while the prompt cache is warm."
                ),
            },
        });
    }

    /// Hands a prompt to the lead.
    async fn submit(&mut self, text: String, images: Vec<Block>) -> Result<(), String> {
        self.submit_as(text, images, None).await
    }

    /// Where the run of the prompt named `request_id` stands, when this session has taken
    /// such a prompt before.
    fn run_state(&self, request_id: &str) -> Option<RunState> {
        let seen = |state: &str, run_id, stop, text| RunState {
            request_id: Some(request_id.to_string()),
            run_id,
            state: state.to_string(),
            duplicate: true,
            stop,
            text,
        };
        if let Some(run) = self.active.session.request_run(request_id) {
            let state = match (&run.stop, self.busy()) {
                (Some(_), _) => "ended",
                (None, true) => "running",
                // Its process ended before it did.
                (None, false) => "interrupted",
            };
            return Some(seen(state, Some(run.run_id), run.stop, run.text));
        }
        self.named
            .contains(&(self.active.session.id(), request_id.to_string()))
            .then(|| seen("queued", None, None, None))
    }

    /// Hands the lead a prompt its sender named.
    async fn submit_as(
        &mut self,
        text: String,
        images: Vec<Block>,
        request_id: Option<String>,
    ) -> Result<(), String> {
        let submitted = self.app.submit_as(&self.active, text, images, request_id).await;
        match submitted {
            Ok(()) => self.working = true,
            Err(_) => {
                if let Some(chain) = self.restore_chain.take() {
                    self.active.lead.send(AgentCmd::SetModel(chain));
                }
            }
        }
        submitted
    }

    /// Sends a prompt on `model` for this one prompt when it is given.
    async fn prompt_on(&mut self, text: String, model: Option<Vec<ModelSpec>>) -> Result<(), String> {
        if let Some(chain) = model {
            self.idle_to("run a command that uses its own model")?;
            // A model that resolves to nothing is not worth failing the command for.
            if !self.app.router.resolve(&chain).is_empty() {
                self.restore_chain = Some(self.active.lead.status().chain);
                self.active.lead.send(AgentCmd::SetModel(chain));
            }
        }
        self.submit(text, Vec::new()).await
    }

    /// Runs one command. `Ok(None)` when its response comes later, from a task beside the loop.
    async fn run(&mut self, id: &Value, command: Command) -> Result<Option<Value>, String> {
        let team = |team: &Option<Arc<Team>>| {
            team.clone()
                .ok_or("there are no sub-agents in this session".to_string())
        };
        let done = |value: Value| Ok(Some(value));
        match command {
            Command::Prompt {
                text,
                images,
                request_id,
            } => {
                // The same prompt again: the run it started the first time is the answer.
                if let Some(seen) = request_id.as_deref().and_then(|id| self.run_state(id)) {
                    return data(seen).map(Some);
                }
                self.not_handing_off()?;
                let images = images
                    .into_iter()
                    .map(|image| Block::Image {
                        mime: image.mime,
                        data: image.data,
                    })
                    .collect();
                self.submit_as(text, images, request_id.clone()).await?;
                let Some(request_id) = request_id else {
                    return done(Value::Null);
                };
                self.named.insert((self.active.session.id(), request_id.clone()));
                data(RunState {
                    request_id: Some(request_id),
                    run_id: None,
                    state: "queued".to_string(),
                    duplicate: false,
                    stop: None,
                    text: None,
                })
                .map(Some)
            }
            Command::ContinueRun => {
                self.not_handing_off()?;
                if self.busy() {
                    return Err("the lead is working; there is no interrupted run to go on with".to_string());
                }
                let run = self.app.continue_run(&self.active)?;
                self.working = true;
                data(RunState {
                    request_id: run.request_id,
                    run_id: Some(run.run_id),
                    state: "running".to_string(),
                    duplicate: false,
                    stop: None,
                    text: None,
                })
                .map(Some)
            }
            Command::Steer { text, images } => {
                self.not_handing_off()?;
                let mut content = vec![Block::text(text)];
                content.extend(images.into_iter().map(|image| Block::Image {
                    mime: image.mime,
                    data: image.data,
                }));
                self.active.lead.send(AgentCmd::Steer(UserInput::blocks(content)));
                done(Value::Null)
            }
            Command::FollowUp { text } => {
                self.not_handing_off()?;
                self.active.lead.send(AgentCmd::FollowUp(UserInput::text(text)));
                done(Value::Null)
            }
            Command::Abort => {
                if let Some((_, cancel)) = self.handing_off.take() {
                    cancel.cancel();
                }
                self.active.lead.abort();
                done(Value::Null)
            }
            Command::GetState => data(self.state()).map(Some),
            Command::SetModel { chain } => {
                let chain = self
                    .app
                    .model_specs(&chain)
                    .map_err(|e| e.trim_start_matches("--model ").to_string())?;
                let router = &self.app.router;
                let mut usable = router.resolve(&chain);
                // A virtual model has no provider of its own; that it is defined was checked above.
                usable.retain(|model| {
                    router.provider(&model.provider).is_some() || azazel_ai::is_virtual(model)
                });
                if usable.is_empty() {
                    return Err("none of these models is configured".to_string());
                }
                self.active.lead.send(AgentCmd::SetModel(chain.clone()));
                // Recorded, so that a resumed session comes back on it.
                self.active.session.append(EntryKind::ModelChange { chain });
                data(Models {
                    models: usable.iter().map(ToString::to_string).collect(),
                })
                .map(Some)
            }
            Command::SetMode { mode } => {
                let mode: PermissionMode = mode.parse()?;
                self.app.permissions.set_mode(mode);
                done(json!({ "mode": mode.to_string() }))
            }
            Command::SetAgent { agent } => {
                self.app.set_agent(&agent)?;
                done(json!({ "agent": agent }))
            }
            Command::AgentSend { to, message } => {
                let reached = team(&self.team)?.send_from_user(&to, &message)?;
                done(json!({ "agents": reached }))
            }
            Command::AgentMention { name, text } => {
                let mentioned = team(&self.team)?.mention(&name, &text).await.transpose()?;
                data(mentioned.map(|mentioned| azazel_protocol::Mentioned {
                    agent: mentioned.agent,
                    started: mentioned.started,
                }))
                .map(Some)
            }
            Command::AgentStop { agent } => {
                let team = team(&self.team)?;
                let stopped = match agent.as_str() {
                    "all" => team.stop_all(),
                    agent => team.stop(agent)?,
                };
                done(json!({ "agents": stopped }))
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
                done(Value::Null)
            }
            Command::UiResponse { request_id, value } => {
                let reply = locked(&self.waiting.answers).remove(&request_id);
                reply
                    .ok_or_else(|| format!("no request is waiting as `{request_id}`"))?
                    .send(value)
                    .map_err(|_| "the agent is no longer waiting for this answer".to_string())?;
                done(Value::Null)
            }
            Command::NewSession { name } => self.new_session(name).await.map(Some),
            Command::ResumeSession { session_id, path } => {
                self.resume_session(session_id, path).await.map(Some)
            }
            Command::ForkSession { entry_id } => self.fork_session(entry_id).await.map(Some),
            Command::ListSessions { query } => self.list_sessions(query.as_deref()).map(Some),
            Command::GetMessages { leaf, before, limit } => {
                self.messages(leaf.as_deref(), before.as_deref(), limit).map(Some)
            }
            Command::GetAgentMessages { agent, before, limit } => {
                self.agent_messages(&agent, before.as_deref(), limit).map(Some)
            }
            Command::GetTree => data(self.tree()).map(Some),
            Command::SetLeaf { entry_id, before } => self.set_leaf(&entry_id, before).await.map(Some),
            Command::SetLabel { entry_id, label } => {
                if self.active.session.entry(&entry_id).is_none() {
                    return Err(format!("no entry `{entry_id}` in this session"));
                }
                self.active.session.append(EntryKind::Label {
                    target_id: entry_id,
                    label: label.trim().to_string(),
                });
                done(Value::Null)
            }
            Command::GetSession => self.session_copy().map(Some),
            Command::SetName { name } => {
                let name = name.trim();
                if name.is_empty() {
                    return Err("`name` must not be empty".to_string());
                }
                self.active.session.append(EntryKind::SessionInfo {
                    name: name.to_string(),
                });
                done(Value::Null)
            }
            Command::SetThinking { level } => {
                let level: ThinkingLevel = level.parse().map_err(|e| format!("{e}"))?;
                self.active.lead.send(AgentCmd::SetThinking(Some(level)));
                // Recorded, so that a resumed session comes back on it.
                self.active.session.append(EntryKind::ThinkingChange {
                    level: level.to_string(),
                });
                done(Value::Null)
            }
            Command::SetTier { tier } => {
                self.app.router.set_tier(crate::routing::parse_tier(&tier)?);
                done(Value::Null)
            }
            Command::Compact { instructions } => {
                self.active.lead.send(AgentCmd::Compact { instructions });
                done(Value::Null)
            }
            Command::Retry => self.retry().await.map(Some),
            Command::Rewind { entry_id, what } => self.rewind(&entry_id, what).await.map(Some),
            Command::Shell { command, share } => {
                self.shell(id.clone(), command, share);
                Ok(None)
            }
            Command::Export { format, path } => self.export(format, path.as_deref()).map(Some),
            Command::Btw { question } => {
                self.btw(id.clone(), question)?;
                Ok(None)
            }
            Command::Handoff { focus } => {
                self.handoff(id.clone(), focus.as_deref().unwrap_or_default())?;
                Ok(None)
            }
            Command::ListModels { all } => self.models(all).map(Some),
            Command::ListProviders => self.providers().map(Some),
            Command::RefreshModels { provider } => self.refresh_models(&provider).await.map(Some),
            Command::ListRoles => data(crate::roles::list(&self.roles_place())).map(Some),
            Command::SetRole { role, chain, save } => {
                let chain = chain.as_deref().map(crate::roles::parse_chain).transpose()?;
                data(crate::roles::set(&self.roles_place(), &role, chain, save)?).map(Some)
            }
            Command::SavePreset { name } => {
                data(crate::roles::save_preset(&self.roles_place(), &name)?).map(Some)
            }
            Command::ApplyPreset { name } => {
                let roles = crate::roles::apply_preset(&self.roles_place(), &name)?;
                self.profile_changed(&name, None);
                data(roles).map(Some)
            }
            Command::SetProfile { name } => self.set_profile(&name).map(Some),
            Command::SaveProfile { name } => {
                data(crate::roles::save_preset(&self.roles_place(), &name).map(|_| self.state())?).map(Some)
            }
            Command::ListCommands => data(self.commands()).map(Some),
            Command::ListFiles => {
                let root = self.app.paths.cwd.clone();
                let ignored = self.app.config.tui.complete_ignored;
                let files = tokio::task::spawn_blocking(move || crate::tui::project_files(&root, ignored))
                    .await
                    .map_err(|e| e.to_string())?;
                data(files).map(Some)
            }
            Command::GetUsage => data(self.app.router.limits().snapshot()).map(Some),
            Command::GetSettings => data(admin::get_settings(&self.app)?).map(Some),
            Command::SetSetting { path, value, scope } => {
                let changed = admin::set_setting(&self.app, &path, value, scope).await?;
                // The setting is changed: a frame that cannot be made must not fail the response.
                // A client with a light background asks `get_theme` again with its own `light`.
                if path == "tui.theme"
                    && let Ok(theme) = admin::get_theme(&self.app, None)
                {
                    self.out.line(&Frame::Theme(theme));
                }
                data(changed).map(Some)
            }
            Command::GetTheme { light } => data(admin::get_theme(&self.app, light)?).map(Some),
            Command::GetContext => {
                data(admin::get_context(&self.active.lead, &self.active.session)).map(Some)
            }
            Command::GetCost => data(admin::get_cost(
                &self.app,
                &self.active.lead,
                &self.active.session,
            ))
            .map(Some),
            Command::TrustProject { trusted } => data(admin::trust_project(&self.app, trusted)?).map(Some),
            Command::AddDir { path } => data(admin::add_dir(&self.app, &path)?).map(Some),
            Command::ForgetApprovals => data(admin::forget_approvals(&self.app)?).map(Some),
            Command::RunCommand { name, args, only } => self.command(id, &name, args, only.as_deref()).await,
            // A background session answers these itself, before its lines get here.
            Command::Attach { .. } | Command::Stop { .. } => Err(
                "this is not a background session: it serves the one client it was started for".to_string(),
            ),
        }
    }

    /// `shell`: the command runs beside the loop, so the session keeps answering meanwhile.
    fn shell(&self, id: Value, command: String, share: bool) {
        let shell = self.app.shell.clone();
        let mut options = RunOptions::new(self.app.paths.cwd.clone());
        options.keep = (200, 800);
        options.cancel = self.cancel.child_token();
        let finished = self.finished.clone();
        tokio::spawn(async move {
            let (output, status) = match run_command(&shell, &command, options).await {
                Ok(result) => {
                    let status = match result.code {
                        Some(code) => format!("exit code: {code}"),
                        None => "killed".to_string(),
                    };
                    (result.output.render(), status)
                }
                Err(e) => (String::new(), format!("could not start: {e}")),
            };
            let _ = finished.send(Finished::Shell {
                id,
                command,
                output,
                status,
                share,
            });
        });
    }

    /// `btw`: a question beside the conversation, answered from what the session knows so far.
    fn btw(&self, id: Value, question: String) -> Result<(), String> {
        let services = &self.app.registry.services;
        let side = services
            .get::<Side>()
            .ok_or("side questions are not available in this session")?;
        let context = settled(
            self.active
                .session
                .build_context(None)
                .into_iter()
                .map(|entry| entry.message)
                .collect(),
        );
        let agent = self.active.lead.info().clone();
        let chain = self.active.lead.status().chain;
        let (out, cancel) = (self.out.clone(), self.cancel.child_token());
        tokio::spawn(async move {
            let (deltas, delta_id) = (out.clone(), id.clone());
            let mut on_text = move |delta: &str| {
                deltas.line(&Frame::BtwDelta {
                    id: delta_id.clone(),
                    delta: delta.to_string(),
                });
            };
            let asked = SideQuestion {
                agent: &agent,
                context,
                question,
                chain,
                purpose: "btw",
                framed: true,
            };
            let answer = side.ask(asked, &mut on_text, cancel).await.and_then(|answer| {
                data(BtwAnswer {
                    answer: answer.text,
                    model: answer.model.to_string(),
                    dropped_tools: answer.dropped_tools,
                })
            });
            out.respond(id, answer);
        });
        Ok(())
    }

    /// `handoff`: keeps a draft brief made without a model, asks the model for the brief beside
    /// the loop, and lets [`Self::finish`] open the session that continues from it.
    fn handoff(&mut self, id: Value, focus: &str) -> Result<(), String> {
        self.idle_to("hand the work off")?;
        let services = &self.app.registry.services;
        let side = services
            .get::<Side>()
            .ok_or("side questions are not available in this session")?;
        let session = self.active.session.clone();
        let context = crate::handoff::closed_context(&session);
        if context.is_empty() {
            return Err("there is nothing to hand off yet".to_string());
        }
        let cwd = self.app.paths.cwd.clone();
        let todos = services
            .get::<azazel_tools::TodoStore>()
            .map(|store| store.get(&self.active.lead.id))
            .unwrap_or_default();
        let from = session.id();
        let draft = crate::handoff::draft(&session, &todos, &cwd);
        // A failed request leaves this brief behind.
        let kept = crate::handoff::save_draft(&self.app.paths.sessions_dir().join(&from), &from, &draft);
        let question = crate::handoff::prompt(focus, &crate::handoff::ranked_files(&session, &cwd));
        let agent = self.active.lead.info().clone();
        let chain = self.active.lead.status().chain;
        let (finished, cancel) = (self.finished.clone(), self.cancel.child_token());
        // `abort` cancels the request through this token.
        self.handoffs += 1;
        let seq = self.handoffs;
        self.handing_off = Some((seq, cancel.clone()));
        let draft = kept.as_ref().ok().cloned();
        tokio::spawn(async move {
            let asked = SideQuestion {
                agent: &agent,
                context,
                question,
                chain,
                purpose: "handoff",
                framed: true,
            };
            let brief = side
                .ask(asked, &mut |_: &str| {}, cancel)
                .await
                .map_err(|why| match &kept {
                    Ok(file) => format!(
                        "the request failed: {why}; a draft brief is in {}",
                        file.display()
                    ),
                    Err(_) => format!("the request failed: {why}"),
                })
                .and_then(|answer| match answer.text.trim() {
                    "" if answer.dropped_tools.is_empty() => Err("the model gave no answer".to_string()),
                    "" => Err(format!(
                        "the model tried to use {} instead of answering; ask again",
                        answer.dropped_tools.join(", ")
                    )),
                    text => Ok(text.to_string()),
                });
            let _ = finished.send(Finished::Handoff {
                id,
                from,
                seq,
                draft,
                brief,
            });
        });
        Ok(())
    }

    /// `run_command`: a skill (`skill:<name>`) or a command an extension registered. Registered
    /// commands run beside the loop: some ask the user, and the answer arrives as a command.
    ///
    /// `only` limits where the command may come from: a remote browser that may not run the
    /// project's own commands sends it, and a command that resolves to another source is
    /// refused, whatever its name says.
    async fn command(
        &mut self,
        id: &Value,
        name: &str,
        args: String,
        only: Option<&[CommandOrigin]>,
    ) -> Result<Option<Value>, String> {
        let name = name.trim_start_matches('/');
        if let Some(only) = only
            && let Some(origin) = session::origin_of(&self.app, name)
            && !only.contains(&origin)
        {
            return Err(format!(
                "`/{name}` is a command of the project or of a script; this client may not run those"
            ));
        }
        // What the session says about itself is text, whoever asks.
        if let Some(report) = crate::reports::run(&self.app, &self.active, name, &args).await {
            return data(CommandResult::Show { text: report? }).map(Some);
        }
        if let Some(skill_name) = name.strip_prefix("skill:") {
            let skills = self.app.registry.skills.clone();
            let skill = skills
                .get(skill_name)
                .filter(|skill| skill.user_invocable)
                .ok_or_else(|| format!("no skill named `{skill_name}`"))?;
            skills.mark_used(&skill);
            azazel_core::skills::allow_for_prompt(&self.app.registry.services, &skill);
            let text = azazel_core::skills::invocation_text(&skill, &args);
            self.prompt_on(text.clone(), skill.model.clone()).await?;
            return data(CommandResult::Prompt { text }).map(Some);
        }
        let command = self
            .app
            .registry
            .command(name)
            .ok_or_else(|| format!("no command named `{name}`"))?;
        let context = self.app.registry.bus.ctx();
        let (id, finished) = (id.clone(), self.finished.clone());
        tokio::spawn(async move {
            let outcome = command.handler.run(&args, &context).await;
            let _ = finished.send(Finished::Command { id, outcome });
        });
        Ok(None)
    }

    /// Answers a command that ran beside the loop.
    async fn finish(&mut self, finished: Finished) {
        match finished {
            Finished::Shell {
                id,
                command,
                output,
                status,
                share,
            } => {
                if share {
                    // The lead sees what the user ran, without being asked to respond.
                    let note = format!("{SHELL_NOTE}{command}` myself:\n{output}\n{status}");
                    self.active.lead.send(AgentCmd::Append(UserInput::text(note)));
                }
                self.out.respond(id, data(ShellResult { output, status }));
            }
            Finished::Command { id, outcome } => {
                let result = match outcome {
                    CommandOutcome::Done => Ok(CommandResult::Done),
                    CommandOutcome::Show(text) => Ok(CommandResult::Show { text }),
                    CommandOutcome::Prompt { text, model } => self
                        .prompt_on(text.clone(), model)
                        .await
                        .map(|()| CommandResult::Prompt { text }),
                    CommandOutcome::Error(error) => Err(error),
                };
                self.out.respond(id, result.and_then(data));
            }
            Finished::Handoff {
                id,
                from,
                seq,
                draft,
                brief,
            } => {
                // `abort` ended the wait; a brief that still arrived is not used.
                let current = self.handing_off.as_ref().is_some_and(|(open, _)| *open == seq);
                if current {
                    self.handing_off = None;
                }
                let result = match brief {
                    _ if !current => Err("the hand-off was cancelled".to_string()),
                    Ok(brief) => self.continue_from(&from, &brief).await,
                    Err(why) => Err(why),
                };
                // The draft is only for a hand-off that failed.
                if let (Ok(_), Some(draft)) = (&result, &draft) {
                    let _ = std::fs::remove_file(draft);
                }
                self.out.respond(id, result.and_then(data));
            }
        }
    }

    /// Handles one input line and answers it.
    async fn line(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        let (id, command) = Request::parse(line);
        let outcome = match command {
            Ok(command) => self.run(&id, command).await,
            Err(error) => Err(error),
        };
        match outcome {
            Ok(Some(data)) => self.out.respond(id, Ok(data)),
            Ok(None) => {}
            Err(error) => self.out.respond(id, Err(error)),
        }
    }
}

/// Serves the session on standard input and output until the input closes.
pub async fn run(app: Arc<App>, active: Active) -> u8 {
    serve(
        app,
        active,
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
}

/// Serves the session to whoever is at the other end of `reader` and `writer`, until the reader
/// ends.
pub async fn serve(
    app: Arc<App>,
    active: Active,
    reader: impl AsyncBufRead + Unpin,
    mut writer: impl AsyncWrite + Unpin + Send + 'static,
) -> u8 {
    let (lines, mut queued) = mpsc::unbounded_channel::<Option<String>>();
    let writing = tokio::spawn(async move {
        while let Some(Some(mut line)) = queued.recv().await {
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
    });
    Out(lines.clone()).line(&hello());
    let input = futures::stream::unfold(reader.lines(), |mut lines| async {
        let line = lines.next_line().await.ok().flatten()?;
        Some((line, lines))
    });
    serve_lines(app, active, input, lines, Arc::default()).await;
    let _ = writing.await;
    EXIT_OK
}

/// The first frame a client is sent.
pub(crate) fn hello() -> Frame {
    Frame::Hello {
        protocol: PROTOCOL,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Serves the session: command lines come from `input`, frames go to `lines` as JSON text. Ends
/// when `input` does, and says so with a last `None`. The `hello` frame is the caller's to send;
/// `waiting` lets the caller see which requests to the user are still open.
pub(crate) async fn serve_lines(
    app: Arc<App>,
    active: Active,
    input: impl futures::Stream<Item = String>,
    lines: mpsc::UnboundedSender<Option<String>>,
    waiting: Arc<Waiting>,
) {
    let out = Out(lines);
    // Requests from agents go through the loop below rather than straight out, so they are written
    // in their place among the lead's events (see `Requests`).
    let (request_lines, mut requests) = mpsc::unbounded_channel::<Fenced>();
    // The `profile_changed` frame of a switch on a usage limit is written in its place among the
    // lead's events too, and takes the place of the notice an RPC client would be sent.
    let profile_frames = Requests {
        lead: active.lead.clone(),
        lines: request_lines.clone(),
    };
    app.registry
        .services
        .insert(Arc::new(crate::profiles::limit::ProfileFrames(Arc::new(
            move |frame| profile_frames.line(&frame),
        ))));
    app.registry.services.insert(Arc::new(Ui(Arc::new(RpcBridge {
        out: Requests {
            lead: active.lead.clone(),
            lines: request_lines,
        },
        waiting: waiting.clone(),
    }))));

    let team = app.registry.services.get::<Team>();
    let mut team_events = team.as_ref().map(|team| team.subscribe());
    let source = if active.resumed {
        StartSource::Resume
    } else {
        StartSource::Startup
    };
    app.start(source).await;
    let (finished, mut finishing) = mpsc::unbounded_channel();
    let (events, emitted) = active.lead.subscribe_counted();
    let mut pending = Pending {
        written: emitted,
        lines: VecDeque::new(),
    };
    let mut session = Session {
        events,
        app,
        active,
        team,
        waiting,
        out: out.clone(),
        finished,
        working: false,
        restore_chain: None,
        handoff_offered: None,
        handing_off: None,
        handoffs: 0,
        named: HashSet::new(),
        cancel: CancellationToken::new(),
        listed: Default::default(),
    };

    let mut input = std::pin::pin!(input);
    loop {
        tokio::select! {
            // Agent events first: an event is always queued before the request it causes.
            biased;
            Some(event) = session.events.recv() => {
                // What sub-agents did before this event of the lead goes out first: a report
                // comes before the run of the lead it starts.
                while let Some(earlier) = team_events.as_mut().map(|events| events.try_recv()) {
                    match earlier {
                        Ok(earlier) => team_line(&out, &session, earlier),
                        Err(TryRecvError::Lagged(_)) => out.line(&Frame::Roster { members: session.roster() }),
                        Err(TryRecvError::Closed) => team_events = None,
                        Err(TryRecvError::Empty) => break,
                    }
                }
                while let Ok(fenced) = requests.try_recv() {
                    pending.lines.push_back(fenced);
                }
                pending.flush(&out);
                session.on_event(event);
                pending.written += 1;
                pending.flush(&out);
            }
            Some(event) = async { Some(team_events.as_mut()?.recv().await) } => match event {
                Ok(event) => team_line(&out, &session, event),
                // Events were missed; the roster says where everyone stands now.
                Err(RecvError::Lagged(_)) => out.line(&Frame::Roster { members: session.roster() }),
                Err(RecvError::Closed) => team_events = None,
            },
            Some(fenced) = requests.recv() => {
                pending.lines.push_back(fenced);
                pending.flush(&out);
            }
            Some(done) = finishing.recv() => session.finish(done).await,
            line = input.next() => match line {
                Some(line) => session.line(&line).await,
                None => break,
            },
        }
    }

    // Nothing the lead emits now is still to come before them.
    pending.written = u64::MAX;
    pending.flush(&out);
    // Requests still open end the way they do when nobody is there.
    session.cancel.cancel();
    locked(&session.waiting.approvals).clear();
    locked(&session.waiting.answers).clear();
    session.app.finish(&session.active).await;
    // The bridge the agents were given keeps a sender, so the end of the output is said, not
    // inferred from the channel closing.
    let _ = out.0.send(None);
}

/// Request lines that wait for the events the lead emitted before them.
pub(crate) struct Pending {
    /// The lead's events written so far, counted from its first.
    pub(crate) written: u64,
    pub(crate) lines: VecDeque<Fenced>,
}

impl Pending {
    /// Writes the lines, in the order they came, whose events of the lead have all been written.
    fn flush(&mut self, out: &Out) {
        self.flush_to(|line| {
            let _ = out.0.send(Some(line));
        });
    }

    /// Hands `write` the lines, in the order they came, whose events of the lead have all been
    /// written.
    pub(crate) fn flush_to(&mut self, mut write: impl FnMut(String)) {
        while let Some((_, line)) = self.lines.pop_front_if(|(after, _)| *after <= self.written) {
            write(line);
        }
    }
}

/// Writes what the team reports.
fn team_line(out: &Out, session: &Session, event: TeamEvent) {
    out.line(&team_frame(session.team.as_deref(), event));
}

/// What the team reporting `event` is to a client: the frame.
pub(crate) fn team_frame(team: Option<&Team>, event: TeamEvent) -> Value {
    match event {
        TeamEvent::Agent { id, event } => event_json(&id, &event),
        TeamEvent::Input { id, text } => to_value(&Frame::UserInput {
            kind: "prompt".to_string(),
            text,
            agent: Some(id),
        }),
        TeamEvent::Roster => to_value(&Frame::Roster {
            members: session::roster_of(team),
        }),
    }
}

fn to_value(frame: &Frame) -> Value {
    serde_json::to_value(frame).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use azazel_agent::ToolOutput;
    use azazel_ai::{Message, ModelRef, Role, StopReason, StreamEvent, Tier};

    use super::*;

    /// Every event an agent can emit, so that a new one cannot be added without the protocol
    /// crate learning to read it.
    fn every_event() -> Vec<AgentEvent> {
        let model = ModelRef::new("mock", "lead");
        let events = vec![
            AgentEvent::AgentStart,
            AgentEvent::TurnStart { turn: 1 },
            AgentEvent::MessageStart,
            AgentEvent::Stream(StreamEvent::TextDelta {
                index: 0,
                delta: "hi".into(),
            }),
            AgentEvent::MessageEnd {
                message: Message::user("hi"),
            },
            AgentEvent::ToolStart {
                call_id: "c1".into(),
                name: "read".into(),
                input: json!({ "path": "a" }),
            },
            AgentEvent::ToolUpdate {
                call_id: "c1".into(),
                text: "…".into(),
            },
            AgentEvent::ToolEnd {
                call_id: "c1".into(),
                output: ToolOutput::text("done"),
                duration_ms: 3,
            },
            AgentEvent::TurnEnd,
            AgentEvent::Retry {
                attempt: 2,
                model: model.clone(),
                stalled_secs: None,
            },
            AgentEvent::Retry {
                attempt: 1,
                model: model.clone(),
                stalled_secs: Some(20),
            },
            AgentEvent::Fallback {
                from: model.clone(),
                to: model.clone(),
            },
            AgentEvent::Account {
                provider: "mock".into(),
                from: "work".into(),
                to: "home".into(),
                until: 9,
                limit: "5-hour limit".into(),
            },
            AgentEvent::TierDropped {
                model,
                tier: Tier::Priority,
            },
            AgentEvent::Rule { name: "r".into() },
            AgentEvent::Compaction {
                tokens_before: 9,
                tokens_after: 3,
            },
            AgentEvent::Idle {
                stop: StopReason::Stop,
            },
            AgentEvent::Failed { error: "no".into() },
        ];
        for event in &events {
            // Fails to compile when a variant is added: add it to the list above and to `Frame`.
            match event {
                AgentEvent::AgentStart
                | AgentEvent::TurnStart { .. }
                | AgentEvent::MessageStart
                | AgentEvent::Stream(_)
                | AgentEvent::MessageEnd { .. }
                | AgentEvent::ToolStart { .. }
                | AgentEvent::ToolUpdate { .. }
                | AgentEvent::ToolEnd { .. }
                | AgentEvent::TurnEnd
                | AgentEvent::Retry { .. }
                | AgentEvent::Fallback { .. }
                | AgentEvent::Account { .. }
                | AgentEvent::TierDropped { .. }
                | AgentEvent::Rule { .. }
                | AgentEvent::Compaction { .. }
                | AgentEvent::Idle { .. }
                | AgentEvent::Failed { .. } => {}
            }
        }
        events
    }

    #[test]
    fn every_agent_event_is_a_frame_of_the_protocol() {
        for event in every_event() {
            let wire = event_json("lead", &event);
            let frame: Frame = serde_json::from_value(wire.clone()).unwrap_or_else(|e| panic!("{wire}: {e}"));
            assert_ne!(frame, Frame::Unknown, "{wire}");
            assert_eq!(frame.agent(), Some("lead"), "{wire}");
            // Nothing is lost on the way through the typed frame.
            assert_eq!(serde_json::to_value(&frame).unwrap(), wire);
        }
    }

    #[test]
    fn a_frame_read_back_is_the_event_it_was_written_from() {
        // What a UI attached from another process draws is what a local one draws.
        for event in every_event() {
            let wire = event_json("scout-1", &event);
            let frame: Frame = serde_json::from_value(wire.clone()).unwrap();
            let (agent, back) = crate::tui::agent_event(frame).unwrap_or_else(|| panic!("{wire}"));
            assert_eq!(agent, "scout-1");
            assert_eq!(event_json(&agent, &back), wire);
        }
        assert!(crate::tui::agent_event(Frame::Roster { members: Vec::new() }).is_none());
    }

    #[test]
    fn a_side_question_leaves_out_calls_that_are_still_open() {
        let call = |id: &str| Block::ToolCall {
            id: id.into(),
            name: "read".into(),
            input: json!({}),
            signature: None,
        };
        let assistant = |blocks: Vec<Block>| Message {
            role: Role::Assistant,
            content: blocks,
            meta: None,
        };
        let result = |id: &str| Message {
            role: Role::Tool,
            content: vec![Block::tool_result(id, "ok", false)],
            meta: None,
        };
        let finished = vec![
            Message::user("go"),
            assistant(vec![call("c1")]),
            result("c1"),
            assistant(vec![Block::text("done")]),
        ];
        assert_eq!(settled(finished.clone()), finished);
        let open = vec![
            Message::user("go"),
            assistant(vec![call("c1"), call("c2")]),
            result("c1"),
        ];
        assert_eq!(settled(open), [Message::user("go")]);
    }
}
