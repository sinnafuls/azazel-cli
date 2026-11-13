//! How fast the lead writes, for the status line.
//!
//! A message is timed from its first text or thinking to its end. While it streams, the speed
//! is an estimate from the characters that have arrived; when it ends, the tokens the provider
//! counted (reasoning included) take the estimate's place.

use std::time::{Duration, Instant};

/// Characters that make one token, near enough for an estimate.
const CHARS_PER_TOKEN: f64 = 4.0;
/// A message that has streamed for less than this has no speed to speak of yet.
const SETTLE: Duration = Duration::from_millis(300);
/// Nor has one that has said less than this many characters: a dozen tokens.
const SETTLE_CHARS: u64 = 48;
/// A message that arrived whole was not streamed: there is nothing to time.
const SHORTEST: Duration = Duration::from_millis(50);

#[derive(Debug, Default)]
pub(super) struct Rate {
    /// When the message being streamed first said something, and the characters it has sent.
    stream: Option<(Instant, u64)>,
    /// Tokens per second of the last message that ended.
    last: Option<f64>,
}

impl Rate {
    /// A new prompt: the speed of the turn before it is no longer shown.
    pub fn reset(&mut self) {
        *self = Rate::default();
    }

    /// Text or thinking of the message being streamed arrived at `now`; the first of it starts
    /// the clock.
    pub fn text(&mut self, delta: &str, now: Instant) {
        let (_, chars) = self.stream.get_or_insert((now, 0));
        *chars += delta.chars().count() as u64;
    }

    /// Arguments of a tool call arrived. They count once the clock runs, and do not start it.
    pub fn arguments(&mut self, delta: &str) {
        if let Some((_, chars)) = &mut self.stream {
            *chars += delta.chars().count() as u64;
        }
    }

    /// The message ended at `now` with `output` tokens; 0 when the provider did not count.
    pub fn end(&mut self, output: u64, now: Instant) {
        let Some((started, chars)) = self.stream.take() else {
            return;
        };
        let took = now.saturating_duration_since(started);
        if took < SHORTEST {
            return;
        }
        let tokens = if output > 0 {
            output as f64
        } else {
            chars as f64 / CHARS_PER_TOKEN
        };
        self.last = Some(tokens / took.as_secs_f64());
    }

    /// The message being streamed is given up (a retry, a rule): it is not measured.
    pub fn abandon(&mut self) {
        self.stream = None;
    }

    /// Tokens per second to show at `now`: the estimate for the message being streamed once it
    /// has run for a moment, else what the last finished message measured.
    pub fn current(&self, now: Instant) -> Option<f64> {
        match self.stream {
            Some((started, chars))
                if chars >= SETTLE_CHARS && now.saturating_duration_since(started) >= SETTLE =>
            {
                let took = now.saturating_duration_since(started).as_secs_f64();
                Some(chars as f64 / CHARS_PER_TOKEN / took)
            }
            _ => self.last,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_timed_from_its_first_delta_and_counted_in_the_providers_tokens() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut rate = Rate::default();
        assert_eq!(rate.current(at(0)), None);

        // The request went out at 0; the model starts to think two seconds later. 400
        // characters every half second are 200 estimated tokens a second.
        rate.text(&"t".repeat(400), at(2000));
        assert_eq!(rate.current(at(2100)), None, "too early to say");
        rate.text(&"t".repeat(400), at(2500));
        assert_eq!(rate.current(at(2500)), Some(400.0));
        rate.text(&"x".repeat(400), at(3000));
        assert_eq!(rate.current(at(3000)), Some(300.0));
        // Arguments of a tool call are output like any other.
        rate.arguments(&"a".repeat(800));
        assert_eq!(rate.current(at(4000)), Some(250.0));

        // The provider counted 390 tokens for the four seconds since the first delta: the
        // wait before it is not part of the speed.
        rate.end(390, at(6000));
        assert_eq!(rate.current(at(6000)), Some(97.5));
        assert_eq!(rate.current(at(60_000)), Some(97.5), "it stays after the turn");

        // The next message of the turn shows the last value until it has one of its own: a
        // moment and a few words are not enough to estimate from.
        rate.text(&"y".repeat(40), at(7000));
        assert_eq!(rate.current(at(7100)), Some(97.5));
        assert_eq!(
            rate.current(at(7900)),
            Some(97.5),
            "forty characters say nothing yet"
        );
        rate.text(&"y".repeat(360), at(8000));
        assert_eq!(rate.current(at(8000)), Some(100.0));
        rate.end(150, at(8500));
        assert_eq!(rate.current(at(9000)), Some(100.0));

        // A new prompt starts from nothing.
        rate.reset();
        assert_eq!(rate.current(at(9000)), None);
    }

    #[test]
    fn what_was_not_streamed_or_not_finished_is_not_measured() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut rate = Rate::default();

        // A tool call alone does not start the clock.
        rate.arguments("{\"path\": \"a.rs\"}");
        rate.end(40, at(900));
        assert_eq!(rate.current(at(900)), None);

        // A reply that arrives in one piece has no duration to divide by.
        rate.text("the whole reply at once", at(1000));
        rate.end(500, at(1001));
        assert_eq!(rate.current(at(1001)), None);

        // A reply that is withdrawn half way leaves the last measurement alone.
        rate.text(&"a".repeat(800), at(2000));
        rate.end(100, at(3000));
        assert_eq!(rate.current(at(3000)), Some(100.0));
        rate.text(&"b".repeat(4000), at(4000));
        rate.abandon();
        assert_eq!(rate.current(at(9000)), Some(100.0));

        // Without a count from the provider the characters stand in for it.
        rate.text(&"c".repeat(2000), at(10_000));
        rate.end(0, at(12_000));
        assert_eq!(rate.current(at(12_000)), Some(250.0));
    }
}
