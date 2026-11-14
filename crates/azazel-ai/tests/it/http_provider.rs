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

const MESSAGES_STREAM: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

/// An Anthropic-format provider with two stored accounts, `default` and `work`.
fn two_accounts(server: &MockServer, dir: &std::path::Path) -> (Arc<HttpProvider>, Arc<Credentials>) {
    let config = ProviderConfig {
        requires_key: true,
        ..ProviderConfig::new("acme", WireApi::AnthropicMessages, server.uri())
    };
    let credentials = Arc::new(Credentials::with_env(
        dir.join("auth.json"),
        Arc::new(|_: &str| None),
    ));
    for (key, secret) in [("acme", "k-default"), ("acme#work", "k-work")] {
        let key_entry = azazel_ai::Credential::ApiKey {
            key: secret.to_string(),
        };
        credentials.store(key, key_entry).unwrap();
    }
    let provider = HttpProvider::new(config, http_client(), credentials.clone());
    (Arc::new(provider), credentials)
}

#[tokio::test]
async fn usage_windows_are_kept_per_account_and_a_full_one_moves_work_to_the_next_account() {
    let server = MockServer::start().await;
    let reset = (azazel_ai::credentials::now_ms() / 1000 + 7200).to_string();
    // The default account's 5-hour window is full; the work account answers.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "k-default"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("anthropic-ratelimit-unified-status", "rejected")
                .insert_header("anthropic-ratelimit-unified-reset", reset.as_str())
                .insert_header("anthropic-ratelimit-unified-representative-claim", "five_hour")
                .insert_header("anthropic-ratelimit-unified-5h-utilization", "1.0")
                .insert_header("anthropic-ratelimit-unified-5h-reset", reset.as_str())
                .insert_header("anthropic-ratelimit-unified-7d-utilization", "0.3")
                .insert_header("anthropic-ratelimit-unified-7d-reset", "4000000000")
                .set_body_string(
                    r#"{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed your account's rate limit."}}"#,
                ),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "k-work"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("anthropic-ratelimit-unified-status", "allowed")
                .insert_header("anthropic-ratelimit-unified-5h-utilization", "0.25")
                .insert_header("anthropic-ratelimit-unified-5h-reset", "4000000000")
                .set_body_raw(MESSAGES_STREAM, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (provider, credentials) = two_accounts(&server, dir.path());
    let router = azazel_ai::Router::new(
        vec![provider],
        azazel_ai::Catalog::default(),
        BTreeMap::new(),
        azazel_ai::RetryPolicy::default(),
    )
    .with_accounts(credentials.clone());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let (_, stream) = router
        .stream_with(
            &["acme/claude-mock".parse().unwrap()],
            &request(),
            CancellationToken::new(),
            &mut azazel_ai::RouteState::with_events(tx),
        )
        .await
        .unwrap();
    let events: Vec<StreamEvent> = stream.collect().await;
    assert!(matches!(events.last(), Some(StreamEvent::Done { message }) if message.text() == "hi"));

    let limits = router.limits();
    let default = limits.windows("acme", "default");
    assert_eq!(
        default
            .iter()
            .map(|w| (w.name.as_str(), w.used, w.status.as_str()))
            .collect::<Vec<_>>(),
        [("5h", 1.0, "rejected"), ("7d", 0.3, "allowed")]
    );
    assert_eq!(limits.blocked_until("acme", "default"), reset.parse().ok());
    let work = limits.windows("acme", "work");
    assert_eq!((work.len(), work[0].used), (1, 0.25));
    assert_eq!(limits.blocked_until("acme", "work"), None);
    assert_eq!(credentials.selected("acme").as_deref(), Some("work"));
    match rx.try_recv().unwrap() {
        azazel_ai::RouteEvent::Account {
            from,
            to,
            until,
            limit,
            ..
        } => {
            assert_eq!((from.as_str(), to.as_str()), ("default", "work"));
            assert_eq!(Some(until), reset.parse().ok());
            assert_eq!(limit, "5-hour limit");
        }
        other => panic!("{other:?}"),
    }
}

/// What an account answers to `POST /v1/messages`: a reply, or a refusal for a full 5-hour
/// window that resets at `reset`.
fn answers(key: &str, reset: Option<&str>) -> Mock {
    let template = match reset {
        Some(reset) => ResponseTemplate::new(429)
            .insert_header("anthropic-ratelimit-unified-status", "rejected")
            .insert_header("anthropic-ratelimit-unified-reset", reset)
            .insert_header("anthropic-ratelimit-unified-representative-claim", "five_hour")
            .insert_header("anthropic-ratelimit-unified-5h-utilization", "1.0")
            .insert_header("anthropic-ratelimit-unified-5h-reset", reset)
            .set_body_string(
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed your account's rate limit."}}"#,
            ),
        None => ResponseTemplate::new(200)
            .insert_header("anthropic-ratelimit-unified-status", "allowed")
            .set_body_raw(MESSAGES_STREAM, "text/event-stream"),
    };
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", key))
        .respond_with(template)
}

/// A router over an Anthropic-format provider whose stored accounts are `accounts` (label and
/// key), as a run that has just started: blocks are kept in `dir/limits.json`, and `order` is
/// `[accounts] order` for the provider.
fn run_with(
    server: &MockServer,
    dir: &std::path::Path,
    accounts: &[(&str, &str)],
    order: &[&str],
) -> (azazel_ai::Router, Arc<Credentials>) {
    let config = ProviderConfig {
        requires_key: true,
        ..ProviderConfig::new("acme", WireApi::AnthropicMessages, server.uri())
    };
    let order = BTreeMap::from([(
        "acme".to_string(),
        order.iter().map(|label| label.to_string()).collect::<Vec<_>>(),
    )]);
    let credentials =
        Arc::new(Credentials::with_env(dir.join("auth.json"), Arc::new(|_: &str| None)).ordered(&order));
    for (label, secret) in accounts {
        let key = azazel_ai::Credential::ApiKey {
            key: secret.to_string(),
        };
        credentials
            .store(&azazel_ai::credentials::account_key("acme", label), key)
            .unwrap();
    }
    credentials.limits().persist(dir.join("limits.json"));
    let provider = Arc::new(HttpProvider::new(config, http_client(), credentials.clone()));
    let router = azazel_ai::Router::new(
        vec![provider],
        azazel_ai::Catalog::default(),
        BTreeMap::new(),
        azazel_ai::RetryPolicy::default(),
    )
    .with_accounts(credentials.clone());
    (router, credentials)
}

/// Asks `model` once and returns the account switches the router announced on the way.
async fn ask(router: &azazel_ai::Router, model: &str) -> Vec<(String, String, String)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_, stream) = router
        .stream_with(
            &[format!("acme/{model}").parse().unwrap()],
            &request(),
            CancellationToken::new(),
            &mut azazel_ai::RouteState::with_events(tx),
        )
        .await
        .unwrap();
    let events: Vec<StreamEvent> = stream.collect().await;
    assert!(matches!(events.last(), Some(StreamEvent::Done { message }) if message.text() == "hi"));
    std::iter::from_fn(|| rx.try_recv().ok())
        .map(|event| match event {
            azazel_ai::RouteEvent::Account { from, to, limit, .. } => (from, to, limit),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn an_account_that_ran_out_is_not_started_on_again_by_the_next_run() {
    let server = MockServer::start().await;
    let reset = (azazel_ai::credentials::now_ms() / 1000 + 7200).to_string();
    // The default account is asked once in all: by the run that finds it out of allowance.
    answers("k-default", Some(&reset)).expect(1).mount(&server).await;
    answers("k-work", None).expect(3).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let accounts = [("default", "k-default"), ("work", "k-work")];

    let (first, _) = run_with(&server, dir.path(), &accounts, &[]);
    assert_eq!(
        ask(&first, "claude-mock").await,
        [(
            "default".to_string(),
            "work".to_string(),
            "5-hour limit".to_string()
        )]
    );
    drop(first);

    // A new run starts on the default account again, knows from the file that it is out
    // until the reset, and moves on without asking it. It says which window ran out.
    let (second, credentials) = run_with(&server, dir.path(), &accounts, &[]);
    assert_eq!(credentials.selected("acme").as_deref(), Some("default"));
    let block = second
        .limits()
        .blocked_for("acme", "default", "claude-mock")
        .unwrap();
    assert_eq!(Some(block.until), reset.parse().ok());
    assert_eq!(
        ask(&second, "claude-mock").await,
        [(
            "default".to_string(),
            "work".to_string(),
            "5-hour limit".to_string()
        )]
    );
    assert_eq!(credentials.selected("acme").as_deref(), Some("work"));
    // Work stays there, quietly.
    assert_eq!(ask(&second, "claude-mock").await, []);
}

#[tokio::test]
async fn a_request_starts_on_the_first_account_in_order_that_has_room_for_its_model() {
    let server = MockServer::start().await;
    answers("k-default", None).expect(2).mount(&server).await;
    answers("k-work", None).expect(0).mount(&server).await;
    answers("k-zed", None).expect(1).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let accounts = [("default", "k-default"), ("work", "k-work"), ("zed", "k-zed")];
    let (router, credentials) = run_with(&server, dir.path(), &accounts, &["zed"]);
    let limits = router.limits();
    let later = azazel_ai::credentials::now_ms() / 1000 + 86_400;

    // A usage report said the weekly allowance of the Fable models is used up on the default
    // account. Other models are still asked there.
    let fable = azazel_ai::Window::new("7d-fable", 1.0, Some(later), "rejected").scoped("Fable");
    limits.record_report(
        "acme",
        "default",
        &[azazel_ai::Window::new("7d", 0.5, Some(later), "allowed"), fable],
        later - 86_400,
    );
    assert_eq!(ask(&router, "claude-opus-mock").await, []);
    // A Fable model is not: the request goes to the account `[accounts] order` names first,
    // not to the next one by label, and the default account is never asked.
    assert_eq!(
        ask(&router, "claude-fable-mock").await,
        [(
            "default".to_string(),
            "zed".to_string(),
            "7-day Fable limit".to_string()
        )]
    );
    assert_eq!(credentials.selected("acme").as_deref(), Some("zed"));
    // The block holds for the Fable models alone.
    assert_eq!(limits.blocked_until("acme", "default"), None);
    assert!(
        limits
            .blocked_for("acme", "default", "claude-fable-mock")
            .is_some()
    );

    // With every account out of allowance there is nowhere to move to: the provider is asked
    // on the account in use, and its answer counts, not what was known.
    credentials.select("acme", "default");
    for label in ["default", "work", "zed"] {
        limits.block("acme", label, later);
    }
    assert_eq!(ask(&router, "claude-opus-mock").await, []);
    assert_eq!(limits.blocked_until("acme", "default"), None);
    assert!(limits.blocked_until("acme", "work").is_some());
}

#[tokio::test]
async fn the_service_tier_is_sent_and_dropped_once_the_model_refuses_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(wiremock::matchers::body_partial_json(
            serde_json::json!({"service_tier": "priority"}),
        ))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"Unsupported parameter: 'service_tier' is not supported with this model.","type":"invalid_request_error","param":"service_tier","code":"unsupported_parameter"}}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CHAT_STREAM, "text/event-stream"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let router = azazel_ai::Router::new(
        vec![Arc::new(provider(&server, dir.path(), Some("sk-test")))],
        azazel_ai::Catalog::default(),
        BTreeMap::new(),
        azazel_ai::RetryPolicy::default(),
    );
    router.set_tier(Some(azazel_ai::Tier::Priority));
    let fast = Request {
        tier: router.tier(),
        ..request()
    };
    let chain = ["acme/mock-model".parse().unwrap()];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    for _ in 0..2 {
        let (_, stream) = router
            .stream_with(
                &chain,
                &fast,
                CancellationToken::new(),
                &mut azazel_ai::RouteState::with_events(tx.clone()),
            )
            .await
            .unwrap();
        let events: Vec<StreamEvent> = stream.collect().await;
        assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
    }

    // Said once; the second prompt went out without the tier from the start.
    assert!(matches!(
        rx.try_recv(),
        Ok(azazel_ai::RouteEvent::TierDropped { .. })
    ));
    assert!(rx.try_recv().is_err());
    let sent: Vec<Option<String>> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        // The router also asks an unknown model's provider for its model list.
        .filter(|sent| sent.method == wiremock::http::Method::POST)
        .map(|sent| {
            let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
            body.get("service_tier")
                .and_then(|tier| tier.as_str())
                .map(str::to_string)
        })
        .collect();
    assert_eq!(sent, [Some("priority".to_string()), None, None]);
}

