#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use azazel_ai::providers::http::{HttpProvider, http_client};
use azazel_ai::{
    AiError, Cost, Credentials, Message, ModelInfo, Provider, ProviderConfig, Request, StreamEvent, WireApi,
};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHAT_STREAM: &str = concat!(
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n",
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"id\":\"r1\",\"choices\":[],\"usage\":{\"prompt_tokens\":1000000,\"completion_tokens\":500000}}\n\n",
    "data: [DONE]\n\n",
);

fn provider(server: &MockServer, dir: &std::path::Path, key: Option<&'static str>) -> HttpProvider {
    let config = ProviderConfig {
        api_key_env: vec!["ACME_KEY".into()],
        requires_key: key.is_some(),
        ..ProviderConfig::new("acme", WireApi::OpenAiChat, server.uri())
    };
    let credentials = Credentials::with_env(
        dir.join("auth.json"),
        Arc::new(move |name| (name == "ACME_KEY").then_some(key?.to_string())),
    );
    HttpProvider::new(config, http_client(), Arc::new(credentials))
}

fn model() -> ModelInfo {
    ModelInfo {
        cost: Some(Cost {
            input: 2.0,
            output: 8.0,
            cache_read: 0.0,
            cache_write: 0.0,
        }),
        ..ModelInfo::unknown("acme", "mock-model")
    }
}

fn request() -> Request {
    Request {
        messages: vec![Message::user("hi")],
        ..Request::default()
    }
}

#[tokio::test]
async fn streams_a_response_and_prices_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CHAT_STREAM, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let provider = provider(&server, dir.path(), Some("sk-test"));

    let stream = provider
        .stream(&model(), &request(), CancellationToken::new())
        .await
        .unwrap();
    let events: Vec<StreamEvent> = stream.collect().await;
    assert_eq!(events.first(), Some(&StreamEvent::Start));
    let Some(StreamEvent::Done { message }) = events.last() else {
        panic!("{events:?}")
    };
    assert_eq!(message.text(), "Hello");
    let meta = message.meta.as_ref().unwrap();
    assert_eq!(
        (meta.provider.as_str(), meta.model.as_str()),
        ("acme", "mock-model")
    );
    assert_eq!(meta.usage.cost_usd, Some(2.0 + 4.0));
}

#[tokio::test]
async fn an_azure_resource_key_goes_in_the_api_key_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("api-key", "az-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CHAT_STREAM, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let settings = azazel_ai::ProviderSettings {
        api: Some(WireApi::OpenAiChat),
        base_url: Some(server.uri()),
        ..Default::default()
    };
    let config = azazel_ai::providers::presets::resolve(&BTreeMap::from([("azure".to_string(), settings)]))
        .unwrap()
        .into_iter()
        .find(|p| p.id == "azure")
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let credentials = Credentials::with_env(
        dir.path().join("auth.json"),
        Arc::new(|name| (name == "AZURE_OPENAI_API_KEY").then(|| "az-key".to_string())),
    );
    let provider = HttpProvider::new(config, http_client(), Arc::new(credentials));

    let stream = provider
        .stream(&model(), &request(), CancellationToken::new())
        .await
        .unwrap();
    let events: Vec<StreamEvent> = stream.collect().await;
    assert!(
        matches!(events.last(), Some(StreamEvent::Done { .. })),
        "{events:?}"
    );
    let sent = &server.received_requests().await.unwrap()[0];
    assert!(!sent.headers.contains_key("authorization"));
}

#[tokio::test]
async fn http_failures_map_to_error_kinds() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let provider = provider(&server, dir.path(), None);
    let cases = [
        (
            ResponseTemplate::new(429).insert_header("retry-after", "7"),
            "rate",
        ),
        (ResponseTemplate::new(401).set_body_string("bad key"), "auth"),
        (ResponseTemplate::new(503), "overloaded"),
        (ResponseTemplate::new(500).set_body_string("boom"), "server"),
        (
            ResponseTemplate::new(404).set_body_string("no such model"),
            "invalid",
        ),
    ];
    for (response, expected) in cases {
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(response)
            .mount(&server)
            .await;
        let error = match provider
            .stream(&model(), &request(), CancellationToken::new())
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("{expected}: expected an error"),
        };
        let ok = match (&error, expected) {
            (AiError::RateLimit { retry_after }, "rate") => *retry_after == Some(Duration::from_secs(7)),
            (AiError::Auth(body), "auth") => body.contains("bad key"),
            (AiError::Overloaded, "overloaded") => true,
            (AiError::Server { status: 500, body }, "server") => body == "boom",
            (AiError::InvalidRequest(body), "invalid") => {
                body.contains("404") && body.contains("no such model")
            }
            _ => false,
        };
        assert!(ok, "{expected}: got {error:?}");
    }
}

#[tokio::test]
async fn body_cut_short_ends_with_network_error_carrying_the_partial() {
    let server = MockServer::start().await;
    let truncated: String = CHAT_STREAM.split_inclusive("\n\n").take(2).collect();
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(truncated, "text/event-stream"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let provider = provider(&server, dir.path(), None);

    let stream = provider
        .stream(&model(), &request(), CancellationToken::new())
        .await
        .unwrap();
    let events: Vec<StreamEvent> = stream.collect().await;
    match events.last() {
        Some(StreamEvent::Error {
            error: AiError::Network(_),
            partial: Some(partial),
        }) => {
            assert_eq!(partial.text(), "Hello")
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn cancelling_before_the_response_arrives_aborts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .set_body_raw(CHAT_STREAM, "text/event-stream"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let provider = provider(&server, dir.path(), None);
    let cancel = CancellationToken::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        canceller.cancel();
    });
    let result = provider.stream(&model(), &request(), cancel).await;
    assert!(matches!(result, Err(AiError::Aborted)));
}

#[tokio::test]
async fn lists_models_from_the_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"data": [{"id": "mock-b"}, {"id": "mock-a"}]})),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let provider = provider(&server, dir.path(), None);
    let mut ids: Vec<String> = provider
        .list_models()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    ids.sort();
    assert_eq!(ids, ["mock-a", "mock-b"]);
}
