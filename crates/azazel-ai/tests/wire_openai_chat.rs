#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use azazel_ai::wire::{decode_sse, openai_chat};
use azazel_ai::{
    AiError, AssistantMeta, Auth, Block, Cost, INVALID_JSON_KEY, Message, ModelInfo, ProviderConfig, Request,
    StopReason, StreamEvent, SystemPart, ThinkingLevel, Tier, ToolSpec, Usage, WireApi,
};
use serde_json::{Value, json};

fn provider() -> ProviderConfig {
    ProviderConfig::new("mock", WireApi::OpenAiChat, "http://localhost:9/v1")
}

fn model(reasoning: bool) -> ModelInfo {
    ModelInfo {
        vision: true,
        reasoning,
        ..ModelInfo::unknown("mock", "mock-model")
    }
}

fn body(req: &Request) -> Value {
    openai_chat::build_body(&provider(), &model(false), req)
}

fn assistant(content: Vec<Block>) -> Message {
    Message::assistant(
        content,
        AssistantMeta {
            provider: "mock".into(),
            model: "mock-model".into(),
            api: WireApi::OpenAiChat,
            usage: Usage::default(),
            stop: StopReason::Stop,
            response_id: None,
            duration_ms: 0,
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

fn decode(bytes: &[u8]) -> Vec<StreamEvent> {
    let mut decoder = openai_chat::decoder(&provider(), &model(true));
    decode_sse(bytes, &mut *decoder)
}

/// One chunk carrying `delta` for the first choice.
fn chunk(delta: Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 1762819200,
        "model": "mock-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
    })
}

fn sse(chunks: &[Value], done: bool) -> Vec<u8> {
    let mut out = String::new();
    for chunk in chunks {
        out.push_str(&format!("data: {chunk}\n\n"));
    }
    if done {
        out.push_str("data: [DONE]\n\n");
    }
    out.into_bytes()
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

fn text_deltas(events: &[StreamEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

fn call_deltas(events: &[StreamEvent], wanted: usize) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCallDelta { index, delta } if *index == wanted => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

fn thinking(text: &str) -> Block {
    Block::Thinking {
        text: text.into(),
        signature: None,
        redacted: false,
    }
}

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

#[test]
fn text_fixture() {
    let events = decode(include_bytes!("fixtures/openai-chat/text.sse"));
    insta::assert_snapshot!(serde_json::to_string_pretty(&events).unwrap());

    assert_eq!(events.first(), Some(&StreamEvent::Start));
    let message = done(&events);
    assert_eq!(
        message.content,
        vec![Block::text("Hello! How can I help you today?")]
    );
    assert_eq!(text_deltas(&events), "Hello! How can I help you today?");
    let meta = meta(message);
    assert_eq!(meta.stop, StopReason::Stop);
    assert_eq!(
        meta.usage,
        Usage {
            input: 19,
            output: 9,
            ..Usage::default()
        }
    );
    assert_eq!(
        meta.response_id.as_deref(),
        Some("chatcmpl-B7qVtK2mXw9LpR4sYh1ZcN8e")
    );
    assert_eq!(
        (
            meta.provider.as_str(),
            meta.model.as_str(),
            meta.api,
            meta.duration_ms
        ),
        ("mock", "mock-model", WireApi::OpenAiChat, 0)
    );
}

#[test]
fn tool_call_fixture() {
    let events = decode(include_bytes!("fixtures/openai-chat/tool_call.sse"));
    insta::assert_snapshot!(serde_json::to_string_pretty(&events).unwrap());

    assert_eq!(events.first(), Some(&StreamEvent::Start));
    assert_eq!(
        events[1],
        StreamEvent::ToolCallStart {
            index: 0,
            id: "call_Zx81QmN2vLr7TtWc".into(),
            name: "read".into(),
        }
    );
    assert_eq!(call_deltas(&events, 0), r#"{"path":"src/main.rs","limit":40}"#);
    let call = tool_call(
        "call_Zx81QmN2vLr7TtWc",
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
            input: 156,
            output: 21,
            cache_read: 256,
            ..Usage::default()
        }
    );
}

#[test]
fn thinking_tool_fixture() {
    let events = decode(include_bytes!("fixtures/openai-chat/thinking_tool.sse"));
    insta::assert_snapshot!(serde_json::to_string_pretty(&events).unwrap());

    assert_eq!(events.first(), Some(&StreamEvent::Start));
    let message = done(&events);
    assert_eq!(
        message.content,
        vec![
            thinking("The user wants the tests run. I should call the shell tool."),
            Block::text("I'll run the test suite."),
            tool_call("call_00_T3stRunQ7xYb", "shell", json!({"command": "cargo test"})),
        ]
    );
    // Each block ends before the next one starts streaming.
    let ends: Vec<usize> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::BlockEnd { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(ends, vec![0, 1, 2]);
    assert!(events.contains(&StreamEvent::ToolCallStart {
        index: 2,
        id: "call_00_T3stRunQ7xYb".into(),
        name: "shell".into(),
    }));
    let meta = meta(message);
    assert_eq!(meta.stop, StopReason::ToolUse);
    assert_eq!(
        meta.usage,
        Usage {
            input: 146,
            output: 96,
            cache_read: 384,
            reasoning: 64,
            ..Usage::default()
        }
    );
}

#[test]
fn midstream_error_fixture() {
    let events = decode(include_bytes!("fixtures/openai-chat/midstream_error.sse"));
    insta::assert_snapshot!(serde_json::to_string_pretty(&events).unwrap());

    assert_eq!(events.first(), Some(&StreamEvent::Start));
    let (error, partial) = failure(&events);
    assert_eq!(
        error,
        &AiError::Server {
            status: 500,
            body: "The server had an error while processing your request. Sorry about that!".into(),
        }
    );
    assert!(error.is_retryable());
    let partial = partial.unwrap();
    assert_eq!(partial.content, vec![Block::text("The answer is")]);
    assert_eq!(meta(partial).stop, StopReason::Error);
    // The text never completed, so it has no BlockEnd.
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::BlockEnd { .. }))
    );
}

// ---------------------------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------------------------

#[test]
fn tool_call_without_id_gets_a_generated_unique_id() {
    let chunks = [chunk(
        json!({"role": "assistant", "tool_calls": [{
            "index": 0,
            "type": "function",
            "function": {"name": "glob", "arguments": "{\"pattern\":\"*.rs\"}"},
        }]}),
        Some("tool_calls"),
    )];
    let first = decode(&sse(&chunks, true));
    let second = decode(&sse(&chunks, true));

    let call_of = |events: &[StreamEvent]| {
        let calls: Vec<(String, String, Value)> = done(events)
            .tool_calls()
            .map(|(id, name, input)| (id.to_string(), name.to_string(), input.clone()))
            .collect();
        assert_eq!(calls.len(), 1);
        calls[0].clone()
    };
    let (id, name, input) = call_of(&first);
    assert!(id.starts_with("call_") && id.len() > "call_".len(), "{id}");
    assert_eq!((name.as_str(), &input), ("glob", &json!({"pattern": "*.rs"})));
    assert_ne!(id, call_of(&second).0);

    // The call arrived whole: it is announced with the generated id and its arguments in one delta.
    assert_eq!(
        first[1..3],
        [
            StreamEvent::ToolCallStart {
                index: 0,
                id,
                name: "glob".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                delta: "{\"pattern\":\"*.rs\"}".into(),
            },
        ]
    );
    assert_eq!(meta(done(&first)).stop, StopReason::ToolUse);
}

#[test]
fn stream_closed_without_done_marker_completes_when_a_finish_reason_was_seen() {
    let chunks = [
        chunk(json!({"role": "assistant", "content": "All"}), None),
        chunk(json!({"content": " good."}), Some("stop")),
    ];
    let events = decode(&sse(&chunks, false));
    let message = done(&events);
    assert_eq!(message.text(), "All good.");
    assert_eq!(meta(message).stop, StopReason::Stop);
    assert!(events.contains(&StreamEvent::BlockEnd {
        index: 0,
        block: Block::text("All good."),
    }));
}

#[test]
fn stream_cut_before_a_finish_reason_is_a_network_error_with_the_partial_text() {
    let chunks = [chunk(json!({"role": "assistant", "content": "All"}), None)];
    let events = decode(&sse(&chunks, false));
    assert_eq!(events.first(), Some(&StreamEvent::Start));
    let (error, partial) = failure(&events);
    assert_eq!(
        error,
        &AiError::Network("stream ended before the response was complete".into())
    );
    assert_eq!(partial.unwrap().content, vec![Block::text("All")]);

    // Nothing at all arrived: still `Start` first, and no partial message.
    let events = decode(b"");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], StreamEvent::Start);
    assert!(matches!(failure(&events), (AiError::Network(_), None)));
}

