//! Multi-line prompt editor.
//!
//! The cursor is a byte offset that always sits on a grapheme-cluster boundary, so one key press
//! moves over or deletes what the user sees as one character.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

use crate::render::Line;
use crate::text::{self, grapheme_width};
use crate::theme::{Role, Theme};

const MAX_UNDO: usize = 200;
const MAX_HISTORY: usize = 1000;

#[derive(Clone)]
struct Snapshot {
    text: String,
    cursor: usize,
}

/// The kind of edit made last. Consecutive edits of the same kind share one undo step.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Run {
    #[default]
    None,
    Typing,
    Deleting,
}

/// The visible part of an editor's content, as returned by [`Editor::view`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorView {
    pub lines: Vec<Line>,
    /// The cursor as `(row, column)` within `lines`.
    pub cursor: (u16, u16),
    /// Rows of content scrolled out above the first line.
    pub above: usize,
    /// Rows of content below the last line.
    pub below: usize,
}

#[derive(Default)]
pub struct Editor {
    text: String,
    cursor: usize,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    run: Run,
    /// Submitted prompts, oldest first.
    history: Vec<String>,
    /// Entry of `history` being shown while browsing with up/down.
    history_pos: Option<usize>,
    /// What was being typed before history browsing started.
    draft: String,
    /// Display column that vertical movement tries to keep.
    preferred_col: Option<usize>,
    /// First row shown by the last [`Editor::view`]; moved only as far as the cursor demands.
    scroll: Cell<usize>,
}

impl Editor {
    pub fn new() -> Editor {
        Editor::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Byte offset of the cursor in [`Editor::text`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replaces the content and puts the cursor at the end. Undo returns to the previous content.
    pub fn set_text(&mut self, text: &str) {
        self.begin_edit(Run::None);
        self.text = clean(text);
        self.cursor = self.text.len();
    }

    /// Inserts text at the cursor as one undo step (bracketed paste).
    pub fn insert_str(&mut self, text: &str) {
        let text = clean(text);
        if text.is_empty() {
            return;
        }
        self.begin_edit(Run::None);
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    pub fn insert_newline(&mut self) {
        self.begin_edit(Run::None);
        self.text.insert(self.cursor, '\n');
        self.cursor += 1;
    }

    /// Returns the content, records it in the prompt history and clears the editor.
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.push_history(text.clone());
        self.cursor = 0;
        self.undo.clear();
        self.redo.clear();
        self.run = Run::None;
        self.draft.clear();
        self.preferred_col = None;
        text
    }

    /// Adds a prompt to the history reachable with the up key. Blank entries and immediate
    /// repeats are ignored.
    pub fn push_history(&mut self, entry: String) {
        self.history_pos = None;
        let entry = clean(&entry);
        if entry.trim().is_empty() || self.history.last() == Some(&entry) {
            return;
        }
        self.history.push(entry);
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
    }

    /// The whitespace-delimited token that ends at the cursor, for `/command` and `@path`
    /// completion. Empty when the cursor follows whitespace.
    pub fn token_before_cursor(&self) -> &str {
        let before = &self.text[..self.cursor];
        let start = before
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
        &before[start..]
    }

    /// Replaces the token that ends at the cursor and leaves the cursor after the replacement.
    pub fn replace_token_before_cursor(&mut self, replacement: &str) {
        let replacement = clean(replacement);
        let start = self.cursor - self.token_before_cursor().len();
        self.begin_edit(Run::None);
        self.text.replace_range(start..self.cursor, &replacement);
        self.cursor = start + replacement.len();
    }

    /// Handles an editing or movement key. Returns `false` for keys the editor has no use for
    /// (enter, escape, function keys, unbound chords), which the caller may then act on.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        let mods = key.modifiers;
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let alt = mods.contains(KeyModifiers::ALT);
        let other = mods.intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META);
        if other {
            return false;
        }
        match key.code {
            // Control together with alt is how AltGr characters arrive on Windows.
            KeyCode::Char(c) if ctrl == alt => {
                if c.is_control() {
                    return false;
                }
                self.type_char(c);
            }
            KeyCode::Char(c) if ctrl => return self.control_key(c, mods.contains(KeyModifiers::SHIFT)),
            KeyCode::Char(c) => return self.alt_key(c),
            KeyCode::Backspace if ctrl || alt => self.delete_to(self.word_left(self.cursor), Run::None),
            KeyCode::Backspace => self.delete_to(self.prev_boundary(self.cursor), Run::Deleting),
            KeyCode::Delete if ctrl || alt => self.delete_to(self.word_right(self.cursor), Run::None),
            KeyCode::Delete => self.delete_to(self.next_boundary(self.cursor), Run::Deleting),
            KeyCode::Left if ctrl || alt => self.move_to(self.word_left(self.cursor)),
            KeyCode::Left => self.move_to(self.prev_boundary(self.cursor)),
            KeyCode::Right if ctrl || alt => self.move_to(self.word_right(self.cursor)),
            KeyCode::Right => self.move_to(self.next_boundary(self.cursor)),
            KeyCode::Home => self.move_to(self.line_start(self.cursor)),
            KeyCode::End => self.move_to(self.line_end(self.cursor)),
            KeyCode::Up => return self.move_up(),
            KeyCode::Down => return self.move_down(),
            _ => return false,
        }
        true
    }

