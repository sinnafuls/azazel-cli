//! RPC mode (`azazel rpc`): a session driven by another program over standard input and output.
//!
//! Both directions carry one JSON object per line; standard output carries nothing else. The
//! lines are the types of the `azazel-protocol` crate: the client writes [`Command`]s, each with
//! an `id` that its response repeats, and reads [`Frame`]s. `docs/rpc.md` describes them for
//! people and `docs/rpc.schema.json` for programs.
//!
//! The session ends when the input closes.

mod session;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use azazel_agent::{AgentCmd, AgentEvent, UserInput};
use azazel_ai::{Block, Message, ModelSpec, Role, ThinkingLevel};
use azazel_core::process::{RunOptions, run as run_command};
use azazel_core::side::{Side, SideQuestion};
use azazel_core::{
    ApprovalDecision, ApprovalRequest, CommandOutcome, EntryKind, Level, PermissionMode, PlanDecision,
    StartSource, Ui, UiBridge, UiError,
};
use azazel_protocol::{
    BtwAnswer, Command, CommandResult, Decision, Frame, Models, PROTOCOL, Request, Response, ShellResult,
    UiRequest,
};
use azazel_team::{Team, TeamEvent};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::app::{Active, App};
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

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The data of a response.
fn data(value: impl Serialize) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|e| e.to_string())
}

/// Requests the client has been sent and not yet answered.
#[derive(Default)]
struct Waiting {
    next: AtomicU64,
    approvals: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
    answers: Mutex<HashMap<String, oneshot::Sender<Value>>>,
}

