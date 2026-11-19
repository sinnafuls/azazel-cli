//! Vim-style editing for the prompt [`Editor`]: an insert mode that is the editor as it is, and a
//! normal mode with motions, counts and the operators people reach for in a prompt.
//!
//! Covered: `h j k l w b e 0 ^ $ gg G`, counts, `i a I A o O`, `x`, `d`, `c` and `y` with a
//! motion or doubled (`dd`, `cc`, `yy`), `D`, `C`, `p`, `P`, `u` and `ctrl+r`. There is no visual
//! mode, no `.` repeat and no register besides the editor's kill ring.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

use super::editor::Editor;

/// Largest count a command takes; more is a typo, not an intention.
const MAX_COUNT: usize = 9999;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Insert,
    Normal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Delete,
    Change,
    Yank,
}

/// Where a motion leads. `inclusive` motions take the character at the target with them when an
/// operator uses them (`e`); `linewise` ones make the operator work on whole lines (`G`).
struct Motion {
    target: usize,
    inclusive: bool,
    linewise: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punctuation,
}

fn class(grapheme: &str) -> Class {
    match grapheme.chars().next() {
        Some(c) if c.is_whitespace() => Class::Space,
        Some(c) if c.is_alphanumeric() || c == '_' => Class::Word,
        _ => Class::Punctuation,
    }
}

/// The modal state that sits in front of an editor.
#[derive(Debug, Default)]
pub struct Vim {
    mode: Mode,
    count: Option<usize>,
    /// An operator waiting for its motion, with the count typed before it.
    operator: Option<(Operator, usize)>,
    /// `g` was pressed and waits for the second `g`.
    g: bool,
    /// The last text cut or copied as whole lines; pasting it puts it on lines of its own.
    linewise: Option<String>,
}

