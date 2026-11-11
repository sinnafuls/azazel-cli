//! Permission rules, modes and the decision an agent's tool call gets.

pub mod gate;
pub mod plan;
pub mod shell;

use std::collections::HashMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::{PoisonError, RwLock};

use azazel_agent::{Access, AgentInfo};
use globset::{GlobBuilder, GlobMatcher};

use crate::config::{LoadedConfig, PermissionMode};
use crate::paths::{Paths, canonical};
use shell::ShellAnalysis;

pub const PLAN_DENIED: &str = "plan mode: only read-only tools are available. Write the plan to a file under \
                               .azazel/plans/ and call plan_submit.";

/// `tool` or `tool(pattern)`. The tool part is a glob over tool names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub tool: String,
    pub pattern: Option<String>,
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.pattern {
            Some(pattern) => write!(f, "{}({pattern})", self.tool),
            None => f.write_str(&self.tool),
        }
    }
}

impl FromStr for Rule {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let invalid = || format!("invalid permission rule `{s}`: expected `tool` or `tool(pattern)`");
        let (tool, pattern) = match s.split_once('(') {
            Some((tool, rest)) => {
                let pattern = rest
                    .strip_suffix(')')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .ok_or_else(invalid)?;
                (tool, Some(pattern))
            }
            None => (s, None),
        };
        let valid_name = !tool.is_empty()
            && tool
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '*' | '?'));
        if !valid_name {
            return Err(invalid());
        }
        Ok(Rule {
            tool: tool.to_string(),
            pattern: pattern.map(str::to_string),
        })
    }
}

/// What a tool's subjects are, which decides how rule patterns match them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectKind {
    Command,
    Path,
    Domain,
    Other,
}

pub fn subject_kind(tool: &str) -> SubjectKind {
    match tool {
        "shell" => SubjectKind::Command,
        "read" | "write" | "edit" | "glob" | "grep" | "lsp" => SubjectKind::Path,
        "web_fetch" | "browser" => SubjectKind::Domain,
        _ => SubjectKind::Other,
    }
}

fn glob(pattern: &str, literal_separator: bool) -> Option<GlobMatcher> {
    GlobBuilder::new(pattern)
        .literal_separator(literal_separator)
        .case_insensitive(cfg!(windows))
        .build()
        .ok()
        .map(|g| g.compile_matcher())
}

/// Forward slashes, `.`/`..` folded, no trailing slash.
fn normal(path: &Path) -> String {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string()
}

/// Where relative paths and patterns are anchored.
pub struct MatchCtx<'a> {
    pub project_root: &'a Path,
    /// The calling agent's working directory.
    pub cwd: &'a Path,
    pub home: Option<&'a Path>,
}

impl MatchCtx<'_> {
    /// A path subject as written by the model, made absolute.
    pub fn resolve(&self, subject: &str) -> PathBuf {
        let subject = subject.trim();
        if let Some(rest) = subject.strip_prefix("~/")
            && let Some(home) = self.home
        {
            return home.join(rest);
        }
        let path = Path::new(subject);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }
}

impl Rule {
    pub fn matches_tool(&self, tool: &str) -> bool {
        self.tool == tool || glob(&self.tool, false).is_some_and(|g| g.is_match(tool))
    }

    /// Whether this rule covers one subject of a call to `tool`. A rule without a pattern covers
    /// every subject.
    pub fn matches_subject(&self, tool: &str, subject: &str, ctx: &MatchCtx<'_>) -> bool {
        if !self.matches_tool(tool) {
            return false;
        }
        let Some(pattern) = &self.pattern else {
            return true;
        };
        match subject_kind(tool) {
            SubjectKind::Command => match pattern.strip_suffix(":*") {
                Some(prefix) => {
                    subject == prefix || subject.strip_prefix(prefix).is_some_and(|r| r.starts_with(' '))
                }
                None => glob(pattern, false).is_some_and(|g| g.is_match(subject)),
            },
            SubjectKind::Path => path_matches(pattern, subject, ctx),
            SubjectKind::Domain => match (pattern.strip_prefix("domain:"), subject.strip_prefix("domain:")) {
                (Some(wanted), Some(host)) => {
                    let (wanted, host) = (wanted.to_ascii_lowercase(), host.to_ascii_lowercase());
                    host == wanted || host.strip_suffix(&wanted).is_some_and(|rest| rest.ends_with('.'))
                }
                _ => glob(pattern, false).is_some_and(|g| g.is_match(subject)),
            },
            SubjectKind::Other => glob(pattern, false).is_some_and(|g| g.is_match(subject)),
        }
    }
}

