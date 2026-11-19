use std::time::Duration;

use azazel_ai::providers::presets::presets;
use azazel_ai::wire::{decode_sse, google};
use azazel_ai::{
    AiError, AssistantMeta, Auth, Block, Message, ModelInfo, ProviderConfig, Request, StopReason,
    StreamEvent, SystemPart, ThinkingLevel, ToolSpec, Usage, WireApi,
};
use serde_json::{Value, json};

const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

fn provider() -> ProviderConfig {
    let mut provider = presets()
        .into_iter()
        .find(|p| p.api == WireApi::GoogleGenerative)
        .unwrap();
    provider.id = "mock".into();
    provider.base_url = "https://mock.invalid/v1beta".into();
    provider
}

/// `mock-model` gets the budget-era request shapes; ids of the form `gemini-<major>-…` select the
/// shapes of that model generation.
fn model_named(id: &str) -> ModelInfo {
    ModelInfo {
        reasoning: true,
        vision: true,
        max_output: 65_536,
        ..ModelInfo::unknown("mock", id)
    }
}

fn model() -> ModelInfo {
    model_named("mock-model")
}

fn request(messages: Vec<Message>) -> Request {
    Request {
        messages,
        ..Request::default()
    }
}

fn thinking_request(level: ThinkingLevel) -> Request {
    Request {
        thinking: level,
        ..request(vec![Message::user("hi")])
    }
}

fn body(model: &ModelInfo, req: &Request) -> Value {
    google::build_body(&provider(), model, req)
}

fn assistant(content: Vec<Block>, model: &str) -> Message {
    Message::assistant(
        content,
        AssistantMeta {
            provider: "mock".into(),
            model: model.into(),
            api: WireApi::GoogleGenerative,
            usage: Usage::default(),
            stop: StopReason::ToolUse,
            response_id: None,
            duration_ms: 0,
            sources: Vec::new(),
        },
    )
}

fn call(id: &str, name: &str, input: Value, signature: Option<&str>) -> Block {
    Block::ToolCall {
        id: id.into(),
        name: name.into(),
        input,
        signature: signature.map(str::to_string),
    }
}

fn image(mime: &str) -> Block {
    Block::Image {
        mime: mime.into(),
        data: "aW1hZ2U=".into(),
    }
}

fn inline(mime: &str) -> Value {
    json!({"inlineData": {"mimeType": mime, "data": "aW1hZ2U="}})
}

fn decode(bytes: &[u8]) -> Vec<StreamEvent> {
    let mut decoder = google::decoder(&provider(), &model());
    decode_sse(bytes, &mut *decoder)
}

/// Feeds each chunk as one SSE record, then ends the stream.
fn decode_chunks(chunks: &[Value]) -> Vec<StreamEvent> {
    let mut decoder = google::decoder(&provider(), &model());
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(decoder.feed(None, &chunk.to_string()));
    }
    events.extend(decoder.finish());
    events
}

fn chunk(parts: Value, finish: Option<&str>) -> Value {
    let mut candidate = json!({"content": {"role": "model", "parts": parts}, "index": 0});
    if let Some(reason) = finish {
        candidate["finishReason"] = reason.into();
    }
    json!({"candidates": [candidate]})
}

fn done(events: &[StreamEvent]) -> &Message {
    match events.last().unwrap() {
        StreamEvent::Done { message } => message,
        other => panic!("expected Done, got {other:?}"),
    }
}

fn failure(events: &[StreamEvent]) -> (&AiError, Option<&Message>) {
    match events.last().unwrap() {
        StreamEvent::Error { error, partial } => (error, partial.as_ref()),
        other => panic!("expected Error, got {other:?}"),
    }
}

fn terminal_count(events: &[StreamEvent]) -> usize {
    events.iter().filter(|e| e.is_terminal()).count()
}

// ---- decoder: fixtures ----

