//! Banked resets, and what happens when a subscription runs out in the middle of a run.
//!
//! When a request fails because an account is out of allowance and no other account of the
//! provider and no other model of the chain could take it, the agent asks here
//! ([`LimitFlow`]). With somebody at the keyboard the choice is theirs: spend a banked reset,
//! wait for the window to reset, or go on with another model. With nobody there the failure
//! stands, unless `usage.auto_reset` lets a banked reset be spent.
//!
//! A reset is spent through [`azazel_ai::providers::resets::Desk`], once, and what the
//! provider's usage report says afterwards settles it. `/reset-usage` spends one on purpose.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{AgentInfo, LimitHandler, OnLimit, Recovery, Services};
use azazel_ai::credentials::now_ms;
use azazel_ai::limits::time_left;
use azazel_ai::providers::resets::{self, Desk, Entry, Offers, Outcome, Pending, has_room};
use azazel_ai::providers::usage::{self, ResetOffer, UsageReport};
use azazel_ai::{AiError, LimitBlock, ModelRef, ModelSpec, ProviderConfig, Router, Window};
use azazel_core::{
    Command, CommandHandler, CommandOutcome, EntryKind, ExtCtx, ExtError, Extension, Level, Registrar,
    SessionStore, Ui, UiError,
};
use tokio_util::sync::CancellationToken;

use crate::accounts::clock;
use crate::usage::Source;

/// The title of the question a run asks when its account is out of allowance.
const TITLE: &str = "Usage limit reached";
const USE: &str = "Use the reset";
const AGAIN: &str = "Send the same reset request again";
const WAIT: &str = "Wait until reset";
const PICK: &str = "Pick another model";
/// An unattended run does not try a reset for an account more often than this.
const AUTO_EVERY_SECS: u64 = 5 * 60;
/// The wait looks this often whether the limit is gone before its time.
const LOOK_EVERY: Duration = Duration::from_secs(15);
/// Waited past the reset time, for the provider's clock.
const PAST_RESET_SECS: u64 = 1;

/// Where the journal of redemptions is kept.
pub fn journal_dir(paths: &azazel_core::Paths) -> PathBuf {
    paths.home.join("resets")
}

/// The runs that are waiting for a usage window to reset, for whoever shows it.
#[derive(Default)]
pub struct Waits {
    next: AtomicU64,
    /// Id, what is waited for, and Unix seconds at which it is over.
    waiting: Mutex<Vec<(u64, String, u64)>>,
}

impl Waits {
    /// What the run that has waited longest is waiting for, and until when.
    pub fn current(&self) -> Option<(String, u64)> {
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .first()
            .map(|(_, what, until)| (what.clone(), *until))
    }

    fn begin(self: &Arc<Self>, what: String, until: u64) -> Waiting {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id, what, until));
        Waiting {
            waits: self.clone(),
            id,
        }
    }
}

/// One wait; it is over when this is dropped.
struct Waiting {
    waits: Arc<Waits>,
    id: u64,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.waits
            .waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(id, ..)| *id != self.id);
    }
}

/// One offer as a row of the usage views: `2 weekly resets · first lapses Tue 09:00`.
pub fn offer_text(offer: &ResetOffer, now: u64) -> Option<String> {
    let (one, many) = match offer.kind.as_str() {
        resets::SESSION => ("session reset", "session resets"),
        resets::WEEKLY => ("weekly reset", "weekly resets"),
        resets::CREDITS => ("banked reset", "banked resets"),
        _ => ("reset", "resets"),
    };
    if !offer.eligible {
        return Some(format!("{many}: not offered to this account"));
    }
    let next = offer.next_available_at.filter(|at| *at > now);
    let mut text = match (offer.available, next) {
        (0, Some(at)) => return Some(format!("{one} used; the next one comes {}", clock(at))),
        (0, None) if offer.kind == resets::SESSION => return Some(format!("{one} used")),
        (0, None) => return None,
        (1, _) => format!("1 {one}"),
        (count, _) => format!("{count} {many}"),
    };
    match (offer.expires_at.filter(|at| *at > now), offer.available) {
        (Some(at), 1) => text.push_str(&format!(" · lapses {}", clock(at))),
        (Some(at), _) => text.push_str(&format!(" · first lapses {}", clock(at))),
        (None, _) => {}
    }
    match (offer.usable, next) {
        (Some(false), Some(at)) => text.push_str(&format!(" · usable again {}", clock(at))),
        (Some(false), None) => text.push_str(" · usable once the window it gives back is full"),
        _ => {}
    }
    Some(text)
}

/// A limit a run ran into: the account that is out of allowance, and for which model.
struct Hit {
    provider: ProviderConfig,
    account: String,
    model: ModelRef,
    block: LimitBlock,
}

impl Hit {
    fn key(&self) -> String {
        format!(
            "{}#{}#{}",
            self.provider.id,
            self.account,
            self.block.scope.as_deref().unwrap_or_default()
        )
    }
}

/// How spending a reset ended, for whoever asked for it.
struct Spent {
    /// The windows are back, as far as the provider says: the request can go again.
    back: bool,
    level: Level,
    line: String,
}

/// What is known when the question is put.
struct Prepared {
    /// The windows that stand in the way, by name.
    full: Vec<String>,
    offers: Result<Offers, String>,
    /// The account's last redemption, when its outcome is still open.
    open: Option<Entry>,
}

/// Everything reading and spending resets needs.
#[derive(Clone)]
pub struct Resets {
    pub source: Source,
    /// `[usage] redact_email`.
    pub redact_email: bool,
}

impl Resets {
    fn desk(&self) -> &Desk {
        &self.source.desk
    }

    fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.source.providers.iter().find(|provider| provider.id == id)
    }

    /// The account as it is named to the user: its label, with the e-mail address of its
    /// login behind it.
    fn who(&self, provider: &str, account: &str) -> String {
        self.source
            .credentials
            .accounts(provider)
            .iter()
            .find(|stored| stored.label == account)
            .map(|stored| crate::usage::describe(stored, self.redact_email))
            .unwrap_or_else(|| account.to_string())
    }

    /// Asks the provider for the account's usage now and keeps the answer.
    async fn fresh(&self, provider: &ProviderConfig, account: &str) -> Option<UsageReport> {
        let client = azazel_ai::providers::http::http_client();
        match crate::usage::fetch(&self.source, &client, provider, account, true).await {
            Ok(report) => Some(report),
            Err(why) => {
                tracing::debug!("usage of {} ({account}) was not fetched: {why}", provider.id);
                None
            }
        }
    }

    /// Spends `pending` and says how it went. The redemption is sent once; the usage report
    /// fetched afterwards settles it in the journal when it shows room.
    async fn spend(&self, provider: &ProviderConfig, pending: &Pending) -> Spent {
        let (id, account) = (provider.id.as_str(), pending.account.as_str());
        let who = format!("{id}, account {}", self.who(id, account));
        let outcome = self.desk().redeem(pending).await;
        let limits = self.source.credentials.limits();
        let sent = !matches!(
            outcome,
            Outcome::Refused(_) | Outcome::NotSpent(_) | Outcome::Open(_)
        );
        let room = match sent {
            true => {
                // What the account was known to hold is no longer true: the next look asks.
                usage::forget_resets(&self.source.cache_file, id, account);
                self.fresh(provider, account)
                    .await
                    .map(|report| has_room(&report, pending.clears.as_deref()))
            }
            false => None,
        };
        let done = |back: bool, level: Level, line: String| Spent { back, level, line };
        match outcome {
            Outcome::Reset { cleared, .. } => {
                limits.unblock(id, account);
                let gave: Vec<String> = cleared
                    .unwrap_or_default()
                    .iter()
                    .map(|name| Window::new(name.as_str(), 0.0, None, "").label())
                    .collect();
                let gave = match gave.is_empty() {
                    true => String::new(),
                    false => format!(" It gave back: {}.", gave.join(", ")),
                };
                done(
                    true,
                    Level::Info,
                    format!("{who}: one reset spent ({}).{gave}", pending.title),
                )
            }
            Outcome::NothingToReset => {
                limits.unblock(id, account);
                done(
                    true,
                    Level::Info,
                    format!("{who}: the provider says no window is full. No reset was spent."),
                )
            }
            Outcome::AlreadySpent { .. } if room == Some(true) => {
                limits.unblock(id, account);
                done(
                    true,
                    Level::Info,
                    format!(
                        "{who}: the provider says this reset was spent before, and the window has room again."
                    ),
                )
            }
            Outcome::AlreadySpent { next_available_at } => {
                let next = match next_available_at.filter(|at| *at > now_ms() / 1000) {
                    Some(at) => format!(" The next one comes {}.", clock(at)),
                    None => String::new(),
                };
                done(
                    false,
                    Level::Warn,
                    format!(
                        "{who}: the provider says this reset was spent before; nothing more was spent, and the \
                         limit is still in place.{next}"
                    ),
                )
            }
            Outcome::Uncertain(_) if room == Some(true) => {
                limits.unblock(id, account);
                done(
                    true,
                    Level::Warn,
                    format!(
                        "{who}: the reset request got no clear answer, but the usage report shows room again, \
                         so the reset counts as spent."
                    ),
                )
            }
            Outcome::Uncertain(why) => done(
                false,
                Level::Warn,
                format!(
                    "{who}: it is not known whether the reset was spent: {why}. It is not sent again unasked. \
                     /usage shows what the account has left; /reset-usage {id} sends the same request once \
                     more, which cannot spend a second reset."
                ),
            ),
            Outcome::NotSpent(why) | Outcome::Refused(why) => {
                done(false, Level::Error, format!("{who}: no reset was spent: {why}"))
            }
            Outcome::Open(entry) => done(
                false,
                Level::Warn,
                format!(
                    "{who}: no reset was spent: another reset of this account was sent {} and its outcome is \
                     not settled yet",
                    clock(entry.at)
                ),
            ),
        }
    }
}

/// Decides what happens when a run finds its account out of allowance.
pub struct LimitFlow {
    resets: Resets,
    services: Arc<Services>,
    /// `[usage] auto_reset`.
    auto_reset: bool,
    /// Nobody is at the keyboard of this process: a background session.
    unattended: bool,
    /// The only provider model names resolve to in this run (`--provider`).
    only: Option<String>,
    /// Every provider of the configuration, for the models that can be picked.
    providers: Vec<ProviderConfig>,
    waits: Arc<Waits>,
    /// One question at a time: the agents that hit the same limit together see one.
    turn: tokio::sync::Mutex<()>,
    /// The limits somebody chose to wait out, by [`Hit::key`], with their reset time.
    waited: Mutex<HashMap<String, u64>>,
}

impl LimitFlow {
    fn router(&self) -> Option<Arc<Router>> {
        self.services.get::<Router>()
    }

    /// The first model of `chain` whose provider is used through a login that is out of
    /// allowance for it.
    fn hit(&self, chain: &[ModelSpec]) -> Option<Hit> {
        let router = self.router()?;
        let credentials = &self.resets.source.credentials;
        let limits = credentials.limits();
        router.resolve(chain).into_iter().find_map(|model| {
            let provider = self.resets.provider(&model.provider)?;
            // A key from the environment or the configuration is no subscription.
            if !credentials.uses_store(&provider.id) {
                return None;
            }
            let account = credentials.selected(&provider.id)?;
            let login = credentials
                .accounts(&provider.id)
                .iter()
                .any(|stored| stored.label == account && stored.login);
            let block = limits
                .blocked_for(&provider.id, &account, &model.model)
                .filter(|_| login)?;
            Some(Hit {
                provider: provider.clone(),
                account,
                model,
                block,
            })
        })
    }

    fn still_blocked(&self, hit: &Hit) -> Option<LimitBlock> {
        self.resets
            .source
            .credentials
            .limits()
            .blocked_for(&hit.provider.id, &hit.account, &hit.model.model)
    }

    /// Reads what there is to know before anything is offered: the account's usage now, the
    /// resets it holds, and whether the journal still holds a redemption open. Nothing is
    /// spent by this.
    async fn prepare(&self, hit: &Hit) -> Prepared {
        let (id, account) = (hit.provider.id.as_str(), hit.account.as_str());
        let supported = resets::supported(&hit.provider);
        let report = match supported {
            true => self.resets.fresh(&hit.provider, account).await,
            false => None,
        };
        let mut full: Vec<String> = report
            .iter()
            .flat_map(|report| &report.windows)
            .filter(|window| window.name != "overage" && window.full() && window.counts(&hit.model.model))
            .map(|window| window.name.clone())
            .collect();
        if let Some(name) = &hit.block.window
            && !full.contains(name)
        {
            full.push(name.clone());
        }
        let offers = match supported {
            true => {
                self.resets
                    .desk()
                    .offers(&hit.provider, &self.resets.source.credentials, account, true)
                    .await
            }
            false => Err(format!("{id} has no banked resets")),
        };
        // The offers just read are what the usage views show from now on.
        if let (Ok(offers), Some(mut report)) = (&offers, report) {
            report.resets = offers.shown.clone();
            report.resets_asked_at = Some(now_ms() / 1000);
            usage::keep(&self.resets.source.cache_file, &report);
        }
        Prepared {
            full,
            offers,
            open: self.resets.desk().journal().open(id, account),
        }
    }

