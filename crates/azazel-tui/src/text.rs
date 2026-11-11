//! ANSI-aware text measurement, wrapping and truncation shared by every component.
//!
//! Strings handled here may contain escape sequences (SGR colour codes, OSC hyperlinks). They
//! take no columns, are never split, and SGR state is tracked so that every produced line is
//! self-contained: it re-opens the styles active at its start and resets them at its end.

use unicode_segmentation::{Graphemes, UnicodeSegmentation};
use unicode_width::UnicodeWidthStr;

const ESC: char = '\x1b';
const SGR_RESET: &str = "\x1b[0m";
const TAB_WIDTH: usize = 4;

pub(crate) enum Segment<'a> {
    /// One complete escape sequence.
    Escape(&'a str),
    /// One grapheme cluster of visible (or control) text.
    Grapheme(&'a str),
}

pub(crate) struct Segments<'a> {
    rest: &'a str,
    run: Graphemes<'a>,
}

/// Splits a string into escape sequences and grapheme clusters, in order.
pub(crate) fn segments(s: &str) -> Segments<'_> {
    Segments {
        rest: s,
        run: "".graphemes(true),
    }
}

impl<'a> Iterator for Segments<'a> {
    type Item = Segment<'a>;

    fn next(&mut self) -> Option<Segment<'a>> {
        if let Some(g) = self.run.next() {
            return Some(Segment::Grapheme(g));
        }
        if self.rest.is_empty() {
            return None;
        }
        if self.rest.starts_with(ESC) {
            let (seq, rest) = self.rest.split_at(escape_len(self.rest));
            self.rest = rest;
            return Some(Segment::Escape(seq));
        }
        let end = self.rest.find(ESC).unwrap_or(self.rest.len());
        let (run, rest) = self.rest.split_at(end);
        self.rest = rest;
        self.run = run.graphemes(true);
        self.run.next().map(Segment::Grapheme)
    }
}

/// Byte length of the escape sequence at the start of `s` (which begins with ESC). Unterminated
/// sequences end where the next sequence starts or at the end of the string.
fn escape_len(s: &str) -> usize {
    let b = s.as_bytes();
    match b.get(1) {
        None => 1,
        Some(b'[') => {
            let mut i = 2;
            while i < b.len() && (0x20..=0x3f).contains(&b[i]) {
                i += 1;
            }
            if i < b.len() && (0x40..=0x7e).contains(&b[i]) {
                i + 1
            } else {
                i
            }
        }
        Some(b']' | b'P' | b'X' | b'^' | b'_') => {
            let mut i = 2;
            while i < b.len() {
                match b[i] {
                    0x07 => return i + 1,
                    0x1b => return if b.get(i + 1) == Some(&b'\\') { i + 2 } else { i },
                    _ => i += 1,
                }
            }
            b.len()
        }
        Some(_) => {
            let mut i = 1;
            while i < b.len() && (0x20..=0x2f).contains(&b[i]) {
                i += 1;
            }
            if i < b.len() && (0x30..=0x7e).contains(&b[i]) {
                i + 1
            } else {
                i
            }
        }
    }
}

fn is_control(g: &str) -> bool {
    g.chars().next().is_some_and(char::is_control)
}

/// Display columns of one grapheme cluster. Control characters take none.
pub(crate) fn grapheme_width(g: &str) -> usize {
    match g.as_bytes() {
        [] => 0,
        [b] => usize::from((0x20..0x7f).contains(b)),
        _ if is_control(g) => 0,
        _ => UnicodeWidthStr::width(g),
    }
}

/// Display columns of `s`, ignoring escape sequences. Wide characters count two columns, combining
/// marks and other zero-width characters none.
pub fn width(s: &str) -> usize {
    if s.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return s.len();
    }
    segments(s)
        .map(|seg| match seg {
            Segment::Grapheme(g) => grapheme_width(g),
            Segment::Escape(_) => 0,
        })
        .sum()
}

/// Removes every escape sequence, keeping the text.
pub fn strip_ansi(s: &str) -> String {
    if !s.contains(ESC) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for seg in segments(s) {
        if let Segment::Grapheme(g) = seg {
            out.push_str(g);
        }
    }
    out
}

