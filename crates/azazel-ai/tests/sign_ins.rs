#![allow(clippy::unwrap_used)]

//! The built-in sign-ins of ChatGPT without a browser, GitHub Copilot Enterprise, DigitalOcean,
//! Poe and Snowflake, and Azure through the Azure CLI, each against a stand-in authorisation
//! server: the presets with their addresses moved to a local mock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use azazel_ai::credentials::now_ms;
use azazel_ai::oauth::login_with;
use azazel_ai::oauth::map::NEVER;
use azazel_ai::providers::azure::{self, AzureCli};
use azazel_ai::providers::http::{HttpProvider, http_client};
use azazel_ai::providers::presets::{self, AuthStyle, ProviderConfig};
use azazel_ai::{Auth, Credential, Credentials, LoginUi, Message, ModelInfo, OAuthSettings, Provider, Request, WireApi};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request as Received, ResponseTemplate};

/// `settings` with every address that starts with `from` starting with `to` instead.
fn moved(mut settings: OAuthSettings, from: &str, to: &str) -> OAuthSettings {
    let swap = |text: &mut String| {
        if let Some(rest) = text.strip_prefix(from) {
            *text = format!("{to}{rest}");
        }
    };
    swap(&mut settings.authorize_url);
    swap(&mut settings.token_url);
    swap(&mut settings.device_url);
    swap(&mut settings.device_poll_url);
    for text in [&mut settings.verification_url, &mut settings.api_base_url]
        .into_iter()
        .flatten()
    {
        swap(text);
    }
    settings
}

fn builtin(id: &str) -> ProviderConfig {
    presets::presets_with(&|_| None)
        .into_iter()
        .find(|provider| provider.id == id)
        .unwrap()
}

/// The pairs of a query string or a form body.
fn pairs(encoded: &str) -> BTreeMap<String, String> {
    reqwest::Url::parse(&format!("http://x/?{encoded}"))
        .unwrap()
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

async fn received(server: &MockServer, at: &str) -> Vec<Received> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == at)
        .collect()
}

/// The person signing in: sees what is shown, and answers questions from `answers` in order.
struct Person {
    shown: mpsc::UnboundedSender<String>,
    answers: Mutex<Vec<String>>,
    asked: Mutex<Vec<String>>,
}

fn person(answers: &[&str]) -> (Person, mpsc::UnboundedReceiver<String>) {
    let (shown, seen) = mpsc::unbounded_channel();
    let person = Person {
        shown,
        answers: Mutex::new(answers.iter().rev().map(|a| a.to_string()).collect()),
        asked: Mutex::default(),
    };
    (person, seen)
}

#[async_trait]
impl LoginUi for Person {
    fn show(&self, text: &str) {
        let _ = self.shown.send(text.to_string());
    }

    async fn ask(&self, prompt: &str) -> Option<String> {
        self.asked.lock().unwrap().push(prompt.to_string());
        self.answers.lock().unwrap().pop()
    }
}

/// The query of the sign-in address, from the shown texts.
async fn sign_in_query(seen: &mut mpsc::UnboundedReceiver<String>) -> BTreeMap<String, String> {
    loop {
        let shown = seen.recv().await.unwrap();
        if let Some(url) = shown.strip_prefix("Sign in on this page:\n") {
            return pairs(url.split_once('?').unwrap().1);
        }
    }
}

fn jwt(claims: Value) -> String {
    let head = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let body = URL_SAFE_NO_PAD.encode(claims.to_string());
    format!("{head}.{body}.")
}

