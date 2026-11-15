//! The agent's todo list.

use crate::glyphs::Glyph;
use crate::render::Line;
use crate::text;
use crate::theme::{Role, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoState {
    Pending,
    InProgress,
    Done,
}

/// One entry per item, wrapped with a hanging indent: `○` pending, `◉` in progress (emphasised),
/// `✓` done (muted); `[ ]`, `[~]` and `[x]` at the `ascii` glyph level.
pub fn render(items: &[(String, TodoState)], width: usize, theme: &Theme) -> Vec<Line> {
    let mut lines = Vec::with_capacity(items.len());
    for (item, state) in items {
        let item = super::one_line(item);
        let (mark, marker, styled) = match state {
            TodoState::Pending => {
                let mark = theme.glyph(Glyph::TodoPending);
                (mark, mark.to_string(), item)
            }
            TodoState::InProgress => {
                let mark = theme.glyph(Glyph::TodoActive);
                (
                    mark,
                    theme.paint(Role::Accent, mark),
                    format!("\x1b[1m{item}\x1b[22m"),
                )
            }
            TodoState::Done => {
                let mark = theme.glyph(Glyph::TodoDone);
                (
                    mark,
                    theme.paint(Role::Success, mark),
                    theme.paint(Role::Muted, &item),
                )
            }
        };
        // Later rows of an item line up under its first word.
        let indent = text::width(mark) + 1;
        let avail = width.saturating_sub(indent).max(1);
        for (i, piece) in text::wrap(&styled, avail).into_iter().enumerate() {
            let line = if i == 0 {
                format!("{marker} {piece}")
            } else {
                format!("{}{piece}", " ".repeat(indent))
            };
            lines.push(text::truncate(&line, width, ""));
        }
    }
    lines
}

/// The list in at most `max` items: every item when they fit; otherwise the done ones fold into
/// one muted row (`✓ 11 done`), the item in progress shows with the open items after it, and
/// what is left out is counted under them (`… 3 more`). `max` 0 shows nothing.
pub fn render_at_most(items: &[(String, TodoState)], max: usize, width: usize, theme: &Theme) -> Vec<Line> {
    if max == 0 {
        return Vec::new();
    }
    if items.len() <= max {
        return render(items, width, theme);
    }
    let done = items
        .iter()
        .filter(|(_, state)| *state == TodoState::Done)
        .count();
    let open: Vec<(String, TodoState)> = items
        .iter()
        .filter(|(_, state)| *state != TodoState::Done)
        .cloned()
        .collect();
    // The item in progress leads; open items before it are counted with the rest.
    let start = open
        .iter()
        .position(|(_, state)| *state == TodoState::InProgress)
        .unwrap_or(0);
    let done_row = usize::from(done > 0 && max > 1);
    let room = max - done_row;
    let shown = if start == 0 && open.len() <= room {
        open.len()
    } else {
        room.saturating_sub(1).max(1).min(open.len() - start)
    };
    let more = open.len() - shown;
    let muted = |line: String| {
        text::truncate(
            &theme.paint(Role::Muted, &line),
            width,
            theme.glyph(Glyph::Ellipsis),
        )
    };
    let mut lines = Vec::new();
    if done_row == 1 {
        lines.push(muted(format!("{} {done} done", theme.glyph(Glyph::TodoDone))));
    }
    lines.extend(render(&open[start..start + shown], width, theme));
    if more > 0 {
        lines.push(muted(format!("{} {more} more", theme.glyph(Glyph::Ellipsis))));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::GlyphLevel;

    fn items(list: &[(&str, TodoState)]) -> Vec<(String, TodoState)> {
        list.iter()
            .map(|(text, state)| ((*text).to_string(), *state))
            .collect()
    }

    #[test]
    fn a_long_list_folds_what_is_done_and_counts_what_does_not_fit() {
        let mut list: Vec<(String, TodoState)> =
            (1..=11).map(|n| (format!("done {n}"), TodoState::Done)).collect();
        list.push(("now".to_string(), TodoState::InProgress));
        list.extend((1..=5).map(|n| (format!("next {n}"), TodoState::Pending)));
        let theme = Theme::dark();
        let plain = |max| -> Vec<String> {
            render_at_most(&list, max, 40, &theme)
                .iter()
                .map(|l| text::strip_ansi(l))
                .collect()
        };
        assert_eq!(
            plain(5),
            ["✓ 11 done", "◉ now", "○ next 1", "○ next 2", "… 3 more"]
        );
        // Room for every open item: nothing is counted.
        assert_eq!(plain(7).len(), 7);
        assert_eq!(plain(7).last().map(String::as_str), Some("○ next 5"));
        // A short list is shown whole, done items too; 0 hides it.
        assert_eq!(render_at_most(&list[10..13], 8, 40, &theme).len(), 3);
        assert!(plain(0).is_empty());
        // The item in progress shows even in the smallest room.
        assert_eq!(plain(1), ["◉ now", "… 5 more"]);
    }

    #[test]
    fn marks_each_state() {
        let list = items(&[
            ("read the code", TodoState::Done),
            ("write the fix", TodoState::InProgress),
            ("run the tests", TodoState::Pending),
        ]);
        let lines = render(&list, 40, &Theme::dark());
        let plain: Vec<String> = lines.iter().map(|l| text::strip_ansi(l)).collect();
        assert_eq!(plain, ["✓ read the code", "◉ write the fix", "○ run the tests"]);
        assert!(lines[1].contains("\x1b[1m"), "the item in progress is emphasised");
        assert!(!lines[2].contains('\x1b'), "pending items are unstyled");
    }

    #[test]
    fn never_exceeds_the_width() {
        let list = items(&[
            ("a done item with several words", TodoState::Done),
            ("日本語のタスク", TodoState::InProgress),
        ]);
        for level in GlyphLevel::ALL {
            let theme = Theme::dark().with_glyphs(level);
            for width in 0..30 {
                for line in render(&list, width, &theme) {
                    assert!(text::width(&line) <= width, "{level} width {width}: {line:?}");
                }
            }
        }
    }

    #[test]
    fn the_ascii_level_marks_states_in_brackets_and_indents_under_the_text() {
        let theme = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        let list = items(&[
            ("read the code", TodoState::Done),
            ("write the fix for it", TodoState::InProgress),
            ("run the tests", TodoState::Pending),
        ]);
        let plain: Vec<String> = render(&list, 18, &theme)
            .iter()
            .map(|l| text::strip_ansi(l))
            .collect();
        assert_eq!(
            plain,
            [
                "[x] read the code",
                "[~] write the fix",
                "    for it",
                "[ ] run the tests"
            ]
        );
    }
}
