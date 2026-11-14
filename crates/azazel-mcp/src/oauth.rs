//! OAuth for remote MCP servers: tokens kept in `auth.json` and the browser login flow.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use azazel_ai::{Credential, Credentials};
use reqwest::Url;
use rmcp::transport::auth::{
    AuthClient, AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore,
    StoredCredentials,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

use crate::config::OAuthSettings;

/// Path of the loopback redirect URI.
const CALLBACK_PATH: &str = "/callback";
/// Client name sent with dynamic client registration.
const CLIENT_NAME: &str = "azazel";
/// Field of an `auth.json` entry holding rmcp's `StoredCredentials`.
const STORED_FIELD: &str = "mcp";
/// Largest request head the redirect listener reads.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// How long one connection to the redirect listener may take to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// The `auth.json` key of a server's tokens: `mcp:<url>`.
pub fn credential_key(url: &str) -> String {
    format!("mcp:{url}")
}

/// rmcp credential store backed by one `auth.json` entry.
pub struct AuthJsonStore {
    credentials: Arc<Credentials>,
    key: String,
}

impl AuthJsonStore {
    /// A store for the tokens of the server at `url`.
    pub fn new(credentials: Arc<Credentials>, url: &str) -> Self {
        Self {
            credentials,
            key: credential_key(url),
        }
    }

    /// Applies `change` to `auth.json` on a blocking thread, because it waits for the file's advisory lock.
    async fn update(
        &self,
        change: impl FnOnce(&Credentials, &str) -> std::io::Result<()> + Send + 'static,
    ) -> Result<(), AuthError> {
        let credentials = Arc::clone(&self.credentials);
        let key = self.key.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            change(&credentials, &key)
                .map_err(|e| format!("cannot update {}: {e}", credentials.auth_file().display()))
        })
        .await;
        match outcome {
            Ok(Ok(())) => Ok(()),
            Ok(Err(reason)) => Err(AuthError::CredentialStoreError(reason)),
            Err(e) => Err(AuthError::CredentialStoreError(format!(
                "the auth.json update did not finish: {e}"
            ))),
        }
    }
}

#[async_trait]
impl CredentialStore for AuthJsonStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let Some(Credential::OAuth { mut extra, .. }) = self.credentials.load().remove(&self.key) else {
            return Ok(None);
        };
        let Some(stored) = extra.remove(STORED_FIELD) else {
            return Ok(None);
        };
        match serde_json::from_value(stored) {
            Ok(stored) => Ok(Some(stored)),
            Err(_) => {
                tracing::warn!(
                    "the login stored under {} in auth.json is unreadable; log in again",
                    self.key
                );
                Ok(None)
            }
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let stored = serde_json::to_value(credentials).map_err(|e| {
            AuthError::CredentialStoreError(format!("cannot serialise the login for {}: {e}", self.key))
        })?;
        let entry = entry_for(stored);
        self.update(move |file, key| file.store(key, entry)).await
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.update(|file, key| file.remove(key).map(|_| ())).await
    }
}

