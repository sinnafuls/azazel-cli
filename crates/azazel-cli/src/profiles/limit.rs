//! Moving a session to another profile when the provider reports a usage limit, and offering
//! the way back once the limit's window has reset.
//!
//! A usage limit reaches here through the hook the agent loop already calls when a request
//! failed for lack of allowance and no account or model of its chain could take it
//! (`resets::LimitFlow`, which asks [`ProfileSwitcher::on_limit`] first). What counts as a
//! limit, and not as throttling the router rides out by itself, is decided by
//! [`azazel_ai::providers::http::blocked_until`]: only a refusal with a reset time, or a 429
//! that asks for more than a minute, has an end time. A 429 with a short `retry-after`, an
//! overloaded provider and a server error have none and never switch anything.
//!
//! The switch happens once per window: while one is active nothing else switches, so a profile
//! that hits its own limit ends the request with the normal error instead of bouncing back.
//! The way back is looked at when the next prompt is sent, and never while the lead is running.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use azazel_agent::{AgentInfo, Recovery, Services};
use azazel_ai::credentials::now_ms;
use azazel_ai::providers::http::limit_window;
use azazel_ai::{AiError, ModelSpec, Router};
use azazel_core::config::ProfileReset;
use azazel_core::{EntryKind, Event, EventHandler, ExtCtx, Level, Outcome, SessionStore, Ui};
use azazel_protocol::Frame;
use azazel_team::Team;

use super::{AccessCheck, Profile, Reason, switch, usability};

/// What a switch on a usage limit leaves behind, until the window has reset.
#[derive(Debug, Clone, PartialEq)]
pub struct Switched {
    /// The profile the session was on, when it was on a named one.
    pub from: Option<String>,
    pub to: String,
    /// The provider whose limit was reached.
    pub provider: String,
    /// Unix seconds at which the window resets.
    pub until: u64,
    /// Whether the provider said when the window resets. A guessed time (the provider named
    /// none) is no reason to go back by itself: the limit may still stand then.
    pub known: bool,
    /// The chain the lead was left on; another one means somebody picked a model by hand.
    left: Vec<ModelSpec>,
    /// The roles the target profile replaced, as they were: what going back restores when
    /// there was no named profile to go back to.
    saved: BTreeMap<String, Vec<ModelSpec>>,
}

#[derive(Default)]
struct State {
    /// The profile the session is on, as far as this switcher knows.
    current: Option<String>,
    /// What the session's last `ProfileChange` entry said when `current` was set; another value
    /// means somebody switched by hand since.
    seen: Option<String>,
    active: Option<Switched>,
}

/// Switches the lead to `[profiles] on_limit` on a usage limit and back after the reset.
pub struct ProfileSwitcher {
    on_limit: String,
    on_reset: ProfileReset,
    profiles: Vec<Profile>,
    access: AccessCheck,
    services: Arc<Services>,
    /// Unix seconds; a test sets the time.
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// The chain the lead asks now; a test sets it, as there is no lead there.
    lead_chain: Arc<dyn Fn() -> Option<Vec<ModelSpec>> + Send + Sync>,
    state: Mutex<State>,
}

/// Where an RPC host sends the `profile_changed` frame of a switch the switcher made. A service
/// the RPC host registers: the terminal UI and print mode have none, and get the notice instead.
pub struct ProfileFrames(pub Arc<dyn Fn(Frame) + Send + Sync>);

/// Tells the user, wherever there is one: a notice in the terminal UI, a `notify` request to an
/// RPC client, a line on standard error without either.
fn announce(services: &Services, level: Level, text: &str) {
    match services.get::<Ui>().filter(|ui| ui.0.present()) {
        Some(ui) => ui.0.notify(level, text),
        None => eprintln!("azazel: {text}"),
    }
}

