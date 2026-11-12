//! Modal prompts shown in place of the input line: pick from a list, or type a line.

use std::path::PathBuf;

use azazel_core::permissions::{Rule, RuleKind, SourcedRule};
use azazel_core::session::SessionSummary;
use azazel_core::{ApprovalDecision, PlanDecision, UiError};
use azazel_tui::components::composer;
use azazel_tui::{Editor, KeyCombo, Line, Role, SelectItem, SelectList, Theme, text};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::bridge::Reply;
use super::sessions::{PickFor, SideRun, TreeFilter};

/// A point the session can be rewound to.
#[derive(Debug, Clone)]
pub struct RewindPoint {
    /// The checkpoint entry; its parent is where the conversation resumes.
    pub entry_id: String,
    pub commit: String,
    pub prompt: String,
}

/// What happens with an overlay's result.
pub enum Action {
    Ask {
        reply: Reply<Result<Vec<String>, UiError>>,
        options: Vec<String>,
        multi: bool,
        picked: Vec<bool>,
    },
    AskFree {
        reply: Reply<Result<Vec<String>, UiError>>,
    },
    Confirm {
        reply: Reply<Result<bool, UiError>>,
    },
    UiSelect {
        reply: Reply<Result<usize, UiError>>,
    },
    UiInput {
        reply: Reply<Result<String, UiError>>,
    },
    Approval {
        reply: Reply<Option<ApprovalDecision>>,
        has_rule: bool,
    },
    DenyFeedback {
        reply: Reply<Option<ApprovalDecision>>,
    },
    Plan {
        reply: Reply<Result<PlanDecision, UiError>>,
    },
    PlanFeedback {
        reply: Reply<Result<PlanDecision, UiError>>,
    },
    Model(Vec<String>),
    Thinking,
    Mode,
    Theme(Vec<String>),
    /// The session file of each row; choosing one resumes it.
    Resume(Vec<PathBuf>),
    /// A session picker (`/resume`, `/consult`): the session of each row, and what the picker
    /// is for.
    Sessions {
        rows: Vec<SessionSummary>,
        purpose: PickFor,
        named_only: bool,
    },
    /// A new name being typed for a session of a picker, which opens again afterwards.
    SessionRename(Box<SessionSummary>, PickFor, bool),
    /// Whether to delete a session of a picker.
    SessionDelete(Box<SessionSummary>, PickFor, bool),
    /// A question being typed for a saved session.
    ConsultQuestion(Box<SessionSummary>),
    /// A request outside the conversation whose reply is arriving.
    Side(SideRun),
    /// The answer to a question asked on the side: close, copy, or read it full screen.
    SideAnswer {
        title: String,
        question: String,
        answer: String,
    },
    /// `(entry id, is a user message, text)` per row, and which entries are listed.
    Tree(Vec<(String, bool, String)>, TreeFilter),
    /// A label being typed for an entry of the tree.
    TreeLabel {
        entry: String,
        filter: TreeFilter,
    },
    History(Vec<String>),
    Rewind(Vec<RewindPoint>),
    RewindWhat(RewindPoint),
    /// The agent hub: the agent id of each row.
    Agents(Vec<String>),
    /// A message being typed for this agent.
    AgentMessage(String),
    /// `(server, enabled)` per row.
    #[cfg(feature = "mcp")]
    McpServer(Vec<(String, bool)>),
    /// What can be done with `server`, per row.
    #[cfg(feature = "mcp")]
    McpChoice {
        server: String,
        actions: Vec<&'static str>,
    },
    /// The code block of each row, to copy.
    CopyCode(Vec<String>),
    /// Background jobs: the id of each row.
    Jobs(Vec<String>),
    /// What to do with this job: read its output, stop it.
    JobChoice(String),
    /// `/permissions`: the rule of each row.
    Permissions(Vec<SourcedRule>),
    /// A rule being typed, such as `allow shell(cargo test:*)`.
    PermissionAdd,
    /// Where a new rule goes: this session, or saved for the project.
    PermissionScope(Rule, RuleKind),
}

