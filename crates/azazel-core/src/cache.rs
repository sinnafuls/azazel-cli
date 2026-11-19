//! The provider's prompt cache, seen from here: how much of each request's input was read from
//! it, which part of a request changed when one missed it, and keeping it warm while a session
//! waits for its user.
//!
//! A provider caches a request as a prefix: the tools, then the system prompt, then the
//! conversation. The next request reads the cache as far as it repeats the previous one byte for
//! byte, so one changed part early in a request costs everything after it.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use azazel_agent::{AgentInfo, Services};
use azazel_ai::{
    AiError, Message, ModelRef, ModelSpec, Request, Router, StreamEvent, ThinkingLevel, Usage, WireApi,
};
use futures::StreamExt;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::{CacheConfig, CacheWarming};
use crate::ext::Level;
use crate::side::SideCosts;
use crate::ui::Ui;

/// How long a provider keeps a cached prefix that nothing reads.
pub const CACHE_TTL: Duration = Duration::from_secs(300);
/// How long a Claude subscription login has its prefix cached (see `wire::anthropic`).
pub const LOGIN_CACHE_TTL: Duration = Duration::from_secs(3600);

/// Whether a provider is used through a subscription login, by its id.
pub type LoginCheck = Arc<dyn Fn(&str) -> bool + Send + Sync>;
/// How long before the cache would expire it is read again.
const WARM_MARGIN: Duration = Duration::from_secs(30);
/// A prefix shorter than this is not worth a request to keep.
pub const MIN_WARM_PREFIX: u64 = 4096;
/// Misses kept for `/cost`.
const MAX_MISSES: usize = 8;
/// Stream events a warming request may produce before it is cut off: it asks for one token, and a
/// model that thinks first is not left to think.
const MAX_WARM_EVENTS: usize = 8;
/// Under which purpose the warming requests are counted in [`SideCosts`].
pub const WARMING_PURPOSE: &str = "cache warming";

/// Input tokens by where they came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheTotals {
    pub requests: u32,
    /// Tokens that were neither read from the cache nor written to it.
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl CacheTotals {
    fn add(&mut self, usage: &Usage) {
        self.requests += 1;
        self.input += usage.input;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
    }

    fn merge(&mut self, other: &CacheTotals) {
        self.requests += other.requests;
        self.input += other.input;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }

    /// The share of the input that was read from the cache, in percent; `None` when no input
    /// was counted.
    pub fn hit_rate(&self) -> Option<u64> {
        let total = self.input + self.cache_read + self.cache_write;
        (total > 0).then(|| self.cache_read * 100 / total)
    }
}

/// A request that read less from the cache than the one before it, though it repeated it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Miss {
    pub at: jiff::Timestamp,
    pub agent: String,
    pub model: String,
    /// Tokens read from the cache by this request and by the one before it.
    pub read: u64,
    pub read_before: u64,
    pub cause: String,
}

impl Miss {
    /// One line for the user.
    pub fn describe(&self) -> String {
        format!(
            "{} ({}) read {} tokens from the prompt cache where the request before read {}: {}",
            self.agent,
            self.model,
            tokens(self.read),
            tokens(self.read_before),
            self.cause
        )
    }
}

/// What one request looked like to the cache.
#[derive(Debug, Clone)]
struct Fingerprint {
    model: String,
    thinking: ThinkingLevel,
    /// Name and hash of each tool, in name order (the order they are sent in).
    tools: Vec<(String, u64)>,
    /// Name and hash of each system part, in the order they are sent in.
    system: Vec<(String, u64)>,
    messages: Vec<u64>,
    cache_read: u64,
    at: Instant,
}

fn hash_of(parts: &[&str]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for part in parts {
        part.hash(&mut hasher);
    }
    hasher.finish()
}

impl Fingerprint {
    fn of(request: &Request, message: &Message, at: Instant) -> Option<Fingerprint> {
        let meta = message.meta.as_ref()?;
        let mut tools: Vec<(String, u64)> = request
            .tools
            .iter()
            .map(|tool| {
                let schema = tool.schema.to_string();
                (tool.name.clone(), hash_of(&[&tool.description, &schema]))
            })
            .collect();
        tools.sort();
        let system = request
            .system
            .iter()
            .enumerate()
            .map(|(index, part)| {
                let name = match part.name.as_str() {
                    "" => format!("#{}", index + 1),
                    name => name.to_string(),
                };
                (name, hash_of(&[&part.text]))
            })
            .collect();
        let messages = request
            .messages
            .iter()
            .map(|message| {
                let content = serde_json::to_string(&message.content).unwrap_or_default();
                hash_of(&[&format!("{:?}", message.role), &content])
            })
            .collect();
        Some(Fingerprint {
            model: format!("{}/{}", meta.provider, meta.model),
            thinking: request.thinking,
            tools,
            system,
            messages,
            cache_read: meta.usage.cache_read,
            at,
        })
    }

    /// Why this request could not read what `before` left in the cache: the first thing, in the
    /// order a request is cached in, that is not what it was.
    fn cause(&self, before: &Fingerprint, ttl: Duration) -> String {
        if self.model != before.model {
            return format!(
                "the model changed from {} to {}, and each model has a cache of its own",
                before.model, self.model
            );
        }
        if self.tools != before.tools {
            let names =
                |tools: &[(String, u64)]| -> Vec<String> { tools.iter().map(|t| t.0.clone()).collect() };
            let (now, then) = (names(&self.tools), names(&before.tools));
            let added: Vec<&str> = now
                .iter()
                .filter(|name| !then.contains(name))
                .map(String::as_str)
                .collect();
            let removed: Vec<&str> = then
                .iter()
                .filter(|name| !now.contains(name))
                .map(String::as_str)
                .collect();
            let changed: Vec<&str> = self
                .tools
                .iter()
                .filter(|(name, hash)| before.tools.iter().any(|(n, h)| n == name && h != hash))
                .map(|(name, _)| name.as_str())
                .collect();
            let mut what = Vec::new();
            for (label, names) in [("added", added), ("removed", removed), ("changed", changed)] {
                if !names.is_empty() {
                    what.push(format!("{label} {}", listed(&names)));
                }
            }
            return format!("the tool list changed ({})", what.join("; "));
        }
        for (index, (name, hash)) in self.system.iter().enumerate() {
            match before.system.get(index) {
                Some((was, _)) if was != name => {
                    return match before.system.iter().any(|(n, _)| n == name) {
                        true => format!("the system prompt lost its part `{was}`"),
                        false => format!("the system prompt gained the part `{name}`"),
                    };
                }
                Some((_, was)) if was != hash => {
                    return format!("the system prompt part `{name}` changed");
                }
                Some(_) => {}
                None => return format!("the system prompt gained the part `{name}`"),
            }
        }
        if let Some((name, _)) = before.system.get(self.system.len()) {
            return format!("the system prompt lost its part `{name}`");
        }
        if self.thinking != before.thinking {
            return format!(
                "the thinking level changed from {} to {}",
                before.thinking, self.thinking
            );
        }
        let changed = before
            .messages
            .iter()
            .zip(&self.messages)
            .position(|(was, is)| was != is);
        if let Some(index) = changed {
            return format!(
                "message {} of {} in the conversation is not what was sent before (an old tool result was \
                 shortened, or a hook rewrote it)",
                index + 1,
                before.messages.len()
            );
        }
        let idle = self.at.saturating_duration_since(before.at);
        if idle >= ttl {
            return format!(
                "{} passed since the request before, and the cache is kept for {}",
                minutes(idle),
                minutes(ttl)
            );
        }
        "nothing that was sent changed, so the provider dropped it".to_string()
    }
}