    /// The reset to offer for `hit`: one that gives back every window in the way.
    fn usable(prepared: &Prepared) -> Option<&Pending> {
        let offers = prepared.offers.as_ref().ok()?;
        // Without knowing which window is full there is no telling what a reset would do.
        if prepared.full.is_empty() {
            return None;
        }
        offers
            .spendable
            .iter()
            .find(|pending| pending.gives_back(&prepared.full))
    }

    fn notify(&self, level: Level, text: &str) {
        match self.services.get::<Ui>() {
            Some(ui) => ui.0.notify(level, text),
            None => tracing::info!("{text}"),
        }
    }

    /// The models a run can go on with: the roles, then the models of the providers that can
    /// be asked, without those the account that ran out would be asked for.
    fn models(&self, hit: &Hit) -> Vec<(ModelSpec, String)> {
        let Some(router) = self.router() else {
            return Vec::new();
        };
        let credentials = &self.resets.source.credentials;
        let limits = credentials.limits();
        // Whether the limit that was hit stands in the way of this model too.
        let out = |model: &ModelRef| {
            model.provider == hit.provider.id && !limits.open(&hit.provider.id, &hit.account, &model.model)
        };
        let mut rows = Vec::new();
        for role in router.roles().keys() {
            let resolved = router.resolve(&[ModelSpec::Role(role.clone())]);
            if let Some(first) = resolved.first().filter(|first| !out(first)) {
                rows.push((ModelSpec::Role(role.clone()), first.to_string()));
            }
        }
        for provider in router.provider_ids() {
            let (usable, login) = crate::models::standing(
                &router,
                &crate::models::Listed::default(),
                provider,
                &self.providers,
                credentials,
            );
            if !usable || self.only.as_deref().is_some_and(|only| only != provider) {
                continue;
            }
            for info in router.catalog().for_provider(provider) {
                let model = ModelRef::new(info.provider.as_str(), info.id.as_str());
                if out(&model) {
                    continue;
                }
                let detail = crate::settings::model_detail(info.context_window, info.cost.as_ref(), login);
                rows.push((ModelSpec::Model(model), detail));
            }
        }
        rows
    }

    /// "Pick another model": the chain the run goes on with, or `None` when nothing was
    /// picked.
    async fn pick(&self, ui: &Ui, agent: &AgentInfo, hit: &Hit) -> Option<Vec<ModelSpec>> {
        let rows = self.models(hit);
        if rows.is_empty() {
            ui.0.notify(Level::Warn, "there is no other model to go on with");
            return None;
        }
        let options: Vec<String> = rows
            .iter()
            .map(|(spec, detail)| format!("{spec}\t{detail}"))
            .collect();
        let prompt = format!(
            "Continue on which model?\n{} is out of allowance on {} until {}.",
            hit.model.model,
            hit.provider.id,
            clock(hit.block.until)
        );
        let index = ui.0.select(&prompt, &options).await.ok()?;
        let chain = vec![rows.get(index)?.0.clone()];
        // The lead's choice is the session's: a resumed session comes back on it.
        if agent.depth == 0
            && let Some(session) = self.services.get::<SessionStore>()
        {
            session.append(EntryKind::ModelChange { chain: chain.clone() });
        }
        Some(chain)
    }

    /// Waits until the limit of `hit` is over: its reset time has passed, or a usage report
    /// or another run found the account open again before that. False when the run was
    /// cancelled.
    async fn wait(&self, hit: &Hit, cancel: &CancellationToken) -> bool {
        let until = hit.block.until + PAST_RESET_SECS;
        let _shown = self.waits.begin(
            format!("waiting for the {} of {}", hit.block.limit(), hit.provider.id),
            hit.block.until,
        );
        loop {
            let now = now_ms() / 1000;
            if now >= until || self.still_blocked(hit).is_none() {
                return true;
            }
            let nap = LOOK_EVERY.min(Duration::from_secs(until - now));
            tokio::select! {
                _ = cancel.cancelled() => return false,
                _ = tokio::time::sleep(nap) => {}
            }
        }
    }

    /// With nobody to ask: a banked reset is spent when `usage.auto_reset` allows it, one
    /// gives back what is full, and nothing about an earlier one is open.
    async fn unattended(&self, hit: &Hit, cancel: &CancellationToken) -> Recovery {
        if !self.auto_reset || !resets::supported(&hit.provider) {
            return Recovery::Fail;
        }
        let _turn = tokio::select! {
            _ = cancel.cancelled() => return Recovery::Fail,
            turn = self.turn.lock() => turn,
        };
        // Another agent of this run got the allowance back while this one waited its turn.
        if self.still_blocked(hit).is_none() {
            return Recovery::Retry;
        }
        let (id, account) = (hit.provider.id.as_str(), hit.account.as_str());
        let now = now_ms() / 1000;
        if let Some(entry) = self.resets.desk().journal().entry(id, account)
            && (entry.open(now) || now.saturating_sub(entry.at) < AUTO_EVERY_SECS)
        {
            self.notify(
                Level::Warn,
                &format!(
                    "{id}, account {account}: no banked reset is spent automatically: one was sent {} and {}",
                    clock(entry.at),
                    match entry.open(now) {
                        true => "its outcome is not settled yet; /usage shows what the account has left",
                        false => "another is not tried so soon",
                    }
                ),
            );
            return Recovery::Fail;
        }
        let prepared = self.prepare(hit).await;
        // The usage report just fetched showed room: the limit is gone.
        if self.still_blocked(hit).is_none() {
            return Recovery::Retry;
        }
        // Read before the request went out, and never spent over an open redemption.
        let Some(pending) = Self::usable(&prepared).filter(|_| prepared.open.is_none()) else {
            return Recovery::Fail;
        };
        let spent = self.resets.spend(&hit.provider, pending).await;
        self.notify(spent.level, &spent.line);
        match spent.back {
            true => Recovery::Retry,
            false => Recovery::Fail,
        }
    }

