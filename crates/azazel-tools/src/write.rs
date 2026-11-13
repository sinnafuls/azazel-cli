//! `write` and `edit`: the two tools that change files, both refusing to clobber unseen content.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::config::ReadGuard;
use azazel_core::tracker::{LineRange, Splice};
use azazel_core::{FileTracker, Freshness};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::check::{Checked, Checks};
use crate::format::Formatters;
use crate::util::{display_path, parse_input, resolve_path, unified_diff, write_atomic};

pub const NEVER_READ: &str = "read the file before overwriting it";
pub const CHANGED: &str = "file changed since you last read it; read it again";

/// Names Windows keeps for devices. A file with one of them as its stem (`aux.c`, `NUL`) cannot
/// be opened there, so none is written anywhere: a project made on Linux is cloned on Windows.
const RESERVED_STEMS: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9",
    "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// The refusal for a path whose file name is a device name up to its first dot.
fn reserved_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    RESERVED_STEMS
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
        .then(|| format!("`{name}` is a reserved Windows device name; pick another file name"))
}

/// Above this size no diff is attached to the result.
const MAX_DIFF_BYTES: usize = 1024 * 1024;

fn path_subject(input: &Value) -> Vec<String> {
    input
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .into_iter()
        .collect()
}

/// Files that are prose or output rather than code: any read of one lets it be edited.
const READ_ANYWHERE: [&str; 3] = ["md", "txt", "log"];

/// `lines 40-52`, `line 7`, `lines 3-4, 9 and 20-22`: at most four ranges, then how many more.
fn lines_text(ranges: &[LineRange]) -> String {
    const SHOWN: usize = 4;
    let mut parts: Vec<String> = ranges
        .iter()
        .take(SHOWN)
        .map(|&(first, last)| {
            if first == last {
                first.to_string()
            } else {
                format!("{first}-{last}")
            }
        })
        .collect();
    let word = match ranges {
        [(first, last)] if first == last => "line",
        _ => "lines",
    };
    if ranges.len() > SHOWN {
        return format!(
            "{word} {} and {} more places",
            parts.join(", "),
            ranges.len() - SHOWN
        );
    }
    let last = parts.pop().unwrap_or_default();
    if parts.is_empty() {
        format!("{word} {last}")
    } else {
        format!("{word} {} and {last}", parts.join(", "))
    }
}

/// The read guard for one change of one file: what the agent must have read of it before it may
/// change it. `/allow-edit` lets one change through; an allowance that was taken and then not
/// used, because the change failed for another reason, is given back.
pub(crate) struct Guarded<'a> {
    tracker: &'a FileTracker,
    mode: ReadGuard,
    agent: &'a str,
    path: &'a Path,
    allowed: bool,
    spent: bool,
}

