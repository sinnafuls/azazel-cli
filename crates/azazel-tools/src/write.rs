//! `write` and `edit`: the two tools that change files, both refusing to clobber unseen content.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::{FileTracker, Freshness};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::util::{display_path, parse_input, resolve_path, unified_diff, write_atomic};

pub const NEVER_READ: &str = "read the file before overwriting it";
pub const CHANGED: &str = "file changed since you last read it; read it again";

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

/// The current text of `path` when the agent may overwrite it, or the refusal.
fn fresh_content(tracker: &FileTracker, agent: &str, path: &Path) -> Result<Vec<u8>, ToolOutput> {
    match tracker.check(agent, path) {
        Freshness::NeverRead => Err(ToolOutput::error(NEVER_READ)),
        Freshness::Changed => Err(ToolOutput::error(CHANGED)),
        Freshness::Fresh => {
            std::fs::read(path).map_err(|e| ToolOutput::error(format!("{}: {e}", path.display())))
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

fn diff_details(ctx: &ToolCtx, path: &Path, old: &str, new: &str) -> Value {
    let shown = display_path(&ctx.cwd, path);
    let diff = if old.len() + new.len() <= MAX_DIFF_BYTES {
        unified_diff(old, new, &shown)
    } else {
        String::new()
    };
    json!({"path": shown, "diff": diff})
}

pub struct WriteTool {
    tracker: Arc<FileTracker>,
}

impl WriteTool {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        WriteTool { tracker }
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
         must have been read first. To change part of a file use `edit` instead."
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
        let guard = self.tracker.lock(&path).await;
        if path.is_dir() {
            return ToolOutput::error(format!("{} is a directory", path.display()));
        }
        let old = if path.exists() {
            match fresh_content(&self.tracker, &ctx.agent_id, &path) {
                Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
                Err(refusal) => return refusal,
            }
        } else {
            None
        };
        if let Err(e) = write_atomic(&path, input.content.as_bytes()) {
            return ToolOutput::error(format!("{}: {e}", path.display()));
        }
        self.tracker
            .record_write(&ctx.agent_id, &path, input.content.as_bytes());
        drop(guard);
        let verb = if old.is_some() { "replaced" } else { "created" };
        let summary = format!(
            "{verb} {} ({} bytes)",
            display_path(&ctx.cwd, &path),
            input.content.len()
        );
        ToolOutput::text(with_diagnostics(ctx, &path, summary).await).with_details(diff_details(
            ctx,
            &path,
            old.as_deref().unwrap_or(""),
            &input.content,
        ))
    }
}

pub struct EditTool {
    tracker: Arc<FileTracker>,
}

impl EditTool {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        EditTool { tracker }
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

/// Applies `edits` in order. Matching is done on LF text; a file that uses CRLF throughout gets its
/// line endings back afterwards. Returns the new text and the number of replacements made.
fn apply_edits(original: &str, edits: &[Replacement]) -> Result<(String, usize), String> {
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
    for (i, edit) in edits.iter().enumerate() {
        let n = i + 1;
        let old = to_lf(&edit.old);
        let new = to_lf(&edit.new);
        if old.is_empty() {
            return Err(format!("old text is empty (edit #{n})"));
        }
        match work.matches(&old).count() {
            0 => return Err(format!("old text not found (edit #{n})")),
            1 => work = work.replacen(&old, &new, 1),
            count if edit.replace_all => {
                work = work.replace(&old, &new);
                replaced += count - 1;
            }
            count => {
                return Err(format!(
                    "old text matches {count} times (edit #{n}); add surrounding lines or set replace_all"
                ));
            }
        }
        replaced += 1;
    }
    let body = if crlf { work.replace('\n', "\r\n") } else { work };
    Ok((format!("{bom}{body}"), replaced))
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Change an existing file by exact text replacement. Each edit replaces `old` with `new`; `old` \
         must match the file exactly (including indentation) and occur once, unless `replace_all` is \
         true. Edits apply in order, and nothing is written if any of them fails. Read the file first."
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
        let bytes = match fresh_content(&self.tracker, &ctx.agent_id, &path) {
            Ok(bytes) => bytes,
            Err(refusal) => return refusal,
        };
        let Ok(original) = String::from_utf8(bytes) else {
            return ToolOutput::error(format!(
                "{}: not valid UTF-8; it cannot be edited as text",
                path.display()
            ));
        };
        let (updated, replaced) = match apply_edits(&original, &input.edits) {
            Ok(result) => result,
            Err(error) => return ToolOutput::error(error),
        };
        if updated == original {
            return ToolOutput::error("the edits leave the file unchanged");
        }
        if let Err(e) = write_atomic(&path, updated.as_bytes()) {
            return ToolOutput::error(format!("{}: {e}", path.display()));
        }
        self.tracker
            .record_write(&ctx.agent_id, &path, updated.as_bytes());
        let noun = if replaced == 1 {
            "replacement"
        } else {
            "replacements"
        };
        drop(guard);
        let summary = format!("edited {} ({replaced} {noun})", display_path(&ctx.cwd, &path));
        ToolOutput::text(with_diagnostics(ctx, &path, summary).await)
            .with_details(diff_details(ctx, &path, &original, &updated))
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

    #[test]
    fn edits_apply_in_order_and_see_earlier_results() {
        let (text, n) = apply_edits(
            "one two\n",
            &[edit("one", "1", false), edit("1 two", "done", false)],
        )
        .unwrap();
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
        let (text, n) = apply_edits("x x x\n", &[edit("x", "y", true)]).unwrap();
        assert_eq!((text.as_str(), n), ("y y y\n", 3));
    }

    #[test]
    fn crlf_files_keep_crlf_even_when_the_model_sends_lf() {
        let original = "\u{feff}fn a() {\r\n    one();\r\n}\r\n";
        let (text, _) = apply_edits(
            original,
            &[edit("{\n    one();", "{\n    two();\n    three();", false)],
        )
        .unwrap();
        assert_eq!(text, "\u{feff}fn a() {\r\n    two();\r\n    three();\r\n}\r\n");
    }

    #[test]
    fn mixed_line_endings_are_left_exactly_as_they_are() {
        let original = "a\r\nb\nc\r\n";
        let (text, _) = apply_edits(original, &[edit("b", "B", false)]).unwrap();
        assert_eq!(text, "a\r\nB\nc\r\n");
        // No normalisation happened, so an LF-only `old` cannot span the CRLF.
        assert!(apply_edits(original, &[edit("a\nb", "x", false)]).is_err());
    }
}
