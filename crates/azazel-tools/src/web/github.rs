//! GitHub addresses in `web_fetch` and `read`.
//!
//! A repository address is cloned one commit deep into the artifacts directory and answered with
//! its tree, its README and the path of the clone. A directory, a file, a pull request, an issue
//! and an Actions run are answered from GitHub's REST API: through the `gh` program when it is
//! installed and signed in, so that private repositories work, and without a login otherwise.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use azazel_agent::ToolCtx;
use azazel_core::ArtifactStore;
use azazel_core::process::AgentEnv;
use reqwest::Url;
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::{Mutex, OnceCell};

use super::fetch::{self, Fetcher, MAX_BODY_BYTES};

/// How long one `gh` call may take.
const GH_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a clone may take.
const CLONE_TIMEOUT: Duration = Duration::from_secs(180);
/// A repository GitHub reports as larger than this is not cloned.
const MAX_CLONE_MB: u64 = 500;
/// The most lines of a repository's tree in one answer.
const MAX_TREE_LINES: usize = 250;
/// The most files of a clone that are looked at for its tree.
const MAX_TREE_FILES: usize = 20_000;
/// The most characters of a README in one answer.
const MAX_README_CHARS: usize = 60_000;
/// The most bytes of the log of one Actions job.
const MAX_LOG_BYTES: usize = 32 * 1024 * 1024;
/// The last lines of a failed step's log that are returned.
const LOG_TAIL_LINES: usize = 150;
/// The most failed jobs of a run whose logs are read.
const MAX_LOG_JOBS: usize = 5;

const JSON: &str = "application/vnd.github+json";
const RAW: &str = "application/vnd.github.raw+json";
const DIFF: &str = "application/vnd.github.diff";
const API_VERSION: &str = "2022-11-28";

/// First path segments of `github.com` that are pages of the site, not accounts.
const NOT_OWNERS: &[&str] = &[
    "about",
    "account",
    "apps",
    "codespaces",
    "collections",
    "contact",
    "customer-stories",
    "dashboard",
    "enterprise",
    "events",
    "explore",
    "features",
    "issues",
    "join",
    "login",
    "marketplace",
    "new",
    "notifications",
    "organizations",
    "orgs",
    "pricing",
    "pulls",
    "readme",
    "resources",
    "search",
    "security",
    "settings",
    "site",
    "solutions",
    "sponsors",
    "stars",
    "team",
    "topics",
    "trending",
    "users",
    "watching",
];

/// A repository, as `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Repo {
    owner: String,
    name: String,
}

impl fmt::Display for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// What a GitHub address names. `rest` is what follows `tree/`, `blob/` or `raw/`: a branch,
/// tag or commit, then a path, as the segments of the address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Target {
    Repo(Repo),
    Tree {
        repo: Repo,
        rest: Vec<String>,
    },
    Blob {
        repo: Repo,
        rest: Vec<String>,
        /// The lines the address ends in, as `#L10-L20` does.
        lines: Option<(usize, usize)>,
    },
    Raw {
        repo: Repo,
        rest: Vec<String>,
    },
    Pull {
        repo: Repo,
        number: u64,
        /// The changes instead of the conversation.
        diff: bool,
    },
    Issue {
        repo: Repo,
        number: u64,
    },
    Run {
        repo: Repo,
        run: u64,
        attempt: Option<u64>,
        job: Option<u64>,
    },
}

/// The lines a fragment such as `L10`, `L10-L20` or `L10C3-L20C8` names.
fn line_range(fragment: &str) -> Option<(usize, usize)> {
    let line = |part: &str| {
        let digits = part.strip_prefix('L')?;
        let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
        digits[..end].parse::<usize>().ok().filter(|line| *line >= 1)
    };
    let (first, last) = fragment.split_once('-').unwrap_or((fragment, fragment));
    let (first, last) = (line(first)?, line(last)?);
    (first <= last).then_some((first, last))
}

/// What `url` names on GitHub, when it is something read in its own way. Every other address of
/// the site is fetched as the page it is.
pub(super) fn parse(url: &Url) -> Option<Target> {
    let host = url.host_str()?.to_ascii_lowercase();
    let raw_host = host == "raw.githubusercontent.com";
    if !raw_host && !matches!(host.as_str(), "github.com" | "www.github.com") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let [owner, name, rest @ ..] = segments.as_slice() else {
        return None;
    };
    let name = match raw_host {
        true => name,
        false => name.strip_suffix(".git").unwrap_or(name),
    };
    let word = |text: &str, also: &str| {
        !text.is_empty()
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || also.contains(c))
    };
    if !word(owner, "-") || !word(name, "-._") || name.chars().all(|c| c == '.') {
        return None;
    }
    if NOT_OWNERS.contains(&owner.to_ascii_lowercase().as_str()) {
        return None;
    }
    let repo = Repo {
        owner: owner.to_string(),
        name: name.to_string(),
    };
    let owned = |parts: &[&str]| parts.iter().map(|part| part.to_string()).collect::<Vec<_>>();
    if raw_host {
        return (rest.len() >= 2).then(|| Target::Raw {
            repo,
            rest: owned(rest),
        });
    }
    let number = |text: &str| text.parse::<u64>().ok();
    match rest {
        [] | ["tree"] => Some(Target::Repo(repo)),
        ["tree", rest @ ..] => Some(Target::Tree {
            repo,
            rest: owned(rest),
        }),
        ["blob", rest @ ..] if rest.len() >= 2 => Some(Target::Blob {
            repo,
            rest: owned(rest),
            lines: url.fragment().and_then(line_range),
        }),
        ["raw", rest @ ..] if rest.len() >= 2 => Some(Target::Raw {
            repo,
            rest: owned(rest),
        }),
        ["pull", pull, tail @ ..] => {
            let patch = pull.strip_suffix(".diff").or_else(|| pull.strip_suffix(".patch"));
            let diff = patch.is_some() || matches!(tail, ["files" | "changes", ..]);
            Some(Target::Pull {
                repo,
                number: number(patch.unwrap_or(pull))?,
                diff,
            })
        }
        ["issues", issue] => Some(Target::Issue {
            repo,
            number: number(issue)?,
        }),
        ["actions", "runs", run, tail @ ..] => {
            let (attempt, job) = match tail {
                ["job", job, ..] => (None, number(job)),
                ["attempts", attempt, "job", job, ..] => (number(attempt), number(job)),
                ["attempts", attempt, ..] => (number(attempt), None),
                _ => (None, None),
            };
            Some(Target::Run {
                repo,
                run: number(run)?,
                attempt,
                job,
            })
        }
        _ => None,
    }
}

/// Why an API request has no answer.
enum Failure {
    /// There is no such thing, or not for this login.
    Missing,
    Other(String),
}

/// Where GitHub is reached, and the programs it is reached with.
pub struct GitHub {
    api: String,
    raw: String,
    remote: String,
    gh: Option<PathBuf>,
    git: Option<PathBuf>,
    signed_in: OnceCell<bool>,
    /// Held while a repository is cloned, so that two calls never clone into one directory.
    cloning: Mutex<()>,
}

