//! Modal prompts: pick from a list, or type a line. Each is drawn in a box, which the UI shows
//! as a popup over the window or, where there is no room for one, in place of the input.

use std::cell::Cell;
use std::path::PathBuf;

use azazel_core::permissions::{Rule, RuleKind, SourcedRule};
use azazel_core::session::SessionSummary;
use azazel_core::{ApprovalDecision, PermissionMode, PlanDecision, UiError};
use azazel_tui::components::markdown;
use azazel_tui::components::overlay::Frame;
use azazel_tui::{
    Editor, Glyph, GlyphLevel, Hint, KeyCombo, Line, Role, Scroll, SelectItem, SelectList, Theme, text,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::bridge::Reply;
use super::fleet::{Pane, PaneKey};
use super::form::{Form, Step};
use super::sessions::{PickFor, SideRun, TreeFilter};

/// A point the session can be rewound to.
#[derive(Debug, Clone)]
pub struct RewindPoint {
    /// The checkpoint entry; its parent is where the conversation resumes.
    pub entry_id: String,
    pub commit: String,
    pub prompt: String,
}

/// What a row of a permission prompt answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalRow {
    Once,
    Session,
    Always,
    Deny,
    DenyWhy,
}

/// What happens with an overlay's result.
pub enum Action {
    /// One question of an agent, answered in a form.
    Ask {
        reply: Reply<Result<Vec<String>, UiError>>,
    },
    /// Several questions of an agent, answered together.
    Form {
        reply: Reply<Result<Vec<Vec<String>>, UiError>>,
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
        /// What each row answers, in the order shown.
        rows: Vec<ApprovalRow>,
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
    /// Whether to enter bypass mode: the second row says yes.
    Bypass,
    Theme(Vec<String>),
    /// `/glyphs`: the symbol set of each row; `None` is `auto`.
    Glyphs(Vec<Option<GlyphLevel>>),
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
    /// The popup that shows an agent at work.
    AgentPane,
    /// `/terminals`: the terminal of each row; choosing one shows its screen.
    Terminals(Vec<String>),
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
    /// `/context`: the request whose categories the rows are; choosing one reads its text.
    Context(Box<azazel_agent::ContextReport>),
    /// `/commit` with nothing staged: stage everything, or cancel.
    CommitStage(Box<super::gitcmd::Pending>),
    /// The commit message being edited, for the repository at `top`.
    Commit {
        top: PathBuf,
        stage_all: bool,
    },
    /// `/share`: whether to upload this; the first row says yes.
    Share(Box<crate::share::Shared>),
    /// `/bug`: per row, whether the report takes the conversation along; `None` cancels.
    Bug {
        description: String,
        rows: Vec<Option<bool>>,
    },
    /// Whether to open an issue with the summary of this report; the second row says yes.
    BugIssue(Box<crate::bug::Report>),
    /// `/terminal-setup`: whether to add the key binding to the terminal's file; the first
    /// row says yes.
    TerminalSetup(Box<super::setup::Change>),
}

impl Action {
    /// Answers the waiting agent when the user dismisses the overlay.
    pub fn cancel(self) {
        match self {
            Action::Ask { reply } => drop(reply.send(Err(UiError::Cancelled))),
            Action::Form { reply } => drop(reply.send(Err(UiError::Cancelled))),
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
            Action::Ask { reply } => reply.is_closed(),
            Action::Form { reply } => reply.is_closed(),
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
    /// The questions of an agent.
    Form(Box<Form>),
    /// One agent at work, and a line to type to it.
    Pane(Box<Pane>),
}

/// What a form or the popup of an agent draws inside the box, under any text.
pub struct Widget {
    pub lines: Vec<Line>,
    /// The typing cursor as `(row, column)` within `lines`.
    pub cursor: Option<(usize, usize)>,
    /// Where a list or a transcript is when it does not fit, and the row of `lines` it starts
    /// at.
    pub scroll: Option<(usize, Scroll)>,
}

pub struct Overlay {
    pub title: String,
    /// Text shown above the list or input: plain paragraphs, or Markdown when `markdown` is set.
    pub body: Vec<String>,
    /// The body is Markdown (a plan) and is rendered as the transcript renders a reply.
    pub markdown: bool,
    /// A paragraph under the body in the warning colour. It is never cut away with the body.
    pub warning: Option<String>,
    /// What is asked would destroy work: the box is drawn in the warning colour.
    pub destructive: bool,
    /// The last render had to cut the body, so `ctrl+o` shows all of it.
    cut: Cell<bool>,
    pub kind: Kind,
    pub action: Action,
    /// Keys that act on the selected row without closing the overlay, each with the word the
    /// hint line shows for it (`ctrl+d`, `delete`). They win over typing into a filter, so a
    /// filterable list should use chords. A key with no word stays out of the hint line: an
    /// overlay with more keys than the line holds names them in its body.
    pub keys: Vec<(KeyCombo, &'static str)>,
    /// What the hint line says `enter` does in a list.
    enter: &'static str,
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
    /// `ctrl+o` on a body that does not fit the box: show all of it full screen.
    ReadBody,
    /// A form was sent: the answers, one list per question.
    Answers(Vec<Vec<String>>),
    /// Text typed to the agent of a popup. The popup stays open.
    Send(String),
    /// The agent of a popup is to be stopped. The popup stays open.
    Stop,
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

/// Columns of text in the box of an overlay, in a window `columns` wide.
pub fn inner_width(columns: usize) -> usize {
    Frame::inner_width(azazel_tui::modal::box_width(columns)).max(1)
}

/// The most rows of a list a box shows at once.
const MAX_LIST_ROWS: usize = 16;
/// Rows kept for the list or the input when a long body competes with them.
const KEPT_FOR_LIST: usize = 6;
/// The most rows a box takes when it stands in place of the input instead of being a popup.
pub const INLINE_ROWS: usize = 26;

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
        Overlay::new(
            title.into(),
            body,
            Kind::Select {
                list: SelectList::new(items),
                filterable,
                filter: String::new(),
            },
            action,
        )
    }

    fn new(title: String, body: Vec<String>, kind: Kind, action: Action) -> Self {
        Overlay {
            title,
            body,
            markdown: false,
            warning: None,
            destructive: false,
            cut: Cell::new(false),
            kind,
            action,
            keys: Vec::new(),
            enter: "pick",
        }
    }

    /// Names what `enter` does in the list, where that is not picking a row.
    pub fn with_enter(mut self, word: &'static str) -> Self {
        self.enter = word;
        self
    }

    /// The question before bypass mode: what it lifts, with "No" as the row a stray Enter
    /// picks. The second row answers yes.
    pub fn bypass_warning(current: PermissionMode) -> Self {
        let body = vec![
            "Every tool call of the lead, of every sub-agent and of what runs in the background runs \
             without asking."
                .to_string(),
            "Protected paths, secret files, destructive commands and `ask` rules are no longer asked \
             about."
                .to_string(),
            "Only your deny rules and the push guard still apply.".to_string(),
        ];
        let items = vec![
            SelectItem::new(format!("No, stay in {current}"), ""),
            SelectItem::new("Yes, bypass permissions", "until you change the mode"),
        ];
        Overlay::select("Enter bypass mode?", body, items, false, Action::Bypass).destructive(true)
    }

    /// Gives the overlay keys of its own; see [`Overlay::keys`].
    pub fn with_keys(mut self, keys: Vec<(KeyCombo, &'static str)>) -> Self {
        self.keys = keys;
        self
    }

    /// Adds a paragraph in the warning colour under the body.
    pub fn with_warning(mut self, warning: Option<String>) -> Self {
        self.warning = warning.filter(|warning| !warning.trim().is_empty());
        self
    }

    /// Draws the box in the warning colour when what is asked would destroy work.
    pub fn destructive(mut self, destructive: bool) -> Self {
        self.destructive = destructive;
        self
    }

    /// Renders the body as Markdown.
    pub fn markdown(mut self) -> Self {
        self.markdown = true;
        self
    }

    pub fn input(title: impl Into<String>, body: Vec<String>, action: Action) -> Self {
        Overlay::new(
            title.into(),
            body,
            Kind::Input {
                editor: Editor::new(),
                secret: false,
            },
            action,
        )
    }

    /// An input whose text is shown as dots, for API keys and passwords.
    pub fn secret(title: impl Into<String>, body: Vec<String>, action: Action) -> Self {
        let mut overlay = Overlay::input(title, body, action);
        if let Kind::Input { secret, .. } = &mut overlay.kind {
            *secret = true;
        }
        overlay
    }

    /// The questions of an agent in a box.
    pub fn form(form: Form, action: Action) -> Self {
        Overlay::new(
            "The agent asks".to_string(),
            Vec::new(),
            Kind::Form(Box::new(form)),
            action,
        )
    }

    /// The popup that shows an agent at work. Its title follows the agent.
    pub fn pane(pane: Pane, action: Action) -> Self {
        let title = pane.agent.clone();
        Overlay::new(title, Vec::new(), Kind::Pane(Box::new(pane)), action)
    }

    /// The editor of an overlay that takes text which may also be written in the user's own
    /// editor: an input that is not a secret, a typed answer, the line to an agent.
    pub fn editor_mut(&mut self) -> Option<&mut Editor> {
        match &mut self.kind {
            Kind::Input {
                editor,
                secret: false,
            } => Some(editor),
            Kind::Form(form) => form.editor_mut(),
            Kind::Pane(pane) => Some(pane.editor_mut()),
            Kind::Input { secret: true, .. } | Kind::Select { .. } => None,
        }
    }

    pub fn on_key(&mut self, key: &KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // A form and the popup of an agent have keys of their own, `esc` among them.
        match &mut self.kind {
            Kind::Pane(pane) => {
                return match pane.on_key(key) {
                    PaneKey::Pending => Outcome::Pending,
                    PaneKey::Close => Outcome::Cancelled,
                    PaneKey::Send(typed) => Outcome::Send(typed),
                    PaneKey::Stop => Outcome::Stop,
                };
            }
            Kind::Form(form) => {
                if ctrl && key.code == KeyCode::Char('o') && self.cut.get() {
                    return Outcome::ReadBody;
                }
                return match form.on_key(key) {
                    Step::Pending => Outcome::Pending,
                    Step::Cancelled => Outcome::Cancelled,
                    Step::Done(answers) => Outcome::Answers(answers),
                };
            }
            Kind::Select { .. } | Kind::Input { .. } => {}
        }
        if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c')) {
            return Outcome::Cancelled;
        }
        let pressed = KeyCombo::from_event(key);
        if let Some(key) = self.keys.iter().position(|(combo, _)| *combo == pressed) {
            return match &self.kind {
                Kind::Select { list, .. } => list
                    .selected()
                    .map_or(Outcome::Pending, |index| Outcome::Hotkey { index, key }),
                Kind::Input { .. } | Kind::Form(_) | Kind::Pane(_) => Outcome::Hotkey { index: 0, key },
            };
        }
        if ctrl && key.code == KeyCode::Char('o') && self.cut.get() {
            return Outcome::ReadBody;
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
            Kind::Input { editor, secret } => {
                if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT) {
                    return Outcome::Text(editor.text().to_string());
                }
                // A commit message or feedback may run over several lines.
                if key.code == KeyCode::Enter && !*secret {
                    editor.insert_newline();
                    return Outcome::Pending;
                }
                editor.handle_key(key);
                Outcome::Pending
            }
            // Handled above.
            Kind::Form(_) | Kind::Pane(_) => Outcome::Pending,
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
            Kind::Form(form) => form.on_paste(pasted),
            Kind::Pane(pane) => pane.on_paste(pasted),
        }
    }

    /// The body as the full-screen viewer shows it when the box cannot hold all of it.
    pub fn body_text(&self) -> String {
        if let Kind::Form(form) = &self.kind {
            return form.body_text();
        }
        let mut text = self.body.join("\n");
        if let Some(warning) = &self.warning {
            text.push_str("\n\n");
            text.push_str(warning);
        }
        text
    }

    /// The key hints for the bottom edge of the box.
    fn hints(&self, theme: &Theme) -> Vec<Hint> {
        let mut hints = Vec::new();
        match &self.kind {
            Kind::Select { filterable, .. } => {
                if *filterable {
                    hints.push(Hint::new("type", "to filter"));
                }
                let arrows = format!("{}{}", theme.glyph(Glyph::MoreUp), theme.glyph(Glyph::MoreDown));
                hints.push(Hint::new(arrows, "move"));
                hints.push(Hint::new("enter", self.enter));
            }
            Kind::Input { secret, .. } => {
                if *secret {
                    hints.push(Hint::new("", "hidden while you type"));
                }
                hints.push(Hint::new("enter", "confirm"));
            }
            Kind::Form(form) => hints.extend(form.hints(theme)),
            // Its own way out is named among them.
            Kind::Pane(pane) => return pane.hints(),
        }
        for (combo, what) in self.keys.iter().filter(|(_, what)| !what.is_empty()) {
            hints.push(Hint::new(key_label(combo), *what));
        }
        if self.cut.get() {
            hints.push(Hint::new("ctrl+o", "read all"));
        }
        hints.push(Hint::new("esc", "cancel"));
        hints
    }

    /// The overlay as a box `width` columns wide and at most `max_rows` rows tall, and the cursor
    /// position within it when it has one. The title rides the top edge, the key hints the
    /// bottom edge; the body comes first, then the list or the input. A body that does not fit
    /// is cut, with a row that says how much is missing.
    pub fn render(&self, width: usize, max_rows: usize, theme: &Theme) -> (Vec<Line>, Option<(u16, u16)>) {
        let mut frame = Frame::new(&self.title, width);
        if self.destructive {
            frame.border = Role::Warning;
        }
        let inner = Frame::inner_width(width).max(1);
        let total = max_rows.saturating_sub(frame.chrome_rows()).max(1);
        // The tabs of a form stay whatever else is cut, as long as the box has rows for more
        // than them.
        let head: Vec<Line> = match &self.kind {
            Kind::Form(form) => form.tabs(inner, theme),
            _ => Vec::new(),
        };
        let head = if head.len() + 2 > total { Vec::new() } else { head };
        let head_rows = head.len();
        let interior = total - head_rows;

        let mut body: Vec<Line> = if let Kind::Form(form) = &self.kind {
            form.body(inner, theme)
        } else if self.markdown {
            markdown::render(&self.body.join("\n"), inner, theme)
        } else {
            // What a tool or a model put into the body (a command, a path) is shown as text:
            // nothing in it may move the cursor or colour the box.
            let mut lines = Vec::new();
            for raw in &self.body {
                if raw.is_empty() {
                    lines.push(String::new());
                } else {
                    lines.extend(text::wrap(&text::sanitize(raw), inner));
                }
            }
            lines
        };
        let mut warning: Vec<Line> = Vec::new();
        if let Some(note) = &self.warning {
            if !body.is_empty() {
                warning.push(String::new());
            }
            warning.extend(
                text::wrap(&text::sanitize(note), inner)
                    .iter()
                    .map(|line| theme.paint(Role::Warning, line)),
            );
        }
        // The list or the input keeps some rows whatever the body needs, and one blank row
        // sets them apart from it.
        let kept = match &self.kind {
            Kind::Select { list, filterable, .. } => {
                list.len().clamp(1, KEPT_FOR_LIST) + usize::from(*filterable)
            }
            Kind::Input { secret: true, .. } => 1,
            Kind::Input { .. } => 3,
            Kind::Form(form) => form.kept(),
            Kind::Pane(_) => interior,
        };
        let has_body = !body.is_empty() || !warning.is_empty();
        let text_room = interior.saturating_sub(kept + usize::from(has_body));
        // The warning stays as long as there is any room at all; the body is cut before it.
        if warning.len() > text_room {
            if warning.first().is_some_and(String::is_empty) {
                warning.remove(0);
            }
            warning.truncate(text_room);
        }
        let room = text_room - warning.len();
        let hidden = body.len().saturating_sub(room);
        self.cut.set(hidden > 0);
        if hidden > 0 {
            // The row that says so takes the place of one more line.
            let shown = room.saturating_sub(1);
            let more = format!(
                "{} {} more lines",
                theme.glyph(Glyph::Ellipsis),
                body.len() - shown
            );
            body.truncate(shown);
            if room > 0 {
                body.push(theme.paint(Role::Muted, &more));
            }
        }
        let mut rows = head;
        rows.append(&mut body);
        rows.append(&mut warning);
        if has_body && rows.len() > head_rows {
            rows.push(String::new());
        }

        let prompt = theme.paint(Role::Accent, &theme.glyphs().prefix(Glyph::Prompt));
        let mut cursor = None;
        match &self.kind {
            Kind::Select { list, filterable, .. } => {
                if *filterable {
                    let (line, column) = list.filter_row(inner, theme);
                    cursor = Some((rows.len(), usize::from(column)));
                    rows.push(line);
                }
                let room = total.saturating_sub(rows.len()).clamp(1, MAX_LIST_ROWS);
                let view = list.view(inner, room, theme);
                frame.scrollbar = view.scroll.map(|scroll| (rows.len(), scroll));
                rows.extend(view.lines);
            }
            Kind::Input {
                editor,
                secret: false,
            } => {
                let room = total.saturating_sub(rows.len()).max(1);
                let view = editor.view(inner, room, theme, &prompt);
                cursor = Some((
                    rows.len() + usize::from(view.cursor.0),
                    usize::from(view.cursor.1),
                ));
                rows.extend(view.lines);
            }
            Kind::Input { editor, secret: true } => {
                // One dot per character, so a typo can still be counted, never read.
                let lead = text::width(&prompt);
                let dots = editor.text().chars().count().min(inner.saturating_sub(lead + 1));
                let before = editor.text()[..editor.cursor()].chars().count().min(dots);
                cursor = Some((rows.len(), lead + before));
                rows.push(format!("{prompt}{}", theme.glyph(Glyph::Bullet).repeat(dots)));
            }
            Kind::Form(form) => {
                let room = total.saturating_sub(rows.len()).clamp(1, MAX_LIST_ROWS);
                let widget = form.widget(inner, room, &prompt, theme);
                cursor = widget.cursor.map(|(row, column)| (rows.len() + row, column));
                frame.scrollbar = widget.scroll.map(|(first, scroll)| (rows.len() + first, scroll));
                rows.extend(widget.lines);
            }
            Kind::Pane(pane) => {
                let room = total.saturating_sub(rows.len()).max(1);
                let widget = pane.widget(inner, room, &prompt, theme);
                cursor = widget.cursor.map(|(row, column)| (rows.len() + row, column));
                frame.scrollbar = widget.scroll.map(|(first, scroll)| (rows.len() + first, scroll));
                rows.extend(widget.lines);
            }
        }

        let hints = self.hints(theme);
        frame.hints = &hints;
        let (top, left) = frame.origin();
        let cursor = cursor.map(|(row, column)| ((row + top) as u16, (column + left) as u16));
        (frame.render(&rows, theme), cursor)
    }
}

/// What an approval shows above its choices: the command of a call that runs one, whole (its
/// subjects name only the programs in it, and the box covers the card that shows the rest);
/// else what the call touches; else its arguments. A `pty` call shows the program in the
/// terminal, then what it would type into it.
pub fn approval_body(tool: &str, subjects: &[String], input: &serde_json::Value) -> Vec<String> {
    if tool == "pty" && !subjects.is_empty() {
        let mut body = subjects.to_vec();
        body.push(super::view::tool_summary(tool, input));
        return body;
    }
    if let Some(command) = input.get("command").and_then(serde_json::Value::as_str)
        && !command.trim().is_empty()
    {
        return vec![command.trim_end().to_string()];
    }
    if subjects.is_empty() {
        return serde_json::to_string_pretty(input)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
    }
    subjects.to_vec()
}

/// The longest first line of a prompt that still serves as the title of its box.
const MAX_TITLE: usize = 48;

/// A prompt as the title and body of its box. A short first line is the title and the lines
/// after it the body; a prompt whose first line is long is all body under the title `fallback`.
pub fn heading(prompt: &str, fallback: &str) -> (String, Vec<String>) {
    let mut lines = prompt.trim().lines();
    let first = lines.next().unwrap_or_default().trim();
    if first.is_empty() || text::width(first) > MAX_TITLE {
        let body = prompt.trim().lines().map(str::to_string).collect();
        return (fallback.to_string(), body);
    }
    (first.to_string(), lines.map(str::to_string).collect())
}

/// One option of a list as its row: what follows a tab is the detail shown beside the label
/// (`Theme<tab>dark`).
pub fn option_item(option: &str) -> SelectItem {
    match option.split_once('\t') {
        Some((label, detail)) => SelectItem::new(label.trim_end(), detail.trim()),
        None => SelectItem::new(option, ""),
    }
}

/// The rows of a permission prompt and what each answers. Allowing for the session or for good
/// needs a rule that covers the call, and is never offered for a command that destroys work. A
/// secret file can be allowed for the session, never for good.
pub fn approval_rows(
    rule: Option<&str>,
    destructive: bool,
    secret: bool,
) -> (Vec<SelectItem>, Vec<ApprovalRow>) {
    let mut rows = vec![(ApprovalRow::Once, SelectItem::new("Allow once", ""))];
    if secret && !destructive {
        rows.push((
            ApprovalRow::Session,
            SelectItem::new("Allow this session", "this file, until azazel exits"),
        ));
    } else if let Some(rule) = rule.filter(|_| !destructive) {
        rows.push((
            ApprovalRow::Session,
            SelectItem::new("Allow this session", "until azazel exits; nothing is saved"),
        ));
        rows.push((
            ApprovalRow::Always,
            SelectItem::new(format!("Always allow {rule}"), "saved for this project"),
        ));
    }
    rows.push((ApprovalRow::Deny, SelectItem::new("Deny", "")));
    rows.push((ApprovalRow::DenyWhy, SelectItem::new("Deny and say why", "")));
    let (answers, items) = rows.into_iter().unzip();
    (items, answers)
}

#[cfg(test)]
mod tests {
    use azazel_core::{AskKind, Question};
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
    fn shift_enter_adds_a_line_to_an_input_but_not_to_a_secret() {
        let shift_enter = KeyEvent {
            modifiers: KeyModifiers::SHIFT,
            ..key(KeyCode::Enter)
        };
        let mut input = Overlay::input("Commit message", Vec::new(), Action::Mode);
        input.on_paste("Fix the parser");
        assert!(matches!(input.on_key(&shift_enter), Outcome::Pending));
        input.on_paste("It dropped the last token.");
        let typed = input.on_key(&key(KeyCode::Enter));
        assert!(
            matches!(&typed, Outcome::Text(text) if text == "Fix the parser\nIt dropped the last token.")
        );

        let mut secret = Overlay::secret("Key", Vec::new(), Action::Mode);
        secret.on_paste("sk-1");
        assert!(matches!(secret.on_key(&shift_enter), Outcome::Pending));
        assert!(matches!(secret.on_key(&key(KeyCode::Enter)), Outcome::Text(text) if text == "sk-1"));
    }

