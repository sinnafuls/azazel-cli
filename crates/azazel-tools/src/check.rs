//! Checks on write: after `write` or `edit` changes a file, the linter for its kind runs on it.
//! Errors go into the tool result at once; everything found is kept until the lead stops, when
//! it is told what stands on the lines it changed.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::ToolCtx;
use azazel_core::config::CheckReport;
use azazel_core::paths::canonical;
use azazel_core::process::AgentEnv;
use azazel_core::trust::ESLINT_CONFIGS;
use azazel_core::{
    Event, EventHandler, EventKind, ExtCtx, ExtError, Extension, Level, Outcome, Registrar, Ui,
};
use process_wrap::tokio::{CommandWrap, KillOnDrop};
use serde_json::Value;
use similar::{DiffOp, TextDiff};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;

use crate::format::{FILE, words};
use crate::scan::SecretScan;
use crate::util::display_path;

/// How long a write waits for its checker. One that takes longer goes on in the background.
pub const TIMEOUT: Duration = Duration::from_secs(5);
/// How long a checker may run at all.
pub const LIMIT: Duration = Duration::from_secs(60);
/// Checkers running at the same time, over all agents.
const CONCURRENT: usize = 3;
/// Larger files are not checked.
const MAX_FILE_BYTES: usize = 1024 * 1024;
/// Errors listed in the result of one write.
const MAX_WRITE_ERRORS: usize = 10;
/// Findings listed in the report at the end of a turn.
const MAX_REPORTED: usize = 30;
/// What is kept of each output stream of a checker.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// What is kept of one message.
const MAX_MESSAGE_CHARS: usize = 300;
/// How long a checker's output is read after the checker itself has exited.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(300);
/// How long a checker that was ended is waited for.
const REAP: Duration = Duration::from_secs(1);
/// How long comparing two versions of a file may take when working out the changed lines.
const DIFF_TIMEOUT: Duration = Duration::from_millis(250);

/// The built-in checkers, for `azazel doctor` and the documentation: the name an agent is told,
/// the program looked for on `PATH`, the files it checks, and whether checking runs code of the
/// project (its build scripts and macros, its lint configuration), which only a trusted project
/// may have done for it.
pub const BUILTIN: [(&str, &str, &str, bool); 8] = [
    ("clippy", "cargo-clippy", "rs", true),
    ("ruff", "ruff", "py, pyi", false),
    ("eslint", "eslint", "js, ts", true),
    ("oxlint", "oxlint", "js, ts", false),
    ("shellcheck", "shellcheck", "sh, bash", false),
    ("go vet", "go", "go", true),
    ("actionlint", "actionlint", "workflows", false),
    ("hadolint", "hadolint", "Dockerfile", false),
];

const SCRIPT_EXTENSIONS: [&str; 8] = ["js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    /// Advice: what a checker calls a note, info, style or help.
    Note,
}

impl Severity {
    fn word(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

/// One thing a checker said about a file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    /// 1-based.
    pub line: u32,
    /// 1-based; 0 when the checker named no column.
    pub column: u32,
    pub severity: Severity,
    pub message: String,
}

impl Finding {
    /// `path:line:column severity message [checker]`, the shape of a language server's
    /// diagnostics.
    fn render(&self, shown: &str, checker: &str) -> String {
        let column = match self.column {
            0 => String::new(),
            column => format!(":{column}"),
        };
        format!(
            "{shown}:{}{column} {} {} [{checker}]",
            self.line,
            self.severity.word(),
            self.message
        )
    }
}

/// How a checker writes its findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    /// `file:line[:column]: [severity:] message`: gcc, rustc's short format, `shellcheck -f gcc`.
    /// A line that names no severity is a warning.
    Gcc,
    /// `ruff check --output-format concise`: no severity; a syntax error is an error.
    Ruff,
    /// eslint's `unix` format, which oxlint also writes: the message ends in `[Error/rule]` or
    /// `[Warning/rule]`.
    Unix,
    /// `eslint -f json`.
    Eslint,
    /// `go vet`: what the compiler says is prefixed `vet: ` and is an error.
    GoVet,
    /// `actionlint -oneline`: everything is an error.
    Actionlint,
    /// `file:line CODE severity: message`.
    Hadolint,
}

