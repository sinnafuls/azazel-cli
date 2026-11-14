//! Drives the renderer's byte output into an emulated terminal and checks what ends up on screen.

use std::num::NonZeroU16;

use azazel_tui::render::{Floor, Frame, HistoryItem, Renderer};

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap()
}

fn lines(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

fn history(items: &[&str]) -> Vec<HistoryItem> {
    items.iter().map(|s| HistoryItem::from(*s)).collect()
}

fn frame(above: &[&str], viewport: &[&str]) -> Frame {
    Frame {
        history: history(above),
        viewport: lines(viewport),
        cursor: None,
    }
}

struct Harness {
    renderer: Renderer<Vec<u8>>,
    parser: vt100::Parser,
}

impl Harness {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            renderer: Renderer::new(Vec::new(), cols, rows),
            parser: vt100::Parser::new(nz(rows), nz(cols), 200),
        }
    }

    /// Moves what the renderer wrote since the last call into the terminal and returns it.
    fn flush(&mut self) -> Vec<u8> {
        let bytes = std::mem::take(self.renderer.get_mut());
        self.parser.process(&bytes);
        bytes
    }

    fn render(&mut self, frame: Frame) -> Vec<u8> {
        self.renderer.render(frame).unwrap();
        self.flush()
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.set_size(nz(rows), nz(cols));
        self.renderer.resize(cols, rows);
    }

    /// Every visible row, right-trimmed.
    fn screen(&self) -> Vec<String> {
        let cols = self.parser.screen().size().1.get();
        self.parser
            .screen()
            .rows(0, cols)
            .map(|row| row.trim_end().to_string())
            .collect()
    }

    /// The visible rows without the blank rows at the bottom.
    fn used_rows(&self) -> Vec<String> {
        let mut rows = self.screen();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows
    }
}

fn count(haystack: &[String], needle: &str) -> usize {
    haystack.iter().filter(|row| row.as_str() == needle).count()
}

#[test]
fn history_sits_above_a_growing_viewport() {
    let mut term = Harness::new(40, 12);
    term.render(frame(&["h1", "h2"], &["a1"]));
    assert_eq!(term.used_rows(), ["h1", "h2", "a1"]);
    term.render(frame(&["h3"], &["b1", "b2"]));
    assert_eq!(term.used_rows(), ["h1", "h2", "h3", "b1", "b2"]);
    term.render(frame(&[], &["c1", "c2", "c3"]));

    let rows = term.used_rows();
    assert_eq!(rows, ["h1", "h2", "h3", "c1", "c2", "c3"]);
    for line in ["h1", "h2", "h3"] {
        assert_eq!(count(&rows, line), 1, "{line} must appear exactly once");
    }
}

#[test]
fn history_scrolls_into_native_scrollback_in_order() {
    let mut term = Harness::new(20, 5);
    let above = (1..=8).map(|i| format!("h{i}").into()).collect();
    term.render(Frame {
        history: above,
        viewport: lines(&["v1", "v2"]),
        cursor: None,
    });
    assert_eq!(term.screen(), ["h6", "h7", "h8", "v1", "v2"]);

    // Later frames touch the live region only.
    let bytes = term.render(frame(&[], &["w1", "w2"]));
    assert!(!String::from_utf8(bytes).unwrap().contains("h8"));
    assert_eq!(term.screen(), ["h6", "h7", "h8", "w1", "w2"]);

    term.render(frame(&["h9"], &["w1", "w2"]));
    assert_eq!(term.screen(), ["h7", "h8", "h9", "w1", "w2"]);

    // Six lines have scrolled off; viewing the oldest of them shows history in its original order.
    term.parser.screen_mut().set_scrollback(6);
    assert_eq!(term.screen(), ["h1", "h2", "h3", "h4", "h5"]);
}

#[test]
fn changing_one_row_rewrites_only_that_row() {
    let mut term = Harness::new(40, 24);
    let mut viewport: Vec<String> = (0..10).map(|i| format!("row-{i}-content")).collect();
    term.render(Frame {
        history: Vec::new(),
        viewport: viewport.clone(),
        cursor: None,
    });
    assert_eq!(term.used_rows(), viewport);

    viewport[4] = "CHANGED".to_string();
    let bytes = term.render(Frame {
        history: Vec::new(),
        viewport: viewport.clone(),
        cursor: None,
    });
    let written = String::from_utf8(bytes.clone()).unwrap();

    assert!(written.contains("CHANGED"));
    assert!(
        !written.contains("row-"),
        "unchanged rows were rewritten: {written:?}"
    );
    // Synchronized-output begin and end (16 bytes), one cursor move, a carriage return and an
    // erase-line (9 bytes) surround the row's own text.
    assert!(
        bytes.len() <= "CHANGED".len() + 28,
        "wrote {} bytes: {written:?}",
        bytes.len()
    );
    assert_eq!(term.used_rows(), viewport);

    // Two adjacent changes rewrite exactly those two rows.
    viewport[0] = "TOP".to_string();
    viewport[1] = "SECOND".to_string();
    let bytes = term.render(Frame {
        history: Vec::new(),
        viewport: viewport.clone(),
        cursor: None,
    });
    let written = String::from_utf8(bytes).unwrap();
    assert!(written.contains("TOP") && written.contains("SECOND"));
    assert!(!written.contains("row-") && !written.contains("CHANGED"));
    assert_eq!(term.used_rows(), viewport);
}