/// Tells the user of a switch to profile `to`: an RPC client gets the `profile_changed` frame and
/// nothing else, so that it hears of the switch once; elsewhere it is [`announce`]d as `text`.
fn announce_switch(services: &Services, frame: Frame, text: &str) {
    match services.get::<ProfileFrames>() {
        Some(frames) => (frames.0)(frame),
        None => announce(services, Level::Info, text),
    }
}

/// The provider whose limit was reached, among those of the models `tried`: the one whose
/// account was blocked last, else the last model's.
fn failed_provider(router: &Router, tried: &[azazel_ai::ModelRef]) -> String {
    let blocked = router
        .limits()
        .snapshot()
        .into_iter()
        .filter(|account| tried.iter().any(|model| model.provider == account.provider))
        .flat_map(|account| {
            let provider = account.provider;
            account
                .blocks
                .into_iter()
                .map(move |block| (block.since, provider.clone()))
        })
        .max_by_key(|(since, _)| *since)
        .map(|(_, provider)| provider);
    blocked
        .or_else(|| tried.last().map(|model| model.provider.clone()))
        .unwrap_or_default()
}

/// What ran out at `provider`, as a person calls it (`5-hour limit`): the block that lasts
/// longest, else `usage limit`.
fn limit_label(router: &Router, provider: &str) -> String {
    router
        .limits()
        .snapshot()
        .into_iter()
        .filter(|account| account.provider == provider)
        .flat_map(|account| account.blocks)
        .max_by_key(|block| block.until)
        .map_or_else(|| "usage limit".to_string(), |block| block.limit())
}

const CACHE_COST: &str =
    "The conversation is sent in full to the new model once, so the next reply costs more.";

impl ProfileSwitcher {
    pub fn new(
        on_limit: &str,
        on_reset: ProfileReset,
        profiles: Vec<Profile>,
        access: AccessCheck,
        services: Arc<Services>,
        current: Option<String>,
    ) -> Self {
        let lead_chain = {
            let services = services.clone();
            Arc::new(move || {
                let lead = services.get::<Team>()?.lead()?;
                Some(lead.status().chain)
            })
        };
        ProfileSwitcher {
            on_limit: on_limit.trim().to_string(),
            on_reset,
            profiles,
            access,
            services,
            clock: Arc::new(|| now_ms() / 1000),
            lead_chain,
            state: Mutex::new(State {
                current,
                ..State::default()
            }),
        }
    }

    #[cfg(test)]
    fn with_clock(mut self, clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    #[cfg(test)]
    fn with_lead_chain(mut self, lead_chain: Arc<dyn Fn() -> Option<Vec<ModelSpec>> + Send + Sync>) -> Self {
        self.lead_chain = lead_chain;
        self
    }

    /// The switch that is in force, for whoever shows the state of the session.
    pub fn active(&self) -> Option<Switched> {
        self.state().active.clone()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.name == name)
    }

    /// The profile the session is on: the one the session's record names when it changed since
    /// this switcher last looked, else what it set itself.
    fn current(state: &State, session: Option<&SessionStore>) -> Option<String> {
        let last = session.and_then(|session| session.last_profile(None));
        match last == state.seen {
            true => state.current.clone(),
            false => last,
        }
    }

