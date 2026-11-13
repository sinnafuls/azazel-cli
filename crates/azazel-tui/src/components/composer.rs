//! The input as it appears on screen: an [`Editor`] between two full-width rules.
//!
//! ```text
//! ── ⠹ running shell · 12s · esc to interrupt ─────────────
//! ❯ fix the failing test
//! ──────────────────────────────────────────────────────────
//! ```

use super::editor::Editor;
use crate::glyphs::Glyph;
use crate::render::Line;
use crate::text;
use crate::theme::{Role, Theme};

/// Columns a rule keeps for itself on each side of a label.
const CAP: usize = 2;

pub struct Composer<'a> {
    /// Leads the first row of the input; later rows are indented to line up with it.
    pub prompt: &'a str,
    /// Colour of the prompt.
    pub prompt_role: Role,
    /// Shown muted while the editor is empty.
    pub placeholder: &'a str,
    /// Colour of both rules.
    pub rule: Role,
    /// Set into the top rule after its first two columns, for example the working indicator.
    /// May carry its own styling.
    pub status: Option<&'a str>,
    /// Most rows of input shown at once; the rest scrolls.
    pub max_rows: usize,
}

/// How many rows of input a screen `rows` tall shows before the input scrolls.
pub fn max_rows(rows: usize) -> usize {
    (rows * 3 / 10).max(5)
}

/// The rule above the input, the visible rows of the input and the rule below it, with the cursor
/// as `(row, column)` in those lines. Input that is scrolled out of view is counted in the rule
/// on its side (`↑ 3 more`).
pub fn render(editor: &Editor, composer: &Composer, width: usize, theme: &Theme) -> (Vec<Line>, (u16, u16)) {
    let prompt = theme.paint(composer.prompt_role, composer.prompt);
    let mut view = editor.view(width, composer.max_rows, theme, &prompt);
    if editor.is_empty() && !composer.placeholder.is_empty() {
        let room = width.saturating_sub(usize::from(view.cursor.1));
        let hint = text::truncate(composer.placeholder, room, theme.glyph(Glyph::Ellipsis));
        if let Some(first) = view.lines.first_mut() {
            first.push_str(&theme.paint(Role::Muted, &hint));
        }
    }
    let more = |arrow: &str, hidden: usize| (hidden > 0).then(|| format!("{arrow} {hidden} more"));
    let mut lines = Vec::with_capacity(view.lines.len() + 2);
    lines.push(rule(
        width,
        composer.status,
        more(theme.glyph(Glyph::MoreUp), view.above).as_deref(),
        composer.rule,
        theme,
    ));
    lines.append(&mut view.lines);
    lines.push(rule(
        width,
        None,
        more(theme.glyph(Glyph::MoreDown), view.below).as_deref(),
        composer.rule,
        theme,
    ));
    (lines, (view.cursor.0.saturating_add(1), view.cursor.1))
}