#[test]
fn resize_redraws_the_viewport_once_and_keeps_history_single() {
    let mut term = Harness::new(40, 12);
    term.render(frame(&["h1", "h2", "h3"], &["v1", "v2", "v3", "v4"]));

    term.resize(30, 10);
    assert_eq!(term.renderer.size(), (30, 10));
    let bytes = term.render(frame(&[], &["v1", "v2", "v3", "v4"]));
    let written = String::from_utf8(bytes).unwrap();
    assert!(
        written.contains("v1"),
        "an unchanged frame must still be repainted after a resize"
    );
    assert!(
        !written.contains("h1") && !written.contains("h3"),
        "history must not be rewritten"
    );

    let rows = term.used_rows();
    assert_eq!(rows, ["h1", "h2", "h3", "v1", "v2", "v3", "v4"]);
    for line in ["h1", "h2", "h3", "v1", "v2", "v3", "v4"] {
        assert_eq!(count(&rows, line), 1, "{line} must appear exactly once");
    }

    // The repaint happens once: the same frame again is a no-op.
    assert!(term.render(frame(&[], &["v1", "v2", "v3", "v4"])).is_empty());

    // A resize to the same size does not force a repaint.
    term.resize(30, 10);
    assert!(term.render(frame(&[], &["v1", "v2", "v3", "v4"])).is_empty());
}

#[test]
fn every_write_is_one_synchronized_update() {
    let mut term = Harness::new(40, 12);
    for bytes in [
        term.render(frame(&["h"], &["a", "b"])),
        term.render(frame(&[], &["a", "c"])),
        term.render(frame(&[], &[])),
    ] {
        let written = String::from_utf8(bytes).unwrap();
        assert!(written.starts_with("\x1b[?2026h"), "{written:?}");
        assert!(written.ends_with("\x1b[?2026l"), "{written:?}");
        assert_eq!(written.matches("\x1b[?2026h").count(), 1);
        assert_eq!(written.matches("\x1b[?2026l").count(), 1);
    }
}

#[test]
fn shrinking_viewport_leaves_no_stale_rows() {
    let mut term = Harness::new(40, 12);
    term.render(frame(&["h"], &["r1", "r2", "r3", "r4", "r5"]));
    term.render(frame(&[], &["r1", "x"]));
    assert_eq!(term.used_rows(), ["h", "r1", "x"]);

    term.render(frame(&[], &[]));
    assert_eq!(term.used_rows(), ["h"]);

    // The region grows again from where it began.
    term.render(frame(&[], &["again", "and again"]));
    assert_eq!(term.used_rows(), ["h", "again", "and again"]);
}

#[test]
fn growing_viewport_at_the_bottom_scrolls_history_up() {
    let mut term = Harness::new(20, 4);
    term.render(frame(&["h1", "h2"], &["a"]));
    assert_eq!(term.screen(), ["h1", "h2", "a", ""]);
    term.render(frame(&[], &["a", "b", "c"]));
    assert_eq!(term.screen(), ["h2", "a", "b", "c"]);
    term.render(frame(&[], &["a", "B", "c"]));
    assert_eq!(term.screen(), ["h2", "a", "B", "c"]);
}

#[test]
fn viewport_taller_than_terminal_shows_its_last_rows() {
    let mut term = Harness::new(20, 4);
    let mut tall = frame(&[], &["r1", "r2", "r3", "r4", "r5", "r6"]);
    tall.cursor = Some((5, 1));
    term.render(tall);
    assert_eq!(term.screen(), ["r3", "r4", "r5", "r6"]);
    assert_eq!(term.parser.screen().cursor_position(), (3, 1));
    assert!(!term.parser.screen().hide_cursor());

    // A cursor on a row that was cut off cannot be shown.
    let mut clipped = frame(&[], &["r1", "r2", "r3", "r4", "r5", "r6"]);
    clipped.cursor = Some((0, 0));
    term.render(clipped);
    assert!(term.parser.screen().hide_cursor());
    assert_eq!(term.screen(), ["r3", "r4", "r5", "r6"]);
}

#[test]
fn cursor_is_placed_relative_to_the_viewport() {
    let mut term = Harness::new(40, 12);
    let mut shown = frame(&["h"], &["one", "two", "three"]);
    shown.cursor = Some((1, 2));
    term.render(shown);
    assert_eq!(term.parser.screen().cursor_position(), (2, 2));
    assert!(!term.parser.screen().hide_cursor());

    // Moving only the cursor rewrites no row text.
    let mut moved = frame(&[], &["one", "two", "three"]);
    moved.cursor = Some((2, 5));
    let written = String::from_utf8(term.render(moved)).unwrap();
    assert!(!written.contains("one") && !written.contains("three"));
    assert_eq!(term.parser.screen().cursor_position(), (3, 5));

    term.render(frame(&[], &["one", "two", "three"]));
    assert!(term.parser.screen().hide_cursor());

    let mut back = frame(&[], &["one", "two", "three"]);
    back.cursor = Some((0, 0));
    term.render(back);
    assert!(!term.parser.screen().hide_cursor());
    assert_eq!(term.parser.screen().cursor_position(), (1, 0));
}

