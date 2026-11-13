#![allow(clippy::unwrap_used)]

use azazel_ai::wire::{decode_lines, ollama};
use azazel_ai::{
    AiError, AssistantMeta, Auth, Block, Message, ModelInfo, ProviderConfig, Request, StopReason,
    StreamEvent, SystemPart, ThinkingLevel, ToolSpec, Usage, WireApi,
};
use serde_json::{Value, json};

fn provider() -> ProviderConfig {
    ProviderConfig::new("mock", WireApi::OllamaChat, "http://localhost:9")
}

fn model(reasoning: bool) -> ModelInfo {
    ModelInfo {
        vision: true,
        reasoning,
        ..ModelInfo::unknown("mock", "mock-model")
    }
}

fn assistant(content: Vec<Block>) -> Message {
    Message::assistant(
        content,
        AssistantMeta {
            provider: "mock".into(),
            model: "mock-model".into(),
            api: WireApi::OllamaChat,
            usage: Usage::default(),
            stop: StopReason::ToolUse,
            response_id: None,
            duration_ms: 0,
        },
    )
}

fn decode(body: &str) -> Vec<StreamEvent> {
    let mut decoder = ollama::decoder(&provider(), &model(true));
    decode_lines(body.as_bytes(), decoder.as_mut())
}

fn lines(chunks: &[Value]) -> String {
    chunks.iter().map(|chunk| format!("{chunk}\n")).collect()
}

#[test]
fn a_conversation_becomes_the_native_message_shapes() {
    let req = Request {
        system: vec![
            SystemPart {
                text: "You are terse.".into(),
                name: String::new(),
                cache_boundary: false,
            },
            SystemPart {
                text: "Use tools.".into(),
                name: String::new(),
                cache_boundary: true,
            },
        ],
        messages: vec![
            Message::user_blocks(vec![
                Block::text("what is in this picture?"),
                Block::Image {
                    mime: "image/png".into(),
                    data: "ZmFrZQ==".into(),
                },
            ]),
            assistant(vec![
                Block::Thinking {
                    text: "I should look.".into(),
                    signature: None,
                    redacted: false,
                },
                Block::text("Let me check."),
                Block::ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    input: json!({"path": "a.txt"}),
                    signature: None,
                },
            ]),
            Message::tool(vec![Block::tool_result("call_1", "file contents", false)]),
        ],
        tools: vec![ToolSpec {
            name: "read".into(),
            description: "Reads a file".into(),
            schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }],
        max_output: Some(2048),
        temperature: Some(0.2),
        ..Request::default()
    };
    let body = ollama::build_body(&provider(), &model(false), &req);
    assert_eq!(
        body,
        json!({
            "model": "mock-model",
            "messages": [
                {"role": "system", "content": "You are terse.\n\nUse tools."},
                // Images go beside the text as bare base64, not as data URLs.
                {"role": "user", "content": "what is in this picture?", "images": ["ZmFrZQ=="]},
                // Earlier reasoning is not replayed; arguments are an object and calls have no id.
                {"role": "assistant", "content": "Let me check.", "tool_calls": [
                    {"type": "function", "function": {"name": "read", "arguments": {"path": "a.txt"}}}
                ]},
                // A result names the tool it answers.
                {"role": "tool", "content": "file contents", "tool_name": "read"},
            ],
            "tools": [{"type": "function", "function": {
                "name": "read",
                "description": "Reads a file",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
            }}],
            "options": {"num_predict": 2048, "temperature": 0.2},
            "stream": true,
        })
    );
}

#[test]
fn thinking_is_a_named_effort_and_is_switched_off_explicitly() {
    let think = |model: &ModelInfo, level| {
        let req = Request {
            messages: vec![Message::user("hi")],
            thinking: level,
            ..Request::default()
        };
        ollama::build_body(&provider(), model, &req).get("think").cloned()
    };
    let reasoning = model(true);
    assert_eq!(think(&reasoning, ThinkingLevel::Minimal), Some(json!("low")));
    assert_eq!(think(&reasoning, ThinkingLevel::Medium), Some(json!("medium")));
    assert_eq!(think(&reasoning, ThinkingLevel::Xhigh), Some(json!("high")));
    assert_eq!(think(&reasoning, ThinkingLevel::Max), Some(json!("max")));
    // A model that can think does so unless told not to; one that cannot is told nothing.
    assert_eq!(think(&reasoning, ThinkingLevel::Off), Some(json!(false)));
    assert_eq!(think(&model(false), ThinkingLevel::High), None);
    assert_eq!(think(&model(false), ThinkingLevel::Off), None);
}

