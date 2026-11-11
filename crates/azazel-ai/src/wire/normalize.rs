//! Makes a transcript acceptable to the provider and model it is about to be sent to.

use std::collections::HashSet;

use crate::catalog::ModelInfo;
use crate::types::{Block, Message, Role, WireApi};

pub const INTERRUPTED_TEXT: &str = "tool call interrupted";
pub const IMAGE_OMITTED_TEXT: &str = "[image omitted: model has no vision]";

pub struct Target<'a> {
    pub provider: &'a str,
    pub model: &'a ModelInfo,
    pub api: WireApi,
}

pub fn normalize(messages: &[Message], target: &Target<'_>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for message in messages {
        let same_model = message
            .meta
            .as_ref()
            .is_some_and(|m| m.provider == target.provider && m.model == target.model.id);
        let content: Vec<Block> = message
            .content
            .iter()
            .filter_map(|block| convert(block, same_model, target))
            .collect();
        if !content.is_empty() {
            out.push(Message {
                role: message.role,
                content,
                meta: message.meta.clone(),
            });
        }
    }
    let out = close_tool_calls(out);
    if target.api.requires_alternation() {
        merge_same_role(out)
    } else {
        out
    }
}

fn convert(block: &Block, same_model: bool, target: &Target<'_>) -> Option<Block> {
    match block {
        Block::Thinking { .. } => same_model.then(|| block.clone()),
        Block::Opaque { provider, .. } => (provider == target.provider).then(|| block.clone()),
        Block::Image { .. } if !target.model.vision => Some(Block::text(IMAGE_OMITTED_TEXT)),
        Block::ToolCall {
            id,
            name,
            input,
            signature,
        } => Some(Block::ToolCall {
            id: wire_id(id),
            name: name.clone(),
            input: input.clone(),
            signature: signature.clone(),
        }),
        Block::ToolResult {
            call_id,
            content,
            is_error,
        } => Some(Block::ToolResult {
            call_id: wire_id(call_id),
            content: content
                .iter()
                .filter_map(|b| convert(b, same_model, target))
                .collect(),
            is_error: *is_error,
        }),
        Block::Text { .. } | Block::Image { .. } => Some(block.clone()),
    }
}

/// Ids valid as `[A-Za-z0-9_-]{1,64}` pass through. Anything else is cleaned, cut to 55 characters and
/// suffixed with 8 hex digits of the original id's BLAKE3 hash, so distinct ids stay distinct.
pub fn wire_id(id: &str) -> String {
    let valid = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if !id.is_empty() && id.len() <= 64 && id.chars().all(valid) {
        return id.to_string();
    }
    let mut out: String = id
        .chars()
        .map(|c| if valid(c) { c } else { '_' })
        .take(55)
        .collect();
    out.push('_');
    out.push_str(&blake3::hash(id.as_bytes()).to_hex()[..8]);
    out
}