#[test]
fn suspend_erases_the_viewport_and_the_next_frame_redraws_it() {
    let mut term = Harness::new(40, 12);
    term.render(frame(&["h"], &["a", "b"]));
    assert!(term.parser.screen().hide_cursor());

    term.renderer.suspend().unwrap();
    assert!(!term.flush().is_empty());
    assert_eq!(term.used_rows(), ["h"]);
    assert_eq!(term.parser.screen().cursor_position(), (1, 0));
    assert!(!term.parser.screen().hide_cursor());

    // Suspending again has nothing left to do.
    term.renderer.suspend().unwrap();
    assert!(term.flush().is_empty());

    // Something else uses the terminal, then the region comes back below its output.
    term.parser.process(b"child output\r\n");
    term.render(frame(&[], &["a", "b"]));
    assert_eq!(term.used_rows(), ["h", "child output", "a", "b"]);
}

/// Draws a frame the way an application with a bottom-anchored input does: blank rows between
/// what follows the transcript (`top`) and what stays at the bottom (`bottom`).
fn anchored(term: &mut Harness, floor: &mut Floor, above: &[&str], top: &[&str], bottom: &[&str]) -> Vec<u8> {
    anchored_items(term, floor, history(above), top, bottom)
}

fn anchored_items(
    term: &mut Harness,
    floor: &mut Floor,
    above: Vec<HistoryItem>,
    top: &[&str],
    bottom: &[&str],
) -> Vec<u8> {
    let (_, height) = term.renderer.size();
    let rows = above.iter().map(HistoryItem::rows).sum();
    let filler = floor.pad(rows, top.len() + bottom.len(), height);
    let mut viewport = lines(top);
    viewport.resize(top.len() + filler, String::new());
    viewport.extend(lines(bottom));
    term.render(Frame {
        history: above,
        viewport,
        cursor: None,
    })
}

#[test]
fn floor_pins_the_last_rows_while_the_transcript_fills_the_gap() {
    let mut term = Harness::new(20, 6);
    // The program starts below whatever the shell left on screen.
    term.parser.process(b"$ ls\r\nfile\r\n$ app\r\n");
    let mut floor = Floor::new(6);
    let chrome = ["rule", ">", "status"];

    // The first frame fills the screen: the input is on the last rows at once and the shell's
    // lines have scrolled away.
    anchored(&mut term, &mut floor, &["banner"], &[], &chrome);
    assert_eq!(term.screen(), ["banner", "", "", "rule", ">", "status"]);

    // New transcript lines take rows from the gap; the input does not move.
    anchored(&mut term, &mut floor, &["you: hi"], &[], &chrome);
    assert_eq!(term.screen(), ["banner", "you: hi", "", "rule", ">", "status"]);
    anchored(&mut term, &mut floor, &[], &["streaming"], &chrome);
    assert_eq!(
        term.screen(),
        ["banner", "you: hi", "streaming", "rule", ">", "status"]
    );

    // With the gap used up the transcript scrolls, as it always did.
    anchored(&mut term, &mut floor, &["a", "b"], &[], &chrome);
    assert_eq!(term.screen(), ["you: hi", "a", "b", "rule", ">", "status"]);

    // A region that grew keeps its height when its content shrinks again, so the input stays
    // put, and the next transcript line fills the row that was freed.
    anchored(&mut term, &mut floor, &[], &["card"], &chrome);
    assert_eq!(term.screen(), ["a", "b", "card", "rule", ">", "status"]);
    anchored(&mut term, &mut floor, &[], &[], &chrome);
    assert_eq!(term.screen(), ["a", "b", "", "rule", ">", "status"]);
    anchored(&mut term, &mut floor, &["c"], &[], &chrome);
    assert_eq!(term.screen(), ["a", "b", "c", "rule", ">", "status"]);

    // Nothing was lost or doubled on the way into the scrollback.
    term.parser.screen_mut().set_scrollback(5);
    assert_eq!(term.screen(), ["$ ls", "file", "$ app", "banner", "you: hi", "a"]);
    term.parser.screen_mut().set_scrollback(0);

    // Leaving erases the region: the shell's prompt lands right under the transcript.
    term.renderer.suspend().unwrap();
    term.flush();
    assert_eq!(term.screen(), ["a", "b", "c", "", "", ""]);
    assert_eq!(term.parser.screen().cursor_position(), (3, 0));
}

#[test]
fn floor_follows_the_screen() {
    let mut floor = Floor::new(10);
    assert_eq!(floor.pad(0, 3, 10), 7);
    assert_eq!(floor.rows(), 10);

    // A shorter screen caps the region; a taller one grows it by the rows the screen gained.
    floor.resize(5, 10);
    assert_eq!(floor.pad(0, 3, 5), 2);
    floor.resize(30, 5);
    assert_eq!(floor.rows(), 30);
    assert_eq!(floor.pad(0, 3, 30), 27);

    // A region the terminal re-wrapped onto more rows keeps them: its bottom is still the
    // bottom of the screen.
    let mut wrapped = Floor::new(20);
    assert_eq!(wrapped.pad(10, 4, 20), 6);
    wrapped.resize(20, 13);
    assert_eq!(wrapped.rows(), 13);
    wrapped.resize(12, 15);
    assert_eq!(wrapped.rows(), 12);

    // Told where the region begins, the floor is that, within the screen.
    wrapped.anchor(7);
    assert_eq!(wrapped.rows(), 7);
    wrapped.anchor(40);
    assert_eq!(wrapped.rows(), 12);
    wrapped.anchor(0);
    assert_eq!(wrapped.rows(), 1);

    // Content taller than the screen needs no padding and does not raise the floor past it.
    floor.anchor(5);
    assert_eq!(floor.pad(0, 9, 30), 0);
    assert_eq!(floor.pad(0, 40, 30), 0);
    assert_eq!(floor.rows(), 30);
    // More history than there are rows left cannot take the floor below the content.
    assert_eq!(floor.pad(100, 3, 30), 0);
    assert_eq!(floor.rows(), 3);

    // After the screen was cleared the region fills it again.
    floor.reset(8);
    assert_eq!(floor.pad(1, 3, 8), 4);
}

