//! Drives the popup painter's byte output into an emulated terminal and checks what ends up on
//! screen, and how much was written to get it there.

use std::num::NonZeroU16;

use azazel_tui::components::overlay::Frame;
use azazel_tui::modal::{self, Modal, Screen};
use azazel_tui::{Role, Theme, text};

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap()
}

struct Harness {
    modal: Modal,
    parser: vt100::Parser,
    theme: Theme,
}

impl Harness {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            modal: Modal::new(cols, rows),
            parser: vt100::Parser::new(nz(rows), nz(cols), 0),
            theme: Theme::dark(),
        }
    }

    /// Paints a frame into the terminal and returns the bytes it took.
    fn render(&mut self, screen: &Screen<'_>) -> Vec<u8> {
        let mut bytes = Vec::new();
        self.modal.render(&mut bytes, screen, &self.theme).unwrap();
        self.parser.process(&bytes);
        bytes
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.set_size(nz(rows), nz(cols));
        self.modal.resize(cols, rows);
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

    fn foreground(&self, row: u16, col: u16) -> vt100::Color {
        self.parser.screen().cell(row, col).unwrap().fgcolor()
    }
}

fn backdrop(rows: usize) -> Vec<String> {
    (0..rows)
        .map(|row| format!("transcript row {row:02} {}", "x".repeat(70)))
        .collect()
}

fn boxed(title: &str, body: &[&str], width: usize, theme: &Theme) -> Vec<String> {
    let body: Vec<String> = body.iter().map(|row| row.to_string()).collect();
    Frame::new(title, width).render(&body, theme)
}

/// How many rows a frame addressed: each starts with a cursor position at column 1.
fn rows_written(bytes: &[u8]) -> usize {
    String::from_utf8_lossy(bytes).matches(";1H\x1b[2K").count()
}

#[test]
fn the_box_is_centred_over_the_dimmed_backdrop_above_the_bottom_rows() {
    let mut harness = Harness::new(80, 24);
    let behind = backdrop(30);
    let popup = boxed("Pick", &["one", "two"], modal::box_width(80), &harness.theme);
    assert_eq!(popup.len(), 4);
    harness.render(&Screen {
        backdrop: &behind,
        popup: &popup,
        cursor: None,
        working: "-- working --",
        status: "status line",
    });
    let screen = harness.screen();
    assert_eq!(screen.len(), 24);
    // 22 rows above the bottom two: the 4 rows of the box start on row 9. The box is 72 wide
    // in 80 columns: 4 columns of the backdrop on each side.
    for (row, line) in screen.iter().enumerate().take(22) {
        let behind = format!("transcript row {row:02} {}", "x".repeat(70));
        if (9..13).contains(&row) {
            let shown: String = line.chars().collect();
            assert!(shown.starts_with("tran"), "{row}: {line}");
            assert!(shown.ends_with("xxxx"), "{row}: {line}");
            assert_eq!(shown.chars().count(), 80, "{row}: {line}");
        } else {
            assert_eq!(*line, behind[..80], "{row}");
        }
    }
    assert_eq!(screen[9][4..].chars().take(8).collect::<String>(), "╭─ Pick ");
    assert!(screen[10].contains("│ one"), "{}", screen[10]);
    assert!(screen[11].contains("│ two"), "{}", screen[11]);
    assert!(screen[12].contains('╰'), "{}", screen[12]);
    assert_eq!(screen[22], "-- working --");
    assert_eq!(screen[23], "status line");

    // The backdrop is dimmed, on both sides of the box too; the box is not.
    let dim = match harness.theme.color(Role::Backdrop) {
        azazel_tui::Color::Rgb(r, g, b) => vt100::Color::Rgb(r, g, b),
        other => panic!("{other:?}"),
    };
    assert_eq!(harness.foreground(0, 0), dim);
    assert_eq!(harness.foreground(10, 1), dim);
    assert_eq!(harness.foreground(10, 78), dim);
    assert_ne!(harness.foreground(10, 7), dim);
    assert_eq!(harness.foreground(21, 40), dim);
    assert!(harness.parser.screen().hide_cursor());
}

