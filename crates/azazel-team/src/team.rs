//! The team: sub-agents that work for the lead, or for each other, at the same time.

use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use azazel_agent::{
    Agent, AgentConfig, AgentEvent, AgentHandle, AgentHooks, AgentInfo, ArtifactSink, CompactionSettings,
    Gate, Limits, Services, Tool, ToolSource, UserInput,
};
use azazel_ai::{ModelSpec, Router, StopReason, StreamEvent, Usage};
use azazel_core::agents::{AgentDef, AgentDefs, Isolation};
use azazel_core::side::{Btw, Side};
use azazel_core::transcript::activity;
use azazel_core::{
    ArtifactStore, EntryKind, Event, EventBus, Outcome, Paths, PermissionGate, PermissionState,
    PromptBuilder, Rule, SessionStore, ToolCaller, ToolRegistry,
};
use serde_json::Value;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, broadcast, watch};
use tokio_util::sync::CancellationToken;

use crate::report::{NO_REPORT, REPORT_TOOL, ReportTool, Typed};
use crate::status::{MemberStatus, RunningTool, last_lines, written_paths};
use crate::worktree::{self, ApplyError, Worktree};

/// Tools that manage other agents. A sub-agent gets them only as far as its definition allows.
pub const ORCHESTRATION_TOOLS: [&str; 8] = [
    "task",
    "agent_send",
    "agent_stop",
    "agent_wait",
    "agent_list",
    "agent_status",
    "worktree_apply",
    "worktree_discard",
];
/// The id under which the lead is addressed.
pub const LEAD: &str = "lead";
const POLL: Duration = Duration::from_millis(100);
/// How long a question asked on the side (`agent_send` with `ask`) may take.
pub const ASK_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberState {
    /// Waiting for one of the `team.max_parallel` slots.
    Queued,
    Running {
        activity: String,
    },
    WaitingApproval,
    /// The lead between prompts.
    Idle,
    Done,
    Failed(String),
    Stopped,
}

impl MemberState {
    /// Whether the agent's current piece of work is over.
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            MemberState::Done | MemberState::Failed(_) | MemberState::Stopped
        )
    }

    /// Whether the agent is working or about to: queued, running, or held up by a prompt.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            MemberState::Queued | MemberState::Running { .. } | MemberState::WaitingApproval
        )
    }

    /// The word for this state in results and transcripts.
    pub fn label(&self) -> &'static str {
        match self {
            MemberState::Queued => "queued",
            MemberState::Running { .. } => "running",
            MemberState::WaitingApproval => "waiting for approval",
            MemberState::Idle => "idle",
            MemberState::Done => "done",
            MemberState::Failed(_) => "failed",
            MemberState::Stopped => "stopped",
        }
    }
}

impl fmt::Display for MemberState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemberState::Running { activity } if !activity.is_empty() => write!(f, "running: {activity}"),
            MemberState::Failed(why) => write!(f, "failed: {why}"),
            other => f.write_str(other.label()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemberInfo {
    pub id: String,
    pub def: String,
    pub parent: Option<String>,
    pub depth: u32,
    pub state: MemberState,
    pub elapsed: Duration,
    /// Input and output tokens used so far.
    pub tokens: u64,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone)]
pub enum TeamEvent {
    /// A member appeared or changed state.
    Roster,
    /// Something a sub-agent did.
    Agent { id: String, event: Box<AgentEvent> },
    /// What a sub-agent was handed to work on: the prompt it started with, or a message sent
    /// to it since.
    Input { id: String, text: String },
}

/// An id a spawn has chosen for an agent it is still setting up. Once the agent is a member the
/// id is its; when the spawn fails the id is free again.
struct Reservation<'a> {
    reserved: &'a Mutex<HashSet<String>>,
    id: String,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.reserved
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

/// What came of `@name text` typed by the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mentioned {
    /// The agent the text went to.
    pub agent: String,
    /// The agent was started for it, from the definition of that name.
    pub started: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpawnRequest {
    /// Name of the agent definition.
    pub def: String,
    pub prompt: String,
    /// The id to give the agent; `<definition>-<n>` when absent.
    pub name: Option<String>,
    pub model: Option<Vec<ModelSpec>>,
    pub isolation: Option<Isolation>,
    /// Narrows the agent to these tools, within what its definition grants (a skill's
    /// `allowed-tools`). `None` leaves the definition's grant as it is.
    pub tools: Option<Vec<Rule>>,
    /// A JSON Schema the agent's result must match; it then reports a value through `report`
    /// instead of prose. `None` uses the definition's `output-schema`, if it has one.
    pub schema: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub id: String,
    pub state: MemberState,
    /// What the agent hands back, as its caller reads it: its last message, or for a typed
    /// result the value in a fenced block, followed by what happened in its worktree.
    pub report: String,
    /// The value of a typed result alone, as JSON text.
    pub json: Option<String>,
    pub elapsed: Duration,
}

impl Finished {
    /// The form in which a result reaches an agent that did not wait for it.
    pub fn as_message(&self) -> String {
        format!(
            "<agent-result id=\"{}\" status=\"{}\" elapsed=\"{}s\">\n{}\n</agent-result>",
            self.id,
            self.state.label(),
            self.elapsed.as_secs(),
            self.report
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WaitOutcome {
    Finished(Vec<Finished>),
    /// A message for the caller arrived; it is delivered when the call returns.
    Message,
    /// Steering input (what the user typed, a message) reached the caller; it reads it next.
    Steered,
    Timeout,
    /// None of the agents waited for is working.
    NothingRunning,
    Cancelled,
}

/// How a wait for every agent of a batch ended.
#[derive(Debug, Clone, PartialEq)]
pub enum WaitAll {
    /// Every agent finished; their results in the order asked for.
    Finished(Vec<Finished>),
    /// Steering input reached the caller first. `finished` ended meanwhile; `running` go on, and
    /// each of their results reaches the caller as a message when it is ready.
    Steered {
        finished: Vec<Finished>,
        running: Vec<String>,
    },
    /// The call was cancelled; the agents were stopped.
    Cancelled,
}

/// How a message reached an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The agent is working: it reads the message before its next step.
    NextStep,
    /// The agent was at rest: the message started a turn of its own.
    Woke,
    /// The agent has not started yet: the message goes with its prompt.
    Queued,
}

impl fmt::Display for Delivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Delivery::NextStep => "delivered at its next step",
            Delivery::Woke => "woke it",
            Delivery::Queued => "queued",
        })
    }
}

/// What an agent is doing, followed from its events.
#[derive(Debug, Default)]
struct Track {
    /// `(call id, tool, what it works on, when it started, the files it would change)`.
    running: Vec<(String, String, String, Instant, Vec<String>)>,
    /// The text of the message being written now.
    live: String,
    /// The last message that had text.
    said: String,
    files: Vec<String>,
}

