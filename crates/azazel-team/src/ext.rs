//! What the team adds to a session: its tools, the `agent://` scheme, the prompt parts that tell
//! agents who they are and whom they can delegate to, and the `/agents`, `/msg`, `/stop`, `/task`,
//! `/review`, `/security-review` and `/playtest` commands.

use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{AgentInfo, Services, ToolCtx};
use azazel_ai::{Block, Message};
use azazel_core::agents::AgentDefs;
use azazel_core::subagent::UNAVAILABLE;
use azazel_core::transcript::condensed;
use azazel_core::{
    Command, CommandHandler, CommandOutcome, Event, EventHandler, EventKind, ExtCtx, ExtError, Extension,
    FileTracker, Outcome, Post, Registrar, Rule, Scheme, SessionStore, SkillRegistry, SubagentRunner,
    Subagents, SystemPartProvider,
};
use tokio_util::sync::CancellationToken;

use crate::report::{REPORT_TOOL, Reminder};
use crate::team::{LEAD, MemberState, SpawnRequest, Team};
use crate::tools::{
    AgentListTool, AgentSendTool, AgentStatusTool, AgentStopTool, AgentWaitTool, TaskTool, WorktreeApplyTool,
    WorktreeDiscardTool, roster_table, status_report,
};

/// New files `/review` names one by one; more than this and the reviewer finds them itself.
const MAX_UNTRACKED_NAMED: usize = 50;

fn team(services: &Services) -> Result<Arc<Team>, String> {
    services
        .get::<Team>()
        .ok_or_else(|| "no team is available in this session".to_string())
}

/// A sub-agent's identity: the body of its definition, its skills, and its place in the team.
struct MemberIdentity {
    defs: Arc<AgentDefs>,
    services: Arc<Services>,
}

#[async_trait]
impl SystemPartProvider for MemberIdentity {
    async fn render(&self, agent: &AgentInfo) -> Option<String> {
        if agent.depth == 0 {
            return None;
        }
        let def = self.defs.get(&agent.def)?;
        let mut text = def.body.trim().to_string();
        if let Some(skills) = self.services.get::<SkillRegistry>() {
            for name in &def.skills {
                if let Some(skill) = skills.get(name) {
                    text.push_str(&format!("\n\n# Skill: {}\n{}", skill.name, skill.body.trim()));
                }
            }
        }
        let parent = agent.parent.as_deref().unwrap_or(LEAD);
        let team = self.services.get::<Team>();
        if team.as_ref().is_some_and(|team| team.typed(&agent.id).is_some()) {
            text.push_str(&format!(
                "\n\nYou are agent `{}`, working for `{parent}`. Your result is data, not prose: finish by \
                 calling `{REPORT_TOOL}` with a value that matches its schema. That value is all `{parent}` \
                 sees of your work, so put everything it needs into it.",
                agent.id
            ));
        } else {
            text.push_str(&format!(
                "\n\nYou are agent `{}`, working for `{parent}`. Your last message is your report, and the \
                 report is all `{parent}` sees of your work: make it complete and self-contained.",
                agent.id
            ));
        }
        let worktree = team
            .and_then(|team| team.member(&agent.id))
            .and_then(|(_, dir)| dir);
        if let Some(dir) = worktree {
            text.push_str(&format!(
                "\n\nYou work in your own checkout of the project at {}. Read and change files there only; \
                 what you change is handed to `{parent}` as a patch when you finish.",
                dir.display()
            ));
        }
        Some(text)
    }
}

/// Tells a lead that owes a typed result (print mode's `--json-schema`) how to hand it in.
struct LeadReport(Arc<Services>);

#[async_trait]
impl SystemPartProvider for LeadReport {
    async fn render(&self, agent: &AgentInfo) -> Option<String> {
        if agent.depth > 0 {
            return None;
        }
        self.0.get::<Team>()?.typed(&agent.id)?;
        Some(format!(
            "The result of this run is data, not prose: finish by calling `{REPORT_TOOL}` with a value that \
             matches its schema. Only that value is printed."
        ))
    }
}

