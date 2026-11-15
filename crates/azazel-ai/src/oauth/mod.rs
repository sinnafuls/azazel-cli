//! OAuth 2.0 logins for providers whose authorisation server is described in configuration
//! (`[providers.<id>.oauth]`): the authorisation-code flow with PKCE over a loopback redirect, the
//! device flow, and refreshing the tokens kept in `auth.json`.
//!
//! Subscription logins (Claude Pro/Max, ChatGPT Codex, Copilot, SuperGrok and the rest) are the
//! same two flows with different details: a fixed callback port, a JSON token body, extra
//! authorise parameters, an account id buried in the answer. Those details are settings, so the
//! built-in providers in `presets.rs` and a provider someone writes in their own config go
//! through exactly the same code.

pub mod device;
pub mod loopback;
pub mod map;
pub mod pkce;
pub mod refresh;

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub use map::{CredentialMap, Expiry, Field, Tokens};

use crate::credentials::Credential;

/// How long the user has to finish signing in on the page.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OAuthFlow {
    /// Authorisation code; the browser is sent back to a loopback address.
    Pkce,
    /// The user enters a code on another page.
    Device,
    /// The token comes back in the redirect itself (`response_type=token`); there is no code to
    /// exchange and nothing to refresh it with.
    Implicit,
    /// The user enters a code on another page, after which the server hands over an
    /// authorisation code and its PKCE verifier, exchanged as a browser login's would be:
    /// OpenAI's device sign-in.
    DeviceAuthCode,
}

/// One value a sign-in needs before it starts, such as the account or the server it signs in
/// to. `{name}` in the addresses, the scopes and `api_base_url` is replaced by the answer, which
/// is kept with the login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginInput {
    pub name: String,
    /// What the person is asked when neither the command line nor `env` answers.
    pub prompt: String,
    /// An environment variable that answers instead.
    #[serde(default)]
    pub env: Option<String>,
    /// May be left empty; a scope that needs it is then left out.
    #[serde(default)]
    pub optional: bool,
    /// Removed from the end of an answer that repeats it (`.snowflakecomputing.com`).
    #[serde(default)]
    pub suffix: Option<String>,
}

/// The login field that holds the API address requests made with the login go to, when it is
/// not the provider's own (a GitHub Enterprise host, a Snowflake account).
pub const BASE_URL_FIELD: &str = "base_url";

/// How a token request carries its parameters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BodyKind {
    #[default]
    Form,
    Json,
}

/// Whether a configured secret is written as it is used. Public client ids are published, but
/// writing them plainly makes secret scanners complain about files that hold no secret.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Encoding {
    #[default]
    Plain,
    Base64,
}

impl Encoding {
    fn decode(self, value: &str) -> Result<String, OAuthError> {
        match self {
            Encoding::Plain => Ok(value.to_string()),
            Encoding::Base64 => STANDARD
                .decode(value.trim())
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .ok_or_else(|| OAuthError::Config("a base64 setting is not readable".to_string())),
        }
    }
}

/// An account endpoint asked who signed in, when the token answer does not say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserInfo {
    pub url: String,
    /// Stored field name to the path of its value in the answer.
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

/// What a refresh does differently from the login that created the credential.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshSettings {
    /// A token endpoint other than the login's.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub body: Option<BodyKind>,
    /// Extra body parameters; `{refresh_token}`, `{client_id}` are filled in.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Where the renewed credential sits, when it differs from the login's answer.
    #[serde(default)]
    pub credential: Option<CredentialMap>,
    #[serde(default)]
    pub userinfo: Option<UserInfo>,
}