    #[test]
    fn the_bypass_question_starts_on_no_and_says_what_is_lifted() {
        let theme = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        let mut overlay = Overlay::bypass_warning(PermissionMode::AcceptEdits);
        let (lines, _) = overlay.render(100, 20, &theme);
        let shown: Vec<String> = lines.iter().map(|line| text::strip_ansi(line)).collect();
        let shown = shown.join("\n");
        for part in [
            "Enter bypass mode?",
            "Protected paths, secret files, destructive commands and `ask` rules",
            "Only your deny rules and the push guard still apply.",
            "No, stay in accept-edits",
            "Yes, bypass permissions",
        ] {
            assert!(shown.contains(part), "{part} in\n{shown}");
        }
        // A stray Enter keeps the mode; yes is one row down; Esc is no.
        assert!(matches!(overlay.on_key(&key(KeyCode::Enter)), Outcome::Chosen(0)));
        let mut overlay = Overlay::bypass_warning(PermissionMode::Manual);
        overlay.on_key(&key(KeyCode::Down));
        assert!(matches!(overlay.on_key(&key(KeyCode::Enter)), Outcome::Chosen(1)));
        let mut overlay = Overlay::bypass_warning(PermissionMode::Manual);
        assert!(matches!(overlay.on_key(&key(KeyCode::Esc)), Outcome::Cancelled));
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
                "╰─ ↑↓ move · enter pick · m message · S stop all · ctrl+d delete · esc cancel ─"
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

    fn shown(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|line| text::strip_ansi(line)).collect()
    }