impl Default for GitHub {
    /// GitHub itself, with the `gh` and `git` programs on `PATH`.
    fn default() -> Self {
        GitHub::new(which::which("gh").ok(), which::which("git").ok())
    }
}

impl GitHub {
    /// GitHub itself, reached with these programs; `None` is a program that is not installed.
    pub fn new(gh: Option<PathBuf>, git: Option<PathBuf>) -> Self {
        GitHub {
            api: "https://api.github.com".to_string(),
            raw: "https://raw.githubusercontent.com".to_string(),
            remote: "https://github.com".to_string(),
            gh,
            git,
            signed_in: OnceCell::new(),
            cloning: Mutex::new(()),
        }
    }

    /// The same, with the REST API at `api`, file contents at `raw` and repositories to clone
    /// under `remote`, in place of `api.github.com`, `raw.githubusercontent.com` and `github.com`.
    pub fn at(mut self, api: &str, raw: &str, remote: &str) -> Self {
        self.api = api.trim_end_matches('/').to_string();
        self.raw = raw.trim_end_matches('/').to_string();
        self.remote = remote.trim_end_matches('/').to_string();
        self
    }

    /// What `target` holds, as text; `None` when the address is best fetched as it is.
    pub(super) async fn read(
        &self,
        target: &Target,
        url: &Url,
        http: &Fetcher,
        ctx: &ToolCtx,
    ) -> Result<Option<String>, String> {
        let call = Call {
            github: self,
            http,
            ctx,
        };
        match target {
            Target::Repo(repo) => call.repository(repo).await.map(Some),
            Target::Tree { repo, rest } => call.directory(repo, rest).await.map(Some),
            Target::Blob { repo, rest, lines } => {
                let text = call.file(repo, rest, url).await?;
                Ok(Some(match lines {
                    Some(lines) => select_lines(&text, *lines),
                    None => text,
                }))
            }
            // Without a login the address itself serves the file, with no hourly limit.
            Target::Raw { .. } if !self.signed_in(ctx).await => Ok(None),
            Target::Raw { repo, rest } => call.file(repo, rest, url).await.map(Some),
            Target::Pull { repo, number, diff } => call.pull(repo, *number, *diff).await.map(Some),
            Target::Issue { repo, number } => call.issue(repo, *number).await.map(Some),
            Target::Run {
                repo,
                run,
                attempt,
                job,
            } => call.run(repo, *run, *attempt, *job).await.map(Some),
        }
    }

    /// Whether `gh` is installed and holds a login for github.com. Asked once.
    async fn signed_in(&self, ctx: &ToolCtx) -> bool {
        let ask = || async {
            let Some(gh) = &self.gh else { return false };
            let mut command = Command::new(gh);
            // Whether there is a token is all that is asked: the token itself is not read.
            command
                .args(["auth", "token", "--hostname", "github.com"])
                .stdout(Stdio::null());
            matches!(run(command, GH_TIMEOUT, ctx).await, Ok(output) if output.status.success())
        };
        *self.signed_in.get_or_init(ask).await
    }
}

/// A program whose output is captured.
fn program(path: &Path) -> Command {
    let mut command = Command::new(path);
    command.stdout(Stdio::piped());
    command
}

/// Runs `command` to its end, for at most `limit`, with the environment every program an agent
/// starts gets. Nothing can be typed into it.
async fn run(mut command: Command, limit: Duration, ctx: &ToolCtx) -> Result<Output, String> {
    command
        .current_dir(&ctx.cwd)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .env("GH_PAGER", "cat")
        // A clone that needs a login fails; it never asks for one.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        // Pointers to large files stay pointers.
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(env) = ctx.services.get::<AgentEnv>() {
        command.envs(env.vars());
    }
    let program = command.as_std().get_program().to_string_lossy().into_owned();
    tokio::select! {
        output = tokio::time::timeout(limit, command.output()) => output
            .map_err(|_| format!("`{program}` did not finish within {} s", limit.as_secs()))?
            .map_err(|e| format!("cannot run `{program}`: {e}")),
        _ = ctx.cancel.cancelled() => Err("aborted by user".to_string()),
    }
}

/// What a program that failed said, or how it ended when it said nothing.
fn complaint(output: &Output) -> String {
    let said = String::from_utf8_lossy(&output.stderr);
    match said.lines().map(str::trim).rfind(|line| !line.is_empty()) {
        Some(line) => line.to_string(),
        None => format!("it ended with {}", output.status),
    }
}

/// The string under `key`, or nothing.
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// The login of the account under `key`.
fn login<'a>(value: &'a Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(|account| account.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("ghost")
}

/// The day of the time under `key`, as `2026-01-31`.
fn day<'a>(value: &'a Value, key: &str) -> &'a str {
    text(value, key).get(..10).unwrap_or_default()
}

/// The items of the array under `key`.
fn items<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// `value` as a query parameter, when it is the segments of a path.
fn query(value: &str) -> String {
    value.replace('&', "%26").replace('+', "%2B")
}

/// The ways `rest` can be a branch, tag or commit followed by a path: a branch name may hold
/// slashes, and the address does not say where it ends. The shortest name comes first.
fn splits(rest: &[String], needs_path: bool) -> impl Iterator<Item = (String, String)> + '_ {
    // What a raw address may spell out in full.
    let rest = match rest {
        [refs, kind, rest @ ..] if refs == "refs" && (kind == "heads" || kind == "tags") => rest,
        rest => rest,
    };
    let longest = rest.len().saturating_sub(usize::from(needs_path)).min(4);
    (1..=longest).map(move |at| (rest[..at].join("/"), rest[at..].join("/")))
}

/// Lines `first..=last` of `text`, then a line that says which lines of how many they are.
fn select_lines(text: &str, (first, last): (usize, usize)) -> String {
    let total = fetch::lines(text).count();
    if first > total {
        return format!(
            "{text}\n[the file has {total} lines, so there is no line {first}; this is all of it]"
        );
    }
    let last = last.min(total);
    let mut out = String::new();
    for line in fetch::lines(text).skip(first - 1).take(last + 1 - first) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!("[lines {first}-{last} of the file's {total}]"));
    out
}

/// The directories and files of a repository, for its tree.
#[derive(Default)]
struct Dir {
    dirs: BTreeMap<String, Dir>,
    files: Vec<String>,
}

impl Dir {
    fn add(&mut self, path: &str) {
        match path.split_once('/') {
            Some((dir, rest)) => self.dirs.entry(dir.to_string()).or_default().add(rest),
            None => self.files.push(path.to_string()),
        }
    }

    /// The files in this directory and below.
    fn count(&self) -> usize {
        self.files.len() + self.dirs.values().map(Dir::count).sum::<usize>()
    }

