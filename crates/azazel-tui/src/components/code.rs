//! What reading and searching files returns, drawn for the inside of a tool card: a numbered and
//! highlighted listing of a file, a directory as a tree, search hits under the file they are in
//! and found files under their directory.
//!
//! ```text
//!  12 │ fn main() {          ├─ src/                 src/main.rs
//!  13 │     run();           ├─ Cargo.toml  1.2 KB    12 │ fn main() {
//!  14 │ }                    └─ README.md   3.4 KB    40 │     main_loop();
//! ```
//!
//! Every function takes the rows to show and nothing more: what a card leaves out is cut before
//! it is drawn, so a long file costs no more than the rows on screen.

use std::borrow::Cow;
use std::collections::HashMap;

use super::{markdown, one_line};
use crate::glyphs::{Glyph, GlyphLevel};
use crate::render::Line;
use crate::text;
use crate::theme::{RESET, Role, Theme};

/// A listing of more text than this is numbered but not highlighted.
pub const MAX_HIGHLIGHT_BYTES: usize = 128 * 1024;
/// Narrowest column of line numbers.
const MIN_GUTTER: usize = 3;
/// Widest column of names in a tree before the sizes stop lining up.
const NAME_COLUMN: usize = 32;
const BOLD: &str = "\x1b[1m";

fn digits(number: u64) -> usize {
    number.checked_ilog10().map_or(1, |tens| tens as usize + 1)
}

/// Columns of the number column when `last` is the highest number in it.
fn gutter(last: u64) -> usize {
    digits(last).max(MIN_GUTTER)
}

/// One row behind the number column: `  12 │ content`, the number right-aligned and muted.
/// `content` is styled already and is cut to what is left of `width`; where not even the column
/// fits, the content stands alone.
fn numbered(number: Option<u64>, gutter: usize, content: &str, width: usize, theme: &Theme) -> Line {
    let ellipsis = theme.glyph(Glyph::Ellipsis);
    // The number, a space, the bar and a space.
    let lead = gutter + 3;
    let mut row = String::new();
    let room = if width > lead {
        let number = match number {
            Some(number) => format!("{number:>gutter$}"),
            None => " ".repeat(gutter),
        };
        row.push_str(&theme.paint(Role::Muted, &number));
        row.push(' ');
        row.push_str(&theme.paint(Role::Border, theme.glyph(Glyph::TreeLine)));
        row.push(' ');
        width - lead
    } else {
        width
    };
    row.push_str(&text::truncate(content, room, ellipsis));
    if content.contains('\x1b') && !row.ends_with(RESET) {
        row.push_str(RESET);
    }
    row
}

/// The lines of a file behind a column of line numbers that starts at `first_line`, coloured as
/// the language `path` names by its extension. Lines longer than `width` are cut, not wrapped.
/// Text of an unknown kind, and text above [`MAX_HIGHLIGHT_BYTES`], is numbered and left plain.
pub fn listing(text: &str, path: &str, first_line: usize, width: usize, theme: &Theme) -> Vec<Line> {
    listing_rows(text, path, first_line, width, false, theme)
}

/// Like [`listing`], but a line longer than `width` continues on the next rows, behind an empty
/// number column, as a diff does. For boxes that must show all of the text.
pub fn listing_wrapped(text: &str, path: &str, first_line: usize, width: usize, theme: &Theme) -> Vec<Line> {
    listing_rows(text, path, first_line, width, true, theme)
}