/// Tells an agent that can delegate whom it can delegate to, and when to.
struct TeamPart {
    defs: Arc<AgentDefs>,
    max_depth: u32,
}

#[async_trait]
impl SystemPartProvider for TeamPart {
    async fn render(&self, agent: &AgentInfo) -> Option<String> {
        let all = self.defs.subagents();
        let available: Vec<_> = if agent.depth == 0 {
            all
        } else {
            let own = self.defs.get(&agent.def)?;
            if agent.depth >= self.max_depth {
                return None;
            }
            all.into_iter()
                .filter(|def| own.spawns.contains(&def.name))
                .collect()
        };
        if available.is_empty() {
            return None;
        }
        let mut text = String::from(
            "# Team\nThe `task` tool hands work to specialist agents. Each starts with a fresh context and sees \
             only the prompt you write, so give it the goal, the paths that matter, the constraints and what to \
             report.\n\
             - Sub-tasks that do not depend on each other: list them in one `task` call; they run in parallel.\n\
             - Finding your way in unfamiliar code: `scout`. A second pair of eyes on a change: `reviewer`.\n\
             - Small or strictly sequential work: do it yourself.\n\
             Agents you can start:\n",
        );
        for def in available {
            text.push_str(&format!("- {}: {}\n", def.name, def.description));
        }
        Some(text)
    }
}

/// `agent://<id>` is an agent's latest report (for a typed result, the JSON value);
/// `agent://<id>/transcript` what it did, condensed.
struct AgentScheme;

#[async_trait]
impl Scheme for AgentScheme {
    fn scheme(&self) -> &str {
        "agent"
    }

    async fn read(&self, rest: &str, ctx: &ToolCtx) -> Result<Vec<Block>, String> {
        let team = team(&ctx.services)?;
        let (id, part) = rest.split_once('/').unwrap_or((rest, ""));
        // The status needs no transcript: it is there for the lead and for `all` too.
        if part == "status" {
            return Ok(vec![Block::text(status_report(&team, id).await?)]);
        }
        let path = team
            .transcript_path(id)
            .ok_or_else(|| format!("no agent named `{id}`"))?;
        match part {
            "" => match team.result(id) {
                // A typed result is read as the value alone, so it parses.
                Some(result) => Ok(vec![Block::text(result.json.unwrap_or(result.report))]),
                None => Err(format!(
                    "agent `{id}` has not reported yet; agent://{id}/transcript shows its progress"
                )),
            },
            "transcript" => {
                let store = SessionStore::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                Ok(vec![Block::text(condensed(&store))])
            }
            other => Err(format!(
                "agent://{id}/{other}: expected agent://{id}, agent://{id}/transcript or agent://{id}/status"
            )),
        }
    }
}

/// Runs a sub-agent for a lower layer (a skill that runs in its own context) and returns its report.
struct Runner(Arc<Services>);

#[async_trait]
impl SubagentRunner for Runner {
    async fn run(&self, def: &str, prompt: String, parent: &AgentInfo) -> Result<String, String> {
        self.start(def, prompt, parent, None).await
    }

    async fn run_limited(
        &self,
        def: &str,
        prompt: String,
        parent: &AgentInfo,
        tools: Vec<Rule>,
    ) -> Result<String, String> {
        self.start(def, prompt, parent, Some(tools)).await
    }
}

