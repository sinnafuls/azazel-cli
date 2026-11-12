//! The tools agents use to work as a team: `task`, `agent_send`, `agent_stop`, `agent_wait`,
//! `agent_list`, `worktree_apply` and `worktree_discard`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_ai::ModelSpec;
use azazel_core::agents::Isolation;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::team::{Finished, MemberInfo, SpawnRequest, Team, WaitOutcome};

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

fn spawn_request(agent: TaskAgent) -> Result<SpawnRequest, String> {
    let model = match agent.model {
        None => None,
        Some(OneOrMany::One(spec)) => Some(vec![spec]),
        Some(OneOrMany::Many(specs)) => Some(specs),
    };
    let model = model
        .map(|specs| {
            specs
                .iter()
                .map(|spec| {
                    spec.parse::<ModelSpec>()
                        .map_err(|e| format!("model `{spec}`: {e}"))
                })
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
         value that matches the schema instead of prose."
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
                                "description": "Model or fallback chain, overriding the definition",
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
        let requests: Result<Vec<SpawnRequest>, String> =
            input.agents.into_iter().map(spawn_request).collect();
        let requests = match requests {
            Ok(requests) => requests,
            Err(why) => return ToolOutput::error(why),
        };
        let mut ids = Vec::new();
        for request in requests {
            match team.spawn(&ctx.agent, request, !input.wait).await {
                Ok(id) => ids.push(id),
                Err(why) => {
                    // All or nothing: a half-started batch would leave the caller guessing.
                    for id in &ids {
                        let _ = team.stop(id);
                    }
                    return ToolOutput::error(why);
                }
            }
        }
        if !input.wait {
            return ToolOutput::text(format!(
                "started: {}. Each result arrives as a message when it is ready; `agent_wait` blocks until \
                 the next one.",
                ids.join(", ")
            ));
        }
        match team.wait_all(&ids, &ctx.cancel).await {
            Some(results) => {
                let sections: Vec<String> = results.iter().map(result_section).collect();
                ToolOutput::text(sections.join("\n\n"))
            }
            None => ToolOutput::error("cancelled; the agents this call started were stopped"),
        }
    }
}

#[derive(Deserialize)]
struct SendInput {
    to: String,
    message: String,
}

pub struct AgentSendTool;

#[async_trait]
impl Tool for AgentSendTool {
    fn name(&self) -> &str {
        "agent_send"
    }

    fn description(&self) -> &str {
        "Send a message to another agent: an agent id, `parent` (whoever started you), or `all`. An agent \
         that is working sees it before its next step; one that has finished picks its work up again with the \
         message, keeping what it knows."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "An agent id, `parent`, or `all`" },
                "message": { "type": "string" }
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
        match team.send(&ctx.agent, &input.to, &input.message) {
            Ok(reached) => ToolOutput::text(format!("delivered to: {}", reached.join(", "))),
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
