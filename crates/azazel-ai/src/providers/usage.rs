//! What a subscription has left of its usage windows, asked from the provider itself.
//!
//! Two providers answer: Anthropic for a Claude login (`GET {base}/api/oauth/usage`) and the
//! ChatGPT backend for a ChatGPT login (`GET {root}/wham/usage`, beside the `/codex` address
//! requests go to). Neither endpoint is documented; both are what the vendors' own clients ask.
//! The windows a response's headers report, without asking, are in [`crate::limits`].

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::credentials::{Auth, Credentials, account_key, now_ms};
use crate::limits::{REJECTED, Window};
use crate::oauth::refresh::fresh_login;
use crate::providers::presets::ProviderConfig;

const ANTHROPIC: &str = "anthropic";
const CHATGPT: &str = "openai-codex";
const TIMEOUT: Duration = Duration::from_secs(10);

/// What one account has left, as the provider reported it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    pub provider: String,
    pub account: String,
    /// The plan as the provider names it (`plus`, `pro`), when it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    /// Whether usage beyond the plan is paid for: on, off, or not said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_usage: Option<bool>,
    /// Unix seconds.
    pub fetched_at: u64,
}

/// Whether `provider` has an endpoint that reports a login's usage.
pub fn supported(provider: &ProviderConfig) -> bool {
    provider.oauth.is_some() && matches!(provider.id.as_str(), ANTHROPIC | CHATGPT)
}

/// Asks `provider` what the login stored as `account` has left, and keeps the answer in the
/// limit store. Errors say what went wrong without the address or the token.
pub async fn fetch(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    credentials: &Credentials,
    account: &str,
) -> Result<UsageReport, String> {
    let id = provider.id.as_str();
    let url = match id {
        ANTHROPIC => format!("{}/api/oauth/usage", provider.base_url),
        // Requests go to `{root}/codex`; the account endpoints sit beside it.
        CHATGPT => format!(
            "{}/wham/usage",
            provider
                .base_url
                .strip_suffix("/codex")
                .unwrap_or(&provider.base_url)
        ),
        _ => return Err(format!("{id} does not report usage")),
    };
    let is_login = credentials
        .accounts(id)
        .iter()
        .any(|stored| stored.label == account && stored.login);
    if !is_login {
        return Err(format!(
            "account {account} of {id} is not a login; usage is reported for subscriptions only"
        ));
    }
    let (access, fields) = fresh_login(credentials, provider, &account_key(id, account))
        .await
        .map_err(|e| e.to_string())?;
    let mut request = client
        .get(url)
        .timeout(TIMEOUT)
        .bearer_auth(&access)
        .header("accept", "application/json");
    for (name, value) in (Auth::OAuth { access, fields }).login_headers(provider) {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("could not reach {id} for its usage: {}", e.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 | 403 => {
                format!("{id} refused the login when asked for its usage (HTTP {status}): sign in again")
            }
            _ => format!("{id} answered HTTP {status} when asked for its usage"),
        });
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| format!("{id} sent usage that is not JSON"))?;
    let now = now_ms() / 1000;
    let mut report = match id {
        ANTHROPIC => parse_anthropic(&body),
        _ => parse_chatgpt(&body, now),
    };
    report.provider = id.to_string();
    report.account = account.to_string();
    report.fetched_at = now;
    credentials.limits().record(id, account, report.windows.clone());
    Ok(report)
}

fn blank() -> UsageReport {
    UsageReport {
        provider: String::new(),
        account: String::new(),
        plan: None,
        windows: Vec::new(),
        extra_usage: None,
        fetched_at: 0,
    }
}

/// `allowed` below a full window, `rejected` at it.
fn status(used: f32) -> String {
    if used >= 1.0 { REJECTED } else { "allowed" }.to_string()
}