fn fields(credential: &Credential) -> &BTreeMap<String, Value> {
    match credential {
        Credential::OAuth { extra, .. } => extra,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn chatgpt_without_a_browser_exchanges_the_code_the_device_page_approves() {
    let server = MockServer::start().await;
    let issuer = server.uri();
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .and(body_partial_json(json!({"client_id": "app_EMoamEEZ73f0CkXaXp7hrann"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_auth_id": "da-1", "user_code": "ABCD-1234", "interval": "0"
        })))
        .mount(&server)
        .await;
    // Not entered yet, then approved.
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(ResponseTemplate::new(403))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "authorization_code": "ac-1", "code_verifier": "cv-1"
        })))
        .mount(&server)
        .await;
    let access = jwt(json!({
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-7"},
        "email": "me@example.com"
    }));
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": access, "refresh_token": "rt-1", "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;
    let settings = moved(
        presets::device_login("openai-codex").unwrap(),
        "https://auth.openai.com",
        &issuer,
    );
    let (person, mut seen) = person(&[]);
    let credential = login_with(
        &http_client(),
        &settings,
        &BTreeMap::new(),
        &person,
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    let Credential::OAuth { refresh, ref extra, .. } = credential else {
        panic!("{credential:?}");
    };
    assert_eq!(refresh, "rt-1");
    assert_eq!(extra["account_id"], "acct-7");
    let mut lines = Vec::new();
    while let Ok(line) = seen.try_recv() {
        lines.push(line);
    }
    assert!(
        lines.contains(&format!("Open {issuer}/codex/device and enter the code ABCD-1234")),
        "{lines:?}"
    );
    let polls = received(&server, "/api/accounts/deviceauth/token").await;
    assert_eq!(polls.len(), 2);
    let poll: Value = serde_json::from_slice(&polls[1].body).unwrap();
    assert_eq!(poll, json!({"device_auth_id": "da-1", "user_code": "ABCD-1234"}));
    let exchange = pairs(&String::from_utf8_lossy(&received(&server, "/oauth/token").await[0].body));
    assert_eq!(exchange["grant_type"], "authorization_code");
    assert_eq!(exchange["code"], "ac-1");
    assert_eq!(exchange["code_verifier"], "cv-1");
    // The exchange names OpenAI's own device callback, whatever server the test stands in for.
    assert_eq!(exchange["redirect_uri"], "https://auth.openai.com/deviceauth/callback");
    // The login renews the way the browser sign-in's does.
    let codex = builtin("openai-codex").oauth.unwrap();
    assert_eq!(codex.token_url, "https://auth.openai.com/oauth/token");
    assert!(presets::device_login("anthropic").is_none());
}

#[tokio::test]
async fn copilot_enterprise_signs_in_at_the_company_host_and_keeps_its_api_address() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ghe.example.com/login/device/code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "dc-1", "user_code": "WDJB-MJHT",
            "verification_uri": "https://ghe.example.com/login/device", "interval": 0
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/ghe.example.com/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "gho_enterprise", "token_type": "bearer"
        })))
        .mount(&server)
        .await;
    let settings = moved(
        presets::enterprise_login("github-copilot").unwrap(),
        "https://{domain}",
        &format!("{}/{{domain}}", server.uri()),
    );
    let (person, _seen) = person(&[]);
    // The domain is given as a user would paste it.
    let given = BTreeMap::from([("domain".to_string(), "https://ghe.example.com/".to_string())]);
    let credential = login_with(&http_client(), &settings, &given, &person, &CancellationToken::new())
        .await
        .unwrap();
    let extra = fields(&credential);
    assert_eq!(extra["domain"], "ghe.example.com");
    assert_eq!(extra["base_url"], "https://copilot-api.ghe.example.com");
    let Credential::OAuth { expires, .. } = credential else { unreachable!() };
    assert_eq!(expires, NEVER);

    // A domain that is not a host name never reaches an address.
    let bad = BTreeMap::from([("domain".to_string(), "evil.example/x?y".to_string())]);
    let refused = login_with(&http_client(), &settings, &bad, &person, &CancellationToken::new()).await;
    assert!(refused.unwrap_err().to_string().contains("not a valid domain"));
}

const SSE_DONE: &str = "data: [DONE]\n\n";

