//! Framed card for a tool call, running or finished.
//!
//! ```text
//! ╭─── ✔ shell cargo test ──────────────────╮
//! │ test result: ok. 12 passed              │
//! ╰── 1.4s · exit 0 ────────────────────────╯
//! ```

use std::time::Duration;

use super::spinner::Spinner;
use super::{format_elapsed, one_line};
use crate::render::Line;
use crate::text;
use crate::theme::{Role, Theme};

/// How many trailing output lines the card of a running tool shows.
pub const MAX_OUTPUT_LINES: usize = 5;

/// Narrowest card that still gets a frame.
const MIN_FRAMED_WIDTH: usize = 16;
/// Columns the frame takes from every body row: `│ ` and ` │`.
const SIDES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardState {
    Running,
    Success,
    /// Finished without doing its work: interrupted or refused.
    Warning,
    Error,
}

impl CardState {
    fn border(self) -> Role {
        match self {
            CardState::Running => Role::Accent,
            // A finished call steps back so the transcript stays readable.
            CardState::Success => Role::BorderMuted,
            CardState::Warning => Role::Warning,
            CardState::Error => Role::Error,
        }
    }

    fn icon(self, theme: &Theme) -> String {
        match self {
            CardState::Running => theme.paint(Role::Accent, Spinner::now()),
            CardState::Success => theme.paint(Role::Success, "✔"),
            CardState::Warning => theme.paint(Role::Warning, "⚠"),
            CardState::Error => theme.paint(Role::Error, "✘"),
        }
    }
}

pub struct Card<'a> {
    pub state: CardState,
    pub name: &'a str,
    /// What the call is about, on one line; shortened to fit the header.
    pub summary: &'a str,
    /// Rows for the inside of the card, already styled. Rows wider than [`inner_width`] are cut.
    pub body: &'a [Line],
    /// Facts about the call, set into the bottom border: `1.4s`, `exit 0`.
    pub facts: &'a [String],
}

/// Columns a body row has inside a card `width` columns wide.
pub fn inner_width(width: usize) -> usize {
    if width < MIN_FRAMED_WIDTH {
        width
    } else {
        width - SIDES
    }
}