/// The answer of `GET /api/oauth/usage`: `five_hour`, `seven_day`, `seven_day_opus` and
/// `seven_day_sonnet`, each `{ utilization: 0..100, resets_at: RFC 3339 }` or null, and
/// `extra_usage: { is_enabled, .. }`.
pub fn parse_anthropic(body: &Value) -> UsageReport {
    let mut report = blank();
    for (field, name) in [
        ("five_hour", "5h"),
        ("seven_day", "7d"),
        ("seven_day_opus", "7d-opus"),
        ("seven_day_sonnet", "7d-sonnet"),
    ] {
        let bucket = &body[field];
        let Some(percent) = bucket["utilization"].as_f64() else {
            continue;
        };
        let used = (percent / 100.0) as f32;
        report.windows.push(Window {
            name: name.to_string(),
            used,
            resets_at: bucket["resets_at"].as_str().and_then(unix_seconds),
            status: status(used),
        });
    }
    report.extra_usage = body["extra_usage"]["is_enabled"].as_bool();
    report
}

/// The answer of `GET /wham/usage`: `plan_type`, `rate_limit.primary_window` and
/// `.secondary_window`, each `{ used_percent, limit_window_seconds, reset_at | reset_after_seconds }`,
/// and `credits: { has_credits, unlimited }`.
pub fn parse_chatgpt(body: &Value, now: u64) -> UsageReport {
    let mut report = blank();
    report.plan = body["plan_type"].as_str().map(str::to_string);
    let limit = &body["rate_limit"];
    let reached = limit["limit_reached"].as_bool() == Some(true);
    for field in ["primary_window", "secondary_window"] {
        let window = &limit[field];
        let Some(percent) = window["used_percent"].as_f64() else {
            continue;
        };
        let used = (percent / 100.0) as f32;
        report.windows.push(Window {
            name: window_name(window["limit_window_seconds"].as_u64(), field),
            used,
            resets_at: window["reset_at"]
                .as_u64()
                .or_else(|| Some(now + window["reset_after_seconds"].as_u64()?)),
            status: if reached {
                status(used)
            } else {
                "allowed".to_string()
            },
        });
    }
    let credits = &body["credits"];
    if credits.is_object() {
        report.extra_usage = Some(
            credits["has_credits"].as_bool() == Some(true) || credits["unlimited"].as_bool() == Some(true),
        );
    }
    report
}

/// A window by its length: `5h`, `7d`; `primary` or `secondary` when the length is not said.
pub(crate) fn window_name(seconds: Option<u64>, field: &str) -> String {
    const HOUR: u64 = 3600;
    match seconds {
        Some(seconds) if seconds >= 48 * HOUR => format!("{}d", (seconds + 12 * HOUR) / (24 * HOUR)),
        Some(seconds) if seconds >= HOUR => format!("{}h", (seconds + HOUR / 2) / HOUR),
        Some(seconds) if seconds > 0 => format!("{}min", seconds.div_ceil(60)),
        _ => field.trim_end_matches("_window").to_string(),
    }
}

