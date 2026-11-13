//! A full-screen, read-only view over rendered items: a transcript, a diff, a changelog.
//!
//! The pager holds the items, a cursor row, a search and a line selection, and renders one
//! screen of rows. It never touches the terminal: the caller owns the alternate screen, feeds it
//! keys and acts on the [`Outcome`]s that ask for a copy or for a link to be opened.

use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::markdown;
use crate::render::Line;
use crate::text;
use crate::theme::{Role, Theme};

/// Columns the pager keeps left of every row for the cursor and selection marks.
const GUTTER: usize = 2;
/// Rows one notch of the mouse wheel moves.
pub const WHEEL_ROWS: isize = 3;

/// A fenced code block of an item: its rows within the item and the code itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeBlock {
    pub rows: Range<usize>,
    pub code: String,
}

/// One thing shown: a prompt, a reply, a tool call, one file of a diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Item {
    /// Rendered rows, no wider than [`content_width`].
    pub lines: Vec<Line>,
    /// What `y` copies for this item, as Markdown.
    pub source: String,
    /// `[` and `]` stop here.
    pub anchor: bool,
    pub blocks: Vec<CodeBlock>,
}

/// What the caller does after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Close,
    /// Put `text` on the clipboard; `what` names it for the confirmation (`the message`).
    Copy {
        text: String,
        what: &'static str,
    },
    /// Open this link.
    Open(String),
}

/// The width items are rendered at for a screen `width` columns wide.
pub fn content_width(width: usize) -> usize {
    width.saturating_sub(GUTTER).max(1)
}

/// A piece of a Markdown document: prose, or one fenced code block with its fences.
enum Piece<'a> {
    Prose(&'a str),
    Code { fenced: &'a str, code: &'a str },
}

/// The fence a line opens: its character and length. Fences indented by four or more columns
/// are code already and open nothing.
fn fence(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let mark = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let length = trimmed.chars().take_while(|c| *c == mark).count();
    (length >= 3).then_some((mark, length))
}

/// Splits a Markdown document at its top-level fenced code blocks. An unterminated fence runs to
/// the end, as it does when rendered.
fn pieces(source: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut prose_from = 0;
    let mut open: Option<(char, usize, usize, usize)> = None;
    let mut at = 0;
    for line in source.split_inclusive('\n') {
        let (start, end) = (at, at + line.len());
        at = end;
        let bare = line.trim_end_matches(['\n', '\r']);
        match open {
            None => {
                if let Some((mark, length)) = fence(bare) {
                    if prose_from < start {
                        out.push(Piece::Prose(&source[prose_from..start]));
                    }
                    open = Some((mark, length, start, end));
                }
            }
            Some((mark, length, block, code_from)) => {
                let closes = fence(bare).is_some_and(|(m, n)| m == mark && n >= length)
                    && bare
                        .trim_start_matches(' ')
                        .trim_start_matches(mark)
                        .trim()
                        .is_empty();
                if closes {
                    out.push(Piece::Code {
                        fenced: &source[block..end],
                        code: &source[code_from..start],
                    });
                    open = None;
                    prose_from = end;
                }
            }
        }
    }
    match open {
        Some((_, _, block, code_from)) => out.push(Piece::Code {
            fenced: &source[block..],
            code: &source[code_from.min(source.len())..],
        }),
        None if prose_from < source.len() => out.push(Piece::Prose(&source[prose_from..])),
        None => {}
    }
    out
}

/// The fenced code blocks of a Markdown document, in order, without their fences.
pub fn code_blocks(source: &str) -> Vec<String> {
    fenced_blocks(source).into_iter().map(|(_, code)| code).collect()
}

/// The fenced code blocks of a Markdown document, in order, as `(language, code)`.
pub fn fenced_blocks(source: &str) -> Vec<(String, String)> {
    pieces(source)
        .into_iter()
        .filter_map(|piece| match piece {
            Piece::Code { fenced, code } => {
                let info = fenced
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim_start()
                    .trim_start_matches(['`', '~']);
                let language = info.split([' ', ',', '\t']).next().unwrap_or_default();
                Some((
                    language.to_string(),
                    code.strip_suffix('\n').unwrap_or(code).to_string(),
                ))
            }
            Piece::Prose(_) => None,
        })
        .collect()
}

