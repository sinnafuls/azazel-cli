//! `ast_grep`: search and rewrite code by its syntax tree with the external ast-grep program
//! (`ast-grep`, or `sg`). The tool exists only when that program is installed.
//!
//! The program is run as `ast-grep run --pattern=<p> [--rewrite=<r>] [--lang=<l>] --json=stream
//! [paths]`, which prints one match per line as JSON (<https://ast-grep.github.io/guide/tools/json.html>):
//!
//! ```json
//! { "text": "Some(matched)",
//!   "range": { "byteOffset": { "start": 10828, "end": 10841 },
//!              "start": { "line": 303, "column": 2 }, "end": { "line": 303, "column": 15 } },
//!   "file": "crates/config/src/rule/mod.rs",
//!   "lines": "  Some(matched)",
//!   "replacement": "None",
//!   "replacementOffsets": { "start": 10828, "end": 10841 },
//!   "language": "Rust",
//!   "metaVariables": { "single": { "A": { "text": "matched", "range": { } } },
//!                      "multi": {}, "transformed": {} } }
//! ```
//!
//! Lines, columns and byte offsets count from zero; `replacement` and `replacementOffsets` are
//! there only with `--rewrite`. It exits with 0 when something matched and with 1 when nothing
//! did. ast-grep is never asked to write: the tool makes the replacements itself, so that they
//! pass the read guard and are formatted, checked and reported like any other edit.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::agent::ABORTED_TEXT;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::FileTracker;
use azazel_core::config::ReadGuard;
use azazel_core::process::AgentEnv;
use azazel_core::tracker::{LineRange, Splice};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::util::{display_path, parse_input, resolve_path, unified_diff};
use crate::write::{Change, Guarded, Written, commit, replace_ranges};

/// How long one run of ast-grep may take.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Bytes of matches read before the search is called too wide.
const MAX_OUTPUT: usize = 32 * 1024 * 1024;
/// Bytes kept of what ast-grep says on standard error.
const MAX_COMPLAINT: usize = 16 * 1024;
const DEFAULT_MATCHES: usize = 50;
const MAX_MATCHES: usize = 200;
/// Characters of a source line or of a captured text shown for one match.
const MAX_SHOWN_CHARS: usize = 200;
/// Bytes of diff one preview returns; the files that do not fit are named instead.
const MAX_PREVIEW_BYTES: usize = 256 * 1024;
/// Refusals listed when files may not be written.
const MAX_REFUSALS: usize = 10;

/// Whether `program`, found under the name `sg`, is ast-grep: on Linux that name usually
/// belongs to a program that runs a command under another group.
fn is_ast_grep(program: &Path) -> bool {
    let version = std::process::Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    version.is_ok_and(|output| {
        let said = String::from_utf8_lossy(&output.stdout);
        said.starts_with("ast-grep ") || said.starts_with("sg ")
    })
}

/// Where the ast-grep program is, when it is installed.
pub fn program() -> Option<PathBuf> {
    which::which("ast-grep")
        .ok()
        .or_else(|| which::which("sg").ok().filter(|sg| is_ast_grep(sg)))
}

#[derive(Deserialize)]
struct Offsets {
    start: usize,
    end: usize,
}

#[derive(Deserialize)]
struct Point {
    line: u32,
}

#[derive(Deserialize)]
struct Span {
    #[serde(rename = "byteOffset")]
    bytes: Offsets,
    start: Point,
    end: Point,
}

#[derive(Deserialize)]
struct Captured {
    text: String,
}

#[derive(Deserialize, Default)]
struct MetaVariables {
    #[serde(default)]
    single: BTreeMap<String, Captured>,
    #[serde(default)]
    multi: BTreeMap<String, Vec<Captured>>,
}

/// One match as ast-grep prints it.
#[derive(Deserialize)]
struct Match {
    text: String,
    range: Span,
    file: String,
    #[serde(default)]
    lines: String,
    replacement: Option<String>,
    #[serde(rename = "replacementOffsets")]
    replacement_offsets: Option<Offsets>,
    #[serde(rename = "metaVariables", default)]
    captures: MetaVariables,
}