impl<'a> Guarded<'a> {
    pub fn new(tracker: &'a FileTracker, mode: ReadGuard, agent: &'a str, path: &'a Path) -> Self {
        let anywhere = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                READ_ANYWHERE
                    .iter()
                    .any(|known| extension.eq_ignore_ascii_case(known))
            });
        Guarded {
            tracker,
            mode: match mode {
                ReadGuard::Range if anywhere => ReadGuard::File,
                mode => mode,
            },
            agent,
            path,
            allowed: false,
            spent: false,
        }
    }

    /// Lets the change go on when there is no `refusal` or the user allowed this edit.
    fn pass(&mut self, refusal: Option<String>) -> Result<(), ToolOutput> {
        match refusal {
            Some(why) if !self.allowed => {
                if !self.tracker.take_allowed(self.path) {
                    return Err(ToolOutput::error(why));
                }
                self.allowed = true;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// The current content of the file when the agent has read it and it has not changed since.
    pub fn content(&mut self) -> Result<Vec<u8>, ToolOutput> {
        let refusal = match self.mode {
            ReadGuard::Off => None,
            _ => match self.tracker.check(self.agent, self.path) {
                Freshness::NeverRead => Some(NEVER_READ),
                Freshness::Changed => Some(CHANGED),
                Freshness::Fresh => None,
            },
        };
        self.pass(refusal.map(str::to_string))?;
        std::fs::read(self.path).map_err(|e| ToolOutput::error(format!("{}: {e}", self.path.display())))
    }

    /// Passes when the agent has read every line of the file, which replacing it takes.
    pub fn whole(&mut self, shown: &str) -> Result<(), ToolOutput> {
        let refusal = (self.mode == ReadGuard::Range && !self.tracker.read_whole(self.agent, self.path))
            .then(|| {
                format!(
                    "you have read only part of {shown}; read the rest before overwriting it, or change it with edit"
                )
            });
        self.pass(refusal)
    }

    /// Passes when the agent has read the lines `targets` of the file, which `what` changes.
    pub fn lines(&mut self, what: &str, shown: &str, targets: &[LineRange]) -> Result<(), ToolOutput> {
        let refusal = match self.mode {
            ReadGuard::Range => {
                let unread = self.tracker.unread(self.agent, self.path, targets);
                (!unread.is_empty()).then(|| {
                    let them = match unread[..] {
                        [(first, last)] if first == last => "it",
                        _ => "them",
                    };
                    format!(
                        "{what} targets {} of {shown}, which you have not read; read {them} first",
                        lines_text(&unread)
                    )
                })
            }
            _ => None,
        };
        self.pass(refusal)
    }

    /// The change was made: an allowance that let it through is used up.
    pub fn spend(&mut self) {
        self.spent = true;
    }
}

impl Drop for Guarded<'_> {
    fn drop(&mut self) {
        if self.allowed && !self.spent {
            self.tracker.allow_once(self.path);
        }
    }
}

/// `summary` of a finished write, followed by the errors a language server reports for the new
/// content when one covers the file.
#[cfg(feature = "lsp")]
async fn with_diagnostics(ctx: &ToolCtx, path: &Path, summary: String) -> String {
    let found = match ctx.services.get::<crate::lsp::LspManager>() {
        Some(servers) => servers.after_write(path).await,
        None => None,
    };
    match found {
        Some(found) => format!("{summary}\n{found}"),
        None => summary,
    }
}

#[cfg(not(feature = "lsp"))]
async fn with_diagnostics(_: &ToolCtx, _: &Path, summary: String) -> String {
    summary
}

/// Runs the formatter for `path` when format on write is on, while the caller still holds the
/// file's lock. Returns what the file holds afterwards and a note for the result. The formatter's
/// version of the file is recorded as what the agent knows, so its next edit is not refused as
/// stale (the lines it rewrote count as read where what they replace had been); a formatter that
/// fails costs a note and nothing else.
async fn format_written(
    ctx: &ToolCtx,
    tracker: &FileTracker,
    path: &Path,
    written: String,
) -> (String, Option<String>) {
    let Some(formatters) = ctx.services.get::<Formatters>() else {
        return (written, None);
    };
    let Some(ran) = formatters.run(path, ctx).await else {
        return (written, None);
    };
    let note = match ran {
        Ok(formatter) => format!("formatted with {formatter}"),
        Err(why) => why,
    };
    match std::fs::read(path) {
        Ok(bytes) if bytes != written.as_bytes() => {
            tracker.record_formatted(&ctx.agent_id, path, &bytes);
            (String::from_utf8_lossy(&bytes).into_owned(), Some(note))
        }
        _ => (written, Some(note)),
    }
}

/// Runs the checker for `path` when checks on write are on, while the caller still holds the
/// file's lock and after the formatter. `old` is what the file held before the write, `written`
/// what it holds now.
async fn check_written(ctx: &ToolCtx, path: &Path, old: Option<&str>, written: &str) -> Checked {
    match ctx.services.get::<Checks>() {
        Some(checks) => checks.after_write(ctx, path, old, written).await,
        None => Checked::default(),
    }
}

/// `summary`, then what the formatter did or why the file was not checked.
fn with_note(summary: String, note: Option<String>) -> String {
    match note {
        Some(note) => format!("{summary}; {note}"),
        None => summary,
    }
}

/// `text`, then the errors the file's checker found.
fn with_checks(text: String, block: Option<String>) -> String {
    match block {
        Some(block) => format!("{text}\n{block}"),
        None => text,
    }
}

fn diff_details(ctx: &ToolCtx, path: &Path, old: &str, new: &str) -> Value {
    let shown = display_path(&ctx.cwd, path);
    let diff = if old.len() + new.len() <= MAX_DIFF_BYTES {
        unified_diff(old, new, &shown)
    } else {
        String::new()
    };
    json!({"path": shown, "diff": diff})
}

/// How a change is remembered as what its agent knows of the file.
pub(crate) enum Change<'a> {
    /// The whole content is the agent's.
    Whole,
    /// The agent replaced these runs of lines, in this order.
    Lines(&'a [Splice]),
}

/// A file as a change left it.
pub(crate) struct Written {
    /// What the file holds, after the formatter.
    pub content: String,
    formatted: Option<String>,
    checked: Checked,
}

/// Writes `new` over `path` while the caller holds the file's lock, records it for the agent,
/// then runs the formatter and the checker for the file. `old` is what the file held, `None`
/// for a new file.
pub(crate) async fn commit(
    ctx: &ToolCtx,
    tracker: &FileTracker,
    path: &Path,
    old: Option<&str>,
    new: String,
    change: Change<'_>,
) -> Result<Written, ToolOutput> {
    if let Err(e) = write_atomic(path, new.as_bytes()) {
        return Err(ToolOutput::error(format!("{}: {e}", path.display())));
    }
    match change {
        Change::Whole => tracker.record_write(&ctx.agent_id, path, new.as_bytes()),
        Change::Lines(splices) => tracker.record_edit(
            &ctx.agent_id,
            path,
            old.unwrap_or_default().as_bytes(),
            new.as_bytes(),
            splices,
        ),
    }
    let (content, formatted) = format_written(ctx, tracker, path, new).await;
    let checked = check_written(ctx, path, old, &content).await;
    Ok(Written {
        content,
        formatted,
        checked,
    })
}

impl Written {
    /// `summary`, what the formatter and the checker did, then what the file's language server
    /// and its checker report. Call it once the file's lock is released.
    pub async fn report(self, ctx: &ToolCtx, path: &Path, summary: String) -> String {
        let summary = with_note(with_note(summary, self.formatted), self.checked.note);
        with_checks(with_diagnostics(ctx, path, summary).await, self.checked.block)
    }
}

pub struct WriteTool {
    tracker: Arc<FileTracker>,
    guard: ReadGuard,
}

impl WriteTool {
    pub fn new(tracker: Arc<FileTracker>, guard: ReadGuard) -> Self {
        WriteTool { tracker, guard }
    }
}

#[derive(Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Create a file or replace its whole content. Parent directories are created. An existing file \
         must have been read first, all of it. To change part of a file use `edit` instead."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string", "description": "The complete new content of the file"}
            },
            "required": ["path", "content"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Write
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        path_subject(input)
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: WriteInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        let path = resolve_path(&ctx.cwd, &input.path);
        if let Some(refusal) = reserved_name(&path) {
            return ToolOutput::error(refusal);
        }
        let guard = self.tracker.lock(&path).await;
        if path.is_dir() {
            return ToolOutput::error(format!("{} is a directory", path.display()));
        }
        let mut guarded = Guarded::new(&self.tracker, self.guard, &ctx.agent_id, &path);
        let old = if path.exists() {
            let shown = display_path(&ctx.cwd, &path);
            match guarded
                .content()
                .and_then(|bytes| guarded.whole(&shown).map(|()| bytes))
            {
                Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
                Err(refusal) => return refusal,
            }
        } else {
            None
        };
        let change = commit(
            ctx,
            &self.tracker,
            &path,
            old.as_deref(),
            input.content,
            Change::Whole,
        );
        let written = match change.await {
            Ok(written) => written,
            Err(error) => return error,
        };
        guarded.spend();
        drop(guarded);
        drop(guard);
        let verb = if old.is_some() { "replaced" } else { "created" };
        let summary = format!(
            "{verb} {} ({} bytes)",
            display_path(&ctx.cwd, &path),
            written.content.len()
        );
        let details = diff_details(ctx, &path, old.as_deref().unwrap_or(""), &written.content);
        ToolOutput::text(written.report(ctx, &path, summary).await).with_details(details)
    }
}