    /// With somebody to ask: the question, and what follows from the answer.
    async fn attended(&self, ui: &Ui, agent: &AgentInfo, hit: &Hit, cancel: &CancellationToken) -> Recovery {
        let turn = tokio::select! {
            _ = cancel.cancelled() => return Recovery::Fail,
            turn = self.turn.lock() => turn,
        };
        let (id, account) = (hit.provider.id.as_str(), hit.account.as_str());
        loop {
            // Somebody answered for this limit while this agent waited its turn.
            let Some(block) = self.still_blocked(hit) else {
                return Recovery::Retry;
            };
            let joined = self
                .waited
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&hit.key())
                .is_some_and(|until| *until == block.until);
            if joined {
                drop(turn);
                return match self.wait(hit, cancel).await {
                    true => Recovery::Retry,
                    false => Recovery::Fail,
                };
            }
            let prepared = self.prepare(hit).await;
            if self.still_blocked(hit).is_none() {
                return Recovery::Retry;
            }
            let usable = Self::usable(&prepared);
            let uncertain = prepared.open.as_ref().filter(|entry| entry.uncertain());
            let mut text = format!(
                "{id}: {} reached until {}.",
                hit.block.limit(),
                clock(hit.block.until)
            );
            let mut options = Vec::new();
            match (uncertain, usable) {
                (Some(_), _) => options.push(AGAIN.to_string()),
                (None, Some(pending)) => {
                    text.push_str(&match pending.available {
                        1 => " You have 1 banked reset.".to_string(),
                        count => format!(" You have {count} banked resets."),
                    });
                    options.push(USE.to_string());
                }
                (None, None) => {}
            }
            options.push(WAIT.to_string());
            options.push(PICK.to_string());

            let mut context = vec![format!("Account {}.", self.resets.who(id, account))];
            if agent.depth > 0 {
                context.push(format!("Agent {} is the one that ran out.", agent.id));
            }
            match (&prepared.open, usable) {
                (Some(entry), _) if entry.uncertain() => context.push(format!(
                    "A reset of this account was sent {} and it is not known whether it was spent{}. Sending \
                     the same request again cannot spend a second one.",
                    clock(entry.at),
                    entry
                        .detail
                        .as_deref()
                        .map(|why| format!(" ({why})"))
                        .unwrap_or_default()
                )),
                (open, Some(pending)) => {
                    let mut about = pending.title.clone();
                    if let Some(description) = &pending.description {
                        about.push_str(&format!(": {description}"));
                    }
                    if let Some(at) = pending.expires_at {
                        about.push_str(&format!(" It lapses {} if it is not spent.", clock(at)));
                    }
                    context.push(about);
                    if let Some(entry) = open {
                        context.push(format!(
                            "A reset of this account was already spent {}, and the provider still reports the \
                             limit.",
                            clock(entry.at)
                        ));
                    }
                    context.push("Spending a reset cannot be undone.".to_string());
                }
                (_, None) => {
                    if let Err(why) = &prepared.offers
                        && resets::supported(&hit.provider)
                    {
                        context.push(format!("Banked resets could not be checked: {why}."));
                    }
                }
            }
            // The first line is the title of the box, the rest what it says.
            let prompt = format!("{TITLE}\n{text}\n\n{}", context.join("\n"));
            let answer = tokio::select! {
                _ = cancel.cancelled() => return Recovery::Fail,
                answer = ui.0.select(&prompt, &options) => answer,
            };
            let answer = match answer {
                Ok(index) => options.get(index).cloned().unwrap_or_default(),
                // Nobody is there after all.
                Err(UiError::NoUser) => {
                    drop(turn);
                    return self.unattended(hit, cancel).await;
                }
                // Dismissed: the failure stands, as it would without the question.
                Err(UiError::Cancelled) => return Recovery::Fail,
            };
            match answer.as_str() {
                USE | AGAIN => {
                    let pending = match (answer.as_str(), usable) {
                        (USE, Some(pending)) => {
                            // The user was told about the redemption that is still open.
                            let mut pending = pending.clone();
                            pending.over = prepared.open.as_ref().map(|entry| entry.request_id.clone());
                            Ok(pending)
                        }
                        _ => {
                            self.resets
                                .desk()
                                .again(&hit.provider, &self.resets.source.credentials, account)
                                .await
                        }
                    };
                    let spent = match pending {
                        Ok(pending) => self.resets.spend(&hit.provider, &pending).await,
                        Err(why) => Spent {
                            back: false,
                            level: Level::Error,
                            line: format!("{id}: no reset was sent: {why}"),
                        },
                    };
                    ui.0.notify(spent.level, &spent.line);
                    if spent.back {
                        return Recovery::Retry;
                    }
                    // The question comes again, with what is known now.
                }
                WAIT => {
                    self.waited
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(hit.key(), hit.block.until);
                    drop(turn);
                    let left = hit.block.until.saturating_sub(now_ms() / 1000);
                    ui.0.notify(
                        Level::Info,
                        &format!(
                            "waiting until {} ({}) for the {} of {id} to reset; interrupt the run to stop waiting",
                            clock(hit.block.until),
                            time_left(left),
                            hit.block.limit()
                        ),
                    );
                    let waited = self.wait(hit, cancel).await;
                    self.waited
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&hit.key());
                    return match waited {
                        true => Recovery::Retry,
                        false => Recovery::Fail,
                    };
                }
                PICK => {
                    if let Some(chain) = self.pick(ui, agent, hit).await {
                        let names: Vec<String> = chain.iter().map(ToString::to_string).collect();
                        ui.0.notify(
                            Level::Info,
                            &format!("{} goes on with {}", agent.id, names.join(", ")),
                        );
                        return Recovery::Model(chain);
                    }
                }
                _ => return Recovery::Fail,
            }
        }
    }
}

#[async_trait]
impl LimitHandler for LimitFlow {
    async fn out_of_allowance(
        &self,
        agent: &AgentInfo,
        chain: &[ModelSpec],
        _error: &AiError,
        cancel: &CancellationToken,
    ) -> Recovery {
        let Some(hit) = self.hit(chain) else {
            return Recovery::Fail;
        };
        // A run nobody watches is not asked, and nothing is read for a question.
        match self
            .services
            .get::<Ui>()
            .filter(|ui| ui.0.present() && !self.unattended)
        {
            Some(ui) => self.attended(&ui, agent, &hit, cancel).await,
            None => self.unattended(&hit, cancel).await,
        }
    }
}

