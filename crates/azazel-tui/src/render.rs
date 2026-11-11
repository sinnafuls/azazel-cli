//! Inline differential renderer.
//!
//! The renderer owns a *live region* at the bottom of the terminal's normal screen. History lines
//! are written once above it and become ordinary scrollback; the region itself is compared row by
//! row with what was drawn last time and only the changed range is rewritten.
//!
//! The absolute position of the region is unknown and never queried. The renderer only remembers
//! on which row of the region it left the terminal cursor and moves relative to that.

use std::io;

use crate::text;

/// One terminal row: text plus SGR sequences. Its display width must not exceed the terminal
/// width; the renderer cuts lines that are too wide rather than letting them wrap.
pub type Line = String;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    /// Lines to append to the transcript above the live region. Written exactly once.
    pub history: Vec<Line>,
    /// The complete live region for this frame.
    pub viewport: Vec<Line>,
    /// Where to show the terminal cursor as `(row within viewport, column)`; `None` hides it.
    pub cursor: Option<(u16, u16)>,
}

const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const HIDE_CURSOR: &str = "\x1b[?25l";
const ERASE_LINE: &str = "\x1b[2K";
const ERASE_BELOW: &str = "\x1b[J";
const SGR_RESET: &str = "\x1b[0m";

pub struct Renderer<W: io::Write> {
    out: W,
    width: u16,
    height: u16,
    /// The rows of the live region as they are on screen.
    drawn: Vec<Line>,
    /// Row of the terminal cursor counted from the first row of the live region. Always below
    /// `max(drawn.len(), 1)`: an empty region still owns the row the cursor is on.
    cursor_row: usize,
    /// Cursor placement requested by the last frame, after clipping.
    cursor: Option<(usize, usize)>,
    cursor_visible: bool,
    /// Set by `resize`: the next frame repaints the whole region.
    redraw: bool,
}

impl<W: io::Write> Renderer<W> {
    pub fn new(out: W, width: u16, height: u16) -> Self {
        Self {
            out,
            width: width.max(1),
            height: height.max(1),
            drawn: Vec::new(),
            cursor_row: 0,
            cursor: None,
            cursor_visible: true,
            redraw: false,
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
            self.draw_all(&mut buf, history, &rows, width);
        }
        self.drawn = rows;
        self.redraw = false;
        self.place_cursor(&mut buf, cursor);
        buf.push_str(SYNC_END);
        self.write(&buf)
    }

    /// Records a new terminal size. The next frame erases and repaints the live region; history
    /// already written is left to the terminal. A terminal that re-wraps its content when it gets
    /// narrower moves rows the renderer cannot see, so part of the old region may stay behind
    /// above the repainted one.
    pub fn resize(&mut self, width: u16, height: u16) {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) != (self.width, self.height) {
            self.width = width;
            self.height = height;
            self.redraw = true;
        }
    }

    /// Erases the live region, shows the cursor and leaves it where the region began. Call before
    /// exiting or handing the terminal to a child process; the next frame draws the region again
    /// from wherever the cursor is then.
    pub fn suspend(&mut self) -> io::Result<()> {
        let mut buf = String::new();
        if !self.drawn.is_empty() || self.cursor.is_some() {
            if self.cursor_row > 0 {
                csi(&mut buf, self.cursor_row, 'A');
            }
            buf.push('\r');
            buf.push_str(ERASE_BELOW);
        }
        if !self.cursor_visible {
            buf.push_str(SHOW_CURSOR);
        }
        self.drawn.clear();
        self.cursor_row = 0;
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
        }

        if rows.len() < old_len {
            self.move_to_row(buf, rows.len());
            buf.push('\r');
            buf.push_str(ERASE_BELOW);
            self.move_to_row(buf, rows.len().saturating_sub(1));
        }
    }

    /// Erases the region, appends history where it began and paints the region below it.
    fn draw_all(&mut self, buf: &mut String, history: Vec<Line>, rows: &[Line], width: usize) {
        if self.cursor_row > 0 {
            csi(buf, self.cursor_row, 'A');
        }
        buf.push('\r');
        buf.push_str(ERASE_BELOW);
        for line in history {
            push_line(buf, &fit(line, width));
            buf.push_str("\r\n");
        }
        for (i, line) in rows.iter().enumerate() {
            if i > 0 {
                buf.push_str("\r\n");
            }
            push_line(buf, line);
        }
        self.cursor_row = rows.len().saturating_sub(1);
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

fn push_line(buf: &mut String, line: &str) {
    buf.push_str(line);
    // Attributes left open would colour the next erase and the rows below.
    if line.contains('\x1b') && !line.ends_with(SGR_RESET) {
        buf.push_str(SGR_RESET);
    }
}

/// Guarantees that a line occupies exactly one terminal row.
fn fit(line: Line, width: usize) -> Line {
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
