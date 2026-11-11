//! Status line: `mode · model · context % · cost · cwd`.

use crate::render::Line;
use crate::text;
use crate::theme::{Role, Theme};

/// Joins the non-empty segments with ` · ` and cuts the line on the right when it is too long.
/// Segments may carry their own SGR styling.
pub fn render(segments: &[String], width: usize, theme: &Theme) -> Line {
    let separator = theme.paint(Role::Muted, " · ");
    let mut line = String::new();
    for segment in segments.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if !line.is_empty() {
            line.push_str(&separator);
        }
        line.push_str(segment);
    }
    text::truncate(&line, width, "…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn joins_segments_and_skips_empty_ones() {
        let line = render(&segments(&["ask", "", "mock/model", " 12% "]), 80, &Theme::dark());
        assert_eq!(text::strip_ansi(&line), "ask · mock/model · 12%");
    }

    #[test]
    fn truncates_on_the_right() {
        let line = render(
            &segments(&["ask", "mock/model", "/a/very/long/working/directory"]),
            20,
            &Theme::dark(),
        );
        let plain = text::strip_ansi(&line);
        assert_eq!(text::width(&line), 20);
        assert!(plain.starts_with("ask · mock/model"));
        assert!(plain.ends_with('…'));
    }

    #[test]
    fn empty_input_is_an_empty_line() {
        assert_eq!(render(&[], 10, &Theme::dark()), "");
        assert_eq!(render(&segments(&["status"]), 0, &Theme::dark()), "");
    }
}