/// `[providers.<id>.oauth]`: the authorisation server `azazel login <id>` signs in with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthSettings {
    pub flow: OAuthFlow,
    pub client_id: String,
    /// A client id the provider does not register: the PKCE verifier alone proves the client.
    #[serde(default)]
    pub anonymous_client: bool,
    #[serde(default)]
    pub client_id_encoding: Encoding,
    /// Sent with every token request when it is set; some installed clients ship one.
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub client_secret_encoding: Encoding,
    /// The authorisation endpoint; needed for `pkce`.
    #[serde(default)]
    pub authorize_url: String,
    pub token_url: String,
    /// An OpenID discovery document (`…/.well-known/openid-configuration`) whose
    /// `token_endpoint` and `device_authorization_endpoint` replace the two above.
    #[serde(default)]
    pub discovery_url: Option<String>,
    /// The device authorisation endpoint; needed for `device` and `device-auth-code`.
    #[serde(default)]
    pub device_url: String,
    /// Where a `device-auth-code` login asks whether the user has entered the code.
    #[serde(default)]
    pub device_poll_url: String,
    /// The page the code is entered on, for a device endpoint that does not name one.
    #[serde(default)]
    pub verification_url: Option<String>,
    /// Values asked before the sign-in starts.
    #[serde(default)]
    pub inputs: Vec<LoginInput>,
    /// Kept with the login as the address its requests go to; `{input}` holes are filled.
    #[serde(default)]
    pub api_base_url: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Proof Key for Code Exchange. A server that never registered this client relies on it; a
    /// few refuse a request that carries a challenge they did not ask for.
    #[serde(default = "yes")]
    pub pkce: bool,
    /// Whether the redirect must echo the `state` that was sent.
    #[serde(default = "yes")]
    pub check_state: bool,
    /// Whether the sign-in address carries the parameters every authorisation-code server takes
    /// (`response_type`, `client_id`, `redirect_uri`, `state`, `scope`). A provider with its own
    /// vocabulary turns them off and describes the whole query in `authorize_params`.
    #[serde(default = "yes")]
    pub authorize_standard: bool,
    /// Extra parameters on the sign-in address; `{redirect_uri}`, `{code_challenge}`, `{state}`,
    /// `{client_id}` are filled in.
    #[serde(default)]
    pub authorize_params: BTreeMap<String, String>,
    /// Host written in the redirect address; the listener is always on 127.0.0.1.
    #[serde(default)]
    pub redirect_host: Option<String>,
    /// Port of the redirect address; 0 takes a free port, for servers that accept any port on the
    /// loopback address.
    #[serde(default)]
    pub redirect_port: u16,
    #[serde(default)]
    pub redirect_path: Option<String>,
    /// A redirect address the server insists on, loopback or not. A non-loopback one (a desktop
    /// scheme) is not listened for: the user pastes what the browser ends up at.
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub token_body: BodyKind,
    /// Whether the code exchange carries `grant_type`, `code`, `redirect_uri` and `client_id`.
    #[serde(default = "yes")]
    pub token_standard: bool,
    /// Extra body parameters for the code exchange; `{code}`, `{state}`, `{redirect_uri}`,
    /// `{code_verifier}`, `{client_id}` are filled in.
    #[serde(default)]
    pub token_params: BTreeMap<String, String>,
    #[serde(default)]
    pub token_headers: BTreeMap<String, String>,
    /// Headers for the device authorisation request.
    #[serde(default)]
    pub device_headers: BTreeMap<String, String>,
    #[serde(default)]
    pub credential: CredentialMap,
    #[serde(default)]
    pub refresh: Option<RefreshSettings>,
    #[serde(default)]
    pub userinfo: Option<UserInfo>,
    /// Shown before the browser opens, when the provider needs the user to know something.
    #[serde(default)]
    pub instructions: Option<String>,
}

fn yes() -> bool {
    true
}

impl Default for OAuthSettings {
    fn default() -> Self {
        OAuthSettings {
            flow: OAuthFlow::Pkce,
            client_id: String::new(),
            anonymous_client: false,
            client_id_encoding: Encoding::Plain,
            client_secret: String::new(),
            client_secret_encoding: Encoding::Plain,
            authorize_url: String::new(),
            token_url: String::new(),
            discovery_url: None,
            device_url: String::new(),
            device_poll_url: String::new(),
            verification_url: None,
            inputs: Vec::new(),
            api_base_url: None,
            scopes: Vec::new(),
            pkce: true,
            check_state: true,
            authorize_standard: true,
            authorize_params: BTreeMap::new(),
            redirect_host: None,
            redirect_port: 0,
            redirect_path: None,
            redirect_uri: None,
            token_body: BodyKind::Form,
            token_standard: true,
            token_params: BTreeMap::new(),
            token_headers: BTreeMap::new(),
            device_headers: BTreeMap::new(),
            credential: CredentialMap::default(),
            refresh: None,
            userinfo: None,
            instructions: None,
        }
    }
}

