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

/// One row of a file's diff as the diff viewer lays it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// `@@ -12,7 +12,9 @@ fn name()`.
    Hunk(String),
    Context {
        old: usize,
        new: usize,
        text: String,
    },
    Added {
        new: usize,
        text: String,
    },
    Removed {
        old: usize,
        text: String,
    },
    /// Something said about the file rather than a line of it: a change of mode, a binary file,
    /// `\ No newline at end of file`.
    Note(String),
}

/// Header lines the file list of the viewer already says (the path, that the file is new,
/// deleted or renamed), or that say nothing to a reader (the blob ids).
const SAID_ELSEWHERE: [&str; 10] = [
    "--- ",
    "+++ ",
    "index ",
    "diff --git ",
    "new file",
    "deleted file mode",
    "rename from ",
    "rename to ",
    "similarity index",
    "dissimilarity index",
];

/// The rows of one file's unified diff (from its header lines on, as `git diff` writes it).
pub fn rows(diff_text: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let (mut old_left, mut new_left) = (0usize, 0usize);
    let (mut old, mut new) = (0usize, 0usize);
    let mut in_hunks = false;
    for raw in diff_text.lines() {
        let line = text::sanitize(raw);
        if line.starts_with("@@") {
            let (from, to) = hunk_ranges(&line).unwrap_or_default();
            (old_left, new_left) = (from.count, to.count);
            (old, new) = (from.start, to.start);
            in_hunks = true;
            rows.push(Row::Hunk(line));
            continue;
        }
        if old_left == 0 && new_left == 0 {
            let header = !in_hunks && SAID_ELSEWHERE.iter().any(|said| line.starts_with(said));
            if !header && !line.is_empty() {
                rows.push(Row::Note(
                    line.strip_prefix('\\').unwrap_or(&line).trim().to_string(),
                ));
            }
            continue;
        }
        // Inside a hunk the first character decides, so `--x` is a removed line.
        let body = line.get(1..).unwrap_or_default().to_string();
        match line.as_bytes().first() {
            Some(b'+') => {
                new_left = new_left.saturating_sub(1);
                rows.push(Row::Added { new, text: body });
                new += 1;
            }
            Some(b'-') => {
                old_left = old_left.saturating_sub(1);
                rows.push(Row::Removed { old, text: body });
                old += 1;
            }
            Some(b'\\') => rows.push(Row::Note(line[1..].trim().to_string())),
            _ => {
                old_left = old_left.saturating_sub(1);
                new_left = new_left.saturating_sub(1);
                rows.push(Row::Context { old, new, text: body });
                old += 1;
                new += 1;
            }
        }
    }
    rows
}

/// How the viewer lays a diff out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Layout {
    /// Old and new side by side instead of one column.
    pub split: bool,
    /// Long lines continue on the next row instead of being cut.
    pub wrap: bool,
}

/// A diff laid out for the viewer.
#[derive(Debug, Clone, Default)]
pub struct Laid {
    pub lines: Vec<Line>,
    /// The line each hunk starts on.
    pub hunks: Vec<usize>,
}

/// What a cell of the viewer holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Context,
    Added,
    Removed,
    /// The side of a split row with no line on it.
    Empty,
}

/// The code of each row coloured as the language `path` names: the old file's lines and the new
/// file's are each highlighted as one text, so a line is coloured knowing the lines before it.
/// `None` for a row without code, or when the language is not known.
fn highlighted(rows: &[Row], path: &str, theme: &Theme) -> Vec<Option<String>> {
    let (mut old, mut new) = (Vec::new(), Vec::new());
    for row in rows {
        match row {
            Row::Context { text, .. } => {
                old.push(text.as_str());
                new.push(text.as_str());
            }
            Row::Added { text, .. } => new.push(text),
            Row::Removed { text, .. } => old.push(text),
            Row::Hunk(_) | Row::Note(_) => {}
        }
    }
    let paint = |lines: &[&str]| {
        let joined = lines.join("\n");
        match joined.len() > super::code::MAX_HIGHLIGHT_BYTES {
            true => None,
            false => super::markdown::highlight_file(path, &joined, theme),
        }
    };
    let (old, new) = match (paint(&old), paint(&new)) {
        (Some(old), Some(new)) => (old, new),
        _ => return vec![None; rows.len()],
    };
    let (mut old, mut new) = (old.into_iter(), new.into_iter());
    rows.iter()
        .map(|row| match row {
            Row::Context { .. } => {
                old.next();
                new.next()
            }
            Row::Added { .. } => new.next(),
            Row::Removed { .. } => old.next(),
            Row::Hunk(_) | Row::Note(_) => None,
        })
        .collect()
}

