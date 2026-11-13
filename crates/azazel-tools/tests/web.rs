#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use azazel_agent::{Access, AgentInfo, Services, Tool, ToolCtx, ToolOutput, ToolSource, UpdateSink};
use azazel_ai::Block;
use azazel_core::{ArtifactStore, ExtCtx, Registry};
use azazel_tools::web::{
    Brave, DuckDuckGo, Exa, Fetcher, MAX_BODY_BYTES, SearchBackend, SearchHit, SearchQuery, Searxng, Tavily,
    UrlScheme, WebExt, WebFetchTool, WebGrepTool, WebSearchTool, active_backend, backend_from_config,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{bearer_token, header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BRAVE: &str = include_str!("fixtures/web/brave.json");
const TAVILY: &str = include_str!("fixtures/web/tavily.json");
const EXA: &str = include_str!("fixtures/web/exa.json");
const SEARXNG: &str = include_str!("fixtures/web/searxng.json");
const DUCKDUCKGO: &str = include_str!("fixtures/web/duckduckgo.html");
const DUCKDUCKGO_CHALLENGE: &str = include_str!("fixtures/web/duckduckgo_challenge.html");
const ARTICLE: &str = include_str!("fixtures/web/article.html");

const ARTICLE_TITLE: &str = "How the borrow checker tracks ownership in Rust";
const ARTICLE_SENTENCES: [&str; 2] = [
    "Every value in a Rust program has exactly one owner at any moment",
    "Lifetimes are the names the compiler gives to the regions of code in which a borrow is valid.",
];
const PAGE_CHROME: [&str; 6] = [
    "Pricing plans",
    "Careers portal",
    "Sign in to your account",
    "TRACKING_SNIPPET_MARKER",
    "NAV_CLICK_HANDLER_MARKER",
    "font-family",
];

struct Bench {
    dir: tempfile::TempDir,
    services: Arc<Services>,
}

fn bench() -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let services = Arc::new(Services::new());
    services.insert(Arc::new(ArtifactStore::new(dir.path().join("artifacts"))));
    Bench { dir, services }
}

impl Bench {
    fn agent(&self) -> AgentInfo {
        AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id: "s".into(),
        }
    }

    fn ctx(&self) -> ToolCtx {
        let info = self.agent();
        ToolCtx {
            agent_id: info.id.clone(),
            session_id: info.session_id.clone(),
            agent: info,
            call_id: "c1".into(),
            cwd: self.dir.path().to_path_buf(),
            cancel: CancellationToken::new(),
            update: UpdateSink::default(),
            services: self.services.clone(),
        }
    }

    fn artifacts(&self) -> Vec<PathBuf> {
        match std::fs::read_dir(self.dir.path().join("artifacts")) {
            Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    async fn search(&self, backend: impl SearchBackend + 'static, input: Value) -> ToolOutput {
        let backend: Arc<dyn SearchBackend> = Arc::new(backend);
        WebSearchTool::new(Ok(backend), reqwest::Client::new())
            .call(input, &self.ctx())
            .await
    }
}

fn fetch_tool() -> WebFetchTool {
    WebFetchTool::new(Arc::new(Fetcher::new().unwrap()))
}

/// `web_fetch`, `web_grep` and `read` on one fetcher, as the extension registers them.
struct Web {
    fetch: WebFetchTool,
    grep: WebGrepTool,
    read: azazel_tools::ReadTool,
}

fn web() -> Web {
    let fetcher = Arc::new(Fetcher::new().unwrap());
    let schemes = Arc::new(azazel_core::Schemes::default());
    schemes.add("web", Arc::new(UrlScheme::http(fetcher.clone())));
    Web {
        fetch: WebFetchTool::new(fetcher.clone()),
        grep: WebGrepTool::new(fetcher),
        read: azazel_tools::ReadTool::new(Arc::default(), schemes),
    }
}

/// A plain-text page of 400 numbered lines, `line 1: …` to `line 400: …`; every 50th mentions a
/// release.
fn long_page() -> String {
    (1..=400)
        .map(|n| {
            let note = if n % 50 == 0 {
                format!("Release {} shipped", n / 50)
            } else {
                "nothing to report".to_string()
            };
            format!("line {n}: {note}\n")
        })
        .collect()
}

fn json_body(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "application/json")
}

fn html_body(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/html; charset=utf-8")
}

fn redirect(status: u16, location: impl Into<String>) -> ResponseTemplate {
    ResponseTemplate::new(status).insert_header("location", location.into())
}

/// Whether `markdown` has a heading line reading exactly `title`.
fn has_heading(markdown: &str, title: &str) -> bool {
    markdown
        .lines()
        .any(|line| line.starts_with('#') && line.trim_start_matches('#').trim() == title)
}

fn ok(output: &ToolOutput) -> String {
    let text = output.text_content();
    assert!(!output.is_error, "{text}");
    text
}

fn failed(output: &ToolOutput) -> String {
    let text = output.text_content();
    assert!(output.is_error, "expected an error, got: {text}");
    text
}

async fn bodies(server: &MockServer) -> Vec<Value> {
    let requests = server.received_requests().await.unwrap();
    requests.iter().map(|r| r.body_json().unwrap()).collect()
}

#[tokio::test]
async fn brave_puts_site_and_recency_in_the_query_string_and_reads_web_results() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/res/v1/web/search"))
        .and(header("x-subscription-token", "brave-key"))
        .and(query_param("q", "tokio select macro site:docs.rs"))
        .and(query_param("count", "5"))
        .and(query_param("freshness", "pw"))
        .and(query_param("text_decorations", "false"))
        .respond_with(json_body(BRAVE))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/res/v1/web/search"))
        .and(query_param("q", "plain"))
        .and(query_param("count", "8"))
        .and(query_param_is_missing("freshness"))
        .respond_with(json_body(r#"{"type": "search", "query": {"original": "plain"}}"#))
        .expect(1)
        .mount(&server)
        .await;
    let bench = bench();

    let filtered = bench
        .search(
            Brave::new(server.uri(), "brave-key"),
            json!({"query": "tokio select macro", "limit": 5, "site": "docs.rs", "recency": "week"}),
        )
        .await;
    assert_eq!(
        ok(&filtered),
        "1. select in tokio - Rust\n   \
         https://docs.rs/tokio/latest/tokio/macro.select.html\n   \
         Waits on multiple concurrent branches, returning when the first branch completes, cancelling the \
         remaining branches.\n\n\
         2. tokio::select - Rust & async\n   \
         https://docs.rs/tokio/latest/tokio/index.html\n   \
         A runtime for writing reliable network applications without compromising speed. Tokio's select! \
         macro lets a task wait on several futures.\n\n\
         3. futures::select - Rust\n   \
         https://docs.rs/futures/latest/futures/macro.select.html"
    );

    let plain = bench
        .search(Brave::new(server.uri(), "brave-key"), json!({"query": "plain"}))
        .await;
    assert_eq!(ok(&plain), "no results for \"plain\"");
}

#[tokio::test]
async fn tavily_sends_a_bearer_key_and_filters_in_the_json_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .and(bearer_token("tvly-key"))
        .respond_with(json_body(TAVILY))
        .expect(2)
        .mount(&server)
        .await;
    let bench = bench();

    let filtered = bench
        .search(
            Tavily::new(server.uri(), "tvly-key"),
            json!({
                "query": "rust 2024 edition changes",
                "limit": 3,
                "site": "https://doc.rust-lang.org/",
                "recency": "month"
            }),
        )
        .await;
    assert_eq!(
        ok(&filtered),
        "1. Rust 2024 - The Rust Edition Guide\n   \
         https://doc.rust-lang.org/edition-guide/rust-2024/index.html\n   \
         The Rust 2024 edition was stabilized in Rust 1.85. It changes how return-position impl Trait \
         captures lifetimes, [...] makes unsafe extern blocks explicit, and reserves the gen keyword.\n\n\
         2. Announcing Rust 1.85.0 and Rust 2024\n   \
         https://blog.rust-lang.org/2025/02/20/Rust-1.85.0/\n   \
         Rust 1.85.0 stabilizes the 2024 edition as well as async closures."
    );
    ok(&bench
        .search(Tavily::new(server.uri(), "tvly-key"), json!({"query": "plain"}))
        .await);

    assert_eq!(
        bodies(&server).await,
        [
            json!({
                "query": "rust 2024 edition changes",
                "max_results": 3,
                "include_domains": ["doc.rust-lang.org"],
                "time_range": "month"
            }),
            json!({"query": "plain", "max_results": 8}),
        ]
    );
}

#[tokio::test]
async fn exa_sends_its_key_header_and_turns_recency_into_a_start_date() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .and(header("x-api-key", "exa-key"))
        .respond_with(json_body(EXA))
        .expect(3)
        .mount(&server)
        .await;
    let bench = bench();

    let filtered = bench
        .search(
            Exa::new(server.uri(), "exa-key"),
            json!({"query": "large language models", "limit": 4, "site": "arxiv.org", "recency": "year"}),
        )
        .await;
    assert_eq!(
        ok(&filtered),
        "1. A Comprehensive Overview of Large Language Models\n   \
         https://arxiv.org/abs/2307.06435\n   \
         Such requirements have limited their adoption and have motivated more efficient architectures. \
         This article reviews training strategies, context length improvements and fine-tuning.\n\n\
         2. https://example.org/untitled-notes\n   \
         https://example.org/untitled-notes\n   \
         Lecture notes on attention and transformer models.\n\n\
         3. A page without extracted contents\n   \
         https://example.org/no-contents"
    );
    ok(&bench
        .search(
            Exa::new(server.uri(), "exa-key"),
            json!({"query": "large language models", "recency": "day"}),
        )
        .await);
    ok(&bench
        .search(Exa::new(server.uri(), "exa-key"), json!({"query": "plain"}))
        .await);

    let bodies = bodies(&server).await;
    assert_eq!(bodies[0]["query"], "large language models");
    assert_eq!(bodies[0]["numResults"], 4);
    assert_eq!(bodies[0]["contents"], json!({"highlights": true}));
    assert_eq!(bodies[0]["includeDomains"], json!(["arxiv.org"]));
    let since_a_year = bodies[0]["startPublishedDate"].as_str().unwrap();
    let since_a_day = bodies[1]["startPublishedDate"].as_str().unwrap();
    for date in [since_a_year, since_a_day] {
        // ISO 8601 in UTC, as the API wants it: 2025-01-31T08:00:00.000Z.
        assert_eq!(date.len(), 24, "{date}");
        assert_eq!(&date[4..5], "-", "{date}");
        assert_eq!(&date[10..11], "T", "{date}");
        assert!(date.ends_with(".000Z"), "{date}");
    }
    assert!(
        since_a_year < since_a_day,
        "{since_a_year} is not before {since_a_day}"
    );
    assert_eq!(
        bodies[2],
        json!({"query": "plain", "numResults": 8, "contents": {"highlights": true}})
    );
}

