//! A web search run by the provider itself (`HostedTool::WebSearch`): what each wire API is sent
//! and which pages are read out of its answer, driven through `HttpProvider` against a server
//! that plays the provider.

use std::sync::Arc;

use azazel_ai::providers::http::{HttpProvider, http_client};
use azazel_ai::providers::presets::presets_with;
use azazel_ai::wire::hosted_api;
use azazel_ai::{
    AiError, Credentials, HostedTool, Message, ModelInfo, Provider, ProviderConfig, Request, StreamEvent,
    WebSource, WireApi,
};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A provider at `config`'s address with the key `k` in its environment.
fn keyed(config: ProviderConfig, dir: &std::path::Path) -> HttpProvider {
    let config = ProviderConfig {
        api_key_env: vec!["KEY".into()],
        requires_key: true,
        ..config
    };
    let credentials = Credentials::with_env(
        dir.join("auth.json"),
        Arc::new(|name| (name == "KEY").then(|| "k".to_string())),
    );
    HttpProvider::new(config, http_client(), Arc::new(credentials))
}

fn search(domains: &[&str], max_uses: Option<u32>) -> Request {
    Request {
        messages: vec![Message::user("Query: who won")],
        hosted: vec![HostedTool::WebSearch {
            domains: domains.iter().map(|domain| domain.to_string()).collect(),
            max_uses,
        }],
        ..Request::default()
    }
}

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

/// The JSON body of the one request `server` received.
async fn sent(server: &MockServer) -> Value {
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "{received:?}");
    serde_json::from_slice(&received[0].body).unwrap()
}

