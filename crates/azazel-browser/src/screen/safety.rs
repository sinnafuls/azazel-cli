//! What stands between the model and the user's mouse and keyboard: the lock between sessions,
//! the list of applications input may go to, and the Esc key.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_agent::ToolCtx;
use azazel_agent::agent::ABORTED_TEXT;
use azazel_core::ext::{Event, EventHandler, ExtCtx, Outcome};
use azazel_core::process::pid_alive;
use tokio_util::sync::CancellationToken;

use super::abort::{ABORTED_BY_ESC, Abort, sends_escape};
use super::hook::Monitor;
use super::lock::Lock;
use super::scope;

/// The actions that move the mouse or press keys. `focus` is not one: it changes which window
/// has the keyboard, and the next input action is checked against that window.
pub fn is_input(action: &str) -> bool {
    matches!(action, "move" | "click" | "drag" | "scroll" | "key" | "type")
}

pub struct Safety {
    lock_file: PathBuf,
    allow_apps: Vec<String>,
    abort: Arc<Abort>,
    lock: Arc<Mutex<Option<Lock>>>,
    monitor: Mutex<Option<Monitor>>,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn release(lock: &Mutex<Option<Lock>>) {
    if let Some(held) = locked(lock).take() {
        held.release();
    }
}

impl Safety {
    /// `home` is the azazel home, where the lock file lives; `allow_apps` is `[screen] allow_apps`.
    pub fn new(home: &std::path::Path, allow_apps: Vec<String>) -> Safety {
        Safety {
            lock_file: home.join("screen.lock"),
            allow_apps,
            abort: Arc::default(),
            lock: Arc::default(),
            monitor: Mutex::new(None),
        }
    }

    /// Everything that must hold before the next mouse or keyboard input of the run `cancel` of
    /// `agent` in `session`; takes the lock and starts watching for Esc on the first one.
    fn check(&self, session: &str, agent: &str, cancel: &CancellationToken) -> Result<(), String> {
        if self.abort.is_aborted() {
            return Err(ABORTED_BY_ESC.to_string());
        }
        if cancel.is_cancelled() {
            return Err(ABORTED_TEXT.to_string());
        }
        if !self.allow_apps.is_empty() {
            let names = scope::foreground()?;
            if !scope::permits(&self.allow_apps, &names) {
                return Err(format!(
                    "input refused: the application in the foreground is `{}`, and `[screen] allow_apps` \
                     lists only {}; focus one of them first",
                    names.first().map_or("unknown", String::as_str),
                    self.allow_apps.join(", ")
                ));
            }
        }
        {
            let mut held = locked(&self.lock);
            if held.is_none() {
                *held = Some(Lock::acquire(&self.lock_file, session, pid_alive)?);
            }
        }
        self.arm()?;
        self.abort.watch(agent, cancel);
        Ok(())
    }

    /// Starts watching the keyboard for the user's Esc.
    fn arm(&self) -> Result<(), String> {
        let mut monitor = locked(&self.monitor);
        if monitor.is_some() {
            return Ok(());
        }
        let (abort, lock) = (self.abort.clone(), self.lock.clone());
        let started = Monitor::start(Arc::new(move || {
            // The lock goes at once: whoever aborts has taken the desktop back.
            if abort.escape_pressed(Instant::now()) {
                release(&lock);
            }
        }));
        match started {
            Ok(started) => *monitor = Some(started),
            Err(why) if cfg!(windows) => {
                release(&self.lock);
                return Err(format!(
                    "screen input is off, as the Esc key cannot be watched: {why}"
                ));
            }
            Err(why) => tracing::debug!("screen: {why}"),
        }
        Ok(())
    }

    /// Stops watching the keyboard and lets go of the lock; an abort stays until the user asks
    /// for something new.
    pub fn idle(&self) {
        *locked(&self.monitor) = None;
        release(&self.lock);
    }

    /// The user asked for something new, or the session ended: an abort is over.
    pub fn reset(&self) {
        self.idle();
        self.abort.reset();
    }
}

/// What one `screen` call needs while it acts.
pub struct Run {
    safety: Arc<Safety>,
    cancel: CancellationToken,
    session: String,
    agent: String,
}

impl Run {
    pub fn new(safety: &Arc<Safety>, ctx: &ToolCtx) -> Run {
        Run {
            safety: safety.clone(),
            cancel: ctx.cancel.clone(),
            session: ctx.session_id.clone(),
            agent: ctx.agent_id.clone(),
        }
    }

