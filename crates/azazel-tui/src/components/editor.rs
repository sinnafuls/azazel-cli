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
/// Cuts the kill ring remembers.
const MAX_KILLS: usize = 20;
/// A paste with more characters than this is folded even when it has few lines.
const FOLD_PASTE_CHARS: usize = 800;

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

/// Where the last yank put its text and which entry of the kill ring it was, so that `alt+y` can
/// swap in the one before.
#[derive(Clone, Copy)]
struct Yank {
    start: usize,
    end: usize,
    index: usize,
}

/// A long paste shown as one token (`[paste #2 +41 lines]`) with the text it stands for.
#[derive(Clone)]
struct Paste {
    token: String,
    text: String,
}

/// Something in the text that is not text, such as an attached image. The cursor steps over
/// it and the delete keys remove it as one character. It is shown as `[<name> #N]`, where N
/// counts the atoms of that name from the start of the text, so the numbers have no gaps
/// however atoms come and go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Atom(char);

/// The character that stands for an atom in the text, and what the atom is called.
#[derive(Clone)]
struct Named {
    ch: char,
    name: String,
}

/// Atoms are characters of a private-use plane no font and no text uses.
const ATOM_FIRST: u32 = 0x10_0000;
const ATOM_COUNT: u32 = 0xFFFE;

fn is_atom(c: char) -> bool {
    (ATOM_FIRST..ATOM_FIRST + ATOM_COUNT).contains(&u32::from(c))
}

/// How an atom called `name` is shown when it is the `number`th of that name in the text.
pub fn atom_label(name: &str, number: usize) -> String {
    format!("[{name} #{number}]")
}

/// `text` with every atom written as its label.
fn labelled(text: &str, atoms: &[Named]) -> String {
    if atoms.is_empty() || !text.contains(is_atom) {
        return text.to_string();
    }
    let mut numbers: Vec<(&str, usize)> = Vec::new();
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        let Some(atom) = atoms.iter().find(|atom| atom.ch == c) else {
            out.push(c);
            continue;
        };
        let at = match numbers.iter().position(|(name, _)| *name == atom.name) {
            Some(at) => at,
            None => {
                numbers.push((&atom.name, 0));
                numbers.len() - 1
            }
        };
        numbers[at].1 += 1;
        out.push_str(&atom_label(&atom.name, numbers[at].1));
    }
    out
}

/// What an editor held, taken out to be put back later (a stashed draft).
#[derive(Clone, Default)]
pub struct Draft {
    text: String,
    pastes: Vec<Paste>,
    atoms: Vec<Named>,
}

impl Draft {
    /// The draft as it was shown: folded pastes as their tokens, atoms as their labels.
    pub fn shown(&self) -> String {
        labelled(&self.text, &self.atoms)
    }
}

/// One row of the content as it is drawn, and where its atoms are in it.
#[derive(Default)]
struct Row {
    text: String,
    chips: Vec<(usize, usize)>,
    /// Ghost text drawn after the content, already cut to the free cells of the row.
    ghost: String,
}

impl Row {
    fn paint(&self, theme: &Theme) -> String {
        let mut out = self.paint_content(theme);
        if !self.ghost.is_empty() {
            out.push_str(&theme.paint(Role::Muted, &self.ghost));
        }
        out
    }

    fn paint_content(&self, theme: &Theme) -> String {
        if self.chips.is_empty() {
            return theme.paint(Role::Text, &self.text);
        }
        let mut out = String::new();
        let mut at = 0;
        for &(start, end) in &self.chips {
            if at < start {
                out.push_str(&theme.paint(Role::Text, &self.text[at..start]));
            }
            out.push_str(&theme.paint(Role::Accent, &self.text[start..end]));
            at = end;
        }
        if at < self.text.len() {
            out.push_str(&theme.paint(Role::Text, &self.text[at..]));
        }
        out
    }
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
    /// Cells a row of text had in the last [`Editor::view`], where up and down find the rows the
    /// text wraps to. 0 until something was drawn: lines are then not wrapped.
    avail: Cell<usize>,
    /// Text cut with the kill keys, newest last.
    kills: Vec<String>,
    /// Set while the last key was a yank.
    yank: Option<Yank>,
    /// Folded pastes whose tokens may be in the text.
    pastes: Vec<Paste>,
    /// Number of the last folded paste.
    paste_seq: usize,
    /// Atoms whose characters may be in the text, or in what undo brings back.
    atoms: Vec<Named>,
    /// Atoms made so far: each has a character of its own for as long as the editor lives.
    atom_seq: u32,
    /// A suggestion shown dim after the cursor; Tab or the right arrow writes it into the text.
    /// Whoever suggests sets it again after every change, so it never outlives the text it fits.
    ghost: Option<String>,
}

impl Editor {
    pub fn new() -> Editor {
        Editor::default()
    }

    /// The content as it is kept: a folded paste is its token, an atom one private character.
    /// [`Editor::shown`] is what the user sees.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The content as shown: a folded paste is its token, an atom its label.
    pub fn shown(&self) -> String {
        labelled(&self.text, &self.atoms)
    }

    /// The content with every folded paste written out and every atom as its label.
    pub fn expanded(&self) -> String {
        self.expanded_with(&mut |text| text.to_string())
    }

    /// The content with every folded paste replaced by what `send` makes of its text, and every
    /// atom as its label.
    pub fn expanded_with(&self, send: &mut dyn FnMut(&str) -> String) -> String {
        let spans = self.token_spans();
        if spans.is_empty() {
            return self.shown();
        }
        let mut out = String::with_capacity(self.text.len());
        let mut at = 0;
        for (start, end, paste) in spans {
            out.push_str(&self.text[at..start]);
            out.push_str(&send(&self.pastes[paste].text));
            at = end;
        }
        out.push_str(&self.text[at..]);
        labelled(&out, &self.atoms)
    }