fn sse(records: &[Value]) -> ResponseTemplate {
    let body: String = records
        .iter()
        .map(|record| format!("data: {record}\n\n"))
        .collect();
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn page(url: &str, title: &str, snippet: &str) -> WebSource {
    WebSource {
        url: url.to_string(),
        title: title.to_string(),
        snippet: snippet.to_string(),
    }
}

fn sources(message: &Message) -> &[WebSource] {
    &message.meta.as_ref().unwrap().sources
}

/// Where `mark` sits in `text`, in characters, as the Responses API counts.
fn span(text: &str, mark: &str) -> (usize, usize) {
    let start = text[..text.find(mark).unwrap()].chars().count();
    (start, start + mark.chars().count())
}

#[tokio::test]
async fn anthropic_gets_the_server_tool_and_its_results_and_citations_become_the_pages() {
    let server = MockServer::start().await;
    let quoted = "Claude Elwood Shannon (April 30, 1916 – February 24, 2001) was an American mathematician";
    let wikipedia = "https://en.wikipedia.org/wiki/Claude_Shannon";
    let records = [
        json!({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": 12}}}),
        json!({"type": "content_block_start", "index": 0, "content_block":
            {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search"}}),
        json!({"type": "content_block_delta", "index": 0, "delta":
            {"type": "input_json_delta", "partial_json": "{\"query\":\"claude shannon birth date\"}"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {
        "type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [
            {"type": "web_search_result", "url": "https://www.britannica.com/biography/Claude-Shannon",
             "title": "Claude Shannon | Britannica", "encrypted_content": "Abc"},
            {"type": "web_search_result", "url": wikipedia, "title": "Claude Shannon - Wikipedia",
             "encrypted_content": "Eqgf", "page_age": "April 30, 2025"},
        ]}}),
        json!({"type": "content_block_stop", "index": 1}),
        // A search that failed carries an error where the results would be.
        json!({"type": "content_block_start", "index": 2, "content_block": {
            "type": "web_search_tool_result", "tool_use_id": "srvtoolu_2",
            "content": {"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}}}),
        json!({"type": "content_block_stop", "index": 2}),
        json!({"type": "content_block_start", "index": 3, "content_block":
            {"type": "text", "text": "", "citations": []}}),
        json!({"type": "content_block_delta", "index": 3, "delta": {"type": "citations_delta", "citation": {
            "type": "web_search_result_location", "url": wikipedia, "title": "Claude Shannon - Wikipedia",
            "encrypted_index": "Eo8B", "cited_text": quoted}}}),
        json!({"type": "content_block_delta", "index": 3, "delta":
            {"type": "text_delta", "text": "Claude Shannon was born on April 30, 1916."}}),
        json!({"type": "content_block_stop", "index": 3}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 40}}),
        json!({"type": "message_stop"}),
    ];
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "k"))
        .respond_with(sse(&records))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config = ProviderConfig::new("anthropic", WireApi::AnthropicMessages, server.uri());
    let provider = keyed(config, dir.path());
    assert!(provider.hosts_web_search());

    let model = ModelInfo::unknown("anthropic", "claude-opus-5-5");
    let message = finish(&provider, &model, &search(&["wikipedia.org"], Some(2))).await;
    assert_eq!(
        sent(&server).await["tools"],
        json!([{
            "type": "web_search_20250305",
            "name": "web_search",
            "max_uses": 2,
            "allowed_domains": ["wikipedia.org"],
        }])
    );
    assert_eq!(message.text(), "Claude Shannon was born on April 30, 1916.");
    // The page the answer quotes comes first, with the quote; the other result follows bare.
    assert_eq!(
        sources(&message),
        [
            page(wikipedia, "Claude Shannon - Wikipedia", quoted),
            page(
                "https://www.britannica.com/biography/Claude-Shannon",
                "Claude Shannon | Britannica",
                ""
            ),
        ]
    );
}

#[tokio::test]
async fn the_responses_api_gets_the_web_search_tool_and_its_url_citations_become_the_pages() {
    let server = MockServer::start().await;
    let blog = "([tokio.rs](https://tokio.rs/blog/announcing-1-48))";
    let releases = "([github.com](https://github.com/tokio-rs/tokio/releases))";
    let text = format!(
        "Tokio 1.48 came out in October 2025 – a small release. {blog}\n\nIts changelog lists the fixes.\n{releases}"
    );
    let citation = |mark: &str, url: &str, title: &str| {
        let (start, end) = span(&text, mark);
        json!({"type": "url_citation", "start_index": start, "end_index": end, "url": url, "title": title})
    };
    let annotations = json!([
        citation(
            blog,
            "https://tokio.rs/blog/announcing-1-48",
            "Announcing Tokio 1.48"
        ),
        citation(
            releases,
            "https://github.com/tokio-rs/tokio/releases",
            "Releases · tokio-rs/tokio"
        ),
    ]);
    let records = [
        json!({"type": "response.created", "response": {"id": "resp_1"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item":
            {"type": "web_search_call", "id": "ws_1", "status": "in_progress"}}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": {
            "type": "web_search_call", "id": "ws_1", "status": "completed",
            "action": {"type": "search", "query": "tokio latest release"}}}),
        json!({"type": "response.output_item.added", "output_index": 1, "item":
            {"type": "message", "id": "msg_1", "role": "assistant", "content": []}}),
        json!({"type": "response.output_text.delta", "output_index": 1, "content_index": 0, "delta": text}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": {
            "type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": annotations}]}}),
        json!({"type": "response.completed", "response":
            {"id": "resp_1", "usage": {"input_tokens": 100, "output_tokens": 20}}}),
    ];
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer k"))
        .respond_with(sse(&records))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config = ProviderConfig::new("openai", WireApi::OpenAiResponses, format!("{}/v1", server.uri()));
    let provider = keyed(config, dir.path());
    assert!(provider.hosts_web_search());

    let model = ModelInfo::unknown("openai", "gpt-6-astra");
    let message = finish(&provider, &model, &search(&[], Some(2))).await;
    // No limit on searches to pass on, and no filter without a domain.
    assert_eq!(sent(&server).await["tools"], json!([{"type": "web_search"}]));
    assert_eq!(message.text(), text);
    // Each page carries the paragraph that cites it, without the citation marks; a citation on
    // a line of its own belongs to the paragraph above.
    assert_eq!(
        sources(&message),
        [
            page(
                "https://tokio.rs/blog/announcing-1-48",
                "Announcing Tokio 1.48",
                "Tokio 1.48 came out in October 2025 – a small release."
            ),
            page(
                "https://github.com/tokio-rs/tokio/releases",
                "Releases · tokio-rs/tokio",
                "Its changelog lists the fixes."
            ),
        ]
    );
}

#[tokio::test]
async fn xai_is_searched_on_its_responses_endpoint_though_it_converses_in_chat_completions() {
    let server = MockServer::start().await;
    let text =
        "xAI was founded in March 2023.[[1]](https://x.ai/company) Grok is its model.[[2]](https://x.ai/)";
    let citation = |mark: &str, url: &str, number: &str| {
        let (start, end) = span(text, mark);
        json!({"type": "url_citation", "start_index": start, "end_index": end, "url": url, "title": number})
    };
    let annotations = json!([
        citation("[[1]](https://x.ai/company)", "https://x.ai/company", "1"),
        citation("[[2]](https://x.ai/)", "https://x.ai/", "2"),
    ]);
    let records = [
        json!({"type": "response.output_item.done", "output_index": 0, "item": {
            "type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": annotations}]}}),
        json!({"type": "response.completed", "response": {
            "id": "r1", "citations": ["https://x.ai/company", "https://en.wikipedia.org/wiki/XAI_(company)"]}}),
    ];
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer k"))
        .respond_with(sse(&records))
        .mount(&server)
        .await;
    let chat = concat!(
        "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(chat, "text/event-stream"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config = ProviderConfig::new("xai", WireApi::OpenAiChat, format!("{}/v1", server.uri()));
    let provider = keyed(config, dir.path());
    assert!(provider.hosts_web_search());
    let model = ModelInfo::unknown("xai", "grok-4.7");

    let message = finish(&provider, &model, &search(&["x.ai"], None)).await;
    let received = server.received_requests().await.unwrap();
    assert_eq!(received[0].url.path(), "/v1/responses");
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(
        body["tools"],
        json!([{"type": "web_search", "filters": {"allowed_domains": ["x.ai"]}}])
    );
    // A citation's number is no title; the page the search opened and the answer did not cite
    // comes last.
    assert_eq!(
        sources(&message),
        [
            page(
                "https://x.ai/company",
                "",
                "xAI was founded in March 2023. Grok is its model."
            ),
            page(
                "https://x.ai/",
                "",
                "xAI was founded in March 2023. Grok is its model."
            ),
            page("https://en.wikipedia.org/wiki/XAI_(company)", "", ""),
        ]
    );

    // A conversation with the same provider still goes where it always went.
    let plain = Request {
        messages: vec![Message::user("hi")],
        ..Request::default()
    };
    assert_eq!(finish(&provider, &model, &plain).await.text(), "Hello");
    let received = server.received_requests().await.unwrap();
    assert_eq!(received[1].url.path(), "/v1/chat/completions");
}

