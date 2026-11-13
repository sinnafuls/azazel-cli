//! Instruction files the user keeps for agents: `AGENTS.md` (and `CLAUDE.md` for compatibility).
//!
//! The files from the project root down to the working directory are part of the system prompt.
//! The ones in other directories join the result of the first tool call that touches a file
//! there. Any of them, and any rule file, may pull in other files with `@path`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use azazel_agent::AgentInfo;
use azazel_ai::Block;
use serde_json::Value;

use crate::ext::{Event, EventHandler, EventKind, ExtCtx, Outcome};
use crate::paths::{Paths, canonical};
use crate::permissions::PermissionState;
use crate::prompt::SystemPartProvider;
use crate::scheme::split_url;

/// Total size of context files placed in the system prompt.
pub const MAX_CONTEXT_BYTES: usize = 64 * 1024;
const TRUNCATED: &str = "\n[truncated: the context files exceed 64 KB]";
/// Tool calls whose paths are remembered until their result arrives.
const MAX_PENDING_CALLS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    pub path: PathBuf,
    pub text: String,
}

/// `text` cut to at most `budget` bytes at a character boundary, and whether it was cut.
fn cut(text: &str, budget: usize) -> (&str, bool) {
    if text.len() <= budget {
        return (text, false);
    }
    let mut end = budget;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// How `@path` in a context file or a rule file is followed. A line that is only `@path`, or a
/// word `@path` in a sentence, names a file whose text is placed there. The path is relative to
/// the file that names it, or to the project root. A word that names no file is left as it is,
/// and so is everything inside code.
#[derive(Debug, Clone)]
pub struct Includes {
    project_root: PathBuf,
    home: PathBuf,
    /// How many files deep an include may reach; 0 follows none.
    depth: u32,
}

/// What one top-level file has pulled in so far.
struct Expansion {
    /// The files being read, outermost first: a file named again here would never end.
    open: Vec<PathBuf>,
    /// Every file placed so far. A second mention stays a mention.
    placed: HashSet<PathBuf>,
    budget: usize,
}

impl Includes {
    pub fn new(paths: &Paths, depth: u32) -> Self {
        Includes {
            project_root: canonical(&paths.project_root),
            home: canonical(&paths.home),
            depth,
        }
    }

    /// The text of `file` with the files it names placed in it.
    pub fn expand(&self, file: &Path, text: &str) -> String {
        if self.depth == 0 || !text.contains('@') {
            return text.to_string();
        }
        let file = canonical(file);
        let mut expansion = Expansion {
            placed: HashSet::from([file.clone()]),
            open: vec![file],
            budget: MAX_CONTEXT_BYTES,
        };
        self.expand_in(text, &mut expansion)
    }

    fn expand_in(&self, text: &str, expansion: &mut Expansion) -> String {
        let mut out = String::with_capacity(text.len());
        let mut fence: Option<&str> = None;
        for line in text.split_inclusive('\n') {
            let trimmed = line.trim();
            let marker = ["```", "~~~"].into_iter().find(|mark| trimmed.starts_with(mark));
            match (fence, marker) {
                (None, Some(mark)) => fence = Some(mark),
                (Some(open), Some(mark)) if open == mark => fence = None,
                _ => {}
            }
            if fence.is_some() || marker.is_some() || !line.contains('@') {
                out.push_str(line);
                continue;
            }
            let blocks = self.blocks_of(line, expansion);
            // A line that is nothing but the name of a file gives way to the file.
            let alone =
                blocks.len() == 1 && trimmed.starts_with('@') && !trimmed.contains(char::is_whitespace);
            if !alone {
                out.push_str(line);
            }
            for block in blocks {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&block);
                out.push('\n');
            }
        }
        out
    }

    /// What the `@path` words of `line` stand for: the files, or why one was left out.
    fn blocks_of(&self, line: &str, expansion: &mut Expansion) -> Vec<String> {
        let mut blocks = Vec::new();
        let mut in_code = false;
        for word in line.split_whitespace() {
            let ticks = word.matches('`').count();
            let quoted = in_code || ticks > 0;
            if ticks % 2 == 1 {
                in_code = !in_code;
            }
            let Some(named) = word.strip_prefix('@').filter(|rest| !rest.is_empty() && !quoted) else {
                continue;
            };
            let including = expansion.open.last().cloned().unwrap_or_default();
            let Some(target) = self.resolve(named, &including) else {
                continue;
            };
            let shown = self.shown(&target);
            let refused = |why: &str| format!("[not included: {shown}: {why}]");
            if expansion.open.contains(&target) {
                blocks.push(refused("it includes itself"));
                continue;
            }
            if expansion.placed.contains(&target) {
                continue;
            }
            if let Some(why) = self.forbidden(&target, &including) {
                blocks.push(refused(why));
                continue;
            }
            if expansion.open.len() as u32 > self.depth {
                blocks.push(refused(&format!(
                    "includes are followed {} deep (`context.include_depth`)",
                    self.depth
                )));
                continue;
            }
            if expansion.budget == 0 {
                blocks.push(refused("the context files exceed 64 KB"));
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&target) else {
                continue;
            };
            let (text, was_cut) = cut(text.trim_end(), expansion.budget);
            expansion.budget -= text.len();
            expansion.placed.insert(target.clone());
            expansion.open.push(target);
            let inner = self.expand_in(text, expansion);
            expansion.open.pop();
            let note = if was_cut { TRUNCATED } else { "" };
            blocks.push(format!(
                "<include file=\"{shown}\">\n{}{note}\n</include>",
                inner.trim_end()
            ));
        }
        blocks
    }

    /// The file `named` stands for, when it is one. Marks that end a sentence are not part of
    /// the name unless a file is called that.
    fn resolve(&self, named: &str, including: &Path) -> Option<PathBuf> {
        let bare = named.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"']);
        [named, bare].into_iter().find_map(|name| {
            let name = Path::new(name);
            let candidates = match name.strip_prefix("~") {
                Ok(rest) => vec![std::env::home_dir()?.join(rest)],
                Err(_) if name.is_absolute() => vec![name.to_path_buf()],
                Err(_) => vec![
                    including.parent().unwrap_or(Path::new("")).join(name),
                    self.project_root.join(name),
                ],
            };
            candidates
                .into_iter()
                .find(|path| path.is_file())
                .map(|path| canonical(&path))
        })
    }

    /// Why `target` may not be placed in `including`. A file of the project may pull in files of
    /// the project only, so a repository cannot reach into the user's own files; nothing pulls in
    /// a file that holds credentials.
    fn forbidden(&self, target: &Path, including: &Path) -> Option<&'static str> {
        let in_project = target.starts_with(&self.project_root);
        let in_home = target.starts_with(&self.home);
        if !in_project && !in_home {
            return Some("only files of the project and of ~/.azazel can be included");
        }
        if !in_project && including.starts_with(&self.project_root) && !including.starts_with(&self.home) {
            return Some("a file of the project can include files of the project only");
        }
        crate::permissions::holds_credentials(target, &self.home, &self.project_root)
            .then_some("it holds credentials")
    }

    /// `path` as it is written in the prompt: relative to the project when it is inside it.
    fn shown(&self, path: &Path) -> String {
        path.strip_prefix(&self.project_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

/// The instruction file of `dir`: `AGENTS.md`, or `CLAUDE.md` when `claude_compat` is on and
/// there is no `AGENTS.md`.
fn file_in(dir: &Path, claude_compat: bool) -> Option<PathBuf> {
    let agents = dir.join("AGENTS.md");
    if agents.is_file() {
        return Some(agents);
    }
    Some(dir.join("CLAUDE.md")).filter(|claude| claude_compat && claude.is_file())
}

/// `~/.azazel/AGENTS.md`, then one file per directory from the project root down to the working
/// directory: `AGENTS.md`, or `CLAUDE.md` when `claude_compat` is on and there is no `AGENTS.md`.
/// What a file includes with `@path` is part of its text.
pub fn load(paths: &Paths, claude_compat: bool, includes: &Includes) -> Vec<ContextFile> {
    let mut candidates = vec![paths.home.join("AGENTS.md")];
    let mut dirs: Vec<&Path> = paths
        .cwd
        .ancestors()
        .take_while(|dir| dir.starts_with(&paths.project_root))
        .collect();
    dirs.reverse();
    if dirs.is_empty() {
        dirs.push(&paths.cwd);
    }
    candidates.extend(dirs.into_iter().filter_map(|dir| file_in(dir, claude_compat)));
    candidates
        .into_iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            (!text.trim().is_empty()).then(|| ContextFile {
                text: includes.expand(&path, &text),
                path,
            })
        })
        .collect()
}