/// One side of a row: `gutter` (plain text), the sign, then `code` (styled already), filled with
/// the side's background, exactly `width` columns per row. Cut to one row, or wrapped.
fn cell(side: Side, gutter: &str, code: &str, width: usize, wrap: bool, theme: &Theme) -> Vec<Line> {
    let (background, sign, mark) = match side {
        Side::Added => (Some(Role::DiffAddBg), Role::DiffAdd, "+"),
        Side::Removed => (Some(Role::DiffDelBg), Role::DiffDel, "-"),
        Side::Context => (None, Role::Muted, " "),
        Side::Empty => (None, Role::Muted, " "),
    };
    let lead = text::width(gutter) + 1;
    if width <= lead {
        return vec![" ".repeat(width)];
    }
    let room = width - lead;
    let pieces = match wrap {
        true => text::wrap_hard(code, room),
        false => vec![text::truncate(
            code,
            room,
            theme.glyph(crate::glyphs::Glyph::Ellipsis),
        )],
    };
    let fill = background.map(|role| theme.bg(role)).unwrap_or_default();
    let blank = " ".repeat(text::width(gutter));
    pieces
        .iter()
        .enumerate()
        .map(|(i, piece)| {
            let (gutter, mark) = if i == 0 {
                (gutter, mark)
            } else {
                (blank.as_str(), " ")
            };
            let mut line = fill.clone();
            line.push_str(&theme.fg(Role::Muted));
            line.push_str(gutter);
            line.push_str(&theme.fg(sign));
            line.push_str(mark);
            line.push_str(&theme.fg(Role::Text));
            line.push_str(piece);
            // The piece may end with a reset; the fill goes on to the edge.
            line.push_str(&fill);
            line.extend(std::iter::repeat_n(' ', room.saturating_sub(text::width(piece))));
            line.push_str(RESET);
            line
        })
        .collect()
}

/// A line that spans the whole width: a hunk header or a note.
fn spanning(line: &str, role: Role, width: usize, wrap: bool, theme: &Theme) -> Vec<Line> {
    let pieces = match wrap {
        true => text::wrap_hard(line, width),
        false => vec![text::truncate(
            line,
            width,
            theme.glyph(crate::glyphs::Glyph::Ellipsis),
        )],
    };
    pieces.iter().map(|piece| theme.paint(role, piece)).collect()
}

