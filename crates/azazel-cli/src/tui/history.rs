//! What this process wrote to the transcript, kept so that the transcript can be drawn again:
//! `ctrl+o` erases the screen and the scrollback and writes it all once more, with tool output
//! in full or cut short.
//!
//! Tool calls are kept as calls, with all they returned, and drawn afresh at the new setting and
//! the window's width. Everything else (prompts, replies, thinking, notices, pictures) reads the
//! same at either setting and is kept as the rows it was written as. Only the newest items are
//! kept; the session file has the whole conversation, which `/transcript` shows.

use std::collections::VecDeque;
use std::io::Write;
use std::time::Instant;

use azazel_tui::HistoryItem;

use super::calls::Finished;
use super::{Tui, view};

/// Items kept. Past this the oldest go, and are not drawn again.
const MAX_ITEMS: usize = 300;

/// Moves the cursor home, erases the screen, then the scrollback. The screen comes first: a
/// terminal that keeps what it erases (tmux) would push it into the scrollback afterwards.
const ERASE_ALL: &[u8] = b"\x1b[H\x1b[2J\x1b[3J";

/// One thing said in the transcript.
pub(super) enum Said {
    /// Rows that look the same whatever the setting.
    Rows(Vec<HistoryItem>),
    /// A group of tool calls written together: a card each, or a tree of compact rows.
    Calls(Vec<Finished>),
}

#[derive(Default)]
pub(super) struct History {
    items: VecDeque<Said>,
    /// How many items went before the first that is kept.
    dropped: usize,
}

impl History {
    pub(super) fn push(&mut self, said: Said) {
        self.items.push_back(said);
        if self.items.len() > MAX_ITEMS {
            self.items.pop_front();
            self.dropped += 1;
        }
    }

    /// Where the next item will be: a place [`History::said`] finds it by while it is kept.
    pub(super) fn end(&self) -> usize {
        self.dropped + self.items.len()
    }

    pub(super) fn said(&self, at: usize) -> Option<&Said> {
        self.items.get(at.checked_sub(self.dropped)?)
    }

    pub(super) fn clear(&mut self) {
        self.dropped += self.items.len();
        self.items.clear();
    }
}

impl Tui {
    /// Writes rows to the transcript and keeps them for drawing it again.
    pub(super) fn keep_rows(&mut self, items: Vec<HistoryItem>) {
        self.after_rows = false;
        self.transcript.extend(items.iter().cloned());
        self.history.push(Said::Rows(items));
    }

    /// Whether the scrollback can be erased: inside tmux, screen or zellij it cannot, and a
    /// transcript drawn again would stand under the old one.
    pub(super) fn can_redraw(&self) -> bool {
        !self.caps.multiplexer
    }

    /// Erases the screen and the scrollback. The live region starts over at the top of an empty
    /// screen and fills it again.
    pub(super) fn erase_all(&mut self) {
        self.transcript.clear();
        self.backdrop.clear();
        self.after_rows = false;
        let _ = self.renderer.suspend();
        self.renderer.forget_transcript();
        let out = self.renderer.get_mut();
        let _ = out.write_all(ERASE_ALL);
        let _ = out.flush();
        let (_, rows) = self.renderer.size();
        self.floor.reset(rows);
    }

    /// Draws the transcript again as it is kept, at the settings and the width there are now.
    /// The next frame writes it, then the live region under it.
    pub(super) fn redraw_transcript(&mut self) {
        let started = Instant::now();
        // What waits in the live region is written in the look it has now.
        self.restyle_calls();
        self.erase_all();
        let width = self.width();
        let mut rows: Vec<HistoryItem> = Vec::new();
        if self.history.dropped > 0 {
            let note = view::notice(
                "earlier turns are not drawn again; /transcript shows the whole conversation",
                width,
                &self.theme,
            );
            rows.extend(note.into_iter().map(HistoryItem::Line));
        }
        let mut after_rows = false;
        for said in &self.history.items {
            match said {
                Said::Rows(items) => {
                    rows.extend(items.iter().cloned());
                    after_rows = false;
                }
                Said::Calls(calls) => self.draw_group(calls, width, &mut after_rows, &mut rows),
            }
        }
        self.after_rows = after_rows;
        tracing::debug!(
            items = self.history.items.len(),
            rows = rows.len(),
            took_ms = started.elapsed().as_secs_f64() * 1000.0,
            "transcript drawn again"
        );
        self.transcript = rows;
    }

    /// `ctrl+o`: tool output in full, or cut short again, everywhere in the transcript. Where
    /// the scrollback cannot be erased only what is written from now on changes; in the
    /// compact style the last group of calls is written again as full cards.
    pub(super) fn toggle_tool_output(&mut self) {
        self.expand_tools = !self.expand_tools;
        if self.can_redraw() {
            self.redraw_transcript();
        } else if self.expand_tools {
            self.expand_calls();
        } else {
            self.restyle_calls();
        }
        self.notice = Some(format!(
            "tool output: {}",
            if self.expand_tools {
                "expanded"
            } else {
                "collapsed"
            }
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_items_are_kept_and_found_where_they_were_put() {
        let mut history = History::default();
        let first = history.end();
        history.push(Said::Rows(vec!["first".into()]));
        for n in 0..MAX_ITEMS + 5 {
            history.push(Said::Rows(vec![format!("row {n}").into()]));
        }
        assert_eq!(history.items.len(), MAX_ITEMS);
        assert!(history.said(first).is_none(), "the oldest went");
        let last = history.end() - 1;
        assert!(
            matches!(history.said(last), Some(Said::Rows(rows)) if rows[0] == HistoryItem::Line(format!("row {}", MAX_ITEMS + 4)))
        );
        history.clear();
        assert!(history.said(last).is_none());
        assert_eq!(history.end(), MAX_ITEMS + 6);
    }
}
