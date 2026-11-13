//! Inline differential renderer.
//!
//! The renderer owns a *live region* at the bottom of the terminal's normal screen. History lines
//! are written once above it and become ordinary scrollback; the region itself is compared row by
//! row with what was drawn last time and only the changed range is rewritten.
//!
//! The absolute position of the region is unknown and never queried. The renderer only remembers
//! on which row of the region it left the terminal cursor and moves relative to that. A caller
//! that wants the last rows of the region to stay on the last rows of the screen keeps the region
//! tall enough with a [`Floor`].

use std::io;

use crate::text;

/// One terminal row: text plus SGR sequences. Its display width must not exceed the terminal
/// width; the renderer cuts lines that are too wide rather than letting them wrap.
pub type Line = String;

/// One thing appended to the transcript above the live region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryItem {
    /// One row of text.
    Line(Line),
    /// Something the terminal draws over several rows, such as an inline image. The renderer
    /// makes `rows` empty rows, goes back to the first of them, writes `paint` and returns to
    /// the row below the block, wherever `paint` left the cursor.
    Block { rows: u16, paint: String },
}

impl HistoryItem {
    /// Rows the item takes in the transcript.
    pub fn rows(&self) -> usize {
        match self {
            HistoryItem::Line(_) => 1,
            HistoryItem::Block { rows, .. } => usize::from(*rows),
        }
    }
}

impl From<String> for HistoryItem {
    fn from(line: String) -> Self {
        HistoryItem::Line(line)
    }
}

impl From<&str> for HistoryItem {
    fn from(line: &str) -> Self {
        HistoryItem::Line(line.to_string())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    /// What to append to the transcript above the live region. Written exactly once.
    pub history: Vec<HistoryItem>,
    /// The complete live region for this frame.
    pub viewport: Vec<Line>,
    /// Where to show the terminal cursor as `(row within viewport, column)`; `None` hides it.
    pub cursor: Option<(u16, u16)>,
}

/// Keeps the bottom of the live region on the last row of the screen.
///
/// The floor is the number of rows the region must occupy. It starts at the height of the screen,
/// so the first frame fills the screen and whatever the terminal showed before scrolls away. A
/// frame with fewer rows of its own is padded with blank rows (the caller decides where they go);
/// each history row written above the region takes one row from the floor, so the transcript
/// grows into the padding while the rows at the bottom stay where they are.
///
/// When the screen changes size the floor follows it without asking the terminal anything
/// ([`Floor::resize`]); a caller that can learn where the cursor is says so afterwards
/// ([`Floor::anchor`]), which is exact whatever the terminal did with its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Floor {
    rows: usize,
    /// Rows of the screen, as last heard.
    height: usize,
}

impl Floor {
    /// A floor for a region that has not been drawn yet on a screen `height` rows tall.
    pub fn new(height: u16) -> Floor {
        let height = usize::from(height.max(1));
        Floor { rows: height, height }
    }

    /// Rows the region occupies after the last [`Floor::pad`].
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The screen was cleared or the region erased with the cursor at the top: fill it again.
    pub fn reset(&mut self, height: u16) {
        *self = Floor::new(height);
    }

    /// The screen is `height` rows tall from now on, and the region as it was last drawn takes
    /// `occupied` rows of it ([`Renderer::rows_on_screen`]: more than it had when the terminal
    /// re-wrapped rows that no longer fit; 0 while the region is not on the screen).
    ///
    /// A shorter screen caps the region. A taller one grows it by the rows the screen gained,
    /// which is where a terminal that adds them below its content has left the region; one
    /// that brings rows back from its scrollback instead has blank rows in the region for a
    /// while, and the rows at the bottom are on the bottom either way.
    pub fn resize(&mut self, height: u16, occupied: usize) {
        let height = usize::from(height.max(1));
        let gained = height.saturating_sub(self.height);
        self.rows = (self.rows.max(occupied) + gained).min(height);
        self.height = height;
    }