impl OAuthSettings {
    /// Says what is missing for the chosen flow.
    pub fn check(&self) -> Result<(), String> {
        let missing = |key: &str| format!("`{key}` is needed for this flow");
        if self.client_id.trim().is_empty() && !self.anonymous_client {
            return Err(missing("client_id"));
        }
        let blank = |value: &str| value.trim().is_empty();
        if blank(&self.token_url) && self.discovery_url.is_none() && self.flow != OAuthFlow::Implicit {
            return Err(missing("token_url"));
        }
        self.client().map_err(|e| e.to_string())?;
        match self.flow {
            OAuthFlow::Pkce | OAuthFlow::Implicit if blank(&self.authorize_url) => {
                Err(missing("authorize_url"))
            }
            OAuthFlow::Device if blank(&self.device_url) && self.discovery_url.is_none() => {
                Err(missing("device_url"))
            }
            OAuthFlow::DeviceAuthCode if blank(&self.device_url) => Err(missing("device_url")),
            OAuthFlow::DeviceAuthCode if blank(&self.device_poll_url) => Err(missing("device_poll_url")),
            OAuthFlow::DeviceAuthCode if self.verification_url.is_none() => Err(missing("verification_url")),
            OAuthFlow::DeviceAuthCode if self.redirect_uri.is_none() => Err(missing("redirect_uri")),
            _ => Ok(()),
        }
    }

    /// These settings with the answers to their inputs filled in. A scope that needs an answer
    /// that was left empty is left out.
    pub fn filled(&self, answers: &[(String, String)]) -> OAuthSettings {
        if answers.is_empty() {
            return self.clone();
        }
        let vars: Vec<(&str, &str)> = answers.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        let put = |text: &str| fill(text, &vars);
        let put_some = |text: &Option<String>| text.as_deref().map(put);
        let empty_hole = |scope: &str| {
            answers
                .iter()
                .any(|(name, value)| value.is_empty() && scope.contains(&format!("{{{name}}}")))
        };
        let mut out = self.clone();
        out.authorize_url = put(&self.authorize_url);
        out.token_url = put(&self.token_url);
        out.device_url = put(&self.device_url);
        out.device_poll_url = put(&self.device_poll_url);
        out.discovery_url = put_some(&self.discovery_url);
        out.verification_url = put_some(&self.verification_url);
        out.api_base_url = put_some(&self.api_base_url);
        out.scopes = self
            .scopes
            .iter()
            .filter(|scope| !empty_hole(scope))
            .map(|scope| put(scope))
            .collect();
        if let Some(userinfo) = &mut out.userinfo {
            userinfo.url = put(&userinfo.url);
        }
        out
    }

    /// The client id and secret as they go on the wire.
    pub(crate) fn client(&self) -> Result<(String, String), OAuthError> {
        let id = self.client_id_encoding.decode(&self.client_id)?;
        let secret = match self.client_secret.is_empty() {
            true => String::new(),
            false => self.client_secret_encoding.decode(&self.client_secret)?,
        };
        Ok((id, secret))
    }

    fn redirect_path(&self) -> &str {
        self.redirect_path.as_deref().unwrap_or(loopback::DEFAULT_PATH)
    }

    fn redirect_host(&self) -> &str {
        self.redirect_host.as_deref().unwrap_or("127.0.0.1")
    }

    /// The token endpoint and body shape a refresh uses.
    pub(crate) fn refresh_call(&self) -> (&str, BodyKind, &BTreeMap<String, String>) {
        match &self.refresh {
            Some(r) => (
                r.url.as_deref().unwrap_or(&self.token_url),
                r.body.unwrap_or(self.token_body),
                &r.headers,
            ),
            None => (self.token_url.as_str(), self.token_body, &self.token_headers),
        }
    }

