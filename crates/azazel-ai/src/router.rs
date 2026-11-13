//! Resolves fallback chains and drives retries, cooldowns and fallback across providers.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::catalog::{Catalog, ModelInfo};
use crate::credentials::Credentials;
use crate::dispatch::{RouteReason, RouteRequest, VirtualModels, is_virtual};
use crate::error::AiError;
use crate::limits::LimitStore;
use crate::provider::Provider;
use crate::providers::http::blocked_until;
use crate::stream::EventStream;
use crate::types::{ModelRef, ModelSpec, Request, Role, Tier};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    /// Attempts per model before falling back.
    pub max_attempts: u32,
    /// How long a model that exhausted its attempts (or failed authentication) is skipped.
    pub cooldown: Duration,
    /// First retry delay; doubles per attempt, capped at 60 s.
    pub base_delay: Duration,
    pub max_jitter: Duration,
    /// How often one model may stall ([`AiError::Stalled`]) within a request before the router
    /// moves down the chain. A stall is not one of the `max_attempts`. 0 asks the stalled model
    /// again for as long as it stalls.
    pub max_stalls: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 3,
            cooldown: Duration::from_secs(300),
            base_delay: Duration::from_secs(1),
            max_jitter: Duration::from_millis(250),
            max_stalls: 3,
        }
    }
}

const MAX_DELAY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq)]
pub enum RouteEvent {
    /// The same model will be tried again after `delay`.
    Retry {
        attempt: u32,
        model: ModelRef,
        error: AiError,
        delay: Duration,
    },
    /// `from` is out for this request; `to` is next.
    Fallback {
        from: ModelRef,
        to: ModelRef,
        error: AiError,
    },
    /// Account `from` of `provider` is out of allowance until `until` (Unix seconds); the same
    /// model is asked again on account `to`.
    Account {
        provider: String,
        from: String,
        to: String,
        until: u64,
        /// What ran out, as a person calls it: `5-hour limit`, or `usage limit` when the
        /// provider did not say.
        limit: String,
    },
    /// `model` refused the service tier; it is asked again, and from now on, without one.
    TierDropped { model: ModelRef, tier: Tier },
}

/// Per-request routing state: failures counted so far and an optional event sink. Keep one for the
/// lifetime of a logical request so that failures reported after streaming began count as attempts
/// and a model that used up its attempts is not tried again for that request.
#[derive(Default)]
pub struct RouteState {
    failures: HashMap<String, u32>,
    /// How often each model stalled.
    stalls: HashMap<String, u32>,
    /// The pause before the next attempt, after a reply that failed while it was streamed.
    wait: Option<Duration>,
    /// The model most recently given up on, until the next candidate is announced.
    gave_up: Option<(ModelRef, AiError)>,
    /// The latest failure booked against a model, for a virtual model asked to route the retry.
    failed: Option<(ModelRef, AiError)>,
    /// The request comes from outside an agent's conversation.
    direct: bool,
    /// The virtual model the latest stream was dispatched by.
    via: Option<ModelRef>,
    events: Option<UnboundedSender<RouteEvent>>,
}

impl RouteState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_events(events: UnboundedSender<RouteEvent>) -> Self {
        RouteState {
            events: Some(events),
            ..Self::default()
        }
    }

    /// The virtual model that picked the model of the stream [`Router::stream_with`] returned
    /// last; `None` when the chain named that model itself.
    pub fn via(&self) -> Option<&ModelRef> {
        self.via.as_ref()
    }

    fn emit(&self, event: RouteEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }

    fn failures(&self, model: &ModelRef) -> u32 {
        self.failures.get(&model.key()).copied().unwrap_or(0)
    }

    /// Counts one failure; a failure that cannot be retried uses up all attempts. Returns the count.
    fn record(&mut self, model: &ModelRef, error: &AiError, max_attempts: u32) -> u32 {
        let failures = self.failures.entry(model.key()).or_insert(0);
        *failures = if error.is_retryable() {
            *failures + 1
        } else {
            max_attempts.max(*failures + 1)
        };
        *failures
    }

    /// Counts one stall of `model`. Returns the count.
    fn stalled(&mut self, model: &ModelRef) -> u32 {
        let stalls = self.stalls.entry(model.key()).or_insert(0);
        *stalls += 1;
        *stalls
    }

    /// `model` is not asked again for this request.
    fn give_up(&mut self, model: &ModelRef, max_attempts: u32) {
        let failures = self.failures.entry(model.key()).or_insert(0);
        *failures = max_attempts.max(*failures);
    }
}

pub struct Router {
    providers: HashMap<String, Arc<dyn Provider>>,
    catalog: Catalog,
    roles: RwLock<BTreeMap<String, Vec<ModelSpec>>>,
    policy: RetryPolicy,
    cooldowns: Mutex<HashMap<String, Instant>>,
    /// What each provider's model list said, asked once per process when the catalogue does not
    /// know a model (a subscription backend serves models no snapshot lists).
    listed: tokio::sync::Mutex<HashMap<String, Option<Arc<Vec<ModelInfo>>>>>,
    /// The stored accounts requests are made with; without it no account is ever switched.
    accounts: Option<Arc<Credentials>>,
    limits: Arc<LimitStore>,
    /// The service tier agents ask for on their own requests.
    tier: RwLock<Option<Tier>>,
    /// Models that refused a tier; they are asked without one for the rest of the process.
    tierless: Mutex<HashSet<String>>,
    /// The virtual models a chain may name; each picks the models of a request made with it.
    virtuals: Arc<VirtualModels>,
}

/// Whether `error` is a model saying it does not take the service tier of the request.
fn refuses_tier(error: &AiError) -> bool {
    let AiError::InvalidRequest(message) = error else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    ["service_tier", "service tier", "speed", "fast mode"]
        .iter()
        .any(|needle| message.contains(needle))
}

impl Router {
    pub fn new(
        providers: Vec<Arc<dyn Provider>>,
        catalog: Catalog,
        roles: BTreeMap<String, Vec<ModelSpec>>,
        policy: RetryPolicy,
    ) -> Self {
        Router {
            providers: providers.into_iter().map(|p| (p.id().to_string(), p)).collect(),
            catalog,
            roles: RwLock::new(roles),
            policy,
            cooldowns: Mutex::default(),
            listed: tokio::sync::Mutex::default(),
            accounts: None,
            limits: Arc::default(),
            tier: RwLock::default(),
            tierless: Mutex::default(),
            virtuals: Arc::default(),
        }
    }

    /// Lets the router move work to another stored account of a provider when the one in use
    /// runs out of allowance. The limit store becomes the one the credentials feed.
    pub fn with_accounts(mut self, credentials: Arc<Credentials>) -> Self {
        self.limits = credentials.limits().clone();
        self.accounts = Some(credentials);
        self
    }

    /// Routes `virtual/<name>` through the models registered in `virtuals`, now and later.
    pub fn with_virtual(mut self, virtuals: Arc<VirtualModels>) -> Self {
        self.virtuals = virtuals;
        self
    }