/// A full-width rule (`─`, or `-` at the `ascii` glyph level) in the colour of `role`.
///
/// `lead` is set in near the left end (`── lead ─────`) and may carry its own styling; it is cut
/// when the rule is too short. `note` is shown muted: centred when it is alone, near the right
/// end beside a lead, and left out when there is no room for it.
pub fn rule(width: usize, lead: Option<&str>, note: Option<&str>, role: Role, theme: &Theme) -> Line {
    let dash = theme.glyphs().border().horizontal;
    let dashes = |n: usize| theme.paint(role, &dash.repeat(n));
    let note = note.filter(|note| !note.is_empty());
    let lead = lead.filter(|lead| !lead.is_empty());
    // A label needs a cap and a space on both sides.
    let framed = |label: &str| text::width(label) + 2 * (CAP + 1);
    match (lead, note) {
        (Some(lead), note) if width > 2 * (CAP + 1) => {
            let note = note.filter(|note| framed(lead) + text::width(note) + 2 <= width);
            let taken = note.map_or(0, |note| text::width(note) + 2);
            let lead = text::truncate(lead, width - 2 * (CAP + 1) - taken, theme.glyph(Glyph::Ellipsis));
            let fill = width - (CAP + 1) - text::width(&lead) - 1 - taken - CAP;
            let mut line = format!("{} {lead} {}", dashes(CAP), dashes(fill));
            if let Some(note) = note {
                line.push_str(&format!(" {} ", theme.paint(Role::Muted, note)));
            }
            line.push_str(&dashes(CAP));
            line
        }
        (None, Some(note)) if framed(note) <= width => {
            let fill = width - text::width(note) - 2;
            let left = fill / 2;
            format!(
                "{} {} {}",
                dashes(left),
                theme.paint(Role::Muted, note),
                dashes(fill - left)
            )
        }
        _ => dashes(width),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::GlyphLevel;

    fn composer(status: Option<&'static str>) -> Composer<'static> {
        Composer {
            prompt: "❯ ",
            prompt_role: Role::Accent,
            placeholder: "Ask anything",
            rule: Role::Border,
            status,
            max_rows: 5,
        }
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| text::strip_ansi(l)).collect()
    }

    fn with_text(text: &str) -> Editor {
        let mut editor = Editor::new();
        editor.set_text(text);
        editor
    }

    #[test]
    fn empty_input_shows_the_placeholder_between_two_rules() {
        let (lines, cursor) = render(&Editor::new(), &composer(None), 20, &Theme::dark());
        assert_eq!(
            plain(&lines),
            ["────────────────────", "❯ Ask anything", "────────────────────"]
        );
        assert_eq!(cursor, (1, 2));
        assert!(lines[1].contains(&Theme::dark().fg(Role::Muted)));

        // The placeholder gives way as soon as something is typed, and is cut when it is too long.
        let (lines, cursor) = render(&with_text("hi"), &composer(None), 20, &Theme::dark());
        assert_eq!(plain(&lines)[1], "❯ hi");
        assert_eq!(cursor, (1, 4));
        let (lines, _) = render(&Editor::new(), &composer(None), 10, &Theme::dark());
        assert_eq!(plain(&lines)[1], "❯ Ask any…");
    }

    #[test]
    fn rules_take_the_colour_they_are_given() {
        let theme = Theme::dark();
        let mut bash = composer(None);
        bash.rule = Role::Bash;
        bash.prompt_role = Role::Bash;
        let (lines, _) = render(&with_text("!ls"), &bash, 12, &theme);
        for line in &lines {
            assert!(line.starts_with(&theme.fg(Role::Bash)), "{line:?}");
        }
    }

    #[test]
    fn status_rides_the_top_rule() {
        let (lines, _) = render(
            &Editor::new(),
            &composer(Some("working · 3s")),
            30,
            &Theme::dark(),
        );
        let shown = plain(&lines);
        assert_eq!(shown[0], "── working · 3s ──────────────");
        assert_eq!(shown[2], "─".repeat(30));

        // Too narrow for all of it: the status is cut, the rule keeps its ends.
        let (lines, _) = render(
            &Editor::new(),
            &composer(Some("working · 3s")),
            14,
            &Theme::dark(),
        );
        assert_eq!(plain(&lines)[0], "── working… ──");
    }

    #[test]
    fn tall_input_scrolls_and_the_rules_count_what_is_hidden() {
        let theme = Theme::dark();
        let mut editor = with_text("1\n2\n3\n4\n5\n6\n7\n8");
        let (lines, cursor) = render(&editor, &composer(None), 24, &theme);
        let shown = plain(&lines);
        assert_eq!(shown.len(), 7);
        assert_eq!(shown[0], "─────── ↑ 3 more ───────");
        assert_eq!(&shown[1..6], ["  4", "  5", "  6", "  7", "  8"]);
        assert_eq!(shown[6], "─".repeat(24));
        assert_eq!(cursor, (5, 3));

        // Moving up scrolls only when the cursor would leave the window.
        let up = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::NONE,
        );
        for _ in 0..5 {
            editor.handle_key(&up);
        }
        let (lines, cursor) = render(&editor, &composer(Some("busy")), 24, &theme);
        let shown = plain(&lines);
        assert_eq!(shown[0], "── busy ──── ↑ 2 more ──");
        assert_eq!(&shown[1..6], ["  3", "  4", "  5", "  6", "  7"]);
        assert_eq!(shown[6], "─────── ↓ 1 more ───────");
        assert_eq!(cursor, (1, 3));
    }

    #[test]
    fn the_ascii_level_draws_with_ascii_only() {
        let theme = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        let mut ascii = composer(Some("working | 3s"));
        ascii.prompt = "> ";
        let editor = with_text("1\n2\n3\n4\n5\n6\n7\n8");
        let (lines, _) = render(&editor, &ascii, 30, &theme);
        let shown = plain(&lines);
        assert_eq!(shown[0], "-- working | 3s -- ^ 3 more --");
        assert_eq!(shown[6], "-".repeat(30));
        for width in 0..40 {
            let (lines, _) = render(&editor, &ascii, width, &theme);
            for line in plain(&lines) {
                assert!(line.is_ascii(), "width {width}: {line:?}");
            }
            let (lines, _) = render(&Editor::new(), &ascii, width, &theme);
            assert!(plain(&lines).iter().all(|line| line.is_ascii()), "width {width}");
        }
    }

    #[test]
    fn input_height_follows_the_screen() {
        assert_eq!(max_rows(10), 5);
        assert_eq!(max_rows(24), 7);
        assert_eq!(max_rows(50), 15);
    }

    #[test]
    fn every_line_fits_any_width() {
        let theme = Theme::dark();
        let editor = with_text("some text 日本語 that wraps\nand a second line\n3\n4\n5\n6\n7");
        for width in 0..40 {
            let (lines, (row, _)) = render(&editor, &composer(Some("working · 12s")), width, &theme);
            for line in &lines {
                assert!(text::width(line) <= width.max(1), "width {width}: {line:?}");
            }
            assert!(usize::from(row) < lines.len());
            if width > 0 {
                assert_eq!(text::width(&lines[0]), width, "the top rule spans the width");
            }
        }
    }
}
