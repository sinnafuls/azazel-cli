#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use azazel_ai::wire::{decode_sse, openai_responses};
use azazel_ai::{
    AiError, AssistantMeta, Auth, Block, INVALID_JSON_KEY, Message, ModelInfo, ProviderConfig, Request,
    StopReason, StreamEvent, SystemPart, ThinkingLevel, Tier, ToolSpec, Usage, WireApi,
};
use serde_json::{Value, json};

fn provider() -> ProviderConfig {
    ProviderConfig::new("mock", WireApi::OpenAiResponses, "http://localhost:9/v1")
}

fn model(reasoning: bool) -> ModelInfo {
    ModelInfo {
        vision: true,
        reasoning,
        ..ModelInfo::unknown("mock", "mock-model")
    }
}

fn body(req: &Request) -> Value {
    openai_responses::build_body(&provider(), &model(false), req)
}

fn assistant(content: Vec<Block>) -> Message {
    Message::assistant(
        content,
        AssistantMeta {
            provider: "mock".into(),
            model: "mock-model".into(),
            api: WireApi::OpenAiResponses,
            usage: Usage::default(),
            stop: StopReason::Stop,
            response_id: None,
            duration_ms: 0,
            sources: Vec::new(),
        },
    )
}

fn tool_call(id: &str, name: &str, input: Value) -> Block {
    Block::ToolCall {
        id: id.into(),
        name: name.into(),
        input,
        signature: None,
    }
}

fn thinking(text: &str) -> Block {
    Block::Thinking {
        text: text.into(),
        signature: None,
        redacted: false,
    }
}

fn opaque(item: Value) -> Block {
    Block::Opaque {
        provider: "mock".into(),
        item,
    }
}

fn decode(bytes: &[u8]) -> Vec<StreamEvent> {
    let mut decoder = openai_responses::decoder(&provider(), &model(true));
    decode_sse(bytes, &mut *decoder)
}

/// Frames each payload the way the API does: an `event:` line naming its `type`, then the JSON.
fn sse(payloads: &[Value]) -> Vec<u8> {
    let mut out = String::new();
    for payload in payloads {
        let kind = payload["type"].as_str().unwrap();
        out.push_str(&format!("event: {kind}\ndata: {payload}\n\n"));
    }
    out.into_bytes()
}

fn created() -> Value {
    json!({"type": "response.created", "response": {"id": "resp_test", "status": "in_progress", "usage": null}})
}

fn item_added(output_index: u64, item: Value) -> Value {
    json!({"type": "response.output_item.added", "output_index": output_index, "item": item})
}

fn item_done(output_index: u64, item: Value) -> Value {
    json!({"type": "response.output_item.done", "output_index": output_index, "item": item})
}

fn text_delta(output_index: u64, delta: &str) -> Value {
    json!({
        "type": "response.output_text.delta",
        "item_id": "msg_test",
        "output_index": output_index,
        "content_index": 0,
        "delta": delta,
    })
}

fn message_item(text: Option<&str>) -> Value {
    match text {
        Some(text) => json!({
            "id": "msg_test",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "annotations": [], "text": text}],
        }),
        None => {
            json!({"id": "msg_test", "type": "message", "status": "in_progress", "role": "assistant", "content": []})
        }
    }
}

fn completed(usage: Value) -> Value {
    json!({"type": "response.completed", "response": {"id": "resp_test", "status": "completed", "usage": usage}})
}

fn done(events: &[StreamEvent]) -> &Message {
    match events.last() {
        Some(StreamEvent::Done { message }) => message,
        other => panic!("expected a terminal Done, got {other:?}"),
    }
}

fn failure(events: &[StreamEvent]) -> (&AiError, Option<&Message>) {
    match events.last() {
        Some(StreamEvent::Error { error, partial }) => (error, partial.as_ref()),
        other => panic!("expected a terminal Error, got {other:?}"),
    }
}

fn meta(message: &Message) -> &AssistantMeta {
    message.meta.as_ref().unwrap()
}

fn deltas(events: &[StreamEvent], wanted: usize) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta { index, delta }
            | StreamEvent::ThinkingDelta { index, delta }
            | StreamEvent::ToolCallDelta { index, delta }
                if *index == wanted =>
            {
                Some(delta.as_str())
            }
            _ => None,
        })
        .collect()
}

fn block_ends(events: &[StreamEvent]) -> Vec<usize> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::BlockEnd { index, .. } => Some(*index),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

