#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_ai::credentials::now_ms;
use azazel_ai::oauth::{device, login, pkce};
use azazel_ai::{
    AiError, Auth, Credential, Credentials, LoginUi, OAuthError, OAuthFlow, OAuthSettings, ProviderConfig,
    WireApi,
};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn settings(server: &MockServer, flow: OAuthFlow) -> OAuthSettings {
    OAuthSettings {
        flow,
        client_id: "app-1".into(),
        authorize_url: format!("{}/authorize", server.uri()),
        token_url: format!("{}/token", server.uri()),
        device_url: format!("{}/device", server.uri()),
        scopes: vec!["models".into(), "offline_access".into()],
        ..Default::default()
    }
}

/// The pairs of a query string or a form body.
fn pairs(encoded: &str) -> BTreeMap<String, String> {
    reqwest::Url::parse(&format!("http://x/?{encoded}"))
        .unwrap()
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

/// The form bodies the token endpoint received, in order.
async fn token_forms(server: &MockServer) -> Vec<BTreeMap<String, String>> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/token")
        .map(|request| pairs(&String::from_utf8_lossy(&request.body)))
        .collect()
}

fn granted(access: &str, refresh: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "access_token": access,
        "refresh_token": refresh,
        "expires_in": 3600,
        "token_type": "Bearer"
    }))
}

fn refused(code: &str) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({ "error": code }))
}

/// What the person pastes, made from the query of the sign-in address they saw.
type Paste = fn(&BTreeMap<String, String>) -> String;

/// The person signing in: what they are shown goes to `shown`, and when asked to paste the
/// redirect address they answer with `pasted`.
struct Person {
    shown: mpsc::UnboundedSender<String>,
    last: Mutex<String>,
    pasted: Option<Paste>,
}

fn person(pasted: Option<Paste>) -> (Person, mpsc::UnboundedReceiver<String>) {
    let (shown, seen) = mpsc::unbounded_channel();
    let person = Person {
        shown,
        last: Mutex::default(),
        pasted,
    };
    (person, seen)
}

/// The query of the sign-in address in a shown text.
fn sign_in_query(shown: &str) -> BTreeMap<String, String> {
    let url = shown.lines().last().unwrap();
    pairs(url.split_once('?').unwrap().1)
}

#[async_trait]
impl LoginUi for Person {
    fn show(&self, text: &str) {
        *self.last.lock().unwrap() = text.to_string();
        let _ = self.shown.send(text.to_string());
    }

    async fn ask(&self, _: &str) -> Option<String> {
        let query = sign_in_query(&self.last.lock().unwrap());
        self.pasted.map(|pasted| pasted(&query))
    }
}

#[tokio::test]
async fn a_pkce_login_ends_when_the_browser_reaches_the_loopback_address() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(granted("at-1", "rt-1"))
        .expect(1)
        .mount(&server)
        .await;
    let settings = settings(&server, OAuthFlow::Pkce);
    let (person, mut seen) = person(None);
    let http = reqwest::Client::new();

    // The browser: asks for an icon first, then follows the redirect the server would send.
    let browser = tokio::spawn({
        let http = http.clone();
        async move {
            let query = sign_in_query(&seen.recv().await.unwrap());
            let redirect = &query["redirect_uri"];
            let icon = http
                .get(redirect.replace("/callback", "/favicon.ico"))
                .send()
                .await
                .unwrap();
            assert_eq!(icon.status(), 404);
            let landed = http
                .get(format!("{redirect}?code=the%20code&state={}", query["state"]))
                .send()
                .await
                .unwrap();
            assert_eq!(landed.status(), 200);
            assert!(landed.text().await.unwrap().contains("Signed in"));
            query
        }
    });
    let before = now_ms();
    let credential = login(&http, &settings, &person, &CancellationToken::new())
        .await
        .unwrap();
    let query = browser.await.unwrap();

    let Credential::OAuth {
        access,
        refresh,
        expires,
        ..
    } = credential
    else {
        panic!("an OAuth entry");
    };
    assert_eq!((access.as_str(), refresh.as_str()), ("at-1", "rt-1"));
    assert!(expires >= before + 3_600_000 && expires <= now_ms() + 3_600_000);

    assert_eq!(query["response_type"], "code");
    assert_eq!(query["client_id"], "app-1");
    assert_eq!(query["scope"], "models offline_access");
    assert_eq!(query["code_challenge_method"], "S256");
    assert!(
        query["redirect_uri"].starts_with("http://127.0.0.1:")
            && query["redirect_uri"].ends_with("/callback")
    );

    let forms = token_forms(&server).await;
    assert_eq!(forms.len(), 1);
    let form = &forms[0];
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "the code");
    assert_eq!(form["client_id"], "app-1");
    assert_eq!(form["redirect_uri"], query["redirect_uri"]);
    // The verifier sent to the token endpoint is the one the challenge was made from.
    assert_eq!(pkce::challenge(&form["code_verifier"]), query["code_challenge"]);
}