#[tokio::test]
async fn searxng_asks_the_configured_instance_for_json() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/searx/search"))
        .and(query_param("q", "wiremock rust site:docs.rs"))
        .and(query_param("format", "json"))
        .and(query_param("time_range", "year"))
        .respond_with(json_body(SEARXNG))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/searx/search"))
        .and(query_param("q", "plain"))
        .and(query_param("format", "json"))
        .and(query_param_is_missing("time_range"))
        .respond_with(json_body(r#"{"query": "plain", "results": []}"#))
        .expect(1)
        .mount(&server)
        .await;
    let bench = bench();
    // An instance served below a path, written with a trailing slash.
    let instance = format!("{}/searx/", server.uri());

    let filtered = bench
        .search(
            Searxng::new(instance.clone()),
            json!({"query": "wiremock rust", "site": "docs.rs", "recency": "year"}),
        )
        .await;
    assert_eq!(
        ok(&filtered),
        "1. wiremock - Rust\n   \
         https://docs.rs/wiremock/latest/wiremock/\n   \
         wiremock provides HTTP mocking to perform black-box testing of Rust applications that interact \
         with third-party APIs.\n\n\
         2. MockServer in wiremock - Rust\n   \
         https://docs.rs/wiremock/latest/wiremock/struct.MockServer.html"
    );
    let plain = bench
        .search(Searxng::new(instance), json!({"query": "plain"}))
        .await;
    assert_eq!(ok(&plain), "no results for \"plain\"");
}

#[tokio::test]
async fn duckduckgo_reads_the_html_results_page_and_skips_advertisements() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/html/"))
        .and(query_param("q", "rust programming language site:rust-lang.org"))
        .and(query_param("df", "d"))
        .respond_with(html_body(DUCKDUCKGO))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/html/"))
        .and(query_param("q", "rust programming language"))
        .and(query_param_is_missing("df"))
        .respond_with(html_body(DUCKDUCKGO))
        .expect(1)
        .mount(&server)
        .await;
    let bench = bench();

    let filtered = bench
        .search(
            DuckDuckGo::new(server.uri()),
            json!({"query": "rust programming language", "site": "rust-lang.org", "recency": "day"}),
        )
        .await;
    assert_eq!(
        ok(&filtered),
        "1. Rust (programming language) - Wikipedia\n   \
         https://en.wikipedia.org/wiki/Rust_(programming_language)\n   \
         Rust is a general-purpose programming language that emphasizes performance, type safety, \
         concurrency, and memory safety.\n\n\
         2. std - Rust\n   \
         https://doc.rust-lang.org/beta/std/index.html\n   \
         The Rust Standard Library is the foundation of portable Rust software. It offers core types, \
         like Vec<T> and Option<T>, library-defined operations on language primitives, and more.\n\n\
         3. Announcing Rust 1.99.1 | Rust Blog\n   \
         https://blog.rust-lang.org/2026/10/15/Rust-1.99.1/"
    );

    // `limit` caps what the page offers.
    let two = bench
        .search(
            DuckDuckGo::new(server.uri()),
            json!({"query": "rust programming language", "limit": 2}),
        )
        .await;
    let two = ok(&two);
    assert!(two.contains("2. std - Rust"), "{two}");
    assert!(!two.contains("3. "), "{two}");
}

