#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use azazel_ai::wire::{anthropic, decode_sse};
use azazel_ai::{
    AiError, AssistantMeta, Auth, Block, INVALID_JSON_KEY, Message, ModelInfo, ProviderConfig, Request, Role,
    StopReason, StreamEvent, SystemPart, ThinkingLevel, Tier, ToolSpec, Usage, Window, WireApi,
};
use serde_json::{Value, json};

/// Takes the manual `budget_tokens` thinking form (not a Claude id).
const MANUAL_MODEL: &str = "mock-model";
/// Takes adaptive thinking with every effort level.
const ADAPTIVE_MODEL: &str = "claude-mock-5-1";

fn provider() -> ProviderConfig {
    ProviderConfig {
        requires_key: true,
        ..ProviderConfig::new("mock", WireApi::AnthropicMessages, "https://mock.invalid")
    }
}

fn model(id: &str) -> ModelInfo {
    ModelInfo {
        max_output: 64_000,
        vision: true,
        reasoning: true,
        ..ModelInfo::unknown("mock", id)
    }
}

fn request(messages: Vec<Message>) -> Request {
    Request {
        messages,
        ..Request::default()
    }
}

fn build(req: &Request) -> Value {
    anthropic::build_body(&provider(), &model(MANUAL_MODEL), req)
}

fn build_with_level(model_id: &str, level: ThinkingLevel, max_output: Option<u32>) -> Value {
    let mut req = request(vec![Message::user("hi")]);
    req.thinking = level;
    req.max_output = max_output;
    anthropic::build_body(&provider(), &model(model_id), &req)
}

fn assistant(content: Vec<Block>) -> Message {
    let meta = AssistantMeta {
        provider: "mock".to_string(),
        model: MANUAL_MODEL.to_string(),
        api: WireApi::AnthropicMessages,
        usage: Usage::default(),
        stop: StopReason::ToolUse,
        response_id: None,
        duration_ms: 0,
    };
    Message::assistant(content, meta)
}

fn call(id: &str, name: &str, input: Value) -> Block {
    Block::ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        input,
        signature: None,
    }
}

fn thinking(text: &str, signature: Option<&str>, redacted: bool) -> Block {
    Block::Thinking {
        text: text.to_string(),
        signature: signature.map(str::to_string),
        redacted,
    }
}

fn cache_marker() -> Value {
    json!({"type": "ephemeral"})
}

fn count_cache_markers(value: &Value) -> usize {
    match value {
        Value::Object(map) => {
            usize::from(map.contains_key("cache_control"))
                + map.values().map(count_cache_markers).sum::<usize>()
        }
        Value::Array(items) => items.iter().map(count_cache_markers).sum(),
        _ => 0,
    }
}

fn decode(bytes: &[u8]) -> Vec<StreamEvent> {
    let mut decoder = anthropic::decoder(&provider(), &model(MANUAL_MODEL));
    decode_sse(bytes, decoder.as_mut())
}

/// Serialises stream payloads the way the API frames them: the event name repeats `type`.
fn sse(records: &[Value]) -> Vec<u8> {
    let mut out = String::new();
    for record in records {
        let event = record["type"].as_str().unwrap();
        out.push_str(&format!("event: {event}\ndata: {record}\n\n"));
    }
    out.into_bytes()
}

fn message_start() -> Value {
    json!({
        "type": "message_start",
        "message": {
            "id": "msg_inline", "type": "message", "role": "assistant", "model": "mock-model",
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 5, "output_tokens": 1}
        }
    })
}

fn message_delta(stop_reason: &str) -> Value {
    json!({
        "type": "message_delta",
        "delta": {"stop_reason": stop_reason, "stop_sequence": null},
        "usage": {"output_tokens": 9}
    })
}

fn message_stop() -> Value {
    json!({"type": "message_stop"})
}

fn block_start(index: u64, block: Value) -> Value {
    json!({"type": "content_block_start", "index": index, "content_block": block})
}

fn block_delta(index: u64, delta: Value) -> Value {
    json!({"type": "content_block_delta", "index": index, "delta": delta})
}

fn block_stop(index: u64) -> Value {
    json!({"type": "content_block_stop", "index": index})
}

fn text_block(index: u64, text: &str) -> [Value; 3] {
    [
        block_start(index, json!({"type": "text", "text": ""})),
        block_delta(index, json!({"type": "text_delta", "text": text})),
        block_stop(index),
    ]
}

fn done(events: &[StreamEvent]) -> &Message {
    match events.last() {
        Some(StreamEvent::Done { message }) => message,
        other => panic!("expected a final Done event, got {other:?}"),
    }
}

fn failure(events: &[StreamEvent]) -> (&AiError, Option<&Message>) {
    match events.last() {
        Some(StreamEvent::Error { error, partial }) => (error, partial.as_ref()),
        other => panic!("expected a final Error event, got {other:?}"),
    }
}

fn block_end_indexes(events: &[StreamEvent]) -> Vec<usize> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::BlockEnd { index, .. } => Some(*index),
            _ => None,
        })
        .collect()
}

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64, reasoning: u64) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        reasoning,
        cost_usd: None,
    }
}

// ---- decoder: fixtures ----