#[test]
fn the_hosted_service_caps_the_output_and_extra_members_replace_built_ones() {
    let req = Request {
        messages: vec![Message::user("hi")],
        max_output: Some(200_000),
        ..Request::default()
    };
    let mut cloud = ProviderConfig::new("ollama-cloud", WireApi::OllamaChat, "https://ollama.com");
    let body = ollama::build_body(&cloud, &model(false), &req);
    assert_eq!(body["options"]["num_predict"], 65_536);
    let local = ollama::build_body(&provider(), &model(false), &req);
    assert_eq!(local["options"]["num_predict"], 200_000);

    cloud.extra_body = json!({"keep_alive": "10m", "options": {"num_ctx": 32768}})
        .as_object()
        .cloned()
        .unwrap();
    let body = ollama::build_body(&cloud, &model(false), &req);
    assert_eq!(body["keep_alive"], "10m");
    assert_eq!(body["options"], json!({"num_ctx": 32768}));
}

#[test]
fn the_request_goes_to_the_native_endpoint_whatever_suffix_the_address_has() {
    let client = reqwest::Client::new();
    let req = Request {
        messages: vec![Message::user("hi")],
        ..Request::default()
    };
    for base in [
        "http://box:11434",
        "http://box:11434/",
        "http://box:11434/v1",
        "http://box:11434/api",
    ] {
        let provider = ProviderConfig::new("mock", WireApi::OllamaChat, base);
        let sent = ollama::build_request(&client, &provider, &model(false), &req, &Auth::None)
            .build()
            .unwrap();
        assert_eq!(sent.url().as_str(), "http://box:11434/api/chat", "{base}");
        assert!(sent.headers().get("authorization").is_none());
        let listed = ollama::models_request(&client, &provider, &Auth::ApiKey("k".into()))
            .build()
            .unwrap();
        assert_eq!(listed.url().as_str(), "http://box:11434/api/tags", "{base}");
        assert_eq!(listed.headers()["authorization"], "Bearer k");
    }
}

#[test]
fn text_and_thinking_stream_as_blocks_and_usage_arrives_with_done() {
    let events = decode(&lines(&[
        json!({"model": "m", "message": {"role": "assistant", "thinking": "Hm"}, "done": false}),
        json!({"model": "m", "message": {"role": "assistant", "thinking": "m."}, "done": false}),
        json!({"model": "m", "message": {"role": "assistant", "content": "Hel"}, "done": false}),
        json!({"model": "m", "message": {"role": "assistant", "content": "lo"}, "done": false}),
        json!({"model": "m", "message": {"role": "assistant", "content": ""}, "done": true,
               "done_reason": "stop", "prompt_eval_count": 30, "prompt_eval_cached_count": 10,
               "eval_count": 7, "total_duration": 123}),
    ]));
    let thinking = Block::Thinking {
        text: "Hmm.".into(),
        signature: None,
        redacted: false,
    };
    assert_eq!(
        events[..7],
        [
            StreamEvent::Start,
            StreamEvent::ThinkingDelta {
                index: 0,
                delta: "Hm".into()
            },
            StreamEvent::ThinkingDelta {
                index: 0,
                delta: "m.".into()
            },
            StreamEvent::BlockEnd {
                index: 0,
                block: thinking.clone()
            },
            StreamEvent::TextDelta {
                index: 1,
                delta: "Hel".into()
            },
            StreamEvent::TextDelta {
                index: 1,
                delta: "lo".into()
            },
            StreamEvent::BlockEnd {
                index: 1,
                block: Block::text("Hello")
            },
        ]
    );
    let [StreamEvent::Done { message }] = &events[7..] else {
        panic!("{events:?}")
    };
    assert_eq!(message.content, [thinking, Block::text("Hello")]);
    let meta = message.meta.as_ref().unwrap();
    assert_eq!(
        (meta.provider.as_str(), meta.model.as_str(), meta.api, meta.stop),
        ("mock", "mock-model", WireApi::OllamaChat, StopReason::Stop)
    );
    assert_eq!(
        meta.usage,
        Usage {
            input: 20,
            output: 7,
            cache_read: 10,
            ..Usage::default()
        }
    );
}

