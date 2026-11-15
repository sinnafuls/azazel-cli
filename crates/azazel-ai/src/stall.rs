//! The stall watchdog: how long a response may stay silent, or take in all, before the attempt is
//! given up.

use std::time::Duration;

use tokio::time::Instant;

use crate::error::AiError;

/// How long a provider may send nothing before a request counts as stalled, and how long the
/// whole request may take. `None` waits for as long as the connection stays open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StallLimits {
    /// From sending the request to the first event of the response.
    pub first_event: Option<Duration>,
    /// Between two events of a response that has begun.
    pub stall: Option<Duration>,
    /// From sending the request to the end of the response.
    pub total: Option<Duration>,
}

impl StallLimits {
    /// The limits in seconds, as the configuration gives them; 0 turns one off.
    pub fn from_secs(first_event: u64, stall: u64) -> Self {
        StallLimits {
            first_event: limit(first_event),
            stall: limit(stall),
            total: None,
        }
    }
}

fn limit(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// One provider's own limits in seconds (`timeout_secs`, `first_event_secs`, `stall_secs` of
/// `[providers.<id>]`); `None` keeps the one of `[retry]`, and 0 turns it off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timeouts {
    pub total: Option<u64>,
    pub first_event: Option<u64>,
    pub stall: Option<u64>,
}

impl Timeouts {
    /// `global` with what this provider sets in its place.
    pub fn over(self, global: StallLimits) -> StallLimits {
        let pick = |own: Option<u64>, global: Option<Duration>| own.map_or(global, limit);
        StallLimits {
            first_event: pick(self.first_event, global.first_event),
            stall: pick(self.stall, global.stall),
            total: pick(self.total, global.total),
        }
    }
}

/// Watches one response: started when the request is sent, told of everything that arrives.
pub(crate) struct Watchdog {
    stall: Option<Duration>,
    /// When the response counts as stalled, and how long it will have been silent by then.
    due: Option<(Instant, Duration)>,
    /// When the whole request has taken too long, and how long that is.
    deadline: Option<(Instant, Duration)>,
}

impl Watchdog {
    /// The request is being sent now.
    pub(crate) fn start(limits: StallLimits) -> Self {
        let now = Instant::now();
        Watchdog {
            stall: limits.stall,
            due: limits.first_event.map(|limit| (now + limit, limit)),
            deadline: limits.total.map(|limit| (now + limit, limit)),
        }
    }

    /// Something of the response arrived.
    pub(crate) fn heard(&mut self) {
        self.due = self.stall.map(|limit| (Instant::now() + limit, limit));
    }

    /// Returns once the response has been silent, or has taken, too long, with the error that
    /// ends the attempt; never returns when the limits in force are off.
    pub(crate) async fn expired(&self) -> AiError {
        let silent = self.due.map(|(at, limit)| {
            let error = AiError::Stalled {
                secs: limit.as_secs().max(1),
            };
            (at, error)
        });
        let late = self.deadline.map(|(at, limit)| {
            let error = AiError::Network(format!(
                "no complete answer within {} s (the provider's timeout_secs)",
                limit.as_secs().max(1)
            ));
            (at, error)
        });
        match silent.into_iter().chain(late).min_by_key(|(at, _)| *at) {
            Some((at, error)) => {
                tokio::time::sleep_until(at).await;
                error
            }
            None => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_first_event_has_its_own_limit_and_each_later_one_restarts_the_clock() {
        let mut watchdog = Watchdog::start(StallLimits::from_secs(20, 120));
        let began = Instant::now();
        // Nothing arrives: the request is given up after the first-event limit.
        assert_eq!(watchdog.expired().await, AiError::Stalled { secs: 20 });
        assert_eq!(began.elapsed(), Duration::from_secs(20));

        // Once something arrived, the longer limit counts from the latest arrival.
        watchdog.heard();
        tokio::time::advance(Duration::from_secs(100)).await;
        watchdog.heard();
        let heard = Instant::now();
        assert_eq!(watchdog.expired().await, AiError::Stalled { secs: 120 });
        assert_eq!(heard.elapsed(), Duration::from_secs(120));
    }

    #[tokio::test(start_paused = true)]
    async fn a_limit_that_is_off_never_expires() {
        let mut watchdog = Watchdog::start(StallLimits::from_secs(0, 30));
        let waited = tokio::time::timeout(Duration::from_secs(3600), watchdog.expired()).await;
        assert!(waited.is_err(), "no first-event limit: {waited:?}");
        watchdog.heard();
        assert_eq!(watchdog.expired().await, AiError::Stalled { secs: 30 });

        let mut watchdog = Watchdog::start(StallLimits::from_secs(5, 0));
        watchdog.heard();
        let waited = tokio::time::timeout(Duration::from_secs(3600), watchdog.expired()).await;
        assert!(waited.is_err(), "no mid-stream limit: {waited:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_whole_request_ends_at_its_deadline_however_lively_it_is() {
        let limits = Timeouts {
            total: Some(30),
            ..Timeouts::default()
        }
        .over(StallLimits::from_secs(20, 10));
        let mut watchdog = Watchdog::start(limits);
        let began = Instant::now();
        for _ in 0..5 {
            tokio::time::advance(Duration::from_secs(5)).await;
            watchdog.heard();
        }
        let error = watchdog.expired().await;
        assert!(
            matches!(&error, AiError::Network(why) if why.contains("30 s")),
            "{error:?}"
        );
        assert_eq!(began.elapsed(), Duration::from_secs(30));
    }

    #[test]
    fn a_provider_s_own_limits_replace_the_global_ones_and_zero_turns_one_off() {
        let global = StallLimits::from_secs(20, 120);
        assert_eq!(Timeouts::default().over(global), global);
        let own = Timeouts {
            total: Some(600),
            first_event: Some(90),
            stall: Some(0),
        };
        assert_eq!(
            own.over(global),
            StallLimits {
                first_event: Some(Duration::from_secs(90)),
                stall: None,
                total: Some(Duration::from_secs(600)),
            }
        );
    }
}