pub struct EditTool {
    tracker: Arc<FileTracker>,
    guard: ReadGuard,
}

impl EditTool {
    pub fn new(tracker: Arc<FileTracker>, guard: ReadGuard) -> Self {
        EditTool { tracker, guard }
    }
}

#[derive(Deserialize)]
struct EditInput {
    path: String,
    edits: Vec<Replacement>,
}

#[derive(Deserialize)]
struct Replacement {
    old: String,
    new: String,
    #[serde(default)]
    replace_all: bool,
}

/// What a set of replacements made of a text.
#[derive(Debug, PartialEq)]
pub(crate) struct Applied {
    pub text: String,
    /// How many replacements were made.
    pub replaced: usize,
    /// The lines of the original text each replacement's old text lies on.
    pub targets: Vec<LineRange>,
    /// The replacements as runs of lines, in an order in which they can be applied one by one.
    pub splices: Vec<Splice>,
}

fn line_ends(text: &str) -> u32 {
    u32::try_from(text.matches('\n').count()).unwrap_or(u32::MAX)
}

/// The run of lines that replacing `old`, found on line `line`, with `new` replaces. An `old`
/// that ends with its line leaves the line after it alone; one that ends inside a line rewrites
/// that line.
pub(crate) fn splice_at(line: u32, old: &str, new: &str) -> Splice {
    let open = u32::from(!old.ends_with('\n'));
    Splice {
        first: line,
        old: line_ends(old) + open,
        new: line_ends(new) + open,
    }
}

