//! Banked resets: giving a usage window back before its time.
//!
//! Two vendors hand them out. A ChatGPT plan holds reset credits
//! (`GET {root}/wham/rate-limit-reset-credits`, spent with `POST {root}/wham/rate-limit-reset-credits/consume`).
//! A Claude subscription takes part in two programs, which the usage endpoint evaluates only
//! when a flag asks it to: one reset of the 5-hour window per week
//! (`GET {base}/api/oauth/usage?at_wall=1&skip_spend=1`, member `juniper_tide`) and earned
//! grants that give a weekly window back (`?cedar_ember=1&skip_spend=1`, member
//! `cedar_ember`). Both are spent with `POST {base}/api/organizations/{org}/reset_rate_limits`.
//! None of this is documented; it is what the vendors' own clients do.
//!
//! Spending a reset cannot be undone, so it goes one way only:
//!
//! 1. the offers are read, which spends nothing ([`Desk::offers`]);
//! 2. the account, the reset and a request id are pinned in a [`Pending`];
//! 3. the redemption is written to the journal before it is sent ([`Desk::redeem`]);
//! 4. it is sent once. Nothing here sends it a second time;
//! 5. an answer that does not arrive, or cannot be read, is [`Outcome::Uncertain`]: never
//!    "not spent". The journal keeps the redemption open until a usage report fetched
//!    afterwards shows room in the window ([`Journal::settle`]). While it is open,
//!    [`Desk::redeem`] refuses a different redemption for the account unless the caller says
//!    it knows ([`Pending::over`]), and [`Desk::again`] sends the very same one, with the same
//!    request id, which a provider that has already honoured it answers without spending
//!    another.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::usage::{ANTHROPIC, CHATGPT, Login, ResetOffer, UsageReport, chatgpt_root, dashed, unix_seconds};
use crate::credentials::{Credentials, now_ms};
use crate::providers::presets::ProviderConfig;

/// A reset credit of a ChatGPT plan.
pub const CREDITS: &str = "credits";
/// The weekly reset of a Claude subscription's 5-hour window.
pub const SESSION: &str = "session";
/// An earned reset of a Claude subscription's weekly window.
pub const WEEKLY: &str = "weekly";

const JUNIPER: &str = "juniper_tide";
const CEDAR: &str = "cedar_ember";
/// How long a provider gets to say what an account holds.
const READ_TIMEOUT: Duration = Duration::from_secs(12);
/// How long a provider gets to answer a redemption before its outcome is uncertain.
const SPEND_TIMEOUT: Duration = Duration::from_secs(30);
/// A redemption is open for this long at most: by then every window it could have given back
/// has reset by itself.
const OPEN_SECS: u64 = 7 * 24 * 3600;

/// Whether `provider` hands out resets this program knows how to read and spend.
pub fn supported(provider: &ProviderConfig) -> bool {
    super::usage::supported(provider)
}

/// Whether the resets of `provider` take a question of their own, beside the usage report:
/// the report itself says nothing about them.
pub fn asked_apart(provider: &ProviderConfig) -> bool {
    asked_apart_of(&provider.id)
}

pub(crate) fn asked_apart_of(provider: &str) -> bool {
    provider == ANTHROPIC
}

/// Whether `report` shows room in the windows named `windows` (in every window that fills,
/// when none is named): what a reset that worked leaves behind. A report without any of them
/// shows nothing.
pub fn has_room(report: &UsageReport, windows: Option<&[String]>) -> bool {
    let mut about = report
        .windows
        .iter()
        .filter(|window| window.name != "overage")
        .filter(|window| windows.is_none_or(|names| names.contains(&window.name)))
        .peekable();
    about.peek().is_some() && about.all(|window| !window.full())
}

/// A window as this program names it, from the name a Claude reset program gives it.
fn window_name(theirs: &str) -> String {
    match theirs {
        "five_hour" => "5h".to_string(),
        "seven_day" => "7d".to_string(),
        // The weekly allowance of the Fable models.
        "seven_day_overage_included" => "7d-fable".to_string(),
        other => match other.strip_prefix("seven_day_") {
            Some(rest) => format!("7d-{}", dashed(rest)),
            None => dashed(other),
        },
    }
}

fn window_names(list: &Value) -> Option<Vec<String>> {
    list.as_array()?
        .iter()
        .map(|name| name.as_str().map(window_name))
        .collect()
}

/// A time of an answer: `None` inside when the member is missing or null, `None` outside when
/// it is there and cannot be read.
fn time(value: &Value) -> Option<Option<u64>> {
    match value {
        Value::Null => Some(None),
        Value::String(text) => unix_seconds(text).map(Some),
        _ => None,
    }
}

/// A flag of an answer that may be left out.
fn flag(value: &Value, missing: bool) -> Option<bool> {
    match value {
        Value::Null => Some(missing),
        Value::Bool(flag) => Some(*flag),
        _ => None,
    }
}

/// The identifier of a grant or an organisation: nothing that could change the address or the
/// body it is put into.
fn plain_id(id: &str, longest: usize) -> bool {
    !id.is_empty()
        && id.len() <= longest
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A request id as the providers want it: a random UUID.
fn request_id() -> String {
    let mut bytes: [u8; 16] = rand::random();
    bytes[6] = bytes[6] & 0x0f | 0x40;
    bytes[8] = bytes[8] & 0x3f | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// The `juniper_tide` member of a usage answer: the weekly reset of the 5-hour window.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Session {
    eligible: bool,
    /// The account is in the group that is offered the reset.
    offered: bool,
    available: bool,
    next_available_at: Option<u64>,
    /// When the week the reset belongs to ends.
    week_ends_at: Option<u64>,
}

/// `None` for a member that is null (the program was not evaluated) or not as expected: what
/// cannot be read is unknown, and nothing is spent on an unknown.
fn parse_session(block: &Value) -> Option<Session> {
    let arm = match &block["arm"] {
        Value::Null => None,
        Value::String(arm) => Some(arm.as_str()),
        _ => return None,
    };
    Some(Session {
        eligible: block["eligible"].as_bool()?,
        offered: arm == Some("reset"),
        available: flag(&block["available"], false)?,
        next_available_at: time(&block["next_available_at"])?,
        week_ends_at: time(&block["weekly_resets_at"])?,
    })
}

/// One grant of the `cedar_ember` program.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Grant {
    id: String,
    label: Option<String>,
    left: u32,
    starts_at: Option<u64>,
    ends_at: Option<u64>,
    /// The windows it gives back, as this program names them.
    clears: Vec<String>,
    paused: bool,
    usable_now: bool,
    /// Whether it gives back the weekly window of the whole account as well.
    weekly: Option<bool>,
    /// What stands in the way of using it, in the provider's words.
    blocking: Vec<String>,
}

impl Grant {
    fn held(&self, now: u64) -> bool {
        self.left > 0 && self.ends_at.is_none_or(|at| at > now)
    }

    fn spendable(&self, now: u64) -> bool {
        self.held(now)
            && !self.paused
            && self.usable_now
            && self.blocking.is_empty()
            && self.starts_at.is_none_or(|at| at <= now)
            && self.weekly != Some(false)
    }

    fn windows(&self) -> Vec<String> {
        let mut windows = self.clears.clone();
        if self.weekly == Some(true) && !windows.iter().any(|name| name == "7d") {
            windows.push("7d".to_string());
        }
        windows
    }
}

/// The `cedar_ember` member of a usage answer: earned resets of a weekly window.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Weekly {
    eligible: bool,
    at_limit: Option<bool>,
    grants: Vec<Grant>,
    /// The grant the provider would spend next.
    next: Option<String>,
    cooldown_until: Option<u64>,
}