    /// Where a refreshed credential sits in the answer.
    pub(crate) fn refresh_credential(&self) -> &CredentialMap {
        self.refresh
            .as_ref()
            .and_then(|r| r.credential.as_ref())
            .unwrap_or(&self.credential)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OAuthError {
    #[error("{0}")]
    Config(String),
    /// An OAuth error answer: its `error` code and `error_description`.
    #[error("the authorisation server refused: {code}{}", if .description.is_empty() { String::new() } else { format!(" ({})", .description) })]
    Refused { code: String, description: String },
    #[error("could not reach the authorisation server: {0}")]
    Network(String),
    #[error("the authorisation server answered something unexpected: {0}")]
    Protocol(String),
    #[error("the redirect does not belong to this login")]
    State,
    #[error("the sign-in was not finished in time")]
    TimedOut,
    #[error("cancelled")]
    Cancelled,
}

impl Tokens {
    /// The `auth.json` entry for these tokens. `previous` is the entry being replaced, whose
    /// refresh token and account fields carry over when the answer repeats neither.
    pub fn into_credential(self, previous: Option<&Credential>) -> Credential {
        let (old_refresh, mut extra) = match previous {
            Some(Credential::OAuth { refresh, extra, .. }) => (refresh.clone(), extra.clone()),
            _ => (String::new(), BTreeMap::new()),
        };
        extra.extend(self.extra);
        Credential::OAuth {
            access: self.access,
            refresh: self.refresh.unwrap_or(old_refresh),
            expires: self.expires,
            extra,
        }
    }
}

/// `pairs` as an `application/x-www-form-urlencoded` body.
fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (name, value) in pairs {
        if !body.is_empty() {
            body.push('&');
        }
        body.push_str(name);
        body.push('=');
        for byte in value.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                body.push(char::from(byte));
            } else {
                body.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    body
}

/// `{name}` replaced by what `vars` says, so a provider can describe the body it needs.
pub(crate) fn fill(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (name, value) in vars {
        let hole = format!("{{{name}}}");
        if out.contains(&hole) {
            out = out.replace(&hole, value);
        }
    }
    out
}

/// A request to a token or device endpoint.
pub(crate) struct Call<'a> {
    pub url: &'a str,
    pub body: BodyKind,
    pub params: Vec<(String, String)>,
    pub headers: &'a BTreeMap<String, String>,
}

/// Posts a token request and returns the JSON answer; an answer with an `error` member is
/// [`OAuthError::Refused`] whatever its status.
pub(crate) async fn post(http: &reqwest::Client, call: &Call<'_>) -> Result<Value, OAuthError> {
    let mut request = http
        .post(call.url)
        // Some servers answer form-encoded text unless asked for JSON.
        .header("accept", "application/json")
        .timeout(REQUEST_TIMEOUT);
    request = match call.body {
        BodyKind::Form => {
            let pairs: Vec<(&str, &str)> = call
                .params
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            request
                .header("content-type", "application/x-www-form-urlencoded")
                .body(form_body(&pairs))
        }
        BodyKind::Json => {
            let object: serde_json::Map<String, Value> = call
                .params
                .iter()
                .map(|(name, value)| (name.clone(), Value::String(value.clone())))
                .collect();
            request.json(&Value::Object(object))
        }
    };
    for (name, value) in call.headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|e| OAuthError::Network(e.to_string()))?;
    read_answer(response).await
}

/// The JSON of an answer, or the refusal it carries.
async fn read_answer(response: reqwest::Response) -> Result<Value, OAuthError> {
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| OAuthError::Network(e.to_string()))?;
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if let Some(code) = body.get("error").and_then(Value::as_str) {
        let description = body
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return Err(OAuthError::Refused {
            code: code.to_string(),
            description: description.to_string(),
        });
    }
    if !status.is_success() || !body.is_object() {
        let shown: String = text.chars().take(200).collect();
        return Err(OAuthError::Protocol(format!("HTTP {}: {shown}", status.as_u16())));
    }
    Ok(body)
}

/// One request to a token endpoint, read through `map`.
pub(crate) async fn token_request(
    http: &reqwest::Client,
    call: &Call<'_>,
    map: &CredentialMap,
) -> Result<Tokens, OAuthError> {
    let body = post(http, call).await?;
    map.read(&body)
        .ok_or_else(|| OAuthError::Protocol("the answer has no access_token".to_string()))
}

/// Asks an account endpoint who signed in and keeps what it says.
pub(crate) async fn ask_userinfo(
    http: &reqwest::Client,
    userinfo: &UserInfo,
    tokens: &mut Tokens,
) -> Result<(), OAuthError> {
    let response = http
        .get(&userinfo.url)
        .header("accept", "application/json")
        .bearer_auth(&tokens.access)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| OAuthError::Network(e.to_string()))?;
    let body = read_answer(response).await?;
    for (name, path) in &userinfo.fields {
        if let Some(found) = map::text(&body, path) {
            tokens.extra.insert(name.clone(), Value::String(found));
        }
    }
    Ok(())
}

