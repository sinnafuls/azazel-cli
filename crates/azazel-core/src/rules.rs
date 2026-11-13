//! Rule files: Markdown instructions that apply always, when an agent touches matching files,
//! when a reply matches a pattern as it is written, or on request.
//!
//! Rules are text for the model. They run nothing, so they load without project trust, as
//! `AGENTS.md` does.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

use async_trait::async_trait;
use azazel_agent::{AgentInfo, ToolCtx};
use azazel_ai::Block;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde_json::{Map, Value};

use crate::config::StreamRuleConfig;
use crate::context::{ContextFile, Includes};
use crate::ext::{Event, EventHandler, EventKind, ExtCtx, ExtError, Extension, Outcome, Registrar};
use crate::frontmatter;
use crate::paths::{Paths, canonical};
use crate::permissions::PermissionState;
use crate::prompt::SystemPartProvider;
use crate::scheme::{Scheme, split_url};

/// What is kept of one rule's text.
pub const MAX_RULE_BYTES: usize = 16 * 1024;
const TRUNCATED: &str = "\n[truncated: this rule exceeds 16 KB]";
/// How far back a scan of a growing reply starts again, so a match that straddles two fragments
/// is still found.
const SCAN_OVERLAP: usize = 512;
/// Paths remembered between a tool call and its result. Denied calls never produce a result, so
/// the oldest are dropped beyond this.
const MAX_PENDING_CALLS: usize = 256;
/// The tools whose `path` decides which path rules are added.
const FILE_TOOLS: [&str; 3] = ["read", "write", "edit"];

/// Where a rule was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `~/.azazel/rules/`
    Home,
    /// `<project>/.azazel/rules/`
    Project,
    /// `.claude/rules/`
    Claude,
    /// `.cursor/rules/*.mdc` and `.cursorrules`
    Cursor,
    /// `.windsurf/rules/*.md` and `.windsurfrules`
    Windsurf,
    /// `.github/copilot-instructions.md` and `.github/instructions/*.instructions.md`
    Copilot,
    /// `GEMINI.md`
    Gemini,
    /// `.clinerules`
    Cline,
    /// `[[stream_rules]]` in the configuration
    Config,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Home => "home",
            Origin::Project => "project",
            Origin::Claude => "claude",
            Origin::Cursor => "cursor",
            Origin::Windsurf => "windsurf",
            Origin::Copilot => "copilot",
            Origin::Gemini => "gemini",
            Origin::Cline => "cline",
            Origin::Config => "config",
        }
    }

    /// The origin a rule file most likely has, judged by where it lies (for `azazel rules test`).
    pub fn guess(file: &Path) -> Origin {
        let text = file.to_string_lossy().replace('\\', "/");
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.ends_with(".mdc") || name == ".cursorrules" {
            Origin::Cursor
        } else if text.contains(".windsurf/") || name == ".windsurfrules" {
            Origin::Windsurf
        } else if name.ends_with(".instructions.md") || name == "copilot-instructions.md" {
            Origin::Copilot
        } else if text.contains(".claude/") {
            Origin::Claude
        } else if text.contains(".clinerules") {
            Origin::Cline
        } else if name == "GEMINI.md" {
            Origin::Gemini
        } else {
            Origin::Project
        }
    }
}

/// What a `trigger` is matched against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The reply as it is written.
    Text,
    /// The reply's thinking as it is written.
    Thinking,
    /// The arguments of a finished call to this tool.
    Tool(String),
}

impl Scope {
    fn parse(text: &str) -> Result<Scope, String> {
        match text.trim() {
            "" | "text" => Ok(Scope::Text),
            "thinking" => Ok(Scope::Thinking),
            other => match other.strip_prefix("tool:").map(str::trim) {
                Some(tool) if !tool.is_empty() => Ok(Scope::Tool(tool.to_string())),
                _ => Err(format!(
                    "unknown scope `{other}`: expected text, thinking or tool:<name>"
                )),
            },
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scope::Text => f.write_str("text"),
            Scope::Thinking => f.write_str("thinking"),
            Scope::Tool(tool) => write!(f, "tool:{tool}"),
        }
    }
}

/// How a rule reaches an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applies {
    /// Part of the system prompt of every request.
    Always,
    /// Added to the result of the first `read`, `write` or `edit` of a matching file.
    Paths,
    /// Stops a reply (or denies a tool call) that matches the trigger.
    Trigger,
    /// Listed in the prompt by name and read through `rule://<name>`.
    OnRequest,
}

#[derive(Debug, Clone)]
pub struct Rule {
    /// Unique among the loaded rules; what `rule://<name>` and messages call it.
    pub name: String,
    /// The file, or `config` for `[[stream_rules]]`.
    pub source: PathBuf,
    pub origin: Origin,
    pub description: Option<String>,
    /// Globs relative to the project root.
    pub paths: Vec<String>,
    pub always: bool,
    pub trigger: Option<Regex>,
    pub scope: Scope,
    /// Agent definition names the rule is limited to; empty means every agent.
    pub agents: Vec<String>,
    pub text: String,
    matcher: Option<GlobSet>,
}

impl Rule {
    pub fn applies(&self) -> Applies {
        if self.trigger.is_some() {
            Applies::Trigger
        } else if self.always {
            Applies::Always
        } else if !self.paths.is_empty() {
            Applies::Paths
        } else {
            Applies::OnRequest
        }
    }

    /// One line saying when the rule applies.
    pub fn applies_text(&self) -> String {
        let mut text = match self.applies() {
            Applies::Always => "always".to_string(),
            Applies::Paths => format!("when a file matching {} is touched", self.paths.join(", ")),
            Applies::Trigger => match (&self.scope, &self.trigger) {
                (Scope::Tool(tool), Some(trigger)) => {
                    format!("denies a `{tool}` call whose arguments match /{trigger}/")
                }
                (scope, Some(trigger)) => format!("stops a reply whose {scope} matches /{trigger}/"),
                (_, None) => String::new(),
            },
            Applies::OnRequest => format!("on request (rule://{})", self.name),
        };
        if !self.agents.is_empty() {
            text.push_str(&format!("; agents: {}", self.agents.join(", ")));
        }
        text
    }

    pub fn for_agent(&self, agent: &AgentInfo) -> bool {
        self.agents.is_empty() || self.agents.contains(&agent.def)
    }