    /// Inserts an atom called `name` at the cursor as one undo step.
    pub fn insert_atom(&mut self, name: &str) -> Atom {
        let atom = self.new_atom(name);
        self.begin_edit(Run::None);
        self.text.insert(self.cursor, atom.0);
        self.cursor += atom.0.len_utf8();
        atom
    }

    /// Makes the first `literal` in the text an atom called `name`: how a draft that was
    /// written out (a queued message, the file an editor saved) gets its atoms back. `None`
    /// when the text does not have it.
    pub fn atomize(&mut self, literal: &str, name: &str) -> Option<Atom> {
        let start = self.text.find(literal).filter(|_| !literal.is_empty())?;
        let end = start + literal.len();
        let atom = self.new_atom(name);
        let mut utf8 = [0u8; 4];
        let written = atom.0.encode_utf8(&mut utf8);
        self.text.replace_range(start..end, written);
        if self.cursor >= end {
            self.cursor = self.cursor - literal.len() + written.len();
        } else if self.cursor > start {
            self.cursor = start + written.len();
        }
        Some(atom)
    }

    /// The atoms in the text, in the order they stand in it.
    pub fn atoms(&self) -> Vec<Atom> {
        self.text
            .chars()
            .filter(|c| self.atoms.iter().any(|atom| atom.ch == *c))
            .map(Atom)
            .collect()
    }

    fn new_atom(&mut self, name: &str) -> Atom {
        let ch = char::from_u32(ATOM_FIRST + self.atom_seq % ATOM_COUNT).unwrap_or('\u{100000}');
        self.atom_seq += 1;
        self.atoms.push(Named {
            ch,
            name: name.to_string(),
        });
        Atom(ch)
    }

    /// The label of the atom at `pos`, when one is there.
    fn label_at(&self, pos: usize) -> Option<String> {
        let c = self.text[pos..].chars().next()?;
        let name = &self.atoms.iter().find(|atom| atom.ch == c)?.name;
        let same = |c: &char| self.atoms.iter().any(|atom| atom.ch == *c && atom.name == *name);
        Some(atom_label(
            name,
            self.text[..pos].chars().filter(same).count() + 1,
        ))
    }

    /// Display columns of the grapheme cluster at `pos`: an atom is as wide as its label.
    fn cell_width(&self, pos: usize, grapheme: &str) -> usize {
        match self.label_at(pos) {
            Some(label) => text::width(&label),
            None => grapheme_width(grapheme),
        }
    }

    /// Display columns of `start..end`, which lie on one line.
    fn columns(&self, start: usize, end: usize) -> usize {
        self.text[start..end]
            .grapheme_indices(true)
            .map(|(i, grapheme)| self.cell_width(start + i, grapheme))
            .sum()
    }

