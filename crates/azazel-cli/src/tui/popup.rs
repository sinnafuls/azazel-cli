//! Popups: an open overlay, or the terminal of a program an agent runs, drawn as a centred box
//! over the whole window.
//!
//! The transcript lives in the terminal's scrollback, where rows cannot be drawn over and
//! brought back. So while an overlay is open the UI switches to the alternate screen and paints
//! a stand-in there: the last rows it wrote to the transcript, dimmed, with the box in the
//! middle and the working row and status line at the bottom. Nothing is written to the real
//! transcript meanwhile; what is said waits and reaches it when the popup closes and the
//! alternate screen is left, which shows the transcript exactly as it was.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use azazel_tui::components::composer;
use azazel_tui::modal::{self, Modal, Screen};
use azazel_tui::{HistoryItem, text};

use super::Tui;
use super::pager::{self, Source, Start};

/// How long the popup screen stays after its box closed. A menu that leads to another (the
/// settings) opens the next box within this time, so the screen does not flicker between them.
const LINGER: Duration = Duration::from_millis(120);

/// Rows of transcript kept for the backdrop: more than any window is tall.
const BACKDROP_ROWS: usize = 300;

/// The pinned rows of a frame with nothing but the input: a blank row, the input between its
/// two rules, and the status line.
pub(super) const PLAIN_PINNED_ROWS: usize = 5;

/// The last rows written to the transcript, as plain text, for painting behind a popup.
#[derive(Default)]
pub(super) struct Backdrop {
    rows: VecDeque<String>,
}

impl Backdrop {
    /// Remembers what a frame writes to the transcript.
    pub(super) fn record(&mut self, history: &[HistoryItem]) {
        let mut rows = Vec::new();
        for item in history {
            plain_rows(item, &mut rows);
        }
        let excess = (self.rows.len() + rows.len()).saturating_sub(BACKDROP_ROWS);
        self.rows.drain(..excess.min(self.rows.len()));
        let skip = rows.len().saturating_sub(BACKDROP_ROWS);
        self.rows.extend(rows.into_iter().skip(skip));
    }

    /// The screen was cleared: nothing is behind a popup any more.
    pub(super) fn clear(&mut self) {
        self.rows.clear();
    }

    /// The rows remembered, oldest first.
    pub(super) fn rows(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.rows.iter().map(String::as_str)
    }
}

/// A transcript item as the rows of plain text it takes; an image is blank rows.
fn plain_rows(item: &HistoryItem, out: &mut Vec<String>) {
    match item {
        HistoryItem::Line(line) => out.push(text::strip_ansi(line)),
        HistoryItem::Block { rows, .. } => {
            out.extend(std::iter::repeat_n(String::new(), usize::from(*rows)));
        }
    }
}

/// The rows behind a popup, from the top of the window down: the last `zone` rows of what was
/// written to the transcript, what waits to be written, and what is in flight. With fewer rows
/// than that they start at the top, as they do on the real screen.
fn behind(recorded: &VecDeque<String>, waiting: &[String], live: &[String], zone: usize) -> Vec<String> {
    let total = recorded.len() + waiting.len() + live.len();
    recorded
        .iter()
        .chain(waiting)
        .chain(live)
        .skip(total.saturating_sub(zone))
        .cloned()
        .collect()
}

/// The popup screen while it is up.
pub(super) struct Popup {
    screen: Modal,
    /// The box closed and nothing has taken its place: the screen is left at this time.
    closing: Option<Instant>,
    /// The transcript items said since the popup opened that are in `waiting`.
    seen: usize,
    /// Those items as plain rows.
    waiting: Vec<String>,
}

impl Popup {
    /// Where the box was last painted, as `(top, left, rows, width)` in the window.
    pub(super) fn placed(&self) -> Option<(usize, usize, usize, usize)> {
        self.screen.placed()
    }

    fn new(columns: u16, rows: u16) -> Popup {
        Popup {
            screen: Modal::new(columns, rows),
            closing: None,
            seen: 0,
            waiting: Vec::new(),
        }
    }