/// The `auth.json` entry for rmcp's serialised `StoredCredentials`: the whole value under `mcp`, with the
/// access token, refresh token and expiry (Unix milliseconds) mirrored into the named fields.
fn entry_for(stored: Value) -> Credential {
    let text = |pointer: &str| {
        stored
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let seconds = |pointer: &str| stored.pointer(pointer).and_then(Value::as_u64);
    let access = text("/token_response/access_token");
    let refresh = text("/token_response/refresh_token");
    let expires = match (
        seconds("/token_received_at"),
        seconds("/token_response/expires_in"),
    ) {
        (Some(received), Some(lifetime)) => received.saturating_add(lifetime).saturating_mul(1000),
        _ => 0,
    };
    Credential::OAuth {
        access,
        refresh,
        expires,
        extra: BTreeMap::from([(STORED_FIELD.to_string(), stored)]),
    }
}

/// An rmcp error with its causes, as one line.
fn describe(error: &AuthError) -> String {
    let mut text = error.to_string();
    let mut cause = std::error::Error::source(error);
    while let Some(inner) = cause {
        let inner_text = inner.to_string();
        if !text.contains(&inner_text) {
            text.push_str(": ");
            text.push_str(&inner_text);
        }
        cause = inner.source();
    }
    text
}

/// An HTTP client that sends (and refreshes) the stored tokens of the server at `url`.
/// `Ok(None)` when nothing is stored for it.
pub async fn authorized_client(
    url: &str,
    credentials: Arc<Credentials>,
    http: reqwest::Client,
) -> Result<Option<AuthClient<reqwest::Client>>, String> {
    let store = AuthJsonStore::new(credentials, url);
    let stored = store
        .load()
        .await
        .map_err(|e| format!("cannot read the login for {url}: {}", describe(&e)))?;
    if stored.is_none() {
        return Ok(None);
    }
    let mut manager = AuthorizationManager::new(url)
        .await
        .map_err(|e| format!("cannot set up OAuth for {url}: {}", describe(&e)))?;
    manager
        .with_client(http.clone())
        .map_err(|e| format!("cannot set up OAuth for {url}: {}", describe(&e)))?;
    manager.set_credential_store(store);
    let usable = manager
        .initialize_from_store()
        .await
        .map_err(|e| format!("cannot restore the login for {url}: {}", describe(&e)))?;
    Ok(usable.then(|| AuthClient::new(http, manager)))
}

/// Runs the authorization-code flow with PKCE against the server at `url` and stores the tokens.
/// Binds a loopback listener on `127.0.0.1:<settings.callback_port>` (0 = any free port), uses
/// `settings.client_id` when it is not empty and dynamic client registration otherwise, requests
/// `settings.scope` (space separated) when not empty, calls `show_url` with the authorization URL
/// (the caller prints it and/or opens a browser), then waits up to `wait` for the redirect.
pub async fn login(
    url: &str,
    settings: &OAuthSettings,
    credentials: Arc<Credentials>,
    http: reqwest::Client,
    show_url: &(dyn Fn(&str) + Send + Sync),
    wait: Duration,
) -> Result<(), String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, settings.callback_port))
        .await
        .map_err(|e| {
            format!(
                "cannot listen on 127.0.0.1:{} for the OAuth redirect: {e}",
                settings.callback_port
            )
        })?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("cannot read the address of the OAuth redirect listener: {e}"))?
        .port();

    let mut manager = AuthorizationManager::new(url)
        .await
        .map_err(|e| format!("cannot set up OAuth for {url}: {}", describe(&e)))?;
    manager
        .with_client(http)
        .map_err(|e| format!("cannot set up OAuth for {url}: {}", describe(&e)))?;
    manager.set_credential_store(AuthJsonStore::new(credentials, url));
    let resolution = manager
        .resolve_metadata()
        .await
        .map_err(|e| format!("OAuth discovery for {url} failed: {}", describe(&e)))?;
    manager.set_metadata(resolution.metadata);

    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let mut request = AuthorizationRequest::new(redirect_uri).with_client_name(CLIENT_NAME);
    request = if settings.scope.trim().is_empty() {
        request.with_initial_scope_selection()
    } else {
        request.with_scopes(settings.scope.split_whitespace())
    };
    if !settings.client_id.is_empty() {
        request = request.with_preregistered_client(settings.client_id.as_str());
    }
    let session = AuthorizationSession::new(manager, request)
        .await
        .map_err(|(_, e)| {
            let hint = if matches!(e, AuthError::RegistrationFailed(_)) && settings.client_id.is_empty() {
                "; set oauth.clientId for this server if it needs a pre-registered client"
            } else {
                ""
            };
            format!("cannot start the login for {url}: {}{hint}", describe(&e))
        })?;
    show_url(session.get_authorization_url());

    let (mut stream, request_line) = match tokio::time::timeout(wait, accept_callback(&listener)).await {
        Ok(accepted) => accepted?,
        Err(_) => {
            return Err(format!(
                "no OAuth redirect for {url} arrived within {} seconds",
                wait.as_secs()
            ));
        }
    };
    let outcome = match read_callback(&request_line) {
        Ok(callback) => session
            .handle_callback_with_issuer(&callback.code, &callback.state, callback.issuer.as_deref())
            .await
            .map(|_| ())
            .map_err(|e| format!("the token exchange for {url} failed: {}", describe(&e))),
        Err(reason) => Err(format!("the login for {url} failed: {reason}")),
    };
    respond(
        &mut stream,
        "200 OK",
        &result_page(outcome.as_ref().err().map(String::as_str)),
    )
    .await;
    outcome
}

/// Forgets the server's tokens. False when none were stored.
pub fn logout(url: &str, credentials: &Credentials) -> std::io::Result<bool> {
    credentials.remove(&credential_key(url))
}

/// What the loopback listener extracts from the redirect request line, e.g.
/// `GET /callback?code=abc&state=xyz HTTP/1.1` -> `Ok(("abc", "xyz"))`; an `error=` parameter or a
/// missing `code`/`state` is `Err` with a readable reason. Percent-decoding applies.
pub fn parse_callback(request_line: &str) -> Result<(String, String), String> {
    read_callback(request_line).map(|callback| (callback.code, callback.state))
}

