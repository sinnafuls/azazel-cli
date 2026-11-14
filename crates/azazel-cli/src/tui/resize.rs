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

    /// The size the terminal has now, asked before a frame. A change it has not reported yet
    /// counts as reported now; asking again at the size already waiting does not start the wait
    /// again, or frames that keep coming would keep the size from ever settling.
    pub(super) fn check(&mut self, size: (u16, u16), drawn: (u16, u16), now: Instant) {
        let expected = self.pending.map_or(drawn, |(waiting, _)| waiting);
        if size != expected {
            self.note(size, now);
        }
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

/// A Windows console tells a program that its window changed size only when the program asks
/// for it (`ENABLE_WINDOW_INPUT`), which crossterm's raw mode does not. Windows Terminal holds
/// to that, so without this not one size change reaches the UI there, and it goes on drawing
/// at the size it started with. True when the flag was off and is on now: the caller turns it
/// off again when it gives the terminal back.
#[cfg(windows)]
pub(super) fn hear_sizes() -> bool {
    let Some((console, mode)) = console_input() else {
        return false;
    };
    mode & ENABLE_WINDOW_INPUT == 0 && console.set_mode(mode | ENABLE_WINDOW_INPUT).is_ok()
}

/// Puts back what [`hear_sizes`] changed.
#[cfg(windows)]
pub(super) fn stop_hearing_sizes() {
    if let Some((console, mode)) = console_input() {
        let _ = console.set_mode(mode & !ENABLE_WINDOW_INPUT);
    }
}

#[cfg(windows)]
const ENABLE_WINDOW_INPUT: u32 = 0x0008;

#[cfg(windows)]
fn console_input() -> Option<(crossterm_winapi::ConsoleMode, u32)> {
    use crossterm_winapi::{ConsoleMode, Handle};
    let console = ConsoleMode::from(Handle::current_in_handle().ok()?);
    let mode = console.mode().ok()?;
    Some((console, mode))
}

#[cfg(not(windows))]
pub(super) fn hear_sizes() -> bool {
    false
}

#[cfg(not(windows))]
pub(super) fn stop_hearing_sizes() {}

impl Tui {
    /// Takes in a change of the window's size once it has settled; at once when the UI is
    /// ending, which has one frame left to draw. True when the window is at rest and can be
    /// drawn in.
    ///
    /// The size is asked for before every frame as well: the terminal re-wraps the screen the
    /// moment it changes size, and its report arrives a little later. A frame drawn at the old
    /// size in between (the spinner turns many times a second) would land on rows the
    /// re-wrap has moved, and leave pieces of the old region on the screen.
    pub(super) fn settle_resize(&mut self) -> bool {
        self.ask_size();
        self.settle(self.quit)
    }

    fn ask_size(&mut self) {
        if let Ok(size) = crossterm::terminal::size() {
            self.resize.check(size, self.renderer.size(), Instant::now());
        }
    }

    /// The terminal reported a size.
    pub(super) fn size_reported(&mut self, reported: (u16, u16)) {
        // The report is not what counts: a Windows console reports the size of its buffer,
        // one more than the window in both directions.
        let size = crossterm::terminal::size().unwrap_or(reported);
        self.resize.note(size, Instant::now());
    }

    fn settle(&mut self, force: bool) -> bool {
        let Some(reported) = self.resize.settled(Instant::now(), force) else {
            return !self.resize.pending();
        };
        let (width, height) = crossterm::terminal::size().unwrap_or(reported);
        self.renderer.resize(width, height);
        self.floor.resize(height, self.renderer.rows_on_screen());
        self.anchor_floor();
        true
    }

    /// The live region has just been taken off the screen ([`Renderer::suspend`]) and the
    /// alternate screen is about to be shown: remembers the row the region began on and the
    /// width of the screen, for [`Tui::back_on_main`].
    ///
    /// [`Renderer::suspend`]: azazel_tui::Renderer::suspend
    pub(super) fn leaving_main(&mut self) {
        if self.guard.in_alt() {
            return;
        }
        let (width, height) = self.renderer.size();
        let top = match cursor_row() {
            Some(row) => usize::from(row),
            None => usize::from(height).saturating_sub(self.floor.rows()),
        };
        self.alt_from = Some((width, top));
    }

    /// The alternate screen has just been left. A window that changed width meanwhile has
    /// had its transcript re-wrapped, which may have put the cursor inside it; the region
    /// begins below it again, at the size the window has now.
    pub(super) fn back_on_main(&mut self) {
        self.ask_size();
        self.settle(true);
        if let Some((width, top)) = self.alt_from.take() {
            let _ = self.renderer.follow_rewrap(self.backdrop.rows(), top, width);
        }
        self.anchor_floor();
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

    #[test]
    fn a_size_asked_for_before_each_frame_is_waited_for_once() {
        let start = Instant::now();
        let at = |millis: u64| start + Duration::from_millis(millis);
        let mut resize = Resize::default();
        // The size the UI is drawn at: nothing to wait for.
        resize.check((80, 24), (80, 24), at(0));
        assert!(!resize.pending());

        // The window changed before the terminal said so: the frame that asks waits.
        resize.check((60, 24), (80, 24), at(10));
        assert_eq!(resize.deadline(), Some(at(10) + SETTLE));
        // Frames keep asking at the same size, faster than the wait: the wait does not start
        // again, and the size settles.
        for millis in [20, 40, 60] {
            resize.check((60, 24), (80, 24), at(millis));
        }
        assert_eq!(resize.settled(at(70), false), Some((60, 24)));

        // A further change while one waits starts the wait again.
        resize.check((50, 20), (60, 24), at(100));
        resize.check((40, 20), (60, 24), at(120));
        assert_eq!(resize.settled(at(170), false), None);
        assert_eq!(resize.settled(at(180), false), Some((40, 20)));
    }
}
