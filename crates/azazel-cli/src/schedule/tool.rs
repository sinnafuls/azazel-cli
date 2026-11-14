//! The `schedule` tool: an agent adds, lists, pauses, resumes and removes schedules of the
//! project. All but listing run things later with nobody watching, so they ask for permission
//! like a command, and the checks a schedule runs are asked about as `shell` commands.

use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::{ExtError, Extension, PermissionState, Registrar};
use azazel_team::Team;
use jiff::Zoned;
use serde_json::{Value, json};

use super::check::{Check, Ends};
use super::{Options, Scheduler, Target, now_in, table};

const DESCRIPTION: &str = "Run a prompt later, or again and again: at a time, on an interval, on a cron \
expression, when a CI run fails, or when files change. Each run starts a background session with the prompt \
(`session: \"this\"` sends it to this session instead). A run knows nothing of this conversation unless it \
goes to this session, so write the prompt to stand on its own. `if` runs a shell check at each due time and \
the prompt runs only when it holds; `until` ends the schedule once its check holds; checks run before any \
model is asked, so they cost nothing. `ends` stops it at a time or after a number of runs. `list` shows what \
is scheduled in this project, `pause`/`resume` stop and restart one, `remove` takes one out.";

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
        let options = options(input)?;
        let now = now_in(options.tz.as_deref())?;
        let when = super::when::When::parse(text(input, "when"), &now)?;
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
            let schedule = scheduler.add(when, &prompt, target, args, options)?;
            Ok::<String, String>(scheduler.added_line(&schedule))
        });
        added.await.map_err(|e| e.to_string())?
    }

    /// `remove`, `pause` or `resume`: what talks to the system's scheduler runs a program and
    /// waits for it.
    async fn change(&self, input: &Value, action: &'static str) -> Result<String, String> {
        let id = text(input, "id").to_string();
        if id.is_empty() {
            return Err(format!("`{action}` needs the `id` of the schedule"));
        }
        let scheduler = self.scheduler.clone();
        let changed = tokio::task::spawn_blocking(move || match action {
            "pause" => scheduler
                .pause(&id)
                .map(|schedule| format!("paused schedule {} ({})", schedule.id, schedule.when)),
            "resume" => scheduler.resume(&id).map(|schedule| {
                format!(
                    "resumed {}",
                    scheduler.added_line(&schedule).trim_start_matches("schedule ")
                )
            }),
            _ => scheduler
                .remove(&id)
                .map(|schedule| format!("removed schedule {} ({})", schedule.id, schedule.when)),
        });
        changed.await.map_err(|e| e.to_string())?
    }
}

/// The checks, the end and the zone of an `add`.
fn options(input: &Value) -> Result<Options, String> {
    let check = |name: &str| {
        input
            .get(name)
            .filter(|value| !value.is_null())
            .map(|value| Check::from_json(value, name))
            .transpose()
    };
    let tz = Some(text(input, "tz")).filter(|tz| !tz.is_empty());
    let ends = match input.get("ends").filter(|ends| !ends.is_null()) {
        None => None,
        Some(ends) => {
            let times = match ends.get("times").filter(|times| !times.is_null()) {
                None => None,
                Some(times) => Some(
                    times
                        .as_u64()
                        .and_then(|times| u32::try_from(times).ok())
                        .ok_or("`ends.times` is a number of runs")?,
                ),
            };
            Ends::parse(
                ends.get("after").and_then(Value::as_str),
                ends.get("at").and_then(Value::as_str),
                times,
                &now_in(tz)?,
            )?
        }
    };
    Ok(Options {
        if_check: check("if")?,
        until: check("until")?,
        ends,
        tz: tz.map(str::to_string),
    })
}

/// The commands of the checks an `add` gives.
fn check_commands(input: &Value) -> Vec<String> {
    ["if", "until"]
        .into_iter()
        .filter_map(|name| input.get(name)?.get("run")?.as_str())
        .map(|run| run.trim().to_string())
        .filter(|run| !run.is_empty())
        .collect()
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
        let check = |about: &str| {
            json!({
                "type": "object",
                "description": format!("{about}. It holds when `run` exits with `is` (0), what it prints matches `matches`, and, with `changes`, it printed something else than the time before (never on its first run)"),
                "properties": {
                    "run": {"type": "string", "description": "a shell command, run in the project"},
                    "is": {"type": "integer"},
                    "matches": {"type": "string", "description": "a regular expression; `^` and `$` are line ends"},
                    "changes": {"type": "boolean"}
                },
                "required": ["run"]
            })
        };
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "list", "pause", "resume", "remove"]},
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
                "if": check("`add`: run the prompt at a due time only when this check holds"),
                "until": check("`add`: at each due time, first run this check; once it holds the schedule ends"),
                "ends": {
                    "type": "object",
                    "description": "`add`: when the schedule stops by itself",
                    "properties": {
                        "after": {"type": "string", "description": "this long after it is added: `2h`, `7d`"},
                        "at": {"type": "string", "description": "at this time, as `at` takes it"},
                        "times": {"type": "integer", "minimum": 1, "description": "after this many runs"}
                    }
                },
                "tz": {
                    "type": "string",
                    "description": "`add`: the time zone a cron expression or `at` time is in (`Europe/Berlin`); the local one by default"
                },
                "id": {"type": "string", "description": "`pause`, `resume`, `remove`: the id of the schedule"}
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
            "remove" | "pause" | "resume" => vec![text(input, "id").to_string()],
            _ => Vec::new(),
        }
    }

    fn commands(&self, input: &Value) -> Vec<String> {
        match text(input, "action") {
            "add" => check_commands(input),
            _ => Vec::new(),
        }
    }

    fn approval_note(&self, input: &Value) -> Option<String> {
        (text(input, "action") == "add").then(|| {
            let note =
                "a schedule runs its prompt later, by itself, with the permissions this session has now";
            match check_commands(input).as_slice() {
                [] => note.to_string(),
                commands => format!(
                    "{note}; at each due time it runs, without asking: {}",
                    commands
                        .iter()
                        .map(|command| format!("`{command}`"))
                        .collect::<Vec<String>>()
                        .join(", ")
                ),
            }
        })
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let done = match text(&input, "action") {
            "list" => Ok(table(&self.scheduler.store().list(), &Zoned::now())),
            "add" => self.add(&input, ctx).await,
            "remove" => self.change(&input, "remove").await,
            "pause" => self.change(&input, "pause").await,
            "resume" => self.change(&input, "resume").await,
            other => Err(format!(
                "`action` is `add`, `list`, `pause`, `resume` or `remove`, not `{other}`"
            )),
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