#[test]
fn text_fixture() {
    let events = decode(include_bytes!("fixtures/openai-responses/text.sse"));
    insta::assert_snapshot!(serde_json::to_string_pretty(&events).unwrap());

    assert_eq!(events.first(), Some(&StreamEvent::Start));
    let message = done(&events);
    assert_eq!(
        message.content,
        vec![Block::text("Hello! How can I help you today?")]
    );
    assert_eq!(deltas(&events, 0), "Hello! How can I help you today?");
    let meta = meta(message);
    assert_eq!(meta.stop, StopReason::Stop);
    assert_eq!(
        meta.usage,
        Usage {
            input: 21,
            output: 9,
            ..Usage::default()
        }
    );
    assert_eq!(
        meta.response_id.as_deref(),
        Some("resp_0b6f2e41c7d93a5800690f3c2a1b4c81")
    );
    assert_eq!(
        (
            meta.provider.as_str(),
            meta.model.as_str(),
            meta.api,
            meta.duration_ms
        ),
        ("mock", "mock-model", WireApi::OpenAiResponses, 0)
    );
}

#[test]
fn tool_call_fixture() {
    let events = decode(include_bytes!("fixtures/openai-responses/tool_call.sse"));
    insta::assert_snapshot!(serde_json::to_string_pretty(&events).unwrap());

    assert_eq!(events.first(), Some(&StreamEvent::Start));
    // The tool-call id is the item's `call_id`, not its `fc_…` item id.
    assert_eq!(
        events[1],
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_h7Kp2LmQ9sVtXw3R".into(),
            name: "read".into(),
        }
    );
    assert_eq!(deltas(&events, 0), r#"{"path":"src/main.rs","limit":40}"#);
    let call = tool_call(
        "call_h7Kp2LmQ9sVtXw3R",
        "read",
        json!({"path": "src/main.rs", "limit": 40}),
    );
    assert!(events.contains(&StreamEvent::BlockEnd {
        index: 0,
        block: call.clone(),
    }));
    let message = done(&events);
    assert_eq!(message.content, vec![call]);
    let meta = meta(message);
    assert_eq!(meta.stop, StopReason::ToolUse);
    assert_eq!(
        meta.usage,
        Usage {
            input: 124,
            output: 18,
            cache_read: 256,
            ..Usage::default()
        }
    );
}

#[test]
fn thinking_tool_fixture() {
    let events = decode(include_bytes!("fixtures/openai-responses/thinking_tool.sse"));
    insta::assert_snapshot!(serde_json::to_string_pretty(&events).unwrap());

    assert_eq!(events.first(), Some(&StreamEvent::Start));
    let summary =
        "**Checking the tests** The user wants the tests run.\n\n**Choosing the tool** The shell tool fits.";
    let reasoning_item = json!({
        "id": "rs_07e3b9d05a6c41f200690f3e5630c890",
        "type": "reasoning",
        "encrypted_content": "gAAAAABpDz5XU2VhbGVkLXJlYXNvbmluZy1zdGF0ZS1mb3ItcmVwbGF5LW9ubHk=",
        "summary": [
            {"type": "summary_text", "text": "**Checking the tests** The user wants the tests run."},
            {"type": "summary_text", "text": "**Choosing the tool** The shell tool fits."},
        ],
    });
    let message = done(&events);
    assert_eq!(
        message.content,
        vec![
            thinking(summary),
            opaque(reasoning_item),
            Block::text("I'll run the test suite."),
            tool_call("call_Q4dTn8Yb1ZsGm6Xe", "shell", json!({"command": "cargo test"})),
        ]
    );
    // The streamed summary matches the final block, paragraph break included.
    assert_eq!(deltas(&events, 0), summary);
    assert_eq!(deltas(&events, 2), "I'll run the test suite.");
    assert_eq!(block_ends(&events), vec![0, 1, 2, 3]);
    assert!(events.contains(&StreamEvent::ToolCallStart {
        index: 3,
        id: "call_Q4dTn8Yb1ZsGm6Xe".into(),
        name: "shell".into(),
    }));
    let meta = meta(message);
    assert_eq!(meta.stop, StopReason::ToolUse);
    assert_eq!(
        meta.usage,
        Usage {
            input: 128,
            output: 142,
            cache_read: 512,
            reasoning: 96,
            ..Usage::default()
        }
    );
}