impl Waiting {
    fn id(&self) -> String {
        format!("r{}", self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

/// Agents reach the client through this: each call becomes a request line and waits for the
/// command that answers it. When the client has gone, the call ends as it does with nobody there.
struct RpcBridge {
    out: Out,
    waiting: Arc<Waiting>,
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

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Info => "info",
        Level::Warn => "warn",
        Level::Error => "error",
    }
}

#[async_trait]
impl UiBridge for RpcBridge {
    async fn ask(&self, question: &str, options: &[String], multi: bool) -> Result<Vec<String>, UiError> {
        let request = UiRequest::Ask {
            question: question.to_string(),
            options: options.to_vec(),
            multi,
        };
        match self.request(request).await {
            Some(Value::String(one)) => Ok(vec![one]),
            Some(Value::Array(many)) => Ok(many
                .into_iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()),
            _ => Err(UiError::Cancelled),
        }
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
        let access = serde_json::to_value(request.access)
            .ok()
            .and_then(|access| access.as_str().map(str::to_string))
            .unwrap_or_default();
        self.out.line(&Frame::ApprovalRequest {
            request_id: id,
            agent: request.agent.id,
            tool: request.tool,
            access,
            subjects: request.subjects,
            input: request.input,
            rule: request.rule,
            note: request.note,
        });
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
    /// Ends side questions when the session ends.
    cancel: CancellationToken,
}

/// The conversation up to the last point where every tool call has its result: a side question
/// asked while tools run must not carry calls that are still open.
fn settled(mut context: Vec<Message>) -> Vec<Message> {
    let Some(at) = context
        .iter()
        .rposition(|message| message.role == Role::Assistant && message.tool_calls().next().is_some())
    else {
        return context;
    };
    let answered: Vec<&str> = context[at + 1..]
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            Block::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    if !context[at].tool_calls().all(|(id, _, _)| answered.contains(&id)) {
        context.truncate(at);
    }
    context
}

impl Session {
    fn busy(&self) -> bool {
        self.working || self.active.lead.status().busy
    }

    /// Refuses what cannot be done while the lead works.
    fn idle_to(&self, what: &str) -> Result<(), String> {
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
        }
    }

    /// Hands a prompt to the lead.
    async fn submit(&mut self, text: String, images: Vec<Block>) -> Result<(), String> {
        let submitted = self.app.submit(&self.active, text, images).await;
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
            Command::Prompt { text, images } => {
                let images = images
                    .into_iter()
                    .map(|image| Block::Image {
                        mime: image.mime,
                        data: image.data,
                    })
                    .collect();
                self.submit(text, images).await?;
                done(Value::Null)
            }
            Command::Steer { text } => {
                self.active.lead.send(AgentCmd::Steer(UserInput::text(text)));
                done(Value::Null)
            }
            Command::FollowUp { text } => {
                self.active.lead.send(AgentCmd::FollowUp(UserInput::text(text)));
                done(Value::Null)
            }
            Command::Abort => {
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
                usable.retain(|model| router.provider(&model.provider).is_some());
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
            Command::AgentSend { to, message } => {
                let reached = team(&self.team)?.send_from_user(&to, &message)?;
                done(json!({ "agents": reached }))
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
            Command::GetMessages { leaf } => self.messages(leaf.as_deref()).map(Some),
            Command::GetTree => data(self.tree()).map(Some),
            Command::SetLeaf { entry_id } => self.set_leaf(&entry_id).await.map(Some),
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
                self.active.lead.send(AgentCmd::SetThinking(level));
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
            Command::ListModels => self.models().map(Some),
            Command::GetUsage => data(self.app.router.limits().snapshot()).map(Some),
            Command::RunCommand { name, args } => self.command(id, &name, args).await,
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

    /// `run_command`: a skill (`skill:<name>`) or a command an extension registered. Registered
    /// commands run beside the loop: some ask the user, and the answer arrives as a command.
    async fn command(&mut self, id: &Value, name: &str, args: String) -> Result<Option<Value>, String> {
        let name = name.trim_start_matches('/');
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
    let out = Out(lines);
    out.line(&Frame::Hello {
        protocol: PROTOCOL,
        version: env!("CARGO_PKG_VERSION").to_string(),
    });
    // Requests from agents go through the loop below rather than straight out, so they are written
    // after the events emitted before them (a `tool_start` comes before its `approval_request`).
    let (request_lines, mut requests) = mpsc::unbounded_channel::<Option<String>>();
    let waiting = Arc::new(Waiting::default());
    app.registry.services.insert(Arc::new(Ui(Arc::new(RpcBridge {
        out: Out(request_lines),
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
    let mut session = Session {
        events: active.lead.subscribe(),
        app,
        active,
        team,
        waiting,
        out: out.clone(),
        finished,
        working: false,
        restore_chain: None,
        cancel: CancellationToken::new(),
    };

    let mut input = reader.lines();
    loop {
        tokio::select! {
            // Agent events first: an event is always queued before the request it causes.
            biased;
            Some(event) = session.events.recv() => session.on_event(event),
            Some(event) = async { Some(team_events.as_mut()?.recv().await) } => match event {
                Ok(TeamEvent::Agent { id, event }) => out.line(&event_json(&id, &event)),
                Ok(TeamEvent::Roster) => out.line(&Frame::Roster { members: session.roster() }),
                // Events were missed; the roster says where everyone stands now.
                Err(RecvError::Lagged(_)) => out.line(&Frame::Roster { members: session.roster() }),
                Err(RecvError::Closed) => team_events = None,
            },
            Some(Some(line)) = requests.recv() => {
                let _ = out.0.send(Some(line));
            }
            Some(done) = finishing.recv() => session.finish(done).await,
            line = input.next_line() => match line {
                Ok(Some(line)) => session.line(&line).await,
                _ => break,
            },
        }
    }

    // Requests still open end the way they do when nobody is there.
    session.cancel.cancel();
    locked(&session.waiting.approvals).clear();
    locked(&session.waiting.answers).clear();
    session.app.finish(&session.active).await;
    // The bridge the agents were given keeps a sender, so the end of the output is said, not
    // inferred from the channel closing.
    let _ = out.0.send(None);
    let _ = writing.await;
    EXIT_OK
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use azazel_agent::ToolOutput;
    use azazel_ai::{ModelRef, StopReason, StreamEvent, Tier};

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