#[test]
fn overlong_lines_are_cut_instead_of_wrapping() {
    let mut term = Harness::new(10, 5);
    let viewport = ["0123456789ABCDEF", "日本語日本語日本語", "next"];
    term.render(frame(&["history is far too long"], &viewport));
    // Ten columns hold ten narrow or five wide characters.
    assert_eq!(
        term.screen(),
        ["history is", "0123456789", "日本語日本", "next", ""]
    );
}

#[test]
fn styles_do_not_bleed_into_following_rows() {
    let mut term = Harness::new(20, 5);
    term.render(frame(&[], &["\x1b[31;44mred on blue", "plain"]));
    let screen = term.parser.screen();
    assert_eq!(screen.cell(0, 0).unwrap().fgcolor(), vt100::Color::Idx(1));
    assert_eq!(screen.cell(1, 0).unwrap().fgcolor(), vt100::Color::Default);
    assert_eq!(screen.cell(1, 0).unwrap().bgcolor(), vt100::Color::Default);
    // Cells erased while repainting a row must not pick up the old background either.
    term.render(frame(&[], &["\x1b[31;44mred on blue", "p"]));
    let screen = term.parser.screen();
    assert_eq!(screen.cell(1, 3).unwrap().bgcolor(), vt100::Color::Default);
    assert_eq!(term.used_rows(), ["red on blue", "p"]);
}

#[test]
fn a_block_takes_its_rows_between_two_lines() {
    let mut term = Harness::new(20, 8);
    // The paint writes on each of its rows and leaves the cursor at the end of the last one.
    let block = HistoryItem::Block {
        rows: 3,
        paint: "AAA\x1b[1B\rBBB\x1b[1B\rCCC".to_string(),
    };
    term.render(Frame {
        history: vec!["before".into(), block, "after".into()],
        viewport: lines(&["v1", "v2"]),
        cursor: Some((1, 2)),
    });
    assert_eq!(
        term.used_rows(),
        ["before", "AAA", "BBB", "CCC", "after", "v1", "v2"]
    );
    assert_eq!(term.parser.screen().cursor_position(), (6, 2));
    // The renderer still knows where its rows are: a change lands on the right one.
    term.render(frame(&[], &["v1", "changed"]));
    assert_eq!(
        term.used_rows(),
        ["before", "AAA", "BBB", "CCC", "after", "v1", "changed"]
    );
}

#[test]
fn an_image_block_that_scrolls_the_screen_keeps_the_viewport_on_its_rows() {
    let mut term = Harness::new(20, 10);
    let mut floor = Floor::new(10);
    anchored(&mut term, &mut floor, &["one", "two"], &[], &["input", "status"]);
    assert_eq!(term.screen()[8..], ["input", "status"]);
    // A kitty image (the terminal draws it without moving the cursor), marked on its first and
    // last row so the test can see where it went.
    let image = HistoryItem::Block {
        rows: 6,
        paint: "\x1b_Ga=T,f=100,q=2,C=1,r=6;AAAA\x1b\\top\x1b[5B\rbottom".to_string(),
    };
    anchored_items(
        &mut term,
        &mut floor,
        vec!["card".into(), image, "text after".into()],
        &[],
        &["input", "status"],
    );
    // Ten rows of transcript and two of viewport on a ten-row screen: the first two rows of the
    // transcript have scrolled away.
    assert_eq!(
        term.screen(),
        [
            "card",
            "top",
            "",
            "",
            "",
            "",
            "bottom",
            "text after",
            "input",
            "status"
        ]
    );
    anchored(
        &mut term,
        &mut floor,
        &["later"],
        &["streaming"],
        &["input!", "status"],
    );
    assert_eq!(
        term.screen(),
        [
            "",
            "",
            "",
            "",
            "bottom",
            "text after",
            "later",
            "streaming",
            "input!",
            "status"
        ]
    );
}

#[test]
fn a_block_taller_than_the_screen_is_left_blank() {
    let mut term = Harness::new(20, 4);
    let block = HistoryItem::Block {
        rows: 4,
        paint: "PAINT".to_string(),
    };
    term.render(Frame {
        history: vec![block],
        viewport: lines(&["v"]),
        cursor: None,
    });
    assert_eq!(term.screen(), ["", "", "", "v"]);
}

// ------------------------------------------------------------ changes of size