#[test]
fn thinking_tool_fixture_keeps_block_order_signature_and_usage() {
    let events = decode(include_bytes!("../fixtures/google-generative/thinking_tool.sse"));
    insta::assert_snapshot!(serde_json::to_string_pretty(&events).unwrap());

    assert_eq!(events[0], StreamEvent::Start);
    assert_eq!(terminal_count(&events), 1);
    let thinking_deltas = events
        .iter()
        .filter(|e| matches!(e, StreamEvent::ThinkingDelta { index: 0, .. }))
        .count();
    assert_eq!(thinking_deltas, 2);

    let message = done(&events);
    assert_eq!(
        message.content,
        vec![
            Block::Thinking {
                text: "**Locating the timeout default**\n\nThe user wants to know where the request timeout \
                       is set. I should search the config module before answering."
                    .into(),
                signature: None,
                redacted: false,
            },
            Block::text("Let me search the config module."),
            call(
                "fc_4mT9qX2a",
                "grep",
                json!({"pattern": "timeout", "path": "src/config"}),
                Some("CiQB0e2Kb3lYc2lnbmF0dXJlLW9wYXF1ZS1ieXRlcw=="),
            ),
        ]
    );
    // Every block is closed, in index order, before the terminal event.
    let ended: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::BlockEnd { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(ended, [0, 1, 2]);

    let meta = message.meta.as_ref().unwrap();
    assert_eq!(meta.stop, StopReason::ToolUse);
    assert_eq!(
        meta.usage,
        Usage {
            input: 40,
            output: 94,
            cache_read: 200,
            cache_write: 0,
            reasoning: 64,
            cost_usd: None,
            cache_ttl_secs: None,
        }
    );
}

// ---- decoder: behaviour not covered by the fixtures ----

#[test]
fn part_kind_changes_start_new_blocks_and_thought_signature_is_kept() {
    let events = decode_chunks(&[
        chunk(json!([{"text": "one"}, {"text": " two"}]), None),
        chunk(
            json!([{"text": "pondering", "thought": true, "thoughtSignature": "dGhvdWdodA=="}]),
            None,
        ),
        chunk(json!([{"text": "three"}]), Some("STOP")),
    ]);
    assert_eq!(
        done(&events).content,
        vec![
            Block::text("one two"),
            Block::Thinking {
                text: "pondering".into(),
                signature: Some("dGhvdWdodA==".into()),
                redacted: false,
            },
            Block::text("three"),
        ]
    );
    assert!(events.contains(&StreamEvent::ThinkingDelta {
        index: 1,
        delta: "pondering".into()
    }));
    assert!(events.contains(&StreamEvent::TextDelta {
        index: 2,
        delta: "three".into()
    }));
}

#[test]
fn blocking_finish_reason_is_an_error_with_the_partial_message() {
    let events = decode_chunks(&[
        chunk(json!([{"text": "so far"}]), None),
        json!({"candidates": [{"finishReason": "SAFETY", "index": 0}]}),
    ]);
    let (error, partial) = failure(&events);
    assert_eq!(error, &AiError::InvalidRequest("response blocked: SAFETY".into()));
    assert_eq!(partial.unwrap().text(), "so far");

    let events = decode_chunks(&[chunk(json!([]), Some("MALFORMED_FUNCTION_CALL"))]);
    let (error, partial) = failure(&events);
    assert_eq!(
        error,
        &AiError::InvalidRequest("response blocked: MALFORMED_FUNCTION_CALL".into())
    );
    assert!(partial.is_none());
}

#[test]
fn stream_closed_without_finish_reason_is_a_network_error() {
    let events = decode_chunks(&[chunk(json!([{"text": "half an ans"}]), None)]);
    let (error, partial) = failure(&events);
    assert_eq!(
        error,
        &AiError::Network("stream ended before the response was complete".into())
    );
    assert_eq!(partial.unwrap().text(), "half an ans");

    // An empty body still produces `Start` followed by the terminal event.
    let events = decode(b"");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], StreamEvent::Start);
    assert!(matches!(failure(&events), (AiError::Network(_), None)));
}

