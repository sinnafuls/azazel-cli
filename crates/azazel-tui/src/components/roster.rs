//! Agent roster: one line per agent that has not finished. With a row selected it is a list
//! to move in: the fleet view.

use std::time::Duration;

use super::{format_elapsed, format_tokens, one_line};
use crate::glyphs::Glyph;
use crate::render::Line;
use crate::text;
use crate::theme::{RESET, Role, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterState {
    Queued,
    Running,
    /// Blocked on the user, for example an approval prompt.
    Waiting,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterRow {
    pub id: String,
    /// The definition the agent was started from; left out when empty.
    pub def: String,
    pub activity: String,
    pub elapsed: Duration,
    pub tokens: u64,
    pub state: RosterState,
}

/// One line per row: `● <id>  <def>  <activity>  <elapsed>  <tokens>` (`*` for the bullet at the
/// `ascii` glyph level). The bullet is coloured by state and the activity is shortened first
/// when the line is too long.
///
/// With `selected` the roster has the keyboard: every row gives up two columns to a rail, and
/// the selected one has the rail drawn, the selection background across the width and its id
/// in bold, as the selected row of any list has.
pub fn render(rows: &[RosterRow], selected: Option<usize>, width: usize, theme: &Theme) -> Vec<Line> {
    rows.iter()
        .enumerate()
        .map(|(at, row)| match selected {
            None => render_row(row, width, theme),
            Some(chosen) => focused_row(row, at == chosen, width, theme),
        })
        .collect()
}

/// The first `limit` rows as [`render`] draws them without a selection, and under them a muted
/// `… N more` for the rows left out.
pub fn collapsed(rows: &[RosterRow], limit: usize, width: usize, theme: &Theme) -> Vec<Line> {
    let shown = rows.len().min(limit);
    let mut lines = render(&rows[..shown], None, width, theme);
    if rows.len() > shown {
        let more = format!("{} {} more", theme.glyph(Glyph::Ellipsis), rows.len() - shown);
        lines.push(text::truncate(
            &theme.paint(Role::Muted, &more),
            width,
            theme.glyph(Glyph::Ellipsis),
        ));
    }
    lines
}

/// Columns the rail of a roster that has the keyboard takes.
const RAIL: usize = 2;

fn focused_row(row: &RosterRow, selected: bool, width: usize, theme: &Theme) -> Line {
    if width <= RAIL {
        return " ".repeat(width);
    }
    let line = render_row(row, width - RAIL, theme);
    if !selected {
        return format!("{}{line}", " ".repeat(RAIL));
    }
    let background = theme.bg(Role::SelectedBg);
    // The fill goes on after each reset inside the row.
    let content = line.replace(RESET, &format!("{RESET}{background}"));
    let pad = " ".repeat(width - RAIL - text::width(&line));
    format!(
        "{background}{}{} {RESET}{background}{content}{pad}{RESET}",
        theme.fg(Role::Accent),
        theme.glyph(Glyph::Rail),
    )
}

fn render_row(row: &RosterRow, width: usize, theme: &Theme) -> Line {
    let role = match row.state {
        RosterState::Queued => Role::Border,
        RosterState::Running => Role::Success,
        RosterState::Waiting => Role::Warning,
        RosterState::Idle => Role::Muted,
    };
    let id = one_line(&row.id);
    let def = one_line(&row.def);
    let elapsed = format_elapsed(row.elapsed);
    let tokens = format_tokens(row.tokens);
    let def_width = if def.is_empty() { 0 } else { text::width(&def) + 2 };

    // Everything except the activity: bullet, id, definition and the two right-hand columns with
    // separators.
    let fixed = 2 + text::width(&id) + def_width + 2 + 2 + elapsed.len() + 2 + tokens.len();
    let ellipsis = theme.glyph(Glyph::Ellipsis);
    let activity = text::truncate(&one_line(&row.activity), width.saturating_sub(fixed), ellipsis);
    let def = if def.is_empty() {
        String::new()
    } else {
        format!("  {}", theme.paint(Role::Muted, &def))
    };

    let line = format!(
        "{} {id}{def}  {activity}  {}  {}",
        theme.paint(role, theme.glyph(Glyph::Bullet)),
        theme.paint(Role::Muted, &elapsed),
        theme.paint(Role::Muted, &tokens),
    );
    text::truncate(&line, width, ellipsis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Color;

    fn row(id: &str, activity: &str, secs: u64, tokens: u64, state: RosterState) -> RosterRow {
        RosterRow {
            id: id.to_string(),
            def: String::new(),
            activity: activity.to_string(),
            elapsed: Duration::from_secs(secs),
            tokens,
            state,
        }
    }

    #[test]
    fn formats_one_line_per_row() {
        let rows = [
            row("scout-1", "grep TODO", 42, 1234, RosterState::Running),
            row(
                "coder-2",
                "waiting for approval",
                185,
                15_700,
                RosterState::Waiting,
            ),
        ];
        let lines = render(&rows, None, 80, &Theme::dark());
        let plain: Vec<String> = lines.iter().map(|l| text::strip_ansi(l)).collect();
        assert_eq!(
            plain,
            [
                "● scout-1  grep TODO  42s  1.2k",
                "● coder-2  waiting for approval  3m05s  15.7k"
            ]
        );
    }

    #[test]
    fn bullet_colour_follows_state() {
        let mut theme = Theme::dark();
        theme.set_color(Role::Border, Color::Indexed(240));
        theme.set_color(Role::Success, Color::Indexed(2));
        theme.set_color(Role::Warning, Color::Indexed(3));
        theme.set_color(Role::Muted, Color::Indexed(8));
        let bullet = |state| {
            let lines = render(&[row("a", "b", 1, 1, state)], None, 40, &theme);
            lines[0].split('●').next().map(str::to_string)
        };
        assert_eq!(bullet(RosterState::Queued).as_deref(), Some("\x1b[38;5;240m"));
        assert_eq!(bullet(RosterState::Running).as_deref(), Some("\x1b[32m"));
        assert_eq!(bullet(RosterState::Waiting).as_deref(), Some("\x1b[33m"));
        assert_eq!(bullet(RosterState::Idle).as_deref(), Some("\x1b[90m"));
    }

    #[test]
    fn shortens_the_activity_before_the_counters() {
        let rows = [row(
            "scout-1",
            "reading a very long list of files in the repository",
            42,
            1234,
            RosterState::Running,
        )];
        let lines = render(&rows, None, 32, &Theme::dark());
        let plain = text::strip_ansi(&lines[0]);
        assert_eq!(text::width(&lines[0]), 32);
        assert!(plain.starts_with("● scout-1  reading a"), "{plain:?}");
        assert!(plain.ends_with("…  42s  1.2k"), "{plain:?}");
    }

    #[test]
    fn never_exceeds_the_width() {
        let rows = [row(
            "an-agent-with-a-long-id",
            "activity",
            4000,
            2_500_000,
            RosterState::Idle,
        )];
        for width in 0..50 {
            for line in render(&rows, None, width, &Theme::dark()) {
                assert!(text::width(&line) <= width, "width {width}: {line:?}");
            }
        }
    }

    #[test]
    fn a_roster_with_the_keyboard_marks_the_selected_row_and_keeps_the_columns() {
        let rows = [
            row("scout-1", "grep TODO", 42, 1234, RosterState::Running),
            row(
                "coder-2",
                "editing src/main.rs",
                185,
                15_700,
                RosterState::Waiting,
            ),
        ];
        let mut theme = Theme::dark();
        theme.set_color(Role::SelectedBg, Color::Indexed(237));
        let lines = render(&rows, Some(1), 60, &theme);
        let plain: Vec<String> = lines.iter().map(|l| text::strip_ansi(l)).collect();
        assert_eq!(plain[0], "  ● scout-1  grep TODO  42s  1.2k");
        assert!(
            plain[1].starts_with("▌ ● coder-2  editing src/main.rs  3m05s  15.7k"),
            "{:?}",
            plain[1]
        );
        // The selected row is filled across the width, and only that one.
        assert_eq!(text::width(&lines[1]), 60);
        assert!(lines[1].starts_with("\x1b[48;5;237m"), "{:?}", lines[1]);
        assert!(!lines[0].contains("48;5;237"), "{:?}", lines[0]);
        // Every reset inside the row is followed by the fill again.
        let resets = lines[1].matches(RESET).count();
        assert_eq!(lines[1].matches("\x1b[0m\x1b[48;5;237m").count(), resets - 1);

        for width in 0..50 {
            for selected in [0, 1] {
                for line in render(&rows, Some(selected), width, &theme) {
                    assert!(text::width(&line) <= width, "width {width}: {line:?}");
                }
            }
        }
        let ascii = theme.with_glyphs(crate::glyphs::GlyphLevel::Ascii);
        for line in render(&rows, Some(0), 40, &ascii) {
            assert!(text::strip_ansi(&line).is_ascii(), "{line:?}");
        }
    }

    #[test]
    fn the_ascii_level_draws_with_ascii_only() {
        let theme = Theme::dark().with_glyphs(crate::glyphs::GlyphLevel::Ascii);
        let rows = [row(
            "scout-1",
            "reading a very long list of files in the repository",
            42,
            1234,
            RosterState::Running,
        )];
        let wide = text::strip_ansi(&render(&rows, None, 80, &theme)[0]);
        assert!(wide.starts_with("* scout-1  reading"), "{wide:?}");
        for width in 0..50 {
            let line = text::strip_ansi(&render(&rows, None, width, &theme)[0]);
            assert!(line.is_ascii(), "width {width}: {line:?}");
            assert!(text::width(&line) <= width, "width {width}: {line:?}");
        }
        assert!(text::strip_ansi(&render(&rows, None, 32, &theme)[0]).ends_with("...  42s  1.2k"));
    }
}