    #[test]
    fn render_fits_the_given_rows_and_places_the_cursor_on_the_input() {
        let theme = Theme::dark();
        let body: Vec<String> = (0..40).map(|i| format!("body {i}")).collect();
        let overlay = Overlay::input("Feedback", body, Action::Mode);
        let (lines, cursor) = overlay.render(40, 20, &theme);
        // A one-line input needs less than the rows kept for it: the box is as tall as its
        // content.
        assert_eq!(lines.len(), 18);
        assert!(lines.iter().all(|l| text::width(l) == 40));
        let rows = shown(&lines);
        // The title rides the top edge and the key hints the bottom edge.
        assert_eq!(rows[0], format!("╭─ Feedback {}╮", "─".repeat(27)));
        assert_eq!(rows[1], format!("│ body 0{}│", " ".repeat(31)));
        // 18 rows inside at most: 3 kept for the input, a blank row, 13 lines of body and the
        // row that says how many more there are.
        assert_eq!(rows[13], format!("│ body 12{}│", " ".repeat(30)));
        assert_eq!(rows[14], format!("│ … 27 more lines{}│", " ".repeat(22)));
        assert_eq!(rows[15], format!("│{}│", " ".repeat(38)));
        let (row, column) = cursor.unwrap();
        assert_eq!((row, column), (16, 4));
        assert!(rows[16].starts_with("│ ❯"), "{}", rows[16]);
        assert!(
            rows[17].starts_with("╰─ enter confirm · ctrl+o read all ─"),
            "{}",
            rows[17]
        );
    }

