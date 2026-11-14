//! Permission rules, modes and the decision an agent's tool call gets.

pub mod gate;
pub mod plan;
mod secrets;
pub mod shell;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::{PoisonError, RwLock};

use azazel_agent::{Access, AgentInfo};
use globset::{GlobBuilder, GlobMatcher};

use crate::config::{DestructivePolicy, LoadedConfig, PermissionMode};
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

/// The source of a rule that lasts until azazel exits.
pub const SESSION: &str = "session";

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
        "read" | "write" | "edit" | "glob" | "grep" | "lsp" | "ast_grep" => SubjectKind::Path,
        "web_fetch" | "web_grep" | "browser" | "http" => SubjectKind::Domain,
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
/// Whether `path` (absolute) is on the built-in list of files that hold credentials. For code
/// that reads a file on its own account, such as an `@path` include, where no agent asked and
/// no user can be asked.
pub(crate) fn holds_credentials(path: &Path, azazel_home: &Path, project_root: &Path) -> bool {
    let home = std::env::home_dir().map(|home| canonical(&home));
    let ctx = MatchCtx {
        project_root,
        cwd: project_root,
        home: home.as_deref(),
    };
    secrets::is_secret(path, &[], azazel_home, &ctx)
}

pub struct MatchCtx<'a> {
    pub project_root: &'a Path,
    /// The calling agent's working directory.
    pub cwd: &'a Path,
    pub home: Option<&'a Path>,
}

