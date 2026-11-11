//! Turning a token answer into an `auth.json` entry: where the tokens and the account fields sit
//! in the answer, and when the access token stops working.
//!
//! Authorisation servers agree on `access_token` and little else. A subscription login answers
//! with the account it signed in, sometimes nested (`account.uuid`), sometimes only inside the
//! access token's own JWT claims, and says how long the token lives in three different ways.
//! A provider describes its answer here instead of needing its own code.

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::credentials::now_ms;

/// Stored for a login whose token the server never expires. Far enough away that the refresh
/// margin can be added to `now` without overflowing, and that no clock makes it look past.
pub const NEVER: u64 = u64::MAX / 4;
/// Lifetime assumed for a token whose server does not say.
pub const DEFAULT_EXPIRES_SECS: u64 = 3600;

/// The value at a dotted path (`account.uuid`), if the answer has one. A key that contains dots
/// itself is looked up whole first, because OpenID claims are named after URLs
/// (`https://api.openai.com/auth.chatgpt_account_id`).
pub fn at<'a>(body: &'a Value, path: &str) -> Option<&'a Value> {
    if let Some(found) = body.get(path) {
        return Some(found);
    }
    let mut end = path.len();
    while let Some(dot) = path[..end].rfind('.') {
        if let Some(found) = body.get(&path[..dot])
            && let Some(deeper) = at(found, &path[dot + 1..])
        {
            return Some(deeper);
        }
        end = dot;
    }
    None
}

/// The non-empty string at a dotted path.
pub fn text(body: &Value, path: &str) -> Option<String> {
    at(body, path)
        .and_then(Value::as_str)
        .filter(|found| !found.is_empty())
        .map(str::to_string)
}

/// The claims of a JWT, without checking its signature: the token was just handed to us over TLS
/// by the server that signed it, and nothing here is trusted beyond display and routing.
pub fn claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `extra` filled in where `first` says nothing, one level deep, which is as deep as the nested
/// claim objects of an OpenID answer go.
fn merge(first: Value, extra: Value) -> Value {
    let Value::Object(extra) = extra else {
        return first;
    };
    let Value::Object(mut kept) = first else {
        return Value::Object(extra);
    };
    for (name, value) in extra {
        match (kept.get_mut(&name), value) {
            (None, value) => {
                kept.insert(name, value);
            }
            (Some(Value::Object(held)), Value::Object(deeper)) => {
                for (name, value) in deeper {
                    held.entry(name).or_insert(value);
                }
            }
            _ => {}
        }
    }
    Value::Object(kept)
}

/// The first of `names` (`user_id|sub`) the claims have.
pub fn claim(claims: &Value, names: &str) -> Option<String> {
    names.split('|').find_map(|name| text(claims, name.trim()))
}

/// Where one stored field comes from: a place in the answer, a claim of the access token, or
/// both, the answer first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    /// A dotted path into the token answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A claim of the access token; alternatives are separated by `|`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<String>,
}

impl Field {
    /// Reads the field, preferring what the answer says over what the token claims.
    fn read(&self, body: &Value, claims: &Value) -> Option<String> {
        let found = self.path.as_deref().and_then(|path| text(body, path));
        found.or_else(|| self.claim.as_deref().and_then(|names| claim(claims, names)))
    }
}

/// When the access token stops working.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Expiry {
    /// Seconds from now, as `expires_in` usually says.
    Seconds {
        #[serde(default = "expires_in")]
        path: String,
        /// Subtracted from the lifetime, so the token is replaced before a server refuses it.
        #[serde(default)]
        skew_ms: u64,
    },
    /// The `exp` claim of the access token.
    Jwt {
        #[serde(default)]
        skew_ms: u64,
        /// Used when the token carries no `exp`.
        #[serde(default = "day_ms")]
        fallback_ms: u64,
    },
    /// The server ends the session itself; the token is used until it is refused.
    Never,
}

fn expires_in() -> String {
    "expires_in".to_string()
}

fn day_ms() -> u64 {
    24 * 60 * 60 * 1000
}

impl Default for Expiry {
    fn default() -> Self {
        Expiry::Seconds {
            path: expires_in(),
            skew_ms: 0,
        }
    }
}

impl Expiry {
    /// The unix millisecond the token stops being used at.
    fn at(&self, body: &Value, access: &str) -> u64 {
        match self {
            Expiry::Never => NEVER,
            Expiry::Seconds { path, skew_ms } => {
                let seconds = at(body, path)
                    .and_then(seconds_of)
                    .unwrap_or(DEFAULT_EXPIRES_SECS);
                now_ms() + seconds.saturating_mul(1000).saturating_sub(*skew_ms)
            }
            Expiry::Jwt { skew_ms, fallback_ms } => claims(access)
                .as_ref()
                .and_then(|claims| at(claims, "exp").and_then(seconds_of))
                .map_or_else(
                    || now_ms() + fallback_ms,
                    |exp| exp.saturating_mul(1000).saturating_sub(*skew_ms),
                ),
        }
    }
}

/// `expires_in` as a number, or as the string some servers send.
fn seconds_of(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

/// Where the parts of a credential sit in a token answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialMap {
    #[serde(default = "access_token")]
    pub access: String,
    #[serde(default = "refresh_token")]
    pub refresh: String,
    #[serde(default)]
    pub expires: Expiry,
    /// Fields kept beside the tokens: `account_id`, `email`, `org_id`, `org_name`, and whatever
    /// else a provider needs at request time.
    #[serde(default)]
    pub fields: BTreeMap<String, Field>,
}

fn access_token() -> String {
    "access_token".to_string()
}

fn refresh_token() -> String {
    "refresh_token".to_string()
}