/// `path:line[:column]` at the start of `text`, and what follows it. A drive letter's colon is
/// not taken for the one before the line number.
fn location(text: &str) -> Option<(&str, u32, u32, &str)> {
    let mut from = 0;
    while let Some(at) = text[from..].find(':') {
        let colon = from + at;
        from = colon + 1;
        let rest = &text[from..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let Ok(line) = rest[..digits].parse::<u32>() else {
            continue;
        };
        if colon == 0 {
            continue;
        }
        let after = &rest[digits..];
        // `closed`: a colon ended the location, so anything may follow it.
        let (column, after, closed) = match after.strip_prefix(':') {
            Some(more) => {
                let digits = more.bytes().take_while(u8::is_ascii_digit).count();
                match (more[..digits].parse::<u32>(), more[digits..].strip_prefix(':')) {
                    (Ok(column), Some(rest)) => (column, rest, true),
                    (Ok(column), None) => (column, &more[digits..], false),
                    (Err(_), _) => (0, more, true),
                }
            }
            None => (0, after, false),
        };
        if closed || after.is_empty() || after.starts_with(char::is_whitespace) {
            return Some((&text[..colon], line, column, after.trim_start()));
        }
    }
    None
}

/// A leading `error:`, `warning[E0123]:`, `note:` and the like, with the code in brackets and
/// what follows the colon.
fn severity(text: &str) -> Option<(Severity, Option<&str>, &str)> {
    let (head, rest) = text.split_once(':')?;
    let (word, code) = match head.split_once('[') {
        Some((word, code)) => (word, code.strip_suffix(']')),
        None => (head, None),
    };
    let severity = match word.trim().to_ascii_lowercase().as_str() {
        "error" | "fatal error" | "fatal" => Severity::Error,
        "warning" | "warn" => Severity::Warning,
        "note" | "info" | "information" | "style" | "help" | "hint" => Severity::Note,
        _ => return None,
    };
    Some((severity, code, rest.trim_start()))
}

/// `message` on one line and no longer than [`MAX_MESSAGE_CHARS`], with `code` after it.
fn message(message: &str, code: Option<&str>) -> String {
    let mut out = String::new();
    for word in message.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    if out.chars().count() > MAX_MESSAGE_CHARS {
        out = out.chars().take(MAX_MESSAGE_CHARS).collect();
        out.push('…');
    }
    match code {
        Some(code) if !code.is_empty() => format!("{out} [{code}]"),
        _ => out,
    }
}

/// One line of a checker's output as the file it names and the finding, when it is one.
fn parse_line(style: Style, line: &str) -> Option<(String, Finding)> {
    let line = line.trim();
    let (line, compiler) = match (style, line.strip_prefix("vet: ")) {
        (Style::GoVet, Some(rest)) => (rest, true),
        _ => (line, false),
    };
    let (path, row, column, rest) = location(line)?;
    let (severity, text) = match style {
        Style::Gcc => match severity(rest) {
            Some((severity, code, text)) => (severity, message(text, code)),
            None => (Severity::Warning, message(rest, None)),
        },
        Style::Ruff => {
            let syntax = ["SyntaxError", "invalid-syntax", "E999"]
                .iter()
                .any(|mark| rest.starts_with(mark));
            let severity = if syntax {
                Severity::Error
            } else {
                Severity::Warning
            };
            (severity, message(rest, None))
        }
        Style::Unix => {
            let tagged = rest
                .strip_suffix(']')
                .and_then(|open| open.rsplit_once(" ["))
                .and_then(|(text, tag)| {
                    let (level, rule) = tag.split_once('/').unwrap_or((tag, ""));
                    let severity = match level {
                        "Error" => Severity::Error,
                        "Warning" => Severity::Warning,
                        _ => return None,
                    };
                    Some((severity, message(text, Some(rule))))
                });
            tagged.unwrap_or_else(|| (Severity::Warning, message(rest, None)))
        }
        Style::GoVet if compiler => (Severity::Error, message(rest, None)),
        Style::GoVet => (Severity::Warning, message(rest, None)),
        Style::Actionlint => (Severity::Error, message(rest, None)),
        Style::Hadolint => {
            let (code, said) = rest.split_once(' ')?;
            let (severity, _, text) = severity(said)?;
            (severity, message(text, Some(code)))
        }
        Style::Eslint => return None,
    };
    if text.is_empty() {
        return None;
    }
    let finding = Finding {
        line: row,
        column,
        severity,
        message: text,
    };
    Some((path.to_string(), finding))
}

/// What `eslint -f json` printed, as files and findings.
fn parse_eslint(output: &str) -> Vec<(String, Finding)> {
    let report = output
        .lines()
        .find_map(|line| serde_json::from_str::<Value>(line.trim()).ok());
    let mut found = Vec::new();
    for file in report.as_ref().and_then(Value::as_array).into_iter().flatten() {
        let Some(path) = file["filePath"].as_str() else {
            continue;
        };
        for item in file["messages"].as_array().into_iter().flatten() {
            let number = |name: &str| item[name].as_u64().and_then(|n| u32::try_from(n).ok());
            let severity = match item["severity"].as_u64() {
                Some(2) => Severity::Error,
                _ => Severity::Warning,
            };
            let text = message(
                item["message"].as_str().unwrap_or_default(),
                item["ruleId"].as_str(),
            );
            if text.is_empty() {
                continue;
            }
            found.push((
                path.to_string(),
                Finding {
                    line: number("line").unwrap_or(1),
                    column: number("column").unwrap_or(0),
                    severity,
                    message: text,
                },
            ));
        }
    }
    found
}

fn parse(style: Style, stdout: &str, stderr: &str) -> Vec<(String, Finding)> {
    match style {
        Style::Eslint => parse_eslint(stdout),
        _ => stdout
            .lines()
            .chain(stderr.lines())
            .filter_map(|line| parse_line(style, line))
            .collect(),
    }
}

/// A checker for one file, ready to run.
#[derive(Debug, Clone)]
struct Runner {
    /// What the agent is told the checker is called.
    name: String,
    program: PathBuf,
    arguments: Vec<String>,
    cwd: PathBuf,
    style: Style,
    /// The directories a relative path in the output may start from.
    bases: Vec<PathBuf>,
}

/// The name `[tools.check]` knows the file's kind by: its extension, or `dockerfile`.
fn kind_of(file: &Path) -> Option<String> {
    let name = file.file_name()?.to_str()?;
    if name == "Dockerfile" || name.starts_with("Dockerfile.") {
        return Some("dockerfile".to_string());
    }
    Some(file.extension()?.to_str()?.to_ascii_lowercase())
}

fn is_workflow(file: &Path) -> bool {
    let names: Vec<&std::ffi::OsStr> = file
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    names
        .windows(2)
        .any(|pair| pair[0] == ".github" && pair[1] == "workflows")
}

/// The directory and the name of the package `file` belongs to: the nearest `Cargo.toml` above
/// it, when that is a package's and not only a workspace's.
fn rust_package(file: &Path) -> Option<(PathBuf, String)> {
    let dir = file
        .ancestors()
        .skip(1)
        .find(|dir| dir.join("Cargo.toml").is_file())?;
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "name"
        {
            let name = value.trim().trim_start_matches(['"', '\'']);
            let name = name.split(['"', '\'']).next().unwrap_or_default();
            return (!name.is_empty()).then(|| (dir.to_path_buf(), name.to_string()));
        }
    }
    None
}

/// The directory of the workspace the package in `package` is a member of, which the paths in
/// cargo's messages start from.
fn workspace_root(package: &Path) -> PathBuf {
    let states_workspace = |dir: &&Path| {
        std::fs::read_to_string(dir.join("Cargo.toml")).is_ok_and(|manifest| {
            manifest.lines().any(|line| {
                let line = line.trim();
                line == "[workspace]" || line.starts_with("[workspace.")
            })
        })
    };
    package
        .ancestors()
        .find(states_workspace)
        .unwrap_or(package)
        .to_path_buf()
}

fn uses_eslint(file: &Path, root: &Path) -> bool {
    file.ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(root))
        .any(|dir| ESLINT_CONFIGS.iter().any(|name| dir.join(name).is_file()))
}

/// The built-in checker for a file of `kind`, when its program is installed. A checker that runs
/// the project's code is one only when the project is `trusted`.
fn builtin(
    kind: &str,
    file: &Path,
    cwd: &Path,
    (root, trusted): (&Path, bool),
    find: &dyn Fn(&str) -> Option<PathBuf>,
) -> Option<Runner> {
    let path = file.to_string_lossy().into_owned();
    let on_file = |name: &str, program: PathBuf, arguments: &[&str], style: Style| Runner {
        name: name.to_string(),
        program,
        arguments: arguments
            .iter()
            .map(ToString::to_string)
            .chain([path.clone()])
            .collect(),
        cwd: cwd.to_path_buf(),
        style,
        bases: vec![cwd.to_path_buf()],
    };
    match kind {
        // cargo builds the package's build scripts and procedural macros, and runs them.
        "rs" if trusted => {
            let (package, name) = rust_package(file)?;
            find("cargo-clippy")?;
            let mut arguments = vec![
                "clippy".to_string(),
                "-p".to_string(),
                name,
                "--message-format=short".to_string(),
            ];
            // Tests, benches and examples are built only when asked for.
            let beside_src = file
                .strip_prefix(&package)
                .ok()
                .and_then(|inside| inside.components().next())
                .is_some_and(|first| {
                    ["tests", "benches", "examples"]
                        .iter()
                        .any(|dir| first.as_os_str() == *dir)
                });
            if beside_src {
                arguments.push("--all-targets".to_string());
            }
            Some(Runner {
                name: "clippy".to_string(),
                program: find("cargo")?,
                arguments,
                bases: vec![workspace_root(&package), package.clone()],
                cwd: package,
                style: Style::Gcc,
            })
        }
        "py" | "pyi" => Some(on_file(
            "ruff",
            find("ruff")?,
            &["check", "--output-format", "concise", "--no-fix"],
            Style::Ruff,
        )),
        "sh" | "bash" => Some(on_file(
            "shellcheck",
            find("shellcheck")?,
            &["-f", "gcc"],
            Style::Gcc,
        )),
        // go builds the package, cgo and all.
        "go" if trusted => {
            let package = file.parent()?.to_path_buf();
            Some(Runner {
                name: "go vet".to_string(),
                program: find("go")?,
                arguments: vec!["vet".to_string(), ".".to_string()],
                bases: vec![package.clone()],
                cwd: package,
                style: Style::GoVet,
            })
        }
        "yml" | "yaml" if is_workflow(file) => Some(on_file(
            "actionlint",
            find("actionlint")?,
            &["-oneline"],
            Style::Actionlint,
        )),
        "dockerfile" => Some(on_file(
            "hadolint",
            find("hadolint")?,
            &["--no-color"],
            Style::Hadolint,
        )),
        script if SCRIPT_EXTENSIONS.contains(&script) => {
            // eslint loads the project's configuration, which is a program; oxlint only reads
            // the file.
            let eslint = (trusted && uses_eslint(file, root))
                .then(|| find("eslint"))
                .flatten();
            match eslint {
                Some(eslint) => Some(on_file("eslint", eslint, &["-f", "json"], Style::Eslint)),
                None => Some(on_file("oxlint", find("oxlint")?, &["-f", "unix"], Style::Unix)),
            }
        }
        _ => None,
    }
}