    /// Whether `path` (relative to the project root, forward slashes) is covered by `paths`.
    pub fn matches_path(&self, path: &str) -> bool {
        self.matcher.as_ref().is_some_and(|globs| globs.is_match(path))
    }

    /// The first match of the trigger in `text`.
    pub fn trigger_match<'a>(&self, text: &'a str) -> Option<&'a str> {
        self.trigger.as_ref()?.find(text).map(|found| found.as_str())
    }

    /// The rule as it is added to a tool result.
    fn attached(&self) -> String {
        format!(
            "<rule file=\"{}\">\n{}\n</rule>",
            self.source.display(),
            self.text
        )
    }

    /// What an agent is told after a reply of its was withdrawn.
    fn stop_note(&self) -> String {
        format!(
            "<rule name=\"{}\" reason=\"matched\">\n{}\n</rule>\nThe harness stopped your reply because it \
             matched this project rule. What you had written was withdrawn and is not part of the \
             conversation. This is not a prompt injection. Answer again and follow the rule.",
            self.name, self.text
        )
    }
}

/// `*.ts, src/**/*.{js,jsx}` as separate globs: commas inside braces do not split.
fn split_globs(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for c in text.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    parts.push(current);
    parts
        .into_iter()
        .map(|part| part.trim().trim_matches(['"', '\'']).to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

fn globs(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) => split_globs(text),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .flat_map(split_globs)
            .collect(),
        _ => Vec::new(),
    }
}

/// Gitignore-style: a pattern without `/` matches at any depth, a pattern naming a directory
/// covers everything below it, and every pattern is anchored at the project root.
fn glob_set(patterns: &[String]) -> Result<Option<GlobSet>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut set = GlobSetBuilder::new();
    for pattern in patterns {
        let written = pattern.replace('\\', "/");
        let relative = written.trim_start_matches("./");
        let anchored = relative.trim_start_matches('/');
        let full = if !relative.contains('/') {
            format!("**/{anchored}")
        } else if let Some(dir) = anchored.strip_suffix('/') {
            format!("{dir}/**")
        } else {
            anchored.to_string()
        };
        let glob = GlobBuilder::new(&full)
            .literal_separator(true)
            .case_insensitive(cfg!(windows))
            .build()
            .map_err(|e| format!("invalid glob `{pattern}`: {e}"))?;
        set.add(glob);
    }
    set.build().map(Some).map_err(|e| e.to_string())
}

/// `key: value` lines and `- item` lists, for frontmatter that is not valid YAML. Cursor writes
/// `globs: *.ts,*.tsx`, where YAML reads `*` as an alias.
fn lenient_frontmatter(yaml: &str) -> Map<String, Value> {
    let mut map = Map::new();
    let mut list_key: Option<String> = None;
    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(item) = trimmed.strip_prefix("- ")
            && let Some(key) = &list_key
        {
            let item = item.trim().trim_matches(['"', '\'']).to_string();
            match map.get_mut(key) {
                Some(Value::Array(items)) => items.push(Value::String(item)),
                _ => {
                    map.insert(key.clone(), Value::Array(vec![Value::String(item)]));
                }
            }
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim().to_string(), value.trim());
        list_key = value.is_empty().then(|| key.clone());
        let value = match value {
            "" => Value::Null,
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            text => Value::String(text.trim_matches(['"', '\'']).to_string()),
        };
        map.insert(key, value);
    }
    map
}

fn cut(text: &str) -> String {
    let text = text.trim();
    if text.len() <= MAX_RULE_BYTES {
        return text.to_string();
    }
    let mut end = MAX_RULE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{TRUNCATED}", &text[..end])
}

/// Reads one rule file. The frontmatter keys of every supported tool map onto the same rule:
/// `paths` (`globs`, `applyTo`), `always` (`alwaysApply`), `description`, `trigger`, `scope`,
/// `agents`. A file without frontmatter always applies.
pub fn parse(name: &str, source: &Path, origin: Origin, raw: &str) -> Result<Rule, String> {
    let (meta, body) = match frontmatter::split(raw) {
        Some((yaml, body)) => (
            frontmatter::parse(yaml).unwrap_or_else(|_| lenient_frontmatter(yaml)),
            body,
        ),
        None => (Map::new(), raw),
    };
    let text_of = |key: &str| {
        meta.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    };
    let flag = |key: &str| match meta.get(key) {
        Some(Value::Bool(on)) => Some(*on),
        Some(Value::String(text)) => match text.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    };
    let mut paths = ["paths", "globs", "applyTo"]
        .iter()
        .map(|key| globs(meta.get(*key)))
        .find(|found| !found.is_empty())
        .unwrap_or_default();
    let mut always = flag("always").or_else(|| flag("alwaysApply"));
    let mut trigger = text_of("trigger");
    // Windsurf's `trigger` names an activation mode, not a pattern.
    if origin == Origin::Windsurf
        && let Some(mode) = trigger.take()
    {
        match mode {
            "always_on" => always = Some(true),
            "glob" => always = Some(false),
            _ => {
                always = Some(false);
                paths.clear();
            }
        }
    }
    let trigger = trigger
        .map(|pattern| Regex::new(pattern).map_err(|e| format!("invalid trigger `{pattern}`: {e}")))
        .transpose()?;
    let description = text_of("description").map(str::to_string);
    // With nothing saying when it applies, a rule applies always; `always: false` alone makes it
    // one that is read on request.
    let always = always.unwrap_or(paths.is_empty() && trigger.is_none() && description.is_none());
    let text = cut(body);
    if text.is_empty() {
        return Err("the rule has no text".to_string());
    }
    Ok(Rule {
        name: name.to_string(),
        source: source.to_path_buf(),
        origin,
        description,
        matcher: glob_set(&paths)?,
        paths,
        always,
        trigger,
        scope: Scope::parse(text_of("scope").unwrap_or_default())?,
        agents: frontmatter::string_list(meta.get("agents")).unwrap_or_default(),
        text,
    })
}

/// The rule a `[[stream_rules]]` entry describes; `index` counts from 0.
pub fn from_config(index: usize, entry: &StreamRuleConfig) -> Result<Rule, String> {
    let trigger =
        Regex::new(&entry.pattern).map_err(|e| format!("invalid pattern `{}`: {e}", entry.pattern))?;
    Ok(Rule {
        name: format!("stream-rule-{}", index + 1),
        source: PathBuf::from("config"),
        origin: Origin::Config,
        description: None,
        paths: Vec::new(),
        always: false,
        trigger: Some(trigger),
        scope: Scope::parse(&entry.scope)?,
        agents: Vec::new(),
        text: cut(&entry.message),
        matcher: None,
    })
}