#[test]
fn in_band_error_objects_are_classified() {
    let classify = |error: Value| {
        let events = decode_chunks(&[json!({ "error": error })]);
        failure(&events).0.clone()
    };
    assert_eq!(
        classify(json!({"code": 429, "status": "RESOURCE_EXHAUSTED", "message": "quota"})),
        AiError::RateLimit { retry_after: None }
    );
    assert_eq!(
        classify(json!({"status": "UNAUTHENTICATED", "message": "token expired"})),
        AiError::Auth("token expired".into())
    );
    assert_eq!(
        classify(json!({
            "code": 400,
            "status": "INVALID_ARGUMENT",
            "message": "The input token count (1250000) exceeds the maximum number of tokens allowed (1048576)."
        })),
        AiError::ContextOverflow
    );
    assert_eq!(
        classify(json!({"code": 400, "status": "INVALID_ARGUMENT", "message": "bad part"})),
        AiError::Server {
            status: 400,
            body: "bad part".into()
        }
    );
    assert_eq!(
        classify(json!({"code": 500, "status": "INTERNAL", "message": "boom"})),
        AiError::Server {
            status: 500,
            body: "boom".into()
        }
    );
}

#[test]
fn tool_call_ids_are_unique_without_api_ids() {
    let ids = |events: &[StreamEvent]| -> Vec<String> {
        done(events)
            .tool_calls()
            .map(|(id, _, _)| id.to_string())
            .collect()
    };
    let two_calls = json!([
        {"functionCall": {"name": "read", "args": {"path": "a"}}},
        {"functionCall": {"name": "read"}}
    ]);

    // Numbered within the response when the chunk has a response id.
    let mut with_response_id = chunk(two_calls, Some("STOP"));
    with_response_id["responseId"] = "resp7".into();
    let events = decode_chunks(&[with_response_id]);
    assert_eq!(ids(&events), ["call_resp7_0", "call_resp7_1"]);
    // Missing arguments become an empty object.
    let inputs: Vec<&Value> = done(&events).tool_calls().map(|(_, _, input)| input).collect();
    assert_eq!(inputs, [&json!({"path": "a"}), &json!({})]);

    // Process-wide counter otherwise: two responses never share an id.
    let bare = chunk(
        json!([{"functionCall": {"name": "read", "args": {}}}]),
        Some("STOP"),
    );
    let first = ids(&decode_chunks(std::slice::from_ref(&bare)));
    let second = ids(&decode_chunks(std::slice::from_ref(&bare)));
    assert!(first[0].starts_with("call_") && second[0].starts_with("call_"));
    assert_ne!(first, second);
}

// ---- build_body ----

#[test]
fn the_notes_of_a_request_follow_the_conversation_and_stay_out_of_the_system_instruction() {
    let mut req = request(vec![Message::user("hi")]);
    req.tail = vec![SystemPart {
        text: "Date: today".into(),
        name: "now".into(),
        cache_boundary: false,
    }];
    let body = body(&model(), &req);
    assert!(body.get("systemInstruction").is_none());
    assert_eq!(
        body["contents"],
        json!([{"role": "user", "parts": [
            {"text": "hi"},
            {"text": "<system-note>\nDate: today\n</system-note>"},
        ]}])
    );
}

#[test]
fn system_parts_go_to_system_instruction_and_empty_ones_are_dropped() {
    let mut req = request(vec![Message::user("hi")]);
    assert!(body(&model(), &req).get("systemInstruction").is_none());

    req.system = vec![
        SystemPart {
            text: "You are helpful.".into(),
            name: String::new(),
            cache_boundary: false,
        },
        SystemPart {
            text: String::new(),
            name: String::new(),
            cache_boundary: false,
        },
        SystemPart {
            text: "Environment: test".into(),
            name: String::new(),
            cache_boundary: true,
        },
    ];
    let body = body(&model(), &req);
    assert_eq!(
        body["systemInstruction"],
        json!({"parts": [{"text": "You are helpful."}, {"text": "Environment: test"}]})
    );
    assert_eq!(
        body["contents"],
        json!([{"role": "user", "parts": [{"text": "hi"}]}])
    );
}