/// The matches in what ast-grep printed: one JSON object per line, or one array of them.
fn parse_matches(output: &str) -> Result<Vec<Match>, String> {
    let unreadable = |e: serde_json::Error| format!("ast-grep's output could not be read: {e}");
    if output.trim_start().starts_with('[') {
        return serde_json::from_str(output).map_err(unreadable);
    }
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(unreadable))
        .collect()
}

fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match joined.char_indices().nth(MAX_SHOWN_CHARS) {
        Some((end, _)) => format!("{}…", &joined[..end]),
        None => joined,
    }
}

/// `path:line: source line`, then one indented line for each metavariable of the pattern.
fn match_lines(shown: &str, found: &Match, out: &mut String) {
    let source = found.lines.lines().next().unwrap_or(&found.text);
    out.push_str(&format!(
        "{shown}:{}: {}",
        found.range.start.line + 1,
        one_line(source)
    ));
    let more = found.range.end.line.saturating_sub(found.range.start.line);
    if more > 0 {
        out.push_str(&format!(" (+{more} lines)"));
    }
    out.push('\n');
    for (name, captured) in &found.captures.single {
        out.push_str(&format!("  ${name} = {}\n", one_line(&captured.text)));
    }
    for (name, captured) in &found.captures.multi {
        let texts: Vec<&str> = captured.iter().map(|one| one.text.as_str()).collect();
        out.push_str(&format!("  $$${name} = {}\n", one_line(&texts.join(" "))));
    }
}

/// Reads `stream` to its end and returns at most `keep` bytes of it, and whether there was more.
async fn read_capped(stream: Option<impl AsyncRead + Unpin>, keep: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut more = false;
    let Some(mut stream) = stream else {
        return (kept, more);
    };
    let mut chunk = vec![0; 64 * 1024];
    while let Ok(read) = stream.read(&mut chunk).await {
        if read == 0 {
            break;
        }
        let room = keep - kept.len();
        kept.extend_from_slice(&chunk[..read.min(room)]);
        more |= read > room;
    }
    (kept, more)
}

/// The replacements one run makes in one file.
struct Rewrite {
    path: PathBuf,
    shown: String,
    old: String,
    new: String,
    /// The lines of `old` that are replaced.
    targets: Vec<LineRange>,
    /// The replacements as runs of lines, lowest last.
    splices: Vec<Splice>,
    replacements: usize,
    /// Matches inside another match, which this run leaves alone.
    nested: usize,
}

/// What the replacements `found` (all in the file at `path`, in the order of the file) make of
/// the file's content `old`. `None` when they change nothing.
fn rewrite(path: &Path, shown: &str, old: String, found: &[&Match]) -> Result<Option<Rewrite>, String> {
    let mut ranges: Vec<(Range<usize>, &str)> = Vec::with_capacity(found.len());
    let mut nested = 0;
    for one in found {
        let (Some(offsets), Some(replacement)) = (&one.replacement_offsets, &one.replacement) else {
            continue;
        };
        let range = offsets.start..offsets.end;
        let matched = &one.range.bytes;
        let as_searched = old.get(matched.start..matched.end) == Some(one.text.as_str())
            && old.is_char_boundary(range.start)
            && old.is_char_boundary(range.end)
            && range.start <= range.end
            && range.end <= old.len();
        if !as_searched {
            return Err(format!("{shown} changed while it was searched; run it again"));
        }
        if ranges.last().is_some_and(|(before, _)| range.start < before.end) {
            nested += 1;
            continue;
        }
        if &old[range.clone()] != replacement {
            ranges.push((range, replacement));
        }
    }
    if ranges.is_empty() {
        return Ok(None);
    }
    let (new, targets, mut splices) = replace_ranges(&old, &ranges);
    splices.reverse();
    Ok(Some(Rewrite {
        path: path.to_path_buf(),
        shown: shown.to_string(),
        new,
        targets,
        splices,
        replacements: ranges.len(),
        nested,
        old,
    }))
}

fn plural(count: usize, one: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {one}s")
    }
}

#[derive(Deserialize)]
struct Input {
    action: String,
    pattern: String,
    lang: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    rewrite: Option<String>,
    max_matches: Option<usize>,
    #[serde(default)]
    apply: bool,
}

/// The `ast_grep` tool.
pub struct AstGrepTool {
    program: PathBuf,
    tracker: Arc<FileTracker>,
    guard: ReadGuard,
}