#[tokio::test]
async fn a_redirect_from_another_login_or_a_refusal_ends_the_login_without_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(granted("at", "rt"))
        .expect(0)
        .mount(&server)
        .await;
    let settings = settings(&server, OAuthFlow::Pkce);
    let http = reqwest::Client::new();

    for (query, expected) in [
        ("code=x&state=someone-elses", OAuthError::State),
        (
            "error=access_denied&error_description=The+user+said+no",
            OAuthError::Refused {
                code: "access_denied".into(),
                description: "The user said no".into(),
            },
        ),
    ] {
        let (person, mut seen) = person(None);
        let browser = tokio::spawn({
            let http = http.clone();
            async move {
                let redirect = sign_in_query(&seen.recv().await.unwrap())["redirect_uri"].clone();
                http.get(format!("{redirect}?{query}"))
                    .send()
                    .await
                    .unwrap()
                    .status()
            }
        });
        let outcome = login(&http, &settings, &person, &CancellationToken::new()).await;
        assert_eq!(outcome.unwrap_err(), expected);
        assert_eq!(browser.await.unwrap(), 400);
    }

    // Nobody signs in: the login ends when it is cancelled.
    let (person, _seen) = person(None);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        login(&http, &settings, &person, &cancel).await.unwrap_err(),
        OAuthError::Cancelled
    );
}

#[tokio::test]
async fn when_the_redirect_port_is_taken_the_pasted_address_finishes_the_login() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(granted("at-1", "rt-1"))
        .mount(&server)
        .await;
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut settings = settings(&server, OAuthFlow::Pkce);
    settings.redirect_port = taken.local_addr().unwrap().port();
    let http = reqwest::Client::new();

    let (pasting, _seen) = person(Some(|query| {
        format!("{}?code=pasted&state={}", query["redirect_uri"], query["state"])
    }));
    let credential = login(&http, &settings, &pasting, &CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(credential, Credential::OAuth { access, .. } if access == "at-1"));
    let form = &token_forms(&server).await[0];
    assert_eq!(form["code"], "pasted");
    assert_eq!(
        form["redirect_uri"],
        format!("http://127.0.0.1:{}/callback", settings.redirect_port)
    );

    // With nobody to paste it, the login says why it cannot go on.
    let (silent, _seen) = person(None);
    let failed = login(&http, &settings, &silent, &CancellationToken::new())
        .await
        .unwrap_err();
    let OAuthError::Config(why) = failed else {
        panic!("{failed:?}");
    };
    assert!(
        why.starts_with(&format!("cannot listen on port {}: ", settings.redirect_port)),
        "{why}"
    );
}