    /// How many entries there are `depth` levels down; this directory's own are at level 1.
    fn entries_at(&self, depth: usize) -> usize {
        match depth {
            1 => self.dirs.len() + self.files.len(),
            _ => self.dirs.values().map(|dir| dir.entries_at(depth - 1)).sum(),
        }
    }

    /// Writes the entries down to `depth` levels, directories first; a directory whose content
    /// is deeper than that is one line with the number of files in it.
    fn write(&self, depth: usize, indent: usize, out: &mut Vec<String>) {
        let pad = "  ".repeat(indent);
        for (name, dir) in &self.dirs {
            if depth > 1 {
                out.push(format!("{pad}{name}/"));
                dir.write(depth - 1, indent + 1, out);
            } else {
                let files = dir.count();
                let noun = if files == 1 { "file" } else { "files" };
                out.push(format!("{pad}{name}/ ({files} {noun})"));
            }
        }
        let mut files: Vec<&String> = self.files.iter().collect();
        files.sort();
        out.extend(files.into_iter().map(|name| format!("{pad}{name}")));
    }
}

/// The tree of a repository with these `files` (paths with `/` between their parts), as deep as
/// fits [`MAX_TREE_LINES`] lines.
fn tree(files: &[String]) -> String {
    let mut root = Dir::default();
    for file in files {
        root.add(file);
    }
    let mut depth = 1;
    let mut lines = root.entries_at(1);
    loop {
        let below = root.entries_at(depth + 1);
        if below == 0 || lines + below > MAX_TREE_LINES {
            break;
        }
        lines += below;
        depth += 1;
    }
    let mut out = Vec::new();
    root.write(depth, 0, &mut out);
    let hidden = out.len().saturating_sub(MAX_TREE_LINES);
    out.truncate(MAX_TREE_LINES);
    if hidden > 0 {
        out.push(format!("[{hidden} more entries]"));
    }
    out.join("\n")
}

/// The files under `root`, as paths with `/` between their parts, without what git keeps for
/// itself. `Err` holds the files found when there are more than [`MAX_TREE_FILES`].
fn files_under(root: &Path) -> Result<Vec<String>, Vec<String>> {
    let mut files = Vec::new();
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if is_dir && prefix.is_empty() && name == ".git" {
                continue;
            }
            let path = format!("{prefix}{name}");
            if is_dir {
                pending.push((entry.path(), format!("{path}/")));
            } else if files.len() == MAX_TREE_FILES {
                return Err(files);
            } else {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// The README among the files at the top of `dir`: its name and its text.
fn readme_in(dir: &Path) -> Option<(String, String)> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.to_ascii_lowercase().starts_with("readme"))
        .collect();
    // `README.md` before `README.rst` and the translations.
    names.sort_by_key(|name| (!name.eq_ignore_ascii_case("readme.md"), name.len(), name.clone()));
    let name = names.into_iter().next()?;
    let bytes = std::fs::read(dir.join(&name)).ok()?;
    Some((name, String::from_utf8_lossy(&bytes).into_owned()))
}

/// `readme` under its heading, cut at [`MAX_README_CHARS`] characters with a line that says so.
fn readme_section(name: &str, readme: &str, whole: Option<&Path>) -> String {
    let readme = readme.trim();
    match readme.char_indices().nth(MAX_README_CHARS) {
        None => format!("\n\n## {name}\n\n{readme}"),
        Some((end, _)) => {
            let rest = match whole {
                Some(path) => format!("; the whole file is {}", path.display()),
                None => String::new(),
            };
            format!(
                "\n\n## {name}\n\n{}\n[the first {MAX_README_CHARS} characters of {name}{rest}]",
                &readme[..end]
            )
        }
    }
}

/// The first lines of a repository's answer: its name, what it says it is, and a line of facts.
fn repository_heading(repo: &Repo, about: Option<&Value>) -> String {
    let mut out = format!("# {repo}");
    let Some(about) = about else { return out };
    let description = text(about, "description").trim();
    if !description.is_empty() {
        out.push_str(&format!("\n{description}"));
    }
    let mut facts = Vec::new();
    let branch = text(about, "default_branch");
    if !branch.is_empty() {
        facts.push(format!("default branch {branch}"));
    }
    if let Some(stars) = about.get("stargazers_count").and_then(Value::as_u64) {
        facts.push(format!("{stars} stars"));
    }
    let language = text(about, "language");
    if !language.is_empty() {
        facts.push(language.to_string());
    }
    let license = about
        .get("license")
        .map_or("", |license| text(license, "spdx_id"));
    if !license.is_empty() && license != "NOASSERTION" {
        facts.push(license.to_string());
    }
    if about.get("archived").and_then(Value::as_bool) == Some(true) {
        facts.push("archived".to_string());
    }
    if let Some(parent) = about.get("parent") {
        facts.push(format!("fork of {}", text(parent, "full_name")));
    }
    let homepage = text(about, "homepage");
    if !homepage.is_empty() {
        facts.push(homepage.to_string());
    }
    if !facts.is_empty() {
        out.push_str(&format!("\n{}", facts.join(" · ")));
    }
    out
}

/// A comment, review or description as its author, its day and what it says.
fn remark(out: &mut String, heading: &str, body: &str) {
    out.push_str(&format!("\n\n### {heading}\n\n"));
    let body = body.trim();
    out.push_str(if body.is_empty() { "(no text)" } else { body });
}

/// The part of a job's log a step wrote: the lines stamped from `started` to `completed`, both
/// to the second, with the stamps taken off.
fn step_log<'a>(log: &'a str, started: &str, completed: &str) -> Vec<&'a str> {
    let second = |stamp: &'a str| stamp.get(..19);
    let (Some(started), Some(completed)) = (started.get(..19), completed.get(..19)) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    let mut inside = false;
    for line in log.lines() {
        match stamped(line) {
            Some((stamp, rest)) => {
                inside = second(stamp).is_some_and(|at| started <= at && at <= completed);
                if inside {
                    lines.push(rest);
                }
            }
            // A line without a stamp goes on from the one before it.
            None if inside => lines.push(line),
            None => {}
        }
    }
    lines
}

/// A log line as its time stamp (`2026-01-31T12:00:03.1234567Z`) and what follows it.
fn stamped(line: &str) -> Option<(&str, &str)> {
    let line = line.trim_start_matches('\u{feff}');
    let (stamp, rest) = line.split_once(' ').unwrap_or((line, ""));
    let bytes = stamp.as_bytes();
    let is_stamp = bytes.len() >= 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[bytes.len() - 1] == b'Z'
        && bytes[..4].iter().all(u8::is_ascii_digit);
    is_stamp.then_some((stamp, rest))
}

/// The last [`LOG_TAIL_LINES`] of `lines`, under a line that says how many came before.
fn tail(lines: &[&str]) -> String {
    let skipped = lines.len().saturating_sub(LOG_TAIL_LINES);
    let shown = lines[skipped..].join("\n");
    match skipped {
        0 => shown,
        _ => format!("[{skipped} earlier lines are not shown]\n{shown}"),
    }
}