fn parse_grant(grant: &Value) -> Option<Grant> {
    let id = grant["id"].as_str().filter(|id| plain_id(id, 40))?;
    let label = match &grant["label"] {
        Value::Null => None,
        Value::String(label) => Some(label.trim().to_string()).filter(|label| !label.is_empty()),
        _ => return None,
    };
    let list = |value: &Value| match value {
        Value::Null => Some(Vec::new()),
        list => window_names(list),
    };
    Some(Grant {
        id: id.to_string(),
        label,
        left: u32::try_from(grant["resets_left"].as_u64()?).ok()?,
        starts_at: time(&grant["starts_at"])?,
        ends_at: time(&grant["ends_at"])?,
        clears: list(&grant["clears"])?,
        paused: flag(&grant["paused"], false)?,
        usable_now: flag(&grant["usable_now"], false)?,
        weekly: match &grant["gives_back_weekly"] {
            Value::Null => None,
            Value::Bool(weekly) => Some(*weekly),
            _ => return None,
        },
        blocking: list(&grant["blocking"])?,
    })
}

/// `None` for a member that is null or not as expected, one grant that cannot be read
/// included.
fn parse_weekly(block: &Value) -> Option<Weekly> {
    let grants = match &block["grants"] {
        Value::Null => Vec::new(),
        Value::Array(grants) => grants.iter().map(parse_grant).collect::<Option<Vec<_>>>()?,
        _ => return None,
    };
    Some(Weekly {
        eligible: block["eligible"].as_bool()?,
        at_limit: match &block["at_limit"] {
            Value::Null => None,
            Value::Bool(at_limit) => Some(*at_limit),
            _ => return None,
        },
        next: block["next_grant_id"].as_str().map(str::to_string),
        cooldown_until: time(&block["cooldown_until"])?,
        grants,
    })
}

/// One reset credit of a ChatGPT plan that can be spent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Credit {
    id: String,
    title: String,
    description: Option<String>,
    expires_at: Option<u64>,
}

/// Text of a provider as it may be shown: no control characters, not endless.
fn shown_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(200).collect()
}

/// The answer of `GET /wham/rate-limit-reset-credits`: how many credits the account holds,
/// and those of them that can be spent at `now`, the one that lapses first leading and those
/// that never lapse last. `None` for an answer that is not as expected.
fn parse_credits(body: &Value, now: u64) -> Option<(u32, Vec<Credit>)> {
    let count = u32::try_from(body["available_count"].as_u64()?).unwrap_or(u32::MAX);
    let mut credits: Vec<Credit> = body["credits"]
        .as_array()?
        .iter()
        .filter_map(|credit| {
            let id = credit["id"].as_str().filter(|id| !id.is_empty())?;
            if credit["status"].as_str() != Some("available") {
                return None;
            }
            // A time that cannot be read is no permission to spend the credit.
            let expires_at = time(&credit["expires_at"])?;
            if expires_at.is_some_and(|at| at <= now) {
                return None;
            }
            let title = credit["title"]
                .as_str()
                .or_else(|| credit["reset_type"].as_str())
                .map(shown_text)
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| "Usage reset".to_string());
            Some(Credit {
                id: id.to_string(),
                title,
                description: credit["description"]
                    .as_str()
                    .map(shown_text)
                    .filter(|text| !text.is_empty()),
                expires_at,
            })
        })
        .collect();
    credits.sort_by_key(|credit| credit.expires_at.unwrap_or(u64::MAX));
    Some((count, credits))
}

/// One reset as it is about to be spent: the account, the reset and the request id are fixed,
/// so that nothing that changes while the user reads the question can spend a different one.
#[derive(Clone)]
pub struct Pending {
    pub provider: String,
    pub account: String,
    /// [`CREDITS`], [`SESSION`] or [`WEEKLY`].
    pub kind: String,
    /// What the provider calls it, or what it is.
    pub title: String,
    pub description: Option<String>,
    /// Unix seconds at which it lapses unspent, when the provider says.
    pub expires_at: Option<u64>,
    /// How many of this kind the account holds, this one included.
    pub available: u32,
    /// The windows it gives back, as this program names them (`5h`, `7d`, `7d-fable`); `None`
    /// when the provider does not say, which is every window of the plan.
    pub clears: Option<Vec<String>>,
    /// The request id of a redemption for this account that the journal still holds open and
    /// that whoever spends this one knows about. Without it, [`Desk::redeem`] does not spend a
    /// second reset while the outcome of the first is not settled.
    pub over: Option<String>,
    request_id: String,
    /// Where the redemption goes. `None` for a Claude login that does not say which
    /// organisation it belongs to: the provider is asked, with this login, before anything
    /// is sent.
    url: Option<String>,
    /// The address of the provider.
    base_url: String,
    body: Value,
    login: Arc<Login>,
}

// The token never reaches a log.
impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("provider", &self.provider)
            .field("account", &self.account)
            .field("kind", &self.kind)
            .field("title", &self.title)
            .field("request_id", &self.request_id)
            .finish_non_exhaustive()
    }
}

impl Pending {
    /// Whether spending it gives back every window of `full` (names as in
    /// [`crate::limits::Window::name`]).
    pub fn gives_back(&self, full: &[String]) -> bool {
        match &self.clears {
            None => true,
            Some(clears) => full.iter().all(|name| clears.contains(name)),
        }
    }

    /// The id that makes sending this redemption twice spend one reset.
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
}

/// What an account holds, as the provider said when it was asked.
#[derive(Debug, Clone)]
pub struct Offers {
    /// For the usage views: per kind how many, until when, and whether one can be spent.
    pub shown: Vec<ResetOffer>,
    /// Those that can be spent now, the one to spend first leading.
    pub spendable: Vec<Pending>,
}

/// How a redemption ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The provider gave the windows back.
    Reset {
        /// The windows it named, as this program names them.
        cleared: Option<Vec<String>>,
        next_available_at: Option<u64>,
    },
    /// The provider says this reset was spent before: by this very redemption when it was
    /// sent an earlier time, or (the weekly reset of the 5-hour window) by anything this week.
    /// Nothing more was spent.
    AlreadySpent { next_available_at: Option<u64> },
    /// The provider says no window is full. Nothing was spent.
    NothingToReset,
    /// The provider answered that it did not spend the reset, and why.
    NotSpent(String),
    /// The provider turned the request away before doing anything with it, or it was never
    /// sent. Nothing was spent.
    Refused(String),
    /// The request went out and no answer that can be relied on came back. The reset may be
    /// spent.
    Uncertain(String),
    /// Nothing was sent: the journal holds another redemption for the account whose outcome
    /// is not settled.
    Open(Box<Entry>),
}

/// How far a redemption written to the journal got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Written down; the request is on its way, or the process ended before the answer.
    Pending,
    /// The provider confirmed it; a usage report has not shown the room yet.
    Spent,
    /// The provider answered that nothing was spent.
    NotSpent,
    /// No answer that can be relied on.
    Uncertain,
    /// A usage report fetched afterwards showed room: nothing about it is open.
    Confirmed,
}

/// One redemption as the journal keeps it. It holds no token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub provider: String,
    pub account: String,
    pub kind: String,
    pub title: String,
    /// What was sent, the request id included.
    pub body: Value,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clears: Option<Vec<String>>,
    /// Unix seconds at which it was first written, just before the request went out.
    pub at: u64,
    /// Unix seconds at which the reset would have lapsed unspent, when the provider said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    pub state: State,
    /// What the provider said, or what went wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Entry {
    /// Whether, at `now`, it is not known yet that the account is as the redemption left it:
    /// the request may still be on its way, its outcome is uncertain, or no usage report has
    /// shown the room it made. Once the reset it spent would have lapsed anyway, or a week has
    /// passed, nothing about it is open any more.
    pub fn open(&self, now: u64) -> bool {
        matches!(self.state, State::Pending | State::Uncertain | State::Spent)
            && now < self.at.saturating_add(OPEN_SECS)
            && self.expires_at.is_none_or(|at| now < at)
    }

    /// Whether the reset may be spent without anybody knowing.
    pub fn uncertain(&self) -> bool {
        matches!(self.state, State::Pending | State::Uncertain)
    }
}

