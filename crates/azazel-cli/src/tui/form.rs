//! The form an agent's questions are answered in: one question, or several with a row of tabs
//! to move between them and a last tab that shows the answers before they are sent.

use azazel_core::{AskKind, Excerpt, Question};
use azazel_tui::components::{code, diff};
use azazel_tui::{Editor, Glyph, Hint, Line, Role, SelectItem, SelectList, Theme, text};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::overlay::Widget;

/// The most rows the typed answer of a question takes.
const EDITOR_ROWS: usize = 3;
/// Rows of a list a long body always leaves room for.
const KEPT_FOR_LIST: usize = 6;
/// The longest name a question without a `header` gets in the row of tabs.
const TAB_WIDTH: usize = 16;

/// One question and what has been answered to it so far.
struct Field {
    question: Question,
    /// The rows of a choice; empty for a typed answer.
    list: SelectList,
    /// Which options of a `Multiselect` are ticked.
    picked: Vec<bool>,
    /// The answer being typed: to a `Text` question, or instead of the options of a `Select`.
    editor: Editor,
    /// The user chose to type an answer instead of taking an option.
    typing: bool,
    /// `None` until the question is answered.
    answer: Option<Vec<String>>,
}

/// What a key did to a form.
pub enum Step {
    Pending,
    Cancelled,
    /// The answers, one list per question in their order; a question left open has none.
    Done(Vec<Vec<String>>),
}

pub struct Form {
    fields: Vec<Field>,
    /// The tab in view: a question, or `fields.len()` for the tab that sends the answers.
    at: usize,
    /// The row that lets the user type an answer, drawn in the symbols in use: `Other…`.
    other: String,
}

impl Field {
    fn new(question: Question, other: &str) -> Field {
        let picked = vec![false; question.options.len()];
        let mut field = Field {
            list: SelectList::new(Vec::new()),
            picked,
            editor: Editor::new(),
            typing: false,
            answer: None,
            question,
        };
        field.list.set_items(field.items(other));
        // A stray Enter gives the answer the agent would give.
        let offered = field.offered();
        let start = field
            .question
            .recommended
            .as_ref()
            .and_then(|recommended| offered.iter().position(|option| option == recommended));
        field.list.set_cursor(start.unwrap_or(0));
        field
    }

    /// The answers the rows of the list stand for, in their order.
    fn offered(&self) -> Vec<String> {
        match self.question.kind {
            AskKind::Confirm => vec!["yes".to_string(), "no".to_string()],
            AskKind::Select | AskKind::Multiselect => self.question.options.clone(),
            AskKind::Text => Vec::new(),
        }
    }

    fn items(&self, other: &str) -> Vec<SelectItem> {
        let question = &self.question;
        let recommended = |answer: &str| match &question.recommended {
            Some(recommended) if recommended == answer => "recommended",
            _ => "",
        };
        match question.kind {
            AskKind::Text => Vec::new(),
            AskKind::Confirm => vec![
                SelectItem::new("Yes", recommended("yes")),
                SelectItem::new("No", recommended("no")),
            ],
            AskKind::Select => {
                let mut items: Vec<SelectItem> = question
                    .options
                    .iter()
                    .map(|option| SelectItem::new(option.as_str(), recommended(option)))
                    .collect();
                if question.allow_freeform {
                    items.push(SelectItem::new(other, "type your own answer"));
                }
                items
            }
            AskKind::Multiselect => {
                let mut items: Vec<SelectItem> = question
                    .options
                    .iter()
                    .zip(&self.picked)
                    .map(|(option, on)| SelectItem::new(option.as_str(), recommended(option)).checked(*on))
                    .collect();
                items.push(SelectItem::new("Done", "confirm the selection"));
                items
            }
        }
    }

    /// Whether keys are typed into the editor rather than moving in the list.
    fn types(&self) -> bool {
        self.question.kind == AskKind::Text || self.typing
    }