#[test]
fn midstream_error_fixture() {
    let events = decode(include_bytes!("fixtures/openai-responses/midstream_error.sse"));
    insta::assert_snapshot!(serde_json::to_string_pretty(&events).unwrap());

    assert_eq!(events.first(), Some(&StreamEvent::Start));
    let (error, partial) = failure(&events);
    let AiError::Server { status, body } = error else {
        panic!("expected a server error, got {error:?}");
    };
    assert_eq!(*status, 500);
    assert!(body.starts_with("An error occurred while processing your request."));
    assert!(error.is_retryable());
    let partial = partial.unwrap();
    assert_eq!(partial.content, vec![Block::text("The answer is")]);
    let meta = meta(partial);
    assert_eq!(meta.stop, StopReason::Error);
    assert_eq!(
        meta.response_id.as_deref(),
        Some("resp_02a9d4f7c18e63b500690f3f0c77de84")
    );
    // The message item never completed, so it has no BlockEnd.
    assert!(block_ends(&events).is_empty());
}

// ---------------------------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------------------------

#[test]
fn incomplete_at_the_output_limit_is_length_and_keeps_the_streamed_text() {
    let events = decode(&sse(&[
        created(),
        item_added(0, message_item(None)),
        text_delta(0, "Once upon"),
        text_delta(0, " a"),
        json!({"type": "response.incomplete", "response": {
            "id": "resp_test",
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "usage": {"input_tokens": 10, "output_tokens": 4},
        }}),
    ]));
    let message = done(&events);
    assert_eq!(message.content, vec![Block::text("Once upon a")]);
    assert_eq!(block_ends(&events), vec![0]);
    assert_eq!(meta(message).stop, StopReason::Length);
    assert_eq!(meta(message).usage.output, 4);
}

#[test]
fn incomplete_for_another_reason_is_a_normal_stop() {
    let events = decode(&sse(&[
        created(),
        item_added(0, message_item(None)),
        text_delta(0, "I can't help with that."),
        item_done(0, message_item(Some("I can't help with that."))),
        json!({"type": "response.incomplete", "response": {
            "id": "resp_test",
            "status": "incomplete",
            "incomplete_details": {"reason": "content_filter"},
        }}),
    ]));
    assert_eq!(meta(done(&events)).stop, StopReason::Stop);
}

#[test]
fn reasoning_without_a_summary_yields_only_the_opaque_item() {
    let item = json!({"id": "rs_test", "type": "reasoning", "encrypted_content": "sealed", "summary": []});
    let events = decode(&sse(&[
        created(),
        item_added(0, json!({"id": "rs_test", "type": "reasoning", "summary": []})),
        item_done(0, item.clone()),
        item_added(1, message_item(None)),
        text_delta(1, "Hi."),
        item_done(1, message_item(Some("Hi."))),
        completed(
            json!({"input_tokens": 5, "output_tokens": 40, "output_tokens_details": {"reasoning_tokens": 32}}),
        ),
    ]));
    assert_eq!(done(&events).content, vec![opaque(item), Block::text("Hi.")]);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::ThinkingDelta { .. }))
    );
    assert_eq!(meta(done(&events)).usage.reasoning, 32);
}

#[test]
fn raw_reasoning_text_is_streamed_as_thinking() {
    let item = json!({
        "id": "rs_test",
        "type": "reasoning",
        "summary": [],
        "content": [{"type": "reasoning_text", "text": "Two plus two is four."}],
    });
    let events = decode(&sse(&[
        created(),
        item_added(0, json!({"id": "rs_test", "type": "reasoning", "summary": []})),
        json!({"type": "response.reasoning_text.delta", "item_id": "rs_test", "output_index": 0, "content_index": 0, "delta": "Two plus two"}),
        json!({"type": "response.reasoning_text.delta", "item_id": "rs_test", "output_index": 0, "content_index": 0, "delta": " is four."}),
        item_done(0, item.clone()),
        completed(json!({"input_tokens": 5, "output_tokens": 9})),
    ]));
    assert_eq!(deltas(&events, 0), "Two plus two is four.");
    assert_eq!(
        done(&events).content,
        vec![thinking("Two plus two is four."), opaque(item)]
    );
}