    /// Names of the registered virtual models, sorted; each is usable as `virtual/<name>`.
    pub fn virtual_models(&self) -> Vec<String> {
        self.virtuals.names()
    }

    /// The model whose limits (context window, prices) hold for a conversation on `chain`: its
    /// first model, or for a virtual one the model that answered last, when one has.
    pub fn limits_model(&self, chain: &[ModelSpec], answered: Option<&ModelRef>) -> Option<ModelRef> {
        let first = self.resolve(chain).into_iter().next()?;
        match answered {
            Some(answered) if is_virtual(&first) => Some(answered.clone()),
            _ => Some(first),
        }
    }

    /// What the providers reported about each account's usage windows, and which accounts are
    /// out of allowance.
    pub fn limits(&self) -> Arc<LimitStore> {
        self.limits.clone()
    }

    /// The service tier agents put on their requests; `None` is the provider's default.
    pub fn tier(&self) -> Option<Tier> {
        *self.tier.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets the tier for requests from now on. The standard tier is the same as none.
    pub fn set_tier(&self, tier: Option<Tier>) {
        *self.tier.write().unwrap_or_else(PoisonError::into_inner) = tier.filter(|t| *t != Tier::Standard);
    }

    /// Whether `model` refused a service tier earlier and is now asked without one.
    pub fn is_tierless(&self, model: &ModelRef) -> bool {
        self.tierless
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&model.key())
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The role table as it is now.
    pub fn roles(&self) -> BTreeMap<String, Vec<ModelSpec>> {
        self.roles.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Points `role` at `chain` for the rest of the process, so a default model chosen in a
    /// running session reaches every role and agent built on it.
    pub fn set_role(&self, role: &str, chain: Vec<ModelSpec>) {
        self.roles
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(role.to_string(), chain);
    }

    pub fn provider(&self, id: &str) -> Option<&Arc<dyn Provider>> {
        self.providers.get(id)
    }

    /// Whether the provider of `model` searches the web itself, so a request to it may carry
    /// [`crate::types::HostedTool::WebSearch`].
    pub fn hosts_web_search(&self, model: &ModelRef) -> bool {
        self.providers
            .get(&model.provider)
            .is_some_and(|provider| provider.hosts_web_search())
    }

    /// Provider ids, sorted.
    pub fn provider_ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self.providers.keys().map(String::as_str).collect();
        ids.sort_unstable();
        ids
    }

    pub fn model_info(&self, model: &ModelRef) -> ModelInfo {
        self.catalog.get(&model.provider, &model.model)
    }

    /// What is known about `model`, asking its provider for its model list once when the catalogue
    /// has no entry (what a subscription backend serves is authoritative). A provider
    /// that does not answer is not asked again this session.
    async fn model_info_discovered(&self, model: &ModelRef) -> ModelInfo {
        if self.catalog.contains(&model.provider, &model.model) {
            return self.catalog.get(&model.provider, &model.model);
        }
        let mut listed = self.listed.lock().await;
        let entry = listed
            .entry(model.provider.clone())
            .or_insert_with(|| None)
            .clone();
        let found = match entry {
            Some(found) => found,
            None => {
                let answer = match self.providers.get(&model.provider) {
                    Some(provider) => provider.list_models().await.ok().map(Arc::new),
                    None => None,
                };
                listed.insert(model.provider.clone(), answer.clone());
                answer.unwrap_or_default()
            }
        };
        found
            .iter()
            .find(|known| known.id == model.model && known.provider == model.provider)
            .cloned()
            .unwrap_or_else(|| self.catalog.get(&model.provider, &model.model))
    }

    /// Expands roles depth-first, keeps the first occurrence of each model, and skips unknown roles
    /// and roles that refer back to themselves.
    pub fn resolve(&self, chain: &[ModelSpec]) -> Vec<ModelRef> {
        let roles = self.roles();
        let mut out = Vec::new();
        let mut visiting = Vec::new();
        expand(&roles, chain, &mut visiting, &mut out);
        out
    }

    pub async fn stream(
        &self,
        chain: &[ModelSpec],
        req: &Request,
        cancel: CancellationToken,
    ) -> Result<(ModelRef, EventStream), AiError> {
        let mut state = RouteState {
            direct: true,
            ..RouteState::default()
        };
        self.stream_with(chain, req, cancel, &mut state).await
    }

    /// Like [`Router::stream`], continuing from the failures already recorded in `state`.
    pub async fn stream_with(
        &self,
        chain: &[ModelSpec],
        req: &Request,
        cancel: CancellationToken,
        state: &mut RouteState,
    ) -> Result<(ModelRef, EventStream), AiError> {
        let resolved = self.resolve(chain);
        if resolved.is_empty() {
            return Err(AiError::InvalidRequest("no model configured".to_string()));
        }
        state.via = None;
        if let Some(delay) = state.wait.take() {
            tokio::select! {
                _ = cancel.cancelled() => return Err(AiError::Aborted),
                _ = tokio::time::sleep(delay) => {}
            }
        }
        // A cancelled request does not wait for a virtual model to make up its mind.
        let (resolved, routes) = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AiError::Aborted),
            dispatched = self.dispatch(resolved, req, state) => dispatched?,
        };
        let open: Vec<&ModelRef> = resolved
            .iter()
            .filter(|m| state.failures(m) < self.policy.max_attempts)
            .collect();
        let ready: Vec<&ModelRef> = open.iter().copied().filter(|m| !self.in_cooldown(m)).collect();
        let candidates = if ready.is_empty() { open } else { ready };