impl Runner {
    async fn start(
        &self,
        def: &str,
        prompt: String,
        parent: &AgentInfo,
        tools: Option<Vec<Rule>>,
    ) -> Result<String, String> {
        let team = team(&self.0).map_err(|_| UNAVAILABLE.to_string())?;
        if !team.may_spawn(parent, def) {
            return Err(UNAVAILABLE.to_string());
        }
        let request = SpawnRequest {
            def: def.to_string(),
            prompt,
            name: None,
            model: None,
            isolation: None,
            tools,
            schema: None,
        };
        let id = team.spawn(parent, request, false).await?;
        let results = team
            .wait_all(std::slice::from_ref(&id), &CancellationToken::new())
            .await;
        match results.and_then(|mut results| results.pop()) {
            Some(result) if matches!(result.state, MemberState::Failed(_)) => Err(result.report),
            Some(result) => Ok(result.report),
            None => Err(format!("agent `{id}` was stopped")),
        }
    }
}

struct AgentsCommand(Arc<AgentDefs>);

#[async_trait]
impl CommandHandler for AgentsCommand {
    async fn run(&self, _: &str, ctx: &ExtCtx) -> CommandOutcome {
        let mut out = String::from("Agents:\n");
        match team(&ctx.services) {
            Ok(team) => out.push_str(&roster_table(&team.list())),
            Err(why) => out.push_str(&format!("{why}\n")),
        }
        out.push_str("\nDefinitions:\n");
        for def in self.0.list() {
            out.push_str(&format!(
                "{} [{}]: {}\n",
                def.name,
                def.source.label(),
                def.description
            ));
        }
        for invalid in self.0.invalid() {
            out.push_str(&format!(
                "invalid: {}: {}\n",
                invalid.path.display(),
                invalid.error
            ));
        }
        CommandOutcome::Show(out)
    }
}

struct MsgCommand;

#[async_trait]
impl CommandHandler for MsgCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        let Some((to, message)) = args.trim().split_once(char::is_whitespace) else {
            return CommandOutcome::Error("usage: /msg <agent id|all> <message>".to_string());
        };
        match team(&ctx.services).and_then(|team| team.send_from_user(to, message.trim())) {
            Ok(reached) => CommandOutcome::Show(format!("sent to: {}", reached.join(", "))),
            Err(why) => CommandOutcome::Error(why),
        }
    }
}

struct StopCommand;

#[async_trait]
impl CommandHandler for StopCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        let id = args.trim();
        if id.is_empty() {
            return CommandOutcome::Error("usage: /stop <agent id|all>".to_string());
        }
        let stopped = team(&ctx.services).and_then(|team| {
            if id == "all" {
                Ok(team.stop_all())
            } else {
                team.stop(id)
            }
        });
        match stopped {
            Ok(stopped) if stopped.is_empty() => CommandOutcome::Show("no agent is running".to_string()),
            Ok(stopped) => CommandOutcome::Show(format!("stopped: {}", stopped.join(", "))),
            Err(why) => CommandOutcome::Error(why),
        }
    }
}

/// The task `/playtest <launch command> [-- <what to test>]` gives the playtester, working in `cwd`.
/// Launch commands contain ` -- ` themselves (`cargo run -- --windowed`), so what to test is what
/// follows the last one, unless that reads like an argument.
fn playtest_task(args: &str, cwd: &std::path::Path) -> Result<String, String> {
    let (launch, focus) = match args.rsplit_once(" -- ") {
        Some((launch, focus)) if !focus.trim_start().starts_with('-') => (launch.trim(), focus.trim()),
        _ => (args.trim(), ""),
    };
    if launch.is_empty() {
        return Err("usage: /playtest <launch command> [-- <what to test>]".to_string());
    }
    let focus = if focus.is_empty() {
        "the main flows a new player or user would try first"
    } else {
        focus
    };
    Ok(format!(
        "Playtest this build.\n\nLaunch command: `{launch}`\nWorking directory: {}\nWhat to test: {focus}\n\n\
         Start the build as a background `shell` job, wait for its window, then loop: capture, act, \
         observe. Report the steps you took, each defect with the path of the frame that shows it, and \
         any crash output from `job`.",
        cwd.display()
    ))
}

struct PlaytestCommand;