    #[test]
    fn every_kind_of_box_fits_any_width_and_height() {
        let theme = Theme::dark();
        let long: Vec<SelectItem> = (0..40)
            .map(|i| SelectItem::new(format!("provider/model-{i}"), "ctx 200k · $3/$15"))
            .collect();
        let body = vec![
            "a paragraph that is long enough to be wrapped inside of a narrow box, more than once"
                .to_string(),
            String::new(),
            "second".to_string(),
        ];
        let mut secret = Overlay::secret("Key", body.clone(), Action::Mode);
        secret.on_paste("sk-secret-secret");
        let mut input = Overlay::input("Why?", body.clone(), Action::Mode);
        input.on_paste("line one\nline two");
        let (reply, _answers) = tokio::sync::oneshot::channel();
        let form = Overlay::form(three_questions(), Action::Form { reply });
        // A choice with a long context, where the user types an answer of their own.
        let mut asked = Question {
            kind: AskKind::Select,
            options: vec!["red".to_string(), "blue".to_string()],
            allow_freeform: true,
            ..Question::text("Which colour should the door be painted in the end?")
        };
        asked.context = Some(body.join("\n"));
        let (reply, _answer) = tokio::sync::oneshot::channel();
        let mut typing = Overlay::form(Form::new(vec![asked], "Other…"), Action::Ask { reply });
        typing.on_key(&key(KeyCode::Char('3')));
        typing.on_paste("a colour of my own");
        let mut agent = Pane::new("scout-1");
        agent.set_lines(
            (0..50)
                .map(|row| format!("row {row} of what the agent did"))
                .collect(),
        );
        let mut pane = Overlay::pane(agent, Action::AgentPane);
        pane.on_paste("look again");
        let overlays = [
            Overlay::select("Model", body.clone(), long, true, Action::Mode),
            Overlay::select(
                "Confirm",
                body.clone(),
                items(&["Yes", "No"]),
                false,
                Action::Mode,
            )
            .with_warning(Some("this deletes files outside the project".to_string()))
            .destructive(true),
            Overlay::select(
                "Plan review",
                vec!["# Plan\n\n- one\n- two\n\n```rust\nfn main() {}\n```".to_string()],
                items(&["Approve", "Revise"]),
                false,
                Action::Mode,
            )
            .markdown(),
            input,
            secret,
            form,
            typing,
            pane,
        ];
        for overlay in &overlays {
            for width in 5..=160 {
                for max_rows in [4, 8, 20, 60] {
                    let (lines, cursor) = overlay.render(width, max_rows, &theme);
                    assert!(
                        lines.len() <= max_rows.max(4),
                        "{} at {width}x{max_rows}",
                        overlay.title
                    );
                    for line in &lines {
                        assert_eq!(text::width(line), width, "{} at {width}: {line:?}", overlay.title);
                    }
                    if let Some((row, column)) = cursor {
                        assert!(
                            usize::from(row) < lines.len(),
                            "{} at {width}x{max_rows}",
                            overlay.title
                        );
                        assert!(
                            usize::from(column) < width,
                            "{} at {width}x{max_rows}",
                            overlay.title
                        );
                    }
                }
            }
            // Too narrow for a frame: bare rows that still fit.
            for width in 0..5 {
                for line in overlay.render(width, 20, &theme).0 {
                    assert!(text::width(&line) <= width);
                }
            }
        }
    }