#[test]
fn tool_calls_arrive_whole_and_get_an_id() {
    let events = decode(&lines(&[
        json!({"message": {"role": "assistant", "content": "Looking.", "tool_calls": [
            {"function": {"name": "read", "arguments": {"path": "a.txt"}}},
            {"function": {"name": "glob", "arguments": "{\"pattern\": \"*.rs\"}"}},
            {"function": {"arguments": {}}},
        ]}, "done": false}),
        json!({"message": {"role": "assistant", "content": ""}, "done": true, "done_reason": "stop"}),
    ]));
    let Some(StreamEvent::Done { message }) = events.last() else {
        panic!("{events:?}")
    };
    let calls: Vec<(&str, &Value)> = message
        .tool_calls()
        .map(|(_, name, input)| (name, input))
        .collect();
    assert_eq!(
        calls,
        [
            ("read", &json!({"path": "a.txt"})),
            ("glob", &json!({"pattern": "*.rs"}))
        ]
    );
    let ids: Vec<&str> = message.tool_calls().map(|(id, _, _)| id).collect();
    assert!(ids.iter().all(|id| !id.is_empty()) && ids[0] != ids[1]);
    // A response with calls stops for them, whatever reason the server gives.
    assert_eq!(message.meta.as_ref().unwrap().stop, StopReason::ToolUse);
    // Each call is announced, carries its arguments once, and ends.
    let starts = events
        .iter()
        .filter(|event| matches!(event, StreamEvent::ToolCallStart { .. }))
        .count();
    assert_eq!(starts, 2);
    assert!(events.contains(&StreamEvent::ToolCallDelta {
        index: 1,
        delta: "{\"path\":\"a.txt\"}".into()
    }));
}

#[test]
fn the_output_limit_is_reported_as_length() {
    let events = decode(&lines(&[
        json!({"message": {"content": "cut"}, "done": false}),
        json!({"message": {"content": ""}, "done": true, "done_reason": "length"}),
    ]));
    let Some(StreamEvent::Done { message }) = events.last() else {
        panic!("{events:?}")
    };
    assert_eq!(message.meta.as_ref().unwrap().stop, StopReason::Length);
}

#[test]
fn failures_inside_the_stream_end_it_with_what_was_received() {
    // An error line after some output.
    let events = decode(&lines(&[
        json!({"message": {"content": "partial"}, "done": false}),
        json!({"error": "model runner has unexpectedly stopped"}),
        json!({"message": {"content": "never read"}, "done": false}),
    ]));
    let Some(StreamEvent::Error { error, partial }) = events.last() else {
        panic!("{events:?}")
    };
    assert!(matches!(error, AiError::Server { status: 500, .. }), "{error:?}");
    assert_eq!(partial.as_ref().unwrap().content, [Block::text("partial")]);

    // A prompt that does not fit is an overflow, so the agent can compact and retry.
    let events = decode(&lines(&[
        json!({"error": "prompt too long; exceeded max context length by 4 tokens"}),
    ]));
    assert_eq!(
        events,
        [
            StreamEvent::Start,
            StreamEvent::Error {
                error: AiError::ContextOverflow,
                partial: None
            }
        ]
    );

    // The server loaded the model and said nothing.
    let events = decode(&lines(&[json!({"done": true, "done_reason": "load"})]));
    assert!(
        matches!(
            events.last(),
            Some(StreamEvent::Error {
                error: AiError::InvalidRequest(_),
                ..
            })
        ),
        "{events:?}"
    );

    // The connection closed before the server said it was done.
    let events = decode(&lines(&[json!({"message": {"content": "half"}, "done": false})]));
    let Some(StreamEvent::Error { error, partial }) = events.last() else {
        panic!("{events:?}")
    };
    assert!(matches!(error, AiError::Network(_)));
    assert_eq!(partial.as_ref().unwrap().content, [Block::text("half")]);
}

#[test]
fn lines_split_across_chunks_and_a_last_line_without_a_newline_are_read() {
    let body = "{\"message\":{\"content\":\"a\u{e9}\"},\"done\":false}\r\n\n{\"message\":{\"content\":\"\"},\"done\":true}";
    let whole = decode(body);
    let Some(StreamEvent::Done { message }) = whole.last() else {
        panic!("{whole:?}")
    };
    assert_eq!(message.text(), "a\u{e9}");
}

#[test]
fn the_tag_list_names_the_installed_models() {
    let body = json!({"models": [
        {"name": "mock:8b", "model": "mock:8b", "size": 5},
        {"name": "only-name"},
        {"size": 1},
    ]});
    let ids: Vec<String> = ollama::parse_models(&provider(), &body)
        .into_iter()
        .map(|model| model.key())
        .collect();
    assert_eq!(ids, ["mock/mock:8b", "mock/only-name"]);
    assert_eq!(
        ollama::classify_error(400, r#"{"error": "prompt too long; exceeded context length"}"#),
        Some(AiError::ContextOverflow)
    );
    assert_eq!(
        ollama::classify_error(404, r#"{"error": "model 'x' not found"}"#),
        None
    );
}