#[async_trait]
impl CommandHandler for PlaytestCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        let prompt = match playtest_task(args, &ctx.cwd) {
            Ok(prompt) => prompt,
            Err(usage) => return CommandOutcome::Error(usage),
        };
        let team = match team(&ctx.services) {
            Ok(team) => team,
            Err(why) => return CommandOutcome::Error(why),
        };
        let Some(lead) = team.lead() else {
            return CommandOutcome::Error("there is no session to report to".to_string());
        };
        let request = SpawnRequest {
            def: "playtester".to_string(),
            prompt,
            name: None,
            model: None,
            isolation: None,
            tools: None,
            schema: None,
        };
        match team.spawn(lead.info(), request, true).await {
            Ok(id) => CommandOutcome::Show(format!(
                "started {id}; its report goes to the lead when it is done"
            )),
            Err(why) => CommandOutcome::Error(why),
        }
    }
}

/// A commit, branch or tag as `/review` accepts it: one word that git cannot take for an option.
fn usable_ref(reference: &str) -> bool {
    !reference.is_empty()
        && !reference.starts_with('-')
        && reference
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/-~^@{}".contains(c))
}

/// The task `/review [ref]` gives the reviewer: which diff to read, what to report, and that it
/// changes nothing. `untracked` are new files the diff does not show; `security` is the body of the
/// `security-review` skill for `/security-review`.
fn review_task(reference: Option<&str>, untracked: &[String], security: Option<&str>) -> String {
    let what = if security.is_some() {
        "Review for security defects"
    } else {
        "Review"
    };
    let mut task = match reference {
        Some(reference) => format!(
            "{what} the commits on this branch since `{reference}`.\n\n\
             Run `git diff {reference}...HEAD` to read the change; `git log --oneline {reference}..HEAD` \
             lists its commits."
        ),
        None => format!(
            "{what} the uncommitted changes in this working tree.\n\n\
             Run `git diff HEAD` to read the change to tracked files, staged or not."
        ),
    };
    if !untracked.is_empty() {
        task.push_str("\nThese files are new and not in that diff; read each in full:\n");
        for file in untracked {
            task.push_str(&format!("- {file}\n"));
        }
    }
    task.push_str(
        "\n\nReport findings only, the most severe first. Give each one its `file:line`, a severity \
         (`blocker`, `should-fix` or `nit`), what is wrong and why it matters, and a concrete fix. \
         Do not edit, create or delete any file, and run nothing but `git diff`, `git log` and \
         `git show`.",
    );
    if let Some(skill) = security {
        task.push_str(&format!(
            "\n\nLimit the review to security: leave out style and maintainability. Work by this \
             skill.\n\n# Skill: security-review\n{}",
            skill.trim()
        ));
    }
    task
}

