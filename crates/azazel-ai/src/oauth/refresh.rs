//! Keeping a stored login usable: a token with little time left is exchanged for a new one, once,
//! however many requests and processes want it at the same moment.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use super::{Call, OAuthError, ask_userinfo, discover, fill, token_request};
use crate::credentials::{Credential, Credentials, now_ms};
use crate::error::AiError;
use crate::providers::presets::ProviderConfig;

/// A token with less than this left is refreshed before it is used.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// The account fields of a stored login, as a header can use them.
fn fields(extra: &BTreeMap<String, Value>) -> BTreeMap<String, String> {
    extra
        .iter()
        .filter_map(|(name, value)| match value {
            Value::String(text) => Some((name.clone(), text.clone())),
            other => other.as_i64().map(|number| (name.clone(), number.to_string())),
        })
        .collect()
}

/// The access token of the login stored under `key` (an account of `provider`) and the account
/// fields kept with it, refreshed first when the token is about to expire.
pub(crate) async fn fresh_login(
    credentials: &Credentials,
    provider: &ProviderConfig,
    key: &str,
) -> Result<(String, BTreeMap<String, String>), AiError> {
    let id = &provider.id;
    let expired = || {
        AiError::Auth(
            match key
                .strip_prefix(id.as_str())
                .and_then(|rest| rest.strip_prefix('#'))
            {
                Some(label) => {
                    format!(
                        "login for {id} (account {label}) has expired: run `azazel login {id} --as {label}`"
                    )
                }
                None => format!("login for {id} has expired: run `azazel login {id}`"),
            },
        )
    };
    // The stored login when its access token is good for at least `margin` more milliseconds.
    let usable = |entry: Option<&Credential>, margin: u64| match entry {
        Some(Credential::OAuth {
            access,
            expires,
            extra,
            ..
        }) if *expires > now_ms() + margin => Some((access.clone(), fields(extra))),
        _ => None,
    };
    let margin = REFRESH_MARGIN.as_millis() as u64;
    if let Some(login) = usable(credentials.load().get(key), margin) {
        return Ok(login);
    }

    // One refresh at a time in this process, and the file lock keeps other processes out. Whoever
    // waited reads the file again: the one that held the lock has usually refreshed already.
    let _turn = credentials.refreshing().lock().await;
    let lock = credentials.lock_file().await.map_err(|e| {
        AiError::Auth(format!(
            "could not lock {}: {e}",
            credentials.auth_file().display()
        ))
    })?;
    let mut entries = credentials.load();
    let stored = entries.get(key);
    if let Some(login) = usable(stored, margin) {
        return Ok(login);
    }
    let still_good = usable(stored, 0);
    let (Some(Credential::OAuth { refresh, .. }), Some(settings)) = (stored, &provider.oauth) else {
        return still_good.ok_or_else(expired);
    };
    if refresh.is_empty() {
        return still_good.ok_or_else(expired);
    }
    let refresh = refresh.clone();
    let (client_id, client_secret) = settings
        .client()
        .map_err(|why| AiError::Auth(format!("[providers.{id}.oauth]: {why}")))?;
    let (configured, body, headers) = settings.refresh_call();
    // A provider that publishes a discovery document is asked again: the token endpoint it hands
    // out can move between the login and the first refresh.
    let discovered =
        settings.discovery_url.is_some() && settings.refresh.as_ref().is_none_or(|r| r.url.is_none());
    let url = match discovered {
        true => {
            discover(credentials.http(), settings)
                .await
                .map_err(|e| AiError::Network(format!("could not reach the login server of {id}: {e}")))?
                .0
        }
        false => configured.to_string(),
    };
    let mut params = vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("refresh_token".to_string(), refresh.clone()),
        ("client_id".to_string(), client_id.clone()),
    ];
    if !client_secret.is_empty() {
        params.push(("client_secret".to_string(), client_secret));
    }
    if let Some(extra) = &settings.refresh {
        let vars = [
            ("refresh_token", refresh.as_str()),
            ("client_id", client_id.as_str()),
        ];
        for (name, template) in &extra.params {
            params.push((name.clone(), fill(template, &vars)));
        }
    }
    let call = Call {
        url: &url,
        body,
        params,
        headers,
    };
    match token_request(credentials.http(), &call, settings.refresh_credential()).await {
        Ok(mut tokens) => {
            // The account endpoint is asked again only when the provider says a refreshed token
            // can belong to a different account or region than the one stored.
            if let Some(userinfo) = settings.refresh.as_ref().and_then(|r| r.userinfo.as_ref()) {
                ask_userinfo(credentials.http(), userinfo, &mut tokens)
                    .await
                    .map_err(|e| AiError::Network(format!("could not read the account of {id}: {e}")))?;
            }
            let renewed = tokens.into_credential(stored);
            let login = match &renewed {
                Credential::OAuth { access, extra, .. } => (access.clone(), fields(extra)),
                Credential::ApiKey { key } => (key.clone(), BTreeMap::new()),
            };
            entries.insert(key.to_string(), renewed);
            credentials.write_entries(&entries).map_err(|e| {
                AiError::Auth(format!(
                    "could not write {}: {e}",
                    credentials.auth_file().display()
                ))
            })?;
            drop(lock);
            Ok(login)
        }
        // Whatever went wrong, the token in hand still works for a while. After that, a refresh
        // token the server no longer accepts means signing in again; anything else may pass.
        Err(why) => still_good.ok_or_else(|| match why {
            OAuthError::Refused { .. } => expired(),
            other => AiError::Network(format!("could not refresh the login for {id}: {other}")),
        }),
    }
}
