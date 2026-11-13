//! `/usage` and `azazel usage`: what each subscription has left of its usage windows.
//!
//! The provider is asked (see [`azazel_ai::providers::usage`]) at most every five minutes per
//! account; the answers are kept in `cache/limits.json`. When it cannot be asked, what the
//! headers of this session's responses said about the account is shown instead.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use azazel_ai::credentials::{account_key, now_ms};
use azazel_ai::limits::time_left;
use azazel_ai::providers::http::http_client;
use azazel_ai::providers::usage::{self, UsageReport};
use azazel_ai::{Account, Credentials, ProviderConfig, Window};
use azazel_core::{Command, CommandHandler, CommandOutcome, ExtCtx, ExtError, Extension, Registrar};
use serde_json::json;

use crate::print::{EXIT_FAILED, EXIT_OK};

/// An answer of a provider is good for this long.
const FRESH_SECS: u64 = 5 * 60;
const BAR: usize = 20;
/// The share of a window from which the status line mentions it.
pub const NEARLY_FULL: f32 = 0.8;

/// What is known about one account.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub provider: String,
    pub account: Account,
    /// What the provider answered, now or within the last five minutes.
    pub report: Option<UsageReport>,
    /// What response headers said, when the provider could not be asked.
    pub seen: Vec<Window>,
    /// Why the provider could not be asked.
    pub problem: Option<String>,
}

type Cache = BTreeMap<String, UsageReport>;

fn read_cache(file: &Path) -> Cache {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_cache(file: &Path, cache: &Cache) {
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string(cache) {
        let _ = std::fs::write(file, text);
    }
}

/// The usage of every stored login of the providers that report it. An answer in `cache_file`
/// younger than five minutes is used as it is; the rest are asked for and kept there.
pub async fn collect(
    providers: &[ProviderConfig],
    credentials: &Credentials,
    cache_file: &Path,
) -> Vec<Entry> {
    let client = http_client();
    let mut cache = read_cache(cache_file);
    let now = now_ms() / 1000;
    let mut entries = Vec::new();
    let mut asked = false;
    for provider in providers.iter().filter(|provider| usage::supported(provider)) {
        for account in credentials.accounts(&provider.id) {
            if !account.login {
                continue;
            }
            let key = account_key(&provider.id, &account.label);
            let cached = cache
                .get(&key)
                .filter(|report| now.saturating_sub(report.fetched_at) < FRESH_SECS)
                .cloned();
            let (report, problem) = match cached {
                Some(report) => {
                    credentials
                        .limits()
                        .record(&provider.id, &account.label, report.windows.clone());
                    (Some(report), None)
                }
                None => match usage::fetch(&client, provider, credentials, &account.label).await {
                    Ok(report) => {
                        cache.insert(key, report.clone());
                        asked = true;
                        (Some(report), None)
                    }
                    Err(why) => (None, Some(why)),
                },
            };
            let seen = match report {
                Some(_) => Vec::new(),
                None => credentials.limits().windows(&provider.id, &account.label),
            };
            entries.push(Entry {
                provider: provider.id.clone(),
                account,
                report,
                seen,
                problem,
            });
        }
    }
    if asked {
        write_cache(cache_file, &cache);
    }
    entries
}

fn bar(used: f32) -> String {
    let filled = ((used.clamp(0.0, 1.0) * BAR as f32).round() as usize).min(BAR);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(BAR - filled))
}

fn window_line(window: &Window, now: u64) -> String {
    let percent = (window.used * 100.0).round().clamp(0.0, 999.0) as u32;
    let mut line = format!("  {:<10} {} {percent:>3}%", window.name, bar(window.used));
    if let Some(at) = window.resets_at.filter(|at| *at > now) {
        let _ = write!(line, "  resets in {}", time_left(at - now));
    }
    if window.exhausted() {
        line.push_str("  (no requests until then)");
    }
    line
}

