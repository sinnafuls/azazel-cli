//! What each provider account has left of its usage windows, as the provider last reported it,
//! and which accounts are out of allowance until when.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::credentials::now_ms;

/// The status a provider gives a window that takes no more requests.
pub const REJECTED: &str = "rejected";

/// One usage window of an account (a subscription's 5-hour or weekly allowance, for example).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Short name: `5h`, `7d`, `7d-opus`, `overage`.
    pub name: String,
    /// Share of the window that is used, 0.0 to 1.0.
    pub used: f32,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    /// `allowed`, `allowed_warning`, `rejected`, or whatever else the provider said.
    pub status: String,
}

impl Window {
    /// The window as a person calls it: `5-hour limit`, `7-day limit`.
    pub fn label(&self) -> String {
        match self.name.as_str() {
            "5h" => "5-hour limit".to_string(),
            "7d" => "7-day limit".to_string(),
            "7d-opus" => "7-day Opus limit".to_string(),
            "7d-sonnet" => "7-day Sonnet limit".to_string(),
            "overage" => "extra usage".to_string(),
            other => format!("{other} limit"),
        }
    }

    /// Whether the provider refuses requests because of this window.
    pub fn exhausted(&self) -> bool {
        self.status.starts_with(REJECTED)
    }
}

/// The limits of one account, as [`LimitStore::snapshot`] hands them out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountLimits {
    pub provider: String,
    pub account: String,
    pub windows: Vec<Window>,
    /// Unix seconds until which the account is out of allowance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_until: Option<u64>,
}

#[derive(Default)]
struct Entry {
    windows: Vec<Window>,
    blocked_until: Option<u64>,
}

/// Limits per `(provider, account)`. Filled from response headers; read by the router, which
/// moves work off an account that is out of allowance, and by the usage views.
#[derive(Default)]
pub struct LimitStore {
    entries: Mutex<HashMap<(String, String), Entry>>,
}

fn now_secs() -> u64 {
    now_ms() / 1000
}

impl LimitStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<(String, String), Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps `windows` as the latest word on each of them; windows the response did not mention
    /// stay as they were.
    pub fn record(&self, provider: &str, account: &str, windows: Vec<Window>) {
        if windows.is_empty() {
            return;
        }
        let mut entries = self.entries();
        let entry = entries
            .entry((provider.to_string(), account.to_string()))
            .or_default();
        for window in windows {
            match entry.windows.iter_mut().find(|known| known.name == window.name) {
                Some(known) => *known = window,
                None => entry.windows.push(window),
            }
        }
    }

    pub fn windows(&self, provider: &str, account: &str) -> Vec<Window> {
        self.entries()
            .get(&(provider.to_string(), account.to_string()))
            .map(|entry| entry.windows.clone())
            .unwrap_or_default()
    }

    /// The window the provider refuses requests for, when it named one.
    pub fn exhausted(&self, provider: &str, account: &str) -> Option<Window> {
        self.windows(provider, account)
            .into_iter()
            .find(Window::exhausted)
    }

    /// Marks the account as out of allowance until `until` (Unix seconds).
    pub fn block(&self, provider: &str, account: &str, until: u64) {
        self.entries()
            .entry((provider.to_string(), account.to_string()))
            .or_default()
            .blocked_until = Some(until);
    }

    /// The account answered again.
    pub fn unblock(&self, provider: &str, account: &str) {
        if let Some(entry) = self
            .entries()
            .get_mut(&(provider.to_string(), account.to_string()))
        {
            entry.blocked_until = None;
        }
    }

    /// Until when the account is out of allowance; `None` once that time has passed.
    pub fn blocked_until(&self, provider: &str, account: &str) -> Option<u64> {
        self.entries()
            .get(&(provider.to_string(), account.to_string()))
            .and_then(|entry| entry.blocked_until)
            .filter(|until| *until > now_secs())
    }

    /// Every account something is known about, sorted by provider and account.
    pub fn snapshot(&self) -> Vec<AccountLimits> {
        let now = now_secs();
        let mut all: Vec<AccountLimits> = self
            .entries()
            .iter()
            .map(|((provider, account), entry)| AccountLimits {
                provider: provider.clone(),
                account: account.clone(),
                windows: entry.windows.clone(),
                blocked_until: entry.blocked_until.filter(|until| *until > now),
            })
            .collect();
        all.sort_by(|a, b| (&a.provider, &a.account).cmp(&(&b.provider, &b.account)));
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(name: &str, used: f32, status: &str) -> Window {
        Window {
            name: name.to_string(),
            used,
            resets_at: Some(4_000_000_000),
            status: status.to_string(),
        }
    }

    #[test]
    fn windows_are_kept_per_account_and_replaced_by_name() {
        let store = LimitStore::new();
        store.record(
            "anthropic",
            "default",
            vec![window("5h", 0.2, "allowed"), window("7d", 0.5, "allowed")],
        );
        store.record("anthropic", "work", vec![window("5h", 0.9, "allowed_warning")]);
        store.record("anthropic", "default", vec![window("5h", 1.0, REJECTED)]);
        store.record("anthropic", "default", Vec::new());

        let default = store.windows("anthropic", "default");
        assert_eq!(default.len(), 2);
        assert_eq!(default[0], window("5h", 1.0, REJECTED));
        assert_eq!(default[1].used, 0.5);
        assert_eq!(store.exhausted("anthropic", "default").unwrap().label(), "5-hour limit");
        assert_eq!(store.exhausted("anthropic", "work"), None);
        assert!(store.windows("openai", "default").is_empty());

        let all = store.snapshot();
        assert_eq!(
            all.iter().map(|a| a.account.as_str()).collect::<Vec<_>>(),
            ["default", "work"]
        );
    }

    #[test]
    fn a_block_ends_when_its_time_has_passed_or_the_account_answers() {
        let store = LimitStore::new();
        assert_eq!(store.blocked_until("anthropic", "default"), None);
        store.block("anthropic", "default", now_secs() + 600);
        assert!(store.blocked_until("anthropic", "default").is_some());
        assert_eq!(store.blocked_until("anthropic", "work"), None);
        store.unblock("anthropic", "default");
        assert_eq!(store.blocked_until("anthropic", "default"), None);

        store.block("anthropic", "default", now_secs().saturating_sub(1));
        assert_eq!(store.blocked_until("anthropic", "default"), None);
        assert_eq!(store.snapshot()[0].blocked_until, None);
    }
}
