//! Checks on write: after `write` or `edit` changes a file, the linter for its kind runs on it.
//! Errors go into the tool result at once; everything found is kept until the lead stops, when
//! it is told what stands on the lines it changed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::ToolCtx;
use azazel_core::config::CheckReport;
use azazel_core::paths::canonical;
use azazel_core::process::AgentEnv;
use azazel_core::{
    Event, EventHandler, EventKind, ExtCtx, ExtError, Extension, Level, Outcome, Registrar, Ui,
};
use process_wrap::tokio::{CommandWrap, KillOnDrop};
use serde_json::Value;
use similar::{DiffOp, TextDiff};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::Semaphore;

use crate::format::{FILE, words};
use crate::scan::SecretScan;
use crate::util::display_path;

/// How long a checker may take on one file.
pub const TIMEOUT: Duration = Duration::from_secs(5);
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
/// How long comparing two versions of a file may take when working out the changed lines.
const DIFF_TIMEOUT: Duration = Duration::from_millis(250);

/// The built-in checkers, for `azazel doctor` and the documentation: the name an agent is told,
/// the program looked for on `PATH`, and the files it checks.
pub const BUILTIN: [(&str, &str, &str); 8] = [
    ("clippy", "cargo-clippy", "rs"),
    ("ruff", "ruff", "py, pyi"),
    ("eslint", "eslint", "js, ts"),
    ("oxlint", "oxlint", "js, ts"),
    ("shellcheck", "shellcheck", "sh, bash"),
    ("go vet", "go", "go"),
    ("actionlint", "actionlint", "workflows"),
    ("hadolint", "hadolint", "Dockerfile"),
];

const SCRIPT_EXTENSIONS: [&str; 8] = ["js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts"];