#[test]
fn tools_are_sent_as_function_declarations_with_raw_json_schema() {
    let mut req = request(vec![Message::user("hi")]);
    assert!(body(&model(), &req).get("tools").is_none());

    req.tools = vec![
        ToolSpec {
            name: "read".into(),
            description: "Read a file".into(),
            schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"path": {"type": "string"}, "mode": {"anyOf": [{"const": "text"}, {"type": "null"}]}},
                "required": ["path"],
                "additionalProperties": false
            }),
            defer: false,
        },
        ToolSpec {
            name: "status".into(),
            description: "No arguments".into(),
            schema: Value::Null,
            defer: false,
        },
    ];
    let body = body(&model(), &req);
    assert_eq!(
        body["tools"],
        json!([{"functionDeclarations": [
            {
                "name": "read",
                "description": "Read a file",
                "parametersJsonSchema": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}, "mode": {"anyOf": [{"const": "text"}, {"type": "null"}]}},
                    "required": ["path"],
                    "additionalProperties": false
                }
            },
            {
                "name": "status",
                "description": "No arguments",
                "parametersJsonSchema": {"type": "object", "properties": {}}
            }
        ]}])
    );
}

#[test]
fn signed_tool_call_and_its_result_are_replayed() {
    let req = request(vec![
        Message::user("find the timeout"),
        assistant(
            vec![
                Block::Thinking {
                    text: "private".into(),
                    signature: Some("dGhpbms=".into()),
                    redacted: false,
                },
                Block::text("Searching."),
                call(
                    "fc_1",
                    "grep",
                    json!({"pattern": "timeout"}),
                    Some("c2lnbmF0dXJl"),
                ),
                Block::Opaque {
                    provider: "mock".into(),
                    item: json!({"native": true}),
                },
            ],
            "mock-model",
        ),
        Message::tool(vec![Block::tool_result("fc_1", "src/config.rs:12", false)]),
    ]);
    assert_eq!(
        body(&model(), &req)["contents"],
        json!([
            {"role": "user", "parts": [{"text": "find the timeout"}]},
            {"role": "model", "parts": [
                {"text": "Searching."},
                {
                    "functionCall": {"id": "fc_1", "name": "grep", "args": {"pattern": "timeout"}},
                    "thoughtSignature": "c2lnbmF0dXJl"
                }
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"id": "fc_1", "name": "grep", "response": {"output": "src/config.rs:12"}}}
            ]}
        ])
    );
}

#[test]
fn tool_errors_use_the_error_key_and_text_blocks_are_joined() {
    let req = request(vec![
        Message::user("go"),
        assistant(
            vec![call("c1", "shell", json!({"command": "false"}), None)],
            "mock-model",
        ),
        Message::tool(vec![Block::ToolResult {
            call_id: "c1".into(),
            content: vec![Block::text("exit code: 1"), Block::text("stderr: nope")],
            is_error: true,
        }]),
    ]);
    let body = body(&model(), &req);
    assert_eq!(
        body["contents"][2]["parts"][0]["functionResponse"],
        json!({"id": "c1", "name": "shell", "response": {"error": "exit code: 1\nstderr: nope"}})
    );
    // An unsigned call stays unsigned for a model that does not validate signatures.
    assert!(body["contents"][1]["parts"][0].get("thoughtSignature").is_none());
}