/// The built-in checkers that are installed, each as `name (the files it checks)` and whether it
/// is used: one that runs the project's code is not while the project is not `trusted`.
pub fn installed(trusted: bool) -> Vec<(String, bool)> {
    let found = |program: &str| which::which(program).is_ok();
    BUILTIN
        .iter()
        // clippy is a subcommand of cargo.
        .filter(|(_, program, _, _)| found(program) && (*program != "cargo-clippy" || found("cargo")))
        .map(|(name, _, kinds, runs_code)| (format!("{name} ({kinds})"), trusted || !runs_code))
        .collect()
}

/// The kinds of file whose checker in `[tools.check]` is not installed, as `(kind, program)`.
pub fn missing(configured: &BTreeMap<String, String>) -> Vec<(String, String)> {
    configured
        .iter()
        .filter_map(|(kind, command)| {
            let program = words(command).into_iter().next()?;
            which::which(&program).is_err().then(|| (kind.clone(), program))
        })
        .collect()
}

/// The 1-based, inclusive line ranges of `new` that this turn changed: the lines that differ
/// from `old`, the lines on either side of a removal, and the lines of `before` (ranges in `old`)
/// where they are now.
fn touched_lines(old: &str, new: &str, before: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let line = |index: usize| u32::try_from(index).unwrap_or(u32::MAX);
    let diff = TextDiff::configure().timeout(DIFF_TIMEOUT).diff_lines(old, new);
    let mut spans = Vec::new();
    for op in diff.ops() {
        match *op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                let (first, last) = (line(old_index + 1), line(old_index + len));
                for &(start, end) in before {
                    let (start, end) = (start.max(first), end.min(last));
                    if start <= end {
                        let moved = |at: u32| (at - first).saturating_add(line(new_index + 1));
                        spans.push((moved(start), moved(end)));
                    }
                }
            }
            DiffOp::Insert {
                new_index, new_len, ..
            }
            | DiffOp::Replace {
                new_index, new_len, ..
            } => spans.push((line(new_index + 1), line(new_index + new_len))),
            // What a removed line breaks shows on the lines that now meet where it was.
            DiffOp::Delete { new_index, .. } => {
                let last = line(new.lines().count());
                let (above, below) = (line(new_index).max(1), line(new_index + 1).min(last));
                if above <= below {
                    spans.push((above, below));
                }
            }
        }
    }
    spans.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// `2 errors, 1 warning`.
fn counts(findings: &[&Finding]) -> String {
    let mut parts = Vec::new();
    for (severity, one, many) in [
        (Severity::Error, "error", "errors"),
        (Severity::Warning, "warning", "warnings"),
        (Severity::Note, "note", "notes"),
    ] {
        match findings.iter().filter(|f| f.severity == severity).count() {
            0 => {}
            1 => parts.push(format!("1 {one}")),
            n => parts.push(format!("{n} {many}")),
        }
    }
    parts.join(", ")
}

fn plural(n: usize, one: &str, many: &str) -> String {
    match n {
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    }
}

/// `5 s`, or `300 ms` for a part of a second.
fn human(time: Duration) -> String {
    match time.subsec_millis() {
        0 => format!("{} s", time.as_secs()),
        _ => format!("{} ms", time.as_millis()),
    }
}

/// What this turn knows of one file an agent wrote.
#[derive(Debug, Default)]
struct Written {
    /// The file as the agent named it, made absolute.
    path: PathBuf,
    checker: String,
    /// The line ranges changed in this turn, as the file is numbered now.
    touched: Vec<(u32, u32)>,
    /// What the checker found in the file as last written.
    findings: Vec<Finding>,
    /// Why the file as last written was not checked.
    unchecked: Option<String>,
}

#[derive(Default)]
struct Turn {
    /// Which turn of the session this is. A check that outlives its turn is dropped.
    number: u64,
    files: BTreeMap<PathBuf, Written>,
    /// The lead has had this turn's report.
    reported: bool,
}

/// What a run of a checker came to, before anything is picked out for one file.
enum Ran {
    /// Every finding, with the file the checker named for it.
    Found(Vec<(String, Finding)>),
    TimedOut,
    Failed(String),
}

/// A written file waiting for its check.
struct Job {
    runner: Runner,
    file: PathBuf,
    /// The turn the file was written in.
    turn: u64,
    env: Option<Arc<AgentEnv>>,
}

/// A checker, the kind of file it is for and the directory it runs in. One run of it there goes
/// on in the background at a time.
type Place = (String, String, PathBuf);

/// The background run of a checker in one place.
struct Slot {
    id: u64,
    task: JoinHandle<()>,
    /// The files written while it ran, checked when it is done.
    queued: Vec<Job>,
}

impl Slot {
    /// A file written twice is checked once, as it is now.
    fn queue(&mut self, job: Job) {
        self.queued.retain(|queued| queued.file != job.file);
        self.queued.push(job);
    }
}

/// Ends a run when it is let go of before it has finished.
struct Run(Option<JoinHandle<Ran>>);