#[test]
fn reasoning_is_read_from_either_field_name() {
    for field in ["reasoning_content", "reasoning"] {
        let chunks = [
            chunk(json!({"role": "assistant", "content": "", field: "Let me"}), None),
            chunk(json!({"content": null, field: " think."}), None),
            chunk(json!({"content": "Done."}), None),
            chunk(json!({}), Some("stop")),
        ];
        let events = decode(&sse(&chunks, true));
        assert_eq!(
            done(&events).content,
            vec![thinking("Let me think."), Block::text("Done.")],
            "{field}"
        );
        assert!(events.contains(&StreamEvent::ThinkingDelta {
            index: 0,
            delta: "Let me".into(),
        }));
    }
}

#[test]
fn reasoning_sent_under_both_field_names_is_not_duplicated() {
    let chunks = [
        chunk(json!({"reasoning_content": "Once.", "reasoning": "Once."}), None),
        chunk(json!({"content": "ok"}), Some("stop")),
    ];
    let events = decode(&sse(&chunks, true));
    assert_eq!(done(&events).content, vec![thinking("Once."), Block::text("ok")]);
}

#[test]
fn typed_content_parts_are_split_into_thinking_and_text() {
    let chunks = [
        chunk(
            json!({"content": [{"type": "thinking", "thinking": [{"type": "text", "text": "Hmm."}]}]}),
            None,
        ),
        chunk(
            json!({"content": [{"type": "text", "text": "Yes."}]}),
            Some("stop"),
        ),
    ];
    let events = decode(&sse(&chunks, true));
    assert_eq!(done(&events).content, vec![thinking("Hmm."), Block::text("Yes.")]);
}