#[test]
fn function_call_delivered_only_as_a_finished_item_is_still_announced() {
    let events = decode(&sse(&[
        created(),
        item_done(
            0,
            json!({"id": "fc_test", "type": "function_call", "status": "completed", "call_id": "call_x", "name": "todo", "arguments": ""}),
        ),
        completed(json!({"input_tokens": 5, "output_tokens": 9})),
    ]));
    let call = tool_call("call_x", "todo", json!({}));
    assert_eq!(
        events[1..3],
        [
            StreamEvent::ToolCallStart {
                index: 0,
                id: "call_x".into(),
                name: "todo".into(),
            },
            StreamEvent::BlockEnd {
                index: 0,
                block: call.clone(),
            },
        ]
    );
    assert_eq!(done(&events).content, vec![call]);
    assert_eq!(meta(done(&events)).stop, StopReason::ToolUse);
}

#[test]
fn arguments_that_are_not_json_are_reported_under_the_invalid_key() {
    let events = decode(&sse(&[
        created(),
        item_added(
            0,
            json!({"id": "fc_test", "type": "function_call", "call_id": "call_x", "name": "read", "arguments": ""}),
        ),
        json!({"type": "response.function_call_arguments.delta", "item_id": "fc_test", "output_index": 0, "delta": "{\"path\":"}),
        item_done(
            0,
            json!({"id": "fc_test", "type": "function_call", "call_id": "call_x", "name": "read", "arguments": "{\"path\":"}),
        ),
        completed(json!({"input_tokens": 5, "output_tokens": 9})),
    ]));
    assert_eq!(
        done(&events).content,
        vec![tool_call(
            "call_x",
            "read",
            json!({ INVALID_JSON_KEY: "{\"path\":" })
        )]
    );
}

#[test]
fn items_this_harness_does_not_use_produce_no_blocks() {
    let search = json!({"id": "ws_test", "type": "web_search_call", "status": "completed", "action": {"type": "search", "query": "rust"}});
    let events = decode(&sse(&[
        created(),
        item_added(0, search.clone()),
        json!({"type": "response.web_search_call.completed", "item_id": "ws_test", "output_index": 0}),
        item_done(0, search),
        item_added(1, message_item(None)),
        text_delta(1, "Found it."),
        item_done(1, message_item(Some("Found it."))),
        completed(json!({"input_tokens": 5, "output_tokens": 9})),
    ]));
    assert_eq!(done(&events).content, vec![Block::text("Found it.")]);
    assert_eq!(block_ends(&events), vec![0]);
}

#[test]
fn error_events_are_classified() {
    let cases = [
        (
            json!({"type": "error", "code": "context_length_exceeded", "message": "Your input exceeds the context window of this model. Please adjust your input and try again.", "param": "input"}),
            AiError::ContextOverflow,
        ),
        (
            json!({"type": "error", "code": "rate_limit_exceeded", "message": "Rate limit reached.", "param": null}),
            AiError::RateLimit { retry_after: None },
        ),
        (
            json!({"type": "error", "code": "ERR_SOMETHING", "message": "Something went wrong", "param": null}),
            AiError::Server {
                status: 500,
                body: "Something went wrong".into(),
            },
        ),
        (
            json!({"type": "response.failed", "response": {"id": "resp_test", "status": "failed", "error": {"code": "invalid_prompt", "message": "The prompt was rejected."}}}),
            AiError::InvalidRequest("The prompt was rejected.".into()),
        ),
        (
            json!({"type": "response.failed", "response": {"id": "resp_test", "status": "failed", "error": {"code": "invalid_prompt", "message": "This endpoint's maximum context length is 8192 tokens."}}}),
            AiError::ContextOverflow,
        ),
        (
            json!({"type": "response.failed", "response": {
                "id": "resp_test",
                "status": "failed",
                "error": {"code": "server_error", "message": "Provider is busy"},
                "error_type": "provider_overloaded",
            }}),
            AiError::Overloaded,
        ),
        (
            json!({"type": "response.failed", "response": {"id": "resp_test", "status": "failed", "error": null}}),
            AiError::Server {
                status: 500,
                body: "the provider reported a failed response".into(),
            },
        ),
    ];
    for (payload, expected) in cases {
        let events = decode(&sse(&[created(), payload]));
        assert_eq!(events.first(), Some(&StreamEvent::Start));
        let (error, partial) = failure(&events);
        assert_eq!(error, &expected);
        // Nothing had streamed yet.
        assert!(partial.is_none());
    }
}

