//! `patch`: adds, changes, moves and deletes files in one call, in the `*** Begin Patch` envelope
//! that OpenAI's GPT and Codex models are trained to write. Every file is checked before any is
//! written, and each one goes the way of a `write` or an `edit`: the read guard, the formatter,
//! the checker, the language server and the record of what the agent knows.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_ai::ModelRef;
use azazel_core::FileTracker;
use azazel_core::config::{PatchMode, ReadGuard};
use azazel_core::tracker::{LineRange, Splice};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::util::{display_path, parse_input, resolve_path, unified_diff};
use crate::write::{Change, Guarded, Written, commit, replace_ranges, reserved_name};

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";
const END_OF_FILE: &str = "*** End of File";

/// Whether `model` is of a family trained on this patch format: OpenAI's GPT models from GPT-5
/// on (not `gpt-4*`, not the open-weight `gpt-oss`) and the Codex models.
pub fn patch_family(model: &ModelRef) -> bool {
    let id = model
        .model
        .rsplit('/')
        .next()
        .unwrap_or(&model.model)
        .to_ascii_lowercase();
    id.contains("codex") || (id.contains("gpt-") && !id.contains("gpt-4") && !id.contains("oss"))
}

/// One file operation of a patch.
#[derive(Debug, PartialEq)]
enum Op {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

impl Op {
    fn path(&self) -> &str {
        match self {
            Op::Add { path, .. } | Op::Delete { path } | Op::Update { path, .. } => path,
        }
    }
}

/// One `@@` hunk of an update: the lines it expects (`old`, its context and removed lines) and
/// what they become (`new`, its context and added lines).
#[derive(Debug, Default, PartialEq)]
struct Hunk {
    /// The text after `@@`: a line found before the hunk's lines, to tell apart places that look
    /// the same.
    anchor: Option<String>,
    old: Vec<String>,
    new: Vec<String>,
    /// `*** End of File`: the old lines are the last of the file.
    at_end: bool,
}

/// The header that starts a file operation, if `line` is one.
fn op_header(line: &str) -> bool {
    [ADD, DELETE, UPDATE]
        .iter()
        .any(|header| line.starts_with(header))
}

/// The lines between the envelope's first and last line. A patch passed through a here-document
/// (`apply_patch <<'EOF'` … `EOF`) is taken without it.
fn body(text: &str) -> Result<Vec<&str>, String> {
    let mut lines: Vec<&str> = text.trim().lines().collect();
    let wrapped = lines.len() > 2
        && lines[0].contains("<<")
        && lines[1].trim() == BEGIN
        && lines[lines.len() - 2].trim() == END;
    if wrapped {
        lines = lines[1..lines.len() - 1].to_vec();
    }
    if lines.first().map(|line| line.trim()) != Some(BEGIN) {
        return Err(format!("the patch must start with `{BEGIN}`"));
    }
    if lines.len() < 2 || lines.last().map(|line| line.trim()) != Some(END) {
        return Err(format!("the patch must end with `{END}`"));
    }
    Ok(lines[1..lines.len() - 1].to_vec())
}

/// The operations of a patch, in order.
fn parse(text: &str) -> Result<Vec<Op>, String> {
    let lines = body(text)?;
    let mut ops = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end();
        i += 1;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(path) = line.strip_prefix(ADD) {
            let path = path.trim().to_string();
            let mut content = String::new();
            while i < lines.len() && !op_header(lines[i]) {
                let Some(added) = lines[i].strip_prefix('+') else {
                    return Err(format!(
                        "`{ADD}{path}`: every line of a new file starts with `+`, not `{}`",
                        lines[i]
                    ));
                };
                content.push_str(added);
                content.push('\n');
                i += 1;
            }
            ops.push(Op::Add { path, content });
        } else if let Some(path) = line.strip_prefix(DELETE) {
            ops.push(Op::Delete {
                path: path.trim().to_string(),
            });
        } else if let Some(path) = line.strip_prefix(UPDATE) {
            let path = path.trim().to_string();
            let move_to = lines
                .get(i)
                .and_then(|line| line.trim().strip_prefix(MOVE.trim_end()))
                .map(|to| to.trim().to_string());
            if move_to.is_some() {
                i += 1;
            }
            let mut hunks: Vec<Hunk> = Vec::new();
            while i < lines.len() && !op_header(lines[i]) {
                let line = lines[i].trim_end_matches('\r');
                i += 1;
                let trimmed = line.trim_end();
                if trimmed == "@@" || trimmed.starts_with("@@ ") {
                    let anchor = trimmed[2..].trim();
                    hunks.push(Hunk {
                        anchor: (!anchor.is_empty()).then(|| anchor.to_string()),
                        ..Hunk::default()
                    });
                    continue;
                }
                if trimmed == END_OF_FILE {
                    if let Some(hunk) = hunks.last_mut() {
                        hunk.at_end = true;
                    }
                    continue;
                }
                // The first hunk may leave out its `@@`.
                if hunks.is_empty() {
                    hunks.push(Hunk::default());
                }
                let n = hunks.len();
                let hunk = &mut hunks[n - 1];
                match line.chars().next() {
                    // A blank line in a hunk is a blank context line whose space was lost.
                    None => {
                        hunk.old.push(String::new());
                        hunk.new.push(String::new());
                    }
                    Some(' ') => {
                        hunk.old.push(line[1..].to_string());
                        hunk.new.push(line[1..].to_string());
                    }
                    Some('-') => hunk.old.push(line[1..].to_string()),
                    Some('+') => hunk.new.push(line[1..].to_string()),
                    Some(_) => {
                        return Err(format!(
                            "`{UPDATE}{path}`, hunk {n}: a line starts with neither ` `, `-` nor `+`: `{line}`"
                        ));
                    }
                }
            }
            if hunks.is_empty() && move_to.is_none() {
                return Err(format!(
                    "`{UPDATE}{path}` has no hunk and no `{}`",
                    MOVE.trim_end()
                ));
            }
            ops.push(Op::Update { path, move_to, hunks });
        } else {
            return Err(format!(
                "expected `{}`, `{}` or `{}`, found `{line}`",
                ADD.trim_end(),
                UPDATE.trim_end(),
                DELETE.trim_end()
            ));
        }
    }
    if ops.is_empty() {
        return Err("the patch changes no file".to_string());
    }
    Ok(ops)
}

