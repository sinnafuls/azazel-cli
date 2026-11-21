//! The tools agents use to work as a team: `task`, `agent_send`, `agent_stop`, `agent_wait`,
//! `agent_list`, `agent_status`, `worktree_apply` and `worktree_discard`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_ai::ModelSpec;
use azazel_core::agents::Isolation;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::status::{status_table, status_text};
use crate::team::{Finished, MemberInfo, SpawnRequest, Team, WaitAll, WaitOutcome};

/// Characters of a report returned inline; the rest is read through `agent://<id>`.
pub const REPORT_LIMIT: usize = 8000;
const DEFAULT_WAIT_SECS: u64 = 600;

fn team(ctx: &ToolCtx) -> Result<Arc<Team>, ToolOutput> {
    ctx.services
        .get::<Team>()
        .ok_or_else(|| ToolOutput::error("no team is available in this session"))
}

fn parse<T: for<'de> Deserialize<'de>>(input: Value) -> Result<T, ToolOutput> {
    serde_json::from_value(input).map_err(|e| ToolOutput::error(format!("invalid arguments: {e}")))
}

/// A result as a section of tool output, with long reports cut and pointed to.
pub fn result_section(finished: &Finished) -> String {
    let mut report: String = finished.report.chars().take(REPORT_LIMIT).collect();
    if report.len() < finished.report.len() {
        report.push_str(&format!("\n[report cut; full report: agent://{}]", finished.id));
    }
    format!(
        "## {} ({}, {}s)\n{report}",
        finished.id,
        finished.state.label(),
        finished.elapsed.as_secs()
    )
}

/// Which agents a `task` call ran and how each ended, for a caller that is a program; their
/// results are read from the team by id.
fn results_details(results: &[Finished]) -> Value {
    let agents: Vec<Value> = results
        .iter()
        .map(|finished| json!({ "id": finished.id, "status": finished.state.label() }))
        .collect();
    json!({ "agents": agents })
}

