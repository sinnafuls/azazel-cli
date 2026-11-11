//! Activity spinner.

use std::time::Duration;

/// Time between spinner frames. A redraw timer is only needed while a spinner is visible.
pub const INTERVAL: Duration = Duration::from_millis(80);

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct Spinner;

impl Spinner {
    pub const INTERVAL: Duration = INTERVAL;

    /// The glyph for a tick; one column wide. Ticks advance once per [`INTERVAL`].
    pub fn frame(tick: u64) -> &'static str {
        let index = usize::try_from(tick % 10).unwrap_or_default();
        FRAMES[index % FRAMES.len()]
    }

    /// The tick reached after `elapsed` time.
    pub fn tick_at(elapsed: Duration) -> u64 {
        u64::try_from(elapsed.as_millis() / INTERVAL.as_millis()).unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text;

    #[test]
    fn frames_cycle_and_are_one_column_wide() {
        assert_eq!(Spinner::INTERVAL, Duration::from_millis(80));
        let cycle: Vec<&str> = (0..10).map(Spinner::frame).collect();
        for (i, glyph) in cycle.iter().enumerate() {
            assert_eq!(text::width(glyph), 1);
            assert!(!cycle[..i].contains(glyph), "frame {i} repeats an earlier frame");
        }
        assert_eq!(Spinner::frame(10), Spinner::frame(0));
        assert_eq!(Spinner::frame(u64::MAX), Spinner::frame(u64::MAX % 10));
    }

    #[test]
    fn ticks_follow_elapsed_time() {
        assert_eq!(Spinner::tick_at(Duration::ZERO), 0);
        assert_eq!(Spinner::tick_at(Duration::from_millis(79)), 0);
        assert_eq!(Spinner::tick_at(Duration::from_millis(80)), 1);
        assert_eq!(Spinner::tick_at(Duration::from_millis(805)), 10);
    }
}
