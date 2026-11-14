#![allow(clippy::unwrap_used)]

//! `web_search` through the search tool of the lead's model provider: the request it makes on
//! the lead's model, the results it reads from the answer's pages, and when the configured
//! backend answers instead.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, ToolOutput, UpdateSink};
use azazel_ai::testing::{MOCK_SEARCH_QUEUE, MockProvider, MockStep};
use azazel_ai::{
    AiError, Block, Catalog, HostedTool, ModelRef, ModelSpec, Provider, Request, RetryPolicy, Router,
    WebSource,
};
use azazel_core::PromptBuilder;
use azazel_core::side::{LeadModel, SEARCH_PURPOSE, Side, SideCosts};
use azazel_tools::web::{ProviderSearch, SearchBackend, SearchHit, SearchQuery, WebSearchTool};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A backend that always finds the same page and counts how often it is asked.
#[derive(Clone, Default)]
struct Fixed {
    asked: Arc<AtomicUsize>,
}

#[async_trait]
impl SearchBackend for Fixed {
    fn name(&self) -> &'static str {
        "fixed"
    }

    fn base_url(&self) -> &str {
        "https://search.example"
    }

    async fn search(&self, _: &reqwest::Client, query: &SearchQuery) -> Result<Vec<SearchHit>, String> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Ok(vec![SearchHit {
            title: format!("Backend page for {}", query.query),
            url: "https://backend.example/page".to_string(),
            snippet: "From the configured backend.".to_string(),
        }])
    }
}

/// A session whose lead is on `mock/lead`, with the services a provider search needs.
struct Bench {
    mock: Arc<MockProvider>,
    costs: Arc<SideCosts>,
    services: Arc<Services>,
    backend: Fixed,
}

fn bench() -> Bench {
    let mock = Arc::new(MockProvider::new());
    let router = Router::new(
        vec![mock.clone() as Arc<dyn Provider>],
        Catalog::default(),
        BTreeMap::from([(
            "lead".to_string(),
            vec![ModelSpec::Model(ModelRef::new("mock", "lead"))],
        )]),
        RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::ZERO,
            max_jitter: Duration::ZERO,
            ..RetryPolicy::default()
        },
    );
    let costs = Arc::new(SideCosts::default());
    let services = Arc::new(Services::new());
    services.insert(Arc::new(Side {
        router: Arc::new(router),
        prompt: Arc::new(PromptBuilder::default()),
        tools: Arc::new(azazel_core::ext::ToolRegistry::default()),
        costs: costs.clone(),
    }));
    services.insert(Arc::new(LeadModel(Arc::new(|| {
        vec![ModelSpec::Role("lead".to_string())]
    }))));
    Bench {
        mock,
        costs,
        services,
        backend: Fixed::default(),
    }
}

impl Bench {
    fn tool(&self, provider: ProviderSearch) -> WebSearchTool {
        let backend: Arc<dyn SearchBackend> = Arc::new(self.backend.clone());
        WebSearchTool::new(Ok(backend), azazel_ai::tls::client()).with_provider(provider)
    }

    async fn call(&self, tool: &WebSearchTool, input: Value) -> ToolOutput {
        let agent = AgentInfo {
            id: "scout-1".into(),
            def: "scout".into(),
            parent: Some("lead".into()),
            depth: 1,
            session_id: "s".into(),
        };
        let ctx = ToolCtx {
            agent_id: agent.id.clone(),
            session_id: agent.session_id.clone(),
            agent,
            call_id: "c1".into(),
            cwd: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            update: UpdateSink::default(),
            services: self.services.clone(),
        };
        tool.call(input, &ctx).await
    }

    fn backend_calls(&self) -> usize {
        self.backend.asked.load(Ordering::SeqCst)
    }

    /// The requests that asked the provider to search.
    fn searches(&self) -> Vec<(String, Request)> {
        self.mock
            .requests()
            .into_iter()
            .filter(|(_, request)| !request.hosted.is_empty())
            .collect()
    }
}

fn page(url: &str, title: &str, snippet: &str) -> WebSource {
    WebSource {
        url: url.to_string(),
        title: title.to_string(),
        snippet: snippet.to_string(),
    }
}