/// Lines `first..=last` of a text that `splices` were applied to, as lines of the text before
/// them. A line one of them wrote stands for the lines it replaced.
fn before_splices(splices: &[Splice], (mut first, mut last): LineRange) -> LineRange {
    for splice in splices.iter().rev() {
        let end = splice.first + splice.new;
        let back = |line: u32, inside: u32| {
            if line < splice.first {
                line
            } else if line >= end {
                line - splice.new + splice.old
            } else {
                inside
            }
        };
        first = back(first, splice.first);
        last = back(last, splice.first + splice.old.max(1) - 1);
    }
    (first, last.max(first))
}

/// Replaces every text of `found` (byte ranges of `text`, ascending, not overlapping) with the
/// text given for it, and says which lines of `text` that touches and how the lines move.
/// Returns the new text, the lines targeted and the replacements as runs of lines, lowest first.
pub(crate) fn replace_ranges(
    text: &str,
    found: &[(std::ops::Range<usize>, &str)],
) -> (String, Vec<LineRange>, Vec<Splice>) {
    let mut out = String::with_capacity(text.len());
    let mut targets = Vec::with_capacity(found.len());
    let mut splices = Vec::with_capacity(found.len());
    let (mut at, mut line) = (0, 1);
    for (range, new) in found {
        line += line_ends(&text[at..range.start]);
        out.push_str(&text[at..range.start]);
        out.push_str(new);
        let old = &text[range.clone()];
        let splice = splice_at(line, old, new);
        targets.push((line, line + splice.old - 1));
        splices.push(splice);
        line += line_ends(old);
        at = range.end;
    }
    out.push_str(&text[at..]);
    (out, targets, splices)
}