/// Gitignore-style: a pattern without `/` matches at any depth; a pattern naming a directory covers
/// everything below it; relative patterns are anchored at the project root.
fn path_matches(pattern: &str, subject: &str, ctx: &MatchCtx<'_>) -> bool {
    let path = normal(&ctx.resolve(subject));
    let pattern = pattern.replace('\\', "/");
    let pattern = pattern.trim_end_matches('/');
    let absolute = if let Some(rest) = pattern.strip_prefix("~/") {
        match ctx.home {
            Some(home) => format!("{}/{rest}", normal(home)),
            None => return false,
        }
    } else if Path::new(pattern).is_absolute() || pattern.starts_with('/') {
        pattern.to_string()
    } else if pattern.contains('/') {
        format!("{}/{pattern}", globset::escape(&normal(ctx.project_root)))
    } else {
        format!("{}/**/{pattern}", globset::escape(&normal(ctx.project_root)))
    };
    [absolute.clone(), format!("{absolute}/**")]
        .iter()
        .any(|p| glob(p, true).is_some_and(|g| g.is_match(&path)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcedRule {
    pub rule: Rule,
    pub kind: RuleKind,
    /// The config file, or `session`, `environment or command line`.
    pub source: String,
}

/// One tool call as the permission engine sees it.
pub struct Call<'a> {
    pub agent: &'a AgentInfo,
    pub tool: &'a str,
    pub access: Access,
    pub subjects: &'a [String],
    /// For `shell` calls: the analysis of the command line.
    pub shell: Option<&'a ShellAnalysis>,
    /// For `shell` calls: the raw command line.
    pub command: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny(String),
    /// Ask the user. `rule` is what "always allow" would add, when a single rule can cover the call.
    Prompt {
        rule: Option<String>,
    },
}

/// Called with the new mode after the session mode changes.
pub type ModeWatcher = std::sync::Arc<dyn Fn(PermissionMode) + Send + Sync>;

/// The session's permission state: mode, rules, and where agents work.
pub struct PermissionState {
    mode: RwLock<PermissionMode>,
    mode_watchers: RwLock<Vec<ModeWatcher>>,
    agent_modes: RwLock<HashMap<String, PermissionMode>>,
    rules: RwLock<Vec<SourcedRule>>,
    agent_cwds: RwLock<HashMap<String, PathBuf>>,
    /// Tool grants of agents whose definition narrows a tool to a pattern.
    agent_grants: RwLock<HashMap<String, Vec<Rule>>>,
    project_root: PathBuf,
    /// The project root and `additional_dirs`.
    roots: Vec<PathBuf>,
    cwd: PathBuf,
    home: PathBuf,
    user_home: Option<PathBuf>,
    plans_dir: PathBuf,
    local_config: PathBuf,
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

impl PermissionState {
    pub fn new(paths: &Paths, loaded: &LoadedConfig) -> Result<Self, String> {
        let permissions = &loaded.config.permissions;
        let mut rules = Vec::new();
        for (kind, list, key) in [
            (RuleKind::Deny, &permissions.deny, "deny"),
            (RuleKind::Ask, &permissions.ask, "ask"),
            (RuleKind::Allow, &permissions.allow, "allow"),
        ] {
            for text in list {
                let source = loaded
                    .layers
                    .iter()
                    .find(|layer| {
                        layer
                            .table
                            .get("permissions")
                            .and_then(|p| p.get(key))
                            .and_then(|v| v.as_array())
                            .is_some_and(|rules| rules.iter().any(|r| r.as_str() == Some(text)))
                    })
                    .map(|layer| layer.path.display().to_string())
                    .unwrap_or_else(|| "environment or command line".to_string());
                rules.push(SourcedRule {
                    rule: text.parse()?,
                    kind,
                    source,
                });
            }
        }
        let mut roots = vec![canonical(&paths.project_root)];
        roots.extend(
            permissions
                .additional_dirs
                .iter()
                .map(|dir| canonical(&paths.project_root.join(dir))),
        );
        Ok(PermissionState {
            mode: RwLock::new(permissions.mode),
            mode_watchers: RwLock::default(),
            agent_modes: RwLock::default(),
            rules: RwLock::new(rules),
            agent_cwds: RwLock::default(),
            agent_grants: RwLock::default(),
            project_root: canonical(&paths.project_root),
            roots,
            cwd: paths.cwd.clone(),
            home: canonical(&paths.home),
            user_home: std::env::home_dir(),
            plans_dir: paths.plans_dir(),
            local_config: paths.project_local_config(),
        })
    }

    pub fn mode(&self) -> PermissionMode {
        *read(&self.mode)
    }

    /// Changes the session mode and tells the watchers when it actually changed.
    pub fn set_mode(&self, mode: PermissionMode) {
        let previous = std::mem::replace(&mut *write(&self.mode), mode);
        if previous != mode {
            let watchers: Vec<ModeWatcher> = read(&self.mode_watchers).clone();
            for watcher in watchers {
                watcher(mode);
            }
        }
    }

    /// `watcher` is called after every change of the session mode.
    pub fn on_mode_change(&self, watcher: ModeWatcher) {
        write(&self.mode_watchers).push(watcher);
    }

    /// Whether `path` is a file under `.azazel/plans/`.
    pub fn is_plan_file(&self, path: &Path) -> bool {
        Self::under(path, &self.plans_dir)
    }

    /// The session mode, unless this agent is pinned to another one.
    pub fn mode_for(&self, agent: &AgentInfo) -> PermissionMode {
        read(&self.agent_modes)
            .get(&agent.id)
            .copied()
            .unwrap_or_else(|| self.mode())
    }

    pub fn pin_agent_mode(&self, agent: &str, mode: PermissionMode) {
        write(&self.agent_modes).insert(agent.to_string(), mode);
    }

    /// Records that `agent` works somewhere other than the session's directory (a worktree).
    pub fn set_agent_cwd(&self, agent: &str, cwd: PathBuf) {
        write(&self.agent_cwds).insert(agent.to_string(), cwd);
    }

    /// Limits `agent` to what its definition grants: a grant with a pattern restricts its tool to
    /// calls whose every subject matches it. Tools granted without a pattern, and tools no grant
    /// names, are not affected here.
    pub fn restrict_agent(&self, agent: &str, grants: Vec<Rule>) {
        write(&self.agent_grants).insert(agent.to_string(), grants);
    }

    pub fn forget_agent(&self, agent: &str) {
        write(&self.agent_modes).remove(agent);
        write(&self.agent_cwds).remove(agent);
        write(&self.agent_grants).remove(agent);
    }

    /// `Err` when the agent's definition restricts this tool to patterns the call does not fit.
    fn within_grants(&self, call: &Call<'_>, ctx: &MatchCtx<'_>) -> Result<(), String> {
        let grants = read(&self.agent_grants);
        let Some(grants) = grants.get(&call.agent.id) else {
            return Ok(());
        };
        let mine: Vec<&Rule> = grants
            .iter()
            .filter(|grant| grant.matches_tool(call.tool))
            .collect();
        if mine.is_empty() || mine.iter().any(|grant| grant.pattern.is_none()) {
            return Ok(());
        }
        let unparsed_shell = call.shell.is_some_and(|analysis| !analysis.parsed);
        let covered = !unparsed_shell
            && !call.subjects.is_empty()
            && call.subjects.iter().all(|subject| {
                mine.iter()
                    .any(|grant| grant.matches_subject(call.tool, subject, ctx))
            });
        if covered {
            return Ok(());
        }
        let allowed: Vec<String> = mine.iter().map(|grant| grant.to_string()).collect();
        Err(format!(
            "agent `{}` may use `{}` only as: {}",
            call.agent.id,
            call.tool,
            allowed.join(", ")
        ))
    }

    pub fn cwd_of(&self, agent: &AgentInfo) -> PathBuf {
        read(&self.agent_cwds)
            .get(&agent.id)
            .cloned()
            .unwrap_or_else(|| self.cwd.clone())
    }

    pub fn rules(&self) -> Vec<SourcedRule> {
        read(&self.rules).clone()
    }

    pub fn add_session_rule(&self, rule: Rule, kind: RuleKind) {
        write(&self.rules).push(SourcedRule {
            rule,
            kind,
            source: "session".to_string(),
        });
    }

    /// Adds an allow rule for this session and appends it to `.azazel/config.local.toml`.
    pub fn remember_allow(&self, rule: &Rule) -> std::io::Result<()> {
        if !read(&self.rules)
            .iter()
            .any(|r| r.kind == RuleKind::Allow && &r.rule == rule)
        {
            write(&self.rules).push(SourcedRule {
                rule: rule.clone(),
                kind: RuleKind::Allow,
                source: self.local_config.display().to_string(),
            });
        }
        let text = std::fs::read_to_string(&self.local_config).unwrap_or_default();
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e}")))?;
        let permissions = doc
            .entry("permissions")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        let invalid = || {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "`permissions.allow` is not an array",
            )
        };
        let allow = permissions
            .as_table_like_mut()
            .ok_or_else(invalid)?
            .entry("allow")
            .or_insert(toml_edit::value(toml_edit::Array::new()))
            .as_array_mut()
            .ok_or_else(invalid)?;
        let wanted = rule.to_string();
        if !allow.iter().any(|v| v.as_str() == Some(&wanted)) {
            allow.push(wanted);
        }
        if let Some(dir) = self.local_config.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.local_config, doc.to_string())
    }

    fn ctx<'a>(&'a self, cwd: &'a Path) -> MatchCtx<'a> {
        MatchCtx {
            project_root: &self.project_root,
            cwd,
            home: self.user_home.as_deref(),
        }
    }

    fn inside_roots(&self, path: &Path) -> bool {
        let path = normal(path);
        self.roots.iter().any(|root| {
            let root = normal(root);
            let (path, root) = if cfg!(windows) {
                (path.to_ascii_lowercase(), root.to_ascii_lowercase())
            } else {
                (path.clone(), root)
            };
            path == root || path.strip_prefix(&root).is_some_and(|rest| rest.starts_with('/'))
        })
    }

    fn under(path: &Path, dir: &Path) -> bool {
        let (path, dir) = (normal(path), normal(dir));
        let (path, dir) = if cfg!(windows) {
            (path.to_ascii_lowercase(), dir.to_ascii_lowercase())
        } else {
            (path, dir)
        };
        path.strip_prefix(&dir).is_some_and(|rest| rest.starts_with('/'))
    }

    /// Writes to these always need the user's say-so: git internals, azazel's own config and state,
    /// shell profiles.
    fn is_protected(&self, path: &Path) -> bool {
        const PROFILES: [&str; 7] = [
            ".bashrc",
            ".bash_profile",
            ".zshrc",
            ".zprofile",
            ".profile",
            "profile.ps1",
            "microsoft.powershell_profile.ps1",
        ];
        let text = normal(path);
        let name = text.rsplit('/').next().unwrap_or_default().to_ascii_lowercase();
        let project_config = Self::under(path, &self.project_root.join(".azazel"))
            && path.parent().is_some_and(|p| normal(p).ends_with("/.azazel"))
            && name.starts_with("config")
            && name.ends_with(".toml");
        text.split('/').any(|part| part == ".git")
            || project_config
            || Self::under(path, &self.home)
            || PROFILES.contains(&name.as_str())
    }

    fn first_match(&self, kind: RuleKind, call: &Call<'_>, ctx: &MatchCtx<'_>) -> Option<SourcedRule> {
        read(&self.rules)
            .iter()
            .filter(|r| r.kind == kind && r.rule.matches_tool(call.tool))
            .find(|r| {
                r.rule.pattern.is_none()
                    || call
                        .subjects
                        .iter()
                        .any(|s| r.rule.matches_subject(call.tool, s, ctx))
            })
            .cloned()
    }

    /// Steps that no mode, hook or rule can override: deny rules and the push guard. Also reports
    /// whether the call writes to a protected path, which forces a prompt.
    pub fn precheck(&self, call: &Call<'_>) -> Result<bool, String> {
        let cwd = self.cwd_of(call.agent);
        let ctx = self.ctx(&cwd);
        if let Some(hit) = self.first_match(RuleKind::Deny, call, &ctx) {
            return Err(format!("denied by rule `{}` ({})", hit.rule, hit.source));
        }
        self.within_grants(call, &ctx)?;
        if let (Some(analysis), Some(command)) = (call.shell, call.command)
            && shell::pushes(command, analysis)
        {
            return Err(shell::PUSH_DENIED.to_string());
        }
        let protected = call.access == Access::Write
            && subject_kind(call.tool) == SubjectKind::Path
            && call.subjects.iter().any(|s| self.is_protected(&ctx.resolve(s)));
        Ok(protected)
    }

    /// Ask rules, the mode, then allow rules. `protected` comes from [`PermissionState::precheck`];
    /// `skip_prompt` is a hook's request to allow without asking.
    pub fn decide(&self, call: &Call<'_>, protected: bool, skip_prompt: bool) -> Verdict {
        let cwd = self.cwd_of(call.agent);
        let ctx = self.ctx(&cwd);
        let kind = subject_kind(call.tool);
        let prompt = || Verdict::Prompt {
            rule: self.suggest_rule(call, &ctx),
        };
        if protected {
            return Verdict::Prompt { rule: None };
        }
        if self.first_match(RuleKind::Ask, call, &ctx).is_some() {
            return if skip_prompt { Verdict::Allow } else { prompt() };
        }

        let paths_inside =
            || kind != SubjectKind::Path || call.subjects.iter().all(|s| self.inside_roots(&ctx.resolve(s)));
        let mode = self.mode_for(call.agent);
        if mode == PermissionMode::Plan && call.access != Access::Read {
            let plan_file = matches!(call.tool, "write" | "edit")
                && !call.subjects.is_empty()
                && call
                    .subjects
                    .iter()
                    .all(|s| Self::under(&ctx.resolve(s), &self.plans_dir));
            return if plan_file || call.tool == "plan_submit" {
                Verdict::Allow
            } else {
                Verdict::Deny(PLAN_DENIED.to_string())
            };
        }
        let by_mode = match (mode, call.access) {
            (PermissionMode::Auto, _) => true,
            (_, Access::Read) => paths_inside(),
            (PermissionMode::AcceptEdits, Access::Write) => kind == SubjectKind::Path && paths_inside(),
            _ => false,
        };
        if by_mode {
            return Verdict::Allow;
        }

        let rules = read(&self.rules);
        let allows = |subject: &str| {
            rules
                .iter()
                .any(|r| r.kind == RuleKind::Allow && r.rule.matches_subject(call.tool, subject, &ctx))
        };
        let unparsed_shell = call.shell.is_some_and(|analysis| !analysis.parsed);
        let by_rule = !unparsed_shell
            && if call.subjects.is_empty() {
                rules.iter().any(|r| {
                    r.kind == RuleKind::Allow && r.rule.pattern.is_none() && r.rule.matches_tool(call.tool)
                })
            } else {
                call.subjects.iter().all(|s| allows(s))
            };
        drop(rules);
        if by_rule || skip_prompt {
            Verdict::Allow
        } else {
            prompt()
        }
    }

    /// The rule "always allow" would add: narrow enough to mean something, wide enough to stop
    /// the same prompt from coming back.
    fn suggest_rule(&self, call: &Call<'_>, ctx: &MatchCtx<'_>) -> Option<String> {
        match subject_kind(call.tool) {
            SubjectKind::Command => {
                let analysis = call.shell.filter(|a| a.parsed)?;
                let [command] = analysis.commands.as_slice() else {
                    return None;
                };
                let program = command.words.first()?;
                let subcommand = command.words.get(1).filter(|w| {
                    !w.is_empty()
                        && w.chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                        && !w.starts_with('-')
                });
                Some(match subcommand {
                    Some(sub) => format!("shell({program} {sub}:*)"),
                    None => format!("shell({program}:*)"),
                })
            }
            SubjectKind::Path => {
                let [subject] = call.subjects else {
                    return None;
                };
                let path = ctx.resolve(subject);
                let dir = normal(path.parent()?);
                let root = normal(&self.project_root);
                Some(match dir.strip_prefix(&root) {
                    Some("") => format!("{}({})", call.tool, path.file_name()?.to_string_lossy()),
                    Some(rest) if rest.starts_with('/') => format!("{}({}/**)", call.tool, &rest[1..]),
                    _ => format!("{}({dir}/**)", call.tool),
                })
            }
            SubjectKind::Domain => match call.subjects {
                [subject] if subject.starts_with("domain:") => Some(format!("{}({subject})", call.tool)),
                _ => None,
            },
            SubjectKind::Other => Some(call.tool.to_string()),
        }
    }

    /// The `mode` part of the system prompt for `agent`.
    pub fn mode_prompt(&self, agent: &AgentInfo) -> Option<String> {
        match self.mode_for(agent) {
            PermissionMode::Plan => Some(
                "# Plan mode\nYou are planning, not changing anything. Investigate with read-only tools. \
                 When the approach is clear, write the plan to `.azazel/plans/<short-name>.md`: what will \
                 change, in which files, in what order, and how it will be verified. Then call `plan_submit` \
                 with that path and wait for the user's decision. Every other write and every command is \
                 refused in this mode."
                    .to_string(),
            ),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