    #[test]
    fn a_filterable_list_has_a_filter_row_a_match_count_and_a_scrollbar() {
        let theme = Theme::dark();
        let models: Vec<SelectItem> = (0..40)
            .map(|i| SelectItem::new(format!("provider/model-{i:02}"), "ctx 200k"))
            .collect();
        let mut overlay = Overlay::select(
            "Model",
            vec!["current: x".to_string()],
            models,
            true,
            Action::Mode,
        );
        let (lines, cursor) = overlay.render(50, 12, &theme);
        let rows = shown(&lines);
        assert_eq!(
            rows,
            [
                "╭─ Model ────────────────────────────────────────╮",
                "│ current: x                                     │",
                "│                                                │",
                "│ ⌕                                        40/40 │",
                "│ ▌ provider/model-00  ctx 200k                 ┃│",
                "│   provider/model-01  ctx 200k                 ││",
                "│   provider/model-02  ctx 200k                 ││",
                "│   provider/model-03  ctx 200k                 ││",
                "│   provider/model-04  ctx 200k                 ││",
                "│   provider/model-05  ctx 200k                 ││",
                "│   provider/model-06  ctx 200k                 ││",
                "╰─ type to filter · ↑↓ move · enter pick ────────╯",
            ]
        );
        // The typing cursor stands on the filter row, after the search symbol.
        assert_eq!(cursor, Some((3, 4)));

        overlay_type(&mut overlay, "model-3");
        let (lines, cursor) = overlay.render(50, 12, &theme);
        let rows = shown(&lines);
        assert_eq!(rows[3], "│ ⌕ model-3                                13/40 │");
        assert_eq!(cursor, Some((3, 11)));
        assert!(rows[4].starts_with("│ ▌ provider/model-3"), "{}", rows[4]);
        // A tall box shows all 13 matches, and no bar since they fit.
        let (lines, _) = overlay.render(50, 60, &theme);
        assert_eq!(lines.len(), 18);
        assert!(!shown(&lines).iter().any(|row| row.contains('━')));
    }