    /// A request of `agent` to `chain` failed with `error` for lack of allowance. `Some` when
    /// the lead was moved to the `on_limit` profile: the request goes again, to the lead's role.
    pub fn on_limit(&self, agent: &AgentInfo, chain: &[ModelSpec], error: &AiError) -> Option<Recovery> {
        // Sub-agents keep their models and fall back under their own role chains.
        if agent.depth != 0 || self.on_limit.is_empty() {
            return None;
        }
        let (until, known) = limit_window(error)?;
        let router = self.services.get::<Router>()?;
        let target = self.profile(&self.on_limit)?;
        let team = self.services.get::<Team>();
        let lead_handle = team.as_ref().and_then(|team| team.lead());
        let session = self.services.get::<SessionStore>();
        let mut state = self.state();
        // One switch per window.
        if state.active.is_some() {
            return None;
        }
        let from = Self::current(&state, session.as_deref());
        if from.as_deref() == Some(target.name.as_str()) {
            return None;
        }
        // The target has to offer a model the failed request did not already try.
        let lead = vec![ModelSpec::Role("lead".to_string())];
        let tried = router.resolve(chain);
        let next = router.resolve_with(&target.roles, &lead);
        if !next
            .iter()
            .any(|model| !tried.iter().any(|tried| tried.key() == model.key()))
        {
            return None;
        }
        let usable = usability(target, &*self.access);
        if !usable.usable {
            drop(state);
            announce(
                &self.services,
                Level::Warn,
                &format!(
                    "usage limit reached; not switching to profile {}, which {}",
                    target.name,
                    usable.problem().unwrap_or_default()
                ),
            );
            return None;
        }
        let provider = failed_provider(&router, &tried);
        let roles = router.roles();
        let saved = target
            .roles
            .keys()
            .filter_map(|role| Some((role.clone(), roles.get(role)?.clone())))
            .collect();
        let done = switch(
            target,
            &router,
            lead_handle.as_ref().map(|lead| (lead, session.as_deref())),
            team.as_deref(),
            Reason::Limit,
        );
        state.current = Some(target.name.clone());
        state.seen = session.as_deref().and_then(|session| session.last_profile(None));
        state.active = Some(Switched {
            from: from.clone(),
            to: target.name.clone(),
            provider: provider.clone(),
            until,
            known,
            left: lead.clone(),
            saved,
        });
        drop(state);
        announce_switch(
            &self.services,
            Frame::ProfileChanged {
                from,
                to: target.name.clone(),
                reason: Reason::Limit.as_str().to_string(),
                until: Some(until),
                limit: Some(limit_label(&router, &provider)),
            },
            &format!(
                "{provider}: usage limit reached until {}. Switched to {done}. {CACHE_COST}",
                crate::accounts::clock(until)
            ),
        );
        Some(Recovery::Model(lead))
    }