#[test]
fn a_short_backdrop_leaves_the_rest_of_the_window_blank() {
    let mut harness = Harness::new(60, 16);
    let behind = vec!["only row".to_string()];
    let popup = boxed("", &["x"], 44, &harness.theme);
    harness.render(&Screen {
        backdrop: &behind,
        popup: &popup,
        cursor: None,
        working: "",
        status: "s",
    });
    let screen = harness.screen();
    assert_eq!(screen[0], "only row");
    assert_eq!(screen[1], "");
    // 14 rows above the bottom two, 3 rows of box: rows 5 to 7, 8 columns in.
    assert_eq!(screen[5], format!("{}╭{}╮", " ".repeat(8), "─".repeat(42)));
    assert_eq!(screen[7], format!("{}╰{}╯", " ".repeat(8), "─".repeat(42)));
    assert_eq!(screen[15], "s");
}

#[test]
fn only_the_rows_that_changed_are_written() {
    let mut harness = Harness::new(80, 24);
    let behind = backdrop(22);
    let theme = harness.theme.clone();
    let frame = |selected: &str| boxed("Pick", &["one", "two", selected], 72, &theme);
    let first = harness.render(&Screen {
        backdrop: &behind,
        popup: &frame("three"),
        cursor: None,
        working: "w",
        status: "s",
    });
    assert_eq!(rows_written(&first), 24);
    assert_eq!(
        String::from_utf8_lossy(&first).matches("\x1b[?2026h").count(),
        1,
        "one synchronized update"
    );

    // The same frame again: nothing at all.
    let same = harness.render(&Screen {
        backdrop: &behind,
        popup: &frame("three"),
        cursor: None,
        working: "w",
        status: "s",
    });
    assert!(same.is_empty(), "{:?}", String::from_utf8_lossy(&same));

    // One row of the box and the working row change: two rows are addressed.
    let changed = harness.render(&Screen {
        backdrop: &behind,
        popup: &frame("THREE"),
        cursor: None,
        working: "working",
        status: "s",
    });
    assert_eq!(
        rows_written(&changed),
        2,
        "{:?}",
        String::from_utf8_lossy(&changed)
    );
    assert!(changed.len() < 400, "{} bytes", changed.len());
    let screen = harness.screen();
    assert!(screen[11].contains("│ THREE"), "{}", screen[11]);
    assert_eq!(screen[22], "working");
    assert!(screen[9].contains("│ one"));

    // A new row of transcript moves every backdrop row. The five rows the box is on show the
    // same four columns of it on either side as before, so they are left alone.
    let mut scrolled = behind[1..].to_vec();
    scrolled.push("the newest row".to_string());
    let moved = harness.render(&Screen {
        backdrop: &scrolled,
        popup: &frame("THREE"),
        cursor: None,
        working: "working",
        status: "s",
    });
    assert_eq!(rows_written(&moved), 17);
    assert_eq!(harness.screen()[21], "the newest row");
}