    #[test]
    fn a_destructive_approval_has_a_warning_border_and_keeps_its_note_when_the_body_is_cut() {
        let theme = Theme::dark();
        let subjects: Vec<String> = (0..30).map(|i| format!("rm -rf /data/set-{i}")).collect();
        let build = |destructive: bool| {
            Overlay::select(
                "lead wants to use shell",
                subjects.clone(),
                items(&["Allow once", "Deny", "Deny and say why"]),
                false,
                Action::Mode,
            )
            .with_warning(Some("this command deletes files outside the project".to_string()))
            .destructive(destructive)
            .starting_at(1)
        };
        let (lines, cursor) = build(true).render(60, 14, &theme);
        assert_eq!(cursor, None);
        let rows = shown(&lines);
        assert_eq!(rows.len(), 14);
        // 12 rows inside: 3 options, a blank row, a blank row and the note, and 6 for the
        // subjects, the last of which says how many are missing.
        assert!(rows[1].starts_with("│ rm -rf /data/set-0 "), "{}", rows[1]);
        assert!(rows[5].starts_with("│ rm -rf /data/set-4 "), "{}", rows[5]);
        assert!(rows[6].starts_with("│ … 25 more lines "), "{}", rows[6]);
        assert!(rows[8].starts_with("│ this command deletes files outside the project"));
        assert!(rows[10].starts_with("│   Allow once"), "{}", rows[10]);
        assert!(rows[11].starts_with("│ ▌ Deny "), "{}", rows[11]);
        assert!(
            rows[13].starts_with("╰─ ↑↓ move · enter pick · ctrl+o read all · esc cancel ─"),
            "{}",
            rows[13]
        );
        // The note and the whole border are in the warning colour.
        let warning = theme.fg(Role::Warning);
        assert!(lines[8].contains(&warning));
        for line in &lines {
            assert!(line.contains(&warning), "{line:?}");
            assert!(!line.contains(&theme.fg(Role::Border)), "{line:?}");
        }
        // The same request without the danger: the usual border, the note still stands out.
        let (calm, _) = build(false).render(60, 14, &theme);
        assert_eq!(shown(&calm), rows);
        assert!(calm[0].contains(&theme.fg(Role::Border)));
        assert!(!calm[0].contains(&warning));
        assert!(calm[8].contains(&warning));
    }

    #[test]
    fn ctrl_o_asks_for_the_whole_body_only_when_the_box_cut_it() {
        let ctrl_o = KeyEvent {
            modifiers: KeyModifiers::CONTROL,
            ..key(KeyCode::Char('o'))
        };
        let theme = Theme::dark();
        let body: Vec<String> = (0..30).map(|i| format!("step {i}")).collect();
        let mut plan = Overlay::select(
            "Plan review",
            body,
            items(&["Approve", "Revise"]),
            false,
            Action::Mode,
        )
        .with_warning(Some("careful".to_string()));
        // Nothing has been drawn yet, so nothing is known to be cut.
        assert!(matches!(plan.on_key(&ctrl_o), Outcome::Pending));
        plan.render(60, 12, &theme);
        assert!(matches!(plan.on_key(&ctrl_o), Outcome::ReadBody));
        assert!(plan.body_text().starts_with("step 0\nstep 1\n"));
        assert!(plan.body_text().ends_with("step 29\n\ncareful"));
        // In a box tall enough for all of it the key does nothing and is not offered.
        let (lines, _) = plan.render(60, 60, &theme);
        assert!(matches!(plan.on_key(&ctrl_o), Outcome::Pending));
        assert!(!shown(&lines).last().unwrap().contains("ctrl+o"));

        // A filter does not take the chord for typing.
        let many: Vec<SelectItem> = (0..20)
            .map(|i| SelectItem::new(format!("item {i}"), ""))
            .collect();
        let mut list = Overlay::select("Pick", vec!["short".to_string()], many, true, Action::Mode);
        list.render(60, 12, &theme);
        assert!(matches!(list.on_key(&ctrl_o), Outcome::Pending));
        assert!(matches!(list.on_key(&key(KeyCode::Enter)), Outcome::Chosen(0)));
    }

    #[test]
    fn a_markdown_body_is_rendered_and_a_secret_is_dots() {
        let theme = Theme::dark();
        let plan = Overlay::select(
            "Plan review",
            vec!["# Steps\n\n1. **Read** the parser\n2. Fix it".to_string()],
            items(&["Approve and run", "Revise"]),
            false,
            Action::Mode,
        )
        .markdown();
        let rows = shown(&plan.render(50, 20, &theme).0);
        assert!(rows.iter().any(|row| row.starts_with("│ Steps")), "{rows:#?}");
        assert!(
            rows.iter().any(|row| row.contains("1. Read the parser")),
            "{rows:#?}"
        );
        assert!(!rows.iter().any(|row| row.contains("**")), "{rows:#?}");

        let mut secret = Overlay::secret("Input", vec!["API key for x".to_string()], Action::Mode);
        secret.on_paste("sk-12345");
        let (lines, cursor) = secret.render(44, 20, &theme);
        let rows = shown(&lines);
        assert_eq!(rows[3], format!("│ ❯ ●●●●●●●●{}│", " ".repeat(31)));
        assert_eq!(cursor, Some((3, 12)));
        assert!(!rows.join("\n").contains("sk-"));
        assert_eq!(rows[4], "╰─ hidden while you type · enter confirm ──╯");
    }