#[test]
fn finish_reason_length_maps_to_length() {
    let chunks = [chunk(json!({"content": "Once upon a"}), Some("length"))];
    let events = decode(&sse(&chunks, true));
    assert_eq!(meta(done(&events)).stop, StopReason::Length);
    assert_eq!(done(&events).text(), "Once upon a");
}

#[test]
fn parallel_tool_calls_are_merged_by_index_even_when_interleaved() {
    let chunks = [
        chunk(
            json!({"tool_calls": [
                {"index": 0, "id": "call_a", "type": "function", "function": {"name": "read", "arguments": ""}},
                {"index": 1, "id": "call_b", "type": "function", "function": {"name": "glob", "arguments": ""}},
            ]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [{"index": 1, "function": {"arguments": "{\"pattern\":"}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"path\":\"a\"}"}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [{"index": 1, "function": {"arguments": "\"*\"}"}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ];
    let events = decode(&sse(&chunks, true));
    assert_eq!(
        done(&events).content,
        vec![
            tool_call("call_a", "read", json!({"path": "a"})),
            tool_call("call_b", "glob", json!({"pattern": "*"})),
        ]
    );
    assert_eq!(call_deltas(&events, 1), "{\"pattern\":\"*\"}");
}

#[test]
fn a_new_id_on_a_reused_index_starts_another_call() {
    let whole = |id: &str, path: &str| {
        chunk(
            json!({"tool_calls": [{
                "index": 0,
                "id": id,
                "type": "function",
                "function": {"name": "read", "arguments": json!({"path": path}).to_string()},
            }]}),
            None,
        )
    };
    let chunks = [
        whole("call_a", "a"),
        whole("call_b", "b"),
        chunk(json!({}), Some("tool_calls")),
    ];
    let events = decode(&sse(&chunks, true));
    assert_eq!(
        done(&events).content,
        vec![
            tool_call("call_a", "read", json!({"path": "a"})),
            tool_call("call_b", "read", json!({"path": "b"})),
        ]
    );
}

#[test]
fn empty_arguments_become_an_empty_object_and_broken_json_is_reported() {
    let chunks = [chunk(
        json!({"tool_calls": [
            {"index": 0, "id": "call_a", "function": {"name": "todo", "arguments": ""}},
            {"index": 1, "id": "call_b", "function": {"name": "read", "arguments": "{\"path\":"}},
        ]}),
        Some("tool_calls"),
    )];
    let events = decode(&sse(&chunks, true));
    assert_eq!(
        done(&events).content,
        vec![
            tool_call("call_a", "todo", json!({})),
            tool_call("call_b", "read", json!({ INVALID_JSON_KEY: "{\"path\":" })),
        ]
    );
}

#[test]
fn malformed_chunks_are_skipped() {
    let mut bytes = b"data: {\"id\":\"chatcmpl-test\",\"choices\":[{\"delta\":{\"cont\n\n".to_vec();
    bytes.extend(sse(&[chunk(json!({"content": "ok"}), Some("stop"))], true));
    let events = decode(&bytes);
    assert_eq!(events.first(), Some(&StreamEvent::Start));
    assert_eq!(done(&events).text(), "ok");
}

#[test]
fn nothing_is_returned_after_a_terminal_event() {
    let mut decoder = openai_chat::decoder(&provider(), &model(false));
    let events = decoder.feed(None, "[DONE]");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], StreamEvent::Start);
    assert!(done(&events).content.is_empty());
    assert!(
        decoder
            .feed(None, &chunk(json!({"content": "late"}), None).to_string())
            .is_empty()
    );
    assert!(decoder.finish().is_empty());
}

#[test]
fn in_band_errors_are_classified() {
    let cases = [
        (
            json!({"error": {"code": 429, "message": "Rate limit exceeded", "metadata": {"error_type": "rate_limit_exceeded"}}}),
            AiError::RateLimit { retry_after: None },
        ),
        (
            json!({"error": {"code": 502, "message": "Upstream is busy", "metadata": {"error_type": "provider_overloaded"}}}),
            AiError::Overloaded,
        ),
        (
            json!({"error": {"code": 502, "message": "Provider disconnected", "metadata": {"error_type": "provider_unavailable"}}}),
            AiError::Server {
                status: 502,
                body: "Provider disconnected".into(),
            },
        ),
        (
            json!({"error": {"code": 401, "message": "User not found."}}),
            AiError::Auth("User not found.".into()),
        ),
        (
            json!({"error": {
                "message": "This model's maximum context length is 8192 tokens. However, your messages resulted in 9001 tokens.",
                "type": "invalid_request_error",
                "code": "context_length_exceeded",
            }}),
            AiError::ContextOverflow,
        ),
        (
            json!({"error": {"message": "Output blocked by the content filter", "type": "content_policy_violation"}}),
            AiError::InvalidRequest("Output blocked by the content filter".into()),
        ),
        (
            json!({"error": "model runner has unexpectedly stopped"}),
            AiError::Server {
                status: 500,
                body: "model runner has unexpectedly stopped".into(),
            },
        ),
    ];
    for (payload, expected) in cases {
        let chunks = [chunk(json!({"content": "Half an"}), None), payload];
        let events = decode(&sse(&chunks, true));
        let (error, partial) = failure(&events);
        assert_eq!(error, &expected);
        assert_eq!(partial.unwrap().content, vec![Block::text("Half an")]);
    }
}

#[test]
fn finish_reason_error_without_an_error_object_fails_the_response() {
    let chunks = [chunk(json!({"content": ""}), Some("error"))];
    let events = decode(&sse(&chunks, true));
    let (error, partial) = failure(&events);
    assert!(matches!(error, AiError::Server { status: 500, .. }));
    assert!(partial.is_none());
}

#[test]
fn an_unfinished_tool_call_is_left_out_of_the_partial_message() {
    let chunks = [
        chunk(json!({"content": "Reading."}), None),
        chunk(
            json!({"tool_calls": [{"index": 0, "id": "call_a", "function": {"name": "read", "arguments": "{\"pa"}}]}),
            None,
        ),
    ];
    let events = decode(&sse(&chunks, false));
    let (_, partial) = failure(&events);
    assert_eq!(partial.unwrap().content, vec![Block::text("Reading.")]);
}

#[test]
fn usage_is_read_from_whichever_chunk_carries_it() {
    let usage_of = |usage: Value| {
        let mut finish = chunk(json!({"content": "x"}), Some("stop"));
        finish["usage"] = usage;
        meta(done(&decode(&sse(&[finish], true)))).usage
    };
    // Cache writes are reported inside `prompt_tokens`, like cache reads.
    assert_eq!(
        usage_of(json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 600, "cache_write_tokens": 300},
        })),
        Usage {
            input: 100,
            output: 50,
            cache_read: 600,
            cache_write: 300,
            ..Usage::default()
        }
    );
    // Servers without the details object report the cache hit at the top level, or not at all.
    assert_eq!(
        usage_of(json!({"prompt_tokens": 80, "completion_tokens": 7, "prompt_cache_hit_tokens": 64})),
        Usage {
            input: 16,
            output: 7,
            cache_read: 64,
            ..Usage::default()
        }
    );
    assert_eq!(
        usage_of(json!({"prompt_tokens": 12, "completion_tokens": 3})),
        Usage {
            input: 12,
            output: 3,
            ..Usage::default()
        }
    );
}

// ---------------------------------------------------------------------------------------------
// build_body
// ---------------------------------------------------------------------------------------------

#[test]
fn body_has_the_streaming_basics_and_omits_optional_fields() {
    let body = body(&Request {
        messages: vec![Message::user("hi")],
        ..Request::default()
    });
    assert_eq!(body["model"], json!("mock-model"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(body["messages"], json!([{"role": "user", "content": "hi"}]));
    for absent in ["tools", "max_tokens", "reasoning_effort", "temperature"] {
        assert!(body.get(absent).is_none(), "{absent}");
    }
}

#[test]
fn system_parts_become_one_leading_system_message() {
    let body = body(&Request {
        system: vec![
            SystemPart {
                text: "You are terse.".into(),
                name: String::new(),
                cache_boundary: false,
            },
            SystemPart {
                text: "  ".into(),
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
        ..Request::default()
    });
    assert_eq!(
        body["messages"],
        json!([
            {"role": "system", "content": "You are terse.\n\nProject: demo."},
            {"role": "user", "content": "hi"},
        ])
    );
}

#[test]
fn output_limit_is_sent_only_when_requested() {
    let body = body(&Request {
        messages: vec![Message::user("hi")],
        max_output: Some(1234),
        ..Request::default()
    });
    assert_eq!(body["max_tokens"], json!(1234));
}

#[test]
fn tools_are_sent_as_function_tools() {
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
            "function": {"name": "read", "description": "Read a file.", "parameters": schema},
        }])
    );
}

#[test]
fn tool_calls_and_results_are_replayed_and_reasoning_is_dropped() {
    let body = body(&Request {
        messages: vec![
            Message::user("list files"),
            assistant(vec![
                Block::Thinking {
                    text: "hidden reasoning".into(),
                    signature: Some("sig".into()),
                    redacted: false,
                },
                Block::Opaque {
                    provider: "mock".into(),
                    item: json!({"type": "reasoning", "encrypted_content": "opaque-item"}),
                },
                Block::text("Looking."),
                tool_call("call_1", "glob", json!({"pattern": "*.rs"})),
                tool_call("call_2", "read", json!({"path": "a.rs"})),
            ]),
            Message::tool(vec![
                Block::tool_result("call_1", "a.rs\nb.rs", false),
                Block::tool_result("call_2", "no such file", true),
            ]),
            assistant(vec![Block::text("Only b.rs is readable.")]),
        ],
        ..Request::default()
    });
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "list files"},
            {
                "role": "assistant",
                "content": "Looking.",
                "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "glob", "arguments": "{\"pattern\":\"*.rs\"}"}},
                    {"id": "call_2", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"a.rs\"}"}},
                ],
            },
            {"role": "tool", "tool_call_id": "call_1", "content": "a.rs\nb.rs"},
            {"role": "tool", "tool_call_id": "call_2", "content": "no such file"},
            {"role": "assistant", "content": "Only b.rs is readable."},
        ])
    );
    let sent = body.to_string();
    assert!(!sent.contains("hidden reasoning") && !sent.contains("opaque-item"));
}