fn listing_rows(
    text: &str,
    path: &str,
    first_line: usize,
    width: usize,
    wrap: bool,
    theme: &Theme,
) -> Vec<Line> {
    let clean = text::sanitize(text);
    let coloured = match text.len() > MAX_HIGHLIGHT_BYTES {
        true => None,
        false => markdown::highlight_file(path, &clean, theme),
    };
    let first = first_line as u64;
    let row = |index: usize, content: &str, gutter: usize| -> Vec<Line> {
        let number = Some(first + index as u64);
        if !wrap {
            return vec![numbered(number, gutter, content, width, theme)];
        }
        // Past the number column; where not even that fits, the content stands alone.
        let room = width.saturating_sub(gutter + 3).max(1);
        let room = if width > gutter + 3 { room } else { width };
        text::wrap_hard(content, room)
            .iter()
            .enumerate()
            .map(|(piece, content)| numbered(number.filter(|_| piece == 0), gutter, content, width, theme))
            .collect()
    };
    match coloured {
        Some(rows) => {
            let gutter = gutter(first + rows.len().saturating_sub(1) as u64);
            rows.iter()
                .enumerate()
                .flat_map(|(index, content)| row(index, content, gutter))
                .collect()
        }
        None => {
            let gutter = gutter(first + clean.split('\n').count().saturating_sub(1) as u64);
            clean
                .split('\n')
                .enumerate()
                .flat_map(|(index, content)| row(index, &theme.paint(Role::Text, content), gutter))
                .collect()
        }
    }
}

/// Icons of files by what their name ends in (or is), with the role that colours each: the
/// theme decides the colour, never the table.
const FILE_ICONS: [(&[&str], &str, Role); 40] = [
    (&["rs"], "\u{e7a8}", Role::Warning),
    (&["py", "pyi"], "\u{e73c}", Role::Accent),
    (&["js", "mjs", "cjs"], "\u{e74e}", Role::Warning),
    (&["ts", "mts", "cts"], "\u{e628}", Role::Accent),
    (&["jsx", "tsx"], "\u{e7ba}", Role::Bash),
    (&["go"], "\u{e627}", Role::Bash),
    (&["c"], "\u{e61e}", Role::Accent),
    (&["cpp", "cc", "cxx", "hpp", "hh"], "\u{e61d}", Role::Accent),
    (&["h"], "\u{f0fd}", Role::Thinking),
    (&["java"], "\u{e738}", Role::Error),
    (&["kt", "kts"], "\u{e634}", Role::Thinking),
    (&["cs"], "\u{e648}", Role::Success),
    (&["rb"], "\u{e739}", Role::Error),
    (&["php"], "\u{e73d}", Role::Thinking),
    (&["swift"], "\u{e755}", Role::Warning),
    (&["lua"], "\u{e620}", Role::Accent),
    (&["zig"], "\u{e6a9}", Role::Warning),
    (&["hs"], "\u{e61f}", Role::Thinking),
    (&["ex", "exs"], "\u{e62d}", Role::Thinking),
    (&["dart"], "\u{e798}", Role::Bash),
    (
        &["sh", "bash", "zsh", "fish", "ps1", "bat", "cmd"],
        "\u{f489}",
        Role::Success,
    ),
    (&["html", "htm"], "\u{e736}", Role::Warning),
    (&["css", "scss", "sass", "less"], "\u{e749}", Role::Accent),
    (&["vue"], "\u{e6a0}", Role::Success),
    (&["svelte"], "\u{e697}", Role::Error),
    (&["json", "jsonc", "json5"], "\u{e60b}", Role::Warning),
    (
        &["toml", "yaml", "yml", "ini", "cfg", "conf", "env"],
        "\u{e615}",
        Role::Muted,
    ),
    (&["xml", "xsl"], "\u{e619}", Role::Warning),
    (&["md", "mdx", "markdown"], "\u{e73e}", Role::Text),
    (&["txt", "log"], "\u{f15c}", Role::Muted),
    (&["lock"], "\u{f023}", Role::Muted),
    (&["sql", "db", "sqlite"], "\u{f1c0}", Role::Bash),
    (
        &["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg"],
        "\u{f1c5}",
        Role::Thinking,
    ),
    (&["pdf"], "\u{f1c1}", Role::Error),
    (
        &["zip", "tar", "gz", "tgz", "xz", "bz2", "7z", "rar"],
        "\u{f1c6}",
        Role::Warning,
    ),
    (&["mp3", "wav", "ogg", "flac"], "\u{f1c7}", Role::Thinking),
    (&["mp4", "mkv", "mov", "webm"], "\u{f1c8}", Role::Thinking),
    (
        &["exe", "dll", "so", "dylib", "bin", "o", "a"],
        "\u{f013}",
        Role::Muted,
    ),
    (
        &["gitignore", "gitattributes", "gitmodules"],
        "\u{f1d3}",
        Role::Error,
    ),
    (&["dockerfile", "dockerignore"], "\u{f308}", Role::Bash),
];

