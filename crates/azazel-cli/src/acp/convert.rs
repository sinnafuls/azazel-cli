//! Between azazel's shapes and the Agent Client Protocol's: prompts, tool calls, MCP servers.

use std::path::{Path, PathBuf};

use agent_client_protocol::v1::schema as acp;
use azazel_agent::{Access, ToolOutput};
use azazel_ai::Block;
use serde_json::{Map, Value, json};

use crate::tui::view::tool_summary;

/// What kind of work a tool does, as the client shows it.
pub(super) fn tool_kind(name: &str, access: Option<Access>) -> acp::ToolKind {
    match name {
        "read" => acp::ToolKind::Read,
        "write" | "edit" => acp::ToolKind::Edit,
        "glob" | "grep" | "semantic_search" | "tool_search" | "memory_search" => acp::ToolKind::Search,
        "shell" | "job" | "pty" | "eval" => acp::ToolKind::Execute,
        "web_fetch" | "web_search" => acp::ToolKind::Fetch,
        // A tool of an extension or an MCP server: what it touches is all that is known of it.
        _ => match access {
            Some(Access::Write) => acp::ToolKind::Edit,
            Some(Access::Exec) => acp::ToolKind::Execute,
            Some(Access::Net) => acp::ToolKind::Fetch,
            Some(Access::Read) | None => acp::ToolKind::Other,
        },
    }
}

/// What a call does, on one line: the tool and what it is about.
pub(super) fn title(name: &str, input: &Value) -> String {
    format!("{name} {}", tool_summary(name, input))
        .trim_end()
        .to_string()
}

/// The file a call is about, when its input names one.
pub(super) fn locations(cwd: &Path, input: &Value) -> Option<Vec<acp::ToolCallLocation>> {
    let path = input.get("path").and_then(Value::as_str)?;
    let mut location = acp::ToolCallLocation::new(cwd.join(path));
    if let Some(line) = input
        .get("offset")
        .and_then(Value::as_u64)
        .and_then(|line| u32::try_from(line).ok())
    {
        location = location.line(line);
    }
    Some(vec![location])
}

/// A call the lead makes, as the client is told of it.
pub(super) fn tool_call(
    cwd: &Path,
    call_id: &str,
    name: &str,
    input: &Value,
    status: acp::ToolCallStatus,
) -> acp::ToolCall {
    let mut call = acp::ToolCall::new(call_id.to_string(), title(name, input))
        .kind(tool_kind(name, None))
        .status(status)
        .raw_input(input.clone());
    if let Some(locations) = locations(cwd, input) {
        call = call.locations(locations);
    }
    call
}

/// The text a file had before an edit, from the text it has now and the unified diff of the
/// edit. `None` when the diff does not fit the text.
fn before_edit(now: &str, diff: &str) -> Option<String> {
    let now: Vec<&str> = now.split_inclusive('\n').collect();
    let mut before = String::new();
    let mut at = 0;
    let mut in_hunk = false;
    let mut lines = diff.split('\n').peekable();
    while let Some(line) = lines.next() {
        if let Some(header) = line.strip_prefix("@@ ") {
            // `-a,b +c,d @@`: the hunk starts at line c of the new text (after it when d is 0).
            let new_range = header.split(' ').nth(1)?.strip_prefix('+')?;
            let (start, count) = match new_range.split_once(',') {
                Some((start, count)) => (start.parse::<usize>().ok()?, count.parse::<usize>().ok()?),
                None => (new_range.parse::<usize>().ok()?, 1),
            };
            let start = if count == 0 { start } else { start.checked_sub(1)? };
            before.extend(now.get(at..start)?.iter().copied());
            at = start;
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue; // the `---` and `+++` lines
        }
        match line.as_bytes().first() {
            Some(b' ') => {
                before.push_str(now.get(at)?);
                at += 1;
            }
            Some(b'+') => at += 1,
            Some(b'-') => {
                before.push_str(&line[1..]);
                // A removed last line that had no line end says so on the next line.
                if !lines.peek().is_some_and(|next| next.starts_with('\\')) {
                    before.push('\n');
                }
            }
            _ => {}
        }
    }
    before.extend(now.get(at..)?.iter().copied());
    Some(before)
}

