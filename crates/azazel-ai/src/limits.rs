//! What each provider account has left of its usage windows, as the provider last reported it,
//! and which accounts are out of allowance until when.
//!
//! Windows come from response headers and from the usage endpoints of the subscriptions
//! ([`crate::providers::usage`]). A block is this program's own note that an account takes no
//! more requests until a time: it is set when the provider refuses for lack of allowance, kept
//! in `cache/limits.json` so that the next run does not start on the same account, and taken
//! away when the account answers again or a later usage report shows room.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::credentials::{account_key, now_ms};

/// The status a provider gives a window that takes no more requests.
pub const REJECTED: &str = "rejected";
/// The window of extra usage: not an allowance that fills.
const OVERAGE: &str = "overage";
/// A usage report heals a block only when it was fetched this long after the block was set:
/// the usage endpoints lag behind the refusal they explain.
const HEAL_AFTER: u64 = 60;

/// One usage window of an account (a subscription's 5-hour or weekly allowance, for example).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Short name: `5h`, `7d`, `7d-fable`, `overage`.
    pub name: String,
    /// Share of the window that is used, 0.0 to 1.0.
    pub used: f32,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    /// `allowed`, `allowed_warning`, `rejected`, or whatever else the provider said.
    pub status: String,
    /// What the window counts when that is not every request of the account, as the provider
    /// names it: a model family such as `Fable`. Such a window says nothing about requests to
    /// other models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Letters and digits of `text`, lower case: what a scope and a model id are compared by.
