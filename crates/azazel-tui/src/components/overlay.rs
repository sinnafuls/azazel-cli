//! The box every picker, prompt and approval is drawn in: a rounded border with the title set
//! into its top edge and the key hints into its bottom edge, filled with the surface colour.

use crate::glyphs::Glyph;
use crate::render::Line;
use crate::text;
use crate::theme::{RESET, Role, Theme};

/// Narrowest width at which a frame with one column of content fits.
const MIN_FRAMED_WIDTH: usize = 5;

/// One key and what it does, for the bottom edge of a frame: `enter pick`. A hint without a key
/// is a remark (`hidden while you type`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub key: String,
    pub action: String,
}

impl Hint {
    pub fn new(key: impl Into<String>, action: impl Into<String>) -> Hint {
        Hint {
            key: key.into(),
            action: action.into(),
        }
    }
}

/// Which part of a list is in view: `visible` rows starting at `offset`, out of `total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scroll {
    pub offset: usize,
    pub visible: usize,
    pub total: usize,
}

impl Scroll {
    /// The rows of the thumb as `(first, length)` within the `visible` rows of the bar.
    pub fn thumb(self) -> (usize, usize) {
        let rows = self.visible;
        if rows == 0 || self.total <= rows {
            return (0, rows);
        }
        let length = (rows * rows / self.total).clamp(1, rows);
        let travel = rows - length;
        let hidden = self.total - rows;
        // Rounded, so the thumb reaches the last row exactly when the list is at its end.
        let first = (self.offset.min(hidden) * travel + hidden / 2) / hidden;
        (first, length)
    }

    /// The bar, one cell per visible row: the track in the quiet border colour, the thumb in
    /// the accent.
    pub fn cells(self, theme: &Theme) -> Vec<String> {
        let (first, length) = self.thumb();
        (0..self.visible)
            .map(|row| {
                if row >= first && row < first + length {
                    format!("{}{}", theme.fg(Role::Accent), theme.glyph(Glyph::ScrollThumb))
                } else {
                    format!(
                        "{}{}",
                        theme.fg(Role::BorderMuted),
                        theme.glyph(Glyph::ScrollTrack)
                    )
                }
            })
            .collect()
    }
}

/// A box around some rows.
#[derive(Debug, Clone)]
pub struct Frame<'a> {
    /// Set into the top edge; cut when the box is too narrow for it.
    pub title: &'a str,
    /// Set into the bottom edge, as many as fit.
    pub hints: &'a [Hint],
    /// The whole box, borders included.
    pub width: usize,
    /// A scrollbar inside the right edge, beside the body rows starting at the given index.
    pub scrollbar: Option<(usize, Scroll)>,
    /// The colour of the border: [`Role::Border`], or [`Role::Warning`] for something that
    /// needs care.
    pub border: Role,
}