    /// The region begins `rows` rows above the bottom of the screen: the terminal said where
    /// the cursor is, so nothing has to be assumed about what a change of size did.
    pub fn anchor(&mut self, rows: usize) {
        self.rows = rows.clamp(1, self.height);
    }

    /// Accounts for a frame that writes `history` rows above a region with `content` rows of
    /// its own, and returns how many blank rows the region needs besides them.
    pub fn pad(&mut self, history: usize, content: usize, height: u16) -> usize {
        let height = usize::from(height.max(1));
        self.height = height;
        self.rows = self.rows.saturating_sub(history).max(content).min(height);
        self.rows.saturating_sub(content)
    }
}

const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const HIDE_CURSOR: &str = "\x1b[?25l";
const ERASE_LINE: &str = "\x1b[2K";
const ERASE_BELOW: &str = "\x1b[J";
const SGR_RESET: &str = "\x1b[0m";
/// DECSC and DECRC: the cursor position is kept by the terminal across a block's paint.
const SAVE_CURSOR: &str = "\x1b7";
const RESTORE_CURSOR: &str = "\x1b8";

pub struct Renderer<W: io::Write> {
    out: W,
    width: u16,
    height: u16,
    /// The rows of the live region as they are on screen.
    drawn: Vec<Line>,
    /// Row of the terminal cursor counted from the first row of the live region. Always below
    /// `max(drawn.len(), 1)`: an empty region still owns the row the cursor is on.
    cursor_row: usize,
    /// Column the terminal cursor was left in.
    cursor_col: usize,
    /// Cursor placement requested by the last frame, after clipping.
    cursor: Option<(usize, usize)>,
    cursor_visible: bool,
    /// Set by `resize`: the next frame repaints the whole region.
    redraw: bool,
    /// The terminal re-wraps what is on screen when it gets narrower.
    rewraps: bool,
}

impl<W: io::Write> Renderer<W> {
    pub fn new(out: W, width: u16, height: u16) -> Self {
        Self {
            out,
            width: width.max(1),
            height: height.max(1),
            drawn: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
            cursor: None,
            cursor_visible: true,
            redraw: false,
            rewraps: true,
        }
    }

    /// Draws a frame. Writes nothing when the frame has no history and repeats the previous
    /// viewport and cursor; otherwise the output is one synchronized update.
    pub fn render(&mut self, frame: Frame) -> io::Result<()> {
        let width = usize::from(self.width);
        let height = usize::from(self.height);
        let Frame {
            history,
            viewport,
            cursor,
        } = frame;

        // A region taller than the screen could not be reached with relative cursor moves, so
        // only its last `height` rows are kept on screen.
        let skipped = viewport.len().saturating_sub(height);
        let rows: Vec<Line> = viewport
            .into_iter()
            .skip(skipped)
            .map(|line| fit(line, width))
            .collect();
        let cursor = cursor.and_then(|(row, col)| {
            let row = usize::from(row).checked_sub(skipped)?;
            (row < rows.len().max(1)).then_some((row, usize::from(col).min(width - 1)))
        });

        if history.is_empty() && !self.redraw && rows == self.drawn && cursor == self.cursor {
            return Ok(());
        }

        let mut buf = String::from(SYNC_BEGIN);
        if history.is_empty() && !self.redraw {
            self.draw_changes(&mut buf, &rows);
        } else {
            self.draw_all(&mut buf, history, &rows, width, height);
        }
        self.drawn = rows;
        self.redraw = false;
        self.place_cursor(&mut buf, cursor);
        buf.push_str(SYNC_END);
        self.write(&buf)
    }

