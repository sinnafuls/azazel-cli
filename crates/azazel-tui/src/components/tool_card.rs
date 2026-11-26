//! A tool call, running or finished: a framed card, or one row in the compact style.
//!
//! ```text
//! ╭─── ✔ shell cargo test ──────────────────╮      ✔ 2 tool calls
//! │ test result: ok. 12 passed              │      ├─ ✔ read (src/main.rs) · 42 lines · 3ms
//! ╰── 1.4s · exit 0 ────────────────────────╯      └─ ✔ shell (cargo test) · 1.4s · exit 0
//! ```

use std::time::Duration;

use super::spinner::Spinner;
use super::{format_elapsed, one_line};
use crate::glyphs::Glyph;
use crate::render::Line;
use crate::text;
use crate::theme::{Role, Theme};

/// How many trailing output lines the card of a running tool shows.
pub const MAX_OUTPUT_LINES: usize = 12;

/// Narrowest card that still gets a frame.
const MIN_FRAMED_WIDTH: usize = 16;
/// Columns the frame takes from every body row: `│ ` and ` │`.
const SIDES: usize = 4;
/// Columns the rows under a compact call are set in by: as wide as the joint of a tree and the
/// space after it.
const COMPACT_INDENT: usize = 3;

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

    /// The icon of the state in its colour, and the columns it takes.
    fn icon(self, theme: &Theme) -> (String, usize) {
        let (role, symbol) = match self {
            CardState::Running => (Role::Accent, Spinner::now_in(theme.glyphs())),
            CardState::Success => (Role::Success, theme.glyph(Glyph::Success)),
            CardState::Warning => (Role::Warning, theme.glyph(Glyph::Warning)),
            CardState::Error => (Role::Error, theme.glyph(Glyph::Fail)),
        };
        (theme.paint(role, symbol), text::width(symbol))
    }

    /// How bad the state is, to pick the one a group of calls shows.
    fn severity(self) -> u8 {
        match self {
            CardState::Success => 0,
            CardState::Running => 1,
            CardState::Warning => 2,
            CardState::Error => 3,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Card<'a> {
    pub state: CardState,
    pub name: &'a str,
    /// What the call is about, on one line; shortened to fit the header.
    pub summary: &'a str,
    /// Where the summary leads when it is clicked: the file a call read or changed.
    pub link: Option<&'a str>,
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

/// The facts of a call on one line, between the separator of the glyph level.
fn joined_facts(facts: &[String], theme: &Theme) -> String {
    facts
        .iter()
        .map(|fact| one_line(fact))
        .filter(|fact| !fact.is_empty())
        .collect::<Vec<_>>()
        .join(theme.glyph(Glyph::Separator))
}

/// Draws the card as wide as `width`: a header `╭─── <icon> <name> <summary> ───╮`, the body
/// between `│` bars and the facts in the bottom border. The frame takes the colour of the state
/// and its characters from the glyph level (`+---` at `ascii`). When the facts do not fit the
/// border they become the last body row. Below sixteen columns there is no frame: header, body
/// and facts are returned as bare lines.
pub fn render(card: &Card, width: usize, theme: &Theme) -> Vec<Line> {
    let name = one_line(card.name);
    let summary = one_line(card.summary);
    let (icon, icon_width) = card.state.icon(theme);
    let facts = joined_facts(card.facts, theme);
    let ellipsis = theme.glyph(Glyph::Ellipsis);
    let mut lines = Vec::with_capacity(card.body.len() + 3);

    if width < MIN_FRAMED_WIDTH {
        let header = format!(
            "{icon} \x1b[1m{name}\x1b[22m {}",
            theme.paint(Role::Muted, &summary)
        );
        lines.push(text::truncate(header.trim_end(), width, ellipsis));
        lines.extend(card.body.iter().map(|row| text::truncate(row, width, ellipsis)));
        if !facts.is_empty() {
            lines.push(text::truncate(&theme.paint(Role::Muted, &facts), width, ellipsis));
        }
        return lines;
    }

    let frame = theme.glyphs().border();
    let border = card.state.border();
    let paint = |piece: &str| theme.paint(border, piece);
    let rule = |cells: usize| frame.horizontal.repeat(cells);
    let inner = width - SIDES;

    // `╭─── `, the icon and a space lead; a space and `╮` trail. The name gives way before the
    // frame does, the summary before the name.
    let fixed = 5 + icon_width + 1 + 2;
    let name = text::truncate(&name, width.saturating_sub(fixed), ellipsis);
    let room = width.saturating_sub(fixed + text::width(&name));
    let summary = if summary.is_empty() || room < 2 {
        String::new()
    } else {
        let shown = theme.paint(Role::Muted, &text::truncate(&summary, room - 1, ellipsis));
        match card.link {
            Some(url) => format!(" {}", theme.link(url, &shown)),
            None => format!(" {shown}"),
        }
    };
    let fill = room - text::width(&summary);
    lines.push(format!(
        "{}{icon} \x1b[1m{name}\x1b[22m{summary} {}",
        paint(&format!("{}{} ", frame.top_left, rule(3))),
        paint(&format!("{}{}", rule(fill), frame.top_right)),
    ));

    let (left, right) = (
        paint(&format!("{} ", frame.vertical)),
        paint(&format!(" {}", frame.vertical)),
    );
    let mut row = |content: &str| {
        let content = text::pad_right(&text::truncate(content, inner, ellipsis), inner);
        lines.push(format!("{left}{content}{right}"));
    };
    for content in card.body {
        row(content);
    }

    let closed = paint(&format!(
        "{}{}{}",
        frame.bottom_left,
        rule(width - 2),
        frame.bottom_right
    ));
    // `╰── ` and ` ╯` around the facts, with at least nothing to fill.
    let framed = text::width(&facts) + 6;
    if facts.is_empty() {
        lines.push(closed);
    } else if framed <= width {
        lines.push(format!(
            "{} {} {}",
            paint(&format!("{}{}", frame.bottom_left, rule(2))),
            theme.paint(Role::Muted, &facts),
            paint(&format!("{}{}", rule(width - framed), frame.bottom_right)),
        ));
    } else {
        row(&theme.paint(Role::Muted, &facts));
        lines.push(closed);
    }
    lines
}

/// Columns a body row has under a compact call drawn `width` columns wide.
pub fn compact_inner_width(width: usize) -> usize {
    width.saturating_sub(COMPACT_INDENT)
}

/// Fewest columns the facts of a compact row keep when the summary wants them too.
const COMPACT_FACTS: usize = 24;

/// One call on one row: `<icon> <name> (<summary>) · <facts>`. Facts that are short stay
/// whole and the summary gives way to them; facts that run long (what went wrong) are cut so
/// that the summary still says what the call was about.
fn compact_row(card: &Card, width: usize, theme: &Theme) -> Line {
    let ellipsis = theme.glyph(Glyph::Ellipsis);
    let name = one_line(card.name);
    let summary = one_line(card.summary);
    let (icon, icon_width) = card.state.icon(theme);
    let facts = match joined_facts(card.facts, theme) {
        facts if facts.is_empty() => facts,
        facts => format!("{}{facts}", theme.glyph(Glyph::Separator)),
    };
    let mut row = format!("{icon} \x1b[1m{name}\x1b[22m");
    let room = width.saturating_sub(icon_width + 1 + text::width(&name));
    // A space and two brackets around the summary.
    let wanted = match summary.is_empty() {
        true => 0,
        false => text::width(&summary) + 3,
    };
    let tail = text::width(&facts);
    let tail = match wanted + tail <= room {
        true => tail,
        false => room
            .saturating_sub(wanted)
            .max(tail.min((room / 2).max(COMPACT_FACTS))),
    };
    let left = room.saturating_sub(tail);
    // Enough of the summary to tell what it is, or none of it.
    if wanted > 0 && left >= 6 {
        let shown = theme.paint(Role::Muted, &text::truncate(&summary, left - 3, ellipsis));
        let shown = match card.link {
            Some(url) => theme.link(url, &shown),
            None => shown,
        };
        row.push_str(&format!(" ({shown})"));
    }
    row.push_str(&theme.paint(Role::Muted, &text::truncate(&facts, tail, ellipsis)));
    text::truncate(&row, width, ellipsis)
}

/// Finished calls in the compact style. One call is one row; two or more stand as a tree under
/// a row that counts them and shows the worst of their states:
///
/// ```text
/// ✘ 3 tool calls · 1 failed
/// ├─ ✔ read (src/main.rs) · 42 lines · 3ms
/// ├─ ✔ edit (src/main.rs) · 12ms
/// │  12 -old
/// │  12 +new
/// └─ ✘ shell (cargo test) · 1.4s · exit 101
/// ```
///
/// The body of a card (a diff) follows its row, set in by [`compact_inner_width`]'s margin, so
/// it is rendered for that width.
pub fn compact(cards: &[Card], width: usize, theme: &Theme) -> Vec<Line> {
    let ellipsis = theme.glyph(Glyph::Ellipsis);
    let inner = compact_inner_width(width);
    let mut lines = Vec::with_capacity(cards.iter().map(|card| card.body.len() + 1).sum::<usize>() + 1);
    let under = |lead: &str, card: &Card, lines: &mut Vec<Line>| {
        for row in card.body {
            lines.push(format!("{lead}{}", text::truncate(row, inner, ellipsis)));
        }
    };
    if let [card] = cards {
        lines.push(compact_row(card, width, theme));
        under(&" ".repeat(COMPACT_INDENT.min(width)), card, &mut lines);
        return lines;
    }
    let Some(worst) = cards
        .iter()
        .map(|card| card.state)
        .max_by_key(|state| state.severity())
    else {
        return lines;
    };
    let (icon, _) = worst.icon(theme);
    let mut header = format!("{icon} \x1b[1m{} tool calls\x1b[22m", cards.len());
    let failed = cards.iter().filter(|card| card.state == CardState::Error).count();
    if failed > 0 {
        let note = format!("{}{failed} failed", theme.glyph(Glyph::Separator));
        header.push_str(&theme.paint(Role::Error, &note));
    }
    lines.push(text::truncate(&header, width, ellipsis));
    // Too narrow for a tree: the rows alone.
    if width <= COMPACT_INDENT {
        lines.extend(cards.iter().map(|card| compact_row(card, width, theme)));
        return lines;
    }
    let paint = |glyph: Glyph| theme.paint(Role::BorderMuted, theme.glyph(glyph));
    for (index, card) in cards.iter().enumerate() {
        let last = index + 1 == cards.len();
        let joint = paint(if last { Glyph::TreeLast } else { Glyph::TreeMid });
        lines.push(format!("{joint} {}", compact_row(card, inner, theme)));
        let lead = match last {
            true => " ".repeat(COMPACT_INDENT),
            false => format!("{}  ", paint(Glyph::TreeLine)),
        };
        under(&lead, card, &mut lines);
    }
    lines
}

/// The card of a call in flight: a spinner in the header, at most the last
/// [`MAX_OUTPUT_LINES`] lines of output (cut, never wrapped) and the time it has been running.
pub fn running(
    name: &str,
    summary: &str,
    output_tail: &[impl AsRef<str>],
    elapsed: Duration,
    width: usize,
    theme: &Theme,
) -> Vec<Line> {
    let inner = inner_width(width);
    let start = output_tail.len().saturating_sub(MAX_OUTPUT_LINES);
    let body: Vec<Line> = output_tail[start..]
        .iter()
        .map(|raw| {
            let row = text::truncate(&one_line(raw.as_ref()), inner, theme.glyph(Glyph::Ellipsis));
            theme.paint(Role::Muted, &row)
        })
        .collect();
    render(
        &Card {
            state: CardState::Running,
            name,
            summary,
            link: None,
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
    use crate::glyphs::GlyphLevel;

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
            link: None,
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

        let lines = running("shell", "make", &output(0), Duration::from_secs(1), 30, &theme);
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
                link: None,
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

    #[test]
    fn the_summary_is_a_link_when_the_terminal_follows_them() {
        let card = Card {
            state: CardState::Success,
            name: "read",
            summary: "src/a_rather_long_directory_name/main.rs",
            link: Some("file:///work/src/main.rs"),
            body: &[],
            facts: &[],
        };
        let linked = Theme::dark().with_links(true);
        for width in [20, 30, 80] {
            let lines = render(&card, width, &linked);
            assert!(
                lines[0].contains("\x1b]8;;file:///work/src/main.rs\x1b\\"),
                "{lines:?}"
            );
            assert_eq!(lines[0].matches(text::LINK_END).count(), 1);
            assert_eq!(text::width(&lines[0]), width);
            // The same card without links differs only by the two sequences.
            let bare = render(&card, width, &Theme::dark());
            assert_eq!(text::strip_ansi(&lines[0]), text::strip_ansi(&bare[0]));
            assert!(!bare[0].contains("\x1b]8"));
        }
    }

    #[test]
    fn the_frame_and_the_icons_follow_the_glyph_level() {
        let body = ["ok. 12 passed".to_string()];
        let facts = ["1.4s".to_string(), "exit 0".to_string()];
        let ascii = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        let lines = render(&done(CardState::Success, &body, &facts), 36, &ascii);
        assert_eq!(
            plain(&lines),
            [
                "+--- [+] shell cargo test ---------+",
                "| ok. 12 passed                    |",
                "+-- 1.4s | exit 0 -----------------+",
            ]
        );
        for (state, icon) in [(CardState::Warning, "[!]"), (CardState::Error, "[-]")] {
            let lines = render(&done(state, &[], &[]), 36, &ascii);
            assert!(
                plain(&lines)[0].starts_with(&format!("+--- {icon} shell")),
                "{lines:?}"
            );
        }
        // A Nerd Font changes the icon, not the frame.
        let nerd = Theme::dark().with_glyphs(GlyphLevel::Nerd);
        let lines = render(&done(CardState::Success, &body, &facts), 36, &nerd);
        assert_eq!(plain(&lines)[0], "╭─── \u{f00c} shell cargo test ───────────╮");

        let long = ["a row that is much longer than the card is wide".to_string()];
        for level in [GlyphLevel::Unicode, GlyphLevel::Nerd, GlyphLevel::Ascii] {
            let theme = Theme::dark().with_glyphs(level);
            for width in 0..=160 {
                for state in [CardState::Running, CardState::Success, CardState::Error] {
                    for line in render(&done(state, &long, &facts), width, &theme) {
                        assert!(text::width(&line) <= width, "{level}, width {width}: {line:?}");
                        if width >= MIN_FRAMED_WIDTH {
                            assert_eq!(text::width(&line), width, "{level}: {line:?}");
                        }
                        if level == GlyphLevel::Ascii {
                            assert!(text::strip_ansi(&line).is_ascii(), "width {width}: {line:?}");
                        }
                    }
                }
            }
        }
    }

    fn row<'a>(state: CardState, name: &'a str, summary: &'a str, facts: &'a [String]) -> Card<'a> {
        Card {
            state,
            name,
            summary,
            link: None,
            body: &[],
            facts,
        }
    }

    #[test]
    fn a_compact_call_is_one_row_that_gives_up_its_summary_first() {
        let theme = Theme::dark();
        let facts = ["42 lines".to_string(), "3ms".to_string()];
        let card = row(CardState::Success, "read", "src/components/tool_card.rs", &facts);
        let at = |width: usize| plain(&compact(std::slice::from_ref(&card), width, &theme));
        assert_eq!(at(60), ["✔ read (src/components/tool_card.rs) · 42 lines · 3ms"]);
        assert_eq!(at(40), ["✔ read (src/component…) · 42 lines · 3ms"]);
        assert_eq!(at(29), ["✔ read (sr…) · 42 lines · 3ms"]);
        assert_eq!(at(28), ["✔ read · 42 lines · 3ms"]);
        assert_eq!(at(12), ["✔ read · 42…"]);
        // What went wrong may run long: it is cut before the summary is.
        let why = [
            "0ms".to_string(),
            "H:/a/rather/long/path/to/missing.txt: not found".to_string(),
        ];
        let failed = row(CardState::Error, "read", "missing.txt", &why);
        assert_eq!(
            plain(&compact(&[failed], 50, &theme)),
            ["✘ read (missing.txt) · 0ms · H:/a/rather/long/pat…"]
        );
        // The name is bold, the summary and the facts step back.
        let line = &compact(std::slice::from_ref(&card), 60, &theme)[0];
        assert!(line.starts_with(&theme.paint(Role::Success, "✔")), "{line:?}");
        assert!(line.contains("\x1b[1mread\x1b[22m"), "{line:?}");
        assert!(
            line.ends_with(&theme.paint(Role::Muted, " · 42 lines · 3ms")),
            "{line:?}"
        );

        let linked = Theme::dark().with_links(true);
        let card = Card {
            link: Some("file:///work/a.rs"),
            ..row(CardState::Success, "read", "a.rs", &[])
        };
        let line = &compact(&[card], 40, &linked)[0];
        assert!(line.contains("\x1b]8;;file:///work/a.rs\x1b\\"), "{line:?}");
        assert_eq!(text::strip_ansi(line), "✔ read (a.rs)");
    }

    #[test]
    fn several_compact_calls_are_a_tree_under_their_count() {
        let theme = Theme::dark();
        let diff = ["1 -old".to_string(), "1 +new".to_string()];
        let none: [String; 0] = [];
        let took = ["12ms".to_string()];
        let cards = [
            row(CardState::Success, "read", "a.rs", &took),
            Card {
                body: &diff,
                ..row(CardState::Success, "edit", "a.rs", &took)
            },
            row(CardState::Warning, "shell", "make", &none),
        ];
        let lines = compact(&cards, 40, &theme);
        assert_eq!(
            plain(&lines),
            [
                "⚠ 3 tool calls",
                "├─ ✔ read (a.rs) · 12ms",
                "├─ ✔ edit (a.rs) · 12ms",
                "│  1 -old",
                "│  1 +new",
                "└─ ⚠ shell (make)",
            ]
        );
        assert!(
            lines[1].starts_with(&theme.paint(Role::BorderMuted, "├─")),
            "{:?}",
            lines[1]
        );

        // The worst state leads, and failures are counted.
        let mut failing = cards.to_vec();
        failing.push(row(CardState::Error, "shell", "cargo test", &none));
        let lines = plain(&compact(&failing, 40, &theme));
        assert_eq!(lines[0], "✘ 4 tool calls · 1 failed");
        assert_eq!(lines[5], "├─ ⚠ shell (make)");
        assert_eq!(lines[6], "└─ ✘ shell (cargo test)");

        // A call alone has no tree; its body is set in as far as it would be in one.
        assert_eq!(
            plain(&compact(&cards[1..2], 40, &theme)),
            ["✔ edit (a.rs) · 12ms", "   1 -old", "   1 +new"]
        );
        assert!(compact(&[], 40, &theme).is_empty());

        let ascii = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        assert_eq!(
            plain(&compact(&cards, 40, &ascii)),
            [
                "[!] 3 tool calls",
                "|- [+] read (a.rs) | 12ms",
                "|- [+] edit (a.rs) | 12ms",
                "|  1 -old",
                "|  1 +new",
                "`- [!] shell (make)",
            ]
        );
        for level in [GlyphLevel::Unicode, GlyphLevel::Nerd, GlyphLevel::Ascii] {
            let theme = Theme::dark().with_glyphs(level);
            for width in 0..=160 {
                for cards in [&failing[..], &failing[..1], &failing[1..2]] {
                    for line in compact(cards, width, &theme) {
                        assert!(text::width(&line) <= width, "{level}, width {width}: {line:?}");
                        if level == GlyphLevel::Ascii {
                            assert!(text::strip_ansi(&line).is_ascii(), "width {width}: {line:?}");
                        }
                    }
                }
            }
        }
    }
}
