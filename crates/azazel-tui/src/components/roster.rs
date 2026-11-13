//! Agent roster: one line per agent that has not finished.

use std::time::Duration;

use super::{format_elapsed, format_tokens, one_line};
use crate::glyphs::Glyph;
use crate::render::Line;
use crate::text;
use crate::theme::{Role, Theme};

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
    pub activity: String,
    pub elapsed: Duration,
    pub tokens: u64,
    pub state: RosterState,
}

/// One line per row: `● <id>  <activity>  <elapsed>  <tokens>` (`*` for the bullet at the
/// `ascii` glyph level). The bullet is coloured by state and the activity is shortened first
/// when the line is too long.
pub fn render(rows: &[RosterRow], width: usize, theme: &Theme) -> Vec<Line> {
    rows.iter().map(|row| render_row(row, width, theme)).collect()
}

fn render_row(row: &RosterRow, width: usize, theme: &Theme) -> Line {
    let role = match row.state {
        RosterState::Queued => Role::Border,
        RosterState::Running => Role::Success,
        RosterState::Waiting => Role::Warning,
        RosterState::Idle => Role::Muted,
    };
    let id = one_line(&row.id);
    let elapsed = format_elapsed(row.elapsed);
    let tokens = format_tokens(row.tokens);

    // Everything except the activity: bullet, id and the two right-hand columns with separators.
    let fixed = 2 + text::width(&id) + 2 + 2 + elapsed.len() + 2 + tokens.len();
    let ellipsis = theme.glyph(Glyph::Ellipsis);
    let activity = text::truncate(&one_line(&row.activity), width.saturating_sub(fixed), ellipsis);

    let line = format!(
        "{} {id}  {activity}  {}  {}",
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
        let lines = render(&rows, 80, &Theme::dark());
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
            let lines = render(&[row("a", "b", 1, 1, state)], 40, &theme);
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
        let lines = render(&rows, 32, &Theme::dark());
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
            for line in render(&rows, width, &Theme::dark()) {
                assert!(text::width(&line) <= width, "width {width}: {line:?}");
            }
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
        let wide = text::strip_ansi(&render(&rows, 80, &theme)[0]);
        assert!(wide.starts_with("* scout-1  reading"), "{wide:?}");
        for width in 0..50 {
            let line = text::strip_ansi(&render(&rows, width, &theme)[0]);
            assert!(line.is_ascii(), "width {width}: {line:?}");
            assert!(text::width(&line) <= width, "width {width}: {line:?}");
        }
        assert!(text::strip_ansi(&render(&rows, 32, &theme)[0]).ends_with("...  42s  1.2k"));
    }
}