fn listed(names: &[&str]) -> String {
    const SHOWN: usize = 4;
    let mut text = names
        .iter()
        .take(SHOWN)
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > SHOWN {
        text.push_str(&format!(" and {} more", names.len() - SHOWN));
    }
    text
}

fn minutes(time: Duration) -> String {
    match time.as_secs() {
        secs if secs < 90 => format!("{secs} s"),
        secs => format!("{} min", (secs + 30) / 60),
    }
}

/// `12.3k`, `4.5M`: token counts as the reports write them.
fn tokens(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{:.1}k", count as f64 / 1e3),
        _ => format!("{:.1}M", count as f64 / 1e6),
    }
}

/// The lead's last answered request, which a warming request repeats.
#[derive(Debug, Clone)]
struct WarmTarget {
    /// Counts the lead's answered requests, so a newer one is told from this one.
    seq: u64,
    /// The model that answered.
    model: ModelRef,
    /// The models the request was sent to: the one that answered is asked again as it is named
    /// here, so with the thinking level its entry gives it.
    chain: Vec<ModelSpec>,
    request: Arc<Request>,
    /// The cached prefix, in tokens.
    prefix: u64,
    /// When the request was answered.
    answered: Instant,
    /// When the cache was last read: `answered`, or the last warming request.
    touched: Instant,
    /// How long the provider keeps it.
    ttl: Duration,
}

/// The lead's cached prefix as its last answered request left it.
#[derive(Debug, Clone)]
struct LeadPrefix {
    model: ModelRef,
    prefix: u64,
    /// The model's API has a cache that a request keeps alive.
    renewable: bool,
    /// Counts the lead's answered requests, so a warming request for an older one is told apart.
    seq: u64,
    /// When the cache was last read: by the request, or by a warming request since.
    touched: Instant,
    /// How long the provider keeps it.
    ttl: Duration,
}

/// The lead's prompt cache at the moment the provider drops it for want of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expiry {
    pub model: ModelRef,
    /// The cached prefix, which the next request sends anew, in tokens.
    pub prefix: u64,
    /// When the provider drops it, unless something reads it before.
    pub at: Instant,
    /// How long the provider keeps a cache that nothing reads.
    pub ttl: Duration,
    /// Whether `cache.warming` can keep this cache alive: the model's API has a cache that a
    /// request renews.
    pub renewable: bool,
}

/// What warming has done in this session and what it will do next.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Warming {
    pub requests: u32,
    pub usage: Usage,
    /// When the next warming request goes out, and when warming ends unless the user comes back.
    pub next: Option<(jiff::Timestamp, jiff::Timestamp)>,
    /// Why the cache is not kept warm right now, when there is something to say.
    pub idle_reason: Option<String>,
}

#[derive(Default)]
struct State {
    session: String,
    models: BTreeMap<String, CacheTotals>,
    /// The last answered request of each agent.
    last: HashMap<String, Fingerprint>,
    misses: VecDeque<Miss>,
    seq: u64,
    target: Option<WarmTarget>,
    warming: Warming,
    /// The lead's cached prefix while no request of the lead is on its way; gone once its
    /// expiry has been told.
    lead: Option<LeadPrefix>,
    /// When the lead's request on its way was sent: the provider's clock for what it caches
    /// starts when it reads the request, not when the answer ends.
    lead_sent: Option<Instant>,
    /// The cache ran out while the session waited, and no request has been sent since.
    cold: bool,
    /// The answered request of the lead (`LeadPrefix::seq`) whose cache nearing its expiry was
    /// already pointed out.
    announced: u64,
}

impl State {
    /// Starts over when another session is open.
    fn enter(&mut self, session: &str) {
        if self.session != session {
            *self = State {
                session: session.to_string(),
                ..State::default()
            };
        }
    }
}

/// Everything `/cost` says about the cache.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CacheReport {
    pub models: Vec<(String, CacheTotals)>,
    pub misses: Vec<Miss>,
    /// `None` when warming is off.
    pub warming: Option<Warming>,
}

impl CacheReport {
    pub fn session(&self) -> CacheTotals {
        let mut all = CacheTotals::default();
        for (_, totals) in &self.models {
            all.merge(totals);
        }
        all
    }

    /// The lines for `/cost`; `None` when no request was counted and warming has nothing to say.
    pub fn render(&self) -> Option<String> {
        let mut lines = Vec::new();
        if let Some(rate) = self.session().hit_rate() {
            lines.push(format!(
                "cache: {rate}% of this session's input was read from the prompt cache"
            ));
            for (model, totals) in &self.models {
                let rate = totals
                    .hit_rate()
                    .map_or_else(|| "no input".to_string(), |rate| format!("{rate}%"));
                lines.push(format!(
                    "  {model}: {rate} · {} read · {} written · {} not cached · {} requests",
                    tokens(totals.cache_read),
                    tokens(totals.cache_write),
                    tokens(totals.input),
                    totals.requests
                ));
            }
        }
        if !self.misses.is_empty() {
            lines.push("cache misses:".to_string());
            for miss in &self.misses {
                lines.push(format!("  {} {}", clock(miss.at), miss.describe()));
            }
        }
        if let Some(warming) = &self.warming {
            let mut line = match warming.requests {
                0 => "cache warming: on, no request sent yet".to_string(),
                requests => {
                    let cost = match warming.usage.cost_usd {
                        Some(cost) => format!(" · ${cost:.4}"),
                        None => String::new(),
                    };
                    format!(
                        "cache warming: {requests} requests · {} read{cost}",
                        tokens(warming.usage.cache_read)
                    )
                }
            };
            match (&warming.next, &warming.idle_reason) {
                (Some((next, until)), _) => {
                    line.push_str(&format!(" · next at {}, until {}", clock(*next), clock(*until)));
                }
                (None, Some(reason)) => line.push_str(&format!(" · {reason}")),
                (None, None) => {}
            }
            lines.push(line);
        }
        (!lines.is_empty()).then(|| lines.join("\n"))
    }
}