/// One line per agent: id, definition, state, time, tokens and cost.
pub fn roster_table(members: &[MemberInfo]) -> String {
    let mut out = String::new();
    for member in members {
        let cost = member
            .cost_usd
            .map(|cost| format!(" · ${cost:.2}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "{}{} [{}] {} · {}s · {} tokens{cost}\n",
            "  ".repeat(member.depth as usize),
            member.id,
            member.def,
            member.state,
            member.elapsed.as_secs(),
            member.tokens
        ));
    }
    out
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct TaskAgent {
    agent: String,
    prompt: String,
    name: Option<String>,
    model: Option<OneOrMany>,
    isolation: Option<String>,
    schema: Option<Value>,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct TaskInput {
    agents: Vec<TaskAgent>,
    #[serde(default = "yes")]
    wait: bool,
}

/// `spec` as a model: `provider/model[:level]`, `@role`, or a bare model id that exactly one of
/// `catalog`'s `providers` has (`deepseek-flash` → `deepseek/deepseek-flash`). A bare name that
/// is ambiguous or unknown fails with the full names to use instead, so the next try is right.
fn model_spec(spec: &str, catalog: &azazel_ai::Catalog, providers: &[String]) -> Result<ModelSpec, String> {
    let parsed = spec.parse::<ModelSpec>();
    let bare = !spec.trim().starts_with('@') && !spec.contains('/');
    let error = match parsed {
        Ok(model) => return Ok(model),
        Err(_) if !bare => return parsed.map_err(|e| format!("model `{spec}`: {e}")),
        Err(e) => e,
    };
    let name = spec.trim().rsplit_once(':').map_or(spec.trim(), |(name, _)| name);
    let listed = |refs: &[azazel_ai::ModelRef]| {
        refs.iter()
            .map(|r| format!("`{r}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match catalog.find(spec, providers) {
        Ok(found) if found.model.eq_ignore_ascii_case(name) => Ok(ModelSpec::Model(found)),
        Ok(near) => Err(format!(
            "model `{spec}`: no model has that id; did you mean {}? Give it as `provider/model`",
            listed(&[near])
        )),
        Err(many) if !many.is_empty() => Err(format!(
            "model `{spec}`: name the provider, use one of {}",
            listed(&many)
        )),
        Err(_) => Err(format!(
            "model `{spec}`: {error}; no usable provider has a model of that name"
        )),
    }
}

fn spawn_request(
    agent: TaskAgent,
    resolve: &dyn Fn(&str) -> Result<ModelSpec, String>,
) -> Result<SpawnRequest, String> {
    let model = match agent.model {
        None => None,
        Some(OneOrMany::One(spec)) => Some(vec![spec]),
        Some(OneOrMany::Many(specs)) => Some(specs),
    };
    let model = model
        .map(|specs| {
            specs
                .iter()
                .map(|spec| resolve(spec))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    let isolation = match agent.isolation.as_deref() {
        None => None,
        Some("none") => Some(Isolation::None),
        Some("worktree") => Some(Isolation::Worktree),
        Some(other) => return Err(format!("isolation `{other}`: expected `none` or `worktree`")),
    };
    if agent.prompt.trim().is_empty() {
        return Err(format!("the prompt for `{}` is empty", agent.agent));
    }
    if agent.schema.as_ref().is_some_and(|schema| !schema.is_object()) {
        return Err(format!(
            "the schema for `{}` must be a JSON Schema object",
            agent.agent
        ));
    }
    Ok(SpawnRequest {
        def: agent.agent,
        prompt: agent.prompt,
        name: agent.name,
        model,
        isolation,
        tools: None,
        schema: agent.schema,
    })
}

pub struct TaskTool;

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "Hand work to specialist agents. Every agent listed starts with a fresh context and sees only its \
         prompt, so each prompt must stand on its own: the goal, the paths that matter, constraints, and what \
         to report back. All agents of one call run in parallel. With `wait` (the default) the call returns \
         every agent's report; with `wait: false` it returns their ids at once and each report arrives later \
         as a message. Give an agent a `schema` when you need its result as data: it then reports a JSON \
         value that matches the schema instead of prose. When an agent has to wait for a free slot the call \
         returns at once and names the queued agents."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agents": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "agent": { "type": "string", "description": "Name of the agent definition" },
                            "prompt": { "type": "string", "description": "The complete task for this agent" },
                            "name": { "type": "string", "description": "Id for the agent; default <agent>-<n>" },
                            "model": {
                                "description": "Model as `provider/model[:thinking]` (a bare model id works when only one provider has it), `@role`, or a fallback chain of those; overrides the definition",
                                "anyOf": [{ "type": "string" }, { "type": "array", "items": { "type": "string" } }]
                            },
                            "isolation": { "type": "string", "enum": ["none", "worktree"] },
                            "schema": {
                                "type": "object",
                                "description": "JSON Schema the agent's result must match; the result is then JSON"
                            }
                        },
                        "required": ["agent", "prompt"]
                    }
                },
                "wait": { "type": "boolean", "description": "Wait for the reports (default true)" }
            },
            "required": ["agents"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let team = match team(ctx) {
            Ok(team) => team,
            Err(output) => return output,
        };
        let input: TaskInput = match parse(input) {
            Ok(input) => input,
            Err(output) => return output,
        };
        if input.agents.is_empty() {
            return ToolOutput::error("`agents` is empty");
        }
        let router = team.router();
        let providers: Vec<String> = router.provider_ids().into_iter().map(str::to_string).collect();
        let resolve = |spec: &str| model_spec(spec, router.catalog(), &providers);
        let requests: Result<Vec<SpawnRequest>, String> = input
            .agents
            .into_iter()
            .map(|agent| spawn_request(agent, &resolve))
            .collect();
        let requests = match requests {
            Ok(requests) => requests,
            Err(why) => return ToolOutput::error(why),
        };
        let mut ids = Vec::new();
        let mut queued = Vec::new();
        for request in requests {
            match team.spawn(&ctx.agent, request, !input.wait).await {
                Ok(id) => {
                    if team.is_queued(&id) {
                        queued.push(id.clone());
                    }
                    ids.push(id);
                }
                Err(why) => {
                    // All or nothing: a half-started batch would leave the caller guessing.
                    for id in &ids {
                        let _ = team.stop(id);
                    }
                    return ToolOutput::error(why);
                }
            }
        }
        // With a slot short, waiting would block the caller behind agents that are not running
        // yet: the call returns, and every agent of it reports as a message.
        if !input.wait || !queued.is_empty() {
            // Agents of a `wait: true` call were not set to report as messages; now they are.
            let (finished, running) = match team.leave_running(&ids) {
                WaitAll::Steered { finished, running } => (finished, running),
                _ => (Vec::new(), ids.clone()),
            };
            let started: Vec<String> = running.into_iter().filter(|id| !queued.contains(id)).collect();
            let mut out = placement_text(&started, &queued, team.slots_busy(), team.max_parallel());
            for finished in &finished {
                out.push_str("\n\n");
                out.push_str(&result_section(finished));
            }
            return ToolOutput::text(out);
        }
        match team.wait_for(&ctx.agent, &ids, &ctx.cancel).await {
            WaitAll::Finished(results) => {
                let sections: Vec<String> = results.iter().map(result_section).collect();
                ToolOutput::text(sections.join("\n\n")).with_details(results_details(&results))
            }
            WaitAll::Steered { finished, running } => {
                ToolOutput::text(steered_text(&finished, &running)).with_details(results_details(&finished))
            }
            WaitAll::Cancelled => ToolOutput::error("cancelled; the agents this call started were stopped"),
        }
    }
}

/// Which agents of a `task` call run and which wait for a slot, and what the caller can do.
fn placement_text(started: &[String], queued: &[String], busy: usize, slots: usize) -> String {
    let mut out = String::new();
    if !started.is_empty() {
        out.push_str(&format!("started: {}. ", started.join(", ")));
    }
    if !queued.is_empty() {
        let (it, starts) = if queued.len() == 1 {
            ("it", "starts")
        } else {
            ("they", "start")
        };
        out.push_str(&format!(
            "queued: {} ({busy} of {slots} slots busy; {it} {starts} when one frees). ",
            queued.join(", ")
        ));
    }
    out.push_str(
        "The call did not wait. Each result arrives as a message when it is ready, and so does the start \
         of a queued agent. `agent_wait` blocks until the next one, `agent_stop` cancels an agent, or carry on.",
    );
    out
}

/// What a wait that steering input ended says: what finished, and what goes on in the background.
fn steered_text(finished: &[Finished], running: &[String]) -> String {
    let mut out = String::from("stopped waiting: a new message for you arrived; it follows.");
    if !running.is_empty() {
        out.push_str(&format!(
            " Still running in the background: {}. Each result arrives as a message when it is ready; \
             `agent_wait` blocks until the next one.",
            running.join(", ")
        ));
    }
    for finished in finished {
        out.push_str("\n\n");
        out.push_str(&result_section(finished));
    }
    out
}

#[derive(Deserialize)]
struct SendInput {
    to: String,
    message: String,
    #[serde(default)]
    ask: bool,
}

pub struct AgentSendTool;

#[async_trait]
impl Tool for AgentSendTool {
    fn name(&self) -> &str {
        "agent_send"
    }

    fn description(&self) -> &str {
        "Send a message to another agent: an agent id, `parent` (whoever started you), or `all`. It returns \
         at once: an agent that is working sees the message before its next step; one that has finished picks \
         its work up again with the message, keeping what it knows; one still starting gets it with its \
         prompt. With `ask: true` the message is a question answered in seconds from what the agent knows \
         now, without interrupting it or adding to its conversation; the answer is the result of this call."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "An agent id, `parent`, or `all`" },
                "message": { "type": "string" },
                "ask": {
                    "type": "boolean",
                    "description": "Ask one agent a question on the side and return its answer (default false)"
                }
            },
            "required": ["to", "message"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let team = match team(ctx) {
            Ok(team) => team,
            Err(output) => return output,
        };
        let input: SendInput = match parse(input) {
            Ok(input) => input,
            Err(output) => return output,
        };
        if input.ask {
            return match team.ask(&ctx.agent, &input.to, &input.message, &ctx.cancel).await {
                Ok(answer) => ToolOutput::text(answer),
                Err(why) => ToolOutput::error(why),
            };
        }
        match team.send(&ctx.agent, &input.to, &input.message) {
            Ok(reached) => {
                let receipts: Vec<String> = reached
                    .iter()
                    .map(|(id, delivery)| format!("{id}: {delivery}"))
                    .collect();
                let details: Vec<Value> = reached
                    .iter()
                    .map(|(id, delivery)| json!({ "id": id, "delivery": delivery.to_string() }))
                    .collect();
                ToolOutput::text(receipts.join("\n")).with_details(json!({ "agents": details }))
            }
            Err(why) => ToolOutput::error(why),
        }
    }
}

