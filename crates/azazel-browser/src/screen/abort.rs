//! The user's way out: a press of Esc while the agent drives the desktop ends the turn. The key
//! press itself is observed by `hook`; what it means is decided here, without a desktop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

/// How long after an Esc the tool sends itself an Esc of the user's counts as the tool's: the
/// event can reach the hook a moment after the injection call returned.
const EXPECTED_SLACK: Duration = Duration::from_millis(150);

pub const ABORTED_BY_ESC: &str = "the user pressed Esc, which stops screen control for this turn; \
    do not send more mouse or keyboard input, and ask the user how to go on";

#[derive(Default)]
pub struct Abort {
    aborted: AtomicBool,
    /// Until when an Esc is the one the tool sends.
    expected_until: Mutex<Option<Instant>>,
    /// The token of the run each agent's last input action belongs to.
    runs: Mutex<HashMap<String, CancellationToken>>,
}

impl Abort {
    /// Whether the user has aborted since the last `reset`.
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    /// Forgets an abort and the runs it covered; the user has asked for something new.
    pub fn reset(&self) {
        self.aborted.store(false, Ordering::SeqCst);
        self.runs.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    /// The tool is about to send an Esc that is held for `hold`: it is not the user's.
    pub fn expect_escape(&self, hold: Duration, now: Instant) {
        *self.expected_until.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(now + hold + EXPECTED_SLACK);
    }

    /// Remembers the run `agent` is in, so that an abort ends it.
    pub fn watch(&self, agent: &str, run: &CancellationToken) {
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(agent.to_string(), run.clone());
    }

    /// An Esc went down at `now`. Returns whether it was the user's, which aborts every run
    /// watched.
    pub fn escape_pressed(&self, now: Instant) -> bool {
        let ours = self
            .expected_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some_and(|until| now <= until);
        if ours {
            return false;
        }
        self.aborted.store(true, Ordering::SeqCst);
        for run in self.runs.lock().unwrap_or_else(PoisonError::into_inner).values() {
            run.cancel();
        }
        true
    }
}

/// Whether `keys` (as the `key` action takes them) press Escape.
pub fn sends_escape(keys: &str) -> bool {
    crate::page::parse_keys(keys).is_ok_and(|key| key.key == "Escape")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_users_escape_aborts_and_cancels_the_watched_runs() {
        let abort = Abort::default();
        let run = CancellationToken::new();
        abort.watch("lead", &run);
        assert!(!abort.is_aborted());
        assert!(abort.escape_pressed(Instant::now()));
        assert!(abort.is_aborted());
        assert!(run.is_cancelled());

        // A new request of the user lifts it.
        abort.reset();
        assert!(!abort.is_aborted());
    }

    #[test]
    fn an_escape_the_tool_sends_does_not_abort() {
        let abort = Abort::default();
        let run = CancellationToken::new();
        abort.watch("lead", &run);
        let now = Instant::now();
        abort.expect_escape(Duration::from_millis(500), now);
        // While it is down, and just after it is let go.
        assert!(!abort.escape_pressed(now + Duration::from_millis(10)));
        assert!(!abort.escape_pressed(now + Duration::from_millis(600)));
        assert!(!abort.is_aborted() && !run.is_cancelled());
        // Later, an Esc is the user's again.
        assert!(abort.escape_pressed(now + Duration::from_millis(700)));
        assert!(abort.is_aborted() && run.is_cancelled());
    }

    #[test]
    fn escape_keys_are_recognised_with_modifiers_and_by_name() {
        assert!(sends_escape("Escape"));
        assert!(sends_escape("Control+Escape"));
        assert!(!sends_escape("Enter"));
        assert!(!sends_escape("e"));
        assert!(!sends_escape("Control+"));
    }
}