/// The icon before the name of a file or directory and the space after it. Icons need a Nerd
/// Font, so there is none at the other glyph levels.
pub fn icon(name: &str, dir: bool, theme: &Theme) -> String {
    if theme.glyphs().level() != GlyphLevel::Nerd {
        return String::new();
    }
    if dir {
        return format!("{} ", theme.paint(Role::Accent, theme.glyph(Glyph::Folder)));
    }
    let name = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase();
    // `main.rs` is known by `rs`, `Dockerfile` and `.gitignore` by what follows the dot or by
    // the whole name.
    let kind = name.rsplit_once('.').map_or(name.as_str(), |(_, kind)| kind);
    let (symbol, role) = FILE_ICONS
        .iter()
        .find(|(kinds, _, _)| kinds.contains(&kind))
        .map_or((theme.glyph(Glyph::File), Role::Muted), |(_, symbol, role)| {
            (*symbol, *role)
        });
    format!("{} ", theme.paint(role, symbol))
}

/// One entry of a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry<'a> {
    pub name: &'a str,
    pub dir: bool,
    /// Bytes of a file.
    pub size: Option<u64>,
}

/// Size for a person: `512 B`, `1.2 KB`, `3.4 MB`.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    // Tenths of the unit, so no rounding in floating point decides the digit shown.
    let mut tenths = bytes * 10 / 1024;
    let mut unit = 0;
    while tenths >= 10_240 && unit + 1 < UNITS.len() {
        tenths /= 1024;
        unit += 1;
    }
    format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[unit])
}