#[tokio::test]
async fn a_backend_that_refuses_is_a_tool_error_saying_why() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/html/"))
        .respond_with(ResponseTemplate::new(202).set_body_raw(DUCKDUCKGO_CHALLENGE, "text/html"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"detail": {"error": "Unauthorized: missing or invalid API key."}})),
        )
        .mount(&server)
        .await;
    let bench = bench();

    let challenged = bench
        .search(DuckDuckGo::new(server.uri()), json!({"query": "anything"}))
        .await;
    let challenged = failed(&challenged);
    assert!(challenged.contains("bot challenge"), "{challenged}");

    let unauthorized = bench
        .search(Tavily::new(server.uri(), "wrong"), json!({"query": "anything"}))
        .await;
    let unauthorized = failed(&unauthorized);
    assert!(unauthorized.contains("tavily"), "{unauthorized}");
    assert!(unauthorized.contains("401"), "{unauthorized}");
    assert!(
        unauthorized.contains("missing or invalid API key"),
        "{unauthorized}"
    );

    let unreadable = bench
        .search(
            Brave::new(format!("{}/nowhere", server.uri()), "k"),
            json!({"query": "anything"}),
        )
        .await;
    assert!(failed(&unreadable).contains("404"));

    let empty = bench
        .search(DuckDuckGo::new(server.uri()), json!({"query": "   "}))
        .await;
    assert_eq!(failed(&empty), "query is empty");
    let bad_recency = bench
        .search(
            DuckDuckGo::new(server.uri()),
            json!({"query": "x", "recency": "decade"}),
        )
        .await;
    assert!(failed(&bad_recency).starts_with("invalid arguments"));
}

fn env_of(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |name| {
        vars.iter()
            .find(|(var, _)| *var == name)
            .map(|(_, value)| value.to_string())
    }
}

#[test]
fn auto_prefers_keyed_backends_then_searxng_then_duckduckgo() {
    type Vars = &'static [(&'static str, &'static str)];
    const BRAVE_KEY: (&str, &str) = ("BRAVE_API_KEY", "b");
    const TAVILY_KEY: (&str, &str) = ("TAVILY_API_KEY", "t");
    const EXA_KEY: (&str, &str) = ("EXA_API_KEY", "e");
    let cases: [(&str, &str, Vars, &str); 13] = [
        ("auto", "", &[], "duckduckgo"),
        ("", "", &[], "duckduckgo"),
        ("auto", "http://searx.local", &[], "searxng"),
        ("auto", "http://searx.local", &[EXA_KEY], "exa"),
        ("auto", "", &[EXA_KEY, TAVILY_KEY], "tavily"),
        ("auto", "", &[EXA_KEY, TAVILY_KEY, BRAVE_KEY], "brave"),
        // A variable that is set but blank is no key.
        ("auto", "", &[("BRAVE_API_KEY", "  "), EXA_KEY], "exa"),
        // An explicit choice wins over what is available.
        ("duckduckgo", "http://searx.local", &[BRAVE_KEY], "duckduckgo"),
        ("searxng", "", &[BRAVE_KEY], "searxng"),
        ("exa", "", &[BRAVE_KEY], "exa"),
        ("Tavily", "", &[], "tavily"),
        // The provider's own search has no backend behind it.
        ("provider", "http://searx.local", &[BRAVE_KEY], "provider"),
        ("bing", "", &[BRAVE_KEY], "none"),
    ];
    for (search, searxng_url, vars, expected) in cases {
        assert_eq!(
            active_backend(search, searxng_url, &env_of(vars)),
            expected,
            "search={search:?} searxng_url={searxng_url:?} env={vars:?}"
        );
    }
}

#[tokio::test]
async fn an_explicit_backend_without_its_key_or_url_errors_naming_what_to_set() {
    let no_env = env_of(&[]);
    for (search, needs) in [
        ("brave", "BRAVE_API_KEY"),
        ("tavily", "TAVILY_API_KEY"),
        ("exa", "EXA_API_KEY"),
        ("searxng", "searxng_url"),
        ("bing", "duckduckgo"),
        ("provider", "no web search of its own"),
    ] {
        let reason = backend_from_config(search, "", &no_env).err().unwrap();
        assert!(reason.contains(needs), "{search}: {reason}");
        assert!(reason.contains(search), "{search}: {reason}");
    }

    let chosen = backend_from_config("auto", "", &env_of(&[("TAVILY_API_KEY", "t")])).unwrap();
    assert_eq!(
        (chosen.name(), chosen.base_url()),
        ("tavily", "https://api.tavily.com")
    );
    let fallback = backend_from_config("auto", "", &no_env).unwrap();
    assert_eq!(
        (fallback.name(), fallback.base_url()),
        ("duckduckgo", "https://html.duckduckgo.com")
    );

    // The tool reports the same reason on every call, and names no host to permission rules.
    let bench = bench();
    let tool = WebSearchTool::new(backend_from_config("brave", "", &no_env), reqwest::Client::new());
    assert!(tool.subjects(&json!({"query": "x"})).is_empty());
    let refused = tool.call(json!({"query": "x"}), &bench.ctx()).await;
    assert!(failed(&refused).contains("BRAVE_API_KEY"));
}