/// The journal of redemptions: one file per account in a directory, holding the account's
/// last one.
#[derive(Debug, Clone)]
pub struct Journal {
    dir: PathBuf,
}

impl Journal {
    pub fn new(dir: PathBuf) -> Journal {
        Journal { dir }
    }

    fn file(&self, provider: &str, account: &str) -> PathBuf {
        self.dir.join(format!("{provider}.{account}.json"))
    }

    /// The last redemption written down for the account. A journal that cannot be read counts
    /// as a redemption with an uncertain outcome: what it held may have been spent.
    pub fn entry(&self, provider: &str, account: &str) -> Option<Entry> {
        let text = match std::fs::read_to_string(self.file(provider, account)) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => return Some(unreadable(provider, account, &e.to_string())),
        };
        Some(serde_json::from_str(&text).unwrap_or_else(|e| unreadable(provider, account, &e.to_string())))
    }

    /// The account's last redemption when something about it is still open.
    pub fn open(&self, provider: &str, account: &str) -> Option<Entry> {
        let now = now_ms() / 1000;
        self.entry(provider, account).filter(|entry| entry.open(now))
    }

    /// Writes `entry` in place of the account's last one, and does not return before it is on
    /// the disk.
    fn write(&self, entry: &Entry) -> std::io::Result<()> {
        use std::io::Write as _;
        std::fs::create_dir_all(&self.dir)?;
        let file = self.file(&entry.provider, &entry.account);
        let partial = file.with_extension(format!("json.{}", std::process::id()));
        let mut out = std::fs::File::create(&partial)?;
        out.write_all(&serde_json::to_vec_pretty(entry)?)?;
        out.sync_all()?;
        drop(out);
        std::fs::rename(&partial, &file).inspect_err(|_| {
            let _ = std::fs::remove_file(&partial);
        })?;
        // The name has to be on the disk too before a request that cannot be taken back.
        #[cfg(unix)]
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    /// The lock that keeps two processes from redeeming for one account at the same moment,
    /// taken; it is released when the file is dropped.
    async fn lock(&self, provider: &str, account: &str) -> std::io::Result<std::fs::File> {
        let dir = self.dir.clone();
        let file = self.file(provider, account).with_extension("lock");
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(file)?;
            lock.lock()?;
            Ok(lock)
        })
        .await
        .map_err(std::io::Error::other)?
    }

    /// Settles the open redemption of the account `report` is about, when the report was
    /// fetched after it and shows room in the windows it was spent for: whatever the answer
    /// to the redemption was, the account is as a reset leaves it. Returns whether it did.
    pub fn settle(&self, report: &UsageReport) -> bool {
        let (provider, account) = (report.provider.as_str(), report.account.as_str());
        // Most accounts have no journal: nothing is read beyond that.
        if !self.file(provider, account).exists() {
            return false;
        }
        let Some(mut entry) = self.open(provider, account) else {
            return false;
        };
        // Times are whole seconds: a report of the very second the reset was sent in counts.
        if report.fetched_at < entry.at || !has_room(report, entry.clears.as_deref()) {
            return false;
        }
        entry.state = State::Confirmed;
        if let Err(e) = self.write(&entry) {
            tracing::warn!("could not settle the reset journal of {provider} ({account}): {e}");
        }
        true
    }
}

/// Where resets are read and spent.
#[derive(Clone)]
pub struct Desk {
    /// Built when a provider is first asked.
    client: Arc<OnceLock<reqwest::Client>>,
    journal: Journal,
    spend_timeout: Duration,
}

impl Desk {
    /// `journal` is the directory redemptions are written to before they are sent.
    pub fn new(journal: PathBuf) -> Desk {
        Desk {
            client: Arc::default(),
            journal: Journal::new(journal),
            spend_timeout: SPEND_TIMEOUT,
        }
    }

    /// Waits this long for the answer to a redemption.
    pub fn with_spend_timeout(mut self, timeout: Duration) -> Desk {
        self.spend_timeout = timeout;
        self
    }

    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    fn client(&self) -> &reqwest::Client {
        self.client.get_or_init(|| {
            // A redirect is never followed with a login on the request.
            crate::tls::client_builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(30))
                .user_agent(concat!("azazel/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default()
        })
    }

    async fn get(&self, login: &Login, url: String, id: &str, what: &str) -> Result<Value, String> {
        let response = login
            .on(self.client().get(url).timeout(READ_TIMEOUT))
            .send()
            .await
            .map_err(|e| format!("could not reach {id} for {what}: {}", e.without_url()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 => format!("{id} refused the login when asked for {what} (HTTP {status}): sign in again"),
                403 => format!("{id} does not let this account see {what} (HTTP {status})"),
                404 => format!("{id} has no {what} for this login (HTTP {status})"),
                _ => format!("{id} answered HTTP {status} when asked for {what}"),
            });
        }
        response
            .json()
            .await
            .map_err(|_| format!("{id} sent {what} that is not JSON"))
    }

    /// Asks `provider` which resets the login stored as `account` holds. Nothing is spent by
    /// asking. With `renew`, a login that is about to expire is renewed first.
    pub async fn offers(
        &self,
        provider: &ProviderConfig,
        credentials: &Credentials,
        account: &str,
        renew: bool,
    ) -> Result<Offers, String> {
        let login = Arc::new(Login::of(provider, credentials, account, renew).await?);
        let now = now_ms() / 1000;
        match provider.id.as_str() {
            CHATGPT => self.credit_offers(provider, account, login, now).await,
            ANTHROPIC => self.program_offers(provider, account, login, now).await,
            id => Err(format!("{id} has no banked resets")),
        }
    }

    async fn credit_offers(
        &self,
        provider: &ProviderConfig,
        account: &str,
        login: Arc<Login>,
        now: u64,
    ) -> Result<Offers, String> {
        let id = provider.id.as_str();
        let url = format!("{}/wham/rate-limit-reset-credits", chatgpt_root(provider));
        let body = self.get(&login, url.clone(), id, "its reset credits").await?;
        let (count, credits) = parse_credits(&body, now)
            .ok_or_else(|| format!("{id} sent reset credits that cannot be read"))?;
        // No more than the account is said to hold, whatever the list has.
        let count = count.min(u32::try_from(credits.len()).unwrap_or(u32::MAX));
        let shown = vec![ResetOffer {
            expires_at: credits.first().and_then(|credit| credit.expires_at),
            usable: Some(count > 0),
            ..ResetOffer::held(CREDITS, count)
        }];
        let spendable = credits
            .into_iter()
            .take(count as usize)
            .map(|credit| {
                let request_id = request_id();
                Pending {
                    provider: id.to_string(),
                    account: account.to_string(),
                    kind: CREDITS.to_string(),
                    title: credit.title,
                    description: credit.description,
                    expires_at: credit.expires_at,
                    available: count,
                    clears: None,
                    over: None,
                    body: json!({ "redeem_request_id": request_id, "credit_id": credit.id }),
                    request_id,
                    url: Some(format!("{url}/consume")),
                    base_url: provider.base_url.clone(),
                    login: login.clone(),
                }
            })
            .collect();
        Ok(Offers { shown, spendable })
    }

