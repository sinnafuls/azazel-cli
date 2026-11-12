//! Unified diffs: a line-number gutter, added and removed lines on filled backgrounds.

use similar::TextDiff;

use crate::render::Line;
use crate::text;
use crate::theme::{RESET, Role, Theme};

const CONTEXT_LINES: usize = 3;

#[derive(Clone, Copy)]
enum Kind {
    FileHeader,
    Hunk,
    Added,
    Removed,
    Context,
    Meta,
}

/// Where a hunk starts and how many lines it covers, on one side of the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Range {
    start: usize,
    count: usize,
}

/// Renders a unified diff `width` columns wide.
///
/// Lines inside a hunk get their line number in a gutter: the new file's for added and unchanged
/// lines, the old file's for removed ones. Added and removed lines are filled with a background
/// across the whole width; hunk headers, file headers and other metadata keep the full width for
/// themselves. A line that is too long continues on the next row, under its text.
pub fn render_unified(diff_text: &str, width: usize, theme: &Theme) -> Vec<Line> {
    let width = width.max(1);
    let last_number = diff_text
        .lines()
        .filter_map(hunk_ranges)
        .map(|(old, new)| (old.start + old.count).max(new.start + new.count))
        .max();
    // The gutter is the widest line number and a space; it is dropped when it would leave no
    // room for the line itself.
    let digits = last_number
        .map(|n| n.to_string().len())
        .filter(|digits| digits + 3 <= width)
        .unwrap_or(0);
    let gutter_width = if digits == 0 { 0 } else { digits + 1 };
    let blank_gutter = " ".repeat(gutter_width);

    let mut out = Vec::new();
    // Lines the current hunk still owes, from its `@@ -a,b +c,d @@` header. Inside a hunk only the
    // first character decides, so a removed line that starts with `--` is not taken for a header.
    let mut old_left = 0usize;
    let mut new_left = 0usize;
    let mut old_number = 0usize;
    let mut new_number = 0usize;
    for raw in diff_text.lines() {
        let line = text::sanitize(raw);
        let mut number = None;
        let kind = if line.starts_with("@@") {
            let (old, new) = hunk_ranges(&line).unwrap_or_default();
            (old_left, new_left) = (old.count, new.count);
            (old_number, new_number) = (old.start, new.start);
            Kind::Hunk
        } else if old_left > 0 || new_left > 0 {
            match line.as_bytes().first() {
                Some(b'+') => {
                    new_left = new_left.saturating_sub(1);
                    number = Some(new_number);
                    new_number += 1;
                    Kind::Added
                }
                Some(b'-') => {
                    old_left = old_left.saturating_sub(1);
                    number = Some(old_number);
                    old_number += 1;
                    Kind::Removed
                }
                Some(b'\\') => Kind::Meta,
                _ => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                    number = Some(new_number);
                    old_number += 1;
                    new_number += 1;
                    Kind::Context
                }
            }
        } else if line.starts_with("+++") || line.starts_with("---") {
            Kind::FileHeader
        } else if line.starts_with('+') {
            Kind::Added
        } else if line.starts_with('-') {
            Kind::Removed
        } else if line.starts_with(' ') {
            Kind::Context
        } else {
            Kind::Meta
        };

        match kind {
            Kind::FileHeader | Kind::Hunk | Kind::Meta => {
                for piece in text::wrap_hard(&line, width) {
                    out.push(match kind {
                        Kind::FileHeader => format!("\x1b[1m{piece}{RESET}"),
                        Kind::Hunk => theme.paint(Role::Accent, &piece),
                        _ => theme.paint(Role::Muted, &piece),
                    });
                }
            }
            Kind::Added | Kind::Removed | Kind::Context => {
                let numbered = match number {
                    Some(n) if digits > 0 => format!("{n:>digits$} "),
                    _ => blank_gutter.clone(),
                };
                let pieces = text::wrap_hard(&line, width - gutter_width);
                for (i, piece) in pieces.iter().enumerate() {
                    let gutter = if i == 0 { &numbered } else { &blank_gutter };
                    out.push(match kind {
                        Kind::Added => filled(
                            Role::DiffAddBg,
                            Role::DiffAdd,
                            gutter,
                            piece,
                            i == 0,
                            width,
                            theme,
                        ),
                        Kind::Removed => filled(
                            Role::DiffDelBg,
                            Role::DiffDel,
                            gutter,
                            piece,
                            i == 0,
                            width,
                            theme,
                        ),
                        _ => format!("{}{piece}", theme.paint(Role::Muted, gutter)),
                    });
                }
            }
        }
    }
    out
}