impl Run {
    /// What the run came to, however long that takes.
    async fn finish(mut self, name: &str) -> Ran {
        match &mut self.0 {
            Some(run) => run
                .await
                .unwrap_or_else(|_| Ran::Failed(format!("{name} could not be run"))),
            None => Ran::Failed(format!("{name} could not be run")),
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Some(run) = &self.0 {
            run.abort();
        }
    }
}

/// The queued files grouped by command line, in the order they were written: one run of a
/// checker that looks at a whole package (clippy) serves every file of the package.
fn batches(queued: Vec<Job>) -> Vec<Vec<Job>> {
    let mut batches: Vec<Vec<Job>> = Vec::new();
    for job in queued {
        let same = batches.iter_mut().find(|batch| {
            let first = &batch[0].runner;
            first.program == job.runner.program
                && first.arguments == job.runner.arguments
                && first.cwd == job.runner.cwd
        });
        match same {
            Some(batch) => batch.push(job),
            None => batches.push(vec![job]),
        }
    }
    batches
}

/// Runs `runner` until it ends or `limit` has passed, when it is ended with everything it
/// started.
async fn execute(
    runner: Runner,
    env: Option<Arc<AgentEnv>>,
    permits: Arc<Semaphore>,
    limit: Duration,
) -> Ran {
    let name = &runner.name;
    let Ok(_permit) = permits.acquire().await else {
        return Ran::Failed(format!("{name} could not be started"));
    };
    // Its own process group or job, so that what it starts (cargo's compilers) ends with it.
    let mut command = CommandWrap::with_new(&runner.program, |command| {
        command
            .args(&runner.arguments)
            .current_dir(&runner.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(env) = &env {
            command.envs(env.vars());
        }
    });
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(process_wrap::tokio::JobObject);
    command.wrap(KillOnDrop);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return Ran::Failed(format!("{name} could not be started: {e}")),
    };
    let (stdout, stderr) = (
        Capture::of(child.stdout().take()),
        Capture::of(child.stderr().take()),
    );
    let Ok(status) = tokio::time::timeout(limit, child.wait()).await else {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(REAP, child.wait()).await;
        return Ran::TimedOut;
    };
    let status = match status {
        Ok(status) => status,
        Err(e) => return Ran::Failed(format!("{name} could not be waited for: {e}")),
    };
    let (stdout, stderr) = tokio::join!(stdout.text(), stderr.text());
    let found = parse(runner.style, &stdout, &stderr);
    if found.is_empty() && !status.success() {
        let why = stderr
            .lines()
            .chain(stdout.lines())
            .map(str::trim)
            .find(|line| !line.is_empty());
        return Ran::Failed(match why {
            Some(why) => {
                let why: String = why.chars().take(200).collect();
                format!("{name} failed: {why}")
            }
            None => format!("{name} failed ({status})"),
        });
    }
    Ran::Found(found)
}

/// What `ran` says of `file`: the findings about it, in order, or why it was not checked.
fn outcome(runner: &Runner, file: &Path, ran: &Ran, limit: Duration) -> Result<Vec<Finding>, String> {
    let found = match ran {
        Ran::Found(found) => found,
        Ran::TimedOut => return Err(format!("{} did not finish within {}", runner.name, human(limit))),
        Ran::Failed(why) => return Err(why.clone()),
    };
    let file = canonical(file);
    let mut same: HashMap<&str, bool> = HashMap::new();
    let mut findings: Vec<Finding> = found
        .iter()
        .filter(|(path, _)| {
            *same.entry(path.as_str()).or_insert_with(|| {
                let named = Path::new(path);
                if named.is_absolute() {
                    canonical(named) == file
                } else {
                    runner
                        .bases
                        .iter()
                        .any(|base| canonical(&base.join(named)) == file)
                }
            })
        })
        .map(|(_, finding)| finding.clone())
        .collect();
    findings.sort();
    findings.dedup();
    Ok(findings)
}

/// What a check adds to the result of a write.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Checked {
    /// Why the file was not checked, for the summary line.
    pub note: Option<String>,
    /// The `checks:` block: the errors found.
    pub block: Option<String>,
}

/// What the lead is told when it stops.
#[derive(Debug, PartialEq, Eq)]
pub struct Report {
    /// For the agent.
    pub text: String,
    /// One line for the user.
    pub summary: String,
}

/// The checkers of a session and what they found in the current turn. Registered as a service;
/// `write` and `edit` check only when it is there.
pub struct Checks {
    /// The project root: a script's eslint config is looked for up to here.
    root: PathBuf,
    on_write: bool,
    /// `[tools.check]`: file extension (without the dot) to command.
    configured: BTreeMap<String, String>,
    /// Whether the project is trusted: only then do the checkers run that run its code.
    trusted: bool,
    report: CheckReport,
    timeout: Duration,
    limit: Duration,
    running: Arc<Semaphore>,
    turn: Mutex<Turn>,
    /// The checks that outlasted the write they were for.
    background: Mutex<HashMap<Place, Slot>>,
    /// How many places have a check in the background.
    busy: watch::Sender<usize>,
    slots: AtomicU64,
    secrets: Option<Arc<SecretScan>>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a checker writes to one of its output streams, read as it comes. The first
/// [`MAX_OUTPUT_BYTES`] are kept.
struct Capture {
    kept: Arc<Mutex<Vec<u8>>>,
    reader: Option<tokio::task::JoinHandle<()>>,
}

impl Capture {
    fn of(pipe: Option<impl AsyncRead + Unpin + Send + 'static>) -> Capture {
        let kept = Arc::new(Mutex::new(Vec::new()));
        let reader = pipe.map(|mut pipe| {
            let kept = kept.clone();
            tokio::spawn(async move {
                let mut chunk = [0u8; 8192];
                while let Ok(n @ 1..) = pipe.read(&mut chunk).await {
                    let mut kept = locked(&kept);
                    let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
            })
        });
        Capture { kept, reader }
    }

    /// What was written, once the checker has exited. Something the checker started may still
    /// hold the stream open; it is given [`DRAIN_AFTER_EXIT`] to finish and then left behind.
    async fn text(mut self) -> String {
        if let Some(reader) = &mut self.reader {
            let _ = tokio::time::timeout(DRAIN_AFTER_EXIT, reader).await;
        }
        String::from_utf8_lossy(&locked(&self.kept)).into_owned()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(reader) = &self.reader {
            reader.abort();
        }
    }
}

impl Checks {
    pub fn new(ext: &ChecksExt) -> Checks {
        Checks {
            root: ext.root.clone(),
            on_write: ext.on_write,
            configured: ext.configured.clone(),
            trusted: ext.trusted,
            report: ext.report,
            timeout: ext.timeout,
            limit: ext.limit,
            running: Arc::new(Semaphore::new(CONCURRENT)),
            turn: Mutex::default(),
            background: Mutex::default(),
            busy: watch::Sender::new(0),
            slots: AtomicU64::new(0),
            secrets: ext
                .gitleaks
                .clone()
                .map(|program| Arc::new(SecretScan::new(program, ext.root.clone()))),
        }
    }

    /// The checker for `file`: the configured one for its kind, else the built-in one when its
    /// program is installed. `find` looks a program up on `PATH`.
    fn runner(
        &self,
        file: &Path,
        cwd: &Path,
        find: &dyn Fn(&str) -> Option<PathBuf>,
    ) -> Option<Result<Runner, String>> {
        let kind = kind_of(file)?;
        let Some(configured) = self.configured.get(&kind) else {
            return builtin(&kind, file, cwd, (&self.root, self.trusted), find).map(Ok);
        };
        let mut words = words(configured);
        if words.is_empty() {
            return None;
        }
        let name = words.remove(0);
        let Some(program) = find(&name) else {
            return Some(Err(format!("checker `{name}` was not found")));
        };
        let path = file.to_string_lossy().into_owned();
        let named = words.iter().any(|word| word.contains(FILE));
        let mut arguments: Vec<String> = words.iter().map(|word| word.replace(FILE, &path)).collect();
        if !named {
            arguments.push(path);
        }
        Some(Ok(Runner {
            name: program
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or(name),
            program,
            arguments,
            cwd: cwd.to_path_buf(),
            style: Style::Gcc,
            bases: vec![cwd.to_path_buf()],
        }))
    }

    /// Records that `file` was written in this turn, which of its lines the turn changed, and
    /// what its checker found or why it was not checked.
    fn record(
        &self,
        file: &Path,
        checker: &str,
        (old, new): (Option<&str>, &str),
        outcome: Result<Vec<Finding>, String>,
    ) {
        let mut turn = locked(&self.turn);
        let written = turn.files.entry(canonical(file)).or_default();
        written.path = file.to_path_buf();
        written.checker = checker.to_string();
        written.touched = match old {
            Some(old) => touched_lines(old, new, &written.touched),
            None => vec![(1, u32::try_from(new.lines().count().max(1)).unwrap_or(u32::MAX))],
        };
        match outcome {
            Ok(findings) => {
                written.findings = findings;
                written.unchecked = None;
            }
            Err(why) => {
                written.findings.clear();
                written.unchecked = Some(why);
            }
        }
    }

