#![allow(clippy::unwrap_used)]

use std::borrow::Cow;
use std::sync::Arc;

use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, ToolOutput, UpdateSink};
use azazel_ai::services::REDACTED;
use azazel_ai::{Credentials, Service};
use azazel_core::ArtifactStore;
use azazel_tools::web::HttpTool;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{basic_auth, bearer_token, body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "tok_live_12345678";

/// Stands in for the display redactor: it hides one made-up kind of key.
fn hide_test_keys(text: &str) -> Cow<'_, str> {
    match text.contains("testkey-") {
        true => Cow::Owned(text.replace("testkey-0001", "[redacted:test-key]")),
        false => Cow::Borrowed(text),
    }
}

struct Bench {
    dir: tempfile::TempDir,
    credentials: Arc<Credentials>,
    services: Arc<Services>,
}

fn bench() -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let services = Arc::new(Services::new());
    services.insert(Arc::new(ArtifactStore::new(dir.path().join("artifacts"))));
    Bench {
        credentials: Arc::new(Credentials::new(dir.path().join("auth.json"))),
        dir,
        services,
    }
}

impl Bench {
    fn ctx(&self) -> ToolCtx {
        let agent = AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id: "s".into(),
        };
        ToolCtx {
            agent_id: agent.id.clone(),
            session_id: agent.session_id.clone(),
            agent,
            call_id: "c1".into(),
            cwd: self.dir.path().to_path_buf(),
            cancel: CancellationToken::new(),
            update: UpdateSink::default(),
            services: self.services.clone(),
        }
    }

    fn tool(&self) -> HttpTool {
        HttpTool::new(self.credentials.clone(), hide_test_keys).unwrap()
    }

    async fn call(&self, input: Value) -> ToolOutput {
        self.tool().call(input, &self.ctx()).await
    }

    /// The text of a call that must succeed.
    async fn ok(&self, input: Value) -> String {
        let output = self.call(input.clone()).await;
        assert!(!output.is_error, "{input}: {}", output.text_content());
        output.text_content()
    }

    /// The text of a call that must fail.
    async fn refused(&self, input: Value) -> String {
        let output = self.call(input.clone()).await;
        assert!(output.is_error, "{input}: {}", output.text_content());
        output.text_content()
    }
}

/// `host:port` of a mock server, as a service names it.
fn host(server: &MockServer) -> String {
    server.uri().strip_prefix("http://").unwrap().to_string()
}