impl Harness {
    /// Changes the size the way a terminal that re-wraps does (Windows Terminal, VTE, kitty,
    /// iTerm2): every row the renderer wrote is a line of its own, a line that no longer fits
    /// goes on over the next rows, the end of the content stays on screen and the cursor stays
    /// on its character. The renderer is not told.
    fn rewrap(&mut self, cols: u16, rows: u16) {
        let screen = self.parser.screen();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let (cursor_row, cursor_col) = (usize::from(cursor_row), usize::from(cursor_col));
        let mut logical = self.screen();
        let used = logical
            .iter()
            .rposition(|line| !line.is_empty())
            .map_or(0, |last| last + 1)
            .max(cursor_row + 1);
        logical.truncate(used);

        let width = usize::from(cols);
        let mut parser = vt100::Parser::new(nz(rows), nz(cols), 200);
        let (mut total, mut cursor) = (0, (0, 0));
        for (index, line) in logical.iter().enumerate() {
            let wraps = line.chars().count().div_ceil(width).max(1);
            if index == cursor_row {
                let within = (cursor_col / width).min(wraps - 1);
                let column = if cursor_col / width < wraps {
                    cursor_col % width
                } else {
                    width - 1
                };
                cursor = (total + within, column);
            }
            total += wraps;
            if index > 0 {
                parser.process(b"\r\n");
            }
            parser.process(line.as_bytes());
        }
        // The last row of the content is on the last row it reached on the screen.
        let bottom = total.min(usize::from(rows)) - 1;
        let row = bottom.saturating_sub(total - 1 - cursor.0);
        parser.process(format!("\x1b[{};{}H", row + 1, cursor.1 + 1).as_bytes());
        self.parser = parser;
    }

    /// The renderer and the floor hear of the size the terminal has now.
    fn settle(&mut self, floor: &mut Floor) {
        let (rows, cols) = self.parser.screen().size();
        self.renderer.resize(cols.get(), rows.get());
        floor.resize(rows.get(), self.renderer.rows_on_screen());
    }

    /// Tells the floor where the region begins from where the terminal has its cursor, as an
    /// application does that can ask.
    fn anchor(&self, floor: &mut Floor) {
        let (row, _) = self.parser.screen().cursor_position();
        let (_, height) = self.renderer.size();
        let top = usize::from(row).saturating_sub(self.renderer.rows_above_cursor());
        floor.anchor(usize::from(height) - top);
    }

    /// Every row the terminal holds: its scrollback, then the screen.
    fn everything(&mut self) -> Vec<String> {
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let depth = self.parser.screen().scrollback();
        let mut rows = Vec::new();
        for offset in (1..=depth).rev() {
            self.parser.screen_mut().set_scrollback(offset);
            rows.push(self.screen()[0].clone());
        }
        self.parser.screen_mut().set_scrollback(0);
        rows.extend(self.screen());
        rows
    }
}

const CHROME: [&str; 3] = ["rule", ">", "status"];

#[test]
fn a_taller_screen_keeps_the_input_on_the_last_rows() {
    let mut term = Harness::new(20, 6);
    let mut floor = Floor::new(6);
    anchored(&mut term, &mut floor, &["banner", "you: hi"], &[], &CHROME);
    assert_eq!(term.screen(), ["banner", "you: hi", "", "rule", ">", "status"]);

    // Nothing has scrolled away, so the terminal adds the new rows below the content.
    term.parser.set_size(nz(9), nz(20));
    term.settle(&mut floor);
    let repaint = anchored(&mut term, &mut floor, &[], &[], &CHROME);
    assert_eq!(
        term.screen(),
        ["banner", "you: hi", "", "", "", "", "rule", ">", "status"]
    );
    // One repaint of the region, and the transcript is not written again.
    let repaint = String::from_utf8(repaint).unwrap();
    assert_eq!(repaint.matches("status").count(), 1);
    assert!(!repaint.contains("banner"));
    assert!(anchored(&mut term, &mut floor, &[], &[], &CHROME).is_empty());

    // The transcript goes on growing into the gap.
    anchored(&mut term, &mut floor, &["reply"], &[], &CHROME);
    assert_eq!(
        term.screen(),
        ["banner", "you: hi", "reply", "", "", "", "rule", ">", "status"]
    );
}

#[test]
fn a_taller_screen_that_brings_scrollback_back_keeps_the_input_on_the_last_rows() {
    let start = || {
        let mut term = Harness::new(20, 6);
        term.parser.process(b"$ ls\r\nfile\r\n$ app\r\n");
        let mut floor = Floor::new(6);
        anchored(&mut term, &mut floor, &["banner"], &[], &CHROME);
        assert_eq!(term.screen(), ["banner", "", "", "rule", ">", "status"]);
        // This terminal fills the new rows from its scrollback: the content moves down.
        term.parser.set_size(nz(9), nz(20));
        assert_eq!(term.screen()[..4], ["$ ls", "file", "$ app", "banner"]);
        term.settle(&mut floor);
        (term, floor)
    };

    // Not knowing what the terminal did, the region takes the rows the screen gained: the
    // rows that came back leave again, and the input is where it belongs.
    let (mut term, mut floor) = start();
    anchored(&mut term, &mut floor, &[], &[], &CHROME);
    assert_eq!(
        term.screen(),
        ["banner", "", "", "", "", "", "rule", ">", "status"]
    );
    assert_eq!(term.everything()[..4], ["$ ls", "file", "$ app", "banner"]);

    // Knowing where the cursor is, the region stays as tall as it was and the rows that came
    // back stay on the screen.
    let (mut term, mut floor) = start();
    term.anchor(&mut floor);
    anchored(&mut term, &mut floor, &[], &[], &CHROME);
    assert_eq!(
        term.screen(),
        ["$ ls", "file", "$ app", "banner", "", "", "rule", ">", "status"]
    );
}