/// The usage of each account: a bar, the share used and the time to the reset per window.
pub fn render(entries: &[Entry], now: u64) -> String {
    if entries.is_empty() {
        return "no subscription is signed in: usage is reported for Claude and ChatGPT logins \
                (azazel login anthropic, azazel login openai-codex)\n"
            .to_string();
    }
    let mut out = String::new();
    for entry in entries {
        let mut heading = format!("{} · {}", entry.provider, entry.account.describe());
        if let Some(plan) = entry.report.as_ref().and_then(|report| report.plan.as_deref()) {
            let _ = write!(heading, " · {plan} plan");
        }
        let _ = writeln!(out, "{heading}");
        let windows = match &entry.report {
            Some(report) => &report.windows,
            None => &entry.seen,
        };
        for window in windows.iter().filter(|window| window.name != "overage") {
            let _ = writeln!(out, "{}", window_line(window, now));
        }
        match entry.report.as_ref().and_then(|report| report.extra_usage) {
            Some(true) => out.push_str("  extra usage: on\n"),
            Some(false) => out.push_str("  extra usage: off\n"),
            None => {}
        }
        if let Some(overage) = entry.seen.iter().find(|window| window.name == "overage") {
            let _ = writeln!(out, "  extra usage: {}", overage.status);
        }
        match (&entry.problem, windows.is_empty()) {
            (Some(why), true) => {
                let _ = writeln!(out, "  {why}");
            }
            (Some(why), false) => {
                let _ = writeln!(out, "  from this session's responses ({why})");
            }
            (None, true) => out.push_str("  the provider reported no usage windows\n"),
            (None, false) => {}
        }
    }
    out
}

fn json_of(entries: &[Entry]) -> serde_json::Value {
    let window = |window: &Window| {
        json!({
            "name": window.name,
            // To a tenth of a percent: the share is kept in single precision.
            "used": (f64::from(window.used) * 1000.0).round() / 1000.0,
            "resets_at": window.resets_at,
            "status": window.status,
        })
    };
    let rows: Vec<serde_json::Value> = entries
        .iter()
        .map(|entry| {
            let windows = match &entry.report {
                Some(report) => &report.windows,
                None => &entry.seen,
            };
            json!({
                "provider": entry.provider,
                "account": entry.account.label,
                "email": entry.account.email,
                "plan": entry.report.as_ref().and_then(|report| report.plan.clone()),
                "windows": windows.iter().map(window).collect::<Vec<_>>(),
                "extra_usage": entry.report.as_ref().and_then(|report| report.extra_usage),
                "fetched_at": entry.report.as_ref().map(|report| report.fetched_at),
                "error": entry.problem,
            })
        })
        .collect();
    json!(rows)
}

/// `azazel usage [--json]`.
pub async fn run(
    providers: &[ProviderConfig],
    credentials: &Credentials,
    cache_file: &Path,
    as_json: bool,
) -> u8 {
    let entries = collect(providers, credentials, cache_file).await;
    if as_json {
        println!("{}", json_of(&entries));
    } else {
        print!("{}", render(&entries, now_ms() / 1000));
    }
    // Nothing to show for any login is a failure; no login at all is not.
    let failed = !entries.is_empty()
        && entries
            .iter()
            .all(|entry| entry.report.is_none() && entry.seen.is_empty());
    if failed { EXIT_FAILED } else { EXIT_OK }
}

/// The fullest window of an account once it has passed [`NEARLY_FULL`], as `5h 84%`, for the
/// status line.
pub fn nearly_full(windows: &[Window]) -> Option<String> {
    windows
        .iter()
        .filter(|window| window.name != "overage" && window.used >= NEARLY_FULL)
        .max_by(|a, b| a.used.total_cmp(&b.used))
        .map(|window| {
            format!(
                "{} {}%",
                window.name,
                (window.used * 100.0).round().clamp(0.0, 999.0) as u32
            )
        })
}

struct UsageCommand {
    providers: Vec<ProviderConfig>,
    credentials: Arc<Credentials>,
    cache_file: PathBuf,
}

#[async_trait]
impl CommandHandler for UsageCommand {
    async fn run(&self, _args: &str, _ctx: &ExtCtx) -> CommandOutcome {
        let entries = collect(&self.providers, &self.credentials, &self.cache_file).await;
        CommandOutcome::Show(render(&entries, now_ms() / 1000).trim_end().to_string())
    }
}

