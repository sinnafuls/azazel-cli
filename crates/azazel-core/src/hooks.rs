//! User-configured hooks: commands, HTTP endpoints, model prompts and agents that run on session
//! events and may block them, rewrite tool input, settle permission requests or add context.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{AgentInfo, Services, ToolOutput};
use azazel_ai::{Block, Message, ModelSpec, Request, Router, StreamEvent, SystemPart};
use regex::Regex;
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{HookConfig, HookKind, PermissionMode};
use crate::ext::{
    Event, EventHandler, EventKind, ExtCtx, ExtError, Extension, Level, Outcome, Registrar, StartSource,
};
use crate::paths::Paths;
use crate::permissions::shell::analyze;
use crate::permissions::{MatchCtx, PermissionState, Rule};
use crate::process::{self, AgentEnv, Shell};
use crate::session::SessionStore;
use crate::skills::{Skill, SkillRegistry, SkillSource};
use crate::subagent::{Subagents, UNAVAILABLE};
use crate::trust::project_skill_roots;
use crate::ui::Ui;

const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// What is kept of each output stream of a command hook.
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
/// How long output is read after the hook's shell has exited; a process it left behind could
/// otherwise keep the pipes open forever.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(300);
/// Tool inputs remembered between a call and its result. Calls that are denied after the hooks ran
/// never produce a result, so the oldest are dropped beyond this.
const MAX_PENDING_CALLS: usize = 256;
const LEAD: &str = "lead";
/// The agent definition `agent` hooks run.
const VERIFIER: &str = "verifier";
const VERDICT_FORMAT: &str = "one JSON object and nothing after it: {\"ok\": true} when the event may proceed, \
                              or {\"ok\": false, \"reason\": \"<why not>\"} when it must not";
/// Tool names as skill hooks know them.
const TOOL_ALIASES: [(&str, &str); 9] = [
    ("shell", "Bash"),
    ("read", "Read"),
    ("write", "Write"),
    ("edit", "Edit"),
    ("glob", "Glob"),
    ("grep", "Grep"),
    ("web_fetch", "WebFetch"),
    ("web_search", "WebSearch"),
    ("task", "Task"),
];
const KINDS: [EventKind; 11] = [
    EventKind::SessionStart,
    EventKind::SessionEnd,
    EventKind::UserPrompt,
    EventKind::ToolCall,
    EventKind::ToolResult,
    EventKind::AgentSpawn,
    EventKind::PreCompact,
    EventKind::PostCompact,
    EventKind::Stop,
    EventKind::PermissionRequest,
    EventKind::Notification,
];

/// Runs the `[[hooks.<event>]]` entries of the config, and the `hooks` a skill carries once that
/// skill has been used in the session (until the session ends).
///
/// Hooks of one event run in config order, then skill hooks in the order their skills were first
/// used. The first hook to block ends the chain; tool-input rewrites accumulate, and each hook sees
/// the input as the hooks before it left it. On events where blocking has no effect every hook
/// runs.
///
/// `matcher` must match the whole tool name (`shell`, `write|edit`, `mcp__github__.*`); an empty
/// matcher or `*` matches every tool. `if` is checked against the simple commands of a `shell`
/// call, the `path` of `read`/`write`/`edit` and `domain:<host>` of `web_fetch`. Both are ignored
/// on events that have no tool.
///
/// A hook that fails (any exit code other than 0 and 2, a timeout, a non-2xx reply, an unreadable
/// answer) never blocks: the user gets a warning and the event proceeds.
///
/// `hookSpecificOutput.additionalContext` is honoured where context has somewhere to go: the user
/// prompt (`user_prompt_submit`, `session_start`) and the tool result (`post_tool_use`,
/// `post_tool_use_failure`).
pub struct HooksExt {
    /// Config event name to hooks, in config order.
    pub hooks: BTreeMap<String, Vec<HookConfig>>,
    pub shell: Shell,
    pub paths: Paths,
    /// Also set `CLAUDE_PROJECT_DIR` for command hooks.
    pub claude_compat: bool,
    /// True when `AZAZEL_HOOKS_DISABLED` was set at startup: nothing is registered (the recursion guard).
    pub disabled: bool,
    /// Whether the project is trusted. Hooks of the project's own skills run only when it is.
    pub trusted: bool,
}