/// Which rule files are read.
#[derive(Debug, Clone)]
pub struct Sources {
    pub paths: Paths,
    /// Read rule files at all (`--no-context-files` turns this off).
    pub files: bool,
    /// Also read `.claude/rules/`.
    pub claude_compat: bool,
    /// Also read the rule files of Cursor, Windsurf, Copilot, Gemini and Cline.
    pub compat: bool,
    /// `[[stream_rules]]`.
    pub stream_rules: Vec<StreamRuleConfig>,
    /// How deep `@path` includes in rule files are followed (`context.include_depth`).
    pub include_depth: u32,
}

/// The rules that were found, and `(file, what is wrong)` for those that were left out.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub rules: Vec<Arc<Rule>>,
    pub problems: Vec<(String, String)>,
}

/// Every file below `dir` with one of `extensions`, sorted by path.
fn files_below(dir: &Path, extensions: &[&str], recurse: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if recurse {
                files_below(&path, extensions, recurse, out);
            }
        } else if path
            .file_name()
            .is_some_and(|name| extensions.iter().any(|ext| name.to_string_lossy().ends_with(ext)))
        {
            out.push(path);
        }
    }
}

/// The name of the rule in `file` under `root`: its path without the extension.
fn rule_name(root: &Path, file: &Path, extension: &str) -> String {
    let relative = file.strip_prefix(root).unwrap_or(file);
    let text = relative.to_string_lossy().replace('\\', "/");
    text.strip_suffix(extension).unwrap_or(&text).to_string()
}

/// Reads every rule file the sources name. The first rule of a name keeps it; a later one is
/// called `<origin>:<name>`.
pub fn load(sources: &Sources) -> Loaded {
    let mut loaded = Loaded::default();
    let includes = Includes::new(&sources.paths, sources.include_depth);
    let add = |name: String, file: &Path, origin: Origin, loaded: &mut Loaded| {
        let raw = match std::fs::read_to_string(file) {
            // What the rule includes is part of its text; the frontmatter is not text.
            Ok(raw) if !raw.trim().is_empty() => match frontmatter::split(&raw) {
                Some((_, body)) => {
                    let head = &raw[..raw.len() - body.len()];
                    format!("{head}{}", includes.expand(file, body))
                }
                None => includes.expand(file, &raw),
            },
            Ok(_) => return,
            Err(e) => {
                loaded.problems.push((file.display().to_string(), e.to_string()));
                return;
            }
        };
        let taken = |name: &str, loaded: &Loaded| loaded.rules.iter().any(|rule| rule.name == name);
        let mut name = name;
        if taken(&name, loaded) {
            name = format!("{}:{name}", origin.label());
        }
        if taken(&name, loaded) {
            return;
        }
        match parse(&name, file, origin, &raw) {
            Ok(rule) => loaded.rules.push(Arc::new(rule)),
            Err(problem) => loaded.problems.push((file.display().to_string(), problem)),
        }
    };
    if sources.files {
        let root = &sources.paths.project_root;
        let mut dirs = vec![
            (sources.paths.project_dir().join("rules"), Origin::Project, ".md"),
            (sources.paths.home.join("rules"), Origin::Home, ".md"),
        ];
        if sources.claude_compat {
            dirs.push((root.join(".claude").join("rules"), Origin::Claude, ".md"));
        }
        if sources.compat {
            dirs.push((root.join(".cursor").join("rules"), Origin::Cursor, ".mdc"));
            dirs.push((root.join(".windsurf").join("rules"), Origin::Windsurf, ".md"));
            dirs.push((
                root.join(".github").join("instructions"),
                Origin::Copilot,
                ".instructions.md",
            ));
            dirs.push((root.join(".clinerules"), Origin::Cline, ".md"));
        }
        for (dir, origin, extension) in dirs {
            let mut files = Vec::new();
            let recurse = matches!(origin, Origin::Project | Origin::Home | Origin::Claude);
            files_below(&dir, &[extension], recurse, &mut files);
            for file in files {
                add(rule_name(&dir, &file, extension), &file, origin, &mut loaded);
            }
        }
        if sources.compat {
            for (file, name, origin) in [
                (".cursorrules", "cursorrules", Origin::Cursor),
                (".windsurfrules", "windsurfrules", Origin::Windsurf),
                (
                    ".github/copilot-instructions.md",
                    "copilot-instructions",
                    Origin::Copilot,
                ),
                ("GEMINI.md", "GEMINI", Origin::Gemini),
                (".clinerules", "clinerules", Origin::Cline),
            ] {
                let file = root.join(file);
                if file.is_file() {
                    add(name.to_string(), &file, origin, &mut loaded);
                }
            }
        }
    }
    for (index, entry) in sources.stream_rules.iter().enumerate() {
        match from_config(index, entry) {
            Ok(rule) => loaded.rules.push(Arc::new(rule)),
            Err(problem) => loaded
                .problems
                .push((format!("stream_rules #{}", index + 1), problem)),
        }
    }
    loaded
}