#[test]
fn web_tools_are_network_access_on_the_host_they_contact() {
    let backend: Arc<dyn SearchBackend> = Arc::new(Brave::new("https://api.search.brave.com", "k"));
    let search = WebSearchTool::new(Ok(backend), reqwest::Client::new());
    let input = json!({"query": "x"});
    assert_eq!(search.access(&input), Access::Net);
    assert_eq!(search.subjects(&input), ["domain:api.search.brave.com"]);

    let fetch = fetch_tool();
    let input = json!({"url": "https://Docs.RS/tokio?x=1"});
    assert_eq!(fetch.access(&input), Access::Net);
    assert_eq!(fetch.subjects(&input), ["domain:docs.rs"]);
    assert!(fetch.subjects(&json!({"url": "not a url"})).is_empty());
}

#[tokio::test]
async fn an_article_page_becomes_markdown_without_the_page_chrome() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/blog/borrow-checker"))
        .respond_with(html_body(ARTICLE))
        .mount(&server)
        .await;
    let bench = bench();
    let url = format!("{}/blog/borrow-checker", server.uri());

    let markdown = ok(&fetch_tool().call(json!({"url": url}), &bench.ctx()).await);
    assert!(has_heading(&markdown, ARTICLE_TITLE), "{markdown}");
    assert!(markdown.contains("## Moves and copies"), "{markdown}");
    for sentence in ARTICLE_SENTENCES {
        assert!(
            markdown.contains(sentence),
            "missing {sentence:?} in:\n{markdown}"
        );
    }
    for chrome in PAGE_CHROME {
        assert!(!markdown.contains(chrome), "found {chrome:?} in:\n{markdown}");
    }
    assert!(!markdown.contains("<p"), "{markdown}");
}

#[tokio::test]
async fn text_is_the_article_without_markup_and_raw_is_the_body_as_sent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/blog/borrow-checker"))
        .respond_with(html_body(ARTICLE))
        .mount(&server)
        .await;
    let bench = bench();
    let tool = fetch_tool();
    let url = format!("{}/blog/borrow-checker", server.uri());

    let text = ok(&tool
        .call(json!({"url": url, "format": "text"}), &bench.ctx())
        .await);
    assert!(text.contains(ARTICLE_TITLE), "{text}");
    for sentence in ARTICLE_SENTENCES {
        assert!(text.contains(sentence), "missing {sentence:?} in:\n{text}");
    }
    for chrome in PAGE_CHROME {
        assert!(!text.contains(chrome), "found {chrome:?} in:\n{text}");
    }
    assert!(!text.lines().any(|line| line.starts_with('#')), "{text}");
    assert!(!text.contains('<'), "{text}");

    let raw = ok(&tool
        .call(json!({"url": url, "format": "raw"}), &bench.ctx())
        .await);
    assert_eq!(raw, ARTICLE);

    let unknown = tool
        .call(json!({"url": url, "format": "pdf"}), &bench.ctx())
        .await;
    assert!(failed(&unknown).starts_with("invalid arguments"));
}

#[tokio::test]
async fn json_and_plain_text_pass_through_unchanged() {
    let server = MockServer::start().await;
    let payload = r#"{"items":[1,2,3],"next":null,"note":"<b>not markup</b>"}"#;
    Mock::given(method("GET"))
        .and(path("/api/items"))
        .respond_with(json_body(payload))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/notes.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("line one\n  line two\n"))
        .mount(&server)
        .await;
    let bench = bench();
    let tool = fetch_tool();

    let json_out = tool
        .call(
            json!({"url": format!("{}/api/items", server.uri())}),
            &bench.ctx(),
        )
        .await;
    assert_eq!(ok(&json_out), payload);
    let text_out = tool
        .call(
            json!({"url": format!("{}/notes.txt", server.uri()), "format": "text"}),
            &bench.ctx(),
        )
        .await;
    assert_eq!(ok(&text_out), "line one\n  line two\n");
    assert!(bench.artifacts().is_empty());
}