    /// Acts on row `index` of the list. True when the question is answered by it.
    fn choose(&mut self, index: usize, other: &str) -> bool {
        let question = &self.question;
        match question.kind {
            AskKind::Text => false,
            AskKind::Confirm | AskKind::Select => match self.offered().get(index) {
                Some(answer) => {
                    self.answer = Some(vec![answer.clone()]);
                    true
                }
                None => {
                    // "Other…": the answer is typed instead.
                    self.typing = question.allow_freeform && index == question.options.len();
                    false
                }
            },
            AskKind::Multiselect => match self.picked.get_mut(index) {
                Some(flag) => {
                    *flag = !*flag;
                    self.list.set_items(self.items(other));
                    self.list.set_cursor(index);
                    false
                }
                None => {
                    let chosen = question
                        .options
                        .iter()
                        .zip(&self.picked)
                        .filter(|(_, on)| **on)
                        .map(|(option, _)| option.clone())
                        .collect();
                    self.answer = Some(chosen);
                    true
                }
            },
        }
    }

    /// Takes what is typed as the answer. Nothing typed leaves the question open.
    fn typed(&mut self) {
        let typed = self.editor.text().trim().to_string();
        self.answer = (!typed.is_empty()).then(|| vec![typed]);
    }

    /// The name of the question in the row of tabs.
    fn tab(&self, ellipsis: &str) -> String {
        match &self.question.header {
            Some(header) => text::sanitize(header),
            None => {
                let line = text::sanitize(self.question.text.lines().next().unwrap_or_default());
                text::truncate(&line, TAB_WIDTH, ellipsis)
            }
        }
    }

    /// What the question is about and the question itself, as rows `width` wide.
    fn body(&self, width: usize, theme: &Theme) -> Vec<Line> {
        let question = &self.question;
        let mut lines: Vec<Line> = Vec::new();
        let wrapped = |raw: &str, lines: &mut Vec<Line>| {
            for line in raw.lines() {
                match line.is_empty() {
                    true => lines.push(String::new()),
                    false => lines.extend(text::wrap(&text::sanitize(line), width)),
                }
            }
        };
        if let Some(context) = &question.context {
            wrapped(context, &mut lines);
            lines.push(String::new());
        }
        match &question.excerpt {
            Some(Excerpt::Code(source)) => {
                lines.extend(code::listing(source.trim_end(), "", 1, width, theme));
                lines.push(String::new());
            }
            Some(Excerpt::Diff(patch)) => {
                lines.extend(diff::render_unified(patch.trim_end(), width, theme));
                lines.push(String::new());
            }
            None => {}
        }
        wrapped(&question.text, &mut lines);
        lines
    }
}

impl Form {
    /// A form for `questions`. `other` is the row that lets the user type an answer where a
    /// choice allows it.
    pub fn new(questions: Vec<Question>, other: &str) -> Form {
        Form {
            fields: questions
                .into_iter()
                .map(|question| Field::new(question, other))
                .collect(),
            at: 0,
            other: other.to_string(),
        }
    }

    /// Every option of every list that takes several ticked, and the row that confirms them
    /// under the cursor: a form whose usual answer is "all of it".
    pub fn all_picked(mut self) -> Form {
        let other = self.other.clone();
        for field in &mut self.fields {
            if field.question.kind == AskKind::Multiselect {
                field.picked.fill(true);
                field.list.set_items(field.items(&other));
                field.list.set_cursor(field.picked.len());
            }
        }
        self
    }

    /// Whether the form has more than one question, and with them tabs and a tab to send from.
    fn several(&self) -> bool {
        self.fields.len() > 1
    }

    fn field(&self) -> Option<&Field> {
        self.fields.get(self.at)
    }

    /// Whether the tab in view takes typed text: the UI offers the user's own editor for it.
    pub fn is_typing(&self) -> bool {
        self.field().is_some_and(Field::types)
    }

    /// The editor of the tab in view, when it takes typed text.
    pub fn editor_mut(&mut self) -> Option<&mut Editor> {
        self.fields
            .get_mut(self.at)
            .filter(|field| field.types())
            .map(|field| &mut field.editor)
    }

    fn answers(&self) -> Vec<Vec<String>> {
        self.fields
            .iter()
            .map(|field| field.answer.clone().unwrap_or_default())
            .collect()
    }

    /// After an answer: a single question is done; among several the next one still open
    /// comes into view, or the tab that sends them once none is.
    fn advance(&mut self) -> Step {
        if !self.several() {
            return Step::Done(self.answers());
        }
        let count = self.fields.len();
        let open = (1..=count)
            .map(|step| (self.at + step) % count)
            .find(|at| self.fields[*at].answer.is_none());
        self.at = open.unwrap_or(count);
        Step::Pending
    }