impl Action {
    /// Answers the waiting agent when the user dismisses the overlay.
    pub fn cancel(self) {
        match self {
            Action::Ask { reply, .. } | Action::AskFree { reply } => {
                drop(reply.send(Err(UiError::Cancelled)))
            }
            Action::Confirm { reply } => drop(reply.send(Err(UiError::Cancelled))),
            Action::UiSelect { reply } => drop(reply.send(Err(UiError::Cancelled))),
            Action::UiInput { reply } => drop(reply.send(Err(UiError::Cancelled))),
            Action::Approval { reply, .. } | Action::DenyFeedback { reply } => {
                drop(reply.send(Some(ApprovalDecision::Deny { feedback: None })))
            }
            Action::Plan { reply } | Action::PlanFeedback { reply } => {
                drop(reply.send(Err(UiError::Cancelled)))
            }
            Action::Side(run) => run.cancel(),
            _ => {}
        }
    }

    /// Whether whoever asked has stopped waiting for the answer (a sign-in finished in the
    /// browser while its "waiting" overlay was still open, for example).
    pub fn abandoned(&self) -> bool {
        match self {
            Action::Ask { reply, .. } | Action::AskFree { reply } => reply.is_closed(),
            Action::Confirm { reply } => reply.is_closed(),
            Action::UiSelect { reply } => reply.is_closed(),
            Action::UiInput { reply } => reply.is_closed(),
            Action::Approval { reply, .. } | Action::DenyFeedback { reply } => reply.is_closed(),
            Action::Plan { reply } | Action::PlanFeedback { reply } => reply.is_closed(),
            _ => false,
        }
    }
}

pub enum Kind {
    Select {
        list: SelectList,
        filterable: bool,
        filter: String,
    },
    Input {
        editor: Editor,
        /// Typed text is shown as dots.
        secret: bool,
    },
}

pub struct Overlay {
    pub title: String,
    /// Plain text shown above the list or input.
    pub body: Vec<String>,
    pub kind: Kind,
    pub action: Action,
    /// Keys that act on the selected row without closing the overlay, each with the word the
    /// hint line shows for it (`ctrl+d`, `delete`). They win over typing into a filter, so a
    /// filterable list should use chords. A key with no word stays out of the hint line: an
    /// overlay with more keys than the line holds names them in its body.
    pub keys: Vec<(KeyCombo, &'static str)>,
}

pub enum Outcome {
    Pending,
    Cancelled,
    /// Index into the items the list was built from.
    Chosen(usize),
    Text(String),
    /// One of the overlay's own keys was pressed: `key` is its index in [`Overlay::keys`],
    /// `index` the selected row among the items the list was built from (0 for an input).
    Hotkey {
        index: usize,
        key: usize,
    },
}

/// A key as the hint line names it: `S` rather than `shift+s`.
fn key_label(combo: &KeyCombo) -> String {
    match combo.code {
        KeyCode::Char(c) if combo.mods == KeyModifiers::SHIFT && c.is_alphabetic() => {
            c.to_uppercase().collect()
        }
        _ => combo.to_string(),
    }
}

const MAX_BODY_LINES: usize = 14;

impl Overlay {
    /// Starts a list with row `index` selected instead of the first.
    pub fn starting_at(mut self, index: usize) -> Self {
        if let Kind::Select { list, .. } = &mut self.kind {
            list.set_cursor(index);
        }
        self
    }

    pub fn select(
        title: impl Into<String>,
        body: Vec<String>,
        items: Vec<SelectItem>,
        filterable: bool,
        action: Action,
    ) -> Self {
        Overlay {
            title: title.into(),
            body,
            kind: Kind::Select {
                list: SelectList::new(items),
                filterable,
                filter: String::new(),
            },
            action,
            keys: Vec::new(),
        }
    }

