//! Agent definitions: Markdown files whose frontmatter describes a specialist (tools, model, limits)
//! and whose body is its system prompt. A definition is a sub-agent the team starts, a primary
//! agent the lead can be ([`LeadAgent`]), or both.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use azazel_ai::{ModelSpec, ThinkingLevel};
use serde_json::{Map, Value};

use crate::config::AgentOverride;
use crate::frontmatter;
use crate::paths::Paths;
use crate::permissions::Rule;

const MAX_NAME_CHARS: usize = 64;

/// Where a definition comes from. Precedence is decided by the order of the roots given to
/// [`discover`], not by this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefSource {
    Builtin,
    Package,
    Home,
    /// `<project>/.claude/agents`: files written for Claude Code.
    Compat,
    Project,
}

impl DefSource {
    pub fn label(&self) -> &'static str {
        match self {
            DefSource::Builtin => "built-in",
            DefSource::Package => "package",
            DefSource::Home => "user",
            DefSource::Compat => "compat",
            DefSource::Project => "project",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Isolation {
    /// The agent works in the shared tree.
    #[default]
    None,
    /// The agent works in its own git worktree.
    Worktree,
}

/// What a definition may be used as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentMode {
    /// An agent the team starts with `task` (the default).
    #[default]
    Subagent,
    /// A persona of the lead, which the user switches to.
    Primary,
    /// Both.
    All,
}

impl AgentMode {
    /// Whether the team may start it as a sub-agent.
    pub fn subagent(self) -> bool {
        self != AgentMode::Primary
    }

