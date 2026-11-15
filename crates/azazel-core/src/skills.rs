//! Skills: instruction packs discovered on disk, listed in the prompt by name and description, and
//! loaded in full only when used.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use async_trait::async_trait;
use azazel_agent::{Access, AgentInfo, Services, Tool, ToolCtx, ToolOutput};
use azazel_ai::{Block, ModelSpec};
use serde_json::{Value, json};

use crate::ext::{Event, EventHandler, EventKind, ExtCtx, Outcome};
use crate::frontmatter;
use crate::paths::{Paths, canonical};
use crate::permissions::{PermissionState, Rule, RuleKind};
use crate::prompt::SystemPartProvider;
use crate::scheme::Scheme;
use crate::subagent::Subagents;
use crate::trust::project_skill_roots;

pub const MAX_NAME_CHARS: usize = 64;
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
/// Above this size the prompt listing shortens every description.
const MAX_LISTING_CHARS: usize = 8000;
const SHORT_DESCRIPTION_CHARS: usize = 200;
const MAX_REFERENCE_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    Builtin,
    /// `~/.claude/skills`, `~/.agents/skills` and their project counterparts.
    Compat,
    Home,
    Package(String),
    Project,
    /// Named on the command line for one run (`--skill`).
    CommandLine,
}

impl SkillSource {
    pub fn label(&self) -> String {
        match self {
            SkillSource::Builtin => "built-in".to_string(),
            SkillSource::Compat => "compat".to_string(),
            SkillSource::Home => "home".to_string(),
            SkillSource::Package(name) => format!("package {name}"),
            SkillSource::Project => "project".to_string(),
            SkillSource::CommandLine => "command line".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillContext {
    /// The body is returned to the calling agent.
    Inline,
    /// The body is run by a sub-agent and its report returned.
    Fork,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub argument_hint: Option<String>,
    /// Listed in the system prompt so the model can pick it.
    pub model_invocable: bool,
    /// Available as `/skill:<name>`.
    pub user_invocable: bool,
    pub context: SkillContext,
    /// Agent definition used for `context: fork`.
    pub agent: Option<String>,
    pub model: Option<Vec<ModelSpec>>,
    pub allowed_tools: Option<Vec<String>>,
    /// Raw `hooks` frontmatter.
    pub hooks: Option<Value>,
    /// Directory holding `SKILL.md`; relative references in the body resolve against it.
    pub dir: PathBuf,
    pub body: String,
    pub source: SkillSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSkill {
    pub path: PathBuf,
    pub error: String,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name.split('-').all(|part| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

/// Parses one `SKILL.md`. The frontmatter must be strict YAML; `name` is kebab-case of at most 64
/// characters and `description` is non-empty and at most 1024 characters.
pub fn parse_skill(file: &Path, text: &str, source: SkillSource) -> Result<Skill, String> {
    let (yaml, body) =
        frontmatter::split(text).ok_or("missing frontmatter: the file must start with `---`")?;
    let meta = frontmatter::parse(yaml)?;
    let text_of = |key: &str| {
        meta.get(key)
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let flag = |key: &str, default: bool| meta.get(key).and_then(Value::as_bool).unwrap_or(default);

    let name = text_of("name").ok_or("`name` is missing")?;
    if !valid_name(&name) {
        return Err(format!(
            "`name` must be lowercase words joined by hyphens, at most {MAX_NAME_CHARS} characters"
        ));
    }
    let description = text_of("description").ok_or("`description` is missing or empty")?;
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "`description` is longer than {MAX_DESCRIPTION_CHARS} characters"
        ));
    }
    let model = frontmatter::string_list(meta.get("model"))
        .map(|specs| {
            specs
                .iter()
                .filter_map(|s| s.parse::<ModelSpec>().ok())
                .collect::<Vec<_>>()
        })
        .filter(|chain| !chain.is_empty());
    Ok(Skill {
        name,
        description,
        when_to_use: text_of("when_to_use"),
        argument_hint: text_of("argument-hint"),
        model_invocable: !flag("disable-model-invocation", false),
        user_invocable: flag("user-invocable", true),
        context: if text_of("context").as_deref() == Some("fork") {
            SkillContext::Fork
        } else {
            SkillContext::Inline
        },
        agent: text_of("agent"),
        model,
        allowed_tools: frontmatter::string_or_list(meta.get("allowed-tools")),
        hooks: meta.get("hooks").cloned(),
        dir: file.parent().map(Path::to_path_buf).unwrap_or_default(),
        body: body.trim_start_matches(['\r', '\n']).to_string(),
        source,
    })
}

/// A directory whose sub-directories hold `SKILL.md` files, or (`single`) one such directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRoot {
    pub path: PathBuf,
    pub source: SkillSource,
    /// When set, only these skills of the root are offered to the model; the rest stay available
    /// to the user as `/skill:<name>`.
    pub model_invocable: Option<Vec<String>>,
    /// `path` is itself a skill's directory.
    pub single: bool,
    /// Whether the hooks its skills carry are kept. Off for packages whose code is not enabled.
    pub hooks: bool,
}

impl SkillRoot {
    pub fn new(path: PathBuf, source: SkillSource) -> Self {
        SkillRoot {
            path,
            source,
            model_invocable: None,
            single: false,
            hooks: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SkillSet {
    pub skills: BTreeMap<String, Skill>,
    pub invalid: Vec<InvalidSkill>,
}

/// Reads every root in order; a later root replaces an earlier skill of the same name.
pub fn discover(roots: &[SkillRoot]) -> SkillSet {
    let mut set = SkillSet::default();
    for root in roots {
        let mut dirs: Vec<PathBuf> = if root.single {
            vec![root.path.clone()]
        } else {
            std::fs::read_dir(&root.path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .collect()
        };
        dirs.sort();
        for dir in dirs {
            let file = dir.join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            match parse_skill(&file, &text, root.source.clone()) {
                Ok(mut skill) => {
                    if let Some(allowed) = &root.model_invocable {
                        skill.model_invocable &= allowed.contains(&skill.name);
                    }
                    if !root.hooks {
                        skill.hooks = None;
                    }
                    set.skills.insert(skill.name.clone(), skill);
                }
                Err(error) => set.invalid.push(InvalidSkill { path: file, error }),
            }
        }
    }
    set
}

/// Skill roots, lowest precedence first: the built-in skills, then those of installed packages,
/// the user's and the project's.
pub fn standard_roots(
    paths: &Paths,
    claude_compat: bool,
    packages: &crate::packages::Loaded,
) -> Vec<SkillRoot> {
    let mut roots = Vec::new();
    match crate::builtin::skill_root(&paths.cache_dir()) {
        Ok(root) => roots.push(SkillRoot::new(root, SkillSource::Builtin)),
        Err(e) => tracing::warn!("built-in skills are unavailable: {e}"),
    }
    roots.extend(packages.skill_roots.iter().cloned());
    if claude_compat && let Some(home) = std::env::home_dir() {
        roots.push(SkillRoot::new(
            home.join(".claude").join("skills"),
            SkillSource::Compat,
        ));
        roots.push(SkillRoot::new(
            home.join(".agents").join("skills"),
            SkillSource::Compat,
        ));
    }
    roots.push(SkillRoot::new(paths.home.join("skills"), SkillSource::Home));
    let project = project_skill_roots(paths, claude_compat);
    let last = project.len().saturating_sub(1);
    for (i, path) in project.into_iter().enumerate() {
        let source = if i == last {
            SkillSource::Project
        } else {
            SkillSource::Compat
        };
        roots.push(SkillRoot::new(path, source));
    }
    roots
}

/// What an agent receives when a skill is loaded: where it lives, then its body with `$ARGUMENTS`
/// filled in.
pub fn invocation_text(skill: &Skill, args: &str) -> String {
    let args = args.trim();
    let mut body = skill.body.replace("$ARGUMENTS", args);
    if !args.is_empty() && !skill.body.contains("$ARGUMENTS") {
        body.push_str(&format!("\n\nArguments: {args}"));
    }
    format!("Skill directory: {}\n\n{body}", skill.dir.display())
}

/// A skill's `allowed-tools` as permission rules, or `None` when it lists nothing. Skills written
/// for Claude Code name its tools (`Read`, `Bash(git diff:*)`); those become azazel's. An entry
/// that is not a rule is left out, so a list in which nothing is a rule allows nothing: a
/// mistyped limit must not turn into no limit.
pub fn allowed_rules(skill: &Skill) -> Option<Vec<Rule>> {
    let listed = skill.allowed_tools.as_ref().filter(|list| !list.is_empty())?;
    let mut rules: Vec<Rule> = Vec::new();
    for entry in listed {
        match entry.parse::<Rule>() {
            Ok(mut rule) => {
                rule.tool = crate::agents::compat_tool(&rule.tool).to_string();
                if !rules.contains(&rule) {
                    rules.push(rule);
                }
            }
            Err(_) => tracing::warn!(
                "skill {}: `{entry}` in allowed-tools is not `tool` or `tool(pattern)`",
                skill.name
            ),
        }
    }
    Some(rules)
}

/// The agent a user's prompt goes to.
const LEAD: &str = "lead";

struct Grant {
    agent: String,
    /// The prompt count of the run it belongs to.
    run: u64,
    rule: Rule,
    /// What the rule's source reads in the permission state.
    source: String,
}

/// What loaded skills allow. Each `allowed-tools` entry of a skill loaded into an agent's own
/// context is an allow rule for the rest of that agent's run, and is taken away when the run
/// ends. Registered as a service by [`SkillsExt`].
pub struct SkillGrants {
    /// Whether the project is trusted. Its own skills allow nothing when it is not: a file in a
    /// repository must not be able to waive the user's approval.
    trusted: bool,
    /// Counts the user's prompts.
    run: AtomicU64,
    grants: Mutex<Vec<Grant>>,
}

impl SkillGrants {
    pub fn new(trusted: bool) -> SkillGrants {
        SkillGrants {
            trusted,
            run: AtomicU64::new(0),
            grants: Mutex::default(),
        }
    }

    fn from_project(skill: &Skill, permissions: &PermissionState) -> bool {
        match skill.source {
            SkillSource::Project => true,
            // `.claude/skills` exists both under the home directory and in the project.
            SkillSource::Compat => permissions
                .roots()
                .first()
                .is_some_and(|root| canonical(&skill.dir).starts_with(root)),
            SkillSource::Builtin | SkillSource::Home | SkillSource::Package(_) | SkillSource::CommandLine => {
                false
            }
        }
    }

    /// Allows what `skill` lists for the run of `agent`: the run in progress, or with
    /// `next_prompt` the run the user's next prompt starts (a skill the user invoked).
    pub fn allow(&self, services: &Services, skill: &Skill, agent: &str, next_prompt: bool) {
        let (Some(rules), Some(permissions)) = (allowed_rules(skill), services.get::<PermissionState>())
        else {
            return;
        };
        if !self.trusted && Self::from_project(skill, &permissions) {
            tracing::warn!(
                "allowed-tools of skill {} is ignored: the project is not trusted",
                skill.name
            );
            return;
        }
        let run = self.run.load(Ordering::SeqCst) + u64::from(next_prompt);
        let source = format!("skill {}, while {agent} works", skill.name);
        let mut grants = self.grants.lock().unwrap_or_else(PoisonError::into_inner);
        for rule in rules {
            permissions.add_temporary_rule(rule.clone(), RuleKind::Allow, &source);
            match grants
                .iter_mut()
                .find(|grant| grant.source == source && grant.rule == rule)
            {
                Some(known) => known.run = run,
                None => grants.push(Grant {
                    agent: agent.to_string(),
                    run,
                    rule,
                    source: source.clone(),
                }),
            }
        }
    }

    /// Takes away the grants `gone` picks.
    fn end(&self, services: &Services, gone: impl Fn(&Grant) -> bool) {
        let ended: Vec<Grant> = {
            let mut grants = self.grants.lock().unwrap_or_else(PoisonError::into_inner);
            let (ended, kept) = std::mem::take(&mut *grants).into_iter().partition(|g| gone(g));
            *grants = kept;
            ended
        };
        if ended.is_empty() {
            return;
        }
        if let Some(permissions) = services.get::<PermissionState>() {
            permissions.remove_rules(|rule| {
                rule.kind == RuleKind::Allow
                    && ended
                        .iter()
                        .any(|grant| grant.source == rule.source && grant.rule == rule.rule)
            });
        }
    }
}

/// Call when the user invokes `skill` themselves: what it allows holds for the run of the prompt
/// that carries it.
pub fn allow_for_prompt(services: &Services, skill: &Skill) {
    if let Some(grants) = services.get::<SkillGrants>() {
        grants.allow(services, skill, LEAD, true);
    }
}

struct GrantsHandler;

#[async_trait]
impl EventHandler for GrantsHandler {
    async fn handle(&self, event: &mut Event, ctx: &ExtCtx) -> Outcome {
        let Some(grants) = ctx.services.get::<SkillGrants>() else {
            return Outcome::Continue;
        };
        match event {
            // A new prompt is a new run: what an earlier one was allowed is over, also when that
            // run was interrupted and never reached its stop.
            Event::UserPrompt { .. } => {
                let run = grants.run.fetch_add(1, Ordering::SeqCst) + 1;
                grants.end(&ctx.services, |grant| grant.agent == LEAD && grant.run < run);
            }
            Event::Stop { agent, .. } | Event::AgentEnd { agent, .. } => {
                grants.end(&ctx.services, |grant| grant.agent == agent.id);
            }
            Event::SessionStart { .. } | Event::SessionEnd => grants.end(&ctx.services, |_| true),
            _ => {}
        }
        Outcome::Continue
    }
}

/// Ends what skills allowed when the run that loaded them ends. Load it after every other
/// extension: a handler that keeps an agent going blocks the `Stop` event before it gets here, so
/// this one sees only the stops that are final.
pub struct SkillGrantsExt;

impl crate::ext::Extension for SkillGrantsExt {
    fn id(&self) -> &str {
        "skill-grants"
    }

    fn register(&self, r: &mut crate::ext::Registrar) -> Result<(), crate::ext::ExtError> {
        for kind in [
            EventKind::UserPrompt,
            EventKind::Stop,
            EventKind::AgentEnd,
            EventKind::SessionStart,
            EventKind::SessionEnd,
        ] {
            r.on(kind, Arc::new(GrantsHandler));
        }
        Ok(())
    }
}

type InvokeWatcher = Arc<dyn Fn(&Skill) + Send + Sync>;

/// The session's skills. Registered as a service.
#[derive(Default)]
pub struct SkillRegistry {
    roots: RwLock<Vec<SkillRoot>>,
    set: RwLock<SkillSet>,
    invoked: Mutex<Vec<String>>,
    watchers: RwLock<Vec<InvokeWatcher>>,
}

impl SkillRegistry {
    /// Adds a root (higher precedence than those already present) and re-reads everything.
    pub fn add_root(&self, root: SkillRoot) {
        self.roots
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(root);
        self.reload();
    }

    /// Replaces all roots and re-reads everything.
    pub fn set_roots(&self, roots: Vec<SkillRoot>) {
        *self.roots.write().unwrap_or_else(PoisonError::into_inner) = roots;
        self.reload();
    }

    pub fn reload(&self) {
        let roots = self.roots.read().unwrap_or_else(PoisonError::into_inner).clone();
        *self.set.write().unwrap_or_else(PoisonError::into_inner) = discover(&roots);
    }

    pub fn get(&self, name: &str) -> Option<Skill> {
        self.set
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .skills
            .get(name)
            .cloned()
    }

    /// Valid skills, sorted by name.
    pub fn list(&self) -> Vec<Skill> {
        self.set
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .skills
            .values()
            .cloned()
            .collect()
    }

    pub fn invalid(&self) -> Vec<InvalidSkill> {
        self.set
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .invalid
            .clone()
    }

    /// `watcher` runs the first time each skill is used in the session.
    pub fn on_first_use(&self, watcher: InvokeWatcher) {
        self.watchers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(watcher);
    }

    /// Forgets which skills were used, so the next use of each counts as its first again. Call
    /// when a session ends.
    pub fn reset_used(&self) {
        self.invoked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Notes that `skill` was used; tells the watchers when it is the first time.
    pub fn mark_used(&self, skill: &Skill) {
        {
            let mut invoked = self.invoked.lock().unwrap_or_else(PoisonError::into_inner);
            if invoked.contains(&skill.name) {
                return;
            }
            invoked.push(skill.name.clone());
        }
        let watchers = self
            .watchers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for watcher in watchers {
            watcher(skill);
        }
    }

    /// The `skills` part of the system prompt: one line per model-invocable skill, by name.
    pub fn listing(&self) -> Option<String> {
        let skills: Vec<Skill> = self.list().into_iter().filter(|s| s.model_invocable).collect();
        if skills.is_empty() {
            return None;
        }
        let render = |limit: Option<usize>| {
            let mut out = String::from(
                "# Skills\nWhen a task matches one of these, load it with the `skill` tool and follow what it says.\n",
            );
            for skill in &skills {
                let description = match limit {
                    Some(limit) if skill.description.chars().count() > limit => {
                        let cut: String = skill.description.chars().take(limit).collect();
                        format!("{cut}…")
                    }
                    _ => skill.description.clone(),
                };
                out.push_str(&format!("- {}: {description}\n", skill.name));
            }
            out.trim_end().to_string()
        };
        let full = render(None);
        Some(if full.chars().count() > MAX_LISTING_CHARS {
            render(Some(SHORT_DESCRIPTION_CHARS))
        } else {
            full
        })
    }
}

pub struct SkillsPart(pub Arc<SkillRegistry>);

#[async_trait]
impl SystemPartProvider for SkillsPart {
    async fn render(&self, _: &AgentInfo) -> Option<String> {
        self.0.listing()
    }
}

pub struct SkillTool {
    registry: Arc<SkillRegistry>,
}

impl SkillTool {
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        SkillTool { registry }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load a skill by name. Returns the skill's instructions and the directory its reference files \
         live in; read those files with `read` (or `skill://<name>/<file>`) when the instructions point \
         to them."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "args": {"type": "string", "description": "Arguments for the skill, if it takes any"}
            },
            "required": ["name"]
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
        let Some(name) = input.get("name").and_then(Value::as_str) else {
            return ToolOutput::error("invalid arguments: `name` is required");
        };
        let Some(skill) = self.registry.get(name.trim()) else {
            let known: Vec<String> = self
                .registry
                .list()
                .into_iter()
                .map(|s| s.name)
                .take(40)
                .collect();
            return ToolOutput::error(format!(
                "no skill named `{name}`; available: {}",
                known.join(", ")
            ));
        };
        self.registry.mark_used(&skill);
        let text = invocation_text(
            &skill,
            input.get("args").and_then(Value::as_str).unwrap_or_default(),
        );
        if skill.context == SkillContext::Fork
            && let Some(runner) = ctx.services.get::<Subagents>()
        {
            let def = skill.agent.as_deref().unwrap_or("coder");
            // What the skill allows is all its agent gets, inside what the definition grants.
            let report = match allowed_rules(&skill) {
                Some(tools) => runner.0.run_limited(def, text.clone(), &ctx.agent, tools).await,
                None => runner.0.run(def, text.clone(), &ctx.agent).await,
            };
            match report {
                Ok(report) => return ToolOutput::text(report),
                // An agent that may not start others follows the skill itself.
                Err(error) if error == crate::subagent::UNAVAILABLE => {}
                Err(error) => return ToolOutput::error(error),
            }
        }
        if let Some(grants) = ctx.services.get::<SkillGrants>() {
            grants.allow(&ctx.services, &skill, &ctx.agent.id, false);
        }
        ToolOutput::text(text)
    }
}

/// `skill://<name>` is the skill body; `skill://<name>/<relative path>` a file beside `SKILL.md`.
pub struct SkillScheme(pub Arc<SkillRegistry>);

#[async_trait]
impl Scheme for SkillScheme {
    fn scheme(&self) -> &str {
        "skill"
    }

    async fn read(&self, rest: &str, _: &ToolCtx) -> Result<Vec<Block>, String> {
        let (name, relative) = rest.split_once('/').unwrap_or((rest, ""));
        let skill = self
            .0
            .get(name)
            .ok_or_else(|| format!("no skill named `{name}`"))?;
        if relative.is_empty() {
            return Ok(vec![Block::text(skill.body)]);
        }
        let root = dunce::canonicalize(&skill.dir).map_err(|e| format!("{}: {e}", skill.dir.display()))?;
        let target = dunce::canonicalize(root.join(relative))
            .map_err(|_| format!("skill `{name}` has no file `{relative}`"))?;
        if !target.starts_with(&root) {
            return Err(format!("`{relative}` is outside skill `{name}`"));
        }
        let meta = tokio::fs::metadata(&target).await.map_err(|e| e.to_string())?;
        if !meta.is_file() {
            return Err(format!("`{relative}` is not a file"));
        }
        if meta.len() > MAX_REFERENCE_BYTES {
            return Err(format!(
                "`{relative}` is {} bytes; read it by path with offset and limit",
                meta.len()
            ));
        }
        let text = tokio::fs::read_to_string(&target)
            .await
            .map_err(|e| format!("{relative}: {e}"))?;
        Ok(vec![Block::text(text)])
    }
}

/// One entry per skill (name, where it comes from, whether the model is offered it, and its
/// description), then the files that are not valid skills.
pub fn listing(skills: &[Skill], invalid: &[InvalidSkill]) -> String {
    let mut out = String::new();
    for skill in skills {
        let flags = match (skill.model_invocable, skill.user_invocable) {
            (true, true) => "",
            (true, false) => " (model only)",
            (false, true) => " (manual: /skill:name)",
            (false, false) => " (disabled)",
        };
        out.push_str(&format!(
            "{} [{}]{flags}\n  {}\n",
            skill.name,
            skill.source.label(),
            skill.description
        ));
    }
    for invalid in invalid {
        out.push_str(&format!(
            "invalid: {}\n  {}\n",
            invalid.path.display(),
            invalid.error
        ));
    }
    if out.is_empty() {
        "no skills found".to_string()
    } else {
        out
    }
}

struct ListSkills(Arc<SkillRegistry>);

#[async_trait]
impl crate::ext::CommandHandler for ListSkills {
    async fn run(&self, _: &str, _: &crate::ext::ExtCtx) -> crate::ext::CommandOutcome {
        crate::ext::CommandOutcome::Show(listing(&self.0.list(), &self.0.invalid()))
    }
}

/// Registers skill roots, the `skills` prompt part, the `skill` tool, `skill://` and `/skills`.
pub struct SkillsExt {
    /// Lowest precedence first.
    pub roots: Vec<SkillRoot>,
    /// Whether the project is trusted; `allowed-tools` of its own skills counts only when it is.
    pub trusted: bool,
}

impl crate::ext::Extension for SkillsExt {
    fn id(&self) -> &str {
        "skills"
    }

    fn register(&self, r: &mut crate::ext::Registrar) -> Result<(), crate::ext::ExtError> {
        let registry = r
            .services()
            .get::<SkillRegistry>()
            .ok_or_else(|| crate::ext::ExtError("the skill registry service is missing".to_string()))?;
        for root in &self.roots {
            registry.add_root(root.clone());
        }
        r.service(Arc::new(SkillGrants::new(self.trusted)));
        r.system_part("skills", Arc::new(SkillsPart(registry.clone())));
        r.tool(Arc::new(SkillTool::new(registry.clone())));
        r.scheme(Arc::new(SkillScheme(registry.clone())));
        r.command(crate::ext::Command {
            name: "skills".to_string(),
            description: "List skills and where they come from".to_string(),
            argument_hint: None,
            handler: Arc::new(ListSkills(registry)),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, dir: &str, frontmatter: &str, body: &str) -> PathBuf {
        let path = root.join(dir);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("SKILL.md"), format!("---\n{frontmatter}\n---\n{body}")).unwrap();
        path
    }

    fn roots(tmp: &Path) -> Vec<SkillRoot> {
        vec![
            SkillRoot::new(tmp.join("builtin"), SkillSource::Builtin),
            SkillRoot::new(tmp.join("home"), SkillSource::Home),
            SkillRoot::new(tmp.join("project"), SkillSource::Project),
        ]
    }

    #[test]
    fn later_roots_win_and_identity_is_the_frontmatter_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            &tmp.path().join("builtin"),
            "review",
            "name: code-review\ndescription: built-in review",
            "B",
        );
        write_skill(
            &tmp.path().join("home"),
            "my-review",
            "name: code-review\ndescription: my review",
            "H",
        );
        write_skill(
            &tmp.path().join("home"),
            "notes",
            "name: notes\ndescription: take notes",
            "N",
        );
        write_skill(
            &tmp.path().join("project"),
            "taste-skill",
            "name: design-taste\ndescription: project taste",
            "P",
        );

        let set = discover(&roots(tmp.path()));
        assert_eq!(
            set.skills.keys().collect::<Vec<_>>(),
            ["code-review", "design-taste", "notes"]
        );
        let review = &set.skills["code-review"];
        assert_eq!(
            (review.description.as_str(), review.body.as_str(), &review.source),
            ("my review", "H", &SkillSource::Home)
        );
        assert!(review.dir.ends_with("my-review"));
        assert!(set.invalid.is_empty());
    }

    #[test]
    fn invalid_skills_are_reported_with_the_reason_never_dropped_silently() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home");
        write_skill(&root, "a-bad-name", "name: Bad_Name\ndescription: x", "");
        write_skill(&root, "b-no-description", "name: fine", "");
        write_skill(
            &root,
            "c-long",
            &format!("name: long\ndescription: {}", "d".repeat(1025)),
            "",
        );
        write_skill(&root, "d-colon", "name: colon\ndescription: Use when: always", "");
        write_skill(&root, "e-dup", "name: dup\nname: dup2\ndescription: x", "");
        std::fs::create_dir_all(root.join("f-raw")).unwrap();
        std::fs::write(root.join("f-raw/SKILL.md"), "# No frontmatter\n").unwrap();
        write_skill(
            &root,
            "g-ok",
            &format!("name: ok\ndescription: {}", "d".repeat(1024)),
            "",
        );

        let set = discover(&[SkillRoot::new(root, SkillSource::Home)]);
        assert_eq!(set.skills.keys().collect::<Vec<_>>(), ["ok"]);
        let errors: Vec<&str> = set.invalid.iter().map(|i| i.error.as_str()).collect();
        assert_eq!(errors.len(), 6);
        assert!(errors[0].contains("lowercase words joined by hyphens"));
        assert!(errors[1].contains("`description` is missing"));
        assert!(errors[2].contains("longer than 1024"));
        assert!(errors[3].contains("invalid frontmatter"));
        assert!(errors[4].contains("invalid frontmatter"));
        assert!(errors[5].contains("missing frontmatter"));
        assert!(set.invalid[0].path.ends_with("a-bad-name/SKILL.md"));
    }

    #[test]
    fn frontmatter_keys_are_honoured() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_skill(
            tmp.path(),
            "s",
            "name: deploy\ndescription: ship it\nwhen_to_use: on release day\nargument-hint: \"[env]\"\n\
             disable-model-invocation: true\nuser-invocable: false\ncontext: fork\nagent: tester\n\
             model: [\"@slow\", \"not a model\"]\nallowed-tools: read, shell(git diff:*)\nhooks:\n  Stop: []\nunknown-key: ignored",
            "\nDeploy to $ARGUMENTS now.\n",
        );
        let text = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
        let skill = parse_skill(&dir.join("SKILL.md"), &text, SkillSource::Project).unwrap();
        assert_eq!(skill.when_to_use.as_deref(), Some("on release day"));
        assert_eq!(skill.argument_hint.as_deref(), Some("[env]"));
        assert!(!skill.model_invocable && !skill.user_invocable);
        assert_eq!(
            (skill.context, skill.agent.as_deref()),
            (SkillContext::Fork, Some("tester"))
        );
        assert_eq!(skill.model, Some(vec![ModelSpec::Role("slow".into())]));
        assert_eq!(
            skill.allowed_tools,
            Some(vec!["read".to_string(), "shell(git diff:*)".to_string()])
        );
        assert_eq!(skill.hooks, Some(json!({"Stop": []})));
        assert_eq!(skill.body, "Deploy to $ARGUMENTS now.\n");

        assert_eq!(
            invocation_text(&skill, " staging "),
            format!("Skill directory: {}\n\nDeploy to staging now.\n", dir.display())
        );
        let plain = Skill {
            body: "No placeholder.".into(),
            ..skill
        };
        assert!(invocation_text(&plain, "x y").ends_with("No placeholder.\n\nArguments: x y"));
        assert!(invocation_text(&plain, "").ends_with("No placeholder."));
    }

    #[test]
    fn listing_is_sorted_hides_non_model_skills_and_shortens_when_large() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home");
        write_skill(&root, "b", "name: beta\ndescription: second", "");
        write_skill(&root, "a", "name: alpha\ndescription: first", "");
        write_skill(
            &root,
            "h",
            "name: hidden\ndescription: not listed\ndisable-model-invocation: true",
            "",
        );
        let registry = SkillRegistry::default();
        assert_eq!(registry.listing(), None);
        registry.add_root(SkillRoot::new(root.clone(), SkillSource::Home));
        let listing = registry.listing().unwrap();
        assert!(listing.ends_with("- alpha: first\n- beta: second"), "{listing}");
        assert!(!listing.contains("hidden"));

        // A root may offer only some of its skills to the model.
        registry.set_roots(vec![SkillRoot {
            model_invocable: Some(vec!["beta".into()]),
            ..SkillRoot::new(root.clone(), SkillSource::Home)
        }]);
        assert!(!registry.listing().unwrap().contains("alpha"));
        assert!(registry.get("alpha").is_some());

        for i in 0..12 {
            write_skill(
                &root,
                &format!("big{i}"),
                &format!("name: big-{i}\ndescription: {}", "w".repeat(900)),
                "",
            );
        }
        registry.set_roots(vec![SkillRoot::new(root, SkillSource::Home)]);
        let listing = registry.listing().unwrap();
        assert!(listing.chars().count() < 4000, "{}", listing.chars().count());
        assert!(listing.contains(&format!("- big-0: {}…", "w".repeat(200))));
    }

    /// A project whose permission state is a service, and a skill of `source` that allows two
    /// things.
    struct Granting {
        _tmp: tempfile::TempDir,
        services: Arc<Services>,
        permissions: Arc<PermissionState>,
        skill: Skill,
    }

    fn granting(source: SkillSource, trusted: bool) -> Granting {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(project.join(".azazel")).unwrap();
        let home = tmp.path().join("home");
        let paths = Paths::discover(&project, Some(&home.to_string_lossy()));
        let loaded = crate::config::Config::load(&paths, &crate::config::LoadOptions::default()).unwrap();
        let permissions = Arc::new(PermissionState::new(&paths, &loaded).unwrap());
        let services = Arc::new(Services::new());
        services.insert(permissions.clone());
        services.insert(Arc::new(SkillGrants::new(trusted)));
        let dir = write_skill(
            &project.join(".azazel/skills"),
            "deploy",
            "name: deploy\ndescription: ship it\nallowed-tools: Bash(git status:*), Read, not a rule(",
            "Deploy.",
        );
        let text = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
        let skill = parse_skill(&dir.join("SKILL.md"), &text, source).unwrap();
        Granting {
            _tmp: tmp,
            services,
            permissions,
            skill,
        }
    }

    impl Granting {
        /// The allow rules skills have added, as `rule (source)`.
        fn granted(&self) -> Vec<String> {
            self.permissions
                .rules()
                .iter()
                .filter(|rule| rule.source.starts_with("skill "))
                .map(|rule| format!("{} ({})", rule.rule, rule.source))
                .collect()
        }

        fn allow(&self, agent: &str, next_prompt: bool) {
            let grants = self.services.get::<SkillGrants>().unwrap();
            grants.allow(&self.services, &self.skill, agent, next_prompt);
        }

        async fn happens(&self, mut event: Event) {
            let ctx = ExtCtx {
                services: self.services.clone(),
                cwd: PathBuf::from("."),
                session_id: "s".into(),
            };
            GrantsHandler.handle(&mut event, &ctx).await;
        }
    }

    fn agent(id: &str) -> AgentInfo {
        AgentInfo {
            id: id.into(),
            def: id.into(),
            parent: None,
            depth: 0,
            session_id: "s".into(),
        }
    }

    fn prompt() -> Event {
        Event::UserPrompt {
            text: "go".into(),
            images: Vec::new(),
        }
    }

    #[tokio::test]
    async fn what_a_loaded_skill_allows_lasts_until_the_agents_run_ends() {
        let f = granting(SkillSource::Home, false);
        // Claude Code tool names are mapped; what is not a rule is left out.
        assert_eq!(
            allowed_rules(&f.skill).unwrap(),
            [
                "shell(git status:*)".parse::<Rule>().unwrap(),
                "read".parse().unwrap()
            ]
        );

        let mut mistyped = f.skill.clone();
        mistyped.allowed_tools = Some(vec!["shell(".to_string()]);
        assert_eq!(allowed_rules(&mistyped), Some(Vec::new()));
        mistyped.allowed_tools = Some(Vec::new());
        assert_eq!(allowed_rules(&mistyped), None);

        f.allow("lead", false);
        f.allow("lead", false);
        f.allow("coder-1", false);
        assert_eq!(
            f.granted(),
            [
                "shell(git status:*) (skill deploy, while lead works)",
                "read (skill deploy, while lead works)",
                "shell(git status:*) (skill deploy, while coder-1 works)",
                "read (skill deploy, while coder-1 works)",
            ]
        );

        // The sub-agent finishing ends its own grants only; the lead stopping ends the lead's.
        f.happens(Event::AgentEnd {
            agent: agent("coder-1"),
            status: "done".into(),
        })
        .await;
        assert_eq!(f.granted().len(), 2);
        f.happens(Event::Stop {
            agent: agent("lead"),
            stop_hook_active: false,
        })
        .await;
        assert_eq!(f.granted(), [] as [String; 0]);
    }

    #[tokio::test]
    async fn a_skill_the_user_invokes_allows_for_the_prompt_that_follows_and_no_longer() {
        let f = granting(SkillSource::Home, false);
        // A grant left behind by a run that was interrupted ends with the next prompt.
        f.allow("lead", false);
        f.happens(prompt()).await;
        assert_eq!(f.granted(), [] as [String; 0]);

        allow_for_prompt(&f.services, &f.skill);
        f.happens(prompt()).await;
        assert_eq!(f.granted().len(), 2);
        f.happens(prompt()).await;
        assert_eq!(f.granted(), [] as [String; 0]);

        f.allow("lead", false);
        f.happens(Event::SessionEnd).await;
        assert_eq!(f.granted(), [] as [String; 0]);
    }

    #[tokio::test]
    async fn a_projects_own_skill_allows_nothing_until_the_project_is_trusted() {
        let untrusted = granting(SkillSource::Project, false);
        untrusted.allow("lead", false);
        assert_eq!(untrusted.granted(), [] as [String; 0]);
        // A `.claude/skills`-style skill counts as the project's when it lives inside it.
        let compat = granting(SkillSource::Compat, false);
        compat.allow("lead", false);
        assert_eq!(compat.granted(), [] as [String; 0]);

        let trusted = granting(SkillSource::Project, true);
        trusted.allow("lead", false);
        assert_eq!(trusted.granted().len(), 2);
    }
}
