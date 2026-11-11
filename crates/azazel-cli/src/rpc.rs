//! RPC mode (`azazel rpc`): a session driven by another program over standard input and output.
//!
//! Both directions carry one JSON object per line; standard output carries nothing else.
//!
//! Commands, each with an `id` that its response repeats:
//!
//! | `type` | Members | Does |
//! |---|---|---|
//! | `prompt` | `text`, `images`? (`[{"mime","data"}]`, base64) | Starts work on the input |
//! | `steer` | `text` | Hands the working agent input before its next request |
//! | `follow_up` | `text` | Queues input for when the agent would go idle |
//! | `abort` | | Stops the current work |
//! | `get_state` | | Answers with the session's state and the roster |
//! | `set_model` | `chain` (`["provider/model", "@role", …]`) | Changes the lead's model chain |
//! | `set_mode` | `mode` (`plan`, `ask`, `accept-edits`, `auto`) | Changes the permission mode |
//! | `agent_send` | `to`, `message` | Messages a sub-agent, or `all` |
//! | `agent_stop` | `agent` | Stops a sub-agent and the agents it started, or `all` |
//! | `approval_response` | `request_id`, `decision` (`allow`, `always`, `deny`), `feedback`? | Answers an `approval_request` |
//! | `ui_response` | `request_id`, `value` | Answers a `ui_request`; `null` dismisses it |
//!
//! Responses: `{"id","type":"response","success":true,"data":…}` or
//! `{"id","type":"response","success":false,"error":"…"}`.
//!
//! Events: every agent event as `{"type":"<name>","agent":"<id>",…}`, and
//!
//! - `approval_request {"request_id","agent","tool","access","subjects","input","rule"?,"note"?}`;
//! - `ui_request {"request_id","kind",…}` where `kind` and the expected `value` are: `ask`
//!   (`question`, `options`, `multi`; a list of the chosen options, or one string when there are
//!   no options), `confirm` (`prompt`; a boolean), `select` (`prompt`, `options`; the index),
//!   `input` (`prompt`; a string), `plan` (`path`, `plan`; `true` to approve, or a string of
//!   feedback) and `notify` (`level`, `text`; no answer is expected);
//! - `roster {"members":[{"id","def","parent","depth","state","elapsed_ms","tokens","cost_usd"}]}`
//!   whenever a sub-agent appears or changes state.
//!
//! The session ends when standard input closes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use azazel_agent::{AgentCmd, UserInput};
use azazel_ai::{Block, ModelSpec};
use azazel_core::{
    ApprovalDecision, ApprovalRequest, Level, PermissionMode, PlanDecision, StartSource, Ui, UiBridge,
    UiError,
};
use azazel_team::{MemberInfo, Team, TeamEvent};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, oneshot};

use crate::app::{Active, App};
use crate::print::{EXIT_OK, event_json};

/// Protocol lines on their way to standard output; `None` ends the output.
#[derive(Clone)]
struct Out(mpsc::UnboundedSender<Option<String>>);

impl Out {
    fn send(&self, line: Value) {
        let _ = self.0.send(Some(line.to_string()));
    }
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
    /// Sends a `ui_request` of `kind` and returns the client's `value`; `None` when the client
    /// dismissed it or went away.
    async fn request(&self, kind: &str, mut members: Value) -> Option<Value> {
        let id = self.waiting.id();
        let (reply, answer) = oneshot::channel();
        locked(&self.waiting.answers).insert(id.clone(), reply);
        members["type"] = "ui_request".into();
        members["request_id"] = id.into();
        members["kind"] = kind.into();
        self.out.send(members);
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
        let members = json!({ "question": question, "options": options, "multi": multi });
        match self.request("ask", members).await {
            Some(Value::String(one)) => Ok(vec![one]),
            Some(Value::Array(many)) => Ok(many
                .into_iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()),
            _ => Err(UiError::Cancelled),
        }
    }

    async fn confirm(&self, prompt: &str) -> Result<bool, UiError> {
        match self.request("confirm", json!({ "prompt": prompt })).await {
            Some(Value::Bool(answer)) => Ok(answer),
            _ => Err(UiError::Cancelled),
        }
    }

    async fn select(&self, prompt: &str, options: &[String]) -> Result<usize, UiError> {
        let members = json!({ "prompt": prompt, "options": options });
        let chosen = match self.request("select", members).await {
            Some(Value::Number(index)) => index.as_u64().and_then(|index| usize::try_from(index).ok()),
            Some(Value::String(option)) => options.iter().position(|known| *known == option),
            _ => None,
        };
        chosen
            .filter(|index| *index < options.len())
            .ok_or(UiError::Cancelled)
    }

