//! Turning what servers return into blocks the model can take.

use azazel_agent::{ToolCtx, ToolOutput};
use azazel_ai::Block;
use azazel_core::ArtifactStore;
use base64::Engine;
use rmcp::model::{CallToolResult, ContentBlock, ResourceContents};

/// A file extension for stored binary content, from its MIME type.
fn extension(mime: &str) -> String {
    let subtype = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let subtype = subtype.trim().strip_prefix("x-").unwrap_or(subtype.trim());
    let known = match subtype {
        "mpeg" => "mp3",
        "plain" => "txt",
        "octet-stream" | "" => "bin",
        other => other,
    };
    let clean: String = known
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if clean.is_empty() {
        "bin".to_string()
    } else {
        clean.to_ascii_lowercase()
    }
}

/// Stores base64 content the model cannot take inline under the artifacts directory and returns
/// the line that tells the model where it is.
fn store_binary(ctx: &ToolCtx, index: usize, mime: &str, data: &str) -> String {
    let Some(store) = ctx.services.get::<ArtifactStore>() else {
        return format!("[{mime} content omitted: no artifact store]");
    };
    let bytes = match base64::engine::general_purpose::STANDARD.decode(data.trim()) {
        Ok(bytes) => bytes,
        Err(e) => return format!("[{mime} content omitted: not valid base64 ({e})]"),
    };
    let name = format!("mcp-{}-{index}.{}", ctx.call_id, extension(mime));
    match store.save(&name, &bytes) {
        Ok(path) => format!(
            "saved {mime} content ({} bytes) to {}",
            bytes.len(),
            path.display()
        ),
        Err(e) => format!("[{mime} content omitted: could not store it ({e})]"),
    }
}

fn push_resource(out: &mut Vec<Block>, resource: &ResourceContents, ctx: &ToolCtx, index: usize) {
    match resource {
        ResourceContents::TextResourceContents { text, .. } => out.push(Block::text(text.clone())),
        ResourceContents::BlobResourceContents { blob, mime_type, .. } => {
            let mime = mime_type.as_deref().unwrap_or("application/octet-stream");
            if mime.starts_with("image/") {
                out.push(Block::Image {
                    mime: mime.to_string(),
                    data: blob.clone(),
                });
            } else {
                out.push(Block::text(store_binary(ctx, index, mime, blob)));
            }
        }
        // A kind of content added to the protocol after this was written.
        _ => out.push(Block::text("[content of an unknown kind omitted]")),
    }
}

/// The contents of a resource as blocks: text stays text, images stay images, other binary data is
/// stored as an artifact.
pub fn resource_blocks(contents: &[ResourceContents], ctx: &ToolCtx) -> Vec<Block> {
    let mut out = Vec::new();
    for (index, resource) in contents.iter().enumerate() {
        push_resource(&mut out, resource, ctx, index);
    }
    out
}

/// The text of content blocks, for prompts: text and embedded text resources, in order.
pub fn content_text(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(text) => Some(text.text.clone()),
        ContentBlock::Resource(embedded) => match &embedded.resource {
            ResourceContents::TextResourceContents { text, .. } => Some(text.clone()),
            _ => None,
        },
        ContentBlock::ResourceLink(link) => Some(format!("resource: {}", link.uri)),
        _ => None,
    }
}