#[test]
fn stream_cut_before_a_terminal_event_is_a_network_error_with_the_partial_text() {
    let events = decode(&sse(&[
        created(),
        item_added(0, message_item(None)),
        text_delta(0, "Half"),
        item_done(0, message_item(Some("Half"))),
    ]));
    let (error, partial) = failure(&events);
    assert_eq!(
        error,
        &AiError::Network("stream ended before the response was complete".into())
    );
    assert_eq!(partial.unwrap().content, vec![Block::text("Half")]);

    let events = decode(b"");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], StreamEvent::Start);
    assert!(matches!(failure(&events), (AiError::Network(_), None)));
}

#[test]
fn an_unfinished_tool_call_is_left_out_of_the_partial_message() {
    let events = decode(&sse(&[
        created(),
        item_added(0, message_item(None)),
        text_delta(0, "Reading."),
        item_done(0, message_item(Some("Reading."))),
        item_added(
            1,
            json!({"id": "fc_test", "type": "function_call", "call_id": "call_x", "name": "read", "arguments": ""}),
        ),
        json!({"type": "response.function_call_arguments.delta", "item_id": "fc_test", "output_index": 1, "delta": "{\"pa"}),
        json!({"type": "error", "code": "server_error", "message": "boom", "param": null}),
    ]));
    let (_, partial) = failure(&events);
    assert_eq!(partial.unwrap().content, vec![Block::text("Reading.")]);
}

#[test]
fn malformed_payloads_are_skipped_and_nothing_follows_a_terminal_event() {
    let mut decoder = openai_responses::decoder(&provider(), &model(false));
    assert!(
        decoder
            .feed(Some("response.created"), "{\"type\":\"response.cre")
            .is_empty()
    );
    assert_eq!(
        decoder.feed(Some("response.created"), &created().to_string()),
        vec![StreamEvent::Start]
    );
    let events = decoder.feed(
        Some("response.completed"),
        &completed(json!({"input_tokens": 1, "output_tokens": 0})).to_string(),
    );
    assert!(done(&events).content.is_empty());
    assert_eq!(meta(done(&events)).response_id.as_deref(), Some("resp_test"));
    assert!(
        decoder
            .feed(
                Some("response.output_text.delta"),
                &text_delta(0, "late").to_string()
            )
            .is_empty()
    );
    assert!(decoder.finish().is_empty());
}

#[test]
fn event_type_falls_back_to_the_sse_event_name() {
    let mut decoder = openai_responses::decoder(&provider(), &model(false));
    let events = decoder.feed(
        Some("response.output_text.delta"),
        r#"{"output_index":0,"content_index":0,"delta":"Hi"}"#,
    );
    assert_eq!(
        events,
        vec![
            StreamEvent::Start,
            StreamEvent::TextDelta {
                index: 0,
                delta: "Hi".into(),
            },
        ]
    );
}

#[test]
fn usage_separates_cache_reads_and_writes_from_input() {
    let events = decode(&sse(&[
        created(),
        completed(json!({
            "input_tokens": 1000,
            "input_tokens_details": {"cached_tokens": 600, "cache_write_tokens": 300},
            "output_tokens": 50,
            "output_tokens_details": {"reasoning_tokens": 20},
            "total_tokens": 1050,
        })),
    ]));
    assert_eq!(
        meta(done(&events)).usage,
        Usage {
            input: 100,
            output: 50,
            cache_read: 600,
            cache_write: 300,
            reasoning: 20,
            cost_usd: None,
        }
    );
}

// ---------------------------------------------------------------------------------------------
// build_body
// ---------------------------------------------------------------------------------------------

#[test]
fn the_notes_of_a_request_follow_the_conversation() {
    let body = body(&Request {
        messages: vec![
            Message::user("hi"),
            assistant(vec![Block::text("hello")]),
            Message::user("go on"),
        ],
        tail: vec![
            SystemPart {
                text: "Date: today".into(),
                name: "now".into(),
                cache_boundary: false,
            },
            SystemPart {
                text: "# Plan mode".into(),
                name: "mode".into(),
                cache_boundary: false,
            },
        ],
        ..Request::default()
    });
    let input = body["input"].as_array().unwrap();
    assert_eq!(
        input.last().unwrap(),
        &json!({"type": "message", "role": "user", "content": [
            {"type": "input_text", "text": "go on"},
            {"type": "input_text", "text": "<system-note>\nDate: today\n\n# Plan mode\n</system-note>"},
        ]})
    );
    // They are no part of the instructions, which stay what they were from request to request.
    assert!(body.get("instructions").is_none());
}