/// Lays the rows of one file's diff out `width` columns wide: one column with the old and the
/// new line number, or the old file on the left and the new one on the right, removed lines
/// beside the added lines that replace them. Code is coloured as the language `path` names.
pub fn lay_out(rows: &[Row], path: &str, width: usize, layout: Layout, theme: &Theme) -> Laid {
    let width = width.max(1);
    let code = highlighted(rows, path, theme);
    let shown = |index: usize, text: &str| code[index].clone().unwrap_or_else(|| text.to_string());
    let last = rows
        .iter()
        .map(|row| match row {
            Row::Context { old, new, .. } => (*old).max(*new),
            Row::Added { new, .. } => *new,
            Row::Removed { old, .. } => *old,
            Row::Hunk(_) | Row::Note(_) => 0,
        })
        .max()
        .unwrap_or(0);
    let digits = last.max(1).ilog10() as usize + 1;
    let number = |n: Option<usize>| match n {
        Some(n) => format!("{n:>digits$} "),
        None => " ".repeat(digits + 1),
    };
    let mut laid = Laid::default();
    let (left, right) = (
        (width.saturating_sub(1)) / 2,
        width.saturating_sub(1) - (width.saturating_sub(1)) / 2,
    );
    let bar = theme.paint(Role::Border, theme.glyph(crate::glyphs::Glyph::TreeLine));
    let mut index = 0;
    while index < rows.len() {
        match &rows[index] {
            Row::Hunk(header) => {
                laid.hunks.push(laid.lines.len());
                laid.lines
                    .extend(spanning(header, Role::Accent, width, layout.wrap, theme));
                index += 1;
            }
            Row::Note(note) => {
                laid.lines
                    .extend(spanning(note, Role::Muted, width, layout.wrap, theme));
                index += 1;
            }
            Row::Context { old, new, text } if !layout.split => {
                let gutter = format!("{}{}", number(Some(*old)), number(Some(*new)));
                let code = shown(index, text);
                laid.lines
                    .extend(cell(Side::Context, &gutter, &code, width, layout.wrap, theme));
                index += 1;
            }
            Row::Added { new, text } if !layout.split => {
                let gutter = format!("{}{}", number(None), number(Some(*new)));
                let code = shown(index, text);
                laid.lines
                    .extend(cell(Side::Added, &gutter, &code, width, layout.wrap, theme));
                index += 1;
            }
            Row::Removed { old, text } if !layout.split => {
                let gutter = format!("{}{}", number(Some(*old)), number(None));
                let code = shown(index, text);
                laid.lines
                    .extend(cell(Side::Removed, &gutter, &code, width, layout.wrap, theme));
                index += 1;
            }
            Row::Context { old, new, text } => {
                let code = shown(index, text);
                let l = cell(
                    Side::Context,
                    &number(Some(*old)),
                    &code,
                    left,
                    layout.wrap,
                    theme,
                );
                let r = cell(
                    Side::Context,
                    &number(Some(*new)),
                    &code,
                    right,
                    layout.wrap,
                    theme,
                );
                side_by_side(&mut laid.lines, l, r, left, right, &bar);
                index += 1;
            }
            Row::Added { .. } | Row::Removed { .. } => {
                // A run of changed lines: what was removed on the left, what came instead on the
                // right, row by row.
                let (mut removed, mut added) = (Vec::new(), Vec::new());
                while let Some(row) = rows.get(index) {
                    match row {
                        Row::Removed { old, text } => removed.push((*old, shown(index, text))),
                        Row::Added { new, text } => added.push((*new, shown(index, text))),
                        _ => break,
                    }
                    index += 1;
                }
                for pair in 0..removed.len().max(added.len()) {
                    let l = match removed.get(pair) {
                        Some((old, code)) => {
                            cell(Side::Removed, &number(Some(*old)), code, left, layout.wrap, theme)
                        }
                        None => cell(Side::Empty, &number(None), "", left, false, theme),
                    };
                    let r = match added.get(pair) {
                        Some((new, code)) => {
                            cell(Side::Added, &number(Some(*new)), code, right, layout.wrap, theme)
                        }
                        None => cell(Side::Empty, &number(None), "", right, false, theme),
                    };
                    side_by_side(&mut laid.lines, l, r, left, right, &bar);
                }
            }
        }
    }
    laid
}