/// `/reset-usage [provider]`: spends one banked reset on purpose.
struct ResetCommand {
    resets: Resets,
}

impl ResetCommand {
    /// The provider to spend a reset of: the one named, the only one with a login that hands
    /// resets out, or the one the user picks.
    async fn provider(&self, named: &str, ui: &Ui) -> Result<Option<&ProviderConfig>, String> {
        let credentials = &self.resets.source.credentials;
        let with_login: Vec<&ProviderConfig> = self
            .resets
            .source
            .providers
            .iter()
            .filter(|provider| resets::supported(provider))
            .filter(|provider| {
                credentials
                    .accounts(&provider.id)
                    .iter()
                    .any(|account| account.login)
            })
            .collect();
        if !named.is_empty() {
            return match with_login.iter().find(|provider| provider.id == named) {
                Some(provider) => Ok(Some(*provider)),
                None => Err(format!(
                    "`{named}` has no subscription login with banked resets: they are handed out to Claude and \
                     ChatGPT subscriptions (anthropic, openai-codex)"
                )),
            };
        }
        match with_login.as_slice() {
            [] => Err(
                "no subscription is signed in: banked resets belong to Claude and ChatGPT logins (/login anthropic, \
                 /login openai-codex)"
                    .to_string(),
            ),
            [only] => Ok(Some(*only)),
            many => {
                let ids: Vec<String> = many.iter().map(|provider| provider.id.clone()).collect();
                match ui.0.select("Spend a reset of which provider?", &ids).await {
                    Ok(index) => Ok(many.get(index).copied()),
                    Err(_) => Ok(None),
                }
            }
        }
    }

    /// The login to spend from: the one in use, or the one the user picks when there are
    /// several.
    async fn account(&self, provider: &ProviderConfig, ui: &Ui) -> Option<String> {
        let credentials = &self.resets.source.credentials;
        let logins: Vec<_> = credentials
            .accounts(&provider.id)
            .into_iter()
            .filter(|account| account.login)
            .collect();
        if let [only] = logins.as_slice() {
            return Some(only.label.clone());
        }
        let in_use = credentials.selected(&provider.id);
        let rows: Vec<String> = logins
            .iter()
            .map(|account| {
                let who = crate::usage::describe(account, self.resets.redact_email);
                match in_use.as_deref() == Some(account.label.as_str()) {
                    true => format!("{who}\tin use"),
                    false => who,
                }
            })
            .collect();
        let prompt = format!("Spend a reset of which account of {}?", provider.id);
        let index = ui.0.select(&prompt, &rows).await.ok()?;
        logins.get(index).map(|account| account.label.clone())
    }

    async fn spend(&self, named: &str, ui: &Ui) -> Result<Option<String>, String> {
        let Some(provider) = self.provider(named, ui).await? else {
            return Ok(None);
        };
        let Some(account) = self.account(provider, ui).await else {
            return Ok(None);
        };
        let id = provider.id.as_str();
        let who = self.resets.who(id, &account);
        let credentials = &self.resets.source.credentials;
        let desk = self.resets.desk();
        let now = now_ms() / 1000;
        let open = desk.journal().open(id, &account);

        // A redemption whose outcome nobody knows comes first: the same one is sent again.
        if let Some(entry) = open.as_ref().filter(|entry| entry.uncertain()) {
            let prompt = format!(
                "Send the reset request again?\n{id} · account {who}\nA reset was sent {} and it is not known \
                 whether it was spent{}.\nThe same request is sent once more; it cannot spend a second reset.",
                clock(entry.at),
                entry
                    .detail
                    .as_deref()
                    .map(|why| format!(" ({why})"))
                    .unwrap_or_default()
            );
            let rows = ["Cancel".to_string(), AGAIN.to_string()];
            if ui.0.select(&prompt, &rows).await != Ok(1) {
                return Ok(None);
            }
            let pending = desk.again(provider, credentials, &account).await?;
            let spent = self.resets.spend(provider, &pending).await;
            return match spent.level {
                Level::Error => Err(spent.line),
                _ => Ok(Some(spent.line)),
            };
        }

        let offers = desk.offers(provider, credentials, &account, true).await?;
        if offers.spendable.is_empty() {
            let known: Vec<String> = offers
                .shown
                .iter()
                .filter_map(|offer| offer_text(offer, now))
                .collect();
            return Ok(Some(match known.is_empty() {
                true => format!("{id}, account {who}, holds no banked reset"),
                false => format!(
                    "{id}, account {who}, holds no reset that can be spent now: {}",
                    known.join("; ")
                ),
            }));
        }
        let mut pending = match offers.spendable.as_slice() {
            [only] => only.clone(),
            many => {
                let rows: Vec<String> = many
                    .iter()
                    .map(|pending| match &pending.description {
                        Some(description) => format!("{}\t{description}", pending.title),
                        None => pending.title.clone(),
                    })
                    .collect();
                let prompt = format!("Spend which reset?\n{id} · account {who}");
                match ui.0.select(&prompt, &rows).await {
                    Ok(index) => match many.get(index) {
                        Some(pending) => pending.clone(),
                        None => return Ok(None),
                    },
                    Err(_) => return Ok(None),
                }
            }
        };
        let mut prompt = format!("Spend a banked reset?\n{id} · account {who}\n{}", pending.title);
        if let Some(description) = &pending.description {
            prompt.push_str(&format!(": {description}"));
        }
        prompt.push_str(&match pending.available {
            1 => "\nIt is the only one the account holds.".to_string(),
            count => format!("\nThe account holds {count}."),
        });
        if let Some(at) = pending.expires_at {
            prompt.push_str(&format!(" It lapses {} if it is not spent.", clock(at)));
        }
        if let Some(entry) = &open {
            prompt.push_str(&format!(
                "\nA reset of this account was already spent {}, and no usage report has shown the room it \
                 made yet.",
                clock(entry.at)
            ));
        }
        prompt.push_str("\nSpending it cannot be undone.");
        // The row a stray Enter lands on spends nothing.
        let rows = ["Cancel".to_string(), "Spend the reset".to_string()];
        if ui.0.select(&prompt, &rows).await != Ok(1) {
            return Ok(None);
        }
        pending.over = open.map(|entry| entry.request_id);
        let spent = self.resets.spend(provider, &pending).await;
        match spent.level {
            Level::Error => Err(spent.line),
            _ => Ok(Some(spent.line)),
        }
    }
}