/// Runs git in `cwd`; its output when it succeeded.
async fn git(cwd: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Starts `request` for the lead on the user's behalf; the report goes to the lead.
async fn start_for_user(ctx: &ExtCtx, request: SpawnRequest) -> CommandOutcome {
    let team = match team(&ctx.services) {
        Ok(team) => team,
        Err(why) => return CommandOutcome::Error(why),
    };
    let Some(lead) = team.lead() else {
        return CommandOutcome::Error("there is no session to report to".to_string());
    };
    match team.spawn(lead.info(), request, true).await {
        Ok(id) => CommandOutcome::Show(format!(
            "started {id}; its report goes to the lead when it is done"
        )),
        Err(why) => CommandOutcome::Error(why),
    }
}

/// `/review [ref]` and `/security-review [ref]`: a reviewer reads the uncommitted changes, or the
/// commits since `ref`, and reports to the lead.
struct ReviewCommand {
    security: bool,
}

#[async_trait]
impl CommandHandler for ReviewCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        let name = if self.security {
            "security-review"
        } else {
            "review"
        };
        let reference = args.trim();
        if !reference.is_empty() && !usable_ref(reference) {
            return CommandOutcome::Error(format!(
                "usage: /{name} [ref], where ref is a commit, branch or tag"
            ));
        }
        if git(&ctx.cwd, &["rev-parse", "--is-inside-work-tree"])
            .await
            .is_none()
        {
            return CommandOutcome::Error(format!("/{name} needs a git repository"));
        }
        let mut untracked = Vec::new();
        let reference = if reference.is_empty() {
            let status = git(&ctx.cwd, &["status", "--porcelain"])
                .await
                .unwrap_or_default();
            if status.trim().is_empty() {
                return CommandOutcome::Show(format!(
                    "there are no uncommitted changes to review; /{name} <ref> reviews the commits since a ref"
                ));
            }
            untracked = git(&ctx.cwd, &["ls-files", "--others", "--exclude-standard"])
                .await
                .unwrap_or_default()
                .lines()
                .take(MAX_UNTRACKED_NAMED)
                .map(str::to_string)
                .collect();
            None
        } else {
            let commit = format!("{reference}^{{commit}}");
            if git(&ctx.cwd, &["rev-parse", "--verify", "--quiet", &commit])
                .await
                .is_none()
            {
                return CommandOutcome::Error(format!(
                    "`{reference}` is not a commit, branch or tag of this repository"
                ));
            }
            Some(reference)
        };
        let skill = self
            .security
            .then(|| ctx.services.get::<SkillRegistry>())
            .flatten()
            .and_then(|skills| skills.get("security-review"));
        if self.security && skill.is_none() {
            return CommandOutcome::Error(
                "the security-review skill is not loaded in this session".to_string(),
            );
        }
        let request = SpawnRequest {
            def: "reviewer".to_string(),
            prompt: review_task(
                reference,
                &untracked,
                skill.as_ref().map(|skill| skill.body.as_str()),
            ),
            name: None,
            model: None,
            isolation: None,
            tools: None,
            schema: None,
        };
        start_for_user(ctx, request).await
    }
}

/// `/task <agent> [name=<id>] <prompt>` as a request; `Err` is the usage.
fn task_request(args: &str) -> Result<SpawnRequest, String> {
    let usage = || "usage: /task <agent> [name=<id>] <prompt>; /task alone lists the agents".to_string();
    let (def, rest) = args.trim().split_once(char::is_whitespace).ok_or_else(usage)?;
    let rest = rest.trim_start();
    let (name, prompt) = match rest.strip_prefix("name=") {
        Some(named) => {
            let (name, prompt) = named.split_once(char::is_whitespace).ok_or_else(usage)?;
            (Some(name.to_string()), prompt.trim())
        }
        None => (None, rest.trim()),
    };
    if prompt.is_empty() || name.as_deref() == Some("") {
        return Err(usage());
    }
    Ok(SpawnRequest {
        def: def.to_string(),
        prompt: prompt.to_string(),
        name,
        model: None,
        isolation: None,
        tools: None,
        schema: None,
    })
}

/// `/task`: the user starts any defined agent; its report goes to the lead.
struct TaskCommand(Arc<AgentDefs>);

#[async_trait]
impl CommandHandler for TaskCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        if args.trim().is_empty() {
            let mut out = String::from("Agents you can start with /task <agent> [name=<id>] <prompt>:\n");
            for def in self.0.list() {
                out.push_str(&format!("{}: {}\n", def.name, def.description));
            }
            return CommandOutcome::Show(out);
        }
        match task_request(args) {
            Ok(request) => start_for_user(ctx, request).await,
            Err(usage) => CommandOutcome::Error(usage),
        }
    }
}

/// Tells an agent, before its next request, which files it read were changed by another agent.
struct StaleFiles;

#[async_trait]
impl EventHandler for StaleFiles {
    async fn handle(&self, event: &mut Event, ctx: &ExtCtx) -> Outcome {
        if let Event::Context { agent, messages } = event
            && let Some(tracker) = ctx.services.get::<FileTracker>()
        {
            let notes: Vec<String> = tracker
                .take_stale(&agent.id)
                .into_iter()
                .map(|(path, by)| {
                    format!(
                        "{} was modified by {by}; read it again before editing",
                        path.display()
                    )
                })
                .collect();
            if !notes.is_empty() {
                messages.push(Message::user(notes.join("\n")));
            }
        }
        Outcome::Continue
    }
}