/// Each file wrapped as `<context file="path">…</context>`. When the total exceeds
/// [`MAX_CONTEXT_BYTES`] the files nearest the root win: later ones are cut or dropped.
pub fn render(files: &[ContextFile]) -> Option<String> {
    let mut out = String::new();
    let mut budget = MAX_CONTEXT_BYTES;
    for file in files {
        if budget == 0 {
            break;
        }
        let (text, was_cut) = cut(file.text.trim_end(), budget);
        budget -= text.len();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("<context file=\"{}\">\n{text}", file.path.display()));
        if was_cut {
            out.push_str(TRUNCATED);
        }
        out.push_str("\n</context>");
    }
    if out.is_empty() {
        return None;
    }
    Some(format!(
        "# Project instructions\nThe user keeps these instructions for you. Follow them.\n\n{out}"
    ))
}

/// The `context_files` part of the system prompt. Files are re-read for every request, so edits
/// take effect on the next turn. Rules that always apply follow the files, inside the same cap.
pub struct ContextFilesPart {
    pub paths: Paths,
    pub claude_compat: bool,
    pub includes: Includes,
    pub rules: Option<std::sync::Arc<crate::rules::Rules>>,
}

#[async_trait]
impl SystemPartProvider for ContextFilesPart {
    async fn render(&self, agent: &AgentInfo) -> Option<String> {
        let mut files = load(&self.paths, self.claude_compat, &self.includes);
        if let Some(rules) = &self.rules {
            files.extend(rules.always_for(agent));
        }
        render(&files)
    }
}