#[derive(Deserialize)]
struct StopInput {
    id: String,
    reason: Option<String>,
}

pub struct AgentStopTool;

#[async_trait]
impl Tool for AgentStopTool {
    fn name(&self) -> &str {
        "agent_stop"
    }

    fn description(&self) -> &str {
        "Stop an agent you started, and every agent it started. What it was doing is abandoned."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "reason": { "type": "string", "description": "Why, for the record" }
            },
            "required": ["id"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let team = match team(ctx) {
            Ok(team) => team,
            Err(output) => return output,
        };
        let input: StopInput = match parse(input) {
            Ok(input) => input,
            Err(output) => return output,
        };
        if !team.manages(&ctx.agent, &input.id) {
            return ToolOutput::error(format!("`{}` is not an agent you started", input.id));
        }
        match team.stop(&input.id) {
            Ok(stopped) => {
                if let Some(reason) = &input.reason {
                    tracing::info!("{} stopped {}: {reason}", ctx.agent.id, stopped.join(", "));
                }
                ToolOutput::text(format!("stopped: {}", stopped.join(", ")))
            }
            Err(why) => ToolOutput::error(why),
        }
    }
}

#[derive(Deserialize)]
struct WaitInput {
    ids: Option<Vec<String>>,
    timeout_secs: Option<u64>,
}

