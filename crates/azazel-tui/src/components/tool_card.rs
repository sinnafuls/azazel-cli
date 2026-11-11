//! Card for a running tool call: what it is doing and the tail of its output.

use std::time::Duration;

use super::{format_elapsed, one_line};
use crate::render::Line;
use crate::text;
use crate::theme::{Role, Theme};

/// How many trailing output lines a card shows.
pub const MAX_OUTPUT_LINES: usize = 5;

const INDENT: &str = "  ";

/// A header line `▸ <name> <summary>  <elapsed>` followed by at most the last
/// [`MAX_OUTPUT_LINES`] lines of output, indented and muted. The summary is shortened to keep the
/// elapsed time visible; output lines are cut, never wrapped.
pub fn render(
    name: &str,
    summary: &str,
    output_tail: &[String],
    elapsed: Duration,
    width: usize,
    theme: &Theme,
) -> Vec<Line> {
    let name = one_line(name);
    let summary = one_line(summary);
    let elapsed = format_elapsed(elapsed);

    // Marker, name and the elapsed column with its separator; the summary takes what is left.
    let fixed = 2 + text::width(&name) + 2 + elapsed.len();
    let room = width.saturating_sub(fixed + 1);
    let summary = if summary.is_empty() || room == 0 {
        String::new()
    } else {
        format!(" {}", text::truncate(&summary, room, "…"))
    };
    let header = format!(
        "{} \x1b[1m{name}\x1b[22m{summary}  {}",
        theme.paint(Role::Accent, "▸"),
        theme.paint(Role::Muted, &elapsed),
    );

    let mut lines = Vec::with_capacity(1 + MAX_OUTPUT_LINES);
    lines.push(text::truncate(&header, width, "…"));

    let body_width = width.saturating_sub(INDENT.len());
    let start = output_tail.len().saturating_sub(MAX_OUTPUT_LINES);
    for raw in &output_tail[start..] {
        let cut = text::truncate(&one_line(raw), body_width, "…");
        let line = format!("{INDENT}{}", theme.paint(Role::Muted, &cut));
        lines.push(text::truncate(&line, width, ""));
    }
    lines
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

    #[test]
    fn header_shows_name_summary_and_elapsed() {
        let lines = render(
            "shell",
            "cargo test -p azazel-tui",
            &[],
            Duration::from_secs(7),
            60,
            &Theme::dark(),
        );
        assert_eq!(plain(&lines), ["▸ shell cargo test -p azazel-tui  7s"]);
    }

    #[test]
    fn shows_only_the_last_five_output_lines() {
        let lines = render(
            "shell",
            "make",
            &output(8),
            Duration::from_secs(65),
            60,
            &Theme::dark(),
        );
        assert_eq!(
            plain(&lines),
            [
                "▸ shell make  1m05s",
                "  line 4",
                "  line 5",
                "  line 6",
                "  line 7",
                "  line 8"
            ]
        );
        let lines = render(
            "shell",
            "make",
            &output(2),
            Duration::from_secs(1),
            60,
            &Theme::dark(),
        );
        assert_eq!(plain(&lines), ["▸ shell make  1s", "  line 1", "  line 2"]);
    }

    #[test]
    fn summary_is_shortened_and_elapsed_stays_visible() {
        let summary = "grep -rn a_rather_long_pattern crates/ --include=*.rs";
        let lines = render("shell", summary, &[], Duration::from_secs(12), 30, &Theme::dark());
        let header = text::strip_ansi(&lines[0]);
        assert_eq!(text::width(&lines[0]), 30);
        assert!(header.starts_with("▸ shell grep -rn"), "{header:?}");
        assert!(header.ends_with("…  12s"), "{header:?}");
    }

    #[test]
    fn output_is_cleaned_and_cut_to_the_width() {
        let tail = ["\x1b[32mcoloured\x1b[0m\ttabbed output that is quite long\r".to_string()];
        let lines = render("shell", "", &tail, Duration::ZERO, 20, &Theme::dark());
        let body = text::strip_ansi(&lines[1]);
        // Eighteen columns remain after the indent: seventeen of text and the ellipsis.
        assert_eq!(body, "  coloured    tabbe…");
        assert!(!lines[1].contains("\x1b[32m"));
        for width in 0..40 {
            for line in render("shell", "a summary", &tail, Duration::ZERO, width, &Theme::dark()) {
                assert!(text::width(&line) <= width, "width {width}: {line:?}");
            }
        }
    }
}
