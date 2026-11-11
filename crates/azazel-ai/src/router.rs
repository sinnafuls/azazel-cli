//! Resolves fallback chains and drives retries, cooldowns and fallback across providers.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::catalog::{Catalog, ModelInfo};
use crate::error::AiError;
use crate::provider::Provider;
use crate::stream::EventStream;
use crate::types::{ModelRef, ModelSpec, Request};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    /// Attempts per model before falling back.
    pub max_attempts: u32,
    /// How long a model that exhausted its attempts (or failed authentication) is skipped.
    pub cooldown: Duration,
    /// First retry delay; doubles per attempt, capped at 60 s.
    pub base_delay: Duration,
    pub max_jitter: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 3,
            cooldown: Duration::from_secs(300),
            base_delay: Duration::from_secs(1),
            max_jitter: Duration::from_millis(250),
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
}

/// Per-request routing state: failures counted so far and an optional event sink. Keep one for the
/// lifetime of a logical request so that failures reported after streaming began count as attempts
/// and a model that used up its attempts is not tried again for that request.
#[derive(Default)]
pub struct RouteState {
    failures: HashMap<String, u32>,
    /// The model most recently given up on, until the next candidate is announced.
    gave_up: Option<(ModelRef, AiError)>,
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
        }
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
        self.stream_with(chain, req, cancel, &mut RouteState::new()).await
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
                Ok(stream) => return Ok((model.clone(), stream)),
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
        let req: Cow<'_, Request> = match model.thinking {
            Some(level) if level != req.thinking => Cow::Owned(Request {
                thinking: level,
                ..req.clone()
            }),
            _ => Cow::Borrowed(req),
        };
        loop {
            if cancel.is_cancelled() {
                return Err(AiError::Aborted);
            }
            let error = match provider.stream(&info, &req, cancel.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(e @ (AiError::ContextOverflow | AiError::InvalidRequest(_) | AiError::Aborted)) => {
                    return Err(e);
                }
                Err(e) => e,
            };
            let attempt = state.record(model, &error, self.policy.max_attempts);
            if attempt >= self.policy.max_attempts {
                return Err(error);
            }
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

    /// Records a failure that arrived after streaming had begun; call [`Router::stream_with`] again
    /// with the same `state` to retry or fall back. Returns true when the same model will be retried.
    pub fn report_stream_failure(&self, model: &ModelRef, error: &AiError, state: &mut RouteState) -> bool {
        let attempt = state.record(model, error, self.policy.max_attempts);
        if attempt < self.policy.max_attempts {
            state.emit(RouteEvent::Retry {
                attempt,
                model: model.clone(),
                error: error.clone(),
                delay: Duration::ZERO,
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
}