/// Where a provider's token and device endpoints are. A provider that publishes an OpenID
/// discovery document is asked rather than pinned, because xAI rotates its token endpoint.
pub(crate) async fn discover(
    http: &reqwest::Client,
    settings: &OAuthSettings,
) -> Result<(String, String), OAuthError> {
    let mut token = settings.token_url.clone();
    let mut device = settings.device_url.clone();
    let Some(url) = &settings.discovery_url else {
        return Ok((token, device));
    };
    let response = http
        .get(url)
        .header("accept", "application/json")
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| OAuthError::Network(e.to_string()))?;
    let body = read_answer(response).await?;
    if let Some(found) = map::text(&body, "token_endpoint") {
        token = found;
    }
    if let Some(found) = map::text(&body, "device_authorization_endpoint") {
        device = found;
    }
    if token.is_empty() {
        return Err(OAuthError::Protocol(format!("{url} names no token_endpoint")));
    }
    Ok((token, device))
}

/// How a login talks to the person signing in.
#[async_trait]
pub trait LoginUi: Send + Sync {
    /// Shows `text`.
    fn show(&self, text: &str);
    /// Opens `url` in the user's browser when that is possible. The address has been shown already.
    fn open(&self, _url: &str) {}
    /// Asks for one line; `None` when there is nobody to ask.
    async fn ask(&self, prompt: &str) -> Option<String>;
}

/// The page the user signs in on.
fn authorize_url(
    settings: &OAuthSettings,
    client_id: &str,
    challenge: Option<&str>,
    state: &str,
    redirect: &str,
) -> Result<String, OAuthError> {
    let mut url = reqwest::Url::parse(&settings.authorize_url)
        .map_err(|e| OAuthError::Config(format!("authorize_url: {e}")))?;
    {
        let mut query = url.query_pairs_mut();
        if settings.authorize_standard {
            let response_type = match settings.flow {
                OAuthFlow::Implicit => "token",
                _ => "code",
            };
            query
                .append_pair("response_type", response_type)
                .append_pair("client_id", client_id)
                .append_pair("redirect_uri", redirect);
            if let Some(challenge) = challenge {
                query
                    .append_pair("code_challenge", challenge)
                    .append_pair("code_challenge_method", "S256");
            }
            query.append_pair("state", state);
            if !settings.scopes.is_empty() {
                query.append_pair("scope", &settings.scopes.join(" "));
            }
        }
        let scope = settings.scopes.join(" ");
        let vars = [
            ("redirect_uri", redirect),
            ("code_challenge", challenge.unwrap_or_default()),
            ("state", state),
            ("client_id", client_id),
            ("scope", scope.as_str()),
        ];
        for (name, template) in &settings.authorize_params {
            query.append_pair(name, &fill(template, &vars));
        }
    }
    Ok(url.into())
}

/// The parameters of a code exchange: what every server takes, then what the provider adds.
fn code_params(
    settings: &OAuthSettings,
    client: &(String, String),
    code: &str,
    state: &str,
    redirect: &str,
    verifier: Option<&str>,
) -> Vec<(String, String)> {
    let (client_id, client_secret) = client;
    let mut params = Vec::new();
    if settings.token_standard {
        params.extend([
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".to_string(), code.to_string()),
            ("redirect_uri".to_string(), redirect.to_string()),
            ("client_id".to_string(), client_id.clone()),
        ]);
        if !client_secret.is_empty() {
            params.push(("client_secret".to_string(), client_secret.clone()));
        }
        if let Some(verifier) = verifier {
            params.push(("code_verifier".to_string(), verifier.to_string()));
        }
    }
    let vars = [
        ("code", code),
        ("state", state),
        ("redirect_uri", redirect),
        ("client_id", client_id.as_str()),
        ("code_verifier", verifier.unwrap_or_default()),
    ];
    for (name, template) in &settings.token_params {
        params.push((name.clone(), fill(template, &vars)));
    }
    params
}

