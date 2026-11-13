//! `/loop` in the interactive UI: the prompt is sent again when the lead has finished a round,
//! until the loop has had its rounds, the condition holds, a round fails, or `esc` stops it.

use std::time::{Duration, Instant};

use azazel_core::side::Side;

use super::{Internal, Tui, view};
use crate::loops::{self, Loop, Next, Parsed, Until, Verdict};

/// Where a loop stands between two rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// The lead is at work on a round.
    Running,
    /// The next round goes out at this moment, or once the lead is idle after it.
    Waiting(Instant),
    /// The judge is being asked whether the condition holds.
    Judging,
}

pub(super) struct Looping {
    looped: Loop,
    state: State,
    /// Tells this loop's verdicts from those of one that was stopped meanwhile.
    id: u64,
}

/// A wait as a few words, rounded up to what a person reads: `45s`, `9m`.
fn wait_text(wait: Duration) -> String {
    let seconds = wait.as_secs().max(1);
    match seconds {
        s if s < 60 => format!("{s}s"),
        s => loops::span_text(Duration::from_secs(s.div_ceil(60) * 60)),
    }
}

impl Tui {
    /// `/loop`: starts a loop, stops it, or says where it stands.
    pub(super) async fn loop_command(&mut self, args: &str) {
        let parsed = match loops::parse(args, self.env.config.schedule.loop_max) {
            Ok(parsed) => parsed,
            Err(why) => return self.fail(&why),
        };
        let looped = match parsed {
            Parsed::Status => {
                let status = match &self.looping {
                    Some(looping) => format!(
                        "loop: {}{}; /loop stop or esc ends it",
                        looping.looped.describe(),
                        match looping.state {
                            State::Running => String::new(),
                            State::Judging => ", checking the condition".to_string(),
                            State::Waiting(at) => format!(
                                ", next round in {}",
                                wait_text(at.saturating_duration_since(Instant::now()))
                            ),
                        }
                    ),
                    None => format!("no loop is running\n{}", loops::USAGE),
                };
                return self.info(&status);
            }
            Parsed::Stop => {
                return match self.looping.is_some() {
                    true => self.stop_loop("loop stopped"),
                    false => self.info("no loop is running"),
                };
            }
            Parsed::Start(looped) => looped,
        };
        if self.looping.is_some() {
            return self.fail("a loop is running already; /loop stop ends it");
        }
        if matches!(looped.until, Until::Condition(_)) && !self.here_only("a loop with a condition") {
            return;
        }
        if !self.idle_to("start a loop") {
            return;
        }
        self.loop_serial += 1;
        self.looping = Some(Looping {
            looped,
            state: State::Running,
            id: self.loop_serial,
        });
        self.loop_send().await;
    }

    /// Ends the loop and says why.
    pub(super) fn stop_loop(&mut self, why: &str) {
        if self.looping.take().is_some() {
            self.info(why);
        }
    }

    /// Sends the loop's prompt as its next round.
    async fn loop_send(&mut self) {
        let Some(looping) = &mut self.looping else {
            return;
        };
        looping.looped.sent(Instant::now());
        looping.state = State::Running;
        let (prompt, about) = (looping.looped.prompt.clone(), looping.looped.describe());
        self.info(&format!("loop: {about} (esc stops it)"));
        let lines = view::user_prompt(&prompt, self.width(), &self.theme);
        self.say(lines);
        if let Err(reason) = self.backend.prompt(prompt, Vec::new()).await {
            self.fail(&format!("prompt rejected: {reason}"));
            self.stop_loop("loop stopped: its prompt was not taken");
        }
    }

    /// The lead has finished a run. `ok` is whether it ended by itself and without an error;
    /// `interlude` whether a queued message of the user went out in its place, whose run is
    /// waited for before the loop goes on.
    pub(super) async fn loop_after_run(&mut self, ok: bool, interlude: bool) {
        let Some(looping) = &self.looping else {
            return;
        };
        // A run that ended while the loop waited was the user's own.
        if looping.state != State::Running {
            return;
        }
        if !ok {
            return self.stop_loop("loop stopped: the round did not finish");
        }
        if interlude {
            return;
        }
        let next = looping.looped.after_round(Instant::now());
        self.loop_advance(next).await;
    }

    async fn loop_advance(&mut self, next: Next) {
        match next {
            // The user sent something of their own meanwhile: the round follows it.
            Next::Send if self.busy => self.loop_wait(Duration::ZERO),
            Next::Send => self.loop_send().await,
            Next::Wait(wait) => {
                if !wait.is_zero() {
                    self.info(&format!(
                        "loop: next round in {} (esc stops it)",
                        wait_text(wait)
                    ));
                }
                self.loop_wait(wait);
            }
            Next::Judge => self.loop_judge(),
            Next::Done(why) => {
                self.looping = None;
                self.info(&why);
            }
        }
    }