    #[test]
    fn an_approval_shows_the_whole_command_or_what_the_call_touches() {
        let subjects = vec!["cat".to_string()];
        let heredoc = serde_json::json!({"command": "cat <<'EOF' > notes.txt\nline\nEOF\n"});
        assert_eq!(
            approval_body("shell", &subjects, &heredoc),
            ["cat <<'EOF' > notes.txt\nline\nEOF"]
        );
        let write = serde_json::json!({"path": "src/a.rs", "content": "fn main() {}"});
        assert_eq!(
            approval_body("write", &["src/a.rs".to_string()], &write),
            ["src/a.rs"]
        );
        // Nothing to name: the arguments as they are.
        let other = serde_json::json!({"query": "x"});
        assert_eq!(
            approval_body("memory_search", &[], &other),
            ["{", "  \"query\": \"x\"", "}"]
        );
        assert_eq!(
            approval_body("shell", &subjects, &serde_json::json!({"command": "  "})),
            ["cat"]
        );
        // Typing into a terminal: the program that runs there, then what would be typed.
        let typed = serde_json::json!({"action": "send", "id": "pty-1", "text": "yes", "submit": true});
        assert_eq!(
            approval_body("pty", &["npm init".to_string()], &typed),
            ["npm init", "type \"yes\" enter pty-1"]
        );
    }

    #[test]
    fn a_body_from_a_tool_cannot_break_the_box() {
        let theme = Theme::dark();
        let command = "cat <<'EOF' > notes.txt\n\tindented\x1b[31m red\x1b[0m\r\nbell\x07 here\nEOF";
        let overlay = Overlay::select(
            "lead wants to use shell",
            vec![command.to_string()],
            items(&["Allow once", "Deny"]),
            false,
            Action::Mode,
        )
        .with_warning(Some("careful:\tthis \x1b[1mwrites\x1b[0m a file".to_string()));
        let (lines, _) = overlay.render(50, 20, &theme);
        let rows = shown(&lines);
        assert_eq!(
            rows[1..7],
            [
                "│ cat <<'EOF' > notes.txt                        │",
                "│     indented red                               │",
                "│ bell here                                      │",
                "│ EOF                                            │",
                "│                                                │",
                "│ careful:    this writes a file                 │",
            ]
        );
        for line in &lines {
            assert_eq!(text::width(line), 50);
            assert!(!line.contains(['\n', '\r', '\t', '\x07']), "{line:?}");
        }
        // The colours a body brought along are gone.
        for line in &lines[1..7] {
            assert!(
                !line.contains("\x1b[31m") && !line.contains("\x1b[1m"),
                "{line:?}"
            );
        }
    }

    #[test]
    fn a_prompt_gives_the_box_its_title_and_its_options_their_details() {
        assert_eq!(
            heading("Which role?", "Choose"),
            ("Which role?".to_string(), Vec::<String>::new())
        );
        assert_eq!(
            heading(
                "Settings\ncredentials for: anthropic\ndefault model: none",
                "Choose"
            ),
            (
                "Settings".to_string(),
                vec![
                    "credentials for: anthropic".to_string(),
                    "default model: none".to_string()
                ]
            )
        );
        // A first line too long for a border is body text under the usual title.
        let long = "Paste the code the browser shows after you have signed in to your account";
        assert_eq!(
            heading(long, "Input"),
            ("Input".to_string(), vec![long.to_string()])
        );
        assert_eq!(heading("", "Input"), ("Input".to_string(), Vec::<String>::new()));

        assert_eq!(option_item("Theme\tdark"), SelectItem::new("Theme", "dark"));
        assert_eq!(option_item("Done"), SelectItem::new("Done", ""));
    }

    #[test]
    fn an_approval_offers_what_can_hold_for_its_call() {
        let offered = |rule: Option<&str>, destructive: bool, secret: bool| {
            let (items, rows) = approval_rows(rule, destructive, secret);
            assert_eq!(items.len(), rows.len());
            let labels: Vec<String> = items.iter().map(|item| item.label.clone()).collect();
            (labels, rows)
        };
        let labels = |rule: Option<&str>, destructive: bool| offered(rule, destructive, false);
        // A rule that covers the call can be given for the session or for good.
        let (shown, rows) = labels(Some("shell(cargo test:*)"), false);
        assert_eq!(
            shown,
            [
                "Allow once",
                "Allow this session",
                "Always allow shell(cargo test:*)",
                "Deny",
                "Deny and say why"
            ]
        );
        assert_eq!(
            rows,
            [
                ApprovalRow::Once,
                ApprovalRow::Session,
                ApprovalRow::Always,
                ApprovalRow::Deny,
                ApprovalRow::DenyWhy
            ]
        );
        // Without one, and for a call that destroys work whatever rule there is, it is this
        // call or nothing.
        let once = [ApprovalRow::Once, ApprovalRow::Deny, ApprovalRow::DenyWhy];
        assert_eq!(labels(None, false).1, once);
        assert_eq!(labels(Some("shell(git reset:*)"), true).1, once);
        assert_eq!(labels(None, true).0, ["Allow once", "Deny", "Deny and say why"]);
        // A secret file can be allowed for the session, with or without a rule, never for
        // good; a command that also destroys work only once.
        let session = [
            ApprovalRow::Once,
            ApprovalRow::Session,
            ApprovalRow::Deny,
            ApprovalRow::DenyWhy,
        ];
        assert_eq!(offered(None, false, true).1, session);
        assert_eq!(offered(Some("read(.env)"), false, true).1, session);
        assert_eq!(offered(None, true, true).1, once);
    }