/// `path` as written by someone working in `cwd`, relative to the (canonical) project root with
/// forward slashes: what the globs of a rule are matched against. A path outside the project
/// stays absolute.
pub fn project_relative(project_root: &Path, cwd: &Path, path: &str) -> String {
    let path = path.trim();
    let full = match (path.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => cwd.join(path),
    };
    let full = canonical(&full);
    let shown = full.strip_prefix(project_root).unwrap_or(&full);
    shown.to_string_lossy().replace('\\', "/")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the rules have done in the open session.
#[derive(Default)]
struct State {
    session: String,
    /// Names of the rules that reached an agent.
    applied: BTreeSet<String>,
    /// `(agent id, rule name)` of path rules already added for that agent.
    attached: HashSet<(String, String)>,
    /// `(agent id, rule name)` of stream rules that stopped a reply of that agent.
    fired: HashSet<(String, String)>,
    /// `(agent id, call id, path)` of file calls whose result has not arrived.
    calls: VecDeque<(String, String, String)>,
    /// Per agent, how much of the reply's text and thinking has been scanned.
    scanned: HashMap<String, (usize, usize)>,
}

impl State {
    /// Starts over when another session is open.
    fn enter(&mut self, session: &str) {
        if self.session != session {
            *self = State {
                session: session.to_string(),
                ..State::default()
            };
        }
    }
}

/// The loaded rules and what they did in this session; a service.
pub struct Rules {
    sources: Sources,
    project_root: PathBuf,
    loaded: RwLock<Arc<Loaded>>,
    state: Mutex<State>,
}

impl Rules {
    pub fn new(sources: Sources) -> Self {
        let loaded = load(&sources);
        for (file, problem) in &loaded.problems {
            tracing::warn!("rule {file} is ignored: {problem}");
        }
        Rules {
            project_root: canonical(&sources.paths.project_root),
            sources,
            loaded: RwLock::new(Arc::new(loaded)),
            state: Mutex::default(),
        }
    }

    /// Reads the rule files again; returns what was found.
    pub fn reload(&self) -> Arc<Loaded> {
        let loaded = Arc::new(load(&self.sources));
        *self.loaded.write().unwrap_or_else(PoisonError::into_inner) = loaded.clone();
        loaded
    }

    pub fn loaded(&self) -> Arc<Loaded> {
        self.loaded.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn get(&self, name: &str) -> Option<Arc<Rule>> {
        self.loaded().rules.iter().find(|rule| rule.name == name).cloned()
    }

    /// Whether the rule reached an agent in the session `session`.
    pub fn applied(&self, session: &str, name: &str) -> bool {
        let state = lock(&self.state);
        state.session == session && state.applied.contains(name)
    }

    /// The rules that are part of every request of `agent`, as context files.
    pub fn always_for(&self, agent: &AgentInfo) -> Vec<ContextFile> {
        let loaded = self.loaded();
        let rules: Vec<&Arc<Rule>> = loaded
            .rules
            .iter()
            .filter(|rule| rule.applies() == Applies::Always && rule.for_agent(agent))
            .collect();
        if rules.is_empty() {
            return Vec::new();
        }
        let mut state = lock(&self.state);
        state.enter(&agent.session_id);
        rules
            .into_iter()
            .map(|rule| {
                state.applied.insert(rule.name.clone());
                ContextFile {
                    path: rule.source.clone(),
                    text: rule.text.clone(),
                }
            })
            .collect()
    }

    /// The path rules that cover `path`, written as an agent in `cwd` would write it.
    pub fn for_path(&self, path: &str, cwd: &Path) -> Vec<Arc<Rule>> {
        let relative = project_relative(&self.project_root, cwd, path);
        self.loaded()
            .rules
            .iter()
            .filter(|rule| rule.applies() == Applies::Paths && rule.matches_path(&relative))
            .cloned()
            .collect()
    }

    fn scan(&self, agent: &AgentInfo, so_far: &str, thinking: bool) -> Option<String> {
        let loaded = self.loaded();
        let wanted = if thinking { Scope::Thinking } else { Scope::Text };
        let mut rules = loaded
            .rules
            .iter()
            .filter(|rule| rule.trigger.is_some() && rule.scope == wanted && rule.for_agent(agent))
            .peekable();
        rules.peek()?;
        let mut state = lock(&self.state);
        state.enter(&agent.session_id);
        let scanned = state.scanned.entry(agent.id.clone()).or_default();
        let done = if thinking { &mut scanned.1 } else { &mut scanned.0 };
        // A shorter text than was scanned is a new reply.
        let mut from = if *done > so_far.len() {
            0
        } else {
            done.saturating_sub(SCAN_OVERLAP)
        };
        while !so_far.is_char_boundary(from) {
            from -= 1;
        }
        *done = so_far.len();
        let fresh = &so_far[from..];
        // The pattern first: it almost never matches, and the record needs two strings to ask.
        let hit = rules.find(|rule| {
            rule.trigger
                .as_ref()
                .is_some_and(|trigger| trigger.is_match(fresh))
                && !state.fired.contains(&(agent.id.clone(), rule.name.clone()))
        })?;
        state.fired.insert((agent.id.clone(), hit.name.clone()));
        state.applied.insert(hit.name.clone());
        Some(hit.stop_note())
    }

    /// The note for the first stream rule the reply of `agent` matches and that has not stopped a
    /// reply of this agent in this session.
    pub fn stream_text(&self, agent: &AgentInfo, so_far: &str) -> Option<String> {
        self.scan(agent, so_far, false)
    }

    /// Like [`Rules::stream_text`] for rules of scope `thinking`.
    pub fn stream_thinking(&self, agent: &AgentInfo, so_far: &str) -> Option<String> {
        self.scan(agent, so_far, true)
    }

    /// The `tool:<name>` rule that the arguments of a call match.
    fn denies(&self, agent: &AgentInfo, tool: &str, input: &Value) -> Option<Arc<Rule>> {
        let loaded = self.loaded();
        let mut rules = loaded
            .rules
            .iter()
            .filter(|rule| {
                rule.trigger.is_some()
                    && matches!(&rule.scope, Scope::Tool(name) if name == tool)
                    && rule.for_agent(agent)
            })
            .peekable();
        rules.peek()?;
        // As JSON, and as the plain strings in it: a pattern with quotes or line breaks is
        // written against the latter.
        let mut texts = vec![input.to_string()];
        strings(input, &mut texts);
        let hit = rules
            .find(|rule| {
                rule.trigger
                    .as_ref()
                    .is_some_and(|trigger| texts.iter().any(|text| trigger.is_match(text)))
            })?
            .clone();
        let mut state = lock(&self.state);
        state.enter(&agent.session_id);
        state.applied.insert(hit.name.clone());
        Some(hit)
    }
}

fn strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
        Value::Object(fields) => fields.values().for_each(|item| strings(item, out)),
        _ => {}
    }
}

struct Handler(Arc<Rules>);