/// What `write` and `edit` changed, for the client to show as a diff.
fn diff(cwd: &Path, details: &Value) -> Option<acp::Diff> {
    let unified = details
        .get("diff")
        .and_then(Value::as_str)
        .filter(|diff| !diff.is_empty())?;
    let path = cwd.join(details.get("path").and_then(Value::as_str)?);
    let now = std::fs::read_to_string(&path).ok()?;
    let before = before_edit(&now, unified)?;
    Some(acp::Diff::new(path, now).old_text((!before.is_empty()).then_some(before)))
}

fn content_block(block: &Block) -> Option<acp::ContentBlock> {
    match block {
        Block::Text { text } => Some(acp::ContentBlock::Text(acp::TextContent::new(text.clone()))),
        Block::Image { mime, data } => Some(acp::ContentBlock::Image(acp::ImageContent::new(
            data.clone(),
            mime.clone(),
        ))),
        _ => None,
    }
}

/// The end of a call: its status, what it returned, and the diff of an edit.
pub(super) fn tool_result(cwd: &Path, call_id: &str, output: &ToolOutput) -> acp::ToolCallUpdate {
    let mut content: Vec<acp::ToolCallContent> = output
        .content
        .iter()
        .filter_map(content_block)
        .map(acp::ToolCallContent::from)
        .collect();
    if let Some(diff) = output.details.as_ref().and_then(|details| diff(cwd, details)) {
        content.push(acp::ToolCallContent::from(diff));
    }
    let mut fields = acp::ToolCallUpdateFields::new()
        .status(if output.is_error {
            acp::ToolCallStatus::Failed
        } else {
            acp::ToolCallStatus::Completed
        })
        .content(content);
    if let Some(details) = &output.details {
        fields = fields.raw_output(details.clone());
    }
    acp::ToolCallUpdate::new(call_id.to_string(), fields)
}

/// An MCP server the client brings, as an entry of `mcp.json`.
pub(super) fn mcp_entry(server: acp::McpServer) -> Option<(String, Value)> {
    let pairs = |pairs: Vec<(String, String)>| -> Map<String, Value> {
        pairs
            .into_iter()
            .map(|(name, value)| (name, Value::String(value)))
            .collect()
    };
    match server {
        acp::McpServer::Stdio(server) => Some((
            server.name,
            json!({
                "command": server.command.to_string_lossy(),
                "args": server.args,
                "env": pairs(server.env.into_iter().map(|variable| (variable.name, variable.value)).collect()),
            }),
        )),
        acp::McpServer::Http(server) => Some((
            server.name,
            json!({
                "type": "http",
                "url": server.url,
                "headers": pairs(server.headers.into_iter().map(|header| (header.name, header.value)).collect()),
            }),
        )),
        acp::McpServer::Sse(server) => Some((
            server.name,
            json!({
                "type": "sse",
                "url": server.url,
                "headers": pairs(server.headers.into_iter().map(|header| (header.name, header.value)).collect()),
            }),
        )),
        _ => None,
    }
}

/// The path a `file://` address names; any other address as it is.
fn path_of(uri: &str) -> String {
    reqwest::Url::parse(uri)
        .ok()
        .filter(|url| url.scheme() == "file")
        .and_then(|url| url.to_file_path().ok())
        .map_or_else(|| uri.to_string(), |path: PathBuf| path.display().to_string())
}