#[test]
fn assistant_turn_with_only_tool_calls_has_null_content_and_empty_turns_are_skipped() {
    let body = body(&Request {
        messages: vec![
            Message::user("go"),
            assistant(vec![Block::Thinking {
                text: "only reasoning".into(),
                signature: None,
                redacted: false,
            }]),
            assistant(vec![tool_call("call_1", "todo", json!({}))]),
            Message::tool(vec![Block::tool_result("call_1", "ok", false)]),
        ],
        ..Request::default()
    });
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["content"], Value::Null);
    assert_eq!(messages[1]["tool_calls"][0]["function"]["arguments"], json!("{}"));
}

#[test]
fn user_images_become_data_url_parts() {
    let body = body(&Request {
        messages: vec![Message::user_blocks(vec![
            Block::text("what is this?"),
            Block::Image {
                mime: "image/png".into(),
                data: "AAAA".into(),
            },
        ])],
        ..Request::default()
    });
    assert_eq!(
        body["messages"],
        json!([{
            "role": "user",
            "content": [
                {"type": "text", "text": "what is this?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
            ],
        }])
    );
}

#[test]
fn tool_result_images_follow_in_a_user_message_merged_with_the_next_user_turn() {
    let body = body(&Request {
        messages: vec![
            Message::user("take a screenshot"),
            assistant(vec![tool_call("call_9", "screenshot", json!({}))]),
            Message::tool(vec![Block::ToolResult {
                call_id: "call_9".into(),
                content: vec![Block::Image {
                    mime: "image/jpeg".into(),
                    data: "BBBB".into(),
                }],
                is_error: false,
            }]),
            Message::user("what does it show?"),
        ],
        ..Request::default()
    });
    let messages = body["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "tool", "user"]);

    // The tool message itself is text only and never empty.
    assert_eq!(messages[2]["tool_call_id"], json!("call_9"));
    assert!(!messages[2]["content"].as_str().unwrap().is_empty());

    let parts = messages[3]["content"].as_array().unwrap();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0]["type"], json!("text"));
    assert_eq!(
        parts[1],
        json!({"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,BBBB"}})
    );
    assert_eq!(parts[2], json!({"type": "text", "text": "what does it show?"}));
}