/// A Copilot login kept as an enterprise one would be, pointing at `server`.
fn copilot_with_login(server: &MockServer, dir: &Path) -> HttpProvider {
    let credentials = Credentials::with_env(dir.join("auth.json"), Arc::new(|_| None));
    credentials
        .store(
            "github-copilot",
            Credential::OAuth {
                access: "gho_enterprise".into(),
                refresh: String::new(),
                expires: NEVER,
                extra: BTreeMap::from([("base_url".to_string(), json!(server.uri()))]),
            },
        )
        .unwrap();
    HttpProvider::new(builtin("github-copilot"), http_client(), Arc::new(credentials))
}

#[tokio::test]
async fn copilot_asks_each_model_on_the_api_that_serves_it_at_the_login_address() {
    let server = MockServer::start().await;
    for at in ["/chat/completions", "/responses", "/v1/messages"] {
        Mock::given(method("POST"))
            .and(path(at))
            .and(header("authorization", "Bearer gho_enterprise"))
            .and(header("x-initiator", "user"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(SSE_DONE, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let copilot = copilot_with_login(&server, dir.path());
    let request = Request {
        messages: vec![Message::user("hi")],
        ..Request::default()
    };
    for model in ["gemini-2.5-pro", "gpt-5.2", "claude-sonnet-4.5"] {
        let stream = copilot
            .stream(
                &ModelInfo::unknown("github-copilot", model),
                &request,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let _: Vec<_> = stream.collect().await;
    }
    // The Messages route takes the GitHub login, which is no Claude subscription.
    let messages = &received(&server, "/v1/messages").await[0];
    let body: Value = serde_json::from_slice(&messages.body).unwrap();
    assert!(!body.to_string().contains("Claude Code"), "{body}");
    let routes = builtin("github-copilot");
    assert_eq!(routes.api_for("gpt-5-mini"), WireApi::OpenAiChat);
    assert_eq!(routes.api_for("gpt-4.1"), WireApi::OpenAiChat);
    assert_eq!(routes.api_for("grok-code-fast-1"), WireApi::OpenAiResponses);
}

#[tokio::test]
async fn digitalocean_reads_the_token_from_the_address_the_browser_returns_to() {
    let mut settings = builtin("digitalocean").oauth.unwrap();
    // Any free port: the real one is fixed, and tests must not hold one.
    settings.redirect_port = 0;
    let (person, mut seen) = person(&[]);
    let http = http_client();
    let browser = tokio::spawn({
        let http = http.clone();
        async move {
            let query = sign_in_query(&mut seen).await;
            let redirect = query["redirect_uri"].clone();
            // The server sends the token in the fragment, which the browser does not send.
            let first = http.get(&redirect).send().await.unwrap();
            assert_eq!(first.status(), 200);
            assert!(first.text().await.unwrap().contains("location.hash"));
            let landed = http
                .get(format!(
                    "{redirect}?access_token=do-token&token_type=bearer&expires_in=2592000&state={}",
                    query["state"]
                ))
                .send()
                .await
                .unwrap();
            assert_eq!(landed.status(), 200);
            query
        }
    });
    let before = now_ms();
    let credential = login_with(&http, &settings, &BTreeMap::new(), &person, &CancellationToken::new())
        .await
        .unwrap();
    let query = browser.await.unwrap();
    assert_eq!(query["response_type"], "token");
    assert_eq!(query["scope"], "genai:read inference:query");
    assert!(!query.contains_key("code_challenge"));
    assert!(query["redirect_uri"].starts_with("http://localhost:"));
    assert!(query["redirect_uri"].ends_with("/auth/callback"));
    let Credential::OAuth {
        access,
        refresh,
        expires,
        ..
    } = credential
    else {
        panic!("{credential:?}");
    };
    assert_eq!((access.as_str(), refresh.as_str()), ("do-token", ""));
    assert!(expires >= before + 2_592_000_000);
}

#[tokio::test]
async fn poe_exchanges_the_code_for_a_key_that_lasts_as_long_as_it_says() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "api_key": "poe-key-1", "api_key_expires_in": null
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "api_key": "poe-key-2", "api_key_expires_in": 60
        })))
        .mount(&server)
        .await;
    let mut settings = builtin("poe").oauth.unwrap();
    settings.token_url = format!("{}/token", server.uri());
    let http = http_client();
    let mut keys = Vec::new();
    for _ in 0..2 {
        let (person, mut seen) = person(&[]);
        let browser = tokio::spawn({
            let http = http.clone();
            async move {
                let query = sign_in_query(&mut seen).await;
                let redirect = &query["redirect_uri"];
                http.get(format!("{redirect}?code=pc-1&state={}", query["state"]))
                    .send()
                    .await
                    .unwrap();
                query
            }
        });
        let credential = login_with(&http, &settings, &BTreeMap::new(), &person, &CancellationToken::new())
            .await
            .unwrap();
        let query = browser.await.unwrap();
        assert_eq!(query["scope"], "apikey:create");
        assert_eq!(query["code_challenge_method"], "S256");
        assert!(query["redirect_uri"].starts_with("http://127.0.0.1:"));
        let Credential::OAuth { access, expires, .. } = credential else {
            panic!("{credential:?}");
        };
        keys.push((access, expires));
    }
    assert_eq!(keys[0], ("poe-key-1".to_string(), NEVER));
    assert_eq!(keys[1].0, "poe-key-2");
    assert!(keys[1].1 <= now_ms() + 60_000);
    let form = pairs(&String::from_utf8_lossy(&received(&server, "/token").await[0].body));
    assert_eq!(form["code"], "pc-1");
    assert_eq!(form["client_id"], "client_728290227fc048cc9262091a1ea197ea");
    assert!(form.contains_key("code_verifier"));
}