    /// Whether the lead can be it.
    pub fn primary(self) -> bool {
        self != AgentMode::Subagent
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// `None`: every registered tool except the orchestration tools. An entry with a pattern
    /// grants the tool and restricts this agent's use of it to that pattern.
    pub tools: Option<Vec<Rule>>,
    pub disallowed_tools: Vec<String>,
    /// The fallback chain; never empty when present.
    pub model: Option<Vec<ModelSpec>>,
    pub thinking: Option<ThinkingLevel>,
    /// `permission-mode: plan`: the agent works read-only whatever the session mode is.
    pub plan_mode: bool,
    pub mode: AgentMode,
    /// `#rrggbb`: the colour the UI shows the agent in.
    pub color: Option<String>,
    pub max_turns: Option<u32>,
    pub skills: Vec<String>,
    /// Definitions this agent may spawn itself.
    pub spawns: Vec<String>,
    pub isolation: Isolation,
    /// `output-schema`: a JSON Schema the agent's result must match. The agent then hands in a
    /// value through `report` instead of prose.
    pub output_schema: Option<Value>,
    /// The system prompt.
    pub body: String,
    pub source: DefSource,
    /// The file the definition was read from; `None` for built-ins.
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidDef {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct DefSet {
    defs: BTreeMap<String, Arc<AgentDef>>,
    invalid: Vec<InvalidDef>,
}

/// The definitions of a session by name, with the files that failed to parse. Shared by
/// everything that starts agents, so that reading the files again ([`AgentDefs::replace`])
/// reaches all of them.
#[derive(Debug, Default)]
pub struct AgentDefs {
    set: RwLock<DefSet>,
}

impl Clone for AgentDefs {
    fn clone(&self) -> Self {
        AgentDefs {
            set: RwLock::new(self.read().clone()),
        }
    }
}

impl PartialEq for AgentDefs {
    fn eq(&self, other: &Self) -> bool {
        *self.read() == *other.read()
    }
}

impl AgentDefs {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, DefSet> {
        self.set.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_mut(&mut self) -> &mut DefSet {
        self.set.get_mut().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn get(&self, name: &str) -> Option<Arc<AgentDef>> {
        self.read().defs.get(name).cloned()
    }

    /// Sorted by name.
    pub fn list(&self) -> Vec<Arc<AgentDef>> {
        self.read().defs.values().cloned().collect()
    }

    /// The definition `name` when the team may start it as a sub-agent.
    pub fn subagent(&self, name: &str) -> Option<Arc<AgentDef>> {
        self.get(name).filter(|def| def.mode.subagent())
    }

    /// The definitions the team may start, sorted by name.
    pub fn subagents(&self) -> Vec<Arc<AgentDef>> {
        self.list()
            .into_iter()
            .filter(|def| def.mode.subagent())
            .collect()
    }

    /// The definition `name` when the lead can be it.
    pub fn primary(&self, name: &str) -> Option<Arc<AgentDef>> {
        self.get(name).filter(|def| def.mode.primary())
    }

    /// The definitions the lead can be, in the order `tab` steps through them: `build` first,
    /// then the others by name.
    pub fn primaries(&self) -> Vec<Arc<AgentDef>> {
        let mut primaries: Vec<Arc<AgentDef>> =
            self.list().into_iter().filter(|def| def.mode.primary()).collect();
        primaries.sort_by_key(|def| def.name != BUILD);
        primaries
    }

    pub fn invalid(&self) -> Vec<InvalidDef> {
        self.read().invalid.clone()
    }

    /// Takes over the definitions of `other` (the files read again). Agents already running keep
    /// the definition they started with.
    pub fn replace(&self, other: AgentDefs) {
        let other = other.set.into_inner().unwrap_or_else(PoisonError::into_inner);
        *self.set.write().unwrap_or_else(PoisonError::into_inner) = other;
    }

    /// Applies `[agents.<name>]` config overrides (`model`, `thinking`) to the definitions that exist.
    /// An empty `model` override removes the definition's own chain.
    pub fn apply_overrides(&mut self, overrides: &BTreeMap<String, AgentOverride>) {
        for (name, over) in overrides {
            let Some(def) = self.set_mut().defs.get_mut(name) else {
                continue;
            };
            if over.model.is_none() && over.thinking.is_none() {
                continue;
            }
            let def = Arc::make_mut(def);
            if let Some(chain) = &over.model {
                def.model = Some(chain.clone()).filter(|chain| !chain.is_empty());
            }
            if let Some(level) = over.thinking {
                def.thinking = Some(level);
            }
        }
    }

    fn load(&mut self, file: &Path, text: &str, source: DefSource) {
        match parse_def(file, text, source) {
            Ok(def) => {
                self.set_mut().defs.insert(def.name.clone(), Arc::new(def));
            }
            Err(error) => self.set_mut().invalid.push(InvalidDef {
                path: file.to_path_buf(),
                error,
            }),
        }
    }
}

fn valid_name(name: &str) -> bool {
    name.chars().count() <= MAX_NAME_CHARS
        && name.split('-').all(|part| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

/// The text of a scalar: a string trimmed, a number or boolean as written.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A value as quoted in an error message.
fn shown(value: &Value) -> String {
    scalar(value).unwrap_or_else(|| value.to_string())
}

/// The first of `keys` present in the frontmatter, with the name it was written under.
fn field<'a>(meta: &'a Map<String, Value>, keys: &[&'static str]) -> Option<(&'static str, &'a Value)> {
    keys.iter().find_map(|k| meta.get(*k).map(|v| (*k, v)))
}

/// The entries of a list-valued key: a YAML list of scalars, or one string split on the commas
/// that are not inside parentheses. Empty entries are dropped; an empty value is an empty list.
fn entries(key: &str, value: &Value) -> Result<Vec<String>, String> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::String(_) => Ok(frontmatter::string_or_list(Some(value)).unwrap_or_default()),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let entry = scalar(item)
                    .ok_or_else(|| format!("{key}: every entry must be a string, got `{item}`"))?;
                if !entry.is_empty() {
                    out.push(entry);
                }
            }
            Ok(out)
        }
        other => Err(format!(
            "{key}: expected a string or a list of strings, got `{}`",
            shown(other)
        )),
    }
}

/// A result schema: a mapping, or a string holding one as JSON. An empty value is no schema.
fn result_schema(key: &str, value: &Value) -> Result<Option<Value>, String> {
    let schema = match value {
        Value::Null => return Ok(None),
        Value::String(text) if text.trim().is_empty() => return Ok(None),
        Value::String(text) => {
            serde_json::from_str::<Value>(text).map_err(|e| format!("{key}: not valid JSON: {e}"))?
        }
        other => other.clone(),
    };
    if !schema.is_object() {
        return Err(format!(
            "{key}: expected a JSON Schema (a mapping), got `{}`",
            shown(&schema)
        ));
    }
    Ok(Some(schema))
}

/// The azazel tool a Claude Code tool name stands for; other names are kept as written.
pub fn compat_tool(name: &str) -> &str {
    match name {
        "Read" => "read",
        "Write" => "write",
        "Edit" | "MultiEdit" => "edit",
        "Bash" => "shell",
        "Glob" => "glob",
        "Grep" => "grep",
        "WebFetch" => "web_fetch",
        "WebSearch" => "web_search",
        "Task" => "task",
        "TodoWrite" => "todo",
        other => other,
    }
}

/// The role a Claude Code model alias stands for; other values are kept as written.
pub fn compat_model(name: &str) -> &str {
    match name {
        "haiku" => "@smol",
        "sonnet" => "@default",
        "opus" => "@slow",
        other => other,
    }
}

fn tool_rules(key: &str, value: &Value, source: DefSource) -> Result<Vec<Rule>, String> {
    let mut rules: Vec<Rule> = Vec::new();
    for entry in entries(key, value)? {
        let Ok(mut rule) = entry.parse::<Rule>() else {
            return Err(format!(
                "{key}: invalid entry `{entry}`: expected `tool` or `tool(pattern)`"
            ));
        };
        if source == DefSource::Compat {
            rule.tool = compat_tool(&rule.tool).to_string();
        }
        if rule.tool == "run_code" {
            tracing::warn!("{key}: `run_code` is now `eval(luau)`; change it in the agent file");
            rule.tool = "eval".to_string();
            rule.pattern.get_or_insert_with(|| "luau".to_string());
        }
        if !rules.contains(&rule) {
            rules.push(rule);
        }
    }
    Ok(rules)
}

fn tool_names(key: &str, value: &Value, source: DefSource) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = Vec::new();
    for entry in entries(key, value)? {
        let Ok(Rule { tool, pattern: None }) = entry.parse::<Rule>() else {
            return Err(format!(
                "{key}: invalid entry `{entry}`: expected a tool name without a pattern"
            ));
        };
        let name = match source {
            DefSource::Compat => compat_tool(&tool).to_string(),
            _ => tool,
        };
        let name = if name == "run_code" {
            tracing::warn!("{key}: `run_code` is now `eval`; change it in the agent file");
            "eval".to_string()
        } else {
            name
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Ok(names)
}

fn model_chain(key: &str, value: &Value, source: DefSource) -> Result<Option<Vec<ModelSpec>>, String> {
    let mut chain = Vec::new();
    for entry in entries(key, value)? {
        let written = match source {
            DefSource::Compat if entry == "inherit" => continue,
            DefSource::Compat => compat_model(&entry),
            _ => entry.as_str(),
        };
        let Ok(spec) = written.parse::<ModelSpec>() else {
            return Err(format!(
                "{key}: invalid entry `{entry}`: expected `provider/model[:thinking]` or `@role`"
            ));
        };
        chain.push(spec);
    }
    Ok(Some(chain).filter(|chain| !chain.is_empty()))
}

fn thinking_level(key: &str, value: &Value) -> Result<Option<ThinkingLevel>, String> {
    match value {
        Value::Null => Ok(None),
        // Some YAML dialects read a bare `off` as `false`.
        Value::Bool(false) => Ok(Some(ThinkingLevel::Off)),
        other => {
            let text = shown(other);
            match text.parse::<ThinkingLevel>() {
                Ok(level) => Ok(Some(level)),
                Err(_) => {
                    let levels = ThinkingLevel::ALL.map(ThinkingLevel::as_str).join(", ");
                    Err(format!("{key}: unknown level `{text}`; expected one of {levels}"))
                }
            }
        }
    }
}

/// A key whose value is one of two words, the first being the default.
fn choice(key: &str, value: &Value, default: &str, other: &str) -> Result<bool, String> {
    if value.is_null() {
        return Ok(false);
    }
    let text = shown(value);
    if text == default {
        Ok(false)
    } else if text == other {
        Ok(true)
    } else {
        Err(format!("{key}: expected `{default}` or `{other}`, got `{text}`"))
    }
}

fn agent_mode(key: &str, value: &Value) -> Result<AgentMode, String> {
    match value {
        Value::Null => Ok(AgentMode::Subagent),
        other => match shown(other).as_str() {
            "subagent" => Ok(AgentMode::Subagent),
            "primary" => Ok(AgentMode::Primary),
            "all" => Ok(AgentMode::All),
            // What `mode` meant before primary agents: the key for it is `permission-mode` now.
            text @ ("plan" | "inherit") => Err(format!(
                "{key}: expected `subagent`, `primary` or `all`; `{text}` is a value of `permission-mode`"
            )),
            text => Err(format!(
                "{key}: expected `subagent`, `primary` or `all`, got `{text}`"
            )),
        },
    }
}

/// The colours a definition can name, besides `#rrggbb`: Claude Code's and the usual ones.
const COLORS: [(&str, &str); 11] = [
    ("red", "#e5534b"),
    ("orange", "#e8913a"),
    ("yellow", "#d4b02f"),
    ("green", "#57ab5a"),
    ("cyan", "#39c5cf"),
    ("blue", "#539bf5"),
    ("purple", "#a371f7"),
    ("magenta", "#c061cb"),
    ("pink", "#e275ad"),
    ("gray", "#8b949e"),
    ("grey", "#8b949e"),
];

/// `#rrggbb`, lowercased, or the value of a colour name.
fn agent_color(key: &str, value: &Value) -> Result<Option<String>, String> {
    let Some(text) = scalar(value).filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    let lower = text.to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix('#')
        && hex.len() == 6
        && hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Ok(Some(lower));
    }
    match COLORS.iter().find(|(name, _)| *name == lower) {
        Some((_, hex)) => Ok(Some((*hex).to_string())),
        None => {
            let names: Vec<&str> = COLORS.iter().map(|(name, _)| *name).collect();
            Err(format!(
                "{key}: expected `#rrggbb` or one of {}, got `{text}`",
                names.join(", ")
            ))
        }
    }
}

fn turn_limit(key: &str, value: &Value) -> Result<Option<u32>, String> {
    let turns = match value {
        Value::Null => return Ok(None),
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        Value::String(s) => s.trim().parse::<u32>().ok(),
        _ => None,
    };
    match turns {
        Some(turns) if turns > 0 => Ok(Some(turns)),
        _ => Err(format!(
            "{key}: expected a positive integer, got `{}`",
            shown(value)
        )),
    }
}

/// Parses one definition. `file` is where the text came from: it becomes `AgentDef::path` (not
/// for built-ins) and identifies the definition in reports. Every error names the frontmatter key
/// and the value that was rejected.
///
/// Definitions from [`DefSource::Compat`] are read as Claude Code wrote them: its tool names and
/// model aliases are mapped to their azazel counterparts.
pub fn parse_def(file: &Path, text: &str, source: DefSource) -> Result<AgentDef, String> {
    let (yaml, body) =
        frontmatter::split(text).ok_or("missing frontmatter: the file must start with `---`")?;
    let meta = frontmatter::parse(yaml)?;

    let name = meta
        .get("name")
        .and_then(scalar)
        .filter(|name| !name.is_empty())
        .ok_or("name: missing")?;
    if !valid_name(&name) {
        return Err(format!(
            "name: `{name}` must be lowercase words joined by hyphens, at most {MAX_NAME_CHARS} characters"
        ));
    }
    let description = meta
        .get("description")
        .and_then(scalar)
        .filter(|description| !description.is_empty())
        .ok_or("description: missing or empty")?;

    let tools = field(&meta, &["tools"])
        .map(|(key, value)| tool_rules(key, value, source))
        .transpose()?;
    let disallowed_tools = field(&meta, &["disallowed-tools", "disallowedTools"])
        .map(|(key, value)| tool_names(key, value, source))
        .transpose()?
        .unwrap_or_default();
    let model = field(&meta, &["model"])
        .map(|(key, value)| model_chain(key, value, source))
        .transpose()?
        .flatten();
    let thinking = field(&meta, &["thinking"])
        .map(|(key, value)| thinking_level(key, value))
        .transpose()?
        .flatten();
    let plan_mode = match field(&meta, &["permission-mode", "permissionMode"]) {
        None => false,
        // Claude Code's other modes have no counterpart here: such an agent follows the session.
        Some((_, value)) if source == DefSource::Compat => value.as_str() == Some("plan"),
        Some((key, value)) => choice(key, value, "inherit", "plan")?,
    };
    let mode = field(&meta, &["mode"])
        .map(|(key, value)| agent_mode(key, value))
        .transpose()?
        .unwrap_or_default();
    let color = match field(&meta, &["color"]) {
        None => None,
        // A colour Claude Code knows and azazel does not is left to the theme.
        Some((key, value)) if source == DefSource::Compat => agent_color(key, value).ok().flatten(),
        Some((key, value)) => agent_color(key, value)?,
    };
    let max_turns = field(&meta, &["max-turns", "maxTurns"])
        .map(|(key, value)| turn_limit(key, value))
        .transpose()?
        .flatten();
    let skills = field(&meta, &["skills"])
        .map(|(key, value)| entries(key, value))
        .transpose()?
        .unwrap_or_default();
    let spawns = field(&meta, &["spawns"])
        .map(|(key, value)| entries(key, value))
        .transpose()?
        .unwrap_or_default();
    let worktree = field(&meta, &["isolation"])
        .map(|(key, value)| choice(key, value, "none", "worktree"))
        .transpose()?
        .unwrap_or(false);
    let output_schema = field(&meta, &["output-schema", "outputSchema"])
        .map(|(key, value)| result_schema(key, value))
        .transpose()?
        .flatten();

    Ok(AgentDef {
        name,
        description,
        tools,
        disallowed_tools,
        model,
        thinking,
        plan_mode,
        mode,
        color,
        max_turns,
        skills,
        spawns,
        isolation: if worktree {
            Isolation::Worktree
        } else {
            Isolation::None
        },
        output_schema,
        body: body.trim().to_string(),
        source,
        path: (source != DefSource::Builtin).then(|| file.to_path_buf()),
    })
}

/// Directories holding definitions besides the built-in ones, lowest precedence first: those of
/// installed packages (Package), `~/.azazel/agents` (Home), `<project>/.claude/agents` (Compat;
/// only when `claude_compat`), `<project>/.azazel/agents` (Project).
pub fn def_roots(
    paths: &Paths,
    claude_compat: bool,
    packages: &crate::packages::Loaded,
) -> Vec<(PathBuf, DefSource)> {
    let mut roots: Vec<(PathBuf, DefSource)> = packages
        .agent_roots
        .iter()
        .map(|dir| (dir.clone(), DefSource::Package))
        .collect();
    roots.push((paths.home.join("agents"), DefSource::Home));
    if claude_compat {
        roots.push((
            paths.project_root.join(".claude").join("agents"),
            DefSource::Compat,
        ));
    }
    roots.push((paths.project_dir().join("agents"), DefSource::Project));
    roots
}

/// Every `*.md` below `dir` in sorted order. Symlinked directories are not followed.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut found: Vec<(PathBuf, bool)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| (entry.path(), entry.file_type().is_ok_and(|kind| kind.is_dir())))
        .collect();
    found.sort();
    for (path, is_dir) in found {
        if is_dir {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("md")) {
            out.push(path);
        }
    }
}

/// Built-ins (`(file name, text)` pairs) first, then every `*.md` under each root (recursively, in
/// sorted order). A later definition replaces an earlier one with the same `name`. Files that do
/// not parse are reported in `invalid`, never dropped silently; a built-in that does not parse is
/// reported with its file name as the path.
pub fn discover(builtin: &[(&str, &str)], roots: &[(PathBuf, DefSource)]) -> AgentDefs {
    let mut defs = AgentDefs::default();
    for (file, text) in builtin {
        defs.load(Path::new(file), text, DefSource::Builtin);
    }
    for (root, source) in roots {
        let mut files = Vec::new();
        collect(root, &mut files);
        for file in files {
            match std::fs::read_to_string(&file) {
                Ok(text) => defs.load(&file, &text, *source),
                Err(e) => defs.set_mut().invalid.push(InvalidDef {
                    path: file,
                    error: format!("cannot read the file: {e}"),
                }),
            }
        }
    }
    defs
}

/// The primary agent a session starts with when `[agents] default` names no other.
pub const BUILD: &str = "build";

type LeadWatcher = Arc<dyn Fn(&Arc<AgentDef>) + Send + Sync>;

/// Why `name` cannot be the lead, with the agents that can.
pub fn not_primary(defs: &AgentDefs, name: &str) -> String {
    let primaries: Vec<String> = defs.primaries().iter().map(|def| def.name.clone()).collect();
    let primaries = match primaries.as_slice() {
        [] => "there are none".to_string(),
        names => format!("the primary agents are: {}", names.join(", ")),
    };
    match defs.get(name) {
        Some(_) => format!("`{name}` is a sub-agent, not a primary agent; {primaries}"),
        None => format!("there is no agent `{name}`; {primaries}"),
    }
}

/// The primary agent the lead is: the definition whose body is the lead's identity and whose
/// model, thinking level, tools, limits, skills and colour apply to the lead while it is
/// selected. Registered as a service; the prompt, the lead's tools and the UI read it.
pub struct LeadAgent {
    defs: Arc<AgentDefs>,
    default: String,
    current: RwLock<String>,
    watchers: RwLock<Vec<LeadWatcher>>,
}

impl LeadAgent {
    /// The lead starts as `default`; `Err` when that is not a primary agent.
    pub fn new(defs: Arc<AgentDefs>, default: &str) -> Result<LeadAgent, String> {
        if defs.primary(default).is_none() {
            return Err(not_primary(&defs, default));
        }
        Ok(LeadAgent {
            defs,
            default: default.to_string(),
            current: RwLock::new(default.to_string()),
            watchers: RwLock::default(),
        })
    }

    /// The agent a session starts as (`[agents] default`).
    pub fn default_name(&self) -> &str {
        &self.default
    }

    pub fn name(&self) -> String {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The definition of the agent the lead is; `None` when the files were read again without it.
    pub fn def(&self) -> Option<Arc<AgentDef>> {
        self.defs.primary(&self.name())
    }

    /// The agents the lead can be, in the order `tab` steps through them.
    pub fn primaries(&self) -> Vec<Arc<AgentDef>> {
        self.defs.primaries()
    }

    /// Makes the lead `name` and tells the watchers when that changed it. Returns whether it did.
    pub fn set(&self, name: &str) -> Result<bool, String> {
        let def = self
            .defs
            .primary(name)
            .ok_or_else(|| not_primary(&self.defs, name))?;
        let previous = std::mem::replace(
            &mut *self.current.write().unwrap_or_else(PoisonError::into_inner),
            name.to_string(),
        );
        if previous == name {
            return Ok(false);
        }
        let watchers = self
            .watchers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for watcher in watchers {
            watcher(&def);
        }
        Ok(true)
    }

    /// Puts the lead back to the agent a session recorded, without telling the watchers: nothing
    /// changed that would need recording again.
    pub fn restore(&self, name: &str) -> Result<Arc<AgentDef>, String> {
        let def = self
            .defs
            .primary(name)
            .ok_or_else(|| not_primary(&self.defs, name))?;
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = name.to_string();
        Ok(def)
    }

    /// The primary agent after the one the lead is (before it, when `forward` is false), going
    /// round; `None` when there is no other.
    pub fn next(&self, forward: bool) -> Option<String> {
        let names: Vec<String> = self.primaries().iter().map(|def| def.name.clone()).collect();
        let current = self.name();
        let at = names.iter().position(|name| *name == current);
        let next = match (at, forward) {
            (None, _) => 0,
            (Some(at), true) => (at + 1) % names.len(),
            (Some(at), false) => (at + names.len() - 1) % names.len(),
        };
        names.get(next).filter(|name| **name != current).cloned()
    }

    /// `watcher` is called with the new definition after every change of the agent.
    pub fn on_change(&self, watcher: LeadWatcher) {
        self.watchers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(watcher);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVIEWER: &str = "---
name: reviewer
description: Reviews a diff for correctness, security and maintainability. Use after code changes.
tools: read, grep, glob, shell(git diff:*), shell(git log:*), shell(git show:*)
disallowed-tools: write, edit
model: [\"@slow\", \"@default\"]
thinking: high
permission-mode: plan
color: purple
max-turns: 40
skills: [code-review]
spawns: []
isolation: none
---
System prompt body.
";

    fn rule(text: &str) -> Rule {
        text.parse().unwrap()
    }

    fn spec(text: &str) -> ModelSpec {
        text.parse().unwrap()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    /// A definition named `agent` with the given extra frontmatter lines.
    fn parse(extra: &str, source: DefSource) -> Result<AgentDef, String> {
        let text = format!("---\nname: agent\ndescription: does things\n{extra}\n---\nBody");
        parse_def(Path::new("agent.md"), &text, source)
    }

    fn write_def(dir: &Path, file: &str, name: &str, about: &str) {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text = format!("---\nname: {name}\ndescription: {about}\n---\n{about} body");
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn the_documented_example_parses_to_every_field() {
        let file = Path::new("agents/review.md");
        let description =
            "Reviews a diff for correctness, security and maintainability. Use after code changes.";
        let def = parse_def(file, REVIEWER, DefSource::Project).unwrap();
        assert_eq!(
            def,
            AgentDef {
                name: "reviewer".into(),
                description: description.into(),
                tools: Some(vec![
                    rule("read"),
                    rule("grep"),
                    rule("glob"),
                    rule("shell(git diff:*)"),
                    rule("shell(git log:*)"),
                    rule("shell(git show:*)"),
                ]),
                disallowed_tools: strings(&["write", "edit"]),
                model: Some(vec![spec("@slow"), spec("@default")]),
                thinking: Some(ThinkingLevel::High),
                plan_mode: true,
                mode: AgentMode::Subagent,
                color: Some("#a371f7".into()),
                max_turns: Some(40),
                skills: strings(&["code-review"]),
                spawns: Vec::new(),
                isolation: Isolation::None,
                output_schema: None,
                body: "System prompt body.".into(),
                source: DefSource::Project,
                path: Some(file.to_path_buf()),
            }
        );
        assert_eq!(parse_def(file, REVIEWER, DefSource::Builtin).unwrap().path, None);
    }

    #[test]
    fn an_output_schema_is_a_mapping_or_json_text() {
        let nested = parse(
            "output-schema:\n  type: object\n  properties:\n    files:\n      type: array\n  required: [files]",
            DefSource::Home,
        )
        .unwrap();
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "files": { "type": "array" } },
            "required": ["files"]
        });
        assert_eq!(nested.output_schema, Some(schema.clone()));
        let text = parse(&format!("outputSchema: '{schema}'"), DefSource::Home).unwrap();
        assert_eq!(text.output_schema, Some(schema));
        assert_eq!(
            parse("output-schema:", DefSource::Home).unwrap().output_schema,
            None
        );

        assert_eq!(
            parse("output-schema: [1, 2]", DefSource::Home),
            Err("output-schema: expected a JSON Schema (a mapping), got `[1,2]`".to_string())
        );
        assert!(
            parse("output-schema: '{not json'", DefSource::Home)
                .is_err_and(|e| e.starts_with("output-schema: not valid JSON: "))
        );
    }

    #[test]
    fn validation_failures_name_the_key_and_the_value() {
        let file = Path::new("x.md");
        let raw = |text: &str| parse_def(file, text, DefSource::Project).unwrap_err();
        assert_eq!(
            raw("# Reviewer\nNo frontmatter here.\n"),
            "missing frontmatter: the file must start with `---`"
        );
        assert_eq!(raw("---\ndescription: x\n---\n"), "name: missing");
        assert_eq!(
            raw("---\nname: Bad_Name\ndescription: x\n---\n"),
            "name: `Bad_Name` must be lowercase words joined by hyphens, at most 64 characters"
        );
        let long = "n".repeat(65);
        for name in ["-lead", "trail-", "two--dashes", "has space", long.as_str()] {
            let error = raw(&format!("---\nname: \"{name}\"\ndescription: x\n---\n"));
            assert!(error.starts_with(&format!("name: `{name}` must be")), "{error}");
        }
        let longest = "n".repeat(64);
        let text = format!("---\nname: {longest}\ndescription: x\n---\n");
        assert_eq!(parse_def(file, &text, DefSource::Project).unwrap().name, longest);
        let duplicate = raw("---\nname: a\nname: b\ndescription: x\n---\n");
        assert!(duplicate.contains("invalid frontmatter"), "{duplicate}");
        assert_eq!(
            raw("---\nname: ok\ndescription: \"   \"\n---\n"),
            "description: missing or empty"
        );
        assert_eq!(raw("---\nname: ok\n---\n"), "description: missing or empty");

        let extra = |lines: &str| parse(lines, DefSource::Project).unwrap_err();
        assert_eq!(
            extra("tools: read, shell("),
            "tools: invalid entry `shell(`: expected `tool` or `tool(pattern)`"
        );
        assert_eq!(
            extra("tools: [read, \"bad name\"]"),
            "tools: invalid entry `bad name`: expected `tool` or `tool(pattern)`"
        );
        assert_eq!(
            extra("tools: {read: true}"),
            "tools: expected a string or a list of strings, got `{\"read\":true}`"
        );
        assert_eq!(
            extra("disallowed-tools: write, shell(rm:*)"),
            "disallowed-tools: invalid entry `shell(rm:*)`: expected a tool name without a pattern"
        );
        assert_eq!(
            extra("model: [\"@slow\", nonsense]"),
            "model: invalid entry `nonsense`: expected `provider/model[:thinking]` or `@role`"
        );
        assert_eq!(
            extra("thinking: loud"),
            "thinking: unknown level `loud`; expected one of off, minimal, low, medium, high, xhigh, max"
        );
        assert_eq!(
            extra("permission-mode: yolo"),
            "permission-mode: expected `inherit` or `plan`, got `yolo`"
        );
        assert_eq!(
            extra("mode: yolo"),
            "mode: expected `subagent`, `primary` or `all`, got `yolo`"
        );
        assert_eq!(
            extra("mode: plan"),
            "mode: expected `subagent`, `primary` or `all`; `plan` is a value of `permission-mode`"
        );
        assert_eq!(
            extra("color: chartreuse"),
            "color: expected `#rrggbb` or one of red, orange, yellow, green, cyan, blue, purple, magenta, \
             pink, gray, grey, got `chartreuse`"
        );
        assert_eq!(
            extra("color: \"#12345\""),
            "color: expected `#rrggbb` or one of red, orange, yellow, green, cyan, blue, purple, magenta, \
             pink, gray, grey, got `#12345`"
        );
        assert_eq!(
            extra("permissionMode: acceptEdits"),
            "permissionMode: expected `inherit` or `plan`, got `acceptEdits`"
        );
        assert_eq!(
            extra("max-turns: 0"),
            "max-turns: expected a positive integer, got `0`"
        );
        assert_eq!(
            extra("maxTurns: -3"),
            "maxTurns: expected a positive integer, got `-3`"
        );
        assert_eq!(
            extra("max-turns: many"),
            "max-turns: expected a positive integer, got `many`"
        );
        assert_eq!(
            extra("max-turns: 4294967296"),
            "max-turns: expected a positive integer, got `4294967296`"
        );
        assert_eq!(
            extra("isolation: container"),
            "isolation: expected `none` or `worktree`, got `container`"
        );
        assert_eq!(
            extra("skills: [[nested]]"),
            "skills: every entry must be a string, got `[\"nested\"]`"
        );
    }

    #[test]
    fn an_absent_tools_key_differs_from_an_empty_one() {
        assert_eq!(parse("", DefSource::Project).unwrap().tools, None);
        for empty in ["tools: []", "tools: \"\"", "tools:"] {
            assert_eq!(
                parse(empty, DefSource::Project).unwrap().tools,
                Some(Vec::new()),
                "{empty}"
            );
        }
        // Other empty values fall back to their defaults.
        let def = parse(
            "model: []\nthinking:\nmode:\npermission-mode:\ncolor:\nmax-turns:\nskills:\nisolation:",
            DefSource::Project,
        )
        .unwrap();
        assert_eq!(
            (
                def.model,
                def.thinking,
                def.plan_mode,
                def.mode,
                def.color,
                def.max_turns,
                def.isolation
            ),
            (
                None,
                None,
                false,
                AgentMode::Subagent,
                None,
                None,
                Isolation::None
            )
        );
        assert!(def.skills.is_empty());
    }

    #[test]
    fn commas_inside_parentheses_do_not_split_tool_entries() {
        let def = parse(
            "tools: read, shell(git log -1, x), write(.azazel/plans/**) ,grep",
            DefSource::Project,
        )
        .unwrap();
        let granted: Vec<(String, Option<String>)> = def
            .tools
            .unwrap()
            .into_iter()
            .map(|r| (r.tool, r.pattern))
            .collect();
        assert_eq!(
            granted,
            [
                ("read".to_string(), None),
                ("shell".to_string(), Some("git log -1, x".to_string())),
                ("write".to_string(), Some(".azazel/plans/**".to_string())),
                ("grep".to_string(), None),
            ]
        );
    }

    #[test]
    fn camel_case_aliases_are_accepted_and_lose_to_the_kebab_case_key() {
        let aliased = parse(
            "disallowedTools: write\nmaxTurns: 7\npermissionMode: plan",
            DefSource::Project,
        )
        .unwrap();
        assert_eq!(aliased.disallowed_tools, strings(&["write"]));
        assert_eq!(aliased.max_turns, Some(7));
        assert!(aliased.plan_mode);

        let both = parse(
            "disallowedTools: write\ndisallowed-tools: edit\nmaxTurns: 7\nmax-turns: 9\n\
             permissionMode: plan\npermission-mode: inherit",
            DefSource::Project,
        )
        .unwrap();
        assert_eq!(both.disallowed_tools, strings(&["edit"]));
        assert_eq!(both.max_turns, Some(9));
        assert!(!both.plan_mode);
    }

    #[test]
    fn compat_definitions_map_claude_tool_names_and_models() {
        let def = parse(
            "tools: Read, Bash(git diff:*), MultiEdit, Edit, WebFetch, WebSearch, Glob, Grep, Task, \
             TodoWrite, NotebookEdit, mcp__github__*\n\
             disallowedTools: Write, Edit, MultiEdit, Agent\nmodel: sonnet",
            DefSource::Compat,
        )
        .unwrap();
        assert_eq!(
            def.tools,
            Some(vec![
                rule("read"),
                rule("shell(git diff:*)"),
                rule("edit"),
                rule("web_fetch"),
                rule("web_search"),
                rule("glob"),
                rule("grep"),
                rule("task"),
                rule("todo"),
                rule("NotebookEdit"),
                rule("mcp__github__*"),
            ])
        );
        assert_eq!(def.disallowed_tools, strings(&["write", "edit", "Agent"]));
        assert_eq!(def.model, Some(vec![spec("@default")]));

        let aliases = [
            ("inherit", None),
            ("haiku", Some("@smol")),
            ("sonnet", Some("@default")),
            ("opus", Some("@slow")),
            ("mock/a", Some("mock/a")),
        ];
        for (written, expected) in aliases {
            let def = parse(&format!("model: {written}"), DefSource::Compat).unwrap();
            assert_eq!(def.model, expected.map(|role| vec![spec(role)]), "{written}");
        }

        // The tool `run_code` is now `eval` with `language: "luau"`.
        let old = parse(
            "tools: read, run_code\ndisallowed-tools: run_code",
            DefSource::Project,
        )
        .unwrap();
        assert_eq!(old.tools, Some(vec![rule("read"), rule("eval(luau)")]));
        assert_eq!(old.disallowed_tools, strings(&["eval"]));

        // Outside compat the same words are ordinary names.
        let native = parse("tools: Read, Bash(ls:*)", DefSource::Project).unwrap();
        assert_eq!(native.tools, Some(vec![rule("Read"), rule("Bash(ls:*)")]));
        for alias in ["inherit", "sonnet"] {
            let error = parse(&format!("model: {alias}"), DefSource::Project).unwrap_err();
            assert!(
                error.starts_with(&format!("model: invalid entry `{alias}`")),
                "{error}"
            );
        }
    }

    #[test]
    fn later_sources_replace_earlier_ones_by_name_not_file_name() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let compat = tmp.path().join("claude");
        let project = tmp.path().join("project");
        let builtin = [
            ("scout.md", "---\nname: scout\ndescription: stock scout\n---\nB"),
            ("coder.md", "---\nname: coder\ndescription: stock coder\n---\nC"),
        ];
        write_def(&home, "explorer.md", "scout", "home scout");
        write_def(&home, "notes.md", "notes", "home notes");
        write_def(&compat, "look-around.md", "scout", "compat scout");
        write_def(&project, "nested/deep/finder.md", "scout", "project scout");
        let roots = [
            (home.clone(), DefSource::Home),
            (compat, DefSource::Compat),
            (project.clone(), DefSource::Project),
        ];

        let winner = |count: usize| {
            let defs = discover(&builtin, &roots[..count]);
            assert!(defs.invalid().is_empty());
            let scout = defs.get("scout").unwrap();
            (scout.description.clone(), scout.source)
        };
        assert_eq!(winner(0), ("stock scout".to_string(), DefSource::Builtin));
        assert_eq!(winner(1), ("home scout".to_string(), DefSource::Home));
        assert_eq!(winner(2), ("compat scout".to_string(), DefSource::Compat));
        assert_eq!(winner(3), ("project scout".to_string(), DefSource::Project));

        let defs = discover(&builtin, &roots);
        let names: Vec<String> = defs.list().iter().map(|def| def.name.clone()).collect();
        assert_eq!(names, ["coder", "notes", "scout"]);
        assert_eq!(
            defs.get("scout").unwrap().path,
            Some(project.join("nested").join("deep").join("finder.md"))
        );
        assert_eq!(defs.get("scout").unwrap().body, "project scout body");
        let coder = defs.get("coder").unwrap();
        assert_eq!((coder.source, coder.path.clone()), (DefSource::Builtin, None));
        assert_eq!(defs.get("notes").unwrap().path, Some(home.join("notes.md")));
        assert!(defs.get("explorer").is_none());
    }

    #[test]
    fn invalid_definitions_are_reported_while_the_valid_ones_load() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(tmp.path(), "good.md", "good", "fine");
        std::fs::write(tmp.path().join("broken.md"), "# Just a prompt\n").unwrap();
        std::fs::write(
            tmp.path().join("limits.md"),
            "---\nname: limits\ndescription: x\nmax-turns: 0\n---\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("binary.md"), [0xff_u8, 0xfe, 0x00]).unwrap();
        let builtin = [
            ("bad.md", "no frontmatter"),
            ("ok.md", "---\nname: ok\ndescription: fine\n---\n"),
        ];

        let defs = discover(&builtin, &[(tmp.path().to_path_buf(), DefSource::Project)]);
        let names: Vec<String> = defs.list().iter().map(|def| def.name.clone()).collect();
        assert_eq!(names, ["good", "ok"]);
        let invalid = defs.invalid();
        assert_eq!(invalid.len(), 4);
        assert_eq!(invalid[0].path, PathBuf::from("bad.md"));
        assert_eq!(
            invalid[0].error,
            "missing frontmatter: the file must start with `---`"
        );
        assert_eq!(invalid[1].path, tmp.path().join("binary.md"));
        assert!(
            invalid[1].error.starts_with("cannot read the file"),
            "{}",
            invalid[1].error
        );
        assert_eq!(invalid[2].path, tmp.path().join("broken.md"));
        assert!(invalid[2].error.starts_with("missing frontmatter"));
        assert_eq!(invalid[3].path, tmp.path().join("limits.md"));
        assert_eq!(
            invalid[3].error,
            "max-turns: expected a positive integer, got `0`"
        );
    }

    #[test]
    fn overrides_change_only_model_and_thinking_of_existing_definitions() {
        let builtin = [
            ("reviewer.md", REVIEWER),
            (
                "coder.md",
                "---\nname: coder\ndescription: writes code\nmodel: \"@default\"\nthinking: low\n---\nC",
            ),
            (
                "scout.md",
                "---\nname: scout\ndescription: looks around\nmodel: \"@smol\"\n---\nS",
            ),
        ];
        let before = discover(&builtin, &[]);
        let mut defs = before.clone();
        let overrides = BTreeMap::from([
            (
                "reviewer".to_string(),
                AgentOverride {
                    model: Some(vec![spec("mock/a"), spec("@default")]),
                    thinking: Some(ThinkingLevel::Low),
                },
            ),
            (
                "coder".to_string(),
                AgentOverride {
                    model: None,
                    thinking: Some(ThinkingLevel::Max),
                },
            ),
            (
                "scout".to_string(),
                AgentOverride {
                    model: Some(Vec::new()),
                    thinking: None,
                },
            ),
            (
                "ghost".to_string(),
                AgentOverride {
                    model: Some(vec![spec("mock/b")]),
                    thinking: Some(ThinkingLevel::High),
                },
            ),
        ]);
        defs.apply_overrides(&overrides);

        let expect = |name: &str, model: Option<Vec<ModelSpec>>, thinking: Option<ThinkingLevel>| {
            let original = before.get(name).unwrap();
            assert_eq!(
                *defs.get(name).unwrap(),
                AgentDef {
                    model,
                    thinking,
                    ..(*original).clone()
                },
                "{name}"
            );
        };
        expect(
            "reviewer",
            Some(vec![spec("mock/a"), spec("@default")]),
            Some(ThinkingLevel::Low),
        );
        expect("coder", Some(vec![spec("@default")]), Some(ThinkingLevel::Max));
        expect("scout", None, None);
        assert!(defs.get("ghost").is_none());
        assert_eq!(defs.list().len(), 3);

        // A definition handed out earlier is not changed behind its holder's back.
        assert_eq!(
            before.get("reviewer").unwrap().thinking,
            Some(ThinkingLevel::High)
        );

        let mut untouched = before.clone();
        untouched.apply_overrides(&BTreeMap::new());
        assert_eq!(untouched, before);
    }

    fn primaries_and_subagents(extra: &[(&str, &str)]) -> Arc<AgentDefs> {
        let mut builtin = vec![
            (
                "build.md",
                "---\nname: build\ndescription: builds\nmode: primary\n---\nB",
            ),
            (
                "plan.md",
                "---\nname: plan\ndescription: plans\nmode: primary\n---\nP",
            ),
            ("scout.md", "---\nname: scout\ndescription: looks\n---\nS"),
            ("docs.md", "---\nname: docs\ndescription: both\nmode: all\n---\nD"),
        ];
        builtin.extend_from_slice(extra);
        Arc::new(discover(&builtin, &[]))
    }

    #[test]
    fn a_definition_is_a_subagent_a_primary_agent_or_both() {
        assert_eq!(parse("", DefSource::Home).unwrap().mode, AgentMode::Subagent);
        for (written, mode) in [
            ("subagent", AgentMode::Subagent),
            ("primary", AgentMode::Primary),
            ("all", AgentMode::All),
        ] {
            assert_eq!(
                parse(&format!("mode: {written}"), DefSource::Home).unwrap().mode,
                mode
            );
        }
        let defs = primaries_and_subagents(&[]);
        let names = |defs: Vec<Arc<AgentDef>>| defs.iter().map(|def| def.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(defs.primaries()), ["build", "docs", "plan"]);
        assert_eq!(names(defs.subagents()), ["docs", "scout"]);
        assert!(defs.subagent("plan").is_none());
        assert!(defs.primary("scout").is_none());
        assert!(defs.subagent("docs").is_some() && defs.primary("docs").is_some());
    }

    #[test]
    fn colours_are_hex_or_names_and_claude_code_colours_never_fail() {
        assert_eq!(
            parse("color: \"#A0B1C2\"", DefSource::Home)
                .unwrap()
                .color
                .as_deref(),
            Some("#a0b1c2")
        );
        assert_eq!(
            parse("color: blue", DefSource::Home).unwrap().color.as_deref(),
            Some("#539bf5")
        );
        assert_eq!(parse("color: chartreuse", DefSource::Compat).unwrap().color, None);
    }

    #[test]
    fn the_lead_switches_only_between_primary_agents_and_tells_its_watchers() {
        let defs = primaries_and_subagents(&[]);
        assert_eq!(
            LeadAgent::new(defs.clone(), "scout").err().unwrap(),
            "`scout` is a sub-agent, not a primary agent; the primary agents are: build, docs, plan"
        );
        let lead = LeadAgent::new(defs.clone(), "build").unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let watching = seen.clone();
        lead.on_change(Arc::new(move |def| {
            watching.lock().unwrap().push(def.name.clone())
        }));

        assert_eq!(lead.next(true).as_deref(), Some("docs"));
        assert_eq!(lead.next(false).as_deref(), Some("plan"));
        assert_eq!(lead.set("plan"), Ok(true));
        assert_eq!(lead.set("plan"), Ok(false));
        assert_eq!(lead.next(true).as_deref(), Some("build"));
        assert!(lead.set("scout").is_err());
        assert!(
            lead.set("ghost")
                .unwrap_err()
                .starts_with("there is no agent `ghost`")
        );
        assert_eq!(lead.name(), "plan");
        assert_eq!(lead.def().unwrap().body, "P");
        // Coming back to what a session recorded tells nobody.
        assert_eq!(lead.restore("build").unwrap().name, "build");
        assert_eq!(*seen.lock().unwrap(), ["plan"]);
        assert_eq!(lead.default_name(), "build");

        let alone = LeadAgent::new(
            Arc::new(discover(
                &[(
                    "build.md",
                    "---\nname: build\ndescription: b\nmode: primary\n---\n",
                )],
                &[],
            )),
            "build",
        )
        .unwrap();
        assert_eq!(alone.next(true), None);
    }
}
