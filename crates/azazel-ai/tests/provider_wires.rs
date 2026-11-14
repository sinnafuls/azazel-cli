#![allow(clippy::unwrap_used)]

//! The wire APIs and model discovery added beside the four original ones, each driven through
//! `HttpProvider` against a server that plays the provider.

use std::collections::BTreeMap;
use std::sync::Arc;

use azazel_ai::providers::http::{HttpProvider, http_client};
use azazel_ai::providers::presets::presets_with;
use azazel_ai::{
    AiError, Block, Cost, Credential, Credentials, Discovery, Message, ModelInfo, Provider, ProviderConfig,
    Request, StopReason, StreamEvent, ThinkingLevel, WireApi,
};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{
    body_partial_json, header, headers, method, path, query_param, query_param_is_missing,
};
use wiremock::{Mock, MockServer, Request as Received, ResponseTemplate};

const CHAT_STREAM: &str = concat!(
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n",
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"id\":\"r1\",\"choices\":[],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":500,\"cost\":0.0125}}\n\n",
    "data: [DONE]\n\n",
);

/// A recorded Responses stream; its text is "Hello! How can I help you today?".
const RESPONSES_STREAM: &str = include_str!("fixtures/openai-responses/text.sse");

/// A provider at `server` with `key` in the environment variable `KEY`.
fn keyed(config: ProviderConfig, dir: &std::path::Path, key: Option<&'static str>) -> HttpProvider {
    let config = ProviderConfig {
        api_key_env: vec!["KEY".into()],
        requires_key: key.is_some(),
        ..config
    };
    let credentials = Credentials::with_env(
        dir.join("auth.json"),
        Arc::new(move |name| (name == "KEY").then_some(key?.to_string())),
    );
    HttpProvider::new(config, http_client(), Arc::new(credentials))
}

fn request() -> Request {
    Request {
        messages: vec![Message::user("hi")],
        ..Request::default()
    }
}

/// A note for a request's tail, and what a provider is sent for `note("Date: today")`.
fn note(text: &str) -> azazel_ai::SystemPart {
    azazel_ai::SystemPart {
        text: text.into(),
        name: "now".into(),
        cache_boundary: false,
    }
}

const NOTE_SENT: &str = "<system-note>\nDate: today\n</system-note>";

async fn finish(provider: &HttpProvider, model: &ModelInfo, req: &Request) -> Message {
    let stream = provider
        .stream(model, req, CancellationToken::new())
        .await
        .unwrap();
    let events: Vec<StreamEvent> = stream.collect().await;
    match events.last() {
        Some(StreamEvent::Done { message }) => message.clone(),
        _ => panic!("{events:?}"),
    }
}

async fn body_of(server: &MockServer, wanted: &str) -> Value {
    let received: Vec<Received> = server.received_requests().await.unwrap();
    let request = received
        .iter()
        .find(|request| request.url.path() == wanted)
        .unwrap_or_else(|| panic!("nothing was sent to {wanted}"));
    serde_json::from_slice(&request.body).unwrap()
}

