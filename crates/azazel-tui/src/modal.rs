//! A popup over the whole window, painted on the terminal's alternate screen.
//!
//! The inline renderer cannot draw over the transcript: rows it wrote above its live region
//! belong to the terminal's scrollback and cannot be brought back. A popup therefore takes the
//! alternate screen, where every row can be addressed, and paints a stand-in for what was
//! behind it: the last rows of the transcript as dimmed plain text (the *backdrop*), the box in
//! the middle, and the working row and the status line on the last two rows. Leaving the
//! alternate screen brings the real transcript back untouched.
//!
//! [`Modal`] only produces the bytes of a frame. Entering and leaving the alternate screen is
//! the caller's business.

use std::io;

use crate::render::{self, Line};
use crate::text::{self, Segment};
use crate::theme::{RESET, Role, Theme};

/// The smallest window a popup is drawn in; below it the caller shows the box inline.
pub const MIN_COLS: u16 = 44;
pub const MIN_ROWS: u16 = 12;

/// Rows at the bottom of the window that stay with the working row and the status line.
const BOTTOM_ROWS: usize = 2;

const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const HIDE_CURSOR: &str = "\x1b[?25l";
const ERASE_LINE: &str = "\x1b[2K";

/// Whether a window of this size has room for a popup.
pub fn fits(cols: u16, rows: u16) -> bool {
    cols >= MIN_COLS && rows >= MIN_ROWS
}

/// How wide the box is in a window `cols` columns wide: nine tenths of it, at least 44 and at
/// most 100 columns; the whole width of a window narrower than 44.
pub fn box_width(cols: usize) -> usize {
    if cols < usize::from(MIN_COLS) {
        cols
    } else {
        (cols * 9 / 10).clamp(usize::from(MIN_COLS), 100)
    }
}

/// The most rows the box may take in a window `rows` rows tall.
pub fn box_rows(rows: usize) -> usize {
    rows.saturating_sub(BOTTOM_ROWS + 2).max(1)
}

/// One frame of the popup screen.
#[derive(Debug, Clone, Copy)]
pub struct Screen<'a> {
    /// What is behind the box, as plain text: row `i` goes on row `i` of the window. Rows
    /// beyond the space above the bottom rows are not shown; missing rows are blank.
    pub backdrop: &'a [String],
    /// The box, every line the same width. It is centred in the space above the bottom rows.
    pub popup: &'a [Line],
    /// Where the typing cursor is, as `(row, column)` within the box; `None` hides it.
    pub cursor: Option<(u16, u16)>,
    /// The second-last row of the window: what the agent is doing.
    pub working: &'a str,
    /// The last row of the window.
    pub status: &'a str,
}

/// Paints [`Screen`]s, rewriting only the rows that changed since the last one.
#[derive(Debug)]
pub struct Modal {
    width: u16,
    height: u16,
    /// The rows as they are on the alternate screen.
    drawn: Vec<Line>,
    /// Where the cursor was left, in window coordinates; `None` while it is hidden.
    cursor: Option<(usize, usize)>,
    /// Nothing on the screen can be relied on: the next frame paints every row.
    repaint: bool,
}