#[test]
fn body_is_a_stateless_streaming_request_and_omits_optional_fields() {
    let body = body(&Request {
        messages: vec![Message::user("hi")],
        ..Request::default()
    });
    assert_eq!(body["model"], json!("mock-model"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["store"], json!(false));
    assert_eq!(
        body["input"],
        json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}])
    );
    for absent in [
        "instructions",
        "tools",
        "max_output_tokens",
        "prompt_cache_key",
        "reasoning",
        "include",
        "temperature",
    ] {
        assert!(body.get(absent).is_none(), "{absent}");
    }
}

#[test]
fn system_parts_become_instructions_and_the_cache_key_is_forwarded() {
    let body = body(&Request {
        system: vec![
            SystemPart {
                text: "You are terse.".into(),
                name: String::new(),
                cache_boundary: false,
            },
            SystemPart {
                text: String::new(),
                name: String::new(),
                cache_boundary: true,
            },
            SystemPart {
                text: "Project: demo.".into(),
                name: String::new(),
                cache_boundary: false,
            },
        ],
        messages: vec![Message::user("hi")],
        max_output: Some(4321),
        cache_key: Some("session-7".into()),
        ..Request::default()
    });
    assert_eq!(body["instructions"], json!("You are terse.\n\nProject: demo."));
    assert_eq!(body["prompt_cache_key"], json!("session-7"));
    assert_eq!(body["max_output_tokens"], json!(4321));
    // The system prompt is not repeated as an input item.
    assert_eq!(body["input"].as_array().unwrap().len(), 1);
}

#[test]
fn tools_are_sent_as_flat_function_tools() {
    let schema = json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]});
    let body = body(&Request {
        messages: vec![Message::user("hi")],
        tools: vec![ToolSpec {
            name: "read".into(),
            description: "Read a file.".into(),
            schema: schema.clone(),
        }],
        ..Request::default()
    });
    assert_eq!(
        body["tools"],
        json!([{
            "type": "function",
            "name": "read",
            "description": "Read a file.",
            "parameters": schema,
            "strict": false,
        }])
    );
}

#[test]
fn conversation_is_replayed_as_input_items_in_block_order() {
    let reasoning = json!({
        "id": "rs_test",
        "type": "reasoning",
        "encrypted_content": "sealed",
        "summary": [{"type": "summary_text", "text": "visible summary"}],
    });
    let body = body(&Request {
        messages: vec![
            Message::user_blocks(vec![
                Block::text("what is in this file?"),
                Block::Image {
                    mime: "image/png".into(),
                    data: "AAAA".into(),
                },
            ]),
            assistant(vec![
                thinking("display-only thinking"),
                opaque(reasoning.clone()),
                Block::text("Looking."),
                tool_call("call_1", "read", json!({"path": "a.rs"})),
            ]),
            Message::tool(vec![Block::tool_result("call_1", "fn main() {}", false)]),
            assistant(vec![Block::text("It is a Rust program.")]),
        ],
        ..Request::default()
    });
    assert_eq!(
        body["input"],
        json!([
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "what is in this file?"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "auto"},
                ],
            },
            reasoning,
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Looking.", "annotations": []}],
            },
            {"type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a.rs\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "fn main() {}"},
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "It is a Rust program.", "annotations": []}],
            },
        ])
    );
    assert!(!body.to_string().contains("display-only thinking"));
}

#[test]
fn only_this_providers_replayable_items_are_sent_back() {
    let body = body(&Request {
        messages: vec![
            Message::user("go"),
            assistant(vec![
                Block::Opaque {
                    provider: "other".into(),
                    item: json!({"type": "reasoning", "id": "rs_other", "encrypted_content": "foreign", "summary": []}),
                },
                // Without encrypted content the item only exists in server-side state.
                opaque(json!({"type": "reasoning", "id": "rs_stored", "summary": []})),
                opaque(
                    json!({"type": "reasoning", "id": "rs_sealed", "encrypted_content": "sealed", "summary": []}),
                ),
                Block::text("Done."),
            ]),
        ],
        ..Request::default()
    });
    let types: Vec<&str> = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["message", "reasoning", "message"]);
    assert_eq!(body["input"][1]["id"], json!("rs_sealed"));
}