#[test]
fn a_shorter_screen_keeps_the_input_and_loses_no_transcript() {
    let mut term = Harness::new(20, 8);
    let mut floor = Floor::new(8);
    let above = ["one", "two", "three", "four"];
    // The cursor is in the input, with the status row below it.
    let paint = |term: &mut Harness, floor: &mut Floor, above: &[&str]| {
        let (_, height) = term.renderer.size();
        let filler = floor.pad(above.len(), CHROME.len(), height);
        let mut viewport = vec![String::new(); filler];
        viewport.extend(lines(&CHROME));
        term.render(Frame {
            history: history(above),
            viewport,
            cursor: Some((filler as u16 + 1, 2)),
        });
    };
    paint(&mut term, &mut floor, &above);
    assert_eq!(
        term.screen(),
        ["one", "two", "three", "four", "", "rule", ">", "status"]
    );

    term.parser.set_size(nz(5), nz(20));
    term.settle(&mut floor);
    paint(&mut term, &mut floor, &[]);
    assert_eq!(term.screen()[2..], ["rule", ">", "status"]);
    assert_eq!(term.parser.screen().cursor_position(), (3, 2));
    let all = term.everything();
    for line in above.iter().chain(&CHROME) {
        assert_eq!(count(&all, line), 1, "{line} in {all:#?}");
    }
}

#[test]
fn a_burst_of_sizes_is_one_repaint_at_the_last_of_them() {
    let mut term = Harness::new(20, 6);
    let mut floor = Floor::new(6);
    anchored(&mut term, &mut floor, &["banner", "you: hi"], &[], &CHROME);

    // The window is dragged: the terminal is at each size in turn, the renderer hears of the
    // last one only.
    for (cols, rows) in [(30, 8), (12, 5), (26, 7), (24, 10)] {
        term.parser.set_size(nz(rows), nz(cols));
    }
    term.settle(&mut floor);
    let repaint = anchored(&mut term, &mut floor, &[], &[], &CHROME);
    let repaint = String::from_utf8(repaint).unwrap();
    assert_eq!(repaint.matches("\x1b[?2026h").count(), 1);
    for row in CHROME {
        assert_eq!(repaint.matches(row).count(), 1, "{row}");
    }

    let screen = term.screen();
    assert_eq!(screen.len(), 10);
    assert_eq!(screen[7..], ["rule", ">", "status"]);
    let all = term.everything();
    for line in ["banner", "you: hi", "rule", ">", "status"] {
        assert_eq!(count(&all, line), 1, "{line} in {all:#?}");
    }
    assert!(anchored(&mut term, &mut floor, &[], &[], &CHROME).is_empty());

    // Told where the cursor is after such a burst, the floor is exact whatever happened.
    for (cols, rows) in [(18, 4), (40, 12)] {
        term.parser.set_size(nz(rows), nz(cols));
    }
    term.settle(&mut floor);
    term.anchor(&mut floor);
    anchored(&mut term, &mut floor, &["later"], &[], &CHROME);
    let screen = term.screen();
    assert_eq!(screen[9..], ["rule", ">", "status"]);
    let all = term.everything();
    for line in ["banner", "you: hi", "later", "rule", ">", "status"] {
        assert_eq!(count(&all, line), 1, "{line} in {all:#?}");
    }
}

/// A frame with the cursor in the input, as the application draws it: what is in flight, blank
/// rows down to the floor, then a rule, the input, a rule and the status line, all as wide as
/// the screen.
fn wide_frame(term: &mut Harness, floor: &mut Floor, above: &[&str], top: &[&str]) {
    let (width, height) = term.renderer.size();
    let rule = "-".repeat(usize::from(width));
    let status = format!("{:<1$}think", "model", usize::from(width) - 5);
    let bottom = [rule.as_str(), "> hello", rule.as_str(), status.as_str()];
    let filler = floor.pad(above.len(), top.len() + bottom.len(), height);
    let mut viewport = lines(top);
    viewport.resize(top.len() + filler, String::new());
    let input = viewport.len() as u16 + 1;
    viewport.extend(lines(&bottom));
    term.render(Frame {
        history: history(above),
        viewport,
        cursor: Some((input, 7)),
    });
}