impl Modal {
    /// A painter for an alternate screen that has just been entered.
    pub fn new(width: u16, height: u16) -> Modal {
        Modal {
            width: width.max(1),
            height: height.max(1),
            drawn: Vec::new(),
            cursor: None,
            repaint: true,
        }
    }

    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// Records a new window size. The next frame paints every row.
    pub fn resize(&mut self, width: u16, height: u16) {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) != (self.width, self.height) {
            self.width = width;
            self.height = height;
            self.repaint = true;
        }
    }

    /// Something else wrote to the alternate screen, or it was left and entered again: the next
    /// frame paints every row.
    pub fn invalidate(&mut self) {
        self.repaint = true;
    }

    /// Whether the next frame paints every row.
    pub fn is_stale(&self) -> bool {
        self.repaint
    }

    /// Paints a frame. Writes nothing when it repeats the last one; otherwise the output is one
    /// synchronized update that addresses only the rows that differ.
    pub fn render(&mut self, out: &mut impl io::Write, screen: &Screen<'_>, theme: &Theme) -> io::Result<()> {
        let (rows, place) = self.compose(screen, theme);
        let cursor = screen.cursor.and_then(|(row, column)| {
            let (top, left, box_rows, box_width) = place;
            let (row, column) = (usize::from(row), usize::from(column));
            (row < box_rows && column < box_width).then_some((top + row, left + column))
        });
        if !self.repaint && rows == self.drawn && cursor == self.cursor {
            return Ok(());
        }

        let mut buf = String::from(SYNC_BEGIN);
        for (row, line) in rows.iter().enumerate() {
            if self.repaint || self.drawn.get(row) != Some(line) {
                buf.push_str(&format!("\x1b[{};1H{ERASE_LINE}{line}", row + 1));
                if line.contains('\x1b') && !line.ends_with(RESET) {
                    buf.push_str(RESET);
                }
            }
        }
        match cursor {
            Some((row, column)) => {
                buf.push_str(&format!("\x1b[{};{}H", row + 1, column + 1));
                if self.repaint || self.cursor.is_none() {
                    buf.push_str(SHOW_CURSOR);
                }
            }
            None if self.repaint || self.cursor.is_some() => buf.push_str(HIDE_CURSOR),
            None => {}
        }
        buf.push_str(SYNC_END);

        let written = out.write_all(buf.as_bytes()).and_then(|()| out.flush());
        // After a failed write what reached the terminal is unknown.
        self.repaint = written.is_err();
        self.drawn = rows;
        self.cursor = cursor;
        written
    }

    /// Every row of the window for `screen`, and where the box went as
    /// `(top, left, rows, width)`.
    fn compose(&self, screen: &Screen<'_>, theme: &Theme) -> (Vec<Line>, (usize, usize, usize, usize)) {
        let (width, height) = (usize::from(self.width), usize::from(self.height));
        let bottom = BOTTOM_ROWS.min(height.saturating_sub(1));
        let area = height - bottom;
        let box_width = screen
            .popup
            .iter()
            .map(|line| text::width(line))
            .max()
            .unwrap_or(0)
            .min(width);
        let box_rows = screen.popup.len().min(area);
        let left = (width - box_width) / 2;
        let top = (area - box_rows) / 2;
        let dim = |plain: &str| theme.paint(Role::Backdrop, plain);

        let mut rows = Vec::with_capacity(height);
        for row in 0..area {
            let behind = screen.backdrop.get(row).map_or("", String::as_str);
            let line = match row.checked_sub(top).and_then(|at| screen.popup.get(at)) {
                Some(line) if row < top + box_rows => {
                    let line = render::fit(line.clone(), box_width);
                    let before = text::pad_right(&columns(behind, 0, left), left);
                    let after = columns(behind, left + box_width, width);
                    format!(
                        "{}{}{RESET}{}",
                        dim(&before),
                        text::pad_right(line.strip_suffix(RESET).unwrap_or(&line), box_width),
                        dim(after.trim_end()),
                    )
                }
                _ => dim(columns(behind, 0, width).trim_end()),
            };
            rows.push(line);
        }
        if bottom == BOTTOM_ROWS {
            rows.push(render::fit(screen.working.to_string(), width));
        }
        if bottom > 0 {
            rows.push(render::fit(screen.status.to_string(), width));
        }
        (rows, (top, left, box_rows, box_width))
    }
}

/// The part of plain `text` that lies in the columns `from..to`. A wide character cut by either
/// end leaves spaces for the cells of it that are inside; control characters and escape
/// sequences are dropped.
fn columns(plain: &str, from: usize, to: usize) -> String {
    let mut out = String::new();
    let mut at = 0;
    for segment in text::segments(plain) {
        let Segment::Grapheme(grapheme) = segment else {
            continue;
        };
        let wide = text::grapheme_width(grapheme);
        if wide == 0 {
            continue;
        }
        let (start, end) = (at, at + wide);
        at = end;
        if end <= from {
            continue;
        }
        if start >= to {
            break;
        }
        if start >= from && end <= to {
            out.push_str(grapheme);
        } else {
            out.extend(std::iter::repeat_n(' ', end.min(to) - start.max(from)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_box_is_nine_tenths_of_the_window_within_its_bounds() {
        assert_eq!(box_width(20), 20);
        assert_eq!(box_width(43), 43);
        assert_eq!(box_width(44), 44);
        assert_eq!(box_width(48), 44);
        assert_eq!(box_width(80), 72);
        assert_eq!(box_width(100), 90);
        assert_eq!(box_width(112), 100);
        assert_eq!(box_width(300), 100);
        assert_eq!(box_rows(24), 20);
        assert_eq!(box_rows(12), 8);
        assert_eq!(box_rows(2), 1);
    }

    #[test]
    fn small_windows_have_no_room_for_a_popup() {
        assert!(fits(44, 12));
        assert!(fits(200, 60));
        assert!(!fits(43, 12));
        assert!(!fits(44, 11));
        assert!(!fits(20, 40));
    }

    #[test]
    fn columns_cut_plain_text_by_cells() {
        assert_eq!(columns("hello world", 0, 5), "hello");
        assert_eq!(columns("hello world", 6, 20), "world");
        assert_eq!(columns("hello", 7, 9), "");
        // A wide character cut by an end leaves a space for the cell that is inside.
        assert_eq!(columns("a日本b", 0, 2), "a ");
        assert_eq!(columns("a日本b", 2, 6), " 本b");
        assert_eq!(columns("a日本b", 1, 5), "日本");
        assert_eq!(columns("a\x1b[31mb\tc", 0, 9), "abc");
    }
}