    /// What a check that ran after its write found, when the file is still as that turn wrote
    /// it.
    fn settle(&self, job: &Job, ran: &Ran) {
        let outcome = outcome(&job.runner, &job.file, ran, self.limit);
        let mut turn = locked(&self.turn);
        if turn.number != job.turn {
            return;
        }
        if let Some(written) = turn.files.get_mut(&canonical(&job.file)) {
            match outcome {
                Ok(findings) => {
                    written.findings = findings;
                    written.unchecked = None;
                }
                Err(why) => {
                    written.findings.clear();
                    written.unchecked = Some(why);
                }
            }
        }
    }

    /// Sees the run for `first` through in the background, then checks the files written in
    /// its place meanwhile, until none is left.
    async fn supervise(self: Arc<Self>, place: Place, id: u64, first: Job, run: Run) {
        let ran = run.finish(&first.runner.name).await;
        self.settle(&first, &ran);
        loop {
            let queued = {
                let mut background = locked(&self.background);
                match background.get_mut(&place) {
                    Some(slot) if slot.id == id && !slot.queued.is_empty() => {
                        std::mem::take(&mut slot.queued)
                    }
                    Some(slot) if slot.id == id => {
                        background.remove(&place);
                        self.busy.send_modify(|busy| *busy = busy.saturating_sub(1));
                        return;
                    }
                    // A new turn has let go of this run.
                    _ => return,
                }
            };
            for batch in batches(queued) {
                let job = &batch[0];
                let ran = execute(
                    job.runner.clone(),
                    job.env.clone(),
                    self.running.clone(),
                    self.limit,
                )
                .await;
                for job in &batch {
                    self.settle(job, &ran);
                }
            }
        }
    }

    /// Checks `file` after a write that replaced `old` (`None` for a new file) with `new`. Call
    /// it while the file's lock is held. Never fails the write: a checker that is missing or
    /// fails is one note, and so is one that takes longer than the write waits for it; that one
    /// goes on in the background, and what it finds is in the report at the end of the turn.
    pub async fn after_write(
        self: &Arc<Self>,
        ctx: &ToolCtx,
        file: &Path,
        old: Option<&str>,
        new: &str,
    ) -> Checked {
        if !self.on_write {
            return Checked::default();
        }
        let find = |name: &str| which::which(name).ok();
        // `why` is for the report at the end of the turn, `note` for the result of the write.
        let unchecked = |checker: &str, why: String, note: Option<String>| {
            self.record(file, checker, (old, new), Err(why));
            Checked { note, block: None }
        };
        let runner = match self.runner(file, &ctx.cwd, &find) {
            None => return Checked::default(),
            Some(Ok(runner)) => runner,
            Some(Err(why)) => return unchecked("", why.clone(), Some(why)),
        };
        let name = runner.name.clone();
        if new.len() > MAX_FILE_BYTES {
            return unchecked(&name, "larger than 1 MB".to_string(), None);
        }
        let place: Place = (
            name.clone(),
            kind_of(file).unwrap_or_default(),
            runner.cwd.clone(),
        );
        let job = Job {
            runner,
            file: file.to_path_buf(),
            turn: locked(&self.turn).number,
            env: ctx.services.get::<AgentEnv>(),
        };
        let still_running = format!("{name} was still running");
        let behind = format!(
            "{name} is still checking an earlier write; what it finds here joins the report when you stop"
        );
        // A run of this checker is in the background here: the file waits behind it. It is
        // recorded before it is queued, so that the run after this one finds it.
        let job = {
            let mut background = locked(&self.background);
            match background.get_mut(&place) {
                Some(slot) => {
                    let checked = unchecked(&name, still_running, Some(behind));
                    slot.queue(job);
                    return checked;
                }
                None => job,
            }
        };
        let mut run = Run(Some(tokio::spawn(execute(
            job.runner.clone(),
            job.env.clone(),
            self.running.clone(),
            self.limit,
        ))));
        let waited = match &mut run.0 {
            Some(handle) => tokio::select! {
                waited = tokio::time::timeout(self.timeout, handle) => waited,
                _ = ctx.cancel.cancelled() => {
                    let why = format!("{name} was cancelled");
                    return unchecked(&name, why.clone(), Some(why));
                }
            },
            None => return Checked::default(),
        };
        let ran = match waited {
            Ok(Ok(ran)) => ran,
            Ok(Err(_)) => Ran::Failed(format!("{name} could not be run")),
            // Nothing would show what a run in the background finds.
            Err(_) if self.report == CheckReport::Off => {
                let why = format!("{name} did not finish within {}", human(self.timeout));
                return unchecked(&name, why.clone(), Some(why));
            }
            Err(_) => {
                let checked = unchecked(&name, still_running, None);
                let mut background = locked(&self.background);
                // Another write got there while this one waited: one run at a time.
                if let Some(slot) = background.get_mut(&place) {
                    slot.queue(job);
                    return Checked {
                        note: Some(behind),
                        ..checked
                    };
                }
                let id = self.slots.fetch_add(1, Ordering::Relaxed);
                let task = tokio::spawn(self.clone().supervise(place.clone(), id, job, run));
                let queued = Vec::new();
                background.insert(place, Slot { id, task, queued });
                self.busy.send_modify(|busy| *busy += 1);
                return Checked {
                    note: Some(format!(
                        "{name} is still running; what it finds joins the report when you stop"
                    )),
                    ..checked
                };
            }
        };
        match outcome(&job.runner, file, &ran, self.limit) {
            Ok(findings) => {
                let shown = display_path(&ctx.cwd, file);
                let errors: Vec<&Finding> = findings
                    .iter()
                    .filter(|finding| finding.severity == Severity::Error)
                    .collect();
                let mut lines: Vec<String> = errors
                    .iter()
                    .take(MAX_WRITE_ERRORS)
                    .map(|finding| finding.render(&shown, &name))
                    .collect();
                if errors.len() > MAX_WRITE_ERRORS {
                    lines.push(format!("… {} more errors", errors.len() - MAX_WRITE_ERRORS));
                }
                let block = (!lines.is_empty()).then(|| format!("checks:\n{}", lines.join("\n")));
                self.record(file, &name, (old, new), Ok(findings));
                Checked { note: None, block }
            }
            Err(why) => unchecked(&name, why.clone(), Some(why)),
        }
    }

    /// Ends the checks that are still running.
    fn let_go(&self) {
        let mut background = locked(&self.background);
        for (_, slot) in background.drain() {
            slot.task.abort();
        }
        self.busy.send_replace(0);
    }

    /// Forgets the turn that ended: a prompt starts a new one.
    pub fn new_turn(&self) {
        self.let_go();
        {
            let mut turn = locked(&self.turn);
            *turn = Turn {
                number: turn.number + 1,
                ..Turn::default()
            };
        }
        if let Some(secrets) = &self.secrets {
            secrets.start();
        }
    }

    /// Whether the lead's report has to wait for a check that is still running.
    pub fn waits(&self) -> bool {
        self.report != CheckReport::Off && !locked(&self.turn).reported && *self.busy.borrow() > 0
    }

    /// Returns once no check is running in the background, or after the time one run and the
    /// run for what waited behind it may take.
    pub async fn settled(&self) {
        let mut busy = self.busy.subscribe();
        let _ = tokio::time::timeout(self.limit * 2, busy.wait_for(|busy| *busy == 0)).await;
    }