/// Makes untrusted text (model output, program output) safe to place in a line: escape sequences
/// and control characters are removed, tabs expand to the next multiple of four columns and
/// `\r\n` becomes `\n`. Newlines are kept.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut col = 0usize;
    for seg in segments(s) {
        let Segment::Grapheme(g) = seg else { continue };
        match g {
            "\n" | "\r\n" => {
                out.push('\n');
                col = 0;
            }
            "\t" => {
                let n = TAB_WIDTH - col % TAB_WIDTH;
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            _ if is_control(g) => {}
            _ => {
                out.push_str(g);
                col += grapheme_width(g);
            }
        }
    }
    out
}

/// The SGR attributes active at a point in a string.
#[derive(Clone, Default, PartialEq, Eq)]
struct SgrState {
    fg: Option<String>,
    bg: Option<String>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
    strike: bool,
}

impl SgrState {
    fn apply(&mut self, seq: &str) {
        let Some(body) = seq.strip_prefix("\x1b[").and_then(|s| s.strip_suffix('m')) else {
            return;
        };
        if body.starts_with(['?', '>', '<', '=']) {
            return;
        }
        let mut params = body.split(';');
        while let Some(p) = params.next() {
            match p {
                "" | "0" => *self = Self::default(),
                "1" => self.bold = true,
                "2" => self.dim = true,
                "3" => self.italic = true,
                "4" => self.underline = true,
                "7" => self.inverse = true,
                "9" => self.strike = true,
                "22" => {
                    self.bold = false;
                    self.dim = false;
                }
                "23" => self.italic = false,
                "24" => self.underline = false,
                "27" => self.inverse = false,
                "29" => self.strike = false,
                "39" => self.fg = None,
                "49" => self.bg = None,
                "38" | "48" => {
                    let Some(mode) = params.next() else { return };
                    let count = match mode {
                        "5" => 1,
                        "2" => 3,
                        _ => continue,
                    };
                    let mut color = format!("{p};{mode}");
                    for _ in 0..count {
                        let Some(value) = params.next() else { return };
                        color.push(';');
                        color.push_str(value);
                    }
                    if p == "38" {
                        self.fg = Some(color);
                    } else {
                        self.bg = Some(color);
                    }
                }
                _ => match p.parse::<u8>() {
                    Ok(30..=37 | 90..=97) => self.fg = Some(p.to_string()),
                    Ok(40..=47 | 100..=107) => self.bg = Some(p.to_string()),
                    Ok(_) => {}
                    // Colon-separated colour forms such as `38:2::r:g:b`.
                    Err(_) if p.starts_with("38:") => self.fg = Some(p.to_string()),
                    Err(_) if p.starts_with("48:") => self.bg = Some(p.to_string()),
                    Err(_) => {}
                },
            }
        }
    }

    /// One SGR sequence that re-creates this state from the default, or nothing when default.
    fn prefix(&self) -> String {
        let mut params: Vec<&str> = Vec::new();
        for (on, code) in [
            (self.bold, "1"),
            (self.dim, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
            (self.inverse, "7"),
            (self.strike, "9"),
        ] {
            if on {
                params.push(code);
            }
        }
        if let Some(fg) = &self.fg {
            params.push(fg);
        }
        if let Some(bg) = &self.bg {
            params.push(bg);
        }
        if params.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", params.join(";"))
        }
    }
}

fn close_line(mut line: String) -> String {
    if line.contains(ESC) && !line.ends_with(SGR_RESET) {
        line.push_str(SGR_RESET);
    }
    line
}

struct Wrapper {
    width: usize,
    lines: Vec<String>,
    /// SGR state after everything consumed so far.
    state: SgrState,
    /// SGR state at the start of the current logical (newline-delimited) line.
    line_state: SgrState,
    cur: String,
    cur_w: usize,
    cur_has: bool,
    /// Whitespace (and escape sequences) seen since the last word; dropped at a wrap point.
    space: String,
    space_w: usize,
    word: String,
    word_w: usize,
    /// SGR state just before the first grapheme of `word`.
    word_state: SgrState,
}