#[test]
fn adjacent_user_messages_are_merged_into_one_turn() {
    let body = body(&Request {
        messages: vec![Message::user("first"), Message::user("second")],
        ..Request::default()
    });
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": "first\n\nsecond"}])
    );
}

#[test]
fn thinking_level_maps_to_reasoning_effort() {
    let effort = |reasoning: bool, level: ThinkingLevel| {
        let req = Request {
            messages: vec![Message::user("hi")],
            thinking: level,
            ..Request::default()
        };
        openai_chat::build_body(&provider(), &model(reasoning), &req)
            .get("reasoning_effort")
            .cloned()
    };
    assert_eq!(effort(true, ThinkingLevel::Minimal), Some(json!("minimal")));
    assert_eq!(effort(true, ThinkingLevel::Low), Some(json!("low")));
    assert_eq!(effort(true, ThinkingLevel::Medium), Some(json!("medium")));
    assert_eq!(effort(true, ThinkingLevel::Max), Some(json!("high")));
    assert_eq!(effort(true, ThinkingLevel::Off), None);
    assert_eq!(effort(false, ThinkingLevel::High), None);
}

#[test]
fn temperature_is_sent_unless_reasoning_is_requested() {
    let temperature = |level: ThinkingLevel| {
        let req = Request {
            messages: vec![Message::user("hi")],
            thinking: level,
            temperature: Some(0.2),
            ..Request::default()
        };
        openai_chat::build_body(&provider(), &model(true), &req)
            .get("temperature")
            .cloned()
    };
    assert_eq!(temperature(ThinkingLevel::Off), Some(json!(0.2)));
    assert_eq!(temperature(ThinkingLevel::High), None);
}