    /// Called when a prompt is about to be sent: once the window of an automatic switch has
    /// reset, `[profiles] on_reset` decides what happens.
    pub fn at_turn_boundary(&self) {
        let now = (self.clock)();
        let team = self.services.get::<Team>();
        let lead_handle = team.as_ref().and_then(|team| team.lead());
        // Never in the middle of a run: a prompt that arrives while the lead works waits.
        if lead_handle.as_ref().is_some_and(|lead| lead.status().busy) {
            return;
        }
        let session = self.services.get::<SessionStore>();
        let mut state = self.state();
        let Some(active) = state.active.clone().filter(|active| now >= active.until) else {
            return;
        };
        // The window is over either way: a limit after this one is a new one.
        state.active = None;
        // Somebody moved to another profile by hand in the meantime.
        if Self::current(&state, session.as_deref()).as_deref() != Some(active.to.as_str()) {
            return;
        }
        // A reset time the provider did not give is a guess: going back by itself could land on
        // the limit again and switch once more every window, so the user is asked instead.
        let on_reset = match self.on_reset {
            ProfileReset::Auto if !active.known => ProfileReset::Ask,
            other => other,
        };
        match on_reset {
            ProfileReset::Off => {}
            ProfileReset::Ask => {
                drop(state);
                let how = match &active.from {
                    Some(from) => format!("Return to profile {from}? Run /profile {from}."),
                    None => "/model or /profile moves back.".to_string(),
                };
                announce(
                    &self.services,
                    Level::Info,
                    &format!(
                        "The usage limit of {} {} {how} Going back sends the conversation in full to \
                         the other model once, so the next reply costs more.",
                        active.provider,
                        match active.known {
                            true => "has reset.",
                            false => "may have reset (the provider did not say when).",
                        }
                    ),
                );
            }
            ProfileReset::Auto => {
                let Some(router) = self.services.get::<Router>() else {
                    return;
                };
                // A model picked by hand since the switch stays.
                if (self.lead_chain)().is_some_and(|chain| chain != active.left) {
                    return;
                }
                let lead = lead_handle.as_ref().map(|lead| (lead, session.as_deref()));
                let back = active.from.as_deref().and_then(|name| self.profile(name));
                let done = match back {
                    Some(profile) => {
                        let usable = usability(profile, &*self.access);
                        if !usable.usable {
                            drop(state);
                            announce(
                                &self.services,
                                Level::Warn,
                                &format!(
                                    "The usage limit of {} has reset, but profile {} {}",
                                    active.provider,
                                    profile.name,
                                    usable.problem().unwrap_or_default()
                                ),
                            );
                            return;
                        }
                        state.current = Some(profile.name.clone());
                        (
                            switch(profile, &router, lead, team.as_deref(), Reason::Reset),
                            Some(profile.name.clone()),
                        )
                    }
                    None => {
                        state.current = None;
                        // Written down, so that the record stops naming the profile of the limit.
                        if let Some(session) = session.as_deref()
                            && session.last_profile(None).is_some()
                        {
                            session.append(EntryKind::ProfileChange {
                                profile: String::new(),
                            });
                        }
                        (
                            format!(
                                "the models from before the limit: {}",
                                crate::routing::apply_roles(&router, &active.saved, lead, team.as_deref())
                            ),
                            None,
                        )
                    }
                };
                state.seen = session.as_deref().and_then(|session| session.last_profile(None));
                drop(state);
                let (done, back_to) = done;
                let text = format!(
                    "The usage limit of {} has reset; went back to {done}. {CACHE_COST}",
                    active.provider
                );
                match back_to {
                    Some(to) => announce_switch(
                        &self.services,
                        Frame::ProfileChanged {
                            from: Some(active.to.clone()),
                            to,
                            reason: Reason::Reset.as_str().to_string(),
                            until: None,
                            limit: None,
                        },
                        &text,
                    ),
                    // No named profile to report: the roles are back as they were.
                    None => announce(&self.services, Level::Info, &text),
                }
            }
        }
    }
}

/// Looks at the reset of an automatic switch whenever a prompt is sent.
pub struct AtPrompt(pub Arc<ProfileSwitcher>);

