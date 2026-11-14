//! The device authorisation flow (RFC 8628): the user enters a short code on another page while
//! this side asks the token endpoint whether they have finished.

use std::time::Duration;

use serde::Deserialize;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{
    BodyKind, Call, LoginUi, OAuthError, OAuthSettings, Tokens, code_params, fill, post, read_answer, token_request,
};

/// How much longer to wait between requests each time the server says `slow_down`.
pub const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
const DEFAULT_INTERVAL_SECS: u64 = 5;
const DEFAULT_EXPIRES_SECS: u64 = 900;
const GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// What the device endpoint answers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    #[serde(alias = "verification_url")]
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    /// Seconds until the codes stop working.
    pub expires_in: Option<u64>,
    /// Seconds to wait between token requests.
    pub interval: Option<u64>,
}

/// Asks for a device code and the code the user enters.
pub async fn start(
    http: &reqwest::Client,
    settings: &OAuthSettings,
    device_url: &str,
) -> Result<DeviceCode, OAuthError> {
    let (client_id, client_secret) = settings.client()?;
    let mut params = vec![("client_id".to_string(), client_id)];
    if !client_secret.is_empty() {
        params.push(("client_secret".to_string(), client_secret));
    }
    let scope = settings.scopes.join(" ");
    if !scope.is_empty() {
        params.push(("scope".to_string(), scope));
    }
    let call = Call {
        url: device_url,
        body: settings.token_body,
        params,
        headers: &settings.device_headers,
    };
    let body = post(http, &call).await?;
    serde_json::from_value(body).map_err(|e| OAuthError::Protocol(format!("device endpoint: {e}")))
}

/// Asks the token endpoint until the user has finished, the codes expire or `cancel` fires.
/// `step` is added to the wait whenever the server answers `slow_down`.
pub async fn poll(
    http: &reqwest::Client,
    settings: &OAuthSettings,
    token_url: &str,
    code: &DeviceCode,
    step: Duration,
    cancel: &CancellationToken,
) -> Result<Tokens, OAuthError> {
    let (client_id, client_secret) = settings.client()?;
    let deadline = Instant::now() + Duration::from_secs(code.expires_in.unwrap_or(DEFAULT_EXPIRES_SECS));
    let mut interval = Duration::from_secs(code.interval.unwrap_or(DEFAULT_INTERVAL_SECS));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err(OAuthError::Cancelled),
            _ = tokio::time::sleep(interval) => {}
        }
        if Instant::now() >= deadline {
            return Err(OAuthError::TimedOut);
        }
        let mut params = vec![
            ("grant_type".to_string(), GRANT.to_string()),
            ("device_code".to_string(), code.device_code.clone()),
            ("client_id".to_string(), client_id.clone()),
        ];
        if !client_secret.is_empty() {
            params.push(("client_secret".to_string(), client_secret.clone()));
        }
        let call = Call {
            url: token_url,
            body: settings.token_body,
            params,
            headers: &settings.token_headers,
        };
        match token_request(http, &call, &settings.credential).await {
            Ok(tokens) => return Ok(tokens),
            Err(OAuthError::Refused { code, .. }) if code == "authorization_pending" => {}
            Err(OAuthError::Refused { code, .. }) if code == "slow_down" => interval += step,
            Err(OAuthError::Refused { code, .. }) if code == "expired_token" => {
                return Err(OAuthError::TimedOut);
            }
            Err(other) => return Err(other),
        }
    }
}

/// What the device endpoint of a `device-auth-code` login answers.
#[derive(Debug, Deserialize)]
struct AuthCodeDevice {
    device_auth_id: String,
    user_code: String,
    /// Seconds between polls, as a number or as text.
    #[serde(default)]
    interval: Option<serde_json::Value>,
}

/// What its poll endpoint answers once the user has entered the code.
#[derive(Debug, Deserialize)]
struct Approved {
    authorization_code: String,
    code_verifier: String,
}

/// A `device-auth-code` login: the device endpoint hands out a code to enter on
/// `verification_url`, the poll endpoint answers 403 or 404 until the user has, and then an
/// authorisation code with its PKCE verifier, which is exchanged at `token_url` like a browser
/// login's with `redirect_uri` as its redirect.
pub(super) async fn login_auth_code(
    http: &reqwest::Client,
    settings: &OAuthSettings,
    token_url: &str,
    ui: &dyn LoginUi,
    cancel: &CancellationToken,
) -> Result<Tokens, OAuthError> {
    let client = settings.client()?;
    let call = Call {
        url: &settings.device_url,
        body: BodyKind::Json,
        params: vec![("client_id".to_string(), client.0.clone())],
        headers: &settings.device_headers,
    };
    let device: AuthCodeDevice = serde_json::from_value(post(http, &call).await?)
        .map_err(|e| OAuthError::Protocol(format!("device endpoint: {e}")))?;
    let page = settings.verification_url.as_deref().unwrap_or_default();
    if let Some(instructions) = &settings.instructions {
        ui.show(&fill(instructions, &[("user_code", device.user_code.as_str())]));
    }
    ui.show(&format!("Open {page} and enter the code {}", device.user_code));
    ui.open(page);
    let seconds = device
        .interval
        .as_ref()
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.trim().parse().ok()))
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    let interval = Duration::from_secs(seconds);
    let deadline = Instant::now() + Duration::from_secs(DEFAULT_EXPIRES_SECS);
    let approved = loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err(OAuthError::Cancelled),
            _ = tokio::time::sleep(interval) => {}
        }
        if Instant::now() >= deadline {
            return Err(OAuthError::TimedOut);
        }
        let mut request = http
            .post(&settings.device_poll_url)
            .header("accept", "application/json")
            .timeout(super::REQUEST_TIMEOUT)
            .json(&serde_json::json!({
                "device_auth_id": device.device_auth_id,
                "user_code": device.user_code,
            }));
        for (name, value) in &settings.device_headers {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|e| OAuthError::Network(e.to_string()))?;
        // Not entered yet.
        if matches!(response.status().as_u16(), 403 | 404) {
            continue;
        }
        let body = read_answer(response).await?;
        break serde_json::from_value::<Approved>(body)
            .map_err(|e| OAuthError::Protocol(format!("device poll endpoint: {e}")))?;
    };
    let redirect = settings.redirect_uri.as_deref().unwrap_or_default();
    let params = code_params(
        settings,
        &client,
        &approved.authorization_code,
        "",
        redirect,
        Some(&approved.code_verifier),
    );
    let call = Call {
        url: token_url,
        body: settings.token_body,
        params,
        headers: &settings.token_headers,
    };
    token_request(http, &call, &settings.credential).await
}
