//! The agent's todo list.

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
/// `✓` done (muted).
pub fn render(items: &[(String, TodoState)], width: usize, theme: &Theme) -> Vec<Line> {
    let avail = width.saturating_sub(2).max(1);
    let mut lines = Vec::with_capacity(items.len());
    for (item, state) in items {
        let item = super::one_line(item);
        let (marker, styled) = match state {
            TodoState::Pending => ("○".to_string(), item),
            TodoState::InProgress => (theme.paint(Role::Accent, "◉"), format!("\x1b[1m{item}\x1b[22m")),
            TodoState::Done => (theme.paint(Role::Success, "✓"), theme.paint(Role::Muted, &item)),
        };
        for (i, piece) in text::wrap(&styled, avail).into_iter().enumerate() {
            let line = if i == 0 {
                format!("{marker} {piece}")
            } else {
                format!("  {piece}")
            };
            lines.push(text::truncate(&line, width, ""));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(list: &[(&str, TodoState)]) -> Vec<(String, TodoState)> {
        list.iter()
            .map(|(text, state)| ((*text).to_string(), *state))
            .collect()
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
    fn wraps_with_a_hanging_indent() {
        let list = items(&[("refactor the renderer to diff rows", TodoState::Pending)]);
        let lines = render(&list, 16, &Theme::dark());
        assert_eq!(lines, ["○ refactor the", "  renderer to", "  diff rows"]);
    }

    #[test]
    fn never_exceeds_the_width() {
        let list = items(&[
            ("a done item with several words", TodoState::Done),
            ("日本語のタスク", TodoState::InProgress),
        ]);
        for width in 0..30 {
            for line in render(&list, width, &Theme::dark()) {
                assert!(text::width(&line) <= width, "width {width}: {line:?}");
            }
        }
    }

    #[test]
    fn empty_list_renders_nothing() {
        assert!(render(&[], 40, &Theme::dark()).is_empty());
    }
}
