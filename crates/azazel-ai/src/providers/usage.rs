//! What a subscription has left of its usage windows, asked from the provider itself.
//!
//! Two providers answer: Anthropic for a Claude login (`GET {base}/api/oauth/usage`) and the
//! ChatGPT backend for a ChatGPT login (`GET {root}/wham/usage`, beside the `/codex` address
//! requests go to). Neither endpoint is documented; both are what the vendors' own clients ask.
//! The windows a response's headers report, without asking, are in [`crate::limits`].

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::credentials::{Auth, Credentials, account_key, now_ms};
use crate::limits::{REJECTED, Window, read_file, update_file};
use crate::oauth::refresh::{fresh_login, stored_login};
use crate::providers::presets::ProviderConfig;

const ANTHROPIC: &str = "anthropic";
const CHATGPT: &str = "openai-codex";
const TIMEOUT: Duration = Duration::from_secs(10);

/// What extra usage (requests beyond the plan, paid for one by one) has cost in the current
/// period.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spend {
    pub used: f64,
    /// The most it may cost, when the account set a cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    /// `USD`.
    pub currency: String,
}

/// Resets of usage windows the account holds and may spend when a window is full.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetOffer {
    /// What the provider hands out: `credits` for the reset credits of a ChatGPT plan.
    pub kind: String,
    /// How many can be spent now.
    pub available: u32,
    /// Unix seconds at which the first of them lapses, when the provider says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Unix seconds at which the next one arrives, when none is available and the provider
    /// says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_available_at: Option<u64>,
}

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
    /// What that has cost so far, when it is on and the provider says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<Spend>,
    /// The resets the account holds, as far as the usage endpoint itself tells.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resets: Vec<ResetOffer>,
    /// Unix seconds.
    pub fetched_at: u64,
}

/// Whether `provider` has an endpoint that reports a login's usage.
pub fn supported(provider: &ProviderConfig) -> bool {
    provider.oauth.is_some() && matches!(provider.id.as_str(), ANTHROPIC | CHATGPT)
}

/// Asks `provider` what the login stored as `account` has left, and keeps the answer in the
/// limit store. With `renew`, a login that is about to expire is renewed first, as for any
/// request; without it the stored token is used while it lasts and nothing is asked once it
/// has run out, so that looking at an account in the background never renews its login.
/// Errors say what went wrong without the address or the token.
pub async fn fetch(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    credentials: &Credentials,
    account: &str,
    renew: bool,
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
    let key = account_key(id, account);
    let (access, fields) = match renew {
        true => fresh_login(credentials, provider, &key)
            .await
            .map_err(|e| e.to_string())?,
        false => stored_login(credentials, &key).ok_or_else(|| {
            format!("the login of account {account} of {id} is due for renewal; it is renewed when it is next used")
        })?,
    };
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
    credentials
        .limits()
        .record_report(id, account, &report.windows, now);
    Ok(report)
}

/// The reports kept in `file` (`cache/limits.json`), each under the `auth.json` key of its
/// account.
pub fn cached(file: &Path) -> BTreeMap<String, UsageReport> {
    read_file(file)
        .into_iter()
        .filter_map(|(key, value)| {
            let report: UsageReport = serde_json::from_value(value).ok()?;
            // An entry that only holds the blocks of an account has never been fetched.
            (report.fetched_at > 0).then_some((key, report))
        })
        .collect()
}

/// Keeps `report` in `file` as the last answer about its account. The blocks the file holds
/// for the account stay.
pub fn keep(file: &Path, report: &UsageReport) {
    let Ok(mut value) = serde_json::to_value(report) else {
        return;
    };
    update_file(file, |all| {
        let key = account_key(&report.provider, &report.account);
        if let Some(blocks) = all.get(&key).and_then(|stored| stored.get("blocks")) {
            value["blocks"] = blocks.clone();
        }
        all.insert(key, value);
    });
}

fn blank() -> UsageReport {
    UsageReport {
        provider: String::new(),
        account: String::new(),
        plan: None,
        windows: Vec::new(),
        extra_usage: None,
        spend: None,
        resets: Vec::new(),
        fetched_at: 0,
    }
}

/// `allowed` below a full window, `rejected` at it.
fn status(used: f32) -> String {
    if used >= 1.0 { REJECTED } else { "allowed" }.to_string()
}

/// The words of `text` in lower case with dashes between them: what a window name ends in.
fn dashed(text: &str) -> String {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("-")
}