/// The local time of day, `14:05`.
fn clock(at: jiff::Timestamp) -> String {
    at.to_zoned(jiff::tz::TimeZone::system())
        .strftime("%H:%M")
        .to_string()
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the prompt cache did for the requests of this session; a service.
pub struct CacheLedger {
    warm: bool,
    warm_for: Duration,
    report_misses: bool,
    /// Which providers are used through a login, whose cache lasts longer.
    logins: Option<LoginCheck>,
    state: Mutex<State>,
    /// Woken whenever the request to keep warm changes.
    changed: Notify,
}

impl CacheLedger {
    pub fn new(config: &CacheConfig) -> Self {
        CacheLedger {
            warm: config.warming == CacheWarming::Idle,
            warm_for: Duration::from_secs(u64::from(config.warm_minutes) * 60),
            report_misses: config.report_misses,
            logins: None,
            state: Mutex::default(),
            changed: Notify::new(),
        }
    }

    /// Tells the ledger which providers are used through a login: a Claude subscription has its
    /// prompt cached for an hour.
    pub fn with_logins(mut self, logins: LoginCheck) -> Self {
        self.logins = Some(logins);
        self
    }

    /// How long the provider keeps what a request to `provider` over `api` cached: what its
    /// usage says when it says, else what a login or the API is known to give.
    fn ttl_of(&self, provider: &str, api: WireApi, usage: &Usage) -> Duration {
        if let Some(secs) = usage.cache_ttl_secs.filter(|secs| *secs > 0) {
            return Duration::from_secs(secs);
        }
        let login = self.logins.as_ref().is_some_and(|check| check(provider));
        match api {
            WireApi::AnthropicMessages if login => LOGIN_CACHE_TTL,
            _ => CACHE_TTL,
        }
    }

    /// Whether the cache is kept warm while the session is idle.
    pub fn warms(&self) -> bool {
        self.warm
    }

    /// A request of `agent` is on its way: whatever was being kept warm for it is about to be
    /// read by the request itself.
    pub fn sent(&self, agent: &AgentInfo) {
        if agent.depth != 0 {
            return;
        }
        let mut state = locked(&self.state);
        state.enter(&agent.session_id);
        state.lead = None;
        state.cold = false;
        state.lead_sent = Some(Instant::now());
        if state.target.take().is_some() {
            state.warming.next = None;
            drop(state);
            self.changed.notify_waiters();
        }
    }

    /// Counts an answered request. Returns the miss when it read less from the cache than the
    /// request before it although the conversation had not been cut down.
    pub fn answered(
        &self,
        agent: &AgentInfo,
        request: &Arc<Request>,
        chain: &[ModelSpec],
        message: &Message,
    ) -> Option<Miss> {
        let meta = message.meta.as_ref()?;
        let now = Instant::now();
        let print = Fingerprint::of(request, message, now)?;
        let ttl = self.ttl_of(&meta.provider, meta.api, &meta.usage);
        let mut state = locked(&self.state);
        state.enter(&agent.session_id);
        state
            .models
            .entry(print.model.clone())
            .or_default()
            .add(&meta.usage);

        let miss = state.last.get(&agent.id).and_then(|before| {
            let grew = print.messages.len() >= before.messages.len();
            (grew && print.cache_read < before.cache_read).then(|| Miss {
                at: jiff::Timestamp::now(),
                agent: agent.id.clone(),
                model: print.model.clone(),
                read: print.cache_read,
                read_before: before.cache_read,
                cause: print.cause(before, ttl),
            })
        });
        if let Some(miss) = &miss {
            if state.misses.len() >= MAX_MISSES {
                state.misses.pop_front();
            }
            state.misses.push_back(miss.clone());
        }
        state.last.insert(agent.id.clone(), print);

        if agent.depth == 0 {
            let prefix = meta.usage.cache_read + meta.usage.cache_write;
            state.seq += 1;
            // The clock starts when the provider read the request; an answer that took long
            // leaves less time than a full lifetime after it.
            let touched = state.lead_sent.take().map_or(now, |sent| sent.min(now));
            state.cold = false;
            // A short prefix costs little to send again: its expiry is not worth a word.
            state.lead = (prefix >= MIN_WARM_PREFIX).then(|| LeadPrefix {
                model: ModelRef::new(meta.provider.clone(), meta.model.clone()),
                prefix,
                renewable: meta.api == WireApi::AnthropicMessages,
                seq: state.seq,
                touched,
                ttl,
            });
        }
        if self.warm && agent.depth == 0 {
            let prefix = meta.usage.cache_read + meta.usage.cache_write;
            state.warming.next = None;
            state.warming.idle_reason = None;
            state.target = None;
            if meta.api != WireApi::AnthropicMessages {
                state.warming.idle_reason = Some(format!(
                    "{}/{} has no cache that a request can keep alive",
                    meta.provider, meta.model
                ));
            } else if prefix < MIN_WARM_PREFIX {
                state.warming.idle_reason = Some(format!(
                    "the cached prefix is {} tokens, under the {MIN_WARM_PREFIX} worth keeping",
                    tokens(prefix)
                ));
            } else {
                state.target = Some(WarmTarget {
                    seq: state.seq,
                    model: ModelRef::new(meta.provider.clone(), meta.model.clone()),
                    chain: chain.to_vec(),
                    request: request.clone(),
                    prefix,
                    answered: now,
                    touched: now,
                    ttl,
                });
            }
            drop(state);
            self.changed.notify_waiters();
        }
        miss
    }

    /// What `/cost` shows for the session `session`.
    pub fn report(&self, session: &str) -> CacheReport {
        let state = locked(&self.state);
        if state.session != session {
            return CacheReport {
                warming: self.warm.then(Warming::default),
                ..CacheReport::default()
            };
        }
        CacheReport {
            models: state
                .models
                .iter()
                .map(|(model, totals)| (model.clone(), *totals))
                .collect(),
            misses: state.misses.iter().cloned().collect(),
            warming: self.warm.then(|| state.warming.clone()),
        }
    }

    fn target(&self) -> Option<WarmTarget> {
        locked(&self.state).target.clone()
    }

    /// Records when the next warming request is due, as the user's clock shows it.
    fn plan(&self, seq: u64, due_in: Option<(Duration, Duration)>, reason: Option<&str>) {
        let mut state = locked(&self.state);
        if state.target.as_ref().is_none_or(|target| target.seq != seq) {
            return;
        }
        let stamp = |after: Duration| {
            jiff::Timestamp::now()
                .checked_add(jiff::SignedDuration::try_from(after).unwrap_or_default())
                .unwrap_or_else(|_| jiff::Timestamp::now())
        };
        state.warming.next = due_in.map(|(next, until)| (stamp(next), stamp(until)));
        state.warming.idle_reason = reason.map(str::to_string);
        if due_in.is_none() {
            state.target = None;
        }
    }

    /// Books a warming request that was answered, for the target it was sent for. The provider
    /// read the cache when the request arrived, so the clock restarts from `sent`, as it does for
    /// a request of the lead.
    fn warmed(&self, seq: u64, sent: Instant, usage: &Usage) {
        let mut state = locked(&self.state);
        state.warming.requests += 1;
        state.warming.usage.add(usage);
        if let Some(target) = state.target.as_mut().filter(|target| target.seq == seq) {
            target.touched = sent;
        }
        if let Some(lead) = state.lead.as_mut().filter(|lead| lead.seq == seq) {
            lead.touched = sent;
        }
    }

    /// When the provider drops the lead's cached prefix in the session `session`, unless a
    /// request reads it first. `None` while a request of the lead is on its way, when the
    /// prefix is too short to matter, and once [`CacheLedger::expired`] has told of it.
    pub fn expiry(&self, session: &str) -> Option<Expiry> {
        let state = locked(&self.state);
        let lead = state.lead.as_ref().filter(|_| state.session == session)?;
        Some(Expiry {
            model: lead.model.clone(),
            prefix: lead.prefix,
            at: lead.touched + lead.ttl,
            ttl: lead.ttl,
            renewable: lead.renewable,
        })
    }

    /// The lead's cached prefix, once, when the provider has dropped it: nothing read it for
    /// as long as a cache is kept.
    pub fn expired(&self, session: &str) -> Option<Expiry> {
        let expiry = self
            .expiry(session)
            .filter(|expiry| expiry.at <= Instant::now())?;
        let mut state = locked(&self.state);
        state.lead = None;
        state.cold = true;
        Some(expiry)
    }

    /// When the cache of the lead's last answered request is [`NEAR_EXPIRY`] from running out,
    /// unless that was pointed out already.
    pub fn near_expiry_at(&self, session: &str) -> Option<Instant> {
        let state = locked(&self.state);
        let lead = state
            .lead
            .as_ref()
            .filter(|lead| state.session == session && lead.seq != state.announced)?;
        Some(lead.touched + lead.ttl.saturating_sub(NEAR_EXPIRY.min(lead.ttl / 2)))
    }

    /// The warm cache, once per answered request, when it is [`NEAR_EXPIRY`] from running out.
    pub fn take_near_expiry(&self, session: &str) -> Option<CacheClock> {
        let at = self.near_expiry_at(session)?;
        if Instant::now() < at {
            return None;
        }
        // `at` stays in the past once it has come, so every way out of here that says nothing
        // must still count the request as announced, or the main loop would wake for it again
        // at once.
        let clock = self.clock(session);
        let mut state = locked(&self.state);
        match clock {
            Some(CacheClock::Warm { seq, .. }) => {
                state.announced = seq;
                clock
            }
            _ => {
                if let Some(lead) = state.lead.as_ref() {
                    state.announced = lead.seq;
                }
                None
            }
        }
    }

    /// The state of the lead's prompt cache in the session `session` as a clock shows it: time
    /// left while it is warm, `Cold` once it has run out and nothing has read it since. `None`
    /// while a request of the lead is on its way, and when there is no cache worth a word.
    pub fn clock(&self, session: &str) -> Option<CacheClock> {
        let state = locked(&self.state);
        if state.session != session {
            return None;
        }
        let Some(lead) = state.lead.as_ref() else {
            return state.cold.then_some(CacheClock::Cold);
        };
        let left = (lead.touched + lead.ttl).saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Some(CacheClock::Cold);
        }
        Some(CacheClock::Warm {
            left,
            ttl: lead.ttl,
            prefix: lead.prefix,
            seq: lead.seq,
        })
    }
}