/// Sends the person to the sign-in page and returns the parameters the browser was redirected
/// back with, and the redirect address the server was given.
async fn redirected(
    settings: &OAuthSettings,
    client_id: &str,
    challenge: Option<&str>,
    state: &str,
    ui: &dyn LoginUi,
    cancel: &CancellationToken,
) -> Result<(loopback::Params, String), OAuthError> {
    let expected = settings.check_state.then_some(state);
    let implicit = settings.flow == OAuthFlow::Implicit;
    // A redirect the server insists on that is not a loopback address cannot be listened for.
    let fixed = settings.redirect_uri.as_deref();
    let listen =
        fixed.is_none_or(|uri| uri.starts_with("http://127.0.0.1") || uri.starts_with("http://localhost"));
    let listener = match listen {
        true => Some(
            loopback::Loopback::bind(
                settings.redirect_host(),
                settings.redirect_port,
                settings.redirect_path(),
            )
            .await,
        ),
        false => None,
    };
    let redirect = match (fixed, &listener) {
        (Some(uri), _) => uri.to_string(),
        (None, Some(Ok(listener))) => listener.redirect_uri(),
        (None, _) => loopback::redirect_uri(
            settings.redirect_host(),
            settings.redirect_port,
            settings.redirect_path(),
        ),
    };
    let url = authorize_url(settings, client_id, challenge, state, &redirect)?;
    if let Some(instructions) = &settings.instructions {
        ui.show(instructions);
    }
    ui.show(&format!("Sign in on this page:\n{url}"));
    ui.open(&url);
    let params = match listener {
        Some(Ok(listener)) => tokio::select! {
            params = listener.wait_params(expected, implicit) => params?,
            _ = tokio::time::sleep(LOGIN_TIMEOUT) => return Err(OAuthError::TimedOut),
            _ = cancel.cancelled() => return Err(OAuthError::Cancelled),
        },
        // Nothing is listening: either the provider redirects somewhere this process cannot
        // receive, or something else holds the port. Either way the browser's address bar ends up
        // holding what the listener would have been given.
        other => {
            let (prompt, blocked) = match other {
                Some(Err(why)) => (
                    format!(
                        "Port {} is in use ({why}). After signing in, paste the address the browser \
                         ends up at:",
                        settings.redirect_port
                    ),
                    format!("cannot listen on port {}: {why}", settings.redirect_port),
                ),
                _ => (
                    "After signing in, paste the address the browser ends up at:".to_string(),
                    "there is nobody to finish the sign-in".to_string(),
                ),
            };
            let pasted = ui.ask(&prompt).await.ok_or(OAuthError::Config(blocked))?;
            loopback::parse_pasted(&pasted, expected, implicit)?
        }
    };
    Ok((params, redirect))
}

async fn login_with_code(
    http: &reqwest::Client,
    settings: &OAuthSettings,
    token_url: &str,
    ui: &dyn LoginUi,
    cancel: &CancellationToken,
) -> Result<Tokens, OAuthError> {
    let client = settings.client()?;
    let pkce = settings.pkce.then(pkce::Pkce::new);
    let state = pkce::random_token();
    let challenge = pkce.as_ref().map(|p| p.challenge.as_str());
    let (redirect_params, redirect) = redirected(settings, &client.0, challenge, &state, ui, cancel).await?;
    let code = loopback::code_of(&redirect_params)?;
    let params = code_params(
        settings,
        &client,
        &code,
        &state,
        &redirect,
        pkce.as_ref().map(|p| p.verifier.as_str()),
    );
    let call = Call {
        url: token_url,
        body: settings.token_body,
        params,
        headers: &settings.token_headers,
    };
    token_request(http, &call, &settings.credential).await
}

/// An implicit grant: the redirect carries the token, read through the credential map as a
/// token answer would be.
async fn login_implicit(
    settings: &OAuthSettings,
    ui: &dyn LoginUi,
    cancel: &CancellationToken,
) -> Result<Tokens, OAuthError> {
    let client = settings.client()?;
    let state = pkce::random_token();
    let (params, _) = redirected(settings, &client.0, None, &state, ui, cancel).await?;
    let answer: serde_json::Map<String, Value> = params
        .into_iter()
        .map(|(name, value)| (name, Value::String(value)))
        .collect();
    settings
        .credential
        .read(&Value::Object(answer))
        .ok_or_else(|| OAuthError::Protocol("the redirect carries no access token".to_string()))
}

async fn login_with_device(
    http: &reqwest::Client,
    settings: &OAuthSettings,
    endpoints: &(String, String),
    ui: &dyn LoginUi,
    cancel: &CancellationToken,
) -> Result<Tokens, OAuthError> {
    let code = device::start(http, settings, &endpoints.1).await?;
    if let Some(instructions) = &settings.instructions {
        ui.show(&fill(instructions, &[("user_code", code.user_code.as_str())]));
    }
    ui.show(&format!(
        "Open {} and enter the code {}",
        code.verification_uri, code.user_code
    ));
    ui.open(
        code.verification_uri_complete
            .as_deref()
            .unwrap_or(&code.verification_uri),
    );
    device::poll(
        http,
        settings,
        &endpoints.0,
        &code,
        device::SLOW_DOWN_STEP,
        cancel,
    )
    .await
}

