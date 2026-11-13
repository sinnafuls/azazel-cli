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

    /// Whether nothing is left of the window: the provider refuses requests for it, or it is
    /// used to the brim. Extra usage is not an allowance that fills.
    pub fn full(&self) -> bool {
        self.name != "overage" && (self.exhausted() || self.used >= 0.995)
    }

    /// `the 5-hour limit of this account is full; it resets in 2 h 10 min`.
    pub fn full_line(&self, now: u64) -> String {
        match self.resets_at.filter(|at| *at > now) {
            Some(at) => format!(
                "the {} of this account is full; it resets in {}",
                self.label(),
                time_left(at - now)
            ),
            None => format!("the {} of this account is full", self.label()),
        }
    }
}

/// A wait as a person says it: `under a minute`, `45 min`, `2 h 10 min`, `3 d 4 h`.
pub fn time_left(seconds: u64) -> String {
    let minutes = seconds / 60;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => "under a minute".to_string(),
        (0, 0, m) => format!("{m} min"),
        (0, h, 0) => format!("{h} h"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, 0, _) => format!("{d} d"),
        (d, h, _) => format!("{d} d {h} h"),
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
    /// Provider id, then account label.
    entries: Mutex<HashMap<String, HashMap<String, Entry>>>,
}

type Entries = HashMap<String, HashMap<String, Entry>>;

fn now_secs() -> u64 {
    now_ms() / 1000
}

/// The entry of an account, made when there is none yet.
fn entry_of<'a>(entries: &'a mut Entries, provider: &str, account: &str) -> &'a mut Entry {
    entries
        .entry(provider.to_string())
        .or_default()
        .entry(account.to_string())
        .or_default()
}

impl LimitStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn entries(&self) -> MutexGuard<'_, Entries> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps `windows` as the latest word on each of them; windows the response did not mention
    /// stay as they were.
    pub fn record(&self, provider: &str, account: &str, windows: Vec<Window>) {
        if windows.is_empty() {
            return;
        }
        let mut entries = self.entries();
        let entry = entry_of(&mut entries, provider, account);
        for window in windows {
            match entry.windows.iter_mut().find(|known| known.name == window.name) {
                Some(known) => *known = window,
                None => entry.windows.push(window),
            }
        }
    }

    pub fn windows(&self, provider: &str, account: &str) -> Vec<Window> {
        self.entries()
            .get(provider)
            .and_then(|accounts| accounts.get(account))
            .map(|entry| entry.windows.clone())
            .unwrap_or_default()
    }

    /// The window the provider refuses requests for, when it named one.
    pub fn exhausted(&self, provider: &str, account: &str) -> Option<Window> {
        self.windows(provider, account)
            .into_iter()
            .find(Window::exhausted)
    }

    /// The window of the account that has nothing left, when one is known.
    pub fn full(&self, provider: &str, account: &str) -> Option<Window> {
        let windows = self.windows(provider, account);
        windows
            .iter()
            .find(|window| window.full() && window.exhausted())
            .or_else(|| windows.iter().find(|window| window.full()))
            .cloned()
    }

    /// Marks the account as out of allowance until `until` (Unix seconds).
    pub fn block(&self, provider: &str, account: &str, until: u64) {
        entry_of(&mut self.entries(), provider, account).blocked_until = Some(until);
    }

    /// The account answered again.
    pub fn unblock(&self, provider: &str, account: &str) {
        if let Some(entry) = self
            .entries()
            .get_mut(provider)
            .and_then(|accounts| accounts.get_mut(account))
        {
            entry.blocked_until = None;
        }
    }

    /// Until when the account is out of allowance; `None` once that time has passed.
    pub fn blocked_until(&self, provider: &str, account: &str) -> Option<u64> {
        self.entries()
            .get(provider)
            .and_then(|accounts| accounts.get(account))
            .and_then(|entry| entry.blocked_until)
            .filter(|until| *until > now_secs())
    }

    /// Every account something is known about, sorted by provider and account.
    pub fn snapshot(&self) -> Vec<AccountLimits> {
        let now = now_secs();
        let entries = self.entries();
        let mut all: Vec<AccountLimits> = entries
            .iter()
            .flat_map(|(provider, accounts)| {
                accounts.iter().map(move |(account, entry)| AccountLimits {
                    provider: provider.clone(),
                    account: account.clone(),
                    windows: entry.windows.clone(),
                    blocked_until: entry.blocked_until.filter(|until| *until > now),
                })
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
        assert_eq!(
            store.exhausted("anthropic", "default").unwrap().label(),
            "5-hour limit"
        );
        assert_eq!(store.exhausted("anthropic", "work"), None);
        assert!(store.windows("openai", "default").is_empty());

        let all = store.snapshot();
        assert_eq!(
            all.iter().map(|a| a.account.as_str()).collect::<Vec<_>>(),
            ["default", "work"]
        );
    }

    #[test]
    fn a_full_window_says_which_it_is_and_when_it_resets() {
        assert_eq!(time_left(0), "under a minute");
        assert_eq!(time_left(59), "under a minute");
        assert_eq!(time_left(45 * 60), "45 min");
        assert_eq!(time_left(2 * 3600), "2 h");
        assert_eq!(time_left(2 * 3600 + 10 * 60 + 59), "2 h 10 min");
        assert_eq!(time_left(3 * 86_400 + 4 * 3600 + 30 * 60), "3 d 4 h");
        assert_eq!(time_left(7 * 86_400), "7 d");

        let store = LimitStore::new();
        assert_eq!(store.full("anthropic", "default"), None);
        store.record(
            "anthropic",
            "default",
            vec![
                window("5h", 0.4, "allowed"),
                window("7d", 0.999, "allowed"),
                window("overage", 1.0, "rejected (out_of_credits)"),
            ],
        );
        // Used to the brim counts; extra usage is not a window that fills.
        let full = store.full("anthropic", "default").unwrap();
        assert_eq!(full.name, "7d");
        assert_eq!(
            full.full_line(4_000_000_000 - 7_800),
            "the 7-day limit of this account is full; it resets in 2 h 10 min"
        );
        assert_eq!(
            full.full_line(4_000_000_001),
            "the 7-day limit of this account is full"
        );
        // The window the provider refuses for comes before one that is merely full.
        store.record("anthropic", "default", vec![window("5h", 1.0, REJECTED)]);
        assert_eq!(store.full("anthropic", "default").unwrap().name, "5h");
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