/// The directory a clone is made in before it is moved to its place. It is removed when the
/// clone fails or the call is given up, so that half a clone is never left behind.
struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One read of a GitHub address.
struct Call<'a> {
    github: &'a GitHub,
    http: &'a Fetcher,
    ctx: &'a ToolCtx,
}

impl Call<'_> {
    /// The answer of the REST API to a GET of `path`, asked for as `accept`.
    async fn api(&self, path: &str, accept: &str) -> Result<Vec<u8>, Failure> {
        self.api_within(path, accept, MAX_BODY_BYTES).await
    }

    async fn api_within(&self, path: &str, accept: &str, limit: usize) -> Result<Vec<u8>, Failure> {
        match &self.github.gh {
            Some(gh) if self.github.signed_in(self.ctx).await => {
                let mut command = program(gh);
                command.args(["api", "--hostname", "github.com", "-H"]);
                command.arg(format!("Accept: {accept}"));
                command.arg("-H");
                command.arg(format!("X-GitHub-Api-Version: {API_VERSION}"));
                command.arg(path);
                let output = run(command, GH_TIMEOUT, self.ctx).await.map_err(Failure::Other)?;
                if output.status.success() {
                    return Ok(output.stdout);
                }
                let why = complaint(&output);
                Err(match why.contains("HTTP 404") {
                    true => Failure::Missing,
                    false => Failure::Other(format!("`gh api {path}` failed: {why}")),
                })
            }
            _ => {
                let url = format!("{}/{path}", self.github.api);
                let url = Url::parse(&url).map_err(|e| Failure::Other(format!("{url}: {e}")))?;
                let headers = [("Accept", accept), ("X-GitHub-Api-Version", API_VERSION)];
                let (url, response) = self.http.get(url, &headers).await.map_err(Failure::Other)?;
                let status = response.status();
                if status.is_success() {
                    return fetch::body(response, &url, limit).await.map_err(Failure::Other);
                }
                if status.as_u16() == 404 {
                    return Err(Failure::Missing);
                }
                let spent = response
                    .headers()
                    .get("x-ratelimit-remaining")
                    .is_some_and(|left| left == "0");
                if spent {
                    return Err(Failure::Other(
                        "GitHub's hourly limit for requests without a login is used up; sign in with \
                         `gh auth login` to go on"
                            .to_string(),
                    ));
                }
                let said = fetch::body(response, &url, MAX_BODY_BYTES)
                    .await
                    .unwrap_or_default();
                let said = serde_json::from_slice::<Value>(&said).unwrap_or_default();
                let said = text(&said, "message");
                Err(Failure::Other(match said.is_empty() {
                    true => format!("GitHub answered HTTP {status}"),
                    false => format!("GitHub answered HTTP {status}: {said}"),
                }))
            }
        }
    }

    /// The JSON the REST API answers a GET of `path` with.
    async fn json(&self, path: &str) -> Result<Value, Failure> {
        let body = self.api(path, JSON).await?;
        serde_json::from_slice(&body)
            .map_err(|e| Failure::Other(format!("GitHub's answer for {path} is not JSON: {e}")))
    }

    /// The first hundred items of the list at `path`.
    async fn list(&self, path: String, what: &str) -> Result<Vec<Value>, String> {
        let page = self.json(&format!("{path}?per_page=100")).await;
        match page.map_err(|failure| self.explain(failure, what))? {
            Value::Array(items) => Ok(items),
            _ => Err(format!("GitHub's answer for {what} is not a list")),
        }
    }

    /// `failure` in words; something missing is `what`.
    fn explain(&self, failure: Failure, what: &str) -> String {
        match failure {
            Failure::Other(why) => why,
            Failure::Missing if self.github.signed_in.get() == Some(&true) => {
                format!("GitHub has no {what}, or the account `gh` is signed in to cannot see it")
            }
            Failure::Missing => format!(
                "GitHub has no {what} that can be seen without a login; if it is private, sign in \
                 with `gh auth login`"
            ),
        }
    }

    async fn repository(&self, repo: &Repo) -> Result<String, String> {
        let what = format!("repository {repo}");
        let (about, unknown) = match self.json(&format!("repos/{repo}")).await {
            Ok(about) => (Some(about), None),
            Err(Failure::Missing) => return Err(self.explain(Failure::Missing, &what)),
            // The limit on requests without a login does not stop a clone.
            Err(Failure::Other(why)) => (None, Some(why)),
        };
        let mut out = repository_heading(repo, about.as_ref());
        let megabytes = about
            .as_ref()
            .and_then(|about| about.get("size"))
            .and_then(Value::as_u64)
            .map_or(0, |kilobytes| kilobytes / 1024);
        let cloned = if megabytes > MAX_CLONE_MB {
            Err(format!(
                "it is {megabytes} MB, and repositories over {MAX_CLONE_MB} MB are not cloned"
            ))
        } else {
            self.clone(repo).await
        };
        match cloned {
            Ok(dir) => {
                let at = self.head(&dir).await;
                out.push_str(&format!(
                    "\n\nCloned to {}{at}, one commit deep. Read and search the files there with \
                     `read`, `grep` and `glob`.",
                    dir.display()
                ));
                let read = tokio::task::spawn_blocking(move || {
                    let readme = readme_in(&dir);
                    (files_under(&dir), readme, dir)
                });
                let (files, readme, dir) = read.await.map_err(|_| "the clone could not be listed")?;
                let (files, more) = match files {
                    Ok(files) => (files, ""),
                    Err(files) => (files, "the first "),
                };
                out.push_str(&format!(
                    "\n\n## Files ({more}{})\n\n{}",
                    files.len(),
                    tree(&files)
                ));
                if let Some((name, readme)) = readme {
                    out.push_str(&readme_section(&name, &readme, Some(&dir.join(&name))));
                }
            }
            Err(why) => {
                if let Some(unknown) = unknown {
                    return Err(format!("{repo} could not be cloned ({why}), and {unknown}"));
                }
                out.push_str(&format!(
                    "\n\nNot cloned: {why}. This is what GitHub says is in it."
                ));
                let branch = about.as_ref().map_or("", |about| text(about, "default_branch"));
                let branch = if branch.is_empty() { "HEAD" } else { branch };
                let listing = self
                    .json(&format!("repos/{repo}/git/trees/{}?recursive=1", query(branch)))
                    .await
                    .map_err(|failure| self.explain(failure, &format!("tree of {repo}")))?;
                let files: Vec<String> = items(&listing, "tree")
                    .iter()
                    .filter(|entry| text(entry, "type") != "tree")
                    .map(|entry| text(entry, "path").to_string())
                    .collect();
                let more = match listing.get("truncated").and_then(Value::as_bool) {
                    Some(true) => "the first ",
                    _ => "",
                };
                out.push_str(&format!(
                    "\n\n## Files ({more}{})\n\n{}",
                    files.len(),
                    tree(&files)
                ));
                if let Ok(readme) = self.api(&format!("repos/{repo}/readme"), RAW).await {
                    out.push_str(&readme_section("README", &String::from_utf8_lossy(&readme), None));
                }
            }
        }
        Ok(out)
    }

    /// The directory under the artifacts directory that holds a clone of `repo`, cloning it
    /// when it is not there yet.
    async fn clone(&self, repo: &Repo) -> Result<PathBuf, String> {
        let store = self.ctx.services.get::<ArtifactStore>();
        let store = store.ok_or("there is no artifacts directory to clone into")?;
        let home = store.dir().join("github");
        let dir = home.join(format!("{}-{}", repo.owner, repo.name));
        let _one_at_a_time = self.github.cloning.lock().await;
        if dir.join(".git").exists() {
            return Ok(dir);
        }
        let git = self.github.git.as_ref().ok_or("git is not installed")?;
        // Cloned beside its place and moved there whole, so a clone that was cut short is never
        // taken for a repository.
        let call: String = self
            .ctx
            .call_id
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        let staging = Staging(home.join(format!(".{}-{}-{call}", repo.owner, repo.name)));
        std::fs::create_dir_all(&home).map_err(|e| format!("{}: {e}", home.display()))?;
        let _ = std::fs::remove_dir_all(&staging.0);
        // One commit of one branch, with every file as it is in the repository.
        const SHALLOW: [&str; 6] = [
            "--quiet",
            "--depth=1",
            "--single-branch",
            "--no-tags",
            "--config",
            "core.autocrlf=false",
        ];
        let command = match &self.github.gh {
            Some(gh) if self.github.signed_in(self.ctx).await => {
                let mut command = program(gh);
                command.args(["repo", "clone"]).arg(repo.to_string());
                command.arg(&staging.0).arg("--").args(SHALLOW);
                command
            }
            _ => {
                let mut command = program(git);
                // No stored login is used, and none is asked for: this is a clone of what
                // anyone can see.
                command.args(["-c", "credential.helper=", "-c", "core.longpaths=true", "clone"]);
                command.args(SHALLOW);
                command.arg(format!("{}/{repo}.git", self.github.remote));
                command.arg(&staging.0);
                command
            }
        };
        let failed = match run(command, CLONE_TIMEOUT, self.ctx).await {
            Ok(output) if output.status.success() => None,
            Ok(output) => Some(complaint(&output)),
            Err(why) => Some(why),
        };
        if let Some(why) = failed {
            return Err(format!("the clone failed: {why}"));
        }
        match std::fs::rename(&staging.0, &dir) {
            Ok(()) => Ok(dir),
            Err(error) => Err(format!(
                "the clone could not be moved to {}: {error}",
                dir.display()
            )),
        }
    }

    /// The branch and commit a clone is at, as ` (branch main, commit 1a2b3c4)`, or nothing
    /// when git does not say.
    async fn head(&self, dir: &Path) -> String {
        let Some(git) = &self.github.git else {
            return String::new();
        };
        let mut command = program(git);
        command.arg("-C").arg(dir);
        command.args(["rev-parse", "HEAD", "--abbrev-ref", "HEAD"]);
        let Ok(output) = run(command, GH_TIMEOUT, self.ctx).await else {
            return String::new();
        };
        let said = String::from_utf8_lossy(&output.stdout);
        match said.lines().collect::<Vec<_>>().as_slice() {
            [commit, branch] if output.status.success() => {
                format!(" (branch {branch}, commit {})", commit.get(..7).unwrap_or(commit))
            }
            _ => String::new(),
        }
    }

    /// What a directory of a repository holds.
    async fn directory(&self, repo: &Repo, rest: &[String]) -> Result<String, String> {
        let mut tried = Vec::new();
        for (reference, path) in splits(rest, false) {
            let listing = self
                .json(&format!("repos/{repo}/contents/{path}?ref={}", query(&reference)))
                .await;
            let entries = match listing {
                Ok(Value::Array(entries)) => entries,
                Ok(_) => {
                    return Err(format!(
                        "{path} is a file of {repo}, not a directory: fetch \
                         https://github.com/{repo}/blob/{reference}/{path}"
                    ));
                }
                Err(Failure::Missing) => {
                    tried.push(reference);
                    continue;
                }
                Err(Failure::Other(why)) => return Err(why),
            };
            let place = if path.is_empty() {
                String::new()
            } else {
                format!(": {path}/")
            };
            let mut out = format!("{repo} at {reference}{place}\n");
            let mut lines: Vec<(bool, String)> = entries
                .iter()
                .map(|entry| {
                    let name = text(entry, "name");
                    match text(entry, "type") {
                        "dir" => (false, format!("{name}/")),
                        "file" => {
                            let size = entry.get("size").and_then(Value::as_u64).unwrap_or(0);
                            (true, format!("{name}\t{size} bytes"))
                        }
                        kind => (true, format!("{name}\t{kind}")),
                    }
                })
                .collect();
            lines.sort();
            for (_, line) in lines {
                out.push('\n');
                out.push_str(&line);
            }
            if entries.is_empty() {
                out.push_str("\n(empty directory)");
            }
            return Ok(out);
        }
        let what = format!("directory {} in {repo}", rest.join("/"));
        Err(self.explain(Failure::Missing, &named(&what, &tried)))
    }

    /// The text of a file of a repository.
    async fn file(&self, repo: &Repo, rest: &[String], url: &Url) -> Result<String, String> {
        let what = format!("file {} in {repo}", rest.join("/"));
        if !self.github.signed_in(self.ctx).await {
            // The file server takes the address as it is, slashes in the branch name and all.
            let address = format!("{}/{repo}/{}", self.github.raw, rest.join("/"));
            let address = Url::parse(&address).map_err(|e| format!("{address}: {e}"))?;
            let (address, response) = self.http.get(address, &[]).await?;
            return match response.status().as_u16() {
                200..=299 => {
                    let body = fetch::body(response, &address, MAX_BODY_BYTES).await?;
                    fetch::file_text(url, body, self.ctx).await
                }
                404 => Err(self.explain(Failure::Missing, &what)),
                status => Err(format!("{address} returned HTTP {status}")),
            };
        }
        let mut tried = Vec::new();
        for (reference, path) in splits(rest, true) {
            let path = format!("repos/{repo}/contents/{path}?ref={}", query(&reference));
            match self.api(&path, RAW).await {
                Ok(body) => return fetch::file_text(url, body, self.ctx).await,
                Err(Failure::Missing) => tried.push(reference),
                Err(Failure::Other(why)) => return Err(why),
            }
        }
        Err(self.explain(Failure::Missing, &named(&what, &tried)))
    }

    async fn pull(&self, repo: &Repo, number: u64, diff: bool) -> Result<String, String> {
        let what = format!("pull request {repo}#{number}");
        let path = format!("repos/{repo}/pulls/{number}");
        if diff {
            let diff = self.api(&path, DIFF).await;
            let diff = diff.map_err(|failure| self.explain(failure, &what))?;
            return Ok(String::from_utf8_lossy(&diff).into_owned());
        }
        let pull = self.json(&path).await;
        let pull = pull.map_err(|failure| self.explain(failure, &what))?;
        let (files, comments, reviews, notes) = tokio::join!(
            self.list(format!("{path}/files"), "list of changed files"),
            self.list(
                format!("repos/{repo}/issues/{number}/comments"),
                "list of comments"
            ),
            self.list(format!("{path}/reviews"), "list of reviews"),
            self.list(format!("{path}/comments"), "list of review comments"),
        );
        let (files, comments, reviews, notes) = (files?, comments?, reviews?, notes?);

        let state = if !text(&pull, "merged_at").is_empty() {
            format!("merged {}", day(&pull, "merged_at"))
        } else if pull.get("draft").and_then(Value::as_bool) == Some(true) {
            "draft".to_string()
        } else if text(&pull, "state") == "closed" {
            format!("closed {}", day(&pull, "closed_at"))
        } else {
            "open".to_string()
        };
        let count = |key: &str| pull.get(key).and_then(Value::as_u64).unwrap_or(0);
        let branch = |key: &str| pull.get(key).map_or("", |side| text(side, "label"));
        let mut out = format!(
            "# {} ({repo}#{number})\n{state} · @{} · {} into {} · +{} -{} in {} files · opened {}",
            text(&pull, "title"),
            login(&pull, "user"),
            branch("head"),
            branch("base"),
            count("additions"),
            count("deletions"),
            count("changed_files"),
            day(&pull, "created_at"),
        );
        labels(&mut out, &pull);
        let body = text(&pull, "body").trim();
        out.push_str(&format!(
            "\n\n{}",
            if body.is_empty() { "(no description)" } else { body }
        ));

        if !files.is_empty() {
            out.push_str(&format!("\n\n## Files ({})\n", count("changed_files")));
        }
        for file in &files {
            let lines = |key: &str| file.get(key).and_then(Value::as_u64).unwrap_or(0);
            out.push_str(&format!(
                "\n{} {} +{} -{}",
                text(file, "status"),
                text(file, "filename"),
                lines("additions"),
                lines("deletions")
            ));
        }
        if files.len() as u64 != count("changed_files") {
            out.push_str(&format!("\n[the first {} files]", files.len()));
        }
        // A review without words only holds its comments on lines, which follow.
        let reviews: Vec<&Value> = reviews
            .iter()
            .filter(|review| !text(review, "body").trim().is_empty() || text(review, "state") != "COMMENTED")
            .collect();
        if !reviews.is_empty() {
            out.push_str(&format!("\n\n## Reviews ({})", reviews.len()));
            for review in reviews {
                let verdict = text(review, "state").to_ascii_lowercase().replace('_', " ");
                let heading = format!(
                    "@{} · {verdict} · {}",
                    login(review, "user"),
                    day(review, "submitted_at")
                );
                remark(&mut out, &heading, text(review, "body"));
            }
        }
        if !notes.is_empty() {
            out.push_str(&format!("\n\n## Comments on lines ({})", notes.len()));
            for note in &notes {
                let line = ["line", "original_line"]
                    .iter()
                    .find_map(|key| note.get(*key).and_then(Value::as_u64))
                    .map_or(String::new(), |line| format!(":{line}"));
                let heading = format!(
                    "@{} · {}{line} · {}",
                    login(note, "user"),
                    text(note, "path"),
                    day(note, "created_at")
                );
                remark(&mut out, &heading, text(note, "body"));
            }
        }
        conversation(&mut out, &comments);
        out.push_str(&format!(
            "\n\nThe changes themselves: https://github.com/{repo}/pull/{number}.diff"
        ));
        Ok(out)
    }

    async fn issue(&self, repo: &Repo, number: u64) -> Result<String, String> {
        let what = format!("issue {repo}#{number}");
        let issue = self.json(&format!("repos/{repo}/issues/{number}")).await;
        let issue = issue.map_err(|failure| self.explain(failure, &what))?;
        // GitHub numbers pull requests among the issues.
        if issue.get("pull_request").is_some() {
            return self.pull(repo, number, false).await;
        }
        let comments = self
            .list(
                format!("repos/{repo}/issues/{number}/comments"),
                "list of comments",
            )
            .await?;
        let state = match (text(&issue, "state"), text(&issue, "state_reason")) {
            ("closed", "") => format!("closed {}", day(&issue, "closed_at")),
            ("closed", reason) => {
                format!(
                    "closed {} ({})",
                    day(&issue, "closed_at"),
                    reason.replace('_', " ")
                )
            }
            (state, _) => state.to_string(),
        };
        let mut out = format!(
            "# {} ({repo}#{number})\n{state} · @{} · opened {}",
            text(&issue, "title"),
            login(&issue, "user"),
            day(&issue, "created_at"),
        );
        labels(&mut out, &issue);
        let body = text(&issue, "body").trim();
        out.push_str(&format!(
            "\n\n{}",
            if body.is_empty() { "(no description)" } else { body }
        ));
        conversation(&mut out, &comments);
        let all = issue.get("comments").and_then(Value::as_u64).unwrap_or(0);
        if all > comments.len() as u64 {
            out.push_str(&format!("\n\n[the first {} of {all} comments]", comments.len()));
        }
        Ok(out)
    }

    async fn run(
        &self,
        repo: &Repo,
        run: u64,
        attempt: Option<u64>,
        job: Option<u64>,
    ) -> Result<String, String> {
        let what = format!("Actions run {run} in {repo}");
        let path = match attempt {
            Some(attempt) => format!("repos/{repo}/actions/runs/{run}/attempts/{attempt}"),
            None => format!("repos/{repo}/actions/runs/{run}"),
        };
        let filter = if attempt.is_some() { "" } else { "filter=latest&" };
        let jobs = format!("{path}/jobs?{filter}per_page=100");
        let (about, jobs) = tokio::join!(self.json(&path), self.json(&jobs));
        let about = about.map_err(|failure| self.explain(failure, &what))?;
        let jobs = jobs.map_err(|failure| self.explain(failure, &what))?;
        let jobs: Vec<&Value> = items(&jobs, "jobs")
            .iter()
            .filter(|listed| job.is_none() || listed.get("id").and_then(Value::as_u64) == job)
            .collect();

        let outcome = |value: &Value| match text(value, "conclusion") {
            "" => text(value, "status").replace('_', " "),
            conclusion => conclusion.replace('_', " "),
        };
        let number = |key: &str| about.get(key).and_then(Value::as_u64).unwrap_or(0);
        let mut out = format!(
            "# {} #{}: {} ({repo})\n{} · {} on {} · commit {} · attempt {} · {}",
            text(&about, "name"),
            number("run_number"),
            text(&about, "display_title"),
            outcome(&about),
            text(&about, "event"),
            text(&about, "head_branch"),
            text(&about, "head_sha").get(..7).unwrap_or_default(),
            number("run_attempt"),
            text(&about, "created_at"),
        );
        let failed = |value: &Value| matches!(text(value, "conclusion"), "failure" | "timed_out");
        out.push_str(&format!("\n\n## Jobs ({})\n", jobs.len()));
        for listed in &jobs {
            out.push_str(&format!("\n{}: {}", text(listed, "name"), outcome(listed)));
            for step in items(listed, "steps").iter().filter(|step| failed(step)) {
                let number = step.get("number").and_then(Value::as_u64).unwrap_or(0);
                out.push_str(&format!("; step {number} \"{}\" failed", text(step, "name")));
            }
        }
        let broken: Vec<&&Value> = jobs.iter().filter(|listed| failed(listed)).collect();
        if broken.is_empty() {
            return Ok(out);
        }
        out.push_str("\n\n## Logs of the failed steps");
        for listed in broken.iter().take(MAX_LOG_JOBS) {
            self.failed_steps(repo, listed, &mut out).await;
        }
        if broken.len() > MAX_LOG_JOBS {
            out.push_str(&format!(
                "\n\n[the logs of the first {MAX_LOG_JOBS} of {} failed jobs; fetch a job's own \
                 address for another]",
                broken.len()
            ));
        }
        Ok(out)
    }

    /// Adds what the failed steps of `job` logged to `out`, and where the whole log was saved.
    async fn failed_steps(&self, repo: &Repo, job: &Value, out: &mut String) {
        let id = job.get("id").and_then(Value::as_u64).unwrap_or(0);
        let name = text(job, "name");
        let path = format!("repos/{repo}/actions/jobs/{id}/logs");
        let log = match self.api_within(&path, JSON, MAX_LOG_BYTES).await {
            Ok(log) => log,
            Err(failure) => {
                let why = match failure {
                    Failure::Missing => "GitHub no longer has it".to_string(),
                    Failure::Other(why) => why,
                };
                let login = match self.github.signed_in.get() {
                    Some(true) => "",
                    _ => "; GitHub gives logs only to a login: sign in with `gh auth login`",
                };
                out.push_str(&format!(
                    "\n\n### {name}\n\n(the log could not be read: {why}{login})"
                ));
                return;
            }
        };
        let saved = self.ctx.services.get::<ArtifactStore>().and_then(|store| {
            store
                .save(&format!("{}-job-{id}.log", self.ctx.call_id), &log)
                .ok()
        });
        let log = String::from_utf8_lossy(&log);
        let mut any = false;
        let steps = items(job, "steps");
        for step in steps
            .iter()
            .filter(|step| matches!(text(step, "conclusion"), "failure" | "timed_out"))
        {
            let lines = step_log(&log, text(step, "started_at"), text(step, "completed_at"));
            if lines.is_empty() {
                continue;
            }
            any = true;
            out.push_str(&format!(
                "\n\n### {name} / {}\n\n{}",
                text(step, "name"),
                tail(&lines)
            ));
        }
        if !any {
            // No step is to blame, or the log is not stamped as expected: the end of the log.
            let lines: Vec<&str> = log
                .lines()
                .map(|line| stamped(line).map_or(line, |(_, rest)| rest))
                .collect();
            out.push_str(&format!("\n\n### {name}\n\n{}", tail(&lines)));
        }
        if let Some(saved) = saved {
            out.push_str(&format!("\n[the whole log of this job: {}]", saved.display()));
        }
    }
}