#[async_trait]
impl EventHandler for AtPrompt {
    async fn handle(&self, ev: &mut Event, _ctx: &ExtCtx) -> Outcome {
        if matches!(ev, Event::UserPrompt { .. }) {
            self.0.at_turn_boundary();
        }
        Outcome::Continue
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::super::Access;
    use azazel_ai::{Catalog, RetryPolicy};
    use azazel_core::{ApprovalDecision, ApprovalRequest, Question, UiBridge, UiError};

    use super::*;
    use crate::profiles::ProfileSource;

    #[derive(Default)]
    struct Notes(Mutex<Vec<(Level, String)>>);

    #[async_trait]
    impl UiBridge for Notes {
        async fn ask(&self, _: &Question) -> Result<Vec<String>, UiError> {
            Err(UiError::NoUser)
        }
        async fn confirm(&self, _: &str) -> Result<bool, UiError> {
            Err(UiError::NoUser)
        }
        async fn select(&self, _: &str, _: &[String]) -> Result<usize, UiError> {
            Err(UiError::NoUser)
        }
        async fn input(&self, _: &str) -> Result<String, UiError> {
            Err(UiError::NoUser)
        }
        fn notify(&self, level: Level, text: &str) {
            self.0.lock().unwrap().push((level, text.to_string()));
        }
        async fn approval(&self, _: ApprovalRequest) -> Option<ApprovalDecision> {
            None
        }
    }

    fn chain(specs: &[&str]) -> Vec<ModelSpec> {
        specs.iter().map(|spec| spec.parse().unwrap()).collect()
    }

    fn profile(name: &str, model: &str) -> Profile {
        Profile {
            name: name.to_string(),
            source: ProfileSource::User,
            roles: BTreeMap::from([("default".to_string(), chain(&[model]))]),
        }
    }

    fn now() -> u64 {
        now_ms() / 1000
    }

    fn limit(secs: u64) -> AiError {
        AiError::Quota {
            resets_at: Some(now() + secs),
            message: "the 5-hour limit is used up".to_string(),
        }
    }

    struct Bench {
        switcher: ProfileSwitcher,
        router: Arc<Router>,
        notes: Arc<Notes>,
        /// Seconds the clock is ahead of now.
        ahead: Arc<AtomicU64>,
    }

    fn bench(
        on_limit: &str,
        on_reset: ProfileReset,
        current: Option<&str>,
        logged_in: fn(&str) -> bool,
    ) -> Bench {
        let roles = BTreeMap::from([
            ("default".to_string(), chain(&["acme/a"])),
            ("lead".to_string(), chain(&["@default"])),
        ]);
        let router = Arc::new(Router::new(
            Vec::new(),
            Catalog::default(),
            roles,
            RetryPolicy::default(),
        ));
        let notes = Arc::new(Notes::default());
        let services = Arc::new(Services::new());
        services.insert(router.clone());
        services.insert(Arc::new(Ui(notes.clone())));
        let ahead = Arc::new(AtomicU64::new(0));
        let offset = ahead.clone();
        let switcher = ProfileSwitcher::new(
            on_limit,
            on_reset,
            vec![
                profile("a", "acme/a"),
                profile("b", "acme/b"),
                profile("c", "other/c"),
            ],
            Arc::new(move |id: &str| match logged_in(id) {
                true => Access::Ready,
                false => Access::NoLogin,
            }),
            services,
            current.map(str::to_string),
        )
        .with_clock(Arc::new(move || now() + offset.load(Ordering::SeqCst)));
        Bench {
            switcher,
            router,
            notes,
            ahead,
        }
    }

    fn agent(depth: u32) -> AgentInfo {
        AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth,
            session_id: "s".into(),
        }
    }

    fn hit(bench: &Bench, error: &AiError) -> Option<Recovery> {
        bench.switcher.on_limit(&agent(0), &chain(&["@lead"]), error)
    }

    fn lead_model(bench: &Bench) -> String {
        bench.router.resolve(&chain(&["@lead"]))[0].key()
    }