#[test]
fn text_fixture_decodes_to_one_text_block() {
    let events = decode(include_bytes!("fixtures/anthropic-messages/text.sse"));
    insta::assert_snapshot!("anthropic_text", serde_json::to_string_pretty(&events).unwrap());

    assert_eq!(events.first(), Some(&StreamEvent::Start));
    // `ping` yields nothing: Start, three deltas, BlockEnd, Done.
    assert_eq!(events.len(), 6);
    let streamed: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta { index: 0, delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    let message = done(&events);
    assert_eq!(message.role, Role::Assistant);
    assert_eq!(
        message.content,
        [Block::text("The build passes: 42 tests ran and none failed.")]
    );
    assert_eq!(streamed, message.text());

    let meta = message.meta.as_ref().unwrap();
    assert_eq!(meta.stop, StopReason::Stop);
    assert_eq!(meta.usage, usage(12, 17, 2048, 300, 0));
    assert_eq!(meta.response_id.as_deref(), Some("msg_01Xk3Tq8PvRn2mLw7HsYd5Bc"));
    assert_eq!(
        (
            meta.provider.as_str(),
            meta.model.as_str(),
            meta.api,
            meta.duration_ms
        ),
        ("mock", "mock-model", WireApi::AnthropicMessages, 0)
    );
}

#[test]
fn tool_call_fixture_joins_argument_fragments() {
    let events = decode(include_bytes!("fixtures/anthropic-messages/tool_call.sse"));
    insta::assert_snapshot!(
        "anthropic_tool_call",
        serde_json::to_string_pretty(&events).unwrap()
    );

    assert_eq!(events.first(), Some(&StreamEvent::Start));
    let started = events.iter().find_map(|event| match event {
        StreamEvent::ToolCallStart { index, id, name } => Some((*index, id.as_str(), name.as_str())),
        _ => None,
    });
    assert_eq!(started, Some((1, "toolu_01T1x1fJ34qAmk2tNTrN7Up6", "read")));
    let fragments: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCallDelta { index: 1, delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    // The API opens every tool call with an empty fragment, which carries nothing to show.
    assert_eq!(
        fragments,
        ["{\"path\":", " \"src/ma", "in.rs\"", ", \"limit\": 40}"]
    );

    let message = done(&events);
    assert_eq!(message.text(), "I'll read the entry point first.");
    let expected_input = json!({"path": "src/main.rs", "limit": 40});
    assert_eq!(
        message.tool_calls().collect::<Vec<_>>(),
        [("toolu_01T1x1fJ34qAmk2tNTrN7Up6", "read", &expected_input)]
    );
    assert!(events.contains(&StreamEvent::BlockEnd {
        index: 1,
        block: message.content[1].clone(),
    }));
    let meta = message.meta.as_ref().unwrap();
    assert_eq!(meta.stop, StopReason::ToolUse);
    assert_eq!(meta.usage, usage(472, 58, 0, 0, 0));
}

#[test]
fn thinking_tool_fixture_keeps_signature_and_block_order() {
    let events = decode(include_bytes!("fixtures/anthropic-messages/thinking_tool.sse"));
    insta::assert_snapshot!(
        "anthropic_thinking_tool",
        serde_json::to_string_pretty(&events).unwrap()
    );

    assert_eq!(events.first(), Some(&StreamEvent::Start));
    let thought =
        "The user wants every unfinished marker.\nA search over src/ is the cheapest way to find them.";
    let streamed: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ThinkingDelta { index: 0, delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(streamed, thought);
    assert_eq!(block_end_indexes(&events), [0, 1, 2]);

    let message = done(&events);
    let signature = "EqQBCgIYAhIM1gbcDa9GJwZA2b3hGgxBdjrkzLoky3dl1pkiMOYds0Mock==";
    assert_eq!(
        message.content,
        [
            thinking(thought, Some(signature), false),
            Block::text("Searching the sources for markers."),
            call(
                "toolu_01NcR4yWm8Zp2KsVb6HtJq3L",
                "grep",
                json!({"pattern": "FIXME", "path": "src"})
            ),
        ]
    );
    let meta = message.meta.as_ref().unwrap();
    assert_eq!(meta.stop, StopReason::ToolUse);
    assert_eq!(meta.usage, usage(41, 143, 1536, 0, 61));
    assert_eq!(meta.response_id.as_deref(), Some("msg_01Qd7Vn5KsGx3RtB9YwAe2Hu"));
}

#[test]
fn midstream_error_fixture_ends_with_overloaded_and_completed_blocks() {
    let events = decode(include_bytes!("fixtures/anthropic-messages/midstream_error.sse"));
    insta::assert_snapshot!(
        "anthropic_midstream_error",
        serde_json::to_string_pretty(&events).unwrap()
    );

    assert_eq!(events.first(), Some(&StreamEvent::Start));
    assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
    let (error, partial) = failure(&events);
    assert_eq!(*error, AiError::Overloaded);

    // The tool call was still streaming its arguments, so only the finished text block survives.
    let started = events
        .iter()
        .any(|event| matches!(event, StreamEvent::ToolCallStart { index: 1, name, .. } if name == "shell"));
    assert!(started);
    assert_eq!(block_end_indexes(&events), [0]);
    let partial = partial.unwrap();
    assert_eq!(partial.role, Role::Assistant);
    assert_eq!(partial.content, [Block::text("Let me run the tests.")]);
    let meta = partial.meta.as_ref().unwrap();
    assert_eq!(meta.stop, StopReason::Error);
    assert_eq!(meta.usage.input, 88);
}

// ---- decoder: protocol details ----

#[test]
fn server_blocks_stay_opaque_and_api_indexes_become_dense() {
    let search_result = json!({
        "type": "web_search_tool_result",
        "tool_use_id": "srvtoolu_1",
        "content": [{"type": "web_search_result", "title": "Mock", "url": "https://mock.invalid/a"}]
    });
    let mut records = vec![
        message_start(),
        block_start(0, json!({"type": "redacted_thinking", "data": "ENCRYPTED"})),
        block_stop(0),
        block_start(
            1,
            json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}}),
        ),
        block_delta(
            1,
            json!({"type": "input_json_delta", "partial_json": "{\"query\":"}),
        ),
        block_delta(
            1,
            json!({"type": "input_json_delta", "partial_json": " \"rust\"}"}),
        ),
        block_stop(1),
        block_start(2, search_result.clone()),
        block_stop(2),
    ];
    // The API index jumps from 2 to 7; ours continues at 3.
    records.extend(text_block(7, "Found it."));
    records.extend([message_delta("end_turn"), message_stop()]);
    let events = decode(&sse(&records));

    assert_eq!(block_end_indexes(&events), [0, 1, 2, 3]);
    assert!(events.contains(&StreamEvent::TextDelta {
        index: 3,
        delta: "Found it.".to_string(),
    }));
    let message = done(&events);
    assert_eq!(
        message.content,
        [
            thinking("", Some("ENCRYPTED"), true),
            Block::Opaque {
                provider: "mock".to_string(),
                item: json!({
                    "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                    "input": {"query": "rust"}
                }),
            },
            Block::Opaque {
                provider: "mock".to_string(),
                item: search_result,
            },
            Block::text("Found it."),
        ]
    );
    // A server-side tool call is not ours to run.
    assert_eq!(message.meta.as_ref().unwrap().stop, StopReason::Stop);
}

