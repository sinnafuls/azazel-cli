//! Unified diffs, coloured.

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

/// Colours a unified diff: added and removed lines, hunk headers and file headers. Lines wider
/// than `width` continue on the next row in the same colour.
pub fn render_unified(diff_text: &str, width: usize, theme: &Theme) -> Vec<Line> {
    let mut out = Vec::new();
    // Lines the current hunk still owes, from its `@@ -a,b +c,d @@` header. Inside a hunk only the
    // first character decides, so a removed line that starts with `--` is not taken for a header.
    let mut old_left = 0usize;
    let mut new_left = 0usize;
    for raw in diff_text.lines() {
        let line = text::sanitize(raw);
        let kind = if line.starts_with("@@") {
            (old_left, new_left) = hunk_counts(&line).unwrap_or((0, 0));
            Kind::Hunk
        } else if old_left > 0 || new_left > 0 {
            match line.as_bytes().first() {
                Some(b'+') => {
                    new_left = new_left.saturating_sub(1);
                    Kind::Added
                }
                Some(b'-') => {
                    old_left = old_left.saturating_sub(1);
                    Kind::Removed
                }
                Some(b'\\') => Kind::Meta,
                _ => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
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
        for piece in text::wrap_hard(&line, width) {
            out.push(paint(kind, &piece, theme));
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

/// The old and new line counts of a hunk header such as `@@ -12,7 +12,9 @@ fn name()`.
fn hunk_counts(header: &str) -> Option<(usize, usize)> {
    let mut parts = header.strip_prefix("@@")?.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let count = |range: &str| match range.split_once(',') {
        Some((_, n)) => n.parse::<usize>().ok(),
        None => Some(1),
    };
    Some((count(old)?, count(new)?))
}

fn paint(kind: Kind, piece: &str, theme: &Theme) -> Line {
    match kind {
        Kind::FileHeader => format!("\x1b[1m{piece}{RESET}"),
        Kind::Hunk => theme.paint(Role::Accent, piece),
        Kind::Added => theme.paint(Role::DiffAdd, piece),
        Kind::Removed => theme.paint(Role::DiffDel, piece),
        Kind::Meta => theme.paint(Role::Muted, piece),
        Kind::Context => piece.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Color;

    const ADD: &str = "\x1b[32m";
    const DEL: &str = "\x1b[31m";
    const HUNK: &str = "\x1b[34m";
    const META: &str = "\x1b[90m";
    const BOLD: &str = "\x1b[1m";

    fn theme() -> Theme {
        let mut theme = Theme::dark();
        theme.set_color(Role::DiffAdd, Color::Indexed(2));
        theme.set_color(Role::DiffDel, Color::Indexed(1));
        theme.set_color(Role::Accent, Color::Indexed(4));
        theme.set_color(Role::Muted, Color::Indexed(8));
        theme
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| text::strip_ansi(l)).collect()
    }

    #[test]
    fn diffs_two_texts_with_headers_and_colours() {
        let lines = render_texts("a\nb\nc\n", "a\nB\nc\n", "src/f.txt", 80, &theme());
        assert_eq!(
            plain(&lines),
            [
                "--- a/src/f.txt",
                "+++ b/src/f.txt",
                "@@ -1,3 +1,3 @@",
                " a",
                "-b",
                "+B",
                " c"
            ]
        );
        assert!(lines[0].starts_with(BOLD) && lines[1].starts_with(BOLD));
        assert!(lines[2].starts_with(HUNK));
        assert_eq!(lines[3], " a");
        assert!(lines[4].starts_with(DEL));
        assert!(lines[5].starts_with(ADD));
    }

    #[test]
    fn equal_texts_produce_no_diff() {
        assert!(render_texts("same\n", "same\n", "f", 80, &theme()).is_empty());
    }

    #[test]
    fn keeps_three_lines_of_context() {
        let numbered: Vec<String> = (1..=12).map(|i| format!("line {i}")).collect();
        let old = numbered.join("\n") + "\n";
        let new = old.replace("line 6\n", "line six\n");
        let lines = plain(&render_texts(&old, &new, "f", 80, &theme()));
        let context: Vec<&String> = lines.iter().filter(|l| l.starts_with(' ')).collect();
        assert_eq!(
            context,
            [" line 3", " line 4", " line 5", " line 7", " line 8", " line 9"]
        );
        assert!(lines.contains(&"-line 6".to_string()) && lines.contains(&"+line six".to_string()));
    }

    #[test]
    fn removed_line_starting_with_dashes_is_not_a_file_header() {
        let lines = render_texts("-- comment\nkeep\n", "keep\n", "q.sql", 80, &theme());
        let removed = lines
            .iter()
            .find(|l| text::strip_ansi(l) == "--- comment")
            .expect("removed line is shown");
        assert!(removed.starts_with(DEL), "{removed:?}");
        // The real file header above it is still a header.
        assert!(lines[0].starts_with(BOLD));
    }

    #[test]
    fn colours_metadata_and_bare_fragments() {
        let source = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n";
        let lines = render_unified(source, 80, &theme());
        let starts = [META, META, BOLD, BOLD, HUNK, DEL, META, ADD];
        assert_eq!(lines.len(), starts.len());
        for (line, start) in lines.iter().zip(starts) {
            assert!(line.starts_with(start), "{line:?} should start with {start:?}");
        }

        // A fragment without any header is still coloured by its first character.
        let lines = render_unified("+added\n-removed\n context", 80, &theme());
        assert!(lines[0].starts_with(ADD) && lines[1].starts_with(DEL));
        assert_eq!(lines[2], " context");
    }

    #[test]
    fn long_lines_continue_in_the_same_colour() {
        let source = format!("+{}", "a".repeat(24));
        let lines = render_unified(&source, 10, &theme());
        assert_eq!(plain(&lines), ["+aaaaaaaaa", "aaaaaaaaaa", "aaaaa"]);
        for line in &lines {
            assert!(line.starts_with(ADD) && line.ends_with(RESET));
        }
    }

    #[test]
    fn hunk_header_counts() {
        assert_eq!(hunk_counts("@@ -12,7 +12,9 @@ fn name()"), Some((7, 9)));
        assert_eq!(hunk_counts("@@ -1 +1,2 @@"), Some((1, 2)));
        assert_eq!(hunk_counts("@@ -0,0 +1 @@"), Some((0, 1)));
        assert_eq!(hunk_counts("@@ garbage"), None);
    }
}