/// The paths a patch names, its move targets included, without checking the rest of it: what
/// permission is asked for.
fn named_paths(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            [ADD, DELETE, UPDATE, MOVE]
                .iter()
                .find_map(|header| line.strip_prefix(header.trim_end()))
                .map(|path| path.trim().to_string())
        })
        .filter(|path| !path.is_empty())
        .collect()
}

/// Where the lines `wanted` are among `lines`, at `from` or after; with `at_end`, only as the
/// last lines. Lines are compared as they are, then without trailing space, then without any
/// space around them, the first way that finds them winning.
fn seek(lines: &[&str], wanted: &[String], from: usize, at_end: bool) -> Option<usize> {
    if wanted.len() > lines.len() {
        return None;
    }
    let last = lines.len() - wanted.len();
    let starts = if at_end {
        last..=last
    } else {
        from.min(last + 1)..=last
    };
    let ways: [fn(&str, &str) -> bool; 3] = [
        |a, b| a == b,
        |a, b| a.trim_end() == b.trim_end(),
        |a, b| a.trim() == b.trim(),
    ];
    ways.iter().find_map(|same| {
        starts.clone().find(|&at| {
            at >= from
                && wanted
                    .iter()
                    .zip(&lines[at..])
                    .all(|(want, have)| same(have, want))
        })
    })
}

