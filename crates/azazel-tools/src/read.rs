//! `read`: text files with line numbers, images, PDFs, notebooks, Office and mail files,
//! directory listings and registered URL schemes.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::media::{MAX_IMAGE_SIDE, image_block, image_mime};
use azazel_core::scheme::{LOCAL, split_url};
use azazel_core::{FileTracker, Lines, Schemes};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::util::{parse_input, resolve_target};

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
const MAX_BYTES: usize = 256 * 1024;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DIR_ENTRIES: usize = 500;

pub struct ReadTool {
    tracker: Arc<FileTracker>,
    schemes: Arc<Schemes>,
}

impl ReadTool {
    pub fn new(tracker: Arc<FileTracker>, schemes: Arc<Schemes>) -> Self {
        ReadTool { tracker, schemes }
    }

    /// What the scheme of the address in `input` says reading it touches, when it is an address
    /// of a registered scheme.
    fn scheme_access(&self, input: &Value) -> Option<(Access, Option<String>)> {
        let (scheme, rest) = split_url(input.get("path")?.as_str()?.trim())?;
        Some(self.schemes.get(scheme)?.access(rest))
    }
}

/// The text of a PDF, read off the async threads: parsing a large one takes a while.
#[cfg(feature = "pdf")]
async fn read_pdf(path: &Path, bytes: Vec<u8>, pages: Option<&str>) -> ToolOutput {
    let pages = match pages.map(crate::pdf::parse_pages).transpose() {
        Ok(pages) => pages,
        Err(error) => return ToolOutput::error(error),
    };
    let read = tokio::task::spawn_blocking(move || crate::pdf::read(&bytes, pages, MAX_BYTES)).await;
    match read {
        Ok(Ok(text)) => ToolOutput::text(text),
        Ok(Err(error)) => ToolOutput::error(format!("{}: {error}", path.display())),
        Err(_) => ToolOutput::error(format!("{}: the PDF could not be read", path.display())),
    }
}

#[cfg(not(feature = "pdf"))]
async fn read_pdf(path: &Path, _: Vec<u8>, _: Option<&str>) -> ToolOutput {
    ToolOutput::error(format!(
        "{}: this build of azazel cannot read PDFs",
        path.display()
    ))
}

/// A notebook as text plus the first image its cells produced.
async fn notebook_output(shown: crate::notebook::Rendered) -> ToolOutput {
    let mut content = vec![azazel_ai::Block::text(shown.text)];
    if let Some((_, bytes)) = shown.image {
        let decoded = tokio::task::spawn_blocking(move || image_block(&bytes, MAX_IMAGE_SIDE)).await;
        if let Ok(Ok(block)) = decoded {
            content.push(block);
        }
    }
    ToolOutput {
        content,
        is_error: false,
        details: None,
    }
}