#[test]
fn empty_tool_arguments_become_an_object_and_invalid_ones_are_kept_raw() {
    let tool = |id: &str| json!({"type": "tool_use", "id": id, "name": "todo", "input": {}});
    let records = [
        message_start(),
        block_start(0, tool("call_empty")),
        block_delta(0, json!({"type": "input_json_delta", "partial_json": ""})),
        block_stop(0),
        block_start(1, tool("call_cut")),
        block_delta(
            1,
            json!({"type": "input_json_delta", "partial_json": "{\"items\": [1, "}),
        ),
        block_stop(1),
        message_delta("max_tokens"),
        message_stop(),
    ];
    let events = decode(&sse(&records));

    let message = done(&events);
    let empty = json!({});
    let cut = json!({ INVALID_JSON_KEY: "{\"items\": [1, " });
    assert_eq!(
        message.tool_calls().collect::<Vec<_>>(),
        [("call_empty", "todo", &empty), ("call_cut", "todo", &cut)]
    );
    // A message that contains tool calls reports ToolUse even when the output was cut.
    assert_eq!(message.meta.as_ref().unwrap().stop, StopReason::ToolUse);
}

#[test]
fn stop_reason_maps_to_length_only_when_output_was_cut() {
    let cases = [
        ("max_tokens", StopReason::Length),
        ("model_context_window_exceeded", StopReason::Length),
        ("end_turn", StopReason::Stop),
        ("stop_sequence", StopReason::Stop),
        ("pause_turn", StopReason::Stop),
        ("refusal", StopReason::Stop),
    ];
    for (stop_reason, expected) in cases {
        let mut records = vec![message_start()];
        records.extend(text_block(0, "partial answer"));
        records.extend([message_delta(stop_reason), message_stop()]);
        let events = decode(&sse(&records));
        let meta = done(&events).meta.clone().unwrap();
        assert_eq!(meta.stop, expected, "{stop_reason}");
        // `message_delta` reports the cumulative output count; input stays from `message_start`.
        assert_eq!(meta.usage, usage(5, 9, 0, 0, 0), "{stop_reason}");
    }
}