/// What `hunks` make of a file's text, the lines of the text they target and the changes as runs
/// of lines, lowest last. `Err` names the hunk that does not fit, counted from 1.
fn apply(original: &str, hunks: &[Hunk]) -> Result<(String, Vec<LineRange>, Vec<Splice>), String> {
    let (bom, body) = match original.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", original),
    };
    let crlf = body.contains("\r\n") && !body.replace("\r\n", "").contains('\n');
    let text = if crlf {
        body.replace("\r\n", "\n")
    } else {
        body.to_string()
    };
    // Where each line starts, and the lines without their endings.
    let mut starts = vec![0];
    starts.extend(
        text.match_indices('\n')
            .map(|(at, _)| at + 1)
            .filter(|&at| at < text.len()),
    );
    if text.is_empty() {
        starts.clear();
    }
    let lines: Vec<&str> = starts
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = starts.get(n + 1).copied().unwrap_or(text.len());
            text[start..end].trim_end_matches('\n')
        })
        .collect();
    let line_end = |n: usize| starts.get(n + 1).copied().unwrap_or(text.len());

    let mut found: Vec<(std::ops::Range<usize>, String)> = Vec::with_capacity(hunks.len());
    let mut cursor = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        let n = index + 1;
        // Where the anchor is: a hunk whose context repeats the anchor line is looked for from it.
        let mut anchored = None;
        if let Some(anchor) = &hunk.anchor {
            let at = seek(&lines, std::slice::from_ref(anchor), cursor, false)
                .ok_or_else(|| format!("hunk {n}: the line `{anchor}` after `@@` was not found"))?;
            anchored = Some(at);
            cursor = at + 1;
        }
        if hunk.old.is_empty() {
            // Only added lines: after the anchor, or at the end of the file.
            let at = if hunk.anchor.is_some() {
                cursor
            } else {
                lines.len()
            };
            let start = starts.get(at).copied().unwrap_or(text.len());
            let mut new = String::new();
            if start == text.len() && !text.is_empty() && !text.ends_with('\n') {
                new.push('\n');
            }
            for line in &hunk.new {
                new.push_str(line);
                new.push('\n');
            }
            found.push((start..start, new));
            cursor = at;
            continue;
        }
        let (mut old, mut new) = (&hunk.old[..], &hunk.new[..]);
        let find = |old: &[String]| {
            seek(&lines, old, cursor, hunk.at_end)
                .or_else(|| anchored.and_then(|from| seek(&lines, old, from, hunk.at_end)))
        };
        let mut at = find(old);
        // A blank line the model added after the last line of the hunk.
        if at.is_none() && old.last().is_some_and(String::is_empty) {
            old = &old[..old.len() - 1];
            if new.last().is_some_and(String::is_empty) {
                new = &new[..new.len() - 1];
            }
            at = find(old);
        }
        let Some(at) = at else {
            let first = old.iter().find(|line| !line.trim().is_empty()).unwrap_or(&old[0]);
            return Err(format!(
                "hunk {n}: its lines were not found, starting with `{first}`"
            ));
        };
        let last = at + old.len() - 1;
        let range = starts[at]..line_end(last);
        let ends_line = text[range.clone()].ends_with('\n');
        let mut replacement = new.join("\n");
        if ends_line && !new.is_empty() {
            replacement.push('\n');
        }
        found.push((range, replacement));
        cursor = last + 1;
    }
    let ranges: Vec<(std::ops::Range<usize>, &str)> = found
        .iter()
        .map(|(range, new)| (range.clone(), new.as_str()))
        .collect();
    let (out, targets, mut splices) = replace_ranges(&text, &ranges);
    // An insertion replaces no line: it targets nothing, and its run is of no old line.
    let mut kept = Vec::with_capacity(targets.len());
    for ((range, new), (target, splice)) in found.iter().zip(targets.into_iter().zip(splices.iter_mut())) {
        if range.is_empty() {
            let added = u32::try_from(new.trim_start_matches('\n').matches('\n').count()).unwrap_or(u32::MAX);
            *splice = Splice {
                first: splice.first + u32::from(new.starts_with('\n')),
                old: 0,
                new: added,
            };
        } else {
            kept.push(target);
        }
    }
    splices.reverse();
    let out = if crlf { out.replace('\n', "\r\n") } else { out };
    Ok((format!("{bom}{out}"), kept, splices))
}

/// A file operation checked and ready to write.
enum Ready {
    Add {
        path: PathBuf,
        old: Option<String>,
        content: String,
    },
    Delete {
        path: PathBuf,
        old: String,
    },
    Update {
        path: PathBuf,
        to: Option<PathBuf>,
        old: String,
        new: String,
        targets: Vec<LineRange>,
        splices: Vec<Splice>,
        hunks: usize,
    },
}

impl Ready {
    fn path(&self) -> &Path {
        match self {
            Ready::Add { path, .. } | Ready::Delete { path, .. } | Ready::Update { path, .. } => path,
        }
    }
}