/// A tool result as the agent sees it. `structuredContent` is appended as JSON when the result has
/// no text of its own.
pub fn tool_output(result: &CallToolResult, ctx: &ToolCtx) -> ToolOutput {
    let mut content = Vec::new();
    for (index, block) in result.content.iter().enumerate() {
        match block {
            ContentBlock::Text(text) => content.push(Block::text(text.text.clone())),
            ContentBlock::Image(image) => content.push(Block::Image {
                mime: image.mime_type.clone(),
                data: image.data.clone(),
            }),
            ContentBlock::Audio(audio) => content.push(Block::text(store_binary(
                ctx,
                index,
                &audio.mime_type,
                &audio.data,
            ))),
            ContentBlock::Resource(embedded) => push_resource(&mut content, &embedded.resource, ctx, index),
            ContentBlock::ResourceLink(link) => {
                content.push(Block::text(format!("resource: {} ({})", link.uri, link.name)))
            }
            _ => content.push(Block::text("[content of an unknown kind omitted]")),
        }
    }
    let has_text = content.iter().any(|block| matches!(block, Block::Text { .. }));
    if !has_text && let Some(structured) = &result.structured_content {
        content.push(Block::text(structured.to_string()));
    }
    if content.is_empty() {
        content.push(Block::text("(the tool returned nothing)"));
    }
    ToolOutput {
        content,
        is_error: result.is_error.unwrap_or(false),
        details: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use azazel_agent::{AgentInfo, Services, UpdateSink};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn context(dir: &std::path::Path) -> ToolCtx {
        let services = Arc::new(Services::new());
        services.insert(Arc::new(ArtifactStore::new(dir.join("artifacts"))));
        let agent = AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id: "s".into(),
        };
        ToolCtx {
            agent_id: agent.id.clone(),
            session_id: agent.session_id.clone(),
            agent,
            call_id: "call1".into(),
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            update: UpdateSink::default(),
            services,
        }
    }

    fn result(value: serde_json::Value) -> CallToolResult {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn text_images_and_errors_map_directly() {
        let dir = tempfile::tempdir().unwrap();
        let output = tool_output(
            &result(json!({
                "content": [
                    { "type": "text", "text": "one" },
                    { "type": "image", "data": "aGk=", "mimeType": "image/png" },
                    { "type": "resource", "resource": { "uri": "mem://a", "text": "embedded" } },
                    { "type": "resource_link", "uri": "file:///x", "name": "x" }
                ],
                "structuredContent": { "ignored": true },
                "isError": true
            })),
            &context(dir.path()),
        );
        assert!(output.is_error);
        assert_eq!(
            output.content,
            vec![
                Block::text("one"),
                Block::Image {
                    mime: "image/png".into(),
                    data: "aGk=".into()
                },
                Block::text("embedded"),
                Block::text("resource: file:///x (x)"),
            ]
        );
    }

    #[test]
    fn binary_content_is_stored_and_structured_content_fills_in_for_missing_text() {
        let dir = tempfile::tempdir().unwrap();
        let output = tool_output(
            &result(json!({
                "content": [
                    { "type": "audio", "data": "aGVsbG8=", "mimeType": "audio/mpeg" },
                    { "type": "resource", "resource": { "uri": "mem://b", "blob": "AAEC", "mimeType": "application/zip" } }
                ]
            })),
            &context(dir.path()),
        );
        assert!(!output.is_error);
        let lines = output.text_content();
        assert!(
            lines.contains("saved audio/mpeg content (5 bytes) to "),
            "{lines}"
        );
        assert_eq!(
            std::fs::read(dir.path().join("artifacts/mcp-call1-0.mp3")).unwrap(),
            b"hello"
        );
        assert_eq!(
            std::fs::read(dir.path().join("artifacts/mcp-call1-1.zip")).unwrap(),
            [0, 1, 2]
        );

        let structured = tool_output(
            &result(json!({ "content": [], "structuredContent": { "n": 3 } })),
            &context(dir.path()),
        );
        assert_eq!(structured.text_content(), "{\"n\":3}");
        let empty = tool_output(&result(json!({ "content": [] })), &context(dir.path()));
        assert_eq!(empty.text_content(), "(the tool returned nothing)");
        let broken = tool_output(
            &result(json!({ "content": [{ "type": "audio", "data": "%%%", "mimeType": "audio/wav" }] })),
            &context(dir.path()),
        );
        assert!(
            broken
                .text_content()
                .starts_with("[audio/wav content omitted: not valid base64")
        );
    }

    #[test]
    fn extensions_come_from_the_mime_subtype() {
        assert_eq!(extension("audio/mpeg"), "mp3");
        assert_eq!(extension("audio/x-wav; rate=44100"), "wav");
        assert_eq!(extension("application/octet-stream"), "bin");
        assert_eq!(extension("weird"), "weird");
        assert_eq!(extension(""), "bin");
    }
}