#[tokio::test]
async fn gemini_is_grounded_with_google_search_and_its_grounding_chunks_become_the_pages() {
    let server = MockServer::start().await;
    let link = |id: &str| format!("https://vertexaisearch.cloud.google.com/grounding-api-redirect/{id}");
    let (first, second) = (
        "Spain won Euro 2024, defeating England 2-1 in the final.",
        "This victory marks Spain's record fourth title.",
    );
    // The pages arrive over two records and the supports count across both.
    let records = [
        json!({"responseId": "r1", "candidates": [{
            "content": {"role": "model", "parts": [{"text": format!("{first} ")}]},
            "groundingMetadata": {"groundingChunks": [{"web": {"uri": link("AAA"), "title": "aljazeera.com"}}]},
        }]}),
        json!({"candidates": [{
            "content": {"role": "model", "parts": [{"text": second}]},
            "finishReason": "STOP",
            "groundingMetadata": {
                "webSearchQueries": ["UEFA Euro 2024 winner"],
                "groundingChunks": [
                    {"web": {"uri": link("BBB"), "title": "uefa.com"}},
                    {"web": {"uri": link("CCC"), "title": "bbc.com"}},
                ],
                "groundingSupports": [
                    {"segment": {"startIndex": 0, "endIndex": 56, "text": first}, "groundingChunkIndices": [1]},
                    {"segment": {"startIndex": 57, "endIndex": 104, "text": second},
                     "groundingChunkIndices": [0, 1]},
                ],
            },
        }], "usageMetadata": {"promptTokenCount": 20, "candidatesTokenCount": 30}}),
    ];
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-3-pro:streamGenerateContent"))
        .and(header("x-goog-api-key", "k"))
        .respond_with(sse(&records))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config = ProviderConfig::new(
        "google",
        WireApi::GoogleGenerative,
        format!("{}/v1beta", server.uri()),
    );
    let provider = keyed(config, dir.path());
    assert!(provider.hosts_web_search());

    let model = ModelInfo::unknown("google", "gemini-3-pro");
    // Grounding takes no domain filter and no limit: both stay out of the request.
    let message = finish(&provider, &model, &search(&["uefa.com"], Some(2))).await;
    assert_eq!(sent(&server).await["tools"], json!([{"googleSearch": {}}]));
    assert_eq!(message.text(), format!("{first} {second}"));
    assert_eq!(
        sources(&message),
        [
            page(&link("BBB"), "uefa.com", &format!("{first} … {second}")),
            page(&link("AAA"), "aljazeera.com", second),
            page(&link("CCC"), "bbc.com", ""),
        ]
    );
}