/// Renders Markdown as an item that knows where its code blocks are, so that one can be copied
/// on its own.
pub fn markdown_item(source: &str, width: usize, theme: &Theme) -> Item {
    let mut item = Item {
        source: source.trim().to_string(),
        ..Item::default()
    };
    for piece in pieces(source) {
        let (rendered, code) = match piece {
            Piece::Prose(prose) if prose.trim().is_empty() => continue,
            Piece::Prose(prose) => (markdown::render(prose, width, theme), None),
            Piece::Code { fenced, code } => (markdown::render(fenced, width, theme), Some(code)),
        };
        if rendered.is_empty() {
            continue;
        }
        if !item.lines.is_empty() {
            item.lines.push(String::new());
        }
        let first = item.lines.len();
        item.lines.extend(rendered);
        if let Some(code) = code {
            item.blocks.push(CodeBlock {
                rows: first..item.lines.len(),
                code: code.strip_suffix('\n').unwrap_or(code).to_string(),
            });
        }
    }
    item
}

/// The first link in a row of text: `http://`, `https://` or `file://` up to the next space or
/// closing bracket.
pub fn link_in(line: &str) -> Option<String> {
    let plain = text::strip_ansi(line);
    text::find_link(&plain).map(|range| plain[range].to_string())
}

pub struct Pager {
    title: String,
    items: Vec<Item>,
    /// `(item, row within the item)` of every row, in order.
    rows: Vec<(usize, usize)>,
    cursor: usize,
    top: usize,
    /// Content rows of the last render, for paging.
    page: usize,
    /// The query being typed after `/`.
    typing: Option<String>,
    /// The committed query, lower-case.
    search: Option<String>,
    /// The row a line selection started on.
    selection: Option<usize>,
    notice: Option<String>,
}

impl Pager {
    /// Opens on the last row, where a transcript is newest.
    pub fn new(title: impl Into<String>, items: Vec<Item>) -> Pager {
        let mut pager = Pager {
            title: title.into(),
            items: Vec::new(),
            rows: Vec::new(),
            cursor: 0,
            top: 0,
            page: 1,
            typing: None,
            search: None,
            selection: None,
            notice: None,
        };
        pager.set_items(items);
        pager.cursor = pager.rows.len().saturating_sub(1);
        pager
    }

    /// Replaces the items (the same ones rendered at another width). The cursor stays in the
    /// item it was in.
    pub fn set_items(&mut self, items: Vec<Item>) {
        let held = self.rows.get(self.cursor).copied();
        self.items = items;
        self.rows = self
            .items
            .iter()
            .enumerate()
            .flat_map(|(index, item)| (0..item.lines.len()).map(move |row| (index, row)))
            .collect();
        self.selection = None;
        self.cursor = match held {
            Some((item, row)) => self
                .rows
                .iter()
                .rposition(|(i, r)| *i == item && *r <= row)
                .or_else(|| self.rows.iter().position(|(i, _)| *i >= item))
                .unwrap_or(0),
            None => 0,
        };
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }

    /// Moves the cursor to the first row, for content that reads from the top (a diff).
    pub fn to_top(&mut self) {
        self.cursor = 0;
        self.top = 0;
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    /// Whether a search query is being typed, which is where pasted text goes.
    pub fn is_typing(&self) -> bool {
        self.typing.is_some()
    }

    pub fn paste(&mut self, pasted: &str) {
        if let Some(query) = &mut self.typing {
            query.push_str(pasted.lines().next().unwrap_or_default());
        }
    }

    /// The cursor row, counted from the first row.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Moves the cursor by `delta` rows (the mouse wheel).
    pub fn scroll(&mut self, delta: isize) {
        let last = self.rows.len().saturating_sub(1);
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
    }

    fn row_text(&self, row: usize) -> &str {
        self.rows
            .get(row)
            .map_or("", |(item, line)| self.items[*item].lines[*line].as_str())
    }

    fn matches(&self, row: usize) -> bool {
        self.search.as_deref().is_some_and(|query| {
            text::strip_ansi(self.row_text(row))
                .to_lowercase()
                .contains(query)
        })
    }

    /// Moves to the next row that matches the search, forwards or backwards, wrapping around.
    fn step_match(&mut self, forward: bool) {
        if self.search.is_none() {
            return self.set_notice("no search yet: / starts one");
        }
        let total = self.rows.len();
        let found = (1..=total)
            .map(|step| {
                if forward {
                    (self.cursor + step) % total
                } else {
                    (self.cursor + total - step) % total
                }
            })
            .find(|row| self.matches(*row));
        match found {
            Some(row) => self.cursor = row,
            None => self.set_notice("no match"),
        }
    }

    /// The first row of `item`, and its first row with something on it, where a jump lands.
    fn landing(&self, item: usize) -> Option<(usize, usize)> {
        let first = self.rows.iter().position(|(i, _)| *i == item)?;
        let text = (first..self.rows.len())
            .take_while(|row| self.rows[*row].0 == item)
            .find(|row| !text::strip_ansi(self.row_text(*row)).trim().is_empty())
            .unwrap_or(first);
        Some((first, text))
    }

    /// Moves to the next or previous anchored item.
    fn step_anchor(&mut self, forward: bool) {
        let Some((current, _)) = self.rows.get(self.cursor).copied() else {
            return;
        };
        let target = if forward {
            (current + 1..self.items.len()).find(|i| self.items[*i].anchor)
        } else {
            // From further down in an item, back goes to its own start before the one before it.
            let below_start = self.landing(current).is_some_and(|(_, text)| self.cursor > text);
            let from = if self.items[current].anchor && below_start {
                current + 1
            } else {
                current
            };
            (0..from).rev().find(|i| self.items[*i].anchor)
        };
        if let Some((first, text)) = target.and_then(|item| self.landing(item)) {
            // The item starts at the top of the screen, so as much of it as fits is shown.
            self.top = first;
            self.cursor = text;
        }
    }

    fn code_at_cursor(&self) -> Option<&str> {
        let (item, row) = self.rows.get(self.cursor).copied()?;
        let blocks = &self.items[item].blocks;
        blocks
            .iter()
            .find(|block| block.rows.contains(&row))
            .or_else(|| blocks.iter().find(|block| block.rows.start > row))
            .or(blocks.last())
            .map(|block| block.code.as_str())
    }

    pub fn on_key(&mut self, key: &KeyEvent) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::Pending;
        }
        self.notice = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(query) = &mut self.typing {
            match key.code {
                KeyCode::Esc => self.typing = None,
                KeyCode::Char('c') if ctrl => self.typing = None,
                KeyCode::Backspace => {
                    query.pop();
                }
                KeyCode::Enter => {
                    let query = self.typing.take().unwrap_or_default().to_lowercase();
                    if !query.is_empty() {
                        self.search = Some(query);
                        // A match on the cursor row itself counts.
                        if !self.matches(self.cursor) {
                            self.step_match(true);
                        }
                    }
                }
                KeyCode::Char(c) if !ctrl => query.push(c),
                _ => {}
            }
            return Outcome::Pending;
        }
        let last = self.rows.len().saturating_sub(1);
        let page = self.page.max(1);
        match key.code {
            KeyCode::Esc if self.selection.is_some() => self.selection = None,
            KeyCode::Esc | KeyCode::Char('q') => return Outcome::Close,
            KeyCode::Char('c') if ctrl => return Outcome::Close,
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(last),
            KeyCode::PageUp | KeyCode::Char('b') => self.cursor = self.cursor.saturating_sub(page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.cursor = (self.cursor + page).min(last),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = last,
            KeyCode::Char('[') => self.step_anchor(false),
            KeyCode::Char(']') => self.step_anchor(true),
            KeyCode::Char('/') => self.typing = Some(String::new()),
            KeyCode::Char('n') => self.step_match(true),
            KeyCode::Char('N') => self.step_match(false),
            KeyCode::Char('v') => {
                self.selection = match self.selection {
                    Some(_) => None,
                    None => Some(self.cursor),
                }
            }
            KeyCode::Char('y') => {
                if let Some(anchor) = self.selection.take() {
                    let (from, to) = (anchor.min(self.cursor), anchor.max(self.cursor));
                    let text = (from..=to)
                        .map(|row| text::strip_ansi(self.row_text(row)).trim_end().to_string())
                        .collect::<Vec<_>>()
                        .join("\n");
                    return Outcome::Copy {
                        text,
                        what: "the selected lines",
                    };
                }
                if let Some((item, _)) = self.rows.get(self.cursor) {
                    return Outcome::Copy {
                        text: self.items[*item].source.clone(),
                        what: "the message",
                    };
                }
            }
            KeyCode::Char('c') => match self.code_at_cursor() {
                Some(code) => {
                    return Outcome::Copy {
                        text: code.to_string(),
                        what: "the code block",
                    };
                }
                None => self.set_notice("no code block in this message"),
            },
            KeyCode::Char('o') => match link_in(self.row_text(self.cursor)) {
                Some(link) => return Outcome::Open(link),
                None => self.set_notice("no link on this line"),
            },
            _ => {}
        }
        Outcome::Pending
    }