impl<'a> Frame<'a> {
    pub fn new(title: &'a str, width: usize) -> Frame<'a> {
        Frame {
            title,
            hints: &[],
            width,
            scrollbar: None,
            border: Role::Border,
        }
    }

    /// Columns a body row may fill in a frame `width` columns wide.
    pub fn inner_width(width: usize) -> usize {
        if width < MIN_FRAMED_WIDTH {
            width
        } else {
            width - 4
        }
    }

    /// Where the first body row starts within the rendered lines, as `(row, column)`: what a
    /// cursor position inside the body is moved by.
    pub fn origin(&self) -> (usize, usize) {
        if self.width < MIN_FRAMED_WIDTH {
            (usize::from(!self.title.is_empty()), 0)
        } else {
            (1, 2)
        }
    }

    /// Rows the frame adds to its body.
    pub fn chrome_rows(&self) -> usize {
        if self.width < MIN_FRAMED_WIDTH {
            usize::from(!self.title.is_empty())
        } else {
            2
        }
    }

    /// Draws `body` inside the box. Every line is exactly `width` columns wide; body lines that
    /// are too long are cut. Below five columns there is no room for a frame and the title and
    /// body are returned bare.
    pub fn render(&self, body: &[Line], theme: &Theme) -> Vec<Line> {
        let width = self.width;
        if width < MIN_FRAMED_WIDTH {
            let title = (!self.title.is_empty()).then_some(self.title);
            return title
                .into_iter()
                .chain(body.iter().map(String::as_str))
                .map(|l| text::truncate(l, width, ""))
                .collect();
        }
        let inner = width - 4;
        let chars = theme.glyphs().border();
        let surface = theme.bg(Role::Surface);
        let edge = theme.fg(self.border);
        let ellipsis = theme.glyph(Glyph::Ellipsis);
        let dashes = |n: usize| chars.horizontal.repeat(n);
        let mut out = Vec::with_capacity(body.len() + 2);

        // Top edge: corner, one dash, the title between spaces, dashes to the other corner.
        let title = text::truncate(self.title, width - MIN_FRAMED_WIDTH, ellipsis);
        let title = text::strip_ansi(&title);
        if title.is_empty() {
            out.push(format!(
                "{surface}{edge}{}{}{}{RESET}",
                chars.top_left,
                dashes(width - 2),
                chars.top_right
            ));
        } else {
            let fill = width - MIN_FRAMED_WIDTH - text::width(&title);
            out.push(format!(
                "{surface}{edge}{}{} {}{title}{edge} {}{}{RESET}",
                chars.top_left,
                chars.horizontal,
                theme.fg(Role::Accent),
                dashes(fill),
                chars.top_right,
            ));
        }

        let bar = self.scrollbar.map(|(first, scroll)| (first, scroll.cells(theme)));
        for (row, line) in body.iter().enumerate() {
            let cut = text::truncate(line, inner, ellipsis);
            // A reset inside the row would end the fill; the fill starts again after each.
            let content = cut.replace(RESET, &format!("{RESET}{surface}"));
            let pad = " ".repeat(inner - text::width(&cut));
            let cell = bar
                .as_ref()
                .and_then(|(first, cells)| cells.get(row.checked_sub(*first)?))
                .map_or(" ", String::as_str);
            out.push(format!(
                "{surface}{edge}{v}{text_color} {content}{pad}{cell}{edge}{v}{RESET}",
                v = chars.vertical,
                text_color = theme.fg(Role::Text),
            ));
        }

        // Bottom edge: the hints that fit, whole, then dashes.
        let hints = self.hints(width - MIN_FRAMED_WIDTH, theme);
        if hints.is_empty() {
            out.push(format!(
                "{surface}{edge}{}{}{}{RESET}",
                chars.bottom_left,
                dashes(width - 2),
                chars.bottom_right
            ));
        } else {
            let fill = width - MIN_FRAMED_WIDTH - text::width(&hints);
            out.push(format!(
                "{surface}{edge}{}{} {hints}{edge} {}{}{RESET}",
                chars.bottom_left,
                chars.horizontal,
                dashes(fill),
                chars.bottom_right,
            ));
        }
        out
    }

    /// The hints as one run of at most `room` columns: keys in the text colour, what they do
    /// muted. A hint that does not fit whole is left out, and so are those after it.
    fn hints(&self, room: usize, theme: &Theme) -> String {
        let separator = theme.glyph(Glyph::Separator);
        let mut out = String::new();
        let mut used = 0;
        for hint in self.hints {
            let gap = usize::from(!hint.key.is_empty() && !hint.action.is_empty());
            let needed = text::width(&hint.key) + gap + text::width(&hint.action);
            let joint = if used == 0 { 0 } else { text::width(separator) };
            if needed == 0 {
                continue;
            }
            if used + joint + needed > room {
                break;
            }
            if used > 0 {
                out.push_str(&theme.fg(Role::Muted));
                out.push_str(separator);
            }
            out.push_str(&theme.fg(Role::Text));
            out.push_str(&hint.key);
            if gap == 1 {
                out.push(' ');
            }
            out.push_str(&theme.fg(Role::Muted));
            out.push_str(&hint.action);
            used += joint + needed;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::GlyphLevel;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| text::strip_ansi(l)).collect()
    }

    fn hints() -> Vec<Hint> {
        vec![
            Hint::new("↑↓", "move"),
            Hint::new("enter", "pick"),
            Hint::new("esc", "cancel"),
        ]
    }

    #[test]
    fn draws_a_box_with_the_title_above_and_the_hints_below() {
        let body = ["first".to_string(), "\x1b[1msecond line\x1b[0m".to_string()];
        let hints = hints();
        let frame = Frame {
            hints: &hints,
            ..Frame::new("Pick", 44)
        };
        let lines = frame.render(&body, &Theme::dark());
        assert_eq!(
            plain(&lines),
            [
                "╭─ Pick ───────────────────────────────────╮",
                "│ first                                    │",
                "│ second line                              │",
                "╰─ ↑↓ move · enter pick · esc cancel ──────╯",
            ]
        );
        assert_eq!(frame.origin(), (1, 2));
        assert_eq!(frame.chrome_rows(), 2);
    }

    #[test]
    fn every_row_is_as_wide_as_the_frame_and_filled_with_the_surface() {
        let theme = Theme::dark();
        let surface = theme.bg(Role::Surface);
        let body = [
            "plain".to_string(),
            theme.paint(Role::Warning, "a warning that is reset in the middle") + " and goes on",
            "a body line that is far too long to fit into a narrow frame, so it is cut".to_string(),
            String::new(),
        ];
        let hints = hints();
        for width in 5..=160 {
            let frame = Frame {
                hints: &hints,
                ..Frame::new("A title that is cut in a narrow frame", width)
            };
            let lines = frame.render(&body, &theme);
            assert_eq!(lines.len(), body.len() + 2);
            for line in &lines {
                assert_eq!(text::width(line), width, "width {width}: {line:?}");
                assert!(line.starts_with(&surface), "width {width}: {line:?}");
                assert!(line.ends_with(RESET), "width {width}: {line:?}");
                // No reset leaves the rest of the row without the fill.
                let body = line.strip_suffix(RESET).unwrap();
                for (at, _) in body.match_indices(RESET) {
                    assert!(body[at + RESET.len()..].starts_with(&surface), "{line:?}");
                }
            }
        }
    }

    #[test]
    fn cuts_long_titles_and_body_lines() {
        let body = ["a body line that does not fit".to_string()];
        let lines = Frame::new("A very long overlay title", 16).render(&body, &Theme::dark());
        assert_eq!(
            plain(&lines),
            ["╭─ A very lon… ╮", "│ a body line… │", "╰──────────────╯"]
        );
    }

    #[test]
    fn hints_that_do_not_fit_are_left_out_whole() {
        let hints = hints();
        let frame = |width| Frame {
            hints: &hints,
            ..Frame::new("", width)
        };
        let theme = Theme::dark();
        let bottom = |width| plain(&frame(width).render(&[], &theme))[1].clone();
        assert_eq!(bottom(40), "╰─ ↑↓ move · enter pick · esc cancel ──╯");
        assert_eq!(bottom(39), "╰─ ↑↓ move · enter pick · esc cancel ─╯");
        assert_eq!(bottom(38), "╰─ ↑↓ move · enter pick · esc cancel ╯");
        assert_eq!(bottom(37), "╰─ ↑↓ move · enter pick ────────────╯");
        assert_eq!(bottom(24), "╰─ ↑↓ move ────────────╯");
        assert_eq!(bottom(12), "╰─ ↑↓ move ╯");
        assert_eq!(bottom(11), "╰─────────╯");
    }

    #[test]
    fn untitled_box_has_a_plain_top_border() {
        let lines = Frame::new("", 7).render(&["x".to_string()], &Theme::dark());
        assert_eq!(plain(&lines), ["╭─────╮", "│ x   │", "╰─────╯"]);
    }

    #[test]
    fn the_border_takes_the_colour_it_is_given() {
        let theme = Theme::dark();
        let calm = Frame::new("Run", 20).render(&["x".to_string()], &theme);
        let careful = Frame {
            border: Role::Warning,
            ..Frame::new("Run", 20)
        }
        .render(&["x".to_string()], &theme);
        assert_eq!(plain(&calm), plain(&careful));
        for (calm, careful) in calm.iter().zip(&careful) {
            assert!(calm.contains(&theme.fg(Role::Border)));
            assert!(!calm.contains(&theme.fg(Role::Warning)));
            assert!(careful.contains(&theme.fg(Role::Warning)));
            assert!(!careful.contains(&theme.fg(Role::Border)));
        }
    }

    #[test]
    fn ascii_frames_are_pure_ascii() {
        let theme = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        let hints = [Hint::new("^v", "move"), Hint::new("esc", "cancel")];
        let frame = Frame {
            hints: &hints,
            scrollbar: Some((
                0,
                Scroll {
                    offset: 0,
                    visible: 2,
                    total: 9,
                },
            )),
            ..Frame::new("A long title that is cut", 25)
        };
        let body = ["a row that is much too long for it".to_string(), "b".to_string()];
        let lines = plain(&frame.render(&body, &theme));
        assert_eq!(
            lines,
            [
                "+- A long title that... +",
                "| a row that is much...#|",
                "| b                    ||",
                "+- ^v move | esc cancel +",
            ]
        );
        assert!(lines.iter().all(|line| line.is_ascii()));
    }

    #[test]
    fn the_scrollbar_stands_inside_the_right_edge_beside_its_rows() {
        let body: Vec<Line> = ["about", "", "one", "two", "three", "four"]
            .iter()
            .map(|row| row.to_string())
            .collect();
        let scroll = Scroll {
            offset: 4,
            visible: 4,
            total: 8,
        };
        let frame = Frame {
            scrollbar: Some((2, scroll)),
            ..Frame::new("", 12)
        };
        assert_eq!(
            plain(&frame.render(&body, &Theme::dark())),
            [
                "╭──────────╮",
                "│ about    │",
                "│          │",
                "│ one     ││",
                "│ two     ││",
                "│ three   ┃│",
                "│ four    ┃│",
                "╰──────────╯",
            ]
        );
    }

    #[test]
    fn the_thumb_covers_its_share_and_reaches_both_ends() {
        let thumb = |offset, visible, total| {
            Scroll {
                offset,
                visible,
                total,
            }
            .thumb()
        };
        // Everything in view: the whole bar.
        assert_eq!(thumb(0, 5, 5), (0, 5));
        assert_eq!(thumb(0, 10, 100), (0, 1));
        assert_eq!(thumb(45, 10, 100), (5, 1));
        assert_eq!(thumb(90, 10, 100), (9, 1));
        assert_eq!(thumb(0, 10, 20), (0, 5));
        assert_eq!(thumb(10, 10, 20), (5, 5));
        for total in 1..60 {
            for visible in 1..=total.min(20) {
                for offset in 0..=total - visible {
                    let (first, length) = thumb(offset, visible, total);
                    assert!(length >= 1 && first + length <= visible);
                    if offset == 0 {
                        assert_eq!(first, 0);
                    }
                    if offset == total - visible {
                        assert_eq!(first + length, visible);
                    }
                }
            }
        }
    }

    #[test]
    fn falls_back_to_bare_lines_when_too_narrow() {
        let frame = Frame::new("Title", 3);
        let lines = frame.render(&["body".to_string()], &Theme::dark());
        assert_eq!(lines, ["Tit", "bod"]);
        assert_eq!(frame.origin(), (1, 0));
        assert_eq!(frame.chrome_rows(), 1);
        assert_eq!(Frame::inner_width(3), 3);
        assert_eq!(Frame::inner_width(44), 40);
    }
}