/// The entries of a directory as one level of a tree: directories in bold accent with their
/// `/`, files with their size in a column beside the names. `more` says that entries follow the
/// ones given, so the last of these does not close the tree.
pub fn tree(entries: &[Entry<'_>], more: bool, width: usize, theme: &Theme) -> Vec<Line> {
    let ellipsis = theme.glyph(Glyph::Ellipsis);
    let names: Vec<String> = entries
        .iter()
        .map(|entry| match entry.dir {
            true => format!("{}/", one_line(entry.name)),
            false => one_line(entry.name),
        })
        .collect();
    let column = names
        .iter()
        .map(|name| text::width(name))
        .max()
        .unwrap_or(0)
        .min(NAME_COLUMN);
    let mut lines = Vec::with_capacity(entries.len());
    for (index, (entry, name)) in entries.iter().zip(&names).enumerate() {
        let joint = match index + 1 == entries.len() && !more {
            true => Glyph::TreeLast,
            false => Glyph::TreeMid,
        };
        let mut row = format!("{} ", theme.paint(Role::BorderMuted, theme.glyph(joint)));
        row.push_str(&icon(entry.name, entry.dir, theme));
        let room = width.saturating_sub(text::width(&row));
        let size = entry.size.map(format_size).unwrap_or_default();
        // The size is the first thing a narrow card gives up.
        let sized = !size.is_empty() && room >= column.max(text::width(name)) + 2 + size.len();
        let shown = text::truncate(name, room, ellipsis);
        if entry.dir {
            row.push_str(&format!("{BOLD}{}", theme.paint(Role::Accent, &shown)));
            row.push_str(RESET);
        } else if sized {
            row.push_str(&text::pad_right(&shown, column));
            row.push_str("  ");
            row.push_str(&theme.paint(Role::Muted, &size));
        } else {
            row.push_str(&shown);
        }
        lines.push(text::truncate(&row, width, ""));
    }
    lines
}

/// A row of a list of found files: the directory they are in, or one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found<'a> {
    /// A directory, without the `/` at its end; empty for the directory searched.
    Dir(&'a str),
    /// The name of a file in the directory above it.
    File(&'a str),
}

/// Paths gathered under the directory they are in: each directory once, where its first file
/// stood, then its files in the order they came.
pub fn by_directory<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<Found<'a>> {
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut at: HashMap<&str, usize> = HashMap::new();
    let mut count = 0;
    for path in paths {
        let (dir, name) = path.rsplit_once(['/', '\\']).unwrap_or(("", path));
        let index = *at.entry(dir).or_insert_with(|| {
            groups.push((dir, Vec::new()));
            groups.len() - 1
        });
        groups[index].1.push(name);
        count += 1;
    }
    let mut rows = Vec::with_capacity(count + groups.len());
    for (dir, names) in groups {
        rows.push(Found::Dir(dir));
        rows.extend(names.into_iter().map(Found::File));
    }
    rows
}

/// Rows of [`by_directory`]: a directory in bold accent, its files indented under it.
pub fn found(rows: &[Found<'_>], width: usize, theme: &Theme) -> Vec<Line> {
    let ellipsis = theme.glyph(Glyph::Ellipsis);
    rows.iter()
        .map(|row| match row {
            Found::Dir(dir) => {
                let name = match dir.is_empty() {
                    true => "./".to_string(),
                    false => format!("{}/", one_line(dir)),
                };
                let lead = icon(dir, true, theme);
                let room = width.saturating_sub(text::width(&lead));
                let shown = text::truncate(&name, room, ellipsis);
                format!("{lead}{BOLD}{}{RESET}", theme.paint(Role::Accent, &shown))
            }
            Found::File(name) => {
                let lead = format!("  {}", icon(name, false, theme));
                let room = width.saturating_sub(text::width(&lead));
                format!("{lead}{}", text::truncate(&one_line(name), room, ellipsis))
            }
        })
        .map(|row| text::truncate(&row, width, ""))
        .collect()
}

/// A row of search results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit<'a> {
    /// The file the rows after it are from.
    File(&'a str),
    /// A line that matches.
    Match { line: u64, text: &'a str },
    /// A line shown for what is around a match.
    Context { line: u64, text: &'a str },
    /// Lines were left out between the row above and the row below.
    Gap,
    /// Anything else the search said, shown as it is.
    Note(&'a str),
}

/// The text a search was for, to pick out in the lines it found. It is looked for as written:
/// a pattern is never compiled here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Needle<'a> {
    pub text: &'a str,
    pub ignore_case: bool,
}

/// `text` with every occurrence of the needle in bold warning colour.
fn mark(text: &str, needle: Needle<'_>, theme: &Theme) -> String {
    if needle.text.is_empty() {
        return text.to_string();
    }
    // ASCII case only, which keeps every byte where it was.
    let (hay, pin): (Cow<'_, str>, Cow<'_, str>) = match needle.ignore_case {
        true => (
            Cow::Owned(text.to_ascii_lowercase()),
            Cow::Owned(needle.text.to_ascii_lowercase()),
        ),
        false => (Cow::Borrowed(text), Cow::Borrowed(needle.text)),
    };
    let on = format!("{}{BOLD}", theme.fg(Role::Warning));
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (start, found) in hay.match_indices(&*pin) {
        out.push_str(&text[copied..start]);
        out.push_str(&on);
        copied = start + found.len();
        out.push_str(&text[start..copied]);
        out.push_str(RESET);
    }
    out.push_str(&text[copied..]);
    out
}

/// Search results: the name of each file in bold accent, under it the lines found in it behind
/// their numbers. What matches `needle` stands out in a matching line; lines of context are
/// muted.
pub fn hits(rows: &[Hit<'_>], needle: Option<Needle<'_>>, width: usize, theme: &Theme) -> Vec<Line> {
    let ellipsis = theme.glyph(Glyph::Ellipsis);
    let last = rows
        .iter()
        .filter_map(|row| match row {
            Hit::Match { line, .. } | Hit::Context { line, .. } => Some(*line),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let gutter = gutter(last);
    rows.iter()
        .map(|row| match row {
            Hit::File(path) => {
                let lead = icon(path, false, theme);
                let room = width.saturating_sub(text::width(&lead));
                let shown = text::truncate(&one_line(path), room, ellipsis);
                format!("{lead}{BOLD}{}{RESET}", theme.paint(Role::Accent, &shown))
            }
            Hit::Match { line, text } => {
                let clean = one_line(text);
                let content = match needle {
                    Some(needle) => mark(&clean, needle, theme),
                    None => clean,
                };
                numbered(Some(*line), gutter, &content, width, theme)
            }
            Hit::Context { line, text } => {
                let content = theme.paint(Role::Muted, &one_line(text));
                numbered(Some(*line), gutter, &content, width, theme)
            }
            Hit::Gap => theme.paint(Role::Muted, &format!("{ellipsis:>gutter$}")),
            Hit::Note(note) => theme.paint(Role::Muted, &text::truncate(&one_line(note), width, ellipsis)),
        })
        .map(|row| text::truncate(&row, width, ""))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEVELS: [GlyphLevel; 3] = [GlyphLevel::Unicode, GlyphLevel::Nerd, GlyphLevel::Ascii];

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|line| text::strip_ansi(line)).collect()
    }

    /// Every row fits the width, and at `ascii` nothing but ASCII is drawn around ASCII text.
    fn fits(what: &str, level: GlyphLevel, width: usize, lines: &[Line]) {
        for line in lines {
            assert!(
                text::width(line) <= width,
                "{what} at {level}, width {width}: {line:?}"
            );
            if level == GlyphLevel::Ascii {
                assert!(
                    text::strip_ansi(line).is_ascii(),
                    "{what}, width {width}: {line:?}"
                );
            }
        }
    }

    const SOURCE: &str = "// The entry point.\nfn main() {\n\tlet answer = \"forty-two\";\n}";

    #[test]
    fn a_listing_numbers_its_lines_from_the_first_and_colours_known_languages() {
        let theme = Theme::dark();
        let lines = listing(SOURCE, "src/main.rs", 9, 60, &theme);
        assert_eq!(
            plain(&lines),
            [
                "  9 │ // The entry point.",
                " 10 │ fn main() {",
                " 11 │     let answer = \"forty-two\";",
                " 12 │ }",
            ]
        );
        // The number is muted, the bar takes the colour of a border, the code its own colours.
        let number = theme.paint(Role::Muted, "  9");
        let bar = theme.paint(Role::Border, "│");
        assert!(
            lines[0].starts_with(&format!("{number} {bar} \x1b[38;2;")),
            "{:?}",
            lines[0]
        );
        let keyword = lines[1].split("fn").next().unwrap_or_default();
        let name = lines[1].split("main").next().unwrap_or_default();
        assert_ne!(
            keyword.rsplit('\x1b').next(),
            name.rsplit('\x1b').next(),
            "a keyword and a name differ in colour: {:?}",
            lines[1]
        );
        for line in &lines {
            assert!(line.ends_with(RESET), "{line:?}");
        }

        // The same text under a name that says nothing is numbered and left as it is.
        let unknown = listing(SOURCE, "notes.unknown", 9, 60, &theme);
        assert_eq!(plain(&unknown), plain(&lines));
        assert_eq!(
            unknown[1],
            format!("{} {bar} fn main() {{", theme.paint(Role::Muted, " 10"))
        );
    }

    #[test]
    fn the_number_column_grows_with_the_numbers_and_long_lines_are_cut() {
        let theme = Theme::dark();
        let lines = listing("a\nb", "x.txt", 9999, 40, &theme);
        assert_eq!(plain(&lines), [" 9999 │ a", "10000 │ b"]);
        let long = "x".repeat(80);
        assert_eq!(
            plain(&listing(&long, "x.txt", 1, 16, &theme)),
            [format!("  1 │ {}…", "x".repeat(9))]
        );
        // Escape sequences in a file are dropped, not passed to the terminal.
        let shown = listing("a\x1b[31mred\x1b[0m\x07", "x.txt", 1, 40, &theme);
        assert_eq!(plain(&shown), ["  1 │ ared"]);
        assert!(!shown[0].contains("\x1b[31m"));
    }

    #[test]
    fn text_above_the_limit_is_numbered_without_colours() {
        let theme = Theme::dark();
        let big = format!(
            "fn main() {{}}\n{}",
            "// filler\n".repeat(MAX_HIGHLIGHT_BYTES / 10)
        );
        assert!(big.len() > MAX_HIGHLIGHT_BYTES);
        let lines = listing(&big, "big.rs", 1, 40, &theme);
        let bar = theme.paint(Role::Border, "│");
        assert!(
            lines[0].ends_with(&format!("{bar} fn main() {{}}")),
            "{:?}",
            lines[0]
        );
    }

    #[test]
    fn a_tree_marks_directories_and_lines_up_sizes() {
        let entries = [
            Entry {
                name: "src",
                dir: true,
                size: None,
            },
            Entry {
                name: "Cargo.toml",
                dir: false,
                size: Some(1234),
            },
            Entry {
                name: "a.rs",
                dir: false,
                size: Some(5 * 1024 * 1024),
            },
        ];
        let theme = Theme::dark();
        let lines = tree(&entries, false, 40, &theme);
        assert_eq!(
            plain(&lines),
            ["├─ src/", "├─ Cargo.toml  1.2 KB", "└─ a.rs        5.0 MB"]
        );
        let accent = format!("{BOLD}{}", theme.fg(Role::Accent));
        assert!(lines[0].contains(&format!("{accent}src/")), "{:?}", lines[0]);
        assert!(lines[0].starts_with(&theme.fg(Role::BorderMuted)));
        // More entries follow what is shown: the tree stays open.
        assert_eq!(
            plain(&tree(&entries[..2], true, 40, &theme))[1],
            "├─ Cargo.toml  1.2 KB"
        );
        // No room for the size: the name alone, cut when it has to be.
        assert_eq!(plain(&tree(&entries[1..2], false, 12, &theme)), ["└─ Cargo.to…"]);

        let ascii = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        assert_eq!(
            plain(&tree(&entries, false, 40, &ascii)),
            ["|- src/", "|- Cargo.toml  1.2 KB", "`- a.rs        5.0 MB"]
        );
    }

    #[test]
    fn icons_need_a_nerd_font_and_take_their_colour_from_the_theme() {
        let nerd = Theme::dark().with_glyphs(GlyphLevel::Nerd);
        assert_eq!(icon("main.rs", false, &Theme::dark()), "");
        assert_eq!(
            icon("src/main.RS", false, &nerd),
            format!("{} ", nerd.paint(Role::Warning, "\u{e7a8}"))
        );
        assert_eq!(
            icon("Dockerfile", false, &nerd),
            format!("{} ", nerd.paint(Role::Bash, "\u{f308}"))
        );
        assert_eq!(
            icon(".gitignore", false, &nerd),
            format!("{} ", nerd.paint(Role::Error, "\u{f1d3}"))
        );
        assert_eq!(
            icon("data.unknown", false, &nerd),
            format!("{} ", nerd.paint(Role::Muted, nerd.glyph(Glyph::File)))
        );
        assert_eq!(
            icon("rs", true, &nerd),
            format!("{} ", nerd.paint(Role::Accent, nerd.glyph(Glyph::Folder)))
        );
        let rows = plain(&tree(
            &[Entry {
                name: "lib.rs",
                dir: false,
                size: Some(9),
            }],
            false,
            40,
            &nerd,
        ));
        assert_eq!(rows, ["└─ \u{e7a8} lib.rs  9 B"]);
        // One cell each, and no kind listed twice.
        let mut kinds: Vec<&str> = Vec::new();
        for (names, symbol, _) in FILE_ICONS {
            assert_eq!(text::width(symbol), 1, "{names:?}");
            kinds.extend(names);
        }
        let listed = kinds.len();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), listed);
    }

    #[test]
    fn found_files_stand_under_their_directory() {
        let paths = [
            "src/tui/mod.rs",
            "Cargo.toml",
            "src/main.rs",
            "src/tui/view.rs",
            "README.md",
        ];
        let rows = by_directory(paths);
        assert_eq!(
            rows,
            [
                Found::Dir("src/tui"),
                Found::File("mod.rs"),
                Found::File("view.rs"),
                Found::Dir(""),
                Found::File("Cargo.toml"),
                Found::File("README.md"),
                Found::Dir("src"),
                Found::File("main.rs"),
            ]
        );
        let theme = Theme::dark();
        let lines = found(&rows[..5], 40, &theme);
        assert_eq!(
            plain(&lines),
            ["src/tui/", "  mod.rs", "  view.rs", "./", "  Cargo.toml"]
        );
        assert!(lines[0].starts_with(&format!("{BOLD}{}", theme.fg(Role::Accent))));
        let nerd = Theme::dark().with_glyphs(GlyphLevel::Nerd);
        assert_eq!(
            plain(&found(&rows[..2], 40, &nerd)),
            ["\u{f115} src/tui/", "  \u{e7a8} mod.rs"]
        );
    }

    fn sample_hits() -> Vec<Hit<'static>> {
        vec![
            Hit::File("src/main.rs"),
            Hit::Context {
                line: 8,
                text: "// starts here",
            },
            Hit::Match {
                line: 9,
                text: "fn main() { Main::run(main) }",
            },
            Hit::Gap,
            Hit::Match {
                line: 120,
                text: "\tmain_loop();",
            },
            Hit::File("src/lib.rs"),
            Hit::Match {
                line: 3,
                text: "pub fn main_loop() {}",
            },
            Hit::Note("[limit of 3 reached; narrow the search or raise limit]"),
        ]
    }

    #[test]
    fn hits_stand_under_their_file_with_the_match_picked_out() {
        let theme = Theme::dark();
        let needle = Needle {
            text: "main",
            ignore_case: false,
        };
        let lines = hits(&sample_hits(), Some(needle), 60, &theme);
        assert_eq!(
            plain(&lines),
            [
                "src/main.rs",
                "  8 │ // starts here",
                "  9 │ fn main() { Main::run(main) }",
                "  …",
                "120 │     main_loop();",
                "src/lib.rs",
                "  3 │ pub fn main_loop() {}",
                "[limit of 3 reached; narrow the search or raise limit]",
            ]
        );
        let marked = format!("{}{BOLD}main{RESET}", theme.fg(Role::Warning));
        assert_eq!(lines[2].matches(&marked).count(), 2, "{:?}", lines[2]);
        assert!(lines[2].contains("Main::run"), "the other case is left alone");
        assert!(lines[4].contains(&format!("{marked}_loop")), "{:?}", lines[4]);
        // Context is muted and never marked, though it may hold the text too.
        assert!(!lines[1].contains(BOLD));
        assert!(lines[1].ends_with(&theme.paint(Role::Muted, "// starts here")));
        assert!(lines[0].starts_with(&format!("{BOLD}{}", theme.fg(Role::Accent))));

        let any_case = Needle {
            text: "MAIN",
            ignore_case: true,
        };
        let lines = hits(&sample_hits(), Some(any_case), 60, &theme);
        assert_eq!(lines[2].matches(BOLD).count(), 3, "{:?}", lines[2]);
        // Without a needle, and with one that is not in the line, nothing is marked.
        let absent = Needle {
            text: "fn \\w+",
            ignore_case: false,
        };
        for needle in [None, Some(absent)] {
            let lines = hits(&sample_hits(), needle, 60, &theme);
            assert!(!lines[2].contains(BOLD), "{:?}", lines[2]);
        }
    }

    #[test]
    fn every_row_fits_every_width_at_every_glyph_level() {
        let entries = [
            Entry {
                name: "a_directory_with_a_rather_long_name",
                dir: true,
                size: None,
            },
            Entry {
                name: "Cargo.toml",
                dir: false,
                size: Some(123_456),
            },
        ];
        let needle = Needle {
            text: "main",
            ignore_case: true,
        };
        let paths = [
            "src/tui/mod.rs",
            "Cargo.toml",
            "a/rather/deep/directory/for/a/file.rs",
        ];
        for level in LEVELS {
            let theme = Theme::dark().with_glyphs(level);
            for width in 1..=160 {
                fits(
                    "listing",
                    level,
                    width,
                    &listing(SOURCE, "a.rs", 98, width, &theme),
                );
                fits(
                    "plain listing",
                    level,
                    width,
                    &listing(SOURCE, "a", 1, width, &theme),
                );
                fits("tree", level, width, &tree(&entries, false, width, &theme));
                fits("found", level, width, &found(&by_directory(paths), width, &theme));
                fits(
                    "hits",
                    level,
                    width,
                    &hits(&sample_hits(), Some(needle), width, &theme),
                );
            }
        }
    }
}