#[test]
fn tool_results_are_strings_or_a_content_list_when_they_carry_images() {
    let body = body(&Request {
        messages: vec![
            Message::user("look"),
            assistant(vec![
                tool_call("call_1", "screenshot", json!({})),
                tool_call("call_2", "read", json!({"path": "a"})),
            ]),
            Message::tool(vec![
                Block::ToolResult {
                    call_id: "call_1".into(),
                    content: vec![
                        Block::text("captured 1 frame"),
                        Block::Image {
                            mime: "image/jpeg".into(),
                            data: "BBBB".into(),
                        },
                    ],
                    is_error: false,
                },
                Block::ToolResult {
                    call_id: "call_2".into(),
                    content: vec![Block::text("line one"), Block::text("line two")],
                    is_error: true,
                },
            ]),
        ],
        ..Request::default()
    });
    let input = body["input"].as_array().unwrap();
    assert_eq!(
        input[3],
        json!({
            "type": "function_call_output",
            "call_id": "call_1",
            "output": [
                {"type": "input_text", "text": "captured 1 frame"},
                {"type": "input_image", "image_url": "data:image/jpeg;base64,BBBB", "detail": "auto"},
            ],
        })
    );
    assert_eq!(
        input[4],
        json!({"type": "function_call_output", "call_id": "call_2", "output": "line one\nline two"})
    );
}

#[test]
fn adjacent_user_messages_are_merged_but_tool_output_stays_its_own_item() {
    let body = body(&Request {
        messages: vec![
            Message::user("first"),
            Message::user("second"),
            assistant(vec![tool_call("call_1", "todo", json!({}))]),
            Message::tool(vec![Block::tool_result("call_1", "ok", false)]),
            Message::user("third"),
        ],
        ..Request::default()
    });
    assert_eq!(
        body["input"],
        json!([
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "first"}, {"type": "input_text", "text": "second"}],
            },
            {"type": "function_call", "call_id": "call_1", "name": "todo", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "third"}]},
        ])
    );
}

#[test]
fn thinking_level_maps_to_reasoning_effort_with_encrypted_reasoning_included() {
    let build = |reasoning: bool, level: ThinkingLevel| {
        let req = Request {
            messages: vec![Message::user("hi")],
            thinking: level,
            ..Request::default()
        };
        openai_responses::build_body(&provider(), &model(reasoning), &req)
    };
    let include = json!(["reasoning.encrypted_content"]);

    let body = build(true, ThinkingLevel::Low);
    assert_eq!(body["reasoning"], json!({"effort": "low", "summary": "auto"}));
    assert_eq!(body["include"], include);
    assert_eq!(
        build(true, ThinkingLevel::Minimal)["reasoning"]["effort"],
        json!("minimal")
    );
    assert_eq!(
        build(true, ThinkingLevel::Medium)["reasoning"]["effort"],
        json!("medium")
    );
    assert_eq!(
        build(true, ThinkingLevel::Max)["reasoning"]["effort"],
        json!("high")
    );

    // No effort is requested, but a reasoning model still reasons and its items must be replayable.
    let body = build(true, ThinkingLevel::Off);
    assert!(body.get("reasoning").is_none());
    assert_eq!(body["include"], include);

    let body = build(false, ThinkingLevel::High);
    assert!(body.get("reasoning").is_none());
    assert!(body.get("include").is_none());
}

#[test]
fn temperature_is_sent_only_to_models_that_do_not_reason() {
    let temperature = |reasoning: bool| {
        let req = Request {
            messages: vec![Message::user("hi")],
            temperature: Some(0.2),
            ..Request::default()
        };
        openai_responses::build_body(&provider(), &model(reasoning), &req)
            .get("temperature")
            .cloned()
    };
    assert_eq!(temperature(false), Some(json!(0.2)));
    assert_eq!(temperature(true), None);
}

// ---------------------------------------------------------------------------------------------
// HTTP requests, classify_error, parse_models
// ---------------------------------------------------------------------------------------------

#[test]
fn request_targets_responses_with_bearer_auth_and_the_json_body() {
    let client = azazel_ai::tls::client();
    let req = Request {
        messages: vec![Message::user("hi")],
        ..Request::default()
    };
    let build = |auth: &Auth| {
        openai_responses::build_request(&client, &provider(), &model(false), &req, auth)
            .build()
            .unwrap()
    };

    let request = build(&Auth::ApiKey("sk-test".into()));
    assert_eq!(request.method(), reqwest::Method::POST);
    assert_eq!(request.url().as_str(), "http://localhost:9/v1/responses");
    assert_eq!(request.headers()["authorization"], "Bearer sk-test");
    assert_eq!(request.headers()["content-type"], "application/json");
    let sent: Value = serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(sent, body(&req));

    let request = build(&Auth::OAuth {
        access: "token-1".into(),
        fields: BTreeMap::new(),
    });
    assert_eq!(request.headers()["authorization"], "Bearer token-1");

    assert!(build(&Auth::None).headers().get("authorization").is_none());
}