    fn told(bench: &Bench) -> Vec<String> {
        bench
            .notes
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, text)| text.clone())
            .collect()
    }

    #[test]
    fn a_usage_limit_moves_the_lead_once_and_a_second_limit_moves_nothing() {
        let bench = bench("b", ProfileReset::Ask, Some("a"), |_| true);
        let recovery = hit(&bench, &limit(7200));
        assert_eq!(recovery, Some(Recovery::Model(chain(&["@lead"]))));
        assert_eq!(lead_model(&bench), "acme/b");
        let notes = told(&bench);
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("usage limit reached") && notes[0].contains("costs more"),
            "{notes:?}"
        );
        // The profile that took over hits a limit too: no second switch, the error stands.
        assert_eq!(hit(&bench, &limit(7200)), None);
        assert_eq!(lead_model(&bench), "acme/b");
        assert_eq!(told(&bench).len(), 1);
        assert_eq!(bench.switcher.active().unwrap().from.as_deref(), Some("a"));
    }

    #[test]
    fn nothing_switches_without_a_usable_on_limit_or_for_throttling() {
        let off = bench("", ProfileReset::Ask, None, |_| true);
        assert_eq!(hit(&off, &limit(7200)), None);

        let unknown = bench("nope", ProfileReset::Ask, None, |_| true);
        assert_eq!(hit(&unknown, &limit(7200)), None);

        let no_login = bench("b", ProfileReset::Ask, None, |id| id != "acme");
        assert_eq!(hit(&no_login, &limit(7200)), None);
        assert_eq!(lead_model(&no_login), "acme/a");
        assert!(
            told(&no_login)[0].contains("needs a login"),
            "{:?}",
            told(&no_login)
        );

        let same = bench("a", ProfileReset::Ask, Some("a"), |_| true);
        assert_eq!(hit(&same, &limit(7200)), None);

        let sub_agent = bench("b", ProfileReset::Ask, None, |_| true);
        assert_eq!(
            sub_agent
                .switcher
                .on_limit(&agent(1), &chain(&["@lead"]), &limit(7200)),
            None
        );

        // Throttling and overload have no reset window, so they are the router's to handle.
        let busy = bench("b", ProfileReset::Ask, None, |_| true);
        for error in [
            AiError::RateLimit {
                retry_after: Some(Duration::from_secs(1)),
            },
            AiError::RateLimit { retry_after: None },
            AiError::Overloaded,
        ] {
            assert_eq!(hit(&busy, &error), None, "{error:?}");
        }
        assert_eq!(lead_model(&busy), "acme/a");
        assert!(told(&busy).is_empty());
        // A 429 that asks for hours is a limit.
        let long = AiError::RateLimit {
            retry_after: Some(Duration::from_secs(7200)),
        };
        assert!(hit(&busy, &long).is_some());
    }

    #[test]
    fn a_profile_with_the_same_models_is_not_a_way_out() {
        let bench = bench("a", ProfileReset::Ask, None, |_| true);
        assert_eq!(hit(&bench, &limit(7200)), None);
    }

    #[test]
    fn ask_only_tells_the_user_when_the_window_has_reset() {
        let bench = bench("b", ProfileReset::Ask, Some("a"), |_| true);
        hit(&bench, &limit(7200)).unwrap();
        bench.switcher.at_turn_boundary();
        assert_eq!(told(&bench).len(), 1, "not before the reset");
        bench.ahead.store(7201, Ordering::SeqCst);
        bench.switcher.at_turn_boundary();
        let notes = told(&bench);
        assert_eq!(notes.len(), 2);
        assert!(
            notes[1].contains("has reset") && notes[1].contains("/profile a"),
            "{notes:?}"
        );
        assert_eq!(lead_model(&bench), "acme/b");
        // Said once.
        bench.switcher.at_turn_boundary();
        assert_eq!(told(&bench).len(), 2);
    }

    #[test]
    fn auto_goes_back_after_the_reset_and_never_before() {
        let bench = bench("b", ProfileReset::Auto, Some("a"), |_| true);
        hit(&bench, &limit(7200)).unwrap();
        bench.ahead.store(7100, Ordering::SeqCst);
        bench.switcher.at_turn_boundary();
        assert_eq!(lead_model(&bench), "acme/b");
        bench.ahead.store(7201, Ordering::SeqCst);
        bench.switcher.at_turn_boundary();
        assert_eq!(lead_model(&bench), "acme/a");
        assert!(bench.switcher.active().is_none());
        assert!(told(&bench)[1].contains("went back"), "{:?}", told(&bench));
    }

    #[test]
    fn auto_restores_the_earlier_models_when_there_was_no_named_profile() {
        let bench = bench("b", ProfileReset::Auto, None, |_| true);
        bench.router.set_role("default", chain(&["acme/mine"]));
        hit(&bench, &limit(3600)).unwrap();
        assert_eq!(lead_model(&bench), "acme/b");
        bench.ahead.store(3601, Ordering::SeqCst);
        bench.switcher.at_turn_boundary();
        assert_eq!(lead_model(&bench), "acme/mine");
    }

    fn guessed() -> AiError {
        AiError::Quota {
            resets_at: None,
            message: "out of extra usage".to_string(),
        }
    }

    #[test]
    fn auto_asks_instead_of_going_back_when_the_reset_time_was_a_guess() {
        let bench = bench("b", ProfileReset::Auto, Some("a"), |_| true);
        hit(&bench, &guessed()).unwrap();
        assert!(!bench.switcher.active().unwrap().known);
        bench.ahead.store(15 * 60 + 1, Ordering::SeqCst);
        bench.switcher.at_turn_boundary();
        assert_eq!(lead_model(&bench), "acme/b");
        let notes = told(&bench);
        assert!(
            notes[1].contains("may have reset") && notes[1].contains("/profile a"),
            "{notes:?}"
        );
        assert!(bench.switcher.active().is_none());
    }

    #[test]
    fn a_model_picked_by_hand_after_the_switch_stays_when_auto_would_go_back() {
        let bench = bench("b", ProfileReset::Auto, Some("a"), |_| true);
        hit(&bench, &limit(3600)).unwrap();
        // The lead was left on `@lead`; `/model x` moved it elsewhere.
        let picked = Arc::new(Mutex::new(Some(chain(&["acme/x"]))));
        let seen = picked.clone();
        let Bench {
            switcher,
            router,
            notes,
            ahead,
        } = bench;
        let bench = Bench {
            switcher: switcher.with_lead_chain(Arc::new(move || seen.lock().unwrap().clone())),
            router,
            notes,
            ahead,
        };
        bench.ahead.store(3601, Ordering::SeqCst);
        bench.switcher.at_turn_boundary();
        assert_eq!(lead_model(&bench), "acme/b");
        assert_eq!(told(&bench).len(), 1, "nothing said, nothing changed");

        // On `@lead` still, the way back is taken.
        let again = bench_with_lead_on_role();
        hit(&again, &limit(3600)).unwrap();
        again.ahead.store(3601, Ordering::SeqCst);
        again.switcher.at_turn_boundary();
        assert_eq!(lead_model(&again), "acme/a");
    }

    fn bench_with_lead_on_role() -> Bench {
        let bench = bench("b", ProfileReset::Auto, Some("a"), |_| true);
        let Bench {
            switcher,
            router,
            notes,
            ahead,
        } = bench;
        Bench {
            switcher: switcher.with_lead_chain(Arc::new(|| Some(chain(&["@lead"])))),
            router,
            notes,
            ahead,
        }
    }

    #[test]
    fn going_back_to_no_named_profile_is_written_into_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let session = Arc::new(SessionStore::create(dir.path(), dir.path(), None).unwrap());
        let bench = bench("b", ProfileReset::Auto, None, |_| true);
        bench.switcher.services.insert(session.clone());
        hit(&bench, &limit(3600)).unwrap();
        // What the switch records when there is a lead.
        session.append(EntryKind::ProfileChange {
            profile: "b".to_string(),
        });
        assert_eq!(session.last_profile(None).as_deref(), Some("b"));
        bench.ahead.store(3601, Ordering::SeqCst);
        bench.switcher.at_turn_boundary();
        assert_eq!(lead_model(&bench), "acme/a");
        assert_eq!(session.last_profile(None), None);
        let book = crate::profiles::Book::new(Vec::new(), Vec::new(), Arc::new(|_: &str| Access::Ready), None);
        assert_eq!(book.current(&session), None);
    }

    #[test]
    fn off_stays_and_a_new_window_may_switch_again() {
        let off = bench("b", ProfileReset::Off, Some("a"), |_| true);
        hit(&off, &limit(60)).unwrap();
        off.ahead.store(61, Ordering::SeqCst);
        off.switcher.at_turn_boundary();
        assert_eq!(lead_model(&off), "acme/b");
        assert_eq!(told(&off).len(), 1);
        assert!(off.switcher.active().is_none());

        let again = bench("b", ProfileReset::Auto, Some("a"), |_| true);
        hit(&again, &limit(60)).unwrap();
        again.ahead.store(61, Ordering::SeqCst);
        again.switcher.at_turn_boundary();
        assert_eq!(lead_model(&again), "acme/a");
        assert!(hit(&again, &limit(7200)).is_some());
    }
}