impl AstGrepTool {
    pub fn new(program: PathBuf, tracker: Arc<FileTracker>, guard: ReadGuard) -> Self {
        AstGrepTool {
            program,
            tracker,
            guard,
        }
    }

    /// Runs ast-grep for `input` and returns its matches in the order of their files and of
    /// their places in them, each with the file it is in.
    async fn matches(&self, input: &Input, ctx: &ToolCtx) -> Result<Vec<(PathBuf, Match)>, String> {
        let mut command = tokio::process::Command::new(&self.program);
        command.arg("run").arg(format!("--pattern={}", input.pattern));
        if let Some(rewrite) = &input.rewrite {
            command.arg(format!("--rewrite={rewrite}"));
        }
        if let Some(lang) = input
            .lang
            .as_deref()
            .map(str::trim)
            .filter(|lang| !lang.is_empty())
        {
            command.arg(format!("--lang={lang}"));
        }
        command.args(["--json=stream", "--color=never"]);
        for path in &input.paths {
            // A path is never taken for an option.
            if path.starts_with('-') {
                command.arg(format!("./{path}"));
            } else {
                command.arg(path);
            }
        }
        command
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(env) = ctx.services.get::<AgentEnv>() {
            command.envs(env.vars());
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("ast-grep could not be started: {e}"))?;
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let run = async {
            let ((found, too_many), (complaint, _)) = tokio::join!(
                read_capped(stdout, MAX_OUTPUT),
                read_capped(stderr, MAX_COMPLAINT)
            );
            (found, too_many, complaint, child.wait().await)
        };
        let (found, too_many, complaint, status) = tokio::select! {
            done = tokio::time::timeout(TIMEOUT, run) => {
                done.map_err(|_| format!("ast-grep did not finish within {} s", TIMEOUT.as_secs()))?
            }
            _ = ctx.cancel.cancelled() => return Err(ABORTED_TEXT.to_string()),
        };
        if too_many {
            return Err("too many matches to read; narrow the pattern or the paths".to_string());
        }
        let complaint = String::from_utf8_lossy(&complaint);
        let complaint = complaint.lines().map(str::trim).find(|line| !line.is_empty());
        let code = status.ok().and_then(|status| status.code());
        // Exit code 1 is "nothing matched", unless ast-grep says what went wrong.
        let failed = match code {
            Some(0) => false,
            Some(1) => found.is_empty() && complaint.is_some(),
            _ => true,
        };
        if failed {
            return Err(match complaint {
                Some(why) => format!("ast-grep failed: {}", one_line(why)),
                None => "ast-grep failed".to_string(),
            });
        }
        let mut matches: Vec<(PathBuf, Match)> = parse_matches(&String::from_utf8_lossy(&found))?
            .into_iter()
            .map(|found| (resolve_path(&ctx.cwd, &found.file), found))
            .collect();
        // ast-grep searches files side by side and prints in the order it finishes them.
        matches.sort_by(|(a_path, a), (b_path, b)| {
            (a_path, a.range.bytes.start, a.range.bytes.end).cmp(&(
                b_path,
                b.range.bytes.start,
                b.range.bytes.end,
            ))
        });
        Ok(matches)
    }

    async fn search(&self, input: &Input, limit: usize, ctx: &ToolCtx) -> Result<ToolOutput, String> {
        let matches = self.matches(input, ctx).await?;
        if matches.is_empty() {
            return Ok(ToolOutput::text("no matches"));
        }
        let mut out = String::new();
        for (path, found) in matches.iter().take(limit) {
            match_lines(&display_path(&ctx.cwd, path), found, &mut out);
        }
        if matches.len() > limit {
            out.push_str(&format!(
                "[{limit} of {} matches shown; narrow the pattern or the paths, or raise max_matches (at most {MAX_MATCHES})]",
                matches.len()
            ));
        } else {
            out.pop();
        }
        Ok(ToolOutput::text(out))
    }