#[test]
fn a_resize_repaints_everything_once_and_centres_the_box_again() {
    let mut harness = Harness::new(100, 30);
    let behind = backdrop(40);
    let theme = harness.theme.clone();
    let popup = boxed("Pick", &["one"], modal::box_width(100), &theme);
    harness.render(&Screen {
        backdrop: &behind,
        popup: &popup,
        cursor: None,
        working: "w",
        status: "s",
    });
    assert!(harness.screen()[12].contains("╭─ Pick"));

    harness.resize(60, 14);
    let popup = boxed("Pick", &["one"], modal::box_width(60), &theme);
    let screen_of = |harness: &mut Harness| {
        harness.render(&Screen {
            backdrop: &behind,
            popup: &popup,
            cursor: None,
            working: "w",
            status: "s",
        })
    };
    let repaint = screen_of(&mut harness);
    assert_eq!(rows_written(&repaint), 14);
    let screen = harness.screen();
    assert_eq!(screen.len(), 14);
    // 12 rows above the bottom two, 3 rows of box: rows 4 to 6; 54 columns wide, 3 in.
    assert_eq!(
        screen.iter().filter(|row| row.contains("Pick")).count(),
        1,
        "{screen:#?}"
    );
    assert!(screen[4][3..].starts_with("╭─ Pick"), "{}", screen[4]);
    assert!(screen[6].contains('╰'));
    assert_eq!(screen[3], behind[3][..60]);
    assert_eq!(screen[7], behind[7][..60]);
    assert_eq!(screen[12], "w");
    assert_eq!(screen[13], "s");
    for row in &screen {
        assert!(text::width(row) <= 60);
    }
    // Nothing is left to do after the repaint.
    assert!(screen_of(&mut harness).is_empty());

    // The alternate screen was written to by something else: everything is painted again.
    harness.modal.invalidate();
    assert_eq!(rows_written(&screen_of(&mut harness)), 14);
}

#[test]
fn the_cursor_goes_where_the_box_puts_it_and_hides_otherwise() {
    let mut harness = Harness::new(80, 24);
    let popup = boxed("Why?", &["", "> typed"], 72, &harness.theme);
    let mut screen = Screen {
        backdrop: &[],
        popup: &popup,
        cursor: Some((2, 9)),
        working: "",
        status: "",
    };
    harness.render(&screen);
    assert!(!harness.parser.screen().hide_cursor());
    // The box starts on row 9, 4 columns in.
    assert_eq!(harness.parser.screen().cursor_position(), (11, 13));

    // Moving the cursor alone writes no row.
    screen.cursor = Some((2, 10));
    let moved = harness.render(&screen);
    assert_eq!(rows_written(&moved), 0);
    assert_eq!(harness.parser.screen().cursor_position(), (11, 14));

    screen.cursor = None;
    harness.render(&screen);
    assert!(harness.parser.screen().hide_cursor());
}

#[test]
fn a_window_smaller_than_the_box_cuts_it_instead_of_wrapping() {
    let mut harness = Harness::new(20, 5);
    let popup = boxed("A title", &["one", "two", "three", "four"], 44, &harness.theme);
    let behind = backdrop(10);
    harness.render(&Screen {
        backdrop: &behind,
        popup: &popup,
        cursor: Some((40, 40)),
        working: "w",
        status: "s",
    });
    let screen = harness.screen();
    assert_eq!(screen.len(), 5);
    assert!(screen[0].starts_with("╭─ A title"), "{screen:#?}");
    assert!(screen[2].starts_with("│ two"), "{screen:#?}");
    assert_eq!(screen[3], "w");
    assert_eq!(screen[4], "s");
    // A cursor outside what is shown of the box is hidden.
    assert!(harness.parser.screen().hide_cursor());
}

#[test]
fn wide_characters_behind_the_edges_of_the_box_do_not_shift_it() {
    let mut harness = Harness::new(60, 14);
    let behind: Vec<String> = (0..12).map(|_| "日本語".repeat(10)).collect();
    let popup = boxed("", &["x"], 45, &harness.theme);
    harness.render(&Screen {
        backdrop: &behind,
        popup: &popup,
        cursor: None,
        working: "",
        status: "",
    });
    // The box is 45 wide in 60 columns: it starts 7 columns in, in the middle of a wide
    // character, and ends at column 52.
    let screen = harness.parser.screen();
    assert_eq!(screen.cell(4, 7).unwrap().contents(), "╭");
    assert_eq!(screen.cell(4, 51).unwrap().contents(), "╮");
    assert_eq!(screen.cell(4, 4).unwrap().contents(), "語");
    assert_eq!(screen.cell(4, 6).unwrap().contents(), " ");
    assert_eq!(screen.cell(4, 52).unwrap().contents(), "語");
}