#[test]
fn malformed_data_and_unknown_events_are_skipped() {
    let mut stream = sse(&[
        message_start(),
        block_start(0, json!({"type": "text", "text": ""})),
    ]);
    stream.extend_from_slice(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\n\n");
    stream.extend_from_slice(b"event: future_event\ndata: {\"type\":\"future_event\",\"x\":1}\n\n");
    stream.extend(sse(&[
        block_delta(0, json!({"type": "text_delta", "text": "still here"})),
        block_delta(
            0,
            json!({"type": "citations_delta", "citation": {"type": "char_location"}}),
        ),
        block_delta(9, json!({"type": "text_delta", "text": "no such block"})),
        block_stop(0),
        message_delta("end_turn"),
        message_stop(),
    ]));
    let events = decode(&stream);

    assert_eq!(events.len(), 4, "{events:?}");
    assert_eq!(done(&events).content, [Block::text("still here")]);
}

#[test]
fn stream_that_ends_early_is_a_network_error() {
    let cut = "stream ended before the response was complete";

    // Cut inside the first block: nothing completed, so there is no partial message.
    let events = decode(&sse(&[
        message_start(),
        block_start(0, json!({"type": "text", "text": ""})),
        block_delta(0, json!({"type": "text_delta", "text": "par"})),
    ]));
    assert_eq!(events.first(), Some(&StreamEvent::Start));
    assert_eq!(failure(&events), (&AiError::Network(cut.to_string()), None));

    // Cut after a finished block but before the stop reason.
    let mut records = vec![message_start()];
    records.extend(text_block(0, "whole block"));
    let events = decode(&sse(&records));
    let (error, partial) = failure(&events);
    assert_eq!(*error, AiError::Network(cut.to_string()));
    assert_eq!(partial.unwrap().content, [Block::text("whole block")]);

    // An empty body still opens with Start.
    let events = decode(b"");
    assert_eq!(events.len(), 2);
    assert_eq!(events.first(), Some(&StreamEvent::Start));
    assert_eq!(failure(&events), (&AiError::Network(cut.to_string()), None));
}

#[test]
fn stream_missing_only_message_stop_is_complete() {
    let mut records = vec![message_start()];
    records.extend(text_block(0, "all of it"));
    records.push(message_delta("end_turn"));
    let events = decode(&sse(&records));

    let message = done(&events);
    assert_eq!(message.text(), "all of it");
    assert_eq!(message.meta.as_ref().unwrap().stop, StopReason::Stop);
}

#[test]
fn in_band_errors_are_classified_by_error_type() {
    let too_long = "prompt is too long: 215000 tokens > 200000 maximum";
    let cases = [
        ("overloaded_error", "Overloaded", AiError::Overloaded),
        (
            "rate_limit_error",
            "Rate limited",
            AiError::RateLimit { retry_after: None },
        ),
        (
            "authentication_error",
            "invalid x-api-key",
            AiError::Auth("invalid x-api-key".to_string()),
        ),
        (
            "permission_error",
            "not allowed",
            AiError::Auth("not allowed".to_string()),
        ),
        ("invalid_request_error", too_long, AiError::ContextOverflow),
        (
            "api_error",
            "Internal server error",
            AiError::Server {
                status: 500,
                body: "Internal server error".to_string(),
            },
        ),
    ];
    for (kind, message, expected) in cases {
        let record = json!({"type": "error", "error": {"type": kind, "message": message}});
        // The error arrives before `message_start`: Start still comes first.
        let events = decode(&sse(&[record]));
        assert_eq!(
            events,
            [
                StreamEvent::Start,
                StreamEvent::Error {
                    error: expected,
                    partial: None,
                },
            ],
            "{kind}"
        );
    }
}

#[test]
fn nothing_is_returned_after_a_terminal_event() {
    let mut decoder = anthropic::decoder(&provider(), &model(MANUAL_MODEL));
    let events = decoder.feed(Some("message_start"), &message_start().to_string());
    assert_eq!(events, [StreamEvent::Start]);
    let events = decoder.feed(Some("message_stop"), &message_stop().to_string());
    assert!(
        matches!(events.as_slice(), [StreamEvent::Done { .. }]),
        "{events:?}"
    );

    let late = block_start(0, json!({"type": "text", "text": "late"})).to_string();
    assert!(decoder.feed(Some("content_block_start"), &late).is_empty());
    assert!(decoder.finish().is_empty());
}

// ---- build_body ----

#[test]
fn minimal_request_has_required_fields_only() {
    let body = build(&request(vec![Message::user("hi")]));

    assert_eq!(body["model"], "mock-model");
    assert_eq!(body["stream"], true);
    // No explicit limit: the API still requires one, so the model's maximum is sent.
    assert_eq!(body["max_tokens"], 64_000);
    assert_eq!(
        body["messages"],
        json!([{
            "role": "user",
            "content": [{"type": "text", "text": "hi", "cache_control": cache_marker()}]
        }])
    );
    for key in ["system", "tools", "thinking", "output_config", "temperature"] {
        assert!(body.get(key).is_none(), "unexpected `{key}`");
    }
}

#[test]
fn explicit_output_limit_is_sent_and_temperature_depends_on_api_version() {
    let mut req = request(vec![Message::user("hi")]);
    req.max_output = Some(1234);
    req.temperature = Some(0.7);

    // The default API version rejects sampling parameters.
    let body = build(&req);
    assert_eq!(body["max_tokens"], 1234);
    assert!(body.get("temperature").is_none());

    // A provider pinned to the previous version still accepts it, as the exact decimal.
    let mut pinned = provider();
    pinned
        .headers
        .insert("Anthropic-Version".to_string(), "2023-06-01".to_string());
    let body = anthropic::build_body(&pinned, &model(MANUAL_MODEL), &req);
    assert_eq!(body["temperature"], json!(0.7));

    // ...but never together with thinking.
    req.thinking = ThinkingLevel::Low;
    let body = anthropic::build_body(&pinned, &model(MANUAL_MODEL), &req);
    assert!(body.get("temperature").is_none());
    assert_eq!(body["thinking"]["type"], "enabled");
}

#[test]
fn system_parts_become_text_blocks_with_a_marker_on_the_last_boundary() {
    let part = |text: &str, cache_boundary: bool| SystemPart {
        text: text.to_string(),
        cache_boundary,
    };
    let mut req = request(vec![Message::user("hi")]);
    req.system = vec![
        part("identity", false),
        part("   ", false),
        part("rules", true),
        part("skills", true),
        part("environment", false),
    ];
    assert_eq!(
        build(&req)["system"],
        json!([
            {"type": "text", "text": "identity"},
            {"type": "text", "text": "rules"},
            {"type": "text", "text": "skills", "cache_control": cache_marker()},
            {"type": "text", "text": "environment"}
        ])
    );

    // An empty boundary part moves the marker to the block before it.
    req.system = vec![
        part("identity", false),
        part("", true),
        part("environment", false),
    ];
    assert_eq!(
        build(&req)["system"],
        json!([
            {"type": "text", "text": "identity", "cache_control": cache_marker()},
            {"type": "text", "text": "environment"}
        ])
    );

    // Without a boundary nothing in the system prompt is marked.
    req.system = vec![part("identity", false)];
    assert_eq!(
        build(&req)["system"],
        json!([{"type": "text", "text": "identity"}])
    );
}

#[test]
fn tools_map_to_input_schema_and_only_the_last_is_cached() {
    let schema = json!({
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"]
    });
    let mut req = request(vec![Message::user("hi")]);
    req.tools = vec![
        ToolSpec {
            name: "edit".to_string(),
            description: "Edit a file".to_string(),
            schema: schema.clone(),
        },
        ToolSpec {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            schema: Value::Null,
        },
    ];
    assert_eq!(
        build(&req)["tools"],
        json!([
            {"name": "edit", "description": "Edit a file", "input_schema": schema},
            {
                "name": "read",
                "description": "Read a file",
                "input_schema": {"type": "object", "properties": {}},
                "cache_control": cache_marker()
            }
        ])
    );
}

#[test]
fn tool_calls_and_results_replay_and_tool_turn_merges_with_user_turn() {
    let png = Block::Image {
        mime: "image/png".to_string(),
        data: "QUJD".to_string(),
    };
    let req = request(vec![
        Message::user("list files"),
        assistant(vec![
            Block::text("Looking."),
            call("call_1", "glob", json!({"pattern": "*.rs"})),
            call("call_2", "read", Value::Null),
        ]),
        Message::tool(vec![
            Block::tool_result("call_1", "a.rs", false),
            Block::ToolResult {
                call_id: "call_2".to_string(),
                content: vec![Block::text("boom"), png],
                is_error: true,
            },
        ]),
        // A steer sent during the tool batch: on the wire it shares the user turn with the results.
        Message::user("also check b.rs"),
    ]);
    assert_eq!(
        build(&req)["messages"],
        json!([
            {
                "role": "user",
                "content": [{"type": "text", "text": "list files", "cache_control": cache_marker()}]
            },
            {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Looking."},
                    {"type": "tool_use", "id": "call_1", "name": "glob", "input": {"pattern": "*.rs"}},
                    {"type": "tool_use", "id": "call_2", "name": "read", "input": {}}
                ]
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "call_1",
                        "content": [{"type": "text", "text": "a.rs"}]
                    },
                    {
                        "type": "tool_result",
                        "tool_use_id": "call_2",
                        "content": [
                            {"type": "text", "text": "boom"},
                            {
                                "type": "image",
                                "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}
                            }
                        ],
                        "is_error": true
                    },
                    {"type": "text", "text": "also check b.rs", "cache_control": cache_marker()}
                ]
            }
        ])
    );
}

#[test]
fn images_map_to_base64_sources_and_empty_content_is_dropped() {
    let jpeg = Block::Image {
        mime: "image/jpeg".to_string(),
        data: "/9j/4AAQ".to_string(),
    };
    let req = request(vec![
        Message::user_blocks(vec![Block::text(""), jpeg, Block::text("what is this?")]),
        // Left without content, this turn disappears and its neighbours become one user turn.
        assistant(vec![Block::text("  \n")]),
        Message::user("answer briefly"),
    ]);
    assert_eq!(
        build(&req)["messages"],
        json!([{
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": {"type": "base64", "media_type": "image/jpeg", "data": "/9j/4AAQ"}
                },
                {"type": "text", "text": "what is this?"},
                {"type": "text", "text": "answer briefly", "cache_control": cache_marker()}
            ]
        }])
    );
}