#[tokio::test]
async fn a_provider_without_a_search_tool_refuses_the_request_before_sending_it() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = ProviderConfig::new("deepseek", WireApi::OpenAiChat, server.uri());
    let provider = keyed(config, dir.path());
    assert!(!provider.hosts_web_search());

    let model = ModelInfo::unknown("deepseek", "deepseek-chat");
    let refused = provider
        .stream(&model, &search(&[], None), CancellationToken::new())
        .await
        .err();
    assert!(
        matches!(&refused, Some(AiError::InvalidRequest(why)) if why.contains("deepseek")),
        "{refused:?}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[test]
fn the_providers_with_a_search_tool_are_the_vendors_own_endpoints() {
    let presets = presets_with(&|_| None);
    let api = |id: &str| {
        let preset = presets.iter().find(|preset| preset.id == id).unwrap();
        hosted_api(preset)
    };
    assert_eq!(api("anthropic"), Some(WireApi::AnthropicMessages));
    assert_eq!(api("openai"), Some(WireApi::OpenAiResponses));
    assert_eq!(api("openai-codex"), Some(WireApi::OpenAiResponses));
    assert_eq!(api("google"), Some(WireApi::GoogleGenerative));
    assert_eq!(api("google-gemini-cli"), Some(WireApi::GoogleGeminiCli));
    assert_eq!(api("xai"), Some(WireApi::OpenAiResponses));
    assert_eq!(api("xai-oauth"), Some(WireApi::OpenAiResponses));
    // The same formats at someone else's address carry no search tool.
    for other in ["minimax", "openrouter", "meta", "deepseek", "ollama", "azure"] {
        assert_eq!(api(other), None, "{other}");
    }

    // A provider under another name at a vendor's address counts; a vendor's name moved to
    // another address (a proxy) still does.
    let work = ProviderConfig::new("work", WireApi::AnthropicMessages, "https://api.anthropic.com");
    assert_eq!(hosted_api(&work), Some(WireApi::AnthropicMessages));
    let proxied = ProviderConfig::new("openai", WireApi::OpenAiResponses, "http://localhost:4000/v1");
    assert_eq!(hosted_api(&proxied), Some(WireApi::OpenAiResponses));
    let elsewhere = ProviderConfig::new("work", WireApi::AnthropicMessages, "https://llm.example.com");
    assert_eq!(hosted_api(&elsewhere), None);
}