/// Whether extra usage is on and what it has cost: `spend: { enabled, used, limit }` with
/// amounts as `{ amount_minor, exponent, currency }`, or the older
/// `extra_usage: { is_enabled, used_credits, monthly_limit }` in hundredths.
fn extra_usage(body: &Value) -> (Option<bool>, Option<Spend>) {
    let amount = |minor: &Value, exponent: u64| {
        let minor = minor.as_f64().filter(|minor| *minor >= 0.0)?;
        (exponent <= 12).then(|| minor / 10f64.powi(exponent as i32))
    };
    let spend = &body["spend"];
    let (enabled, used, limit, currency) = if spend.is_object() {
        let money = |value: &Value| amount(&value["amount_minor"], value["exponent"].as_u64()?);
        (
            spend["enabled"].as_bool(),
            money(&spend["used"]),
            money(&spend["limit"]),
            spend["used"]["currency"].as_str(),
        )
    } else {
        let legacy = &body["extra_usage"];
        let places = legacy["decimal_places"].as_u64().unwrap_or(2);
        (
            legacy["is_enabled"].as_bool(),
            amount(&legacy["used_credits"], places),
            amount(&legacy["monthly_limit"], places),
            legacy["currency"].as_str(),
        )
    };
    let spend = used.filter(|_| enabled == Some(true)).map(|used| Spend {
        used,
        limit: limit.filter(|limit| *limit > 0.0),
        currency: currency.unwrap_or("USD").to_ascii_uppercase(),
    });
    (enabled, spend)
}

/// The answer of `GET /api/oauth/usage`. Every window it holds becomes one:
///
/// - `five_hour` and `seven_day`, each `{ utilization: 0..100, resets_at: RFC 3339 }` or null;
/// - `limits: [{ kind, percent, resets_at, scope: { model: { display_name } } }]`, where
///   `weekly_scoped` is the weekly allowance of one model family (`Fable`), and `session` and
///   `weekly_all` stand in for the two above when those are missing. Which of them is marked
///   active says only which one binds at the moment, so it is not read;
/// - `seven_day_opus`, `seven_day_sonnet` and any other member of that shape, as a window of
///   what its name says it counts.
///
/// A window that counts one model family, or anything else that is not every request, carries
/// that as its scope. Extra usage is read from `spend` or `extra_usage`; while it is on and
/// has money left, a full window does not stop requests and is not marked as refusing them.
pub fn parse_anthropic(body: &Value) -> UsageReport {
    let mut report = blank();
    let (enabled, spend) = extra_usage(body);
    let paid_beyond = enabled == Some(true)
        && !spend
            .as_ref()
            .is_some_and(|spend| spend.limit.is_some_and(|limit| spend.used >= limit));
    let mut windows: Vec<Window> = Vec::new();
    let mut push = |name: String, scope: Option<String>, percent: f64, resets_at: Option<u64>| {
        if name.is_empty() || windows.iter().any(|known| known.name == name) {
            return;
        }
        let used = (percent / 100.0) as f32;
        windows.push(Window {
            name,
            used,
            resets_at,
            status: match paid_beyond {
                true => "allowed".to_string(),
                false => status(used),
            },
            scope,
        });
    };
    let bucket = |value: &Value| {
        let percent = value["utilization"].as_f64()?;
        Some((percent, value["resets_at"].as_str().and_then(unix_seconds)))
    };
    const KNOWN: [(&str, &str, Option<&str>); 4] = [
        ("five_hour", "5h", None),
        ("seven_day", "7d", None),
        ("seven_day_opus", "7d-opus", Some("Opus")),
        ("seven_day_sonnet", "7d-sonnet", Some("Sonnet")),
    ];
    for (field, name, scope) in KNOWN {
        if let Some((percent, resets_at)) = bucket(&body[field]) {
            push(name.to_string(), scope.map(str::to_string), percent, resets_at);
        }
    }
    for entry in body["limits"].as_array().into_iter().flatten() {
        let (Some(kind), Some(percent)) = (entry["kind"].as_str(), entry["percent"].as_f64()) else {
            continue;
        };
        let resets_at = entry["resets_at"].as_str().and_then(unix_seconds);
        let model = entry["scope"]["model"]["display_name"]
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let (name, scope) = match (kind, model) {
            ("session", _) => ("5h".to_string(), None),
            ("weekly_all", _) => ("7d".to_string(), None),
            ("weekly_scoped", Some(model)) => (format!("7d-{}", dashed(model)), Some(model.to_string())),
            // A weekly allowance of one model that does not say which.
            ("weekly_scoped", None) => continue,
            (kind, Some(model)) => (
                format!("{}-{}", dashed(kind), dashed(model)),
                Some(model.to_string()),
            ),
            (kind, None) => (dashed(kind), Some(kind.replace('_', " "))),
        };
        push(name, scope, percent, resets_at);
    }
    for (field, value) in body.as_object().into_iter().flatten() {
        if KNOWN.iter().any(|(known, ..)| known == field) || field == "extra_usage" {
            continue;
        }
        let Some((percent, resets_at)) = bucket(value) else {
            continue;
        };
        let (length, rest) = match field {
            field if field.starts_with("seven_day_") => ("7d-", &field["seven_day_".len()..]),
            field if field.starts_with("five_hour_") => ("5h-", &field["five_hour_".len()..]),
            field => ("", field.as_str()),
        };
        push(
            format!("{length}{}", dashed(rest)),
            Some(rest.replace('_', " ")),
            percent,
            resets_at,
        );
    }
    // The two windows of the whole account lead, in whatever order the answer had them.
    windows.sort_by_key(|window| match window.name.as_str() {
        "5h" => 0,
        "7d" => 1,
        _ => 2,
    });
    report.windows = windows;
    report.extra_usage = enabled;
    report.spend = spend;
    report
}