impl Extension for HooksExt {
    fn id(&self) -> &str {
        "hooks"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        if self.disabled {
            return Ok(());
        }
        let mut config = Vec::new();
        for (name, entries) in &self.hooks {
            let Some(event) = HookEvent::from_config(name) else {
                tracing::warn!("hooks for unknown event `{name}` are ignored");
                continue;
            };
            for (index, entry) in entries.iter().enumerate() {
                match compile(event, entry) {
                    Ok(hook) => config.push(Arc::new(hook)),
                    Err(problem) => tracing::warn!("{name} hook #{} is ignored: {problem}", index + 1),
                }
            }
        }
        let inner = Arc::new(Inner {
            config,
            skills: RwLock::default(),
            session_context: Mutex::default(),
            calls: Mutex::default(),
            agent_hooks: Mutex::default(),
            shell: self.shell.clone(),
            paths: self.paths.clone(),
            claude_compat: self.claude_compat,
            trusted: self.trusted,
            http: LazyLock::new(reqwest::Client::new),
        });
        if let Some(skills) = r.services().get::<SkillRegistry>() {
            let hooks = Arc::downgrade(&inner);
            skills.on_first_use(Arc::new(move |skill: &Skill| {
                if let Some(hooks) = hooks.upgrade() {
                    hooks.add_skill(skill);
                }
            }));
        }
        let handler: Arc<dyn EventHandler> = Arc::new(Dispatcher(inner));
        for kind in KINDS {
            r.on(kind, handler.clone());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    UserPromptSubmit,
    SessionStart,
    SessionEnd,
    Stop,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    PermissionRequest,
    Notification,
}

impl HookEvent {
    const ALL: [HookEvent; 13] = [
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::PostToolUseFailure,
        HookEvent::UserPromptSubmit,
        HookEvent::SessionStart,
        HookEvent::SessionEnd,
        HookEvent::Stop,
        HookEvent::SubagentStart,
        HookEvent::SubagentStop,
        HookEvent::PreCompact,
        HookEvent::PostCompact,
        HookEvent::PermissionRequest,
        HookEvent::Notification,
    ];

    /// The name in `[[hooks.<event>]]`.
    fn config_name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "pre_tool_use",
            HookEvent::PostToolUse => "post_tool_use",
            HookEvent::PostToolUseFailure => "post_tool_use_failure",
            HookEvent::UserPromptSubmit => "user_prompt_submit",
            HookEvent::SessionStart => "session_start",
            HookEvent::SessionEnd => "session_end",
            HookEvent::Stop => "stop",
            HookEvent::SubagentStart => "subagent_start",
            HookEvent::SubagentStop => "subagent_stop",
            HookEvent::PreCompact => "pre_compact",
            HookEvent::PostCompact => "post_compact",
            HookEvent::PermissionRequest => "permission_request",
            HookEvent::Notification => "notification",
        }
    }

    /// The `hook_event_name` a hook receives, and the key in a skill's `hooks`.
    fn wire_name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::Stop => "Stop",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
            HookEvent::PermissionRequest => "PermissionRequest",
            HookEvent::Notification => "Notification",
        }
    }

    fn from_config(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| event.config_name() == name)
    }

    fn from_wire(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| event.wire_name() == name)
    }
}

enum Action {
    Command(String),
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
    Prompt {
        prompt: String,
        chain: Vec<ModelSpec>,
    },
    Agent {
        prompt: String,
    },
}

/// The skill a hook came with.
struct SkillOrigin {
    name: String,
    dir: PathBuf,
    /// Root of the package the skill was installed with.
    plugin_root: Option<PathBuf>,
}

struct Hook {
    event: HookEvent,
    /// Matches the whole tool name.
    matcher: Option<Regex>,
    when: Option<Rule>,
    action: Action,
    timeout: Duration,
    detached: bool,
    skill: Option<Arc<SkillOrigin>>,
}

impl Hook {
    /// The tool name this hook matches against and is told: skill hooks know the built-in tools by
    /// their aliases.
    fn shown_tool<'a>(&self, tool: &'a str) -> &'a str {
        if self.skill.is_none() {
            return tool;
        }
        TOOL_ALIASES
            .iter()
            .find(|(name, _)| *name == tool)
            .map_or(tool, |(_, alias)| *alias)
    }

    fn selects(&self, tool: &str, input: &Value, scope: &MatchCtx<'_>, posix: bool) -> bool {
        if let Some(matcher) = &self.matcher
            && !matcher.is_match(self.shown_tool(tool))
        {
            return false;
        }
        let Some(rule) = &self.when else {
            return true;
        };
        rule.matches_tool(tool)
            && (rule.pattern.is_none()
                || subjects(tool, input, posix)
                    .iter()
                    .any(|subject| rule.matches_subject(tool, subject, scope)))
    }

    /// How warnings name this hook.
    fn label(&self) -> String {
        let what = match &self.action {
            Action::Command(command) => clip(command, 80),
            Action::Http { url, .. } => clip(url, 80),
            Action::Prompt { .. } => "prompt".to_string(),
            Action::Agent { .. } => "agent".to_string(),
        };
        match &self.skill {
            Some(skill) => format!(
                "{} hook `{what}` of skill {}",
                self.event.config_name(),
                skill.name
            ),
            None => format!("{} hook `{what}`", self.event.config_name()),
        }
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// `None` matches every tool.
fn tool_matcher(pattern: Option<&str>) -> Result<Option<Regex>, String> {
    match pattern.map(str::trim) {
        None | Some("" | "*") => Ok(None),
        Some(pattern) => Regex::new(&format!("^(?:{pattern})$"))
            .map(Some)
            .map_err(|e| format!("invalid matcher `{pattern}`: {e}")),
    }
}

fn compile(event: HookEvent, config: &HookConfig) -> Result<Hook, String> {
    let required = |value: &Option<String>, key: &str| {
        value
            .clone()
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| format!("`{key}` is missing"))
    };
    let action = match config.kind {
        HookKind::Command => Action::Command(required(&config.command, "command")?),
        HookKind::Http => Action::Http {
            url: required(&config.url, "url")?,
            headers: config.headers.clone(),
        },
        HookKind::Prompt => Action::Prompt {
            prompt: required(&config.prompt, "prompt")?,
            chain: match &config.model {
                Some(model) => vec![
                    model
                        .parse::<ModelSpec>()
                        .map_err(|e| format!("invalid model `{model}`: {e}"))?,
                ],
                None => vec![ModelSpec::Role("smol".to_string())],
            },
        },
        HookKind::Agent => Action::Agent {
            prompt: required(&config.prompt, "prompt")?,
        },
    };
    Ok(Hook {
        event,
        matcher: tool_matcher(config.matcher.as_deref())?,
        when: config.when.as_deref().map(str::parse::<Rule>).transpose()?,
        action,
        timeout: Duration::from_secs(config.timeout),
        detached: config.detached,
        skill: None,
    })
}