/// A file of one of these names beside or above a script means the project uses eslint.
const ESLINT_CONFIGS: [&str; 12] = [
    "eslint.config.js",
    "eslint.config.mjs",
    "eslint.config.cjs",
    "eslint.config.ts",
    "eslint.config.mts",
    "eslint.config.cts",
    ".eslintrc",
    ".eslintrc.js",
    ".eslintrc.cjs",
    ".eslintrc.json",
    ".eslintrc.yml",
    ".eslintrc.yaml",
];

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
        let (column, after) = match after.strip_prefix(':') {
            Some(more) => {
                let digits = more.bytes().take_while(u8::is_ascii_digit).count();
                match more[..digits].parse::<u32>() {
                    Ok(column) => (
                        column,
                        more[digits..].strip_prefix(':').unwrap_or(&more[digits..]),
                    ),
                    Err(_) => (0, more),
                }
            }
            None => (0, after),
        };
        if after.is_empty() || after.starts_with(' ') {
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
#[derive(Debug)]
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

/// The built-in checker for a file of `kind`, when its program is installed.
fn builtin(
    kind: &str,
    file: &Path,
    cwd: &Path,
    root: &Path,
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
        "rs" => {
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
        "go" => {
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
            let eslint = uses_eslint(file, root).then(|| find("eslint")).flatten();
            match eslint {
                Some(eslint) => Some(on_file("eslint", eslint, &["-f", "json"], Style::Eslint)),
                None => Some(on_file("oxlint", find("oxlint")?, &["-f", "unix"], Style::Unix)),
            }
        }
        _ => None,
    }
}

/// The built-in checkers that are installed, each as `name (the files it checks)`.
pub fn installed() -> Vec<String> {
    let found = |program: &str| which::which(program).is_ok();
    BUILTIN
        .iter()
        // clippy is a subcommand of cargo.
        .filter(|(_, program, _)| found(program) && (*program != "cargo-clippy" || found("cargo")))
        .map(|(name, _, kinds)| format!("{name} ({kinds})"))
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
/// from `old`, and the lines of `before` (ranges in `old`) where they are now.
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
                        let moved = |at: u32| at - first + line(new_index + 1);
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
            DiffOp::Delete { .. } => {}
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
    files: BTreeMap<PathBuf, Written>,
    /// The kinds of file whose checker ran out of time in this turn, with the directory it ran
    /// in. It is not run there again until the next prompt.
    slow: HashSet<(String, PathBuf)>,
    /// The lead has had this turn's report.
    reported: bool,
}

enum Ran {
    Found(Vec<Finding>),
    TimedOut,
    Failed(String),
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
    report: CheckReport,
    timeout: Duration,
    running: Semaphore,
    turn: Mutex<Turn>,
    secrets: Option<Arc<SecretScan>>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Everything `pipe` writes, of which the first [`MAX_OUTPUT_BYTES`] are kept.
async fn captured(pipe: Option<impl AsyncRead + Unpin>) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

impl Checks {
    pub fn new(ext: &ChecksExt) -> Checks {
        Checks {
            root: ext.root.clone(),
            on_write: ext.on_write,
            configured: ext.configured.clone(),
            report: ext.report,
            timeout: ext.timeout,
            running: Semaphore::new(CONCURRENT),
            turn: Mutex::default(),
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
            return builtin(&kind, file, cwd, &self.root, find).map(Ok);
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

    /// Runs `runner` and returns what it found in `file`.
    async fn run(&self, runner: &Runner, file: &Path, env: Option<Arc<AgentEnv>>) -> Ran {
        let name = &runner.name;
        let Ok(_permit) = self.running.acquire().await else {
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
        let (stdout, stderr) = (child.stdout().take(), child.stderr().take());
        let finished = {
            let output = async { tokio::join!(captured(stdout), captured(stderr), child.wait()) };
            tokio::time::timeout(self.timeout, output).await
        };
        let Ok((stdout, stderr, status)) = finished else {
            let _ = child.start_kill();
            return Ran::TimedOut;
        };
        let status = match status {
            Ok(status) => status,
            Err(e) => return Ran::Failed(format!("{name} could not be waited for: {e}")),
        };
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
        let file = canonical(file);
        let mut same: HashMap<String, bool> = HashMap::new();
        let mut findings: Vec<Finding> = found
            .into_iter()
            .filter(|(path, _)| {
                *same.entry(path.clone()).or_insert_with(|| {
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
            .map(|(_, finding)| finding)
            .collect();
        findings.sort();
        findings.dedup();
        Ran::Found(findings)
    }

    /// Records that `file` was written in this turn, and which of its lines the turn changed.
    fn written(&self, key: &Path, file: &Path, checker: &str, old: Option<&str>, new: &str) {
        let mut turn = locked(&self.turn);
        let written = turn.files.entry(key.to_path_buf()).or_default();
        written.path = file.to_path_buf();
        written.checker = checker.to_string();
        written.touched = match old {
            Some(old) => touched_lines(old, new, &written.touched),
            None => vec![(1, u32::try_from(new.lines().count().max(1)).unwrap_or(u32::MAX))],
        };
    }

    fn settle(&self, key: &Path, outcome: Result<Vec<Finding>, String>) {
        if let Some(written) = locked(&self.turn).files.get_mut(key) {
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

    /// Checks `file` after a write that replaced `old` (`None` for a new file) with `new`. Call
    /// it while the file's lock is held. Never fails the write: a checker that is missing, fails
    /// or takes too long is one note.
    pub async fn after_write(&self, ctx: &ToolCtx, file: &Path, old: Option<&str>, new: &str) -> Checked {
        if !self.on_write {
            return Checked::default();
        }
        let find = |name: &str| which::which(name).ok();
        let key = canonical(file);
        let unchecked = |checker: &str, why: String, note: bool| {
            self.written(&key, file, checker, old, new);
            self.settle(&key, Err(why.clone()));
            Checked {
                note: note.then_some(why),
                block: None,
            }
        };
        let runner = match self.runner(file, &ctx.cwd, &find) {
            None => return Checked::default(),
            Some(Ok(runner)) => runner,
            Some(Err(why)) => return unchecked("", why, true),
        };
        let name = runner.name.as_str();
        if new.len() > MAX_FILE_BYTES {
            return unchecked(name, "larger than 1 MB".to_string(), false);
        }
        let scope = (kind_of(file).unwrap_or_default(), runner.cwd.clone());
        if locked(&self.turn).slow.contains(&scope) {
            let why = format!("{name} ran out of time earlier in this turn and was not run again");
            return unchecked(name, why, true);
        }
        let ran = tokio::select! {
            ran = self.run(&runner, file, ctx.services.get::<AgentEnv>()) => ran,
            _ = ctx.cancel.cancelled() => Ran::Failed(format!("{name} was cancelled")),
        };
        match ran {
            Ran::Found(findings) => {
                self.written(&key, file, name, old, new);
                let shown = display_path(&ctx.cwd, file);
                let errors: Vec<&Finding> = findings
                    .iter()
                    .filter(|finding| finding.severity == Severity::Error)
                    .collect();
                let mut lines: Vec<String> = errors
                    .iter()
                    .take(MAX_WRITE_ERRORS)
                    .map(|finding| finding.render(&shown, name))
                    .collect();
                if errors.len() > MAX_WRITE_ERRORS {
                    lines.push(format!("… {} more errors", errors.len() - MAX_WRITE_ERRORS));
                }
                let block = (!lines.is_empty()).then(|| format!("checks:\n{}", lines.join("\n")));
                self.settle(&key, Ok(findings));
                Checked { note: None, block }
            }
            Ran::TimedOut => {
                locked(&self.turn).slow.insert(scope);
                let why = format!("{name} did not finish within {}", human(self.timeout));
                unchecked(name, why, true)
            }
            Ran::Failed(why) => unchecked(name, why, true),
        }
    }

    /// Forgets the turn that ended: a prompt starts a new one.
    pub fn new_turn(&self) {
        *locked(&self.turn) = Turn::default();
        if let Some(secrets) = &self.secrets {
            secrets.start();
        }
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
                if let Some(report) = self.0.report(&ctx.cwd) {
                    if let Some(ui) = ctx.services.get::<Ui>() {
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
    /// How long one checker may take: [`TIMEOUT`].
    pub timeout: Duration,
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
        Checks::new(&ChecksExt {
            root: root.to_path_buf(),
            on_write: true,
            configured: configured
                .iter()
                .map(|(kind, command)| (kind.to_string(), command.to_string()))
                .collect(),
            report: CheckReport::Delta,
            timeout: TIMEOUT,
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
        // The first line removed and a line appended: both spans move up, the new line is added
        // and joins nothing.
        let third = touched_lines("x\ny\na\nb\nC\nd\ne\n", "y\na\nb\nC\nd\ne\nf\n", &second);
        assert_eq!(third, [(1, 1), (4, 4), (7, 7)]);
        // A changed line that is deleted leaves nothing behind; neighbours merge.
        assert_eq!(touched_lines("a\nb\nc\n", "a\nc\n", &[(2, 2)]), []);
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
