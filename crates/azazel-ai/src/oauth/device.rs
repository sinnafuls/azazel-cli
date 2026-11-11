//! The device authorisation flow (RFC 8628): the user enters a short code on another page while
//! this side asks the token endpoint whether they have finished.

use std::time::Duration;

use serde::Deserialize;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{Call, OAuthError, OAuthSettings, Tokens, post, token_request};

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