/// The command hooks in a skill's `hooks` frontmatter:
/// `{ <Event>: [ { matcher?, hooks: [ { type, command, timeout } ] } ] }`.
fn skill_hooks(skill: &Skill) -> Vec<Hook> {
    let mut out = Vec::new();
    let Some(Value::Object(events)) = &skill.hooks else {
        if skill.hooks.is_some() {
            tracing::warn!(
                "skill {}: `hooks` is not a mapping of events; ignored",
                skill.name
            );
        }
        return out;
    };
    let origin = Arc::new(SkillOrigin {
        name: skill.name.clone(),
        dir: skill.dir.clone(),
        plugin_root: match skill.source {
            SkillSource::Package(_) => skill.dir.parent().and_then(Path::parent).map(Path::to_path_buf),
            _ => None,
        },
    });
    for (name, groups) in events {
        let Some(event) = HookEvent::from_wire(name) else {
            tracing::warn!(
                "skill {}: hooks for unknown event `{name}` are ignored",
                skill.name
            );
            continue;
        };
        for group in groups.as_array().into_iter().flatten() {
            let matcher = match tool_matcher(group.get("matcher").and_then(Value::as_str)) {
                Ok(matcher) => matcher,
                Err(problem) => {
                    tracing::warn!("skill {}: {name} hooks are ignored: {problem}", skill.name);
                    continue;
                }
            };
            for entry in group.get("hooks").and_then(Value::as_array).into_iter().flatten() {
                let kind = entry.get("type").and_then(Value::as_str).unwrap_or("command");
                let command = entry
                    .get("command")
                    .and_then(Value::as_str)
                    .filter(|command| !command.trim().is_empty());
                let (true, Some(command)) = (kind == "command", command) else {
                    tracing::warn!(
                        "skill {}: a {name} hook is ignored: skills may only carry command hooks",
                        skill.name
                    );
                    continue;
                };
                out.push(Hook {
                    event,
                    matcher: matcher.clone(),
                    when: None,
                    action: Action::Command(command.to_string()),
                    timeout: Duration::from_secs(
                        entry
                            .get("timeout")
                            .and_then(Value::as_u64)
                            .unwrap_or(DEFAULT_TIMEOUT_SECS),
                    ),
                    detached: false,
                    skill: Some(origin.clone()),
                });
            }
        }
    }
    out
}