#[tokio::test]
async fn ollama_chat_streams_json_lines_from_the_native_endpoint() {
    let server = MockServer::start().await;
    let answer = concat!(
        "{\"message\":{\"role\":\"assistant\",\"thinking\":\"Easy.\"},\"done\":false}\n",
        "{\"message\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"done\":false}\n",
        "{\"message\":{\"role\":\"assistant\",\"content\":\"lo\"},\"done\":false}\n",
        "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\",",
        "\"prompt_eval_count\":12,\"eval_count\":4}\n",
    );
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(header("authorization", "Bearer ol-key"))
        .and(body_partial_json(
            json!({"model": "mock:8b", "stream": true, "think": "high"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(answer, "application/x-ndjson"))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    // The address is the one of the OpenAI-compatible endpoint; the native one sits beside it.
    let config = ProviderConfig::new("ollama", WireApi::OllamaChat, format!("{}/v1", server.uri()));
    let provider = keyed(config, dir.path(), Some("ol-key"));
    let model = ModelInfo {
        reasoning: true,
        ..ModelInfo::unknown("ollama", "mock:8b")
    };
    let req = Request {
        thinking: ThinkingLevel::High,
        tail: vec![note("Date: today")],
        ..request()
    };

    let message = finish(&provider, &model, &req).await;
    // The request's notes follow the conversation as one more user message.
    assert_eq!(
        body_of(&server, "/api/chat").await["messages"],
        json!([{"role": "user", "content": "hi"}, {"role": "user", "content": NOTE_SENT}])
    );
    assert_eq!(message.text(), "Hello");
    assert!(matches!(&message.content[0], Block::Thinking { text, .. } if text == "Easy."));
    let meta = message.meta.as_ref().unwrap();
    assert_eq!((meta.api, meta.stop), (WireApi::OllamaChat, StopReason::Stop));
    assert_eq!((meta.usage.input, meta.usage.output), (12, 4));
}

#[tokio::test]
async fn ollama_discovery_asks_each_installed_model_what_it_can_do() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [
            {"name": "mock:8b", "model": "mock:8b"},
            {"name": "mock-embed:latest", "model": "mock-embed:latest"},
            {"name": "broken:latest", "model": "broken:latest"},
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .and(body_partial_json(json!({"model": "mock:8b"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "capabilities": ["completion", "tools", "thinking", "vision"],
            "model_info": {"general.architecture": "mockarch", "mockarch.context_length": 40960},
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .and(body_partial_json(json!({"model": "mock-embed:latest"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "capabilities": ["embedding"],
            "model_info": {"mockbert.context_length": 2048},
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .and(body_partial_json(json!({"model": "broken:latest"})))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    // The built-in `ollama` provider, moved to the test server: chat through the OpenAI-compatible
    // endpoint, models from the native one.
    let mut config = presets_with(&|_| None)
        .into_iter()
        .find(|p| p.id == "ollama")
        .unwrap();
    assert_eq!(config.discovery, Discovery::Ollama);
    config.base_url = format!("{}/v1", server.uri());
    let provider = keyed(config, dir.path(), None);

    let models = provider.list_models().await.unwrap();
    let summary: Vec<(&str, u64, bool, bool)> = models
        .iter()
        .map(|m| (m.id.as_str(), m.context_window, m.vision, m.reasoning))
        .collect();
    assert_eq!(
        summary,
        [
            ("mock:8b", 40_960, true, true),
            // A model that could not be asked about is still offered, with defaults.
            ("broken:latest", 128_000, false, false),
        ]
    );
}

#[tokio::test]
async fn lm_studio_discovery_prefers_the_native_list_and_falls_back_to_the_openai_one() {
    let dir = tempfile::tempdir().unwrap();
    let lm_studio = |server: &MockServer| {
        let mut config = presets_with(&|_| None)
            .into_iter()
            .find(|p| p.id == "lmstudio")
            .unwrap();
        config.base_url = format!("{}/v1", server.uri());
        keyed(config, dir.path(), None)
    };

    let native = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v0/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "mock-vl", "type": "vlm", "state": "loaded",
             "loaded_context_length": 16384, "max_context_length": 131072},
            {"id": "embedder", "type": "embeddings"},
        ]})))
        .mount(&native)
        .await;
    let models = lm_studio(&native).list_models().await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(
        (models[0].id.as_str(), models[0].context_window, models[0].vision),
        ("mock-vl", 16_384, true)
    );

    // A server without the native list still names its models the OpenAI way.
    let plain = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "some-model"}]})))
        .mount(&plain)
        .await;
    let models = lm_studio(&plain).list_models().await.unwrap();
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["some-model"]
    );
}