    /// Byte offset of the grapheme boundary of the line `start..end` closest to display
    /// column `col` without passing it.
    fn offset_at_col(&self, start: usize, end: usize, col: usize) -> usize {
        let mut used = 0usize;
        for (i, grapheme) in self.text[start..end].grapheme_indices(true) {
            let w = self.cell_width(start + i, grapheme);
            if used + w > col {
                return start + i;
            }
            used += w;
        }
        end
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

    /// Inserts pasted text. A paste of more than `fold_lines` lines, or of more than 800
    /// characters, goes in as one token that stands for it; `fold_lines` of 0 never folds.
    pub fn insert_paste(&mut self, text: &str, fold_lines: usize) {
        let text = clean(text);
        let lines = text.lines().count();
        let chars = text.chars().count();
        if fold_lines == 0 || (lines <= fold_lines && chars <= FOLD_PASTE_CHARS) {
            return self.insert_str(&text);
        }
        self.paste_seq += 1;
        let token = if lines > 1 {
            format!("[paste #{} +{lines} lines]", self.paste_seq)
        } else {
            format!("[paste #{} +{chars} chars]", self.paste_seq)
        };
        self.pastes.push(Paste {
            token: token.clone(),
            text,
        });
        self.insert_str(&token);
    }

    pub fn insert_newline(&mut self) {
        self.begin_edit(Run::None);
        self.text.insert(self.cursor, '\n');
        self.cursor += 1;
    }

    /// Returns the content with folded pastes written out and atoms as their labels, records
    /// it in the prompt history and clears the editor.
    pub fn take(&mut self) -> String {
        self.take_with(&mut |text| text.to_string())
    }

    /// [`Editor::take`] with every folded paste replaced by what `send` makes of its text. The
    /// history keeps the pasted text itself, so recalling it does not bring back the wrapping.
    pub fn take_with(&mut self, send: &mut dyn FnMut(&str) -> String) -> String {
        let text = self.expanded_with(send);
        self.push_history(self.expanded());
        self.clear();
        text
    }

    /// Takes the content out without recording it, folded pastes and atoms included, so that
    /// [`Editor::restore_draft`] can put it back.
    pub fn take_draft(&mut self) -> Draft {
        let draft = Draft {
            text: std::mem::take(&mut self.text),
            pastes: std::mem::take(&mut self.pastes),
            atoms: std::mem::take(&mut self.atoms),
        };
        self.clear();
        draft
    }

    /// Replaces the content with a draft taken earlier. Undo returns to the previous content.
    pub fn restore_draft(&mut self, draft: Draft) {
        self.begin_edit(Run::None);
        self.pastes.extend(draft.pastes);
        self.atoms.extend(draft.atoms);
        self.text = draft.text;
        self.cursor = self.text.len();
    }

    fn clear(&mut self) {
        self.text.clear();
        self.pastes.clear();
        self.atoms.clear();
        self.cursor = 0;
        self.undo.clear();
        self.redo.clear();
        self.run = Run::None;
        self.draft.clear();
        self.preferred_col = None;
        self.yank = None;
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

    /// Sets the suggestion drawn after the cursor, or removes it. Only its first line is kept.
    pub fn set_ghost(&mut self, ghost: Option<String>) {
        self.ghost = ghost
            .map(|text| text.lines().next().unwrap_or_default().to_string())
            .filter(|text| !text.is_empty());
    }

    /// The suggestion drawn after the cursor.
    pub fn ghost(&self) -> Option<&str> {
        self.ghost.as_deref()
    }

    /// Writes the suggestion into the text at the cursor. `space` adds a space after a word
    /// being completed (Tab); an empty input gets the suggestion as it is. Returns whether there
    /// was one.
    pub fn accept_ghost(&mut self, space: bool) -> bool {
        let Some(ghost) = self.ghost.take() else {
            return false;
        };
        let space = space && !self.text.is_empty();
        self.begin_edit(Run::None);
        self.text.insert_str(self.cursor, &ghost);
        self.cursor += ghost.len();
        if space {
            self.text.insert(self.cursor, ' ');
            self.cursor += 1;
        }
        true
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
        // Only a yank directly before it gives `alt+y` something to replace.
        let yanked = self.yank.take();
        // The right arrow takes the suggestion; any other key makes it stale.
        if key.code == KeyCode::Right && !ctrl && !alt && self.accept_ghost(false) {
            return true;
        }
        self.ghost = None;
        match key.code {
            // Control together with alt is how AltGr characters arrive on Windows.
            KeyCode::Char(c) if ctrl == alt => {
                if c.is_control() {
                    return false;
                }
                self.type_char(c);
            }
            KeyCode::Char(c) if ctrl => return self.control_key(c, mods.contains(KeyModifiers::SHIFT)),
            KeyCode::Char(c) => return self.alt_key(c, yanked),
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
            KeyCode::Up => return self.arrow_up(),
            KeyCode::Down => return self.arrow_down(),
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
        self.avail.set(avail);

        let mut rows: Vec<Row> = Vec::new();
        let mut cursor = (0usize, 0usize);
        let mut offset = 0usize;
        for line in self.text.split('\n') {
            let mut row = Row::default();
            let mut col = 0usize;
            for (i, grapheme) in line.grapheme_indices(true) {
                // An atom is drawn as its label, which stays in one piece.
                let chip = self.label_at(offset + i);
                let w = chip
                    .as_ref()
                    .map_or_else(|| grapheme_width(grapheme), |label| text::width(label));
                if col + w > avail && col > 0 {
                    rows.push(std::mem::take(&mut row));
                    col = 0;
                }
                if offset + i <= self.cursor {
                    cursor = (rows.len(), col);
                }
                match chip {
                    Some(label) => {
                        row.chips.push((row.text.len(), row.text.len() + label.len()));
                        row.text.push_str(&label);
                    }
                    None => row.text.push_str(grapheme),
                }
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

        // The suggestion goes after the cursor, in the cells that are left, and never wraps.
        if let Some(ghost) = &self.ghost
            && let Some(row) = rows.get_mut(cursor.0)
        {
            let room = avail.saturating_sub(cursor.1);
            // A cursor at the start of a row that only a full row before it made is not
            // followed by anything: the suggestion would wrap.
            let wrapped = cursor.1 == 0
                && !self.text.is_empty()
                && !self.text.ends_with(char::is_control)
                && self.cursor == self.text.len();
            if room > 0 && !wrapped && cursor.1 == text::width(&row.text) {
                row.ghost = text::truncate(ghost, room, "");
            }
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
                let line = format!("{lead}{}", row.paint(theme));
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
            'w' => self.kill_to(self.whitespace_word_start(self.cursor)),
            'u' => self.kill_to(self.line_start(self.cursor)),
            'k' => {
                let end = self.line_end(self.cursor);
                // At the end of a line, remove the line break instead.
                let end = if end == self.cursor {
                    self.next_boundary(end)
                } else {
                    end
                };
                self.kill_to(end);
            }
            'z' if shift => self.redo(),
            'z' => self.undo(),
            'y' => self.yank(),
            _ => return false,
        }
        true
    }

    fn alt_key(&mut self, c: char, yanked: Option<Yank>) -> bool {
        match c.to_ascii_lowercase() {
            'b' => self.move_to(self.word_left(self.cursor)),
            'f' => self.move_to(self.word_right(self.cursor)),
            'd' => self.kill_to(self.word_right(self.cursor)),
            'y' => self.yank_pop(yanked),
            _ => return false,
        }
        true
    }

    /// Deletes between the cursor and `target` and keeps what was cut on the kill ring.
    fn kill_to(&mut self, target: usize) {
        let (start, end) = self.span_to(target);
        if start < end {
            self.push_kill(self.text[start..end].to_string());
            self.delete_to(target, Run::None);
        }
    }

    /// Puts `text` on the kill ring as its newest entry.
    pub(crate) fn push_kill(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.kills.push(text);
        if self.kills.len() > MAX_KILLS {
            self.kills.remove(0);
        }
    }

    /// The newest entry of the kill ring.
    pub(crate) fn newest_kill(&self) -> Option<&str> {
        self.kills.last().map(String::as_str)
    }

    /// Inserts the newest kill at the cursor.
    fn yank(&mut self) {
        let Some(text) = self.kills.last().cloned() else {
            return;
        };
        let start = self.cursor;
        self.begin_edit(Run::None);
        self.text.insert_str(start, &text);
        self.cursor = start + text.len();
        self.yank = Some(Yank {
            start,
            end: self.cursor,
            index: self.kills.len() - 1,
        });
    }

    /// Right after a yank: replaces what it inserted with the kill before that one.
    fn yank_pop(&mut self, yanked: Option<Yank>) {
        let Some(Yank { start, end, index }) = yanked else {
            return;
        };
        let index = index.checked_sub(1).unwrap_or(self.kills.len() - 1);
        let Some(text) = self.kills.get(index).cloned() else {
            return;
        };
        self.begin_edit(Run::None);
        self.text.replace_range(start..end, &text);
        self.cursor = start + text.len();
        self.yank = Some(Yank {
            start,
            end: self.cursor,
            index,
        });
    }

    /// Byte ranges of the paste tokens in the text, in order, each with the index of its paste.
    fn token_spans(&self) -> Vec<(usize, usize, usize)> {
        let mut spans: Vec<(usize, usize, usize)> = Vec::new();
        for (index, paste) in self.pastes.iter().enumerate() {
            for (start, token) in self.text.match_indices(paste.token.as_str()) {
                spans.push((start, start + token.len(), index));
            }
        }
        spans.sort_unstable();
        spans
    }

    /// A position inside a paste token moved to its end (`forward`) or its start: a token is
    /// crossed and deleted as one character.
    fn snap(&self, pos: usize, forward: bool) -> usize {
        if self.pastes.is_empty() {
            return pos;
        }
        match self
            .token_spans()
            .into_iter()
            .find(|(start, end, _)| *start < pos && pos < *end)
        {
            Some((_, end, _)) if forward => end,
            Some((start, _, _)) => start,
            None => pos,
        }
    }

    /// The range between the cursor and `target`, widened to whole paste tokens.
    fn span_to(&self, target: usize) -> (usize, usize) {
        let (start, end) = if target < self.cursor {
            (target, self.cursor)
        } else {
            (self.cursor, target)
        };
        (self.snap(start, false), self.snap(end, true))
    }

    /// Saves an undo step unless this edit continues a run of the same kind.
    fn begin_edit(&mut self, run: Run) {
        self.ghost = None;
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
        self.yank = None;
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
        let (start, end) = self.span_to(target);
        if start == end {
            return;
        }
        self.begin_edit(run);
        self.text.replace_range(start..end, "");
        self.cursor = start;
    }

    pub(crate) fn move_to(&mut self, target: usize) {
        self.cursor = self.snap(target, target > self.cursor);
        self.run = Run::None;
        self.preferred_col = None;
    }

    pub(crate) fn undo(&mut self) {
        if let Some(previous) = self.undo.pop() {
            self.redo.push(Snapshot {
                text: std::mem::take(&mut self.text),
                cursor: self.cursor,
            });
            self.restore(previous);
        }
    }

    pub(crate) fn redo(&mut self) {
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

    /// The rows the text is drawn in as byte ranges, wrapped as [`Editor::view`] wraps them.
    /// The cursor at the end of a line that fills its last row has a row of its own, as there.
    fn visual_rows(&self) -> Vec<(usize, usize)> {
        let avail = match self.avail.get() {
            0 => usize::MAX,
            n => n,
        };
        let mut rows = Vec::new();
        let mut offset = 0usize;
        for line in self.text.split('\n') {
            let mut start = offset;
            let mut col = 0usize;
            for (i, grapheme) in line.grapheme_indices(true) {
                let w = self.cell_width(offset + i, grapheme);
                if col + w > avail && col > 0 {
                    rows.push((start, offset + i));
                    start = offset + i;
                    col = 0;
                }
                col += w;
            }
            let end = offset + line.len();
            if end == self.cursor && col >= avail {
                rows.push((start, end));
                start = end;
            }
            rows.push((start, end));
            offset = end + 1;
        }
        rows
    }

    /// Index in `rows` of the row the cursor is drawn in.
    fn cursor_row(&self, rows: &[(usize, usize)]) -> usize {
        rows.iter()
            .rposition(|(start, _)| *start <= self.cursor)
            .unwrap_or(0)
    }

    /// Puts the cursor on row `to`, at the display column it has in row `from` or the one that
    /// vertical movement is keeping.
    fn step_row(&mut self, rows: &[(usize, usize)], from: usize, to: usize) {
        let col = self
            .preferred_col
            .unwrap_or_else(|| self.columns(rows[from].0, self.cursor));
        let (start, end) = rows[to];
        let mut target = self.offset_at_col(start, end, col);
        // The end of a row that wraps is the start of the next one.
        if target == end && end > start && rows.get(to + 1).is_some_and(|next| next.0 == end) {
            target = self.prev_boundary(end);
        }
        self.cursor = self.snap(target, false);
        self.run = Run::None;
        self.preferred_col = Some(col);
    }

    /// The up key: a row up, keeping the column; on the first row, to the start of the text, and
    /// from there back through the history.
    fn arrow_up(&mut self) -> bool {
        let rows = self.visual_rows();
        let row = self.cursor_row(&rows);
        if row > 0 {
            self.step_row(&rows, row, row - 1);
            true
        } else if self.cursor > 0 {
            self.move_to(0);
            true
        } else {
            self.history_previous()
        }
    }

    /// The down key: a row down, keeping the column; on the last row, to the end of the text, and
    /// from there forward through the history.
    fn arrow_down(&mut self) -> bool {
        let rows = self.visual_rows();
        let row = self.cursor_row(&rows);
        if row + 1 < rows.len() {
            self.step_row(&rows, row, row + 1);
            true
        } else if self.cursor < self.text.len() {
            self.move_to(self.text.len());
            true
        } else {
            self.history_next()
        }
    }

    /// A line up, for Vim's `k`: the first line goes on to the history.
    pub(crate) fn move_up(&mut self) -> bool {
        let start = self.line_start(self.cursor);
        if start == 0 {
            return self.history_previous();
        }
        let col = self
            .preferred_col
            .unwrap_or_else(|| self.columns(start, self.cursor));
        let previous_end = start - 1;
        let previous_start = self.line_start(previous_end);
        let target = self.offset_at_col(previous_start, previous_end, col);
        self.cursor = self.snap(target, false);
        self.run = Run::None;
        self.preferred_col = Some(col);
        true
    }

    /// A line down, for Vim's `j`: the last line goes on to the history.
    pub(crate) fn move_down(&mut self) -> bool {
        let end = self.line_end(self.cursor);
        if end == self.text.len() {
            return self.history_next();
        }
        let start = self.line_start(self.cursor);
        let col = self
            .preferred_col
            .unwrap_or_else(|| self.columns(start, self.cursor));
        let next_start = end + 1;
        let next_end = self.line_end(next_start);
        let target = self.offset_at_col(next_start, next_end, col);
        self.cursor = self.snap(target, false);
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
        self.show_history(Some(target), true);
        true
    }

    fn history_next(&mut self) -> bool {
        match self.history_pos {
            None => false,
            Some(i) if i + 1 < self.history.len() => {
                self.show_history(Some(i + 1), false);
                true
            }
            Some(_) => {
                self.show_history(None, false);
                true
            }
        }
    }

    /// Shows an entry, or the draft for `None`. Stepping back leaves the cursor at the start of
    /// the entry and stepping forward at its end, so the next press of the same key goes on
    /// through the history instead of first crossing the entry.
    fn show_history(&mut self, position: Option<usize>, cursor_at_start: bool) {
        self.begin_edit(Run::None);
        self.text = match position {
            Some(i) => self.history[i].clone(),
            None => std::mem::take(&mut self.draft),
        };
        self.cursor = if cursor_at_start { 0 } else { self.text.len() };
        self.history_pos = position;
    }

    pub(crate) fn prev_boundary(&self, pos: usize) -> usize {
        self.text[..pos]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    pub(crate) fn next_boundary(&self, pos: usize) -> usize {
        self.text[pos..]
            .graphemes(true)
            .next()
            .map_or(pos, |g| pos + g.len())
    }

    pub(crate) fn line_start(&self, pos: usize) -> usize {
        self.text[..pos].rfind('\n').map_or(0, |i| i + 1)
    }

    pub(crate) fn line_end(&self, pos: usize) -> usize {
        self.text[pos..].find('\n').map_or(self.text.len(), |i| pos + i)
    }

    /// Removes `start..end` as one undo step and leaves the cursor at `start`; with `kill` the
    /// removed text goes on the kill ring.
    pub(crate) fn remove(&mut self, start: usize, end: usize, kill: bool) {
        if start >= end {
            return;
        }
        if kill {
            self.push_kill(self.text[start..end].to_string());
        }
        self.begin_edit(Run::None);
        self.text.replace_range(start..end, "");
        self.cursor = start;
    }

    /// Inserts `text` at `pos` as one undo step and leaves the cursor after it.
    pub(crate) fn insert_at(&mut self, pos: usize, text: &str) {
        self.begin_edit(Run::None);
        self.text.insert_str(pos, text);
        self.cursor = pos + text.len();
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

/// Normalises inserted text: line endings become `\n`, tabs become spaces, escape sequences and
/// other control characters are dropped, and so are the characters that stand for atoms (only
/// the editor puts those in).
fn clean(input: &str) -> String {
    let cleaned = if input.contains('\r') {
        text::sanitize(&input.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        text::sanitize(input)
    };
    if cleaned.contains(is_atom) {
        cleaned.replace(is_atom, "")
    } else {
        cleaned
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

    fn shown(editor: &Editor, width: usize) -> Vec<String> {
        let (lines, _) = editor.render(width, &Theme::dark(), "> ");
        lines.iter().map(|line| text::strip_ansi(line)).collect()
    }

    #[test]
    fn a_suggestion_is_drawn_dim_after_the_cursor_and_cut_to_the_row() {
        let mut editor = with_text("fix the authen");
        editor.set_ghost(Some("tication middleware".into()));
        assert_eq!(shown(&editor, 40), ["> fix the authentication middleware"]);
        let (lines, cursor) = editor.render(40, &Theme::dark(), "> ");
        assert_eq!(cursor, (0, 16), "the cursor stays before the suggestion");
        assert!(lines[0].ends_with(&format!(
            "{}tication middleware{}",
            Theme::dark().fg(Role::Muted),
            crate::theme::RESET
        )));
        // Cut to the free cells, never wrapped.
        assert_eq!(shown(&editor, 22), ["> fix the authenticati"]);
        // A full row sends the cursor to the next one, where there is nothing to wrap.
        assert_eq!(shown(&editor, 16), ["> fix the authen", "  "]);
        // Not drawn when the cursor is not at the end of its text.
        editor.handle_key(&key(KeyCode::Home));
        editor.set_ghost(Some("tication".into()));
        assert_eq!(shown(&editor, 40), ["> fix the authen"]);
    }

    #[test]
    fn right_takes_the_suggestion_and_tab_takes_it_with_a_space() {
        let mut editor = with_text("fix the authen");
        editor.set_ghost(Some("tication".into()));
        assert!(editor.handle_key(&key(KeyCode::Right)));
        assert_eq!((editor.text(), editor.ghost()), ("fix the authentication", None));

        let mut editor = with_text("fix the authen");
        editor.set_ghost(Some("tication".into()));
        assert!(editor.accept_ghost(true));
        assert_eq!(editor.text(), "fix the authentication ");
        assert_eq!(editor.cursor(), editor.text().len());
        assert!(!editor.accept_ghost(true));
        // One undo takes the whole suggestion back.
        editor.handle_key(&ctrl('z'));
        assert_eq!(editor.text(), "fix the authen");
    }

    #[test]
    fn any_other_key_dismisses_the_suggestion() {
        for event in [
            key(KeyCode::Char('x')),
            key(KeyCode::Left),
            key(KeyCode::Backspace),
            key(KeyCode::Enter),
        ] {
            let mut editor = with_text("fix the authen");
            editor.set_ghost(Some("tication".into()));
            editor.handle_key(&event);
            assert_eq!(editor.ghost(), None, "{event:?}");
        }
        // A suggestion for an empty input is taken as written, without a space.
        let mut editor = Editor::new();
        editor.set_ghost(Some("run the tests".into()));
        assert_eq!(shown(&editor, 40), ["> run the tests"]);
        assert!(editor.accept_ghost(true));
        assert_eq!(editor.text(), "run the tests");
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

        let redo = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        editor.handle_key(&redo);
        assert_eq!(editor.text(), "hello");
        editor.handle_key(&redo);
        assert_eq!(editor.text(), "hello world");
        assert_eq!(editor.cursor(), 11);
        editor.handle_key(&redo);
        assert_eq!(editor.text(), "hello world");

        // A new edit discards what could have been redone.
        editor.handle_key(&ctrl('z'));
        typed(&mut editor, "!");
        editor.handle_key(&redo);
        assert_eq!(editor.text(), "hello!");
    }

    #[test]
    fn kill_ring_keeps_cuts_and_yank_cycles_through_them_newest_first() {
        let mut editor = with_text("one two three");
        editor.handle_key(&ctrl('w'));
        editor.handle_key(&ctrl('w'));
        assert_eq!(editor.text(), "one ");
        editor.handle_key(&ctrl('u'));
        assert_eq!(editor.text(), "");

        // The newest cut comes back first; alt+y swaps in the ones before, then wraps.
        editor.handle_key(&ctrl('y'));
        assert_eq!(editor.text(), "one ");
        editor.handle_key(&alt('y'));
        assert_eq!(editor.text(), "two ");
        editor.handle_key(&alt('y'));
        assert_eq!(editor.text(), "three");
        editor.handle_key(&alt('y'));
        assert_eq!(editor.text(), "one ");
        assert_eq!(editor.cursor(), 4);

        // Anything between the yank and alt+y breaks the pair.
        typed(&mut editor, "x");
        assert!(editor.handle_key(&alt('y')));
        assert_eq!(editor.text(), "one x");

        // ctrl+k and alt+d cut onto the ring too, and a yank is one undo step.
        let mut editor = with_text("alpha beta");
        editor.handle_key(&key(KeyCode::Home));
        editor.handle_key(&alt('d'));
        editor.handle_key(&ctrl('k'));
        assert_eq!(editor.text(), "");
        editor.handle_key(&ctrl('y'));
        assert_eq!(editor.text(), " beta");
        editor.handle_key(&alt('y'));
        assert_eq!(editor.text(), "alpha");
        editor.handle_key(&ctrl('z'));
        editor.handle_key(&ctrl('z'));
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn a_long_paste_folds_into_a_token_and_take_returns_the_text() {
        let long: String = (1..=41).map(|i| format!("line {i}\n")).collect();
        let mut editor = Editor::new();
        typed(&mut editor, "see ");
        editor.insert_paste(&long, 10);
        typed(&mut editor, " ok");
        assert_eq!(editor.text(), "see [paste #1 +41 lines] ok");
        assert_eq!(editor.expanded(), format!("see {long} ok"));

        // A short paste goes in as it is; a long single line folds by its length.
        let mut short = Editor::new();
        short.insert_paste("a\nb", 10);
        assert_eq!(short.text(), "a\nb");
        short.insert_paste(&"x".repeat(801), 10);
        assert_eq!(short.text(), "a\nb[paste #1 +801 chars]");
        let mut never = Editor::new();
        never.insert_paste(&long, 0);
        assert_eq!(never.text(), long);

        // The token is one character to the cursor and to backspace.
        press(&mut editor, key(KeyCode::Left), 4);
        assert_eq!(editor.cursor(), 4, "the cursor skips over the token");
        editor.handle_key(&key(KeyCode::Right));
        assert_eq!(editor.cursor(), "see [paste #1 +41 lines]".len());
        editor.handle_key(&key(KeyCode::Backspace));
        assert_eq!(editor.text(), "see  ok");
        editor.handle_key(&ctrl('z'));
        assert_eq!(editor.text(), "see [paste #1 +41 lines] ok");

        assert_eq!(editor.take(), format!("see {long} ok"));
        assert!(editor.is_empty());
        // The history holds what was sent, not the token.
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.text(), format!("see {long} ok"));
    }

    #[test]
    fn a_draft_keeps_its_folded_pastes_across_take_and_restore() {
        let long = "x\n".repeat(30);
        let mut editor = Editor::new();
        editor.insert_paste(&long, 10);
        typed(&mut editor, " tail");
        let draft = editor.take_draft();
        assert!(editor.is_empty());
        assert_eq!(draft.shown(), "[paste #1 +30 lines] tail");
        typed(&mut editor, "other");
        assert_eq!(editor.take(), "other");

        editor.restore_draft(draft);
        assert_eq!(editor.text(), "[paste #1 +30 lines] tail");
        assert_eq!(editor.take(), format!("{long} tail"));
        // Numbers are not reused, so an old token never names a newer paste.
        editor.insert_paste(&long, 10);
        assert_eq!(editor.text(), "[paste #2 +30 lines]");
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

        // The first press only goes to the start of the draft.
        assert!(editor.handle_key(&key(KeyCode::Up)));
        assert_eq!((editor.text(), editor.cursor()), ("draft", 0));
        assert!(editor.handle_key(&key(KeyCode::Up)));
        assert_eq!(editor.text(), "second");
        assert!(editor.handle_key(&key(KeyCode::Up)));
        assert_eq!(editor.text(), "first");
        assert!(editor.handle_key(&key(KeyCode::Up)));
        assert_eq!(editor.text(), "first", "the oldest entry stays");

        // Going back, the cursor is at the start of the entry: one press to its end.
        assert!(editor.handle_key(&key(KeyCode::Down)));
        assert_eq!((editor.text(), editor.cursor()), ("first", 5));
        assert!(editor.handle_key(&key(KeyCode::Down)));
        assert_eq!(editor.text(), "second");
        assert_eq!(
            editor.cursor(),
            6,
            "a recalled-forward entry has the cursor at its end"
        );
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
        assert_eq!(editor.text(), "one\ntwo", "moved to the start");
        assert_eq!(editor.cursor(), 0);
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.text(), "old");
    }

    #[test]
    fn up_on_the_first_line_goes_to_the_start_before_the_history() {
        let mut editor = Editor::new();
        editor.push_history("old".to_string());
        editor.set_text("hello");
        editor.handle_key(&key(KeyCode::Left));
        assert!(editor.handle_key(&key(KeyCode::Up)));
        assert_eq!((editor.text(), editor.cursor()), ("hello", 0));
        assert!(editor.handle_key(&key(KeyCode::Up)));
        assert_eq!(editor.text(), "old");
    }

    #[test]
    fn down_on_the_last_line_goes_to_the_end_before_the_history() {
        let mut editor = Editor::new();
        editor.push_history("old".to_string());
        editor.push_history("newer".to_string());
        editor.handle_key(&key(KeyCode::Up));
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.text(), "old");
        editor.handle_key(&key(KeyCode::Right));
        // Mid-text on the only line: the end first.
        assert!(editor.handle_key(&key(KeyCode::Down)));
        assert_eq!((editor.text(), editor.cursor()), ("old", 3));
        assert!(editor.handle_key(&key(KeyCode::Down)));
        assert_eq!(editor.text(), "newer");

        // Multi-line: Down on the last line with the cursor before its end.
        let mut editor = with_text("ab\ncd");
        editor.handle_key(&key(KeyCode::Home));
        assert_eq!(editor.cursor(), 3);
        assert!(editor.handle_key(&key(KeyCode::Down)));
        assert_eq!(editor.cursor(), 5);
        assert!(!editor.handle_key(&key(KeyCode::Down)), "nothing to browse");
    }

    #[test]
    fn arrows_keep_their_line_movement_in_the_middle_of_a_text() {
        let mut editor = with_text("abc\ndef\nghi");
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.cursor(), 7, "to the middle line, same column");
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.cursor(), 3);
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.cursor(), 0, "the first line goes to the start");
        editor.handle_key(&key(KeyCode::Down));
        assert_eq!(editor.cursor(), 4, "and down again keeps moving by lines");
    }

    #[test]
    fn arrows_on_an_empty_editor_only_browse_the_history() {
        let mut editor = Editor::new();
        assert!(!editor.handle_key(&key(KeyCode::Up)));
        assert!(!editor.handle_key(&key(KeyCode::Down)));
        editor.push_history("old".to_string());
        assert!(editor.handle_key(&key(KeyCode::Up)));
        assert_eq!(editor.text(), "old");
    }

    #[test]
    fn recalled_entries_can_be_stepped_through_without_extra_presses() {
        let mut editor = Editor::new();
        editor.push_history("one\ntwo".to_string());
        editor.push_history("three\nfour".to_string());
        editor.push_history("five".to_string());
        for want in ["five", "three\nfour", "one\ntwo"] {
            editor.handle_key(&key(KeyCode::Up));
            assert_eq!(editor.text(), want);
            assert_eq!(editor.cursor(), 0, "Up-recall leaves the cursor at the start");
        }
        // Down-recall leaves it at the end, so the next Down goes on at once.
        editor.handle_key(&key(KeyCode::Down));
        assert_eq!(editor.cursor(), 4, "from the start, a line down");
        editor.handle_key(&key(KeyCode::Down));
        assert_eq!(editor.cursor(), 7, "then the end of the entry");
        for want in ["three\nfour", "five"] {
            editor.handle_key(&key(KeyCode::Down));
            assert_eq!(editor.text(), want);
            assert_eq!(editor.cursor(), want.len());
        }
        editor.handle_key(&key(KeyCode::Down));
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn arrows_move_between_wrapped_rows() {
        let theme = Theme::dark();
        // Rows of 8 cells after the prompt: "abcdefgh" and "ijkl".
        let mut editor = with_text("abcdefghijkl");
        editor.render(10, &theme, "> ");
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.cursor(), 4, "to the row above, same column");
        editor.handle_key(&key(KeyCode::Down));
        assert_eq!(editor.cursor(), 12, "to the row below, clamped to its end");
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.cursor(), 4);
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.cursor(), 0, "the first row goes to the start of the text");
        editor.handle_key(&key(KeyCode::Down));
        assert_eq!(editor.cursor(), 8, "the column of the start is kept");
        editor.handle_key(&key(KeyCode::Down));
        assert_eq!(editor.cursor(), 12, "the last row goes to the end of the text");
        assert!(!editor.handle_key(&key(KeyCode::Down)));
    }

    #[test]
    fn up_from_a_wrapped_row_does_not_reach_the_history() {
        let theme = Theme::dark();
        let mut editor = Editor::new();
        editor.push_history("old".to_string());
        editor.set_text("abcdefghijkl");
        editor.render(10, &theme, "> ");
        editor.handle_key(&key(KeyCode::Up));
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.text(), "abcdefghijkl");
        assert_eq!(editor.cursor(), 0);
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.text(), "old");
    }

    #[test]
    fn a_cursor_after_a_full_row_moves_up_into_it() {
        let theme = Theme::dark();
        let mut editor = with_text("abcdefgh");
        editor.render(10, &theme, "> ");
        // The cursor is drawn on a row of its own after the full one.
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.cursor(), 0);
        assert!(editor.handle_key(&key(KeyCode::Down)));
        assert_eq!(editor.cursor(), 8);
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

    #[test]
    fn an_atom_is_one_character_to_the_keys_and_a_numbered_label_to_the_eye() {
        let mut editor = with_text("see ");
        let first = editor.insert_atom("Image");
        typed(&mut editor, " and ");
        let second = editor.insert_atom("Image");
        assert_eq!(editor.shown(), "see [Image #1] and [Image #2]");
        assert_eq!(editor.expanded(), "see [Image #1] and [Image #2]");
        assert_eq!(editor.atoms(), [first, second]);
        assert_ne!(first, second);

        // One press of left is on the far side of the label, one backspace removes it whole.
        let end = editor.cursor();
        editor.handle_key(&key(KeyCode::Left));
        assert_eq!(
            end - editor.cursor(),
            editor.text().chars().last().unwrap().len_utf8()
        );
        editor.handle_key(&key(KeyCode::Right));
        editor.handle_key(&key(KeyCode::Backspace));
        assert_eq!(editor.shown(), "see [Image #1] and ");
        assert_eq!(editor.atoms(), [first]);

        // Removing the first one renumbers what follows; undo brings it back as it was.
        let third = editor.insert_atom("Image");
        assert_eq!(editor.shown(), "see [Image #1] and [Image #2]");
        editor.handle_key(&key(KeyCode::Home));
        press(&mut editor, key(KeyCode::Right), 4);
        editor.handle_key(&key(KeyCode::Delete));
        assert_eq!(editor.shown(), "see  and [Image #1]");
        assert_eq!(editor.atoms(), [third]);
        editor.handle_key(&ctrl('z'));
        assert_eq!(editor.shown(), "see [Image #1] and [Image #2]");
        assert_eq!(editor.atoms(), [first, third]);

        // An atom put in front of the others takes the first number.
        editor.handle_key(&key(KeyCode::Home));
        let fourth = editor.insert_atom("Image");
        assert_eq!(editor.shown(), "[Image #1]see [Image #2] and [Image #3]");
        assert_eq!(editor.atoms(), [fourth, first, third]);
    }

    #[test]
    fn atoms_are_sent_as_their_labels_and_survive_a_stash() {
        let mut editor = with_text("look ");
        let atom = editor.insert_atom("Image");
        let draft = editor.take_draft();
        assert!(editor.is_empty() && editor.atoms().is_empty());
        assert_eq!(draft.shown(), "look [Image #1]");

        // An atom made meanwhile is not confused with the one in the draft.
        let other = editor.insert_atom("Image");
        assert_ne!(other, atom);
        editor.take_draft();
        editor.restore_draft(draft);
        assert_eq!(editor.atoms(), [atom]);
        assert_eq!(editor.shown(), "look [Image #1]");

        // Taking the content writes the label out, and that is what the history has.
        assert_eq!(editor.take(), "look [Image #1]");
        assert!(editor.atoms().is_empty());
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.text(), "look [Image #1]");
        assert!(editor.atoms().is_empty(), "a label that was written out is text");
    }

    #[test]
    fn labels_that_were_written_out_become_atoms_again() {
        let mut editor = with_text("a [Image #1] b [Image #2] c");
        let first = editor.atomize("[Image #1]", "Image").unwrap();
        let second = editor.atomize("[Image #2]", "Image").unwrap();
        assert_eq!(editor.atoms(), [first, second]);
        assert_eq!(editor.shown(), "a [Image #1] b [Image #2] c");
        assert_eq!(editor.cursor(), editor.text().len());
        assert_eq!(editor.atomize("[Image #3]", "Image"), None);
        assert_eq!(editor.atomize("", "Image"), None);

        // Text from outside cannot bring an atom in: its character is dropped.
        let forged: String = editor.text().chars().filter(|c| is_atom(*c)).take(1).collect();
        let mut other = Editor::new();
        other.insert_str(&format!("x{forged}y"));
        assert_eq!(other.text(), "xy");
        other.set_text(&forged);
        assert!(other.is_empty());
    }

    #[test]
    fn a_label_is_drawn_in_one_piece_and_the_cursor_counts_its_columns() {
        let theme = Theme::dark();
        let mut editor = with_text("ab ");
        editor.insert_atom("Image");
        let (lines, cursor) = editor.render(40, &theme, "> ");
        assert_eq!(text::strip_ansi(&lines[0]), "> ab [Image #1]");
        assert_eq!(cursor, (0, 15));
        assert!(lines[0].contains(&theme.paint(Role::Accent, "[Image #1]")));

        // Too narrow for the label after the text: it goes to the next row whole.
        let (lines, cursor) = editor.render(12, &theme, "> ");
        let plain: Vec<String> = lines.iter().map(|line| text::strip_ansi(line)).collect();
        assert_eq!(plain, ["> ab ", "  [Image #1]", "  "]);
        assert_eq!(cursor, (2, 2));
        for width in 1..30 {
            for line in editor.render(width, &theme, "> ").0 {
                assert!(text::width(&line) <= width, "width {width}: {line:?}");
            }
        }

        // Up and down keep the column on the screen, where the label is ten cells wide.
        let mut editor = Editor::new();
        editor.insert_atom("Image");
        typed(&mut editor, "xy");
        editor.insert_newline();
        typed(&mut editor, "0123456789ab");
        editor.handle_key(&key(KeyCode::Left));
        editor.handle_key(&key(KeyCode::Up));
        assert_eq!(editor.render(40, &theme, "").1, (0, 11));
        editor.handle_key(&key(KeyCode::Home));
        editor.handle_key(&key(KeyCode::Down));
        assert_eq!(editor.render(40, &theme, "").1, (1, 0));
    }
}