pub struct AgentWaitTool;

#[async_trait]
impl Tool for AgentWaitTool {
    fn name(&self) -> &str {
        "agent_wait"
    }

    fn description(&self) -> &str {
        "Wait until one of the agents you started has a result you have not seen, or a message for you \
         arrives. Returns the results that are ready."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ids": { "type": "array", "items": { "type": "string" }, "description": "Default: every agent you started" },
                "timeout_secs": { "type": "integer", "description": "Default 600" }
            }
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let team = match team(ctx) {
            Ok(team) => team,
            Err(output) => return output,
        };
        let input: WaitInput = match parse(input) {
            Ok(input) => input,
            Err(output) => return output,
        };
        let timeout = Duration::from_secs(input.timeout_secs.unwrap_or(DEFAULT_WAIT_SECS));
        match team.wait_any(&ctx.agent, input.ids, timeout, &ctx.cancel).await {
            Ok(WaitOutcome::Finished(results)) => {
                let sections: Vec<String> = results.iter().map(result_section).collect();
                ToolOutput::text(sections.join("\n\n"))
            }
            Ok(WaitOutcome::Message) => ToolOutput::text("a message for you arrived; it follows"),
            Ok(WaitOutcome::Steered) => ToolOutput::text(
                "stopped waiting: a new message for you arrived; it follows. The agents go on in the background; \
                 call `agent_wait` again for their results.",
            ),
            Ok(WaitOutcome::Timeout) => ToolOutput::text(format!(
                "nothing finished within {} s; the agents are still working (`agent_list` shows them)",
                timeout.as_secs()
            )),
            Ok(WaitOutcome::NothingRunning) => {
                ToolOutput::text("none of those agents is working and there is no new result")
            }
            Ok(WaitOutcome::Cancelled) => ToolOutput::error("cancelled"),
            Err(why) => ToolOutput::error(why),
        }
    }
}

pub struct AgentListTool;

#[async_trait]
impl Tool for AgentListTool {
    fn name(&self) -> &str {
        "agent_list"
    }

    fn description(&self) -> &str {
        "List every agent of the session: id, definition, state, what it is doing, time, tokens and cost."
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, _: Value, ctx: &ToolCtx) -> ToolOutput {
        match team(ctx) {
            Ok(team) => ToolOutput::text(roster_table(&team.list())),
            Err(output) => output,
        }
    }
}

#[derive(Deserialize)]
struct StatusInput {
    id: String,
}

pub struct AgentStatusTool;

#[async_trait]
impl Tool for AgentStatusTool {
    fn name(&self) -> &str {
        "agent_status"
    }