#[tokio::test]
async fn the_google_model_list_is_read_page_by_page() {
    let server = MockServer::start().await;
    let entry =
        |id: &str| json!({"name": format!("models/{id}"), "supportedGenerationMethods": ["generateContent"]});
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(query_param_is_missing("pageToken"))
        .and(header("x-goog-api-key", "g-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [entry("gemini-a"), entry("gemini-b")],
            "nextPageToken": "page-2",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(query_param("pageToken", "page-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [entry("gemini-c")]})))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config = ProviderConfig::new("google", WireApi::GoogleGenerative, server.uri());
    let provider = keyed(config, dir.path(), Some("g-key"));

    let ids: Vec<String> = provider
        .list_models()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(ids, ["gemini-a", "gemini-b", "gemini-c"]);
}

#[tokio::test]
async fn configured_models_are_listed_beside_discovered_ones_and_alone_when_discovery_is_off() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "found"}, {"id": "both"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let declared = || {
        vec![
            ModelInfo::unknown("acme", "both"),
            ModelInfo::unknown("acme", "configured"),
        ]
    };
    let config = ProviderConfig::new("acme", WireApi::OpenAiChat, server.uri());
    let asked = keyed(config.clone(), dir.path(), None).with_declared(declared());
    let ids: Vec<String> = asked
        .list_models()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(ids, ["found", "both", "configured"]);

    // With discovery off the endpoint is not asked at all (the mock above allows one request).
    let silent = ProviderConfig {
        discovery: Discovery::Off,
        ..config
    };
    let silent = keyed(silent, dir.path(), None).with_declared(declared());
    let ids: Vec<String> = silent
        .list_models()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(ids, ["both", "configured"]);
}

#[tokio::test]
async fn openrouter_gets_its_reasoning_object_routing_and_reports_the_price_itself() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer or-key"))
        .and(header("x-title", "azazel"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CHAT_STREAM, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut config = presets_with(&|_| None)
        .into_iter()
        .find(|p| p.id == "openrouter")
        .unwrap();
    assert_eq!(config.api, WireApi::OpenRouter);
    config.base_url = server.uri();
    config.oauth = None;
    config.extra_body = json!({"provider": {"order": ["anthropic"], "allow_fallbacks": false}})
        .as_object()
        .cloned()
        .unwrap();
    let provider = keyed(config, dir.path(), Some("or-key"));
    let model = ModelInfo {
        reasoning: true,
        // The catalogue's rates would price this response at 0.0105 USD.
        cost: Some(Cost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.0,
            cache_write: 0.0,
        }),
        ..ModelInfo::unknown("openrouter", "anthropic/claude-mock")
    };
    let req = Request {
        thinking: ThinkingLevel::Xhigh,
        tail: vec![note("Date: today")],
        ..request()
    };

    let message = finish(&provider, &model, &req).await;
    let meta = message.meta.as_ref().unwrap();
    assert_eq!(meta.api, WireApi::OpenRouter);
    // What OpenRouter says it charged stands, not the catalogue's estimate.
    assert_eq!(meta.usage.cost_usd, Some(0.0125));

    let body = body_of(&server, "/chat/completions").await;
    assert_eq!(body["reasoning"], json!({"effort": "xhigh"}));
    assert!(body.get("reasoning_effort").is_none());
    assert_eq!(
        body["provider"],
        json!({"order": ["anthropic"], "allow_fallbacks": false})
    );
    // An Anthropic model caches the conversation up to a marked block. The request's notes come
    // after the mark, so the next request, whose notes may differ, still reads what was cached.
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]},
            {"role": "user", "content": NOTE_SENT},
        ])
    );
}

#[tokio::test]
async fn a_plain_chat_provider_sends_no_openrouter_members_and_is_priced_from_the_catalogue() {
    let server = MockServer::start().await;
    let stream = CHAT_STREAM.replace(",\"cost\":0.0125", "");
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config = ProviderConfig::new("acme", WireApi::OpenAiChat, server.uri());
    let provider = keyed(config, dir.path(), None);
    let model = ModelInfo {
        reasoning: true,
        cost: Some(Cost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.0,
            cache_write: 0.0,
        }),
        ..ModelInfo::unknown("acme", "anthropic/claude-mock")
    };
    let req = Request {
        thinking: ThinkingLevel::Xhigh,
        ..request()
    };
    let message = finish(&provider, &model, &req).await;
    assert_eq!(message.meta.as_ref().unwrap().usage.cost_usd, Some(0.0105));
    let body = body_of(&server, "/chat/completions").await;
    assert_eq!(body["reasoning_effort"], "high");
    assert!(body.get("reasoning").is_none() && body.get("provider").is_none());
    assert_eq!(body["messages"][0]["content"], "hi");
}