    /// Records a new terminal size. The next frame erases and repaints the live region at the
    /// new width; history already written is left to the terminal.
    ///
    /// A terminal that re-wraps its content when it gets narrower has put the rows of the
    /// region that no longer fit on more rows than they were drawn on. The renderer works out
    /// how many from what it drew, so that all of the old region is erased; see
    /// [`Renderer::set_rewraps`] for terminals that cut rows instead.
    pub fn resize(&mut self, width: u16, height: u16) {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) != (self.width, self.height) {
            self.width = width;
            self.height = height;
            self.redraw = true;
        }
    }

    /// Whether the terminal re-wraps rows that no longer fit when it gets narrower, as nearly
    /// all do (the default). One that cuts them instead (xterm) keeps every row of the region
    /// on the row it was drawn on.
    pub fn set_rewraps(&mut self, rewraps: bool) {
        self.rewraps = rewraps;
    }

    /// Rows of the screen the region takes as it was last drawn: its own rows, and after the
    /// terminal got narrower the rows its long lines were continued on.
    pub fn rows_on_screen(&self) -> usize {
        self.drawn.iter().map(|row| self.wrapped(row)).sum()
    }

    /// Rows of the screen between the first row of the region and the row the terminal cursor
    /// is on.
    pub fn rows_above_cursor(&self) -> usize {
        let above: usize = self
            .drawn
            .iter()
            .take(self.cursor_row)
            .map(|row| self.wrapped(row))
            .sum();
        // The cursor stays on its character: on a continuation row when its column is past
        // the new width.
        let within = match self.drawn.get(self.cursor_row) {
            Some(row) if self.rewraps => {
                (self.cursor_col / usize::from(self.width)).min(self.wrapped(row) - 1)
            }
            _ => 0,
        };
        above + within
    }

    /// Rows of the screen a row drawn earlier takes at the width the terminal has now.
    fn wrapped(&self, row: &str) -> usize {
        if !self.rewraps {
            return 1;
        }
        text::width(row).div_ceil(usize::from(self.width)).max(1)
    }

    /// Erases the live region, shows the cursor and leaves it where the region began. Call before
    /// exiting or handing the terminal to a child process; the next frame draws the region again
    /// from wherever the cursor is then.
    pub fn suspend(&mut self) -> io::Result<()> {
        let mut buf = String::new();
        if !self.drawn.is_empty() || self.cursor.is_some() {
            let above = self.rows_above_cursor();
            if above > 0 {
                csi(&mut buf, above, 'A');
            }
            buf.push('\r');
            buf.push_str(ERASE_BELOW);
        }
        if !self.cursor_visible {
            buf.push_str(SHOW_CURSOR);
        }
        self.drawn.clear();
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.cursor = None;
        self.cursor_visible = true;
        self.redraw = false;
        if buf.is_empty() { Ok(()) } else { self.write(&buf) }
    }

    /// The terminal size as `(width, height)`.
    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    pub fn get_ref(&self) -> &W {
        &self.out
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.out
    }

    pub fn into_inner(self) -> W {
        self.out
    }

    /// Rewrites the range of rows that differ from what is on screen.
    fn draw_changes(&mut self, buf: &mut String, rows: &[Line]) {
        let old_len = self.drawn.len();
        let common = old_len.min(rows.len());
        let first = match (0..common).find(|&i| self.drawn[i] != rows[i]) {
            Some(i) => i,
            None if old_len != rows.len() => common,
            None => return,
        };
        let end = (first..rows.len())
            .rev()
            .find(|&i| i >= old_len || self.drawn[i] != rows[i])
            .map_or(first, |i| i + 1);

        if first < end {
            if first < old_len.max(1) {
                self.move_to_row(buf, first);
                buf.push('\r');
            } else {
                // Growing: a line feed from the last row creates the next one, scrolling the
                // screen when the region already touches the bottom.
                self.move_to_row(buf, old_len - 1);
                buf.push_str("\r\n");
                self.cursor_row = first;
            }
            for (offset, line) in rows[first..end].iter().enumerate() {
                if offset > 0 {
                    buf.push_str("\r\n");
                    self.cursor_row += 1;
                }
                buf.push_str(ERASE_LINE);
                push_line(buf, line);
            }
            self.cursor_col = end_column(&rows[end - 1], usize::from(self.width));
        }

        if rows.len() < old_len {
            self.move_to_row(buf, rows.len());
            buf.push('\r');
            buf.push_str(ERASE_BELOW);
            self.move_to_row(buf, rows.len().saturating_sub(1));
            self.cursor_col = 0;
        }
    }

    /// Erases the region, appends history where it began and paints the region below it.
    fn draw_all(
        &mut self,
        buf: &mut String,
        history: Vec<HistoryItem>,
        rows: &[Line],
        width: usize,
        height: usize,
    ) {
        // After a change of size the rows above the cursor may have been re-wrapped.
        let above = self.rows_above_cursor();
        if above > 0 {
            csi(buf, above, 'A');
        }
        buf.push('\r');
        buf.push_str(ERASE_BELOW);
        for item in history {
            match item {
                HistoryItem::Line(line) => {
                    push_line(buf, &fit(line, width));
                    buf.push_str("\r\n");
                }
                HistoryItem::Block { rows, paint } => {
                    let rows = usize::from(rows);
                    // The line feeds make the rows, scrolling the screen when they must; only
                    // then is the first of them a place the cursor can go back to.
                    for _ in 0..rows {
                        buf.push_str("\r\n");
                    }
                    // A block as tall as the screen has lost its first row to the scrollback.
                    if rows > 0 && rows < height {
                        buf.push_str(SAVE_CURSOR);
                        csi(buf, rows, 'A');
                        buf.push_str(&paint);
                        buf.push_str(RESTORE_CURSOR);
                    }
                }
            }
        }
        for (i, line) in rows.iter().enumerate() {
            if i > 0 {
                buf.push_str("\r\n");
            }
            push_line(buf, line);
        }
        self.cursor_row = rows.len().saturating_sub(1);
        self.cursor_col = rows.last().map_or(0, |row| end_column(row, width));
    }

    /// Moves between rows that already exist on screen.
    fn move_to_row(&mut self, buf: &mut String, row: usize) {
        if row < self.cursor_row {
            csi(buf, self.cursor_row - row, 'A');
        } else if row > self.cursor_row {
            csi(buf, row - self.cursor_row, 'B');
        }
        self.cursor_row = row;
    }

    fn place_cursor(&mut self, buf: &mut String, cursor: Option<(usize, usize)>) {
        match cursor {
            Some((row, col)) => {
                self.move_to_row(buf, row);
                csi(buf, col + 1, 'G');
                self.cursor_col = col;
                if !self.cursor_visible {
                    buf.push_str(SHOW_CURSOR);
                    self.cursor_visible = true;
                }
            }
            None if self.cursor_visible => {
                buf.push_str(HIDE_CURSOR);
                self.cursor_visible = false;
            }
            None => {}
        }
        self.cursor = cursor;
    }

    fn write(&mut self, buf: &str) -> io::Result<()> {
        let result = self.out.write_all(buf.as_bytes()).and_then(|()| self.out.flush());
        if result.is_err() {
            // What reached the terminal is unknown; repaint on the next frame.
            self.redraw = true;
        }
        result
    }
}

fn csi(buf: &mut String, n: usize, command: char) {
    buf.push_str("\x1b[");
    buf.push_str(&n.to_string());
    buf.push(command);
}

/// The column the cursor is in after `line` was written from the start of a row.
fn end_column(line: &str, width: usize) -> usize {
    text::width(line).min(width.saturating_sub(1))
}

fn push_line(buf: &mut String, line: &str) {
    buf.push_str(line);
    // Attributes left open would colour the next erase and the rows below.
    if line.contains('\x1b') && !line.ends_with(SGR_RESET) {
        buf.push_str(SGR_RESET);
    }
}

/// Guarantees that a line occupies exactly one terminal row.
pub(crate) fn fit(line: Line, width: usize) -> Line {
    // Control characters other than ESC would move the cursor behind the renderer's back.
    let moves_cursor = |c: char| c.is_control() && c != '\x1b';
    let line = if line.contains(moves_cursor) {
        line.replace(moves_cursor, " ")
    } else {
        line
    };
    if text::width(&line) > width {
        text::truncate(&line, width, "")
    } else {
        line
    }
}