/// The windows of one `rate_limit` object of the ChatGPT backend: `primary_window` and
/// `secondary_window`, each `{ used_percent, limit_window_seconds, reset_at | reset_after_seconds }`.
/// They are refused for when the object says `limit_reached` or `allowed: false`.
fn chatgpt_windows(limit: &Value, scope: Option<&str>, reached: bool, now: u64, windows: &mut Vec<Window>) {
    let reached = reached
        || limit["limit_reached"].as_bool() == Some(true)
        || limit["allowed"].as_bool() == Some(false);
    for field in ["primary_window", "secondary_window"] {
        let window = &limit[field];
        let Some(percent) = window["used_percent"].as_f64() else {
            continue;
        };
        let seconds = window["limit_window_seconds"]
            .as_u64()
            .or_else(|| window["window_seconds"].as_u64());
        let length = window_name(seconds, field);
        let name = match scope {
            Some(scope) => format!("{length}-{}", dashed(scope)),
            None => length,
        };
        if windows.iter().any(|known| known.name == name) {
            continue;
        }
        let used = (percent / 100.0) as f32;
        windows.push(Window {
            name,
            used,
            resets_at: window["reset_at"]
                .as_u64()
                .or_else(|| Some(now + window["reset_after_seconds"].as_u64()?)),
            status: if reached {
                status(used)
            } else {
                "allowed".to_string()
            },
            scope: scope.map(str::to_string),
        });
    }
}