/// What permission rules are matched against for a call to one of the built-in tools.
fn subjects(tool: &str, input: &Value, posix: bool) -> Vec<String> {
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    match tool {
        "shell" => field("command")
            .map(|command| analyze(command, posix).subjects())
            .unwrap_or_default(),
        "read" | "write" | "edit" => field("path").map(str::to_string).into_iter().collect(),
        "web_fetch" => field("url")
            .and_then(|url| reqwest::Url::parse(url).ok())
            .and_then(|url| url.host_str().map(|host| format!("domain:{host}")))
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

#[derive(Default)]
enum Verdict {
    #[default]
    Pass,
    Block(String),
    /// Let the tool call through without asking the user.
    Allow,
    /// Leave the decision to the user.
    Ask,
}

/// What one hook answered.
#[derive(Default)]
struct Reply {
    verdict: Verdict,
    updated_input: Option<Value>,
    /// `hookSpecificOutput.additionalContext`.
    context: Option<String>,
    /// Output that was not JSON.
    stdout: Option<String>,
}

impl Reply {
    fn block(reason: impl Into<String>) -> Self {
        Reply {
            verdict: Verdict::Block(reason.into()),
            ..Reply::default()
        }
    }

    /// Reads the JSON a command hook prints or an HTTP hook answers. The strictest instruction in
    /// it wins.
    fn from_json(reply: &Value) -> Self {
        let text = |from: Option<&Value>, key: &str| {
            from.and_then(|value| value.get(key))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        let specific = reply.get("hookSpecificOutput");
        let reason = text(Some(reply), "reason");
        let permission = text(specific, "permissionDecision");
        let decision = text(Some(reply), "decision");
        let verdict = if reply.get("continue").and_then(Value::as_bool) == Some(false) {
            Verdict::Block(
                text(Some(reply), "stopReason")
                    .or(reason)
                    .unwrap_or_else(|| "stopped by a hook".to_string()),
            )
        } else if permission.as_deref() == Some("deny") {
            Verdict::Block(
                text(specific, "permissionDecisionReason")
                    .or(reason)
                    .unwrap_or_else(|| "denied by a hook".to_string()),
            )
        } else if decision.as_deref() == Some("block") {
            Verdict::Block(reason.unwrap_or_else(|| "blocked by a hook".to_string()))
        } else if permission.as_deref() == Some("ask") {
            Verdict::Ask
        } else if permission.as_deref() == Some("allow") || decision.as_deref() == Some("approve") {
            Verdict::Allow
        } else {
            Verdict::Pass
        };
        Reply {
            verdict,
            updated_input: specific
                .and_then(|value| value.get("updatedInput"))
                .filter(|input| input.is_object())
                .cloned(),
            context: text(specific, "additionalContext"),
            stdout: None,
        }
    }

    /// Reads what a command hook printed before exiting 0.
    fn from_stdout(stdout: &str) -> Result<Self, String> {
        let stdout = stdout.trim();
        if stdout.is_empty() {
            Ok(Reply::default())
        } else if stdout.starts_with('{') {
            serde_json::from_str::<Value>(stdout)
                .map(|reply| Reply::from_json(&reply))
                .map_err(|e| format!("printed invalid JSON: {e}"))
        } else {
            Ok(Reply {
                stdout: Some(stdout.to_string()),
                ..Reply::default()
            })
        }
    }

    /// Reads the `{"ok": …}` verdict of a prompt or agent hook: the last such object in `answer`.
    fn from_verdict(answer: &str) -> Result<Self, String> {
        let verdict = answer.rmatch_indices('{').find_map(|(at, _)| {
            let object = serde_json::Deserializer::from_str(&answer[at..])
                .into_iter::<Value>()
                .next()?
                .ok()?;
            let ok = object.get("ok")?.as_bool()?;
            let reason = object
                .get("reason")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .map(str::to_string);
            Some((ok, reason))
        });
        match verdict {
            Some((true, _)) => Ok(Reply::default()),
            Some((false, reason)) => Ok(Reply::block(
                reason.unwrap_or_else(|| "rejected by a hook".to_string()),
            )),
            None => Err(format!(
                "gave no {{\"ok\": …}} verdict; it said: {}",
                clip(answer, 200)
            )),
        }
    }
}

/// One bus event as hooks see it.
struct Occasion<'a> {
    event: HookEvent,
    agent: Option<&'a AgentInfo>,
    /// Name and input of the tool, on tool events.
    tool: Option<(&'a str, &'a Value)>,
    /// The event's own stdin fields.
    fields: Map<String, Value>,
    /// Whether a blocking hook has an effect on this event.
    blockable: bool,
    /// Whether the event comes from an agent that an `agent` hook started.
    nested: bool,
}

impl<'a> Occasion<'a> {
    fn new(event: HookEvent) -> Self {
        Occasion {
            event,
            agent: None,
            tool: None,
            fields: Map::new(),
            blockable: false,
            nested: false,
        }
    }

    fn agent(mut self, agent: &'a AgentInfo) -> Self {
        self.agent = Some(agent);
        self
    }

    fn tool(mut self, name: &'a str, input: &'a Value) -> Self {
        self.tool = Some((name, input));
        self
    }

    fn field(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.fields.insert(key.to_string(), value.into());
        self
    }

    fn blockable(mut self, blockable: bool) -> Self {
        self.blockable = blockable;
        self
    }
}

/// What the hooks of one event decided together.
#[derive(Default)]
struct Summary {
    block: Option<String>,
    /// The last hook that answered `allow` (true) or `ask` (false).
    permission: Option<bool>,
    /// The tool input after the hooks' rewrites, when a hook rewrote it.
    input: Option<Value>,
    context: Vec<String>,
}

impl Summary {
    fn outcome(self) -> Outcome {
        match self.block {
            Some(reason) => Outcome::Block { reason },
            None => Outcome::Continue,
        }
    }
}

/// The stdin fields every hook receives.
struct Common {
    session_id: String,
    transcript_path: String,
    cwd: String,
    agent_id: String,
    permission_mode: &'static str,
}

/// Everything one hook run needs, owned so the run can outlive the event.
struct Job {
    payload: Value,
    cwd: PathBuf,
    services: Arc<Services>,
    session_id: String,
    /// The agent the event belongs to; kept for `agent` hooks only.
    agent: Option<AgentInfo>,
    /// Warnings go to the log only (the hook runs on a notification).
    quiet: bool,
}

impl Job {
    fn warn(&self, text: &str) {
        match self.services.get::<Ui>() {
            Some(ui) if !self.quiet => ui.0.notify(Level::Warn, text),
            _ => tracing::warn!("{text}"),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Inner {
    config: Vec<Arc<Hook>>,
    /// Hooks of the skills used in this session.
    skills: RwLock<Vec<Arc<Hook>>>,
    /// Context printed by `session_start` hooks, waiting for the next user prompt.
    session_context: Mutex<Vec<String>>,
    /// `(agent id, call id, input)` of tool calls whose result has not arrived.
    calls: Mutex<VecDeque<(String, String, Value)>>,
    /// Ids of the agents an `agent` hook is currently running a verifier for.
    agent_hooks: Mutex<Vec<String>>,
    shell: Shell,
    paths: Paths,
    claude_compat: bool,
    trusted: bool,
    http: LazyLock<reqwest::Client>,
}

/// Marks an agent as having an `agent` hook in flight until dropped.
struct InFlight<'a> {
    list: &'a Mutex<Vec<String>>,
    id: String,
}

impl<'a> InFlight<'a> {
    fn new(list: &'a Mutex<Vec<String>>, id: &str) -> Self {
        lock(list).push(id.to_string());
        InFlight {
            list,
            id: id.to_string(),
        }
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let mut list = lock(self.list);
        if let Some(at) = list.iter().position(|id| *id == self.id) {
            list.swap_remove(at);
        }
    }
}

impl Inner {
    fn hooks(&self, event: HookEvent) -> Vec<Arc<Hook>> {
        let skills = self.skills.read().unwrap_or_else(PoisonError::into_inner);
        self.config
            .iter()
            .chain(skills.iter())
            .filter(|hook| hook.event == event)
            .cloned()
            .collect()
    }

    fn has(&self, event: HookEvent) -> bool {
        let skills = self.skills.read().unwrap_or_else(PoisonError::into_inner);
        self.config
            .iter()
            .chain(skills.iter())
            .any(|hook| hook.event == event)
    }

    /// Activates the hooks `skill` carries, replacing those of an earlier use of it.
    fn add_skill(&self, skill: &Skill) {
        if skill.hooks.is_none() {
            return;
        }
        let from_project = skill.source == SkillSource::Project
            || project_skill_roots(&self.paths, true)
                .iter()
                .any(|root| skill.dir.starts_with(root));
        if from_project && !self.trusted {
            tracing::warn!(
                "hooks of skill {} are ignored: the project is not trusted",
                skill.name
            );
            return;
        }
        let hooks = skill_hooks(skill);
        let mut active = self.skills.write().unwrap_or_else(PoisonError::into_inner);
        active.retain(|hook| hook.skill.as_ref().is_none_or(|origin| origin.name != skill.name));
        active.extend(hooks.into_iter().map(Arc::new));
    }

    /// Ends the session's state: skill hooks, unused start context, unanswered calls.
    fn reset(&self) {
        self.skills
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        lock(&self.session_context).clear();
        lock(&self.calls).clear();
    }

    /// Keeps the input of a tool call for the hooks that run on its result.
    fn remember(&self, agent: &str, call_id: &str, input: &Value) {
        if !self.has(HookEvent::PostToolUse) && !self.has(HookEvent::PostToolUseFailure) {
            return;
        }
        let mut calls = lock(&self.calls);
        if calls.len() >= MAX_PENDING_CALLS {
            calls.pop_front();
        }
        calls.push_back((agent.to_string(), call_id.to_string(), input.clone()));
    }

    fn recall(&self, agent: &str, call_id: &str) -> Option<Value> {
        let mut calls = lock(&self.calls);
        let at = calls
            .iter()
            .position(|(owner, id, _)| owner == agent && id == call_id)?;
        calls.remove(at).map(|(_, _, input)| input)
    }

    /// Whether an agent of definition `def` under `parent` is the verifier of an `agent` hook that
    /// is still running. Its own events must not start further `agent` hooks.
    fn hook_started(&self, def: &str, parent: Option<&str>) -> bool {
        def == VERIFIER && parent.is_some_and(|parent| lock(&self.agent_hooks).iter().any(|id| id == parent))
    }

    /// Runs the hooks of `occasion.event` that select it, in order.
    async fn run(self: &Arc<Self>, ctx: &ExtCtx, occasion: Occasion<'_>) -> Summary {
        let mut summary = Summary::default();
        let hooks = self.hooks(occasion.event);
        if hooks.is_empty() {
            return summary;
        }
        let permissions = ctx.services.get::<PermissionState>();
        let cwd = match (&permissions, occasion.agent) {
            (Some(state), Some(agent)) => state.cwd_of(agent),
            _ => ctx.cwd.clone(),
        };
        let common = Common {
            session_id: ctx.session_id.clone(),
            transcript_path: ctx
                .services
                .get::<SessionStore>()
                .map(|session| session.path().display().to_string())
                .unwrap_or_default(),
            cwd: cwd.display().to_string(),
            agent_id: occasion.agent.map_or(LEAD, |agent| agent.id.as_str()).to_string(),
            permission_mode: permissions
                .as_ref()
                .map(|state| match occasion.agent {
                    Some(agent) => state.mode_for(agent),
                    None => state.mode(),
                })
                .map_or("", PermissionMode::as_str),
        };
        let home = std::env::home_dir();
        let scope = MatchCtx {
            project_root: &self.paths.project_root,
            cwd: &cwd,
            home: home.as_deref(),
        };
        let nested = occasion.nested
            || occasion
                .agent
                .is_some_and(|agent| self.hook_started(&agent.def, agent.parent.as_deref()));
        let takes_stdout = matches!(
            occasion.event,
            HookEvent::UserPromptSubmit | HookEvent::SessionStart
        );

        for hook in hooks {
            let runs_agent = matches!(hook.action, Action::Agent { .. });
            if nested && runs_agent {
                continue;
            }
            let tool = occasion
                .tool
                .map(|(name, input)| (name, summary.input.as_ref().unwrap_or(input)));
            if let Some((name, input)) = tool
                && !hook.selects(name, input, &scope, self.shell.is_posix())
            {
                continue;
            }
            let job = Job {
                payload: payload(&hook, &occasion, &common, tool),
                cwd: cwd.clone(),
                services: ctx.services.clone(),
                session_id: ctx.session_id.clone(),
                agent: occasion.agent.filter(|_| runs_agent).cloned(),
                quiet: occasion.event == HookEvent::Notification,
            };
            let reply = self.fire(&hook, job).await;
            if occasion.event == HookEvent::PreToolUse
                && let Some(input) = reply.updated_input
            {
                summary.input = Some(input);
            }
            summary.context.extend(reply.context);
            if takes_stdout {
                summary.context.extend(reply.stdout);
            }
            match reply.verdict {
                Verdict::Block(reason) if occasion.blockable => {
                    summary.block = Some(reason);
                    break;
                }
                Verdict::Allow => summary.permission = Some(true),
                Verdict::Ask => summary.permission = Some(false),
                Verdict::Block(_) | Verdict::Pass => {}
            }
        }
        summary
    }

    /// Runs one hook, or starts it and returns at once when it is detached.
    async fn fire(self: &Arc<Self>, hook: &Arc<Hook>, job: Job) -> Reply {
        if hook.detached {
            let (hooks, hook) = (self.clone(), hook.clone());
            tokio::spawn(async move {
                hooks.execute(&hook, job).await;
            });
            return Reply::default();
        }
        self.execute(hook, job).await
    }

    /// A hook that fails warns the user and answers nothing.
    async fn execute(&self, hook: &Hook, job: Job) -> Reply {
        let result = match &hook.action {
            Action::Command(command) => self.command(hook, command, &job).await,
            Action::Http { url, headers } => self.http(hook, url, headers, &job).await,
            Action::Prompt { prompt, chain } => self.prompt(hook, prompt, chain, &job).await,
            Action::Agent { prompt } => self.agent(hook, prompt, &job).await,
        };
        result.unwrap_or_else(|problem| {
            job.warn(&format!("{} {problem}", hook.label()));
            Reply::default()
        })
    }

    fn env(&self, hook: &Hook, job: &Job) -> Vec<(String, String)> {
        let mut env = job
            .services
            .get::<AgentEnv>()
            .map(|env| env.vars())
            .unwrap_or_default();
        let mut set = |key: &str, value: String| env.push((key.to_string(), value));
        let project = self.paths.project_root.display().to_string();
        if self.claude_compat {
            set("CLAUDE_PROJECT_DIR", project.clone());
        }
        set("AZAZEL_PROJECT_DIR", project);
        set("AZAZEL_SESSION_ID", job.session_id.clone());
        set("AZAZEL_HOOK_EVENT", hook.event.config_name().to_string());
        set("AZAZEL_HOOKS_DISABLED", "1".to_string());
        if let Some(skill) = &hook.skill {
            let dir = skill.dir.display().to_string();
            set("AZAZEL_SKILL_DIR", dir.clone());
            set("CLAUDE_SKILL_DIR", dir);
            if let Some(root) = &skill.plugin_root {
                set("CLAUDE_PLUGIN_ROOT", root.display().to_string());
            }
        }
        env
    }

    async fn command(&self, hook: &Hook, command: &str, job: &Job) -> Result<Reply, String> {
        let env = self.env(hook, job);
        let stdin = job.payload.to_string().into_bytes();
        let run = capture(&self.shell, command, &job.cwd, &env, stdin, hook.timeout)
            .await
            .map_err(|e| format!("could not be started: {e}"))?;
        if run.timed_out {
            return Err(format!("timed out after {} s", hook.timeout.as_secs()));
        }
        let stderr = run.stderr.trim();
        match run.code {
            Some(0) => Reply::from_stdout(&run.stdout),
            Some(2) if stderr.is_empty() => Ok(Reply::block("blocked by a hook")),
            Some(2) => Ok(Reply::block(stderr)),
            Some(code) if stderr.is_empty() => Err(format!("exited with code {code}")),
            Some(code) => Err(format!("exited with code {code}: {stderr}")),
            None => Err("ended without an exit code".to_string()),
        }
    }

    async fn http(
        &self,
        hook: &Hook,
        url: &str,
        headers: &BTreeMap<String, String>,
        job: &Job,
    ) -> Result<Reply, String> {
        let mut request = self.http.post(url).timeout(hook.timeout).json(&job.payload);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request.send().await.map_err(|e| format!("failed: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("answered {status}"));
        }
        let body = response
            .text()
            .await
            .map_err(|e| format!("sent an unreadable reply: {e}"))?;
        let body = body.trim();
        if body.is_empty() {
            return Ok(Reply::default());
        }
        match serde_json::from_str::<Value>(body) {
            Ok(reply) if reply.is_object() => Ok(Reply::from_json(&reply)),
            _ => Err("answered something that is not a JSON object".to_string()),
        }
    }

    async fn prompt(
        &self,
        hook: &Hook,
        prompt: &str,
        chain: &[ModelSpec],
        job: &Job,
    ) -> Result<Reply, String> {
        let router = job
            .services
            .get::<Router>()
            .ok_or("cannot run: no model is available")?;
        let request = Request {
            system: vec![SystemPart {
                text: format!(
                    "You decide whether an event in a coding session may proceed. Follow the user's \
                     instruction, judge the event that comes after it, and reply with {VERDICT_FORMAT}."
                ),
                cache_boundary: false,
            }],
            messages: vec![Message::user(format!("{prompt}\n\nEvent:\n{}", job.payload))],
            ..Request::default()
        };
        let cancel = CancellationToken::new();
        let _stop_on_timeout = cancel.clone().drop_guard();
        let answer = tokio::time::timeout(hook.timeout, complete(&router, chain, &request, cancel))
            .await
            .map_err(|_| format!("timed out after {} s", hook.timeout.as_secs()))?
            .map_err(|e| format!("failed: {e}"))?;
        Reply::from_verdict(&answer)
    }

    async fn agent(&self, hook: &Hook, prompt: &str, job: &Job) -> Result<Reply, String> {
        let runner = job
            .services
            .get::<Subagents>()
            .ok_or_else(|| format!("cannot run: {UNAVAILABLE}"))?;
        let parent = job.agent.clone().unwrap_or_else(|| AgentInfo {
            id: LEAD.to_string(),
            def: LEAD.to_string(),
            parent: None,
            depth: 0,
            session_id: job.session_id.clone(),
        });
        let task = format!(
            "{prompt}\n\nEvent:\n{}\n\nEnd your report with {VERDICT_FORMAT}.",
            job.payload
        );
        let _running = InFlight::new(&self.agent_hooks, &parent.id);
        let report = tokio::time::timeout(hook.timeout, runner.0.run(VERIFIER, task, &parent))
            .await
            .map_err(|_| format!("timed out after {} s", hook.timeout.as_secs()))?
            .map_err(|e| format!("failed: {e}"))?;
        Reply::from_verdict(&report)
    }
}

/// The JSON a hook receives.
fn payload(hook: &Hook, occasion: &Occasion<'_>, common: &Common, tool: Option<(&str, &Value)>) -> Value {
    let mut fields = Map::new();
    let mut put = |key: &str, value: Value| {
        fields.insert(key.to_string(), value);
    };
    put("hook_event_name", occasion.event.wire_name().into());
    put("session_id", common.session_id.as_str().into());
    put("transcript_path", common.transcript_path.as_str().into());
    put("cwd", common.cwd.as_str().into());
    put("agent_id", common.agent_id.as_str().into());
    put("permission_mode", common.permission_mode.into());
    if let Some((name, input)) = tool {
        put("tool_name", hook.shown_tool(name).into());
        put("tool_input", input.clone());
    }
    for (key, value) in &occasion.fields {
        put(key.as_str(), value.clone());
    }
    Value::Object(fields)
}

/// What hooks are told about a tool's result: its text, never its images.
fn tool_response(output: &ToolOutput) -> Value {
    let mut response = Map::new();
    response.insert("content".to_string(), output.text_content().into());
    response.insert("is_error".to_string(), output.is_error.into());
    if let Some(details) = &output.details {
        response.insert("details".to_string(), details.clone());
    }
    Value::Object(response)
}

/// One model call; returns the text of the reply.
async fn complete(
    router: &Router,
    chain: &[ModelSpec],
    request: &Request,
    cancel: CancellationToken,
) -> Result<String, String> {
    let (_, mut events) = router
        .stream(chain, request, cancel)
        .await
        .map_err(|e| e.to_string())?;
    while let Some(event) = std::future::poll_fn(|cx| events.as_mut().poll_next(cx)).await {
        match event {
            StreamEvent::Done { message } => return Ok(message.text()),
            StreamEvent::Error { error, .. } => return Err(error.to_string()),
            _ => {}
        }
    }
    Err("the model returned nothing".to_string())
}

struct Captured {
    /// `None` when the process was killed.
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

type Pipe = (Arc<Mutex<Vec<u8>>>, JoinHandle<()>);

/// Reads `pipe` to its end in the background, keeping the first [`MAX_CAPTURE_BYTES`].
fn collect(mut pipe: impl AsyncRead + Unpin + Send + 'static) -> Pipe {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let sink = buffer.clone();
    let task = tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let mut kept = lock(&sink);
                    let room = MAX_CAPTURE_BYTES.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..read.min(room)]);
                }
            }
        }
    });
    (buffer, task)
}