/// The parameters of a successful authorization redirect.
struct Callback {
    code: String,
    state: String,
    /// The RFC 9207 `iss` parameter.
    issuer: Option<String>,
}

/// The request target of a `GET` request line in origin form.
fn request_target(request_line: &str) -> Option<&str> {
    let mut parts = request_line.split_whitespace();
    let (method, target) = (parts.next()?, parts.next()?);
    (method == "GET" && target.starts_with('/')).then_some(target)
}

/// A request target without its query and fragment.
fn target_path(target: &str) -> &str {
    target.split(['?', '#']).next().unwrap_or(target)
}

fn is_callback_request(request_line: &str) -> bool {
    request_target(request_line).is_some_and(|target| target_path(target) == CALLBACK_PATH)
}

fn read_callback(request_line: &str) -> Result<Callback, String> {
    let target = request_target(request_line)
        .ok_or_else(|| format!("not a GET request for a path: {request_line}"))?;
    let path = target_path(target);
    if path != CALLBACK_PATH {
        return Err(format!("{path} is not the OAuth redirect path {CALLBACK_PATH}"));
    }
    let url = Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|e| format!("the redirect request is malformed: {e}"))?;

    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    let mut issuer: Option<String> = None;
    let mut error: Option<String> = None;
    let mut description: Option<String> = None;
    for (name, value) in url.query_pairs() {
        let slot = match name.as_ref() {
            "code" => &mut code,
            "state" => &mut state,
            "iss" => &mut issuer,
            "error" => &mut error,
            "error_description" => &mut description,
            _ => continue,
        };
        if !value.is_empty() {
            *slot = Some(value.into_owned());
        }
    }

    if let Some(error) = error {
        return Err(match description {
            Some(description) => format!("the authorization server answered {error}: {description}"),
            None => format!("the authorization server answered {error}"),
        });
    }
    let code = code.ok_or("the redirect carries no authorization code")?;
    let state = state.ok_or("the redirect carries no state")?;
    Ok(Callback { code, state, issuer })
}

/// Serves connections until one requests the redirect path; returns that connection, still unanswered,
/// with its request line. Requests for other paths are answered with `404`.
async fn accept_callback(listener: &TcpListener) -> Result<(TcpStream, String), String> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|e| format!("the OAuth redirect listener failed: {e}"))?;
                connections.spawn(serve_connection(stream));
            }
            Some(served) = connections.join_next() => {
                if let Ok(Some(redirect)) = served {
                    return Ok(redirect);
                }
            }
        }
    }
}

/// Reads one request head. A request for the redirect path yields the connection and its request line;
/// anything else is answered with `404` and yields nothing.
async fn serve_connection(mut stream: TcpStream) -> Option<(TcpStream, String)> {
    let request_line = tokio::time::timeout(HEAD_TIMEOUT, read_request_line(&mut stream))
        .await
        .ok()??;
    if is_callback_request(&request_line) {
        return Some((stream, request_line));
    }
    respond(&mut stream, "404 Not Found", NOT_FOUND_PAGE).await;
    None
}

/// Reads a request head up to its blank line and returns its first line.
async fn read_request_line(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        // A terminator may straddle two reads, so the search starts three bytes before the new data.
        let from = head.len().saturating_sub(3);
        head.extend_from_slice(chunk.get(..read)?);
        if head.get(from..)?.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if head.len() > MAX_HEAD_BYTES {
            return None;
        }
    }
    let end = head.iter().position(|byte| *byte == b'\n')?;
    let line = std::str::from_utf8(head.get(..end)?).ok()?;
    Some(line.trim_end().to_string())
}

/// Writes an HTML response and closes the sending side.
async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let sent = async {
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await
    };
    if let Err(e) = sent.await {
        tracing::debug!("cannot answer the OAuth redirect request: {e}");
    }
}

const NOT_FOUND_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Not found</title>\
     </head><body><p>Not found.</p></body></html>";

/// The page shown in the browser once the login has succeeded (`failure` is `None`) or failed.
fn result_page(failure: Option<&str>) -> String {
    let (title, detail) = match failure {
        None => (
            "Login succeeded",
            "azazel stored the tokens for this MCP server.".to_string(),
        ),
        Some(reason) => ("Login failed", escape_html(reason)),
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>azazel: {title}</title></head>\
         <body><h1>{title}</h1><p>{detail}</p><p>You can close this tab.</p></body></html>"
    )
}

fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const SERVER: &str = "https://mcp.example.com/mcp";

    fn stored(value: Value) -> StoredCredentials {
        serde_json::from_value(value).unwrap()
    }

    fn full_login() -> Value {
        json!({
            "client_id": "client-1",
            "token_response": {
                "access_token": "access-1",
                "token_type": "bearer",
                "expires_in": 3600,
                "refresh_token": "refresh-1",
                "scope": "read write"
            },
            "granted_scopes": ["read", "write"],
            "token_received_at": 1_700_000_000u64,
            "issuer": "https://auth.example.com"
        })
    }

    #[test]
    fn credential_key_prefixes_the_url() {
        assert_eq!(credential_key(SERVER), "mcp:https://mcp.example.com/mcp");
    }

    #[test]
    fn parse_callback_reads_code_and_state() {
        assert_eq!(
            parse_callback("GET /callback?code=abc&state=xyz HTTP/1.1"),
            Ok(("abc".to_string(), "xyz".to_string()))
        );
    }

    #[test]
    fn parse_callback_decodes_percent_encoding() {
        assert_eq!(
            parse_callback("GET /callback?state=x%2Fy%3D%3D&code=a%20b%2Bc+d&iss=https%3A%2F%2Fas HTTP/1.1"),
            Ok(("a b+c d".to_string(), "x/y==".to_string()))
        );
        let callback =
            read_callback("GET /callback?code=c&state=s&iss=https%3A%2F%2Fas.example HTTP/1.1").unwrap();
        assert_eq!(callback.issuer.as_deref(), Some("https://as.example"));
    }

    #[test]
    fn parse_callback_reports_the_server_error() {
        let reason = parse_callback(
            "GET /callback?error=access_denied&error_description=The%20user%20said%20no&state=xyz HTTP/1.1",
        )
        .unwrap_err();
        assert!(reason.contains("access_denied"), "{reason}");
        assert!(reason.contains("The user said no"), "{reason}");

        let bare = parse_callback("GET /callback?error=server_error HTTP/1.1").unwrap_err();
        assert!(bare.contains("server_error"), "{bare}");
    }

    #[test]
    fn parse_callback_requires_code_and_state() {
        let no_state = parse_callback("GET /callback?code=abc HTTP/1.1").unwrap_err();
        assert!(no_state.contains("state"), "{no_state}");
        let empty_state = parse_callback("GET /callback?code=abc&state= HTTP/1.1").unwrap_err();
        assert!(empty_state.contains("state"), "{empty_state}");
        let no_code = parse_callback("GET /callback?state=xyz HTTP/1.1").unwrap_err();
        assert!(no_code.contains("code"), "{no_code}");
    }

    #[test]
    fn parse_callback_rejects_other_requests() {
        let other_path = parse_callback("GET /favicon.ico?code=abc&state=xyz HTTP/1.1").unwrap_err();
        assert!(other_path.contains("/favicon.ico"), "{other_path}");
        assert!(parse_callback("GET /callback/extra?code=abc&state=xyz HTTP/1.1").is_err());
        assert!(parse_callback("POST /callback?code=abc&state=xyz HTTP/1.1").is_err());
        assert!(parse_callback("").is_err());
        assert!(!is_callback_request("GET /favicon.ico HTTP/1.1"));
        assert!(is_callback_request("GET /callback?error=access_denied HTTP/1.1"));
    }

    #[test]
    fn result_page_escapes_the_reason() {
        let page = result_page(Some("bad <script>alert(\"x\")</script> & more"));
        assert!(page.contains("Login failed"));
        assert!(page.contains("&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; more"));
        assert!(!page.contains("<script>"));
        assert!(result_page(None).contains("Login succeeded"));
    }

    #[tokio::test]
    async fn store_round_trips_through_auth_json() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = Arc::new(Credentials::new(dir.path().join("auth.json")));
        credentials
            .store(
                "acme",
                Credential::ApiKey {
                    key: "sk-unrelated".into(),
                },
            )
            .unwrap();
        let store = AuthJsonStore::new(Arc::clone(&credentials), SERVER);

        assert!(store.load().await.unwrap().is_none());

        store.save(stored(full_login())).await.unwrap();
        let entries = credentials.load();
        assert_eq!(
            entries.get("acme"),
            Some(&Credential::ApiKey {
                key: "sk-unrelated".into()
            })
        );
        let key = credential_key(SERVER);
        let Some(Credential::OAuth {
            access,
            refresh,
            expires,
            extra,
        }) = entries.get(&key)
        else {
            panic!("no OAuth entry for the server: {entries:?}");
        };
        assert_eq!(access, "access-1");
        assert_eq!(refresh, "refresh-1");
        assert_eq!(*expires, (1_700_000_000 + 3600) * 1000);
        assert_eq!(extra.keys().collect::<Vec<_>>(), ["mcp"]);

        let loaded = store.load().await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "client-1");
        assert_eq!(loaded.granted_scopes, ["read", "write"]);
        assert_eq!(loaded.token_received_at, Some(1_700_000_000));
        assert_eq!(loaded.issuer.as_deref(), Some("https://auth.example.com"));
        let reserialised = serde_json::to_value(&loaded).unwrap();
        assert_eq!(
            reserialised.pointer("/token_response/access_token"),
            Some(&json!("access-1"))
        );
        assert_eq!(
            reserialised.pointer("/token_response/refresh_token"),
            Some(&json!("refresh-1"))
        );
        assert_eq!(
            reserialised.pointer("/token_response/expires_in"),
            Some(&json!(3600))
        );

        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
        let entries = credentials.load();
        assert_eq!(entries.len(), 1);
        assert!(entries.contains_key("acme"));
    }

    #[tokio::test]
    async fn store_mirrors_unknown_fields_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = Arc::new(Credentials::new(dir.path().join("auth.json")));
        let store = AuthJsonStore::new(Arc::clone(&credentials), SERVER);

        store
            .save(stored(json!({
                "client_id": "client-1",
                "token_response": { "access_token": "access-1", "token_type": "bearer" }
            })))
            .await
            .unwrap();

        let entries = credentials.load();
        let key = credential_key(SERVER);
        let Some(Credential::OAuth {
            access,
            refresh,
            expires,
            ..
        }) = entries.get(&key)
        else {
            panic!("no OAuth entry for the server: {entries:?}");
        };
        assert_eq!(access, "access-1");
        assert_eq!(refresh, "");
        assert_eq!(*expires, 0);
        assert!(store.load().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn store_ignores_entries_it_did_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = Arc::new(Credentials::new(dir.path().join("auth.json")));
        let key = credential_key(SERVER);
        let store = AuthJsonStore::new(Arc::clone(&credentials), SERVER);

        credentials
            .store(&key, Credential::ApiKey { key: "sk".into() })
            .unwrap();
        assert!(store.load().await.unwrap().is_none());

        let plain = Credential::OAuth {
            access: "a".into(),
            refresh: "r".into(),
            expires: 1,
            extra: BTreeMap::new(),
        };
        credentials.store(&key, plain).unwrap();
        assert!(store.load().await.unwrap().is_none());

        let broken = Credential::OAuth {
            access: "a".into(),
            refresh: "r".into(),
            expires: 1,
            extra: BTreeMap::from([("mcp".to_string(), json!({ "token_response": 7 }))]),
        };
        credentials.store(&key, broken).unwrap();
        assert!(store.load().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn authorized_client_is_none_without_a_stored_login() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = Arc::new(Credentials::new(dir.path().join("auth.json")));
        let client = authorized_client(SERVER, credentials, azazel_ai::tls::client())
            .await
            .unwrap();
        assert!(client.is_none());
    }

    #[test]
    fn logout_reports_whether_tokens_existed() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = Credentials::new(dir.path().join("auth.json"));
        assert!(!logout(SERVER, &credentials).unwrap());
        credentials
            .store(&credential_key(SERVER), entry_for(full_login()))
            .unwrap();
        assert!(logout(SERVER, &credentials).unwrap());
        assert!(credentials.load().is_empty());
    }

    async fn fetch(addr: std::net::SocketAddr, request_line: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(format!("{request_line}\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn listener_answers_other_paths_and_returns_the_redirect() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let browser = tokio::spawn(async move {
            let favicon = fetch(addr, "GET /favicon.ico HTTP/1.1").await;
            let redirect = fetch(addr, "GET /callback?code=abc&state=xyz HTTP/1.1").await;
            (favicon, redirect)
        });

        let (mut stream, request_line) = accept_callback(&listener).await.unwrap();
        assert_eq!(
            parse_callback(&request_line),
            Ok(("abc".to_string(), "xyz".to_string()))
        );
        respond(&mut stream, "200 OK", &result_page(None)).await;
        drop(stream);

        let (favicon, redirect) = browser.await.unwrap();
        assert!(favicon.starts_with("HTTP/1.1 404 Not Found\r\n"), "{favicon}");
        assert!(redirect.starts_with("HTTP/1.1 200 OK\r\n"), "{redirect}");
        assert!(
            redirect.contains("Content-Type: text/html; charset=utf-8\r\n"),
            "{redirect}"
        );
        assert!(redirect.contains("Connection: close\r\n"), "{redirect}");
        assert!(redirect.contains("Login succeeded"), "{redirect}");
        assert!(redirect.contains("close this tab"), "{redirect}");
    }
}