#[async_trait]
impl EventHandler for Handler {
    async fn handle(&self, ev: &mut Event, ctx: &ExtCtx) -> Outcome {
        let rules = &self.0;
        match ev {
            Event::ToolCall {
                agent,
                call_id,
                tool,
                input,
                ..
            } => {
                if let Some(rule) = rules.denies(agent, tool, input) {
                    return Outcome::Block {
                        reason: format!("denied by rule {}: {}", rule.name, rule.text),
                    };
                }
                let path = input.get("path").and_then(Value::as_str);
                if let Some(path) =
                    path.filter(|path| FILE_TOOLS.contains(&tool.as_str()) && split_url(path).is_none())
                {
                    let mut state = lock(&rules.state);
                    state.enter(&ctx.session_id);
                    if state.calls.len() >= MAX_PENDING_CALLS {
                        state.calls.pop_front();
                    }
                    state
                        .calls
                        .push_back((agent.id.clone(), call_id.clone(), path.to_string()));
                }
            }
            Event::ToolResult {
                agent,
                call_id,
                tool,
                output,
            } if FILE_TOOLS.contains(&tool.as_str()) => {
                let path = {
                    let mut state = lock(&rules.state);
                    let at = state
                        .calls
                        .iter()
                        .position(|(owner, id, _)| *owner == agent.id && id == call_id);
                    at.and_then(|at| state.calls.remove(at)).map(|(_, _, path)| path)
                };
                let Some(path) = path.filter(|_| !output.is_error) else {
                    return Outcome::Continue;
                };
                let cwd = match ctx.services.get::<PermissionState>() {
                    Some(permissions) => permissions.cwd_of(agent),
                    None => ctx.cwd.clone(),
                };
                let covering = rules.for_path(&path, &cwd);
                let mut state = lock(&rules.state);
                state.enter(&ctx.session_id);
                for rule in covering.iter().filter(|rule| rule.for_agent(agent)) {
                    if state.attached.insert((agent.id.clone(), rule.name.clone())) {
                        state.applied.insert(rule.name.clone());
                        output.content.push(Block::text(rule.attached()));
                    }
                }
            }
            // The summary does not keep the rules, so they are added again when next needed.
            Event::PostCompact { agent } => {
                lock(&rules.state)
                    .attached
                    .retain(|(owner, _)| *owner != agent.id);
            }
            // A new reply starts: scan it from its beginning.
            Event::BeforeRequest { agent, .. } => {
                lock(&rules.state).scanned.remove(&agent.id);
            }
            _ => {}
        }
        Outcome::Continue
    }
}

/// Lists the rules that are read on request.
struct OnRequestPart(Arc<Rules>);

#[async_trait]
impl SystemPartProvider for OnRequestPart {
    async fn render(&self, agent: &AgentInfo) -> Option<String> {
        let loaded = self.0.loaded();
        let lines: Vec<String> = loaded
            .rules
            .iter()
            .filter(|rule| rule.applies() == Applies::OnRequest && rule.for_agent(agent))
            .map(|rule| match &rule.description {
                Some(description) => format!("- {}: {description}", rule.name),
                None => format!("- {}", rule.name),
            })
            .collect();
        if lines.is_empty() {
            return None;
        }
        Some(format!(
            "# Rules on request\nThe user keeps these rules for particular kinds of work. Before doing \
             work one of them covers, read it with `read` at `rule://<name>` and follow it.\n{}",
            lines.join("\n")
        ))
    }
}

/// `rule://` lists the rules; `rule://<name>` returns one.
struct RuleScheme(Arc<Rules>);

#[async_trait]
impl Scheme for RuleScheme {
    fn scheme(&self) -> &str {
        "rule"
    }

    async fn read(&self, rest: &str, ctx: &ToolCtx) -> Result<Vec<Block>, String> {
        let loaded = self.0.loaded();
        let name = rest.trim().trim_matches('/');
        if name.is_empty() {
            if loaded.rules.is_empty() {
                return Ok(vec![Block::text("this project has no rules")]);
            }
            let lines: Vec<String> = loaded
                .rules
                .iter()
                .map(|rule| format!("{}: {}", rule.name, rule.applies_text()))
                .collect();
            return Ok(vec![Block::text(lines.join("\n"))]);
        }
        let Some(rule) = loaded.rules.iter().find(|rule| rule.name == name) else {
            let names: Vec<&str> = loaded.rules.iter().map(|rule| rule.name.as_str()).collect();
            return Err(format!(
                "no rule named `{name}`; the rules are: {}",
                if names.is_empty() {
                    "(none)".to_string()
                } else {
                    names.join(", ")
                }
            ));
        };
        let mut state = lock(&self.0.state);
        state.enter(&ctx.agent.session_id);
        state.applied.insert(rule.name.clone());
        Ok(vec![Block::text(rule.text.clone())])
    }
}

/// Registers the rules: the service, the handlers that add path rules and enforce `tool:` rules,
/// the list of rules read on request, and `rule://`.
pub struct RulesExt {
    pub rules: Arc<Rules>,
}