#[tokio::test]
async fn opencode_requests_name_the_conversation_they_belong_to() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("x-opencode-session", "session-7:lead"))
        .and(header("x-opencode-client", "azazel"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CHAT_STREAM, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    // Without the header the gateway refuses, as OpenCode Go does.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("MissingSessionID"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [{ "id": "m" }] })),
        )
        .mount(&server)
        .await;
    let settings = azazel_ai::ProviderSettings {
        base_url: Some(server.uri()),
        ..Default::default()
    };
    let config =
        azazel_ai::providers::presets::resolve(&BTreeMap::from([("opencode-go".to_string(), settings)]))
            .unwrap()
            .into_iter()
            .find(|p| p.id == "opencode-go")
            .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let credentials = Credentials::with_env(
        dir.path().join("auth.json"),
        Arc::new(|name| (name == "OPENCODE_API_KEY").then(|| "oc-key".to_string())),
    );
    let provider = HttpProvider::new(config, http_client(), Arc::new(credentials));

    let asked = Request {
        cache_key: Some("session-7:lead".to_string()),
        ..request()
    };
    let stream = provider
        .stream(&model(), &asked, CancellationToken::new())
        .await
        .unwrap();
    let events: Vec<StreamEvent> = stream.collect().await;
    assert!(
        matches!(events.last(), Some(StreamEvent::Done { .. })),
        "{events:?}"
    );

    // A request of no conversation (the model list) still names a session: the provider's own.
    provider.list_models().await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let listing = requests.iter().find(|r| r.method.as_str() == "GET").unwrap();
    let session = listing
        .headers
        .get("x-opencode-session")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        session.len() == 32 && session.chars().all(|c| c.is_ascii_hexdigit()),
        "{session}"
    );
}
