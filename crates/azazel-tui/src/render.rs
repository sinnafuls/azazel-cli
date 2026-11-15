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

use std::collections::VecDeque;
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

    /// Like [`Floor::pad`], for a renderer that can paint over the last `restorable` rows of the
    /// transcript and paint them back later ([`Renderer::restorable`]). A region that outgrows
    /// its floor for a while (a menu that opens) then rises over the transcript instead of
    /// pushing it into the scrollback, and gives the rows back when it shrinks again, so that no
    /// blank rows are left between the transcript and the region. Returns the blank rows the
    /// region needs and how many of its first rows go over the transcript
    /// ([`Renderer::cover`]); the floor itself grows only by what cannot be covered.
    ///
    /// A frame that writes history covers nothing: the rows just written (a message, a reply, a
    /// finished tool card) are the newest of the transcript and stay in sight right above the
    /// region. What was covered is given back first, and the region is laid out again below.
    pub fn pad_over(
        &mut self,
        history: usize,
        content: usize,
        height: u16,
        restorable: usize,
    ) -> (usize, usize) {
        let height = usize::from(height.max(1));
        self.height = height;
        let room = self.rows.saturating_sub(history).min(height);
        // Only what the region outgrows its floor by is covered: rows it lost to history that
        // was written scroll the transcript as before. The last row of the region stays its
        // own, and only rows on the screen are covered.
        let restorable = if history > 0 { 0 } else { restorable };
        let cover = content
            .saturating_sub(self.rows.min(height))
            .min(restorable)
            .min(content.saturating_sub(1));
        let cover = cover.min(height.saturating_sub(content - cover));
        let own = content - cover;
        self.rows = room.max(own).min(height);
        (self.rows.saturating_sub(own), cover)
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
    /// The last rows written to the transcript, as they were written and newest last, at most a
    /// screen of them; `None` for a row of a block, which cannot be painted again.
    written: VecDeque<Option<Line>>,
    /// The first rows of the region lie over this many of the last rows of `written`.
    covered: usize,
    /// What the next frame covers, as [`Renderer::cover`] asked.
    cover_next: usize,
    /// The screen changed size since `written` was written: nothing more is covered until the
    /// transcript is written again.
    written_stale: bool,
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
            written: VecDeque::new(),
            covered: 0,
            cover_next: 0,
            written_stale: false,
        }
    }

    /// How many of the last rows of the transcript, once `history` is written after them, the
    /// renderer can paint back after a region drew over them: rows of text it wrote itself, at
    /// the size the screen has now. The caller knows whether they are on the screen.
    pub fn restorable(&self, history: &[HistoryItem]) -> usize {
        let mut run = 0;
        for item in history.iter().rev() {
            match item {
                HistoryItem::Line(_) => run += 1,
                HistoryItem::Block { rows: 0, .. } => {}
                HistoryItem::Block { .. } => return run,
            }
        }
        if !self.written_stale {
            run += self.written.iter().rev().take_while(|row| row.is_some()).count();
        }
        run.min(usize::from(self.height))
    }

    /// The first `rows` rows of the next frame go over the last rows of the transcript, which
    /// are on the screen right above the region, instead of below them: a region at the bottom
    /// of the screen grows upwards without scrolling the transcript away. They are painted back
    /// when a later frame covers fewer, or the region is erased. At most
    /// [`Renderer::restorable`] rows are covered.
    pub fn cover(&mut self, rows: usize) {
        self.cover_next = rows;
    }

    /// The screen no longer shows what was written to it last (it was erased, or another
    /// program wrote on it): nothing is covered until the transcript is written again. Call
    /// while the region is off the screen ([`Renderer::suspend`]).
    pub fn forget_transcript(&mut self) {
        self.written.clear();
        self.covered = 0;
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
        // The row with the cursor, and the last row, stay the region's own.
        let cover = std::mem::take(&mut self.cover_next)
            .saturating_sub(skipped)
            .min(rows.len().saturating_sub(1))
            .min(cursor.map_or(usize::MAX, |(row, _)| row))
            .min(self.restorable(&history));

        if history.is_empty()
            && !self.redraw
            && rows == self.drawn
            && cursor == self.cursor
            && cover == self.covered
        {
            return Ok(());
        }

        let mut buf = String::from(SYNC_BEGIN);
        if history.is_empty() && !self.redraw {
            self.draw_over(&mut buf, rows, cursor, cover);
        } else {
            self.draw_all(&mut buf, history, &rows, width, height, cover);
            self.drawn = rows;
            self.place_cursor(&mut buf, cursor);
        }
        self.redraw = false;
        buf.push_str(SYNC_END);
        self.write(&buf)
    }

    /// A frame that writes no history: the rows it covers change, then the rows that differ.
    fn draw_over(
        &mut self,
        buf: &mut String,
        mut rows: Vec<Line>,
        mut cursor: Option<(usize, usize)>,
        cover: usize,
    ) {
        let end = self.written.len();
        if cover > self.covered {
            // The rows right above take part in the region as they are, and are drawn over.
            let rise = cover - self.covered;
            let above = self.written.range(end - cover..end - self.covered);
            let above: Vec<Line> = above.map(|row| row.clone().unwrap_or_default()).collect();
            if self.drawn.is_empty() {
                // An empty region still owns the row the cursor is on.
                self.drawn.push(String::new());
            }
            self.drawn.splice(0..0, above);
            self.cursor_row += rise;
            self.covered = cover;
        }
        // Rows given back are painted as the transcript had them, then leave the region.
        let given = self.covered - cover;
        if given > 0 {
            let back = self.written.range(end - self.covered..end - cover);
            let back: Vec<Line> = back.map(|row| row.clone().unwrap_or_default()).collect();
            rows.splice(0..0, back);
            cursor = cursor.map(|(row, col)| (row + given, col));
        }
        self.draw_changes(buf, &rows);
        self.drawn = rows;
        self.place_cursor(buf, cursor);
        if given > 0 {
            if self.cursor_row < given {
                self.move_to_row(buf, given);
            }
            self.drawn.drain(..given);
            self.cursor_row -= given;
            self.cursor = self.cursor.map(|(row, col)| (row - given, col));
            self.covered = cover;
        }
    }

    /// Records a new terminal size. The next frame erases and repaints the live region at the
    /// new width; history already written is left to the terminal.
    ///
    /// A terminal that re-wraps its content when it gets narrower has put the rows of the
    /// region that no longer fit on more rows than they were drawn on. The renderer works out
    /// how many from what it drew, so that all of the old region is erased; see
    /// [`Renderer::set_rewraps`] for terminals that cut rows instead. Where it cannot be sure
    /// it errs towards too few rows: something of the old region may stay behind above the
    /// new one, and nothing of the transcript is erased.
    pub fn resize(&mut self, width: u16, height: u16) {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) != (self.width, self.height) {
            self.width = width;
            self.height = height;
            self.redraw = true;
            self.written_stale = true;
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
    /// Rows it covers ([`Renderer::cover`]) are the transcript's, and are not counted.
    pub fn rows_on_screen(&self) -> usize {
        self.drawn
            .iter()
            .skip(self.covered)
            .map(|row| self.wrapped(row))
            .sum()
    }

    /// Rows of the screen between the first row of the region and the row the terminal cursor
    /// is on. Rows it covers are the transcript's, and are not counted.
    pub fn rows_above_cursor(&self) -> usize {
        let covered: usize = self
            .drawn
            .iter()
            .take(self.covered)
            .map(|row| self.wrapped(row))
            .sum();
        self.rows_to_top() - covered
    }

    /// Rows of the screen between the first row the region draws on, covered or not, and the
    /// row the terminal cursor is on.
    fn rows_to_top(&self) -> usize {
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

    /// The terminal changed width while its alternate screen was showing, and has just shown
    /// the main screen again; the region was taken off it ([`Renderer::suspend`]) before.
    ///
    /// A terminal that re-wraps does so to the main screen when it shows it again, and some
    /// (Windows Terminal) then put the cursor back on the row and column it had before, where
    /// rows the re-wrap added above it now are: the next frame would be written over the end of
    /// the transcript. This moves the cursor down past them, worked out from `above`, the rows
    /// of the transcript as plain text (oldest first), `top`, the row of the screen the cursor
    /// was on, and `width`, the width the screen had then. Where it cannot be sure it moves too
    /// far: a blank row may be left above the region, and nothing of the transcript is lost.
    pub fn follow_rewrap<'a>(
        &mut self,
        above: impl DoubleEndedIterator<Item = &'a str>,
        top: usize,
        width: u16,
    ) -> io::Result<()> {
        let width = width.max(1);
        if !self.rewraps || width == self.width || !self.drawn.is_empty() {
            return Ok(());
        }
        let rows_at =
            |row: &str, columns: u16| text::width(row.trim_end()).div_ceil(usize::from(columns)).max(1);
        let (mut seen, mut added) = (0, 0);
        for row in above.rev() {
            if seen >= top {
                break;
            }
            let before = rows_at(row, width);
            seen += before;
            added += rows_at(row, self.width).saturating_sub(before);
        }
        // The screen keeps its first row while the transcript grows into the rows below it,
        // and scrolls when it runs out of them; either way the cursor was put back at most on
        // the last row.
        let last = usize::from(self.height) - 1;
        let down = (top + added).min(last) - top.min(last);
        if down == 0 {
            return Ok(());
        }
        self.write(&"\r\n".repeat(down))
    }

    /// Rows of the screen a row drawn earlier takes at the width the terminal has now. A
    /// terminal re-wraps a row up to its last character that is not a space, whatever colour
    /// the spaces after it have; where one counts those too, a row of the old region is left
    /// behind, which is the lesser harm: counting too many would erase transcript rows.
    fn wrapped(&self, row: &str) -> usize {
        if !self.rewraps {
            return 1;
        }
        let written = text::width(text::strip_ansi(row).trim_end());
        written.div_ceil(usize::from(self.width)).max(1)
    }

    /// Erases the live region, shows the cursor and leaves it where the region began. Call before
    /// exiting or handing the terminal to a child process; the next frame draws the region again
    /// from wherever the cursor is then.
    pub fn suspend(&mut self) -> io::Result<()> {
        let mut buf = String::new();
        if !self.drawn.is_empty() || self.cursor.is_some() {
            let above = self.rows_to_top();
            if above > 0 {
                csi(&mut buf, above, 'A');
            }
            buf.push('\r');
            buf.push_str(ERASE_BELOW);
            self.restore_covered(&mut buf);
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

    /// Writes the rows the region covered as the transcript has them, from the row the cursor
    /// is on, and leaves the cursor below them, where the region's own rows began.
    fn restore_covered(&mut self, buf: &mut String) {
        let width = usize::from(self.width);
        let end = self.written.len();
        for row in self.written.range(end - self.covered..) {
            push_line(buf, &fit(row.clone().unwrap_or_default(), width));
            buf.push_str("\r\n");
        }
        self.covered = 0;
        if self.written_stale {
            self.written.clear();
            self.written_stale = false;
        }
    }

    /// Keeps a row written to the transcript, for painting it back after a region covered it.
    fn keep(&mut self, row: Option<Line>) {
        if self.written.len() >= usize::from(self.height) {
            self.written.pop_front();
        }
        self.written.push_back(row);
    }

    /// Erases the region, appends history where it began and paints the region below it, its
    /// first `cover` rows over the last rows of the transcript.
    fn draw_all(
        &mut self,
        buf: &mut String,
        history: Vec<HistoryItem>,
        rows: &[Line],
        width: usize,
        height: usize,
        cover: usize,
    ) {
        // After a change of size the rows above the cursor may have been re-wrapped.
        let above = self.rows_to_top();
        if above > 0 {
            csi(buf, above, 'A');
        }
        buf.push('\r');
        buf.push_str(ERASE_BELOW);
        self.restore_covered(buf);
        for item in history {
            match item {
                HistoryItem::Line(line) => {
                    let line = fit(line, width);
                    push_line(buf, &line);
                    buf.push_str("\r\n");
                    self.keep(Some(line));
                }
                HistoryItem::Block { rows, paint } => {
                    let rows = usize::from(rows);
                    for _ in 0..rows {
                        self.keep(None);
                    }
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
        // The rows covered were written just now or before, and are right above.
        if cover > 0 {
            csi(buf, cover, 'A');
        }
        for (i, line) in rows.iter().enumerate() {
            if i > 0 {
                buf.push_str("\r\n");
            }
            if i < cover {
                buf.push_str(ERASE_LINE);
            }
            push_line(buf, line);
        }
        self.covered = cover;
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

#[cfg(test)]
mod tests {
    use super::*;

    const CHROME: [&str; 3] = ["rule", ">", "status"];

    struct Term {
        renderer: Renderer<Vec<u8>>,
        parser: vt100::Parser,
        floor: Floor,
    }

    impl Term {
        fn new() -> Term {
            let size = |n| std::num::NonZeroU16::new(n).expect("not zero");
            Term {
                renderer: Renderer::new(Vec::new(), 20, 8),
                parser: vt100::Parser::new(size(8), size(20), 100),
                floor: Floor::new(8),
            }
        }

        /// One frame as the UI draws it: `menu` rows above the input, the transcript rows
        /// `history` written above the region.
        fn frame(&mut self, history: Vec<HistoryItem>, menu: &[&str]) {
            let written = history.iter().map(HistoryItem::rows).sum();
            let content: Vec<Line> = menu.iter().chain(&CHROME).map(|row| (*row).to_string()).collect();
            let restorable = self.renderer.restorable(&history);
            let (filler, cover) = self.floor.pad_over(written, content.len(), 8, restorable);
            self.renderer.cover(cover);
            let mut viewport = vec![String::new(); filler];
            viewport.extend(content);
            let cursor = Some((viewport.len() as u16 - 2, 2));
            self.renderer
                .render(Frame {
                    history,
                    viewport,
                    cursor,
                })
                .expect("written");
            let bytes = std::mem::take(self.renderer.get_mut());
            self.parser.process(&bytes);
        }

        fn screen(&self) -> Vec<String> {
            self.parser
                .screen()
                .rows(0, 20)
                .map(|row| row.trim_end().to_string())
                .collect()
        }

        /// Everything the terminal holds, scrollback first.
        fn everything(&mut self) -> Vec<String> {
            self.parser.screen_mut().set_scrollback(usize::MAX);
            let back = self.parser.screen().scrollback();
            let mut rows = Vec::new();
            for offset in (1..=back).rev() {
                self.parser.screen_mut().set_scrollback(offset);
                rows.push(self.screen()[0].clone());
            }
            self.parser.screen_mut().set_scrollback(0);
            rows.extend(self.screen());
            rows
        }
    }

    fn said(range: std::ops::Range<usize>) -> Vec<HistoryItem> {
        range
            .map(|n| HistoryItem::Line(format!("\x1b[1mline {n}\x1b[0m")))
            .collect()
    }

    #[test]
    fn a_menu_that_opens_and_closes_gives_the_transcript_its_rows_back() {
        let mut term = Term::new();
        term.frame(said(0..12), &[]);
        let before = term.screen();
        assert_eq!(before[..5], ["line 7", "line 8", "line 9", "line 10", "line 11"]);
        let all_before = term.everything();

        // The menu rises over the transcript; nothing scrolls into the scrollback.
        term.frame(Vec::new(), &["/help", "/model", "/quit"]);
        assert_eq!(
            term.screen(),
            [
                "line 7", "line 8", "/help", "/model", "/quit", "rule", ">", "status"
            ]
        );
        term.frame(Vec::new(), &["/model"]);
        assert_eq!(term.screen()[3..5], ["line 10", "/model"]);

        // Closed: the rows are the transcript's again, in their style, with no blank band.
        term.frame(Vec::new(), &[]);
        assert_eq!(term.screen(), before);
        assert!(term.parser.screen().cell(4, 0).expect("cell").bold());
        assert_eq!(term.everything(), all_before, "no row doubled or lost");

        // A row said while the menu is open is never covered by it: the region takes rows of
        // its own. When the menu closes they stay blank until what is said next fills them.
        term.frame(said(12..13), &["/a"]);
        assert_eq!(term.screen()[3..], ["line 12", "/a", "rule", ">", "status"]);
        term.frame(Vec::new(), &[]);
        term.frame(said(13..14), &[]);
        assert_eq!(term.screen()[3..], ["line 12", "line 13", "rule", ">", "status"]);
        let all = term.everything();
        assert_eq!(all.iter().filter(|row| row.starts_with("line ")).count(), 14);
    }

    #[test]
    fn rows_written_while_a_tall_region_stands_stay_in_sight_above_it() {
        let mut term = Term::new();
        term.frame(said(0..12), &[]);
        // A menu rises over the transcript.
        term.frame(Vec::new(), &["/help", "/model", "/quit"]);
        assert_eq!(term.screen()[..2], ["line 7", "line 8"]);

        // A message is said while the region is still that tall: the covered rows are given
        // back and the new row lands directly above the region.
        term.frame(said(12..13), &["/help", "/model", "/quit"]);
        let screen = term.screen();
        assert_eq!(screen[1], "line 12", "{screen:?}");
        assert_eq!(screen[2..], ["/help", "/model", "/quit", "rule", ">", "status"]);
        let all = term.everything();
        for n in 0..13 {
            let name = format!("line {n}");
            assert_eq!(all.iter().filter(|row| **row == name).count(), 1, "{name}");
        }

        // Closed: the newest row is still on the screen, and the next one follows it.
        term.frame(Vec::new(), &[]);
        assert!(term.screen().contains(&"line 12".to_string()));
        term.frame(said(13..14), &[]);
        // The rows the menu took stay the region's floor; the transcript grows into them.
        assert_eq!(term.screen()[..3], ["line 11", "line 12", "line 13"]);
    }

    #[test]
    fn a_picture_above_the_input_is_not_drawn_over() {
        let mut term = Term::new();
        term.frame(said(0..12), &[]);
        let picture = HistoryItem::Block {
            rows: 2,
            paint: String::new(),
        };
        term.frame(vec![picture], &[]);
        // The rows of a picture cannot be painted back: the menu takes rows of its own and the
        // transcript scrolls, as it did before.
        term.frame(Vec::new(), &["/help", "/model"]);
        assert_eq!(term.screen()[..3], ["line 11", "", ""]);
        assert_eq!(term.screen()[3..], ["/help", "/model", "rule", ">", "status"]);
        assert_eq!(
            term.everything()
                .iter()
                .filter(|row| row.starts_with("line "))
                .count(),
            12
        );
    }

    #[test]
    fn a_region_that_covers_rows_is_erased_down_to_the_transcript() {
        let mut term = Term::new();
        term.frame(said(0..12), &[]);
        term.frame(Vec::new(), &["/help", "/model"]);
        term.renderer.suspend().expect("written");
        let bytes = std::mem::take(term.renderer.get_mut());
        term.parser.process(&bytes);
        assert_eq!(
            term.screen()[..5],
            ["line 7", "line 8", "line 9", "line 10", "line 11"]
        );
        assert_eq!(term.parser.screen().cursor_position(), (5, 0));
    }
}