#[test]
fn cache_markers_cover_the_last_two_user_turns_and_stay_within_four() {
    let mut req = request(vec![
        Message::user("first"),
        assistant(vec![Block::text("one")]),
        Message::user("second"),
        assistant(vec![call("call_1", "read", json!({"path": "a"}))]),
        Message::tool(vec![Block::tool_result("call_1", "contents", false)]),
    ]);
    req.system = vec![SystemPart {
        text: "rules".to_string(),
        cache_boundary: true,
    }];
    req.tools = vec![ToolSpec {
        name: "read".to_string(),
        description: "Read a file".to_string(),
        schema: json!({"type": "object"}),
    }];
    let body = build(&req);

    assert_eq!(count_cache_markers(&body), 4);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(count_cache_markers(&messages[0]), 0);
    assert_eq!(messages[2]["content"][0]["cache_control"], cache_marker());
    // The marker sits on the tool result itself, not inside its content.
    assert_eq!(messages[4]["content"][0]["type"], "tool_result");
    assert_eq!(messages[4]["content"][0]["cache_control"], cache_marker());
    assert_eq!(count_cache_markers(&messages[4]["content"][0]["content"]), 0);
    assert_eq!(
        count_cache_markers(&messages[1]) + count_cache_markers(&messages[3]),
        0
    );
}

#[test]
fn assistant_turn_replays_thinking_and_opaque_blocks_in_original_order() {
    let server_call = json!({
        "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "x"}
    });
    let req = request(vec![
        Message::user("go"),
        assistant(vec![
            thinking("plan", Some("sig-1"), false),
            thinking("", Some("ENCRYPTED"), true),
            Block::text("On it."),
            // Neither of these can be verified by the API, so they are not sent.
            thinking("unsigned", None, false),
            thinking("empty signature", Some(""), false),
            thinking("progress update", Some("sig-2"), false),
            call("call_1", "read", json!({"path": "a"})),
            Block::Opaque {
                provider: "mock".to_string(),
                item: server_call.clone(),
            },
            Block::Opaque {
                provider: "other".to_string(),
                item: json!({"type": "reasoning", "id": "rs_1"}),
            },
        ]),
        Message::tool(vec![Block::tool_result("call_1", "contents", false)]),
    ]);
    assert_eq!(
        build(&req)["messages"][1],
        json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "plan", "signature": "sig-1"},
                {"type": "redacted_thinking", "data": "ENCRYPTED"},
                {"type": "text", "text": "On it."},
                {"type": "thinking", "thinking": "progress update", "signature": "sig-2"},
                {"type": "tool_use", "id": "call_1", "name": "read", "input": {"path": "a"}},
                server_call
            ]
        })
    );
}

#[test]
fn manual_models_get_a_budget_per_level_below_max_tokens() {
    let levels = [
        (ThinkingLevel::Minimal, 1024),
        (ThinkingLevel::Low, 4096),
        (ThinkingLevel::Medium, 10_000),
        (ThinkingLevel::High, 24_000),
        (ThinkingLevel::Xhigh, 48_000),
        (ThinkingLevel::Max, 62_976),
    ];
    for (level, budget) in levels {
        let body = build_with_level(MANUAL_MODEL, level, None);
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": budget}),
            "{level}"
        );
        assert_eq!(body["max_tokens"], 64_000, "{level}");
        assert!(body.get("output_config").is_none(), "{level}");
    }

    // A requested limit below the budget is raised so the answer still has room.
    let body = build_with_level(MANUAL_MODEL, ThinkingLevel::High, Some(2000));
    assert_eq!(body["thinking"]["budget_tokens"], 24_000);
    assert_eq!(body["max_tokens"], 25_024);
    // One that already clears the budget is left alone.
    let body = build_with_level(MANUAL_MODEL, ThinkingLevel::Low, Some(8000));
    assert_eq!(body["max_tokens"], 8000);
}

#[test]
fn thinking_shape_follows_the_model_generation() {
    let thinking_type = |model_id: &str| {
        let body = build_with_level(model_id, ThinkingLevel::Medium, None);
        body["thinking"]["type"].as_str().unwrap().to_string()
    };
    let manual = [
        "mock-model",
        "claude-mock-4-5",
        "claude-mock-4-5-20990101",
        "claude-mock-4-20990101",
        "claude-3-7-mock-20990101",
        "vendor.claude-mock-4-5-20990101-v1:0",
        "vendor/claude-mock-4.5",
    ];
    for model_id in manual {
        assert_eq!(thinking_type(model_id), "enabled", "{model_id}");
    }
    let adaptive = [
        "claude-mock-4-6",
        "claude-mock-4-7",
        "claude-mock-5",
        "claude-mock-5-1",
        "claude-mock-preview",
        "vendor.claude-mock-5-5-v1:0",
        "claude-mock-5@20990101",
    ];
    for model_id in adaptive {
        assert_eq!(thinking_type(model_id), "adaptive", "{model_id}");
    }
}

#[test]
fn adaptive_models_get_an_effort_per_level_and_no_budget() {
    let levels = [
        (ThinkingLevel::Minimal, "low"),
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::Xhigh, "xhigh"),
        (ThinkingLevel::Max, "max"),
    ];
    for (level, effort) in levels {
        let body = build_with_level(ADAPTIVE_MODEL, level, Some(2000));
        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "summarized"}),
            "{level}"
        );
        assert_eq!(body["output_config"], json!({"effort": effort}), "{level}");
        // No budget to stay above, so the requested limit is sent unchanged.
        assert_eq!(body["max_tokens"], 2000, "{level}");
    }

    // The first adaptive generation and unversioned ids have no `xhigh` effort.
    for model_id in ["claude-mock-4-6", "claude-mock-preview"] {
        let body = build_with_level(model_id, ThinkingLevel::Xhigh, None);
        assert_eq!(body["output_config"]["effort"], "high", "{model_id}");
        let body = build_with_level(model_id, ThinkingLevel::Max, None);
        assert_eq!(body["output_config"]["effort"], "max", "{model_id}");
    }
    let body = build_with_level("claude-mock-4-7", ThinkingLevel::Xhigh, None);
    assert_eq!(body["output_config"]["effort"], "xhigh");
}