    /// One screen: a title row, the content with the cursor row in view, and a row of key hints.
    /// Always `height` rows, none wider than `width`.
    pub fn render(&mut self, width: usize, height: usize, theme: &Theme) -> Vec<Line> {
        let width = width.max(1);
        let content = height.saturating_sub(2).max(1);
        self.page = content;
        let total = self.rows.len();
        // The window moves only as far as the cursor demands.
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + content {
            self.top = self.cursor + 1 - content;
        }
        self.top = self.top.min(total.saturating_sub(1));

        let mut out = Vec::with_capacity(height);
        if height >= 3 {
            let place = if total == 0 {
                "empty".to_string()
            } else {
                format!("{}/{total}", self.cursor + 1)
            };
            let title = text::truncate(&self.title, width.saturating_sub(text::width(&place) + 2), "…");
            let gap = width.saturating_sub(text::width(&title) + text::width(&place));
            let line = format!(
                "{}{}{}",
                theme.paint(Role::Accent, &title),
                " ".repeat(gap),
                theme.paint(Role::Muted, &place)
            );
            out.push(text::truncate(&line, width, ""));
        }
        let selected = self
            .selection
            .map(|anchor| anchor.min(self.cursor)..=anchor.max(self.cursor));
        for row in self.top..self.top + content {
            if row >= total {
                out.push(String::new());
                continue;
            }
            let gutter = if row == self.cursor {
                theme.paint(Role::Accent, "▌ ")
            } else if selected.as_ref().is_some_and(|rows| rows.contains(&row)) {
                theme.paint(Role::Accent, "│ ")
            } else if self.matches(row) {
                theme.paint(Role::Warning, "• ")
            } else {
                "  ".to_string()
            };
            if width <= GUTTER {
                out.push(text::truncate(self.row_text(row), width, ""));
            } else {
                out.push(format!(
                    "{gutter}{}",
                    text::truncate(self.row_text(row), width - GUTTER, "")
                ));
            }
        }
        if height >= 2 {
            let foot = match (&self.typing, &self.notice) {
                (Some(query), _) => format!("{}{query}", theme.paint(Role::Accent, "/")),
                (None, Some(notice)) => theme.paint(Role::Warning, notice),
                (None, None) if self.selection.is_some() => {
                    theme.paint(Role::Muted, "selecting lines · ↑↓ extend · y copy · esc cancel")
                }
                (None, None) => theme.paint(
                    Role::Muted,
                    "↑↓ move · [ ] jump · / search · y copy · c code · v select · o open · q back",
                ),
            };
            out.push(text::truncate(&foot, width, "…"));
        }
        out.truncate(height);
        out
    }