    /// What the lead is told when it stops: what the checkers found in the files written in
    /// this turn, named from `cwd`, and the secrets a finished scan found. `None` when there is
    /// nothing to tell or the turn has had its report.
    pub fn report(&self, cwd: &Path) -> Option<Report> {
        if self.report == CheckReport::Off {
            return None;
        }
        let mut turn = locked(&self.turn);
        if turn.reported {
            return None;
        }
        let delta = self.report == CheckReport::Delta;
        let mut all: Vec<&Finding> = Vec::new();
        let mut per_file = Vec::new();
        let mut listed = Vec::new();
        let mut unchecked = Vec::new();
        for written in turn.files.values() {
            let shown = display_path(cwd, &written.path);
            if let Some(why) = &written.unchecked {
                unchecked.push(format!("{shown} ({why})"));
            }
            if written.findings.is_empty() {
                continue;
            }
            let found: Vec<&Finding> = written.findings.iter().collect();
            per_file.push(format!("{shown}: {}", counts(&found)));
            all.extend(&found);
            listed.extend(
                found
                    .iter()
                    .filter(|finding| {
                        !delta
                            || finding.line == 0
                            || written
                                .touched
                                .iter()
                                .any(|&(start, end)| (start..=end).contains(&finding.line))
                    })
                    .map(|finding| finding.render(&shown, &written.checker)),
            );
        }
        let leaks = self
            .secrets
            .as_ref()
            .map(|secrets| secrets.take())
            .unwrap_or_default();
        if listed.is_empty() && leaks.is_empty() {
            return None;
        }

        let mut text = String::from("<checks>\n");
        let mut summary = Vec::new();
        if !listed.is_empty() {
            let files = plural(per_file.len(), "file", "files");
            text.push_str(&format!(
                "The checkers found {} in {files} written in this turn:\n{}\n",
                counts(&all),
                per_file.join("\n")
            ));
            text.push_str(if delta {
                "On the lines changed in this turn:\n"
            } else {
                "All of them:\n"
            });
            let more = listed.len().saturating_sub(MAX_REPORTED);
            let shown = listed.len() - more;
            text.push_str(&listed[..shown].join("\n"));
            text.push('\n');
            if more > 0 {
                text.push_str(&format!("… {more} more\n"));
            }
            summary.push(match delta {
                true => format!("{} in {files}, {} on changed lines", counts(&all), listed.len()),
                false => format!("{} in {files}", counts(&all)),
            });
        }
        if !unchecked.is_empty() {
            text.push_str(&format!(
                "{} not checked, so nothing found there does not mean it is clean: {}\n",
                plural(unchecked.len(), "file was", "files were"),
                unchecked.join(", ")
            ));
            summary.push(format!(
                "{} not checked",
                plural(unchecked.len(), "file", "files")
            ));
        }
        if !leaks.is_empty() {
            let secrets = plural(leaks.len(), "secret", "secrets");
            text.push_str(&format!(
                "gitleaks found {secrets} in the project (file, line and the rule that matched):\n"
            ));
            for leak in &leaks {
                text.push_str(&format!("{}:{} {}\n", leak.file, leak.line, leak.rule));
            }
            summary.push(format!("{secrets} found by gitleaks"));
        }
        text.push_str(
            "</checks>\nThis is an automatic report, not a message from the user. Fix what your changes \
             caused, say in a line what you leave and why, then finish.",
        );
        turn.reported = true;
        Some(Report {
            text,
            summary: format!("checks: {}; the agent was told", summary.join("; ")),
        })
    }