    /// Soft-wraps the content to `width` columns. `prompt` leads the first row and later rows are
    /// indented to line up with it. Returns the rows and the cursor as `(row, column)` in them.
    pub fn render(&self, width: usize, theme: &Theme, prompt: &str) -> (Vec<Line>, (u16, u16)) {
        let view = self.view(width, usize::MAX, theme, prompt);
        (view.lines, view.cursor)
    }

    /// Like [`Editor::render`], but shows at most `max_rows` rows: a window on the content that
    /// scrolls just far enough to keep the cursor in it.
    pub fn view(&self, width: usize, max_rows: usize, theme: &Theme, prompt: &str) -> EditorView {
        let width = width.max(1);
        let max_rows = max_rows.max(1);
        let prompt_width = text::width(prompt);
        let (prompt, prompt_width) = if prompt_width >= width {
            ("", 0)
        } else {
            (prompt, prompt_width)
        };
        let avail = width - prompt_width;

        let mut rows: Vec<String> = Vec::new();
        let mut cursor = (0usize, 0usize);
        let mut offset = 0usize;
        for line in self.text.split('\n') {
            let mut row = String::new();
            let mut col = 0usize;
            for (i, grapheme) in line.grapheme_indices(true) {
                let w = grapheme_width(grapheme);
                if col + w > avail && col > 0 {
                    rows.push(std::mem::take(&mut row));
                    col = 0;
                }
                if offset + i <= self.cursor {
                    cursor = (rows.len(), col);
                }
                row.push_str(grapheme);
                col += w;
            }
            if offset + line.len() == self.cursor {
                // The cursor needs a cell of its own: after a full row it moves to the next one.
                if col >= avail {
                    rows.push(std::mem::take(&mut row));
                    col = 0;
                }
                cursor = (rows.len(), col);
            }
            rows.push(row);
            offset += line.len() + 1;
        }

        let total = rows.len();
        let shown = total.min(max_rows);
        let mut first = self.scroll.get().min(total - shown);
        if cursor.0 < first {
            first = cursor.0;
        } else if cursor.0 >= first + shown {
            first = cursor.0 + 1 - shown;
        }
        self.scroll.set(first);

        let indent = " ".repeat(prompt_width);
        let lines = rows
            .iter()
            .enumerate()
            .skip(first)
            .take(shown)
            .map(|(i, row)| {
                let lead = if i == 0 { prompt } else { indent.as_str() };
                let line = format!("{lead}{}", theme.paint(Role::Text, row));
                if text::width(&line) > width {
                    text::truncate(&line, width, "")
                } else {
                    line
                }
            })
            .collect();
        let to_u16 = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
        EditorView {
            lines,
            cursor: (to_u16(cursor.0 - first), to_u16(prompt_width + cursor.1)),
            above: first,
            below: total - first - shown,
        }
    }

    fn control_key(&mut self, c: char, shift: bool) -> bool {
        // Some terminals report ctrl+shift+z as an upper-case letter without the shift flag.
        let shift = shift || c.is_ascii_uppercase();
        match c.to_ascii_lowercase() {
            'a' => self.move_to(self.line_start(self.cursor)),
            'e' => self.move_to(self.line_end(self.cursor)),
            'w' => self.delete_to(self.whitespace_word_start(self.cursor), Run::None),
            'u' => self.delete_to(self.line_start(self.cursor), Run::None),
            'k' => {
                let end = self.line_end(self.cursor);
                // At the end of a line, remove the line break instead.
                let end = if end == self.cursor {
                    self.next_boundary(end)
                } else {
                    end
                };
                self.delete_to(end, Run::None);
            }
            'z' if shift => self.redo(),
            'z' => self.undo(),
            'y' => self.redo(),
            _ => return false,
        }
        true
    }