fn text(output: &ToolOutput) -> &str {
    match output.content.as_slice() {
        [Block::Text { text }] => text,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_leads_provider_searches_and_its_pages_become_the_numbered_results() {
    let bench = bench();
    bench.mock.push(
        MOCK_SEARCH_QUEUE,
        MockStep::search(
            "Tokio 1.48 is out.",
            vec![
                page(
                    "https://docs.rs/tokio/latest/tokio/",
                    "tokio - Rust",
                    "A runtime for writing reliable\n  network applications.",
                ),
                // Outside the site asked for: left out even though the provider returned them.
                page("https://tokio.rs/blog", "Blog", "Announcing Tokio 1.48"),
                page(
                    "https://docs.rs/serde/latest/serde/",
                    "serde - Rust",
                    "A serialization framework.",
                ),
                page("https://docs.rs/tokio/latest/tokio/task/", "", ""),
                page(
                    "https://docs.rs/tokio/latest/tokio/net/",
                    "tokio::net",
                    "TCP, UDP and Unix sockets.",
                ),
            ],
        ),
    );
    let tool = bench.tool(ProviderSearch::First);

    let out = bench
        .call(
            &tool,
            json!({"query": "tokio runtime", "site": "https://docs.rs/tokio/", "recency": "month", "limit": 2}),
        )
        .await;
    assert!(!out.is_error, "{out:?}");
    assert_eq!(
        text(&out),
        "1. tokio - Rust\n   https://docs.rs/tokio/latest/tokio/\n   A runtime for writing reliable network applications.\n\n\
         2. https://docs.rs/tokio/latest/tokio/task/\n   https://docs.rs/tokio/latest/tokio/task/"
    );
    let details = out.details.unwrap();
    assert_eq!(details["backend"], "provider");
    assert_eq!(details["results"].as_array().unwrap().len(), 2);
    assert_eq!(bench.backend_calls(), 0);

    // One request, on the lead's model whoever called the tool, carrying the provider's search
    // tool and nothing of the session.
    let searches = bench.searches();
    assert_eq!(searches.len(), 1);
    let (model, request) = &searches[0];
    assert_eq!(model, "lead");
    assert_eq!(
        request.hosted,
        [HostedTool::WebSearch {
            domains: vec!["docs.rs".to_string()],
            max_uses: Some(2),
        }]
    );
    assert!(request.tools.is_empty() && request.cache_key.is_none());
    assert_eq!(request.system.len(), 1);
    assert_eq!(request.messages.len(), 1);
    assert_eq!(
        request.messages[0].text(),
        "Query: tokio runtime\nPages wanted: 2\nOnly pages on the site docs.rs/tokio.\n\
         Only pages published or updated in the last month."
    );
    assert!(bench.costs.by_purpose().contains_key(SEARCH_PURPOSE));
}

#[tokio::test]
async fn a_provider_search_that_fails_or_names_no_page_is_answered_by_the_configured_backend() {
    let bench = bench();
    bench
        .mock
        .push(MOCK_SEARCH_QUEUE, MockStep::fail(AiError::Overloaded));
    // The model answered from memory: text, and no page.
    bench.mock.push(
        MOCK_SEARCH_QUEUE,
        MockStep::search("I believe it is 1.48.", Vec::new()),
    );
    let tool = bench.tool(ProviderSearch::First);

    for why in ["overloaded", "named no pages"] {
        let out = bench.call(&tool, json!({"query": "tokio"})).await;
        assert_eq!(
            text(&out),
            "1. Backend page for tokio\n   https://backend.example/page\n   From the configured backend."
        );
        let details = out.details.unwrap();
        assert_eq!(details["backend"], "fixed");
        let said = details["provider_error"].as_str().unwrap();
        assert!(said.contains(why), "{said}");
    }
    assert_eq!((bench.searches().len(), bench.backend_calls()), (2, 2));
}

#[tokio::test]
async fn each_query_of_a_call_falls_back_on_its_own() {
    let bench = bench();
    bench.mock.push(
        MOCK_SEARCH_QUEUE,
        MockStep::search("", vec![page("https://a.example/1", "A", "first")]),
    );
    bench
        .mock
        .push(MOCK_SEARCH_QUEUE, MockStep::fail(AiError::Overloaded));
    let tool = bench.tool(ProviderSearch::First);

    // Queries are sent in order, so the first is answered and the second refused.
    let out = bench.call(&tool, json!({"queries": ["one", "two"]})).await;
    assert_eq!(
        text(&out),
        "## one\n1. A\n   https://a.example/1\n   first\n\n\
         ## two\n1. Backend page for two\n   https://backend.example/page\n   From the configured backend."
    );
    let details = out.details.unwrap();
    let queries = details["queries"].as_array().unwrap();
    assert_eq!(
        (&queries[0]["backend"], &queries[1]["backend"]),
        (&json!("provider"), &json!("fixed"))
    );
    assert!(queries[0].get("provider_error").is_none());
    assert!(
        queries[1]["provider_error"]
            .as_str()
            .unwrap()
            .contains("overloaded")
    );
}

#[tokio::test]
async fn search_set_to_provider_asks_nothing_else() {
    let bench = bench();
    let tool = bench.tool(ProviderSearch::Only);
    // No permission subject: the request goes where the lead's own requests go.
    assert!(
        WebSearchTool::new(Err("no backend".to_string()), azazel_ai::tls::client())
            .with_provider(ProviderSearch::Only)
            .subjects(&json!({"query": "x"}))
            .is_empty()
    );

    // The lead's provider has no search tool: the search fails and says what to change.
    let out = bench.call(&tool, json!({"query": "tokio"})).await;
    assert!(out.is_error);
    assert!(
        text(&out).contains("[web] search is \"provider\""),
        "{}",
        text(&out)
    );
    assert!(bench.searches().is_empty());

    // It has one and the search fails: the failure is the result.
    bench
        .mock
        .push(MOCK_SEARCH_QUEUE, MockStep::fail(AiError::Overloaded));
    bench.mock.push(
        MOCK_SEARCH_QUEUE,
        MockStep::search("", vec![page("https://a.example/1", "A", "")]),
    );
    let out = bench.call(&tool, json!({"query": "tokio"})).await;
    assert!(out.is_error);
    let said = text(&out);
    assert!(
        said.starts_with("the provider's web search failed: ") && said.contains("overloaded"),
        "{said}"
    );
    let out = bench.call(&tool, json!({"query": "tokio"})).await;
    assert_eq!(text(&out), "1. A\n   https://a.example/1");
    assert_eq!(bench.backend_calls(), 0);
}

#[tokio::test]
async fn the_provider_is_left_alone_when_it_has_no_search_tool_or_the_configuration_says_so() {
    assert_eq!(ProviderSearch::from_config("auto", true), ProviderSearch::First);
    assert_eq!(ProviderSearch::from_config("", true), ProviderSearch::First);
    assert_eq!(ProviderSearch::from_config("auto", false), ProviderSearch::Off);
    // Naming the provider is asking for it, whatever the switch says; naming a backend is
    // asking for that backend.
    assert_eq!(
        ProviderSearch::from_config(" Provider ", false),
        ProviderSearch::Only
    );
    assert_eq!(ProviderSearch::from_config("brave", true), ProviderSearch::Off);

    let bench = bench();
    // The lead's provider has no search tool: `auto` goes straight to the backend.
    let out = bench
        .call(&bench.tool(ProviderSearch::First), json!({"query": "tokio"}))
        .await;
    let details = out.details.unwrap();
    assert_eq!(details["backend"], "fixed");
    assert!(details.get("provider_error").is_none());

    // It has one, and the configuration turned it off.
    bench.mock.push(
        MOCK_SEARCH_QUEUE,
        MockStep::search("", vec![page("https://a.example/1", "A", "")]),
    );
    let out = bench
        .call(&bench.tool(ProviderSearch::Off), json!({"query": "tokio"}))
        .await;
    assert_eq!(out.details.unwrap()["backend"], "fixed");
    assert!(bench.mock.requests().is_empty());
    assert_eq!(bench.mock.remaining(MOCK_SEARCH_QUEUE), 1);
}

#[tokio::test]
async fn redirect_links_of_the_provider_are_replaced_by_the_page_they_lead_to() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/grounding-api-redirect/AAA"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "https://www.uefa.com/euro2024/final"),
        )
        .expect(1)
        .mount(&server)
        .await;
    // A link that no longer answers stays as it is; it still leads somewhere for `web_fetch`.
    Mock::given(method("GET"))
        .and(path("/grounding-api-redirect/BBB"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let links = format!("{}/grounding-api-redirect/", server.uri());
    let bench = bench();
    bench.mock.push(
        MOCK_SEARCH_QUEUE,
        MockStep::search(
            "",
            vec![
                page(&format!("{links}AAA"), "uefa.com", "Spain won Euro 2024."),
                page(&format!("{links}BBB"), "bbc.com", ""),
                page(
                    "https://www.aljazeera.com/sports/euro-2024-final",
                    "aljazeera.com",
                    "",
                ),
            ],
        ),
    );
    let tool = bench.tool(ProviderSearch::First).with_links_at(links.clone());

    let out = bench.call(&tool, json!({"query": "euro 2024"})).await;
    assert_eq!(
        text(&out),
        format!(
            "1. uefa.com\n   https://www.uefa.com/euro2024/final\n   Spain won Euro 2024.\n\n\
             2. bbc.com\n   {links}BBB\n\n\
             3. aljazeera.com\n   https://www.aljazeera.com/sports/euro-2024-final"
        )
    );
}
