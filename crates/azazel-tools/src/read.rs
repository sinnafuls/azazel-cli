//! `read`: text files with line numbers, images, PDFs, notebooks, directory listings and
//! registered URL schemes.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::media::{MAX_IMAGE_SIDE, image_block, image_mime};
use azazel_core::scheme::split_url;
use azazel_core::{FileTracker, Schemes};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::util::{parse_input, resolve_path};

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
         count cells. Read a file before editing it."
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
        if let Some((scheme, rest)) = split_url(input.path.trim()) {
            return match self.schemes.get(scheme) {
                Some(handler) => match handler.read(rest, ctx).await {
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
        let path = resolve_path(&ctx.cwd, &input.path);
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
        self.tracker.record(&ctx.agent_id, &path, &bytes);
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
            return match shown {
                Ok(shown) => notebook_output(shown).await,
                Err(error) => ToolOutput::error(error),
            };
        }
        let text = String::from_utf8_lossy(&bytes);
        render_text(
            text.strip_prefix('\u{feff}').unwrap_or(&text),
            input.offset,
            input.limit,
        )
    }
}

fn render_text(text: &str, offset: Option<usize>, limit: Option<usize>) -> ToolOutput {
    if text.is_empty() {
        return ToolOutput::text("(empty file)");
    }
    let lines: Vec<&str> = text.strip_suffix('\n').unwrap_or(text).split('\n').collect();
    let total = lines.len();
    let first = offset.unwrap_or(1).max(1);
    if first > total {
        return ToolOutput::error(format!(
            "offset {first} is past the end of the file ({total} lines)"
        ));
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
    ToolOutput::text(out)
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
    fn whole_small_file_has_line_numbers_and_no_footer() {
        let out = render_text("alpha\r\nbeta\n", None, None);
        assert_eq!(text(&out), "1\talpha\n2\tbeta");
        assert_eq!(text(&render_text("", None, None)), "(empty file)");
    }

    #[test]
    fn paging_reports_range_and_next_offset() {
        let body: String = (1..=10).map(|i| format!("l{i}\n")).collect();
        let out = render_text(&body, Some(4), Some(3));
        assert_eq!(
            text(&out),
            "4\tl4\n5\tl5\n6\tl6\n[lines 4-6 of 10; continue with offset=7]"
        );
        let tail = render_text(&body, Some(9), None);
        assert_eq!(text(&tail), "9\tl9\n10\tl10\n[lines 9-10 of 10]");
        let past = render_text(&body, Some(11), None);
        assert!(past.is_error && text(&past).contains("past the end of the file (10 lines)"));
    }

    #[test]
    fn long_lines_and_large_files_are_bounded() {
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        let out = text(&render_text(&long, None, None));
        assert!(out.ends_with("… [line truncated]"));
        assert_eq!(out.chars().filter(|c| *c == 'x').count(), MAX_LINE_CHARS);

        let body: String = (0..2000).map(|_| format!("{}\n", "y".repeat(1000))).collect();
        let out = text(&render_text(&body, None, None));
        assert!(out.len() <= MAX_BYTES + 200, "{}", out.len());
        assert!(out.contains("of 2000; continue with offset="));
    }
}