    /// Brings `waiting` up to date with the transcript, which only grows while the popup is open
    /// (or was emptied by a command).
    fn catch_up(&mut self, transcript: &[HistoryItem]) {
        if transcript.len() < self.seen {
            self.seen = 0;
            self.waiting.clear();
        }
        for item in &transcript[self.seen..] {
            plain_rows(item, &mut self.waiting);
        }
        self.seen = transcript.len();
    }
}

impl Tui {
    /// Paints the open overlay as a popup when it is to be one. True when the popup screen is
    /// up and the inline frame must not be drawn.
    pub(super) fn draw_popup(&mut self) -> io::Result<bool> {
        let (columns, rows) = self.renderer.size();
        // `tui.popups` off, or a window too small for one: the box stands in for the input.
        let boxed = self.overlay.is_some() || self.term_open();
        let wanted = self.env.config.tui.popups && !self.quit && boxed && modal::fits(columns, rows);
        if !wanted {
            let Some(popup) = &mut self.popup else {
                return Ok(false);
            };
            // The box closed. The next one may be on its way: the screen stays as it is for a
            // moment. Not when something else has painted over it, or the window is too small.
            let may_follow = !boxed && !self.quit && self.guard.in_alt() && !popup.screen.is_stale();
            if may_follow {
                let now = Instant::now();
                if now < *popup.closing.get_or_insert(now + LINGER) {
                    return Ok(true);
                }
            }
            self.close_popup();
            return Ok(false);
        }

        let mut popup = match self.popup.take() {
            Some(popup) => popup,
            None => {
                // The live region leaves the real screen, so that nothing of it is in the way
                // when the window changes size behind the popup. It is drawn again on return.
                self.renderer.suspend()?;
                self.leaving_main();
                Popup::new(columns, rows)
            }
        };
        // Entered now, or again after another program had the terminal.
        if self.guard.enter_alt() {
            popup.screen.invalidate();
        }
        // The mouse works in the box while it is up (and again after the full-screen viewer
        // or another program had it); leaving the alternate screen stops that.
        self.guard.capture_mouse(self.env.config.tui.mouse);
        popup.closing = None;
        popup.screen.resize(columns, rows);
        popup.catch_up(&self.transcript);

        let (width, height) = (usize::from(columns), usize::from(rows));
        self.refresh_pane(super::overlay::inner_width(width));
        let (lines, cursor) = match &self.overlay {
            Some(overlay) => overlay.render(overlay.box_width(width), modal::box_rows(height), &self.theme),
            None => self
                .term_box(modal::box_width(width), modal::box_rows(height))
                .unwrap_or_default(),
        };
        let live: Vec<String> = self
            .in_flight(width)
            .iter()
            .map(|line| text::strip_ansi(line))
            .collect();
        // The transcript keeps the rows it had on the real screen, above where the input was.
        let zone = height.saturating_sub(self.pinned_rows.max(2));
        let backdrop = behind(&self.backdrop.rows, &popup.waiting, &live, zone);
        let activity = self.voice_status().or_else(|| self.rule_status());
        let working = composer::rule(width, activity.as_deref(), None, self.rule_role(), &self.theme);
        let status = self.status_line(width);
        let painted = popup.screen.render(
            self.renderer.get_mut(),
            &Screen {
                backdrop: &backdrop,
                popup: &lines,
                cursor,
                working: &working,
                status: &status,
            },
            &self.theme,
        );
        self.popup = Some(popup);
        painted.map(|()| true)
    }

    /// When the popup screen is due to be left although nothing else happens.
    pub(super) fn popup_deadline(&self) -> Option<Instant> {
        self.popup.as_ref().and_then(|popup| popup.closing)
    }

    /// Leaves the popup screen. The transcript is back as it was; the next inline frame draws
    /// the live region and writes what was said meanwhile.
    pub(super) fn close_popup(&mut self) {
        if self.popup.take().is_some() {
            self.guard.leave_alt();
            // The window may have changed size behind the popup.
            self.back_on_main();
        }
    }