// ---------------------------------------------------------------------------------------------
// HTTP requests
// ---------------------------------------------------------------------------------------------

#[test]
fn request_targets_chat_completions_with_bearer_auth_and_the_json_body() {
    let client = reqwest::Client::new();
    let req = Request {
        messages: vec![Message::user("hi")],
        ..Request::default()
    };
    let build = |auth: &Auth| {
        openai_chat::build_request(&client, &provider(), &model(false), &req, auth)
            .build()
            .unwrap()
    };

    let request = build(&Auth::ApiKey("sk-test".into()));
    assert_eq!(request.method(), reqwest::Method::POST);
    assert_eq!(request.url().as_str(), "http://localhost:9/v1/chat/completions");
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
        .insert("Authorization".into(), "Bearer override".into());
    provider.headers.insert("X-Title".into(), "azazel".into());
    let client = reqwest::Client::new();
    let request = openai_chat::build_request(
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
    assert_eq!(request.headers()["x-title"], "azazel");
}

#[test]
fn models_request_is_an_authenticated_get() {
    let client = reqwest::Client::new();
    let request = openai_chat::models_request(&client, &provider(), &Auth::ApiKey("sk-test".into()))
        .build()
        .unwrap();
    assert_eq!(request.method(), reqwest::Method::GET);
    assert_eq!(request.url().as_str(), "http://localhost:9/v1/models");
    assert_eq!(request.headers()["authorization"], "Bearer sk-test");
}

// ---------------------------------------------------------------------------------------------
// classify_error and parse_models
// ---------------------------------------------------------------------------------------------

#[test]
fn classify_error_recognises_provider_signals_and_defers_the_rest() {
    let cases: [(u16, &str, Option<AiError>); 12] = [
        (
            400,
            r#"{"error":{"message":"This model's maximum context length is 128000 tokens. However, your messages resulted in 130210 tokens.","type":"invalid_request_error","param":"messages","code":"context_length_exceeded"}}"#,
            Some(AiError::ContextOverflow),
        ),
        (
            400,
            r#"{"error":{"message":"Please reduce the length of the messages or completion.","type":"invalid_request_error","code":"context_length_exceeded"}}"#,
            Some(AiError::ContextOverflow),
        ),
        (
            400,
            r#"{"code":"Client specified an invalid argument","error":"This model's maximum prompt length is 131072 but the request contains 537812 tokens."}"#,
            Some(AiError::ContextOverflow),
        ),
        (
            400,
            r#"{"error":{"code":400,"message":"the request exceeds the available context size, try increasing it","type":"exceed_context_size_error","n_prompt_tokens":9000,"n_ctx":4096}}"#,
            Some(AiError::ContextOverflow),
        ),
        (
            500,
            r#"{"error":{"message":"The engine is currently overloaded, please try again later","type":"server_error"}}"#,
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
            402,
            r#"{"error":{"code":402,"message":"Insufficient credits. Add more to continue."}}"#,
            Some(AiError::Auth(
                "Insufficient credits. Add more to continue.".into(),
            )),
        ),
        (
            429,
            r#"{"error":{"message":"Rate limit reached for requests","type":"requests","code":"rate_limit_exceeded"}}"#,
            None,
        ),
        (
            401,
            r#"{"error":{"message":"Incorrect API key provided. Keys are scoped to a context window policy.","type":"invalid_request_error","code":"invalid_api_key"}}"#,
            None,
        ),
        (
            400,
            r#"{"error":{"message":"Unsupported value: 'temperature' does not support 0.2 with this model.","type":"invalid_request_error","param":"temperature","code":"unsupported_value"}}"#,
            None,
        ),
        (502, "<html><body>Bad Gateway</body></html>", None),
        (500, "", None),
    ];
    for (status, body, expected) in cases {
        assert_eq!(
            openai_chat::classify_error(status, body),
            expected,
            "{status} {body}"
        );
    }
}

#[test]
fn a_used_up_chatgpt_plan_is_a_quota_error_with_its_reset_time() {
    // The ChatGPT backend's answer to a subscription whose usage window is full.
    let body = r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"team","resets_at":1787286694,"eligible_promo":null,"resets_in_seconds":2608165}}"#;
    assert_eq!(
        openai_chat::classify_error(429, body),
        Some(AiError::Quota {
            resets_at: Some(1_787_286_694),
            message: "The usage limit has been reached".into(),
        })
    );
    // Only the time left is given, or the time as text.
    let body =
        r#"{"error":{"type":"usage_limit_reached","message":"Limit reached.","resets_in_seconds":"600"}}"#;
    let Some(AiError::Quota {
        resets_at: Some(at), ..
    }) = openai_chat::classify_error(429, body)
    else {
        panic!("not a quota error");
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!((now + 590..=now + 610).contains(&at), "{at} vs {now}");
    let body = r#"{"error":{"type":"usage_limit_reached","message":"Limit reached."}}"#;
    assert_eq!(
        openai_chat::classify_error(429, body),
        Some(AiError::Quota {
            resets_at: None,
            message: "Limit reached.".into(),
        })
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

#[test]
fn parse_models_fills_reported_fields_and_defaults_the_rest() {
    let body = json!({
        "object": "list",
        "data": [
            {"id": "mock-plain", "object": "model", "created": 1762819200, "owned_by": "mock"},
            {
                "id": "vendor/mock-rich",
                "context_length": 200000,
                "architecture": {"input_modalities": ["text", "image"], "output_modalities": ["text"]},
                "pricing": {"prompt": "0.000003", "completion": "0.000015", "input_cache_read": "0.0000003", "input_cache_write": "0.00000375"},
                "supported_parameters": ["tools", "reasoning", "include_reasoning", "max_tokens"],
                "top_provider": {"context_length": 180000, "max_completion_tokens": 64000, "is_moderated": false},
            },
            {"id": "mock-window", "context_window": 131072, "max_completion_tokens": 32768, "active": true},
            {"id": "mock-vision", "max_context_length": 40000, "capabilities": {"vision": true, "function_calling": true}},
            {"id": "mock-dynamic", "pricing": {"prompt": "-1", "completion": "-1"}},
            {"object": "model"},
            {"id": ""},
        ],
    });
    let models = openai_chat::parse_models(&provider(), &body);
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "mock-plain",
            "vendor/mock-rich",
            "mock-window",
            "mock-vision",
            "mock-dynamic"
        ]
    );
    assert!(models.iter().all(|m| m.provider == "mock"));

    assert_eq!(models[0], ModelInfo::unknown("mock", "mock-plain"));
    assert_eq!(
        models[1],
        ModelInfo {
            context_window: 200_000,
            max_output: 64_000,
            vision: true,
            reasoning: true,
            cost: Some(Cost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            }),
            ..ModelInfo::unknown("mock", "vendor/mock-rich")
        }
    );
    assert_eq!(
        (models[2].context_window, models[2].max_output),
        (131_072, 32_768)
    );
    assert_eq!((models[3].context_window, models[3].vision), (40_000, true));
    assert_eq!(models[3].max_output, ModelInfo::unknown("mock", "x").max_output);
    assert_eq!(models[4].cost, None);
}