#[test]
fn a_narrower_screen_that_rewraps_leaves_nothing_of_the_old_region_behind() {
    let mut term = Harness::new(20, 14);
    let mut floor = Floor::new(14);
    let above = ["h1", "h2", "h3", "h4", "h5", "h6"];
    // A row that is filled to the edge with coloured spaces, as a line of a code block is.
    let code = format!("\x1b[48;5;236mcode{}\x1b[0m", " ".repeat(16));
    let top = ["streaming", code.as_str()];
    wide_frame(&mut term, &mut floor, &above, &top);
    assert_eq!(term.screen()[6..8], ["streaming", "code"]);
    assert_eq!(term.screen()[13], "model          think");

    // Both rules and the status line now take two rows each: the region is three rows taller
    // than it was drawn, and the cursor, which is below the first rule, one row further from
    // its first row. The spaces after `code` are not text and take no row.
    term.rewrap(12, 14);
    assert_eq!(
        count(&term.screen(), "------------") + count(&term.screen(), "--------"),
        4
    );
    term.settle(&mut floor);
    assert_eq!(term.renderer.rows_on_screen(), 11);
    assert_eq!(term.renderer.rows_above_cursor(), 6);
    let code = format!("\x1b[48;5;236mcode{}\x1b[0m", " ".repeat(8));
    wide_frame(&mut term, &mut floor, &[], &["streaming", code.as_str()]);

    let screen = term.screen();
    assert_eq!(screen[..5], ["h4", "h5", "h6", "streaming", "code"]);
    assert_eq!(
        screen[10..],
        ["------------", "> hello", "------------", "model  think"]
    );
    // Nothing of the region as it was at the old width is left above the new one.
    assert!(screen[5..10].iter().all(String::is_empty), "{screen:#?}");
    let all = term.everything();
    assert_eq!(count(&all, "streaming"), 1, "{all:#?}");
    assert_eq!(all.iter().filter(|row| row.starts_with("---")).count(), 2);
    assert_eq!(term.parser.screen().cursor_position(), (11, 7));
}

#[test]
fn a_narrower_screen_that_cuts_rows_keeps_the_region_where_it_was() {
    let mut term = Harness::new(20, 14);
    term.renderer.set_rewraps(false);
    let mut floor = Floor::new(14);
    let above = ["h1", "h2", "h3", "h4", "h5", "h6"];
    wide_frame(&mut term, &mut floor, &above, &["streaming"]);

    // This terminal cuts what no longer fits; every row stays on its row.
    term.parser.set_size(nz(14), nz(12));
    term.settle(&mut floor);
    assert_eq!(term.renderer.rows_on_screen(), 8);
    wide_frame(&mut term, &mut floor, &[], &["streaming"]);
    let screen = term.screen();
    assert_eq!(screen[..7], ["h1", "h2", "h3", "h4", "h5", "h6", "streaming"]);
    assert_eq!(
        screen[10..],
        ["------------", "> hello", "------------", "model  think"]
    );
}

#[test]
fn a_popup_that_is_open_while_the_size_changes_is_centred_again_and_returns_to_the_transcript() {
    use azazel_tui::Theme;
    use azazel_tui::components::overlay::Frame as Boxed;
    use azazel_tui::modal::{self, Modal, Screen};

    let theme = Theme::dark();
    let mut term = Harness::new(60, 16);
    let mut floor = Floor::new(16);
    anchored(&mut term, &mut floor, &["h1", "h2", "h3"], &[], &CHROME);

    // The popup opens: the region leaves the screen with the transcript, and the box is
    // painted on the alternate screen.
    term.renderer.suspend().unwrap();
    term.flush();
    term.parser.process(b"\x1b[?1049h");
    let mut popup = Modal::new(60, 16);
    let behind = lines(&["h1", "h2", "h3"]);
    let paint = |term: &mut Harness, popup: &mut Modal| {
        let (cols, _) = popup.size();
        let body = ["one".to_string()];
        let lines = Boxed::new("Pick", modal::box_width(usize::from(cols))).render(&body, &theme);
        let mut bytes = Vec::new();
        let screen = Screen {
            backdrop: &behind,
            popup: &lines,
            cursor: None,
            working: "working",
            status: "status",
        };
        popup.render(&mut bytes, &screen, &theme).unwrap();
        term.parser.process(&bytes);
    };
    let box_at = |term: &Harness| {
        let screen = term.screen();
        let row = screen.iter().position(|row| row.contains("╭─ Pick")).unwrap();
        let column = screen[row].chars().position(|c| c == '╭').unwrap();
        (row, column)
    };
    paint(&mut term, &mut popup);
    // 14 rows above the bottom two and a box of 3 rows, 54 columns wide in 60.
    assert_eq!(box_at(&term), (5, 3));

    // The window grows, then shrinks, while the box is open.
    for (cols, rows) in [(90, 24), (50, 12)] {
        term.parser.set_size(nz(rows), nz(cols));
        popup.resize(cols, rows);
        term.settle(&mut floor);
        paint(&mut term, &mut popup);
        let screen = term.screen();
        assert_eq!(screen.len(), usize::from(rows));
        assert_eq!(
            screen.iter().filter(|row| row.contains("Pick")).count(),
            1,
            "{screen:#?}"
        );
        let width = modal::box_width(usize::from(cols));
        let top = (usize::from(rows) - 2 - 3) / 2;
        assert_eq!(box_at(&term), (top, (usize::from(cols) - width) / 2));
        assert_eq!(screen[usize::from(rows) - 2..], ["working", "status"]);
    }

    // It closes: the transcript is back as it was, what was said meanwhile follows it, and
    // the input is on the last rows of the window as it is now.
    term.parser.process(b"\x1b[?1049l");
    anchored(&mut term, &mut floor, &["said meanwhile"], &[], &CHROME);
    let screen = term.screen();
    assert_eq!(screen.len(), 12);
    assert_eq!(screen[9..], ["rule", ">", "status"]);
    let all = term.everything();
    let said: Vec<&String> = all.iter().filter(|row| !row.is_empty()).collect();
    assert_eq!(said, ["h1", "h2", "h3", "said meanwhile", "rule", ">", "status"]);
}

