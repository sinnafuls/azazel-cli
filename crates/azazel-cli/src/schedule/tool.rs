//! The `schedule` tool: an agent adds, lists and removes schedules of the project. Adding and
//! removing run things later with nobody watching, so they ask for permission like a command.

use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::{ExtError, Extension, PermissionState, Registrar};
use azazel_team::Team;
use jiff::Zoned;
use serde_json::{Value, json};

use super::when::When;
use super::{Scheduler, Target, table};

const DESCRIPTION: &str = "Run a prompt later, or again and again: at a time, on an interval, on a cron \
expression, when a CI run fails, or when files change. Each run starts a background session with the prompt \
(`session: \"this\"` sends it to this session instead). A run knows nothing of this conversation unless it \
goes to this session, so write the prompt to stand on its own. `list` shows what is scheduled in this \
project, `remove` takes one out.";

/// Registers the `schedule` tool.
pub struct ScheduleExt {
    pub scheduler: Scheduler,
    pub permissions: Arc<PermissionState>,
    /// The options this run was started with that a scheduled one is to have too.
    pub carried: Vec<String>,
    /// The run keeps no session (`--no-session`): there is none a schedule could go to.
    pub ephemeral: bool,
}

struct ScheduleTool {
    scheduler: Scheduler,
    permissions: Arc<PermissionState>,
    carried: Vec<String>,
    ephemeral: bool,
}

fn text<'a>(input: &'a Value, key: &str) -> &'a str {
    input.get(key).and_then(Value::as_str).unwrap_or_default().trim()
}

impl ScheduleTool {
    /// What the runs of a schedule added now are started with: this session's options, and the
    /// mode, model and thinking level it is on.
    fn run_args(&self, ctx: &ToolCtx) -> Vec<String> {
        let mut args = self.carried.clone();
        args.extend(["--mode".to_string(), self.permissions.mode().to_string()]);
        if let Some(lead) = ctx.services.get::<Team>().and_then(|team| team.lead()) {
            let status = lead.status();
            for spec in &status.chain {
                args.extend(["--model".to_string(), spec.to_string()]);
            }
            args.extend(["--thinking".to_string(), status.thinking.to_string()]);
        }
        args
    }

    async fn add(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let when = When::parse(text(input, "when"), &Zoned::now())?;
        let target = match (text(input, "session"), when.is_trigger()) {
            ("this", _) if self.ephemeral => {
                return Err(
                    "this run keeps no session (--no-session), so no schedule can go to it".to_string(),
                );
            }
            ("this", _) => Target::Session(ctx.session_id.clone()),
            ("new", _) | ("", false) => Target::New,
            ("", true) => Target::Open,
            (other, _) => return Err(format!("`session` is `new` or `this`, not `{other}`")),
        };
        let (scheduler, prompt, args) = (
            self.scheduler.clone(),
            text(input, "prompt").to_string(),
            self.run_args(ctx),
        );
        // Talking to the system's scheduler runs a program and waits for it.
        let added = tokio::task::spawn_blocking(move || {
            let schedule = scheduler.add(when, &prompt, target, args)?;
            Ok::<String, String>(scheduler.added_line(&schedule))
        });
        added.await.map_err(|e| e.to_string())?
    }

    async fn remove(&self, input: &Value) -> Result<String, String> {
        let id = text(input, "id").to_string();
        if id.is_empty() {
            return Err("`remove` needs the `id` of the schedule".to_string());
        }
        let scheduler = self.scheduler.clone();
        let removed = tokio::task::spawn_blocking(move || scheduler.remove(&id));
        let schedule = removed.await.map_err(|e| e.to_string())??;
        Ok(format!("removed schedule {} ({})", schedule.id, schedule.when))
    }
}

#[async_trait]
impl Tool for ScheduleTool {
    fn name(&self) -> &str {
        "schedule"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "list", "remove"]},
                "when": {
                    "type": "string",
                    "description": "`add`: five cron fields (\"0 9 * * 1-5\"), `every 10m`, `at +2h`, `at 17:30`, \
                                    `at 2026-12-01 09:00`, `on ci-failure`, or `on file:<glob>`"
                },
                "prompt": {"type": "string", "description": "`add`: what a run is asked to do"},
                "session": {
                    "type": "string",
                    "enum": ["new", "this"],
                    "description": "`add`: a new background session for every run (the default for a time), or \
                                    this session"
                },
                "id": {"type": "string", "description": "`remove`: the id of the schedule"}
            },
            "required": ["action"]
        })
    }

    fn access(&self, input: &Value) -> Access {
        match text(input, "action") {
            "list" => Access::Read,
            _ => Access::Exec,
        }
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        match text(input, "action") {
            "add" => vec![text(input, "when").to_string()],
            "remove" => vec![text(input, "id").to_string()],
            _ => Vec::new(),
        }
    }

    fn approval_note(&self, input: &Value) -> Option<String> {
        (text(input, "action") == "add").then(|| {
            "a schedule runs its prompt later, by itself, with the permissions this session has now"
                .to_string()
        })
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let done = match text(&input, "action") {
            "list" => Ok(table(&self.scheduler.store().list(), &Zoned::now())),
            "add" => self.add(&input, ctx).await,
            "remove" => self.remove(&input).await,
            other => Err(format!("`action` is `add`, `list` or `remove`, not `{other}`")),
        };
        match done {
            Ok(said) => ToolOutput::text(said),
            Err(why) => ToolOutput::error(why),
        }
    }
}

impl Extension for ScheduleExt {
    fn id(&self) -> &str {
        "schedule"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        // Whoever has the session open watches the project's triggers with it.
        r.service(Arc::new(self.scheduler.clone()));
        r.tool(Arc::new(ScheduleTool {
            scheduler: self.scheduler.clone(),
            permissions: self.permissions.clone(),
            carried: self.carried.clone(),
            ephemeral: self.ephemeral,
        }));
        Ok(())
    }
}