impl Track {
    fn see(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageStart => self.live.clear(),
            AgentEvent::Stream(StreamEvent::TextDelta { delta, .. }) => self.live.push_str(delta),
            AgentEvent::MessageEnd { message } => {
                let text = message.text();
                if !text.trim().is_empty() {
                    self.said = text;
                }
                self.live.clear();
            }
            AgentEvent::ToolStart { call_id, name, input } => {
                let what = activity(name, input);
                let detail = what.strip_prefix(name.as_str()).unwrap_or_default().trim();
                self.running.push((
                    call_id.clone(),
                    name.clone(),
                    detail.to_string(),
                    Instant::now(),
                    written_paths(name, input),
                ));
            }
            AgentEvent::ToolEnd { call_id, output, .. } => {
                if let Some(at) = self.running.iter().position(|call| call.0 == *call_id) {
                    let (.., paths) = self.running.remove(at);
                    if !output.is_error {
                        for path in paths {
                            if !self.files.contains(&path) {
                                self.files.push(path);
                            }
                        }
                    }
                }
            }
            AgentEvent::Idle { .. } | AgentEvent::Failed { .. } => {
                self.running.clear();
                self.live.clear();
            }
            _ => {}
        }
    }

    /// The call that has been running longest, and the last lines said.
    fn now(&self) -> (Option<RunningTool>, Vec<String>) {
        let tool = self
            .running
            .first()
            .map(|(_, name, detail, started, _)| RunningTool {
                name: name.clone(),
                detail: detail.clone(),
                elapsed: started.elapsed(),
            });
        let text = if self.live.trim().is_empty() {
            &self.said
        } else {
            &self.live
        };
        (tool, last_lines(text))
    }
}

fn add_usage(total: &mut Option<Usage>, usage: &Usage) {
    total.get_or_insert_with(Usage::default).add(usage);
}

/// What the team needs from the application.
pub struct TeamDeps {
    pub tools: Arc<ToolRegistry>,
    pub hooks: Arc<dyn AgentHooks>,
    pub prompt: Arc<PromptBuilder>,
    pub bus: Arc<EventBus>,
    pub services: Arc<Services>,
    pub router: Arc<Router>,
    pub permissions: Arc<PermissionState>,
    pub paths: Paths,
    pub defs: Arc<AgentDefs>,
    pub max_parallel: usize,
    pub max_depth: u32,
    /// Requests per task for an agent whose definition sets none; 0 is no limit.
    pub max_turns: u32,
    pub max_result_bytes: usize,
    pub compaction: CompactionSettings,
    pub posix_shell: bool,
}

struct Member {
    info: AgentInfo,
    def: Arc<AgentDef>,
    model: Option<Vec<ModelSpec>>,
    /// The tool grant of the request that started it.
    grant: Option<Vec<Rule>>,
    /// The typed result it owes, when it was asked for one.
    typed: Option<Arc<Typed>>,
    state: MemberState,
    handle: Option<AgentHandle>,
    report: Option<String>,
    json: Option<String>,
    transcript: PathBuf,
    started: Instant,
    finished: Option<Instant>,
    /// Pieces of work completed (the first prompt, then one per revival).
    runs: u64,
    /// The latest result has reached whoever it was meant for.
    reported: bool,
    /// Deliver results to the spawner as a message instead of waiting for it to ask.
    notify_parent: bool,
    /// A message was sent to this agent that it has not been handed yet.
    mail: bool,
    worktree: Option<Worktree>,
    patch: Option<PathBuf>,
    stop: CancellationToken,
    track: Track,
    /// What the questions it asked on the side cost.
    asked: Option<Usage>,
    /// Its transcript and its tools, once it is built.
    store: Option<Arc<SessionStore>>,
    tools: Option<Arc<dyn ToolSource>>,
    /// Messages that came before it started.
    pending: Vec<UserInput>,
}

impl Member {
    fn elapsed(&self) -> Duration {
        self.finished
            .unwrap_or_else(Instant::now)
            .duration_since(self.started)
    }

    fn finished(&self) -> Finished {
        Finished {
            id: self.info.id.clone(),
            state: self.state.clone(),
            report: self.report.clone().unwrap_or_default(),
            json: self.json.clone(),
            elapsed: self.elapsed(),
        }
    }

    fn active(&self) -> bool {
        self.state.is_active()
    }
}

struct Lead {
    handle: AgentHandle,
    session: Arc<SessionStore>,
}

struct Inner {
    deps: TeamDeps,
    gate: Arc<dyn Gate>,
    members: Mutex<Vec<Member>>,
    /// Ids that a spawn has chosen and not yet given to its member: two spawns at the same time
    /// must not both take `scout-3`.
    reserved: Mutex<HashSet<String>>,
    lead: RwLock<Option<Lead>>,
    /// The typed result the lead owes (print mode's `--json-schema`).
    lead_report: RwLock<Option<Arc<Typed>>>,
    lead_mail: AtomicBool,
    /// What the lead's questions on the side cost.
    lead_asked: Mutex<Option<Usage>>,
    /// `(caller, the ids it waits for)` of every `agent_wait` in progress; `None` is all children.
    waiters: Mutex<Vec<(String, Option<Vec<String>>)>>,
    semaphore: Arc<Semaphore>,
    events: broadcast::Sender<TeamEvent>,
    changed: Notify,
}

/// The tools one sub-agent may use: what its definition grants, minus what it disallows and what
/// the request that started it leaves out, with the orchestration tools limited to what its place
/// in the team permits. An agent that owes a typed result is offered `report` whatever it is
/// granted.
struct MemberTools {
    registry: Arc<ToolRegistry>,
    def: Arc<AgentDef>,
    grant: Option<Vec<Rule>>,
    can_spawn: bool,
    typed: Option<Arc<Typed>>,
}

impl ToolSource for MemberTools {
    fn tools(&self, agent: &AgentInfo) -> Vec<Arc<dyn Tool>> {
        let mut tools = self.registry.tools(agent);
        tools.retain(|tool| {
            let name = tool.name();
            // The agent's own `report` takes the place of any other tool of that name.
            if name == REPORT_TOOL && self.typed.is_some() {
                return false;
            }
            if ORCHESTRATION_TOOLS.contains(&name) {
                return name == "agent_send" || (self.can_spawn && !name.starts_with("worktree_"));
            }
            if self.def.disallowed_tools.iter().any(|denied| denied == name) {
                return false;
            }
            let granted = |grants: &Option<Vec<Rule>>| match grants {
                None => true,
                Some(grants) => grants.iter().any(|grant| grant.matches_tool(name)),
            };
            granted(&self.def.tools) && granted(&self.grant)
        });
        if let Some(typed) = &self.typed {
            tools.push(Arc::new(ReportTool(typed.clone())));
        }
        tools
    }
}