/// What answers each of `settings.inputs`: `given` (the command line), else its environment
/// variable, else the person. An answer is trimmed of a scheme, slashes and the input's suffix,
/// and must be a host name or a name made of letters, digits, `-`, `_` and `.`, since it goes into
/// addresses.
async fn answers(
    settings: &OAuthSettings,
    given: &BTreeMap<String, String>,
    ui: &dyn LoginUi,
) -> Result<Vec<(String, String)>, OAuthError> {
    let mut out = Vec::new();
    for input in &settings.inputs {
        let from_env = || {
            input
                .env
                .as_deref()
                .and_then(|name| std::env::var(name).ok())
                .filter(|value| !value.trim().is_empty())
        };
        let raw = match given.get(&input.name).cloned().or_else(from_env) {
            Some(value) => value,
            None => ui.ask(&input.prompt).await.unwrap_or_default(),
        };
        let mut value = raw.trim();
        for scheme in ["https://", "http://"] {
            value = value.strip_prefix(scheme).unwrap_or(value);
        }
        value = value.trim_end_matches('/');
        if let Some(suffix) = &input.suffix {
            value = value.strip_suffix(suffix.as_str()).unwrap_or(value);
        }
        if value.is_empty() && !input.optional {
            return Err(OAuthError::Config(format!(
                "the sign-in needs the {}, and none was given",
                input.name
            )));
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        {
            return Err(OAuthError::Config(format!(
                "`{value}` is not a valid {}: use letters, digits, `-`, `_` and `.`",
                input.name
            )));
        }
        out.push((input.name.clone(), value.to_string()));
    }
    Ok(out)
}

/// Signs in with the server `settings` describes and returns the entry to keep in `auth.json`.
pub async fn login(
    http: &reqwest::Client,
    settings: &OAuthSettings,
    ui: &dyn LoginUi,
    cancel: &CancellationToken,
) -> Result<Credential, OAuthError> {
    login_with(http, settings, &BTreeMap::new(), ui, cancel).await
}

/// [`login`], with `given` answering the settings' inputs by name. The answers and the API
/// address the settings name are kept with the login.
pub async fn login_with(
    http: &reqwest::Client,
    settings: &OAuthSettings,
    given: &BTreeMap<String, String>,
    ui: &dyn LoginUi,
    cancel: &CancellationToken,
) -> Result<Credential, OAuthError> {
    settings.check().map_err(OAuthError::Config)?;
    let answers = answers(settings, given, ui).await?;
    let settings = &settings.filled(&answers);
    let endpoints = discover(http, settings).await?;
    let mut tokens = match settings.flow {
        OAuthFlow::Pkce => login_with_code(http, settings, &endpoints.0, ui, cancel).await?,
        OAuthFlow::Device => login_with_device(http, settings, &endpoints, ui, cancel).await?,
        OAuthFlow::Implicit => login_implicit(settings, ui, cancel).await?,
        OAuthFlow::DeviceAuthCode => {
            device::login_auth_code(http, settings, &endpoints.0, ui, cancel).await?
        }
    };
    if let Some(userinfo) = &settings.userinfo {
        ask_userinfo(http, userinfo, &mut tokens).await?;
    }
    for (name, value) in answers.into_iter().filter(|(_, value)| !value.is_empty()) {
        tokens.extra.insert(name, Value::String(value));
    }
    if let Some(base) = &settings.api_base_url {
        tokens.extra.insert(
            BASE_URL_FIELD.to_string(),
            Value::String(base.trim_end_matches('/').to_string()),
        );
    }
    Ok(tokens.into_credential(None))
}

#[cfg(test)]
mod tests {
    use crate::credentials::now_ms;

    use super::*;

    fn settings(flow: OAuthFlow) -> OAuthSettings {
        OAuthSettings {
            flow,
            client_id: "app".into(),
            authorize_url: "https://id.example/authorize?tenant=t 1".into(),
            token_url: "https://id.example/token".into(),
            scopes: vec!["openid".into(), "offline_access".into()],
            ..Default::default()
        }
    }

    #[test]
    fn a_provider_adds_its_own_authorise_parameters_and_may_refuse_pkce() {
        let mut settings = settings(OAuthFlow::Pkce);
        settings.authorize_params = BTreeMap::from([
            ("code".to_string(), "true".to_string()),
            ("originator".to_string(), "azazel".to_string()),
        ]);
        let url = authorize_url(&settings, "app", None, "st", "http://127.0.0.1:7/callback").unwrap();
        assert!(!url.contains("code_challenge"), "{url}");
        assert!(url.ends_with("&state=st&scope=openid+offline_access&code=true&originator=azazel"));
    }

    #[test]
    fn token_parameters_are_the_standard_ones_plus_what_the_provider_describes() {
        let mut settings = settings(OAuthFlow::Pkce);
        settings.token_params = BTreeMap::from([
            ("state".to_string(), "{state}".to_string()),
            ("provider".to_string(), "zai".to_string()),
        ]);
        settings.client_secret = "shh".into();
        let client = settings.client().unwrap();
        let params = code_params(&settings, &client, "c1", "st", "http://back", Some("v1"));
        assert_eq!(
            params,
            vec![
                ("grant_type".to_string(), "authorization_code".to_string()),
                ("code".to_string(), "c1".to_string()),
                ("redirect_uri".to_string(), "http://back".to_string()),
                ("client_id".to_string(), "app".to_string()),
                ("client_secret".to_string(), "shh".to_string()),
                ("code_verifier".to_string(), "v1".to_string()),
                ("provider".to_string(), "zai".to_string()),
                ("state".to_string(), "st".to_string()),
            ]
        );
    }

    #[test]
    fn base64_settings_are_decoded_once_for_the_wire() {
        let settings = OAuthSettings {
            client_id: "YXBwLWlk".into(),
            client_id_encoding: Encoding::Base64,
            client_secret: "c2VjcmV0".into(),
            client_secret_encoding: Encoding::Base64,
            ..settings(OAuthFlow::Pkce)
        };
        assert_eq!(
            settings.client().unwrap(),
            ("app-id".to_string(), "secret".to_string())
        );
        let broken = OAuthSettings {
            client_id: "not base64!".into(),
            client_id_encoding: Encoding::Base64,
            ..settings
        };
        assert!(broken.check().is_err());
    }

    #[test]
    fn settings_name_what_their_flow_lacks() {
        assert_eq!(settings(OAuthFlow::Pkce).check(), Ok(()));
        assert_eq!(
            settings(OAuthFlow::Device).check(),
            Err("`device_url` is needed for this flow".to_string())
        );
        let mut no_client = settings(OAuthFlow::Pkce);
        no_client.client_id = " ".into();
        assert_eq!(
            no_client.check(),
            Err("`client_id` is needed for this flow".to_string())
        );
    }

    #[test]
    fn answers_fill_the_addresses_and_drop_scopes_left_empty() {
        let settings = OAuthSettings {
            token_url: "https://{account}.example/token".into(),
            scopes: vec!["base".into(), "role:{role}".into()],
            api_base_url: Some("https://api.{account}.example".into()),
            ..settings(OAuthFlow::Pkce)
        };
        let filled = settings.filled(&[
            ("account".to_string(), "acme".to_string()),
            ("role".to_string(), String::new()),
        ]);
        assert_eq!(filled.token_url, "https://acme.example/token");
        assert_eq!(filled.scopes, ["base"]);
        assert_eq!(filled.api_base_url.as_deref(), Some("https://api.acme.example"));
    }

    #[test]
    fn a_refresh_that_sends_no_new_refresh_token_keeps_the_old_one() {
        let old = Credential::OAuth {
            access: "a1".into(),
            refresh: "r1".into(),
            expires: 0,
            extra: BTreeMap::from([
                ("account_id".to_string(), Value::from("me")),
                ("org_id".to_string(), Value::from("org-1")),
            ]),
        };
        let tokens = |refresh: Option<&str>| Tokens {
            access: "a2".into(),
            refresh: refresh.map(str::to_string),
            expires: now_ms() + 60_000,
            extra: BTreeMap::from([("email".to_string(), Value::from("me@example.com"))]),
        };
        let Credential::OAuth {
            access,
            refresh,
            expires,
            extra,
        } = tokens(None).into_credential(Some(&old))
        else {
            panic!("an OAuth entry");
        };
        assert_eq!((access.as_str(), refresh.as_str()), ("a2", "r1"));
        // The organisation a credential belongs to is fixed at login; the answer adds to it.
        assert_eq!(extra["org_id"], "org-1");
        assert_eq!(extra["account_id"], "me");
        assert_eq!(extra["email"], "me@example.com");
        let left = expires - now_ms();
        assert!((50_000..=60_000).contains(&left), "{left}");
        let Credential::OAuth { refresh, .. } = tokens(Some("r2")).into_credential(Some(&old)) else {
            panic!("an OAuth entry");
        };
        assert_eq!(refresh, "r2");
    }
}