impl Vim {
    /// Starts in insert mode, where the editor behaves as it does without Vim keys.
    pub fn new() -> Vim {
        Vim::default()
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// What the rule above the input shows: the mode, and a command that is half typed.
    pub fn label(&self) -> Option<String> {
        if self.mode == Mode::Insert {
            return None;
        }
        let mut pending = String::new();
        if let Some((operator, count)) = self.operator {
            if count > 1 {
                pending.push_str(&count.to_string());
            }
            pending.push(match operator {
                Operator::Delete => 'd',
                Operator::Change => 'c',
                Operator::Yank => 'y',
            });
        }
        if let Some(count) = self.count {
            pending.push_str(&count.to_string());
        }
        if self.g {
            pending.push('g');
        }
        Some(if pending.is_empty() {
            "-- NORMAL --".to_string()
        } else {
            format!("-- NORMAL -- {pending}")
        })
    }

    /// Handles a key. `false` leaves it to the caller: every key in insert mode except `esc`,
    /// and in normal mode `esc` with nothing pending, `enter` and the chords with modifiers.
    pub fn handle_key(&mut self, editor: &mut Editor, key: &KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if self.mode == Mode::Insert {
            if key.code != KeyCode::Esc || ctrl || alt {
                return false;
            }
            self.mode = Mode::Normal;
            // Like Vim, leaving insert mode steps back onto the character just typed.
            let at = editor.cursor();
            if at > editor.line_start(at) {
                editor.move_to(editor.prev_boundary(at));
            }
            return true;
        }
        match key.code {
            KeyCode::Esc => {
                let pending = self.count.is_some() || self.operator.is_some() || self.g;
                self.reset();
                pending
            }
            KeyCode::Char('r') if ctrl && !alt => {
                for _ in 0..self.count.take().unwrap_or(1) {
                    editor.redo();
                }
                self.reset();
                self.rest(editor);
                true
            }
            // Control with alt is how AltGr characters arrive on Windows.
            KeyCode::Char(c) if ctrl == alt => {
                let shifted = key.modifiers.contains(KeyModifiers::SHIFT) && c.is_ascii_lowercase();
                self.command(editor, if shifted { c.to_ascii_uppercase() } else { c });
                true
            }
            KeyCode::Backspace if !ctrl && !alt => {
                self.command(editor, 'h');
                true
            }
            KeyCode::Delete if !ctrl && !alt => {
                self.command(editor, 'x');
                true
            }
            // The arrows move by lines like `j` and `k`; the jump to the start and end of the
            // text is for insert mode, where the cursor may rest after the last character.
            KeyCode::Up | KeyCode::Down if !ctrl && !alt => {
                for _ in 0..self.count.unwrap_or(1) {
                    let moved = if key.code == KeyCode::Up {
                        editor.move_up()
                    } else {
                        editor.move_down()
                    };
                    if !moved {
                        break;
                    }
                }
                self.reset();
                self.rest(editor);
                true
            }
            _ => false,
        }
    }

    fn reset(&mut self) {
        self.count = None;
        self.operator = None;
        self.g = false;
    }

    /// In normal mode the cursor rests on a character, never after the last one of a line.
    fn rest(&self, editor: &mut Editor) {
        if self.mode != Mode::Normal {
            return;
        }
        let at = editor.cursor();
        if at == editor.line_end(at) && at > editor.line_start(at) {
            editor.move_to(editor.prev_boundary(at));
        }
    }

    fn insert(&mut self) {
        self.mode = Mode::Insert;
        self.reset();
    }

    fn command(&mut self, editor: &mut Editor, c: char) {
        if let Some(digit) = c.to_digit(10)
            && (digit != 0 || self.count.is_some())
            && !self.g
        {
            let count = self.count.unwrap_or(0) * 10 + digit as usize;
            self.count = Some(count.min(MAX_COUNT));
            return;
        }
        if c == 'g' && !self.g {
            self.g = true;
            return;
        }
        let typed = self.count.take();
        let count = typed.unwrap_or(1);
        let second_g = std::mem::take(&mut self.g);
        if second_g && c != 'g' {
            return self.reset();
        }
        if let Some((operator, before)) = self.operator.take() {
            let count = typed.map_or(before, |n| n * before);
            self.operate(editor, operator, c, count, typed, second_g);
            self.reset();
            return self.rest(editor);
        }
        let at = editor.cursor();
        match c {
            'd' => self.operator = Some((Operator::Delete, count)),
            'c' => self.operator = Some((Operator::Change, count)),
            'y' => self.operator = Some((Operator::Yank, count)),
            'i' => self.insert(),
            'a' => {
                if at < editor.line_end(at) {
                    editor.move_to(editor.next_boundary(at));
                }
                self.insert();
            }
            'I' => {
                editor.move_to(first_non_blank(editor, at));
                self.insert();
            }
            'A' => {
                editor.move_to(editor.line_end(at));
                self.insert();
            }
            'o' => {
                editor.insert_at(editor.line_end(at), "\n");
                self.insert();
            }
            'O' => {
                let start = editor.line_start(at);
                editor.insert_at(start, "\n");
                editor.move_to(start);
                self.insert();
            }
            'x' => {
                let mut end = at;
                for _ in 0..count {
                    if end >= editor.line_end(at) {
                        break;
                    }
                    end = editor.next_boundary(end);
                }
                editor.remove(at, end, true);
            }
            'D' => self.operate(editor, Operator::Delete, '$', count, typed, false),
            'C' => self.operate(editor, Operator::Change, '$', count, typed, false),
            'p' | 'P' => {
                for _ in 0..count {
                    self.paste(editor, c == 'p');
                }
            }
            'u' => {
                for _ in 0..count {
                    editor.undo();
                }
            }
            'j' | 'k' => {
                for _ in 0..count {
                    let moved = if c == 'j' {
                        editor.move_down()
                    } else {
                        editor.move_up()
                    };
                    if !moved {
                        break;
                    }
                }
            }
            _ => {
                if let Some(motion) = motion(editor, c, count, typed, second_g) {
                    editor.move_to(motion.target);
                }
            }
        }
        self.rest(editor);
    }

    /// Applies an operator to what the next key selects: its own letter for whole lines, or a
    /// motion.
    fn operate(
        &mut self,
        editor: &mut Editor,
        operator: Operator,
        key: char,
        count: usize,
        typed: Option<usize>,
        second_g: bool,
    ) {
        let at = editor.cursor();
        let doubled = matches!(
            (operator, key),
            (Operator::Delete, 'd') | (Operator::Change, 'c') | (Operator::Yank, 'y')
        );
        let (start, end, linewise) = if doubled {
            let mut end = editor.line_end(at);
            for _ in 1..count {
                if end >= editor.text().len() {
                    break;
                }
                end = editor.line_end(end + 1);
            }
            (editor.line_start(at), end, true)
        } else {
            // `cw` on a word changes to its end and keeps the space after it, as in Vim.
            let on_word = editor.text()[at..]
                .graphemes(true)
                .next()
                .is_some_and(|g| class(g) != Class::Space);
            let key = if operator == Operator::Change && key == 'w' && on_word {
                'e'
            } else {
                key
            };
            let Some(motion) = motion(editor, key, count, typed, second_g) else {
                return;
            };
            let (from, mut to) = (at.min(motion.target), at.max(motion.target));
            if motion.linewise {
                (editor.line_start(from), editor.line_end(to), true)
            } else {
                if motion.inclusive && to < editor.text().len() {
                    to = editor.next_boundary(to);
                }
                (from, to, false)
            }
        };
        let text = editor.text()[start..end].to_string();
        if linewise {
            self.linewise = Some(text.clone());
        }
        match operator {
            Operator::Yank => {
                editor.push_kill(text);
                editor.move_to(if linewise { at } else { start });
            }
            Operator::Change => {
                editor.remove(start, end, true);
                self.insert();
            }
            Operator::Delete if linewise => {
                // The line goes with one of its line breaks: the one after it, or for the last
                // line the one before.
                let total = editor.text().len();
                let (from, to) = if end < total {
                    (start, end + 1)
                } else if start > 0 {
                    (start - 1, end)
                } else {
                    (start, end)
                };
                editor.push_kill(text);
                editor.remove(from, to, false);
                let line = editor.line_start(editor.cursor().min(editor.text().len()));
                let line = if from < start {
                    editor.line_start(from)
                } else {
                    line
                };
                editor.move_to(first_non_blank(editor, line));
            }
            Operator::Delete => editor.remove(start, end, true),
        }
    }

    /// Inserts the newest kill after the cursor (`p`) or before it (`P`); text that was cut as
    /// whole lines goes on a line of its own.
    fn paste(&mut self, editor: &mut Editor, after: bool) {
        let Some(text) = editor.newest_kill().map(str::to_string) else {
            return;
        };
        let at = editor.cursor();
        if self.linewise.as_deref() == Some(text.as_str()) {
            if after {
                let end = editor.line_end(at);
                editor.insert_at(end, &format!("\n{text}"));
                editor.move_to(first_non_blank(editor, end + 1));
            } else {
                let start = editor.line_start(at);
                editor.insert_at(start, &format!("{text}\n"));
                editor.move_to(first_non_blank(editor, start));
            }
            return;
        }
        let at = if after && at < editor.line_end(at) {
            editor.next_boundary(at)
        } else {
            at
        };
        editor.insert_at(at, &text);
        // The cursor ends on the last character of what was pasted.
        editor.move_to(editor.prev_boundary(editor.cursor()));
    }
}

/// The first character of the line containing `pos` that is not a space, or its end.
fn first_non_blank(editor: &Editor, pos: usize) -> usize {
    let start = editor.line_start(pos);
    let end = editor.line_end(pos);
    let line = &editor.text()[start..end];
    start + line.len() - line.trim_start().len()
}

/// Start of line number `number` (1-based), clamped to the last line.
fn line_number_start(text: &str, number: usize) -> usize {
    let mut start = 0;
    for _ in 1..number {
        match text[start..].find('\n') {
            Some(at) => start += at + 1,
            None => break,
        }
    }
    start
}

/// Resolves a motion key from the cursor. `typed` is the count as typed, which `G` and `gg` read
/// as a line number.
fn motion(editor: &Editor, key: char, count: usize, typed: Option<usize>, second_g: bool) -> Option<Motion> {
    let text = editor.text();
    let at = editor.cursor();
    let plain = |target| {
        Some(Motion {
            target,
            inclusive: false,
            linewise: false,
        })
    };
    match key {
        'h' => {
            let start = editor.line_start(at);
            let mut target = at;
            for _ in 0..count {
                if target <= start {
                    break;
                }
                target = editor.prev_boundary(target);
            }
            plain(target)
        }
        'l' | ' ' => {
            let end = editor.line_end(at);
            let mut target = at;
            for _ in 0..count {
                if target >= end {
                    break;
                }
                target = editor.next_boundary(target);
            }
            plain(target)
        }
        '0' => plain(editor.line_start(at)),
        '^' => plain(first_non_blank(editor, at)),
        '$' => plain(editor.line_end(at)),
        'w' => plain((0..count).fold(at, |pos, _| next_word(text, pos))),
        'b' => plain((0..count).fold(at, |pos, _| previous_word(text, pos))),
        'e' => Some(Motion {
            target: (0..count).fold(at, |pos, _| word_end(text, pos)),
            inclusive: true,
            linewise: false,
        }),
        'G' => Some(Motion {
            target: line_number_start(text, typed.unwrap_or(usize::MAX)),
            inclusive: false,
            linewise: true,
        }),
        'g' if second_g => Some(Motion {
            target: line_number_start(text, typed.unwrap_or(1)),
            inclusive: false,
            linewise: true,
        }),
        _ => None,
    }
}

/// Start of the next word: past the rest of this one and the space after it.
fn next_word(text: &str, from: usize) -> usize {
    let mut graphemes = text[from..].grapheme_indices(true).peekable();
    let Some((_, first)) = graphemes.peek().copied() else {
        return from;
    };
    let kind = class(first);
    if kind != Class::Space {
        while graphemes.next_if(|(_, g)| class(g) == kind).is_some() {}
    }
    while graphemes.next_if(|(_, g)| class(g) == Class::Space).is_some() {}
    graphemes.peek().map_or(text.len(), |(i, _)| from + i)
}

/// Start of the word before `from`, or of this one when the cursor is inside it.
fn previous_word(text: &str, from: usize) -> usize {
    let mut graphemes = text[..from].grapheme_indices(true).rev().peekable();
    while graphemes.next_if(|(_, g)| class(g) == Class::Space).is_some() {}
    let Some((mut start, first)) = graphemes.next() else {
        return 0;
    };
    let kind = class(first);
    while let Some((i, _)) = graphemes.next_if(|(_, g)| class(g) == kind) {
        start = i;
    }
    start
}

/// The last character of this word, or of the next one when the cursor is already on a last
/// character.
fn word_end(text: &str, from: usize) -> usize {
    let mut graphemes = text[from..].grapheme_indices(true).peekable();
    // Always move: the end of the word the cursor is on is found from its next character.
    if graphemes.next().is_none() {
        return from;
    }
    while graphemes.next_if(|(_, g)| class(g) == Class::Space).is_some() {}
    let Some((mut end, first)) = graphemes.next() else {
        return text.grapheme_indices(true).next_back().map_or(0, |(i, _)| i);
    };
    let kind = class(first);
    while let Some((i, _)) = graphemes.next_if(|(_, g)| class(g) == kind) {
        end = i;
    }
    from + end
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An editor holding `text` with the cursor at its start, in normal mode.
    fn normal(text: &str) -> (Vim, Editor) {
        let mut editor = Editor::new();
        editor.set_text(text);
        editor.move_to(0);
        let mut vim = Vim::new();
        vim.mode = Mode::Normal;
        (vim, editor)
    }

    fn keys(vim: &mut Vim, editor: &mut Editor, typed: &str) {
        for c in typed.chars() {
            let event = match c {
                '\x1b' => KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                // Stands for ctrl+r in these tests.
                '\x12' => KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
                c => KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            };
            if !vim.handle_key(editor, &event) {
                editor.handle_key(&event);
            }
        }
    }

    /// The text after typing `typed` in normal mode on `text`, and where the cursor ended.
    fn run(text: &str, typed: &str) -> (String, usize) {
        let (mut vim, mut editor) = normal(text);
        keys(&mut vim, &mut editor, typed);
        (editor.text().to_string(), editor.cursor())
    }

    #[test]
    fn escape_leaves_insert_mode_and_falls_through_in_normal_mode() {
        let mut editor = Editor::new();
        let mut vim = Vim::new();
        assert_eq!(vim.mode(), Mode::Insert);
        assert_eq!(vim.label(), None);
        keys(&mut vim, &mut editor, "hello");
        assert_eq!(editor.text(), "hello", "insert mode types");

        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(vim.handle_key(&mut editor, &escape));
        assert_eq!(vim.mode(), Mode::Normal);
        assert_eq!(
            editor.cursor(),
            4,
            "the cursor steps back onto the last character"
        );
        assert_eq!(vim.label().as_deref(), Some("-- NORMAL --"));

        // A half-typed command is dropped by escape; with nothing pending the key is the caller's.
        keys(&mut vim, &mut editor, "2d");
        assert_eq!(vim.label().as_deref(), Some("-- NORMAL -- 2d"));
        assert!(vim.handle_key(&mut editor, &escape));
        assert_eq!(vim.label().as_deref(), Some("-- NORMAL --"));
        assert!(!vim.handle_key(&mut editor, &escape));
        // Enter and chords are the caller's too; plain letters never type.
        assert!(!vim.handle_key(&mut editor, &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(!vim.handle_key(
            &mut editor,
            &KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ));
        keys(&mut vim, &mut editor, "zq");
        assert_eq!(editor.text(), "hello");
    }

    #[test]
    fn motions_move_by_character_word_and_line() {
        let text = "foo bar.baz  qux\nsecond line\nthird";
        assert_eq!(run(text, "l").1, 1);
        assert_eq!(run(text, "3l").1, 3);
        assert_eq!(run(text, "3lh").1, 2);
        assert_eq!(run(text, "h").1, 0, "h stops at the start of the line");
        assert_eq!(run(text, "w").1, 4);
        assert_eq!(run(text, "ww").1, 7, "punctuation is a word of its own");
        assert_eq!(run(text, "3w").1, 8);
        assert_eq!(run(text, "4w").1, 13);
        assert_eq!(run(text, "5w").1, 17, "w crosses to the next line");
        assert_eq!(run(text, "4wb").1, 8);
        assert_eq!(run(text, "4w2b").1, 7);
        assert_eq!(run(text, "e").1, 2);
        assert_eq!(run(text, "ee").1, 6);
        assert_eq!(run(text, "$").1, 15, "the cursor rests on the last character");
        assert_eq!(run(text, "$0").1, 0);
        assert_eq!(run("   indented", "$^").1, 3);
        assert_eq!(run(text, "j").1, 17);
        assert_eq!(run(text, "2j").1, 29);
        assert_eq!(run(text, "2jk").1, 17);
        assert_eq!(run(text, "G").1, 29);
        assert_eq!(run(text, "Ggg").1, 0);
        assert_eq!(run(text, "2G").1, 17);
        assert_eq!(run(text, "3gg").1, 29);
        assert_eq!(run(text, "wj").1, 21, "j keeps the column");
    }

    #[test]
    fn arrows_in_normal_mode_move_by_lines_and_never_jump_to_the_text_ends() {
        let (mut vim, mut editor) = normal("one\ntwo");
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert!(vim.handle_key(&mut editor, &up));
        assert_eq!(editor.cursor(), 0);
        // On the first line, normal mode stays put like `k`.
        assert!(vim.handle_key(&mut editor, &up));
        assert_eq!(editor.cursor(), 0);
        assert!(vim.handle_key(&mut editor, &down));
        assert_eq!(editor.cursor(), 4);
        assert!(vim.handle_key(&mut editor, &down));
        assert_eq!(editor.cursor(), 4, "the last line does not jump to the end");
    }

    #[test]
    fn delete_operators_cut_characters_words_and_lines() {
        assert_eq!(run("hello", "x"), ("ello".to_string(), 0));
        assert_eq!(run("hello", "3x"), ("lo".to_string(), 0));
        assert_eq!(run("hi", "9x"), (String::new(), 0), "x stays on its line");
        assert_eq!(run("one two three", "dw"), ("two three".to_string(), 0));
        assert_eq!(run("one two three", "2dw"), ("three".to_string(), 0));
        assert_eq!(run("one two three", "d2w"), ("three".to_string(), 0));
        assert_eq!(run("one two three", "wd$"), ("one ".to_string(), 3));
        assert_eq!(run("one two three", "wD"), ("one ".to_string(), 3));
        assert_eq!(run("one two three", "de"), (" two three".to_string(), 0));
        assert_eq!(run("one two three", "wwdb"), ("one three".to_string(), 4));
        assert_eq!(run("a\nb\nc", "dd"), ("b\nc".to_string(), 0));
        assert_eq!(run("a\nb\nc", "jdd"), ("a\nc".to_string(), 2));
        assert_eq!(
            run("a\nb\nc", "jjdd"),
            ("a\nb".to_string(), 2),
            "the last line goes with the break before it"
        );
        assert_eq!(run("a\nb\nc", "2dd"), ("c".to_string(), 0));
        assert_eq!(run("only", "dd"), (String::new(), 0));
        assert_eq!(run("a\nb\nc", "dG"), (String::new(), 0));
        assert_eq!(run("a\nb\nc", "jdgg"), ("c".to_string(), 0));
    }

    #[test]
    fn change_operators_cut_and_enter_insert_mode() {
        assert_eq!(run("one two", "cwX"), ("X two".to_string(), 1));
        assert_eq!(run("one two", "wCX"), ("one X".to_string(), 5));
        assert_eq!(run("a\nold line\nc", "jccnew"), ("a\nnew\nc".to_string(), 5));
        let (mut vim, mut editor) = normal("word");
        keys(&mut vim, &mut editor, "cw");
        assert_eq!(vim.mode(), Mode::Insert);
    }

    #[test]
    fn yank_and_paste_use_the_kill_ring_and_keep_lines_whole() {
        assert_eq!(run("a\nb", "yyp"), ("a\na\nb".to_string(), 2));
        assert_eq!(run("a\nb", "jyyP"), ("a\nb\nb".to_string(), 2));
        assert_eq!(run("a\nb", "ddp"), ("b\na".to_string(), 2));
        assert_eq!(run("ab", "xp"), ("ba".to_string(), 1));
        assert_eq!(run("one two", "dw$p"), ("twoone ".to_string(), 6));
        assert_eq!(run("ab", "x3p"), ("baaa".to_string(), 3));
        assert_eq!(run("one two", "ywwP"), ("one one two".to_string(), 7));

        // What the emacs keys cut can be pasted too.
        let (mut vim, mut editor) = normal("cut me");
        editor.handle_key(&KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        keys(&mut vim, &mut editor, "P");
        assert_eq!(editor.text(), "cut me");
    }

    #[test]
    fn undo_and_redo_step_through_commands() {
        assert_eq!(run("one two three", "dwdwu").0, "two three");
        assert_eq!(run("one two three", "dwdwuu").0, "one two three");
        assert_eq!(run("one two three", "dwdw2u").0, "one two three");
        assert_eq!(run("one two three", "dwdwuu\x12").0, "two three");
        assert_eq!(run("one two three", "dwdwuu\x12\x12").0, "three");
    }
}