#[derive(Deserialize)]
struct Input {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
    pages: Option<String>,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file, list a directory, view an image, or fetch a `scheme://` resource. Text comes back \
         as `<line number><tab><text>`, up to 2000 lines per call; use `offset` (1-based line) and `limit` \
         to page through longer files. A PDF comes back as its text, 20 pages per call; `pages` picks \
         them. A notebook (`.ipynb`) comes back cell by cell with its outputs; `offset` and `limit` \
         count cells. A Word, PowerPoint or Excel file (`.docx`, `.pptx`, `.xlsx`) and a mail message \
         (`.msg`, `.eml`) is converted to Markdown in a directory whose path comes back with an outline \
         by line, beside its pictures, its sheets as CSV files or its attachments; `offset` and `limit` \
         then return lines of that Markdown. Read the lines of a file before editing them."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File or directory path, or a scheme:// URL"},
                "offset": {"type": "integer", "minimum": 1, "description": "First line to return (1-based)"},
                "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of lines"},
                "pages": {"type": "string", "description": "For a PDF: the pages to return, such as \"3\" or \"2-6\""}
            },
            "required": ["path"]
        })
    }

    /// A file is a plain read. An address is whatever its scheme says: reading a web address is
    /// a fetch, and is asked about like `web_fetch`.
    fn access(&self, input: &Value) -> Access {
        self.scheme_access(input)
            .map_or(Access::Read, |(access, _)| access)
    }

    fn replay_safe(&self, _: &Value) -> bool {
        true
    }

    /// A file or a directory listing is the same read again; an address is whatever its scheme
    /// fetches, which may change by itself.
    fn repeatable(&self, input: &Value) -> bool {
        input
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|path| split_url(path.trim()).is_none_or(|(scheme, _)| scheme == LOCAL))
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        let path = input.get("path").and_then(Value::as_str);
        let subject = self.scheme_access(input).and_then(|(_, subject)| subject);
        subject.or_else(|| path.map(str::to_string)).into_iter().collect()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        // A `local://` file is a file like any other: it is read, paged and tracked as one.
        let local = split_url(input.path.trim()).is_some_and(|(scheme, _)| scheme == LOCAL);
        if !local && let Some((scheme, rest)) = split_url(input.path.trim()) {
            let lines = Lines {
                offset: input.offset,
                limit: input.limit,
            };
            return match self.schemes.get(scheme) {
                Some(handler) => match handler.read_lines(rest, lines, ctx).await {
                    Ok(content) => ToolOutput {
                        content,
                        is_error: false,
                        details: None,
                    },
                    Err(error) => ToolOutput::error(error),
                },
                None => ToolOutput::error(format!(
                    "unknown scheme `{scheme}://`; supported: {}",
                    self.schemes.names().join(", ")
                )),
            };
        }
        let path = match resolve_target(ctx, &input.path, "read") {
            Ok(path) => path,
            Err(why) => return ToolOutput::error(why),
        };
        let meta = match tokio::fs::metadata(&path).await {
            Ok(meta) => meta,
            Err(e) => return ToolOutput::error(format!("{}: {e}", path.display())),
        };
        if meta.is_dir() {
            return list_dir(&path);
        }
        if meta.len() > MAX_FILE_BYTES {
            return ToolOutput::error(format!(
                "{}: {} bytes is too large to read; use grep or shell tools to inspect it",
                path.display(),
                meta.len()
            ));
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) => return ToolOutput::error(format!("{}: {e}", path.display())),
        };
        if image_mime(&bytes).is_some() {
            let decoded = tokio::task::spawn_blocking(move || image_block(&bytes, MAX_IMAGE_SIDE)).await;
            return match decoded {
                Ok(Ok(block)) => ToolOutput {
                    content: vec![block],
                    is_error: false,
                    details: None,
                },
                Ok(Err(e)) => ToolOutput::error(format!("{}: cannot decode image: {e}", path.display())),
                Err(e) => ToolOutput::error(format!("{}: cannot decode image: {e}", path.display())),
            };
        }
        if bytes.starts_with(b"%PDF-") {
            return read_pdf(&path, bytes, input.pages.as_deref()).await;
        }
        #[cfg(feature = "office")]
        if let Some(kind) = crate::office::kind(&path, &bytes) {
            return crate::office::read(kind, &path, bytes, input.offset, input.limit, ctx).await;
        }
        if bytes.iter().take(8192).any(|&b| b == 0) {
            let kind = path
                .extension()
                .map(|e| format!(".{} file", e.to_string_lossy()))
                .unwrap_or("unknown type".into());
            return ToolOutput::error(format!(
                "{}: binary file ({} bytes, {kind}); it cannot be read as text",
                path.display(),
                bytes.len()
            ));
        }
        #[cfg(feature = "lsp")]
        if let Some(servers) = ctx.services.get::<crate::lsp::LspManager>() {
            servers.touch(&path);
        }
        let is_notebook = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("ipynb"));
        if is_notebook
            && let Some(shown) = crate::notebook::render(&bytes, input.offset, input.limit, MAX_BYTES)
        {
            // Cells are not lines: any read of a notebook is a read of all of it.
            self.tracker.record(&ctx.agent_id, &path, &bytes);
            return match shown {
                Ok(shown) => notebook_output(shown).await,
                Err(error) => ToolOutput::error(error),
            };
        }
        let text = String::from_utf8_lossy(&bytes);
        let (output, shown) = render_text(
            text.strip_prefix('\u{feff}').unwrap_or(&text),
            input.offset,
            input.limit,
        );
        // What the agent has now seen of the file: the lines that came back, which an edit of
        // other lines does not count as having read.
        let line = |number: usize| u32::try_from(number).unwrap_or(u32::MAX);
        match shown {
            Some((first, last)) => {
                self.tracker
                    .record_lines(&ctx.agent_id, &path, &bytes, line(first), line(last));
            }
            None if !output.is_error => self.tracker.record(&ctx.agent_id, &path, &bytes),
            None => {}
        }
        output
    }
}