struct StatsCommand {
    paths: azazel_core::Paths,
}

#[async_trait]
impl CommandHandler for StatsCommand {
    async fn run(&self, args: &str, _ctx: &ExtCtx) -> CommandOutcome {
        let days = match args.trim() {
            "" => 30,
            text => match text.parse::<u32>() {
                Ok(days) if days > 0 => days,
                _ => return CommandOutcome::Error("usage: /stats [days]".to_string()),
            },
        };
        let paths = self.paths.clone();
        // Reading every project's session index is file work.
        match tokio::task::spawn_blocking(move || crate::stats::text(&paths, days)).await {
            Ok(text) => CommandOutcome::Show(text.trim_end().to_string()),
            Err(e) => CommandOutcome::Error(format!("could not read the sessions: {e}")),
        }
    }
}

/// Registers `/usage` and `/stats`.
pub struct UsageExt {
    /// The providers that take a key or have a login.
    pub providers: Vec<ProviderConfig>,
    pub credentials: Arc<Credentials>,
    pub paths: azazel_core::Paths,
}

/// Where the answers of the providers are kept.
pub fn cache_file(paths: &azazel_core::Paths) -> PathBuf {
    paths.cache_dir().join("limits.json")
}

impl Extension for UsageExt {
    fn id(&self) -> &str {
        "usage"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.command(Command {
            name: "usage".to_string(),
            description: "What your Claude and ChatGPT subscriptions have left, and when each window resets"
                .to_string(),
            argument_hint: None,
            handler: Arc::new(UsageCommand {
                providers: self.providers.clone(),
                credentials: self.credentials.clone(),
                cache_file: cache_file(&self.paths),
            }),
        });
        r.command(Command {
            name: "stats".to_string(),
            description: "Sessions, prompts, tokens and cost per day, model, project and tool".to_string(),
            argument_hint: Some("[days]".to_string()),
            handler: Arc::new(StatsCommand {
                paths: self.paths.clone(),
            }),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use azazel_ai::Credential;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn window(name: &str, used: f32, resets_in: Option<u64>, status: &str) -> Window {
        Window {
            name: name.to_string(),
            used,
            resets_at: resets_in.map(|secs| 1_000_000 + secs),
            status: status.to_string(),
        }
    }

    fn account(label: &str, email: Option<&str>) -> Account {
        Account {
            label: label.to_string(),
            login: true,
            email: email.map(str::to_string),
            org: None,
        }
    }

    #[test]
    fn each_window_is_a_bar_a_share_and_a_time_to_its_reset() {
        let entries = [
            Entry {
                provider: "anthropic".into(),
                account: account("default", Some("a@example.com")),
                report: Some(UsageReport {
                    provider: "anthropic".into(),
                    account: "default".into(),
                    plan: None,
                    windows: vec![
                        window("5h", 0.42, Some(2 * 3600 + 600), "allowed"),
                        window("7d", 1.0, Some(3 * 86_400), "rejected"),
                    ],
                    extra_usage: Some(false),
                    fetched_at: 1_000_000,
                }),
                seen: Vec::new(),
                problem: None,
            },
            Entry {
                provider: "openai-codex".into(),
                account: account("work", None),
                report: None,
                seen: vec![window("5h", 0.05, None, "allowed")],
                problem: Some("openai-codex answered HTTP 500 when asked for its usage".into()),
            },
        ];
        assert_eq!(
            render(&entries, 1_000_000),
            "anthropic · default (a@example.com)\n\
             \x20 5h         [████████░░░░░░░░░░░░]  42%  resets in 2 h 10 min\n\
             \x20 7d         [████████████████████] 100%  resets in 3 d  (no requests until then)\n\
             \x20 extra usage: off\n\
             openai-codex · work\n\
             \x20 5h         [█░░░░░░░░░░░░░░░░░░░]   5%\n\
             \x20 from this session's responses (openai-codex answered HTTP 500 when asked for its usage)\n"
        );
        assert!(render(&[], 0).starts_with("no subscription is signed in"));
        let listed = json_of(&entries);
        assert_eq!(listed[0]["windows"][1]["name"], "7d");
        assert_eq!(listed[1]["error"], entries[1].problem.clone().unwrap());
        assert_eq!(listed[1]["windows"][0]["used"], 0.05);
        assert_eq!(listed[0]["windows"][0]["used"], 0.42);
    }

    #[test]
    fn the_status_line_names_a_window_only_once_it_is_nearly_full() {
        assert_eq!(nearly_full(&[window("5h", 0.79, None, "allowed")]), None);
        assert_eq!(
            nearly_full(&[
                window("5h", 0.84, None, "allowed"),
                window("7d", 0.91, None, "allowed"),
                window("overage", 1.0, None, "allowed"),
            ])
            .as_deref(),
            Some("7d 91%")
        );
    }

    #[tokio::test]
    async fn an_answer_is_asked_for_once_and_then_read_from_the_cache() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/backend-api/wham/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "plan_type": "plus",
                "rate_limit": {
                    "primary_window": { "used_percent": 30, "limit_window_seconds": 18000, "reset_after_seconds": 900 },
                    "secondary_window": { "used_percent": 9, "limit_window_seconds": 604800, "reset_after_seconds": 86400 }
                }
            })))
            .expect(2)
            .mount(&server)
            .await;
        // The Claude login's endpoint is down: its account is still listed, with the reason.
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let credentials = Credentials::with_env(dir.path().join("auth.json"), Arc::new(|_: &str| None));
        let login = |email: &str| Credential::OAuth {
            access: "access".into(),
            refresh: "refresh".into(),
            expires: now_ms() + 3_600_000,
            extra: BTreeMap::from([("email".to_string(), json!(email))]),
        };
        credentials.store("openai-codex", login("c@example.com")).unwrap();
        credentials.store("anthropic", login("a@example.com")).unwrap();
        credentials
            .store("anthropic#key", Credential::ApiKey { key: "k".into() })
            .unwrap();
        let mut providers = crate::login::key_providers(&azazel_core::Config::default()).unwrap();
        for provider in &mut providers {
            match provider.id.as_str() {
                "openai-codex" => provider.base_url = format!("{}/backend-api/codex", server.uri()),
                "anthropic" => provider.base_url = server.uri(),
                _ => {}
            }
        }
        let cache = dir.path().join("cache").join("limits.json");

        let first = collect(&providers, &credentials, &cache).await;
        // Logins only, in provider order; the stored key is not a subscription.
        let listed: Vec<(&str, &str)> = first
            .iter()
            .map(|entry| (entry.provider.as_str(), entry.account.label.as_str()))
            .collect();
        assert_eq!(listed, [("anthropic", "default"), ("openai-codex", "default")]);
        assert!(first[0].report.is_none());
        assert!(first[0].problem.as_deref().unwrap().contains("HTTP 500"));
        let report = first[1].report.clone().unwrap();
        assert_eq!(report.plan.as_deref(), Some("plus"));
        assert_eq!(report.windows.len(), 2);

        // A second look within five minutes does not ask ChatGPT again and shows the same.
        let again = collect(&providers, &credentials, &cache).await;
        assert_eq!(again[1].report, Some(report));
        let shown = render(&again, now_ms() / 1000);
        assert!(
            shown.contains("openai-codex · default (c@example.com) · plus plan"),
            "{shown}"
        );
        assert!(shown.contains("30%  resets in 1"), "{shown}");
        assert!(shown.contains("anthropic answered HTTP 500"), "{shown}");

        // An answer older than five minutes is asked for again: the second request the mock
        // expects.
        let mut aged = read_cache(&cache);
        aged.get_mut("openai-codex").unwrap().fetched_at -= FRESH_SECS + 1;
        write_cache(&cache, &aged);
        let asked = collect(&providers, &credentials, &cache).await;
        assert!(asked[1].report.as_ref().unwrap().fetched_at > aged["openai-codex"].fetched_at);
    }
}