#[derive(Default)]
struct NestedState {
    session: String,
    /// `(agent, call, paths)` of the calls whose result has not arrived.
    calls: VecDeque<(String, String, Vec<String>)>,
    /// `(agent, file)` of what each agent has been given.
    attached: HashSet<(String, PathBuf)>,
    /// Bytes each agent has been given.
    used: HashMap<String, usize>,
}

impl NestedState {
    fn enter(&mut self, session: &str) {
        if self.session != session {
            *self = NestedState {
                session: session.to_string(),
                ..NestedState::default()
            };
        }
    }
}

/// The instruction files of the directories below the working directory. An agent is given the
/// file of a directory with the result of its first tool call that names a path in it: the files
/// between that path and the project root, the nearest first, each once per agent and session
/// (again after a compaction, whose summary does not keep them). The files the system prompt
/// already carries are left out, and all of them together stay within [`MAX_CONTEXT_BYTES`].
pub struct NestedContext {
    paths: Paths,
    project_root: PathBuf,
    cwd: PathBuf,
    claude_compat: bool,
    includes: Includes,
    state: Mutex<NestedState>,
}

impl NestedContext {
    pub fn new(paths: &Paths, claude_compat: bool, includes: Includes) -> Self {
        NestedContext {
            project_root: canonical(&paths.project_root),
            cwd: canonical(&paths.cwd),
            paths: paths.clone(),
            claude_compat,
            includes,
            state: Mutex::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, NestedState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The instruction files that cover `path` and are not in the system prompt, the nearest
    /// first. `agent_cwd` is where the agent works: its own files are in its prompt already, or
    /// are the worktree's copy of what is.
    pub fn files_for(&self, path: &str, agent_cwd: &Path) -> Vec<PathBuf> {
        let agent_cwd = canonical(agent_cwd);
        let full = canonical(&agent_cwd.join(path.trim()));
        if !full.starts_with(&self.project_root) {
            return Vec::new();
        }
        let start = if full.is_dir() {
            Some(full.as_path())
        } else {
            full.parent()
        };
        let Some(start) = start else {
            return Vec::new();
        };
        start
            .ancestors()
            .take_while(|dir| {
                dir.starts_with(&self.project_root)
                    && *dir != self.project_root
                    && !self.cwd.starts_with(dir)
                    && !agent_cwd.starts_with(dir)
            })
            .filter_map(|dir| file_in(dir, self.claude_compat))
            .collect()
    }

    /// What joins the result of a call of `agent` that touched `paths`: the files it has not
    /// been given yet, as one block of text.
    pub fn attach(&self, agent: &AgentInfo, paths: &[String], agent_cwd: &Path) -> Option<String> {
        let mut files: Vec<PathBuf> = Vec::new();
        for path in paths {
            for file in self.files_for(path, agent_cwd) {
                if !files.contains(&file) {
                    files.push(file);
                }
            }
        }
        if files.is_empty() {
            return None;
        }
        let mut state = self.state();
        state.enter(&agent.session_id);
        files.retain(|file| !state.attached.contains(&(agent.id.clone(), file.clone())));
        if files.is_empty() {
            return None;
        }
        // The budget is the system prompt's: what the prompt's own files use is not there for these.
        let in_prompt: usize = load(&self.paths, self.claude_compat, &self.includes)
            .iter()
            .map(|file| file.text.trim_end().len())
            .sum();
        let used = state.used.get(&agent.id).copied().unwrap_or(0);
        let mut budget = MAX_CONTEXT_BYTES.saturating_sub(in_prompt).saturating_sub(used);
        let mut out = String::new();
        for file in files {
            state.attached.insert((agent.id.clone(), file.clone()));
            let Ok(raw) = std::fs::read_to_string(&file) else {
                continue;
            };
            if raw.trim().is_empty() {
                continue;
            }
            let shown = self.includes.shown(&canonical(&file));
            if budget == 0 {
                out.push_str(&format!(
                    "\n[not attached: {shown}: the context files exceed 64 KB; read it when you work there]"
                ));
                continue;
            }
            let text = self.includes.expand(&file, &raw);
            let (text, was_cut) = cut(text.trim_end(), budget);
            budget -= text.len();
            *state.used.entry(agent.id.clone()).or_default() += text.len();
            let note = if was_cut { TRUNCATED } else { "" };
            out.push_str(&format!("\n<context file=\"{shown}\">\n{text}{note}\n</context>"));
        }
        (!out.is_empty()).then(|| {
            format!(
                "The user keeps instructions for the directories you are now working in. Follow them \
                 for the files there.{out}"
            )
        })
    }

    /// Forgets what `agent` was given, so it is given it again.
    fn forget(&self, agent: &AgentInfo) {
        let mut state = self.state();
        state.attached.retain(|(owner, _)| *owner != agent.id);
        state.used.remove(&agent.id);
    }
}

/// The paths a call names: `path`, and each of `paths`. Addresses with a scheme are not files.
fn named_paths(input: &Value) -> Vec<String> {
    let one = input.get("path").and_then(Value::as_str);
    let many = input.get("paths").and_then(Value::as_array);
    one.into_iter()
        .chain(many.into_iter().flatten().filter_map(Value::as_str))
        .filter(|path| !path.trim().is_empty() && split_url(path).is_none())
        .map(str::to_string)
        .collect()
}

struct NestedHandler(Arc<NestedContext>);

#[async_trait]
impl EventHandler for NestedHandler {
    async fn handle(&self, event: &mut Event, ctx: &ExtCtx) -> Outcome {
        let nested = &self.0;
        match event {
            Event::ToolCall {
                agent,
                call_id,
                input,
                ..
            } => {
                let paths = named_paths(input);
                if !paths.is_empty() {
                    let mut state = nested.state();
                    state.enter(&agent.session_id);
                    if state.calls.len() >= MAX_PENDING_CALLS {
                        state.calls.pop_front();
                    }
                    state.calls.push_back((agent.id.clone(), call_id.clone(), paths));
                }
            }
            Event::ToolResult {
                agent,
                call_id,
                output,
                ..
            } => {
                let paths = {
                    let mut state = nested.state();
                    let at = state
                        .calls
                        .iter()
                        .position(|(owner, id, _)| *owner == agent.id && id == call_id);
                    at.and_then(|at| state.calls.remove(at))
                        .map(|(_, _, paths)| paths)
                };
                let Some(paths) = paths.filter(|_| !output.is_error) else {
                    return Outcome::Continue;
                };
                let cwd = match ctx.services.get::<PermissionState>() {
                    Some(permissions) => permissions.cwd_of(agent),
                    None => ctx.cwd.clone(),
                };
                if let Some(text) = nested.attach(agent, &paths, &cwd) {
                    output.content.push(Block::text(text));
                }
            }
            // The summary does not keep the files, so they are given again when next needed.
            Event::PostCompact { agent } => nested.forget(agent),
            _ => {}
        }
        Outcome::Continue
    }
}

/// Registers the `context_files` prompt part and, with `nested`, the files of the directories
/// agents come to work in.
pub struct ContextExt {
    pub paths: Paths,
    pub claude_compat: bool,
    /// The rule files, whose always-on rules belong to this part.
    pub rules: Option<std::sync::Arc<crate::rules::Rules>>,
    /// `context.nested`.
    pub nested: bool,
    /// `context.include_depth`.
    pub include_depth: u32,
}

impl crate::ext::Extension for ContextExt {
    fn id(&self) -> &str {
        "context-files"
    }

    fn register(&self, r: &mut crate::ext::Registrar) -> Result<(), crate::ext::ExtError> {
        let includes = Includes::new(&self.paths, self.include_depth);
        r.system_part(
            "context_files",
            Arc::new(ContextFilesPart {
                paths: self.paths.clone(),
                claude_compat: self.claude_compat,
                includes: includes.clone(),
                rules: self.rules.clone(),
            }),
        );
        if self.nested {
            let nested = Arc::new(NestedContext::new(&self.paths, self.claude_compat, includes));
            r.service(nested.clone());
            let handler: Arc<dyn EventHandler> = Arc::new(NestedHandler(nested));
            for kind in [EventKind::ToolCall, EventKind::ToolResult, EventKind::PostCompact] {
                r.on(kind, handler.clone());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("proj/crates/core");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(tmp.path().join("proj/.azazel")).unwrap();
        let paths = Paths::discover(&cwd, Some(&home.to_string_lossy()));
        (tmp, paths)
    }

    fn names(files: &[ContextFile], paths: &Paths) -> Vec<String> {
        // Canonical, as the loaded paths are: a temporary directory may sit behind a symlink.
        let base = crate::paths::canonical(paths.home.parent().unwrap());
        files
            .iter()
            .map(|f| {
                crate::paths::canonical(&f.path)
                    .strip_prefix(&base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn files_load_root_first_and_claude_md_only_fills_gaps() {
        let (_tmp, paths) = project();
        let root = paths.project_root.clone();
        std::fs::write(paths.home.join("AGENTS.md"), "global").unwrap();
        std::fs::write(root.join("AGENTS.md"), "root agents").unwrap();
        std::fs::write(root.join("CLAUDE.md"), "root claude, shadowed").unwrap();
        std::fs::write(root.join("crates/CLAUDE.md"), "crates claude").unwrap();
        std::fs::write(root.join("crates/core/AGENTS.md"), "core agents").unwrap();
        std::fs::write(
            root.parent().unwrap().join("AGENTS.md"),
            "above the project: ignored",
        )
        .unwrap();

        let files = load(&paths, true, &Includes::new(&paths, 4));
        assert_eq!(
            names(&files, &paths),
            [
                "home/AGENTS.md",
                "proj/AGENTS.md",
                "proj/crates/CLAUDE.md",
                "proj/crates/core/AGENTS.md"
            ]
        );
        let without_compat = load(&paths, false, &Includes::new(&paths, 4));
        assert_eq!(
            names(&without_compat, &paths),
            ["home/AGENTS.md", "proj/AGENTS.md", "proj/crates/core/AGENTS.md"]
        );

        let text = render(&files).unwrap();
        let positions: Vec<usize> = ["global", "root agents", "crates claude", "core agents"]
            .iter()
            .map(|t| text.find(t).unwrap())
            .collect();
        assert!(positions.is_sorted());
        assert!(text.contains(&format!(
            "<context file=\"{}\">\nroot agents\n</context>",
            root.join("AGENTS.md").display()
        )));
    }

    #[test]
    fn empty_or_missing_files_yield_no_part() {
        let (_tmp, paths) = project();
        assert_eq!(render(&load(&paths, true, &Includes::new(&paths, 4))), None);
        std::fs::write(paths.project_root.join("AGENTS.md"), "  \n").unwrap();
        assert_eq!(render(&load(&paths, true, &Includes::new(&paths, 4))), None);
    }

    #[test]
    fn the_cap_keeps_root_most_files_and_cuts_the_rest() {
        let file = |name: &str, size: usize| ContextFile {
            path: PathBuf::from(name),
            text: "q".repeat(size),
        };
        let text = render(&[file("a", 40_000), file("b", 40_000), file("c", 10)]).unwrap();
        assert_eq!(text.matches('q').count(), MAX_CONTEXT_BYTES);
        assert!(text.contains("<context file=\"a\">") && text.contains("<context file=\"b\">"));
        assert!(!text.contains("<context file=\"c\">"));
        assert_eq!(text.matches("[truncated").count(), 1);
    }
    fn put(root: &Path, file: &str, text: &str) {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_file_named_with_an_at_sign_is_placed_where_it_is_named() {
        let (_tmp, paths) = project();
        let root = paths.project_root.clone();
        put(&root, "docs/style.md", "Tabs, never spaces.\n");
        put(&root, "docs/testing.md", "Run `make test`.\nSee @style.md too.\n");
        put(&root, "crates/notes.md", "crate notes");
        put(
            &root,
            "AGENTS.md",
            "# Project\n@docs/testing.md\nKeep to @docs/style.md, and ask @maintainers about @docs/missing.md.\n\
             Mail a@docs/style.md is no include.\n",
        );
        let includes = Includes::new(&paths, 4);
        let text = includes.expand(
            &root.join("AGENTS.md"),
            &std::fs::read_to_string(root.join("AGENTS.md")).unwrap(),
        );
        assert_eq!(
            text,
            "# Project\n\
             <include file=\"docs/testing.md\">\nRun `make test`.\nSee @style.md too.\n\
             <include file=\"docs/style.md\">\nTabs, never spaces.\n</include>\n</include>\n\
             Keep to @docs/style.md, and ask @maintainers about @docs/missing.md.\n\
             Mail a@docs/style.md is no include.\n"
        );
        // A name is tried beside the file that writes it first, then from the project root.
        put(&root, "crates/AGENTS.md", "@notes.md\n@docs/style.md");
        assert_eq!(
            includes.expand(&root.join("crates/AGENTS.md"), "@notes.md\n@docs/style.md"),
            "<include file=\"crates/notes.md\">\ncrate notes\n</include>\n\
             <include file=\"docs/style.md\">\nTabs, never spaces.\n</include>\n"
        );
        // The files the prompt carries have their includes in them.
        let loaded = load(&paths, true, &includes);
        assert!(
            loaded
                .iter()
                .any(|file| file.text.contains("Tabs, never spaces."))
        );
        // Depth 0 follows nothing.
        let off = Includes::new(&paths, 0);
        assert_eq!(
            off.expand(&root.join("AGENTS.md"), "@docs/style.md"),
            "@docs/style.md"
        );
    }

    #[test]
    fn code_is_not_searched_for_includes() {
        let (_tmp, paths) = project();
        let root = paths.project_root.clone();
        put(&root, "a.md", "A");
        let text = "Use `@a.md` like this:\n```sh\n@a.md\n```\n~~~\n@a.md\n~~~\nand `x` @a.md `y`\n";
        assert_eq!(
            Includes::new(&paths, 4).expand(&root.join("AGENTS.md"), text),
            "Use `@a.md` like this:\n```sh\n@a.md\n```\n~~~\n@a.md\n~~~\nand `x` @a.md `y`\n\
             <include file=\"a.md\">\nA\n</include>\n"
        );
    }

    #[test]
    fn includes_stop_at_cycles_at_the_depth_and_at_the_edge_of_the_project() {
        let (tmp, paths) = project();
        let root = paths.project_root.clone();
        put(&root, "a.md", "A\n@b.md");
        put(&root, "b.md", "B\n@a.md\n@c.md");
        put(&root, "c.md", "C\n@d.md");
        put(&root, "d.md", "D");
        let agents = root.join("AGENTS.md");
        assert_eq!(
            Includes::new(&paths, 4).expand(&agents, "@a.md"),
            "<include file=\"a.md\">\nA\n\
             <include file=\"b.md\">\nB\n\
             [not included: a.md: it includes itself]\n\
             <include file=\"c.md\">\nC\n<include file=\"d.md\">\nD\n</include>\n</include>\n\
             </include>\n</include>\n"
        );
        assert_eq!(
            Includes::new(&paths, 2).expand(&agents, "@a.md"),
            "<include file=\"a.md\">\nA\n\
             <include file=\"b.md\">\nB\n\
             [not included: a.md: it includes itself]\n\
             [not included: c.md: includes are followed 2 deep (`context.include_depth`)]\n\
             </include>\n</include>\n"
        );
        // A file that names itself.
        assert_eq!(
            Includes::new(&paths, 4).expand(&agents, "see @AGENTS.md"),
            "see @AGENTS.md"
        );
        put(&root, "AGENTS.md", "see @AGENTS.md");
        assert_eq!(
            Includes::new(&paths, 4).expand(&agents, "see @AGENTS.md"),
            "see @AGENTS.md\n[not included: AGENTS.md: it includes itself]\n"
        );

        // Outside the project, and the user's own files from a file of the project.
        let outside = tmp.path().join("outside.md");
        std::fs::write(&outside, "not yours").unwrap();
        std::fs::write(paths.home.join("private.md"), "mine").unwrap();
        std::fs::write(paths.home.join("auth.json"), "{}").unwrap();
        put(&root, ".env", "KEY=1");
        let includes = Includes::new(&paths, 4);
        let text = includes.expand(
            &agents,
            &format!(
                "@../outside.md\n@{}\n@.env\n",
                paths.home.join("private.md").display()
            ),
        );
        let shown = |path: &Path| canonical(path).to_string_lossy().replace('\\', "/");
        assert_eq!(
            text,
            format!(
                "[not included: {}: only files of the project and of ~/.azazel can be included]\n\
                 [not included: {}: a file of the project can include files of the project only]\n\
                 [not included: .env: it holds credentials]\n",
                shown(&outside),
                shown(&paths.home.join("private.md"))
            )
        );
        // The user's own instructions may name the user's files and the project's, never the login.
        let own = includes.expand(&paths.home.join("AGENTS.md"), "@private.md\n@auth.json\n@d.md\n");
        assert!(own.contains("\">\nmine\n</include>"), "{own}");
        assert!(own.contains("auth.json: it holds credentials]"), "{own}");
        assert!(own.contains("<include file=\"d.md\">\nD\n</include>"), "{own}");
    }

    #[test]
    fn includes_share_the_budget_of_the_context_files() {
        let (_tmp, paths) = project();
        let root = paths.project_root.clone();
        put(&root, "big.md", &"#".repeat(60_000));
        put(&root, "more.md", &"%".repeat(10_000));
        put(&root, "last.md", "never reached");
        let text = Includes::new(&paths, 4).expand(&root.join("AGENTS.md"), "@big.md\n@more.md\n@last.md\n");
        assert_eq!(text.matches('#').count(), 60_000);
        assert_eq!(text.matches('%').count(), MAX_CONTEXT_BYTES - 60_000);
        assert_eq!(text.matches("[truncated").count(), 1);
        assert!(
            text.ends_with("[not included: last.md: the context files exceed 64 KB]\n"),
            "{}",
            &text[text.len() - 200..]
        );
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

    /// A project whose working directory is `crates/core`, with instruction files at the root, in
    /// the working directory and in three directories beside and below it.
    fn nested() -> (tempfile::TempDir, Paths, NestedContext) {
        let (tmp, paths) = project();
        let root = paths.project_root.clone();
        put(&root, "AGENTS.md", "root rules");
        put(&root, "crates/core/AGENTS.md", "core rules");
        put(&root, "crates/core/src/AGENTS.md", "src rules\n@../style.md");
        put(&root, "crates/core/style.md", "core style");
        put(&root, "docs/AGENTS.md", "docs rules");
        put(&root, "docs/api/CLAUDE.md", "api rules");
        put(&root, "docs/api/v2/notes.md", "notes");
        put(&root, "crates/core/src/lib.rs", "");
        let nested = NestedContext::new(&paths, true, Includes::new(&paths, 4));
        (tmp, paths, nested)
    }

    #[test]
    fn the_files_of_other_directories_come_with_the_first_call_that_touches_them() {
        let (_tmp, paths, nested) = nested();
        let lead = agent("lead");
        let cwd = paths.cwd.clone();
        // The root's file and the working directory's are in the prompt; the one below is not.
        let first = nested.attach(&lead, &["src/lib.rs".to_string()], &cwd).unwrap();
        assert_eq!(
            first,
            "The user keeps instructions for the directories you are now working in. Follow them for the \
             files there.\n\
             <context file=\"crates/core/src/AGENTS.md\">\nsrc rules\n\
             <include file=\"crates/core/style.md\">\ncore style\n</include>\n</context>"
        );
        // Once per agent: the next call there brings nothing.
        assert_eq!(nested.attach(&lead, &["src/lib.rs".to_string()], &cwd), None);
        assert_eq!(nested.attach(&lead, &["src".to_string()], &cwd), None);
        assert_eq!(
            nested.attach(
                &lead,
                &["AGENTS.md".to_string(), "../../AGENTS.md".to_string()],
                &cwd
            ),
            None
        );

        // Nearest first, `CLAUDE.md` where there is no `AGENTS.md`, a file that does not exist yet
        // included, and several paths in one call.
        let docs = nested
            .attach(
                &lead,
                &["../../docs/api/v2/new.md".to_string(), "src/lib.rs".to_string()],
                &cwd,
            )
            .unwrap();
        let (api, top) = (docs.find("api rules").unwrap(), docs.find("docs rules").unwrap());
        assert!(api < top, "{docs}");
        assert!(docs.contains("<context file=\"docs/api/CLAUDE.md\">"), "{docs}");
        assert!(
            !docs.contains("src rules") && !docs.contains("root rules"),
            "{docs}"
        );

        // Another agent has seen none of it; after a compaction neither has the first.
        let helper = agent("helper");
        assert!(
            nested
                .attach(&helper, &["../../docs/x.md".to_string()], &cwd)
                .unwrap()
                .contains("docs rules")
        );
        assert_eq!(nested.attach(&lead, &["../../docs/x.md".to_string()], &cwd), None);
        nested.forget(&lead);
        assert!(
            nested
                .attach(&lead, &["../../docs/x.md".to_string()], &cwd)
                .is_some()
        );
        assert_eq!(
            nested.attach(&helper, &["../../docs/x.md".to_string()], &cwd),
            None
        );

        // Nothing outside the project, and nothing without compatibility for `CLAUDE.md`.
        assert_eq!(
            nested.files_for("../../../elsewhere/x.md", &cwd),
            Vec::<PathBuf>::new()
        );
        let strict = NestedContext::new(&paths, false, Includes::new(&paths, 4));
        assert_eq!(
            strict.files_for("../../docs/api/v2/new.md", &cwd),
            [canonical(&paths.project_root).join("docs/AGENTS.md")]
        );
    }

    #[test]
    fn an_agent_in_a_worktree_is_not_given_the_worktree_copy_of_what_its_prompt_has() {
        let (_tmp, paths, nested) = nested();
        let tree = paths.project_root.join(".azazel/worktrees/a1");
        put(&tree, "AGENTS.md", "root rules");
        put(&tree, "docs/AGENTS.md", "docs rules");
        put(&tree, "docs/guide.md", "guide");
        let files = nested.files_for("docs/guide.md", &tree);
        assert_eq!(files, [canonical(&tree).join("docs/AGENTS.md")]);
    }

    #[test]
    fn nested_files_stay_within_what_the_prompt_leaves_of_the_budget() {
        let (_tmp, paths, nested) = nested();
        let root = paths.project_root.clone();
        put(&root, "AGENTS.md", &"p".repeat(40_000));
        put(&root, "docs/AGENTS.md", &"%".repeat(20_000));
        put(&root, "docs/api/CLAUDE.md", &"#".repeat(20_000));
        put(&root, "tools/AGENTS.md", "tools rules");
        let lead = agent("lead");
        // 40 000 bytes are the prompt's (and the working directory's ten): 25 526 are left.
        let left = MAX_CONTEXT_BYTES - 40_000 - "core rules".len();
        let text = nested
            .attach(&lead, &["../../docs/api/x.md".to_string()], &paths.cwd)
            .unwrap();
        assert_eq!(text.matches('#').count(), 20_000);
        assert_eq!(text.matches('%').count(), left - 20_000);
        assert_eq!(text.matches("[truncated").count(), 1);
        let later = nested
            .attach(&lead, &["../../tools/run.sh".to_string()], &paths.cwd)
            .unwrap();
        assert!(
            later.ends_with(
                "\n[not attached: tools/AGENTS.md: the context files exceed 64 KB; read it when you work there]"
            ),
            "{later}"
        );
    }

    #[test]
    fn the_paths_of_a_call_are_its_path_and_paths_and_no_addresses() {
        use serde_json::json;
        assert_eq!(
            named_paths(
                &json!({"path": "src/a.rs", "paths": ["b", 3, "https://x.dev/y", " "], "pattern": "p"})
            ),
            ["src/a.rs", "b"]
        );
        assert_eq!(
            named_paths(&json!({"path": "rule://style"})),
            Vec::<String>::new()
        );
        assert_eq!(named_paths(&json!({"command": "ls"})), Vec::<String>::new());
    }
}