/// Renders the unified diff between two versions of a file with three lines of context. Returns
/// nothing when the texts are equal.
pub fn render_texts(old: &str, new: &str, path: &str, width: usize, theme: &Theme) -> Vec<Line> {
    // A line break in the path would forge diff lines.
    let label = super::one_line(path);
    let diff = TextDiff::from_lines(old, new);
    let unified = diff
        .unified_diff()
        .context_radius(CONTEXT_LINES)
        .header(&format!("a/{label}"), &format!("b/{label}"))
        .to_string();
    render_unified(&unified, width, theme)
}

/// The old and new ranges of a hunk header such as `@@ -12,7 +12,9 @@ fn name()`.
fn hunk_ranges(header: &str) -> Option<(Range, Range)> {
    let mut parts = header.strip_prefix("@@")?.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let range = |text: &str| {
        let (start, count) = match text.split_once(',') {
            Some((start, count)) => (start, count.parse::<usize>().ok()?),
            None => (text, 1),
        };
        Some(Range {
            start: start.parse().ok()?,
            count,
        })
    };
    Some((range(old)?, range(new)?))
}

/// One row of an added or removed line: the gutter, the sign in the colour of `sign` (on the
/// first row), the text in the ordinary text colour, all on `background` up to `width` columns.
fn filled(
    background: Role,
    sign: Role,
    gutter: &str,
    piece: &str,
    first: bool,
    width: usize,
    theme: &Theme,
) -> Line {
    let (mark, rest) = if first { piece.split_at(1) } else { ("", piece) };
    let padding = width.saturating_sub(text::width(gutter) + text::width(piece));
    let mut line = theme.bg(background);
    if !gutter.is_empty() {
        line.push_str(&theme.fg(Role::Muted));
        line.push_str(gutter);
    }
    if !mark.is_empty() {
        line.push_str(&theme.fg(sign));
        line.push_str(mark);
    }
    line.push_str(&theme.fg(Role::Text));
    line.push_str(rest);
    line.extend(std::iter::repeat_n(' ', padding));
    line.push_str(RESET);
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Color;

    const ADD_BG: &str = "\x1b[42m";
    const DEL_BG: &str = "\x1b[41m";
    const ADD: &str = "\x1b[92m";
    const DEL: &str = "\x1b[91m";
    const HUNK: &str = "\x1b[34m";
    const META: &str = "\x1b[90m";
    const BOLD: &str = "\x1b[1m";

    fn theme() -> Theme {
        let mut theme = Theme::dark();
        theme.set_color(Role::DiffAddBg, Color::Indexed(2));
        theme.set_color(Role::DiffDelBg, Color::Indexed(1));
        theme.set_color(Role::DiffAdd, Color::Indexed(10));
        theme.set_color(Role::DiffDel, Color::Indexed(9));
        theme.set_color(Role::Accent, Color::Indexed(4));
        theme.set_color(Role::Muted, Color::Indexed(8));
        theme
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| text::strip_ansi(l)).collect()
    }

    /// The rows without the padding that fills added and removed lines.
    fn trimmed(lines: &[Line]) -> Vec<String> {
        plain(lines).iter().map(|l| l.trim_end().to_string()).collect()
    }

    #[test]
    fn diffs_two_texts_with_headers_numbers_and_fills() {
        let lines = render_texts("a\nb\nc\n", "a\nB\nc\n", "src/f.txt", 20, &theme());
        assert_eq!(
            trimmed(&lines),
            [
                "--- a/src/f.txt",
                "+++ b/src/f.txt",
                "@@ -1,3 +1,3 @@",
                "1  a",
                "2 -b",
                "2 +B",
                "3  c"
            ]
        );
        assert!(lines[0].starts_with(BOLD) && lines[1].starts_with(BOLD));
        assert!(lines[2].starts_with(HUNK));
        // Unchanged lines have a muted number and plain text.
        assert_eq!(lines[3], format!("{META}1 {RESET} a"));
        // Changed lines are filled to the full width, with the sign in its own colour.
        assert_eq!(
            lines[4],
            format!("{DEL_BG}{META}2 {DEL}-\x1b[39mb{}{RESET}", " ".repeat(16))
        );
        assert_eq!(
            lines[5],
            format!("{ADD_BG}{META}2 {ADD}+\x1b[39mB{}{RESET}", " ".repeat(16))
        );
        assert_eq!(text::width(&lines[4]), 20);
    }

    #[test]
    fn equal_texts_produce_no_diff() {
        assert!(render_texts("same\n", "same\n", "f", 80, &theme()).is_empty());
    }

    #[test]
    fn keeps_three_lines_of_context_and_numbers_both_sides() {
        let numbered: Vec<String> = (1..=12).map(|i| format!("line {i}")).collect();
        let old = numbered.join("\n") + "\n";
        let new = old.replace("line 6\n", "line six\nand a half\n");
        let lines = trimmed(&render_texts(&old, &new, "f", 80, &theme()));
        assert_eq!(
            &lines[3..],
            [
                " 3  line 3",
                " 4  line 4",
                " 5  line 5",
                " 6 -line 6",
                " 6 +line six",
                " 7 +and a half",
                " 8  line 7",
                " 9  line 8",
                "10  line 9",
            ]
        );
    }

    #[test]
    fn removed_line_starting_with_dashes_is_not_a_file_header() {
        let lines = render_texts("-- comment\nkeep\n", "keep\n", "q.sql", 80, &theme());
        let removed = lines
            .iter()
            .find(|l| text::strip_ansi(l).trim_end() == "1 --- comment")
            .expect("removed line is shown");
        assert!(removed.starts_with(DEL_BG), "{removed:?}");
        // The real file header above it is still a header.
        assert!(lines[0].starts_with(BOLD));
    }

    #[test]
    fn colours_metadata_and_bare_fragments() {
        let source = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n";
        let lines = render_unified(source, 80, &theme());
        let starts = [META, META, BOLD, BOLD, HUNK, DEL_BG, META, ADD_BG];
        assert_eq!(lines.len(), starts.len());
        for (line, start) in lines.iter().zip(starts) {
            assert!(line.starts_with(start), "{line:?} should start with {start:?}");
        }

        // A fragment without any header has no line numbers but is still filled by its sign.
        let lines = render_unified("+added\n-removed\n context", 80, &theme());
        assert!(lines[0].starts_with(ADD_BG) && lines[1].starts_with(DEL_BG));
        assert_eq!(trimmed(&lines), ["+added", "-removed", " context"]);
        assert_eq!(lines[2], " context");
    }

    #[test]
    fn long_lines_continue_under_their_text() {
        let source = format!("@@ -0,0 +1 @@\n+{}", "a".repeat(20));
        let lines = render_unified(&source, 10, &theme());
        assert_eq!(
            plain(&lines),
            ["@@ -0,0 +1", " @@", "1 +aaaaaaa", "  aaaaaaaa", "  aaaaa   "]
        );
        for line in &lines[2..] {
            assert!(line.starts_with(ADD_BG) && line.ends_with(RESET));
            assert_eq!(text::width(line), 10);
        }
    }

    #[test]
    fn the_gutter_gives_way_when_there_is_no_room() {
        let lines = render_unified("@@ -1 +1 @@\n-a\n+b", 3, &theme());
        assert_eq!(plain(&lines[lines.len() - 2..]), ["-a ", "+b "]);
        for width in 0..12 {
            for line in render_unified(
                "@@ -98,3 +98,3 @@\n ctx\n-old line\n+new line\n ctx",
                width,
                &theme(),
            ) {
                assert!(text::width(&line) <= width.max(1), "width {width}: {line:?}");
            }
        }
    }

    #[test]
    fn hunk_header_ranges() {
        let range = |start, count| Range { start, count };
        assert_eq!(
            hunk_ranges("@@ -12,7 +12,9 @@ fn name()"),
            Some((range(12, 7), range(12, 9)))
        );
        assert_eq!(hunk_ranges("@@ -1 +1,2 @@"), Some((range(1, 1), range(1, 2))));
        assert_eq!(hunk_ranges("@@ -0,0 +1 @@"), Some((range(0, 0), range(1, 1))));
        assert_eq!(hunk_ranges("@@ garbage"), None);
    }
}