    async fn input(&self, prompt: &str) -> Result<String, UiError> {
        match self.request("input", json!({ "prompt": prompt })).await {
            Some(Value::String(text)) => Ok(text),
            _ => Err(UiError::Cancelled),
        }
    }

    fn notify(&self, level: Level, text: &str) {
        self.out.send(json!({
            "type": "ui_request",
            "request_id": self.waiting.id(),
            "kind": "notify",
            "level": level_name(level),
            "text": text
        }));
    }

    async fn approval(&self, request: ApprovalRequest) -> Option<ApprovalDecision> {
        let id = self.waiting.id();
        let (reply, answer) = oneshot::channel();
        locked(&self.waiting.approvals).insert(id.clone(), reply);
        let mut line = json!({
            "type": "approval_request",
            "request_id": id,
            "agent": request.agent.id,
            "tool": request.tool,
            "access": request.access,
            "subjects": request.subjects,
            "input": request.input
        });
        if let Some(rule) = request.rule {
            line["rule"] = rule.into();
        }
        if let Some(note) = request.note {
            line["note"] = note.into();
        }
        self.out.send(line);
        answer.await.ok()
    }

    async fn review_plan(&self, path: &Path, plan: &str) -> Result<PlanDecision, UiError> {
        let members = json!({ "path": path.display().to_string(), "plan": plan });
        match self.request("plan", members).await {
            Some(Value::Bool(true)) => Ok(PlanDecision::Approve),
            Some(Value::String(feedback)) => Ok(PlanDecision::Revise(feedback)),
            _ => Err(UiError::Cancelled),
        }
    }
}

fn member_json(member: &MemberInfo) -> Value {
    json!({
        "id": member.id,
        "def": member.def,
        "parent": member.parent,
        "depth": member.depth,
        "state": member.state.label(),
        "elapsed_ms": member.elapsed.as_millis() as u64,
        "tokens": member.tokens,
        "cost_usd": member.cost_usd
    })
}

struct Session {
    app: Arc<App>,
    active: Active,
    team: Option<Arc<Team>>,
    waiting: Arc<Waiting>,
    out: Out,
}

impl Session {
    fn roster(&self) -> Vec<Value> {
        self.team
            .as_ref()
            .map(|team| team.list().iter().map(member_json).collect())
            .unwrap_or_default()
    }

    fn state(&self) -> Value {
        let status = self.active.lead.status();
        json!({
            "session_id": self.active.session.id(),
            "cwd": self.app.paths.cwd.display().to_string(),
            "mode": self.app.permissions.mode().to_string(),
            "busy": status.busy,
            "turn": status.turn,
            "model": status.model.map(|model| model.to_string()),
            "chain": status.chain.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "thinking": status.thinking.to_string(),
            "context_tokens": status.context_tokens,
            "context_window": status.context_window,
            "usage": status.usage,
            "agents": self.roster()
        })
    }