#[tokio::test]
async fn the_stored_header_is_sent_for_its_host_and_never_comes_back() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(bearer_token(TOKEN))
        .and(header("x-trace", "7"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-echo", format!("Bearer {TOKEN}"))
                .insert_header("set-cookie", "sid=sess-abcdef; Path=/; HttpOnly")
                .set_body_raw(
                    format!(r#"{{"login":"me","sent":"Bearer {TOKEN}","other":"testkey-0001"}}"#),
                    "application/json",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;
    let bench = bench();
    bench
        .credentials
        .store_service(
            "acme",
            Service::header(&host(&server), &format!("Authorization: Bearer {TOKEN}")).unwrap(),
        )
        .unwrap();

    // The agent's own header of that name does not get in the way.
    let input = json!({
        "url": format!("{}/user", server.uri()),
        "headers": {"authorization": "Bearer made-up", "X-Trace": "7"},
    });
    let output = bench.call(input.clone()).await;
    let text = output.text_content();
    assert!(!output.is_error, "{text}");
    assert!(
        text.starts_with(&format!(
            "HTTP 200 OK · GET {}/user · service acme\n",
            server.uri()
        )),
        "{text}"
    );
    assert!(text.contains("content-type: application/json\n"), "{text}");
    assert!(text.contains(&format!("x-echo: {REDACTED}\n")), "{text}");
    // A session opened with the credential is not handed over either.
    assert!(
        text.contains(&format!("set-cookie: sid={REDACTED}; Path=/; HttpOnly\n")),
        "{text}"
    );
    assert!(
        text.ends_with(&format!(
            r#"{{"login":"me","sent":"{REDACTED}","other":"[redacted:test-key]"}}"#
        )),
        "{text}"
    );
    assert!(!text.contains(TOKEN) && !text.contains("sess-abcdef"), "{text}");
    let details = output.details.unwrap();
    assert_eq!(details["status"], 200);
    assert_eq!(details["service"], "acme");
    assert_eq!(details["redacted"], 4);

    // Permission rules see the host and the credential the request would carry.
    let tool = bench.tool();
    assert_eq!(tool.subjects(&input), ["domain:127.0.0.1", "auth:acme"]);
    let note = tool.approval_note(&input).unwrap();
    assert!(note.contains("`acme`") && !note.contains(TOKEN), "{note}");
    assert!(tool.description().contains(&format!("acme ({})", host(&server))));
    assert!(!tool.description().contains(TOKEN));
}

#[tokio::test]
async fn a_request_to_a_host_without_a_service_carries_nothing_and_is_returned_as_it_is() {
    let server = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("key: testkey-0001", "text/plain"))
        .mount(&server)
        .await;
    let bench = bench();
    bench
        .credentials
        .store_service(
            "elsewhere",
            Service::header(&host(&other), &format!("Authorization: Bearer {TOKEN}")).unwrap(),
        )
        .unwrap();

    let input = json!({"url": format!("{}/open", server.uri())});
    let output = bench.call(input.clone()).await;
    // What only a request with a stored credential has hidden stays: the session's own
    // redaction setting decides about the rest.
    assert!(
        output.text_content().ends_with("\nkey: testkey-0001"),
        "{}",
        output.text_content()
    );
    assert_eq!(output.details.unwrap(), json!({"status": 200}));
    let tool = bench.tool();
    assert_eq!(tool.subjects(&input), ["domain:127.0.0.1"]);
    assert_eq!(tool.approval_note(&input), None);

    // A credential goes to the host it was stored for, whatever the agent names.
    let named = bench
        .refused(json!({"url": format!("{}/open", server.uri()), "service": "elsewhere"}))
        .await;
    assert!(named.contains("is not sent to 127.0.0.1"), "{named}");
    let unknown = bench
        .refused(json!({"url": format!("{}/open", server.uri()), "service": "nope"}))
        .await;
    assert!(unknown.contains("stored: elsewhere"), "{unknown}");

    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert!(!received[0].headers.contains_key("authorization"));
    assert!(other.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_stored_query_parameter_replaces_the_agents_and_stays_out_of_the_address_shown() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "a b"))
        .and(query_param("key", "k/secret 123"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("you asked /search?q=a+b&key=k%2Fsecret+123", "text/plain"),
        )
        .expect(2)
        .mount(&server)
        .await;
    let bench = bench();
    bench
        .credentials
        .store_service(
            "maps",
            Service::query(&host(&server), "key=k/secret 123").unwrap(),
        )
        .unwrap();

    let text = bench
        .ok(json!({"url": format!("{}/search?q=a%20b&key=mine", server.uri())}))
        .await;
    assert!(
        text.starts_with(&format!(
            "HTTP 200 OK · GET {}/search?q=a%20b&key=mine · service maps\n",
            server.uri()
        )),
        "{text}"
    );
    assert!(
        text.ends_with(&format!("you asked /search?q=a+b&key={REDACTED}")),
        "{text}"
    );
    // The other parameters of the address are sent as the agent wrote them.
    bench
        .ok(json!({"url": format!("{}/search?q=a%20b", server.uri())}))
        .await;
    let received = server.received_requests().await.unwrap();
    assert_eq!(received[1].url.query(), Some("q=a%20b&key=k%2Fsecret+123"));
}

#[tokio::test]
async fn basic_authentication_and_a_json_body_are_sent_and_none_stops_the_credential() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/items"))
        .and(basic_auth("me", "hunter2-pass"))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({"name": "widget", "tags": ["a", "b"]})))
        .respond_with(ResponseTemplate::new(201).set_body_raw(r#"{"id":7}"#, "application/json"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(401).set_body_raw("who are you", "text/plain"))
        .mount(&server)
        .await;
    let bench = bench();
    bench
        .credentials
        .store_service("shop", Service::basic(&host(&server), "me:hunter2-pass").unwrap())
        .unwrap();

    let created = bench
        .ok(json!({
            "method": "post",
            "url": format!("{}/items", server.uri()),
            "json": {"name": "widget", "tags": ["a", "b"]},
        }))
        .await;
    assert!(created.starts_with("HTTP 201 Created · POST "), "{created}");
    assert!(created.ends_with("\n{\"id\":7}"), "{created}");

    // A refusal is a result, not a failure of the tool, and says the credential was the one sent.
    let refused = bench
        .ok(json!({"method": "DELETE", "url": format!("{}/items/7", server.uri())}))
        .await;
    assert!(
        refused.starts_with("HTTP 401 Unauthorized · DELETE "),
        "{refused}"
    );
    assert!(
        refused.ends_with("who are you\n[the stored credential of `shop` was sent and refused]"),
        "{refused}"
    );

    let input = json!({"method": "DELETE", "url": format!("{}/items/8", server.uri()), "service": "none"});
    let bare = bench.ok(input.clone()).await;
    assert!(bare.ends_with("who are you"), "{bare}");
    assert_eq!(bench.tool().subjects(&input), ["domain:127.0.0.1"]);
    let received = server.received_requests().await.unwrap();
    assert!(received[1].headers.contains_key("authorization"));
    assert!(!received[2].headers.contains_key("authorization"));
}

#[tokio::test]
async fn a_redirect_is_followed_on_the_host_only_and_only_for_a_read() {
    let server = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/old"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/new"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/new"))
        .and(bearer_token(TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_raw("moved here", "text/plain"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/away"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/landing?t={TOKEN}", other.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/old"))
        .respond_with(ResponseTemplate::new(303).insert_header("location", "/new"))
        .mount(&server)
        .await;
    let bench = bench();
    bench
        .credentials
        .store_service(
            "acme",
            Service::header(&host(&server), &format!("Authorization: Bearer {TOKEN}")).unwrap(),
        )
        .unwrap();

    let followed = bench.ok(json!({"url": format!("{}/old", server.uri())})).await;
    assert!(
        followed.starts_with(&format!(
            "HTTP 200 OK · GET {}/new · service acme\n",
            server.uri()
        )),
        "{followed}"
    );
    assert!(followed.ends_with("\nmoved here"), "{followed}");

    // Another host would get neither the request nor the credential; the agent is told where
    // the redirect leads, without the credential the server wrote into it.
    let away = bench.ok(json!({"url": format!("{}/away", server.uri())})).await;
    assert!(away.starts_with("HTTP 307 Temporary Redirect · GET "), "{away}");
    assert!(
        away.ends_with(&format!(
            "[redirects to {}/landing?t={REDACTED}; not followed: only a GET or HEAD that stays on the host is]",
            other.uri()
        )),
        "{away}"
    );
    assert!(!away.contains(TOKEN), "{away}");
    assert!(other.received_requests().await.unwrap().is_empty());

    let posted = bench
        .ok(json!({"method": "POST", "url": format!("{}/old", server.uri()), "body": "x"}))
        .await;
    assert!(posted.starts_with("HTTP 303 See Other · POST "), "{posted}");
    assert!(posted.contains("[redirects to "), "{posted}");
}

#[tokio::test]
async fn a_long_body_is_cut_and_saved_whole_and_other_content_is_saved_to_a_file() {
    let server = MockServer::start().await;
    let long: String = (0..400).map(|n| format!("row {n} {TOKEN}\n")).collect();
    Mock::given(method("GET"))
        .and(path("/rows"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(long.clone(), "text/plain"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/logo.png"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(vec![0x89u8, b'P', b'N', b'G', 0, 1, 2], "image/png"),
        )
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let bench = bench();
    bench
        .credentials
        .store_service(
            "acme",
            Service::header(&host(&server), &format!("X-Api-Key: {TOKEN}")).unwrap(),
        )
        .unwrap();

    let text = bench
        .ok(json!({"url": format!("{}/rows", server.uri()), "limit": 100}))
        .await;
    let whole = long.replace(TOKEN, REDACTED);
    let (_, body) = text.split_once("\n\n").unwrap();
    let (shown, trailer) = body.rsplit_once('\n').unwrap();
    assert_eq!(shown, whole.chars().take(100).collect::<String>());
    let saved = bench.dir.path().join("artifacts").join("c1-response.txt");
    assert_eq!(
        trailer,
        format!(
            "[cut at 100 of {} characters; the whole body is in {}]",
            whole.chars().count(),
            saved.display()
        )
    );
    // The file the agent may read next holds no more of the credential than the result does.
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), whole);

    let image = bench
        .ok(json!({"url": format!("{}/logo.png", server.uri())}))
        .await;
    let logo = bench.dir.path().join("artifacts").join("c1-logo.png");
    assert!(
        image.ends_with(&format!(
            "{}/logo.png is image/png, not text (7 bytes). Saved to {}",
            server.uri(),
            logo.display()
        )),
        "{image}"
    );
    assert_eq!(std::fs::read(&logo).unwrap().len(), 7);

    // No body, no blank line after the headers.
    let head = bench
        .ok(json!({"method": "HEAD", "url": format!("{}/rows", server.uri())}))
        .await;
    assert!(head.starts_with("HTTP 204 No Content · HEAD "), "{head}");
    assert!(!head.contains("\n\n"), "{head}");
}

#[tokio::test]
async fn a_request_that_cannot_be_sent_says_why() {
    let bench = bench();
    let method = bench
        .refused(json!({"method": "TRACE", "url": "http://127.0.0.1:9/"}))
        .await;
    assert!(
        method.contains("GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS"),
        "{method}"
    );
    let both = bench
        .refused(json!({"url": "http://127.0.0.1:9/", "body": "x", "json": {"a": 1}}))
        .await;
    assert_eq!(both, "give `body` or `json`, not both");
    let relative = bench.refused(json!({"url": "/v1/items"})).await;
    assert!(relative.starts_with("invalid URL \"/v1/items\""), "{relative}");
    let file = bench.refused(json!({"url": "file:///etc/passwd"})).await;
    assert!(file.contains("only http and https URLs"), "{file}");
    let metadata = bench
        .refused(json!({"url": "http://169.254.169.254/latest/meta-data/"}))
        .await;
    assert!(metadata.contains("link-local or cloud-metadata"), "{metadata}");
    let header = bench
        .refused(json!({"url": "http://127.0.0.1:9/", "headers": {"bad name": "x"}}))
        .await;
    assert_eq!(header, "`bad name` is not a header name");
    // Nothing listens there: the failure names the request, not a status.
    let down = bench.refused(json!({"url": "http://127.0.0.1:9/x"})).await;
    assert!(down.starts_with("GET http://127.0.0.1:9/x failed: "), "{down}");
}