/// The bottom of the screen holds the input between two rules and the status line, each of
/// them one row as wide as the screen, and nothing of a rule or a status line drawn at another
/// width is left above them.
fn assert_chrome_fits(term: &Harness) {
    let (cols, rows) = term.renderer.size();
    let width = usize::from(cols);
    let screen = term.screen();
    assert_eq!(screen.len(), usize::from(rows));
    let rule = "-".repeat(width);
    let status = format!("{:<1$}think", "model", width - 5);
    let bottom = screen.len() - 4;
    assert_eq!(
        screen[bottom..],
        [rule.clone(), "> hello".to_string(), rule, status],
        "{screen:#?}"
    );
    for row in &screen[..bottom] {
        assert!(
            !row.starts_with('-') && !row.contains("think") && !row.starts_with("model"),
            "a stale row above the input: {row:?} in {screen:#?}"
        );
    }
}

/// A window that changes size and zoom in every way a user can make it, on a terminal that
/// re-wraps (Windows Terminal), with and without the application learning where the cursor is.
#[test]
fn after_any_change_of_size_the_rules_and_status_line_span_the_new_width_on_the_last_rows() {
    let above = ["h1", "h2", "a transcript row as wide as it gets!", "h4"];
    // Wider, narrower (the wide rules and status line re-wrap onto two rows each), more rows
    // and columns (zoomed out), fewer rows and columns (zoomed in), and back.
    let sizes = [(60, 12), (24, 12), (70, 20), (30, 7), (40, 12)];
    for anchored in [false, true] {
        let mut term = Harness::new(40, 12);
        let mut floor = Floor::new(12);
        wide_frame(&mut term, &mut floor, &above, &["streaming"]);
        assert_chrome_fits(&term);
        for (cols, rows) in sizes {
            term.rewrap(cols, rows);
            term.settle(&mut floor);
            if anchored {
                term.anchor(&mut floor);
            }
            wide_frame(&mut term, &mut floor, &[], &["streaming"]);
            assert_chrome_fits(&term);
            // What is in flight is on the screen once. (A re-wrap that grows the screen past its
            // height pushes the first rows of the region into the scrollback, where they stay.)
            assert_eq!(count(&term.screen(), "streaming"), 1, "{cols}x{rows}");
            let all = term.everything();
            for line in ["h1", "h2", "h4"] {
                assert!(count(&all, line) <= 1, "{line} twice at {cols}x{rows}: {all:#?}");
            }
            // An unchanged size writes nothing.
            let before = term.renderer.get_ref().len();
            term.settle(&mut floor);
            wide_frame(&mut term, &mut floor, &[], &["streaming"]);
            assert_eq!(term.renderer.get_ref().len(), before);
        }
    }
}

impl Harness {
    /// What Windows Terminal does to the main screen when it shows it again after the window
    /// changed size behind the alternate screen: it re-wraps it, then puts the cursor back on
    /// the row and column it had when the alternate screen was entered.
    fn rewrap_behind_alt(&mut self, cols: u16, rows: u16) {
        let (row, col) = self.parser.screen().cursor_position();
        self.rewrap(cols, rows);
        let row = row.min(rows - 1);
        let col = col.min(cols - 1);
        self.parser
            .process(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
    }
}

#[test]
fn a_transcript_re_wrapped_behind_a_popup_is_not_written_over_when_the_popup_closes() {
    let long = "a row of the transcript that is wider than the screen will be";
    for (cols, rows) in [(30, 14), (30, 9), (50, 14)] {
        let mut term = Harness::new(64, 14);
        let mut floor = Floor::new(14);
        let above = ["h1", long, "h3", long, "h5"];
        wide_frame(&mut term, &mut floor, &above, &[]);

        // A popup opens: the region leaves the screen; the cursor is where it began.
        term.renderer.suspend().unwrap();
        term.flush();
        let (top, _) = term.parser.screen().cursor_position();
        assert_eq!(top, 5);

        // The window changes size while the popup is up, and the popup closes.
        term.rewrap_behind_alt(cols, rows);
        term.settle(&mut floor);
        term.renderer
            .follow_rewrap(above.into_iter(), usize::from(top), 64)
            .unwrap();
        term.flush();
        term.anchor(&mut floor);
        wide_frame(&mut term, &mut floor, &["said meanwhile"], &[]);

        assert_chrome_fits(&term);
        let all = term.everything();
        let said: Vec<&String> = all
            .iter()
            .take_while(|row| !row.starts_with('-'))
            .filter(|row| !row.is_empty())
            .collect();
        let wrapped: Vec<String> = long
            .chars()
            .collect::<Vec<_>>()
            .chunks(usize::from(cols))
            .map(|chunk| chunk.iter().collect::<String>().trim_end().to_string())
            .collect();
        let mut expected = vec!["h1".to_string()];
        expected.extend(wrapped.iter().cloned());
        expected.push("h3".to_string());
        expected.extend(wrapped.iter().cloned());
        expected.extend(["h5".to_string(), "said meanwhile".to_string()]);
        let expected: Vec<&String> = expected.iter().filter(|row| !row.is_empty()).collect();
        assert_eq!(said, expected, "{cols}x{rows}");
    }
}