#[test]
fn provider_headers_are_applied_last_and_override() {
    let mut provider = provider();
    provider
        .headers
        .insert("authorization".into(), "Bearer override".into());
    provider
        .headers
        .insert("OpenAI-Organization".into(), "org-mock".into());
    let client = azazel_ai::tls::client();
    let request = openai_responses::build_request(
        &client,
        &provider,
        &model(false),
        &Request::default(),
        &Auth::ApiKey("sk-test".into()),
    )
    .build()
    .unwrap();
    let values: Vec<_> = request.headers().get_all("authorization").iter().collect();
    assert_eq!(values, ["Bearer override"]);
    assert_eq!(request.headers()["openai-organization"], "org-mock");
}

#[test]
fn models_request_is_an_authenticated_get() {
    let client = azazel_ai::tls::client();
    let request = openai_responses::models_request(&client, &provider(), &Auth::ApiKey("sk-test".into()))
        .build()
        .unwrap();
    assert_eq!(request.method(), reqwest::Method::GET);
    assert_eq!(request.url().as_str(), "http://localhost:9/v1/models");
    assert_eq!(request.headers()["authorization"], "Bearer sk-test");
}

#[test]
fn classify_error_recognises_provider_signals_and_defers_the_rest() {
    let cases: [(u16, &str, Option<AiError>); 6] = [
        (
            400,
            r#"{"error":{"message":"Your input exceeds the context window of this model. Please adjust your input and try again.","type":"invalid_request_error","param":"input","code":"context_length_exceeded"}}"#,
            Some(AiError::ContextOverflow),
        ),
        (
            500,
            r#"{"error":{"message":"The server is overloaded. Please try again later.","type":"server_error","param":null,"code":null}}"#,
            Some(AiError::Overloaded),
        ),
        (
            429,
            r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}"#,
            Some(AiError::Auth(
                "You exceeded your current quota, please check your plan and billing details.".into(),
            )),
        ),
        (
            429,
            r#"{"error":{"message":"Rate limit reached for requests.","type":"requests","param":null,"code":"rate_limit_exceeded"}}"#,
            None,
        ),
        (
            400,
            r#"{"error":{"message":"Unsupported parameter: 'temperature' is not supported with this model.","type":"invalid_request_error","param":"temperature","code":"unsupported_parameter"}}"#,
            None,
        ),
        (503, "upstream connect error", None),
    ];
    for (status, body, expected) in cases {
        assert_eq!(
            openai_responses::classify_error(status, body),
            expected,
            "{status} {body}"
        );
    }
}

#[test]
fn parse_models_reads_ids_and_defaults_unreported_fields() {
    let body = json!({
        "object": "list",
        "data": [
            {"id": "mock-large", "object": "model", "created": 1762819200, "owned_by": "system"},
            {"id": "mock-small", "object": "model", "created": 1762819100, "owned_by": "system", "context_window": 400000, "max_output_tokens": 128000},
            {"object": "model"},
        ],
    });
    let models = openai_responses::parse_models(&provider(), &body);
    assert_eq!(models.len(), 2);
    assert_eq!(models[0], ModelInfo::unknown("mock", "mock-large"));
    assert_eq!(
        models[1],
        ModelInfo {
            context_window: 400_000,
            max_output: 128_000,
            ..ModelInfo::unknown("mock", "mock-small")
        }
    );
}

#[test]
fn the_service_tier_is_named_only_when_one_is_asked_for() {
    let tier_of = |tier: Option<Tier>| {
        body(&Request {
            messages: vec![Message::user("hi")],
            tier,
            ..Request::default()
        })
        .get("service_tier")
        .cloned()
    };
    assert_eq!(tier_of(Some(Tier::Priority)), Some(json!("priority")));
    assert_eq!(tier_of(Some(Tier::Economy)), Some(json!("flex")));
    assert_eq!(tier_of(Some(Tier::Standard)), None);
    assert_eq!(tier_of(None), None);
}