    async fn program_offers(
        &self,
        provider: &ProviderConfig,
        account: &str,
        login: Arc<Login>,
        now: u64,
    ) -> Result<Offers, String> {
        let id = provider.id.as_str();
        let usage = format!("{}/api/oauth/usage", provider.base_url);
        let weekly = self
            .get(
                &login,
                format!("{usage}?cedar_ember=1&skip_spend=1"),
                id,
                "its weekly resets",
            )
            .await?;
        let session = self
            .get(
                &login,
                format!("{usage}?at_wall=1&skip_spend=1"),
                id,
                "its session reset",
            )
            .await?;
        let weekly = parse_weekly(&weekly[CEDAR]);
        let session = parse_session(&session[JUNIPER]);

        let mut shown = Vec::new();
        let mut spendable = Vec::new();
        // The organisation the reset is claimed against is the one the login was made for.
        let url = stored_organisation(&login).map(|org| claim_url(&provider.base_url, &org));
        let pending = |kind: &str, title: String, body: Value| Pending {
            provider: id.to_string(),
            account: account.to_string(),
            kind: kind.to_string(),
            title,
            description: None,
            expires_at: None,
            available: 1,
            clears: None,
            over: None,
            // The session reset has no id of its own: the provider spends at most one a week
            // whatever is sent.
            request_id: body["request_id"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(request_id),
            url: url.clone(),
            base_url: provider.base_url.clone(),
            body,
            login: login.clone(),
        };
        if let Some(session) = &session {
            match (session.eligible, session.offered) {
                (false, _) => shown.push(ResetOffer {
                    eligible: false,
                    ..ResetOffer::held(SESSION, 0)
                }),
                // An account outside the group that is offered the reset is shown nothing.
                (true, false) => {}
                (true, true) => {
                    shown.push(ResetOffer {
                        expires_at: session.week_ends_at.filter(|_| session.available),
                        next_available_at: session.next_available_at.filter(|_| !session.available),
                        usable: Some(session.available),
                        ..ResetOffer::held(SESSION, u32::from(session.available))
                    });
                    if session.available {
                        spendable.push(Pending {
                            description: Some(
                                "Gives the 5-hour window back now. The weekly windows stay as they are. \
                                 There is one a week."
                                    .to_string(),
                            ),
                            expires_at: session.week_ends_at,
                            clears: Some(vec!["5h".to_string()]),
                            ..pending(
                                SESSION,
                                "Session reset".to_string(),
                                json!({ "program": JUNIPER }),
                            )
                        });
                    }
                }
            }
        }
        if let Some(weekly) = &weekly {
            let cooling = weekly.cooldown_until.filter(|at| *at > now);
            let held: Vec<&Grant> = weekly.grants.iter().filter(|grant| grant.held(now)).collect();
            let count = held
                .iter()
                .map(|grant| grant.left)
                .fold(0u32, u32::saturating_add);
            let mut ready: Vec<&Grant> = held
                .iter()
                .copied()
                .filter(|grant| weekly.eligible && cooling.is_none() && grant.spendable(now))
                .collect();
            // The one the provider would spend leads, then the one that lapses first.
            ready.sort_by_key(|grant| {
                (
                    weekly.next.as_deref() != Some(grant.id.as_str()),
                    grant.ends_at.unwrap_or(u64::MAX),
                )
            });
            if !weekly.eligible {
                shown.push(ResetOffer {
                    eligible: false,
                    ..ResetOffer::held(WEEKLY, 0)
                });
            } else if count > 0 || cooling.is_some() {
                shown.push(ResetOffer {
                    expires_at: held.iter().filter_map(|grant| grant.ends_at).min(),
                    next_available_at: cooling,
                    usable: Some(!ready.is_empty() && weekly.at_limit != Some(false)),
                    ..ResetOffer::held(WEEKLY, count)
                });
            }
            // A grant is spent against a full window only: asked for at any other time, the
            // provider has nothing to give back.
            if weekly.at_limit != Some(false) {
                for grant in ready {
                    let windows = grant.windows();
                    let gives: Vec<String> = windows
                        .iter()
                        .map(|name| crate::limits::Window::new(name.as_str(), 1.0, None, "").label())
                        .collect();
                    spendable.push(Pending {
                        description: (!gives.is_empty())
                            .then(|| format!("Gives back: {}.", gives.join(", "))),
                        expires_at: grant.ends_at,
                        available: count,
                        clears: Some(windows),
                        ..pending(
                            WEEKLY,
                            match &grant.label {
                                Some(label) => format!("Weekly reset ({})", shown_text(label)),
                                None => "Weekly reset".to_string(),
                            },
                            json!({ "program": CEDAR, "grant_id": grant.id, "request_id": request_id() }),
                        )
                    });
                }
            }
        }
        Ok(Offers { shown, spendable })
    }

    /// The account's open redemption, to be sent once more exactly as it was: the same reset
    /// and the same request id, so that a provider that honoured it the first time spends
    /// nothing more. For when the user asks for it knowingly; nothing here does it unasked.
    pub async fn again(
        &self,
        provider: &ProviderConfig,
        credentials: &Credentials,
        account: &str,
    ) -> Result<Pending, String> {
        let id = provider.id.as_str();
        let entry = self
            .journal
            .open(id, account)
            .filter(Entry::uncertain)
            .ok_or_else(|| format!("no reset of account {account} of {id} is waiting for its outcome"))?;
        if entry.request_id.is_empty() {
            return Err(entry
                .detail
                .unwrap_or_else(|| "the journal of the account cannot be read".to_string()));
        }
        let login = Arc::new(Login::of(provider, credentials, account, true).await?);
        let url = match id {
            CHATGPT => Some(format!(
                "{}/wham/rate-limit-reset-credits/consume",
                chatgpt_root(provider)
            )),
            ANTHROPIC => stored_organisation(&login).map(|org| claim_url(&provider.base_url, &org)),
            id => return Err(format!("{id} has no banked resets")),
        };
        Ok(Pending {
            provider: id.to_string(),
            account: account.to_string(),
            kind: entry.kind,
            title: entry.title,
            description: None,
            expires_at: entry.expires_at,
            available: 1,
            clears: entry.clears,
            over: None,
            request_id: entry.request_id,
            url,
            base_url: provider.base_url.clone(),
            body: entry.body,
            login,
        })
    }

    /// Spends `pending`: writes it to the journal, sends it once, and writes down how it
    /// ended. Nothing is sent when the journal cannot be written first, or when it holds a
    /// different redemption for the account whose outcome is open and `pending.over` does not
    /// name it.
    pub async fn redeem(&self, pending: &Pending) -> Outcome {
        let (provider, account) = (pending.provider.as_str(), pending.account.as_str());
        let url = match &pending.url {
            Some(url) => url.clone(),
            None => match self.organisation(pending).await {
                Ok(org) => claim_url(&pending.base_url, &org),
                Err(why) => return Outcome::Refused(format!("the reset was not sent: {why}")),
            },
        };
        let lock = match self.journal.lock(provider, account).await {
            Ok(lock) => lock,
            Err(e) => {
                return Outcome::Refused(format!(
                    "the reset was not sent: the journal could not be locked ({e})"
                ));
            }
        };
        let now = now_ms() / 1000;
        let earlier = self.journal.entry(provider, account);
        if let Some(open) = earlier.as_ref().filter(|entry| entry.open(now))
            && open.request_id != pending.request_id
            && pending.over.as_deref() != Some(open.request_id.as_str())
        {
            return Outcome::Open(Box::new(open.clone()));
        }
        let mut entry = Entry {
            provider: provider.to_string(),
            account: account.to_string(),
            kind: pending.kind.clone(),
            title: pending.title.clone(),
            body: pending.body.clone(),
            request_id: pending.request_id.clone(),
            clears: pending.clears.clone(),
            // The same redemption sent once more keeps the time it was first sent at.
            at: earlier
                .filter(|entry| entry.request_id == pending.request_id)
                .map_or(now, |entry| entry.at),
            expires_at: pending.expires_at,
            state: State::Pending,
            detail: None,
        };
        if let Err(e) = self.journal.write(&entry) {
            return Outcome::Refused(format!(
                "the reset was not sent: it could not be written to {} first ({e})",
                self.journal.dir.display()
            ));
        }
        let outcome = self.send(pending, &url).await;
        let (state, detail) = match &outcome {
            Outcome::Reset { .. } => (State::Spent, None),
            Outcome::AlreadySpent { .. } => (
                State::Spent,
                Some("the provider says it was spent before".to_string()),
            ),
            Outcome::NothingToReset => (
                State::NotSpent,
                Some("the provider says no window is full".to_string()),
            ),
            Outcome::NotSpent(why) | Outcome::Refused(why) => (State::NotSpent, Some(why.clone())),
            Outcome::Uncertain(why) => (State::Uncertain, Some(why.clone())),
            Outcome::Open(_) => (State::Uncertain, None),
        };
        entry.state = state;
        entry.detail = detail;
        // An entry that stays as it was written before the request reads as uncertain.
        if let Err(e) = self.journal.write(&entry) {
            tracing::warn!("could not write the outcome of a reset to the journal: {e}");
        }
        drop(lock);
        outcome
    }

    /// The organisation of the login `pending` was pinned with, asked from the provider with
    /// that same login.
    async fn organisation(&self, pending: &Pending) -> Result<String, String> {
        let (id, account) = (pending.provider.as_str(), pending.account.as_str());
        let profile = self
            .get(
                &pending.login,
                format!("{}/api/oauth/profile", pending.base_url),
                id,
                "the account's organisation",
            )
            .await?;
        profile["organization"]["uuid"]
            .as_str()
            .filter(|org| plain_id(org, 128))
            .map(str::to_string)
            .ok_or_else(|| {
                format!("the login of account {account} of {id} names no organisation to reset the limits of")
            })
    }

    /// The one request that spends. Whatever comes back, it is not sent again.
    async fn send(&self, pending: &Pending, url: &str) -> Outcome {
        let id = pending.provider.as_str();
        let request = pending
            .login
            .on(self.client().post(url).timeout(self.spend_timeout))
            .json(&pending.body);
        let response = match request.send().await {
            Ok(response) => response,
            Err(e) if e.is_timeout() => {
                return Outcome::Uncertain(format!(
                    "{id} did not answer within {} s",
                    self.spend_timeout.as_secs().max(1)
                ));
            }
            // Nothing left this machine.
            Err(e) if e.is_connect() => {
                return Outcome::Refused(format!("{id} could not be reached: {}", e.without_url()));
            }
            Err(e) => return Outcome::Uncertain(format!("the answer of {id} was lost: {}", e.without_url())),
        };
        let status = response.status();
        if !status.is_success() {
            return match status.as_u16() {
                401 => Outcome::Refused(format!("{id} refused the login (HTTP {status}): sign in again")),
                403 => Outcome::Refused(format!(
                    "{id} does not let this account spend a reset (HTTP {status})"
                )),
                400 | 404 | 422 => Outcome::Refused(format!("{id} did not take the request (HTTP {status})")),
                _ => Outcome::Uncertain(format!("{id} answered HTTP {status}")),
            };
        }
        let Ok(body) = response.json::<Value>().await else {
            return Outcome::Uncertain(format!("{id} sent an answer that cannot be read"));
        };
        let next_available_at = time(&body["next_available_at"]).flatten();
        let word = match pending.kind.as_str() {
            CREDITS => body["code"].as_str(),
            _ => body["result"].as_str(),
        };
        match word {
            Some("reset") => Outcome::Reset {
                cleared: window_names(&body["cleared"]).or_else(|| pending.clears.clone()),
                next_available_at,
            },
            Some("already_redeemed" | "already_used") => Outcome::AlreadySpent { next_available_at },
            Some("nothing_to_reset" | "not_limited") => Outcome::NothingToReset,
            Some("no_credit") => Outcome::NotSpent(format!("{id} says the account holds no reset to spend")),
            Some("ineligible") => Outcome::NotSpent(format!("{id} says this account cannot use the reset")),
            Some(other) => Outcome::Uncertain(format!("{id} answered `{}`", shown_text(other))),
            None => Outcome::Uncertain(format!("{id} sent an answer that names no outcome")),
        }
    }
}

/// The organisation a Claude login was made for, when the login says.
fn stored_organisation(login: &Login) -> Option<String> {
    login
        .fields
        .get("org_id")
        .filter(|org| plain_id(org, 128))
        .cloned()
}

/// Where a Claude reset is claimed.
fn claim_url(base_url: &str, organisation: &str) -> String {
    format!("{base_url}/api/organizations/{organisation}/reset_rate_limits")
}

fn unreadable(provider: &str, account: &str, why: &str) -> Entry {
    Entry {
        provider: provider.to_string(),
        account: account.to_string(),
        kind: String::new(),
        title: String::new(),
        body: Value::Null,
        request_id: String::new(),
        clears: None,
        // Read as fresh: what cannot be read is never taken for old and done with.
        at: now_ms() / 1000,
        expires_at: None,
        state: State::Uncertain,
        detail: Some(format!("the journal of the account cannot be read ({why})")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Mutex;

    use wiremock::matchers::{body_json, body_partial_json, header, headers, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::credentials::{Credential, account_key};
    use crate::limits::Window;
    use crate::providers::presets;

    const LATER: &str = "2099-03-09T07:59:00Z";
    const LATER_SECS: u64 = 4_076_726_340;

    fn login(extra: &[(&str, &str)]) -> Credential {
        Credential::OAuth {
            access: "access-token".into(),
            refresh: "refresh-token".into(),
            expires: now_ms() + 3_600_000,
            extra: extra
                .iter()
                .map(|(name, value)| (name.to_string(), json!(value)))
                .collect(),
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

    /// A scratch home with one ChatGPT login and one Claude login, both at `server`.
    struct Bench {
        _dir: tempfile::TempDir,
        credentials: Credentials,
        chatgpt: ProviderConfig,
        claude: ProviderConfig,
        desk: Desk,
    }

    fn bench(server: &MockServer) -> Bench {
        let dir = tempfile::tempdir().unwrap();
        let credentials = Credentials::with_env(dir.path().join("auth.json"), Arc::new(|_: &str| None));
        credentials
            .store("openai-codex", login(&[("account_id", "acct-7")]))
            .unwrap();
        credentials
            .store("anthropic", login(&[("org_id", "org-1")]))
            .unwrap();
        Bench {
            desk: Desk::new(dir.path().join("resets")),
            _dir: dir,
            credentials,
            chatgpt: preset("openai-codex", format!("{}/backend-api/codex", server.uri())),
            claude: preset("anthropic", server.uri()),
        }
    }

    fn report(provider: &str, windows: Vec<Window>) -> UsageReport {
        UsageReport {
            provider: provider.to_string(),
            account: "default".to_string(),
            plan: None,
            windows,
            extra_usage: None,
            spend: None,
            resets: Vec::new(),
            resets_asked_at: None,
            fetched_at: now_ms() / 1000,
        }
    }

    async fn credits_listed(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/backend-api/wham/rate-limit-reset-credits"))
            .and(header("authorization", "Bearer access-token"))
            .and(header("chatgpt-account-id", "acct-7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "available_count": 2,
                "credits": [
                    { "id": "gone", "status": "available", "reset_type": "weekly",
                      "expires_at": "2020-01-01T00:00:00Z" },
                    { "id": "used", "status": "redeemed", "reset_type": "weekly", "expires_at": null },
                    { "id": "keeps", "status": "available", "reset_type": "weekly", "expires_at": null },
                    { "id": "soon", "status": "available", "reset_type": "weekly", "expires_at": LATER,
                      "title": "Weekly reset", "description": "Resets every window of the plan." }
                ]
            })))
            .mount(server)
            .await;
    }

    /// The POSTs the server got, as `(path, body)`.
    async fn posts(server: &MockServer) -> Vec<(String, Value)> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.method.as_str() == "POST")
            .map(|request| {
                (
                    request.url.path().to_string(),
                    serde_json::from_slice(&request.body).unwrap(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn reset_credits_are_read_and_the_one_that_lapses_first_is_the_one_to_spend() {
        let server = MockServer::start().await;
        credits_listed(&server).await;
        let bench = bench(&server);

        let offers = bench
            .desk
            .offers(&bench.chatgpt, &bench.credentials, "default", true)
            .await
            .unwrap();
        assert_eq!(
            offers.shown,
            [ResetOffer {
                expires_at: Some(LATER_SECS),
                usable: Some(true),
                ..ResetOffer::held(CREDITS, 2)
            }]
        );
        // The lapsed and the redeemed one are not on offer; the one with a date leads.
        let titles: Vec<(&str, Option<u64>)> = offers
            .spendable
            .iter()
            .map(|pending| (pending.title.as_str(), pending.expires_at))
            .collect();
        assert_eq!(titles, [("Weekly reset", Some(LATER_SECS)), ("weekly", None)]);
        let first = &offers.spendable[0];
        assert_eq!(
            (first.kind.as_str(), first.available, first.clears.clone()),
            (CREDITS, 2, None)
        );
        assert!(first.gives_back(&["7d".to_string()]));
        // Reading spends nothing, and the token is not in what a log would print.
        assert!(posts(&server).await.is_empty());
        assert!(!format!("{first:?}").contains("access-token"));
        assert_eq!(bench.desk.journal().entry("openai-codex", "default"), None);
    }

    #[tokio::test]
    async fn a_credit_is_written_down_before_it_is_sent_and_sent_once() {
        let server = MockServer::start().await;
        credits_listed(&server).await;
        let bench = bench(&server);
        // What the journal held at the moment the redemption reached the provider.
        let seen: Arc<Mutex<Option<Entry>>> = Arc::default();
        let (journal, at_arrival) = (bench.desk.journal().clone(), seen.clone());
        Mock::given(method("POST"))
            .and(path("/backend-api/wham/rate-limit-reset-credits/consume"))
            .and(header("authorization", "Bearer access-token"))
            .and(header("chatgpt-account-id", "acct-7"))
            .and(body_partial_json(json!({ "credit_id": "soon" })))
            .respond_with(move |_: &Request| {
                *at_arrival.lock().unwrap() = journal.entry("openai-codex", "default");
                ResponseTemplate::new(200).set_body_json(json!({ "code": "reset", "windows_reset": 2 }))
            })
            .expect(1)
            .mount(&server)
            .await;

        let offers = bench
            .desk
            .offers(&bench.chatgpt, &bench.credentials, "default", true)
            .await
            .unwrap();
        let pending = &offers.spendable[0];
        let outcome = bench.desk.redeem(pending).await;
        assert_eq!(
            outcome,
            Outcome::Reset {
                cleared: None,
                next_available_at: None
            }
        );
        let sent = posts(&server).await;
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].1,
            json!({ "redeem_request_id": pending.request_id(), "credit_id": "soon" })
        );
        let before = seen.lock().unwrap().clone().unwrap();
        assert_eq!(
            (before.state, before.request_id.as_str()),
            (State::Pending, pending.request_id())
        );
        let after = bench.desk.journal().entry("openai-codex", "default").unwrap();
        assert_eq!(after.state, State::Spent);
        assert_eq!(after.body, sent[0].1);
        // Spent, and still open until a usage report shows the room it made.
        assert!(after.open(now_ms() / 1000));
        let full = report("openai-codex", vec![Window::new("7d", 1.0, None, "rejected")]);
        assert!(!bench.desk.journal().settle(&full));
        let roomy = report("openai-codex", vec![Window::new("7d", 0.0, None, "allowed")]);
        assert!(bench.desk.journal().settle(&roomy));
        assert_eq!(
            bench
                .desk
                .journal()
                .entry("openai-codex", "default")
                .unwrap()
                .state,
            State::Confirmed
        );
        assert_eq!(bench.desk.journal().open("openai-codex", "default"), None);
    }

    #[tokio::test]
    async fn every_answer_to_a_credit_has_its_outcome() {
        let cases = [
            (
                json!({ "code": "nothing_to_reset", "windows_reset": 0 }),
                State::NotSpent,
            ),
            (json!({ "code": "no_credit" }), State::NotSpent),
            (json!({ "code": "already_redeemed" }), State::Spent),
            (json!({ "code": "on_its_way" }), State::Uncertain),
            (json!({ "windows_reset": 1 }), State::Uncertain),
            (json!("reset"), State::Uncertain),
        ];
        for (answer, state) in cases {
            let server = MockServer::start().await;
            credits_listed(&server).await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(answer.clone()))
                .mount(&server)
                .await;
            let bench = bench(&server);
            let offers = bench
                .desk
                .offers(&bench.chatgpt, &bench.credentials, "default", true)
                .await
                .unwrap();
            let outcome = bench.desk.redeem(&offers.spendable[0]).await;
            match (answer["code"].as_str(), &outcome) {
                (Some("nothing_to_reset"), Outcome::NothingToReset)
                | (Some("no_credit"), Outcome::NotSpent(_))
                | (Some("already_redeemed"), Outcome::AlreadySpent { .. })
                | (Some("on_its_way") | None, Outcome::Uncertain(_)) => {}
                (_, other) => panic!("{answer}: {other:?}"),
            }
            let entry = bench.desk.journal().entry("openai-codex", "default").unwrap();
            assert_eq!(entry.state, state, "{answer}");
            assert_eq!(posts(&server).await.len(), 1, "{answer}");
        }

        // A status that says the request was turned away spends nothing; any other leaves
        // the outcome open. Neither names the address or what the server wrote.
        for (status, refused) in [
            (401, true),
            (403, true),
            (404, true),
            (429, false),
            (500, false),
            (502, false),
        ] {
            let server = MockServer::start().await;
            credits_listed(&server).await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_string("token=secret"))
                .mount(&server)
                .await;
            let bench = bench(&server);
            let offers = bench
                .desk
                .offers(&bench.chatgpt, &bench.credentials, "default", true)
                .await
                .unwrap();
            let outcome = bench.desk.redeem(&offers.spendable[0]).await;
            let (why, state) = match (&outcome, refused) {
                (Outcome::Refused(why), true) => (why, State::NotSpent),
                (Outcome::Uncertain(why), false) => (why, State::Uncertain),
                other => panic!("{status}: {other:?}"),
            };
            assert!(why.contains(&status.to_string()), "{why}");
            assert!(!why.contains("secret") && !why.contains("127.0.0.1"), "{why}");
            assert_eq!(
                bench
                    .desk
                    .journal()
                    .entry("openai-codex", "default")
                    .unwrap()
                    .state,
                state
            );
            assert_eq!(posts(&server).await.len(), 1, "{status}");
        }
    }

    #[tokio::test]
    async fn a_redemption_without_an_answer_is_uncertain_and_never_becomes_a_second_spend() {
        let server = MockServer::start().await;
        credits_listed(&server).await;
        // The provider takes the request and answers too late.
        let slow = Mock::given(method("POST"))
            .and(path("/backend-api/wham/rate-limit-reset-credits/consume"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "code": "reset", "windows_reset": 2 }))
                    .set_delay(Duration::from_secs(30)),
            )
            .mount_as_scoped(&server)
            .await;
        let mut bench = bench(&server);
        bench.desk = bench.desk.with_spend_timeout(Duration::from_millis(300));

        let offers = bench
            .desk
            .offers(&bench.chatgpt, &bench.credentials, "default", true)
            .await
            .unwrap();
        let first = offers.spendable[0].clone();
        let Outcome::Uncertain(why) = bench.desk.redeem(&first).await else {
            panic!("a timeout is not an answer");
        };
        assert!(why.contains("did not answer"), "{why}");
        let entry = bench.desk.journal().open("openai-codex", "default").unwrap();
        assert!(entry.uncertain());
        assert_eq!(entry.request_id, first.request_id());
        assert_eq!(posts(&server).await.len(), 1);
        drop(slow);

        // From here on the provider answers at once, as one that honoured the first request.
        Mock::given(method("POST"))
            .and(path("/backend-api/wham/rate-limit-reset-credits/consume"))
            .and(body_json(
                json!({ "redeem_request_id": first.request_id(), "credit_id": "soon" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "code": "already_redeemed" })))
            .mount(&server)
            .await;

        // Another reset of the account is not spent while the first is open: nothing is sent.
        let offers = bench
            .desk
            .offers(&bench.chatgpt, &bench.credentials, "default", true)
            .await
            .unwrap();
        let second = offers.spendable[0].clone();
        assert_ne!(second.request_id(), first.request_id());
        assert_eq!(
            bench.desk.redeem(&second).await,
            Outcome::Open(Box::new(entry.clone()))
        );
        assert_eq!(posts(&server).await.len(), 1);

        // Asked for knowingly, the very same redemption goes out once more, with the same id.
        let again = bench
            .desk
            .again(&bench.chatgpt, &bench.credentials, "default")
            .await
            .unwrap();
        assert_eq!(again.request_id(), first.request_id());
        assert_eq!(
            bench.desk.redeem(&again).await,
            Outcome::AlreadySpent {
                next_available_at: None
            }
        );
        let sent = posts(&server).await;
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].1, sent[1].1);
        assert_eq!(sent[1].1["redeem_request_id"], first.request_id());
        let settled = bench.desk.journal().entry("openai-codex", "default").unwrap();
        assert_eq!((settled.state, settled.at), (State::Spent, entry.at));
        // Nothing is left to send again.
        assert!(
            bench
                .desk
                .again(&bench.chatgpt, &bench.credentials, "default")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_reset_that_cannot_be_written_down_is_not_sent() {
        let server = MockServer::start().await;
        credits_listed(&server).await;
        let mut bench = bench(&server);
        let offers = bench
            .desk
            .offers(&bench.chatgpt, &bench.credentials, "default", true)
            .await
            .unwrap();
        // The journal's directory is a file: nothing can be written into it.
        let blocked = bench._dir.path().join("blocked");
        std::fs::write(&blocked, "").unwrap();
        bench.desk = Desk::new(blocked);
        let Outcome::Refused(why) = bench.desk.redeem(&offers.spendable[0]).await else {
            panic!("sent without a journal");
        };
        assert!(why.contains("was not sent"), "{why}");
        assert!(posts(&server).await.is_empty());
    }

    #[tokio::test]
    async fn a_journal_that_cannot_be_read_counts_as_an_open_redemption() {
        let server = MockServer::start().await;
        credits_listed(&server).await;
        let bench = bench(&server);
        let dir = bench._dir.path().join("resets");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("openai-codex.default.json"), "{ not json").unwrap();

        let open = bench.desk.journal().open("openai-codex", "default").unwrap();
        assert!(open.uncertain());
        let offers = bench
            .desk
            .offers(&bench.chatgpt, &bench.credentials, "default", true)
            .await
            .unwrap();
        assert!(matches!(
            bench.desk.redeem(&offers.spendable[0]).await,
            Outcome::Open(_)
        ));
        // There is nothing in it to send again either.
        assert!(
            bench
                .desk
                .again(&bench.chatgpt, &bench.credentials, "default")
                .await
                .is_err()
        );
        assert!(posts(&server).await.is_empty());
    }