/// How long before the cache runs out it is pointed out, for a word about using it while it is
/// cheap.
pub const NEAR_EXPIRY: Duration = Duration::from_secs(60);

/// Where the lead's prompt cache stands, for a clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheClock {
    Warm {
        /// Until the provider drops it, unless a request reads it first.
        left: Duration,
        /// How long the provider keeps it.
        ttl: Duration,
        /// The cached prefix, in tokens.
        prefix: u64,
        /// Which answered request of the lead left it, so a word about it is said once.
        seq: u64,
    },
    Cold,
}

impl CacheClock {
    /// `4:12` while there is time left, `cold` after. Whole seconds, rounded down, so the text
    /// changes once a second and not before.
    pub fn text(&self) -> String {
        match self {
            CacheClock::Warm { left, .. } => countdown(*left),
            CacheClock::Cold => "cold".to_string(),
        }
    }

    /// When the text of the clock changes next; `None` for a cache that is cold.
    pub fn next_change(&self) -> Option<Duration> {
        match self {
            CacheClock::Warm { left, .. } => Some(match left.subsec_nanos() {
                0 => Duration::from_secs(1),
                nanos => Duration::from_nanos(u64::from(nanos)),
            }),
            CacheClock::Cold => None,
        }
    }
}

/// `4:12`: minutes and seconds left, the minutes counted past the hour (`59:59`, `60:00`).
pub fn countdown(left: Duration) -> String {
    let secs = left.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Tells the user about a miss when `cache.report_misses` asks for it.
pub fn report_miss(ledger: &CacheLedger, services: &Services, miss: &Miss) {
    tracing::debug!("prompt cache miss: {}", miss.describe());
    if ledger.report_misses
        && let Some(ui) = services.get::<Ui>()
    {
        ui.0.notify(Level::Info, &format!("cache miss: {}", miss.describe()));
    }
}

/// Keeps the lead's cached prefix alive while the session is idle: shortly before the provider
/// would drop it, the lead's last request is sent again asking for one token, which reads the
/// cache and so renews it. Stops `cache.warm_minutes` after the lead's last own request.
pub struct Warmer {
    pub ledger: Arc<CacheLedger>,
    pub router: Arc<Router>,
    /// Where the warming requests are counted, so the run's spend and `/cost` include them.
    pub costs: Arc<SideCosts>,
    pub services: Arc<Services>,
}

impl Warmer {
    /// Runs until the process ends.
    pub async fn run(self) {
        let ledger = &self.ledger;
        loop {
            let changed = ledger.changed.notified();
            tokio::pin!(changed);
            // Registered before the state is read, so a change in between is not lost.
            changed.as_mut().enable();
            let Some(target) = ledger.target() else {
                changed.await;
                continue;
            };
            let every = target.ttl.saturating_sub(WARM_MARGIN.min(target.ttl / 10));
            let due = target.touched + every;
            let end = target.answered + ledger.warm_for;
            if due > end {
                ledger.plan(
                    target.seq,
                    None,
                    Some(&format!(
                        "stopped {} after the last request",
                        minutes(ledger.warm_for)
                    )),
                );
                continue;
            }
            let now = Instant::now();
            ledger.plan(
                target.seq,
                Some((
                    due.saturating_duration_since(now),
                    end.saturating_duration_since(now),
                )),
                None,
            );
            tokio::select! {
                _ = &mut changed => continue,
                _ = tokio::time::sleep_until(due) => {}
            }
            if crate::limits::reached(&self.services) {
                ledger.plan(target.seq, None, Some("stopped: the cost limit is reached"));
                continue;
            }
            let sent = Instant::now();
            match self.warm(&target).await {
                Ok(usage) => {
                    self.costs.add(WARMING_PURPOSE, &usage);
                    ledger.warmed(target.seq, sent, &usage);
                }
                Err(error) => {
                    tracing::warn!("cache warming stopped: {error}");
                    ledger.plan(target.seq, None, Some(&format!("stopped: {error}")));
                }
            }
        }
    }

    /// Sends the target's request again for one token and returns what it cost.
    async fn warm(&self, target: &WarmTarget) -> Result<Usage, AiError> {
        let request = Request {
            max_output: Some(1),
            origin: Some(azazel_ai::RequestOrigin::purpose("", WARMING_PURPOSE)),
            ..Request::clone(&target.request)
        };
        let cancel = CancellationToken::new();
        // The model that answered, as the lead's chain names it: a level set there is part of
        // what was cached.
        let model = self
            .router
            .resolve(&target.chain)
            .into_iter()
            .find(|model| model.key() == target.model.key())
            .unwrap_or_else(|| target.model.clone());
        let chain = [ModelSpec::Model(model)];
        let (_, mut events) = self.router.stream(&chain, &request, cancel.clone()).await?;
        let mut seen = 0;
        while let Some(event) = events.next().await {
            match event {
                StreamEvent::Done { message } => {
                    return Ok(message.meta.map(|meta| meta.usage).unwrap_or_default());
                }
                StreamEvent::Error { error, .. } => return Err(error),
                _ => {
                    seen += 1;
                    if seen > MAX_WARM_EVENTS {
                        break;
                    }
                }
            }
        }
        // Cut off: the cache was read when the request was accepted, but nothing was counted.
        // What it read is what the lead's request left there.
        cancel.cancel();
        let mut usage = Usage {
            cache_read: target.prefix,
            ..Usage::default()
        };
        if let Some(cost) = self.router.model_info(&target.model).cost {
            usage.price(&cost);
        }
        Ok(usage)
    }
}

#[cfg(test)]
mod tests {
    use azazel_ai::testing::{MockProvider, MockStep};
    use azazel_ai::{AssistantMeta, Block, Catalog, Provider, RetryPolicy, StopReason, SystemPart, ToolSpec};
    use serde_json::json;

    use super::*;

    fn agent(id: &str, depth: u32) -> AgentInfo {
        AgentInfo {
            id: id.into(),
            def: id.into(),
            parent: None,
            depth,
            session_id: "s1".into(),
        }
    }

    fn part(name: &str, text: &str) -> SystemPart {
        SystemPart {
            text: text.into(),
            name: name.into(),
            cache_boundary: false,
        }
    }

    fn tool(name: &str, description: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: description.into(),
            schema: json!({"type": "object"}),
            defer: false,
        }
    }

    fn request(turns: usize) -> Request {
        Request {
            system: vec![part("identity", "you are"), part("context_files", "AGENTS v1")],
            tools: vec![tool("read", "reads"), tool("write", "writes")],
            messages: (0..turns)
                .map(|n| Message::user(format!("message {n}")))
                .collect(),
            ..Request::default()
        }
    }

    fn reply(api: WireApi, model: &str, input: u64, read: u64, write: u64) -> Message {
        Message::assistant(
            vec![Block::text("ok")],
            AssistantMeta {
                provider: "mock".into(),
                model: model.into(),
                api,
                usage: Usage {
                    input,
                    cache_read: read,
                    cache_write: write,
                    ..Usage::default()
                },
                stop: StopReason::Stop,
                response_id: None,
                duration_ms: 0,
                sources: Vec::new(),
            },
        )
    }

    fn ledger(warming: CacheWarming) -> CacheLedger {
        CacheLedger::new(&CacheConfig {
            warming,
            ..CacheConfig::default()
        })
    }

    /// A fallback chain whose second model is the one that answers, at a level of its own.
    fn lead_chain() -> Vec<ModelSpec> {
        ["mock/first", "mock/lead:low"]
            .iter()
            .map(|model| ModelSpec::Model(model.parse().unwrap()))
            .collect()
    }

    fn answer(ledger: &CacheLedger, request: Request, read: u64, write: u64) -> Option<Miss> {
        ledger.answered(
            &agent("lead", 0),
            &Arc::new(request),
            &lead_chain(),
            &reply(WireApi::AnthropicMessages, "lead", 10, read, write),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn hit_rates_are_kept_per_model_and_for_the_session() {
        let ledger = ledger(CacheWarming::Off);
        assert_eq!(ledger.report("s1").render(), None);
        // The first request writes the cache, the next two read it.
        assert_eq!(answer(&ledger, request(1), 0, 9_000), None);
        assert_eq!(answer(&ledger, request(3), 9_000, 500), None);
        assert_eq!(answer(&ledger, request(5), 9_500, 500), None);
        let other = reply(WireApi::OpenAiChat, "smol", 1_000, 0, 0);
        assert_eq!(
            ledger.answered(&agent("helper", 1), &Arc::new(request(1)), &[], &other),
            None
        );

        let report = ledger.report("s1");
        let lead = CacheTotals {
            requests: 3,
            input: 30,
            cache_read: 18_500,
            cache_write: 10_000,
        };
        let smol = CacheTotals {
            requests: 1,
            input: 1_000,
            cache_read: 0,
            cache_write: 0,
        };
        assert_eq!(
            report.models,
            [("mock/lead".to_string(), lead), ("mock/smol".to_string(), smol)]
        );
        assert_eq!(lead.hit_rate(), Some(64));
        assert_eq!(smol.hit_rate(), Some(0));
        assert_eq!(report.session().hit_rate(), Some(62));
        assert_eq!(CacheTotals::default().hit_rate(), None);
        assert!(report.misses.is_empty());
        // Another session's requests are not this one's.
        assert_eq!(ledger.report("s2").models, []);
    }

    /// Answers `first` then `second` and returns the cause of the miss between them.
    async fn cause_between(first: Request, second: Request, pause: Duration) -> String {
        let ledger = ledger(CacheWarming::Off);
        assert_eq!(answer(&ledger, first, 20_000, 100), None);
        tokio::time::advance(pause).await;
        let miss = answer(&ledger, second, 2_000, 18_000).expect("a miss");
        assert_eq!((miss.read, miss.read_before), (2_000, 20_000));
        assert_eq!(ledger.report("s1").misses, std::slice::from_ref(&miss));
        miss.cause
    }

    #[tokio::test(start_paused = true)]
    async fn a_miss_names_the_first_part_of_the_request_that_changed() {
        let quick = Duration::ZERO;
        let grown = || request(3);

        let mut edited = grown();
        edited.system[1].text = "AGENTS v2".into();
        assert_eq!(
            cause_between(request(1), edited, quick).await,
            "the system prompt part `context_files` changed"
        );

        let mut gained = grown();
        gained.system.insert(1, part("mcp", "server notes"));
        assert_eq!(
            cause_between(request(1), gained, quick).await,
            "the system prompt gained the part `mcp`"
        );
        let mut lost = grown();
        lost.system.pop();
        assert_eq!(
            cause_between(request(1), lost, quick).await,
            "the system prompt lost its part `context_files`"
        );

        // The tools come before the system prompt in what is cached, so they are named first.
        let mut tools = grown();
        tools.system[1].text = "AGENTS v2".into();
        tools.tools.push(tool("mcp__db__query", "asks"));
        tools.tools[0].description = "reads files".into();
        tools.tools.remove(1);
        assert_eq!(
            cause_between(request(1), tools, quick).await,
            "the tool list changed (added `mcp__db__query`; removed `write`; changed `read`)"
        );
        // The order tools are registered in is not the order they are sent in.
        let mut reordered = grown();
        reordered.tools.reverse();
        reordered.thinking = ThinkingLevel::High;
        assert_eq!(
            cause_between(request(1), reordered, quick).await,
            "the thinking level changed from off to high"
        );

        let mut rewritten = grown();
        rewritten.messages[0] = Message::user("[elided]");
        assert_eq!(
            cause_between(request(2), rewritten, quick).await,
            "message 1 of 2 in the conversation is not what was sent before (an old tool result was \
             shortened, or a hook rewrote it)"
        );

        assert_eq!(
            cause_between(request(1), grown(), Duration::from_secs(7 * 60)).await,
            "7 min passed since the request before, and the cache is kept for 5 min"
        );
        assert_eq!(
            cause_between(request(1), grown(), Duration::from_secs(10)).await,
            "nothing that was sent changed, so the provider dropped it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_smaller_read_is_no_miss_after_compaction_or_for_another_agent_or_model() {
        let ledger = ledger(CacheWarming::Off);
        assert_eq!(answer(&ledger, request(9), 20_000, 100), None);
        // The conversation was summarised: less to read is what was asked for.
        assert_eq!(answer(&ledger, request(2), 1_000, 3_000), None);
        // A sub-agent's first request has nothing to compare with.
        let sub = reply(WireApi::AnthropicMessages, "lead", 10, 0, 500);
        assert_eq!(
            ledger.answered(&agent("helper", 1), &Arc::new(request(1)), &[], &sub),
            None
        );
        // The lead moves to another model: that is a miss, and it says why.
        let moved = reply(WireApi::AnthropicMessages, "other", 10, 0, 4_000);
        let miss = ledger
            .answered(&agent("lead", 0), &Arc::new(request(3)), &[], &moved)
            .unwrap();
        assert_eq!(
            miss.cause,
            "the model changed from mock/lead to mock/other, and each model has a cache of its own"
        );
        assert_eq!(
            miss.describe(),
            "lead (mock/other) read 0 tokens from the prompt cache where the request before read 1.0k: the \
             model changed from mock/lead to mock/other, and each model has a cache of its own"
        );
    }

    fn warmer(mock: Arc<MockProvider>, ledger: Arc<CacheLedger>) -> Warmer {
        let router = Router::new(
            vec![mock as Arc<dyn Provider>],
            Catalog::default(),
            BTreeMap::new(),
            RetryPolicy {
                max_attempts: 1,
                base_delay: Duration::ZERO,
                max_jitter: Duration::ZERO,
                ..RetryPolicy::default()
            },
        );
        Warmer {
            ledger,
            router: Arc::new(router),
            costs: Arc::new(SideCosts::default()),
            services: Arc::new(Services::new()),
        }
    }

    fn warm_reply(read: u64) -> MockStep {
        MockStep::text("o").with_usage(Usage {
            input: 3,
            output: 1,
            cache_read: read,
            ..Usage::default()
        })
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_session_is_kept_warm_until_the_limit_and_a_new_request_takes_over() {
        let mock = Arc::new(MockProvider::new());
        for _ in 0..8 {
            mock.push("lead", warm_reply(12_000));
        }
        let ledger = Arc::new(CacheLedger::new(&CacheConfig {
            warming: CacheWarming::Idle,
            warm_minutes: 12,
            report_misses: false,
            suggest_handoff_at: 0,
        }));
        let warmer = warmer(mock.clone(), ledger.clone());
        let costs = warmer.costs.clone();
        tokio::spawn(warmer.run());

        let mut sent = request(3);
        sent.thinking = ThinkingLevel::High;
        sent.tail = vec![part("now", "Date: today")];
        assert_eq!(answer(&ledger, sent.clone(), 11_000, 1_000), None);

        // Nothing goes out while the cache is still fresh.
        tokio::time::sleep(Duration::from_secs(4 * 60)).await;
        assert!(mock.requests().is_empty());
        let planned = ledger.report("s1").warming.unwrap();
        assert_eq!(planned.requests, 0);
        assert!(planned.next.is_some());

        // Half a minute before the five minutes are over the request goes out again, for one token.
        tokio::time::sleep(Duration::from_secs(45)).await;
        let requests = mock.requests_for("lead");
        assert_eq!(requests.len(), 1);
        // It is the lead's request, sent to the model that answered it at the level the chain
        // gives that model: anything else would not be what was cached.
        assert_eq!(
            requests[0],
            Request {
                max_output: Some(1),
                thinking: ThinkingLevel::Low,
                origin: Some(azazel_ai::RequestOrigin::purpose("", WARMING_PURPOSE)),
                ..sent.clone()
            }
        );
        assert!(mock.requests_for("first").is_empty());

        // A prompt from the user: the request itself reads the cache, and the clock starts again
        // from its answer.
        tokio::time::sleep(Duration::from_secs(60)).await;
        ledger.sent(&agent("lead", 0));
        assert_eq!(ledger.report("s1").warming.unwrap().next, None);
        tokio::time::sleep(Duration::from_secs(10 * 60)).await;
        assert_eq!(mock.requests().len(), 1);
        assert_eq!(answer(&ledger, request(5), 12_000, 400), None);

        // Twelve minutes of warming: at 4:30 and 9:00; the one at 13:30 would be past the limit.
        tokio::time::sleep(Duration::from_secs(30 * 60)).await;
        let requests = mock.requests_for("lead");
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].messages.len(), 5);
        let warming = ledger.report("s1").warming.unwrap();
        assert_eq!(warming.requests, 3);
        assert_eq!(warming.usage.cache_read, 36_000);
        assert_eq!(warming.next, None);
        assert_eq!(
            warming.idle_reason.as_deref(),
            Some("stopped 12 min after the last request")
        );
        assert_eq!(costs.by_purpose()[WARMING_PURPOSE].cache_read, 36_000);
        let text = ledger.report("s1").render().unwrap();
        assert!(
            text.ends_with("cache warming: 3 requests · 36.0k read · stopped 12 min after the last request"),
            "{text}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_leads_cache_expires_once_after_five_minutes_without_a_request() {
        let ledger = ledger(CacheWarming::Off);
        assert_eq!(ledger.expiry("s1"), None);
        let asked = Instant::now();
        assert_eq!(answer(&ledger, request(1), 20_000, 4_000), None);
        let expiry = ledger.expiry("s1").unwrap();
        assert_eq!(
            (
                expiry.model.to_string(),
                expiry.prefix,
                expiry.at,
                expiry.renewable
            ),
            ("mock/lead".to_string(), 24_000, asked + CACHE_TTL, true)
        );
        // Another session has no cache here, and a sub-agent's requests do not count.
        assert_eq!(ledger.expiry("s2"), None);
        tokio::time::sleep(Duration::from_secs(4 * 60)).await;
        let sub = reply(WireApi::AnthropicMessages, "lead", 10, 50_000, 0);
        ledger.answered(&agent("helper", 1), &Arc::new(request(1)), &[], &sub);
        assert_eq!(ledger.expiry("s1").unwrap().at, asked + CACHE_TTL);
        assert_eq!(ledger.expired("s1"), None);

        // Five minutes after the answer it is gone, which is told once.
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(ledger.expired("s1"), Some(expiry));
        assert_eq!((ledger.expired("s1"), ledger.expiry("s1")), (None, None));

        // A request on its way reads the cache itself: nothing expires meanwhile, and the
        // five minutes start again from its answer.
        assert_eq!(answer(&ledger, request(3), 24_000, 500), None);
        ledger.sent(&agent("lead", 0));
        tokio::time::sleep(Duration::from_secs(10 * 60)).await;
        assert_eq!(ledger.expired("s1"), None);
        assert_eq!(answer(&ledger, request(5), 24_500, 500), None);
        assert_eq!(ledger.expiry("s1").unwrap().prefix, 25_000);

        // A short prefix is not worth a word.
        answer(&ledger, request(6), 3_000, 1_000);
        assert_eq!(ledger.expiry("s1"), None);
        tokio::time::sleep(Duration::from_secs(10 * 60)).await;
        assert_eq!(ledger.expired("s1"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn the_lifetime_the_usage_reports_counts_from_when_the_request_was_sent() {
        let ledger = ledger(CacheWarming::Off);
        let sent = Instant::now();
        ledger.sent(&agent("lead", 0));
        // The answer takes a minute to arrive; the provider's clock started when it read the
        // request.
        tokio::time::sleep(Duration::from_secs(60)).await;
        let mut long = reply(WireApi::AnthropicMessages, "lead", 10, 20_000, 4_000);
        if let Some(meta) = long.meta.as_mut() {
            meta.usage.cache_ttl_secs = Some(3600);
        }
        ledger.answered(&agent("lead", 0), &Arc::new(request(1)), &lead_chain(), &long);
        let expiry = ledger.expiry("s1").unwrap();
        assert_eq!((expiry.at, expiry.ttl), (sent + LOGIN_CACHE_TTL, LOGIN_CACHE_TTL));

        // A usage that says nothing falls back to what the API is known to keep: five minutes.
        ledger.sent(&agent("lead", 0));
        let resent = Instant::now();
        assert_eq!(answer(&ledger, request(3), 24_000, 0), None);
        let expiry = ledger.expiry("s1").unwrap();
        assert_eq!((expiry.at, expiry.ttl), (resent + CACHE_TTL, CACHE_TTL));
    }

    #[test]
    fn the_countdown_counts_whole_seconds_down_and_ends_cold() {
        assert_eq!(countdown(Duration::from_secs(252)), "4:12");
        assert_eq!(countdown(Duration::from_millis(251_900)), "4:11");
        assert_eq!(countdown(Duration::from_secs(5)), "0:05");
        assert_eq!(countdown(Duration::from_secs(3600)), "60:00");
        let warm = |left| CacheClock::Warm {
            left,
            ttl: CACHE_TTL,
            prefix: 9_000,
            seq: 1,
        };
        assert_eq!(warm(Duration::from_secs(252)).text(), "4:12");
        assert_eq!(CacheClock::Cold.text(), "cold");
        // The text changes when the whole second does, and a cold clock never changes.
        assert_eq!(
            warm(Duration::from_millis(4_300)).next_change(),
            Some(Duration::from_millis(300))
        );
        assert_eq!(
            warm(Duration::from_secs(4)).next_change(),
            Some(Duration::from_secs(1))
        );
        assert_eq!(CacheClock::Cold.next_change(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn the_clock_runs_down_to_cold_and_starts_again_with_the_next_request() {
        let ledger = ledger(CacheWarming::Off);
        assert_eq!(ledger.clock("s1"), None);
        assert_eq!(answer(&ledger, request(1), 20_000, 4_000), None);
        let left = |ledger: &CacheLedger| match ledger.clock("s1") {
            Some(CacheClock::Warm { left, .. }) => left,
            other => panic!("not warm: {other:?}"),
        };
        assert_eq!(left(&ledger), CACHE_TTL);
        assert_eq!(ledger.clock("s2"), None);
        tokio::time::sleep(Duration::from_secs(48)).await;
        assert_eq!(
            ledger.clock("s1").map(|clock| clock.text()),
            Some("4:12".to_string())
        );
        // Past the lifetime it is cold without anybody having told of it, and stays cold once
        // it has been.
        tokio::time::sleep(Duration::from_secs(300)).await;
        assert_eq!(ledger.clock("s1"), Some(CacheClock::Cold));
        assert!(ledger.expired("s1").is_some());
        assert_eq!(ledger.clock("s1"), Some(CacheClock::Cold));
        // A request on its way reads the cache: no clock until it is answered.
        ledger.sent(&agent("lead", 0));
        assert_eq!(ledger.clock("s1"), None);
        assert_eq!(answer(&ledger, request(3), 24_000, 500), None);
        assert_eq!(left(&ledger), CACHE_TTL);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cache_nearing_its_expiry_is_pointed_out_once_per_request() {
        let ledger = ledger(CacheWarming::Off);
        assert_eq!(ledger.near_expiry_at("s1"), None);
        let asked = Instant::now();
        assert_eq!(answer(&ledger, request(1), 20_000, 4_000), None);
        assert_eq!(ledger.near_expiry_at("s1"), Some(asked + CACHE_TTL - NEAR_EXPIRY));
        tokio::time::sleep(Duration::from_secs(200)).await;
        assert_eq!(ledger.take_near_expiry("s1"), None);
        tokio::time::sleep(Duration::from_secs(45)).await;
        let clock = ledger.take_near_expiry("s1").unwrap();
        assert_eq!(clock.text(), "0:55");
        assert_eq!(ledger.take_near_expiry("s1"), None);
        assert_eq!(ledger.near_expiry_at("s1"), None);
        // The next answer starts a clock of its own.
        assert_eq!(answer(&ledger, request(3), 24_000, 500), None);
        assert!(ledger.near_expiry_at("s1").is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_cache_that_ran_out_before_it_was_pointed_out_is_not_asked_about_again() {
        let ledger = ledger(CacheWarming::Off);
        assert_eq!(answer(&ledger, request(1), 20_000, 4_000), None);
        // Nobody asked while it was near its expiry: when somebody does, it is cold and there is
        // nothing to say, and the moment must not stay in the past for the main loop to wake at.
        tokio::time::sleep(Duration::from_secs(400)).await;
        assert!(ledger.near_expiry_at("s1").is_some());
        assert_eq!(ledger.take_near_expiry("s1"), None);
        assert_eq!(ledger.near_expiry_at("s1"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cache_kept_warm_expires_when_the_warming_ends() {
        let mock = Arc::new(MockProvider::new());
        for _ in 0..4 {
            mock.push("lead", warm_reply(12_000));
        }
        let ledger = Arc::new(CacheLedger::new(&CacheConfig {
            warming: CacheWarming::Idle,
            warm_minutes: 12,
            report_misses: false,
            suggest_handoff_at: 0,
        }));
        tokio::spawn(warmer(mock.clone(), ledger.clone()).run());
        let asked = Instant::now();
        assert_eq!(answer(&ledger, request(3), 11_000, 1_000), None);

        // Warming requests go out at 4:30 and 9:00, and each renews the cache.
        tokio::time::sleep(Duration::from_secs(10 * 60)).await;
        assert_eq!(mock.requests_for("lead").len(), 2);
        assert_eq!(ledger.expired("s1"), None);
        let renewed = ledger.expiry("s1").unwrap().at;
        assert!(
            renewed > asked + Duration::from_secs(13 * 60),
            "{:?}",
            renewed - asked
        );

        // The warming ends twelve minutes after the request; five minutes after its last
        // request the cache is gone.
        tokio::time::sleep(Duration::from_secs(5 * 60)).await;
        assert_eq!(ledger.expired("s1").map(|expiry| expiry.at), Some(renewed));
    }

    #[tokio::test(start_paused = true)]
    async fn only_a_long_prefix_on_the_anthropic_api_is_kept_warm_and_only_for_the_lead() {
        let mock = Arc::new(MockProvider::new());
        let ledger = Arc::new(ledger(CacheWarming::Idle));
        tokio::spawn(warmer(mock.clone(), ledger.clone()).run());
        let wait = || tokio::time::sleep(Duration::from_secs(6 * 60));
        let reason = || ledger.report("s1").warming.unwrap().idle_reason.unwrap();

        // Too short to be worth a request.
        assert_eq!(answer(&ledger, request(1), 3_000, 1_000), None);
        wait().await;
        assert_eq!(
            reason(),
            "the cached prefix is 4.0k tokens, under the 4096 worth keeping"
        );
        // Another API: its cache is not one a request renews.
        let other = reply(WireApi::OpenAiChat, "lead", 10, 50_000, 0);
        ledger.answered(&agent("lead", 0), &Arc::new(request(3)), &[], &other);
        wait().await;
        assert_eq!(reason(), "mock/lead has no cache that a request can keep alive");
        // A sub-agent's requests are its own business.
        let sub = reply(WireApi::AnthropicMessages, "lead", 10, 50_000, 0);
        ledger.answered(&agent("helper", 1), &Arc::new(request(3)), &[], &sub);
        wait().await;
        assert!(mock.requests().is_empty());

        // A warming request that fails ends the warming instead of trying for ever.
        mock.push("lead", MockStep::fail(AiError::Overloaded));
        assert_eq!(answer(&ledger, request(5), 50_000, 0), None);
        wait().await;
        wait().await;
        assert_eq!(mock.requests().len(), 1);
        assert!(reason().starts_with("stopped: "), "{}", reason());
    }

    #[tokio::test(start_paused = true)]
    async fn a_warming_request_that_keeps_talking_is_cut_off_and_counted_as_a_read_of_the_prefix() {
        let mock = Arc::new(MockProvider::new());
        let chatter = (0..40)
            .map(|_| StreamEvent::ThinkingDelta {
                index: 0,
                delta: "hm".into(),
            })
            .collect();
        mock.push(
            "lead",
            MockStep {
                events: [vec![StreamEvent::Start], chatter].concat(),
                ..MockStep::default()
            },
        );
        let ledger = Arc::new(ledger(CacheWarming::Idle));
        tokio::spawn(warmer(mock.clone(), ledger.clone()).run());
        assert_eq!(answer(&ledger, request(3), 20_000, 1_000), None);
        tokio::time::sleep(Duration::from_secs(5 * 60)).await;
        let warming = ledger.report("s1").warming.unwrap();
        assert_eq!((warming.requests, warming.usage.cache_read), (1, 21_000));
        assert!(warming.next.is_some());
    }
}