    fn alt_key(&mut self, c: char) -> bool {
        match c.to_ascii_lowercase() {
            'b' => self.move_to(self.word_left(self.cursor)),
            'f' => self.move_to(self.word_right(self.cursor)),
            'd' => self.delete_to(self.word_right(self.cursor), Run::None),
            _ => return false,
        }
        true
    }

    /// Saves an undo step unless this edit continues a run of the same kind.
    fn begin_edit(&mut self, run: Run) {
        if run == Run::None || run != self.run {
            self.undo.push(Snapshot {
                text: self.text.clone(),
                cursor: self.cursor,
            });
            if self.undo.len() > MAX_UNDO {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
        self.run = run;
        self.preferred_col = None;
    }

    fn type_char(&mut self, c: char) {
        if c.is_whitespace() {
            // Undo then works a word at a time.
            self.run = Run::None;
        }
        self.begin_edit(Run::Typing);
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Deletes between the cursor and `target`, whichever side it is on.
    fn delete_to(&mut self, target: usize, run: Run) {
        let (start, end) = if target < self.cursor {
            (target, self.cursor)
        } else {
            (self.cursor, target)
        };
        if start == end {
            return;
        }
        self.begin_edit(run);
        self.text.replace_range(start..end, "");
        self.cursor = start;
    }

    fn move_to(&mut self, target: usize) {
        self.cursor = target;
        self.run = Run::None;
        self.preferred_col = None;
    }

    fn undo(&mut self) {
        if let Some(previous) = self.undo.pop() {
            self.redo.push(Snapshot {
                text: std::mem::take(&mut self.text),
                cursor: self.cursor,
            });
            self.restore(previous);
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(Snapshot {
                text: std::mem::take(&mut self.text),
                cursor: self.cursor,
            });
            self.restore(next);
        }
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.text = snapshot.text;
        self.cursor = snapshot.cursor;
        self.run = Run::None;
        self.preferred_col = None;
    }

    fn move_up(&mut self) -> bool {
        let start = self.line_start(self.cursor);
        if start == 0 {
            return self.history_previous();
        }
        let col = self
            .preferred_col
            .unwrap_or_else(|| text::width(&self.text[start..self.cursor]));
        let previous_end = start - 1;
        let previous_start = self.line_start(previous_end);
        self.cursor = previous_start + offset_at_col(&self.text[previous_start..previous_end], col);
        self.run = Run::None;
        self.preferred_col = Some(col);
        true
    }

    fn move_down(&mut self) -> bool {
        let end = self.line_end(self.cursor);
        if end == self.text.len() {
            return self.history_next();
        }
        let start = self.line_start(self.cursor);
        let col = self
            .preferred_col
            .unwrap_or_else(|| text::width(&self.text[start..self.cursor]));
        let next_start = end + 1;
        let next_end = self.line_end(next_start);
        self.cursor = next_start + offset_at_col(&self.text[next_start..next_end], col);
        self.run = Run::None;
        self.preferred_col = Some(col);
        true
    }

    fn history_previous(&mut self) -> bool {
        let target = match self.history_pos {
            None if self.history.is_empty() => return false,
            None => self.history.len() - 1,
            Some(0) => return true,
            Some(i) => i - 1,
        };
        if self.history_pos.is_none() {
            self.draft = self.text.clone();
        }
        self.show_history(Some(target));
        true
    }

    fn history_next(&mut self) -> bool {
        match self.history_pos {
            None => false,
            Some(i) if i + 1 < self.history.len() => {
                self.show_history(Some(i + 1));
                true
            }
            Some(_) => {
                self.show_history(None);
                true
            }
        }
    }

    fn show_history(&mut self, position: Option<usize>) {
        self.begin_edit(Run::None);
        self.text = match position {
            Some(i) => self.history[i].clone(),
            None => std::mem::take(&mut self.draft),
        };
        self.cursor = self.text.len();
        self.history_pos = position;
    }

    fn prev_boundary(&self, pos: usize) -> usize {
        self.text[..pos]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, pos: usize) -> usize {
        self.text[pos..]
            .graphemes(true)
            .next()
            .map_or(pos, |g| pos + g.len())
    }

    fn line_start(&self, pos: usize) -> usize {
        self.text[..pos].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, pos: usize) -> usize {
        self.text[pos..].find('\n').map_or(self.text.len(), |i| pos + i)
    }

    /// Start of the word before `from`: skips separators, then letters, digits and underscores.
    fn word_left(&self, from: usize) -> usize {
        let mut pos = from;
        let mut graphemes = self.text[..from].grapheme_indices(true).rev().peekable();
        while let Some((i, _)) = graphemes.next_if(|(_, g)| !is_word(g)) {
            pos = i;
        }
        while let Some((i, _)) = graphemes.next_if(|(_, g)| is_word(g)) {
            pos = i;
        }
        pos
    }

    /// End of the word after `from`.
    fn word_right(&self, from: usize) -> usize {
        let mut pos = from;
        let mut graphemes = self.text[from..].grapheme_indices(true).peekable();
        while let Some((i, g)) = graphemes.next_if(|(_, g)| !is_word(g)) {
            pos = from + i + g.len();
        }
        while let Some((i, g)) = graphemes.next_if(|(_, g)| is_word(g)) {
            pos = from + i + g.len();
        }
        pos
    }

    /// Start of the whitespace-delimited word before `from` (what ctrl+w removes).
    fn whitespace_word_start(&self, from: usize) -> usize {
        let word_end = self.text[..from].trim_end().len();
        let last_space = self.text[..word_end]
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace());
        last_space.map_or(0, |(i, c)| i + c.len_utf8())
    }
}

/// Whether a grapheme cluster is part of a word, judged by its base character so that combining
/// marks stay with their letter.
fn is_word(grapheme: &str) -> bool {
    grapheme
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
}

/// Byte offset in `line` of the grapheme boundary closest to display column `col` without
/// passing it.
fn offset_at_col(line: &str, col: usize) -> usize {
    let mut used = 0usize;
    for (i, grapheme) in line.grapheme_indices(true) {
        let w = grapheme_width(grapheme);
        if used + w > col {
            return i;
        }
        used += w;
    }
    line.len()
}

/// Normalises inserted text: line endings become `\n`, tabs become spaces, escape sequences and
/// other control characters are dropped.
fn clean(input: &str) -> String {
    if input.contains('\r') {
        text::sanitize(&input.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        text::sanitize(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    fn typed(editor: &mut Editor, text: &str) {
        for c in text.chars() {
            assert!(editor.handle_key(&key(KeyCode::Char(c))));
        }
    }

    fn with_text(text: &str) -> Editor {
        let mut editor = Editor::new();
        editor.set_text(text);
        editor
    }

    fn press(editor: &mut Editor, event: KeyEvent, times: usize) {
        for _ in 0..times {
            editor.handle_key(&event);
        }
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut editor = Editor::new();
        assert!(editor.is_empty());
        typed(&mut editor, "helo");
        editor.handle_key(&key(KeyCode::Left));
        typed(&mut editor, "l");
        assert_eq!(editor.text(), "hello");
        assert_eq!(editor.cursor(), 4);
        assert!(editor.handle_key(&KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT)));
        assert_eq!(editor.text(), "hellWo");
    }

    #[test]
    fn backspace_and_delete_remove_whole_grapheme_clusters() {
        let mut editor = with_text("e\u{301}👍🏽🧑‍🤝‍🧑");
        editor.handle_key(&key(KeyCode::Backspace));
        assert_eq!(editor.text(), "e\u{301}👍🏽");
        editor.handle_key(&key(KeyCode::Backspace));
        assert_eq!(editor.text(), "e\u{301}");
        editor.handle_key(&key(KeyCode::Backspace));
        assert_eq!(editor.text(), "");
        // Nothing left: still consumed, nothing changes.
        assert!(editor.handle_key(&key(KeyCode::Backspace)));
        assert_eq!(editor.text(), "");

        let mut editor = with_text("👍🏽e\u{301}x");
        editor.handle_key(&key(KeyCode::Home));
        editor.handle_key(&key(KeyCode::Delete));
        assert_eq!(editor.text(), "e\u{301}x");
        editor.handle_key(&key(KeyCode::Delete));
        assert_eq!(editor.text(), "x");
    }

    #[test]
    fn arrows_move_by_grapheme_cluster() {
        let mut editor = with_text("a👍🏽b");
        editor.handle_key(&key(KeyCode::Left));
        assert_eq!(editor.cursor(), "a👍🏽".len());
        editor.handle_key(&key(KeyCode::Left));
        assert_eq!(editor.cursor(), 1);
        editor.handle_key(&key(KeyCode::Right));
        assert_eq!(editor.cursor(), "a👍🏽".len());
        press(&mut editor, key(KeyCode::Right), 5);
        assert_eq!(editor.cursor(), editor.text().len());
        press(&mut editor, key(KeyCode::Left), 9);
        assert_eq!(editor.cursor(), 0);
    }

    #[test]
    fn word_motions_stop_at_word_edges() {
        let mut editor = with_text("foo bar_baz  qux");
        let ctrl_left = KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL);
        let ctrl_right = KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL);

        editor.handle_key(&ctrl_left);
        assert_eq!(editor.cursor(), 13);
        editor.handle_key(&alt('b'));
        assert_eq!(editor.cursor(), 4, "an underscore does not end a word");
        editor.handle_key(&ctrl_left);
        assert_eq!(editor.cursor(), 0);
        editor.handle_key(&ctrl_left);
        assert_eq!(editor.cursor(), 0);

        editor.handle_key(&ctrl_right);
        assert_eq!(editor.cursor(), 3);
        editor.handle_key(&alt('f'));
        assert_eq!(editor.cursor(), 11);
        editor.handle_key(&ctrl_right);
        assert_eq!(editor.cursor(), 16);
        editor.handle_key(&ctrl_right);
        assert_eq!(editor.cursor(), 16);
    }

    #[test]
    fn word_motion_never_splits_a_cluster() {
        // The combining accent is not a letter, but it belongs to the `e` before it.
        let mut editor = with_text("cafe\u{301} ok");
        editor.handle_key(&key(KeyCode::Home));
        editor.handle_key(&alt('f'));
        assert_eq!(editor.cursor(), "cafe\u{301}".len());
    }

    #[test]
    fn home_end_and_emacs_keys_work_on_the_current_line() {
        let mut editor = with_text("first\nsecond line");
        editor.handle_key(&key(KeyCode::Home));
        assert_eq!(editor.cursor(), 6);
        editor.handle_key(&key(KeyCode::End));
        assert_eq!(editor.cursor(), 17);
        editor.handle_key(&ctrl('a'));
        assert_eq!(editor.cursor(), 6);
        editor.handle_key(&ctrl('e'));
        assert_eq!(editor.cursor(), 17);
    }

    #[test]
    fn kill_keys_delete_words_and_line_parts() {
        let mut editor = with_text("git commit --amend");
        editor.handle_key(&ctrl('w'));
        assert_eq!(editor.text(), "git commit ");
        editor.handle_key(&ctrl('w'));
        assert_eq!(editor.text(), "git ");

        let mut editor = with_text("one\ntwo three");
        press(&mut editor, key(KeyCode::Left), 6);
        editor.handle_key(&ctrl('k'));
        assert_eq!(editor.text(), "one\ntwo");
        editor.handle_key(&ctrl('u'));
        assert_eq!(editor.text(), "one\n");
        assert_eq!(editor.cursor(), 4);

        // At the end of a line ctrl+k joins it with the next one.
        let mut editor = with_text("ab\ncd");
        editor.handle_key(&key(KeyCode::Up));
        editor.handle_key(&ctrl('k'));
        assert_eq!(editor.text(), "abcd");

        let mut editor = with_text("alpha beta");
        editor.handle_key(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(editor.text(), "alpha ");
        editor.handle_key(&key(KeyCode::Home));
        editor.handle_key(&alt('d'));
        assert_eq!(editor.text(), " ");
    }

    #[test]
    fn undo_and_redo_step_through_words() {
        let mut editor = Editor::new();
        typed(&mut editor, "hello world");
        editor.handle_key(&ctrl('z'));
        assert_eq!(editor.text(), "hello");
        editor.handle_key(&ctrl('z'));
        assert_eq!(editor.text(), "");
        editor.handle_key(&ctrl('z'));
        assert_eq!(editor.text(), "", "undo with nothing to undo is harmless");

        editor.handle_key(&ctrl('y'));
        assert_eq!(editor.text(), "hello");
        editor.handle_key(&ctrl('y'));
        assert_eq!(editor.text(), "hello world");
        assert_eq!(editor.cursor(), 11);
        editor.handle_key(&ctrl('y'));
        assert_eq!(editor.text(), "hello world");

        // A new edit discards what could have been redone.
        editor.handle_key(&ctrl('z'));
        typed(&mut editor, "!");
        editor.handle_key(&ctrl('y'));
        assert_eq!(editor.text(), "hello!");
    }

    #[test]
    fn paste_and_deletion_runs_are_single_undo_steps() {
        let mut editor = Editor::new();
        typed(&mut editor, "a");
        editor.insert_str("bcd\r\nef\tg");
        assert_eq!(editor.text(), "abcd\nef  g");
        editor.handle_key(&ctrl('z'));
        assert_eq!(editor.text(), "a");

        let mut editor = with_text("abcdef");
        press(&mut editor, key(KeyCode::Backspace), 3);
        assert_eq!(editor.text(), "abc");
        editor.handle_key(&ctrl('z'));
        assert_eq!(editor.text(), "abcdef");
        assert_eq!(editor.cursor(), 6);
    }

    #[test]
    fn up_and_down_move_between_lines_keeping_the_column() {
        let mut editor = with_text("abcdef\nxy\n12345");
        assert!(editor.handle_key(&key(KeyCode::Up)));
        assert_eq!(editor.cursor(), 9, "clamped to the end of the shorter line");
        assert!(editor.handle_key(&key(KeyCode::Up)));
        assert_eq!(editor.cursor(), 5, "the original column is restored");
        assert!(editor.handle_key(&key(KeyCode::Down)));
        assert!(editor.handle_key(&key(KeyCode::Down)));
        assert_eq!(editor.cursor(), 15);

        // Columns are display columns: a wide character counts as two.
        let mut editor = with_text("日本語\nabcd");
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.cursor(), "日本".len());
    }

    #[test]
    fn up_and_down_browse_history_at_the_edges() {
        let mut editor = Editor::new();
        assert!(!editor.handle_key(&key(KeyCode::Up)), "nothing to browse yet");
        editor.push_history("first".to_string());
        editor.push_history("second".to_string());
        editor.push_history("second".to_string());
        editor.push_history("   ".to_string());
        typed(&mut editor, "draft");

        assert!(editor.handle_key(&key(KeyCode::Up)));
        assert_eq!(editor.text(), "second");
        assert!(editor.handle_key(&key(KeyCode::Up)));
        assert_eq!(editor.text(), "first");
        assert!(editor.handle_key(&key(KeyCode::Up)));
        assert_eq!(editor.text(), "first", "the oldest entry stays");

        assert!(editor.handle_key(&key(KeyCode::Down)));
        assert_eq!(editor.text(), "second");
        assert!(editor.handle_key(&key(KeyCode::Down)));
        assert_eq!(
            editor.text(),
            "draft",
            "the draft comes back after the newest entry"
        );
        assert_eq!(editor.cursor(), 5);
        assert!(!editor.handle_key(&key(KeyCode::Down)));
    }

    #[test]
    fn history_is_reached_only_from_the_first_and_last_line() {
        let mut editor = Editor::new();
        editor.push_history("old".to_string());
        editor.set_text("one\ntwo");
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.text(), "one\ntwo", "moved to the first line");
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.text(), "old");
    }