    /// Gives the overlay keys of its own; see [`Overlay::keys`].
    pub fn with_keys(mut self, keys: Vec<(KeyCombo, &'static str)>) -> Self {
        self.keys = keys;
        self
    }

    pub fn input(title: impl Into<String>, body: Vec<String>, action: Action) -> Self {
        Overlay {
            title: title.into(),
            body,
            kind: Kind::Input {
                editor: Editor::new(),
                secret: false,
            },
            action,
            keys: Vec::new(),
        }
    }

    /// An input whose text is shown as dots, for API keys and passwords.
    pub fn secret(title: impl Into<String>, body: Vec<String>, action: Action) -> Self {
        let mut overlay = Overlay::input(title, body, action);
        if let Kind::Input { secret, .. } = &mut overlay.kind {
            *secret = true;
        }
        overlay
    }

    /// Flips one option of a multi-choice question in place, keeping the selection on it. False
    /// when `index` is not such an option.
    pub fn toggle(&mut self, index: usize) -> bool {
        let (
            Action::Ask {
                options,
                multi: true,
                picked,
                ..
            },
            Kind::Select { list, .. },
        ) = (&mut self.action, &mut self.kind)
        else {
            return false;
        };
        let Some(flag) = picked.get_mut(index) else {
            return false;
        };
        *flag = !*flag;
        list.set_items(ask_items(options, picked, true));
        for _ in 0..index {
            list.move_down();
        }
        true
    }

    pub fn on_key(&mut self, key: &KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c')) {
            return Outcome::Cancelled;
        }
        let pressed = KeyCombo::from_event(key);
        if let Some(key) = self.keys.iter().position(|(combo, _)| *combo == pressed) {
            return match &self.kind {
                Kind::Select { list, .. } => list
                    .selected()
                    .map_or(Outcome::Pending, |index| Outcome::Hotkey { index, key }),
                Kind::Input { .. } => Outcome::Hotkey { index: 0, key },
            };
        }
        match &mut self.kind {
            Kind::Select {
                list,
                filterable,
                filter,
            } => {
                match key.code {
                    KeyCode::Up => list.move_up(),
                    KeyCode::Down | KeyCode::Tab => list.move_down(),
                    KeyCode::PageUp => list.page_up(),
                    KeyCode::PageDown => list.page_down(),
                    KeyCode::Enter => return list.selected().map_or(Outcome::Pending, Outcome::Chosen),
                    KeyCode::Backspace if *filterable => {
                        filter.pop();
                        list.set_filter(filter);
                    }
                    KeyCode::Char(c)
                        if *filterable && !ctrl && !key.modifiers.contains(KeyModifiers::ALT) =>
                    {
                        filter.push(c);
                        list.set_filter(filter);
                    }
                    // Without a filter, digits pick an item directly.
                    KeyCode::Char(c @ '1'..='9') if !*filterable => {
                        let index = c as usize - '1' as usize;
                        if index < list.items().len() {
                            return Outcome::Chosen(index);
                        }
                    }
                    _ => {}
                }
                Outcome::Pending
            }
            Kind::Input { editor, .. } => {
                if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT) {
                    return Outcome::Text(editor.text().to_string());
                }
                editor.handle_key(key);
                Outcome::Pending
            }
        }
    }

    pub fn on_paste(&mut self, pasted: &str) {
        match &mut self.kind {
            Kind::Select {
                list,
                filterable: true,
                filter,
            } => {
                filter.push_str(pasted.lines().next().unwrap_or_default());
                list.set_filter(filter);
            }
            Kind::Select { .. } => {}
            Kind::Input { editor, .. } => editor.insert_str(pasted),
        }
    }

    /// The overlay's rows, at most `max_rows`, and the cursor position within them when it has one.
    pub fn render(&self, width: usize, max_rows: usize, theme: &Theme) -> (Vec<Line>, Option<(u16, u16)>) {
        // Framed like the input it stands in for: the title rides the rule above, the key hints
        // the rule below.
        let title = theme.paint(Role::Accent, &self.title);
        let mut lines = vec![composer::rule(width, Some(&title), None, Role::Border, theme)];
        let prompt = theme.paint(Role::Accent, "❯ ");
        let mut body: Vec<Line> = Vec::new();
        for raw in &self.body {
            if raw.is_empty() {
                body.push(String::new());
            } else {
                body.extend(text::wrap(raw, width));
            }
        }
        let body_room = MAX_BODY_LINES.min(max_rows.saturating_sub(6));
        let hidden = body.len().saturating_sub(body_room);
        lines.extend(body.into_iter().take(body_room));
        if hidden > 0 {
            lines.push(theme.paint(Role::Muted, &format!("… {hidden} more lines")));
        }
        let mut cursor = None;
        let hint = match &self.kind {
            Kind::Select {
                list,
                filterable,
                filter,
            } => {
                if *filterable {
                    lines.push(format!("{} {filter}", theme.paint(Role::Muted, "filter:")));
                    let column = text::width("filter: ") + text::width(filter);
                    cursor = Some((lines.len() as u16 - 1, column.min(width.saturating_sub(1)) as u16));
                }
                let rows = max_rows.saturating_sub(lines.len() + 1).clamp(1, 12);
                lines.extend(list.render(width, rows, theme));
                if *filterable {
                    "type to filter · ↑↓ move · enter choose"
                } else {
                    "↑↓ move · enter choose"
                }
            }
            Kind::Input {
                editor,
                secret: false,
            } => {
                let room = max_rows.saturating_sub(lines.len() + 1).max(1);
                let view = editor.view(width, room, theme, &prompt);
                cursor = Some((lines.len() as u16 + view.cursor.0, view.cursor.1));
                lines.extend(view.lines);
                "enter confirm"
            }
            Kind::Input { editor, secret: true } => {
                // One dot per character, so a typo can still be counted, never read.
                let dots = editor.text().chars().count().min(width.saturating_sub(3));
                let before = editor.text()[..editor.cursor()].chars().count().min(dots);
                cursor = Some((lines.len() as u16, (2 + before) as u16));
                lines.push(format!("{prompt}{}", "•".repeat(dots)));
                "hidden while you type · enter confirm"
            }
        };
        let mut hint = hint.to_string();
        for (combo, what) in self.keys.iter().filter(|(_, what)| !what.is_empty()) {
            hint.push_str(&format!(" · {} {what}", key_label(combo)));
        }
        hint.push_str(" · esc cancel");
        let hint = theme.paint(Role::Muted, &hint);
        lines.push(composer::rule(width, Some(&hint), None, Role::Border, theme));
        (lines, cursor)
    }
}

