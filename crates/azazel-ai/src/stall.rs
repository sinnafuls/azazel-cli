//! The stall watchdog: how long a response may stay silent before the attempt is given up.

use std::time::Duration;

use tokio::time::Instant;

use crate::error::AiError;

/// How long a provider may send nothing before a request counts as stalled. `None` waits for as
/// long as the connection stays open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StallLimits {
    /// From sending the request to the first event of the response.
    pub first_event: Option<Duration>,
    /// Between two events of a response that has begun.
    pub stall: Option<Duration>,
}

impl StallLimits {
    /// The limits in seconds, as the configuration gives them; 0 turns one off.
    pub fn from_secs(first_event: u64, stall: u64) -> Self {
        let limit = |secs: u64| (secs > 0).then(|| Duration::from_secs(secs));
        StallLimits {
            first_event: limit(first_event),
            stall: limit(stall),
        }
    }
}

/// Watches one response: started when the request is sent, told of everything that arrives.
pub(crate) struct Watchdog {
    stall: Option<Duration>,
    /// When the response counts as stalled, and how long it will have been silent by then.
    due: Option<(Instant, Duration)>,
}

impl Watchdog {
    /// The request is being sent now.
    pub(crate) fn start(limits: StallLimits) -> Self {
        Watchdog {
            stall: limits.stall,
            due: limits.first_event.map(|limit| (Instant::now() + limit, limit)),
        }
    }

    /// Something of the response arrived.
    pub(crate) fn heard(&mut self) {
        self.due = self.stall.map(|limit| (Instant::now() + limit, limit));
    }

    /// Returns once the response has been silent for too long, with the error that ends the
    /// attempt; never returns when the limit in force is off.
    pub(crate) async fn expired(&self) -> AiError {
        match self.due {
            Some((at, limit)) => {
                tokio::time::sleep_until(at).await;
                AiError::Stalled {
                    secs: limit.as_secs().max(1),
                }
            }
            None => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_seconds_turns_a_limit_off() {
        assert_eq!(
            StallLimits::from_secs(20, 0),
            StallLimits {
                first_event: Some(Duration::from_secs(20)),
                stall: None,
            }
        );
        assert_eq!(StallLimits::from_secs(0, 0), StallLimits::default());
    }

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
}