/// Shows in the roster that an agent is held up by a permission prompt.
struct ApprovalWatch;

#[async_trait]
impl EventHandler for ApprovalWatch {
    async fn handle(&self, event: &mut Event, ctx: &ExtCtx) -> Outcome {
        if let Event::PermissionRequest { agent, .. } = event
            && let Ok(team) = team(&ctx.services)
        {
            team.waiting_for_approval(&agent.id);
        }
        Outcome::Continue
    }
}

/// Registers the team's tools, prompt parts, scheme, commands and event handlers. The [`Team`]
/// itself is added to the services once the model router exists.
pub struct TeamExt {
    pub defs: Arc<AgentDefs>,
    /// `team.max_depth`: how many levels of agents may exist below the lead.
    pub max_depth: u32,
}

impl Extension for TeamExt {
    fn id(&self) -> &str {
        "team"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        let services = r.services().clone();
        r.tool(Arc::new(TaskTool));
        r.tool(Arc::new(AgentSendTool));
        r.tool(Arc::new(AgentStopTool));
        r.tool(Arc::new(AgentWaitTool));
        r.tool(Arc::new(AgentListTool));
        r.tool(Arc::new(AgentStatusTool));
        r.tool(Arc::new(WorktreeApplyTool));
        r.tool(Arc::new(WorktreeDiscardTool));
        r.scheme(Arc::new(AgentScheme));
        r.system_part(
            "identity",
            Arc::new(MemberIdentity {
                defs: self.defs.clone(),
                services: services.clone(),
            }),
        );
        r.system_part("identity", Arc::new(LeadReport(services.clone())));
        r.system_part(
            "team",
            Arc::new(TeamPart {
                defs: self.defs.clone(),
                max_depth: self.max_depth,
            }),
        );
        // What is sent to an agent from outside its turn (the answers to a question it asked
        // without waiting) finds the agent through the team.
        let agents = Arc::downgrade(&services);
        r.service(Arc::new(Post(Arc::new(move |id: &str, text: String| {
            agents
                .upgrade()
                .and_then(|services| services.get::<Team>())
                .is_some_and(|team| team.post(id, text))
        }))));
        r.service(Arc::new(Subagents(Arc::new(Runner(services)))));
        r.command(Command {
            name: "agents".to_string(),
            description: "The agents of this session and the definitions they can be started from"
                .to_string(),
            argument_hint: None,
            handler: Arc::new(AgentsCommand(self.defs.clone())),
        });
        r.command(Command {
            name: "msg".to_string(),
            description: "Send a message to an agent".to_string(),
            argument_hint: Some("<agent id|all> <message>".to_string()),
            handler: Arc::new(MsgCommand),
        });
        r.command(Command {
            name: "stop".to_string(),
            description: "Stop an agent and the agents it started".to_string(),
            argument_hint: Some("<agent id|all>".to_string()),
            handler: Arc::new(StopCommand),
        });
        r.command(Command {
            name: "playtest".to_string(),
            description: "Have a playtester run a build, play it and report defects with frames".to_string(),
            argument_hint: Some("<launch command> [-- <what to test>]".to_string()),
            handler: Arc::new(PlaytestCommand),
        });
        r.command(Command {
            name: "review".to_string(),
            description: "Have a reviewer read the uncommitted changes, or the commits since a ref"
                .to_string(),
            argument_hint: Some("[ref]".to_string()),
            handler: Arc::new(ReviewCommand { security: false }),
        });
        r.command(Command {
            name: "security-review".to_string(),
            description: "Have a reviewer look for security defects in the same changes".to_string(),
            argument_hint: Some("[ref]".to_string()),
            handler: Arc::new(ReviewCommand { security: true }),
        });
        r.command(Command {
            name: "task".to_string(),
            description: "Start an agent yourself; its report goes to the lead".to_string(),
            argument_hint: Some("<agent> [name=<id>] <prompt>".to_string()),
            handler: Arc::new(TaskCommand(self.defs.clone())),
        });
        r.on(EventKind::Stop, Arc::new(Reminder));
        r.on(EventKind::Context, Arc::new(StaleFiles));
        r.on(EventKind::PermissionRequest, Arc::new(ApprovalWatch));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_playtest_command_splits_the_launch_command_from_what_to_test() {
        let cwd = std::path::Path::new("game");
        let both = playtest_task(
            "cargo run --release -- --windowed -- jumping and the pause menu",
            cwd,
        );
        let both = both.as_deref().unwrap_or_default();
        assert!(
            both.contains("Launch command: `cargo run --release -- --windowed`\n"),
            "{both}"
        );
        assert!(
            both.contains("What to test: jumping and the pause menu\n"),
            "{both}"
        );
        // Arguments after ` -- ` are part of the command, not a description.
        let arguments = playtest_task("cargo run -- --level 3", cwd);
        let arguments = arguments.as_deref().unwrap_or_default();
        assert!(
            arguments.contains("Launch command: `cargo run -- --level 3`\n"),
            "{arguments}"
        );

        let launch_only = playtest_task("  ./build/game.exe  ", cwd);
        let launch_only = launch_only.as_deref().unwrap_or_default();
        assert!(launch_only.contains("Launch command: `./build/game.exe`\nWorking directory: game\n"));
        assert!(launch_only.contains("What to test: the main flows"));

        for empty in ["", "   ", " -- only a focus"] {
            assert!(playtest_task(empty, cwd).is_err_and(|usage| usage.starts_with("usage: /playtest")));
        }
    }

    #[test]
    fn the_review_task_names_the_diff_to_read_and_forbids_edits() {
        let uncommitted = review_task(None, &["src/new.rs".to_string()], None);
        assert!(uncommitted.starts_with("Review the uncommitted changes"));
        assert!(uncommitted.contains("Run `git diff HEAD`"));
        assert!(uncommitted.contains("- src/new.rs\n"));
        assert!(uncommitted.contains("`file:line`") && uncommitted.contains("Do not edit"));

        let since = review_task(Some("main"), &[], None);
        assert!(since.contains("Run `git diff main...HEAD`"));
        assert!(since.contains("`git log --oneline main..HEAD`"));
        assert!(!since.contains("not in that diff"));

        let security = review_task(None, &[], Some("Find injection.\n"));
        assert!(security.starts_with("Review for security defects the uncommitted changes"));
        assert!(security.ends_with("# Skill: security-review\nFind injection."));

        for good in [
            "main",
            "origin/main",
            "v1.2.0",
            "HEAD~3",
            "a1b2c3d",
            "@{upstream}",
        ] {
            assert!(usable_ref(good), "{good}");
        }
        for bad in ["", "--output=x", "main; rm -rf .", "a b", "$(id)", "`id`"] {
            assert!(!usable_ref(bad), "{bad}");
        }
    }

    #[test]
    fn a_task_line_is_an_agent_an_optional_name_and_a_prompt() {
        let plain = task_request("scout  find the parser entry points ").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            (plain.def.as_str(), plain.name.as_deref(), plain.prompt.as_str()),
            ("scout", None, "find the parser entry points")
        );
        let named = task_request("coder name=fixer fix name=value parsing").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            (named.def.as_str(), named.name.as_deref(), named.prompt.as_str()),
            ("coder", Some("fixer"), "fix name=value parsing")
        );
        for incomplete in ["scout", "scout   ", "scout name=x", "scout name= do it"] {
            assert!(
                task_request(incomplete).is_err_and(|usage| usage.starts_with("usage: /task")),
                "{incomplete}"
            );
        }
    }
}