    fn loop_wait(&mut self, wait: Duration) {
        if let Some(looping) = &mut self.looping {
            looping.state = State::Waiting(Instant::now() + wait);
        }
    }

    /// When the main loop is to look at the loop again: the moment its next round is due,
    /// once the lead is idle.
    pub(super) fn loop_deadline(&self) -> Option<Instant> {
        match self.looping.as_ref()?.state {
            State::Waiting(at) if !self.busy => Some(at),
            _ => None,
        }
    }

    /// The moment the loop waited for has come.
    pub(super) async fn loop_tick(&mut self) {
        let due = matches!(
            self.looping.as_ref().map(|looping| looping.state),
            Some(State::Waiting(at)) if at <= Instant::now()
        );
        if due && !self.busy {
            self.loop_send().await;
        }
    }

    /// Asks the small model, beside the UI loop, whether the loop's condition holds.
    fn loop_judge(&mut self) {
        let Some(looping) = &mut self.looping else {
            return;
        };
        let Until::Condition(condition) = looping.looped.until.clone() else {
            return;
        };
        looping.state = State::Judging;
        let (id, prompt) = (looping.id, looping.looped.prompt.clone());
        let done = self.internal_tx.clone();
        let Some(local) = self.backend.local() else {
            let verdict = Err("the session does not run in this process".to_string());
            let _ = done.send(Internal::LoopVerdict { id, verdict });
            return;
        };
        let side = local.app.registry.services.get::<Side>();
        let (lead, session) = (local.active.lead.clone(), local.active.session.clone());
        self.notice = Some(format!("loop: checking \"{condition}\"…"));
        tokio::spawn(async move {
            let verdict = match side {
                Some(side) => {
                    let chain = lead.status().chain;
                    loops::judge(&side, lead.info(), chain, &session, &condition, &prompt).await
                }
                None => Err("no model is available in this session".to_string()),
            };
            let _ = done.send(Internal::LoopVerdict { id, verdict });
        });
    }

    /// The judge has answered.
    pub(super) async fn on_loop_verdict(&mut self, id: u64, verdict: Result<Verdict, String>) {
        let Some(looping) = &self.looping else {
            return;
        };
        if looping.id != id || looping.state != State::Judging {
            return;
        }
        let next = looping.looped.after_verdict(verdict.clone());
        self.notice = None;
        if let Ok(Verdict { met: false, why }) = &verdict {
            self.info(&format!("loop: not yet: {why}"));
        }
        self.loop_advance(next).await;
    }
}

#[cfg(feature = "detach")]
impl Tui {
    /// Watches the triggers of the project (`azazel schedule add --on …`) for as long as the
    /// UI has a session of this process open.
    pub(super) fn watch_triggers(&mut self) {
        let Some(local) = self.backend.local() else {
            return;
        };
        let Some((_, mut events)) = crate::schedule::triggers::watch_app(&local.app) else {
            return;
        };
        let done = self.internal_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if done.send(Internal::Trigger(event)).is_err() {
                    break;
                }
            }
        });
    }

    /// A trigger happened: its prompt goes to the lead like a typed one, after what the lead
    /// is doing.
    pub(super) async fn on_trigger(&mut self, event: crate::schedule::triggers::Event) {
        use crate::schedule::triggers::{Dispatch, Event, dispatch};
        let (schedule, text) = match event {
            Event::Fire { schedule, text } => (schedule, text),
            Event::Note(note) => return self.warn(&note),
        };
        let scheduler = self
            .backend
            .local()
            .and_then(|local| local.app.registry.services.get::<crate::schedule::Scheduler>());
        let Some(scheduler) = scheduler else {
            return;
        };
        match dispatch(&scheduler, &schedule, &text).await {
            Dispatch::Done(line) => self.info(&line),
            Dispatch::Here => {
                self.info(&format!("trigger {} ({})", schedule.id, schedule.when));
                if self.busy {
                    return self.queued.push(text, Vec::new());
                }
                let lines = view::user_prompt(&text, self.width(), &self.theme);
                self.say(lines);
                if let Err(reason) = self.backend.prompt(text, Vec::new()).await {
                    self.fail(&format!("prompt rejected: {reason}"));
                }
            }
        }
    }
}

#[cfg(not(feature = "detach"))]
impl Tui {
    pub(super) fn watch_triggers(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wait_is_said_in_seconds_or_whole_minutes() {
        let text = |seconds: u64| wait_text(Duration::from_secs(seconds));
        assert_eq!(
            [text(0), text(45), text(60), text(61), text(510), text(3600)],
            ["1s", "45s", "1m", "2m", "9m", "1h"]
        );
    }
}