impl Wrapper {
    fn new(width: usize) -> Self {
        Self {
            width,
            lines: Vec::new(),
            state: SgrState::default(),
            line_state: SgrState::default(),
            cur: String::new(),
            cur_w: 0,
            cur_has: false,
            space: String::new(),
            space_w: 0,
            word: String::new(),
            word_w: 0,
            word_state: SgrState::default(),
        }
    }

    fn escape(&mut self, seq: &str) {
        self.state.apply(seq);
        if self.word.is_empty() {
            self.space.push_str(seq);
        } else {
            self.word.push_str(seq);
        }
    }

    fn grapheme(&mut self, g: &str) {
        if g == " " || g == "\t" {
            self.end_word();
            self.space.push(' ');
            self.space_w += 1;
            return;
        }
        if is_control(g) {
            return;
        }
        let w = grapheme_width(g);
        if w > self.width {
            return;
        }
        if self.word_w + w > self.width {
            // The word alone is wider than a line: place what there is and continue on the next.
            self.end_word();
        }
        if self.word.is_empty() {
            self.word_state = self.state.clone();
        }
        self.word.push_str(g);
        self.word_w += w;
    }

    fn end_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        if !self.cur_has {
            // First word of a logical line: leading whitespace is indentation and is kept when it
            // fits beside the word.
            if self.space_w + self.word_w <= self.width {
                self.cur = self.line_state.prefix();
                self.cur.push_str(&self.space);
                self.cur_w = self.space_w;
            } else {
                self.cur = self.word_state.prefix();
            }
        } else if self.cur_w + self.space_w + self.word_w <= self.width {
            self.cur.push_str(&self.space);
            self.cur_w += self.space_w;
        } else {
            self.push_line();
            self.cur = self.word_state.prefix();
        }
        self.cur.push_str(&self.word);
        self.cur_w += self.word_w;
        self.cur_has = true;
        self.space.clear();
        self.space_w = 0;
        self.word.clear();
        self.word_w = 0;
    }

    fn push_line(&mut self) {
        self.lines.push(close_line(std::mem::take(&mut self.cur)));
        self.cur_w = 0;
        self.cur_has = false;
    }

    fn newline(&mut self) {
        self.end_word();
        self.push_line();
        self.space.clear();
        self.space_w = 0;
        self.line_state = self.state.clone();
    }
}

/// Word-wraps `s` to `width` columns. Newlines start a new line; words longer than a line are
/// broken on grapheme boundaries; whitespace at a wrap point is dropped. SGR state is carried
/// onto continuation lines and reset at the end of every line that contains styling. A character
/// wider than `width` cannot be placed and is dropped.
pub fn wrap(s: &str, width: usize) -> Vec<String> {
    let mut wrapper = Wrapper::new(width.max(1));
    for seg in segments(s) {
        match seg {
            Segment::Escape(seq) => wrapper.escape(seq),
            Segment::Grapheme("\n" | "\r\n") => wrapper.newline(),
            Segment::Grapheme(g) => wrapper.grapheme(g),
        }
    }
    wrapper.newline();
    wrapper.lines
}

/// Breaks `s` into lines of at most `width` columns on grapheme boundaries, keeping all
/// whitespace (for code and diffs). SGR state is carried and reset as in [`wrap`].
pub fn wrap_hard(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut state = SgrState::default();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for seg in segments(s) {
        match seg {
            Segment::Escape(seq) => {
                state.apply(seq);
                cur.push_str(seq);
            }
            Segment::Grapheme("\n" | "\r\n") => {
                lines.push(close_line(std::mem::replace(&mut cur, state.prefix())));
                cur_w = 0;
            }
            Segment::Grapheme(g) => {
                let g = if g == "\t" { " " } else { g };
                if is_control(g) {
                    continue;
                }
                let w = grapheme_width(g);
                if w > width {
                    continue;
                }
                if cur_w + w > width {
                    lines.push(close_line(std::mem::replace(&mut cur, state.prefix())));
                    cur_w = 0;
                }
                cur.push_str(g);
                cur_w += w;
            }
        }
    }
    lines.push(close_line(cur));
    lines
}