/// Every tool call must be answered by the `Tool` message that directly follows its assistant
/// message. Calls with no result there (aborted turns, crashes) get a synthetic error result.
fn close_tool_calls(messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    let mut iter = messages.into_iter().peekable();
    while let Some(message) = iter.next() {
        let calls: Vec<String> = if message.role == Role::Assistant {
            message.tool_calls().map(|(id, _, _)| id.to_string()).collect()
        } else {
            Vec::new()
        };
        out.push(message);
        if calls.is_empty() {
            continue;
        }
        let mut results = match iter.next_if(|next| next.role == Role::Tool) {
            Some(tool) => tool,
            None => Message::tool(Vec::new()),
        };
        let answered: HashSet<&str> = results
            .content
            .iter()
            .filter_map(|b| match b {
                Block::ToolResult { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        let missing: Vec<Block> = calls
            .iter()
            .filter(|id| !answered.contains(id.as_str()))
            .map(|id| Block::tool_result(id.clone(), INTERRUPTED_TEXT, true))
            .collect();
        results.content.extend(missing);
        out.push(results);
    }
    out
}

fn merge_same_role(messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for message in messages {
        match out.last_mut() {
            Some(last) if last.role == message.role => {
                last.content.extend(message.content);
                if message.meta.is_some() {
                    last.meta = message.meta;
                }
            }
            _ => out.push(message),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::types::{AssistantMeta, StopReason, Usage};

    fn model(vision: bool) -> ModelInfo {
        ModelInfo {
            vision,
            ..ModelInfo::unknown("mock", "a")
        }
    }

    fn meta(provider: &str, model: &str) -> AssistantMeta {
        AssistantMeta {
            provider: provider.into(),
            model: model.into(),
            api: WireApi::OpenAiChat,
            usage: Usage::default(),
            stop: StopReason::Stop,
            response_id: None,
            duration_ms: 0,
        }
    }

    fn call(id: &str) -> Block {
        Block::ToolCall {
            id: id.into(),
            name: "read".into(),
            input: json!({}),
            signature: None,
        }
    }

    fn thinking() -> Block {
        Block::Thinking {
            text: "hm".into(),
            signature: Some("sig".into()),
            redacted: false,
        }
    }

    fn run(messages: &[Message], api: WireApi, vision: bool) -> Vec<Message> {
        let model = model(vision);
        normalize(
            messages,
            &Target {
                provider: "mock",
                model: &model,
                api,
            },
        )
    }

    #[test]
    fn thinking_survives_only_for_the_same_provider_and_model() {
        let from =
            |provider, model| Message::assistant(vec![thinking(), Block::text("x")], meta(provider, model));
        let out = run(
            &[from("mock", "a"), from("mock", "b"), from("other", "a")],
            WireApi::OpenAiChat,
            true,
        );
        let kept: Vec<bool> = out
            .iter()
            .map(|m| m.content.iter().any(|b| matches!(b, Block::Thinking { .. })))
            .collect();
        assert_eq!(kept, [true, false, false]);
    }

    #[test]
    fn message_left_with_no_blocks_is_removed() {
        let out = run(
            &[
                Message::user("q"),
                Message::assistant(vec![thinking()], meta("other", "x")),
                Message::user("again"),
            ],
            WireApi::OpenAiChat,
            true,
        );
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn opaque_blocks_of_other_providers_are_dropped() {
        let msg = Message::assistant(
            vec![
                Block::Opaque {
                    provider: "mock".into(),
                    item: json!(1),
                },
                Block::Opaque {
                    provider: "other".into(),
                    item: json!(2),
                },
            ],
            meta("mock", "a"),
        );
        let out = run(&[msg], WireApi::OpenAiChat, true);
        assert_eq!(
            out[0].content,
            vec![Block::Opaque {
                provider: "mock".into(),
                item: json!(1)
            }]
        );
    }

    #[test]
    fn ids_are_rewritten_identically_on_call_and_result() {
        let long = "x".repeat(80);
        let messages = [
            Message::assistant(
                vec![call("a.b:c"), call(&long), call("fine_id-1")],
                meta("mock", "a"),
            ),
            Message::tool(vec![
                Block::tool_result("a.b:c", "1", false),
                Block::tool_result(long.clone(), "2", false),
                Block::tool_result("fine_id-1", "3", false),
            ]),
        ];
        let out = run(&messages, WireApi::OpenAiChat, true);
        let calls: Vec<String> = out[0].tool_calls().map(|(id, _, _)| id.to_string()).collect();
        let results: Vec<String> = out[1]
            .content
            .iter()
            .map(|b| match b {
                Block::ToolResult { call_id, .. } => call_id.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(calls, results);
        assert_eq!(calls[2], "fine_id-1");
        for id in &calls {
            assert!(
                id.len() <= 64
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{id}"
            );
        }
        assert_ne!(wire_id("a.b"), wire_id("a:b"));
        assert_eq!(wire_id(&long).len(), 64);
    }

    #[test]
    fn unanswered_calls_get_synthetic_error_results_right_after_the_assistant_message() {
        let messages = [
            Message::assistant(vec![call("c1"), call("c2")], meta("mock", "a")),
            Message::tool(vec![Block::tool_result("c1", "ok", false)]),
            Message::assistant(vec![call("c3")], meta("mock", "a")),
            Message::user("stop that"),
        ];
        let out = run(&messages, WireApi::OpenAiChat, true);
        assert_eq!(
            out.iter().map(|m| m.role).collect::<Vec<_>>(),
            [
                Role::Assistant,
                Role::Tool,
                Role::Assistant,
                Role::Tool,
                Role::User
            ]
        );
        assert_eq!(
            out[1].content,
            vec![
                Block::tool_result("c1", "ok", false),
                Block::tool_result("c2", INTERRUPTED_TEXT, true)
            ]
        );
        assert_eq!(
            out[3].content,
            vec![Block::tool_result("c3", INTERRUPTED_TEXT, true)]
        );
    }

    #[test]
    fn same_role_neighbours_merge_only_for_alternating_apis() {
        let messages = [
            Message::user("a"),
            Message::user("b"),
            Message::assistant(vec![Block::text("c")], meta("mock", "a")),
        ];
        assert_eq!(run(&messages, WireApi::OpenAiChat, true).len(), 3);
        let merged = run(&messages, WireApi::AnthropicMessages, true);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].content, vec![Block::text("a"), Block::text("b")]);
    }

    #[test]
    fn images_become_text_for_models_without_vision_including_inside_tool_results() {
        let image = Block::Image {
            mime: "image/png".into(),
            data: "AAAA".into(),
        };
        let messages = [
            Message::user_blocks(vec![Block::text("look"), image.clone()]),
            Message::assistant(vec![call("c1")], meta("mock", "a")),
            Message::tool(vec![Block::ToolResult {
                call_id: "c1".into(),
                content: vec![image.clone()],
                is_error: false,
            }]),
        ];
        let blind = run(&messages, WireApi::OpenAiChat, false);
        assert_eq!(blind[0].content[1], Block::text(IMAGE_OMITTED_TEXT));
        assert_eq!(
            blind[2].content[0],
            Block::tool_result("c1", IMAGE_OMITTED_TEXT, false)
        );
        let sighted = run(&messages, WireApi::OpenAiChat, true);
        assert_eq!(sighted[0].content[1], image);
    }
}