/// Draws the card as wide as `width`: a header `╭─── <icon> <name> <summary> ───╮`, the body
/// between `│` bars and the facts in the bottom border. The frame takes the colour of the state.
/// When the facts do not fit the border they become the last body row. Below sixteen columns
/// there is no frame: header, body and facts are returned as bare lines.
pub fn render(card: &Card, width: usize, theme: &Theme) -> Vec<Line> {
    let name = one_line(card.name);
    let summary = one_line(card.summary);
    let icon = card.state.icon(theme);
    let facts = card
        .facts
        .iter()
        .map(|fact| one_line(fact))
        .filter(|fact| !fact.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    let mut lines = Vec::with_capacity(card.body.len() + 3);

    if width < MIN_FRAMED_WIDTH {
        let header = format!(
            "{icon} \x1b[1m{name}\x1b[22m {}",
            theme.paint(Role::Muted, &summary)
        );
        lines.push(text::truncate(header.trim_end(), width, "…"));
        lines.extend(card.body.iter().map(|row| text::truncate(row, width, "…")));
        if !facts.is_empty() {
            lines.push(text::truncate(&theme.paint(Role::Muted, &facts), width, "…"));
        }
        return lines;
    }

    let border = card.state.border();
    let paint = |piece: &str| theme.paint(border, piece);
    let inner = width - SIDES;

    // `╭─── `, the icon and a space lead; a space and `╮` trail. The name gives way before the
    // frame does, the summary before the name.
    let fixed = 5 + 2 + 2;
    let name = text::truncate(&name, width - fixed, "…");
    let room = width - fixed - text::width(&name);
    let summary = if summary.is_empty() || room < 2 {
        String::new()
    } else {
        format!(
            " {}",
            theme.paint(Role::Muted, &text::truncate(&summary, room - 1, "…"))
        )
    };
    let fill = room - text::width(&summary);
    lines.push(format!(
        "{}{icon} \x1b[1m{name}\x1b[22m{summary} {}",
        paint("╭─── "),
        paint(&format!("{}╮", "─".repeat(fill))),
    ));

    let (left, right) = (paint("│ "), paint(" │"));
    let mut row = |content: &str| {
        let content = text::pad_right(&text::truncate(content, inner, "…"), inner);
        lines.push(format!("{left}{content}{right}"));
    };
    for content in card.body {
        row(content);
    }

    // `╰── ` and ` ╯` around the facts, with at least nothing to fill.
    let framed = text::width(&facts) + 6;
    if facts.is_empty() {
        lines.push(paint(&format!("╰{}╯", "─".repeat(width - 2))));
    } else if framed <= width {
        lines.push(format!(
            "{} {} {}",
            paint("╰──"),
            theme.paint(Role::Muted, &facts),
            paint(&format!("{}╯", "─".repeat(width - framed))),
        ));
    } else {
        row(&theme.paint(Role::Muted, &facts));
        lines.push(paint(&format!("╰{}╯", "─".repeat(width - 2))));
    }
    lines
}

/// The card of a call in flight: a spinner in the header, at most the last
/// [`MAX_OUTPUT_LINES`] lines of output (cut, never wrapped) and the time it has been running.
pub fn running(
    name: &str,
    summary: &str,
    output_tail: &[String],
    elapsed: Duration,
    width: usize,
    theme: &Theme,
) -> Vec<Line> {
    let inner = inner_width(width);
    let start = output_tail.len().saturating_sub(MAX_OUTPUT_LINES);
    let body: Vec<Line> = output_tail[start..]
        .iter()
        .map(|raw| theme.paint(Role::Muted, &text::truncate(&one_line(raw), inner, "…")))
        .collect();
    render(
        &Card {
            state: CardState::Running,
            name,
            summary,
            body: &body,
            facts: &[format_elapsed(elapsed)],
        },
        width,
        theme,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| text::strip_ansi(l)).collect()
    }

    fn output(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("line {i}")).collect()
    }

    fn done<'a>(state: CardState, body: &'a [Line], facts: &'a [String]) -> Card<'a> {
        Card {
            state,
            name: "shell",
            summary: "cargo test",
            body,
            facts,
        }
    }

    #[test]
    fn finished_card_is_framed_with_facts_in_the_bottom_border() {
        let body = ["ok. 12 passed".to_string()];
        let facts = ["1.4s".to_string(), "exit 0".to_string()];
        let lines = render(&done(CardState::Success, &body, &facts), 34, &Theme::dark());
        assert_eq!(
            plain(&lines),
            [
                "╭─── ✔ shell cargo test ─────────╮",
                "│ ok. 12 passed                  │",
                "╰── 1.4s · exit 0 ───────────────╯",
            ]
        );
        for line in &lines {
            assert_eq!(text::width(line), 34);
        }
    }

    #[test]
    fn frame_and_icon_follow_the_state() {
        let theme = Theme::dark();
        for (state, icon, role) in [
            (CardState::Success, "✔", Role::BorderMuted),
            (CardState::Warning, "⚠", Role::Warning),
            (CardState::Error, "✘", Role::Error),
        ] {
            let lines = render(&done(state, &[], &[]), 30, &theme);
            assert_eq!(lines.len(), 2);
            assert!(text::strip_ansi(&lines[0]).starts_with(&format!("╭─── {icon} shell")));
            for line in &lines {
                assert!(line.starts_with(&theme.fg(role)), "{state:?}: {line:?}");
            }
        }
    }

    #[test]
    fn running_card_shows_a_spinner_the_output_tail_and_the_clock() {
        let theme = Theme::dark();
        let lines = running("shell", "make", &output(8), Duration::from_secs(65), 30, &theme);
        let shown = plain(&lines);
        assert_eq!(shown.len(), 7);
        let header: Vec<char> = shown[0].chars().collect();
        assert_eq!(header[..5].iter().collect::<String>(), "╭─── ");
        assert!(('\u{2800}'..='\u{28ff}').contains(&header[5]), "{:?}", shown[0]);
        assert_eq!(header[6..].iter().collect::<String>(), " shell make ───────────╮");
        assert_eq!(shown[1], "│ line 4                     │");
        assert_eq!(shown[5], "│ line 8                     │");
        assert_eq!(shown[6], "╰── 1m05s ───────────────────╯");
        assert!(lines[0].starts_with(&theme.fg(Role::Accent)));

        let lines = running("shell", "make", &[], Duration::from_secs(1), 30, &theme);
        assert_eq!(lines.len(), 2, "no output yet: header and border only");
    }

    #[test]
    fn long_summaries_and_rows_are_cut_inside_the_frame() {
        let summary = "grep -rn a_rather_long_pattern crates/ --include=*.rs";
        let body = ["\x1b[31ma row that is much longer than the card is wide\x1b[0m".to_string()];
        let lines = render(
            &Card {
                state: CardState::Error,
                name: "shell",
                summary,
                body: &body,
                facts: &[],
            },
            30,
            &Theme::dark(),
        );
        assert_eq!(
            plain(&lines),
            [
                "╭─── ✘ shell grep -rn a_rat… ╮",
                "│ a row that is much longer… │",
                "╰────────────────────────────╯",
            ]
        );
    }

    #[test]
    fn facts_that_do_not_fit_the_border_move_inside() {
        let facts = ["a rather long fact".to_string(), "and another".to_string()];
        let lines = render(&done(CardState::Success, &[], &facts), 24, &Theme::dark());
        assert_eq!(
            plain(&lines),
            [
                "╭─── ✔ shell cargo te… ╮",
                "│ a rather long fact … │",
                "╰──────────────────────╯",
            ]
        );
    }

    #[test]
    fn output_is_cleaned_and_every_line_fits_any_width() {
        let tail = ["\x1b[32mcoloured\x1b[0m\ttabbed output that is quite long\r".to_string()];
        let lines = running("shell", "", &tail, Duration::ZERO, 24, &Theme::dark());
        assert_eq!(text::strip_ansi(&lines[1]), "│ coloured    tabbed … │");
        assert!(!lines[1].contains("\x1b[32m"));
        for width in 0..40 {
            let lines = running("shell", "a summary", &tail, Duration::ZERO, width, &Theme::dark());
            for line in &lines {
                assert!(text::width(line) <= width, "width {width}: {line:?}");
                if width >= MIN_FRAMED_WIDTH {
                    assert_eq!(text::width(line), width, "width {width}: {line:?}");
                }
            }
        }
    }
}