#[test]
fn thinking_off_or_unsupported_sends_no_thinking_fields() {
    for model_id in [MANUAL_MODEL, ADAPTIVE_MODEL] {
        let off = build_with_level(model_id, ThinkingLevel::Off, None);
        let mut plain = model(model_id);
        plain.reasoning = false;
        let mut req = request(vec![Message::user("hi")]);
        req.thinking = ThinkingLevel::High;
        let unsupported = anthropic::build_body(&provider(), &plain, &req);
        for body in [off, unsupported] {
            assert!(body.get("thinking").is_none(), "{model_id}");
            assert!(body.get("output_config").is_none(), "{model_id}");
            assert_eq!(body["max_tokens"], 64_000, "{model_id}");
        }
    }
}

#[test]
fn manual_thinking_waits_for_a_turn_that_opens_with_thinking() {
    let tool_turn = |first: Option<Block>| {
        let mut content: Vec<Block> = first.into_iter().collect();
        content.push(call("call_1", "read", json!({"path": "a"})));
        vec![
            Message::user("go"),
            assistant(content),
            Message::tool(vec![Block::tool_result("call_1", "contents", false)]),
        ]
    };
    let build_medium = |model_id: &str, messages: Vec<Message>| {
        let mut req = request(messages);
        req.thinking = ThinkingLevel::Medium;
        anthropic::build_body(&provider(), &model(model_id), &req)
    };

    // The turn in progress was started without thinking (for example by another model).
    let body = build_medium(MANUAL_MODEL, tool_turn(None));
    assert!(body.get("thinking").is_none());
    assert_eq!(body["max_tokens"], 64_000);

    // It opens with a thinking block: thinking continues.
    let opened = tool_turn(Some(thinking("plan", Some("sig-1"), false)));
    let body = build_medium(MANUAL_MODEL, opened);
    assert_eq!(body["thinking"]["budget_tokens"], 10_000);

    // A new user prompt starts a fresh turn, whatever came before.
    let mut finished = tool_turn(None);
    finished.push(assistant(vec![Block::text("done")]));
    finished.push(Message::user("next"));
    let body = build_medium(MANUAL_MODEL, finished);
    assert_eq!(body["thinking"]["budget_tokens"], 10_000);

    // Adaptive thinking has no such requirement.
    let body = build_medium(ADAPTIVE_MODEL, tool_turn(None));
    assert_eq!(body["thinking"]["type"], "adaptive");
}

// ---- build_request / models_request ----

fn header(request: &reqwest::Request, name: &str) -> Option<String> {
    request
        .headers()
        .get(name)
        .map(|value| value.to_str().unwrap().to_string())
}