/// Applies `edits` in order. Matching is done on LF text; a file that uses CRLF throughout gets its
/// line endings back afterwards.
fn apply_edits(original: &str, edits: &[Replacement]) -> Result<Applied, String> {
    let (bom, body) = match original.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", original),
    };
    let crlf = body.contains("\r\n") && !body.replace("\r\n", "").contains('\n');
    let to_lf = |text: &str| {
        if crlf {
            text.replace("\r\n", "\n")
        } else {
            text.to_string()
        }
    };
    let mut work = to_lf(body);
    let mut replaced = 0;
    let mut targets = Vec::new();
    let mut splices: Vec<Splice> = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        let n = i + 1;
        let old = to_lf(&edit.old);
        let new = to_lf(&edit.new);
        if old.is_empty() {
            return Err(format!("old text is empty (edit #{n})"));
        }
        let found: Vec<(std::ops::Range<usize>, &str)> = work
            .match_indices(&old)
            .map(|(at, _)| (at..at + old.len(), new.as_str()))
            .collect();
        match found.len() {
            0 => return Err(format!("old text not found (edit #{n})")),
            count if count > 1 && !edit.replace_all => {
                return Err(format!(
                    "old text matches {count} times (edit #{n}); add surrounding lines or set replace_all"
                ));
            }
            count => replaced += count,
        }
        let (text, lines, runs) = replace_ranges(&work, &found);
        targets.extend(lines.into_iter().map(|lines| before_splices(&splices, lines)));
        // Lowest last: each run is counted in the text the runs below it have already changed.
        splices.extend(runs.into_iter().rev());
        work = text;
    }
    let body = if crlf { work.replace('\n', "\r\n") } else { work };
    Ok(Applied {
        text: format!("{bom}{body}"),
        replaced,
        targets,
        splices,
    })
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Change an existing file by exact text replacement. Each edit replaces `old` with `new`; `old` \
         must match the file exactly (including indentation) and occur once, unless `replace_all` is \
         true. Edits apply in order, and nothing is written if any of them fails. Read the lines you \
         change first."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "old": {"type": "string", "description": "Exact text to replace"},
                            "new": {"type": "string", "description": "Replacement text"},
                            "replace_all": {"type": "boolean", "description": "Replace every occurrence"}
                        },
                        "required": ["old", "new"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Write
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        path_subject(input)
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: EditInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        if input.edits.is_empty() {
            return ToolOutput::error("no edits given");
        }
        let path = resolve_path(&ctx.cwd, &input.path);
        let guard = self.tracker.lock(&path).await;
        if !path.is_file() {
            return ToolOutput::error(format!(
                "{}: no such file; use write to create it",
                path.display()
            ));
        }
        let mut guarded = Guarded::new(&self.tracker, self.guard, &ctx.agent_id, &path);
        let bytes = match guarded.content() {
            Ok(bytes) => bytes,
            Err(refusal) => return refusal,
        };
        let Ok(original) = String::from_utf8(bytes) else {
            return ToolOutput::error(format!(
                "{}: not valid UTF-8; it cannot be edited as text",
                path.display()
            ));
        };
        let applied = match apply_edits(&original, &input.edits) {
            Ok(applied) => applied,
            Err(error) => return ToolOutput::error(error),
        };
        if applied.text == original {
            return ToolOutput::error("the edits leave the file unchanged");
        }
        let shown = display_path(&ctx.cwd, &path);
        if let Err(refusal) = guarded.lines("edit", &shown, &applied.targets) {
            return refusal;
        }
        let change = commit(
            ctx,
            &self.tracker,
            &path,
            Some(&original),
            applied.text,
            Change::Lines(&applied.splices),
        );
        let written = match change.await {
            Ok(written) => written,
            Err(error) => return error,
        };
        guarded.spend();
        drop(guarded);
        drop(guard);
        let noun = if applied.replaced == 1 {
            "replacement"
        } else {
            "replacements"
        };
        let summary = format!("edited {shown} ({} {noun})", applied.replaced);
        let details = diff_details(ctx, &path, &original, &written.content);
        ToolOutput::text(written.report(ctx, &path, summary).await).with_details(details)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str, replace_all: bool) -> Replacement {
        Replacement {
            old: old.into(),
            new: new.into(),
            replace_all,
        }
    }

    /// The text and the number of replacements `edits` make of `original`.
    fn applied(original: &str, edits: &[Replacement]) -> (String, usize) {
        let applied = apply_edits(original, edits).unwrap();
        (applied.text, applied.replaced)
    }

    #[test]
    fn edits_apply_in_order_and_see_earlier_results() {
        let (text, n) = applied(
            "one two\n",
            &[edit("one", "1", false), edit("1 two", "done", false)],
        );
        assert_eq!((text.as_str(), n), ("done\n", 2));
    }

    #[test]
    fn missing_ambiguous_and_empty_old_text_are_errors_naming_the_edit() {
        assert_eq!(
            apply_edits("a\n", &[edit("a", "b", false), edit("zzz", "y", false)]).unwrap_err(),
            "old text not found (edit #2)"
        );
        assert_eq!(
            apply_edits("x x x\n", &[edit("x", "y", false)]).unwrap_err(),
            "old text matches 3 times (edit #1); add surrounding lines or set replace_all"
        );
        assert_eq!(
            apply_edits("x\n", &[edit("", "y", false)]).unwrap_err(),
            "old text is empty (edit #1)"
        );
        let (text, n) = applied("x x x\n", &[edit("x", "y", true)]);
        assert_eq!((text.as_str(), n), ("y y y\n", 3));
    }

    #[test]
    fn crlf_files_keep_crlf_even_when_the_model_sends_lf() {
        let original = "\u{feff}fn a() {\r\n    one();\r\n}\r\n";
        let (text, _) = applied(
            original,
            &[edit("{\n    one();", "{\n    two();\n    three();", false)],
        );
        assert_eq!(text, "\u{feff}fn a() {\r\n    two();\r\n    three();\r\n}\r\n");
    }

    #[test]
    fn mixed_line_endings_are_left_exactly_as_they_are() {
        let original = "a\r\nb\nc\r\n";
        let (text, _) = applied(original, &[edit("b", "B", false)]);
        assert_eq!(text, "a\r\nB\nc\r\n");
        // No normalisation happened, so an LF-only `old` cannot span the CRLF.
        assert!(apply_edits(original, &[edit("a\nb", "x", false)]).is_err());
    }

    fn run(first: u32, old: u32, new: u32) -> Splice {
        Splice { first, old, new }
    }

    #[test]
    fn an_edit_targets_the_lines_its_old_text_lies_on() {
        let text = "one\ntwo\nthree\nfour\nfive\n";
        // Inside one line; across two lines; whole lines with their endings.
        let inside = apply_edits(text, &[edit("hre", "HRE", false)]).unwrap();
        assert_eq!(
            (inside.targets, inside.splices),
            (vec![(3, 3)], vec![run(3, 1, 1)])
        );
        let across = apply_edits(text, &[edit("wo\nth", "x", false)]).unwrap();
        assert_eq!(across.text, "one\ntxree\nfour\nfive\n");
        assert_eq!(
            (across.targets, across.splices),
            (vec![(2, 3)], vec![run(2, 2, 1)])
        );
        // The line after a whole line that is replaced is not touched by it.
        let whole = apply_edits(text, &[edit("two\nthree\n", "2\n3\n3.5\n", false)]).unwrap();
        assert_eq!((whole.targets, whole.splices), (vec![(2, 3)], vec![run(2, 2, 3)]));
        let removed = apply_edits(text, &[edit("four\n", "", false)]).unwrap();
        assert_eq!(
            (removed.targets, removed.splices),
            (vec![(4, 4)], vec![run(4, 1, 0)])
        );
        // The last line of a file that does not end its last line.
        let open = apply_edits("a\nb", &[edit("b", "b\nc", false)]).unwrap();
        assert_eq!((open.targets, open.splices), (vec![(2, 2)], vec![run(2, 1, 2)]));
    }

    #[test]
    fn later_edits_are_counted_in_the_lines_of_the_file_as_it_was() {
        let text = "one\ntwo\nthree\nfour\nfive\nsix\n";
        // The first edit makes line 2 three lines; the second then finds `five` on line 7.
        let both = apply_edits(
            text,
            &[edit("two", "2a\n2b\n2c", false), edit("five", "5", false)],
        )
        .unwrap();
        assert_eq!(both.targets, [(2, 2), (5, 5)]);
        assert_eq!(both.splices, [run(2, 1, 3), run(7, 1, 1)]);
        // An edit of what an earlier edit wrote targets what that one replaced.
        let again = apply_edits(
            text,
            &[
                edit("two\nthree", "x\ny\nz", false),
                edit("y\nz\nfour", "w", false),
            ],
        )
        .unwrap();
        assert_eq!(again.text, "one\nx\nw\nfive\nsix\n");
        assert_eq!(again.targets, [(2, 3), (2, 4)]);

        // Every occurrence is a target; the runs are given lowest last, so that applying them
        // in turn never moves one still to come.
        let all = apply_edits("a\nb a\nc\na a\n", &[edit("a", "1\n2", true)]).unwrap();
        assert_eq!(all.text, "1\n2\nb 1\n2\nc\n1\n2 1\n2\n");
        assert_eq!(all.replaced, 4);
        assert_eq!(all.targets, [(1, 1), (2, 2), (4, 4), (4, 4)]);
        assert_eq!(
            all.splices,
            [run(4, 1, 2), run(4, 1, 2), run(2, 1, 2), run(1, 1, 2)]
        );
    }

    #[test]
    fn unread_lines_are_named_briefly() {
        assert_eq!(lines_text(&[(40, 52)]), "lines 40-52");
        assert_eq!(lines_text(&[(7, 7)]), "line 7");
        assert_eq!(lines_text(&[(3, 4), (9, 9), (20, 22)]), "lines 3-4, 9 and 20-22");
        assert_eq!(
            lines_text(&[(1, 1), (3, 3), (5, 5), (7, 7), (9, 9), (11, 12)]),
            "lines 1, 3, 5, 7 and 2 more places"
        );
    }
}