#[tokio::test]
async fn snowflake_signs_in_at_the_account_it_is_given_and_renews_there() {
    let server = MockServer::start().await;
    let basic = "Basic TE9DQUxfQVBQTElDQVRJT046TE9DQUxfQVBQTElDQVRJT04=";
    Mock::given(method("POST"))
        .and(path("/myorg-acct/oauth/token-request"))
        .and(header("authorization", basic))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "sf-1", "refresh_token": "sf-r", "expires_in": 600
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/myorg-acct/oauth/token-request"))
        .and(header("authorization", basic))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "sf-2", "expires_in": 600
        })))
        .mount(&server)
        .await;
    let settings = moved(
        builtin("snowflake").oauth.unwrap(),
        "https://{account}.snowflakecomputing.com",
        &format!("{}/{{account}}", server.uri()),
    );
    // The account comes from the person, who pastes the whole host; the role is left empty.
    let (person, mut seen) = person(&["myorg-acct.snowflakecomputing.com", ""]);
    let http = http_client();
    let browser = tokio::spawn({
        let http = http.clone();
        async move {
            let query = sign_in_query(&mut seen).await;
            let redirect = &query["redirect_uri"];
            http.get(format!("{redirect}?code=sc-1&state={}", query["state"]))
                .send()
                .await
                .unwrap();
            query
        }
    });
    let credential = login_with(&http, &settings, &BTreeMap::new(), &person, &CancellationToken::new())
        .await
        .unwrap();
    let query = browser.await.unwrap();
    assert_eq!(person.asked.lock().unwrap().len(), 2);
    assert_eq!(query["client_id"], "LOCAL_APPLICATION");
    // Without a role, the role scope is left out.
    assert_eq!(query["scope"], "refresh_token");
    assert!(query["redirect_uri"].starts_with("http://127.0.0.1:") && query["redirect_uri"].ends_with('/'));
    let extra = fields(&credential);
    assert_eq!(extra["account"], "myorg-acct");
    assert!(!extra.contains_key("role"));
    assert_eq!(
        extra["base_url"],
        format!("{}/myorg-acct/api/v2/cortex/v1", server.uri())
    );

    // The login, now nearly out of time, is renewed at the account it belongs to.
    let dir = tempfile::tempdir().unwrap();
    let credentials = Credentials::with_env(dir.path().join("auth.json"), Arc::new(|_| None));
    let Credential::OAuth { refresh, extra, .. } = credential else { unreachable!() };
    credentials
        .store(
            "snowflake",
            Credential::OAuth {
                access: "sf-1".into(),
                refresh,
                expires: now_ms() + 10_000,
                extra,
            },
        )
        .unwrap();
    let provider = ProviderConfig {
        oauth: Some(settings),
        ..builtin("snowflake")
    };
    let Auth::OAuth { access, fields } = credentials.resolve(&provider).await.unwrap() else {
        panic!("a login");
    };
    assert_eq!(access, "sf-2");
    assert_eq!(fields["account"], "myorg-acct");
    let renewal = pairs(&String::from_utf8_lossy(
        &received(&server, "/myorg-acct/oauth/token-request").await[1].body,
    ));
    assert_eq!(renewal["grant_type"], "refresh_token");
    assert_eq!(renewal["refresh_token"], "sf-r");

    // With a role, the sign-in asks for it.
    let roled = settings_with_role();
    assert_eq!(roled, ["refresh_token", "session:role:ANALYST"]);
}