    /// Something else painted over the popup screen (the full-screen viewer): the next frame
    /// paints all of it.
    pub(super) fn popup_painted_over(&mut self) {
        if let Some(popup) = &mut self.popup {
            popup.screen.invalidate();
        }
    }

    /// `ctrl+o` in a box that cut its body: all of it in the full-screen viewer. The box is
    /// still open afterwards.
    pub(super) async fn read_overlay_body(&mut self) {
        let Some(overlay) = &self.overlay else {
            return;
        };
        let title = overlay.title.clone();
        let source = if overlay.markdown {
            Source::Markdown(overlay.body_text())
        } else {
            Source::Text(overlay.body_text())
        };
        pager::show(self, &title, vec![source], Start::Top).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| text.to_string()).collect()
    }

    #[test]
    fn the_backdrop_keeps_the_last_rows_as_plain_text() {
        let mut backdrop = Backdrop::default();
        backdrop.record(&[
            HistoryItem::Line("\x1b[1mbold\x1b[0m row".to_string()),
            HistoryItem::Block {
                rows: 2,
                paint: "\x1b_Gimage\x1b\\".to_string(),
            },
            HistoryItem::Line("after".to_string()),
        ]);
        assert_eq!(backdrop.rows, ["bold row", "", "", "after"]);

        // Only the last rows are kept, however many arrive at once.
        let many: Vec<HistoryItem> = (0..BACKDROP_ROWS + 50)
            .map(|row| HistoryItem::Line(format!("row {row}")))
            .collect();
        backdrop.record(&many[..10]);
        assert_eq!(backdrop.rows.len(), 14);
        backdrop.record(&many[10..]);
        assert_eq!(backdrop.rows.len(), BACKDROP_ROWS);
        assert_eq!(backdrop.rows[0], "row 50");
        assert_eq!(
            backdrop.rows[BACKDROP_ROWS - 1],
            format!("row {}", BACKDROP_ROWS + 49)
        );
        backdrop.record(&[HistoryItem::Line("one more".to_string())]);
        assert_eq!(backdrop.rows.len(), BACKDROP_ROWS);
        assert_eq!(backdrop.rows[0], "row 51");

        backdrop.clear();
        assert!(backdrop.rows.is_empty());
    }

    #[test]
    fn behind_a_popup_are_the_newest_rows_or_all_of_them_from_the_top() {
        let recorded: VecDeque<String> = rows(&["a", "b", "c"]).into();
        let waiting = rows(&["said meanwhile"]);
        let live = rows(&["streaming"]);
        // More room than rows: they start at the top, in order.
        assert_eq!(
            behind(&recorded, &waiting, &live, 10),
            ["a", "b", "c", "said meanwhile", "streaming"]
        );
        // Less room: the newest rows, ending with what is in flight.
        assert_eq!(
            behind(&recorded, &waiting, &live, 3),
            ["c", "said meanwhile", "streaming"]
        );
        assert_eq!(behind(&recorded, &waiting, &[], 2), ["c", "said meanwhile"]);
        assert!(behind(&recorded, &waiting, &live, 0).is_empty());
    }

    #[test]
    fn what_is_said_while_a_popup_is_open_joins_the_backdrop_once() {
        let mut popup = Popup::new(80, 24);
        let mut transcript = vec![HistoryItem::Line("\x1b[2mfirst\x1b[0m".to_string())];
        popup.catch_up(&transcript);
        popup.catch_up(&transcript);
        assert_eq!(popup.waiting, ["first"]);
        transcript.push(HistoryItem::Block {
            rows: 1,
            paint: String::new(),
        });
        transcript.push(HistoryItem::Line("second".to_string()));
        popup.catch_up(&transcript);
        assert_eq!(popup.waiting, ["first", "", "second"]);
        // A command emptied the transcript: what waited is gone with it.
        transcript.clear();
        transcript.push(HistoryItem::Line("fresh".to_string()));
        popup.catch_up(&transcript);
        assert_eq!(popup.waiting, ["fresh"]);
    }
}