/// Puts the rows of two cells side by side with `bar` between them; the shorter one is filled.
fn side_by_side(out: &mut Vec<Line>, left: Vec<Line>, right: Vec<Line>, lw: usize, rw: usize, bar: &str) {
    for row in 0..left.len().max(right.len()) {
        let l = left.get(row).cloned().unwrap_or_else(|| " ".repeat(lw));
        let r = right.get(row).cloned().unwrap_or_else(|| " ".repeat(rw));
        out.push(format!("{l}{bar}{r}"));
    }
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

    const FILE: &str = "\
diff --git a/src/a.rs b/src/a.rs
old mode 100644
new mode 100755
index 1111111..2222222
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,4 @@ fn main()
 fn main() {
-    let x = 1;
-    let y = 2;
+    let x = 10;
     run(x);
 }
@@ -40,2 +40,3 @@
 end
+more
 tail
\\ No newline at end of file";

    #[test]
    fn a_file_diff_becomes_numbered_rows_without_the_headers_the_list_shows() {
        let rows = rows(FILE);
        let text = |s: &str| s.to_string();
        assert_eq!(rows[0], Row::Note(text("old mode 100644")));
        assert_eq!(rows[1], Row::Note(text("new mode 100755")));
        assert_eq!(rows[2], Row::Hunk(text("@@ -1,4 +1,4 @@ fn main()")));
        assert_eq!(
            rows[3],
            Row::Context {
                old: 1,
                new: 1,
                text: text("fn main() {")
            }
        );
        assert_eq!(
            rows[4],
            Row::Removed {
                old: 2,
                text: text("    let x = 1;")
            }
        );
        assert_eq!(
            rows[5],
            Row::Removed {
                old: 3,
                text: text("    let y = 2;")
            }
        );
        assert_eq!(
            rows[6],
            Row::Added {
                new: 2,
                text: text("    let x = 10;")
            }
        );
        assert_eq!(
            rows[7],
            Row::Context {
                old: 4,
                new: 3,
                text: text("    run(x);")
            }
        );
        assert_eq!(
            rows[10],
            Row::Context {
                old: 40,
                new: 40,
                text: text("end")
            }
        );
        assert_eq!(
            rows[11],
            Row::Added {
                new: 41,
                text: text("more")
            }
        );
        assert_eq!(rows.last(), Some(&Row::Note(text("No newline at end of file"))));
    }

    #[test]
    fn unified_rows_carry_both_line_numbers_and_fit_every_width() {
        let rows = rows(FILE);
        let laid = lay_out(&rows, "src/a.rs", 40, Layout::default(), &theme());
        let shown = trimmed(&laid.lines);
        assert_eq!(laid.hunks, [2, 9]);
        assert_eq!(shown[3], " 1  1  fn main() {");
        assert_eq!(shown[4], " 2    -    let x = 1;");
        assert_eq!(shown[6], "    2 +    let x = 10;");
        assert_eq!(shown[11], "   41 +more");
        for width in 1..120 {
            for layout in [
                Layout::default(),
                Layout {
                    split: true,
                    wrap: false,
                },
                Layout {
                    split: false,
                    wrap: true,
                },
                Layout {
                    split: true,
                    wrap: true,
                },
            ] {
                for line in lay_out(&rows, "src/a.rs", width, layout, &theme()).lines {
                    assert!(text::width(&line) <= width, "{width} {layout:?}: {line:?}");
                }
            }
        }
    }

    #[test]
    fn split_rows_put_a_removed_line_beside_the_line_that_replaced_it() {
        let rows = rows(FILE);
        let laid = lay_out(
            &rows,
            "a.txt",
            61,
            Layout {
                split: true,
                wrap: false,
            },
            &theme(),
        );
        let shown = plain(&laid.lines);
        let halves = |row: &str| {
            let (left, right) = row.split_once('│').unwrap();
            (left.trim_end().to_string(), right.trim_end().to_string())
        };
        assert_eq!(
            halves(&shown[3]),
            (" 1  fn main() {".into(), " 1  fn main() {".into())
        );
        assert_eq!(
            halves(&shown[4]),
            (" 2 -    let x = 1;".into(), " 2 +    let x = 10;".into())
        );
        // A removed line nothing replaced has an empty right side.
        assert_eq!(halves(&shown[5]), (" 3 -    let y = 2;".into(), String::new()));
        assert_eq!(
            halves(&shown[6]),
            (" 4      run(x);".into(), " 3      run(x);".into())
        );
        assert!(
            shown
                .iter()
                .all(|row| text::width(row) == 61 || !row.contains('│'))
        );
    }

    #[test]
    fn long_lines_are_cut_or_wrapped() {
        let diff = "@@ -1 +1 @@\n-short\n+a line that is much too long for the room";
        let rows = rows(diff);
        let cut = lay_out(&rows, "a.txt", 20, Layout::default(), &theme());
        assert_eq!(trimmed(&cut.lines)[2], "  1 +a line that is…");
        let wrapped = lay_out(
            &rows,
            "a.txt",
            20,
            Layout {
                split: false,
                wrap: true,
            },
            &theme(),
        );
        let shown = trimmed(&wrapped.lines);
        assert_eq!(shown.len(), 5);
        assert_eq!(shown[2], "  1 +a line that is");
        assert_eq!(shown[3], "     much too long f");
    }

    #[test]
    fn code_is_coloured_as_its_language() {
        let rows = rows(FILE);
        let rust = lay_out(&rows, "src/a.rs", 60, Layout::default(), &Theme::dark());
        let text = lay_out(&rows, "notes.unknown", 60, Layout::default(), &Theme::dark());
        assert_eq!(plain(&rust.lines), plain(&text.lines));
        // `fn` is a keyword in Rust and nothing in a file of no known language.
        assert_ne!(rust.lines[3], text.lines[3]);
        assert!(rust.lines[3].matches("\x1b[38;2;").count() > text.lines[3].matches("\x1b[38;2;").count());
    }
}