/// What `pipe` has delivered once it closes, or shortly after the process is gone.
async fn finish(pipe: Option<Pipe>) -> String {
    let Some((buffer, mut task)) = pipe else {
        return String::new();
    };
    if tokio::time::timeout(DRAIN_AFTER_EXIT, &mut task).await.is_err() {
        task.abort();
    }
    let bytes = std::mem::take(&mut *lock(&buffer));
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Runs `script` with `stdin`, keeping stdout and stderr apart. The process tree is killed at the
/// timeout.
async fn capture(
    shell: &Shell,
    script: &str,
    cwd: &Path,
    env: &[(String, String)],
    stdin: Vec<u8>,
    timeout: Duration,
) -> io::Result<Captured> {
    let mut child = process::spawn(shell, script, cwd, env, true)?;
    if let Some(mut pipe) = child.stdin().take() {
        tokio::spawn(async move {
            // A hook that does not read its input closes the pipe early; that is not a failure.
            let _ = pipe.write_all(&stdin).await;
            let _ = pipe.shutdown().await;
        });
    }
    let stdout = child.stdout().take().map(collect);
    let stderr = child.stderr().take().map(collect);
    let status = tokio::time::timeout(timeout, child.wait()).await;
    let timed_out = status.is_err();
    if timed_out {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    let stdout = finish(stdout).await;
    let stderr = finish(stderr).await;
    // Whatever the hook left running goes with it.
    let _ = child.start_kill();
    Ok(Captured {
        code: status.ok().and_then(Result::ok).and_then(|status| status.code()),
        stdout,
        stderr,
        timed_out,
    })
}

struct Dispatcher(Arc<Inner>);

#[async_trait]
impl EventHandler for Dispatcher {
    async fn handle(&self, ev: &mut Event, ctx: &ExtCtx) -> Outcome {
        let hooks = &self.0;
        match ev {
            Event::SessionStart { source } => {
                let source = match source {
                    StartSource::Startup => "startup",
                    StartSource::Resume => "resume",
                    StartSource::Clear => "clear",
                    StartSource::Compact => "compact",
                };
                let occasion = Occasion::new(HookEvent::SessionStart).field("source", source);
                let summary = hooks.run(ctx, occasion).await;
                *lock(&hooks.session_context) = summary.context;
                Outcome::Continue
            }
            Event::SessionEnd => {
                hooks.run(ctx, Occasion::new(HookEvent::SessionEnd)).await;
                hooks.reset();
                if let Some(skills) = ctx.services.get::<SkillRegistry>() {
                    skills.reset_used();
                }
                Outcome::Continue
            }
            Event::UserPrompt { text, .. } => {
                let occasion = Occasion::new(HookEvent::UserPromptSubmit)
                    .field("prompt", text.as_str())
                    .blockable(true);
                let summary = hooks.run(ctx, occasion).await;
                if let Some(reason) = summary.block {
                    return Outcome::Block { reason };
                }
                let mut context = std::mem::take(&mut *lock(&hooks.session_context));
                context.extend(summary.context);
                if !context.is_empty() {
                    text.push_str(&format!(
                        "\n\n<hook-context>\n{}\n</hook-context>",
                        context.join("\n")
                    ));
                }
                Outcome::Continue
            }
            Event::ToolCall {
                agent,
                call_id,
                tool,
                input,
                skip_prompt,
            } => {
                let occasion = Occasion::new(HookEvent::PreToolUse)
                    .agent(agent)
                    .tool(tool, input)
                    .field("tool_use_id", call_id.as_str())
                    .blockable(true);
                let summary = hooks.run(ctx, occasion).await;
                if let Some(reason) = summary.block {
                    return Outcome::Block { reason };
                }
                if let Some(rewritten) = summary.input {
                    *input = rewritten;
                }
                if let Some(allow) = summary.permission {
                    *skip_prompt = allow;
                }
                hooks.remember(&agent.id, call_id, input);
                Outcome::Continue
            }
            Event::ToolResult {
                agent,
                call_id,
                tool,
                output,
            } => {
                let event = if output.is_error {
                    HookEvent::PostToolUseFailure
                } else {
                    HookEvent::PostToolUse
                };
                let input = hooks.recall(&agent.id, call_id).unwrap_or(Value::Null);
                if !hooks.has(event) {
                    return Outcome::Continue;
                }
                let occasion = Occasion::new(event)
                    .agent(agent)
                    .tool(tool, &input)
                    .field("tool_use_id", call_id.as_str())
                    .field("tool_response", tool_response(output))
                    .blockable(!output.is_error);
                let summary = hooks.run(ctx, occasion).await;
                output
                    .content
                    .extend(summary.context.into_iter().map(Block::text));
                if let Some(reason) = summary.block {
                    output.content.push(Block::text(reason));
                }
                Outcome::Continue
            }
            Event::AgentSpawn { parent, def, .. } => {
                let mut occasion = Occasion::new(HookEvent::SubagentStart)
                    .agent(parent)
                    .field("agent_type", def.as_str())
                    .blockable(true);
                occasion.nested = hooks.hook_started(def, Some(parent.id.as_str()));
                hooks.run(ctx, occasion).await.outcome()
            }
            Event::PreCompact { agent, manual } => {
                let occasion = Occasion::new(HookEvent::PreCompact)
                    .agent(agent)
                    .field("trigger", if *manual { "manual" } else { "auto" })
                    .blockable(*manual);
                hooks.run(ctx, occasion).await.outcome()
            }
            Event::PostCompact { agent } => {
                hooks
                    .run(ctx, Occasion::new(HookEvent::PostCompact).agent(agent))
                    .await;
                Outcome::Continue
            }
            Event::Stop {
                agent,
                stop_hook_active,
            } => {
                let occasion = if agent.depth == 0 {
                    Occasion::new(HookEvent::Stop)
                } else {
                    let transcript = hooks
                        .paths
                        .agent_transcripts_dir(&ctx.session_id)
                        .join(format!("{}.jsonl", agent.id));
                    Occasion::new(HookEvent::SubagentStop)
                        .field("agent_transcript_path", transcript.display().to_string())
                };
                let occasion = occasion
                    .agent(agent)
                    .field("stop_hook_active", *stop_hook_active)
                    .blockable(true);
                hooks.run(ctx, occasion).await.outcome()
            }
            Event::PermissionRequest {
                agent,
                tool,
                input,
                decision,
                ..
            } => {
                let occasion = Occasion::new(HookEvent::PermissionRequest)
                    .agent(agent)
                    .tool(tool, input)
                    .blockable(true);
                let summary = hooks.run(ctx, occasion).await;
                if let Some(reason) = summary.block {
                    *decision = Some(false);
                    return Outcome::Block { reason };
                }
                match summary.permission {
                    Some(true) => *decision = Some(true),
                    Some(false) => *decision = None,
                    None => {}
                }
                Outcome::Continue
            }
            Event::Notification { text, .. } => {
                let occasion = Occasion::new(HookEvent::Notification).field("message", text.as_str());
                hooks.run(ctx, occasion).await;
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }
}