fn settings_with_role() -> Vec<String> {
    let settings = builtin("snowflake").oauth.unwrap();
    let answers = [
        ("account".to_string(), "acct".to_string()),
        ("role".to_string(), "ANALYST".to_string()),
    ];
    settings.filled(&answers).scopes
}

/// The `fake-az` fixture program, built on first use into the directory this test runs from.
static FAKE_AZ: LazyLock<PathBuf> = LazyLock::new(|| {
    // <target>/<profile>/deps/<this test>
    let test = std::env::current_exe().unwrap();
    let profile_dir = test.parent().unwrap().parent().unwrap();
    let mut build = std::process::Command::new(env!("CARGO"));
    build.args(["build", "--quiet", "--package", "fake-az", "--target-dir"]);
    build.arg(profile_dir.parent().unwrap());
    if profile_dir.file_name().unwrap() == "release" {
        build.arg("--release");
    }
    build.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    assert!(build.status().unwrap().success(), "could not build the fake-az fixture");
    let binary = profile_dir.join(format!("fake-az{}", std::env::consts::EXE_SUFFIX));
    assert!(binary.is_file(), "{} is missing", binary.display());
    binary
});

/// A copy of `fake-az` in `dir` that hands out tokens valid for an hour, named after the
/// resource they are for.
fn fake_az(dir: &Path) -> PathBuf {
    let program = dir.join(format!("az{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(&*FAKE_AZ, &program).unwrap();
    std::fs::create_dir_all(dir.join("az-data")).unwrap();
    let expires = now_ms() / 1000 + 3600;
    std::fs::write(
        dir.join("az-data").join("token.json"),
        format!(r#"{{"accessToken": "entra-for-{{resource}}", "expires_on": {expires}, "tokenType": "Bearer"}}"#),
    )
    .unwrap();
    program
}

fn az_calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("az-data").join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn azure(server: &MockServer, style: AuthStyle) -> ProviderConfig {
    ProviderConfig {
        auth_style: style,
        base_url: server.uri(),
        ..builtin("azure")
    }
}

fn deployments_answer() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"data": [
        {"id": "prod-gpt", "model": "gpt-5.2", "status": "succeeded"},
        {"id": "gpt-4.1", "model": "gpt-4.1", "status": "succeeded"},
        {"id": "half-made", "model": "gpt-5.2", "status": "creating"}
    ]}))
}

fn listed(models: &[ModelInfo]) -> Vec<(&str, Option<&str>)> {
    models.iter().map(|m| (m.id.as_str(), m.name.as_deref())).collect()
}