    /// The replacements the first `limit` matches make, file by file, and how many matches
    /// there were beyond them.
    async fn rewrites(
        &self,
        input: &Input,
        limit: usize,
        ctx: &ToolCtx,
    ) -> Result<(Vec<Rewrite>, usize), String> {
        let matches = self.matches(input, ctx).await?;
        let beyond = matches.len().saturating_sub(limit);
        let mut files: Vec<(&Path, Vec<&Match>)> = Vec::new();
        for (path, found) in matches.iter().take(limit) {
            match files.last_mut() {
                Some((last, all)) if *last == path.as_path() => all.push(found),
                _ => files.push((path.as_path(), vec![found])),
            }
        }
        let mut out = Vec::with_capacity(files.len());
        for (path, found) in files {
            let shown = display_path(&ctx.cwd, path);
            let old = std::fs::read_to_string(path).map_err(|e| format!("{shown}: {e}"))?;
            out.extend(rewrite(path, &shown, old, &found)?);
        }
        Ok((out, beyond))
    }

    /// The diff of what the replacement would do. The agent has then seen the lines it replaces.
    async fn preview(&self, input: &Input, limit: usize, ctx: &ToolCtx) -> Result<ToolOutput, String> {
        let (rewrites, beyond) = self.rewrites(input, limit, ctx).await?;
        if rewrites.is_empty() {
            return Ok(ToolOutput::text(if beyond == 0 {
                "no matches"
            } else {
                "the matches looked at are already what the rewrite gives"
            }));
        }
        let mut diffs = String::new();
        let mut shown: Vec<&str> = Vec::new();
        let mut left_out: Vec<&str> = Vec::new();
        let mut replacements = 0;
        let mut nested = 0;
        for one in &rewrites {
            let diff = unified_diff(&one.old, &one.new, &one.shown);
            if !diffs.is_empty() && diffs.len() + diff.len() > MAX_PREVIEW_BYTES {
                left_out.push(&one.shown);
                continue;
            }
            diffs.push_str(&diff);
            shown.push(&one.shown);
            replacements += one.replacements;
            nested += one.nested;
            self.tracker
                .record_ranges(&ctx.agent_id, &one.path, one.old.as_bytes(), &one.targets);
        }
        let mut out = format!(
            "preview: {} in {}, nothing written. To write them, run this again with apply: true and paths: {}\n",
            plural(replacements, "replacement"),
            plural(shown.len(), "file"),
            json!(shown)
        );
        out.push_str(diffs.trim_end());
        if nested > 0 {
            out.push_str(&format!(
                "\n[{} inside another match left as they are; run it again after applying]",
                plural(nested, "match")
            ));
        }
        if !left_out.is_empty() {
            out.push_str(&format!(
                "\n[the preview is full; not shown, and not to be applied before they are: {}]",
                left_out.join(", ")
            ));
        }
        if beyond > 0 {
            out.push_str(&format!(
                "\n[{beyond} more matches not looked at; run it again after applying, or raise max_matches (at most {MAX_MATCHES})]"
            ));
        }
        Ok(ToolOutput::text(out).with_details(json!({ "diff": diffs })))
    }