#[tokio::test]
async fn azure_addresses_the_resource_and_presents_its_key_in_the_api_key_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/openai/v1/responses"))
        .and(header("api-key", "az-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(RESPONSES_STREAM, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/openai/deployments/gpt-mock/chat/completions"))
        .and(query_param("api-version", "2024-10-21"))
        .and(header("api-key", "az-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CHAT_STREAM, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let azure = |api: WireApi| {
        let uri = server.uri();
        let mut config = presets_with(&move |name| (name == "AZURE_OPENAI_BASE_URL").then(|| uri.clone()))
            .into_iter()
            .find(|p| p.id == "azure")
            .unwrap();
        assert_eq!(config.api, WireApi::AzureOpenAiResponses);
        config.api = api;
        keyed(config, dir.path(), Some("az-key"))
    };
    let model = ModelInfo::unknown("azure", "gpt-mock");

    let responses = finish(&azure(WireApi::AzureOpenAiResponses), &model, &request()).await;
    assert_eq!(responses.text(), "Hello! How can I help you today?");
    assert_eq!(
        responses.meta.as_ref().unwrap().api,
        WireApi::AzureOpenAiResponses
    );
    let sent = body_of(&server, "/openai/v1/responses").await;
    assert_eq!(
        (&sent["model"], &sent["store"]),
        (&json!("gpt-mock"), &json!(false))
    );

    let chat = finish(&azure(WireApi::AzureOpenAiChat), &model, &request()).await;
    assert_eq!(chat.text(), "Hello");
    assert_eq!(chat.meta.as_ref().unwrap().api, WireApi::AzureOpenAiChat);
}

#[tokio::test]
async fn a_provider_whose_address_needs_an_account_says_which_variable_to_set() {
    let dir = tempfile::tempdir().unwrap();
    let config = presets_with(&|_| None)
        .into_iter()
        .find(|p| p.id == "cloudflare-ai-gateway")
        .unwrap();
    let provider = keyed(config, dir.path(), Some("cf-key"));
    let error = match provider
        .stream(
            &ModelInfo::unknown("cloudflare-ai-gateway", "openai/gpt-mock"),
            &request(),
            CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("the request should not have been sent"),
        Err(error) => error,
    };
    assert!(
        matches!(&error, AiError::Auth(why) if why.contains("CLOUDFLARE_ACCOUNT_ID")),
        "{error:?}"
    );
}