#[test]
fn signature_from_another_model_is_not_replayed() {
    let history = |origin: &str| {
        request(vec![
            Message::user("go"),
            assistant(
                vec![
                    call("c1", "read", json!({"path": "a"}), Some("Zm9yZWlnbg==")),
                    call("c2", "read", json!({"path": "b"}), None),
                ],
                origin,
            ),
            Message::tool(vec![
                Block::tool_result("c1", "A", false),
                Block::tool_result("c2", "B", false),
            ]),
        ])
    };
    let signatures = |model: &ModelInfo, req: &Request| -> Vec<Value> {
        body(model, req)["contents"][1]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|part| part.get("thoughtSignature").cloned().unwrap_or(Value::Null))
            .collect()
    };

    // Budget-era target: the foreign signature is dropped and nothing replaces it.
    assert_eq!(
        signatures(&model(), &history("other-model")),
        [Value::Null, Value::Null]
    );
    // Signature-validating target: the first call of the turn gets the documented placeholder.
    let modern = model_named("gemini-3-mock");
    assert_eq!(
        signatures(&modern, &history("other-model")),
        [json!(SKIP_SIGNATURE), Value::Null]
    );
    // Same model: the real signature goes back exactly where it came from.
    assert_eq!(
        signatures(&modern, &history("gemini-3-mock")),
        [json!("Zm9yZWlnbg=="), Value::Null]
    );
}

fn screenshot_history() -> Request {
    request(vec![
        Message::user("look"),
        assistant(
            vec![
                call("c1", "screenshot", json!({}), None),
                call("c2", "read", json!({"path": "a"}), None),
            ],
            "mock-model",
        ),
        Message::tool(vec![
            Block::ToolResult {
                call_id: "c1".into(),
                content: vec![image("image/png"), image("image/gif")],
                is_error: false,
            },
            Block::tool_result("c2", "A", false),
        ]),
    ])
}

#[test]
fn tool_result_images_are_nested_for_models_with_multimodal_function_responses() {
    let body = body(&model_named("gemini-3-mock"), &screenshot_history());
    assert_eq!(
        body["contents"][2],
        json!({"role": "user", "parts": [
            {"functionResponse": {
                "id": "c1",
                "name": "screenshot",
                "response": {"output": "see attached image"},
                "parts": [inline("image/png")]
            }},
            {"functionResponse": {"id": "c2", "name": "read", "response": {"output": "A"}}},
            // Not an accepted nested type, so it stays a plain part.
            inline("image/gif")
        ]})
    );
}