#[tokio::test]
async fn the_device_flow_waits_longer_after_slow_down_and_stops_when_told() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "dc-1",
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://id.example/device",
            "expires_in": 600,
            "interval": 0
        })))
        .mount(&server)
        .await;
    for step in [refused("authorization_pending"), refused("slow_down")] {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(step)
            .up_to_n_times(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(granted("at-d", "rt-d"))
        .mount(&server)
        .await;
    let settings = settings(&server, OAuthFlow::Device);
    let http = reqwest::Client::new();

    let code = device::start(&http, &settings, &settings.device_url)
        .await
        .unwrap();
    assert_eq!((code.user_code.as_str(), code.interval), ("WDJB-MJHT", Some(0)));
    let step = Duration::from_millis(300);
    let begun = Instant::now();
    let tokens = device::poll(
        &http,
        &settings,
        &settings.token_url,
        &code,
        step,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(begun.elapsed() >= step, "{:?}", begun.elapsed());
    assert_eq!(
        (tokens.access.as_str(), tokens.refresh.as_deref()),
        ("at-d", Some("rt-d"))
    );
    let forms = token_forms(&server).await;
    assert_eq!(forms.len(), 3);
    assert_eq!(
        forms[2]["grant_type"],
        "urn:ietf:params:oauth:grant-type:device_code"
    );
    assert_eq!(
        (forms[2]["device_code"].as_str(), forms[2]["client_id"].as_str()),
        ("dc-1", "app-1")
    );

    // The whole login shows where to go and what to enter.
    let (person, mut seen) = person(None);
    let credential = login(&http, &settings, &person, &CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(credential, Credential::OAuth { access, .. } if access == "at-d"));
    assert_eq!(
        seen.recv().await.unwrap(),
        "Open https://id.example/device and enter the code WDJB-MJHT"
    );

    let cancel = CancellationToken::new();
    cancel.cancel();
    let slow = device::DeviceCode {
        interval: Some(60),
        ..code.clone()
    };
    assert_eq!(
        device::poll(&http, &settings, &settings.token_url, &slow, step, &cancel)
            .await
            .unwrap_err(),
        OAuthError::Cancelled
    );
    let stale = device::DeviceCode {
        expires_in: Some(0),
        ..code
    };
    assert_eq!(
        device::poll(
            &http,
            &settings,
            &settings.token_url,
            &stale,
            step,
            &CancellationToken::new()
        )
        .await
        .unwrap_err(),
        OAuthError::TimedOut
    );
}

fn provider(oauth: Option<OAuthSettings>) -> ProviderConfig {
    ProviderConfig {
        id: "gateway".into(),
        api: WireApi::OpenAiChat,
        base_url: "http://gateway.invalid/v1".into(),
        api_key_env: Vec::new(),
        api_key_command: None,
        headers: BTreeMap::new(),
        oauth_headers: BTreeMap::new(),
        requires_key: true,
        auth_style: Default::default(),
        oauth,
    }
}

/// A store whose `gateway` login has `left` to live.
fn store(dir: &Path, left: Duration) -> Arc<Credentials> {
    let credentials = Credentials::with_env(dir.join("auth.json"), Arc::new(|_| None));
    let login = Credential::OAuth {
        access: "at-1".into(),
        refresh: "rt-1".into(),
        expires: now_ms() + left.as_millis() as u64,
        extra: BTreeMap::from([("account".to_string(), json!("me"))]),
    };
    credentials.store("gateway", login).unwrap();
    Arc::new(credentials)
}

fn access(auth: Result<Auth, AiError>) -> String {
    match auth {
        Ok(Auth::OAuth { access, .. }) => access,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn requests_that_find_the_token_expiring_share_one_refresh() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(granted("at-2", "rt-2").set_delay(Duration::from_millis(150)))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let credentials = store(dir.path(), Duration::from_secs(60));
    let provider = provider(Some(settings(&server, OAuthFlow::Pkce)));

    let (a, b, c) = tokio::join!(
        credentials.resolve(&provider),
        credentials.resolve(&provider),
        credentials.resolve(&provider)
    );
    assert_eq!([access(a), access(b), access(c)], ["at-2", "at-2", "at-2"]);
    // Another process, which only shares the file, finds the new login too.
    let other = Credentials::with_env(dir.path().join("auth.json"), Arc::new(|_| None));
    assert_eq!(access(other.resolve(&provider).await), "at-2");

    let form = &token_forms(&server).await[0];
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(
        (form["refresh_token"].as_str(), form["client_id"].as_str()),
        ("rt-1", "app-1")
    );
    let Some(Credential::OAuth { refresh, extra, .. }) = credentials.load().remove("gateway") else {
        panic!("the login is kept");
    };
    assert_eq!(refresh, "rt-2");
    assert_eq!(extra["account"], "me");
}

#[tokio::test]
async fn a_token_with_time_left_is_used_as_it_is() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(granted("at-2", "rt-2"))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let credentials = store(dir.path(), Duration::from_secs(6 * 60));
    let provider = provider(Some(settings(&server, OAuthFlow::Pkce)));
    assert_eq!(access(credentials.resolve(&provider).await), "at-1");
}

#[tokio::test]
async fn a_login_that_cannot_be_refreshed_lasts_until_it_expires() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(refused("invalid_grant"))
        .mount(&server)
        .await;
    let expired = |outcome: Result<Auth, AiError>| {
        assert_eq!(
            outcome.unwrap_err(),
            AiError::Auth("login for gateway has expired: run `azazel login gateway`".into())
        );
    };
    let dir = tempfile::tempdir().unwrap();

    // The server no longer accepts the refresh token.
    let revoked = provider(Some(settings(&server, OAuthFlow::Pkce)));
    assert_eq!(
        access(store(dir.path(), Duration::from_secs(60)).resolve(&revoked).await),
        "at-1"
    );
    expired(store(dir.path(), Duration::ZERO).resolve(&revoked).await);

    // The server cannot be reached: the token in hand is used while it lasts.
    let mut unreachable = settings(&server, OAuthFlow::Pkce);
    unreachable.token_url = "http://127.0.0.1:9/token".into();
    let unreachable = provider(Some(unreachable));
    assert_eq!(
        access(
            store(dir.path(), Duration::from_secs(60))
                .resolve(&unreachable)
                .await
        ),
        "at-1"
    );
    let gone = store(dir.path(), Duration::ZERO)
        .resolve(&unreachable)
        .await
        .unwrap_err();
    assert!(
        matches!(&gone, AiError::Network(why) if why.starts_with("could not refresh the login for gateway: ")),
        "{gone:?}"
    );

    // A provider without an authorisation server has nothing to refresh with.
    let plain = provider(None);
    assert_eq!(
        access(store(dir.path(), Duration::from_secs(60)).resolve(&plain).await),
        "at-1"
    );
    expired(store(dir.path(), Duration::ZERO).resolve(&plain).await);
}