    fn end(&self) {
        self.let_go();
        if let Some(secrets) = &self.secrets {
            secrets.stop();
        }
    }
}

/// Starts a turn at each prompt, hands the lead its report when it stops, and ends the scan with
/// the session.
struct TurnWatch(Arc<Checks>);

#[async_trait]
impl EventHandler for TurnWatch {
    async fn handle(&self, event: &mut Event, ctx: &ExtCtx) -> Outcome {
        match event {
            Event::UserPrompt { .. } => self.0.new_turn(),
            Event::SessionEnd => self.0.end(),
            Event::Stop { agent, .. } if agent.depth == 0 => {
                let ui = ctx.services.get::<Ui>();
                if self.0.waits() {
                    if let Some(ui) = &ui {
                        ui.0.notify(Level::Info, "checks: waiting for a checker that is still running");
                    }
                    self.0.settled().await;
                }
                if let Some(report) = self.0.report(&ctx.cwd) {
                    if let Some(ui) = &ui {
                        ui.0.notify(Level::Info, &report.summary);
                    }
                    return Outcome::Block { reason: report.text };
                }
            }
            _ => {}
        }
        Outcome::Continue
    }
}

/// Registers the [`Checks`] service and the report the lead gets when it stops. Load it after the
/// advisor, whose review looks only at a stop nothing else has answered yet.
pub struct ChecksExt {
    /// The project root.
    pub root: PathBuf,
    /// `tools.check_on_write`.
    pub on_write: bool,
    /// `[tools.check]`.
    pub configured: BTreeMap<String, String>,
    /// `tools.check_report`.
    pub report: CheckReport,
    /// Whether the project is trusted. A checker that runs the project's code is used only
    /// then.
    pub trusted: bool,
    /// How long a write waits for its checker: [`TIMEOUT`].
    pub timeout: Duration,
    /// How long a checker may run at all: [`LIMIT`].
    pub limit: Duration,
    /// The `gitleaks` program, when the project is to be scanned for secrets: what
    /// [`crate::scan::scanner`] found.
    pub gitleaks: Option<PathBuf>,
}

impl Extension for ChecksExt {
    fn id(&self) -> &str {
        "checks"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        let checks = Arc::new(Checks::new(self));
        r.service(checks.clone());
        let watch = Arc::new(TurnWatch(checks));
        r.on(EventKind::UserPrompt, watch.clone());
        r.on(EventKind::Stop, watch.clone());
        r.on(EventKind::SessionEnd, watch);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(line: u32, column: u32, severity: Severity, message: &str) -> Finding {
        Finding {
            line,
            column,
            severity,
            message: message.to_string(),
        }
    }

    fn checks(configured: &[(&str, &str)], root: &Path) -> Checks {
        checks_of(configured, root, true)
    }

    fn checks_of(configured: &[(&str, &str)], root: &Path, trusted: bool) -> Checks {
        Checks::new(&ChecksExt {
            root: root.to_path_buf(),
            on_write: true,
            configured: configured
                .iter()
                .map(|(kind, command)| (kind.to_string(), command.to_string()))
                .collect(),
            trusted,
            report: CheckReport::Delta,
            timeout: TIMEOUT,
            limit: LIMIT,
            gitleaks: None,
        })
    }

    /// Every program is installed, in `/bin`.
    fn all(name: &str) -> Option<PathBuf> {
        Some(Path::new("/bin").join(name))
    }

    #[test]
    fn a_location_is_read_with_or_without_a_column_and_past_a_drive_letter() {
        assert_eq!(
            location("src/a.rs:3:9: warning: unused"),
            Some(("src/a.rs", 3, 9, "warning: unused"))
        );
        assert_eq!(
            location("a.go:7: undefined: x"),
            Some(("a.go", 7, 0, "undefined: x"))
        );
        assert_eq!(
            location(r"C:\work\a b\lib.rs:12:1: error[E0425]: no `x`"),
            Some((r"C:\work\a b\lib.rs", 12, 1, "error[E0425]: no `x`"))
        );
        assert_eq!(
            location("Dockerfile:3 DL3006 warning: tag it"),
            Some(("Dockerfile", 3, 0, "DL3006 warning: tag it"))
        );
        assert_eq!(location("a.c:10:5:no space"), Some(("a.c", 10, 5, "no space")));
        assert_eq!(location("a.c:10\ttabbed"), Some(("a.c", 10, 0, "tabbed")));
        for not_one in [
            "warning: `demo` (lib) generated 1 warning",
            "see http://localhost:8080/help",
            "Found 2 errors.",
        ] {
            assert_eq!(location(not_one), None, "{not_one}");
        }
    }

    #[test]
    fn each_checker_s_lines_give_a_severity_and_a_message() {
        let one = parse_line;
        let found = |path: &str, finding: Finding| Some((path.to_string(), finding));
        // cargo's short format, as clippy writes it.
        assert_eq!(
            one(Style::Gcc, "src/lib.rs:2:9: warning: unused variable: `x`"),
            found(
                "src/lib.rs",
                finding(2, 9, Severity::Warning, "unused variable: `x`")
            )
        );
        assert_eq!(
            one(
                Style::Gcc,
                r"crates\a\src\lib.rs:5:5: error[E0308]: mismatched   types"
            ),
            found(
                r"crates\a\src\lib.rs",
                finding(5, 5, Severity::Error, "mismatched types [E0308]")
            )
        );
        // shellcheck -f gcc.
        assert_eq!(
            one(
                Style::Gcc,
                "run.sh:3:6: note: Double quote to prevent globbing. [SC2086]"
            ),
            found(
                "run.sh",
                finding(3, 6, Severity::Note, "Double quote to prevent globbing. [SC2086]")
            )
        );
        // A checker that names no severity warns.
        assert_eq!(
            one(Style::Gcc, "a.txt:1: trailing space"),
            found("a.txt", finding(1, 0, Severity::Warning, "trailing space"))
        );
        assert_eq!(one(Style::Gcc, "error: could not compile `demo`"), None);

        assert_eq!(
            one(Style::Ruff, "a.py:1:8: F401 [*] `os` imported but unused"),
            found(
                "a.py",
                finding(1, 8, Severity::Warning, "F401 [*] `os` imported but unused")
            )
        );
        assert_eq!(
            one(Style::Ruff, "a.py:2:5: SyntaxError: Expected an expression").map(|(_, f)| f.severity),
            Some(Severity::Error)
        );
        assert_eq!(
            one(
                Style::Unix,
                "/w/a.js:1:7: 'x' is assigned a value but never used. [Warning/no-unused-vars]"
            ),
            found(
                "/w/a.js",
                finding(
                    1,
                    7,
                    Severity::Warning,
                    "'x' is assigned a value but never used. [no-unused-vars]"
                )
            )
        );
        assert_eq!(
            one(Style::Unix, "/w/a.js:2:1: Parsing error [Error/]").map(|(_, f)| (f.severity, f.message)),
            Some((Severity::Error, "Parsing error".to_string()))
        );
        assert_eq!(
            one(
                Style::GoVet,
                "./a.go:9:2: fmt.Printf format %d has arg s of wrong type string"
            )
            .map(|(path, f)| (path, f.severity)),
            Some(("./a.go".to_string(), Severity::Warning))
        );
        assert_eq!(
            one(Style::GoVet, "vet: ./a.go:4:9: undefined: y"),
            found("./a.go", finding(4, 9, Severity::Error, "undefined: y"))
        );
        assert_eq!(one(Style::GoVet, "# example.com/demo"), None);
        assert_eq!(
            one(
                Style::Actionlint,
                ".github/workflows/ci.yml:8:9: \"runs-on\" section is missing in job \"test\" [syntax-check]"
            )
            .map(|(_, f)| (f.line, f.severity)),
            Some((8, Severity::Error))
        );
        assert_eq!(
            one(
                Style::Hadolint,
                "Dockerfile:1 DL3006 warning: Always tag the version of an image explicitly"
            ),
            found(
                "Dockerfile",
                finding(
                    1,
                    0,
                    Severity::Warning,
                    "Always tag the version of an image explicitly [DL3006]"
                )
            )
        );

        let eslint = r#"[{"filePath":"/w/a.ts","messages":[
            {"ruleId":"no-unused-vars","severity":1,"message":"'x' is defined but never used.","line":3,"column":7},
            {"ruleId":null,"fatal":true,"severity":2,"message":"Parsing error: Unexpected token","line":9,"column":1}
        ]},{"filePath":"/w/b.ts","messages":[]}]"#
            .replace('\n', " ");
        assert_eq!(
            parse(Style::Eslint, &eslint, "(node) a warning on stderr"),
            [
                (
                    "/w/a.ts".to_string(),
                    finding(
                        3,
                        7,
                        Severity::Warning,
                        "'x' is defined but never used. [no-unused-vars]"
                    )
                ),
                (
                    "/w/a.ts".to_string(),
                    finding(9, 1, Severity::Error, "Parsing error: Unexpected token")
                ),
            ]
        );
        assert!(parse(Style::Eslint, "Oops! Something went wrong!", "").is_empty());
    }