    #[test]
    fn take_returns_text_and_records_history() {
        let mut editor = Editor::new();
        typed(&mut editor, "run tests");
        assert_eq!(editor.take(), "run tests");
        assert!(editor.is_empty());
        assert_eq!(editor.cursor(), 0);
        editor.handle_key(&ctrl('z'));
        assert!(editor.is_empty(), "undo does not bring a submitted prompt back");
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.text(), "run tests");
    }

    #[test]
    fn completion_token_is_the_word_ending_at_the_cursor() {
        let mut editor = with_text("open @src/ma");
        assert_eq!(editor.token_before_cursor(), "@src/ma");
        editor.replace_token_before_cursor("@src/main.rs ");
        assert_eq!(editor.text(), "open @src/main.rs ");
        assert_eq!(editor.cursor(), editor.text().len());
        assert_eq!(editor.token_before_cursor(), "");

        let mut editor = with_text("/he and more");
        press(&mut editor, key(KeyCode::Left), 9);
        assert_eq!(editor.token_before_cursor(), "/he");
        editor.replace_token_before_cursor("/help");
        assert_eq!(editor.text(), "/help and more");

        let editor = with_text("line one\n@pa");
        assert_eq!(editor.token_before_cursor(), "@pa");
    }

    #[test]
    fn newline_and_paste_normalise_input() {
        let mut editor = with_text("ab");
        editor.handle_key(&key(KeyCode::Left));
        editor.insert_newline();
        assert_eq!(editor.text(), "a\nb");
        assert_eq!(editor.cursor(), 2);
        editor.insert_str("x\ry\x1b[31m\x07");
        assert_eq!(editor.text(), "a\nx\nyb");
    }

    #[test]
    fn leaves_unrelated_keys_to_the_caller() {
        let mut editor = with_text("text");
        assert!(!editor.handle_key(&key(KeyCode::Enter)));
        assert!(!editor.handle_key(&key(KeyCode::Esc)));
        assert!(!editor.handle_key(&key(KeyCode::F(5))));
        assert!(!editor.handle_key(&key(KeyCode::Tab)));
        assert!(!editor.handle_key(&ctrl('l')));
        assert!(!editor.handle_key(&alt('x')));
        assert!(!editor.handle_key(&KeyEvent::new(KeyCode::Char('v'), KeyModifiers::SUPER)));
        let release = KeyEvent::new_with_kind(KeyCode::Char('x'), KeyModifiers::NONE, KeyEventKind::Release);
        assert!(!editor.handle_key(&release));
        assert_eq!(editor.text(), "text");

        // AltGr characters arrive with control and alt both set.
        let altgr = KeyEvent::new(KeyCode::Char('@'), KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert!(editor.handle_key(&altgr));
        assert_eq!(editor.text(), "text@");
    }

    #[test]
    fn render_soft_wraps_and_reports_the_cursor() {
        let theme = Theme::dark();
        let editor = with_text("abcdefghijkl");
        let (lines, cursor) = editor.render(10, &theme, "> ");
        assert_eq!(lines, ["> abcdefgh", "  ijkl"]);
        assert_eq!(cursor, (1, 6));

        let mut editor = editor;
        press(&mut editor, key(KeyCode::Left), 6);
        assert_eq!(editor.render(10, &theme, "> ").1, (0, 8));
        press(&mut editor, key(KeyCode::Left), 6);
        assert_eq!(editor.render(10, &theme, "> ").1, (0, 2));
    }

    #[test]
    fn cursor_after_a_full_row_moves_to_a_new_row() {
        let theme = Theme::dark();
        let editor = with_text("abcdefgh");
        let (lines, cursor) = editor.render(10, &theme, "> ");
        assert_eq!(lines, ["> abcdefgh", "  "]);
        assert_eq!(cursor, (1, 2));

        let empty = Editor::new();
        let (lines, cursor) = empty.render(10, &theme, "> ");
        assert_eq!(lines, ["> "]);
        assert_eq!(cursor, (0, 2));
    }

    #[test]
    fn render_handles_wide_characters_and_multiple_lines() {
        let theme = Theme::dark();
        let editor = with_text("日本語日本");
        let (lines, cursor) = editor.render(10, &theme, "> ");
        assert_eq!(lines, ["> 日本語日", "  本"]);
        assert_eq!(cursor, (1, 4));

        // A wide character that does not fit in the last column wraps whole.
        let (lines, cursor) = editor.render(9, &theme, "> ");
        assert_eq!(lines, ["> 日本語", "  日本"]);
        assert_eq!(cursor, (1, 6));

        let mut editor = with_text("ab\n\ncd");
        let (lines, cursor) = editor.render(10, &theme, "> ");
        assert_eq!(lines, ["> ab", "  ", "  cd"]);
        assert_eq!(cursor, (2, 4));
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.render(10, &theme, "> ").1, (1, 2));
    }

    #[test]
    fn render_never_exceeds_the_width() {
        let theme = Theme::dark();
        let editor = with_text("some text 日本語 with a_long_unbroken_identifier\nand a second line");
        for width in 1..40 {
            let (lines, (row, col)) = editor.render(width, &theme, "\x1b[36m>\x1b[0m ");
            for line in &lines {
                assert!(text::width(line) <= width, "width {width}: {line:?}");
            }
            assert!(usize::from(row) < lines.len());
            assert!(usize::from(col) < width, "width {width}: column {col}");
        }
    }
}