#[test]
fn adjacent_messages_with_the_same_wire_role_are_coalesced() {
    let req = request(vec![
        Message::user("first"),
        assistant(vec![call("c1", "read", json!({"path": "a"}), None)], "mock-model"),
        Message::tool(vec![Block::tool_result("c1", "A", false)]),
        Message::user("and now?"),
        // Nothing sendable: dropped, so its neighbours merge too.
        assistant(
            vec![Block::Thinking {
                text: "hm".into(),
                signature: None,
                redacted: false,
            }],
            "mock-model",
        ),
        Message::user_blocks(vec![Block::text(""), Block::text("still there?")]),
    ]);
    let body = body(&model(), &req);
    let contents = body["contents"].as_array().unwrap();
    let roles: Vec<&str> = contents.iter().map(|c| c["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "model", "user"]);
    assert_eq!(
        contents[2]["parts"],
        json!([
            {"functionResponse": {"id": "c1", "name": "read", "response": {"output": "A"}}},
            {"text": "and now?"},
            {"text": "still there?"}
        ])
    );
}

#[test]
fn thinking_off_sends_no_thinking_config() {
    assert!(
        body(&model(), &thinking_request(ThinkingLevel::Off))
            .get("generationConfig")
            .is_none()
    );

    let plain = ModelInfo {
        reasoning: false,
        ..model()
    };
    assert!(
        body(&plain, &thinking_request(ThinkingLevel::High))
            .get("generationConfig")
            .is_none()
    );
}

#[test]
fn budget_models_get_a_thinking_budget_within_the_documented_range() {
    let config = |id: &str, level: ThinkingLevel| {
        body(&model_named(id), &thinking_request(level))["generationConfig"]["thinkingConfig"].clone()
    };
    assert_eq!(
        config("mock-model", ThinkingLevel::High),
        json!({"thinkingBudget": 24_000, "includeThoughts": true})
    );
    // An unrecognised model gets the budget as computed.
    assert_eq!(
        config("mock-model", ThinkingLevel::Xhigh)["thinkingBudget"],
        48_000
    );
    assert_eq!(
        config("gemini-2.5-pro-mock", ThinkingLevel::Xhigh)["thinkingBudget"],
        32_768
    );
    assert_eq!(
        config("gemini-2.5-pro-mock", ThinkingLevel::Medium)["thinkingBudget"],
        10_000
    );
    assert_eq!(
        config("gemini-2.5-flash-mock", ThinkingLevel::Max)["thinkingBudget"],
        24_576
    );
    assert_eq!(
        config("gemini-2.5-flash-mock", ThinkingLevel::Minimal)["thinkingBudget"],
        1024
    );
}

#[test]
fn thinking_budget_stays_below_a_requested_output_limit() {
    let req = Request {
        max_output: Some(8000),
        ..thinking_request(ThinkingLevel::High)
    };
    assert_eq!(
        body(&model(), &req)["generationConfig"],
        json!({
            "maxOutputTokens": 8000,
            "thinkingConfig": {"thinkingBudget": 6976, "includeThoughts": true}
        })
    );
}

#[test]
fn level_models_get_a_thinking_level_instead_of_a_budget() {
    let modern = model_named("gemini-3-mock");
    let cases = [
        (ThinkingLevel::Minimal, "LOW"),
        (ThinkingLevel::Low, "LOW"),
        (ThinkingLevel::Medium, "MEDIUM"),
        (ThinkingLevel::High, "HIGH"),
        (ThinkingLevel::Xhigh, "HIGH"),
        (ThinkingLevel::Max, "HIGH"),
    ];
    for (level, expected) in cases {
        assert_eq!(
            body(&modern, &thinking_request(level))["generationConfig"]["thinkingConfig"],
            json!({"thinkingLevel": expected, "includeThoughts": true}),
            "{level}"
        );
    }
    // The version is read from the last path segment of the id.
    let body = body(
        &model_named("models/gemini-4-mock"),
        &thinking_request(ThinkingLevel::Low),
    );
    assert_eq!(body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "LOW");
}

#[test]
fn effort_parameter_maps_to_dynamic_budget_or_level() {
    // Only an effort-based `api` makes `thinking_param` return an effort name.
    let mut effort_provider = provider();
    effort_provider.api = WireApi::OpenAiChat;
    let req = thinking_request(ThinkingLevel::High);
    let config = |id: &str| {
        let body = google::build_body(&effort_provider, &model_named(id), &req);
        body["generationConfig"]["thinkingConfig"].clone()
    };
    assert_eq!(
        config("mock-model"),
        json!({"thinkingBudget": -1, "includeThoughts": true})
    );
    assert_eq!(
        config("gemini-3-mock"),
        json!({"thinkingLevel": "HIGH", "includeThoughts": true})
    );
}

// ---- build_request / models_request ----

#[test]
fn build_request_targets_the_stream_endpoint_with_the_api_key_header() {
    let client = azazel_ai::tls::client();
    let req = request(vec![Message::user("hi")]);
    let built = google::build_request(
        &client,
        &provider(),
        &model(),
        &req,
        &Auth::ApiKey("k-123".into()),
    )
    .build()
    .unwrap();

    assert_eq!(built.method(), reqwest::Method::POST);
    assert_eq!(
        built.url().as_str(),
        "https://mock.invalid/v1beta/models/mock-model:streamGenerateContent?alt=sse"
    );
    assert_eq!(built.headers()["x-goog-api-key"], "k-123");
    assert!(built.headers().get("authorization").is_none());
    assert_eq!(built.headers()["content-type"], "application/json");
    let sent: Value = serde_json::from_slice(built.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(sent, body(&model(), &req));
}

// ---- classify_error / parse_models ----

fn error_body(code: u16, status: &str, message: &str, details: Value) -> String {
    json!({"error": {"code": code, "message": message, "status": status, "details": details}}).to_string()
}

#[test]
fn classify_error_reads_retry_delay_only_for_rate_limits() {
    let retry_info = json!([
        {"@type": "type.googleapis.com/google.rpc.QuotaFailure", "violations": []},
        {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "32.5s"}
    ]);
    // Quota messages mention token counts but are not a context overflow.
    let message = "Quota exceeded for metric input token count; the request exceeds the maximum number of \
                   tokens per minute.";
    let body = error_body(429, "RESOURCE_EXHAUSTED", message, retry_info.clone());
    assert_eq!(
        google::classify_error(429, &body),
        Some(AiError::RateLimit {
            retry_after: Some(Duration::from_millis(32_500))
        })
    );
    // Without a delay the generic 429 mapping applies (it can use the HTTP header).
    let body = error_body(
        429,
        "RESOURCE_EXHAUSTED",
        "Resource has been exhausted",
        json!([]),
    );
    assert_eq!(google::classify_error(429, &body), None);
    // A depleted balance shares the status name but must not be retried.
    let body = error_body(
        402,
        "RESOURCE_EXHAUSTED",
        "Your credit balance is depleted.",
        retry_info,
    );
    assert_eq!(google::classify_error(402, &body), None);
}

#[test]
fn classify_error_maps_rejected_key_and_overload() {
    let message = "API key not valid. Please pass a valid API key.";
    let details = json!([{
        "@type": "type.googleapis.com/google.rpc.ErrorInfo",
        "reason": "API_KEY_INVALID",
        "domain": "googleapis.com"
    }]);
    assert_eq!(
        google::classify_error(400, &error_body(400, "INVALID_ARGUMENT", message, details)),
        Some(AiError::Auth(message.into()))
    );
    let unavailable = error_body(503, "UNAVAILABLE", "The service is unavailable.", json!([]));
    assert_eq!(
        google::classify_error(503, &unavailable),
        Some(AiError::Overloaded)
    );
    let overloaded = error_body(500, "INTERNAL", "The model is overloaded.", json!([]));
    assert_eq!(
        google::classify_error(500, &overloaded),
        Some(AiError::Overloaded)
    );
}

#[test]
fn parse_models_keeps_generative_models_and_fills_reported_limits() {
    let listing = json!({
        "models": [
            {
                "name": "models/mock-pro",
                "displayName": "Mock Pro",
                "inputTokenLimit": 1_048_576,
                "outputTokenLimit": 65_536,
                "supportedGenerationMethods": ["generateContent", "countTokens"],
                "thinking": true
            },
            {
                "name": "models/mock-embedding",
                "inputTokenLimit": 2048,
                "outputTokenLimit": 1,
                "supportedGenerationMethods": ["embedContent"]
            },
            {"name": "models/mock-lite", "supportedGenerationMethods": ["generateContent"]},
            {"name": "models/gemini-mock-flash", "displayName": "gemini-mock-flash",
             "supportedGenerationMethods": ["streamGenerateContent"]},
            {"displayName": "nameless", "supportedGenerationMethods": ["generateContent"]},
            {"name": "models/mock-no-methods"}
        ],
        "nextPageToken": "next"
    });
    let models = google::parse_models(&provider(), &listing);
    assert_eq!(
        models,
        vec![
            ModelInfo {
                name: Some("Mock Pro".into()),
                context_window: 1_048_576,
                max_output: 65_536,
                reasoning: true,
                ..ModelInfo::unknown("mock", "mock-pro")
            },
            ModelInfo::unknown("mock", "mock-lite"),
            // The list does not say which models take images; the Gemini family does.
            ModelInfo {
                vision: true,
                ..ModelInfo::unknown("mock", "gemini-mock-flash")
            },
        ]
    );
    assert!(google::parse_models(&provider(), &json!({"error": {"code": 403}})).is_empty());

    // The list is paged: the token of one page asks for the next.
    assert_eq!(google::next_page_token(&listing), Some("next"));
    assert_eq!(google::next_page_token(&json!({"models": []})), None);
    let client = azazel_ai::tls::client();
    let next = google::models_page_request(&client, &provider(), &Auth::None, Some("a b/c"))
        .build()
        .unwrap();
    assert_eq!(next.url().query(), Some("pageSize=1000&pageToken=a%20b%2Fc"));
}