/// `what`, with the branches and tags it was looked for under.
fn named(what: &str, tried: &[String]) -> String {
    match tried {
        [] => what.to_string(),
        tried => format!("{what} (taking the branch or tag to be {})", tried.join(" or ")),
    }
}

/// A line with the labels of an issue or pull request, when it has any.
fn labels(out: &mut String, issue: &Value) {
    let labels: Vec<&str> = items(issue, "labels")
        .iter()
        .map(|label| text(label, "name"))
        .collect();
    if !labels.is_empty() {
        out.push_str(&format!("\nLabels: {}", labels.join(", ")));
    }
}

/// The comments of an issue or pull request, oldest first.
fn conversation(out: &mut String, comments: &[Value]) {
    if comments.is_empty() {
        return;
    }
    out.push_str(&format!("\n\n## Comments ({})", comments.len()));
    for comment in comments {
        let heading = format!("@{} · {}", login(comment, "user"), day(comment, "created_at"));
        remark(out, &heading, text(comment, "body"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(url: &str) -> Option<Target> {
        parse(&Url::parse(url).unwrap())
    }

    fn repo() -> Repo {
        Repo {
            owner: "rust-lang".into(),
            name: "cargo".into(),
        }
    }

    fn words(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn github_addresses_are_told_apart_by_what_they_name() {
        let base = "https://github.com/rust-lang/cargo";
        assert_eq!(target(base), Some(Target::Repo(repo())));
        assert_eq!(target(&format!("{base}.git")), Some(Target::Repo(repo())));
        assert_eq!(target(&format!("{base}/")), Some(Target::Repo(repo())));
        assert_eq!(
            target("https://www.GitHub.com/rust-lang/cargo?tab=readme-ov-file"),
            Some(Target::Repo(repo()))
        );
        assert_eq!(
            target(&format!("{base}/tree/master/src/bin")),
            Some(Target::Tree {
                repo: repo(),
                rest: words(&["master", "src", "bin"])
            })
        );
        assert_eq!(
            target(&format!("{base}/blob/master/src/lib.rs#L10-L24")),
            Some(Target::Blob {
                repo: repo(),
                rest: words(&["master", "src", "lib.rs"]),
                lines: Some((10, 24))
            })
        );
        assert_eq!(
            target(&format!("{base}/raw/master/Cargo.toml")),
            Some(Target::Raw {
                repo: repo(),
                rest: words(&["master", "Cargo.toml"])
            })
        );
        assert_eq!(
            target("https://raw.githubusercontent.com/rust-lang/cargo/refs/heads/master/Cargo.toml"),
            Some(Target::Raw {
                repo: repo(),
                rest: words(&["refs", "heads", "master", "Cargo.toml"])
            })
        );
        let pull = |diff| {
            Some(Target::Pull {
                repo: repo(),
                number: 123,
                diff,
            })
        };
        assert_eq!(target(&format!("{base}/pull/123")), pull(false));
        assert_eq!(target(&format!("{base}/pull/123/commits")), pull(false));
        assert_eq!(target(&format!("{base}/pull/123/files")), pull(true));
        assert_eq!(target(&format!("{base}/pull/123.diff")), pull(true));
        assert_eq!(
            target(&format!("{base}/issues/7#issuecomment-1")),
            Some(Target::Issue {
                repo: repo(),
                number: 7
            })
        );
        let run = |attempt, job| {
            Some(Target::Run {
                repo: repo(),
                run: 99,
                attempt,
                job,
            })
        };
        assert_eq!(target(&format!("{base}/actions/runs/99")), run(None, None));
        assert_eq!(
            target(&format!("{base}/actions/runs/99/job/5")),
            run(None, Some(5))
        );
        assert_eq!(
            target(&format!("{base}/actions/runs/99/attempts/2")),
            run(Some(2), None)
        );
    }

    #[test]
    fn other_pages_of_the_site_are_left_to_the_ordinary_fetch() {
        for page in [
            "https://github.com/",
            "https://github.com/rust-lang",
            "https://github.com/orgs/rust-lang",
            "https://github.com/topics/rust",
            "https://github.com/search?q=cargo",
            "https://github.com/rust-lang/cargo/issues",
            "https://github.com/rust-lang/cargo/issues/new",
            "https://github.com/rust-lang/cargo/pull/abc",
            "https://github.com/rust-lang/cargo/releases/tag/0.90.0",
            "https://github.com/rust-lang/cargo/commit/1a2b3c4",
            "https://github.com/rust-lang/cargo/blob/master",
            "https://github.com/rust-lang/ca%20rgo",
            "https://raw.githubusercontent.com/rust-lang/cargo/master",
            "https://gist.github.com/someone/1a2b3c4",
            "https://example.com/rust-lang/cargo",
        ] {
            assert_eq!(target(page), None, "{page}");
        }
    }

    #[test]
    fn the_lines_of_a_file_address_are_read_from_its_end() {
        assert_eq!(line_range("L7"), Some((7, 7)));
        assert_eq!(line_range("L7-L9"), Some((7, 9)));
        assert_eq!(line_range("L7C3-L9C12"), Some((7, 9)));
        for not_lines in ["", "readme", "L0", "L9-L7", "L7-9", "7"] {
            assert_eq!(line_range(not_lines), None, "{not_lines}");
        }
        let file = "one\ntwo\nthree\nfour\n";
        assert_eq!(
            select_lines(file, (2, 3)),
            "two\nthree\n[lines 2-3 of the file's 4]"
        );
        assert_eq!(select_lines(file, (4, 9)), "four\n[lines 4-4 of the file's 4]");
        assert!(select_lines(file, (5, 6)).starts_with(file));
    }

    #[test]
    fn a_branch_name_with_slashes_is_tried_shortest_first() {
        let rest = words(&["feature", "login", "src", "main.rs"]);
        let file: Vec<_> = splits(&rest, true).collect();
        assert_eq!(
            file,
            [
                ("feature".to_string(), "login/src/main.rs".to_string()),
                ("feature/login".to_string(), "src/main.rs".to_string()),
                ("feature/login/src".to_string(), "main.rs".to_string()),
            ]
        );
        // A directory may be the root of the branch.
        let root: Vec<_> = splits(&words(&["main"]), false).collect();
        assert_eq!(root, [("main".to_string(), String::new())]);
        assert_eq!(splits(&words(&["main"]), true).count(), 0);
        let spelled = words(&["refs", "heads", "main", "a.txt"]);
        let spelled: Vec<_> = splits(&spelled, true).collect();
        assert_eq!(spelled, [("main".to_string(), "a.txt".to_string())]);
        assert_eq!(query("a&b+c/d"), "a%26b%2Bc/d");
    }

    #[test]
    fn a_tree_goes_as_deep_as_fits_and_counts_what_it_leaves_out() {
        let files = words(&["src/main.rs", "src/web/fetch.rs", "Cargo.toml", "README.md"]);
        assert_eq!(
            tree(&files),
            "src/\n  web/\n    fetch.rs\n  main.rs\nCargo.toml\nREADME.md"
        );
        // 3 entries at the top and 300 one level down: only the top fits.
        let mut many: Vec<String> = (0..300).map(|n| format!("data/{n:03}.json")).collect();
        many.push("docs/guide.md".into());
        many.push("LICENSE".into());
        assert_eq!(tree(&many), "data/ (300 files)\ndocs/ (1 file)\nLICENSE");
        // More entries at the top than lines: the list is cut, and says by how much.
        let flat: Vec<String> = (0..MAX_TREE_LINES + 5).map(|n| format!("{n:04}.txt")).collect();
        let shown = tree(&flat);
        assert_eq!(shown.lines().count(), MAX_TREE_LINES + 1);
        assert!(shown.ends_with("0249.txt\n[5 more entries]"), "{shown}");
    }

    #[test]
    fn a_step_log_is_the_lines_stamped_while_the_step_ran() {
        let log = "\u{feff}2026-01-31T12:00:01.0000000Z ##[group]Run actions/checkout\n\
                   2026-01-31T12:00:02.5000000Z done\n\
                   2026-01-31T12:00:03.1000000Z ##[group]Run cargo test\n\
                   2026-01-31T12:00:04.2000000Z test a ... FAILED\n\
                   a line that goes on\n\
                   2026-01-31T12:00:05.9000000Z ##[error]Process completed with exit code 101.\n\
                   2026-01-31T12:00:06.0000000Z Post job cleanup.\n";
        assert_eq!(
            step_log(log, "2026-01-31T12:00:03.000Z", "2026-01-31T12:00:05.000Z"),
            [
                "##[group]Run cargo test",
                "test a ... FAILED",
                "a line that goes on",
                "##[error]Process completed with exit code 101."
            ]
        );
        assert!(step_log(log, "2026-02-01T00:00:00Z", "2026-02-01T00:00:09Z").is_empty());
        assert!(step_log(log, "", "").is_empty());
        assert_eq!(stamped("no stamp here"), None);

        let lines: Vec<String> = (1..=LOG_TAIL_LINES + 2).map(|n| format!("line {n}")).collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let shown = tail(&lines);
        assert!(
            shown.starts_with("[2 earlier lines are not shown]\nline 3\n"),
            "{shown}"
        );
        assert_eq!(tail(&lines[..2]), "line 1\nline 2");
    }

    #[test]
    fn a_readme_among_translations_is_the_plain_one_and_a_long_one_is_cut() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["README.zh-CN.md", "readme.md", "README.rst", "notes.txt"] {
            std::fs::write(dir.path().join(name), name).unwrap();
        }
        std::fs::create_dir_all(dir.path().join(".git/objects")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref").unwrap();
        std::fs::create_dir_all(dir.path().join("src/.git")).unwrap();
        std::fs::write(dir.path().join("src/.git/keep"), "").unwrap();
        assert_eq!(
            readme_in(dir.path()),
            Some(("readme.md".to_string(), "readme.md".to_string()))
        );
        let mut files = files_under(dir.path()).unwrap();
        files.sort();
        assert_eq!(
            files,
            [
                "README.rst",
                "README.zh-CN.md",
                "notes.txt",
                "readme.md",
                "src/.git/keep"
            ]
        );

        let long = "é".repeat(MAX_README_CHARS + 1);
        let cut = readme_section("README.md", &long, Some(Path::new("clone/README.md")));
        assert!(cut.ends_with(&format!(
            "é\n[the first {MAX_README_CHARS} characters of README.md; the whole file is clone/README.md]"
        )));
        assert_eq!(
            readme_section("README.md", " hi \n", None),
            "\n\n## README.md\n\nhi"
        );
    }
}