    fn turn(&mut self, forward: bool) {
        // The tabs of the questions, and the one that sends.
        let tabs = self.fields.len() + 1;
        self.at = match forward {
            true => (self.at + 1) % tabs,
            false => (self.at + tabs - 1) % tabs,
        };
    }

    pub fn on_key(&mut self, key: &KeyEvent) -> Step {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return Step::Cancelled;
        }
        let several = self.several();
        match key.code {
            KeyCode::Tab if several => {
                self.turn(true);
                return Step::Pending;
            }
            KeyCode::BackTab if several => {
                self.turn(false);
                return Step::Pending;
            }
            _ => {}
        }
        let other = self.other.clone();
        let Some(field) = self.fields.get_mut(self.at) else {
            // The tab that sends the answers.
            return match key.code {
                KeyCode::Esc => Step::Cancelled,
                KeyCode::Enter => Step::Done(self.answers()),
                KeyCode::Left => {
                    self.turn(false);
                    Step::Pending
                }
                KeyCode::Right => {
                    self.turn(true);
                    Step::Pending
                }
                _ => Step::Pending,
            };
        };
        if field.types() {
            match key.code {
                // Back to the options; a question that only takes text is dismissed.
                KeyCode::Esc if field.typing => field.typing = false,
                KeyCode::Esc => return Step::Cancelled,
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    field.editor.insert_newline()
                }
                KeyCode::Enter => {
                    field.typed();
                    return self.advance();
                }
                _ => {
                    field.editor.handle_key(key);
                }
            }
            return Step::Pending;
        }
        let chosen = match key.code {
            KeyCode::Esc => return Step::Cancelled,
            KeyCode::Up => {
                field.list.move_up();
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                field.list.move_down();
                None
            }
            KeyCode::PageUp => {
                field.list.page_up();
                None
            }
            KeyCode::PageDown => {
                field.list.page_down();
                None
            }
            KeyCode::Left if several => {
                self.turn(false);
                return Step::Pending;
            }
            KeyCode::Right if several => {
                self.turn(true);
                return Step::Pending;
            }
            KeyCode::Enter => field.list.selected(),
            KeyCode::Char(' ') if field.question.kind == AskKind::Multiselect => {
                field.list.selected().filter(|index| *index < field.picked.len())
            }
            // Digits pick a row directly.
            KeyCode::Char(c @ '1'..='9') => {
                let index = c as usize - '1' as usize;
                (index < field.list.items().len()).then_some(index)
            }
            _ => None,
        };
        match chosen {
            Some(index) if field.choose(index, &other) => self.advance(),
            _ => Step::Pending,
        }
    }

    pub fn on_paste(&mut self, pasted: &str) {
        if let Some(editor) = self.editor_mut() {
            editor.insert_str(pasted);
        }
    }

    /// The row of tabs and the blank row under it; nothing for a single question. The tab in
    /// view is bold in the accent colour, an answered one carries a tick.
    pub fn tabs(&self, width: usize, theme: &Theme) -> Vec<Line> {
        if !self.several() {
            return Vec::new();
        }
        let ellipsis = theme.glyph(Glyph::Ellipsis);
        let tab = |at: usize, name: &str, answered: bool| {
            let tick = match answered {
                true => format!(" {}", theme.paint(Role::Success, theme.glyph(Glyph::Success))),
                false => String::new(),
            };
            match at == self.at {
                true => format!("\x1b[1m{}{name}\x1b[22m{tick}", theme.fg(Role::Accent)),
                false => format!("{}{tick}", theme.paint(Role::Muted, name)),
            }
        };
        let row = |named: bool| {
            let mut parts: Vec<String> = self
                .fields
                .iter()
                .enumerate()
                .map(|(at, field)| {
                    let name = match named {
                        true => format!("{} {}", at + 1, field.tab(ellipsis)),
                        false => (at + 1).to_string(),
                    };
                    tab(at, &name, field.answer.is_some())
                })
                .collect();
            parts.push(tab(self.fields.len(), "Send", false));
            parts.join(&theme.paint(Role::BorderMuted, "  "))
        };
        // The names give way to the numbers alone where the row is too narrow for them.
        let mut line = row(true);
        if text::width(&line) > width {
            line = row(false);
        }
        vec![text::truncate(&line, width, ellipsis), String::new()]
    }

    /// The text of the tab in view: what the question is about and the question, or on the
    /// last tab the answers as they will be sent.
    pub fn body(&self, width: usize, theme: &Theme) -> Vec<Line> {
        if let Some(field) = self.field() {
            return field.body(width, theme);
        }
        let ellipsis = theme.glyph(Glyph::Ellipsis);
        let mut lines = Vec::new();
        for (at, field) in self.fields.iter().enumerate() {
            let asked = text::sanitize(field.question.text.lines().next().unwrap_or_default());
            lines.push(text::truncate(&format!("{}. {asked}", at + 1), width, ellipsis));
            match field.answer.as_ref().filter(|answer| !answer.is_empty()) {
                Some(answer) => {
                    for line in answer.iter().flat_map(|answer| answer.lines()) {
                        for row in text::wrap(&text::sanitize(line), width.saturating_sub(3).max(1)) {
                            lines.push(format!("   {}", theme.paint(Role::Accent, &row)));
                        }
                    }
                }
                None => lines.push(format!("   {}", theme.paint(Role::Warning, "no answer"))),
            }
        }
        lines
    }

    /// The same text for the full-screen viewer, when the box cannot hold it.
    pub fn body_text(&self) -> String {
        let Some(field) = self.field() else {
            return String::new();
        };
        let question = &field.question;
        let mut parts: Vec<&str> = Vec::new();
        parts.extend(question.context.as_deref());
        parts.extend(question.excerpt.as_ref().map(|excerpt| excerpt.text().trim_end()));
        parts.push(&question.text);
        parts.join("\n\n")
    }

    /// Rows the list or the input keeps whatever the text above it needs.
    pub fn kept(&self) -> usize {
        match self.field() {
            None => 1,
            Some(field) if field.question.kind == AskKind::Text => EDITOR_ROWS,
            Some(field) => {
                field.list.len().clamp(1, KEPT_FOR_LIST) + if field.typing { EDITOR_ROWS } else { 0 }
            }
        }
    }

    /// The list or the input of the tab in view, in at most `room` rows.
    pub fn widget(&self, width: usize, room: usize, prompt: &str, theme: &Theme) -> Widget {
        let Some(field) = self.field() else {
            let open = self.fields.iter().filter(|field| field.answer.is_none()).count();
            let line = match open {
                0 => theme.paint(Role::Muted, "enter sends these answers"),
                1 => theme.paint(
                    Role::Warning,
                    "1 question is open; enter sends the answers as they are",
                ),
                open => theme.paint(
                    Role::Warning,
                    &format!("{open} questions are open; enter sends the answers as they are"),
                ),
            };
            return Widget {
                lines: vec![text::truncate(&line, width, theme.glyph(Glyph::Ellipsis))],
                cursor: None,
                scroll: None,
            };
        };
        if field.question.kind == AskKind::Text {
            let view = field
                .editor
                .view(width, room.clamp(1, EDITOR_ROWS), theme, prompt);
            return Widget {
                cursor: Some((usize::from(view.cursor.0), usize::from(view.cursor.1))),
                lines: view.lines,
                scroll: None,
            };
        }
        let typing_rows = if field.typing {
            EDITOR_ROWS.min(room / 2)
        } else {
            0
        };
        let view = field
            .list
            .view(width, room.saturating_sub(typing_rows).max(1), theme);
        let mut widget = Widget {
            scroll: view.scroll.map(|scroll| (0, scroll)),
            lines: view.lines,
            cursor: None,
        };
        if field.typing && typing_rows > 0 {
            let typed = field.editor.view(width, typing_rows, theme, prompt);
            widget.cursor = Some((
                widget.lines.len() + usize::from(typed.cursor.0),
                usize::from(typed.cursor.1),
            ));
            widget.lines.extend(typed.lines);
        }
        widget
    }

    /// The key hints for the bottom edge of the box.
    pub fn hints(&self, theme: &Theme) -> Vec<Hint> {
        let mut hints = Vec::new();
        let arrows = format!("{}{}", theme.glyph(Glyph::MoreUp), theme.glyph(Glyph::MoreDown));
        match self.field() {
            None => hints.push(Hint::new("enter", "send")),
            Some(field) if field.types() => {
                hints.push(Hint::new(
                    "enter",
                    if self.several() { "next" } else { "confirm" },
                ));
                if field.typing {
                    hints.push(Hint::new("esc", "options"));
                }
            }
            Some(field) if field.question.kind == AskKind::Multiselect => {
                hints.push(Hint::new(arrows, "move"));
                hints.push(Hint::new("enter", "tick"));
            }
            Some(_) => {
                hints.push(Hint::new(arrows, "move"));
                hints.push(Hint::new("enter", "pick"));
            }
        }
        if self.several() {
            hints.push(Hint::new("tab", "next question"));
        }
        hints
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEventKind, KeyEventState};

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn question(text: &str, kind: AskKind, options: &[&str]) -> Question {
        Question {
            kind,
            options: options.iter().map(|option| option.to_string()).collect(),
            ..Question::text(text)
        }
    }

    fn type_text(form: &mut Form, typed: &str) {
        for c in typed.chars() {
            assert!(matches!(form.on_key(&key(KeyCode::Char(c))), Step::Pending));
        }
    }

    fn done(step: Step) -> Vec<Vec<String>> {
        match step {
            Step::Done(answers) => answers,
            Step::Pending => panic!("the form is still open"),
            Step::Cancelled => panic!("the form was dismissed"),
        }
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|line| text::strip_ansi(line)).collect()
    }

    #[test]
    fn a_single_question_is_answered_by_the_first_pick() {
        let mut form = Form::new(
            vec![question("Which?", AskKind::Select, &["red", "blue"])],
            "Other…",
        );
        assert!(
            form.tabs(60, &Theme::dark()).is_empty(),
            "one question has no tabs"
        );
        assert!(matches!(form.on_key(&key(KeyCode::Down)), Step::Pending));
        assert_eq!(done(form.on_key(&key(KeyCode::Enter))), [["blue"]]);

        // A digit picks its row; a digit beyond the rows does nothing.
        let mut form = Form::new(
            vec![question("Which?", AskKind::Select, &["red", "blue"])],
            "Other…",
        );
        assert!(matches!(form.on_key(&key(KeyCode::Char('3'))), Step::Pending));
        assert_eq!(done(form.on_key(&key(KeyCode::Char('1')))), [["red"]]);

        let mut form = Form::new(vec![question("Sure?", AskKind::Confirm, &[])], "Other…");
        assert!(matches!(form.on_key(&key(KeyCode::Down)), Step::Pending));
        assert_eq!(done(form.on_key(&key(KeyCode::Enter))), [["no"]]);

        let mut form = Form::new(vec![Question::text("Why?")], "Other…");
        assert!(form.is_typing());
        type_text(&mut form, "because");
        assert_eq!(done(form.on_key(&key(KeyCode::Enter))), [["because"]]);

        // Nothing typed is no answer, and esc dismisses.
        let mut form = Form::new(vec![Question::text("Why?")], "Other…");
        assert_eq!(done(form.on_key(&key(KeyCode::Enter))), [Vec::<String>::new()]);
        let mut form = Form::new(vec![Question::text("Why?")], "Other…");
        assert!(matches!(form.on_key(&key(KeyCode::Esc)), Step::Cancelled));
    }

    #[test]
    fn the_selection_starts_on_the_recommended_option_which_says_so() {
        let mut asked = question("Which?", AskKind::Select, &["sqlite", "postgres", "mysql"]);
        asked.recommended = Some("postgres".to_string());
        let mut form = Form::new(vec![asked], "Other…");
        let theme = Theme::dark();
        let rows = plain(&form.widget(60, 8, "> ", &theme).lines);
        assert!(rows[0].starts_with("  sqlite"), "{rows:?}");
        assert!(
            rows[1].starts_with("▌ postgres") && rows[1].contains("recommended"),
            "{rows:?}"
        );
        assert!(!rows[2].contains("recommended"), "{rows:?}");
        assert_eq!(done(form.on_key(&key(KeyCode::Enter))), [["postgres"]]);

        let mut sure = question("Drop it?", AskKind::Confirm, &[]);
        sure.recommended = Some("no".to_string());
        let mut form = Form::new(vec![sure], "Other…");
        assert_eq!(done(form.on_key(&key(KeyCode::Enter))), [["no"]]);
    }

    #[test]
    fn several_options_are_ticked_and_confirmed_on_done() {
        let mut form = Form::new(
            vec![question("Which?", AskKind::Multiselect, &["a", "b", "c"])],
            "Other…",
        );
        let theme = Theme::dark();
        // Enter and space tick the row the selection is on, which stays there.
        assert!(matches!(form.on_key(&key(KeyCode::Enter)), Step::Pending));
        for _ in 0..2 {
            form.on_key(&key(KeyCode::Down));
        }
        assert!(matches!(form.on_key(&key(KeyCode::Char(' '))), Step::Pending));
        let rows = plain(&form.widget(40, 8, "> ", &theme).lines);
        assert_eq!(
            rows.iter().map(|row| row.trim_end()).collect::<Vec<_>>(),
            ["  ▣ a", "  □ b", "▌ ▣ c", "  Done  confirm the selection"]
        );
        // Ticking again unticks; space on Done does nothing.
        assert!(matches!(form.on_key(&key(KeyCode::Char(' '))), Step::Pending));
        form.on_key(&key(KeyCode::Down));
        assert!(matches!(form.on_key(&key(KeyCode::Char(' '))), Step::Pending));
        assert_eq!(done(form.on_key(&key(KeyCode::Enter))), [["a"]]);
    }

    #[test]
    fn other_takes_a_typed_answer_and_esc_returns_to_the_options() {
        let mut asked = question("Which?", AskKind::Select, &["red", "blue"]);
        asked.allow_freeform = true;
        let mut form = Form::new(vec![asked], "Other…");
        let theme = Theme::dark();
        let rows = plain(&form.widget(60, 8, "> ", &theme).lines);
        assert!(rows[2].contains("Other…") && rows[2].contains("type your own answer"));

        assert!(matches!(form.on_key(&key(KeyCode::Char('3'))), Step::Pending));
        assert!(form.is_typing());
        let widget = form.widget(60, 8, "> ", &theme);
        assert_eq!(widget.lines.len(), 4, "the input stands under the options");
        assert_eq!(widget.cursor, Some((3, 2)));
        // Esc leaves the input, not the question; digits are text while typing.
        assert!(matches!(form.on_key(&key(KeyCode::Esc)), Step::Pending));
        assert!(!form.is_typing());
        form.on_key(&key(KeyCode::Down));
        form.on_key(&key(KeyCode::Down));
        assert!(matches!(form.on_key(&key(KeyCode::Enter)), Step::Pending));
        type_text(&mut form, "teal 1");
        assert_eq!(done(form.on_key(&key(KeyCode::Enter))), [["teal 1"]]);
    }

    fn three() -> Form {
        let mut store = question("Which store?", AskKind::Select, &["sqlite", "postgres"]);
        store.header = Some("Store".to_string());
        let mut auth = question("Add sign-in now?", AskKind::Confirm, &[]);
        auth.header = Some("Auth".to_string());
        let notes = Question::text("Anything else I should know before starting?");
        Form::new(vec![store, auth, notes], "Other…")
    }

    #[test]
    fn answers_move_to_the_next_open_question_and_are_sent_from_the_last_tab() {
        let mut form = three();
        let theme = Theme::dark();
        let tabs = |form: &Form| plain(&form.tabs(70, &theme))[0].clone();
        assert_eq!(tabs(&form), "1 Store  2 Auth  3 Anything else I…  Send");

        // Answering the first brings the second into view; its tab has a tick now.
        assert!(matches!(form.on_key(&key(KeyCode::Enter)), Step::Pending));
        assert_eq!(tabs(&form), "1 Store ✔  2 Auth  3 Anything else I…  Send");
        assert_eq!(plain(&form.body(60, &theme)), ["Add sign-in now?"]);

        // Tab skips ahead without answering; the third is typed.
        assert!(matches!(form.on_key(&key(KeyCode::Tab)), Step::Pending));
        assert!(form.is_typing());
        type_text(&mut form, "ship it");
        // The only open question is the second: that is where the form goes.
        assert!(matches!(form.on_key(&key(KeyCode::Enter)), Step::Pending));
        assert_eq!(plain(&form.body(60, &theme)), ["Add sign-in now?"]);
        assert!(matches!(form.on_key(&key(KeyCode::Char('2'))), Step::Pending));

        // Everything answered: the last tab shows the answers, and enter sends them.
        assert_eq!(
            plain(&form.body(60, &theme)),
            [
                "1. Which store?",
                "   sqlite",
                "2. Add sign-in now?",
                "   no",
                "3. Anything else I should know before starting?",
                "   ship it",
            ]
        );
        assert_eq!(
            plain(&form.widget(60, 4, "> ", &theme).lines),
            ["enter sends these answers"]
        );
        assert_eq!(
            done(form.on_key(&key(KeyCode::Enter))),
            [vec!["sqlite"], vec!["no"], vec!["ship it"]]
        );
    }

    #[test]
    fn a_form_can_be_sent_with_questions_open_and_says_which() {
        let mut form = three();
        let theme = Theme::dark();
        // Shift+tab from the first question goes round to the tab that sends.
        assert!(matches!(form.on_key(&key(KeyCode::BackTab)), Step::Pending));
        let body = plain(&form.body(60, &theme));
        assert_eq!(body[1], "   no answer");
        assert_eq!(
            plain(&form.widget(60, 4, "> ", &theme).lines),
            ["3 questions are open; enter sends the answers as they are"]
        );
        // Left and right turn the tabs where nothing is typed.
        assert!(matches!(form.on_key(&key(KeyCode::Left)), Step::Pending));
        assert!(form.is_typing(), "the third question takes text");
        assert!(matches!(form.on_key(&key(KeyCode::Tab)), Step::Pending));
        let answers = done(form.on_key(&key(KeyCode::Enter)));
        assert_eq!(answers, vec![Vec::<String>::new(); 3]);

        let mut form = three();
        assert!(matches!(form.on_key(&key(KeyCode::Esc)), Step::Cancelled));
    }

    #[test]
    fn the_body_shows_the_context_the_excerpt_and_the_question() {
        let mut asked = question("Apply it?", AskKind::Confirm, &[]);
        asked.context = Some("The parser drops the last token.".to_string());
        asked.excerpt = Some(Excerpt::Diff(
            "--- a/src/lex.rs\n+++ b/src/lex.rs\n@@ -1,2 +1,2 @@\n fn next() {\n-    old();\n+    new();\n"
                .to_string(),
        ));
        let form = Form::new(vec![asked], "Other…");
        let theme = Theme::dark();
        let body = plain(&form.body(50, &theme));
        assert_eq!(body[0], "The parser drops the last token.");
        assert_eq!(body[1], "");
        assert!(body.iter().any(|row| row.contains("-    old();")), "{body:?}");
        assert!(body.iter().any(|row| row.contains("+    new();")), "{body:?}");
        assert_eq!(body.last().map(String::as_str), Some("Apply it?"));
        assert_eq!(
            form.body_text(),
            "The parser drops the last token.\n\n--- a/src/lex.rs\n+++ b/src/lex.rs\n@@ -1,2 +1,2 @@\n fn \
             next() {\n-    old();\n+    new();\n\nApply it?"
        );

        let mut asked = question("Keep this?", AskKind::Confirm, &[]);
        asked.excerpt = Some(Excerpt::Code("fn main() {\n    run();\n}\n".to_string()));
        let form = Form::new(vec![asked], "Other…");
        let body = plain(&form.body(50, &theme));
        // Code is numbered, as a file that was read is.
        assert!(
            body[0].trim_start().starts_with('1') && body[0].contains("fn main() {"),
            "{body:?}"
        );
        assert!(body[1].contains("run();"), "{body:?}");
        for width in 1..60 {
            for row in form.body(width, &theme) {
                assert!(text::width(&row) <= width, "width {width}: {row:?}");
            }
        }
    }

    #[test]
    fn the_tabs_fall_back_to_numbers_and_never_exceed_the_width() {
        let form = three();
        let theme = Theme::dark();
        assert_eq!(plain(&form.tabs(24, &theme))[0], "1  2  3  Send");
        for width in 1..80 {
            for row in form.tabs(width, &theme) {
                assert!(text::width(&row) <= width, "width {width}: {row:?}");
            }
            for row in form.widget(width, 6, "> ", &theme).lines {
                assert!(text::width(&row) <= width, "width {width}: {row:?}");
            }
        }
        let ascii = Theme::dark().with_glyphs(azazel_tui::GlyphLevel::Ascii);
        let mut form = three();
        form.on_key(&key(KeyCode::Enter));
        for row in form.tabs(70, &ascii) {
            assert!(text::strip_ansi(&row).is_ascii(), "{row:?}");
        }
    }
}