/// The answer of `GET /wham/usage`: `plan_type`; `rate_limit` with the windows of the plan;
/// `additional_rate_limits: [{ limit_name, rate_limit }]` with the windows of single models,
/// which carry the limit's name as their scope; `credits: { has_credits, unlimited }`; and
/// `rate_limit_reset_credits: { available_count }`, the resets the account holds.
pub fn parse_chatgpt(body: &Value, now: u64) -> UsageReport {
    let mut report = blank();
    report.plan = body["plan_type"].as_str().map(str::to_string);
    let reached =
        body["limit_reached"].as_bool() == Some(true) || body["limitReached"].as_bool() == Some(true);
    chatgpt_windows(&body["rate_limit"], None, reached, now, &mut report.windows);
    for extra in body["additional_rate_limits"].as_array().into_iter().flatten() {
        let name = extra["limit_name"].as_str().map(str::trim).unwrap_or_default();
        if !name.is_empty() {
            chatgpt_windows(&extra["rate_limit"], Some(name), false, now, &mut report.windows);
        }
    }
    let credits = &body["credits"];
    if credits.is_object() {
        report.extra_usage = Some(
            credits["has_credits"].as_bool() == Some(true) || credits["unlimited"].as_bool() == Some(true),
        );
    }
    if let Some(available) = body["rate_limit_reset_credits"]["available_count"].as_u64() {
        report.resets.push(ResetOffer {
            kind: "credits".to_string(),
            available: u32::try_from(available).unwrap_or(u32::MAX),
            expires_at: None,
            next_available_at: None,
        });
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
            Window::new("5h", 0.42, Some(1_794_506_400), "allowed")
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
                Window::new("5h", 0.12, Some(1_794_506_400), "allowed"),
                // Full, but the backend still takes requests: not refused.
                Window::new("7d", 1.0, Some(4_600), "allowed"),
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

    #[test]
    fn scoped_weekly_limits_and_every_other_window_of_a_claude_answer_become_windows() {
        let report = parse_anthropic(&json!({
            "five_hour": { "utilization": 12.0, "resets_at": "2026-11-12T18:00:00Z" },
            "seven_day": { "utilization": 77.0, "resets_at": "2026-11-15T00:00:00Z" },
            "seven_day_opus": null,
            "seven_day_sonnet": null,
            "seven_day_oauth_apps": { "utilization": 3.0, "resets_at": null },
            "limits": [
                { "kind": "session", "percent": 12, "resets_at": "2026-11-12T18:00:00Z", "is_active": false },
                { "kind": "weekly_all", "percent": 77, "resets_at": "2026-11-15T00:00:00Z", "is_active": false },
                { "kind": "weekly_scoped", "percent": 100, "resets_at": "2026-11-16T00:00:00Z",
                  "scope": { "model": { "display_name": "Fable" } }, "is_active": true },
                // The same family twice, a scoped row that names no model, and rows that are
                // not limits at all.
                { "kind": "weekly_scoped", "percent": 5, "scope": { "model": { "display_name": "Fable" } } },
                { "kind": "weekly_scoped", "percent": 40, "scope": null },
                { "kind": "monthly_pool", "percent": 8.5 },
                { "kind": 7 },
                "junk"
            ],
            "extra_usage": { "is_enabled": false, "monthly_limit": null, "used_credits": null },
            "juniper_tide": null
        }));
        let names: Vec<&str> = report.windows.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["5h", "7d", "7d-fable", "monthly-pool", "7d-oauth-apps"]);
        // The two windows of the whole account are read once, from their own members.
        assert_eq!(
            report.windows[1],
            Window::new("7d", 0.77, unix_seconds("2026-11-15T00:00:00Z"), "allowed")
        );
        // The weekly allowance of the Fable models is full, whatever is marked active: it is
        // about those models only.
        let fable = &report.windows[2];
        assert_eq!(
            *fable,
            Window::new("7d-fable", 1.0, unix_seconds("2026-11-16T00:00:00Z"), REJECTED).scoped("Fable")
        );
        assert_eq!(fable.label(), "7-day Fable limit");
        assert!(fable.counts("claude-fable-5-2") && !fable.counts("claude-opus-5-5"));
        // Windows that count something else than every request say so, and stop no model.
        assert_eq!(report.windows[3].scope.as_deref(), Some("monthly pool"));
        assert_eq!(report.windows[4].scope.as_deref(), Some("oauth apps"));
        assert!(!report.windows[4].counts("claude-opus-5-5"));
        assert_eq!(report.extra_usage, Some(false));
        assert_eq!(report.spend, None);

        // Without the two members of old, the rows of `limits` stand in for them.
        let rows_only = parse_anthropic(&json!({
            "limits": [
                { "kind": "weekly_scoped", "percent": 30, "scope": { "model": { "display_name": "Claude Mythos" } } },
                { "kind": "weekly_all", "percent": 50 },
                { "kind": "session", "percent": 20 }
            ]
        }));
        let names: Vec<&str> = rows_only.windows.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["5h", "7d", "7d-claude-mythos"]);
        assert_eq!(rows_only.windows[0].used, 0.2);
        assert_eq!(rows_only.extra_usage, None);
    }

    #[test]
    fn extra_usage_says_what_it_cost_and_carries_a_full_window_while_it_lasts() {
        let answer = |used: u64| {
            json!({
                "five_hour": { "utilization": 100.0, "resets_at": "2026-11-12T18:00:00Z" },
                "spend": {
                    "enabled": true,
                    "used": { "amount_minor": used, "exponent": 2, "currency": "usd" },
                    "limit": { "amount_minor": 5000, "exponent": 2, "currency": "usd" }
                },
                // The newer member wins over the older one.
                "extra_usage": { "is_enabled": false }
            })
        };
        let paying = parse_anthropic(&answer(1250));
        assert_eq!(paying.extra_usage, Some(true));
        assert_eq!(
            paying.spend,
            Some(Spend {
                used: 12.5,
                limit: Some(50.0),
                currency: "USD".into()
            })
        );
        // Requests beyond the plan are paid for: the full window refuses nothing.
        assert!(!paying.windows[0].exhausted() && paying.windows[0].full());
        // Once the money is spent as well, it does.
        let spent = parse_anthropic(&answer(5000));
        assert!(spent.windows[0].exhausted());

        // The older form, in hundredths, without a cap.
        let legacy = parse_anthropic(&json!({
            "seven_day": { "utilization": 100.0, "resets_at": null },
            "extra_usage": { "is_enabled": true, "monthly_limit": null, "used_credits": 730 }
        }));
        assert_eq!(
            legacy.spend,
            Some(Spend {
                used: 7.3,
                limit: None,
                currency: "USD".into()
            })
        );
        assert!(!legacy.windows[0].exhausted());
    }

    #[test]
    fn the_windows_of_single_models_and_the_banked_resets_of_a_chatgpt_answer_are_read() {
        let report = parse_chatgpt(
            &json!({
                "plan_type": "pro",
                "rate_limit": {
                    "allowed": true,
                    "primary_window": { "used_percent": 40, "window_seconds": 18000, "reset_at": 2_000 },
                    "secondary_window": { "used_percent": 60, "limit_window_seconds": 604800, "reset_at": 9_000 }
                },
                "additional_rate_limits": [
                    {
                        "limit_name": "GPT-5.3-Codex-Spark",
                        "rate_limit": {
                            "allowed": false,
                            "primary_window": { "used_percent": 100, "limit_window_seconds": 18000, "reset_at": 3_000 },
                            "secondary_window": { "used_percent": 20, "limit_window_seconds": 604800, "reset_at": 9_000 }
                        }
                    },
                    { "limit_name": "", "rate_limit": { "primary_window": { "used_percent": 1 } } },
                    { "limit_name": "Idle", "rate_limit": null }
                ],
                "rate_limit_reset_credits": { "available_count": 2 }
            }),
            1_000,
        );
        let names: Vec<&str> = report.windows.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(
            names,
            ["5h", "7d", "5h-gpt-5-3-codex-spark", "7d-gpt-5-3-codex-spark"]
        );
        assert!(
            report
                .windows
                .iter()
                .take(2)
                .all(|w| w.scope.is_none() && !w.exhausted())
        );
        // The model's own window refuses requests to that model and to no other.
        let spark = &report.windows[2];
        assert_eq!(spark.scope.as_deref(), Some("GPT-5.3-Codex-Spark"));
        assert!(spark.exhausted() && spark.counts("gpt-5.3-codex-spark") && !spark.counts("gpt-5.3-codex"));
        assert!(!report.windows[3].exhausted());
        assert_eq!(
            report.resets,
            [ResetOffer {
                kind: "credits".into(),
                available: 2,
                expires_at: None,
                next_available_at: None
            }]
        );

        // A plan the backend refuses for as a whole, said at the top of the answer.
        let refused = parse_chatgpt(
            &json!({
                "limit_reached": true,
                "rate_limit": { "secondary_window": { "used_percent": 100, "limit_window_seconds": 604800 } }
            }),
            0,
        );
        assert!(refused.windows[0].exhausted());
        assert!(refused.resets.is_empty());
    }

    #[tokio::test]
    async fn asking_in_the_background_uses_the_stored_token_and_never_renews_a_login() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .and(header("authorization", "Bearer access-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "five_hour": { "utilization": 10.0, "resets_at": null }
            })))
            .expect(1)
            .mount(&server)
            .await;
        // The token endpoint: nothing may ask it.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let credentials = Credentials::with_env(dir.path().join("auth.json"), Arc::new(|_: &str| None));
        credentials
            .store("anthropic", login("a@example.com", &[]))
            .unwrap();
        // A login whose token runs out within the minute would be renewed by a request.
        credentials
            .store(
                &account_key("anthropic", "work"),
                Credential::OAuth {
                    access: "stale-token".into(),
                    refresh: "refresh-token".into(),
                    expires: now_ms() + 10_000,
                    extra: Default::default(),
                },
            )
            .unwrap();
        let mut claude = preset("anthropic", server.uri());
        if let Some(oauth) = &mut claude.oauth {
            oauth.token_url = format!("{}/token", server.uri());
        }
        let client = crate::tls::client();

        let report = fetch(&client, &claude, &credentials, "default", false)
            .await
            .unwrap();
        assert_eq!(report.windows[0].name, "5h");
        let skipped = fetch(&client, &claude, &credentials, "work", false)
            .await
            .unwrap_err();
        assert!(skipped.contains("due for renewal"), "{skipped}");
        // The stored login is as it was.
        let stored = credentials.load();
        assert!(
            matches!(&stored["anthropic#work"], Credential::OAuth { access, .. } if access == "stale-token")
        );
    }

    #[test]
    fn kept_answers_and_the_blocks_of_an_account_share_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cache").join("limits.json");
        let later = now_ms() / 1000 + 3600;
        let limits = crate::limits::LimitStore::new();
        limits.persist(file.clone());
        limits.block("anthropic", "work", later);
        limits.block("groq", "default", later);

        let mut report =
            parse_anthropic(&json!({ "five_hour": { "utilization": 100.0, "resets_at": null } }));
        report.provider = "anthropic".into();
        report.account = "work".into();
        report.fetched_at = 1_700_000_000;
        keep(&file, &report);

        // The answer is read back; the account that only has a block has no answer.
        let kept = cached(&file);
        assert_eq!(kept.keys().collect::<Vec<_>>(), ["anthropic#work"]);
        assert_eq!(kept["anthropic#work"], report);
        // Keeping an answer left the blocks where they were, for the next run to find.
        let next = crate::limits::LimitStore::new();
        next.persist(file.clone());
        assert_eq!(next.blocked_until("anthropic", "work"), Some(later));
        assert_eq!(next.blocked_until("groq", "default"), Some(later));
        // A file from before blocks were kept in it, one report per key, still reads.
        std::fs::write(
            &file,
            r#"{"openai-codex":{"provider":"openai-codex","account":"default","plan":"plus","windows":[{"name":"5h","used":0.3,"status":"allowed"}],"fetched_at":1700000000}}"#,
        )
        .unwrap();
        let old = cached(&file);
        assert_eq!(old["openai-codex"].plan.as_deref(), Some("plus"));
        assert_eq!(old["openai-codex"].windows[0].scope, None);
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
                "seven_day_opus": null,
                "limits": [
                    { "kind": "weekly_scoped", "percent": 100, "resets_at": "2099-01-01T00:00:00Z",
                      "scope": { "model": { "display_name": "Fable" } }, "is_active": true }
                ],
                "extra_usage": { "is_enabled": false }
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
        let report = fetch(&client, &claude, &credentials, "default", true)
            .await
            .unwrap();
        assert_eq!(
            (report.provider.as_str(), report.account.as_str()),
            ("anthropic", "default")
        );
        assert_eq!(report.windows.len(), 3);
        assert_eq!(report.extra_usage, Some(false));
        // The store knows which requests the full weekly window of the Fable models stops.
        let limits = credentials.limits();
        assert_eq!(
            limits
                .spent("anthropic", "default", "claude-fable-5-2")
                .map(|window| window.label()),
            Some("7-day Fable limit".to_string())
        );
        assert!(limits.open("anthropic", "default", "claude-opus-5-5"));
        assert!(report.fetched_at > 0);
        // The store has what the endpoint said, under the account that was asked about.
        let stored = credentials.limits().windows("anthropic", "default");
        assert_eq!(stored[0].name, "5h");
        assert!((stored[0].used - 0.81).abs() < 1e-6);
        assert!(credentials.limits().windows("anthropic", "work").is_empty());

        let chatgpt = preset("openai-codex", format!("{}/backend-api/codex", server.uri()));
        let report = fetch(&client, &chatgpt, &credentials, "default", true)
            .await
            .unwrap();
        assert_eq!(report.plan.as_deref(), Some("pro"));
        assert_eq!(report.windows[0].name, "5h");
        assert_eq!(credentials.limits().windows("openai-codex", "default").len(), 1);

        // A stored key is not a subscription, and a refusal names neither address nor body.
        let refused = fetch(&client, &claude, &credentials, "work", true)
            .await
            .unwrap_err();
        assert!(refused.contains("not a login"), "{refused}");
        let gone = preset("anthropic", format!("{}/gone", server.uri()));
        let refused = fetch(&client, &gone, &credentials, "default", true)
            .await
            .unwrap_err();
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