/// The blocks of a prompt as one user message: its text, with the text resources the client
/// embedded attached the way `@path` mentions attach files, and its images.
pub(super) fn user_input(prompt: Vec<acp::ContentBlock>) -> (String, Vec<Block>) {
    let mut said: Vec<String> = Vec::new();
    let mut attached = String::new();
    let mut images = Vec::new();
    for block in prompt {
        match block {
            acp::ContentBlock::Text(text) => said.push(text.text),
            acp::ContentBlock::Image(image) => images.push(Block::Image {
                mime: image.mime_type,
                data: image.data,
            }),
            acp::ContentBlock::Resource(resource) => match resource.resource {
                acp::EmbeddedResourceResource::TextResourceContents(file) => {
                    attached.push_str(&format!(
                        "\n\n<file path=\"{}\">\n{}\n</file>",
                        path_of(&file.uri),
                        file.text.trim_end()
                    ));
                }
                acp::EmbeddedResourceResource::BlobResourceContents(file) => {
                    match file.mime_type.filter(|mime| mime.starts_with("image/")) {
                        Some(mime) => images.push(Block::Image {
                            mime,
                            data: file.blob,
                        }),
                        None => said.push(format!("(attached: {}, which is not text)", path_of(&file.uri))),
                    }
                }
                _ => {}
            },
            // A link is not the content: the agent reads the file itself when it needs it.
            acp::ContentBlock::ResourceLink(link) => said.push(path_of(&link.uri)),
            _ => {}
        }
    }
    (said.join("\n") + &attached, images)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_text_before_an_edit_comes_back_from_the_diff() {
        let now = "one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\nextra\n";
        let diff = "--- a/f.txt\n+++ b/f.txt\n@@ -1,5 +1,5 @@\n one\n-two\n+TWO\n three\n four\n five\n\
                    @@ -8,3 +8,4 @@\n eight\n nine\n ten\n+extra\n";
        assert_eq!(
            before_edit(now, diff).unwrap(),
            "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n"
        );
        // A new file, and a file whose last line lost its line end.
        assert_eq!(
            before_edit("a\nb\n", "--- a/n\n+++ b/n\n@@ -0,0 +1,2 @@\n+a\n+b\n").unwrap(),
            ""
        );
        assert_eq!(
            before_edit(
                "a\nc",
                "--- a/n\n+++ b/n\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+c\n\\ No newline at end of file\n"
            )
            .unwrap(),
            "a\nb"
        );
        // A removed line that looks like a header is a removed line.
        assert_eq!(
            before_edit("x\n", "--- a/n\n+++ b/n\n@@ -1,2 +1 @@\n x\n--- old\n").unwrap(),
            "x\n-- old\n"
        );
        // A diff of another text.
        assert_eq!(before_edit("x\n", "@@ -1,9 +5,9 @@\n y\n"), None);
    }

    #[test]
    fn an_edit_is_shown_as_a_diff_of_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "one\nTWO\n").unwrap();
        let output = ToolOutput::text("edited f.txt").with_details(json!({
            "path": "f.txt",
            "diff": "--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n"
        }));
        let update = serde_json::to_value(tool_result(dir.path(), "c1", &output)).unwrap();
        assert_eq!(update["toolCallId"], "c1");
        assert_eq!(update["status"], "completed");
        let content = update["content"].as_array().unwrap();
        assert_eq!(content[0]["content"]["text"], "edited f.txt");
        assert_eq!(
            (
                &content[1]["type"],
                &content[1]["oldText"],
                &content[1]["newText"]
            ),
            (&json!("diff"), &json!("one\ntwo\n"), &json!("one\nTWO\n"))
        );
        let failed = serde_json::to_value(tool_result(dir.path(), "c2", &ToolOutput::error("no"))).unwrap();
        assert_eq!(failed["status"], "failed");
    }

    #[test]
    fn a_prompt_becomes_text_with_attachments_and_images() {
        let (text, images) = user_input(vec![
            acp::ContentBlock::Text(acp::TextContent::new("review this")),
            acp::ContentBlock::Resource(acp::EmbeddedResource::new(
                acp::EmbeddedResourceResource::TextResourceContents(acp::TextResourceContents::new(
                    "fn main() {}\n",
                    "untitled:scratch",
                )),
            )),
            acp::ContentBlock::Image(acp::ImageContent::new("AAAA", "image/png")),
            acp::ContentBlock::ResourceLink(acp::ResourceLink::new("notes", "https://example.com/notes")),
        ]);
        assert_eq!(
            text,
            "review this\nhttps://example.com/notes\n\n<file path=\"untitled:scratch\">\nfn main() {}\n</file>"
        );
        assert_eq!(
            images,
            [Block::Image {
                mime: "image/png".into(),
                data: "AAAA".into()
            }]
        );
    }
}