    /// Writes the replacements to the files `paths` names: every file or none, unless writing
    /// itself fails midway.
    async fn apply(&self, input: &Input, limit: usize, ctx: &ToolCtx) -> Result<ToolOutput, String> {
        let files: Vec<PathBuf> = input
            .paths
            .iter()
            .map(|path| resolve_path(&ctx.cwd, path))
            .collect();
        if files.is_empty() || files.iter().any(|path| !path.is_file()) {
            return Err(
                "with `apply`, `paths` must name the files to change; the preview lists them".to_string(),
            );
        }
        let paths: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
        let locks = self.tracker.lock_all(&paths).await;
        // Searched under the locks: what is replaced is what the files hold now.
        let (rewrites, beyond) = self.rewrites(input, limit, ctx).await?;
        if rewrites.is_empty() {
            return Err("nothing to replace in these files".to_string());
        }
        let mut guards = Vec::with_capacity(rewrites.len());
        let mut refused = Vec::new();
        for one in &rewrites {
            let mut guarded = Guarded::new(&self.tracker, self.guard, &ctx.agent_id, &one.path);
            let passed = guarded.content().and_then(|bytes| {
                if bytes != one.old.as_bytes() {
                    let changed = format!("{} changed while it was searched; run it again", one.shown);
                    return Err(ToolOutput::error(changed));
                }
                guarded.lines("the replacement", &one.shown, &one.targets)
            });
            match passed {
                Ok(()) => guards.push(guarded),
                Err(refusal) => refused.push(format!("{}: {}", one.shown, refusal.text_content())),
            }
        }
        if !refused.is_empty() {
            let more = refused.len().saturating_sub(MAX_REFUSALS);
            refused.truncate(MAX_REFUSALS);
            if more > 0 {
                refused.push(format!("… {more} more files"));
            }
            return Err(format!(
                "nothing was written; preview the replacement (apply: false) or read these lines first:\n{}",
                refused.join("\n")
            ));
        }
        let mut written: Vec<(&Rewrite, Written)> = Vec::with_capacity(rewrites.len());
        for (one, guarded) in rewrites.iter().zip(&mut guards) {
            let change = Change::Lines(&one.splices);
            match commit(
                ctx,
                &self.tracker,
                &one.path,
                Some(&one.old),
                one.new.clone(),
                change,
            )
            .await
            {
                Ok(done) => {
                    guarded.spend();
                    written.push((one, done));
                }
                Err(error) => {
                    let before: Vec<&str> = written.iter().map(|(one, _)| one.shown.as_str()).collect();
                    return Err(format!(
                        "{}; already written: {}",
                        error.text_content(),
                        if before.is_empty() {
                            "nothing".to_string()
                        } else {
                            before.join(", ")
                        }
                    ));
                }
            }
        }
        drop(guards);
        drop(locks);
        let replacements: usize = rewrites.iter().map(|one| one.replacements).sum();
        let nested: usize = rewrites.iter().map(|one| one.nested).sum();
        let mut diffs = String::new();
        for (one, done) in &written {
            diffs.push_str(&unified_diff(&one.old, &done.content, &one.shown));
        }
        // Each file's language server and checker are asked side by side.
        let reports = written.into_iter().map(|(one, done)| {
            let summary = format!("{} ({})", one.shown, plural(one.replacements, "replacement"));
            done.report(ctx, &one.path, summary)
        });
        let reports = futures::future::join_all(reports).await;
        let mut out = format!(
            "replaced: {} in {}\n{}",
            plural(replacements, "replacement"),
            plural(rewrites.len(), "file"),
            reports.join("\n")
        );
        if nested > 0 {
            out.push_str(&format!(
                "\n[{} inside another match left as they are; run it again]",
                plural(nested, "match")
            ));
        }
        if beyond > 0 {
            out.push_str(&format!("\n[{beyond} more matches not looked at; run it again]"));
        }
        Ok(ToolOutput::text(out).with_details(json!({ "diff": diffs })))
    }
}

/// Whether `input` asks for files to be written.
fn writes(input: &Value) -> bool {
    input.get("action").and_then(Value::as_str) == Some("replace")
        && input.get("apply").and_then(Value::as_bool) == Some(true)
}

#[async_trait]
impl Tool for AstGrepTool {
    fn name(&self) -> &str {
        "ast_grep"
    }

