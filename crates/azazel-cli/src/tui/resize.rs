//! Changes of the window's size: dragging its edge, or zooming the font, which changes how many
//! rows and columns fit.
//!
//! A drag reports dozens of sizes. Drawing at each would repaint the live region as often,
//! each time on a screen the terminal is about to re-wrap again; so the UI waits until the size
//! has stopped changing for a moment and draws once, at the size the window has then.

use std::time::{Duration, Instant};

use super::Tui;

/// How long the size has to stay as it is before the UI draws at it.
const SETTLE: Duration = Duration::from_millis(60);

/// A change of size that has been reported and not drawn at yet.
#[derive(Default)]
pub(super) struct Resize {
    /// The size last reported, and when it counts as settled.
    pending: Option<((u16, u16), Instant)>,
}

impl Resize {
    /// The terminal reported a size. Each report starts the wait again.
    pub(super) fn note(&mut self, size: (u16, u16), now: Instant) {
        self.pending = Some((size, now + SETTLE));
    }

    /// Whether a reported size is still waiting to settle.
    pub(super) fn pending(&self) -> bool {
        self.pending.is_some()
    }

    /// When the pending size counts as settled.
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.pending.map(|(_, at)| at)
    }

    /// The size to draw at, once: when the last report is old enough, or at once with `force`.
    pub(super) fn settled(&mut self, now: Instant, force: bool) -> Option<(u16, u16)> {
        let (size, at) = self.pending?;
        (force || now >= at).then(|| {
            self.pending = None;
            size
        })
    }
}

/// The row of the window the terminal cursor is on, counted from the top, where that can be
/// learnt without waiting for the terminal to answer: a Windows console. Elsewhere the reply
/// would arrive among the keys, some time later.
#[cfg(windows)]
fn cursor_row() -> Option<u16> {
    let info = crossterm_winapi::ScreenBuffer::current().ok()?.info().ok()?;
    u16::try_from(info.cursor_pos().y - info.terminal_window().top).ok()
}

#[cfg(not(windows))]
fn cursor_row() -> Option<u16> {
    None
}

impl Tui {
    /// Takes in a change of the window's size once it has settled; at once when the UI is
    /// ending, which has one frame left to draw. True when the window is at rest and can be
    /// drawn in.
    pub(super) fn settle_resize(&mut self) -> bool {
        let Some(reported) = self.resize.settled(Instant::now(), self.quit) else {
            return !self.resize.pending();
        };
        // The report is not what counts: a Windows console reports the size of its buffer,
        // one more than the window in both directions.
        let (width, height) = crossterm::terminal::size().unwrap_or(reported);
        self.renderer.resize(width, height);
        self.floor.resize(height, self.renderer.rows_on_screen());
        self.anchor_floor();
        true
    }

    /// Where the terminal can say where its cursor is, the live region gets exactly the rows
    /// between its first row and the bottom of the window, whatever the terminal did with the
    /// transcript above it (re-wrapped it, brought rows back from the scrollback, or neither).
    pub(super) fn anchor_floor(&mut self) {
        // On the alternate screen the cursor says nothing about the transcript.
        if self.guard.in_alt() {
            return;
        }
        let Some(row) = cursor_row() else {
            return;
        };
        let (_, height) = self.renderer.size();
        let top = usize::from(row).saturating_sub(self.renderer.rows_above_cursor());
        self.floor.anchor(usize::from(height).saturating_sub(top));
    }

    /// Another program had the terminal for a while and the window may have changed size
    /// meanwhile, unheard: what it is now is taken in like a reported size.
    pub(super) fn recheck_size(&mut self) {
        if let Ok(size) = crossterm::terminal::size()
            && size != self.renderer.size()
        {
            self.resize.note(size, Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_of_sizes_settles_on_the_last_one_after_the_window_has_been_still() {
        let start = Instant::now();
        let at = |millis: u64| start + Duration::from_millis(millis);
        let mut resize = Resize::default();
        assert!(!resize.pending());
        assert_eq!(resize.deadline(), None);
        assert_eq!(resize.settled(at(1000), false), None);

        // A drag: a size every few milliseconds. None of them is drawn at while they keep coming.
        for (step, width) in (100u16..110).enumerate() {
            let now = at(step as u64 * 16);
            assert_eq!(resize.settled(now, false), None);
            resize.note((width, 30), now);
            assert!(resize.pending());
        }
        // The last one arrived at 144 ms: it settles 60 ms later, not before.
        assert_eq!(resize.deadline(), Some(at(144) + SETTLE));
        assert_eq!(resize.settled(at(203), false), None);
        assert_eq!(resize.settled(at(204), false), Some((109, 30)));
        // It is handed out once.
        assert!(!resize.pending());
        assert_eq!(resize.settled(at(500), false), None);

        // A UI that is ending does not wait.
        resize.note((80, 24), at(600));
        assert_eq!(resize.settled(at(600), true), Some((80, 24)));
        assert!(!resize.pending());
    }
}
