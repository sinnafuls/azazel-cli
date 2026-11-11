//! A bordered box for pickers, approvals and help.

use crate::render::Line;
use crate::text;
use crate::theme::{Role, Theme};

/// Narrowest width at which a frame with one column of content fits.
const MIN_FRAMED_WIDTH: usize = 5;

/// Draws `body` inside a box as wide as `width` with `title` set into the top border. Body lines
/// that are too long are cut. Below five columns there is no room for a frame and the title and
/// body are returned bare.
pub fn render(title: &str, body: &[Line], width: usize, theme: &Theme) -> Vec<Line> {
    if width < MIN_FRAMED_WIDTH {
        let title = (!title.is_empty()).then_some(title);
        return title
            .into_iter()
            .chain(body.iter().map(String::as_str))
            .map(|l| text::truncate(l, width, ""))
            .collect();
    }
    let inner = width - 4;
    let mut out = Vec::with_capacity(body.len() + 2);

    let title = text::truncate(title, width - MIN_FRAMED_WIDTH, "…");
    if title.is_empty() {
        out.push(theme.paint(Role::Border, &format!("╭{}╮", "─".repeat(width - 2))));
    } else {
        let fill = width - MIN_FRAMED_WIDTH - text::width(&title);
        out.push(format!(
            "{}{}{}",
            theme.paint(Role::Border, "╭─ "),
            theme.paint(Role::Accent, &title),
            theme.paint(Role::Border, &format!(" {}╮", "─".repeat(fill))),
        ));
    }

    let left = theme.paint(Role::Border, "│ ");
    let right = theme.paint(Role::Border, " │");
    for line in body {
        let content = text::pad_right(&text::truncate(line, inner, "…"), inner);
        out.push(format!("{left}{content}{right}"));
    }

    out.push(theme.paint(Role::Border, &format!("╰{}╯", "─".repeat(width - 2))));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| text::strip_ansi(l)).collect()
    }

    #[test]
    fn draws_a_box_of_the_requested_width() {
        let body = ["first".to_string(), "\x1b[1msecond line\x1b[0m".to_string()];
        let lines = render("Pick", &body, 20, &Theme::dark());
        assert_eq!(
            plain(&lines),
            [
                "╭─ Pick ───────────╮",
                "│ first            │",
                "│ second line      │",
                "╰──────────────────╯",
            ]
        );
        for line in &lines {
            assert_eq!(text::width(line), 20);
        }
    }

    #[test]
    fn cuts_long_titles_and_body_lines() {
        let body = ["a body line that does not fit".to_string()];
        let lines = render("A very long overlay title", &body, 16, &Theme::dark());
        assert_eq!(
            plain(&lines),
            ["╭─ A very lon… ╮", "│ a body line… │", "╰──────────────╯"]
        );
    }

    #[test]
    fn untitled_box_has_a_plain_top_border() {
        let lines = render("", &["x".to_string()], 7, &Theme::dark());
        assert_eq!(plain(&lines), ["╭─────╮", "│ x   │", "╰─────╯"]);
    }

    #[test]
    fn falls_back_to_bare_lines_when_too_narrow() {
        let lines = render("Title", &["body".to_string()], 3, &Theme::dark());
        assert_eq!(lines, ["Tit", "bod"]);
    }
}