#[tokio::test]
async fn cloud_code_assist_wraps_the_request_in_the_project_of_the_login() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1internal:loadCodeAssist"))
        .and(header("authorization", "Bearer at-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "currentTier": {"id": "free-tier"},
            "cloudaicompanionProject": "managed-123",
        })))
        // Looked up once for the life of the provider, and kept with the login after that.
        .expect(1)
        .mount(&server)
        .await;
    let chunk = |inner: Value| format!("data: {}\n\n", json!({"response": inner}));
    let answer = [
        chunk(json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "Hel"}]}}]})),
        chunk(json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "lo"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2},
            "responseId": "r-1",
        })),
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/v1internal:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .and(header("authorization", "Bearer at-1"))
        // The matcher reads a comma-separated header as a list of values.
        .and(headers(
            "client-metadata",
            vec![
                "ideType=IDE_UNSPECIFIED",
                "platform=PLATFORM_UNSPECIFIED",
                "pluginType=GEMINI",
            ],
        ))
        .and(body_partial_json(
            json!({"project": "managed-123", "model": "gemini-mock"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(answer, "text/event-stream"))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1internal:retrieveUserQuota"))
        .and(body_partial_json(json!({"project": "managed-123"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"buckets": [
            {"modelId": "gemini-mock", "tokenType": "REQUESTS"},
            {"modelId": "gemini-mock", "tokenType": "TOKENS"},
            {"modelId": "gemini-mock-lite"},
        ]})))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let credentials = Arc::new(Credentials::with_env(
        dir.path().join("auth.json"),
        Arc::new(|_| None),
    ));
    credentials
        .store(
            "google-gemini-cli",
            Credential::OAuth {
                access: "at-1".into(),
                refresh: "rt-1".into(),
                expires: u64::MAX / 4,
                extra: BTreeMap::new(),
            },
        )
        .unwrap();
    let mut config = presets_with(&|_| None)
        .into_iter()
        .find(|p| p.id == "google-gemini-cli")
        .unwrap();
    assert_eq!(config.api, WireApi::GoogleGeminiCli);
    config.base_url = server.uri();
    config.discovery = Discovery::Auto;
    let provider = HttpProvider::new(config.clone(), http_client(), credentials.clone());
    let model = ModelInfo::unknown("google-gemini-cli", "gemini-mock");

    let message = finish(&provider, &model, &request()).await;
    assert_eq!(message.text(), "Hello");
    let meta = message.meta.as_ref().unwrap();
    assert_eq!(meta.api, WireApi::GoogleGeminiCli);
    assert_eq!((meta.usage.input, meta.usage.output), (5, 2));
    let sent = body_of(&server, "/v1internal:streamGenerateContent").await;
    assert_eq!(sent["request"]["contents"][0]["parts"][0]["text"], "hi");

    // The project is now part of the stored login: a new process starts from it.
    let Some(Credential::OAuth { extra, .. }) = credentials.load().remove("google-gemini-cli") else {
        panic!("the login is gone");
    };
    assert_eq!(extra["project_id"], "managed-123");
    let restarted = HttpProvider::new(config, http_client(), credentials);
    assert_eq!(finish(&restarted, &model, &request()).await.text(), "Hello");

    // The models are the ones the account has quota for, each once.
    let ids: Vec<String> = restarted
        .list_models()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(ids, ["gemini-mock", "gemini-mock-lite"]);
}

#[tokio::test]
async fn copilot_is_told_who_started_the_turn_and_whether_it_carries_an_image() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CHAT_STREAM, "text/event-stream"))
        .mount(&server)
        .await;
    let client = azazel_ai::tls::client();
    let copilot = presets_with(&|_| None)
        .into_iter()
        .find(|p| p.id == "github-copilot")
        .unwrap();
    let model = ModelInfo {
        vision: true,
        ..ModelInfo::unknown("github-copilot", "gpt-mock")
    };
    let auth = azazel_ai::Auth::ApiKey("gho_token".into());
    let headers_for = |req: &Request| {
        azazel_ai::wire::openai_chat::build_request(&client, &copilot, &model, req, &auth)
            .build()
            .unwrap()
            .headers()
            .clone()
    };

    let by_user = headers_for(&request());
    assert_eq!(by_user["x-initiator"], "user");
    assert_eq!(by_user["authorization"], "Bearer gho_token");
    assert_eq!(by_user["copilot-integration-id"], "copilot-developer-cli");
    assert!(by_user.get("copilot-vision-request").is_none());

    let after_tool = Request {
        messages: vec![
            Message::user_blocks(vec![
                Block::text("look"),
                Block::Image {
                    mime: "image/png".into(),
                    data: "ZmFrZQ==".into(),
                },
            ]),
            Message::tool(vec![Block::tool_result("call_1", "done", false)]),
        ],
        // The harness's notes are not the user speaking: the agent still started this turn.
        tail: vec![note("Date: today")],
        ..Request::default()
    };
    let by_agent = headers_for(&after_tool);
    assert_eq!(by_agent["x-initiator"], "agent");
    assert_eq!(by_agent["copilot-vision-request"], "true");

    // Any other host gets neither header.
    let plain = ProviderConfig::new("acme", WireApi::OpenAiChat, server.uri());
    let sent = azazel_ai::wire::openai_chat::build_request(&client, &plain, &model, &request(), &auth)
        .build()
        .unwrap();
    assert!(sent.headers().get("x-initiator").is_none());
}

#[tokio::test]
async fn a_key_written_in_the_configuration_is_used_and_a_forced_bearer_replaces_the_native_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer literal-key"))
        .respond_with(ResponseTemplate::new(400).set_body_string("{}"))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config = ProviderConfig {
        api_key: Some("literal-key".into()),
        auth_style: azazel_ai::AuthStyle::Bearer,
        requires_key: true,
        ..ProviderConfig::new("gateway", WireApi::AnthropicMessages, server.uri())
    };
    let credentials = Arc::new(Credentials::with_env(
        dir.path().join("auth.json"),
        Arc::new(|_| None),
    ));
    assert_eq!(
        credentials.source(&config).as_deref(),
        Some("api_key in the configuration")
    );
    let provider = HttpProvider::new(config, http_client(), credentials);
    // The request reaches the server with the configured key as a bearer token (the mock
    // verifies the header); its 400 is what comes back.
    let outcome = provider
        .stream(
            &ModelInfo::unknown("gateway", "claude-mock"),
            &request(),
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(outcome, Err(AiError::InvalidRequest(_))));
}