    fn description(&self) -> &str {
        "Search code by its syntax tree with ast-grep, or rewrite what matches. `pattern` is code in \
         which `$NAME` stands for any one node and `$$$NAME` for any number of them, such as \
         `console.log($$$ARGS)` or `if ($COND) { return $X }`. search: each match as \
         `path:line: source line`, then what every metavariable stands for there. replace: `rewrite` \
         is the code to put in its place and may use the pattern's metavariables; it returns the \
         diff and writes nothing until you run it again with `apply: true` and `paths` naming the \
         files to write. A file is written only when you have seen the lines being replaced, in \
         the preview or by reading them, and it has not changed since. Give `lang` when the \
         pattern could be more than one language. Prefer this to grep and edit for changes that \
         follow the shape of the code."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["search", "replace"] },
                "pattern": { "type": "string", "description": "Code to match, with $NAME and $$$NAME metavariables" },
                "lang": { "type": "string", "description": "The language of the pattern, such as rust, typescript or python" },
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Files and directories to search; the working directory when left out. With apply: the files to write"
                },
                "rewrite": { "type": "string", "description": "replace: the code that takes the place of each match" },
                "max_matches": { "type": "integer", "minimum": 1, "maximum": MAX_MATCHES, "description": "Matches looked at (50 when left out)" },
                "apply": { "type": "boolean", "description": "replace: write the files; false or left out previews" }
            },
            "required": ["action", "pattern"]
        })
    }

    /// A search and a preview read; only a replacement that is applied writes.
    fn access(&self, input: &Value) -> Access {
        if writes(input) {
            Access::Write
        } else {
            Access::Read
        }
    }

    /// The files and directories named; for a replacement that is applied, the files it writes.
    fn subjects(&self, input: &Value) -> Vec<String> {
        input
            .get("paths")
            .and_then(Value::as_array)
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        if input.pattern.trim().is_empty() {
            return ToolOutput::error("`pattern` is empty");
        }
        let limit = input.max_matches.unwrap_or(DEFAULT_MATCHES).clamp(1, MAX_MATCHES);
        let done = match (input.action.as_str(), input.rewrite.is_some()) {
            ("search", false) => self.search(&input, limit, ctx).await,
            ("search", true) => Err("`rewrite` goes with the action `replace`".to_string()),
            ("replace", false) => Err("`replace` needs `rewrite`".to_string()),
            ("replace", true) if input.apply => self.apply(&input, limit, ctx).await,
            ("replace", true) => self.preview(&input, limit, ctx).await,
            (other, _) => Err(format!("unknown action `{other}`; actions: search, replace")),
        };
        done.unwrap_or_else(ToolOutput::error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A match of `text` at byte `start` of a file, on line `line` (counted from zero), as
    /// ast-grep prints one.
    fn printed(file: &str, text: &str, start: usize, line: u32, replacement: Option<&str>) -> Value {
        let mut value = json!({
            "text": text,
            "range": {
                "byteOffset": { "start": start, "end": start + text.len() },
                "start": { "line": line, "column": 0 },
                "end": { "line": line + text.matches('\n').count() as u32, "column": 0 }
            },
            "file": file,
            "lines": text,
            "language": "Rust",
            "metaVariables": { "single": {}, "multi": {}, "transformed": {} }
        });
        if let Some(replacement) = replacement {
            value["replacement"] = replacement.into();
            value["replacementOffsets"] = json!({ "start": start, "end": start + text.len() });
        }
        value
    }

    #[test]
    fn matches_are_read_from_a_stream_and_from_an_array() {
        // The example of ast-grep's documentation, as `--json=stream` prints it.
        let line = r#"{"text":"Some(matched)","range":{"byteOffset":{"start":10828,"end":10841},"start":{"line":303,"column":2},"end":{"line":303,"column":15}},"file":"crates/config/src/rule/mod.rs","lines":"  Some(matched)","replacement":"None","replacementOffsets":{"start":10828,"end":10841},"language":"Rust","metaVariables":{"single":{"A":{"text":"matched","range":{"byteOffset":{"start":10833,"end":10840},"start":{"line":303,"column":7},"end":{"line":303,"column":14}}}},"multi":{},"transformed":{}}}"#;
        let stream = parse_matches(&format!("{line}\n\n{line}\n")).unwrap();
        assert_eq!(stream.len(), 2);
        let found = &stream[0];
        assert_eq!((found.range.bytes.start, found.range.bytes.end), (10828, 10841));
        assert_eq!(found.replacement.as_deref(), Some("None"));
        let mut out = String::new();
        match_lines("crates/config/src/rule/mod.rs", found, &mut out);
        assert_eq!(
            out,
            "crates/config/src/rule/mod.rs:304: Some(matched)\n  $A = matched\n"
        );

        let array = parse_matches(&format!("[\n{line},\n{line}\n]")).unwrap();
        assert_eq!(array.len(), 2);
        // A search has no replacement, and a match without metavariables says so by leaving them out.
        let plain = parse_matches(r#"{"text":"x","range":{"byteOffset":{"start":0,"end":1},"start":{"line":0,"column":0},"end":{"line":0,"column":1}},"file":"a.rs","lines":"x"}"#).unwrap();
        assert!(plain[0].replacement.is_none() && plain[0].captures.single.is_empty());
        assert!(parse_matches("").unwrap().is_empty());
        let other = parse_matches("usage: sg group").err().unwrap();
        assert!(other.starts_with("ast-grep's output could not be read: "));
    }

    #[test]
    fn a_match_shows_its_first_line_and_every_capture() {
        let mut value = printed("a.ts", "call(\n  one,\n  two\n)", 4, 2, None);
        value["lines"] = "    call(\n  one,\n  two\n)".into();
        value["metaVariables"] = json!({
            "single": { "F": { "text": "call" } },
            "multi": { "ARGS": [{ "text": "one" }, { "text": "," }, { "text": "two" }] }
        });
        let found: Match = serde_json::from_value(value).unwrap();
        let mut out = String::new();
        match_lines("src/a.ts", &found, &mut out);
        assert_eq!(
            out,
            "src/a.ts:3: call( (+3 lines)\n  $F = call\n  $$$ARGS = one , two\n"
        );
    }

    fn found(values: &[Value]) -> Vec<Match> {
        values
            .iter()
            .map(|value| serde_json::from_value(value.clone()).unwrap())
            .collect()
    }

    #[test]
    fn replacements_are_made_in_the_text_and_counted_in_lines() {
        let old = "one\nfoo(1)\nthree\nfoo(\n  2\n)\n".to_string();
        let matches = found(&[
            printed("a.rs", "foo(1)", 4, 1, Some("bar(1)")),
            printed("a.rs", "foo(\n  2\n)", 17, 3, Some("bar(2)")),
        ]);
        let all: Vec<&Match> = matches.iter().collect();
        let done = rewrite(Path::new("a.rs"), "a.rs", old, &all).unwrap().unwrap();
        assert_eq!(done.new, "one\nbar(1)\nthree\nbar(2)\n");
        assert_eq!(done.targets, [(2, 2), (4, 6)]);
        // Lowest last, so that the first does not move the second.
        let run = |first, old, new| Splice { first, old, new };
        assert_eq!(done.splices, [run(4, 3, 1), run(2, 1, 1)]);
        assert_eq!((done.replacements, done.nested), (2, 0));
    }

    #[test]
    fn a_match_inside_another_waits_and_one_that_changes_nothing_is_no_change() {
        let old = "f(f(1))\nsame\n".to_string();
        let matches = found(&[
            printed("a.rs", "f(f(1))", 0, 0, Some("g(f(1))")),
            printed("a.rs", "f(1)", 2, 0, Some("g(1)")),
        ]);
        let all: Vec<&Match> = matches.iter().collect();
        let done = rewrite(Path::new("a.rs"), "a.rs", old.clone(), &all)
            .unwrap()
            .unwrap();
        assert_eq!(
            (done.new.as_str(), done.replacements, done.nested),
            ("g(f(1))\nsame\n", 1, 1)
        );

        let same = found(&[printed("a.rs", "same", 8, 1, Some("same"))]);
        let all: Vec<&Match> = same.iter().collect();
        assert!(
            rewrite(Path::new("a.rs"), "a.rs", old.clone(), &all)
                .unwrap()
                .is_none()
        );

        // The file is no longer what was searched.
        let stale = found(&[printed("a.rs", "gone", 8, 1, Some("x"))]);
        let all: Vec<&Match> = stale.iter().collect();
        assert_eq!(
            rewrite(Path::new("a.rs"), "a.rs", old, &all).err().unwrap(),
            "a.rs changed while it was searched; run it again"
        );
    }

    #[test]
    fn only_an_applied_replacement_is_a_write() {
        let tool = AstGrepTool::new(
            PathBuf::from("sg"),
            Arc::new(FileTracker::new()),
            ReadGuard::Range,
        );
        let search = json!({ "action": "search", "pattern": "x", "paths": ["src"] });
        let preview = json!({ "action": "replace", "pattern": "x", "rewrite": "y" });
        let apply = json!({ "action": "replace", "pattern": "x", "rewrite": "y", "apply": true, "paths": ["a.rs", "b.rs"] });
        assert_eq!(tool.access(&search), Access::Read);
        assert_eq!(tool.access(&preview), Access::Read);
        assert_eq!(tool.access(&apply), Access::Write);
        assert_eq!(tool.subjects(&search), ["src"]);
        assert!(tool.subjects(&preview).is_empty());
        assert_eq!(tool.subjects(&apply), ["a.rs", "b.rs"]);
    }
}