    /// Where the terminal cursor belongs while a query is typed: `(row, column)` on the screen.
    pub fn typing_cursor(&self, width: usize, height: usize) -> Option<(u16, u16)> {
        let query = self.typing.as_deref()?;
        let column = (1 + text::width(query)).min(width.saturating_sub(1));
        Some((height.saturating_sub(1) as u16, column as u16))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(pager: &mut Pager, keys: &str) -> Outcome {
        let mut last = Outcome::Pending;
        for c in keys.chars() {
            last = pager.on_key(&match c {
                '\n' => key(KeyCode::Enter),
                '\x1b' => key(KeyCode::Esc),
                c => key(KeyCode::Char(c)),
            });
        }
        last
    }

    fn item(lines: &[&str], anchor: bool) -> Item {
        Item {
            lines: lines.iter().map(|l| l.to_string()).collect(),
            source: lines.join("\n"),
            anchor,
            blocks: Vec::new(),
        }
    }

    /// Two prompts with a reply after each: rows 0, 1-3, 4, 5-6.
    fn transcript() -> Pager {
        Pager::new(
            "Transcript",
            vec![
                item(&["first prompt"], true),
                item(&["a reply", "with an Error in it", "and more"], false),
                item(&["second prompt"], true),
                item(&["another reply", "see https://example.com/docs."], false),
            ],
        )
    }

    #[test]
    fn opens_on_the_last_row_and_moves_within_bounds() {
        let mut pager = transcript();
        assert_eq!(pager.cursor(), 6);
        press(&mut pager, "j");
        assert_eq!(pager.cursor(), 6, "nothing below the last row");
        press(&mut pager, "kk");
        assert_eq!(pager.cursor(), 4);
        press(&mut pager, "g");
        assert_eq!(pager.cursor(), 0);
        press(&mut pager, "k");
        assert_eq!(pager.cursor(), 0);
        press(&mut pager, "G");
        assert_eq!(pager.cursor(), 6);
        pager.scroll(-WHEEL_ROWS);
        assert_eq!(pager.cursor(), 3);
        pager.scroll(-100);
        assert_eq!(pager.cursor(), 0);
        assert_eq!(press(&mut pager, "q"), Outcome::Close);
        assert_eq!(press(&mut pager, "\x1b"), Outcome::Close);
    }

    #[test]
    fn brackets_jump_between_prompts() {
        let mut pager = transcript();
        press(&mut pager, "[");
        assert_eq!(pager.cursor(), 4, "from a reply back to its prompt");
        press(&mut pager, "[");
        assert_eq!(pager.cursor(), 0);
        press(&mut pager, "[");
        assert_eq!(pager.cursor(), 0, "no prompt before the first");
        press(&mut pager, "]");
        assert_eq!(pager.cursor(), 4);
        press(&mut pager, "]");
        assert_eq!(pager.cursor(), 4, "no prompt after the last");
    }

    #[test]
    fn a_jump_lands_on_the_first_row_with_text() {
        let mut pager = Pager::new(
            "t",
            vec![
                item(&["", "first prompt"], true),
                item(&["a reply"], false),
                item(&["", "", "second prompt"], true),
            ],
        );
        assert_eq!(pager.cursor(), 5);
        press(&mut pager, "[");
        assert_eq!(pager.cursor(), 1, "from a prompt's own text to the prompt before");
        press(&mut pager, "j[");
        assert_eq!(pager.cursor(), 1, "from the reply back to its prompt");
        press(&mut pager, "]");
        assert_eq!(pager.cursor(), 5);
        // The blank row above the prompt stays in view.
        let shown = pager.render(30, 6, &Theme::dark());
        assert_eq!(text::strip_ansi(&shown[3]), "▌ second prompt", "{shown:?}");
    }

    #[test]
    fn search_finds_rows_case_insensitively_and_steps_both_ways() {
        let mut pager = transcript();
        press(&mut pager, "g/REPLY\n");
        assert_eq!(pager.cursor(), 1);
        press(&mut pager, "n");
        assert_eq!(pager.cursor(), 5);
        press(&mut pager, "n");
        assert_eq!(pager.cursor(), 1, "the search wraps around");
        press(&mut pager, "N");
        assert_eq!(pager.cursor(), 5);

        // Escape while typing keeps the old search; a query with no match says so.
        press(&mut pager, "/zzz\x1b");
        press(&mut pager, "n");
        assert_eq!(pager.cursor(), 1);
        press(&mut pager, "/zzz\n");
        let shown = pager.render(40, 6, &Theme::dark());
        assert!(text::strip_ansi(&shown[5]).contains("no match"), "{shown:?}");
        // The keys typed into a query are text, not commands.
        assert_eq!(press(&mut pager, "/q"), Outcome::Pending);
        assert!(pager.is_typing());
    }

    #[test]
    fn copies_the_message_the_selection_and_opens_links() {
        let mut pager = transcript();
        assert_eq!(
            press(&mut pager, "y"),
            Outcome::Copy {
                text: "another reply\nsee https://example.com/docs.".to_string(),
                what: "the message"
            }
        );
        assert_eq!(
            press(&mut pager, "o"),
            Outcome::Open("https://example.com/docs".to_string())
        );
        press(&mut pager, "k");
        assert_eq!(press(&mut pager, "o"), Outcome::Pending);

        // v starts a selection at the cursor; y copies its rows and ends it.
        press(&mut pager, "gjv");
        press(&mut pager, "jj");
        assert_eq!(
            press(&mut pager, "y"),
            Outcome::Copy {
                text: "a reply\nwith an Error in it\nand more".to_string(),
                what: "the selected lines"
            }
        );
        assert_eq!(
            press(&mut pager, "y"),
            Outcome::Copy {
                text: "a reply\nwith an Error in it\nand more".to_string(),
                what: "the message"
            }
        );
        // Escape ends a selection before it closes the view.
        press(&mut pager, "v");
        assert_eq!(press(&mut pager, "\x1b"), Outcome::Pending);
        assert_eq!(press(&mut pager, "\x1b"), Outcome::Close);
    }

    const REPLY: &str =
        "Run this:\n\n```sh\ncargo test\ncargo fmt\n```\n\nThen this:\n\n~~~\nplain\n~~~\nDone.";

    #[test]
    fn markdown_items_know_their_code_blocks() {
        assert_eq!(code_blocks(REPLY), ["cargo test\ncargo fmt", "plain"]);
        assert_eq!(code_blocks("no code here"), Vec::<String>::new());
        assert_eq!(
            code_blocks("```\nnever closed\n"),
            ["never closed"],
            "an open fence runs to the end"
        );
        assert_eq!(
            code_blocks("````md\n```\ninner\n```\n````"),
            ["```\ninner\n```"],
            "a longer fence holds shorter ones"
        );

        let theme = Theme::dark();
        let item = markdown_item(REPLY, 40, &theme);
        assert_eq!(item.blocks.len(), 2);
        assert_eq!(item.source, REPLY);
        let plain: Vec<String> = item.lines.iter().map(|l| text::strip_ansi(l)).collect();
        for block in &item.blocks {
            for row in block.rows.clone() {
                let shown = plain[row].trim();
                assert!(block.code.contains(shown), "{shown:?} is not in {:?}", block.code);
            }
        }
        assert!(plain.iter().any(|l| l.contains("Run this:")));
        assert!(plain.last().is_some_and(|l| l.contains("Done.")));

        // c copies the block under the cursor, else the next one of the message.
        let mut pager = Pager::new("t", vec![item.clone()]);
        let first = item.blocks[0].rows.start;
        press(&mut pager, "g");
        assert_eq!(
            press(&mut pager, "c"),
            Outcome::Copy {
                text: "cargo test\ncargo fmt".to_string(),
                what: "the code block"
            }
        );
        pager.scroll(item.blocks[1].rows.start as isize);
        assert_eq!(
            press(&mut pager, "c"),
            Outcome::Copy {
                text: "plain".to_string(),
                what: "the code block"
            }
        );
        pager.scroll(-1000);
        pager.scroll(first as isize);
        assert!(
            matches!(press(&mut pager, "c"), Outcome::Copy { text, .. } if text.starts_with("cargo test"))
        );
        let mut prose = Pager::new("t", vec![markdown_item("just words", 40, &theme)]);
        assert_eq!(press(&mut prose, "c"), Outcome::Pending);
    }

    #[test]
    fn render_fills_the_screen_keeps_the_cursor_in_view_and_fits_the_width() {
        let theme = Theme::dark();
        let rows: Vec<String> = (0..50).map(|i| format!("row {i} {}", "x".repeat(60))).collect();
        let lines: Vec<&str> = rows.iter().map(String::as_str).collect();
        let mut pager = Pager::new("Long", vec![item(&lines, false)]);
        for width in [1, 2, 3, 10, 40] {
            let shown = pager.render(width, 10, &theme);
            assert_eq!(shown.len(), 10);
            for line in &shown {
                assert!(text::width(line) <= width, "width {width}: {line:?}");
            }
        }
        let shown: Vec<String> = pager
            .render(100, 10, &theme)
            .iter()
            .map(|l| text::strip_ansi(l))
            .collect();
        assert!(
            shown[0].starts_with("Long") && shown[0].ends_with("50/50"),
            "{shown:?}"
        );
        assert_eq!(text::width(&shown[0]), 100);
        assert!(shown[8].starts_with("▌ row 49"), "{shown:?}");
        assert!(shown[9].contains("q back"), "{shown:?}");

        // Paging moves by the rows shown, and the window follows the cursor.
        press(&mut pager, "b");
        let shown = pager.render(40, 10, &theme);
        assert!(text::strip_ansi(&shown[1]).starts_with("▌ row 41"), "{shown:?}");
        press(&mut pager, "g");
        let shown = pager.render(40, 10, &theme);
        assert!(text::strip_ansi(&shown[1]).starts_with("▌ row 0"), "{shown:?}");

        // Rows that match the search are marked, and a typed query shows in the last row.
        press(&mut pager, "/row 3\n");
        let shown = pager.render(40, 10, &theme);
        assert!(text::strip_ansi(&shown[4]).starts_with("▌ row 3 "), "{shown:?}");
        press(&mut pager, "nn");
        let shown = pager.render(40, 10, &theme);
        assert!(text::strip_ansi(&shown[8]).starts_with("▌ row 31 "), "{shown:?}");
        assert!(text::strip_ansi(&shown[7]).starts_with("• row 30 "), "{shown:?}");
        press(&mut pager, "g/ab");
        let shown = pager.render(40, 10, &theme);
        assert_eq!(text::strip_ansi(&shown[9]), "/ab");
        assert_eq!(pager.typing_cursor(40, 10), Some((9, 3)));

        // Too short for the title and hints: content only.
        let mut tiny = Pager::new("t", vec![item(&["a", "b"], false)]);
        assert_eq!(tiny.render(10, 1, &theme).len(), 1);
        let empty = Pager::new("t", Vec::new()).render(10, 4, &theme);
        assert_eq!(empty.len(), 4);
        assert!(text::strip_ansi(&empty[0]).ends_with("empty"));
    }

    #[test]
    fn new_items_keep_the_cursor_in_the_same_item() {
        let mut pager = transcript();
        press(&mut pager, "gjj");
        assert_eq!(pager.cursor(), 2);
        // The same items rendered narrower: the reply has more rows now.
        pager.set_items(vec![
            item(&["first", "prompt"], true),
            item(&["a", "reply", "with an", "Error"], false),
            item(&["second prompt"], true),
        ]);
        assert_eq!(pager.cursor(), 3, "still on the second row of the reply");
        pager.set_items(Vec::new());
        assert_eq!(pager.cursor(), 0);
    }

    #[test]
    fn links_end_at_spaces_and_closing_punctuation() {
        assert_eq!(
            link_in("see (https://a.example/x?y=1) now").as_deref(),
            Some("https://a.example/x?y=1")
        );
        assert_eq!(
            link_in("\x1b[4mhttp://b.example\x1b[0m, then").as_deref(),
            Some("http://b.example")
        );
        assert_eq!(
            link_in("file:///tmp/a.txt and https://later.example").as_deref(),
            Some("file:///tmp/a.txt")
        );
        assert_eq!(link_in("nothing here"), None);
        assert_eq!(link_in("https://"), None);
    }
}