#[test]
fn build_request_posts_the_body_with_version_and_auth_headers() {
    let client = reqwest::Client::new();
    let (provider, model) = (provider(), model(MANUAL_MODEL));
    let req = request(vec![Message::user("hi")]);
    let send = |auth: Auth| {
        anthropic::build_request(&client, &provider, &model, &req, &auth)
            .build()
            .unwrap()
    };

    let sent = send(Auth::ApiKey("sk-mock".to_string()));
    assert_eq!(*sent.method(), reqwest::Method::POST);
    assert_eq!(sent.url().as_str(), "https://mock.invalid/v1/messages");
    assert_eq!(header(&sent, "x-api-key").as_deref(), Some("sk-mock"));
    assert_eq!(header(&sent, "authorization"), None);
    assert_eq!(header(&sent, "anthropic-version").as_deref(), Some("2026-10-21"));
    assert_eq!(header(&sent, "content-type").as_deref(), Some("application/json"));
    let body: Value = serde_json::from_slice(sent.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(body, anthropic::build_body(&provider, &model, &req));

    let sent = send(Auth::OAuth {
        access: "tok-mock".to_string(),
        fields: BTreeMap::new(),
    });
    assert_eq!(header(&sent, "authorization").as_deref(), Some("Bearer tok-mock"));
    assert_eq!(header(&sent, "x-api-key"), None);

    let sent = send(Auth::None);
    assert_eq!(header(&sent, "authorization"), None);
    assert_eq!(header(&sent, "x-api-key"), None);
}

#[test]
fn provider_headers_replace_defaults() {
    let client = reqwest::Client::new();
    let mut provider = provider();
    for (name, value) in [
        ("Anthropic-Version", "2023-06-01"),
        ("x-api-key", "from-config"),
        ("anthropic-beta", "mock-beta-2099-01-01"),
    ] {
        provider.headers.insert(name.to_string(), value.to_string());
    }
    let req = request(vec![Message::user("hi")]);
    let auth = Auth::ApiKey("sk-mock".to_string());
    let sent = anthropic::build_request(&client, &provider, &model(MANUAL_MODEL), &req, &auth)
        .build()
        .unwrap();

    for (name, value) in [
        ("anthropic-version", "2023-06-01"),
        ("x-api-key", "from-config"),
        ("anthropic-beta", "mock-beta-2099-01-01"),
    ] {
        let values: Vec<&str> = sent
            .headers()
            .get_all(name)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect();
        assert_eq!(values, [value], "{name}");
    }
}

#[test]
fn models_request_asks_for_the_largest_page() {
    let client = reqwest::Client::new();
    let auth = Auth::ApiKey("sk-mock".to_string());
    let sent = anthropic::models_request(&client, &provider(), &auth)
        .build()
        .unwrap();

    assert_eq!(*sent.method(), reqwest::Method::GET);
    assert_eq!(sent.url().as_str(), "https://mock.invalid/v1/models?limit=1000");
    assert_eq!(header(&sent, "x-api-key").as_deref(), Some("sk-mock"));
    assert_eq!(header(&sent, "anthropic-version").as_deref(), Some("2026-10-21"));
    assert_eq!(header(&sent, "content-type"), None);
    assert!(sent.body().is_none());
}

// ---- classify_error / parse_models ----

#[test]
fn classify_error_returns_only_what_the_generic_mapping_would_miss() {
    let envelope = |kind: &str, message: &str| {
        json!({
            "type": "error",
            "error": {"type": kind, "message": message},
            "request_id": "req_mock"
        })
        .to_string()
    };
    let too_long = "prompt is too long: 215000 tokens > 200000 maximum";
    let over_limit = "input length and `max_tokens` exceed context limit: 199000 + 8192 > 200000";
    let credit = "Your credit balance is too low";
    let invalid = "invalid_request_error";
    let overflow = Some(AiError::ContextOverflow);
    let overloaded = Some(AiError::Overloaded);
    let rate_limit = Some(AiError::RateLimit { retry_after: None });
    let billing = Some(AiError::Auth(credit.to_string()));
    let cases = [
        (400, invalid, too_long, overflow.clone()),
        (400, invalid, over_limit, overflow.clone()),
        (413, "request_too_large", "Request too large", overflow),
        (529, "overloaded_error", "Overloaded", overloaded.clone()),
        // The generic mapping would call this a plain server error.
        (500, "overloaded_error", "Overloaded", overloaded),
        (400, "rate_limit_error", "Slow down", rate_limit.clone()),
        // Not retryable on this account, but another model in the chain may answer.
        (402, "billing_error", credit, billing),
        // A subscription refusing to bill the request to the plan: the account's allowance is
        // used up, so another account or model may answer.
        (
            400,
            invalid,
            "You're out of extra usage. Add more at claude.ai/settings/usage",
            Some(AiError::Quota {
                resets_at: None,
                message: "You're out of extra usage. Add more at claude.ai/settings/usage".into(),
            }),
        ),
        (401, "authentication_error", "invalid x-api-key", None),
        (403, "permission_error", "not allowed", None),
        (429, "rate_limit_error", "Slow down", None),
        (404, "not_found_error", "model: mock-model", None),
        (500, "api_error", "Internal server error", None),
        (504, "timeout_error", "Timed out", None),
    ];
    for (status, kind, message, expected) in cases {
        assert_eq!(
            anthropic::classify_error(status, &envelope(kind, message)),
            expected,
            "{status} {kind}"
        );
    }

    // The edge proxy answers without the JSON envelope.
    assert_eq!(
        anthropic::classify_error(413, "<html>Payload Too Large</html>"),
        Some(AiError::ContextOverflow)
    );
    assert_eq!(anthropic::classify_error(502, "<html>Bad Gateway</html>"), None);
    assert_eq!(anthropic::classify_error(400, ""), None);
}

fn unified(pairs: &[(&str, &str)]) -> reqwest::header::HeaderMap {
    pairs
        .iter()
        .map(|(name, value)| {
            (
                format!("anthropic-ratelimit-unified-{name}").parse().unwrap(),
                value.parse().unwrap(),
            )
        })
        .collect::<reqwest::header::HeaderMap<reqwest::header::HeaderValue>>()
}

fn window(name: &str, used: f32, resets_at: Option<u64>, status: &str) -> Window {
    Window {
        name: name.to_string(),
        used,
        resets_at,
        status: status.to_string(),
    }
}

#[test]
fn limits_reads_the_usage_windows_of_a_subscription_from_the_headers() {
    // What a subscription answers while everything is within its allowance.
    let headers = unified(&[
        ("status", "allowed"),
        ("reset", "1790000000"),
        ("representative-claim", "five_hour"),
        ("5h-utilization", "0.42"),
        ("5h-reset", "1790000000"),
        ("7d-utilization", "0.07"),
        ("7d-reset", "1790500000"),
        ("overage-status", "rejected"),
        ("overage-disabled-reason", "org_level_disabled"),
    ]);
    assert_eq!(
        anthropic::limits(&headers),
        [
            window("5h", 0.42, Some(1_790_000_000), "allowed"),
            window("7d", 0.07, Some(1_790_500_000), "allowed"),
            window("overage", 0.0, None, "rejected (org_level_disabled)"),
        ]
    );

    // The status speaks about the window the claim names; the other stays allowed.
    let headers = unified(&[
        ("status", "rejected"),
        ("reset", "1790500000"),
        ("representative-claim", "seven_day"),
        ("5h-utilization", "0.5"),
        ("5h-reset", "1790000000"),
        ("7d-utilization", "1.0"),
        ("7d-reset", "1790500000"),
    ]);
    let windows = anthropic::limits(&headers);
    assert_eq!(windows[0], window("5h", 0.5, Some(1_790_000_000), "allowed"));
    assert_eq!(windows[1], window("7d", 1.0, Some(1_790_500_000), "rejected"));
    assert!(windows[1].exhausted() && !windows[0].exhausted());
    assert_eq!(windows[1].label(), "7-day limit");

    // A claim without utilisation headers of its own still becomes a window.
    let headers = unified(&[
        ("status", "allowed_warning"),
        ("reset", "1790600000"),
        ("representative-claim", "seven_day_opus"),
        ("overage-status", "allowed"),
        ("overage-reset", "1791000000"),
    ]);
    assert_eq!(
        anthropic::limits(&headers),
        [
            window("7d-opus", 0.0, Some(1_790_600_000), "allowed_warning"),
            window("overage", 0.0, Some(1_791_000_000), "allowed"),
        ]
    );
    let headers = unified(&[("status", "rejected"), ("representative-claim", "five_hour"), ("reset", "1790000000")]);
    assert_eq!(
        anthropic::limits(&headers),
        [window("5h", 1.0, Some(1_790_000_000), "rejected")]
    );

    // An API key's responses carry none of these headers.
    assert!(anthropic::limits(&reqwest::header::HeaderMap::new()).is_empty());
    let headers = unified(&[("5h-utilization", "not a number"), ("status", "allowed")]);
    assert!(anthropic::limits(&headers).is_empty());
}

#[test]
fn a_full_usage_window_is_a_quota_error_with_its_reset_time() {
    let body = json!({"type": "error", "error": {"type": "rate_limit_error", "message": "This request would exceed your account's rate limit."}}).to_string();
    let rejected = unified(&[
        ("status", "rejected"),
        ("reset", "1790000000"),
        ("representative-claim", "five_hour"),
        ("5h-utilization", "1.01"),
        ("5h-reset", "1790000000"),
    ]);
    assert_eq!(
        anthropic::classify_response(429, &rejected, &body),
        Some(AiError::Quota {
            resets_at: Some(1_790_000_000),
            message: "This request would exceed your account's rate limit.".into(),
        })
    );
    // Without a body the window names what ran out.
    assert_eq!(
        anthropic::classify_response(429, &rejected, ""),
        Some(AiError::Quota {
            resets_at: Some(1_790_000_000),
            message: "the 5-hour limit of this account is used up".into(),
        })
    );
    // Asked too often, with the window still open: an ordinary rate limit, left to the generic
    // mapping and its `retry-after`.
    let allowed = unified(&[("status", "allowed_warning"), ("5h-utilization", "0.9"), ("5h-reset", "1790000000")]);
    assert_eq!(anthropic::classify_response(429, &allowed, &body), None);
    assert_eq!(
        anthropic::classify_response(429, &reqwest::header::HeaderMap::new(), &body),
        None
    );

    // The 400 about extra usage takes its reset time from the window the headers call full.
    let extra = json!({"type": "error", "error": {"type": "invalid_request_error", "message": "You're out of extra usage. Add more at claude.ai/settings/usage"}}).to_string();
    let full_week = unified(&[
        ("status", "rejected"),
        ("representative-claim", "seven_day"),
        ("7d-utilization", "1.0"),
        ("7d-reset", "1790500000"),
    ]);
    assert_eq!(
        anthropic::classify_response(400, &full_week, &extra),
        Some(AiError::Quota {
            resets_at: Some(1_790_500_000),
            message: "You're out of extra usage. Add more at claude.ai/settings/usage".into(),
        })
    );
    // Everything else is what `classify_error` says.
    assert_eq!(
        anthropic::classify_response(413, &reqwest::header::HeaderMap::new(), ""),
        Some(AiError::ContextOverflow)
    );
}

#[test]
fn the_priority_tier_is_fast_mode_and_only_claude_models_are_asked_for_it() {
    let client = reqwest::Client::new();
    let auth = Auth::ApiKey("sk-mock".to_string());
    let sent = |provider: &ProviderConfig, model_id: &str, tier: Option<Tier>| {
        let mut req = request(vec![Message::user("hi")]);
        req.tier = tier;
        let built = anthropic::build_request(&client, provider, &model(model_id), &req, &auth)
            .build()
            .unwrap();
        let body: Value = serde_json::from_slice(built.body().unwrap().as_bytes().unwrap()).unwrap();
        (body.get("speed").cloned(), header(&built, "anthropic-beta"))
    };
    let plain = provider();

    // From Opus 5.5 on, and on the other families, `speed` is a regular member.
    for id in ["claude-opus-5-5", "claude-fable-5-2", "claude-mythos-5-2"] {
        assert_eq!(sent(&plain, id, Some(Tier::Priority)), (Some(json!("fast")), None), "{id}");
    }
    // Where fast mode is a preview it needs its beta header.
    for id in ["claude-opus-5", "claude-opus-5-20260801", "claude-opus-4-8"] {
        assert_eq!(
            sent(&plain, id, Some(Tier::Priority)),
            (Some(json!("fast")), Some("fast-mode-2026-02-01".to_string())),
            "{id}"
        );
    }
    // The header joins the ones a login already sends.
    let mut login = provider();
    login
        .headers
        .insert("anthropic-beta".to_string(), "oauth-2025-04-20".to_string());
    assert_eq!(
        sent(&login, "claude-opus-5", Some(Tier::Priority)).1.as_deref(),
        Some("oauth-2025-04-20,fast-mode-2026-02-01")
    );
    assert_eq!(
        sent(&login, "claude-opus-5", None),
        (None, Some("oauth-2025-04-20".to_string()))
    );

    // No tier, the default tier and the cheaper tier send nothing; nor does another vendor's
    // model behind a compatible endpoint.
    for tier in [None, Some(Tier::Standard), Some(Tier::Economy)] {
        assert_eq!(sent(&plain, "claude-opus-5-5", tier), (None, None), "{tier:?}");
    }
    assert_eq!(sent(&plain, MANUAL_MODEL, Some(Tier::Priority)), (None, None));
}

#[test]
fn parse_models_reads_limits_and_capabilities_and_defaults_the_rest() {
    let body = json!({
        "data": [
            {
                "type": "model", "id": "mock-large", "display_name": "Mock Large",
                "created_at": "2099-01-01T00:00:00Z", "lifecycle": "active",
                "max_input_tokens": 1_000_000, "max_tokens": 128_000,
                "capabilities": {
                    "image_input": {"supported": true},
                    "thinking": {"supported": true, "types": {"adaptive": {"supported": true}}}
                }
            },
            {
                "type": "model", "id": "mock-small", "display_name": "Mock Small",
                "lifecycle": "deprecated", "max_input_tokens": null, "max_tokens": 0,
                "capabilities": null
            },
            {
                "type": "model", "id": "mock-text", "display_name": "Mock Text", "lifecycle": "active",
                "max_input_tokens": 200_000, "max_tokens": 16_000,
                "capabilities": {"image_input": {"supported": false}, "thinking": {"supported": false}}
            },
            {"type": "model", "id": "mock-gone", "lifecycle": "retired", "max_tokens": 4096},
            {"type": "model", "display_name": "No id"}
        ],
        "first_id": "mock-large",
        "has_more": false,
        "last_id": "mock-gone"
    });
    let models = anthropic::parse_models(&provider(), &body);

    let large = ModelInfo {
        context_window: 1_000_000,
        max_output: 128_000,
        vision: true,
        reasoning: true,
        ..ModelInfo::unknown("mock", "mock-large")
    };
    let text = ModelInfo {
        context_window: 200_000,
        max_output: 16_000,
        ..ModelInfo::unknown("mock", "mock-text")
    };
    // Unknown limits and capabilities keep the conservative defaults; retired models are left out.
    assert_eq!(models, [large, ModelInfo::unknown("mock", "mock-small"), text]);

    let error_body = json!({"type": "error", "error": {"type": "api_error", "message": "x"}});
    assert!(anthropic::parse_models(&provider(), &error_body).is_empty());
}