        for model in candidates {
            if let Some((from, error)) = state.gave_up.take() {
                state.emit(RouteEvent::Fallback {
                    from,
                    to: model.clone(),
                    error,
                });
            }
            let error = match self.try_model(model, req, &cancel, state).await {
                Ok(stream) => {
                    state.via = routes
                        .iter()
                        .find(|(to, _)| to.provider == model.provider && to.model == model.model)
                        .map(|(_, via)| via.clone());
                    return Ok((model.clone(), stream));
                }
                Err(e @ (AiError::ContextOverflow | AiError::InvalidRequest(_) | AiError::Aborted)) => {
                    return Err(e);
                }
                Err(e) => e,
            };
            self.start_cooldown(model);
            state.gave_up = Some((model.clone(), error));
        }
        match state.gave_up.take() {
            Some((_, error)) => Err(error),
            None => Err(AiError::InvalidRequest(
                "every configured model has failed for this request".to_string(),
            )),
        }
    }

    /// Replaces every virtual model of `resolved` by the models it picks for `req`, in its
    /// place. The second list says which virtual model picked which model; it is empty for a
    /// chain without virtual models. A virtual model that is not registered, fails, or names
    /// nothing usable is left out; when nothing is left the request fails with its reason.
    async fn dispatch(
        &self,
        resolved: Vec<ModelRef>,
        req: &Request,
        state: &RouteState,
    ) -> Result<(Vec<ModelRef>, Vec<(ModelRef, ModelRef)>), AiError> {
        if !resolved.iter().any(is_virtual) {
            return Ok((resolved, Vec::new()));
        }
        let reason = if state.direct {
            RouteReason::Direct
        } else if state.failed.is_some() {
            RouteReason::Retry
        } else if req.messages.last().is_none_or(|last| last.role == Role::User) {
            RouteReason::User
        } else {
            RouteReason::Continuation
        };
        let previous = req.messages.iter().rev().find_map(|message| {
            let meta = message.meta.as_ref()?;
            Some(ModelRef::new(meta.provider.as_str(), meta.model.as_str()))
        });
        let mut out: Vec<ModelRef> = Vec::new();
        let mut routes = Vec::new();
        let mut problem = None;
        let known = |out: &[ModelRef], model: &ModelRef| {
            out.iter()
                .any(|m| m.provider == model.provider && m.model == model.model)
        };
        for model in resolved {
            if !is_virtual(&model) {
                if !known(&out, &model) {
                    out.push(model);
                }
                continue;
            }
            let picked = match self.virtuals.get(&model.model) {
                Some(route) => {
                    let request = RouteRequest {
                        name: &model.model,
                        reason,
                        thinking: model.thinking.unwrap_or(req.thinking),
                        previous: previous.clone(),
                        failed: state.failed.as_ref().map(|(model, error)| (model, error)),
                        request: req,
                    };
                    route.route(&request).await.and_then(|route| {
                        let named = self.resolve(&route.chain);
                        let nested = named.iter().any(is_virtual);
                        let picked: Vec<ModelRef> = named
                            .into_iter()
                            .filter(|picked| !is_virtual(picked))
                            .map(|mut picked| {
                                picked.thinking = picked.thinking.or(route.thinking);
                                picked
                            })
                            .collect();
                        match picked.is_empty() {
                            true if nested => {
                                Err("it named a virtual model, which cannot answer".to_string())
                            }
                            true => Err("it named no model that is configured".to_string()),
                            false => Ok(picked),
                        }
                    })
                }
                None => Err("no extension that is loaded defines it".to_string()),
            };
            match picked {
                Ok(picked) => {
                    for mut to in picked {
                        if known(&out, &to) {
                            continue;
                        }
                        // The level chosen with the virtual model holds unless the pick says otherwise.
                        to.thinking = to.thinking.or(model.thinking);
                        routes.push((to.clone(), model.clone()));
                        out.push(to);
                    }
                }
                Err(why) => {
                    tracing::warn!("{} picked no model: {why}", model.key());
                    problem.get_or_insert(format!("{}: {why}", model.key()));
                }
            }
        }
        match (out.is_empty(), problem) {
            (true, Some(problem)) => Err(AiError::InvalidRequest(problem)),
            _ => Ok((out, routes)),
        }
    }

    async fn try_model(
        &self,
        model: &ModelRef,
        req: &Request,
        cancel: &CancellationToken,
        state: &mut RouteState,
    ) -> Result<EventStream, AiError> {
        let Some(provider) = self.providers.get(&model.provider) else {
            let error = AiError::Auth(format!("unknown provider `{}`", model.provider));
            state.record(model, &error, self.policy.max_attempts);
            return Err(error);
        };
        let info = self.model_info_discovered(model).await;
        let thinking = model.thinking.unwrap_or(req.thinking);
        let tier = req.tier.filter(|_| !self.is_tierless(model));
        let mut req: Cow<'_, Request> = match thinking != req.thinking || tier != req.tier {
            true => Cow::Owned(Request {
                thinking,
                tier,
                ..req.clone()
            }),
            false => Cow::Borrowed(req),
        };
        loop {
            if cancel.is_cancelled() {
                return Err(AiError::Aborted);
            }
            // Read before the request: by the time it fails, another agent may have moved the
            // provider to a different account.
            let account = self
                .accounts
                .as_ref()
                .and_then(|accounts| accounts.selected(&model.provider));
            let error = match provider.stream(&info, &req, cancel.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(e) if req.tier.is_some() && refuses_tier(&e) => {
                    if let Some(tier) = req.to_mut().tier.take() {
                        self.tierless
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(model.key());
                        state.emit(RouteEvent::TierDropped {
                            model: model.clone(),
                            tier,
                        });
                    }
                    continue;
                }
                Err(e @ (AiError::ContextOverflow | AiError::InvalidRequest(_) | AiError::Aborted)) => {
                    return Err(e);
                }
                Err(e) => e,
            };
            if let Some(account) = account
                && self.next_account(&model.provider, &account, &error, state)
            {
                continue;
            }
            let Some(attempt) = self.next_attempt(model, &error, state) else {
                return Err(error);
            };
            let delay = self.delay(attempt, &error);
            state.emit(RouteEvent::Retry {
                attempt,
                model: model.clone(),
                error,
                delay,
            });
            tokio::select! {
                _ = cancel.cancelled() => return Err(AiError::Aborted),
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// After `error` on stored account `account` of `provider`: when the error means the account
    /// is out of allowance, marks it so and selects another account that is not. Returns whether
    /// the same model should be asked again at once. An account is switched for no other reason.
    fn next_account(&self, provider: &str, account: &str, error: &AiError, state: &RouteState) -> bool {
        let Some(accounts) = &self.accounts else {
            return false;
        };
        let Some(until) = blocked_until(error) else {
            return false;
        };
        // A key from the environment or the configuration answers the same whatever is selected.
        if !accounts.uses_store(provider) {
            return false;
        }
        self.limits.block(provider, account, until);
        let open = |label: &str| self.limits.blocked_until(provider, label).is_none();
        // Another agent's request ran into the same limit first and already moved on.
        if accounts
            .selected(provider)
            .is_some_and(|now| now != account && open(&now))
        {
            return true;
        }
        let Some(next) = accounts
            .accounts(provider)
            .into_iter()
            .map(|other| other.label)
            .find(|label| label != account && open(label))
        else {
            return false;
        };
        accounts.select(provider, &next);
        state.emit(RouteEvent::Account {
            provider: provider.to_string(),
            from: account.to_string(),
            to: next,
            until,
            limit: match self.limits.exhausted(provider, account) {
                Some(window) => window.label(),
                None => "usage limit".to_string(),
            },
        });
        true
    }

    /// Books `error` against `model`. Returns the number of the failed attempt when the same
    /// model is to be asked again, `None` when it is out for this request. A stall has its own
    /// count, `max_stalls`, and does not use up the model's attempts.
    fn next_attempt(&self, model: &ModelRef, error: &AiError, state: &mut RouteState) -> Option<u32> {
        state.failed = Some((model.clone(), error.clone()));
        if matches!(error, AiError::Stalled { .. }) {
            let stalls = state.stalled(model);
            if self.policy.max_stalls == 0 || stalls < self.policy.max_stalls {
                return Some(stalls);
            }
            state.give_up(model, self.policy.max_attempts);
            return None;
        }
        let attempt = state.record(model, error, self.policy.max_attempts);
        (attempt < self.policy.max_attempts).then_some(attempt)
    }

    /// Records a failure that arrived after streaming had begun; call [`Router::stream_with`] again
    /// with the same `state` to retry or fall back. Returns true when the same model will be retried.
    /// After a stall the next call waits out the backoff first.
    pub fn report_stream_failure(&self, model: &ModelRef, error: &AiError, state: &mut RouteState) -> bool {
        if let Some(account) = self
            .accounts
            .as_ref()
            .and_then(|accounts| accounts.selected(&model.provider))
            && self.next_account(&model.provider, &account, error, state)
        {
            return true;
        }
        if let Some(attempt) = self.next_attempt(model, error, state) {
            let delay = match error {
                AiError::Stalled { .. } => self.delay(attempt, error),
                _ => Duration::ZERO,
            };
            state.wait = (!delay.is_zero()).then_some(delay);
            state.emit(RouteEvent::Retry {
                attempt,
                model: model.clone(),
                error: error.clone(),
                delay,
            });
            return true;
        }
        self.start_cooldown(model);
        state.gave_up = Some((model.clone(), error.clone()));
        false
    }

    fn delay(&self, attempt: u32, error: &AiError) -> Duration {
        let backoff = match error {
            AiError::RateLimit { retry_after: Some(d) } => *d,
            _ => self
                .policy
                .base_delay
                .saturating_mul(1u32 << attempt.saturating_sub(1).min(16)),
        };
        backoff.min(MAX_DELAY) + jitter(self.policy.max_jitter)
    }

    fn in_cooldown(&self, model: &ModelRef) -> bool {
        let cooldowns = self.cooldowns.lock().unwrap_or_else(PoisonError::into_inner);
        cooldowns
            .get(&model.key())
            .is_some_and(|until| Instant::now() < *until)
    }

    fn start_cooldown(&self, model: &ModelRef) {
        let mut cooldowns = self.cooldowns.lock().unwrap_or_else(PoisonError::into_inner);
        cooldowns.insert(model.key(), Instant::now() + self.policy.cooldown);
    }
}

/// Spreads simultaneous retries; clock sub-second noise is random enough for that.
fn jitter(max: Duration) -> Duration {
    let max_nanos = max.as_nanos() as u64;
    if max_nanos == 0 {
        return Duration::ZERO;
    }
    let noise = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    Duration::from_nanos(noise.wrapping_mul(0x9E37_79B9_7F4A_7C15) % max_nanos)
}

fn expand<'a>(
    roles: &'a BTreeMap<String, Vec<ModelSpec>>,
    chain: &'a [ModelSpec],
    visiting: &mut Vec<&'a str>,
    out: &mut Vec<ModelRef>,
) {
    for spec in chain {
        match spec {
            ModelSpec::Model(model) => {
                if !out
                    .iter()
                    .any(|m| m.provider == model.provider && m.model == model.model)
                {
                    out.push(model.clone());
                }
            }
            ModelSpec::Role(role) => {
                if visiting.contains(&role.as_str()) {
                    tracing::warn!("role @{role} refers to itself; ignoring the cycle");
                    continue;
                }
                let Some(next) = roles.get(role) else {
                    tracing::debug!("role @{role} is not configured");
                    continue;
                };
                visiting.push(role);
                expand(roles, next, visiting, out);
                visiting.pop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;
    use crate::stream::StreamEvent;
    use crate::testing::{MockProvider, MockStep};
    use crate::types::ThinkingLevel;

    fn policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            cooldown: Duration::from_secs(300),
            base_delay: Duration::from_millis(1),
            max_jitter: Duration::ZERO,
            max_stalls: 3,
        }
    }

    fn chain(specs: &[&str]) -> Vec<ModelSpec> {
        specs.iter().map(|s| s.parse().unwrap()).collect()
    }

    fn router(mock: Arc<MockProvider>, roles: &[(&str, &[&str])]) -> Router {
        let roles = roles
            .iter()
            .map(|(name, specs)| (name.to_string(), chain(specs)))
            .collect();
        Router::new(vec![mock], Catalog::default(), roles, policy())
    }

    async fn final_text(stream: EventStream) -> String {
        let events: Vec<StreamEvent> = stream.collect().await;
        match events.last() {
            Some(StreamEvent::Done { message }) => message.text(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn resolve_expands_roles_deduplicates_and_survives_cycles() {
        let r = router(
            Arc::new(MockProvider::new()),
            &[
                ("default", &["mock/a", "mock/b"]),
                ("smol", &["mock/c", "@default", "@loop"]),
                ("loop", &["@smol", "mock/d"]),
            ],
        );
        let names: Vec<String> = r
            .resolve(&chain(&["@smol", "mock/a", "@missing"]))
            .iter()
            .map(ModelRef::to_string)
            .collect();
        assert_eq!(names, ["mock/c", "mock/a", "mock/b", "mock/d"]);
    }

    #[tokio::test]
    async fn falls_back_after_exhausting_attempts_and_skips_model_during_cooldown() {
        let mock = Arc::new(MockProvider::new());
        for _ in 0..3 {
            mock.push("a", MockStep::fail(AiError::Overloaded));
        }
        mock.push("b", MockStep::text("from b"));
        mock.push("b", MockStep::text("b again"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let r = router(mock.clone(), &[]);
        let c = chain(&["mock/a", "mock/b"]);

        let (model, stream) = r
            .stream_with(
                &c,
                &Request::default(),
                CancellationToken::new(),
                &mut RouteState::with_events(tx),
            )
            .await
            .unwrap();
        assert_eq!(model.to_string(), "mock/b");
        assert_eq!(final_text(stream).await, "from b");
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(match e {
                RouteEvent::Retry { attempt, .. } => format!("retry {attempt}"),
                RouteEvent::Fallback { from, to, .. } => format!("fallback {from} -> {to}"),
                other => format!("{other:?}"),
            });
        }
        assert_eq!(events, ["retry 1", "retry 2", "fallback mock/a -> mock/b"]);

        // Within the cooldown `a` is not contacted at all: its queue is empty and would error.
        let (model, stream) = r
            .stream(&c, &Request::default(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(model.to_string(), "mock/b");
        assert_eq!(final_text(stream).await, "b again");
        assert_eq!(mock.requests_for("a").len(), 3);
    }

    #[tokio::test]
    async fn auth_failure_moves_on_without_retrying() {
        let mock = Arc::new(MockProvider::new());
        mock.push("a", MockStep::fail(AiError::Auth("bad key".into())));
        mock.push("b", MockStep::text("ok"));
        let r = router(mock.clone(), &[]);
        let (model, _) = r
            .stream(
                &chain(&["mock/a", "mock/b"]),
                &Request::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(model.to_string(), "mock/b");
        assert_eq!(mock.requests_for("a").len(), 1);
    }

    #[tokio::test]
    async fn context_overflow_is_returned_at_once_without_fallback() {
        let mock = Arc::new(MockProvider::new());
        mock.push("a", MockStep::fail(AiError::ContextOverflow));
        mock.push("b", MockStep::text("never"));
        let r = router(mock.clone(), &[]);
        let result = r
            .stream(
                &chain(&["mock/a", "mock/b"]),
                &Request::default(),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(AiError::ContextOverflow)));
        assert!(mock.requests_for("b").is_empty());
    }

    #[tokio::test]
    async fn exhausted_chain_returns_last_error_and_all_cooling_models_are_still_tried() {
        let mock = Arc::new(MockProvider::new());
        for _ in 0..3 {
            mock.push("a", MockStep::fail(AiError::Network("reset".into())));
        }
        let r = router(mock.clone(), &[]);
        let c = chain(&["mock/a"]);
        let result = r.stream(&c, &Request::default(), CancellationToken::new()).await;
        assert!(matches!(result, Err(AiError::Network(_))));

        mock.push("a", MockStep::text("recovered"));
        let (_, stream) = r
            .stream(&c, &Request::default(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(final_text(stream).await, "recovered");
    }

    #[tokio::test]
    async fn stream_failures_use_up_attempts_then_the_request_falls_back() {
        let mock = Arc::new(MockProvider::new());
        mock.push("a", MockStep::text("a must not be asked again"));
        mock.push("b", MockStep::text("from b"));
        let r = router(mock.clone(), &[]);
        let model = ModelRef::new("mock", "a");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = RouteState::with_events(tx);
        assert!(r.report_stream_failure(&model, &AiError::Overloaded, &mut state));
        assert!(r.report_stream_failure(&model, &AiError::Overloaded, &mut state));
        assert!(!r.report_stream_failure(&model, &AiError::Overloaded, &mut state));
        assert!(r.in_cooldown(&model));

        // Only `a` in the chain: nothing left to try, the stream failure is the result.
        let alone = r
            .stream_with(
                &chain(&["mock/a"]),
                &Request::default(),
                CancellationToken::new(),
                &mut state,
            )
            .await;
        assert!(matches!(alone, Err(AiError::Overloaded)));

        r.report_stream_failure(&model, &AiError::Overloaded, &mut state);
        let (next, stream) = r
            .stream_with(
                &chain(&["mock/a", "mock/b"]),
                &Request::default(),
                CancellationToken::new(),
                &mut state,
            )
            .await
            .unwrap();
        assert_eq!(next.to_string(), "mock/b");
        assert_eq!(final_text(stream).await, "from b");
        assert!(mock.requests_for("a").is_empty());
        let mut saw_fallback = false;
        while let Ok(event) = rx.try_recv() {
            saw_fallback |= matches!(event, RouteEvent::Fallback { ref from, ref to, .. } if from == &model && to == &next);
        }
        assert!(saw_fallback);
    }

    #[tokio::test]
    async fn a_non_retryable_stream_failure_uses_up_the_model_at_once() {
        let r = router(Arc::new(MockProvider::new()), &[]);
        let model = ModelRef::new("mock", "a");
        let mut state = RouteState::new();
        assert!(!r.report_stream_failure(&model, &AiError::Auth("revoked".into()), &mut state));
    }

    #[tokio::test]
    async fn model_thinking_suffix_overrides_request_level() {
        let mock = Arc::new(MockProvider::new());
        mock.push("a", MockStep::text("x"));
        let r = router(mock.clone(), &[]);
        let req = Request {
            thinking: ThinkingLevel::Low,
            ..Request::default()
        };
        let (model, _stream) = r
            .stream(&chain(&["mock/a:high"]), &req, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(model.thinking, Some(ThinkingLevel::High));
        assert_eq!(mock.requests_for("a")[0].thinking, ThinkingLevel::High);
    }

    #[tokio::test]
    async fn cancellation_during_backoff_aborts() {
        let mock = Arc::new(MockProvider::new());
        mock.push(
            "a",
            MockStep::fail(AiError::RateLimit {
                retry_after: Some(Duration::from_secs(30)),
            }),
        );
        let r = router(mock, &[]);
        let cancel = CancellationToken::new();
        let c = chain(&["mock/a"]);
        let req = Request::default();
        let call = r.stream(&c, &req, cancel.clone());
        tokio::pin!(call);
        tokio::select! {
            _ = &mut call => panic!("should be waiting on retry_after"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => cancel.cancel(),
        }
        assert!(matches!(call.await, Err(AiError::Aborted)));
    }

    /// A provider whose answer depends on the stored account the request is made with: the
    /// accounts whose key is in `spent` are out of allowance.
    struct Accounted {
        credentials: Arc<Credentials>,
        config: crate::providers::presets::ProviderConfig,
        spent: Mutex<Vec<(&'static str, AiError)>>,
        keys: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Provider for Accounted {
        fn id(&self) -> &str {
            &self.config.id
        }

        async fn stream(
            &self,
            model: &ModelInfo,
            _: &Request,
            _: CancellationToken,
        ) -> Result<EventStream, AiError> {
            let crate::credentials::Auth::ApiKey(key) = self.credentials.resolve(&self.config).await? else {
                panic!("the test accounts are keys");
            };
            self.keys.lock().unwrap().push(format!("{}:{key}", model.id));
            if let Some((_, error)) = self.spent.lock().unwrap().iter().find(|(spent, _)| *spent == key) {
                return Err(error.clone());
            }
            let message = crate::types::Message::assistant(
                vec![crate::types::Block::text(format!("from {key}"))],
                crate::types::AssistantMeta {
                    provider: self.config.id.clone(),
                    model: model.id.clone(),
                    api: crate::types::WireApi::OpenAiChat,
                    usage: Default::default(),
                    stop: crate::types::StopReason::Stop,
                    response_id: None,
                    duration_ms: 0,
                    sources: Vec::new(),
                },
            );
            Ok(Box::pin(futures::stream::iter([StreamEvent::Done { message }])))
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, AiError> {
            Ok(Vec::new())
        }
    }

    fn quota() -> AiError {
        AiError::Quota {
            resets_at: None,
            message: "the 5-hour limit is used up".into(),
        }
    }

    /// A router over one provider with the accounts `default` (key `k-default`) and `work`
    /// (key `k-work`), and over the scripted `mock` provider as a fallback.
    fn two_accounts(
        dir: &std::path::Path,
        spent: Vec<(&'static str, AiError)>,
    ) -> (Router, Arc<Accounted>, Arc<MockProvider>) {
        use crate::credentials::{Credential, account_key};
        let credentials = Arc::new(Credentials::with_env(
            dir.join("auth.json"),
            Arc::new(|_: &str| None),
        ));
        for label in ["default", "work"] {
            let key = format!("k-{label}");
            credentials
                .store(&account_key("acme", label), Credential::ApiKey { key })
                .unwrap();
        }
        let accounted = Arc::new(Accounted {
            credentials: credentials.clone(),
            config: crate::providers::presets::ProviderConfig {
                requires_key: true,
                ..crate::providers::presets::ProviderConfig::new(
                    "acme",
                    crate::types::WireApi::OpenAiChat,
                    "http://x",
                )
            },
            spent: Mutex::new(spent),
            keys: Mutex::default(),
        });
        let mock = Arc::new(MockProvider::new());
        let router = Router::new(
            vec![accounted.clone(), mock.clone()],
            Catalog::default(),
            BTreeMap::new(),
            policy(),
        )
        .with_accounts(credentials);
        (router, accounted, mock)
    }

    #[tokio::test]
    async fn a_used_up_account_hands_over_to_the_next_and_the_same_model_is_asked_again() {
        let dir = tempfile::tempdir().unwrap();
        let (r, accounted, mock) = two_accounts(dir.path(), vec![("k-default", quota())]);
        mock.push("b", MockStep::text("never"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let c = chain(&["acme/a", "mock/b"]);

        let (model, stream) = r
            .stream_with(
                &c,
                &Request::default(),
                CancellationToken::new(),
                &mut RouteState::with_events(tx),
            )
            .await
            .unwrap();
        assert_eq!(model.to_string(), "acme/a");
        assert_eq!(final_text(stream).await, "from k-work");
        assert_eq!(*accounted.keys.lock().unwrap(), ["a:k-default", "a:k-work"]);
        match rx.try_recv().unwrap() {
            RouteEvent::Account {
                provider,
                from,
                to,
                until,
                limit,
            } => {
                assert_eq!(
                    (provider.as_str(), from.as_str(), to.as_str()),
                    ("acme", "default", "work")
                );
                // No reset time was given: the account is left alone for a quarter of an hour.
                let wait = until - crate::credentials::now_ms() / 1000;
                assert!((890..=900).contains(&wait), "{wait}");
                assert_eq!(limit, "usage limit");
            }
            other => panic!("{other:?}"),
        }
        assert!(rx.try_recv().is_err());
        assert!(r.limits().blocked_until("acme", "default").is_some());
        assert_eq!(r.limits().blocked_until("acme", "work"), None);

        // Work stays on the account it moved to.
        let (_, stream) = r
            .stream(&c, &Request::default(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(final_text(stream).await, "from k-work");
        assert_eq!(accounted.keys.lock().unwrap().len(), 3);
        assert!(mock.requests_for("b").is_empty());
    }

    #[tokio::test]
    async fn with_every_account_used_up_the_chain_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let (r, accounted, mock) =
            two_accounts(dir.path(), vec![("k-default", quota()), ("k-work", quota())]);
        mock.push("b", MockStep::text("from b"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let (model, stream) = r
            .stream_with(
                &chain(&["acme/a", "mock/b"]),
                &Request::default(),
                CancellationToken::new(),
                &mut RouteState::with_events(tx),
            )
            .await
            .unwrap();
        assert_eq!(model.to_string(), "mock/b");
        assert_eq!(final_text(stream).await, "from b");
        // Each account was asked once; a used-up allowance is not retried.
        assert_eq!(*accounted.keys.lock().unwrap(), ["a:k-default", "a:k-work"]);
        let events: Vec<RouteEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(matches!(events[0], RouteEvent::Account { .. }), "{events:?}");
        assert!(
            matches!(
                &events[1],
                RouteEvent::Fallback {
                    error: AiError::Quota { .. },
                    ..
                }
            ),
            "{events:?}"
        );
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn an_ordinary_rate_limit_retries_on_the_same_account() {
        let dir = tempfile::tempdir().unwrap();
        let short = AiError::RateLimit {
            retry_after: Some(Duration::from_millis(1)),
        };
        let (r, accounted, _) = two_accounts(dir.path(), vec![("k-default", short)]);
        let result = r
            .stream(&chain(&["acme/a"]), &Request::default(), CancellationToken::new())
            .await;
        assert!(matches!(result, Err(AiError::RateLimit { .. })));
        assert_eq!(
            *accounted.keys.lock().unwrap(),
            ["a:k-default", "a:k-default", "a:k-default"]
        );
        assert_eq!(r.limits().blocked_until("acme", "default"), None);

        // A wait of more than a minute is a used-up allowance in all but name.
        let dir = tempfile::tempdir().unwrap();
        let long = AiError::RateLimit {
            retry_after: Some(Duration::from_secs(3 * 3600)),
        };
        let (r, accounted, _) = two_accounts(dir.path(), vec![("k-default", long)]);
        let (_, stream) = r
            .stream(&chain(&["acme/a"]), &Request::default(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(final_text(stream).await, "from k-work");
        assert_eq!(*accounted.keys.lock().unwrap(), ["a:k-default", "a:k-work"]);
    }

    #[tokio::test]
    async fn a_refused_tier_is_dropped_for_that_model_and_the_request_is_sent_again() {
        let mock = Arc::new(MockProvider::new());
        mock.push(
            "a",
            MockStep::fail(AiError::InvalidRequest(
                "HTTP 400: Unsupported parameter: 'service_tier'".into(),
            )),
        );
        mock.push("a", MockStep::text("standard"));
        mock.push("a", MockStep::text("again"));
        mock.push("b", MockStep::text("fast"));
        let r = router(mock.clone(), &[]);
        r.set_tier(Some(Tier::Standard));
        assert_eq!(r.tier(), None);
        r.set_tier(Some(Tier::Priority));
        let req = Request {
            tier: r.tier(),
            ..Request::default()
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let (_, stream) = r
            .stream_with(
                &chain(&["mock/a"]),
                &req,
                CancellationToken::new(),
                &mut RouteState::with_events(tx),
            )
            .await
            .unwrap();
        assert_eq!(final_text(stream).await, "standard");
        assert!(matches!(
            rx.try_recv(),
            Ok(RouteEvent::TierDropped {
                tier: Tier::Priority,
                ..
            })
        ));
        for model in ["mock/a", "mock/b"] {
            let (_, stream) = r
                .stream(&chain(&[model]), &req, CancellationToken::new())
                .await
                .unwrap();
            drop(stream);
        }
        let tiers: Vec<Option<Tier>> = mock.requests().iter().map(|(_, sent)| sent.tier).collect();
        assert_eq!(tiers, [Some(Tier::Priority), None, None, Some(Tier::Priority)]);
        assert!(r.is_tierless(&ModelRef::new("mock", "a")));

        // Without a tier on the request, the same words are an ordinary invalid request.
        mock.push("c", MockStep::fail(AiError::InvalidRequest("bad speed".into())));
        let plain = r
            .stream(&chain(&["mock/c"]), &Request::default(), CancellationToken::new())
            .await;
        assert!(matches!(plain, Err(AiError::InvalidRequest(_))));
    }

    fn retries(rx: &mut tokio::sync::mpsc::UnboundedReceiver<RouteEvent>) -> Vec<String> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(match event {
                RouteEvent::Retry { attempt, error, .. } => format!("retry {attempt}: {error}"),
                RouteEvent::Fallback { from, to, error } => format!("fallback {from} -> {to}: {error}"),
                other => format!("{other:?}"),
            });
        }
        events
    }

    #[tokio::test]
    async fn a_stalled_model_is_asked_again_up_to_the_stall_limit_then_the_chain_moves_on() {
        let mock = Arc::new(MockProvider::new());
        for _ in 0..3 {
            mock.push("a", MockStep::fail(AiError::Stalled { secs: 20 }));
        }
        mock.push("a", MockStep::text("a must not be asked a fourth time"));
        mock.push("b", MockStep::text("from b"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let r = router(mock.clone(), &[]);
        let (model, stream) = r
            .stream_with(
                &chain(&["mock/a", "mock/b"]),
                &Request::default(),
                CancellationToken::new(),
                &mut RouteState::with_events(tx),
            )
            .await
            .unwrap();
        assert_eq!(model.to_string(), "mock/b");
        assert_eq!(final_text(stream).await, "from b");
        assert_eq!(
            retries(&mut rx),
            [
                "retry 1: no answer for 20 s",
                "retry 2: no answer for 20 s",
                "fallback mock/a -> mock/b: no answer for 20 s",
            ]
        );
        assert_eq!(mock.requests_for("a").len(), 3);
    }

    #[tokio::test]
    async fn stalls_do_not_use_up_the_attempts_of_a_model() {
        let mock = Arc::new(MockProvider::new());
        // Two stalls and two other failures: neither count reaches its limit of three.
        mock.push("a", MockStep::fail(AiError::Stalled { secs: 20 }));
        mock.push("a", MockStep::fail(AiError::Overloaded));
        mock.push("a", MockStep::fail(AiError::Stalled { secs: 20 }));
        mock.push("a", MockStep::fail(AiError::Overloaded));
        mock.push("a", MockStep::text("fifth time lucky"));
        let r = router(mock.clone(), &[]);
        let (model, stream) = r
            .stream(
                &chain(&["mock/a", "mock/b"]),
                &Request::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(model.to_string(), "mock/a");
        assert_eq!(final_text(stream).await, "fifth time lucky");
    }

    #[tokio::test]
    async fn without_a_stall_limit_a_stalled_model_is_asked_until_it_answers() {
        let mock = Arc::new(MockProvider::new());
        for _ in 0..7 {
            mock.push("a", MockStep::fail(AiError::Stalled { secs: 20 }));
        }
        mock.push("a", MockStep::text("at last"));
        mock.push("b", MockStep::text("never"));
        let roles = BTreeMap::new();
        let policy = RetryPolicy {
            max_stalls: 0,
            ..policy()
        };
        let r = Router::new(vec![mock.clone()], Catalog::default(), roles, policy);
        let (model, stream) = r
            .stream(
                &chain(&["mock/a", "mock/b"]),
                &Request::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(model.to_string(), "mock/a");
        assert_eq!(final_text(stream).await, "at last");
        assert!(mock.requests_for("b").is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_reply_that_stalls_mid_stream_is_asked_for_again_after_a_pause() {
        let mock = Arc::new(MockProvider::new());
        mock.push("a", MockStep::text("second try"));
        mock.push("b", MockStep::text("from b"));
        let policy = RetryPolicy {
            base_delay: Duration::from_secs(1),
            ..policy()
        };
        let r = Router::new(vec![mock.clone()], Catalog::default(), BTreeMap::new(), policy);
        let model = ModelRef::new("mock", "a");
        let c = chain(&["mock/a", "mock/b"]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = RouteState::with_events(tx);
        let stalled = AiError::Stalled { secs: 120 };

        // The first stall: the same model, after the first backoff step.
        assert!(r.report_stream_failure(&model, &stalled, &mut state));
        let before = tokio::time::Instant::now();
        let (asked, stream) = r
            .stream_with(&c, &Request::default(), CancellationToken::new(), &mut state)
            .await
            .unwrap();
        assert_eq!(before.elapsed(), Duration::from_secs(1));
        assert_eq!(asked, model);
        assert_eq!(final_text(stream).await, "second try");

        // The second stall doubles the pause; an abort during it ends the request.
        assert!(r.report_stream_failure(&model, &stalled, &mut state));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let aborted = r.stream_with(&c, &Request::default(), cancel, &mut state).await;
        assert!(matches!(aborted, Err(AiError::Aborted)));

        // The third is the limit: the model is out and the next one answers at once.
        assert!(!r.report_stream_failure(&model, &stalled, &mut state));
        assert!(r.in_cooldown(&model));
        let before = tokio::time::Instant::now();
        let (asked, stream) = r
            .stream_with(&c, &Request::default(), CancellationToken::new(), &mut state)
            .await
            .unwrap();
        assert_eq!(before.elapsed(), Duration::ZERO);
        assert_eq!(asked.to_string(), "mock/b");
        assert_eq!(final_text(stream).await, "from b");
        assert_eq!(
            retries(&mut rx),
            [
                "retry 1: no answer for 120 s",
                "retry 2: no answer for 120 s",
                "fallback mock/a -> mock/b: no answer for 120 s",
            ]
        );
    }

    /// A virtual model that answers with a fixed pick per reason and notes what it was asked.
    struct ByReason {
        picks: Vec<(RouteReason, &'static str)>,
        asked: Mutex<Vec<String>>,
    }

    impl ByReason {
        fn new(picks: &[(RouteReason, &'static str)]) -> Arc<ByReason> {
            Arc::new(ByReason {
                picks: picks.to_vec(),
                asked: Mutex::default(),
            })
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl crate::dispatch::VirtualModel for ByReason {
        async fn route(&self, request: &RouteRequest<'_>) -> Result<crate::dispatch::Route, String> {
            let previous = request.previous.as_ref().map(ToString::to_string);
            let failed = request.failed.map(|(model, error)| format!("{model} ({error})"));
            self.asked.lock().unwrap().push(format!(
                "{} {} {} previous={} failed={} text={}",
                request.name,
                request.reason.as_str(),
                request.thinking,
                previous.as_deref().unwrap_or("-"),
                failed.as_deref().unwrap_or("-"),
                request.last_user_text().as_deref().unwrap_or("-"),
            ));
            let pick = self
                .picks
                .iter()
                .find(|(reason, _)| *reason == request.reason)
                .map(|(_, pick)| *pick)
                .ok_or("nothing to pick for this reason")?;
            if pick == "fail" {
                return Err("the router broke".to_string());
            }
            // `provider/model ...` with an optional `!level` for the whole pick.
            let (chain, thinking) = match pick.split_once('!') {
                Some((chain, level)) => (chain, Some(level.parse().unwrap())),
                None => (pick, None),
            };
            Ok(crate::dispatch::Route {
                chain: chain.split(' ').map(|spec| spec.parse().unwrap()).collect(),
                thinking,
            })
        }
    }

    fn with_virtual(mock: Arc<MockProvider>, name: &str, route: Arc<ByReason>) -> Router {
        let virtuals = Arc::new(VirtualModels::default());
        virtuals.add("test", name, route);
        router(mock, &[("cheap", &["mock/small"])]).with_virtual(virtuals)
    }

    fn asking(text: &str) -> Request {
        Request {
            messages: vec![crate::types::Message::user(text)],
            ..Request::default()
        }
    }

    #[tokio::test]
    async fn a_virtual_model_picks_the_model_of_each_request_and_the_route_is_reported() {
        let mock = Arc::new(MockProvider::new());
        mock.push("small", MockStep::text("from small"));
        mock.push("big", MockStep::text("from big"));
        let route = ByReason::new(&[
            (RouteReason::User, "@cheap"),
            (RouteReason::Continuation, "mock/big!high"),
        ]);
        let r = with_virtual(mock.clone(), "auto", route.clone());
        let c = chain(&["virtual/auto:low"]);

        // What the user wrote goes where the router sends it; a role is as good as a model.
        let mut state = RouteState::new();
        let (model, stream) = r
            .stream_with(&c, &asking("rename x"), CancellationToken::new(), &mut state)
            .await
            .unwrap();
        assert_eq!(model.to_string(), "mock/small:low");
        assert_eq!(
            state.via().map(ToString::to_string).as_deref(),
            Some("virtual/auto:low")
        );
        let reply = match stream.collect::<Vec<_>>().await.pop() {
            Some(StreamEvent::Done { message }) => message,
            other => panic!("{other:?}"),
        };
        assert_eq!(reply.text(), "from small");
        // The level chosen with the virtual model reached the model that had none of its own.
        assert_eq!(mock.requests_for("small")[0].thinking, ThinkingLevel::Low);

        // The request after a tool result is a continuation and knows who answered before.
        let mut after_tools = asking("rename x");
        after_tools.messages.push(reply);
        after_tools.messages.push(crate::types::Message::tool(vec![
            crate::types::Block::tool_result("c1", "done", false),
        ]));
        let mut state = RouteState::new();
        let (model, _) = r
            .stream_with(&c, &after_tools, CancellationToken::new(), &mut state)
            .await
            .unwrap();
        assert_eq!(model.to_string(), "mock/big:high");
        assert_eq!(mock.requests_for("big")[0].thinking, ThinkingLevel::High);
        assert_eq!(
            route.asked(),
            [
                "auto user low previous=- failed=- text=rename x",
                "auto continuation low previous=mock/small failed=- text=rename x",
            ]
        );

        // A chain of real models is not dispatched by anything.
        mock.push("big", MockStep::text("direct"));
        let mut state = RouteState::new();
        let (model, _) = r
            .stream_with(
                &chain(&["mock/big"]),
                &asking("x"),
                CancellationToken::new(),
                &mut state,
            )
            .await
            .unwrap();
        assert_eq!((model.to_string().as_str(), state.via()), ("mock/big", None));
    }

    #[tokio::test]
    async fn a_retry_is_routed_again_with_the_model_that_failed() {
        let mock = Arc::new(MockProvider::new());
        mock.push("small", MockStep::text("cut short"));
        mock.push("big", MockStep::text("whole"));
        let route = ByReason::new(&[
            (RouteReason::User, "mock/small"),
            (RouteReason::Retry, "mock/big"),
        ]);
        let r = with_virtual(mock, "auto", route.clone());
        let c = chain(&["virtual/auto"]);
        let mut state = RouteState::new();
        let (model, _) = r
            .stream_with(&c, &asking("go"), CancellationToken::new(), &mut state)
            .await
            .unwrap();
        // The reply broke off while it was streamed: the same request goes out again.
        r.report_stream_failure(&model, &AiError::Overloaded, &mut state);
        let (model, stream) = r
            .stream_with(&c, &asking("go"), CancellationToken::new(), &mut state)
            .await
            .unwrap();
        assert_eq!(model.to_string(), "mock/big");
        assert_eq!(final_text(stream).await, "whole");
        assert_eq!(
            route.asked()[1],
            "auto retry off previous=- failed=mock/small (provider overloaded) text=go"
        );
    }

    #[tokio::test]
    async fn a_virtual_model_that_cannot_pick_fails_the_request_unless_the_chain_goes_on() {
        let mock = Arc::new(MockProvider::new());
        mock.push("big", MockStep::text("fallback"));
        let route = ByReason::new(&[(RouteReason::Direct, "fail"), (RouteReason::User, "virtual/auto")]);
        let r = with_virtual(mock.clone(), "auto", route.clone());

        // `stream` is a request from outside a conversation.
        let failed = r
            .stream(&chain(&["virtual/auto"]), &asking("x"), CancellationToken::new())
            .await;
        assert_eq!(
            failed.err(),
            Some(AiError::InvalidRequest(
                "virtual/auto: the router broke".to_string()
            ))
        );
        assert_eq!(route.asked(), ["auto direct off previous=- failed=- text=x"]);

        // A virtual model cannot hand the request to a virtual model, and one nobody registered
        // picks nothing either.
        for (spec, why) in [
            (
                "virtual/auto",
                "virtual/auto: it named a virtual model, which cannot answer",
            ),
            (
                "virtual/gone",
                "virtual/gone: no extension that is loaded defines it",
            ),
        ] {
            let failed = r
                .stream_with(
                    &chain(&[spec]),
                    &asking("x"),
                    CancellationToken::new(),
                    &mut RouteState::new(),
                )
                .await;
            assert_eq!(failed.err(), Some(AiError::InvalidRequest(why.to_string())));
        }

        // A request that is cancelled does not wait for a router.
        let cancel = CancellationToken::new();
        cancel.cancel();
        let aborted = r
            .stream_with(
                &chain(&["virtual/auto"]),
                &asking("x"),
                cancel,
                &mut RouteState::new(),
            )
            .await;
        assert_eq!(aborted.err(), Some(AiError::Aborted));

        // With a model after it in the chain, the request goes there.
        let mut state = RouteState::new();
        let (model, stream) = r
            .stream_with(
                &chain(&["virtual/gone", "mock/big"]),
                &asking("x"),
                CancellationToken::new(),
                &mut state,
            )
            .await
            .unwrap();
        assert_eq!(model.to_string(), "mock/big");
        assert_eq!(state.via(), None);
        assert_eq!(final_text(stream).await, "fallback");
    }

    #[test]
    fn the_limits_of_a_virtual_model_are_those_of_the_model_that_answered_last() {
        let r = with_virtual(Arc::new(MockProvider::new()), "auto", ByReason::new(&[]));
        let answered: ModelRef = "mock/big".parse().unwrap();
        let limits = |spec: &str, answered: Option<&ModelRef>| {
            r.limits_model(&chain(&[spec]), answered)
                .map(|model| model.to_string())
        };
        assert_eq!(limits("virtual/auto", None).as_deref(), Some("virtual/auto"));
        assert_eq!(
            limits("virtual/auto", Some(&answered)).as_deref(),
            Some("mock/big")
        );
        assert_eq!(limits("@cheap", Some(&answered)).as_deref(), Some("mock/small"));
        assert_eq!(limits("@nothing", Some(&answered)), None);
        assert_eq!(r.virtual_models(), ["auto"]);
    }
}