    fn description(&self) -> &str {
        "What an agent is doing, at once and without asking it: state and time, the tool it runs now and for \
         how long, the last lines it wrote, the files it changed (and an isolated agent's uncommitted changes \
         and commits), tokens and cost. `id: \"all\"` gives one line per agent."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "An agent id, or `all`" } },
            "required": ["id"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let team = match team(ctx) {
            Ok(team) => team,
            Err(output) => return output,
        };
        let input: StatusInput = match parse(input) {
            Ok(input) => input,
            Err(output) => return output,
        };
        match status_report(&team, &input.id).await {
            Ok(text) => ToolOutput::text(text),
            Err(why) => ToolOutput::error(why),
        }
    }
}

/// The status of agent `id` in full, or of `all` as a table: what `agent_status` and
/// `agent://<id>/status` return.
pub async fn status_report(team: &Team, id: &str) -> Result<String, String> {
    let statuses = team.status(id).await?;
    Ok(if id == "all" {
        status_table(&statuses)
    } else {
        statuses.iter().map(status_text).collect::<Vec<_>>().join("\n")
    })
}

#[derive(Deserialize)]
struct WorktreeInput {
    agent: String,
}

pub struct WorktreeApplyTool;

#[async_trait]
impl Tool for WorktreeApplyTool {
    fn name(&self) -> &str {
        "worktree_apply"
    }

    fn description(&self) -> &str {
        "Bring the changes an isolated agent made in its worktree into the main tree, then remove the \
         worktree. On a conflict the conflicting paths are returned and the worktree is kept."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "agent": { "type": "string", "description": "Id of the isolated agent" } },
            "required": ["agent"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Write
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let team = match team(ctx) {
            Ok(team) => team,
            Err(output) => return output,
        };
        let input: WorktreeInput = match parse(input) {
            Ok(input) => input,
            Err(output) => return output,
        };
        match team.apply_worktree(&input.agent).await {
            Ok(done) => ToolOutput::text(done),
            Err(why) => ToolOutput::error(why),
        }
    }
}

pub struct WorktreeDiscardTool;

#[async_trait]
impl Tool for WorktreeDiscardTool {
    fn name(&self) -> &str {
        "worktree_discard"
    }

    fn description(&self) -> &str {
        "Throw away an isolated agent's worktree and branch, with whatever it changed there."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "agent": { "type": "string", "description": "Id of the isolated agent" } },
            "required": ["agent"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Write
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let team = match team(ctx) {
            Ok(team) => team,
            Err(output) => return output,
        };
        let input: WorktreeInput = match parse(input) {
            Ok(input) => input,
            Err(output) => return output,
        };
        match team.discard_worktree(&input.agent).await {
            Ok(done) => ToolOutput::text(done),
            Err(why) => ToolOutput::error(why),
        }
    }
}

#[cfg(test)]
mod model_name_tests {
    use azazel_ai::{Catalog, ModelInfo, ModelSpec};

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        catalog.merge([
            ModelInfo::unknown("deepseek", "deepseek-flash"),
            ModelInfo::unknown("deepseek", "deepseek-pro"),
            ModelInfo::unknown("openrouter", "deepseek-pro"),
        ]);
        catalog
    }

    fn resolve(spec: &str) -> Result<String, String> {
        let providers = vec!["deepseek".to_string(), "openrouter".to_string()];
        super::model_spec(spec, &catalog(), &providers).map(|spec| spec.to_string())
    }

    #[test]
    fn a_bare_model_id_one_provider_has_is_taken_and_others_say_what_to_write() {
        assert_eq!(
            resolve("deepseek-flash").as_deref(),
            Ok("deepseek/deepseek-flash")
        );
        assert_eq!(
            resolve("deepseek-flash:high").as_deref(),
            Ok("deepseek/deepseek-flash:high")
        );
        let both = resolve("deepseek-pro").unwrap_err();
        assert!(
            both.contains("`deepseek/deepseek-pro`") && both.contains("`openrouter/deepseek-pro`"),
            "{both}"
        );
        // A near miss is never taken silently: it is offered.
        let near = resolve("deepseek-flsh").unwrap_err();
        assert!(near.contains("did you mean `deepseek/deepseek-flash`"), "{near}");
        assert!(resolve("no-such-model-anywhere").is_err());
        assert_eq!(
            resolve("deepseek/deepseek-pro").as_deref(),
            Ok("deepseek/deepseek-pro")
        );
        assert!(matches!("@slow".parse::<ModelSpec>(), Ok(ModelSpec::Role(_))));
        assert_eq!(resolve("@slow").as_deref(), Ok("@slow"));
    }
}