fn slug(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Whether a window or a block with `scope` is about requests to `model`: the scope names the
/// model or its family (`Fable` and `claude-fable-5`, `GPT-5.3-Codex-Spark` and
/// `gpt-5.3-codex-spark`).
pub fn scope_covers(scope: &str, model: &str) -> bool {
    let scope = slug(scope);
    !scope.is_empty() && slug(model).contains(&scope)
}

/// The length a window name starts with, in words: `5-hour` for `5h`, `7-day` for `7d`.
fn span(name: &str) -> String {
    let digits = name.chars().take_while(char::is_ascii_digit).count();
    match (&name[..digits], &name[digits..]) {
        ("", _) => name.to_string(),
        (count, "h") => format!("{count}-hour"),
        (count, "d") => format!("{count}-day"),
        (count, "min") => format!("{count}-minute"),
        _ => name.to_string(),
    }
}

impl Window {
    /// A window that counts every request of the account.
    pub fn new(
        name: impl Into<String>,
        used: f32,
        resets_at: Option<u64>,
        status: impl Into<String>,
    ) -> Self {
        Window {
            name: name.into(),
            used,
            resets_at,
            status: status.into(),
            scope: None,
        }
    }

    /// The window counts only what `scope` names.
    pub fn scoped(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// The window as a person calls it: `5-hour limit`, `7-day limit`, `7-day Fable limit`.
    pub fn label(&self) -> String {
        limit_label(&self.name, self.scope.as_deref())
    }

    /// Whether the provider refuses requests because of this window.
    pub fn exhausted(&self) -> bool {
        self.status.starts_with(REJECTED)
    }

    /// Whether nothing is left of the window: the provider refuses requests for it, or it is
    /// used to the brim. Extra usage is not an allowance that fills.
    pub fn full(&self) -> bool {
        self.name != OVERAGE && (self.exhausted() || self.used >= 0.995)
    }

    /// Whether the window counts requests to `model`.
    pub fn counts(&self, model: &str) -> bool {
        self.scope
            .as_deref()
            .is_none_or(|scope| scope_covers(scope, model))
    }

    /// Whether the provider refuses requests for this window at `now`: it said so, and the
    /// window has not reset since.
    fn spent(&self, now: u64) -> bool {
        self.name != OVERAGE && self.exhausted() && self.resets_at.is_none_or(|at| at > now)
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

/// A window named `name` with `scope`, as a person calls it.
fn limit_label(name: &str, scope: Option<&str>) -> String {
    if name == OVERAGE {
        return "extra usage".to_string();
    }
    let (length, rest) = name.split_once('-').unwrap_or((name, ""));
    let length = span(length);
    match (scope, rest) {
        (Some(scope), _) => format!("{length} {scope} limit"),
        (None, "") => format!("{length} limit"),
        (None, rest) => {
            let mut letters = rest.chars();
            let first = letters.next().map(|c| c.to_ascii_uppercase());
            format!(
                "{length} {}{} limit",
                first.map(String::from).unwrap_or_default(),
                letters.as_str()
            )
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

/// An account that takes no requests until a time, for every model or for the models of one
/// scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitBlock {
    /// Unix seconds until which the account is left alone.
    pub until: u64,
    /// Unix seconds at which the provider last refused.
    #[serde(default)]
    pub since: u64,
    /// The window that ran out, by name, when the provider named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// The models it holds for, as the scope of that window; `None` is every model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl LimitBlock {
    /// Whether requests to `model` are held back.
    pub fn covers(&self, model: &str) -> bool {
        self.scope
            .as_deref()
            .is_none_or(|scope| scope_covers(scope, model))
    }

    /// What ran out, as a person calls it: `5-hour limit`, or `usage limit` when the provider
    /// did not say.
    pub fn limit(&self) -> String {
        match &self.window {
            Some(name) => limit_label(name, self.scope.as_deref()),
            None => "usage limit".to_string(),
        }
    }
}

/// The limits of one account, as [`LimitStore::snapshot`] hands them out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountLimits {
    pub provider: String,
    pub account: String,
    pub windows: Vec<Window>,
    /// Unix seconds until which the account is out of allowance for every model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_until: Option<u64>,
    /// Every block of the account, those for the models of one scope included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<LimitBlock>,
}

#[derive(Default)]
struct Entry {
    windows: Vec<Window>,
    blocks: Vec<LimitBlock>,
}

impl Entry {
    fn live(&self, now: u64) -> impl Iterator<Item = &LimitBlock> {
        self.blocks.iter().filter(move |block| block.until > now)
    }
}

/// Limits per `(provider, account)`. Filled from response headers and usage reports; read by
/// the router, which moves work off an account that is out of allowance, and by the usage
/// views.
#[derive(Default)]
pub struct LimitStore {
    /// Provider id, then account label.
    entries: Mutex<HashMap<String, HashMap<String, Entry>>>,
    /// Where blocks are kept between runs, once [`LimitStore::persist`] named it.
    file: OnceLock<PathBuf>,
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

/// `cache/limits.json` as it is: an object per account under its `auth.json` key, holding the
/// account's last usage report and, under `blocks`, its blocks. Empty when there is no file or
/// it cannot be read.
pub(crate) fn read_file(file: &Path) -> Map<String, Value> {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Reads the file, lets `change` at it, and puts the result in its place in one step, so that
/// a reader never finds half a file. One change at a time within this process; two processes
/// changing it at the same moment can lose one of the changes, and what is lost is a cached
/// answer.
pub(crate) fn update_file(file: &Path, change: impl FnOnce(&mut Map<String, Value>)) {
    static CHANGING: Mutex<()> = Mutex::new(());
    let _one_at_a_time = CHANGING.lock().unwrap_or_else(PoisonError::into_inner);
    let mut all = read_file(file);
    change(&mut all);
    let Ok(text) = serde_json::to_string(&all) else {
        return;
    };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let partial = file.with_extension(format!("json.{}", std::process::id()));
    if std::fs::write(&partial, text).is_ok() && std::fs::rename(&partial, file).is_err() {
        let _ = std::fs::remove_file(&partial);
    }
}

impl LimitStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn entries(&self) -> MutexGuard<'_, Entries> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps blocks in `file` (`cache/limits.json`) from now on, and takes over the ones a
    /// run before this one left there.
    pub fn persist(&self, file: PathBuf) {
        if self.file.set(file).is_ok() {
            self.reload();
        }
    }

    /// Takes over the blocks other runs wrote to the file since it was last read. Of two
    /// blocks for the same scope the one that lasts longer stays.
    pub fn reload(&self) {
        let Some(file) = self.file.get() else {
            return;
        };
        let now = now_secs();
        let stored = read_file(file);
        let mut entries = self.entries();
        for value in stored.values() {
            let (Some(provider), Some(account)) = (value["provider"].as_str(), value["account"].as_str())
            else {
                continue;
            };
            let blocks: Vec<LimitBlock> = serde_json::from_value(value["blocks"].clone()).unwrap_or_default();
            for block in blocks.into_iter().filter(|block| block.until > now) {
                let known = &mut entry_of(&mut entries, provider, account).blocks;
                match known.iter_mut().find(|known| known.scope == block.scope) {
                    Some(known) if known.until >= block.until => {}
                    Some(known) => *known = block,
                    None => known.push(block),
                }
            }
        }
    }

    /// Writes the blocks of one account to the file, when there is one. The account's usage
    /// report in the file stays as it is.
    fn save(&self, provider: &str, account: &str, blocks: &[LimitBlock]) {
        let Some(file) = self.file.get() else {
            return;
        };
        let now = now_secs();
        let blocks: Vec<&LimitBlock> = blocks.iter().filter(|block| block.until > now).collect();
        update_file(file, |all| {
            let key = account_key(provider, account);
            if blocks.is_empty() {
                let Some(stored) = all.get_mut(&key).and_then(Value::as_object_mut) else {
                    return;
                };
                stored.remove("blocks");
                // An entry that was only ever there for its blocks goes with them.
                if stored.get("fetched_at").and_then(Value::as_u64).unwrap_or(0) == 0 {
                    all.remove(&key);
                }
                return;
            }
            let stored = all.entry(key).or_insert_with(
                || json!({ "provider": provider, "account": account, "windows": [], "fetched_at": 0 }),
            );
            stored["blocks"] = json!(blocks);
        });
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

    /// Takes `windows` as everything the provider's usage endpoint knows about the account at
    /// `fetched_at` (Unix seconds): they replace what was known, except what only response
    /// headers tell (extra usage). A block is taken away when the window it was set for shows
    /// room again.
    pub fn record_report(&self, provider: &str, account: &str, windows: &[Window], fetched_at: u64) {
        if windows.is_empty() {
            return;
        }
        let mut entries = self.entries();
        let entry = entry_of(&mut entries, provider, account);
        entry.windows.retain(|known| known.name == OVERAGE);
        entry.windows.splice(0..0, windows.iter().cloned());
        let before = entry.blocks.len();
        entry.blocks.retain(|block| {
            if fetched_at < block.since + HEAL_AFTER {
                return true;
            }
            // Only the window the provider refused for can say that it has room again: a
            // block for a reason nobody named waits out its time.
            let about = block
                .window
                .as_ref()
                .and_then(|name| windows.iter().find(|window| window.name == *name));
            about.is_none_or(Window::full)
        });
        if entry.blocks.len() != before {
            let blocks = entry.blocks.clone();
            drop(entries);
            self.save(provider, account, &blocks);
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

    /// The window that stands in the way of a request to `model` on the account: one the
    /// provider refuses for, a window of that model before one of the whole account. `None`
    /// when the provider takes requests as far as is known.
    pub fn spent(&self, provider: &str, account: &str, model: &str) -> Option<Window> {
        let now = now_secs();
        let windows = self.windows(provider, account);
        let spent = |scoped: bool| {
            windows
                .iter()
                .find(|window| window.scope.is_some() == scoped && window.counts(model) && window.spent(now))
        };
        spent(true).or_else(|| spent(false)).cloned()
    }

    /// The window that explains a refusal of a request to `model` on the account: the one the
    /// provider refuses for, else one that counts the model and is used to the brim.
    pub fn full_for(&self, provider: &str, account: &str, model: &str) -> Option<Window> {
        let now = now_secs();
        self.spent(provider, account, model).or_else(|| {
            self.windows(provider, account).into_iter().find(|window| {
                window.full() && window.counts(model) && window.resets_at.is_none_or(|at| at > now)
            })
        })
    }

    /// Marks the account as out of allowance until `until` (Unix seconds), for every model.
    pub fn block(&self, provider: &str, account: &str, until: u64) {
        self.block_on(provider, account, until, None);
    }

    /// Marks the account as out of allowance until `until` because `window` ran out: for the
    /// models that window counts, or for every model when it counts them all or is not known.
    pub fn block_on(&self, provider: &str, account: &str, until: u64, window: Option<&Window>) {
        let block = LimitBlock {
            until,
            since: now_secs(),
            window: window.map(|window| window.name.clone()),
            scope: window.and_then(|window| window.scope.clone()),
        };
        let mut entries = self.entries();
        let entry = entry_of(&mut entries, provider, account);
        entry.blocks.retain(|known| known.scope != block.scope);
        entry.blocks.push(block);
        let blocks = entry.blocks.clone();
        drop(entries);
        self.save(provider, account, &blocks);
    }

    /// Takes every block off the account and no longer holds against it the windows the
    /// provider last refused for: the next request asks the provider. For when the user picks
    /// the account knowingly.
    pub fn unblock(&self, provider: &str, account: &str) {
        self.lift(provider, account, |_| true);
        if let Some(entry) = self
            .entries()
            .get_mut(provider)
            .and_then(|accounts| accounts.get_mut(account))
        {
            for window in entry.windows.iter_mut().filter(|window| window.name != OVERAGE) {
                if window.exhausted() {
                    window.status = "allowed".to_string();
                }
            }
        }
    }

    /// The account answered a request to `model`: the blocks that held such requests back are
    /// taken off it.
    pub fn unblock_for(&self, provider: &str, account: &str, model: &str) {
        self.lift(provider, account, |block| block.covers(model));
    }

    fn lift(&self, provider: &str, account: &str, lifted: impl Fn(&LimitBlock) -> bool) {
        let mut entries = self.entries();
        let Some(entry) = entries
            .get_mut(provider)
            .and_then(|accounts| accounts.get_mut(account))
        else {
            return;
        };
        let before = entry.blocks.len();
        entry.blocks.retain(|block| !lifted(block));
        if entry.blocks.len() != before {
            let blocks = entry.blocks.clone();
            drop(entries);
            self.save(provider, account, &blocks);
        }
    }

    /// Until when the account is out of allowance for every model; `None` once that time has
    /// passed.
    pub fn blocked_until(&self, provider: &str, account: &str) -> Option<u64> {
        self.blocks(provider, account)
            .into_iter()
            .filter(|block| block.scope.is_none())
            .map(|block| block.until)
            .max()
    }

    /// The block that holds requests to `model` back on the account: the one that lasts
    /// longest.
    pub fn blocked_for(&self, provider: &str, account: &str, model: &str) -> Option<LimitBlock> {
        self.blocks(provider, account)
            .into_iter()
            .filter(|block| block.covers(model))
            .max_by_key(|block| block.until)
    }

    /// Whether a request to `model` can go to the account as far as is known: it is not
    /// blocked for that model and no window that counts the model is refused for.
    pub fn open(&self, provider: &str, account: &str, model: &str) -> bool {
        self.blocked_for(provider, account, model).is_none() && self.spent(provider, account, model).is_none()
    }

    /// The blocks of the account whose time has not passed.
    pub fn blocks(&self, provider: &str, account: &str) -> Vec<LimitBlock> {
        let now = now_secs();
        self.entries()
            .get(provider)
            .and_then(|accounts| accounts.get(account))
            .map(|entry| entry.live(now).cloned().collect())
            .unwrap_or_default()
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
                    blocked_until: entry
                        .live(now)
                        .filter(|block| block.scope.is_none())
                        .map(|block| block.until)
                        .max(),
                    blocks: entry.live(now).cloned().collect(),
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
        Window::new(name, used, Some(4_000_000_000), status)
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

    #[test]
    fn a_window_of_one_model_family_stops_only_requests_to_that_family() {
        let store = LimitStore::new();
        let later = now_secs() + 3600;
        store.record(
            "anthropic",
            "default",
            vec![
                Window::new("5h", 0.4, Some(later), "allowed"),
                Window::new("7d-fable", 1.0, Some(later), REJECTED).scoped("Fable"),
            ],
        );
        assert_eq!(
            store
                .spent("anthropic", "default", "claude-fable-5-2")
                .map(|window| window.name),
            Some("7d-fable".to_string())
        );
        assert_eq!(store.spent("anthropic", "default", "claude-opus-5-5"), None);
        assert!(store.open("anthropic", "default", "claude-opus-5-5"));
        assert!(!store.open("anthropic", "default", "claude-fable-5-2"));

        // A block for that window holds back the same requests and no others.
        let fable = store.spent("anthropic", "default", "claude-fable-5-2").unwrap();
        store.block_on("anthropic", "default", later, Some(&fable));
        assert_eq!(store.blocked_until("anthropic", "default"), None);
        let block = store
            .blocked_for("anthropic", "default", "claude-fable-5-2")
            .unwrap();
        assert_eq!(
            (block.until, block.limit().as_str()),
            (later, "7-day Fable limit")
        );
        assert_eq!(store.blocked_for("anthropic", "default", "claude-opus-5-5"), None);
        // An answer to another model says nothing about it; an answer to that model does.
        store.unblock_for("anthropic", "default", "claude-opus-5-5");
        assert!(
            store
                .blocked_for("anthropic", "default", "claude-fable-5-2")
                .is_some()
        );
        store.unblock_for("anthropic", "default", "claude-fable-5-2");
        assert_eq!(store.blocks("anthropic", "default"), []);

        // A window that has reset since the provider refused for it stands in nobody's way.
        store.record(
            "anthropic",
            "work",
            vec![Window::new("5h", 1.0, Some(now_secs() - 1), REJECTED)],
        );
        assert!(store.open("anthropic", "work", "claude-opus-5-5"));
    }

    #[test]
    fn blocks_outlive_the_process_and_a_later_report_with_room_takes_them_away() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cache").join("limits.json");
        let now = now_secs();
        let week = now + 5 * 86_400;

        let store = LimitStore::new();
        store.persist(file.clone());
        let weekly = Window::new("7d", 1.0, Some(week), REJECTED);
        store.block_on("anthropic", "work", week, Some(&weekly));
        store.block("acme", "default", now + 900);
        // A block whose time has passed is not worth keeping.
        store.block("acme", "old", now - 1);

        // The next run starts with them.
        let next = LimitStore::new();
        next.persist(file.clone());
        let block = next.blocked_for("anthropic", "work", "claude-opus-5-5").unwrap();
        assert_eq!((block.until, block.limit().as_str()), (week, "7-day limit"));
        assert_eq!(next.blocked_until("acme", "default"), Some(now + 900));
        assert_eq!(next.blocks("acme", "old"), []);

        // The file keeps each account under its `auth.json` key, beside its usage report.
        let stored = read_file(&file);
        assert_eq!(stored["anthropic#work"]["blocks"][0]["until"], week);
        assert_eq!(stored["acme"]["fetched_at"], 0);

        // A report from before the refusal, or one that still shows the window full, heals
        // nothing.
        let full = [Window::new("7d", 1.0, Some(week), REJECTED)];
        let room = [
            Window::new("5h", 0.1, None, "allowed"),
            Window::new("7d", 0.6, Some(week), "allowed"),
        ];
        next.record_report("anthropic", "work", &room, now);
        next.record_report("anthropic", "work", &full, now + HEAL_AFTER + 1);
        assert!(next.blocked_until("anthropic", "work").is_some());
        // A later one that shows room does, in the file as well.
        next.record_report("anthropic", "work", &room, now + HEAL_AFTER + 1);
        assert_eq!(next.blocked_until("anthropic", "work"), None);
        assert_eq!(next.windows("anthropic", "work"), room);
        let third = LimitStore::new();
        third.persist(file.clone());
        assert_eq!(third.blocked_until("anthropic", "work"), None);
        assert!(third.blocked_until("acme", "default").is_some());

        // An account that answers again is unblocked for the next run too, and the entry that
        // was only there for the block goes.
        third.unblock("acme", "default");
        assert!(!read_file(&file).contains_key("acme"));
    }
}