    /// Runs one command and returns the data of its response.
    async fn run(&self, command: &Value) -> Result<Value, String> {
        let text = |name: &str| {
            command
                .get(name)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("`{name}` must be a string"))
        };
        let lead = &self.active.lead;
        let team = || {
            self.team
                .as_ref()
                .ok_or("there are no sub-agents in this session")
        };
        let kind = command.get("type").and_then(Value::as_str).unwrap_or_default();
        match kind {
            "prompt" => {
                let mut images = Vec::new();
                for image in command
                    .get("images")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match (
                        image.get("mime").and_then(Value::as_str),
                        image.get("data").and_then(Value::as_str),
                    ) {
                        (Some(mime), Some(data)) => images.push(Block::Image {
                            mime: mime.to_string(),
                            data: data.to_string(),
                        }),
                        _ => return Err("each image needs `mime` and base64 `data`".to_string()),
                    }
                }
                self.app
                    .submit(&self.active, text("text")?.to_string(), images)
                    .await?;
                Ok(Value::Null)
            }
            "steer" => {
                lead.send(AgentCmd::Steer(UserInput::text(text("text")?)));
                Ok(Value::Null)
            }
            "follow_up" => {
                lead.send(AgentCmd::FollowUp(UserInput::text(text("text")?)));
                Ok(Value::Null)
            }
            "abort" => {
                lead.abort();
                Ok(Value::Null)
            }
            "get_state" => Ok(self.state()),
            "set_model" => {
                let names = command
                    .get("chain")
                    .and_then(Value::as_array)
                    .ok_or("`chain` must be a list of model names")?;
                let mut chain = Vec::with_capacity(names.len());
                for name in names {
                    let name = name.as_str().ok_or("`chain` must be a list of model names")?;
                    chain.push(name.parse::<ModelSpec>().map_err(|e| format!("{name}: {e}"))?);
                }
                let router = &self.app.router;
                let mut usable = router.resolve(&chain);
                usable.retain(|model| router.provider(&model.provider).is_some());
                if usable.is_empty() {
                    return Err("none of these models is configured".to_string());
                }
                lead.send(AgentCmd::SetModel(chain));
                Ok(json!({ "models": usable.iter().map(ToString::to_string).collect::<Vec<_>>() }))
            }
            "set_mode" => {
                let mode: PermissionMode = text("mode")?.parse()?;
                self.app.permissions.set_mode(mode);
                Ok(json!({ "mode": mode.to_string() }))
            }
            "agent_send" => {
                let reached = team()?.send_from_user(text("to")?, text("message")?)?;
                Ok(json!({ "agents": reached }))
            }
            "agent_stop" => {
                let stopped = match text("agent")? {
                    "all" => team()?.stop_all(),
                    agent => team()?.stop(agent)?,
                };
                Ok(json!({ "agents": stopped }))
            }
            "approval_response" => {
                let decision = match text("decision")? {
                    "allow" => ApprovalDecision::AllowOnce,
                    "always" => ApprovalDecision::Always,
                    "deny" => ApprovalDecision::Deny {
                        feedback: command
                            .get("feedback")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    },
                    other => return Err(format!("unknown decision `{other}`: allow, always or deny")),
                };
                let request = text("request_id")?;
                let reply = locked(&self.waiting.approvals).remove(request);
                reply
                    .ok_or_else(|| format!("no approval is waiting as `{request}`"))?
                    .send(decision)
                    .map_err(|_| "the agent is no longer waiting for this approval".to_string())?;
                Ok(Value::Null)
            }
            "ui_response" => {
                let request = text("request_id")?;
                let reply = locked(&self.waiting.answers).remove(request);
                reply
                    .ok_or_else(|| format!("no request is waiting as `{request}`"))?
                    .send(command.get("value").cloned().unwrap_or(Value::Null))
                    .map_err(|_| "the agent is no longer waiting for this answer".to_string())?;
                Ok(Value::Null)
            }
            "" => Err("the command has no `type`".to_string()),
            other => Err(format!("unknown command `{other}`")),
        }
    }

    /// Handles one input line and answers it.
    async fn line(&self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        let (id, outcome) = match serde_json::from_str::<Value>(line) {
            Ok(command @ Value::Object(_)) => {
                let outcome = self.run(&command).await;
                (command.get("id").cloned().unwrap_or(Value::Null), outcome)
            }
            Ok(_) => (Value::Null, Err("a command is a JSON object".to_string())),
            Err(e) => (Value::Null, Err(format!("not JSON: {e}"))),
        };
        self.out.send(match outcome {
            Ok(data) => json!({ "id": id, "type": "response", "success": true, "data": data }),
            Err(error) => json!({ "id": id, "type": "response", "success": false, "error": error }),
        });
    }
}

/// Serves the session until standard input closes.
pub async fn run(app: Arc<App>, active: Active) -> u8 {
    let (lines, mut queued) = mpsc::unbounded_channel::<Option<String>>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(Some(mut line)) = queued.recv().await {
            line.push('\n');
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });
    let out = Out(lines);
    let waiting = Arc::new(Waiting::default());
    app.registry.services.insert(Arc::new(Ui(Arc::new(RpcBridge {
        out: out.clone(),
        waiting: waiting.clone(),
    }))));

    let team = app.registry.services.get::<Team>();
    let mut events = active.lead.subscribe();
    let mut team_events = team.as_ref().map(|team| team.subscribe());
    let lead = active.lead.id.clone();
    let source = if active.resumed {
        StartSource::Resume
    } else {
        StartSource::Startup
    };
    app.start(source).await;
    let session = Session {
        app,
        active,
        team,
        waiting,
        out: out.clone(),
    };

    let mut input = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            line = input.next_line() => match line {
                Ok(Some(line)) => session.line(&line).await,
                _ => break,
            },
            Some(event) = events.recv() => out.send(event_json(&lead, &event)),
            Some(event) = async { Some(team_events.as_mut()?.recv().await) } => match event {
                Ok(TeamEvent::Agent { id, event }) => out.send(event_json(&id, &event)),
                Ok(TeamEvent::Roster) => out.send(json!({ "type": "roster", "members": session.roster() })),
                // Events were missed; the roster says where everyone stands now.
                Err(RecvError::Lagged(_)) => out.send(json!({ "type": "roster", "members": session.roster() })),
                Err(RecvError::Closed) => team_events = None,
            },
        }
    }

    // Requests still open end the way they do when nobody is there.
    locked(&session.waiting.approvals).clear();
    locked(&session.waiting.answers).clear();
    session.app.finish(&session.active).await;
    // The bridge the agents were given keeps a sender, so the end of the output is said, not
    // inferred from the channel closing.
    let _ = out.0.send(None);
    let _ = writer.await;
    EXIT_OK
}