/// The rows of an agent's question: the options, and for a multi-choice one their check boxes
/// followed by a `Done` row.
pub fn ask_items(options: &[String], picked: &[bool], multi: bool) -> Vec<SelectItem> {
    if !multi {
        return options.iter().map(|o| SelectItem::new(o.as_str(), "")).collect();
    }
    let mut items: Vec<SelectItem> = options
        .iter()
        .zip(picked)
        .map(|(option, on)| SelectItem::new(format!("[{}] {option}", if *on { 'x' } else { ' ' }), ""))
        .collect();
    items.push(SelectItem::new("Done", "confirm the selection"));
    items
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEventKind;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn items(labels: &[&str]) -> Vec<SelectItem> {
        labels.iter().map(|l| SelectItem::new(*l, "")).collect()
    }

    #[test]
    fn a_list_can_start_on_another_row() {
        let mut overlay = Overlay::select(
            "Approve?",
            Vec::new(),
            items(&["Allow once", "Deny", "Deny and say why"]),
            false,
            Action::Mode,
        )
        .starting_at(1);
        assert!(matches!(overlay.on_key(&key(KeyCode::Enter)), Outcome::Chosen(1)));
        let past_the_end =
            Overlay::select("x", Vec::new(), items(&["a"]), false, Action::Mode).starting_at(5);
        let mut past_the_end = past_the_end;
        assert!(matches!(
            past_the_end.on_key(&key(KeyCode::Enter)),
            Outcome::Chosen(0)
        ));
    }

    #[test]
    fn select_moves_filters_and_reports_the_original_index() {
        let mut overlay = Overlay::select(
            "Pick",
            Vec::new(),
            items(&["alpha", "beta", "gamma"]),
            true,
            Action::Mode,
        );
        assert!(matches!(overlay.on_key(&key(KeyCode::Down)), Outcome::Pending));
        assert!(matches!(overlay.on_key(&key(KeyCode::Enter)), Outcome::Chosen(1)));
        for c in "gam".chars() {
            overlay.on_key(&key(KeyCode::Char(c)));
        }
        assert!(matches!(overlay.on_key(&key(KeyCode::Enter)), Outcome::Chosen(2)));
        assert!(matches!(overlay.on_key(&key(KeyCode::Esc)), Outcome::Cancelled));
    }

    #[test]
    fn unfiltered_select_takes_digits_and_input_returns_its_text() {
        let mut pick = Overlay::select(
            "Allow?",
            vec!["run it".into()],
            items(&["yes", "no"]),
            false,
            Action::Mode,
        );
        assert!(matches!(
            pick.on_key(&key(KeyCode::Char('2'))),
            Outcome::Chosen(1)
        ));
        assert!(matches!(pick.on_key(&key(KeyCode::Char('7'))), Outcome::Pending));

        let mut input = Overlay::input("Why?", Vec::new(), Action::Mode);
        for c in "no".chars() {
            input.on_key(&key(KeyCode::Char(c)));
        }
        input.on_paste(" thanks");
        match input.on_key(&key(KeyCode::Enter)) {
            Outcome::Text(text) => assert_eq!(text, "no thanks"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn own_keys_report_the_selected_row_and_show_in_the_hint_line() {
        let keys = vec![
            (KeyCombo::new(KeyCode::Char('m'), KeyModifiers::NONE), "message"),
            (KeyCombo::new(KeyCode::Char('S'), KeyModifiers::NONE), "stop all"),
            (KeyCombo::new(KeyCode::Char('d'), KeyModifiers::CONTROL), "delete"),
        ];
        let mut overlay = Overlay::select(
            "Agents",
            Vec::new(),
            items(&["lead", "scout", "coder"]),
            false,
            Action::Mode,
        )
        .with_keys(keys.clone());
        overlay.on_key(&key(KeyCode::Down));
        assert!(matches!(
            overlay.on_key(&key(KeyCode::Char('m'))),
            Outcome::Hotkey { index: 1, key: 0 }
        ));
        // Terminals report shift+s as an upper-case letter, with or without the shift flag.
        let shifted = KeyEvent {
            modifiers: KeyModifiers::SHIFT,
            ..key(KeyCode::Char('S'))
        };
        assert!(matches!(
            overlay.on_key(&shifted),
            Outcome::Hotkey { index: 1, key: 1 }
        ));
        assert!(matches!(
            overlay.on_key(&key(KeyCode::Char('S'))),
            Outcome::Hotkey { index: 1, key: 1 }
        ));
        let chord = KeyEvent {
            modifiers: KeyModifiers::CONTROL,
            ..key(KeyCode::Char('d'))
        };
        assert!(matches!(
            overlay.on_key(&chord),
            Outcome::Hotkey { index: 1, key: 2 }
        ));
        // Other keys still do what they did; digits pick a row.
        assert!(matches!(
            overlay.on_key(&key(KeyCode::Char('s'))),
            Outcome::Pending
        ));
        assert!(matches!(
            overlay.on_key(&key(KeyCode::Char('3'))),
            Outcome::Chosen(2)
        ));
        let (lines, _) = overlay.render(100, 20, &Theme::dark());
        let last = text::strip_ansi(&lines[lines.len() - 1]);
        assert!(
            last.starts_with(
                "── ↑↓ move · enter choose · m message · S stop all · ctrl+d delete · esc cancel ─"
            ),
            "{last}"
        );

        // A chord wins over the filter; with no row left to act on it does nothing.
        let mut filtered = Overlay::select("Pick", Vec::new(), items(&["alpha", "beta"]), true, Action::Mode)
            .with_keys(keys);
        overlay_type(&mut filtered, "be");
        assert!(matches!(
            filtered.on_key(&chord),
            Outcome::Hotkey { index: 1, key: 2 }
        ));
        overlay_type(&mut filtered, "zz");
        assert!(matches!(filtered.on_key(&chord), Outcome::Pending));

        let mut input = Overlay::input("Why?", Vec::new(), Action::Mode).with_keys(vec![(
            KeyCombo::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
            "editor",
        )]);
        let chord = KeyEvent {
            modifiers: KeyModifiers::CONTROL,
            ..key(KeyCode::Char('g'))
        };
        assert!(matches!(
            input.on_key(&chord),
            Outcome::Hotkey { index: 0, key: 0 }
        ));
    }

    fn overlay_type(overlay: &mut Overlay, typed: &str) {
        for c in typed.chars() {
            overlay.on_key(&key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn render_fits_the_given_rows_and_places_the_cursor_on_the_input() {
        let theme = Theme::dark();
        let body: Vec<String> = (0..40).map(|i| format!("body {i}")).collect();
        let overlay = Overlay::input("Feedback", body, Action::Mode);
        let (lines, cursor) = overlay.render(40, 20, &theme);
        assert!(lines.len() <= 20, "{}", lines.len());
        assert!(lines.iter().all(|l| text::width(l) <= 40));
        let (row, column) = cursor.unwrap();
        assert!(text::strip_ansi(&lines[row as usize]).starts_with('❯'));
        assert_eq!(column, 2);
        let shown = text::strip_ansi(&lines.join("\n"));
        assert!(shown.contains("more lines"));
        // The title rides the rule above and the key hints the rule below.
        assert!(shown.starts_with("── Feedback ─"), "{shown}");
        let last = text::strip_ansi(&lines[lines.len() - 1]);
        assert!(last.starts_with("── enter confirm · esc cancel ─"), "{last}");
        assert_eq!(text::width(&last), 40);
    }
}