impl Extension for RulesExt {
    fn id(&self) -> &str {
        "rules"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.service(self.rules.clone());
        let handler: Arc<dyn EventHandler> = Arc::new(Handler(self.rules.clone()));
        for kind in [
            EventKind::ToolCall,
            EventKind::ToolResult,
            EventKind::PostCompact,
            EventKind::BeforeRequest,
        ] {
            r.on(kind, handler.clone());
        }
        r.system_part("rule_files", Arc::new(OnRequestPart(self.rules.clone())));
        r.scheme(Arc::new(RuleScheme(self.rules.clone())));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn project() -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join(".azazel")).unwrap();
        let paths = Paths::discover(&root, Some(&tmp.path().join("home").to_string_lossy()));
        (tmp, paths)
    }

    fn write(root: &Path, file: &str, text: &str) {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn sources(paths: &Paths) -> Sources {
        Sources {
            paths: paths.clone(),
            files: true,
            claude_compat: true,
            compat: true,
            stream_rules: Vec::new(),
            include_depth: 4,
        }
    }

    fn agent(id: &str, def: &str) -> AgentInfo {
        AgentInfo {
            id: id.into(),
            def: def.into(),
            parent: None,
            depth: 0,
            session_id: "s1".into(),
        }
    }

    fn rule(origin: Origin, raw: &str) -> Rule {
        parse("r", Path::new("r.md"), origin, raw).unwrap()
    }

    #[test]
    fn every_supported_frontmatter_maps_to_the_same_rule() {
        let body = "Use `?` for errors.";
        let variants = [
            (
                Origin::Project,
                "---\ndescription: Rust style\npaths: [\"**/*.rs\", \"src/**/*.toml\"]\n---\n",
            ),
            (
                Origin::Claude,
                "---\ndescription: Rust style\npaths:\n  - \"**/*.rs\"\n  - src/**/*.toml\n---\n",
            ),
            // Not valid YAML: the unquoted `*` reads as an alias.
            (
                Origin::Cursor,
                "---\ndescription: Rust style\nglobs: **/*.rs,src/**/*.toml\nalwaysApply: false\n---\n",
            ),
            (
                Origin::Windsurf,
                "---\ntrigger: glob\ndescription: Rust style\nglobs: **/*.rs, src/**/*.toml\n---\n",
            ),
            (
                Origin::Copilot,
                "---\ndescription: Rust style\napplyTo: \"**/*.rs,src/**/*.toml\"\n---\n",
            ),
            (
                Origin::Cline,
                "---\ndescription: Rust style\npaths:\n  - \"**/*.rs\"\n  - \"src/**/*.toml\"\n---\n",
            ),
        ];
        for (origin, head) in variants {
            let rule = rule(origin, &format!("{head}{body}\n"));
            assert_eq!(rule.paths, ["**/*.rs", "src/**/*.toml"], "{origin:?}");
            assert_eq!(rule.description.as_deref(), Some("Rust style"), "{origin:?}");
            assert_eq!(rule.applies(), Applies::Paths, "{origin:?}");
            assert!(!rule.always && rule.trigger.is_none(), "{origin:?}");
            assert_eq!(rule.text, body, "{origin:?}");
            assert!(rule.matches_path("crates/core/src/lib.rs"), "{origin:?}");
            assert!(rule.matches_path("src/a/b.toml"), "{origin:?}");
            assert!(!rule.matches_path("Cargo.toml"), "{origin:?}");
        }
    }

    #[test]
    fn how_a_rule_applies_follows_its_frontmatter() {
        // No frontmatter, or frontmatter that says nothing about when: always.
        assert_eq!(rule(Origin::Project, "be brief").applies(), Applies::Always);
        assert_eq!(
            rule(Origin::Claude, "---\nagents: coder, tester\n---\nbe brief").applies(),
            Applies::Always
        );
        for head in ["always: true", "alwaysApply: true"] {
            let rule = rule(
                Origin::Cursor,
                &format!("---\n{head}\nglobs: *.rs\n---\nbe brief"),
            );
            assert_eq!(rule.applies(), Applies::Always, "{head}");
        }
        assert_eq!(
            rule(Origin::Windsurf, "---\ntrigger: always_on\n---\nbe brief").applies(),
            Applies::Always
        );
        // A description alone, Cursor's manual rules and Windsurf's model-decided ones: on request.
        for (origin, head) in [
            (Origin::Project, "description: how to release"),
            (Origin::Cursor, "alwaysApply: false"),
            (
                Origin::Windsurf,
                "trigger: model_decision\ndescription: how to release",
            ),
            (Origin::Windsurf, "trigger: manual\nglobs: *.rs"),
        ] {
            let rule = rule(origin, &format!("---\n{head}\n---\nsteps"));
            assert_eq!(rule.applies(), Applies::OnRequest, "{head}");
        }
        // A trigger makes a stream rule, with a scope.
        let stream = rule(
            Origin::Project,
            "---\ntrigger: \"(?i)as an ai\"\nscope: thinking\n---\nno",
        );
        assert_eq!(stream.applies(), Applies::Trigger);
        assert_eq!(stream.scope, Scope::Thinking);
        assert_eq!(stream.trigger_match("well, As An AI I"), Some("As An AI"));
        let tool = rule(
            Origin::Project,
            "---\ntrigger: npm install\nscope: tool:shell\n---\nuse pnpm",
        );
        assert_eq!(tool.scope, Scope::Tool("shell".into()));

        let agents = rule(Origin::Project, "---\nagents: coder, tester\n---\nx");
        assert!(agents.for_agent(&agent("coder-1", "coder")));
        assert!(!agents.for_agent(&agent("lead", "lead")));

        for bad in [
            "---\ntrigger: \"(\"\n---\nx",
            "---\nscope: loud\n---\nx",
            "---\npaths: [\"a{\"]\n---\nx",
            "---\nalways: true\n---\n  \n",
        ] {
            assert!(
                parse("r", Path::new("r.md"), Origin::Project, bad).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn globs_are_anchored_at_the_project_root() {
        let rule = |paths: &str| rule(Origin::Project, &format!("---\npaths: {paths}\n---\nx"));
        let any_depth = rule("\"*.rs\"");
        assert!(any_depth.matches_path("main.rs") && any_depth.matches_path("a/b/main.rs"));
        assert!(!any_depth.matches_path("main.rs.bak"));
        let dir = rule("docs/");
        assert!(dir.matches_path("docs/a.md") && dir.matches_path("docs/x/y.md"));
        assert!(!dir.matches_path("src/docs/a.md"));
        let one_level = rule("\"src/*.rs\"");
        assert!(one_level.matches_path("src/lib.rs"));
        assert!(!one_level.matches_path("src/a/lib.rs"));
        let braces = rule("\"web/**/*.{ts,tsx}, /Cargo.toml\"");
        assert_eq!(braces.paths, ["web/**/*.{ts,tsx}", "/Cargo.toml"]);
        assert!(braces.matches_path("web/app/page.tsx") && braces.matches_path("Cargo.toml"));
        assert!(!braces.matches_path("crates/x/Cargo.toml"));
    }

    #[test]
    fn a_rule_file_includes_other_files_in_its_text_and_not_in_its_frontmatter() {
        let (_tmp, paths) = project();
        let root = &paths.project_root;
        write(root, "docs/sql.md", "Never `SELECT *`.");
        write(root, "paths.md", "docs/**");
        write(
            root,
            ".azazel/rules/db.md",
            "---\npaths: \"@paths.md\"\n---\nDatabase work:\n@docs/sql.md\n",
        );
        let loaded = load(&sources(&paths));
        let rule = loaded.rules.iter().find(|rule| rule.name == "db").unwrap();
        assert_eq!(
            rule.text,
            "Database work:\n<include file=\"docs/sql.md\">\nNever `SELECT *`.\n</include>"
        );
        // The frontmatter is read as it was written.
        assert!(rule.matches_path("@paths.md") && !rule.matches_path("docs/sql.md"));

        let off = load(&Sources {
            include_depth: 0,
            ..sources(&paths)
        });
        let rule = off.rules.iter().find(|rule| rule.name == "db").unwrap();
        assert_eq!(rule.text, "Database work:\n@docs/sql.md");
    }

    #[test]
    fn a_rule_is_cut_at_the_size_cap() {
        let long = "é".repeat(MAX_RULE_BYTES);
        let rule = rule(Origin::Project, &long);
        assert!(rule.text.ends_with(TRUNCATED));
        let kept = rule.text.strip_suffix(TRUNCATED).unwrap();
        assert!(kept.len() <= MAX_RULE_BYTES && kept.len() >= MAX_RULE_BYTES - 1);
        assert!(kept.chars().all(|c| c == 'é'));
        let short = super::rule_name(Path::new("a"), Path::new("a/b/c.md"), ".md");
        assert_eq!(short, "b/c");
    }

    #[test]
    fn rule_files_of_every_tool_are_found() {
        let (_tmp, paths) = project();
        let root = paths.project_root.clone();
        write(
            &root,
            ".azazel/rules/rust/errors.md",
            "---\npaths: \"*.rs\"\n---\nuse ?",
        );
        write(
            &paths.home,
            "rules/errors.md",
            "shadowed by the project's name? no: rust/errors differs",
        );
        write(&paths.home, "rules/tone.md", "be brief");
        write(&root, ".claude/rules/tone.md", "claude's tone");
        write(&root, ".cursor/rules/ui.mdc", "---\nglobs: *.tsx\n---\ncursor ui");
        write(&root, ".cursorrules", "old cursor rules");
        write(
            &root,
            ".windsurf/rules/go.md",
            "---\ntrigger: always_on\n---\nwindsurf go",
        );
        write(&root, ".windsurfrules", "old windsurf rules");
        write(&root, ".github/copilot-instructions.md", "copilot says");
        write(
            &root,
            ".github/instructions/py.instructions.md",
            "---\napplyTo: \"**/*.py\"\n---\ncopilot python",
        );
        write(&root, "GEMINI.md", "gemini says");
        write(&root, ".clinerules", "cline says");
        write(&root, ".azazel/rules/broken.md", "---\ntrigger: \"(\"\n---\nx");
        write(&root, ".azazel/rules/empty.md", "  \n");

        let loaded = load(&sources(&paths));
        let names: Vec<&str> = loaded.rules.iter().map(|rule| rule.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "rust/errors",
                "errors",
                "tone",
                "claude:tone",
                "ui",
                "go",
                "py",
                "cursorrules",
                "windsurfrules",
                "copilot-instructions",
                "GEMINI",
                "clinerules"
            ]
        );
        assert_eq!(loaded.problems.len(), 1, "{:?}", loaded.problems);
        assert!(loaded.problems[0].0.ends_with("broken.md"));

        let only_own = load(&Sources {
            compat: false,
            claude_compat: false,
            ..sources(&paths)
        });
        assert_eq!(only_own.rules.len(), 3);
        let none = load(&Sources {
            files: false,
            stream_rules: vec![StreamRuleConfig {
                pattern: "TODO".into(),
                message: "no todos".into(),
                scope: "text".into(),
            }],
            ..sources(&paths)
        });
        assert_eq!(none.rules.len(), 1);
        assert_eq!(none.rules[0].name, "stream-rule-1");
        assert_eq!(none.rules[0].applies(), Applies::Trigger);
    }

    fn registry(paths: &Paths, rules: Arc<Rules>) -> crate::ext::Registry {
        let registry = crate::ext::Registry::new(ExtCtx {
            services: Arc::new(azazel_agent::Services::new()),
            cwd: paths.cwd.clone(),
            session_id: "s1".into(),
        });
        registry.load(&RulesExt { rules }).unwrap();
        registry
    }

    async fn touch(
        registry: &crate::ext::Registry,
        agent: &AgentInfo,
        id: &str,
        tool: &str,
        path: &str,
    ) -> String {
        let outcome = registry
            .bus
            .emit(&mut Event::ToolCall {
                agent: agent.clone(),
                call_id: id.into(),
                tool: tool.into(),
                input: json!({"path": path}),
                skip_prompt: false,
            })
            .await;
        assert_eq!(outcome, Outcome::Continue);
        let mut result = Event::ToolResult {
            agent: agent.clone(),
            call_id: id.into(),
            tool: tool.into(),
            output: azazel_agent::ToolOutput::text("content"),
        };
        registry.bus.emit(&mut result).await;
        let Event::ToolResult { output, .. } = result else {
            panic!()
        };
        output.text_content()
    }

    #[tokio::test]
    async fn a_path_rule_is_added_once_per_agent_and_again_after_a_compaction() {
        let (_tmp, paths) = project();
        let root = paths.project_root.clone();
        write(
            &root,
            ".azazel/rules/rust.md",
            "---\npaths: \"**/*.rs\"\n---\nuse ?",
        );
        write(
            &root,
            ".azazel/rules/coder.md",
            "---\npaths: \"*.rs\"\nagents: coder\n---\ncoders only",
        );
        write(&root, "src/lib.rs", "fn main() {}");
        let rules = Arc::new(Rules::new(sources(&paths)));
        let registry = registry(&paths, rules.clone());
        let lead = agent("lead", "lead");

        assert!(!rules.applied("s1", "rust"));
        let first = touch(&registry, &lead, "c1", "read", "src/lib.rs").await;
        assert!(
            first.contains("<rule file=") && first.contains("use ?"),
            "{first}"
        );
        assert!(!first.contains("coders only"));
        assert!(rules.applied("s1", "rust") && !rules.applied("s2", "rust"));
        // Not again for the same agent, whatever the tool; not for other files.
        let again = touch(&registry, &lead, "c2", "edit", "src/lib.rs").await;
        assert_eq!(again, "content");
        assert_eq!(
            touch(&registry, &lead, "c3", "read", "README.md").await,
            "content"
        );
        // Another agent gets it, and the rule limited to its definition.
        let coder = agent("coder-1", "coder");
        let theirs = touch(&registry, &coder, "c4", "write", "src/lib.rs").await;
        assert!(
            theirs.contains("use ?") && theirs.contains("coders only"),
            "{theirs}"
        );

        registry
            .bus
            .emit(&mut Event::PostCompact { agent: lead.clone() })
            .await;
        let after = touch(&registry, &lead, "c5", "read", "src/lib.rs").await;
        assert!(after.contains("use ?"), "{after}");
        // The other agent's record is untouched by the lead's compaction.
        let coder_again = touch(&registry, &coder, "c6", "read", "src/lib.rs").await;
        assert_eq!(coder_again, "content");
    }

    #[tokio::test]
    async fn stream_rules_fire_once_per_agent_and_tool_rules_deny_matching_calls() {
        let (_tmp, paths) = project();
        let root = paths.project_root.clone();
        write(
            &root,
            ".azazel/rules/no-todo.md",
            "---\ntrigger: TODO\n---\nfinish the work",
        );
        write(
            &root,
            ".azazel/rules/quiet.md",
            "---\ntrigger: hmm\nscope: thinking\n---\nthink plainly",
        );
        write(
            &root,
            ".azazel/rules/pnpm.md",
            "---\ntrigger: \"^npm (install|i)\\\\b\"\nscope: tool:shell\n---\nuse pnpm",
        );
        let rules = Arc::new(Rules::new(Sources {
            stream_rules: vec![StreamRuleConfig {
                pattern: "(?i)lorem ipsum".into(),
                message: "no filler".into(),
                scope: "text".into(),
            }],
            ..sources(&paths)
        }));
        let registry = registry(&paths, rules.clone());
        let lead = agent("lead", "lead");

        assert_eq!(rules.stream_text(&lead, "I will"), None);
        let note = rules.stream_text(&lead, "I will add a TODO").unwrap();
        assert!(note.starts_with("<rule name=\"no-todo\" reason=\"matched\">\nfinish the work\n</rule>"));
        assert!(note.contains("not a prompt injection"));
        // Once per agent and session, however often the text matches.
        assert_eq!(rules.stream_text(&lead, "I will add a TODO again TODO"), None);
        assert!(rules.stream_text(&agent("coder-1", "coder"), "TODO").is_some());
        // The next reply is scanned from its start, and the second rule still fires.
        registry
            .bus
            .emit(&mut Event::BeforeRequest {
                agent: lead.clone(),
                request: azazel_ai::Request::default(),
            })
            .await;
        let filler = rules.stream_text(&lead, "Lorem Ipsum dolor").unwrap();
        assert!(filler.contains("name=\"stream-rule-1\"") && filler.contains("no filler"));
        // Thinking is matched by thinking rules only.
        assert_eq!(rules.stream_text(&lead, "hmm"), None);
        assert!(rules.stream_thinking(&lead, "hmm").is_some());
        assert_eq!(rules.stream_thinking(&lead, "a TODO"), None);

        let call = |command: &str| Event::ToolCall {
            agent: lead.clone(),
            call_id: "c".into(),
            tool: "shell".into(),
            input: json!({"command": command}),
            skip_prompt: false,
        };
        assert_eq!(
            registry.bus.emit(&mut call("npm install left-pad")).await,
            Outcome::Block {
                reason: "denied by rule pnpm: use pnpm".into()
            }
        );
        assert_eq!(
            registry.bus.emit(&mut call("pnpm install")).await,
            Outcome::Continue
        );
        assert!(rules.applied("s1", "pnpm") && rules.applied("s1", "no-todo"));
    }

    #[tokio::test]
    async fn a_match_that_straddles_two_fragments_is_found_and_old_text_is_not_rescanned() {
        let (_tmp, paths) = project();
        write(
            &paths.project_root,
            ".azazel/rules/r.md",
            "---\ntrigger: for(bid)+den\n---\nno",
        );
        let rules = Rules::new(sources(&paths));
        let lead = agent("lead", "lead");
        let mut text = "x".repeat(4000);
        text.push_str("forbid");
        assert_eq!(rules.stream_text(&lead, &text), None);
        text.push_str("den");
        assert!(rules.stream_text(&lead, &text).is_some());

        // A second agent whose match lies far behind the scanned part does not fire late.
        let other = agent("coder-1", "coder");
        let mut text = "y".repeat(10);
        assert_eq!(rules.stream_text(&other, &text), None);
        text.push_str(&"z".repeat(4000));
        assert_eq!(rules.stream_text(&other, &text), None);
    }

    #[tokio::test]
    async fn always_rules_are_context_files_and_described_rules_are_listed_and_read() {
        let (_tmp, paths) = project();
        let root = paths.project_root.clone();
        write(&root, ".azazel/rules/tone.md", "be brief");
        write(
            &root,
            ".azazel/rules/coder.md",
            "---\nagents: coder\n---\ncoders only",
        );
        write(
            &root,
            ".azazel/rules/release.md",
            "---\ndescription: how to cut a release\n---\ntag, then build",
        );
        let rules = Arc::new(Rules::new(sources(&paths)));
        let registry = registry(&paths, rules.clone());
        let lead = agent("lead", "lead");

        let always = rules.always_for(&lead);
        assert_eq!(always.len(), 1);
        assert_eq!(always[0].text, "be brief");
        assert_eq!(rules.always_for(&agent("coder-1", "coder")).len(), 2);

        let prompt = registry.prompt.render(&lead).await;
        let listed = prompt
            .iter()
            .find(|part| part.text.starts_with("# Rules on request"))
            .unwrap();
        assert!(listed.text.contains("- release: how to cut a release"));
        assert!(!listed.text.contains("tone"));

        let scheme = registry.schemes.get("rule").unwrap();
        let ctx = ToolCtx {
            agent: lead.clone(),
            agent_id: lead.id.clone(),
            session_id: lead.session_id.clone(),
            call_id: "c".into(),
            cwd: paths.cwd.clone(),
            cancel: tokio_util::sync::CancellationToken::new(),
            update: azazel_agent::UpdateSink::default(),
            services: Arc::new(azazel_agent::Services::new()),
        };
        let text = |blocks: Vec<Block>| match &blocks[0] {
            Block::Text { text } => text.clone(),
            _ => String::new(),
        };
        assert_eq!(
            text(scheme.read("release", &ctx).await.unwrap()),
            "tag, then build"
        );
        assert!(rules.applied("s1", "release"));
        assert!(text(scheme.read("", &ctx).await.unwrap()).contains("release: on request (rule://release)"));
        let missing = scheme.read("nope", &ctx).await.unwrap_err();
        assert!(
            missing.contains("no rule named `nope`") && missing.contains("tone"),
            "{missing}"
        );

        // Edits are picked up by a reload.
        write(&root, ".azazel/rules/tone.md", "be very brief");
        assert_eq!(rules.reload().rules.len(), 3);
        assert_eq!(rules.always_for(&lead)[0].text, "be very brief");
    }
}