#[tokio::test]
async fn azure_cli_tokens_sign_requests_and_list_the_deployments() {
    let server = MockServer::start().await;
    let token = format!("Bearer entra-for-{}", azure::COGNITIVE_SERVICES);
    Mock::given(method("GET"))
        .and(path("/openai/deployments"))
        .and(query_param("api-version", "2022-12-01"))
        .and(header("authorization", token.as_str()))
        .respond_with(deployments_answer())
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let cli = AzureCli::with_program(fake_az(dir.path()));
    // A key in the environment does not replace the Azure CLI the configuration asks for.
    let credentials = Credentials::with_env(
        dir.path().join("auth.json"),
        Arc::new(|name| (name == "AZURE_OPENAI_API_KEY").then(|| "resource-key".to_string())),
    )
    .with_azure_cli(cli);
    let credentials = Arc::new(credentials);
    let provider = azure(&server, AuthStyle::AzureCli);
    assert_eq!(credentials.source(&provider).as_deref(), Some("azure-cli"));

    let http = HttpProvider::new(provider.clone(), http_client(), credentials.clone());
    let models = http.list_models().await.unwrap();
    assert_eq!(
        listed(&models),
        [("gpt-4.1", None), ("prod-gpt", Some("gpt-5.2"))]
    );
    // The token is held: asking again runs no second `az`.
    assert_eq!(
        credentials.resolve(&provider).await.unwrap(),
        Auth::ApiKey(format!("entra-for-{}", azure::COGNITIVE_SERVICES))
    );
    let calls = az_calls(dir.path());
    assert_eq!(
        calls,
        [format!(
            "account\tget-access-token\t--resource\t{}\t--output\tjson",
            azure::COGNITIVE_SERVICES
        )]
    );

    // Not signed in to the CLI: the error says what to do.
    std::fs::remove_file(dir.path().join("az-data").join("token.json")).unwrap();
    credentials.invalidate("azure");
    let failed = credentials.resolve(&provider).await.unwrap_err().to_string();
    assert!(failed.contains("az login"), "{failed}");
}

#[tokio::test]
async fn an_azure_resource_key_lists_the_deployments_of_the_resource() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/openai/deployments"))
        .and(header("api-key", "resource-key"))
        .respond_with(deployments_answer())
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let credentials = Credentials::with_env(
        dir.path().join("auth.json"),
        Arc::new(|name| (name == "AZURE_OPENAI_API_KEY").then(|| "resource-key".to_string())),
    );
    let provider = HttpProvider::new(
        azure(&server, AuthStyle::ApiKey),
        http_client(),
        Arc::new(credentials),
    );
    let models = provider.list_models().await.unwrap();
    assert_eq!(listed(&models).len(), 2);
}

#[tokio::test]
async fn resource_manager_finds_the_resource_and_reads_every_page_of_deployments() {
    let server = MockServer::start().await;
    let management = server.uri();
    let token = format!("Bearer entra-for-{}", azure::MANAGEMENT);
    let id = "/subscriptions/s1/resourceGroups/g1/providers/Microsoft.CognitiveServices/accounts/my-res";
    Mock::given(method("POST"))
        .and(path("/providers/Microsoft.ResourceGraph/resources"))
        .and(header("authorization", token.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": id}]})))
        .mount(&server)
        .await;
    let deployment = |name: &str, model: &str, state: &str| {
        json!({"name": name, "properties": {"model": {"name": model}, "provisioningState": state}})
    };
    Mock::given(method("GET"))
        .and(path(format!("{id}/deployments")))
        .and(query_param("api-version", "2024-10-01"))
        .and(header("authorization", token.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [deployment("prod", "gpt-5.2", "Succeeded"), deployment("broken", "x", "Failed")],
            "nextLink": format!("{management}{id}/deployments?api-version=2024-10-01&page=2")
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{id}/deployments")))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [deployment("small", "gpt-5-mini", "Succeeded")]
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let cli = AzureCli::with_program(fake_az(dir.path()));
    let found = azure::manager_deployments(&http_client(), &cli, &management, "my-res")
        .await
        .unwrap();
    assert_eq!(
        found,
        [
            ("prod".to_string(), "gpt-5.2".to_string()),
            ("small".to_string(), "gpt-5-mini".to_string())
        ]
    );
    let graph: Value = serde_json::from_slice(
        &received(&server, "/providers/Microsoft.ResourceGraph/resources").await[0].body,
    )
    .unwrap();
    assert!(graph["query"].as_str().unwrap().contains("=~ 'my-res'"));
}