/// `text` as numbered lines, and the first and last line that are in it (`None` when none is).
pub(crate) fn render_text(
    text: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> (ToolOutput, Option<(usize, usize)>) {
    if text.is_empty() {
        return (ToolOutput::text("(empty file)"), None);
    }
    let lines: Vec<&str> = text.strip_suffix('\n').unwrap_or(text).split('\n').collect();
    let total = lines.len();
    let first = offset.unwrap_or(1).max(1);
    if first > total {
        let past = format!("offset {first} is past the end of the file ({total} lines)");
        return (ToolOutput::error(past), None);
    }
    let limit = limit.unwrap_or(DEFAULT_LIMIT).max(1);
    let mut out = String::new();
    let mut last = first - 1;
    for (i, line) in lines.iter().enumerate().skip(first - 1).take(limit) {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let shown = match line.char_indices().nth(MAX_LINE_CHARS) {
            Some((end, _)) => format!("{}… [line truncated]", &line[..end]),
            None => line.to_string(),
        };
        if !out.is_empty() && out.len() + shown.len() > MAX_BYTES {
            break;
        }
        out.push_str(&format!("{}\t{shown}\n", i + 1));
        last = i + 1;
    }
    if first > 1 || last < total {
        out.push_str(&format!("[lines {first}-{last} of {total}"));
        if last < total {
            out.push_str(&format!("; continue with offset={}", last + 1));
        }
        out.push(']');
    } else {
        out.pop();
    }
    (ToolOutput::text(out), Some((first, last)))
}

fn list_dir(path: &Path) -> ToolOutput {
    let read_dir = match std::fs::read_dir(path) {
        Ok(read_dir) => read_dir,
        Err(e) => return ToolOutput::error(format!("{}: {e}", path.display())),
    };
    let mut entries: Vec<(String, bool, u64)> = read_dir
        .flatten()
        .map(|entry| {
            let meta = entry.metadata().ok();
            let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
            (
                entry.file_name().to_string_lossy().into_owned(),
                is_dir,
                meta.map_or(0, |m| m.len()),
            )
        })
        .collect();
    entries.sort();
    let total = entries.len();
    let mut out = String::new();
    for (name, is_dir, size) in entries.iter().take(MAX_DIR_ENTRIES) {
        if *is_dir {
            out.push_str(&format!("{name}/\n"));
        } else {
            out.push_str(&format!("{name}\t{size} bytes\n"));
        }
    }
    if total == 0 {
        out.push_str("(empty directory)");
    } else if total > MAX_DIR_ENTRIES {
        out.push_str(&format!(
            "[{MAX_DIR_ENTRIES} of {total} entries shown; use glob to narrow down]"
        ));
    } else {
        out.pop();
    }
    ToolOutput::text(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(output: &ToolOutput) -> String {
        output.text_content()
    }

    #[test]
    fn paging_reports_range_and_next_offset() {
        let body: String = (1..=10).map(|i| format!("l{i}\n")).collect();
        let (out, shown) = render_text(&body, Some(4), Some(3));
        assert_eq!(
            text(&out),
            "4\tl4\n5\tl5\n6\tl6\n[lines 4-6 of 10; continue with offset=7]"
        );
        assert_eq!(shown, Some((4, 6)));
        let (tail, shown) = render_text(&body, Some(9), None);
        assert_eq!(text(&tail), "9\tl9\n10\tl10\n[lines 9-10 of 10]");
        assert_eq!(shown, Some((9, 10)));
        let (past, shown) = render_text(&body, Some(11), None);
        assert!(past.is_error && text(&past).contains("past the end of the file (10 lines)"));
        assert_eq!(shown, None);
    }

    #[test]
    fn long_lines_and_large_files_are_bounded() {
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        let out = text(&render_text(&long, None, None).0);
        assert!(out.ends_with("… [line truncated]"));
        assert_eq!(out.chars().filter(|c| *c == 'x').count(), MAX_LINE_CHARS);

        let body: String = (0..2000).map(|_| format!("{}\n", "y".repeat(1000))).collect();
        let (out, shown) = render_text(&body, None, None);
        let out = text(&out);
        assert!(out.len() <= MAX_BYTES + 200, "{}", out.len());
        assert!(out.contains("of 2000; continue with offset="));
        // What was cut off was not shown, so it is not among the lines read.
        let (first, last) = shown.unwrap();
        assert_eq!(first, 1);
        assert!(out.contains(&format!("[lines 1-{last} of 2000")) && last < 2000);
    }
}