#[tokio::test]
async fn other_content_is_saved_as_an_artifact_and_its_path_returned() {
    let server = MockServer::start().await;
    let image: Vec<u8> = b"\x89PNG\r\n\x1a\n\xff\xfe\x00 not text".to_vec();
    Mock::given(method("GET"))
        .and(path("/files/chart.png"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(image.clone(), "image/png"))
        .mount(&server)
        .await;
    let bench = bench();
    let url = format!("{}/files/chart.png", server.uri());

    let out = fetch_tool().call(json!({"url": url}), &bench.ctx()).await;
    let text = ok(&out);
    let saved = bench.artifacts();
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert!(saved[0].to_string_lossy().ends_with("chart.png"), "{saved:?}");
    assert_eq!(std::fs::read(&saved[0]).unwrap(), image);
    assert_eq!(
        text,
        format!(
            "{url} is image/png, not text ({} bytes). Saved to {}",
            image.len(),
            saved[0].display()
        )
    );
}

#[cfg(feature = "pdf")]
#[tokio::test]
async fn a_pdf_comes_back_as_its_text_under_a_line_naming_the_saved_file() {
    let read = |name: &str| {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/read")
                .join(name),
        )
        .unwrap()
    };
    let pdf = read("three-pages.pdf");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/papers/three-pages.pdf"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(pdf.clone(), "application/pdf"))
        .expect(1)
        .mount(&server)
        .await;
    // A download without a useful type is known by its first bytes.
    Mock::given(method("GET"))
        .and(path("/download"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(pdf.clone(), "application/octet-stream"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/scanned.pdf"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(read("scanned.pdf"), "application/pdf"))
        .mount(&server)
        .await;
    let bench = bench();
    let web = web();
    let url = format!("{}/papers/three-pages.pdf", server.uri());

    let text = ok(&web.fetch.call(json!({"url": url}), &bench.ctx()).await);
    let saved = bench.artifacts();
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert_eq!(std::fs::read(&saved[0]).unwrap(), pdf);
    assert_eq!(
        text,
        format!(
            "{url} is a PDF ({} bytes), saved to {}\n\n\
             --- page 1 ---\nAlpha one\n\n--- page 2 ---\nBeta two\n\n--- page 3 ---\nGamma three",
            pdf.len(),
            saved[0].display()
        )
    );

    // The text pages and greps like any other page, without another request.
    let part = ok(&web
        .fetch
        .call(json!({"url": url, "offset": 6, "limit": 23}), &bench.ctx())
        .await);
    assert_eq!(
        part,
        "--- page 2 ---\nBeta two\n[lines 6-7 of 10; continue with offset=8, or web_grep to search the page]"
    );
    let found = ok(&web
        .grep
        .call(
            json!({"url": url, "pattern": "^gamma", "ignore_case": true}),
            &bench.ctx(),
        )
        .await);
    assert_eq!(found, "10: Gamma three");
    let read = web
        .read
        .call(json!({"path": url, "offset": 9}), &bench.ctx())
        .await;
    assert_eq!(ok(&read), "--- page 3 ---\nGamma three\n[lines 9-10 of 10]");
    assert_eq!(bench.artifacts().len(), 1);

    let download = ok(&web
        .fetch
        .call(json!({"url": format!("{}/download", server.uri())}), &bench.ctx())
        .await);
    assert!(download.ends_with("--- page 3 ---\nGamma three"), "{download}");

    // A PDF without a text layer is saved, and the result says why there is no text.
    let scanned_url = format!("{}/scanned.pdf", server.uri());
    let scanned = ok(&web.fetch.call(json!({"url": scanned_url}), &bench.ctx()).await);
    let saved: Vec<PathBuf> = bench
        .artifacts()
        .into_iter()
        .filter(|path| path.to_string_lossy().ends_with("scanned.pdf"))
        .collect();
    assert_eq!(saved.len(), 1);
    assert!(
        scanned.starts_with(&format!("{scanned_url} is a PDF (")),
        "{scanned}"
    );
    assert!(scanned.contains("no extractable text"), "{scanned}");
    assert!(
        scanned.ends_with(&format!("Saved to {}", saved[0].display())),
        "{scanned}"
    );
}

#[cfg(not(feature = "pdf"))]
#[tokio::test]
async fn a_build_without_the_pdf_reader_saves_a_pdf_and_says_so() {
    let server = MockServer::start().await;
    let pdf: Vec<u8> = b"%PDF-1.7\n\xff\xfe\x00 not text".to_vec();
    Mock::given(method("GET"))
        .and(path("/files/report.pdf"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(pdf.clone(), "application/pdf"))
        .mount(&server)
        .await;
    let bench = bench();
    let url = format!("{}/files/report.pdf", server.uri());

    let text = ok(&fetch_tool().call(json!({"url": url}), &bench.ctx()).await);
    let saved = bench.artifacts();
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert_eq!(std::fs::read(&saved[0]).unwrap(), pdf);
    assert_eq!(
        text,
        format!(
            "{url} is a PDF, which this build of azazel cannot read ({} bytes). Saved to {}",
            pdf.len(),
            saved[0].display()
        )
    );
}

#[tokio::test]
async fn a_long_page_is_paged_by_line_from_one_request() {
    let server = MockServer::start().await;
    let page = long_page();
    Mock::given(method("GET"))
        .and(path("/log.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string(page.clone()))
        .expect(1)
        .mount(&server)
        .await;
    let bench = bench();
    let web = web();
    let url = format!("{}/log.txt", server.uri());
    let fetch = |input: Value| {
        let (web, ctx) = (&web, bench.ctx());
        async move { web.fetch.call(input, &ctx).await }
    };

    // The default window is larger than this page; a smaller one ends on a whole line and names
    // the next.
    assert_eq!(ok(&fetch(json!({"url": url})).await), page);
    let first = ok(&fetch(json!({"url": url, "limit": 100})).await);
    assert_eq!(
        first,
        "line 1: nothing to report\nline 2: nothing to report\nline 3: nothing to report\n\
         [lines 1-3 of 400; continue with offset=4, or web_grep to search the page]"
    );

    // Following each trailer reads the whole page in order.
    let mut seen = String::new();
    let mut offset = 1;
    let mut calls = 0;
    loop {
        let part = ok(&fetch(json!({"url": url, "offset": offset, "limit": 2500})).await);
        let (body, trailer) = part.rsplit_once("\n[").unwrap();
        assert!(body.chars().count() <= 2500, "{}", body.len());
        seen.push_str(body);
        seen.push('\n');
        calls += 1;
        match trailer.split_once("continue with offset=") {
            Some((_, rest)) => offset = rest.split(',').next().unwrap().parse().unwrap(),
            None => {
                assert!(trailer.ends_with("-400 of 400]"), "{trailer}");
                break;
            }
        }
    }
    assert_eq!(seen, page);
    assert!(calls > 3, "{calls}");

    // `read` takes the same address; its `limit` counts lines.
    let read = web
        .read
        .call(json!({"path": url, "offset": 199, "limit": 3}), &bench.ctx())
        .await;
    assert_eq!(
        ok(&read),
        "line 199: nothing to report\nline 200: Release 4 shipped\nline 201: nothing to report\n\
         [lines 199-201 of 400; continue with offset=202, or web_grep to search the page]"
    );
    let tail = web
        .read
        .call(json!({"path": url, "offset": 400}), &bench.ctx())
        .await;
    assert_eq!(ok(&tail), "line 400: Release 8 shipped\n[lines 400-400 of 400]");

    let past = fetch(json!({"url": url, "offset": 401})).await;
    assert_eq!(
        failed(&past),
        "offset 401 is past the end of the page (400 lines)"
    );
    let past = web
        .read
        .call(json!({"path": url, "offset": 999}), &bench.ctx())
        .await;
    assert_eq!(
        failed(&past),
        "offset 999 is past the end of the page (400 lines)"
    );
}

#[tokio::test]
async fn web_grep_finds_lines_in_the_page_web_fetch_cached() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/log.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string(long_page()))
        .expect(1)
        .mount(&server)
        .await;
    let bench = bench();
    let web = web();
    let url = format!("{}/log.txt", server.uri());
    let grep = |input: Value| {
        let (web, ctx) = (&web, bench.ctx());
        let mut input = input;
        input["url"] = json!(url);
        async move { web.grep.call(input, &ctx).await }
    };
    ok(&web
        .fetch
        .call(json!({"url": url, "limit": 100}), &bench.ctx())
        .await);

    // A regular expression, with the line numbers `web_fetch` takes as `offset`.
    let releases = grep(json!({"pattern": r"Release [1-3] "})).await;
    assert_eq!(
        ok(&releases),
        "50: line 50: Release 1 shipped\n100: line 100: Release 2 shipped\n150: line 150: Release 3 shipped"
    );
    assert_eq!(releases.details, Some(json!({"matches": 3, "lines": 400})));
    let at_match = ok(&web
        .fetch
        .call(json!({"url": url, "offset": 100, "limit": 30}), &bench.ctx())
        .await);
    assert!(
        at_match.starts_with("line 100: Release 2 shipped\n["),
        "{at_match}"
    );

    // Context lines, with a separator between stretches that are apart.
    let around = grep(json!({"pattern": "Release [12] ", "context": 1})).await;
    assert_eq!(
        ok(&around),
        "49- line 49: nothing to report\n50: line 50: Release 1 shipped\n51- line 51: nothing to report\n\
         --\n\
         99- line 99: nothing to report\n100: line 100: Release 2 shipped\n101- line 101: nothing to report"
    );
    // More context than allowed is the most allowed: ten lines on each side.
    let wide = ok(&grep(json!({"pattern": "Release 4", "context": 500})).await);
    assert_eq!(wide.lines().count(), 21, "{wide}");
    assert!(wide.starts_with("190- ") && wide.contains("\n200: "), "{wide}");

    // Case and literal text.
    assert_eq!(
        ok(&grep(json!({"pattern": "release 8"})).await),
        "no matches for \"release 8\" (400 lines scanned)"
    );
    assert_eq!(
        ok(&grep(json!({"pattern": "release 8", "ignore_case": true})).await),
        "400: line 400: Release 8 shipped"
    );
    // As a regular expression the dot is any character (lines 10 to 19); as text it is a dot.
    let regex = ok(&grep(json!({"pattern": "line 1.: ", "limit": 100})).await);
    assert_eq!(regex.lines().count(), 10, "{regex}");
    assert_eq!(
        ok(&grep(json!({"pattern": "line 1.: ", "literal": true})).await),
        "no matches for \"line 1.: \" (400 lines scanned)"
    );
    assert_eq!(
        ok(&grep(json!({"pattern": "(shipped", "literal": true})).await),
        "no matches for \"(shipped\" (400 lines scanned)"
    );

    // The match limit: 25 unless named, and never more than 100.
    let default = ok(&grep(json!({"pattern": "nothing"})).await);
    assert_eq!(
        default.lines().filter(|line| line.contains(": line ")).count(),
        25
    );
    assert!(
        default.ends_with(
            "\n[stopped at 25 matches; the next is on line 26: narrow the pattern or raise limit (at most 100)]"
        ),
        "{default}"
    );
    let two = ok(&grep(json!({"pattern": "Release", "limit": 2})).await);
    assert_eq!(
        two,
        "50: line 50: Release 1 shipped\n100: line 100: Release 2 shipped\n\
         [stopped at 2 matches; the next is on line 150: narrow the pattern or raise limit (at most 100)]"
    );
    let most = ok(&grep(json!({"pattern": "nothing", "limit": 5000})).await);
    assert!(
        most.contains("[stopped at 100 matches; the next is on line 103:"),
        "{most}"
    );

    // A pattern that is not a regular expression fails before anything is fetched.
    let bad = WebGrepTool::new(Arc::new(Fetcher::new().unwrap()))
        .call(
            json!({"url": "http://127.0.0.1:9/never", "pattern": "(shipped"}),
            &bench.ctx(),
        )
        .await;
    let reason = failed(&bad);
    assert!(reason.starts_with("invalid pattern \"(shipped\""), "{reason}");
    assert!(
        reason.ends_with("set literal=true to search for the text as written"),
        "{reason}"
    );
    assert_eq!(failed(&grep(json!({"pattern": ""})).await), "pattern is empty");

    // It is a fetch of the page's host.
    let input = json!({"url": "https://Docs.RS/tokio", "pattern": "x"});
    assert_eq!(web.grep.access(&input), Access::Net);
    assert_eq!(web.grep.subjects(&input), ["domain:docs.rs"]);
}

#[tokio::test]
async fn web_grep_searches_the_page_in_the_format_asked_for() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/blog/borrow-checker"))
        .respond_with(html_body(ARTICLE))
        .expect(2)
        .mount(&server)
        .await;
    let bench = bench();
    let web = web();
    let url = format!("{}/blog/borrow-checker", server.uri());

    // The article as Markdown: the heading is a line of its own, and the page chrome is gone.
    let heading = ok(&web
        .grep
        .call(
            json!({"url": url, "pattern": "^#+ Moves and copies$"}),
            &bench.ctx(),
        )
        .await);
    let (number, line) = heading.split_once(": ").unwrap();
    assert_eq!(line, "## Moves and copies");
    let from_there = ok(&web
        .fetch
        .call(
            json!({"url": url, "offset": number.parse::<usize>().unwrap(), "limit": 19}),
            &bench.ctx(),
        )
        .await);
    assert!(from_there.starts_with("## Moves and copies\n["), "{from_there}");
    let chrome = json!({"url": url, "pattern": "TRACKING_SNIPPET_MARKER", "literal": true});
    assert!(ok(&web.grep.call(chrome.clone(), &bench.ctx()).await).starts_with("no matches for"));

    // The body as sent still has it.
    let mut raw = chrome;
    raw["format"] = json!("raw");
    let in_raw = ok(&web.grep.call(raw, &bench.ctx()).await);
    assert!(in_raw.contains("TRACKING_SNIPPET_MARKER"), "{in_raw}");
}

/// A backend that answers after a pause, fails for `broken`, and records how many searches ran
/// at once.
#[derive(Default, Clone)]
struct SlowBackend {
    running: Arc<std::sync::atomic::AtomicUsize>,
    most: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl SearchBackend for SlowBackend {
    fn name(&self) -> &'static str {
        "slow"
    }

    fn base_url(&self) -> &str {
        "https://search.invalid"
    }

    async fn search(&self, _: &reqwest::Client, query: &SearchQuery) -> Result<Vec<SearchHit>, String> {
        use std::sync::atomic::Ordering;
        let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(running, Ordering::SeqCst);
        // The first query takes longest, so it finishes after the ones behind it.
        let pause = if query.query == "first" { 150 } else { 40 };
        tokio::time::sleep(Duration::from_millis(pause)).await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        if query.query == "broken" {
            return Err("slow search returned HTTP 500".to_string());
        }
        Ok((1..=query.limit + 3)
            .map(|n| SearchHit {
                title: format!("{} result {n}", query.query),
                url: format!("https://example.org/{}/{n}", query.query),
                snippet: String::new(),
            })
            .collect())
    }
}

#[tokio::test]
async fn several_queries_run_three_at_a_time_and_answer_in_the_order_asked() {
    let backend = SlowBackend::default();
    let bench = bench();

    let out = bench
        .search(
            backend.clone(),
            json!({"query": "first", "queries": ["second", "broken", "first", "fourth", "fifth"], "limit": 2}),
        )
        .await;
    assert_eq!(
        ok(&out),
        "## first\n\
         1. first result 1\n   https://example.org/first/1\n\n\
         2. first result 2\n   https://example.org/first/2\n\n\
         ## second\n\
         1. second result 1\n   https://example.org/second/1\n\n\
         2. second result 2\n   https://example.org/second/2\n\n\
         ## broken\n\
         slow search returned HTTP 500\n\n\
         ## fourth\n\
         1. fourth result 1\n   https://example.org/fourth/1\n\n\
         2. fourth result 2\n   https://example.org/fourth/2\n\n\
         ## fifth\n\
         1. fifth result 1\n   https://example.org/fifth/1\n\n\
         2. fifth result 2\n   https://example.org/fifth/2"
    );
    assert_eq!(backend.most.load(std::sync::atomic::Ordering::SeqCst), 3);
    let details = out.details.unwrap();
    assert_eq!(details["backend"], "slow");
    let queries = details["queries"].as_array().unwrap();
    assert_eq!(queries.len(), 5);
    assert_eq!(queries[1]["query"], "second");
    assert_eq!(queries[1]["results"].as_array().unwrap().len(), 2);
    assert_eq!(
        queries[2],
        json!({"query": "broken", "backend": "slow", "error": "slow search returned HTTP 500"})
    );

    // When every query fails the call has failed.
    let none = bench
        .search(backend.clone(), json!({"queries": ["broken", " broken "]}))
        .await;
    assert_eq!(failed(&none), "slow search returned HTTP 500");
    let six = bench
        .search(
            backend.clone(),
            json!({"queries": ["a", "b", "c", "d", "e", "f"]}),
        )
        .await;
    assert_eq!(
        failed(&six),
        "6 queries in one call; at most 5 are searched at once"
    );
    assert_eq!(failed(&bench.search(backend, json!({})).await), "query is empty");
}

#[tokio::test]
async fn one_backend_request_failing_leaves_the_other_queries_their_results() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/res/v1/web/search"))
        .and(query_param("q", "tokio select macro"))
        .respond_with(json_body(BRAVE))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/res/v1/web/search"))
        .and(query_param("q", "rate limited"))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/res/v1/web/search"))
        .and(query_param("q", "plain"))
        .respond_with(json_body(r#"{"type": "search", "query": {"original": "plain"}}"#))
        .expect(1)
        .mount(&server)
        .await;
    let bench = bench();

    let out = bench
        .search(
            Brave::new(server.uri(), "brave-key"),
            json!({"queries": ["tokio select macro", "rate limited", "plain"], "limit": 1}),
        )
        .await;
    let text = ok(&out);
    let sections: Vec<&str> = text.split("\n\n## ").collect();
    assert_eq!(sections.len(), 3, "{text}");
    assert_eq!(
        sections[0],
        "## tokio select macro\n\
         1. select in tokio - Rust\n   \
         https://docs.rs/tokio/latest/tokio/macro.select.html\n   \
         Waits on multiple concurrent branches, returning when the first branch completes, cancelling the \
         remaining branches."
    );
    assert!(
        sections[1].starts_with("rate limited\nbrave search failed with HTTP 429"),
        "{text}"
    );
    assert_eq!(sections[2], "plain\nno results for \"plain\"");

    // Every request failing is a failed call that still names each query.
    let all = bench
        .search(
            Brave::new(format!("{}/nowhere", server.uri()), "k"),
            json!({"queries": ["one", "two"]}),
        )
        .await;
    let reason = failed(&all);
    assert!(
        reason.starts_with("## one\nbrave search failed with HTTP 404"),
        "{reason}"
    );
    assert!(
        reason.contains("\n\n## two\nbrave search failed with HTTP 404"),
        "{reason}"
    );
}

#[tokio::test]
async fn a_body_at_the_limit_is_returned_and_one_byte_more_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/exact"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![b'a'; MAX_BODY_BYTES], "text/plain"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/over"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![b'a'; MAX_BODY_BYTES + 1], "text/plain"))
        .mount(&server)
        .await;
    let bench = bench();
    let tool = fetch_tool();

    // It is one line of 5 MB: the largest window shows its start and says how much there is.
    let exact = tool
        .call(
            json!({"url": format!("{}/exact", server.uri()), "limit": 10_000_000}),
            &bench.ctx(),
        )
        .await;
    let exact = ok(&exact);
    let (shown, trailer) = exact.split_once('\n').unwrap();
    assert_eq!(shown.len(), 200_000);
    assert_eq!(
        trailer,
        format!(
            "[lines 1-1 of 1; line 1 is cut at 200000 of {MAX_BODY_BYTES} characters; web_grep searches all \
             of it]"
        )
    );
    let over = tool
        .call(json!({"url": format!("{}/over", server.uri())}), &bench.ctx())
        .await;
    assert!(failed(&over).contains("5 MB limit"));
}

#[tokio::test]
async fn five_redirects_are_followed_and_a_sixth_is_refused() {
    let server = MockServer::start().await;
    // Every redirect status, with relative and absolute locations.
    for (hop, status) in [301, 302, 303, 307, 308].into_iter().enumerate() {
        let location = if hop % 2 == 0 {
            format!("/hop/{}", hop + 1)
        } else {
            format!("{}/hop/{}", server.uri(), hop + 1)
        };
        Mock::given(method("GET"))
            .and(path(format!("/hop/{hop}")))
            .respond_with(redirect(status, location))
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/hop/5"))
        .respond_with(ResponseTemplate::new(200).set_body_string("arrived"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(redirect(302, "/hop/0"))
        .mount(&server)
        .await;
    let bench = bench();
    let tool = fetch_tool();

    let five = tool
        .call(json!({"url": format!("{}/hop/0", server.uri())}), &bench.ctx())
        .await;
    assert_eq!(ok(&five), "arrived");
    let six = tool
        .call(json!({"url": format!("{}/start", server.uri())}), &bench.ctx())
        .await;
    assert!(failed(&six).contains("redirects"));
}

#[tokio::test]
async fn link_local_and_metadata_addresses_are_refused_directly_and_behind_a_redirect() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jump"))
        .respond_with(redirect(302, "http://169.254.169.254/latest/meta-data/"))
        .expect(1)
        .mount(&server)
        .await;
    let bench = bench();
    let tool = fetch_tool();

    for url in [
        "http://169.254.169.254/latest/meta-data/",
        "http://169.254.10.20:8080/",
        "http://2852039166/",
        "http://[fe80::1]/",
        "https://[::ffff:169.254.169.254]/",
        "http://[fd00:ec2::254]/latest/meta-data/",
    ] {
        // Refused from the URL alone: an attempt to connect would outlast this deadline or fail
        // with a different message.
        let out = tokio::time::timeout(
            Duration::from_secs(5),
            tool.call(json!({"url": url}), &bench.ctx()),
        )
        .await
        .unwrap();
        let reason = failed(&out);
        assert!(reason.contains("link-local or cloud-metadata"), "{url}: {reason}");
    }

    let redirected = tool
        .call(json!({"url": format!("{}/jump", server.uri())}), &bench.ctx())
        .await;
    let reason = failed(&redirected);
    assert!(reason.contains("169.254.169.254"), "{reason}");
    assert!(reason.contains("link-local or cloud-metadata"), "{reason}");
}

#[tokio::test]
async fn a_repeated_fetch_is_answered_from_the_cache() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("first answer"))
        .expect(1)
        .mount(&server)
        .await;
    let bench = bench();
    let tool = fetch_tool();
    let url = format!("{}/page", server.uri());

    let first = tool.call(json!({"url": url}), &bench.ctx()).await;
    // The fragment never reaches the server, so it is the same page.
    let second = tool
        .call(json!({"url": format!("{url}#section")}), &bench.ctx())
        .await;
    assert_eq!(ok(&first), "first answer");
    assert_eq!(ok(&second), "first answer");
}

#[tokio::test]
async fn failures_are_not_cached_and_report_the_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/flaky"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/flaky"))
        .respond_with(ResponseTemplate::new(200).set_body_string("recovered"))
        .mount(&server)
        .await;
    let bench = bench();
    let tool = fetch_tool();
    let url = format!("{}/flaky", server.uri());

    let down = tool.call(json!({"url": url}), &bench.ctx()).await;
    assert!(failed(&down).contains("503"));
    let up = tool.call(json!({"url": url}), &bench.ctx()).await;
    assert_eq!(ok(&up), "recovered");

    let missing = tool
        .call(json!({"url": format!("{}/missing", server.uri())}), &bench.ctx())
        .await;
    assert!(failed(&missing).contains("404"));
}