#[async_trait]
impl CommandHandler for ResetCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        let Some(ui) = ctx.services.get::<Ui>() else {
            return CommandOutcome::Error("/reset-usage needs somebody to confirm it".to_string());
        };
        match self.spend(args.trim(), &ui).await {
            Ok(Some(line)) => CommandOutcome::Show(line),
            Ok(None) => CommandOutcome::Done,
            Err(why) => CommandOutcome::Error(why),
        }
    }
}

/// Registers `/reset-usage` and what a run does when its account is out of allowance.
pub struct ResetsExt {
    pub resets: Resets,
    /// `[usage] auto_reset`.
    pub auto_reset: bool,
    /// This process is a background session: nobody is asked.
    pub unattended: bool,
    /// `--provider`.
    pub only: Option<String>,
    /// Every provider of the configuration.
    pub providers: Vec<ProviderConfig>,
}

impl Extension for ResetsExt {
    fn id(&self) -> &str {
        "resets"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        let waits = Arc::new(Waits::default());
        r.service(waits.clone());
        r.service(Arc::new(OnLimit(Arc::new(LimitFlow {
            resets: self.resets.clone(),
            services: r.services().clone(),
            auto_reset: self.auto_reset,
            unattended: self.unattended,
            only: self.only.clone(),
            providers: self.providers.clone(),
            waits,
            turn: tokio::sync::Mutex::new(()),
            waited: Mutex::default(),
        }))));
        r.command(Command {
            name: "reset-usage".to_string(),
            description:
                "Spend a banked reset of a Claude or ChatGPT subscription, after showing what it gives back"
                    .to_string(),
            argument_hint: Some("[provider]".to_string()),
            handler: Arc::new(ResetCommand {
                resets: self.resets.clone(),
            }),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::{BTreeMap, VecDeque};
    use std::sync::atomic::AtomicUsize;

    use azazel_ai::providers::resets::State;
    use azazel_ai::{Catalog, Credential, Credentials, RetryPolicy};
    use azazel_core::{ApprovalDecision, ApprovalRequest, Question, UiBridge};
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;

    const RESET_PATH: &str = "/api/organizations/org-1/reset_rate_limits";

    /// A user who answers from a list, and what they were shown.
    #[derive(Default)]
    struct Asked {
        answers: Mutex<VecDeque<Result<String, UiError>>>,
        /// Each question as its prompt and its options.
        questions: Mutex<Vec<(String, Vec<String>)>>,
        notes: Mutex<Vec<(Level, String)>>,
    }

    impl Asked {
        fn answering(answers: &[Result<&str, UiError>]) -> Arc<Asked> {
            Arc::new(Asked {
                answers: Mutex::new(
                    answers
                        .iter()
                        .map(|answer| answer.clone().map(str::to_string))
                        .collect(),
                ),
                ..Asked::default()
            })
        }

        fn notes(&self, level: Level) -> Vec<String> {
            self.notes
                .lock()
                .unwrap()
                .iter()
                .filter(|(of, _)| *of == level)
                .map(|(_, text)| text.clone())
                .collect()
        }
    }

    #[async_trait]
    impl UiBridge for Asked {
        async fn ask(&self, _: &Question) -> Result<Vec<String>, UiError> {
            Err(UiError::NoUser)
        }
        async fn confirm(&self, _: &str) -> Result<bool, UiError> {
            Err(UiError::NoUser)
        }
        async fn select(&self, prompt: &str, options: &[String]) -> Result<usize, UiError> {
            self.questions
                .lock()
                .unwrap()
                .push((prompt.to_string(), options.to_vec()));
            let answer = self.answers.lock().unwrap().pop_front();
            let answer = answer.unwrap_or(Err(UiError::Cancelled))?;
            Ok(options.iter().position(|option| *option == answer).unwrap())
        }
        async fn input(&self, _: &str) -> Result<String, UiError> {
            Err(UiError::NoUser)
        }
        fn notify(&self, level: Level, text: &str) {
            self.notes.lock().unwrap().push((level, text.to_string()));
        }
        async fn approval(&self, _: ApprovalRequest) -> Option<ApprovalDecision> {
            None
        }
    }

    /// A Claude subscription at `server` whose 5-hour window ran out a moment ago.
    struct Bench {
        dir: tempfile::TempDir,
        flow: LimitFlow,
        credentials: Arc<Credentials>,
    }

    impl Bench {
        async fn new(
            server: &MockServer,
            ui: Option<Arc<Asked>>,
            auto_reset: bool,
            unattended: bool,
        ) -> Bench {
            let dir = tempfile::tempdir().unwrap();
            let credentials = Arc::new(Credentials::with_env(
                dir.path().join("auth.json"),
                Arc::new(|_: &str| None),
            ));
            credentials
                .store(
                    "anthropic",
                    Credential::OAuth {
                        access: "access-token".into(),
                        refresh: "refresh-token".into(),
                        expires: now_ms() + 3_600_000,
                        extra: BTreeMap::from([
                            ("org_id".to_string(), json!("org-1")),
                            ("email".to_string(), json!("jane@example.com")),
                        ]),
                    },
                )
                .unwrap();
            let mut providers = crate::login::key_providers(&azazel_core::Config::default()).unwrap();
            for provider in &mut providers {
                if provider.id == "anthropic" {
                    provider.base_url = server.uri();
                    // A request was made with the stored login.
                    credentials.resolve_account(provider).await.unwrap();
                }
            }
            let model = ModelRef::new("anthropic", "claude-test");
            let roles = BTreeMap::from([("default".to_string(), vec![ModelSpec::Model(model)])]);
            let router = Arc::new(
                Router::new(
                    Vec::new(),
                    Catalog::load(None, &BTreeMap::new()),
                    roles,
                    RetryPolicy::default(),
                )
                .with_accounts(credentials.clone()),
            );
            let services = Arc::new(Services::new());
            services.insert(router);
            if let Some(ui) = ui {
                services.insert(Arc::new(Ui(ui)));
            }
            let flow = LimitFlow {
                resets: Resets {
                    source: Source {
                        providers: providers.clone(),
                        credentials: credentials.clone(),
                        cache_file: dir.path().join("cache").join("limits.json"),
                        desk: Desk::new(dir.path().join("resets"))
                            .with_spend_timeout(Duration::from_millis(300)),
                    },
                    redact_email: true,
                },
                services,
                auto_reset,
                unattended,
                only: None,
                providers,
                waits: Arc::default(),
                turn: tokio::sync::Mutex::new(()),
                waited: Mutex::default(),
            };
            let bench = Bench {
                dir,
                flow,
                credentials,
            };
            bench.run_out();
            bench
        }

        /// The provider refused a request for the 5-hour window.
        fn run_out(&self) {
            let until = now_ms() / 1000 + 3 * 3600;
            let window = Window::new("5h", 1.0, Some(until), "rejected");
            let limits = self.credentials.limits();
            limits.record("anthropic", "default", vec![window.clone()]);
            limits.block_on("anthropic", "default", until, Some(&window));
        }

        fn blocked(&self) -> bool {
            self.credentials
                .limits()
                .blocked_for("anthropic", "default", "claude-test")
                .is_some()
        }

        async fn hit(&self) -> Recovery {
            let agent = AgentInfo {
                id: "lead".into(),
                def: "lead".into(),
                parent: None,
                depth: 0,
                session_id: "s".into(),
            };
            let error = AiError::Quota {
                resets_at: None,
                message: "the 5-hour limit of this account is used up".into(),
            };
            self.flow
                .out_of_allowance(
                    &agent,
                    &[ModelSpec::Role("default".into())],
                    &error,
                    &CancellationToken::new(),
                )
                .await
        }

        fn journal(&self) -> Option<Entry> {
            self.flow.resets.desk().journal().entry("anthropic", "default")
        }
    }

    /// The usage endpoint of an account that holds its session reset; `used` is what its
    /// 5-hour window shows each time it is asked.
    async fn usage_shows(server: &MockServer, used: impl Fn() -> f64 + Send + Sync + 'static) {
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(move |_: &Request| {
                ResponseTemplate::new(200).set_body_json(json!({
                    "five_hour": { "utilization": used(), "resets_at": "2099-01-01T00:00:00Z" },
                    "cedar_ember": null,
                    "juniper_tide": { "eligible": true, "arm": "reset", "available": true }
                }))
            })
            .mount(server)
            .await;
    }

    async fn redemptions(server: &MockServer) -> Vec<Value> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path() == RESET_PATH)
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_reset_whose_answer_is_lost_is_said_to_be_uncertain_and_only_the_same_one_goes_out_again() {
        let server = MockServer::start().await;
        usage_shows(&server, || 100.0).await;
        // The first redemption gets no answer in time; the provider had honoured it.
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        Mock::given(method("POST"))
            .and(path(RESET_PATH))
            .respond_with(move |_: &Request| match counted.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(200)
                    .set_body_json(json!({ "result": "reset" }))
                    .set_delay(Duration::from_secs(30)),
                _ => ResponseTemplate::new(200).set_body_json(json!({ "result": "already_used" })),
            })
            .mount(&server)
            .await;
        let user = Asked::answering(&[Ok(USE), Ok(AGAIN), Err(UiError::Cancelled)]);
        let bench = Bench::new(&server, Some(user.clone()), false, false).await;

        // The window never shows room, so nothing lets the run go on.
        assert_eq!(bench.hit().await, Recovery::Fail);

        let questions = user.questions.lock().unwrap().clone();
        let options: Vec<&[String]> = questions.iter().map(|(_, options)| options.as_slice()).collect();
        assert_eq!(
            options,
            [
                [USE, WAIT, PICK].map(str::to_string).as_slice(),
                // The outcome is open: what is offered is the same request, not another reset.
                [AGAIN, WAIT, PICK].map(str::to_string).as_slice(),
                [USE, WAIT, PICK].map(str::to_string).as_slice(),
            ]
        );
        let mut first = questions[0].0.lines();
        assert_eq!(first.next(), Some("Usage limit reached"));
        let said = first.next().unwrap();
        assert!(
            said.starts_with("anthropic: 5-hour limit reached until ")
                && said.ends_with(" You have 1 banked reset."),
            "{said}"
        );
        let told = questions[1].0.clone();
        assert!(told.contains("it is not known whether it was spent"), "{told}");
        assert!(told.contains("cannot spend a second one"), "{told}");
        // After the provider said it was spent, another one is offered with that said.
        let told = questions[2].0.clone();
        assert!(told.contains("was already spent"), "{told}");

        let warned = user.notes(Level::Warn);
        assert_eq!(warned.len(), 2, "{warned:?}");
        assert!(warned[0].contains("it is not known whether the reset was spent"));
        assert!(warned[0].contains("did not answer within"), "{}", warned[0]);
        assert!(warned[0].starts_with("anthropic, account default (j***@example.com): "));
        assert!(warned[1].contains("spent before"), "{}", warned[1]);
        // Two requests reached the provider, and they were the same redemption.
        assert_eq!(
            redemptions(&server).await,
            [
                json!({ "program": "juniper_tide" }),
                json!({ "program": "juniper_tide" })
            ]
        );
        assert_eq!(bench.journal().unwrap().state, State::Spent);
        assert!(bench.blocked());
    }

    #[tokio::test]
    async fn with_nobody_there_a_reset_is_spent_only_when_allowed_and_nothing_earlier_is_open() {
        // Not allowed: the provider is not even asked what the account holds.
        let server = MockServer::start().await;
        let bench = Bench::new(&server, None, false, true).await;
        assert_eq!(bench.hit().await, Recovery::Fail);
        assert!(server.received_requests().await.unwrap().is_empty());

        // Allowed, and a redemption with an unknown outcome is in the journal: none is sent.
        let server = MockServer::start().await;
        usage_shows(&server, || 100.0).await;
        let user = Asked::answering(&[]);
        let bench = Bench::new(&server, Some(user.clone()), true, true).await;
        let journal = bench.dir.path().join("resets");
        std::fs::create_dir_all(&journal).unwrap();
        std::fs::write(
            journal.join("anthropic.default.json"),
            json!({
                "provider": "anthropic", "account": "default", "kind": "session", "title": "Session reset",
                "body": { "program": "juniper_tide" }, "request_id": "r-1", "clears": ["5h"],
                "at": now_ms() / 1000 - 3600, "state": "uncertain"
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(bench.hit().await, Recovery::Fail);
        assert!(redemptions(&server).await.is_empty());
        let warned = user.notes(Level::Warn);
        assert!(warned[0].contains("its outcome is not settled yet"), "{warned:?}");
        // A background session asks nobody, whatever is there to ask.
        assert!(user.questions.lock().unwrap().is_empty());

        // Allowed and nothing open: one reset, and the run goes on.
        let server = MockServer::start().await;
        let spent = Arc::new(AtomicUsize::new(0));
        let shown = spent.clone();
        usage_shows(&server, move || match shown.load(Ordering::SeqCst) {
            0 => 100.0,
            _ => 2.0,
        })
        .await;
        let counted = spent.clone();
        Mock::given(method("POST"))
            .and(path(RESET_PATH))
            .respond_with(move |_: &Request| {
                counted.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({ "result": "reset" }))
            })
            .mount(&server)
            .await;
        let bench = Bench::new(&server, None, true, true).await;
        assert_eq!(bench.hit().await, Recovery::Retry);
        assert_eq!(redemptions(&server).await.len(), 1);
        assert!(!bench.blocked());
        assert_eq!(bench.journal().unwrap().state, State::Confirmed);

        // The limit is there again minutes later: a second reset is not tried so soon.
        spent.store(0, Ordering::SeqCst);
        bench.run_out();
        assert_eq!(bench.hit().await, Recovery::Fail);
        assert_eq!(redemptions(&server).await.len(), 1);
    }

    #[tokio::test]
    async fn a_limit_that_is_gone_when_looked_at_needs_no_answer() {
        // The usage report shows room: somebody reset the window elsewhere. The block is a
        // minute old, so the report is believed.
        let server = MockServer::start().await;
        usage_shows(&server, || 4.0).await;
        let user = Asked::answering(&[]);
        let bench = Bench::new(&server, Some(user.clone()), false, false).await;
        let limits = bench.credentials.limits();
        limits.unblock("anthropic", "default");
        let file = bench.dir.path().join("cache").join("limits.json");
        let until = now_ms() / 1000 + 3600;
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            json!({ "anthropic": { "provider": "anthropic", "account": "default", "windows": [], "fetched_at": 0,
                "blocks": [{ "until": until, "since": now_ms() / 1000 - 120, "window": "5h" }] } })
            .to_string(),
        )
        .unwrap();
        limits.persist(file);
        assert!(bench.blocked());

        assert_eq!(bench.hit().await, Recovery::Retry);
        assert!(user.questions.lock().unwrap().is_empty());
        assert!(redemptions(&server).await.is_empty());
    }

    #[tokio::test]
    async fn reset_usage_says_what_it_would_spend_and_spends_it_only_when_told_to() {
        let server = MockServer::start().await;
        let spent = Arc::new(AtomicUsize::new(0));
        let shown = spent.clone();
        usage_shows(&server, move || match shown.load(Ordering::SeqCst) {
            0 => 100.0,
            _ => 2.0,
        })
        .await;
        let counted = spent.clone();
        Mock::given(method("POST"))
            .and(path(RESET_PATH))
            .respond_with(move |_: &Request| {
                counted.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({ "result": "reset" }))
            })
            .mount(&server)
            .await;
        let user = Asked::answering(&[Ok("Cancel"), Ok("Spend the reset")]);
        let bench = Bench::new(&server, Some(user.clone()), false, false).await;
        let command = ResetCommand {
            resets: bench.flow.resets.clone(),
        };
        let ctx = ExtCtx {
            services: bench.flow.services.clone(),
            cwd: PathBuf::from("."),
            session_id: String::new(),
        };

        // Shown, and turned down: nothing is sent.
        assert!(matches!(
            command.run("anthropic", &ctx).await,
            CommandOutcome::Done
        ));
        assert!(redemptions(&server).await.is_empty());
        assert_eq!(bench.journal(), None);
        let (prompt, rows) = user.questions.lock().unwrap()[0].clone();
        // The row a stray enter lands on spends nothing.
        assert_eq!(rows, ["Cancel", "Spend the reset"]);
        assert!(
            prompt.starts_with("Spend a banked reset?\nanthropic · account default (j***@example.com)\n")
        );
        assert!(
            prompt.contains("Session reset: Gives the 5-hour window back now."),
            "{prompt}"
        );
        assert!(
            prompt.contains("It is the only one the account holds."),
            "{prompt}"
        );
        assert!(prompt.ends_with("Spending it cannot be undone."), "{prompt}");

        // The provider is the only one of this run with such a login, so it need not be named.
        let CommandOutcome::Show(line) = command.run("", &ctx).await else {
            panic!("the reset was not spent");
        };
        assert!(line.contains("one reset spent (Session reset)"), "{line}");
        assert_eq!(redemptions(&server).await, [json!({ "program": "juniper_tide" })]);
        assert_eq!(bench.journal().unwrap().state, State::Confirmed);
        assert!(!bench.blocked());

        // A provider without such a login is said to have none.
        let CommandOutcome::Error(why) = command.run("groq", &ctx).await else {
            panic!("groq has no resets");
        };
        assert!(why.contains("no subscription login with banked resets"), "{why}");
        assert_eq!(redemptions(&server).await.len(), 1);
    }

    #[test]
    fn offers_read_as_what_the_account_holds_and_when() {
        let now = 1_800_000_000;
        let held = |kind: &str, count: u32| ResetOffer::held(kind, count);
        assert_eq!(
            offer_text(&held(resets::CREDITS, 2), now).as_deref(),
            Some("2 banked resets")
        );
        assert_eq!(offer_text(&held(resets::CREDITS, 0), now), None);
        let lapsing = ResetOffer {
            expires_at: Some(now + 60),
            usable: Some(true),
            ..held(resets::SESSION, 1)
        };
        let text = offer_text(&lapsing, now).unwrap();
        assert!(text.starts_with("1 session reset · lapses "), "{text}");
        let later = ResetOffer {
            usable: Some(false),
            ..held(resets::WEEKLY, 3)
        };
        assert_eq!(
            offer_text(&later, now).as_deref(),
            Some("3 weekly resets · usable once the window it gives back is full")
        );
        let used = ResetOffer {
            next_available_at: Some(now + 60),
            usable: Some(false),
            ..held(resets::SESSION, 0)
        };
        assert!(
            offer_text(&used, now)
                .unwrap()
                .starts_with("session reset used; the next one comes ")
        );
        let outside = ResetOffer {
            eligible: false,
            ..held(resets::WEEKLY, 0)
        };
        assert_eq!(
            offer_text(&outside, now).as_deref(),
            Some("weekly resets: not offered to this account")
        );
    }
}
