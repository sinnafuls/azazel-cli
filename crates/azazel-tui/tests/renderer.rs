#![allow(clippy::unwrap_used)]

//! Drives the renderer's byte output into an emulated terminal and checks what ends up on screen.

use std::num::NonZeroU16;

use azazel_tui::render::{Floor, Frame, Renderer};

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap()
}

fn lines(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

fn frame(history: &[&str], viewport: &[&str]) -> Frame {
    Frame {
        history: lines(history),
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
    let history: Vec<String> = (1..=8).map(|i| format!("h{i}")).collect();
    term.render(Frame {
        history,
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
fn identical_frame_writes_nothing() {
    let mut term = Harness::new(40, 12);
    assert!(!term.render(frame(&[], &["a", "b"])).is_empty());
    assert!(term.render(frame(&[], &["a", "b"])).is_empty());

    let with_cursor = || Frame {
        history: Vec::new(),
        viewport: lines(&["a", "b"]),
        cursor: Some((1, 1)),
    };
    assert!(!term.render(with_cursor()).is_empty());
    assert!(term.render(with_cursor()).is_empty());

    // An empty frame on a fresh terminal is also silent.
    let mut fresh = Harness::new(40, 12);
    assert!(fresh.render(Frame::default()).is_empty());
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
fn anchored(term: &mut Harness, floor: &mut Floor, history: &[&str], top: &[&str], bottom: &[&str]) {
    let (_, height) = term.renderer.size();
    let filler = floor.pad(history.len(), top.len() + bottom.len(), height);
    let mut viewport = lines(top);
    viewport.resize(top.len() + filler, String::new());
    viewport.extend(lines(bottom));
    term.render(Frame {
        history: lines(history),
        viewport,
        cursor: None,
    });
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

    // A shorter screen caps the region; a taller one leaves it as it is.
    floor.resize(5);
    assert_eq!(floor.pad(0, 3, 5), 2);
    floor.resize(30);
    assert_eq!(floor.pad(0, 3, 30), 2);

    // Content taller than the screen needs no padding and does not raise the floor past it.
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
fn into_inner_returns_everything_written() {
    let mut renderer = Renderer::new(Vec::new(), 20, 5);
    renderer.render(frame(&["h"], &["v"])).unwrap();
    let bytes = renderer.into_inner();
    let mut parser = vt100::Parser::new(nz(5), nz(20), 0);
    parser.process(&bytes);
    assert_eq!(parser.screen().contents().trim_end(), "h\nv");
}
