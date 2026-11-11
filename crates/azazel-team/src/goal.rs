//! Goals: an objective the lead keeps working on until the checker agrees it is met, the budget
//! runs out, or it says it is blocked.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_agent::{Access, AgentHandle, AgentInfo, Tool, ToolCtx, ToolOutput, ToolSource};
use azazel_core::checkpoint::Checkpoints;
use azazel_core::{
    Command, CommandHandler, CommandOutcome, EntryKind, Event, EventHandler, EventKind, ExtCtx, ExtError,
    Extension, Level, Outcome, Registrar, SessionStore, SystemPartProvider, Ui,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::checker::{Checker, Claim, Report};
use crate::team::Team;

/// What the lead is told when it stops while its goal is still open.
pub const NUDGE: &str =
    "The goal is not marked complete. Continue working, call goal_done with evidence, or call goal_blocked.";
/// Bytes of diff the verifier is shown.
const DIFF_LIMIT: usize = 60_000;
/// How often failing checks are fed back after one prompt when `verify.on_stop` is set.
const ON_STOP_ROUNDS: u32 = 2;
/// Tools after which the project may differ, so that `verify.on_stop` has something to check.
const CHANGING_TOOLS: [&str; 3] = ["write", "edit", "shell"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalStatus {
    Active,
    /// The checker is running.
    Verifying,
    Done,
    Failed,
    Paused,
}

impl GoalStatus {
    pub fn label(self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Verifying => "verifying",
            GoalStatus::Done => "done",
            GoalStatus::Failed => "failed",
            GoalStatus::Paused => "paused",
        }
    }

    fn parse(label: &str) -> Option<GoalStatus> {
        [
            GoalStatus::Active,
            GoalStatus::Verifying,
            GoalStatus::Done,
            GoalStatus::Failed,
            GoalStatus::Paused,
        ]
        .into_iter()
        .find(|status| status.label() == label)
    }

    /// Whether the lead is expected to keep working on the goal.
    pub fn is_open(self) -> bool {
        matches!(self, GoalStatus::Active | GoalStatus::Verifying)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// Failed attempts to finish after which the goal fails.
    pub max_iterations: u32,
    pub max_cost_usd: Option<f64>,
    pub max_minutes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Goal {
    pub id: String,
    pub objective: String,
    pub status: GoalStatus,
    /// Attempts to finish that the checker rejected.
    pub iteration: u32,
    pub budget: Budget,
    /// Why the goal is paused or failed.
    pub reason: Option<String>,
    started: Instant,
    /// The project as it was when the goal began.
    base: Option<String>,
    /// Times in a row the lead stopped without finishing.
    nudges: u32,
}

impl Goal {
    /// One line for the user: the objective, where it stands and why.
    pub fn describe(&self) -> String {
        let mut line = format!(
            "goal: {}\nstatus: {} · attempt {} of {} · {} min",
            self.objective,
            self.status.label(),
            self.iteration + 1,
            self.budget.max_iterations,
            self.started.elapsed().as_secs() / 60
        );
        if let Some(reason) = &self.reason {
            line.push_str(&format!("\n{reason}"));
        }
        line
    }
}

pub struct GoalSettings {
    /// `goal.max_iterations`: the default budget of failed attempts.
    pub max_iterations: u32,
    /// `goal.max_nudges`: times in a row the lead may stop short before the goal is paused.
    pub max_nudges: u32,
    /// `verify.on_stop`: run the check commands when the lead finishes a prompt that changed things.
    pub on_stop: bool,
}

#[derive(Default)]
struct State {
    goal: Option<Goal>,
    lead: Option<AgentHandle>,
    session: Option<Arc<SessionStore>>,
    checkpoints: Option<Arc<Checkpoints>>,
    /// The lead changed something since the user's last prompt.
    changed: bool,
    /// Times the check commands failed after the current prompt.
    stop_rounds: u32,
}

/// The goal of the open session, and the checker. Registered as a service.
pub struct Goals {
    checker: Checker,
    settings: GoalSettings,
    state: Mutex<State>,
}

impl Goals {
    pub fn new(checker: Checker, settings: GoalSettings) -> Goals {
        Goals {
            checker,
            settings,
            state: Mutex::default(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Binds the goals to an opened session. A goal the session's transcript shows as unfinished
    /// comes back paused.
    pub fn attach(&self, lead: AgentHandle, session: Arc<SessionStore>, checkpoints: Arc<Checkpoints>) {
        let unfinished = session
            .branch(None)
            .into_iter()
            .rev()
            .find_map(|entry| match entry.kind {
                EntryKind::Goal {
                    goal_id,
                    objective,
                    status,
                    iteration,
                } => Some((goal_id, objective, status, iteration)),
                _ => None,
            });
        let goal = unfinished.and_then(|(id, objective, status, iteration)| {
            let status = GoalStatus::parse(&status)?;
            (status.is_open() || status == GoalStatus::Paused).then(|| Goal {
                id,
                objective,
                status: GoalStatus::Paused,
                iteration,
                budget: Budget {
                    max_iterations: self.settings.max_iterations,
                    max_cost_usd: None,
                    max_minutes: None,
                },
                reason: Some("the session was reopened; /goal resume continues".to_string()),
                started: Instant::now(),
                base: None,
                nudges: 0,
            })
        });
        *self.state() = State {
            goal,
            lead: Some(lead),
            session: Some(session),
            checkpoints: Some(checkpoints),
            changed: false,
            stop_rounds: 0,
        };
    }

    /// The default budget: `goal.max_iterations` and no limit on cost or time.
    pub fn default_budget(&self) -> Budget {
        Budget {
            max_iterations: self.settings.max_iterations.max(1),
            max_cost_usd: None,
            max_minutes: None,
        }
    }

    pub fn current(&self) -> Option<Goal> {
        self.state().goal.clone()
    }

    /// Writes the goal's state to the session transcript.
    fn record(&self, state: &State) {
        if let (Some(goal), Some(session)) = (&state.goal, &state.session) {
            session.append(EntryKind::Goal {
                goal_id: goal.id.clone(),
                objective: goal.objective.clone(),
                status: goal.status.label().to_string(),
                iteration: goal.iteration,
            });
        }
    }

    /// Changes the goal, records the change, and returns the goal as it is afterwards.
    fn change(&self, change: impl FnOnce(&mut Goal)) -> Option<Goal> {
        let mut state = self.state();
        change(state.goal.as_mut()?);
        self.record(&state);
        state.goal.clone()
    }

    /// Makes `objective` the session's goal. Fails while another goal is open.
    pub async fn start(&self, objective: &str, budget: Budget) -> Result<Goal, String> {
        let objective = objective.trim();
        if objective.is_empty() {
            return Err("the goal needs an objective".to_string());
        }
        let checkpoints = {
            let state = self.state();
            if let Some(open) = state.goal.as_ref().filter(|goal| goal.status.is_open()) {
                return Err(format!(
                    "a goal is already open: {}; /goal drop abandons it",
                    open.objective
                ));
            }
            state.checkpoints.clone()
        };
        let base = match checkpoints {
            Some(checkpoints) => checkpoints.snapshot("goal start").await.ok().flatten(),
            None => None,
        };
        let mut state = self.state();
        let number = state.session.as_ref().map_or(0, |session| {
            session
                .entries()
                .iter()
                .filter(|entry| matches!(entry.kind, EntryKind::Goal { .. }))
                .count()
        });
        state.goal = Some(Goal {
            id: format!("goal-{}", number + 1),
            objective: objective.to_string(),
            status: GoalStatus::Active,
            iteration: 0,
            budget: Budget {
                max_iterations: budget.max_iterations.max(1),
                ..budget
            },
            reason: None,
            started: Instant::now(),
            base,
            nudges: 0,
        });
        self.record(&state);
        state
            .goal
            .clone()
            .ok_or_else(|| "the goal could not be recorded".to_string())
    }

    /// Stops pursuing the goal until it is resumed.
    pub fn pause(&self, reason: &str) -> Option<Goal> {
        self.change(|goal| {
            goal.status = GoalStatus::Paused;
            goal.reason = Some(reason.to_string());
        })
    }

    /// Takes a paused goal up again. `None` when there is none.
    pub fn resume(&self) -> Option<Goal> {
        if self.current()?.status != GoalStatus::Paused {
            return None;
        }
        self.change(|goal| {
            goal.status = GoalStatus::Active;
            goal.reason = None;
            goal.nudges = 0;
        })
    }

    /// Forgets the goal. Returns it as it was.
    pub fn drop_goal(&self) -> Option<Goal> {
        let mut state = self.state();
        let goal = state.goal.as_mut()?;
        if goal.status.is_open() {
            goal.status = GoalStatus::Paused;
            goal.reason = Some("dropped by the user".to_string());
            self.record(&state);
        }
        state.goal.take()
    }

    /// The `mode` part of the lead's system prompt while a goal is open.
    fn prompt(&self) -> Option<String> {
        let goal = self.current().filter(|goal| goal.status.is_open())?;
        Some(format!(
            "<goal>{}</goal>\nYou are working towards this goal and keep going until it is met. When it is, call \
             `goal_done` with a summary of what you did and the evidence that it works; the claim is then \
             checked independently (the project's build and tests are run and a verifier agent inspects the \
             result), so call it only when the objective is really met. If you cannot make progress without \
             the user, call `goal_blocked` with the reason.",
            goal.objective
        ))
    }

    /// What the whole team has spent so far, when prices are known.
    fn cost(&self, ctx_team: Option<&Team>) -> Option<f64> {
        match ctx_team {
            Some(team) => team
                .list()
                .iter()
                .filter_map(|member| member.cost_usd)
                .reduce(|a, b| a + b),
            None => self
                .state()
                .lead
                .as_ref()
                .and_then(|lead| lead.status().usage.cost_usd),
        }
    }

    /// The limit the goal has run into, if any.
    fn over_budget(&self, goal: &Goal, team: Option<&Team>) -> Option<String> {
        if let Some(minutes) = goal.budget.max_minutes
            && goal.started.elapsed() >= Duration::from_secs(minutes * 60)
        {
            return Some(format!("the time budget of {minutes} min is used up"));
        }
        if let (Some(limit), Some(spent)) = (goal.budget.max_cost_usd, self.cost(team))
            && spent >= limit
        {
            return Some(format!(
                "the cost budget of ${limit:.2} is used up (${spent:.2} spent)"
            ));
        }
        None
    }

    /// The lead says the goal is met: runs the checker. `Ok` when it agrees; `Err` carries what
    /// is still wrong, or why the claim could not be taken.
    pub async fn done(
        &self,
        lead: &AgentInfo,
        summary: &str,
        evidence: &str,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let (goal, checkpoints) = {
            let mut state = self.state();
            let checkpoints = state.checkpoints.clone();
            let goal = state.goal.as_mut().ok_or("there is no goal")?;
            if goal.status != GoalStatus::Active {
                return Err(format!("the goal is {}", goal.status.label()));
            }
            goal.status = GoalStatus::Verifying;
            goal.nudges = 0;
            let goal = goal.clone();
            self.record(&state);
            (goal, checkpoints)
        };
        let changes = match (&checkpoints, &goal.base) {
            (Some(checkpoints), Some(base)) => match checkpoints.snapshot("goal check").await {
                Ok(Some(now)) => checkpoints.diff(base, &now, DIFF_LIMIT).await,
                _ => String::new(),
            },
            _ => String::new(),
        };
        let claim = Claim {
            objective: goal.objective.clone(),
            summary: summary.to_string(),
            evidence: evidence.to_string(),
            changes,
        };
        let report = self.checker.check(lead, &claim, cancel).await;
        if report.passed() {
            self.change(|goal| goal.status = GoalStatus::Done);
            return Ok(report.render());
        }
        let after = self.change(|goal| {
            goal.iteration += 1;
            if goal.iteration >= goal.budget.max_iterations {
                goal.status = GoalStatus::Failed;
                goal.reason = Some(format!("not met after {} attempts", goal.iteration));
            } else {
                goal.status = GoalStatus::Active;
            }
        });
        let mut reply = report.render();
        match after {
            Some(goal) if goal.status == GoalStatus::Failed => reply.push_str(&format!(
                "\n\nThat was attempt {} of {}: the goal has failed. Stop working and tell the user what is \
                 still wrong.",
                goal.iteration, goal.budget.max_iterations
            )),
            Some(goal) => reply.push_str(&format!(
                "\n\nFix this and call goal_done again (attempt {} of {}).",
                goal.iteration + 1,
                goal.budget.max_iterations
            )),
            None => {}
        }
        Err(reply)
    }

    /// Runs the checker outside a goal, for `/verify`.
    pub async fn verify(&self, objective: &str, cancel: &CancellationToken) -> Result<Report, String> {
        let lead = self.state().lead.clone().ok_or("no session is open")?;
        let claim = Claim {
            objective: objective.to_string(),
            ..Claim::default()
        };
        Ok(self.checker.check(lead.info(), &claim, cancel).await)
    }

    /// Called when the lead has nothing more to do. Returns what it should be told to go on with,
    /// if anything: the nudge towards an open goal, or failing checks after a prompt that changed
    /// the project.
    async fn lead_stopped(&self, team: Option<&Team>, ui: Option<&Ui>) -> Option<String> {
        let goal = self.current();
        if let Some(goal) = goal.filter(|goal| goal.status == GoalStatus::Active) {
            let pause = |reason: String| {
                self.pause(&reason);
                if let Some(ui) = ui {
                    ui.0.notify(
                        Level::Warn,
                        &format!("goal paused: {reason}. /goal resume continues it"),
                    );
                }
            };
            if let Some(reason) = self.over_budget(&goal, team) {
                pause(reason);
                return None;
            }
            if goal.nudges >= self.settings.max_nudges {
                pause(format!(
                    "the agent stopped {} times in a row without finishing",
                    goal.nudges + 1
                ));
                return None;
            }
            self.change(|goal| goal.nudges += 1);
            return Some(NUDGE.to_string());
        }

        let due = {
            let state = self.state();
            self.settings.on_stop && state.changed && state.stop_rounds < ON_STOP_ROUNDS
        };
        if !due {
            return None;
        }
        let results = self.checker.run_commands(&CancellationToken::new()).await;
        let failed = results.last().filter(|result| !result.passed())?;
        let mut state = self.state();
        state.stop_rounds += 1;
        Some(format!(
            "The project's checks fail after your changes.\n$ {}\n{}\nFix this before you finish.",
            failed.command, failed.tail
        ))
    }

    fn lead_changed_something(&self) {
        self.state().changed = true;
    }

    fn new_prompt(&self) {
        let mut state = self.state();
        state.changed = false;
        state.stop_rounds = 0;
    }
}

#[derive(Deserialize)]
struct DoneInput {
    summary: String,
    #[serde(default)]
    evidence: String,
}

struct GoalDoneTool(Arc<Goals>);

#[async_trait]
impl Tool for GoalDoneTool {
    fn name(&self) -> &str {
        "goal_done"
    }

    fn description(&self) -> &str {
        "Claim that the goal is met. The claim is checked: the project's build and tests are run and a \
         verifier agent inspects the result. Returns `verified`, or what is still wrong."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "summary": { "type": "string", "description": "What you did to meet the goal" },
                "evidence": { "type": "string", "description": "What shows it works: commands run and their results, files to look at" }
            },
            "required": ["summary", "evidence"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    fn concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: DoneInput = match serde_json::from_value(input) {
            Ok(input) => input,
            Err(e) => return ToolOutput::error(format!("invalid arguments: {e}")),
        };
        match self
            .0
            .done(&ctx.agent, &input.summary, &input.evidence, &ctx.cancel)
            .await
        {
            Ok(report) => ToolOutput::text(report),
            Err(why) => ToolOutput::error(why),
        }
    }
}

struct GoalBlockedTool(Arc<Goals>);

#[async_trait]
impl Tool for GoalBlockedTool {
    fn name(&self) -> &str {
        "goal_blocked"
    }

    fn description(&self) -> &str {
        "Say that you cannot get further towards the goal without the user, and why. The goal is paused."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "reason": { "type": "string", "description": "What is in the way and what you need" } },
            "required": ["reason"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let Some(reason) = input
            .get("reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.trim().is_empty())
        else {
            return ToolOutput::error("`reason` must say what is in the way");
        };
        match self.0.pause(&format!("blocked: {reason}")) {
            Some(_) => ToolOutput::text("the goal is paused; tell the user what you need"),
            None => ToolOutput::error("there is no goal"),
        }
    }
}

/// Offers the goal tools to the lead, and only while a goal is open.
struct GoalTools(Arc<Goals>);

impl ToolSource for GoalTools {
    fn tools(&self, agent: &AgentInfo) -> Vec<Arc<dyn Tool>> {
        let open = self.0.current().is_some_and(|goal| goal.status.is_open());
        if agent.depth > 0 || !open {
            return Vec::new();
        }
        vec![
            Arc::new(GoalDoneTool(self.0.clone())),
            Arc::new(GoalBlockedTool(self.0.clone())),
        ]
    }
}

struct GoalPart(Arc<Goals>);

#[async_trait]
impl SystemPartProvider for GoalPart {
    async fn render(&self, agent: &AgentInfo) -> Option<String> {
        if agent.depth > 0 {
            return None;
        }
        self.0.prompt()
    }
}

/// The prompt that sets the lead to work on a goal.
pub fn goal_prompt(objective: &str) -> String {
    format!("Work towards this goal until it is met:\n{objective}")
}

struct GoalCommand(Arc<Goals>);

#[async_trait]
impl CommandHandler for GoalCommand {
    async fn run(&self, args: &str, _: &ExtCtx) -> CommandOutcome {
        let goals = &self.0;
        let none = || CommandOutcome::Show("there is no goal; /goal <objective> sets one".to_string());
        match args.trim() {
            "" | "status" => goals
                .current()
                .map_or_else(none, |goal| CommandOutcome::Show(goal.describe())),
            "pause" => match goals.pause("paused by the user") {
                Some(goal) => CommandOutcome::Show(goal.describe()),
                None => none(),
            },
            "resume" => match goals.resume() {
                Some(goal) => CommandOutcome::Prompt {
                    text: format!("Continue working towards the goal:\n{}", goal.objective),
                    model: None,
                },
                None => CommandOutcome::Error("there is no paused goal".to_string()),
            },
            "drop" => match goals.drop_goal() {
                Some(goal) => CommandOutcome::Show(format!("dropped the goal: {}", goal.objective)),
                None => none(),
            },
            objective => match goals.start(objective, goals.default_budget()).await {
                Ok(goal) => CommandOutcome::Prompt {
                    text: goal_prompt(&goal.objective),
                    model: None,
                },
                Err(why) => CommandOutcome::Error(why),
            },
        }
    }
}

struct VerifyCommand(Arc<Goals>);

#[async_trait]
impl CommandHandler for VerifyCommand {
    async fn run(&self, args: &str, _: &ExtCtx) -> CommandOutcome {
        let objective = match args.trim() {
            "" => "The work done in this session is complete and correct.",
            given => given,
        };
        match self.0.verify(objective, &CancellationToken::new()).await {
            Ok(report) => CommandOutcome::Show(report.render()),
            Err(why) => CommandOutcome::Error(why),
        }
    }
}

/// Keeps the lead going while its goal is open, and feeds failing checks back when asked to.
struct StopWatch(Arc<Goals>);

#[async_trait]
impl EventHandler for StopWatch {
    async fn handle(&self, event: &mut Event, ctx: &ExtCtx) -> Outcome {
        match event {
            Event::Stop { agent, .. } if agent.depth == 0 => {
                let team = ctx.services.get::<Team>();
                let ui = ctx.services.get::<Ui>();
                match self.0.lead_stopped(team.as_deref(), ui.as_deref()).await {
                    Some(reason) => Outcome::Block { reason },
                    None => Outcome::Continue,
                }
            }
            Event::ToolResult { agent, tool, .. }
                if agent.depth == 0 && CHANGING_TOOLS.contains(&tool.as_str()) =>
            {
                self.0.lead_changed_something();
                Outcome::Continue
            }
            Event::UserPrompt { .. } => {
                self.0.new_prompt();
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }
}

/// Registers goals and the checker: the `Goals` service, the goal tools, the lead's goal prompt,
/// `/goal` and `/verify`.
pub struct GoalExt {
    pub goals: Arc<Goals>,
}

impl Extension for GoalExt {
    fn id(&self) -> &str {
        "goal"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        let goals = self.goals.clone();
        r.service(goals.clone());
        r.tool_source(Arc::new(GoalTools(goals.clone())));
        r.system_part("mode", Arc::new(GoalPart(goals.clone())));
        r.command(Command {
            name: "goal".to_string(),
            description: "Set an objective the agent works on until it is verified".to_string(),
            argument_hint: Some("<objective> | status | pause | resume | drop".to_string()),
            handler: Arc::new(GoalCommand(goals.clone())),
        });
        r.command(Command {
            name: "verify".to_string(),
            description: "Run the project's checks and a verifier agent on the work so far".to_string(),
            argument_hint: Some("[what should be true]".to_string()),
            handler: Arc::new(VerifyCommand(goals.clone())),
        });
        let watch = Arc::new(StopWatch(goals));
        r.on(EventKind::Stop, watch.clone());
        r.on(EventKind::ToolResult, watch.clone());
        r.on(EventKind::UserPrompt, watch);
        Ok(())
    }
}