impl Default for CredentialMap {
    fn default() -> Self {
        CredentialMap {
            access: access_token(),
            refresh: refresh_token(),
            expires: Expiry::default(),
            fields: BTreeMap::new(),
        }
    }
}

/// What a token endpoint granted, read through a [`CredentialMap`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub access: String,
    /// Absent when the server keeps the refresh token it gave before.
    pub refresh: Option<String>,
    /// Unix milliseconds.
    pub expires: u64,
    /// Account fields to keep beside the tokens.
    pub extra: BTreeMap<String, Value>,
}

impl CredentialMap {
    /// Reads a token answer. Fails only when there is no access token to use.
    pub fn read(&self, body: &Value) -> Option<Tokens> {
        let access = text(body, &self.access)?;
        // An OpenID answer carries the account in the access token, the id token, or both; what
        // the access token says wins, as the hook of a provider that needs either would.
        let mut claims = claims(&access).unwrap_or(Value::Null);
        if let Some(id_claims) = text(body, "id_token").as_deref().and_then(self::claims) {
            claims = merge(claims, id_claims);
        }
        let extra = self
            .fields
            .iter()
            .filter_map(|(name, field)| {
                let found = field.read(body, &claims)?;
                Some((name.clone(), Value::String(found)))
            })
            .collect();
        Some(Tokens {
            expires: self.expires.at(body, &access),
            refresh: text(body, &self.refresh),
            access,
            extra,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// `{"alg":"none"}` and `claims`, as a JWT without a signature.
    fn jwt(claims: Value) -> String {
        let head = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let body = URL_SAFE_NO_PAD.encode(claims.to_string());
        format!("{head}.{body}.")
    }

    #[test]
    fn nested_paths_and_claims_fill_the_stored_fields() {
        let access = jwt(json!({ "sub": "acct-9", "exp": 4102444800u64 }));
        let map = CredentialMap {
            fields: BTreeMap::from([
                (
                    "email".to_string(),
                    Field {
                        path: Some("account.email_address".into()),
                        claim: None,
                    },
                ),
                (
                    "account_id".to_string(),
                    Field {
                        path: Some("account.uuid".into()),
                        claim: Some("sub".into()),
                    },
                ),
                (
                    "org_id".to_string(),
                    Field {
                        path: None,
                        claim: Some("missing|sub".into()),
                    },
                ),
            ]),
            ..Default::default()
        };
        let body = json!({
            "access_token": access,
            "refresh_token": "r1",
            "expires_in": 60,
            "account": { "email_address": "a@example.com" }
        });
        let tokens = map.read(&body).unwrap();
        assert_eq!(tokens.refresh.as_deref(), Some("r1"));
        // The answer has no `account.uuid`, so the claim of the token answers for it.
        assert_eq!(tokens.extra["account_id"], json!("acct-9"));
        assert_eq!(tokens.extra["email"], json!("a@example.com"));
        assert_eq!(tokens.extra["org_id"], json!("acct-9"));
        let left = tokens.expires.saturating_sub(now_ms());
        assert!((55_000..=60_000).contains(&left), "{left}ms left");
    }

    #[test]
    fn a_lifetime_is_shortened_by_the_skew_and_a_missing_one_is_assumed() {
        let seconds = |skew_ms| Expiry::Seconds {
            path: expires_in(),
            skew_ms,
        };
        let left = |expiry: Expiry, body: Value| expiry.at(&body, "").saturating_sub(now_ms());
        assert!((9_000..=10_000).contains(&left(seconds(0), json!({ "expires_in": 10 }))));
        assert!((4_000..=5_000).contains(&left(seconds(5_000), json!({ "expires_in": 10 }))));
        // Strings are lifetimes too, and an answer without one is given the default.
        assert!((9_000..=10_000).contains(&left(seconds(0), json!({ "expires_in": "10" }))));
        let assumed = left(seconds(0), json!({}));
        assert!((DEFAULT_EXPIRES_SECS * 1000 - 1_000..=DEFAULT_EXPIRES_SECS * 1000).contains(&assumed));
        // A skew longer than the lifetime makes the token stale at once rather than eternal.
        assert_eq!(left(seconds(60_000), json!({ "expires_in": 10 })), 0);
    }

    #[test]
    fn jwt_expiry_falls_back_when_the_token_says_nothing() {
        let body = |access: &str| json!({ "access_token": access });
        let map = CredentialMap {
            expires: Expiry::Jwt {
                skew_ms: 0,
                fallback_ms: 1_000,
            },
            ..Default::default()
        };
        let exp = now_ms() / 1000 + 120;
        let dated = map.read(&body(&jwt(json!({ "exp": exp })))).unwrap();
        assert_eq!(dated.expires, exp * 1000);

        let plain = map.read(&body("not-a-jwt")).unwrap();
        assert!(plain.expires.saturating_sub(now_ms()) <= 1_000);

        let never = CredentialMap {
            expires: Expiry::Never,
            ..Default::default()
        };
        assert_eq!(never.read(&body("t")).unwrap().expires, NEVER);
    }

    #[test]
    fn an_answer_without_an_access_token_is_not_a_credential() {
        let map = CredentialMap::default();
        assert!(map.read(&json!({ "refresh_token": "r" })).is_none());
        assert!(map.read(&json!({ "access_token": "" })).is_none());
        // Paths can reach into a wrapper the way plan logins answer.
        let nested = CredentialMap {
            access: "data.zai.access_token".into(),
            expires: Expiry::Never,
            ..Default::default()
        };
        let tokens = nested
            .read(&json!({ "data": { "zai": { "access_token": "t" } } }))
            .unwrap();
        assert_eq!((tokens.access.as_str(), tokens.refresh), ("t", None));
    }
}