/// Cuts `s` to at most `max` columns, appending `ellipsis` when something was removed. Escape
/// sequences before the cut are kept and styling is reset at the end.
pub fn truncate(s: &str, max: usize, ellipsis: &str) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    let ellipsis_w = width(ellipsis);
    let (ellipsis, ellipsis_w) = if ellipsis_w > max {
        ("", 0)
    } else {
        (ellipsis, ellipsis_w)
    };
    let budget = max - ellipsis_w;
    let mut out = String::with_capacity(s.len());
    let mut used = 0usize;
    for seg in segments(s) {
        match seg {
            Segment::Escape(seq) => out.push_str(seq),
            Segment::Grapheme(g) => {
                let w = grapheme_width(g);
                if used + w > budget {
                    break;
                }
                if !is_control(g) {
                    out.push_str(g);
                    used += w;
                }
            }
        }
    }
    out.push_str(ellipsis);
    close_line(out)
}

/// Appends spaces until `s` is `target` columns wide. Longer strings are returned unchanged.
pub fn pad_right(s: &str, target: usize) -> String {
    let missing = target.saturating_sub(width(s));
    let mut out = String::with_capacity(s.len() + missing);
    out.push_str(s);
    out.extend(std::iter::repeat_n(' ', missing));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_ignores_escape_sequences() {
        assert_eq!(width("plain"), 5);
        assert_eq!(width("\x1b[1;38;2;10;20;30mbold\x1b[0m"), 4);
        assert_eq!(width("\x1b]8;;https://example.com\x07link\x1b]8;;\x07"), 4);
        assert_eq!(width("\x1b]0;title\x1b\\x"), 1);
        assert_eq!(width(""), 0);
    }

    #[test]
    fn width_of_wide_and_zero_width_characters() {
        assert_eq!(width("日本語"), 6);
        assert_eq!(width("a日b"), 4);
        assert_eq!(width("👍"), 2);
        assert_eq!(width("👍🏽"), 2);
        assert_eq!(width("🧑‍🤝‍🧑"), 2);
        assert_eq!(width("e\u{301}"), 1);
        assert_eq!(width("cafe\u{301}"), 4);
        assert_eq!(width("a\u{200b}b"), 2);
        assert_eq!(width("a\tb\r"), 2);
    }

    #[test]
    fn unterminated_escape_does_not_swallow_following_sequences() {
        assert_eq!(strip_ansi("\x1b]0;title\x1b[31mred"), "red");
        assert_eq!(strip_ansi("text\x1b"), "text");
        assert_eq!(strip_ansi("text\x1b["), "text");
    }

    #[test]
    fn strip_ansi_keeps_only_text() {
        assert_eq!(
            strip_ansi("\x1b[31mred\x1b[0m and \x1b[4mline\x1b[24m"),
            "red and line"
        );
        assert_eq!(strip_ansi("no escapes"), "no escapes");
    }

    #[test]
    fn sanitize_expands_tabs_and_drops_controls() {
        assert_eq!(sanitize("a\tb"), "a   b");
        assert_eq!(sanitize("\tx"), "    x");
        assert_eq!(sanitize("one\r\ntwo\rthree"), "one\ntwothree");
        assert_eq!(sanitize("\x1b[31mred\x1b[0m\x07"), "red");
        assert_eq!(sanitize("ab\n\tc"), "ab\n    c");
    }

    #[test]
    fn wrap_breaks_at_word_boundaries() {
        assert_eq!(wrap("the quick brown fox", 10), ["the quick", "brown fox"]);
        assert_eq!(wrap("exactly ten", 11), ["exactly ten"]);
        assert_eq!(wrap("a  b", 10), ["a  b"]);
        assert_eq!(wrap("trailing   ", 10), ["trailing"]);
    }

    #[test]
    fn wrap_keeps_newlines_and_indentation() {
        assert_eq!(wrap("a\n\nb", 10), ["a", "", "b"]);
        assert_eq!(wrap("  indented text", 20), ["  indented text"]);
        assert_eq!(wrap("", 10), [""]);
        assert_eq!(wrap("one\r\ntwo", 10), ["one", "two"]);
    }

    #[test]
    fn wrap_breaks_long_words_on_grapheme_boundaries() {
        assert_eq!(wrap("abcdefghijkl", 5), ["abcde", "fghij", "kl"]);
        assert_eq!(wrap("hi abcdefghijkl", 5), ["hi", "abcde", "fghij", "kl"]);
        assert_eq!(wrap("日本語テキスト", 6), ["日本語", "テキス", "ト"]);
        // A wide character never straddles the edge: five columns hold two of them.
        assert_eq!(wrap("日本語", 5), ["日本", "語"]);
        assert_eq!(wrap("cafe\u{301} au lait", 4), ["cafe\u{301}", "au", "lait"]);
    }

    #[test]
    fn wrap_carries_sgr_state_and_resets_each_line() {
        let lines = wrap("\x1b[1mbold text here\x1b[0m", 9);
        assert_eq!(lines, ["\x1b[1mbold text\x1b[0m", "\x1b[1mhere\x1b[0m"]);

        let lines = wrap("\x1b[38;5;196mred words that wrap\x1b[0m plain", 9);
        assert_eq!(lines.len(), 3);
        assert_eq!(strip_ansi(&lines[0]), "red words");
        assert_eq!(strip_ansi(&lines[1]), "that wrap");
        assert_eq!(strip_ansi(&lines[2]), "plain");
        assert!(lines[1].starts_with("\x1b[38;5;196m"));
        assert!(lines[0].ends_with("\x1b[0m") && lines[1].ends_with("\x1b[0m"));
        assert!(!lines[2].contains("38;5;196"));
        for line in &lines {
            assert!(width(line) <= 9);
        }
    }

    #[test]
    fn wrap_carries_style_across_newlines() {
        let lines = wrap("\x1b[3mone\ntwo\x1b[23m", 10);
        assert_eq!(lines, ["\x1b[3mone\x1b[0m", "\x1b[3mtwo\x1b[23m\x1b[0m"]);
    }

    #[test]
    fn wrap_never_exceeds_width_with_mixed_content() {
        let source = "\x1b[1mwide 日本語 text\x1b[0m with emoji 👍🏽 and a_very_long_identifier_name";
        for w in 1..30 {
            for line in wrap(source, w) {
                assert!(width(&line) <= w, "width {w}: {line:?}");
            }
        }
    }

    #[test]
    fn wrap_hard_preserves_whitespace() {
        assert_eq!(wrap_hard("ab  cd  ef", 4), ["ab  ", "cd  ", "ef"]);
        assert_eq!(wrap_hard("a\nb", 4), ["a", "b"]);
        assert_eq!(wrap_hard("日本語", 5), ["日本", "語"]);
        assert_eq!(wrap_hard("cafe\u{301}s", 4), ["cafe\u{301}", "s"]);
        let lines = wrap_hard("\x1b[32m+added line\x1b[0m", 6);
        assert_eq!(lines, ["\x1b[32m+added\x1b[0m", "\x1b[32m line\x1b[0m"]);
    }

    #[test]
    fn truncate_adds_ellipsis_only_when_cutting() {
        assert_eq!(truncate("short", 10, "…"), "short");
        assert_eq!(truncate("hello world", 8, "…"), "hello w…");
        assert_eq!(truncate("hello", 0, "…"), "");
        assert_eq!(truncate("hello", 3, ""), "hel");
    }

    #[test]
    fn truncate_respects_wide_characters_and_clusters() {
        assert_eq!(truncate("日本語", 5, "…"), "日本…");
        // Three columns remain after the ellipsis; a second wide character would not fit.
        assert_eq!(truncate("日本語", 4, "…"), "日…");
        assert_eq!(truncate("cafe\u{301}s", 4, ""), "cafe\u{301}");
        assert_eq!(truncate("👍🏽👍🏽👍🏽", 4, ""), "👍🏽👍🏽");
    }

    #[test]
    fn truncate_keeps_style_and_resets() {
        let cut = truncate("\x1b[31mhello world\x1b[0m", 5, "…");
        assert_eq!(cut, "\x1b[31mhell…\x1b[0m");
        assert_eq!(width(&cut), 5);
    }

    #[test]
    fn pad_right_fills_to_width() {
        assert_eq!(pad_right("ab", 5), "ab   ");
        assert_eq!(pad_right("日本", 5), "日本 ");
        assert_eq!(pad_right("\x1b[1mab\x1b[0m", 4), "\x1b[1mab\x1b[0m  ");
        assert_eq!(pad_right("too long", 3), "too long");
    }
}