#[tokio::test]
async fn only_http_and_https_urls_are_fetched() {
    let bench = bench();
    let tool = fetch_tool();
    for url in [
        "file:///etc/passwd",
        "ftp://example.com/readme.txt",
        "data:text/plain,hello",
        "gopher://example.com/",
    ] {
        let out = tool.call(json!({"url": url}), &bench.ctx()).await;
        let reason = failed(&out);
        assert!(reason.contains("only http and https"), "{url}: {reason}");
    }
    let relative = tool.call(json!({"url": "example.com/page"}), &bench.ctx()).await;
    assert!(failed(&relative).contains("invalid URL"));
}

#[tokio::test]
async fn the_extension_registers_its_tools_and_url_schemes_that_read_as_markdown() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/blog/borrow-checker"))
        .respond_with(html_body(ARTICLE))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("format", "json"))
        .respond_with(json_body(SEARXNG))
        .expect(1)
        .mount(&server)
        .await;
    let bench = bench();
    let registry = Registry::new(ExtCtx {
        services: bench.services.clone(),
        cwd: bench.dir.path().into(),
        session_id: "s".into(),
    });
    registry
        .load(&WebExt {
            search: "searxng".into(),
            searxng_url: server.uri(),
            provider_search: true,
        })
        .unwrap();
    let ctx = bench.ctx();

    let names: Vec<String> = registry
        .tools
        .tools(&ctx.agent)
        .iter()
        .map(|tool| tool.name().to_string())
        .collect();
    assert_eq!(names, ["web_search", "web_fetch", "web_grep"]);
    assert!(registry.schemes.get("https").is_some());

    // `read http://…` hands the scheme everything after `http://`.
    let rest = format!("{}/blog/borrow-checker", server.uri());
    let rest = rest.strip_prefix("http://").unwrap();
    let blocks = registry
        .schemes
        .get("http")
        .unwrap()
        .read(rest, &ctx)
        .await
        .unwrap();
    let [Block::Text { text: page }] = blocks.as_slice() else {
        panic!("expected one text block");
    };
    assert!(has_heading(page, ARTICLE_TITLE), "{page}");
    assert!(page.contains(ARTICLE_SENTENCES[0]), "{page}");

    // The scheme and the tool share one cache: the page is requested once.
    let fetch = registry.tools.get(&ctx.agent, "web_fetch").unwrap();
    let again = fetch.call(json!({"url": format!("http://{rest}")}), &ctx).await;
    assert_eq!(&ok(&again), page);

    // So does `web_grep`.
    let grep = registry.tools.get(&ctx.agent, "web_grep").unwrap();
    let found = grep
        .call(
            json!({"url": format!("http://{rest}"), "pattern": "^#+ Moves and copies$"}),
            &ctx,
        )
        .await;
    assert!(ok(&found).ends_with(": ## Moves and copies"), "{}", ok(&found));

    let search = registry.tools.get(&ctx.agent, "web_search").unwrap();
    let found = search.call(json!({"query": "wiremock"}), &ctx).await;
    assert!(ok(&found).starts_with("1. wiremock - Rust\n"));
}

#[test]
fn reading_a_web_address_counts_as_network_access_to_its_host() {
    // The web schemes say what reading one of their addresses touches.
    let schemes = Arc::new(azazel_core::Schemes::default());
    let fetcher = Arc::new(azazel_tools::web::Fetcher::new().unwrap());
    schemes.add(
        "web",
        Arc::new(azazel_tools::web::UrlScheme::http(fetcher.clone())),
    );
    schemes.add("web", Arc::new(azazel_tools::web::UrlScheme::https(fetcher)));
    let read = azazel_tools::ReadTool::new(Arc::default(), schemes);
    let url = json!({ "path": "https://Docs.rs/tokio?search=x" });
    assert_eq!(read.access(&url), Access::Net);
    assert_eq!(read.subjects(&url), ["domain:docs.rs"]);
    // Files, and the harness's own schemes, stay plain reads of that path.
    for path in ["src/main.rs", "artifact://c1", "C:/Users/me/notes.txt"] {
        let input = json!({ "path": path });
        assert_eq!(read.access(&input), Access::Read, "{path}");
        assert_eq!(read.subjects(&input), [path]);
    }
}