    #[test]
    fn a_multi_choice_question_has_check_boxes_that_toggle() {
        let theme = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        let question = Question {
            kind: AskKind::Multiselect,
            options: vec!["tests".to_string(), "docs".to_string()],
            ..Question::text("What should change?")
        };
        let (reply, _answer) = tokio::sync::oneshot::channel();
        let mut overlay = Overlay::form(Form::new(vec![question], "Other..."), Action::Ask { reply });
        overlay.on_key(&key(KeyCode::Down));
        assert!(matches!(overlay.on_key(&key(KeyCode::Enter)), Outcome::Pending));
        let rows = shown(&overlay.render(44, 20, &theme).0);
        assert_eq!(
            rows,
            [
                "+- The agent asks -------------------------+",
                "| What should change?                      |",
                "|                                          |",
                "|   [ ] tests                              |",
                "| > [x] docs                               |",
                "|   Done  confirm the selection            |",
                "+- ^v move | enter tick | esc cancel ------+",
            ]
        );
        assert!(rows.iter().all(|row| row.is_ascii()));
        overlay.on_key(&key(KeyCode::Down));
        assert!(matches!(
            overlay.on_key(&key(KeyCode::Enter)),
            Outcome::Answers(answers) if answers == [["docs"]]
        ));
    }

    fn three_questions() -> Form {
        let mut store = Question {
            kind: AskKind::Select,
            options: vec!["sqlite".to_string(), "postgres".to_string()],
            ..Question::text("Which store should the service use?")
        };
        store.header = Some("Store".to_string());
        store.context = Some("The service keeps sessions.".to_string());
        store.recommended = Some("postgres".to_string());
        let mut auth = Question {
            kind: AskKind::Confirm,
            ..Question::text("Add sign-in now?")
        };
        auth.header = Some("Auth".to_string());
        let mut notes = Question::text("Anything else?");
        notes.header = Some("Notes".to_string());
        Form::new(vec![store, auth, notes], "Other...")
    }

    #[test]
    fn several_questions_are_one_box_with_tabs_and_a_tab_that_sends() {
        let theme = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        let (reply, _answer) = tokio::sync::oneshot::channel();
        let mut overlay = Overlay::form(three_questions(), Action::Form { reply });
        let (lines, cursor) = overlay.render(60, 20, &theme);
        assert_eq!(cursor, None, "a list has no typing cursor");
        assert_eq!(
            shown(&lines),
            [
                "+- The agent asks -----------------------------------------+",
                "| 1 Store  2 Auth  3 Notes  Send                           |",
                "|                                                          |",
                "| The service keeps sessions.                              |",
                "|                                                          |",
                "| Which store should the service use?                      |",
                "|                                                          |",
                "|   sqlite                                                 |",
                "| > postgres  recommended                                  |",
                "+- ^v move | enter pick | tab next question | esc cancel --+",
            ]
        );
        // The recommended row is the one a stray Enter takes; the next question follows.
        assert!(matches!(overlay.on_key(&key(KeyCode::Enter)), Outcome::Pending));
        assert!(matches!(overlay.on_key(&key(KeyCode::Tab)), Outcome::Pending));
        overlay.on_paste("ship it");
        let (lines, cursor) = overlay.render(60, 20, &theme);
        let rows = shown(&lines);
        assert_eq!(
            rows[1],
            "| 1 Store [+]  2 Auth  3 Notes  Send                       |"
        );
        assert_eq!(
            rows[3],
            "| Anything else?                                           |"
        );
        assert_eq!(
            rows[5],
            "| > ship it                                                |"
        );
        // Row 5 of the box, after the border, the space, the prompt and the text.
        assert_eq!(cursor, Some((5, 11)));
        assert!(
            overlay.editor_mut().is_some(),
            "a typed answer can be written in an editor"
        );

        assert!(matches!(overlay.on_key(&key(KeyCode::Enter)), Outcome::Pending));
        assert!(matches!(
            overlay.on_key(&key(KeyCode::Char('1'))),
            Outcome::Pending
        ));
        let rows = shown(&overlay.render(60, 20, &theme).0);
        assert_eq!(
            rows[3..10],
            [
                "| 1. Which store should the service use?                   |",
                "|    postgres                                              |",
                "| 2. Add sign-in now?                                      |",
                "|    yes                                                   |",
                "| 3. Anything else?                                        |",
                "|    ship it                                               |",
                "|                                                          |",
            ]
        );
        assert!(matches!(
            overlay.on_key(&key(KeyCode::Enter)),
            Outcome::Answers(answers) if answers == [vec!["postgres"], vec!["yes"], vec!["ship it"]]
        ));
    }

    #[test]
    fn a_form_keeps_its_tabs_and_its_list_when_the_text_is_too_long_for_the_box() {
        let theme = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        let mut long = Question {
            kind: AskKind::Select,
            options: vec!["apply".to_string(), "skip".to_string()],
            ..Question::text("Apply the patch?")
        };
        long.excerpt = Some(azazel_core::Excerpt::Code(
            (1..=40).map(|n| format!("line {n}\n")).collect::<String>(),
        ));
        let extra = Question::text("Why?");
        let (reply, _answer) = tokio::sync::oneshot::channel();
        let mut overlay = Overlay::form(Form::new(vec![long, extra], "Other..."), Action::Form { reply });
        for width in [30, 44, 80] {
            for rows in [8, 12, 20] {
                let (lines, _) = overlay.render(width, rows, &theme);
                let drawn = shown(&lines);
                assert!(drawn.len() <= rows, "{width}x{rows}: {} rows", drawn.len());
                assert!(
                    drawn.iter().all(|row| text::width(row) == width),
                    "{width}x{rows}"
                );
                let all = drawn.join("\n");
                assert!(all.contains("Send"), "{width}x{rows}: the tabs stay\n{all}");
                assert!(
                    all.contains("apply") && all.contains("skip"),
                    "{width}x{rows}\n{all}"
                );
                assert!(all.contains("more lines"), "{width}x{rows}\n{all}");
            }
        }
        // What was cut can be read whole.
        let ctrl_o = KeyEvent {
            modifiers: KeyModifiers::CONTROL,
            ..key(KeyCode::Char('o'))
        };
        assert!(matches!(overlay.on_key(&ctrl_o), Outcome::ReadBody));
        assert!(overlay.body_text().contains("line 40"));
    }
}