#[test]
fn parse_models_reads_the_shapes_other_servers_use_for_the_same_facts() {
    let body = json!({"data": [
        // vLLM: the window is `max_model_len`.
        {"id": "served", "max_model_len": 32768},
        // llama.cpp: the window the model runs with, before the one it was trained with.
        {"id": "local.gguf", "meta": {"n_ctx": 8192, "n_ctx_train": 131072}},
        // GitHub Copilot.
        {"id": "copilot-model", "name": "Copilot Model", "capabilities": {
            "limits": {"max_context_window_tokens": 128000, "max_output_tokens": 16384},
            "supports": {"vision": true, "tool_calls": true},
        }},
        // A gateway that mirrors models.dev.
        {"id": "mirrored", "modalities": {"input": ["text", "image"], "output": ["text"]},
         "capabilities": {"reasoning": {"supported": true}},
         "limit": {"context": 200000, "output": 32000}},
        // Capabilities as a list of names, and an output limit larger than the window.
        {"id": "listed", "display_name": "Listed", "context_window": 1000,
         "max_output_tokens": 5000, "capabilities": ["chat", "reasoning", "vision"]},
        // `max_tokens` beside a window is the output limit.
        {"id": "gatewayed", "context_length": 1048576, "max_tokens": 65536,
         "capabilities": {"reasoning": true}},
    ]});
    let models = openai_chat::parse_models(&provider(), &body);
    let facts: Vec<(&str, u64, u32, bool, bool)> = models
        .iter()
        .map(|m| {
            (
                m.id.as_str(),
                m.context_window,
                m.max_output,
                m.vision,
                m.reasoning,
            )
        })
        .collect();
    assert_eq!(
        facts,
        [
            ("served", 32_768, 8192, false, false),
            ("local.gguf", 8192, 8192, false, false),
            ("copilot-model", 128_000, 16_384, true, false),
            ("mirrored", 200_000, 32_000, true, true),
            ("listed", 1000, 1000, true, true),
            ("gatewayed", 1_048_576, 65_536, false, true),
        ]
    );
    assert_eq!(models[2].name.as_deref(), Some("Copilot Model"));
    assert_eq!(models[4].name.as_deref(), Some("Listed"));
    // A name that only repeats the id is not kept.
    let same = openai_chat::parse_models(&provider(), &json!([{"id": "x", "name": "x"}]));
    assert_eq!(same[0].name, None);
}

#[test]
fn parse_models_accepts_a_bare_array_and_a_models_key() {
    let entries = json!([{"id": "mock-a"}, {"id": "mock-b"}]);
    for body in [entries.clone(), json!({"models": entries})] {
        let ids: Vec<String> = openai_chat::parse_models(&provider(), &body)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, ["mock-a", "mock-b"]);
    }
    assert!(openai_chat::parse_models(&provider(), &json!({"error": "nope"})).is_empty());
}
