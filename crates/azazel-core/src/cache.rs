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
    ttl: Duration,
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
            ttl: CACHE_TTL,
            state: Mutex::default(),
            changed: Notify::new(),
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
                cause: print.cause(before, self.ttl),
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
            // A short prefix costs little to send again: its expiry is not worth a word.
            state.lead = (prefix >= MIN_WARM_PREFIX).then(|| LeadPrefix {
                model: ModelRef::new(meta.provider.clone(), meta.model.clone()),
                prefix,
                renewable: meta.api == WireApi::AnthropicMessages,
                seq: state.seq,
                touched: now,
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

    /// Books a warming request that was answered, for the target it was sent for.
    fn warmed(&self, seq: u64, usage: &Usage) {
        let mut state = locked(&self.state);
        state.warming.requests += 1;
        state.warming.usage.add(usage);
        let now = Instant::now();
        if let Some(target) = state.target.as_mut().filter(|target| target.seq == seq) {
            target.touched = now;
        }
        if let Some(lead) = state.lead.as_mut().filter(|lead| lead.seq == seq) {
            lead.touched = now;
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
            at: lead.touched + self.ttl,
            ttl: self.ttl,
            renewable: lead.renewable,
        })
    }

    /// The lead's cached prefix, once, when the provider has dropped it: nothing read it for
    /// as long as a cache is kept.
    pub fn expired(&self, session: &str) -> Option<Expiry> {
        let expiry = self
            .expiry(session)
            .filter(|expiry| expiry.at <= Instant::now())?;
        locked(&self.state).lead = None;
        Some(expiry)
    }
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
        let every = ledger.ttl.saturating_sub(WARM_MARGIN.min(ledger.ttl / 10));
        loop {
            let changed = ledger.changed.notified();
            tokio::pin!(changed);
            // Registered before the state is read, so a change in between is not lost.
            changed.as_mut().enable();
            let Some(target) = ledger.target() else {
                changed.await;
                continue;
            };
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
            match self.warm(&target).await {
                Ok(usage) => {
                    self.costs.add(WARMING_PURPOSE, &usage);
                    ledger.warmed(target.seq, &usage);
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
    async fn a_cache_kept_warm_expires_when_the_warming_ends() {
        let mock = Arc::new(MockProvider::new());
        for _ in 0..4 {
            mock.push("lead", warm_reply(12_000));
        }
        let ledger = Arc::new(CacheLedger::new(&CacheConfig {
            warming: CacheWarming::Idle,
            warm_minutes: 12,
            report_misses: false,
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