/// Removes the worktree of an isolated agent and tells extensions it is gone.
async fn remove_worktree(deps: &TeamDeps, agent_id: &str, worktree: &Worktree) -> Result<(), String> {
    worktree::remove(&deps.paths.project_root, worktree).await?;
    let mut removed = Event::WorktreeRemove {
        agent_id: agent_id.to_string(),
        path: worktree.path.clone(),
    };
    deps.bus.emit(&mut removed).await;
    Ok(())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl Inner {
    fn members(&self) -> MutexGuard<'_, Vec<Member>> {
        self.members.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lead(&self) -> Option<(AgentHandle, Arc<SessionStore>)> {
        let lead = self.lead.read().unwrap_or_else(PoisonError::into_inner);
        lead.as_ref()
            .map(|lead| (lead.handle.clone(), lead.session.clone()))
    }

    /// Applies `change` to member `id` and tells everyone watching.
    fn update(&self, id: &str, change: impl FnOnce(&mut Member)) {
        if let Some(member) = self.members().iter_mut().find(|member| member.info.id == id) {
            change(member);
        }
        self.announce();
    }

    fn announce(&self) {
        let _ = self.events.send(TeamEvent::Roster);
        self.changed.notify_waiters();
    }

    fn set_activity(&self, id: &str, activity: impl Into<String>) {
        let activity = activity.into();
        self.update(id, |member| member.state = MemberState::Running { activity });
    }

    fn handle_of(&self, id: &str) -> Option<AgentHandle> {
        if id == LEAD {
            return self.lead().map(|(handle, _)| handle);
        }
        self.members()
            .iter()
            .find(|member| member.info.id == id)
            .and_then(|member| member.handle.clone())
    }

    /// `id` and every agent it spawned, directly or not.
    fn family(&self, id: &str) -> Vec<String> {
        let members = self.members();
        let mut family = vec![id.to_string()];
        let mut next = 0;
        while let Some(parent) = family.get(next).cloned() {
            for member in members.iter() {
                if member.info.parent.as_deref() == Some(parent.as_str()) && !family.contains(&member.info.id)
                {
                    family.push(member.info.id.clone());
                }
            }
            next += 1;
        }
        family
    }

    /// Creates the agent behind member `id`.
    async fn build(&self, id: &str) -> Result<AgentHandle, String> {
        let deps = &self.deps;
        let (info, def, model, grant, typed, worktree, transcript) = {
            let members = self.members();
            let member = members
                .iter()
                .find(|member| member.info.id == id)
                .ok_or("the agent is gone")?;
            (
                member.info.clone(),
                member.def.clone(),
                member.model.clone(),
                member.grant.clone(),
                member.typed.clone(),
                member.worktree.clone(),
                member.transcript.clone(),
            )
        };
        let chain = model
            .or_else(|| def.model.clone())
            .unwrap_or_else(|| vec![ModelSpec::Role("default".to_string())]);
        if deps.router.resolve(&chain).is_empty() {
            let wanted: Vec<String> = chain.iter().map(ToString::to_string).collect();
            return Err(format!(
                "no model is configured for agent `{}` (wanted: {})",
                def.name,
                wanted.join(", ")
            ));
        }
        let cwd = match &worktree {
            Some(worktree) => {
                deps.permissions.set_agent_cwd(id, worktree.path.clone());
                worktree.path.clone()
            }
            None => deps.paths.cwd.clone(),
        };
        // An agent started by one that is kept in plan mode plans too: the session mode would
        // otherwise let it do what its parent may not (the `plan` primary agent).
        let parent_plans = info
            .parent
            .as_deref()
            .is_some_and(|parent| deps.permissions.kept_in_plan_mode(parent));
        if def.plan_mode || parent_plans {
            deps.permissions.keep_in_plan_mode(id);
        }
        if let Some(grants) = &def.tools {
            deps.permissions.restrict_agent(id, grants.clone());
        }
        if let Some(grants) = &grant {
            deps.permissions.narrow_agent(id, grants.clone());
        }
        let lead_session = self.lead().map(|(_, session)| session.path());
        let sink = Arc::new(
            SessionStore::create_at(&transcript, id, &cwd, lead_session.as_deref())
                .map_err(|e| format!("cannot write {}: {e}", transcript.display()))?,
        );
        let can_spawn = !def.spawns.is_empty() && info.depth < deps.max_depth;
        let system = deps.prompt.render(&info).await;
        let tools = Arc::new(MemberTools {
            registry: deps.tools.clone(),
            def: def.clone(),
            grant,
            can_spawn,
            typed,
        });
        // What the agent reaches through a script is what it is offered itself.
        if let Some(caller) = deps.services.get::<ToolCaller>() {
            caller.scope_agent(id, tools.clone());
        }
        // A question asked on the side about this agent reads its conversation and is framed
        // with its own tools, so the request starts as its own requests do.
        if let Some(member) = self.members().iter_mut().find(|member| member.info.id == id) {
            member.store = Some(sink.clone());
            member.tools = Some(tools.clone());
        }
        Ok(Agent::spawn(AgentConfig {
            info,
            system,
            tools,
            chain,
            thinking: def.thinking,
            router: deps.router.clone(),
            gate: self.gate.clone(),
            bus: deps.hooks.clone(),
            services: deps.services.clone(),
            cwd,
            initial_messages: Vec::new(),
            sink,
            artifacts: deps
                .services
                .get::<ArtifactStore>()
                .map(|store| store as Arc<dyn ArtifactSink>),
            limits: Limits {
                max_turns: def.max_turns.or((deps.max_turns > 0).then_some(deps.max_turns)),
                max_result_bytes: deps.max_result_bytes,
            },
            compaction: deps.compaction,
        }))
    }

    /// What an isolated agent changed, saved as a patch and described for its report.
    async fn collect_worktree(&self, id: &str, worktree: &Worktree) -> (String, Option<PathBuf>, bool) {
        match worktree::changes(worktree).await {
            Ok(Some(changes)) => {
                let saved = self
                    .deps
                    .services
                    .get::<ArtifactStore>()
                    .ok_or_else(|| "no artifact store".to_string())
                    .and_then(|store| {
                        store
                            .save(&format!("{id}.patch"), &changes.patch)
                            .map_err(|e| e.to_string())
                    });
                match saved {
                    Ok(patch) => (
                        format!(
                            "Changes made in the worktree (not in the main tree yet; `worktree_apply` with agent \
                             \"{id}\" applies them, `worktree_discard` drops them):\n{}\npatch: {}",
                            changes.diffstat,
                            patch.display()
                        ),
                        Some(patch),
                        true,
                    ),
                    Err(e) => (
                        format!("The worktree has changes, but the patch could not be saved: {e}"),
                        None,
                        true,
                    ),
                }
            }
            Ok(None) => ("The worktree has no changes.".to_string(), None, false),
            Err(e) => (
                format!("The worktree's changes could not be collected: {e}"),
                None,
                true,
            ),
        }
    }

    /// Records the end of a piece of work of member `id` and hands the result on. `json` is the
    /// value of a typed result; `report` then shows it.
    async fn finish(&self, id: &str, state: MemberState, report: String, json: Option<String>) {
        let worktree = self
            .members()
            .iter()
            .find(|m| m.info.id == id)
            .and_then(|m| m.worktree.clone());
        let mut report = if report.trim().is_empty() {
            "(the agent gave no report)".to_string()
        } else {
            report
        };
        let mut patch = None;
        let mut keep_worktree = true;
        if let Some(worktree) = &worktree {
            let (note, saved, keep) = self.collect_worktree(id, worktree).await;
            report = format!("{report}\n\n{note}");
            if worktree.started_dirty {
                report = format!("{}\n\n{report}", worktree::DIRTY_NOTE);
            }
            patch = saved;
            keep_worktree = keep;
            if !keep && let Err(e) = remove_worktree(&self.deps, id, worktree).await {
                tracing::warn!("could not remove the worktree of {id}: {e}");
            }
        }

        let recorded = {
            let mut members = self.members();
            let waiters = self.waiters.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(member) = members.iter_mut().find(|member| member.info.id == id) else {
                return;
            };
            member.state = state;
            member.report = Some(report);
            member.json = json;
            member.finished = Some(Instant::now());
            member.runs += 1;
            if worktree.is_some() {
                member.patch = patch;
                if !keep_worktree {
                    member.worktree = None;
                }
            }
            // A parent that is waiting collects the result itself.
            let awaited = waiters.iter().any(|(caller, ids)| {
                member.info.parent.as_deref() == Some(caller.as_str())
                    && ids.as_ref().is_none_or(|ids| ids.contains(&member.info.id))
            });
            let deliver = member.notify_parent && !awaited;
            member.reported = deliver;
            (
                member.info.clone(),
                member.finished(),
                member.transcript.clone(),
                deliver,
            )
        };
        let (info, finished, transcript, deliver) = recorded;

        if let Some((_, session)) = self.lead() {
            session.append(EntryKind::Agent {
                agent_id: info.id.clone(),
                def: info.def.clone(),
                status: finished.state.label().to_string(),
                transcript: transcript.display().to_string(),
            });
        }
        let mut event = Event::AgentEnd {
            agent: info.clone(),
            status: finished.state.label().to_string(),
        };
        self.deps.bus.emit(&mut event).await;
        self.announce();
        if deliver && let Some(parent) = info.parent.as_deref().and_then(|parent| self.handle_of(parent)) {
            parent.follow_up(UserInput::text(finished.as_message()));
        }
    }

    /// The life of one sub-agent: wait for a slot, start, then follow what it does until it is
    /// shut down.
    async fn run_member(self: Arc<Self>, id: String, prompt: String) {
        let Some((stop, typed)) = self
            .members()
            .iter()
            .find(|m| m.info.id == id)
            .map(|m| (m.stop.clone(), m.typed.clone()))
        else {
            return;
        };
        let slot = tokio::select! {
            permit = self.semaphore.clone().acquire_owned() => permit.ok(),
            _ = stop.cancelled() => None,
        };
        let Some(slot) = slot else {
            return self
                .finish(
                    &id,
                    MemberState::Stopped,
                    "stopped before it started".to_string(),
                    None,
                )
                .await;
        };
        let handle = match self.build(&id).await {
            Ok(handle) => handle,
            Err(why) => {
                return self
                    .finish(&id, MemberState::Failed(why.clone()), why, None)
                    .await;
            }
        };
        let mut events = handle.subscribe();
        let _ = self.events.send(TeamEvent::Input {
            id: id.clone(),
            text: prompt.clone(),
        });
        // Messages sent while it was queued go with its prompt, so its first request has them.
        // The roster is held meanwhile: a later message finds the handle and comes after.
        self.update(&id, |member| {
            let mut content = UserInput::text(prompt).content;
            for input in std::mem::take(&mut member.pending) {
                content.extend(input.content);
            }
            handle.prompt(UserInput::blocks(content));
            member.handle = Some(handle.clone());
            member.state = MemberState::Running {
                activity: "starting".to_string(),
            };
        });

        let mut slot: Option<OwnedSemaphorePermit> = Some(slot);
        let mut said = String::new();
        while let Some(event) = events.recv().await {
            if let Some(member) = self.members().iter_mut().find(|member| member.info.id == id) {
                member.track.see(&event);
            }
            let _ = self.events.send(TeamEvent::Agent {
                id: id.clone(),
                event: Box::new(event.clone()),
            });
            match event {
                AgentEvent::AgentStart => {
                    // A revived agent works even when every slot is taken; it takes one if it can.
                    if slot.is_none() {
                        slot = self.semaphore.clone().try_acquire_owned().ok();
                    }
                    said.clear();
                    // Picked up again with a message: the earlier result no longer stands.
                    if let Some(typed) = &typed
                        && self.members().iter().any(|m| m.info.id == id && m.runs > 0)
                    {
                        typed.reset();
                    }
                    self.update(&id, |member| {
                        member.mail = false;
                        member.finished = None;
                        member.state = MemberState::Running {
                            activity: "thinking".to_string(),
                        };
                    });
                }
                AgentEvent::MessageStart | AgentEvent::ToolEnd { .. } => self.set_activity(&id, "thinking"),
                AgentEvent::MessageEnd { message } => {
                    let text = message.text();
                    if !text.trim().is_empty() {
                        said = text;
                    }
                }
                AgentEvent::ToolStart { name, input, .. } => self.set_activity(&id, activity(&name, &input)),
                AgentEvent::ToolUpdate { .. } => {
                    // Output is flowing, so an approval the call may have waited for was given.
                    self.update(&id, |member| {
                        if member.state == MemberState::WaitingApproval {
                            member.state = MemberState::Running {
                                activity: "working".to_string(),
                            };
                        }
                    });
                }
                AgentEvent::Idle { stop: reason } => {
                    slot = None;
                    let stopped = reason == StopReason::Aborted || self.stop_requested(&id);
                    let said = std::mem::take(&mut said);
                    let json = typed.as_ref().and_then(|typed| typed.json());
                    let (state, report) = match (&json, stopped) {
                        (Some(json), _) => {
                            let state = if stopped {
                                MemberState::Stopped
                            } else {
                                MemberState::Done
                            };
                            (state, format!("```json\n{json}\n```"))
                        }
                        (None, true) => (MemberState::Stopped, said),
                        (None, false) if typed.is_some() => {
                            let mut report = format!("The agent {NO_REPORT}.");
                            if !said.trim().is_empty() {
                                report.push_str(&format!(" Its last message:\n{said}"));
                            }
                            (MemberState::Failed(NO_REPORT.to_string()), report)
                        }
                        (None, false) => (MemberState::Done, said),
                    };
                    self.finish(&id, state, report, json).await;
                }
                AgentEvent::Failed { error } => {
                    slot = None;
                    self.finish(&id, MemberState::Failed(error.clone()), error, None)
                        .await;
                }
                _ => {}
            }
        }
    }

    fn stop_requested(&self, id: &str) -> bool {
        self.members()
            .iter()
            .any(|member| member.info.id == id && member.stop.is_cancelled())
    }

    /// Hands `text` to `target`: as a steer while it works, as a new prompt when it does not,
    /// and with its first prompt when it has not started yet. Never waits for the agent.
    fn deliver(&self, target: &str, text: String) -> Result<Delivery, String> {
        let input = UserInput::text(text.clone());
        if target == LEAD {
            let (handle, _) = self.lead().ok_or("there is no lead to message")?;
            let delivery = if handle.status().busy {
                handle.steer(input);
                Delivery::NextStep
            } else {
                handle.prompt(input);
                Delivery::Woke
            };
            self.lead_mail.store(true, Ordering::SeqCst);
            self.announce();
            return Ok(delivery);
        }
        let mut members = self.members();
        let member = members
            .iter_mut()
            .find(|member| member.info.id == target)
            .ok_or_else(|| format!("no agent named `{target}`"))?;
        let delivery = match member.handle.clone() {
            None if member.state.is_finished() => {
                return Err(format!("agent `{target}` ended before it started"));
            }
            // Still waiting for a slot or being set up: handed over right after its prompt.
            None => {
                member.pending.push(input);
                Delivery::Queued
            }
            // At work, or given its prompt and about to be: commands reach it in order, so a
            // steer sent now is read at its next step.
            Some(handle) if handle.status().busy || member.state.is_active() => {
                handle.steer(input);
                Delivery::NextStep
            }
            Some(handle) => {
                // Revived: whoever spawned it hears about the outcome without having to ask.
                member.stop = CancellationToken::new();
                member.notify_parent = true;
                if !handle.prompt(input) {
                    return Err(format!("agent `{target}` has shut down"));
                }
                Delivery::Woke
            }
        };
        member.mail = true;
        drop(members);
        let _ = self.events.send(TeamEvent::Input {
            id: target.to_string(),
            text,
        });
        self.announce();
        Ok(delivery)
    }
}

/// The agents of a session. Registered as a service; cheap to clone.
#[derive(Clone)]
pub struct Team {
    inner: Arc<Inner>,
}

impl Team {
    pub fn new(deps: TeamDeps) -> Team {
        let gate = PermissionGate::new(
            deps.permissions.clone(),
            deps.bus.clone(),
            deps.services.clone(),
            deps.posix_shell,
        );
        let (events, _) = broadcast::channel(1024);
        let semaphore = Arc::new(Semaphore::new(deps.max_parallel.max(1)));
        Team {
            inner: Arc::new(Inner {
                deps,
                gate: Arc::new(gate),
                members: Mutex::default(),
                reserved: Mutex::default(),
                lead: RwLock::default(),
                lead_report: RwLock::default(),
                lead_mail: AtomicBool::new(false),
                lead_asked: Mutex::default(),
                waiters: Mutex::default(),
                semaphore,
                events,
                changed: Notify::new(),
            }),
        }
    }

    /// The lead of the attached session.
    pub fn lead(&self) -> Option<AgentHandle> {
        self.inner.lead().map(|(handle, _)| handle)
    }

    pub fn defs(&self) -> &AgentDefs {
        &self.inner.deps.defs
    }

    /// How many sub-agents work at the same time (`team.max_parallel`); the rest wait their turn.
    pub fn max_parallel(&self) -> usize {
        self.inner.deps.max_parallel.max(1)
    }

    /// Makes the lead owe a typed result (print mode's `--json-schema`).
    pub fn expect_lead_report(&self, typed: Arc<Typed>) {
        *self
            .inner
            .lead_report
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(typed);
    }

    /// The typed result agent `id` owes, if it was asked for one.
    pub fn typed(&self, id: &str) -> Option<Arc<Typed>> {
        if id == LEAD {
            return self
                .inner
                .lead_report
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
        }
        self.inner
            .members()
            .iter()
            .find(|member| member.info.id == id)
            .and_then(|member| member.typed.clone())
    }

    /// Changes of the roster and everything sub-agents do, from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<TeamEvent> {
        self.inner.events.subscribe()
    }

    /// Makes `lead` the agent sub-agents report to and `session` the transcript they are recorded in.
    pub fn attach(&self, lead: AgentHandle, session: Arc<SessionStore>) {
        *self.inner.lead.write().unwrap_or_else(PoisonError::into_inner) = Some(Lead {
            handle: lead,
            session,
        });
    }

    /// Ends every sub-agent and removes what they left behind. Call when the session closes.
    pub async fn detach(&self) {
        let members: Vec<Member> = std::mem::take(&mut *self.inner.members());
        for member in &members {
            member.stop.cancel();
            if let Some(handle) = &member.handle {
                handle.shutdown().await;
            }
            self.inner.deps.permissions.forget_agent(&member.info.id);
            if let Some(caller) = self.inner.deps.services.get::<ToolCaller>() {
                caller.forget_agent(&member.info.id);
            }
            if let Some(worktree) = &member.worktree
                && let Err(e) = remove_worktree(&self.inner.deps, &member.info.id, worktree).await
            {
                tracing::warn!("could not remove the worktree of {}: {e}", member.info.id);
            }
        }
        *self.inner.lead.write().unwrap_or_else(PoisonError::into_inner) = None;
        self.inner.announce();
    }

    /// Starts an agent for `parent`. With `notify_parent` the result is sent to the parent as a
    /// message when it is ready; otherwise the parent collects it with a wait.
    pub async fn spawn(
        &self,
        parent: &AgentInfo,
        request: SpawnRequest,
        notify_parent: bool,
    ) -> Result<String, String> {
        let inner = &self.inner;
        let deps = &inner.deps;
        let def = deps.defs.subagent(&request.def).ok_or_else(|| {
            let known: Vec<String> = deps.defs.subagents().iter().map(|def| def.name.clone()).collect();
            let what = match deps.defs.get(&request.def) {
                Some(_) => format!("`{}` is a primary agent, which only the lead can be", request.def),
                None => format!("no agent definition named `{}`", request.def),
            };
            format!("{what}; there are: {}", known.join(", "))
        })?;
        if !self.may_spawn(parent, &request.def) {
            return Err(format!(
                "agent `{}` may not start a `{}` agent",
                parent.id, request.def
            ));
        }
        let typed = match request.schema.or_else(|| def.output_schema.clone()) {
            Some(schema) => Some(Arc::new(Typed::new(schema).map_err(|why| {
                format!("the result schema for `{}` cannot be used: {why}", def.name)
            })?)),
            None => None,
        };
        let mut event = Event::AgentSpawn {
            parent: parent.clone(),
            def: def.name.clone(),
            prompt: request.prompt,
        };
        if let Outcome::Block { reason } = deps.bus.emit(&mut event).await {
            return Err(reason);
        }
        let Event::AgentSpawn { prompt, .. } = event else {
            return Err("an extension replaced the spawn event".to_string());
        };

        let (lead, session) = inner.lead().ok_or("no session is open")?;
        let session_id = lead.info().session_id.clone();
        // An earlier process of this session (or the chat it was imported from) may have left an
        // agent's transcript under a name: a new agent takes the next free name instead.
        let transcripts = deps.paths.agent_transcripts_dir(&session.id());
        let on_disk = |id: &str| transcripts.join(format!("{id}.jsonl")).exists();
        let reservation = {
            let members = inner.members();
            let mut reserved = inner.reserved.lock().unwrap_or_else(PoisonError::into_inner);
            let taken = |id: &str| {
                id == LEAD || reserved.contains(id) || members.iter().any(|member| member.info.id == id)
            };
            let free = |id: &str| !taken(id) && !on_disk(id);
            let id = match &request.name {
                Some(name) if !valid_id(name) => {
                    return Err(format!(
                        "`{name}` is not a usable agent name: use letters, digits, `-` and `_`"
                    ));
                }
                Some(name) if taken(name) => return Err(format!("an agent named `{name}` already exists")),
                Some(name) if on_disk(name) => (2..)
                    .map(|n| format!("{name}-{n}"))
                    .find(|candidate| free(candidate))
                    .unwrap_or_default(),
                Some(name) => name.clone(),
                None => (1..)
                    .map(|n| format!("{}-{n}", def.name))
                    .find(|candidate| free(candidate))
                    .unwrap_or_default(),
            };
            reserved.insert(id.clone());
            Reservation {
                reserved: &inner.reserved,
                id,
            }
        };
        let id = reservation.id.clone();
        let worktree = match request.isolation.unwrap_or(def.isolation) {
            Isolation::None => None,
            Isolation::Worktree => {
                let short: String = session_id
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .take(8)
                    .collect();
                let worktree =
                    worktree::create(&deps.paths.project_root, &deps.paths.worktrees_dir(), &short, &id)
                        .await?;
                let mut created = Event::WorktreeCreate {
                    agent_id: id.clone(),
                    path: worktree.path.clone(),
                };
                deps.bus.emit(&mut created).await;
                Some(worktree)
            }
        };
        let info = AgentInfo {
            id: id.clone(),
            def: def.name.clone(),
            parent: Some(parent.id.clone()),
            depth: parent.depth + 1,
            session_id: session_id.clone(),
        };
        let transcript = deps
            .paths
            .agent_transcripts_dir(&session.id())
            .join(format!("{id}.jsonl"));
        inner.members().push(Member {
            info,
            def,
            model: request.model,
            grant: request.tools,
            typed,
            state: MemberState::Queued,
            handle: None,
            report: None,
            json: None,
            transcript,
            started: Instant::now(),
            finished: None,
            runs: 0,
            reported: false,
            notify_parent,
            mail: false,
            worktree,
            patch: None,
            stop: CancellationToken::new(),
            track: Track::default(),
            asked: None,
            store: None,
            tools: None,
            pending: Vec::new(),
        });
        // The member holds the id now.
        drop(reservation);
        inner.announce();
        tokio::spawn(inner.clone().run_member(id.clone(), prompt));
        Ok(id)
    }

    /// Waits until every agent in `ids` has finished and returns their results in that order.
    /// `None` when `cancel` fired first; the agents are stopped then.
    pub async fn wait_all(&self, ids: &[String], cancel: &CancellationToken) -> Option<Vec<Finished>> {
        match self.wait_batch(ids, None, cancel).await {
            WaitAll::Finished(results) => Some(results),
            _ => None,
        }
    }

    /// Like [`Team::wait_all`] for agent `caller`, which is blocked in the wait: when steering
    /// input reaches `caller` first (the user typed while it waited), the wait ends at once and
    /// the agents still running go on, their results reaching `caller` as messages.
    pub async fn wait_for(&self, caller: &AgentInfo, ids: &[String], cancel: &CancellationToken) -> WaitAll {
        let steering = self.inner.handle_of(&caller.id).map(|handle| handle.steering());
        self.wait_batch(ids, steering, cancel).await
    }

    async fn wait_batch(
        &self,
        ids: &[String],
        mut steering: Option<watch::Receiver<u64>>,
        cancel: &CancellationToken,
    ) -> WaitAll {
        loop {
            if steering.as_mut().is_some_and(|rx| *rx.borrow_and_update() > 0) {
                return self.leave_running(ids);
            }
            {
                let mut members = self.inner.members();
                let done = ids.iter().all(|id| {
                    members
                        .iter()
                        .any(|member| member.info.id == *id && member.runs > 0 && member.state.is_finished())
                });
                if done {
                    let mut results = Vec::new();
                    for id in ids {
                        if let Some(member) = members.iter_mut().find(|member| member.info.id == *id) {
                            member.reported = true;
                            results.push(member.finished());
                        }
                    }
                    return WaitAll::Finished(results);
                }
            }
            let steered = async {
                match steering.as_mut() {
                    Some(rx) => {
                        if rx.changed().await.is_err() {
                            // The caller is gone; nothing will steer it any more.
                            std::future::pending::<()>().await;
                        }
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = cancel.cancelled() => {
                    for id in ids {
                        let _ = self.stop(id);
                    }
                    return WaitAll::Cancelled;
                }
                _ = steered => {}
                _ = tokio::time::timeout(POLL, self.inner.changed.notified()) => {}
            }
        }
    }

    /// Ends a wait for `ids` early: what finished is returned, and the rest report to their
    /// parent as messages when they are done.
    fn leave_running(&self, ids: &[String]) -> WaitAll {
        let mut members = self.inner.members();
        let mut finished = Vec::new();
        let mut running = Vec::new();
        for id in ids {
            let Some(member) = members.iter_mut().find(|member| member.info.id == *id) else {
                continue;
            };
            if member.runs > 0 && member.state.is_finished() {
                member.reported = true;
                finished.push(member.finished());
            } else {
                member.notify_parent = true;
                running.push(id.clone());
            }
        }
        WaitAll::Steered { finished, running }
    }

    /// Waits until one of `ids` (default: any agent `caller` spawned) has a result `caller` has not
    /// seen, or a message for `caller` arrives.
    pub async fn wait_any(
        &self,
        caller: &AgentInfo,
        ids: Option<Vec<String>>,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<WaitOutcome, String> {
        let inner = &self.inner;
        if let Some(ids) = &ids {
            let members = inner.members();
            if let Some(unknown) = ids
                .iter()
                .find(|id| !members.iter().any(|member| member.info.id == **id))
            {
                return Err(format!("no agent named `{unknown}`"));
            }
        }
        let entry = (caller.id.clone(), ids.clone());
        inner
            .waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry.clone());
        let deadline = Instant::now() + timeout;
        let mut steering = inner.handle_of(&caller.id).map(|handle| handle.steering());
        let outcome = loop {
            if steering.as_mut().is_some_and(|rx| *rx.borrow_and_update() > 0) {
                break WaitOutcome::Steered;
            }
            {
                let mut members = inner.members();
                let mine = |member: &Member| match &ids {
                    Some(ids) => ids.contains(&member.info.id),
                    None => member.info.parent.as_deref() == Some(caller.id.as_str()),
                };
                let mut ready = Vec::new();
                for member in members.iter_mut().filter(|member| mine(member)) {
                    if member.runs > 0 && member.state.is_finished() && !member.reported {
                        member.reported = true;
                        ready.push(member.finished());
                    }
                }
                if !ready.is_empty() {
                    break WaitOutcome::Finished(ready);
                }
                let mail = if caller.id == LEAD {
                    inner.lead_mail.swap(false, Ordering::SeqCst)
                } else {
                    members
                        .iter_mut()
                        .find(|member| member.info.id == caller.id)
                        .is_some_and(|member| std::mem::take(&mut member.mail))
                };
                if mail {
                    break WaitOutcome::Message;
                }
                if !members.iter().any(|member| mine(member) && member.active()) {
                    break WaitOutcome::NothingRunning;
                }
            }
            if Instant::now() >= deadline {
                break WaitOutcome::Timeout;
            }
            let steered = async {
                match steering.as_mut() {
                    Some(rx) => {
                        if rx.changed().await.is_err() {
                            std::future::pending::<()>().await;
                        }
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = cancel.cancelled() => break WaitOutcome::Cancelled,
                _ = steered => {}
                _ = tokio::time::timeout(POLL, inner.changed.notified()) => {}
            }
        };
        let mut waiters = inner.waiters.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(at) = waiters.iter().position(|waiter| *waiter == entry) {
            waiters.remove(at);
        }
        Ok(outcome)
    }

    /// Sends `message` from agent `from` to `to`: an agent id, `parent`, or `all`. Sub-agents can
    /// reach their parent only. Returns who received it and how. Never waits for the receiver.
    pub fn send(&self, from: &AgentInfo, to: &str, message: &str) -> Result<Vec<(String, Delivery)>, String> {
        let to = self.recipient(from, to)?;
        self.send_text(
            &from.id,
            to,
            format!(
                "<agent-message from=\"{}\">\n{message}\n</agent-message>",
                from.id
            ),
        )
    }

    /// Whom a message of `from` addressed to `to` goes to; sub-agents may reach their parent only.
    fn recipient<'a>(&self, from: &'a AgentInfo, to: &'a str) -> Result<&'a str, String> {
        let parent = from.parent.as_deref();
        let to = if to == "parent" {
            parent.ok_or("the lead has no parent")?
        } else {
            to
        };
        if from.depth > 0 && Some(to) != parent {
            return Err("sub-agents can message their parent only: use `to: \"parent\"`".to_string());
        }
        Ok(to)
    }

    /// Sends a message typed by the user to `to` (an agent id or `all`).
    pub fn send_from_user(&self, to: &str, message: &str) -> Result<Vec<String>, String> {
        self.send_text("", to, message.to_string())
            .map(|reached| reached.into_iter().map(|(id, _)| id).collect())
    }

    /// Asks agent `to` a question on the side, for `from`: one request on `to`'s model with its
    /// conversation as it is now, like `/btw` for the lead. `to` keeps working and nothing is
    /// written to its transcript; a finished agent answers from its final transcript. The cost
    /// is counted to `from`, as `btw`. Fails after [`ASK_TIMEOUT`] or when the model fails.
    pub async fn ask(
        &self,
        from: &AgentInfo,
        to: &str,
        question: &str,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let inner = &self.inner;
        let to = self.recipient(from, to)?;
        if to == "all" {
            return Err("a question goes to one agent: name it in `to`".to_string());
        }
        if to == from.id {
            return Err("an agent cannot ask itself".to_string());
        }
        let base = inner
            .deps
            .services
            .get::<Side>()
            .ok_or("side questions are not available in this session")?;
        let (handle, store, tools) = if to == LEAD {
            let (handle, session) = inner.lead().ok_or("there is no lead to ask")?;
            (handle, session, base.tools.clone())
        } else {
            let members = inner.members();
            let member = members
                .iter()
                .find(|member| member.info.id == to)
                .ok_or_else(|| format!("no agent named `{to}`"))?;
            let (Some(handle), Some(store), Some(tools)) =
                (member.handle.clone(), member.store.clone(), member.tools.clone())
            else {
                return Err(format!(
                    "agent `{to}` has not started yet, so it has nothing to answer from; send the question without `ask`"
                ));
            };
            (handle, store, tools)
        };
        // Framed with the agent's own prompt and tools, so its cached prefix is reused. The cost
        // is counted below, once the asker is known to have had an answer.
        let side = Arc::new(Side {
            router: base.router.clone(),
            prompt: base.prompt.clone(),
            tools,
            costs: Arc::new(Default::default()),
        });
        let btw = Btw::about(side, &handle, &store, &[], question);
        let cancel = cancel.child_token();
        let asked = tokio::time::timeout(ASK_TIMEOUT, btw.ask(&mut |_| {}, cancel.clone())).await;
        let answer = match asked {
            Ok(Ok(answer)) => answer,
            Ok(Err(why)) => return Err(format!("`{to}` could not answer: {why}")),
            Err(_) => {
                cancel.cancel();
                return Err(format!(
                    "`{to}` did not answer within {} s; send the question without `ask` to have it answer in its own turn",
                    ASK_TIMEOUT.as_secs()
                ));
            }
        };
        // Counted to the agent that asked, beside what it spent itself.
        if from.id == LEAD {
            add_usage(
                &mut inner.lead_asked.lock().unwrap_or_else(PoisonError::into_inner),
                &answer.usage,
            );
        } else if let Some(member) = inner
            .members()
            .iter_mut()
            .find(|member| member.info.id == from.id)
        {
            add_usage(&mut member.asked, &answer.usage);
        }
        base.costs.add(azazel_core::side::BTW_PURPOSE, &answer.usage);
        inner.announce();
        let mut text = answer.text.trim().to_string();
        if text.is_empty() {
            text = format!("(`{to}` gave no answer)");
        }
        if !answer.dropped_tools.is_empty() {
            text.push_str(&format!(
                "\n\n(`{to}` tried to use {} to answer; ask without `ask` to let it work on the question)",
                answer.dropped_tools.join(", ")
            ));
        }
        Ok(text)
    }

    /// Hands `text` to agent `id` (the lead is `lead`) from outside its turn: a steer message
    /// while it works, a new prompt when it does not. False when it cannot be reached.
    pub fn post(&self, id: &str, text: String) -> bool {
        self.inner.deliver(id, text).is_ok()
    }

    /// `@name text` typed by the user: `text` goes to the agent with the id `name`, or starts
    /// an agent from the definition of that name, whose result reaches the lead as a message
    /// when it is ready. `None` when `name` is neither.
    pub async fn mention(&self, name: &str, text: &str) -> Option<Result<Mentioned, String>> {
        let known = name != LEAD && self.inner.members().iter().any(|member| member.info.id == name);
        if known {
            return Some(self.send_from_user(name, text).map(|_| Mentioned {
                agent: name.to_string(),
                started: false,
            }));
        }
        self.inner.deps.defs.subagent(name)?;
        let Some((lead, _)) = self.inner.lead() else {
            return Some(Err("no session is open".to_string()));
        };
        let request = SpawnRequest {
            def: name.to_string(),
            prompt: text.to_string(),
            name: None,
            model: None,
            isolation: None,
            tools: None,
            schema: None,
        };
        Some(
            self.spawn(lead.info(), request, true)
                .await
                .map(|agent| Mentioned { agent, started: true }),
        )
    }

    fn send_text(&self, sender: &str, to: &str, text: String) -> Result<Vec<(String, Delivery)>, String> {
        let targets: Vec<String> = if to == "all" {
            let members = self.inner.members();
            members
                .iter()
                .filter(|member| member.handle.is_some() && member.info.id != sender)
                .map(|member| member.info.id.clone())
                .collect()
        } else {
            vec![to.to_string()]
        };
        if targets.is_empty() {
            return Err("there is no agent to message".to_string());
        }
        let mut reached = Vec::new();
        for target in targets {
            let delivery = self.inner.deliver(&target, text.clone())?;
            reached.push((target, delivery));
        }
        Ok(reached)
    }

    /// Aborts `id` and every agent it spawned. Returns the ids that were stopped.
    pub fn stop(&self, id: &str) -> Result<Vec<String>, String> {
        let inner = &self.inner;
        if !inner.members().iter().any(|member| member.info.id == id) {
            return Err(format!("no agent named `{id}`"));
        }
        let family = inner.family(id);
        {
            let mut members = inner.members();
            for member in members
                .iter_mut()
                .filter(|member| family.contains(&member.info.id))
            {
                member.stop.cancel();
                if let Some(handle) = &member.handle {
                    handle.abort();
                }
                // An agent at work reports its own end; one at rest is marked here.
                if !member.active() {
                    member.state = MemberState::Stopped;
                }
            }
        }
        inner.announce();
        Ok(family)
    }

    /// Aborts every sub-agent.
    pub fn stop_all(&self) -> Vec<String> {
        let roots: Vec<String> = {
            let members = self.inner.members();
            members
                .iter()
                .filter(|member| member.info.parent.as_deref() == Some(LEAD))
                .map(|member| member.info.id.clone())
                .collect()
        };
        roots
            .iter()
            .filter_map(|id| self.stop(id).ok())
            .flatten()
            .collect()
    }

    /// Whether `parent` may start an agent from definition `def`: the lead may start any; a
    /// sub-agent only those its own definition lists, and only above the depth limit.
    pub fn may_spawn(&self, parent: &AgentInfo, def: &str) -> bool {
        if parent.depth == 0 {
            return true;
        }
        let deps = &self.inner.deps;
        parent.depth < deps.max_depth
            && deps
                .defs
                .get(&parent.def)
                .is_some_and(|own| own.spawns.iter().any(|name| name == def))
    }

    /// Whether `caller` spawned `id`, directly or through its own agents. The lead may manage all.
    pub fn manages(&self, caller: &AgentInfo, id: &str) -> bool {
        caller.depth == 0
            || (id != caller.id && self.inner.family(&caller.id).iter().any(|member| member == id))
    }

    /// The lead, then every sub-agent in the order they were started.
    pub fn list(&self) -> Vec<MemberInfo> {
        let mut out = Vec::new();
        if let Some((lead, _)) = self.inner.lead() {
            let status = lead.status();
            let state = if status.busy {
                MemberState::Running {
                    activity: String::new(),
                }
            } else {
                MemberState::Idle
            };
            out.push(MemberInfo {
                id: LEAD.to_string(),
                def: LEAD.to_string(),
                parent: None,
                depth: 0,
                state,
                elapsed: Duration::ZERO,
                tokens: status.usage.input + status.usage.output,
                cost_usd: status.usage.cost_usd,
            });
        }
        for member in self.inner.members().iter() {
            let usage = member
                .handle
                .as_ref()
                .map(|handle| handle.status().usage)
                .unwrap_or_default();
            out.push(MemberInfo {
                id: member.info.id.clone(),
                def: member.info.def.clone(),
                parent: member.info.parent.clone(),
                depth: member.info.depth,
                state: member.state.clone(),
                elapsed: member.elapsed(),
                tokens: usage.input + usage.output,
                cost_usd: usage.cost_usd,
            });
        }
        out
    }

    /// What agent `id` (or every agent, for `all`) is doing, from the harness's own records:
    /// state and time, the tool it runs now, the last lines it said, the files it changed and,
    /// for an isolated agent, its worktree's changes and commits. No model is asked.
    pub async fn status(&self, id: &str) -> Result<Vec<MemberStatus>, String> {
        let all = id == "all";
        if !all && id != LEAD && !self.inner.members().iter().any(|member| member.info.id == id) {
            return Err(format!("no agent named `{id}`"));
        }
        let mut out = Vec::new();
        let mut worktrees = Vec::new();
        if (all || id == LEAD)
            && let Some((lead, _)) = self.inner.lead()
        {
            let status = lead.status();
            out.push(MemberStatus {
                id: LEAD.to_string(),
                def: LEAD.to_string(),
                depth: 0,
                state: if status.busy {
                    MemberState::Running {
                        activity: String::new(),
                    }
                } else {
                    MemberState::Idle
                },
                elapsed: Duration::ZERO,
                tool: None,
                text: Vec::new(),
                files: Vec::new(),
                worktree: None,
                tokens: status.usage.input + status.usage.output,
                cost_usd: status.usage.cost_usd,
                asked: *self
                    .inner
                    .lead_asked
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner),
            });
            worktrees.push(None);
        }
        for member in self.inner.members().iter() {
            if !all && member.info.id != id {
                continue;
            }
            let usage = member
                .handle
                .as_ref()
                .map(|handle| handle.status().usage)
                .unwrap_or_default();
            let (tool, text) = member.track.now();
            out.push(MemberStatus {
                id: member.info.id.clone(),
                def: member.info.def.clone(),
                depth: member.info.depth,
                state: member.state.clone(),
                elapsed: member.elapsed(),
                tool,
                text,
                files: member.track.files.clone(),
                worktree: None,
                tokens: usage.input + usage.output,
                cost_usd: usage.cost_usd,
                asked: member.asked,
            });
            worktrees.push(member.worktree.clone());
        }
        // Git is asked once the roster is let go, so no other caller waits for it.
        for (status, worktree) in out.iter_mut().zip(worktrees) {
            if let Some(worktree) = worktree {
                status.worktree = Some(worktree::progress(&worktree).await);
            }
        }
        Ok(out)
    }

    /// The latest result of agent `id`, once it has one.
    pub fn result(&self, id: &str) -> Option<Finished> {
        let members = self.inner.members();
        members
            .iter()
            .find(|member| member.info.id == id && member.runs > 0)
            .map(Member::finished)
    }

    pub fn transcript_path(&self, id: &str) -> Option<PathBuf> {
        self.inner
            .members()
            .iter()
            .find(|member| member.info.id == id)
            .map(|member| member.transcript.clone())
    }

    /// The identity of member `id` and, for an isolated agent, the directory it works in.
    pub fn member(&self, id: &str) -> Option<(AgentInfo, Option<PathBuf>)> {
        let members = self.inner.members();
        let member = members.iter().find(|member| member.info.id == id)?;
        Some((
            member.info.clone(),
            member.worktree.as_ref().map(|worktree| worktree.path.clone()),
        ))
    }

    /// Marks `id` as waiting for the user's answer to a permission prompt.
    pub fn waiting_for_approval(&self, id: &str) {
        if id != LEAD {
            self.inner.update(id, |member| {
                if member.active() {
                    member.state = MemberState::WaitingApproval;
                }
            });
        }
    }

    fn isolated(&self, id: &str) -> Result<(Worktree, Option<PathBuf>, bool), String> {
        let members = self.inner.members();
        let member = members
            .iter()
            .find(|member| member.info.id == id)
            .ok_or_else(|| format!("no agent named `{id}`"))?;
        let worktree = member
            .worktree
            .clone()
            .ok_or_else(|| format!("agent `{id}` has no worktree"))?;
        Ok((worktree, member.patch.clone(), member.active()))
    }

    /// Brings an isolated agent's changes into the main tree and removes its worktree. On a
    /// conflict the worktree is kept and the conflicting paths are returned as the error.
    pub async fn apply_worktree(&self, id: &str) -> Result<String, String> {
        let (worktree, patch, active) = self.isolated(id)?;
        if active {
            return Err(format!(
                "agent `{id}` is still working; wait for it or stop it first"
            ));
        }
        let patch = patch.ok_or_else(|| format!("agent `{id}` made no changes"))?;
        let root = &self.inner.deps.paths.project_root;
        match worktree::apply(root, &patch).await {
            Ok(()) => {
                remove_worktree(&self.inner.deps, id, &worktree).await?;
                self.inner.update(id, |member| {
                    member.worktree = None;
                    member.patch = None;
                });
                Ok(format!(
                    "applied the changes of `{id}` to the main tree and removed its worktree"
                ))
            }
            Err(ApplyError::Conflicts(paths)) => Err(format!(
                "the changes of `{id}` conflict with the main tree in: {}. The conflict markers are in those \
                 files; resolve them, then `worktree_discard` the worktree at {}",
                paths.join(", "),
                worktree.path.display()
            )),
            Err(ApplyError::Failed(why)) => Err(format!("the patch of `{id}` does not apply: {why}")),
        }
    }

    /// Throws an isolated agent's worktree and branch away.
    pub async fn discard_worktree(&self, id: &str) -> Result<String, String> {
        let (worktree, _, active) = self.isolated(id)?;
        if active {
            return Err(format!("agent `{id}` is still working; stop it first"));
        }
        remove_worktree(&self.inner.deps, id, &worktree).await?;
        self.inner.update(id, |member| {
            member.worktree = None;
            member.patch = None;
        });
        Ok(format!("removed the worktree of `{id}`"))
    }
}