impl MatchCtx<'_> {
    /// A path subject as written by the model, made absolute and canonical: the form the roots,
    /// the homes and the plans directory are kept in, so `/var/…` on macOS is `/private/var/…`
    /// and a Windows short name (`RUNNER~1`) is the long one.
    pub fn resolve(&self, subject: &str) -> PathBuf {
        let subject = subject.trim();
        if let Some(rest) = subject.strip_prefix("~/")
            && let Some(home) = self.home
        {
            return canonical(&home.join(rest));
        }
        let path = Path::new(subject);
        if path.is_absolute() {
            canonical(path)
        } else {
            canonical(&self.cwd.join(path))
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
            SubjectKind::Path => path_matches(pattern, &normal(&ctx.resolve(subject)), ctx),
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
/// everything below it; relative patterns are anchored at the project root. `path` is resolved
/// and normalised.
fn path_matches(pattern: &str, path: &str, ctx: &MatchCtx<'_>) -> bool {
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
        .any(|p| glob(p, true).is_some_and(|g| g.is_match(path)))
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
    /// The one mode of the session: the lead and every sub-agent follow it as it changes.
    mode: RwLock<PermissionMode>,
    mode_watchers: RwLock<Vec<ModeWatcher>>,
    /// Agents whose definition keeps them in plan mode whatever the session's mode is. A
    /// definition can only narrow: no agent has a mode that lets it do more than the session.
    plan_agents: RwLock<HashSet<String>>,
    rules: RwLock<Vec<SourcedRule>>,
    agent_cwds: RwLock<HashMap<String, PathBuf>>,
    /// Tool grants of agents whose definition, or the request that started them, narrows a tool
    /// to a pattern. A call has to fit every list.
    agent_grants: RwLock<HashMap<String, Vec<Vec<Rule>>>>,
    project_root: PathBuf,
    /// The project root and `additional_dirs`.
    roots: RwLock<Vec<PathBuf>>,
    cwd: PathBuf,
    home: PathBuf,
    user_home: Option<PathBuf>,
    plans_dir: PathBuf,
    local_config: PathBuf,
    /// What a command that destroys work needs.
    destructive: DestructivePolicy,
    /// `permissions.secret_paths`: patterns added to the built-in list of secret files.
    secret_patterns: Vec<String>,
    /// Secret files the user allowed for this session, as normalised paths.
    secret_grants: RwLock<HashSet<String>>,
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
            plan_agents: RwLock::default(),
            rules: RwLock::new(rules),
            agent_cwds: RwLock::default(),
            agent_grants: RwLock::default(),
            project_root: canonical(&paths.project_root),
            roots: RwLock::new(roots),
            cwd: paths.cwd.clone(),
            home: canonical(&paths.home),
            user_home: std::env::home_dir().map(|home| canonical(&home)),
            plans_dir: canonical(&paths.plans_dir()),
            local_config: paths.project_local_config(),
            destructive: permissions.destructive,
            secret_patterns: permissions.secret_paths.clone(),
            secret_grants: RwLock::default(),
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

    /// The session mode, or plan mode for an agent whose definition keeps it there.
    pub fn mode_for(&self, agent: &AgentInfo) -> PermissionMode {
        if read(&self.plan_agents).contains(&agent.id) {
            PermissionMode::Plan
        } else {
            self.mode()
        }
    }

    /// Keeps `agent` in plan mode whatever the session's mode becomes (a definition with
    /// `mode: plan`).
    pub fn keep_in_plan_mode(&self, agent: &str) {
        write(&self.plan_agents).insert(agent.to_string());
    }

    /// Whether `agent` is kept in plan mode whatever the session's mode is.
    pub fn kept_in_plan_mode(&self, agent: &str) -> bool {
        read(&self.plan_agents).contains(agent)
    }

    /// Puts `agent` under the limits of the definition it follows now, in place of those of the
    /// one before: kept in plan mode or not, and restricted to `grants` or not (the lead switching
    /// between primary agents).
    pub fn set_limits(&self, agent: &str, plan: bool, grants: Option<Vec<Rule>>) {
        match plan {
            true => write(&self.plan_agents).insert(agent.to_string()),
            false => write(&self.plan_agents).remove(agent),
        };
        match grants {
            Some(grants) => write(&self.agent_grants).insert(agent.to_string(), vec![grants]),
            None => write(&self.agent_grants).remove(agent),
        };
    }

    /// Whether the session runs without the safety guards: only deny rules and the push guard
    /// stop a call.
    fn bypass(&self) -> bool {
        self.mode() == PermissionMode::Bypass
    }

    /// Records that `agent` works somewhere other than the session's directory (a worktree).
    pub fn set_agent_cwd(&self, agent: &str, cwd: PathBuf) {
        write(&self.agent_cwds).insert(agent.to_string(), cwd);
    }

    /// Limits `agent` to what its definition grants: a grant with a pattern restricts its tool to
    /// calls whose every subject matches it. Tools granted without a pattern, and tools no grant
    /// names, are not affected here.
    pub fn restrict_agent(&self, agent: &str, grants: Vec<Rule>) {
        write(&self.agent_grants).insert(agent.to_string(), vec![grants]);
    }

    /// Limits `agent` further, to what `grants` allows on top of the limits it already has (a
    /// skill's `allowed-tools` inside what the agent's definition grants).
    pub fn narrow_agent(&self, agent: &str, grants: Vec<Rule>) {
        let mut all = write(&self.agent_grants);
        let lists = all.entry(agent.to_string()).or_default();
        if !lists.contains(&grants) {
            lists.push(grants);
        }
    }

    /// Adds a rule that is in force until [`PermissionState::remove_rules`] takes it away, such
    /// as what a loaded skill allows for the rest of a run. `source` says where it comes from.
    pub fn add_temporary_rule(&self, rule: Rule, kind: RuleKind, source: &str) {
        let mut rules = write(&self.rules);
        let known = rules
            .iter()
            .any(|r| r.kind == kind && r.rule == rule && r.source == source);
        if !known {
            rules.push(SourcedRule {
                rule,
                kind,
                source: source.to_string(),
            });
        }
    }

    /// Removes the rules `gone` picks and returns how many there were.
    pub fn remove_rules(&self, gone: impl Fn(&SourcedRule) -> bool) -> usize {
        let mut rules = write(&self.rules);
        let before = rules.len();
        rules.retain(|rule| !gone(rule));
        before - rules.len()
    }

    pub fn forget_agent(&self, agent: &str) {
        write(&self.plan_agents).remove(agent);
        write(&self.agent_cwds).remove(agent);
        write(&self.agent_grants).remove(agent);
    }

    /// `Err` when the agent's grants restrict this tool to patterns the call does not fit.
    fn within_grants(&self, call: &Call<'_>, ctx: &MatchCtx<'_>) -> Result<(), String> {
        let lists = read(&self.agent_grants);
        lists
            .get(&call.agent.id)
            .into_iter()
            .flatten()
            .try_for_each(|grants| Self::within(grants, call, ctx))
    }

    fn within(grants: &[Rule], call: &Call<'_>, ctx: &MatchCtx<'_>) -> Result<(), String> {
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
            source: SESSION.to_string(),
        });
    }

    /// Allows what `rule` covers until the session ends ("allow this session"). Nothing is
    /// written to a file, and `/forget-approvals` takes it back.
    pub fn allow_for_session(&self, rule: &Rule) {
        let mut rules = write(&self.rules);
        if !rules.iter().any(|r| r.kind == RuleKind::Allow && &r.rule == rule) {
            rules.push(SourcedRule {
                rule: rule.clone(),
                kind: RuleKind::Allow,
                source: SESSION.to_string(),
            });
        }
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

    /// Removes a rule that was added for this session only. False when there is none.
    pub fn remove_session_rule(&self, rule: &Rule, kind: RuleKind) -> bool {
        let mut rules = write(&self.rules);
        let found = rules
            .iter()
            .position(|r| r.source == SESSION && r.kind == kind && &r.rule == rule);
        if let Some(at) = found {
            rules.remove(at);
        }
        found.is_some()
    }

    /// Forgets an approval given with "always allow": the rule stops applying now and leaves
    /// `.azazel/config.local.toml`. False when neither had it.
    pub fn forget_allow(&self, rule: &Rule) -> std::io::Result<bool> {
        let source = self.local_config.display().to_string();
        let in_session = {
            let mut rules = write(&self.rules);
            let before = rules.len();
            rules.retain(|r| !(r.kind == RuleKind::Allow && r.source == source && &r.rule == rule));
            rules.len() != before
        };
        let wanted = rule.to_string();
        let in_file = self.edit_local_allow(|allow| {
            let before = allow.len();
            allow.retain(|value| value.as_str() != Some(&wanted));
            before - allow.len()
        })?;
        Ok(in_session || in_file > 0)
    }

    /// Forgets every remembered approval and every rule added for this session. Returns how many
    /// rules went.
    pub fn clear_remembered(&self) -> std::io::Result<usize> {
        let source = self.local_config.display().to_string();
        let (mut session, mut remembered) = (0, 0);
        session += std::mem::take(&mut *write(&self.secret_grants)).len();
        write(&self.rules).retain(|r| {
            if r.source == SESSION {
                session += 1;
                false
            } else if r.kind == RuleKind::Allow && r.source == source {
                remembered += 1;
                false
            } else {
                true
            }
        });
        let in_file = self.edit_local_allow(|allow| {
            let count = allow.len();
            allow.clear();
            count
        })?;
        // A remembered rule is in the file and in this session: it counts once. The file can
        // hold more than the session does when the project is not trusted.
        Ok(session + remembered.max(in_file))
    }

    /// Changes `permissions.allow` in `.azazel/config.local.toml` and returns what `change`
    /// returns. The file is left alone when it has no such list or nothing changed.
    fn edit_local_allow(
        &self,
        change: impl FnOnce(&mut toml_edit::Array) -> usize,
    ) -> std::io::Result<usize> {
        let text = match std::fs::read_to_string(&self.local_config) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e}")))?;
        let Some(allow) = doc
            .get_mut("permissions")
            .and_then(|permissions| permissions.get_mut("allow"))
            .and_then(|allow| allow.as_array_mut())
        else {
            return Ok(0);
        };
        let changed = change(allow);
        if changed > 0 {
            std::fs::write(&self.local_config, doc.to_string())?;
        }
        Ok(changed)
    }

    fn ctx<'a>(&'a self, cwd: &'a Path) -> MatchCtx<'a> {
        MatchCtx {
            project_root: &self.project_root,
            cwd,
            home: self.user_home.as_deref(),
        }
    }

    /// The directories agents may work in: the project root, then the extra ones.
    pub fn roots(&self) -> Vec<PathBuf> {
        read(&self.roots).clone()
    }

    /// Lets agents work in `dir` too, for the rest of the process. False when it already could.
    pub fn add_root(&self, dir: &Path) -> bool {
        let dir = canonical(dir);
        if self.inside_roots(&dir) {
            return false;
        }
        write(&self.roots).push(dir);
        true
    }

    fn inside_roots(&self, path: &Path) -> bool {
        let path = normal(path);
        read(&self.roots).iter().any(|root| {
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

    /// Steps that no mode, hook or rule can override: deny rules, what the agent's definition
    /// grants, the push guard, and `destructive = "deny"`. Also reports whether the call writes
    /// to a protected path, which forces a prompt in every mode but `bypass`.
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
        if self.destructive == DestructivePolicy::Deny
            && let Some(risk) = self.risk(call, &ctx)
        {
            return Err(format!(
                "this command {risk}; destructive commands are refused here (`[permissions] destructive = \"deny\"`)"
            ));
        }
        let protected = !self.bypass()
            && call.access == Access::Write
            && subject_kind(call.tool) == SubjectKind::Path
            && call.subjects.iter().any(|s| self.is_protected(&ctx.resolve(s)));
        Ok(protected)
    }

    /// The limits of [`PermissionState::precheck`] for text an agent types into a program in a
    /// terminal, taken as a command line: deny rules, the push guard, and `destructive =
    /// "deny"`. What the agent's definition grants was settled by the program it types into.
    pub fn precheck_typed(&self, call: &Call<'_>) -> Result<(), String> {
        let cwd = self.cwd_of(call.agent);
        let ctx = self.ctx(&cwd);
        if let Some(hit) = self.first_match(RuleKind::Deny, call, &ctx) {
            return Err(format!(
                "what you want to type is denied by rule `{}` ({})",
                hit.rule, hit.source
            ));
        }
        if let (Some(analysis), Some(command)) = (call.shell, call.command)
            && shell::pushes(command, analysis)
        {
            return Err(shell::PUSH_DENIED.to_string());
        }
        if self.destructive == DestructivePolicy::Deny
            && let Some(risk) = self.risk(call, &ctx)
        {
            return Err(format!(
                "what you want to type {risk}; destructive commands are refused here (`[permissions] destructive = \"deny\"`)"
            ));
        }
        Ok(())
    }

    /// What a shell call would destroy, whatever the policy is.
    fn risk(&self, call: &Call<'_>, ctx: &MatchCtx<'_>) -> Option<&'static str> {
        let (analysis, command) = (call.shell?, call.command?);
        shell::destructive(command, analysis, &|target| {
            !self.inside_roots(&ctx.resolve(target))
        })
    }

    /// What makes this call one that is asked about in every mode but `bypass`: the work a
    /// destructive command would destroy, in words that follow "this command". `None` for other
    /// calls, and when `[permissions] destructive` is `allow`.
    pub fn destructive(&self, call: &Call<'_>) -> Option<&'static str> {
        if self.destructive != DestructivePolicy::Ask || self.bypass() {
            return None;
        }
        let cwd = self.cwd_of(call.agent);
        self.risk(call, &self.ctx(&cwd))
    }

    fn secret_key(path: &Path) -> String {
        let key = normal(path);
        if cfg!(windows) {
            key.to_ascii_lowercase()
        } else {
            key
        }
    }

    /// Whether `path` is one of the files that hold credentials: the built-in list (`.env`,
    /// `*.pem`, `*.key`, SSH keys, the cloud and package credentials under the home directory,
    /// azazel's own login file) or a match of `permissions.secret_paths`. `path` is canonical,
    /// as [`MatchCtx::resolve`] makes it; a walk makes its root canonical once.
    pub fn is_secret(&self, path: &Path) -> bool {
        secrets::is_secret(path, &self.secret_patterns, &self.home, &self.ctx(&self.cwd))
    }

    /// Whether a tool that reads the files of a directory has to leave `path` out: it is a
    /// secret file the user has not allowed for this session, and the session is not in
    /// `bypass` mode. Naming the file itself is what gets the user asked.
    pub fn withholds(&self, path: &Path) -> bool {
        !self.bypass() && self.is_secret(path) && !read(&self.secret_grants).contains(&Self::secret_key(path))
    }

    /// The secret files a call names, as written and resolved: the path of a file tool, and the
    /// words of a shell command that are one (`cat .env`, `curl -d @.env`, `< ~/.ssh/id_rsa`).
    fn secrets_named(&self, call: &Call<'_>, ctx: &MatchCtx<'_>) -> Vec<(String, PathBuf)> {
        let words: Vec<String> = match (call.shell, call.command) {
            (Some(analysis), Some(command)) => shell::file_words(command, analysis)
                .into_iter()
                .flat_map(secrets::candidates)
                .collect(),
            _ if subject_kind(call.tool) == SubjectKind::Path => call.subjects.to_vec(),
            _ => return Vec::new(),
        };
        words
            .into_iter()
            .filter_map(|word| {
                let path = ctx.resolve(&word);
                secrets::is_secret(&path, &self.secret_patterns, &self.home, ctx).then_some((word, path))
            })
            .collect()
    }

    /// The secret file this call reads or writes without the user having allowed it for the
    /// session, as the call names it. Such a call is asked about in every mode but `bypass`,
    /// and can be allowed once or for the session, never for good.
    pub fn secret(&self, call: &Call<'_>) -> Option<String> {
        if self.bypass() {
            return None;
        }
        let cwd = self.cwd_of(call.agent);
        let named = self.secrets_named(call, &self.ctx(&cwd));
        let granted = read(&self.secret_grants);
        named
            .into_iter()
            .find(|(_, path)| !granted.contains(&Self::secret_key(path)))
            .map(|(word, _)| word)
    }

    /// Allows the secret files `call` names until the session ends or `/forget-approvals`.
    pub fn allow_secrets_for_session(&self, call: &Call<'_>) {
        let cwd = self.cwd_of(call.agent);
        let named = self.secrets_named(call, &self.ctx(&cwd));
        write(&self.secret_grants).extend(named.iter().map(|(_, path)| Self::secret_key(path)));
    }

    /// Plan mode, then (unless the session is in `bypass`) protected paths, ask rules, the mode
    /// and the allow rules. `protected` comes from [`PermissionState::precheck`]; `skip_prompt`
    /// is a hook's request to allow without asking. A destructive command and a call that names
    /// a secret file are asked about whatever the mode, the allow rules and `skip_prompt` say,
    /// and "always" is not offered.
    pub fn decide(&self, call: &Call<'_>, protected: bool, skip_prompt: bool) -> Verdict {
        match self.decide_by_rules(call, protected, skip_prompt) {
            Verdict::Allow | Verdict::Prompt { .. }
                if self.destructive(call).is_some() || self.secret(call).is_some() =>
            {
                Verdict::Prompt { rule: None }
            }
            verdict => verdict,
        }
    }

    fn decide_by_rules(&self, call: &Call<'_>, protected: bool, skip_prompt: bool) -> Verdict {
        let cwd = self.cwd_of(call.agent);
        let ctx = self.ctx(&cwd);
        let kind = subject_kind(call.tool);
        let prompt = || Verdict::Prompt {
            rule: self.suggest_rule(call, &ctx),
        };
        // Plan mode refuses before anything can ask: an approval must not turn into a way to
        // change things while planning.
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
        // Deny rules and the push guard have had their say in `precheck`; nothing else stands
        // in the way in bypass mode.
        if self.bypass() {
            return Verdict::Allow;
        }
        if protected {
            return Verdict::Prompt { rule: None };
        }
        if self.first_match(RuleKind::Ask, call, &ctx).is_some() {
            return if skip_prompt { Verdict::Allow } else { prompt() };
        }

        let paths_inside =
            || kind != SubjectKind::Path || call.subjects.iter().all(|s| self.inside_roots(&ctx.resolve(s)));
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