    #[test]
    fn the_built_in_checker_follows_the_kind_of_file_and_what_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = |relative: &str| {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "").unwrap();
            path
        };
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/*\"]\n").unwrap();
        std::fs::create_dir_all(root.join("crates/demo")).unwrap();
        std::fs::write(
            root.join("crates/demo/Cargo.toml"),
            "[package]\nname = \"demo-lib\" # the library\nversion = \"0.1.0\"\n\n[dependencies]\nname = \"1\"\n",
        )
        .unwrap();
        let checks = checks(&[], root);
        let runner = |path: &Path, find: &dyn Fn(&str) -> Option<PathBuf>| {
            checks.runner(path, root, find).map(|runner| runner.unwrap())
        };
        let line = |runner: &Runner| {
            let program = runner.program.file_name().unwrap().to_string_lossy().into_owned();
            format!("{program} {}", runner.arguments.join(" "))
        };

        // Rust: clippy on the file's own package, from that package's directory.
        let lib = file("crates/demo/src/lib.rs");
        let clippy = runner(&lib, &all).unwrap();
        assert_eq!(clippy.name, "clippy");
        assert_eq!(line(&clippy), "cargo clippy -p demo-lib --message-format=short");
        assert_eq!(clippy.cwd, root.join("crates/demo"));
        assert_eq!(clippy.bases, [root.to_path_buf(), root.join("crates/demo")]);
        let test = runner(&file("crates/demo/tests/it.rs"), &all).unwrap();
        assert_eq!(
            line(&test),
            "cargo clippy -p demo-lib --message-format=short --all-targets"
        );
        // A file that belongs to no package, and a machine without clippy.
        assert!(runner(&file("scripts/tool.rs"), &all).is_none());
        assert!(
            runner(&lib, &|name| (name != "cargo-clippy")
                .then(|| all(name))
                .flatten())
            .is_none()
        );

        let py = file("tools/a.py");
        assert_eq!(
            line(&runner(&py, &all).unwrap()),
            format!("ruff check --output-format concise --no-fix {}", py.display())
        );
        let sh = file("run.sh");
        assert_eq!(
            line(&runner(&sh, &all).unwrap()),
            format!("shellcheck -f gcc {}", sh.display())
        );
        let go = runner(&file("cmd/app/main.go"), &all).unwrap();
        assert_eq!(
            (line(&go).as_str(), go.cwd.clone()),
            ("go vet .", root.join("cmd/app"))
        );

        // Scripts: eslint where the project configures it, else oxlint.
        let ts = file("web/src/a.ts");
        assert_eq!(
            line(&runner(&ts, &all).unwrap()),
            format!("oxlint -f unix {}", ts.display())
        );
        file("web/eslint.config.mjs");
        let eslint = runner(&ts, &all).unwrap();
        assert_eq!(line(&eslint), format!("eslint -f json {}", ts.display()));
        assert_eq!(eslint.style, Style::Eslint);
        let only_oxlint = |name: &str| (name == "oxlint").then(|| all(name)).flatten();
        assert_eq!(runner(&ts, &only_oxlint).unwrap().name, "oxlint");

        // Only workflow files among YAML, and Dockerfiles by name.
        let workflow = file(".github/workflows/ci.yml");
        assert_eq!(
            line(&runner(&workflow, &all).unwrap()),
            format!("actionlint -oneline {}", workflow.display())
        );
        assert!(runner(&file("config/app.yml"), &all).is_none());
        for name in ["Dockerfile", "Dockerfile.dev", "api.dockerfile"] {
            assert_eq!(runner(&file(name), &all).unwrap().name, "hadolint", "{name}");
        }

        // Nothing installed, or a kind nothing checks: no checker and no complaint.
        assert!(checks.runner(&py, root, &|_| None).is_none());
        assert!(checks.runner(&file("notes.md"), root, &all).is_none());
        assert!(checks.runner(&file("Makefile"), root, &all).is_none());

        // In a project that is not trusted the checkers that run its code are off: no clippy
        // (build scripts, macros), no go vet (cgo), and oxlint where eslint would load the
        // project's configuration. The ones that only read the file are as before.
        let untrusted = checks_of(&[], root, false);
        let name = |path: &Path| {
            let runner = untrusted.runner(path, root, &all);
            runner.map(|runner| runner.unwrap().name)
        };
        assert_eq!(name(&lib), None);
        assert_eq!(name(&root.join("cmd/app/main.go")), None);
        assert_eq!(name(&ts).as_deref(), Some("oxlint"));
        assert_eq!(name(&py).as_deref(), Some("ruff"));
        assert_eq!(name(&sh).as_deref(), Some("shellcheck"));
        assert_eq!(name(&workflow).as_deref(), Some("actionlint"));
        assert_eq!(name(&root.join("Dockerfile")).as_deref(), Some("hadolint"));
    }

    #[test]
    fn files_queued_behind_a_run_share_a_run_when_their_command_is_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        let checks = checks(&[], root);
        let job = |relative: &str| Job {
            runner: checks.runner(&root.join(relative), root, &all).unwrap().unwrap(),
            file: root.join(relative),
            turn: 0,
            env: None,
        };
        // Two files of the package's library are one clippy run; a test file needs
        // `--all-targets`, and each shell script is handed to shellcheck by name.
        let queued = ["src/a.rs", "run.sh", "src/b.rs", "tests/it.rs", "other.sh"].map(job);
        let files: Vec<Vec<PathBuf>> = batches(queued.into())
            .iter()
            .map(|batch| batch.iter().map(|job| job.file.clone()).collect())
            .collect();
        let at = |relative: &str| root.join(relative);
        assert_eq!(
            files,
            [
                vec![at("src/a.rs"), at("src/b.rs")],
                vec![at("run.sh")],
                vec![at("tests/it.rs")],
                vec![at("other.sh")],
            ]
        );

        // A file written again while it waits is checked once.
        let mut slot = Slot {
            id: 0,
            task: tokio::runtime::Runtime::new().unwrap().spawn(async {}),
            queued: Vec::new(),
        };
        for relative in ["src/a.rs", "run.sh", "src/a.rs"] {
            slot.queue(job(relative));
        }
        let waiting: Vec<PathBuf> = slot.queued.iter().map(|job| job.file.clone()).collect();
        assert_eq!(waiting, [at("run.sh"), at("src/a.rs")]);
    }

    #[test]
    fn a_configured_checker_replaces_the_built_in_one_for_its_kind() {
        let root = Path::new("/work");
        let checks = checks(
            &[
                ("py", "pylint --output-format=parseable"),
                ("sql", "sqlfluff lint {file} --format github-annotation-native"),
                ("sh", ""),
                ("dockerfile", "docker-lint"),
            ],
            root,
        );
        let line = |path: &str| {
            let runner = checks.runner(Path::new(path), root, &all).unwrap().unwrap();
            (runner.name.clone(), runner.arguments.join(" "), runner.style)
        };
        assert_eq!(
            line("a.py"),
            (
                "pylint".to_string(),
                "--output-format=parseable a.py".to_string(),
                Style::Gcc
            )
        );
        assert_eq!(
            line("q/a.SQL").1,
            "lint q/a.SQL --format github-annotation-native"
        );
        assert_eq!(line("Dockerfile").0, "docker-lint");
        // An empty command turns the check off for the kind.
        assert!(checks.runner(Path::new("run.sh"), root, &all).is_none());
        // A configured program that is missing is said; a built-in one is not.
        assert_eq!(
            checks
                .runner(Path::new("a.py"), root, &|_| None)
                .unwrap()
                .unwrap_err(),
            "checker `pylint` was not found"
        );
    }

    #[test]
    fn changed_lines_are_followed_through_later_edits() {
        let old = "a\nb\nc\nd\ne\n";
        // One line replaced.
        let first = touched_lines(old, "a\nb\nC\nd\ne\n", &[]);
        assert_eq!(first, [(3, 3)]);
        // Two lines added above it: the earlier change is now line 5, beside the new lines.
        let second = touched_lines("a\nb\nC\nd\ne\n", "x\ny\na\nb\nC\nd\ne\n", &first);
        assert_eq!(second, [(1, 2), (5, 5)]);
        // The first line removed and a line appended: both spans move up, and the new line is
        // added.
        let third = touched_lines("x\ny\na\nb\nC\nd\ne\n", "y\na\nb\nC\nd\ne\nf\n", &second);
        assert_eq!(third, [(1, 1), (4, 4), (7, 7)]);
        // Where lines were removed, the lines that now meet count as changed.
        assert_eq!(touched_lines("a\nb\nc\nd\ne\n", "a\nb\ne\n", &[]), [(2, 3)]);
        assert_eq!(touched_lines("a\nb\n", "b\n", &[]), [(1, 1)]);
        assert_eq!(touched_lines("a\nb\n", "a\n", &[(2, 2)]), [(1, 1)]);
        assert_eq!(touched_lines("a\n", "", &[(1, 1)]), []);
        // Neighbouring spans merge.
        assert_eq!(
            touched_lines("a\nb\nc\n", "a\nB\nc\n", &[(1, 1), (3, 3)]),
            [(1, 3)]
        );
    }

    #[test]
    fn a_package_is_found_by_its_nearest_manifest_and_a_workspace_above_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("crates/a/src/deep")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace.package]\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("crates/a/Cargo.toml"),
            "[lib]\nname = \"not_this\"\n[package]\nname = 'a-crate'\n",
        )
        .unwrap();
        let package = root.join("crates/a");
        assert_eq!(
            rust_package(&package.join("src/deep/mod.rs")),
            Some((package.clone(), "a-crate".to_string()))
        );
        assert_eq!(workspace_root(&package), root);
        // The workspace's own manifest names no package.
        assert_eq!(rust_package(&root.join("build.rs")), None);
        // A package that stands alone is its own root.
        std::fs::write(root.join("Cargo.toml"), "# nothing here\n").unwrap();
        assert_eq!(workspace_root(&package), package);
    }
}