    #[test]
    fn a_redemption_is_open_until_it_is_settled_lapses_or_a_week_has_passed() {
        let entry = |state: State, expires_at: Option<u64>| Entry {
            provider: "anthropic".into(),
            account: "default".into(),
            kind: SESSION.into(),
            title: "Session reset".into(),
            body: json!({ "program": JUNIPER }),
            request_id: "r".into(),
            clears: Some(vec!["5h".into()]),
            at: 1_000_000,
            expires_at,
            state,
            detail: None,
        };
        for state in [State::Pending, State::Uncertain, State::Spent] {
            assert!(entry(state, None).open(1_000_001), "{state:?}");
        }
        for state in [State::NotSpent, State::Confirmed] {
            assert!(!entry(state, None).open(1_000_001), "{state:?}");
        }
        assert!(entry(State::Uncertain, None).open(1_000_000 + OPEN_SECS - 1));
        assert!(!entry(State::Uncertain, None).open(1_000_000 + OPEN_SECS));
        // The week the session reset belonged to is over: sending it again would spend the
        // next one.
        assert!(!entry(State::Uncertain, Some(1_000_500)).open(1_000_500));
        assert!(!entry(State::Spent, None).uncertain() && entry(State::Pending, None).uncertain());
    }

    async fn programs(server: &MockServer, weekly: Value, session: Value) {
        for (flag, member, block) in [("cedar_ember", CEDAR, weekly), ("at_wall", JUNIPER, session)] {
            Mock::given(method("GET"))
                .and(path("/api/oauth/usage"))
                .and(query_param(flag, "1"))
                .and(query_param("skip_spend", "1"))
                .and(header("authorization", "Bearer access-token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "five_hour": { "utilization": 100.0, "resets_at": null },
                    member: block
                })))
                .mount(server)
                .await;
        }
    }

    fn fable_grant() -> Value {
        json!({
            "eligible": true, "at_limit": true, "exhausted": ["seven_day_overage_included"],
            "next_grant_id": "fable_launch",
            "grants": [
                { "id": "spent", "resets_left": 0, "starts_at": null, "ends_at": LATER,
                  "clears": ["five_hour"], "paused": false, "usable_now": true, "blocking": [] },
                { "id": "fable_launch", "label": "Fable launch", "resets_left": 2, "starts_at": null,
                  "ends_at": LATER, "clears": ["five_hour", "seven_day_overage_included"],
                  "paused": false, "usable_now": true, "gives_back_weekly": true, "blocking": [] }
            ]
        })
    }

    fn session_ready() -> Value {
        json!({ "eligible": true, "arm": "reset", "available": true, "next_available_at": null,
                "resets_per_week": 1, "weekly_resets_at": LATER })
    }

    #[tokio::test]
    async fn both_claude_programs_are_read_and_each_reset_says_what_it_gives_back() {
        let server = MockServer::start().await;
        programs(&server, fable_grant(), session_ready()).await;
        let bench = bench(&server);

        let offers = bench
            .desk
            .offers(&bench.claude, &bench.credentials, "default", true)
            .await
            .unwrap();
        assert_eq!(
            offers.shown,
            [
                ResetOffer {
                    expires_at: Some(LATER_SECS),
                    usable: Some(true),
                    ..ResetOffer::held(SESSION, 1)
                },
                ResetOffer {
                    expires_at: Some(LATER_SECS),
                    usable: Some(true),
                    ..ResetOffer::held(WEEKLY, 2)
                },
            ]
        );
        let [session, weekly] = offers.spendable.as_slice() else {
            panic!("{:?}", offers.spendable);
        };
        assert_eq!(
            (session.kind.as_str(), session.clears.clone()),
            (SESSION, Some(vec!["5h".to_string()]))
        );
        assert_eq!(
            (weekly.kind.as_str(), weekly.title.as_str(), weekly.available),
            (WEEKLY, "Weekly reset (Fable launch)", 2)
        );
        assert_eq!(
            weekly.clears,
            Some(vec!["5h".to_string(), "7d-fable".to_string(), "7d".to_string()])
        );
        // The session reset does nothing for the weekly window of the Fable models.
        let fable = ["7d-fable".to_string()];
        assert!(!session.gives_back(&fable) && weekly.gives_back(&fable));
        assert!(session.gives_back(&["5h".to_string()]));
        assert!(posts(&server).await.is_empty());
    }

    #[tokio::test]
    async fn claude_resets_are_claimed_against_the_organisation_of_the_login() {
        let server = MockServer::start().await;
        programs(&server, fable_grant(), session_ready()).await;
        Mock::given(method("POST"))
            .and(path("/api/organizations/org-1/reset_rate_limits"))
            .and(header("authorization", "Bearer access-token"))
            .and(headers(
                "anthropic-beta",
                vec!["claude-code-20250219", "oauth-2025-04-20"],
            ))
            .and(body_json(json!({ "program": "juniper_tide" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "reset", "next_available_at": LATER
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/organizations/org-1/reset_rate_limits"))
            .and(body_partial_json(
                json!({ "program": "cedar_ember", "grant_id": "fable_launch" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": "reset", "cleared": ["seven_day_overage_included"]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let bench = bench(&server);
        let offers = bench
            .desk
            .offers(&bench.claude, &bench.credentials, "default", true)
            .await
            .unwrap();
        let [session, weekly] = offers.spendable.as_slice() else {
            panic!("{:?}", offers.spendable);
        };

        assert_eq!(
            bench.desk.redeem(session).await,
            Outcome::Reset {
                cleared: Some(vec!["5h".to_string()]),
                next_available_at: Some(LATER_SECS)
            }
        );
        // The session reset is spent and not settled: the weekly one goes only over it.
        assert!(matches!(bench.desk.redeem(weekly).await, Outcome::Open(_)));
        let mut weekly = weekly.clone();
        weekly.over = Some(session.request_id().to_string());
        assert_eq!(
            bench.desk.redeem(&weekly).await,
            Outcome::Reset {
                cleared: Some(vec!["7d-fable".to_string()]),
                next_available_at: None
            }
        );
        let sent = posts(&server).await;
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1].1["request_id"], weekly.request_id());
        let entry = bench.desk.journal().entry("anthropic", "default").unwrap();
        assert_eq!(
            (entry.kind.as_str(), entry.state, entry.expires_at),
            (WEEKLY, State::Spent, Some(LATER_SECS))
        );
    }

    #[tokio::test]
    async fn every_answer_to_a_claude_reset_has_its_outcome() {
        for (answer, expected) in [
            (
                json!({ "result": "already_used", "next_available_at": LATER }),
                Outcome::AlreadySpent {
                    next_available_at: Some(LATER_SECS),
                },
            ),
            (json!({ "result": "not_limited" }), Outcome::NothingToReset),
            (
                json!({ "result": "ineligible" }),
                Outcome::NotSpent("anthropic says this account cannot use the reset".into()),
            ),
            (
                json!({ "result": "queued" }),
                Outcome::Uncertain("anthropic answered `queued`".into()),
            ),
        ] {
            let server = MockServer::start().await;
            programs(&server, Value::Null, session_ready()).await;
            Mock::given(method("POST"))
                .and(path("/api/organizations/org-1/reset_rate_limits"))
                .respond_with(ResponseTemplate::new(200).set_body_json(answer.clone()))
                .mount(&server)
                .await;
            let bench = bench(&server);
            let offers = bench
                .desk
                .offers(&bench.claude, &bench.credentials, "default", true)
                .await
                .unwrap();
            assert_eq!(
                bench.desk.redeem(&offers.spendable[0]).await,
                expected,
                "{answer}"
            );
            assert_eq!(posts(&server).await.len(), 1);
        }
    }

    #[tokio::test]
    async fn what_cannot_be_spent_is_shown_and_not_offered() {
        // Not in the group that is offered the session reset; a grant held but the account
        // not at its limit.
        let server = MockServer::start().await;
        let mut weekly = fable_grant();
        weekly["at_limit"] = json!(false);
        programs(
            &server,
            weekly,
            json!({ "eligible": true, "arm": "control", "available": true }),
        )
        .await;
        let bench = bench(&server);
        let offers = bench
            .desk
            .offers(&bench.claude, &bench.credentials, "default", true)
            .await
            .unwrap();
        assert_eq!(
            offers.shown,
            [ResetOffer {
                expires_at: Some(LATER_SECS),
                usable: Some(false),
                ..ResetOffer::held(WEEKLY, 2)
            }]
        );
        assert!(offers.spendable.is_empty());

        // The session reset is used up; the provider says the account takes no part in the
        // weekly program.
        let server = MockServer::start().await;
        programs(
            &server,
            json!({ "eligible": false, "ineligible_reason": "plan" }),
            json!({ "eligible": true, "arm": "reset", "available": false, "next_available_at": LATER }),
        )
        .await;
        let bench = self::bench(&server);
        let offers = bench
            .desk
            .offers(&bench.claude, &bench.credentials, "default", true)
            .await
            .unwrap();
        assert_eq!(
            offers.shown,
            [
                ResetOffer {
                    next_available_at: Some(LATER_SECS),
                    usable: Some(false),
                    ..ResetOffer::held(SESSION, 0)
                },
                ResetOffer {
                    eligible: false,
                    ..ResetOffer::held(WEEKLY, 0)
                },
            ]
        );
        assert!(offers.spendable.is_empty());

        // Members that were not evaluated, and a grant that cannot be read, are unknown:
        // nothing is shown, nothing is offered.
        let server = MockServer::start().await;
        let mut broken = fable_grant();
        broken["grants"][1]["resets_left"] = json!("two");
        programs(&server, broken, Value::Null).await;
        let bench = self::bench(&server);
        let offers = bench
            .desk
            .offers(&bench.claude, &bench.credentials, "default", true)
            .await
            .unwrap();
        assert!(offers.shown.is_empty() && offers.spendable.is_empty());

        // A grant that is paused, not yet started, or blocked is held and not spendable.
        for (field, value) in [
            ("paused", json!(true)),
            ("usable_now", json!(false)),
            ("starts_at", json!(LATER)),
            ("blocking", json!(["seven_day"])),
            ("gives_back_weekly", json!(false)),
        ] {
            let server = MockServer::start().await;
            let mut weekly = fable_grant();
            weekly["grants"][1][field] = value;
            programs(&server, weekly, Value::Null).await;
            let bench = self::bench(&server);
            let offers = bench
                .desk
                .offers(&bench.claude, &bench.credentials, "default", true)
                .await
                .unwrap();
            assert_eq!(offers.shown[0].available, 2, "{field}");
            assert_eq!(offers.shown[0].usable, Some(false), "{field}");
            assert!(offers.spendable.is_empty(), "{field}");
        }
    }

    #[tokio::test]
    async fn a_login_without_an_organisation_asks_the_profile_and_refusals_are_plain() {
        let server = MockServer::start().await;
        programs(&server, Value::Null, session_ready()).await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/profile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "account": { "email": "a@example.com" },
                "organization": { "uuid": "org-from-profile" }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/organizations/org-from-profile/reset_rate_limits"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "result": "reset" })))
            .expect(1)
            .mount(&server)
            .await;
        let bench = bench(&server);
        bench
            .credentials
            .store(&account_key("anthropic", "work"), login(&[]))
            .unwrap();
        let offers = bench
            .desk
            .offers(&bench.claude, &bench.credentials, "work", true)
            .await
            .unwrap();
        assert!(matches!(
            bench.desk.redeem(&offers.spendable[0]).await,
            Outcome::Reset { .. }
        ));
        assert!(bench.desk.journal().entry("anthropic", "work").is_some());
        assert_eq!(bench.desk.journal().entry("anthropic", "default"), None);

        // A login whose organisation the provider does not name spends nothing: nothing is
        // sent, nothing is written down.
        let nameless = MockServer::start().await;
        programs(&nameless, Value::Null, session_ready()).await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/profile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "account": {} })))
            .mount(&nameless)
            .await;
        bench
            .credentials
            .store(&account_key("anthropic", "lost"), login(&[]))
            .unwrap();
        let offers = bench
            .desk
            .offers(
                &preset("anthropic", nameless.uri()),
                &bench.credentials,
                "lost",
                true,
            )
            .await
            .unwrap();
        let Outcome::Refused(why) = bench.desk.redeem(&offers.spendable[0]).await else {
            panic!("claimed without an organisation");
        };
        assert!(why.contains("names no organisation"), "{why}");
        assert!(posts(&nameless).await.is_empty());
        assert_eq!(bench.desk.journal().entry("anthropic", "lost"), None);

        // An account that is a stored key holds no resets, and a refusal to read names
        // neither the address nor what the server wrote.
        bench
            .credentials
            .store(
                &account_key("anthropic", "key"),
                Credential::ApiKey { key: "k".into() },
            )
            .unwrap();
        let refused = bench
            .desk
            .offers(&bench.claude, &bench.credentials, "key", true)
            .await
            .unwrap_err();
        assert!(refused.contains("not a login"), "{refused}");
        let gone = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_string("token=secret"))
            .mount(&gone)
            .await;
        let refused = bench
            .desk
            .offers(
                &preset("anthropic", gone.uri()),
                &bench.credentials,
                "default",
                true,
            )
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
    }
}