/// Unix seconds of an RFC 3339 time such as `2026-11-12T18:00:00.123+00:00`.
fn unix_seconds(text: &str) -> Option<u64> {
    let (date, time) = text.trim().split_once(['T', 't', ' '])?;
    let mut date = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    // The offset starts at `Z`, `+` or `-`; a time of day has neither.
    let offset_at = time.find(['Z', 'z', '+', '-']).unwrap_or(time.len());
    let (clock, offset) = time.split_at(offset_at);
    let mut clock = clock.split(':');
    let hour = clock.next()?.parse::<i64>().ok()?;
    let minute = clock.next()?.parse::<i64>().ok()?;
    let second = clock
        .next()
        .unwrap_or("0")
        .split('.')
        .next()?
        .parse::<i64>()
        .ok()?;
    let offset = match offset.as_bytes().first() {
        None | Some(b'Z' | b'z') => 0,
        Some(sign) => {
            let (hours, minutes) = offset[1..].split_once(':')?;
            let seconds = hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60;
            if *sign == b'-' { -seconds } else { seconds }
        }
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second - offset).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use serde_json::json;
    use wiremock::matchers::{header, headers, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::credentials::Credential;
    use crate::providers::presets;

    #[test]
    fn times_are_read_as_unix_seconds() {
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_seconds("2026-11-12T18:00:00Z"), Some(1_794_506_400));
        assert_eq!(
            unix_seconds("2026-11-12T18:00:00.123456+00:00"),
            Some(1_794_506_400)
        );
        assert_eq!(unix_seconds("2026-11-12T20:00:00+02:00"), Some(1_794_506_400));
        assert_eq!(unix_seconds("2026-11-12T13:30:00-04:30"), Some(1_794_506_400));
        assert_eq!(unix_seconds("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        assert_eq!(unix_seconds("tomorrow"), None);
        assert_eq!(unix_seconds("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn windows_are_named_by_their_length() {
        assert_eq!(window_name(Some(18_000), "primary_window"), "5h");
        assert_eq!(window_name(Some(604_800), "secondary_window"), "7d");
        assert_eq!(window_name(Some(86_400), "primary_window"), "24h");
        assert_eq!(window_name(Some(900), "primary_window"), "15min");
        assert_eq!(window_name(None, "secondary_window"), "secondary");
    }

    #[test]
    fn a_claude_answer_becomes_windows() {
        let report = parse_anthropic(&json!({
            "five_hour": { "utilization": 42.0, "resets_at": "2026-11-12T18:00:00+00:00" },
            "seven_day": { "utilization": 100, "resets_at": "2026-11-15T00:00:00Z" },
            "seven_day_opus": null,
            "seven_day_sonnet": { "utilization": 3.5, "resets_at": null },
            "extra_usage": { "is_enabled": false, "monthly_limit": null, "used_credits": null }
        }));
        let names: Vec<&str> = report.windows.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["5h", "7d", "7d-sonnet"]);
        assert_eq!(
            report.windows[0],
            Window {
                name: "5h".into(),
                used: 0.42,
                resets_at: Some(1_794_506_400),
                status: "allowed".into()
            }
        );
        assert!(report.windows[1].exhausted());
        assert_eq!(report.windows[2].resets_at, None);
        assert_eq!(report.extra_usage, Some(false));
        // Anything else, an error object included, is simply no windows.
        let empty = parse_anthropic(&json!({ "error": { "type": "x" } }));
        assert!(empty.windows.is_empty() && empty.extra_usage.is_none());
    }

    #[test]
    fn a_chatgpt_answer_becomes_windows() {
        let report = parse_chatgpt(
            &json!({
                "plan_type": "plus",
                "rate_limit": {
                    "allowed": true,
                    "limit_reached": false,
                    "primary_window": {
                        "used_percent": 12, "limit_window_seconds": 18000,
                        "reset_after_seconds": 600, "reset_at": 1_794_506_400u64
                    },
                    "secondary_window": {
                        "used_percent": 100, "limit_window_seconds": 604800, "reset_after_seconds": 3600
                    }
                },
                "credits": { "has_credits": false, "unlimited": false, "balance": "0" }
            }),
            1_000,
        );
        assert_eq!(report.plan.as_deref(), Some("plus"));
        assert_eq!(
            report.windows,
            [
                Window {
                    name: "5h".into(),
                    used: 0.12,
                    resets_at: Some(1_794_506_400),
                    status: "allowed".into()
                },
                // Full, but the backend still takes requests: not refused.
                Window {
                    name: "7d".into(),
                    used: 1.0,
                    resets_at: Some(4_600),
                    status: "allowed".into()
                },
            ]
        );
        assert_eq!(report.extra_usage, Some(false));

        let reached = parse_chatgpt(
            &json!({ "rate_limit": { "limit_reached": true, "primary_window": { "used_percent": 100 } } }),
            0,
        );
        assert_eq!(reached.windows[0].name, "primary");
        assert!(reached.windows[0].exhausted());
        assert_eq!(reached.extra_usage, None);
    }

    fn login(email: &str, extra: &[(&str, &str)]) -> Credential {
        let mut fields: std::collections::BTreeMap<String, Value> = extra
            .iter()
            .map(|(name, value)| (name.to_string(), json!(value)))
            .collect();
        fields.insert("email".to_string(), json!(email));
        Credential::OAuth {
            access: "access-token".into(),
            refresh: "refresh-token".into(),
            expires: now_ms() + 3_600_000,
            extra: fields,
        }
    }

    fn preset(id: &str, base_url: String) -> ProviderConfig {
        let mut provider = presets::resolve(&Default::default())
            .unwrap()
            .into_iter()
            .find(|provider| provider.id == id)
            .unwrap();
        provider.base_url = base_url;
        provider
    }

    #[tokio::test]
    async fn both_endpoints_are_asked_with_the_login_and_fill_the_store() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .and(header("authorization", "Bearer access-token"))
            .and(headers(
                "anthropic-beta",
                vec!["claude-code-20250219", "oauth-2025-04-20"],
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "five_hour": { "utilization": 81.0, "resets_at": "2026-11-12T18:00:00Z" },
                "seven_day": { "utilization": 20.0, "resets_at": "2026-11-15T00:00:00Z" },
                "extra_usage": { "is_enabled": true }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/backend-api/wham/usage"))
            .and(header("authorization", "Bearer access-token"))
            .and(header("chatgpt-account-id", "acct-7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "plan_type": "pro",
                "rate_limit": {
                    "primary_window": { "used_percent": 5, "limit_window_seconds": 18000, "reset_at": 1_794_506_400u64 }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/gone/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(401).set_body_string("token=secret"))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let credentials = Credentials::with_env(dir.path().join("auth.json"), Arc::new(|_: &str| None));
        credentials
            .store("anthropic", login("a@example.com", &[]))
            .unwrap();
        credentials
            .store(
                &account_key("anthropic", "work"),
                Credential::ApiKey { key: "k".into() },
            )
            .unwrap();
        credentials
            .store(
                "openai-codex",
                login("c@example.com", &[("account_id", "acct-7")]),
            )
            .unwrap();
        let client = crate::tls::client();

        let claude = preset("anthropic", server.uri());
        assert!(supported(&claude));
        let report = fetch(&client, &claude, &credentials, "default").await.unwrap();
        assert_eq!(
            (report.provider.as_str(), report.account.as_str()),
            ("anthropic", "default")
        );
        assert_eq!(report.windows.len(), 2);
        assert_eq!(report.extra_usage, Some(true));
        assert!(report.fetched_at > 0);
        // The store has what the endpoint said, under the account that was asked about.
        let stored = credentials.limits().windows("anthropic", "default");
        assert_eq!(stored[0].name, "5h");
        assert!((stored[0].used - 0.81).abs() < 1e-6);
        assert!(credentials.limits().windows("anthropic", "work").is_empty());

        let chatgpt = preset("openai-codex", format!("{}/backend-api/codex", server.uri()));
        let report = fetch(&client, &chatgpt, &credentials, "default").await.unwrap();
        assert_eq!(report.plan.as_deref(), Some("pro"));
        assert_eq!(report.windows[0].name, "5h");
        assert_eq!(credentials.limits().windows("openai-codex", "default").len(), 1);

        // A stored key is not a subscription, and a refusal names neither address nor body.
        let refused = fetch(&client, &claude, &credentials, "work").await.unwrap_err();
        assert!(refused.contains("not a login"), "{refused}");
        let gone = preset("anthropic", format!("{}/gone", server.uri()));
        let refused = fetch(&client, &gone, &credentials, "default").await.unwrap_err();
        assert!(
            refused.contains("401") && refused.contains("sign in again"),
            "{refused}"
        );
        assert!(
            !refused.contains("secret") && !refused.contains("127.0.0.1"),
            "{refused}"
        );
        let other = preset("openai", server.uri());
        assert!(!supported(&other));
    }
}