    /// Whether the call must stop: the run was cancelled or the user pressed Esc.
    pub fn stopped(&self) -> bool {
        self.cancel.is_cancelled() || self.safety.abort.is_aborted()
    }

    /// The error that says why the call stopped.
    pub fn why_stopped(&self) -> String {
        if self.safety.abort.is_aborted() {
            ABORTED_BY_ESC.to_string()
        } else {
            ABORTED_TEXT.to_string()
        }
    }

    /// Checks that input may go ahead (see `Safety::check`).
    pub fn allow_input(&self) -> Result<(), String> {
        self.safety.check(&self.session, &self.agent, &self.cancel)
    }

    /// Sleeps for `time`, in short steps; returns early, with `true`, when the call must stop.
    pub fn pause(&self, time: Duration) -> bool {
        let end = Instant::now() + time;
        loop {
            if self.stopped() {
                return true;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            std::thread::sleep(left.min(Duration::from_millis(20)));
        }
    }

    /// The tool is about to press `keys` for `hold`: if that is Esc, it is not the user's.
    pub fn expect_escape(&self, keys: &str, hold: Duration) {
        if sends_escape(keys) {
            self.safety.abort.expect_escape(hold, Instant::now());
        }
    }
}

/// Lets go of the desktop when an agent finishes or the session ends, and lifts an abort at the
/// user's next request.
pub struct Lifecycle(pub Arc<Safety>);

#[async_trait]
impl EventHandler for Lifecycle {
    async fn handle(&self, ev: &mut Event, _: &ExtCtx) -> Outcome {
        match ev {
            Event::SessionEnd | Event::UserPrompt { .. } => self.0.reset(),
            Event::Stop { .. } => self.0.idle(),
            _ => {}
        }
        Outcome::Continue
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn only_pointer_and_keyboard_actions_count_as_input() {
        for action in ["move", "click", "drag", "scroll", "key", "type"] {
            assert!(is_input(action), "{action}");
        }
        for action in ["windows", "capture", "focus", "wait", "record"] {
            assert!(!is_input(action), "{action}");
        }
    }

    #[test]
    fn after_the_users_escape_input_fails_until_the_next_request() {
        let home = tempfile::tempdir().unwrap();
        let safety = Safety::new(home.path(), Vec::new());
        let run = CancellationToken::new();
        safety.abort.watch("lead", &run);
        assert!(safety.abort.escape_pressed(Instant::now()));
        assert!(run.is_cancelled());

        let error = safety.check("s1", "lead", &CancellationToken::new()).unwrap_err();
        assert!(error.contains("pressed Esc"), "{error}");
        // Nothing was locked by the refused call.
        assert!(!home.path().join("screen.lock").exists());

        safety.reset();
        assert!(!safety.abort.is_aborted());
    }

    #[test]
    fn a_session_blocked_by_another_is_told_which() {
        let home = tempfile::tempdir().unwrap();
        // A lock of another session whose process (this one) runs.
        let other = Safety::new(home.path(), Vec::new());
        *locked(&other.lock) =
            Some(Lock::acquire(&home.path().join("screen.lock"), "other-session", pid_alive).unwrap());

        let mine = Safety::new(home.path(), Vec::new());
        let error = mine.check("mine", "lead", &CancellationToken::new()).unwrap_err();
        assert!(error.contains("other-session"), "{error}");

        // Letting go frees it for the next session.
        other.reset();
        assert!(!home.path().join("screen.lock").exists());
    }

    #[test]
    fn a_cancelled_run_takes_no_lock() {
        let home = tempfile::tempdir().unwrap();
        let safety = Safety::new(home.path(), Vec::new());
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(safety.check("s1", "lead", &cancel).unwrap_err(), ABORTED_TEXT);
        assert!(!home.path().join("screen.lock").exists());
    }

    #[test]
    fn a_pause_ends_early_when_the_run_is_cancelled() {
        let home = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let run = Run {
            safety: Arc::new(Safety::new(home.path(), Vec::new())),
            cancel: cancel.clone(),
            session: "s1".to_string(),
            agent: "lead".to_string(),
        };
        assert!(!run.pause(Duration::from_millis(10)));
        cancel.cancel();
        let begun = Instant::now();
        assert!(run.pause(Duration::from_secs(30)));
        assert!(begun.elapsed() < Duration::from_secs(5));
    }
}