pub struct PatchTool {
    tracker: Arc<FileTracker>,
    guard: ReadGuard,
    mode: PatchMode,
}

impl PatchTool {
    pub fn new(tracker: Arc<FileTracker>, guard: ReadGuard, mode: PatchMode) -> Self {
        PatchTool { tracker, guard, mode }
    }
}

#[derive(Deserialize)]
struct PatchInput {
    patch_text: String,
}

fn read_text(path: &Path, shown: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{shown}: {e}"))?;
    String::from_utf8(bytes).map_err(|_| format!("{shown}: not valid UTF-8; it cannot be patched as text"))
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

impl PatchTool {
    /// Checks every operation against the files as they are now: nothing is written unless all
    /// of them can be.
    fn prepare(&self, ops: Vec<Op>, ctx: &ToolCtx) -> Result<Vec<Ready>, String> {
        let mut ready = Vec::with_capacity(ops.len());
        let mut seen: Vec<PathBuf> = Vec::new();
        for op in ops {
            let path = resolve_path(&ctx.cwd, op.path());
            let shown = display_path(&ctx.cwd, &path);
            if let Some(refusal) = reserved_name(&path) {
                return Err(refusal);
            }
            if seen.contains(&path) {
                return Err(format!(
                    "{shown} appears twice in the patch; give each file one section"
                ));
            }
            seen.push(path.clone());
            if path.is_dir() {
                return Err(format!("{shown} is a directory"));
            }
            ready.push(match op {
                Op::Add { content, .. } => Ready::Add {
                    old: path.exists().then(|| read_text(&path, &shown)).transpose()?,
                    path,
                    content,
                },
                Op::Delete { .. } => {
                    if !path.is_file() {
                        return Err(format!("{shown}: no such file to delete"));
                    }
                    Ready::Delete {
                        old: read_text(&path, &shown)?,
                        path,
                    }
                }
                Op::Update { move_to, hunks, .. } => {
                    if !path.is_file() {
                        return Err(format!(
                            "{shown}: no such file to update; add it with `{}`",
                            ADD.trim_end()
                        ));
                    }
                    let old = read_text(&path, &shown)?;
                    let (new, targets, splices) =
                        apply(&old, &hunks).map_err(|why| format!("{shown}, {why}; nothing was changed"))?;
                    let to = match move_to.map(|to| resolve_path(&ctx.cwd, &to)) {
                        Some(to) if to == path => None,
                        Some(to) => {
                            let shown_to = display_path(&ctx.cwd, &to);
                            if let Some(refusal) = reserved_name(&to) {
                                return Err(refusal);
                            }
                            if to.exists() || seen.contains(&to) {
                                return Err(format!("cannot move {shown} to {shown_to}: it exists already"));
                            }
                            seen.push(to.clone());
                            Some(to)
                        }
                        None => None,
                    };
                    Ready::Update {
                        path,
                        to,
                        old,
                        new,
                        targets,
                        splices,
                        hunks: hunks.len(),
                    }
                }
            });
        }
        Ok(ready)
    }

    async fn patch(&self, text: &str, ctx: &ToolCtx) -> Result<ToolOutput, String> {
        let ops = parse(text)?;
        let mut paths: Vec<PathBuf> = Vec::new();
        for op in &ops {
            paths.push(resolve_path(&ctx.cwd, op.path()));
            if let Op::Update {
                move_to: Some(to), ..
            } = op
            {
                paths.push(resolve_path(&ctx.cwd, to));
            }
        }
        let held: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let locks = self.tracker.lock_all(&held).await;
        // Read under the locks: what is patched is what the files hold now.
        let ready = self.prepare(ops, ctx)?;

        let mut guards = Vec::with_capacity(ready.len());
        let mut refused = Vec::new();
        for one in &ready {
            let path = one.path();
            let shown = display_path(&ctx.cwd, path);
            let mut guarded = Guarded::new(&self.tracker, self.guard, &ctx.agent_id, path);
            let passed = match one {
                Ready::Add { old: None, .. } => Ok(()),
                Ready::Add { old: Some(_), .. } => guarded.content().and_then(|_| guarded.whole(&shown)),
                Ready::Delete { .. } => guarded.content().map(|_| ()),
                Ready::Update { targets, .. } => guarded
                    .content()
                    .and_then(|_| guarded.lines("the patch", &shown, targets)),
            };
            match passed {
                Ok(()) => guards.push(guarded),
                Err(refusal) => refused.push(format!("{shown}: {}", refusal.text_content())),
            }
        }
        if !refused.is_empty() {
            return Err(format!("nothing was written:\n{}", refused.join("\n")));
        }

        let mut done: Vec<(String, PathBuf, Option<Written>, String)> = Vec::with_capacity(ready.len());
        let mut diffs = String::new();
        let mut files = Vec::with_capacity(ready.len());
        let already = |done: &[(String, PathBuf, Option<Written>, String)]| {
            if done.is_empty() {
                "nothing".to_string()
            } else {
                done.iter()
                    .map(|(_, path, ..)| display_path(&ctx.cwd, path))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        for (one, guarded) in ready.iter().zip(&mut guards) {
            match one {
                Ready::Add { path, old, content } => {
                    let shown = display_path(&ctx.cwd, path);
                    let written = commit(
                        ctx,
                        &self.tracker,
                        path,
                        old.as_deref(),
                        content.clone(),
                        Change::Whole,
                    )
                    .await
                    .map_err(|error| {
                        format!("{}; already written: {}", error.text_content(), already(&done))
                    })?;
                    guarded.spend();
                    diffs.push_str(&unified_diff(
                        old.as_deref().unwrap_or(""),
                        &written.content,
                        &shown,
                    ));
                    let verb = if old.is_some() { "replaced" } else { "created" };
                    let line = format!("A {shown}: {verb} ({} bytes)", written.content.len());
                    files.push(json!({"path": shown, "status": "added"}));
                    done.push((line, path.clone(), Some(written), shown));
                }
                Ready::Delete { path, old } => {
                    let shown = display_path(&ctx.cwd, path);
                    std::fs::remove_file(path)
                        .map_err(|e| format!("{shown}: {e}; already written: {}", already(&done)))?;
                    guarded.spend();
                    diffs.push_str(&unified_diff(old, "", &shown));
                    files.push(json!({"path": shown, "status": "deleted"}));
                    done.push((format!("D {shown}: deleted"), path.clone(), None, shown));
                }
                Ready::Update {
                    path,
                    to,
                    old,
                    new,
                    splices,
                    hunks,
                    ..
                } => {
                    let target = to.as_ref().unwrap_or(path);
                    if to.is_some() {
                        self.tracker.moved(path, target);
                    }
                    let shown = display_path(&ctx.cwd, target);
                    let written = commit(
                        ctx,
                        &self.tracker,
                        target,
                        Some(old),
                        new.clone(),
                        Change::Lines(splices),
                    )
                    .await
                    .map_err(|error| {
                        format!("{}; already written: {}", error.text_content(), already(&done))
                    })?;
                    guarded.spend();
                    let line = match to {
                        Some(_) => {
                            let from = display_path(&ctx.cwd, path);
                            std::fs::remove_file(path).map_err(|e| {
                                format!(
                                    "{from}: {e}; it was copied to {shown}, already written: {}",
                                    already(&done)
                                )
                            })?;
                            files.push(json!({"path": shown, "from": from, "status": "moved"}));
                            format!("R {from} -> {shown}: {}", plural(*hunks, "hunk"))
                        }
                        None => {
                            files.push(json!({"path": shown, "status": "modified"}));
                            format!("M {shown}: {}", plural(*hunks, "hunk"))
                        }
                    };
                    diffs.push_str(&unified_diff(old, &written.content, &shown));
                    done.push((line, target.clone(), Some(written), shown));
                }
            }
        }
        drop(guards);
        drop(locks);
        let count = done.len();
        let first = done.first().map(|(.., shown)| shown.clone()).unwrap_or_default();
        // Each file's language server and checker are asked side by side.
        let reports = done.into_iter().map(|(line, path, written, _)| async move {
            match written {
                Some(written) => written.report(ctx, &path, line).await,
                None => line,
            }
        });
        let reports = futures::future::join_all(reports).await;
        let out = format!("patched {}\n{}", plural(count, "file"), reports.join("\n"));
        Ok(ToolOutput::text(out).with_details(json!({"path": first, "diff": diffs, "files": files})))
    }
}

#[async_trait]
impl Tool for PatchTool {
    fn name(&self) -> &str {
        "patch"
    }

    fn description(&self) -> &str {
        "Add, change, move and delete files with one patch. `patch_text` is the whole patch:\n\
         *** Begin Patch\n\
         *** Add File: <path>      then the new file's lines, each starting with +\n\
         *** Delete File: <path>\n\
         *** Update File: <path>   optionally followed by *** Move to: <new path>\n\
         @@ <a line before the change, optional>\n\
         \u{20}context line            (a space, then the line as it is)\n\
         -removed line\n\
         +added line\n\
         *** End Patch\n\
         Give about three lines of context around each change; an update has one `@@` hunk per \
         place it changes, in file order. Every file is checked before any is written: if a hunk's \
         lines are not found, nothing changes. Read the lines you change first."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "patch_text": {
                    "type": "string",
                    "description": "The whole patch, from `*** Begin Patch` to `*** End Patch`"
                }
            },
            "required": ["patch_text"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Write
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        input
            .get("patch_text")
            .and_then(Value::as_str)
            .map(named_paths)
            .unwrap_or_default()
    }

    fn offered_to(&self, model: &ModelRef) -> bool {
        match self.mode {
            PatchMode::On => true,
            PatchMode::Auto => patch_family(model),
            PatchMode::Off => false,
        }
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: PatchInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        match self.patch(&input.patch_text, ctx).await {
            Ok(output) => output,
            Err(why) => ToolOutput::error(why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(body: &str) -> String {
        format!("{BEGIN}\n{body}\n{END}")
    }

    fn model(id: &str) -> ModelRef {
        ModelRef {
            provider: "p".into(),
            model: id.into(),
            thinking: None,
        }
    }

    #[test]
    fn gpt_and_codex_models_are_the_patch_family() {
        for id in [
            "gpt-5",
            "gpt-5.1-codex-max",
            "openai/gpt-5-mini",
            "codex-mini-latest",
            "GPT-5.2",
        ] {
            assert!(patch_family(&model(id)), "{id}");
        }
        for id in [
            "gpt-4.1",
            "gpt-4o",
            "gpt-oss-120b",
            "claude-opus-5",
            "gemini-3-pro",
            "o3",
        ] {
            assert!(!patch_family(&model(id)), "{id}");
        }
    }

    #[test]
    fn the_envelope_parses_into_its_operations() {
        let ops = parse(&patch(
            "*** Add File: a/new.txt\n+one\n+two\n\
             *** Update File: src/app.py\n*** Move to: src/main.py\n@@ def greet():\n-    print(\"Hi\")\n+    print(\"Hello\")\n\
             *** Delete File: old.txt",
        ))
        .unwrap();
        assert_eq!(
            ops,
            [
                Op::Add {
                    path: "a/new.txt".into(),
                    content: "one\ntwo\n".into()
                },
                Op::Update {
                    path: "src/app.py".into(),
                    move_to: Some("src/main.py".into()),
                    hunks: vec![Hunk {
                        anchor: Some("def greet():".into()),
                        old: vec!["    print(\"Hi\")".into()],
                        new: vec!["    print(\"Hello\")".into()],
                        at_end: false
                    }]
                },
                Op::Delete {
                    path: "old.txt".into()
                },
            ]
        );
        // A here-document around it is taken off.
        let wrapped = format!("apply_patch <<'EOF'\n{}\nEOF\n", patch("*** Delete File: a"));
        assert_eq!(parse(&wrapped).unwrap(), [Op::Delete { path: "a".into() }]);
        assert_eq!(
            named_paths(&patch(
                "*** Update File: a\n*** Move to: b\n@@\n-x\n+y\n*** Add File: c\n+z"
            )),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn a_malformed_patch_is_refused_with_what_is_wrong() {
        assert!(
            parse("*** Add File: a\n+x")
                .unwrap_err()
                .contains("must start with")
        );
        assert!(
            parse(&format!("{BEGIN}\n*** Add File: a\n+x"))
                .unwrap_err()
                .contains("must end with")
        );
        assert!(
            parse(&patch("*** Add File: a\nx"))
                .unwrap_err()
                .contains("starts with `+`")
        );
        assert!(
            parse(&patch("*** Update File: a\n@@\n~x"))
                .unwrap_err()
                .contains("hunk 1")
        );
        assert!(
            parse(&patch("*** Update File: a"))
                .unwrap_err()
                .contains("no hunk")
        );
        assert!(parse(&patch("*** Rename File: a")).unwrap_err().contains("found"));
        assert!(parse(&patch("")).unwrap_err().contains("no file"));
    }

    fn hunk(anchor: Option<&str>, old: &[&str], new: &[&str]) -> Hunk {
        Hunk {
            anchor: anchor.map(str::to_string),
            old: old.iter().map(|line| line.to_string()).collect(),
            new: new.iter().map(|line| line.to_string()).collect(),
            at_end: false,
        }
    }

    #[test]
    fn hunks_find_their_lines_in_order_and_after_their_anchor() {
        let text = "fn a() {\n    x();\n}\nfn b() {\n    x();\n}\n";
        let (out, targets, splices) = apply(
            text,
            &[hunk(Some("fn b() {"), &["    x();"], &["    y();", "    z();"])],
        )
        .unwrap();
        assert_eq!(out, "fn a() {\n    x();\n}\nfn b() {\n    y();\n    z();\n}\n");
        assert_eq!(targets, [(5, 5)]);
        assert_eq!(
            splices,
            [Splice {
                first: 5,
                old: 1,
                new: 2
            }]
        );

        // Two hunks: the second is looked for after the first; the runs come lowest last.
        let (out, targets, splices) = apply(
            text,
            &[
                hunk(None, &["    x();"], &["    one();"]),
                hunk(None, &["    x();"], &[]),
            ],
        )
        .unwrap();
        assert_eq!(out, "fn a() {\n    one();\n}\nfn b() {\n}\n");
        assert_eq!(targets, [(2, 2), (5, 5)]);
        assert_eq!(
            splices,
            [
                Splice {
                    first: 5,
                    old: 1,
                    new: 0
                },
                Splice {
                    first: 2,
                    old: 1,
                    new: 1
                }
            ]
        );

        // Spaces at the ends of lines do not stop a match.
        let (out, ..) = apply("a  \nb\n", &[hunk(None, &["a", "b"], &["a", "c"])]).unwrap();
        assert_eq!(out, "a\nc\n");

        // Only added lines go after the anchor, or at the end.
        let (out, targets, splices) = apply("a\nb\n", &[hunk(Some("a"), &[], &["x"])]).unwrap();
        assert_eq!(out, "a\nx\nb\n");
        assert!(targets.is_empty());
        assert_eq!(
            splices,
            [Splice {
                first: 2,
                old: 0,
                new: 1
            }]
        );
        let (out, ..) = apply("a\nb", &[hunk(None, &[], &["c"])]).unwrap();
        assert_eq!(out, "a\nb\nc\n");

        // Context that repeats the anchor line is found from the anchor.
        let (out, ..) = apply(
            "x\nfn f() {\n    a();\n}\n",
            &[hunk(
                Some("fn f() {"),
                &["fn f() {", "    a();"],
                &["fn f() {", "    b();"],
            )],
        )
        .unwrap();
        assert_eq!(out, "x\nfn f() {\n    b();\n}\n");
    }

    #[test]
    fn line_endings_and_a_bom_are_kept_and_a_missing_line_names_its_hunk() {
        let (out, ..) = apply("\u{feff}a\r\nb\r\n", &[hunk(None, &["b"], &["B", "C"])]).unwrap();
        assert_eq!(out, "\u{feff}a\r\nB\r\nC\r\n");
        let error = apply(
            "a\nb\n",
            &[hunk(None, &["a"], &["A"]), hunk(None, &["zzz"], &["y"])],
        )
        .unwrap_err();
        assert_eq!(error, "hunk 2: its lines were not found, starting with `zzz`");
        let error = apply("a\n", &[hunk(Some("fn missing"), &["a"], &["b"])]).unwrap_err();
        assert!(error.contains("`fn missing`"), "{error}");
        // `*** End of File`: only the last lines will do.
        let at_end = Hunk {
            at_end: true,
            ..hunk(None, &["x"], &["y"])
        };
        assert_eq!(apply("x\na\nx\n", &[at_end]).unwrap().0, "x\na\ny\n");
    }
}
