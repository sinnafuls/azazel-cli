//! The fleet view: the roster as a list to move in, and one agent's work in a popup as it
//! happens, with a line to type to it.
//!
//! What every sub-agent is handed, says and does is kept here as it arrives, whether anyone
//! looks or not, so that opening an agent shows how it got where it is. Only the newest part
//! is kept; the agent's transcript file has all of it.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;

use azazel_agent::AgentEvent;
use azazel_ai::{Block, StopReason, StreamEvent};
use azazel_team::{LEAD, MemberInfo};
use azazel_tui::components::{format_elapsed, format_tokens, markdown};
use azazel_tui::{Editor, Glyph, Hint, Line, Role, Scroll, Theme, text};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::calls::Finished;
use super::overlay::{Action, Kind, Overlay, Widget};
use super::running::{self, RunningTool};
use super::{Tui, view};

/// Finished things kept per agent. Past this the oldest go.
const MAX_ITEMS: usize = 400;
/// Rows of reasoning in flight shown under what is finished.
const THINKING_ROWS: usize = 4;
/// The most rows the line typed to an agent takes.
const INPUT_ROWS: usize = 3;

/// One finished thing of an agent's work.
enum Item {
    /// What the agent was handed: its task, or a message sent to it since.
    Input(String),
    Reply(String),
    Thinking(String),
    Tool(Box<Finished>),
    /// Something that happened to the run: it ended, it failed, a request was sent again.
    Note(String, Role),
}

/// How an agent's work is drawn.
#[derive(Clone, Copy)]
pub(super) struct Look<'a> {
    pub theme: &'a Theme,
    pub show: view::Show<'a>,
    pub thinking: bool,
}

/// What one sub-agent was handed, said and did, newest last.
#[derive(Default)]
pub(super) struct Log {
    items: Vec<Item>,
    /// The reply being written.
    stream: String,
    /// The reasoning being written.
    thinking: String,
    tools: Vec<RunningTool>,
    /// Counts everything that happened, so a view knows when it is out of date.
    version: u64,
    /// The finished items as rows: the width they were laid out for and how many items they
    /// cover.
    drawn: Option<(usize, usize, Vec<Line>)>,
}

/// A message as the view shows it: one from another agent names its sender instead of
/// carrying the tags it travels in.
fn handed(text: &str) -> Cow<'_, str> {
    let inner = text
        .strip_prefix("<agent-message from=\"")
        .and_then(|rest| rest.strip_suffix("</agent-message>"))
        .and_then(|rest| rest.split_once("\">"));
    match inner {
        Some((from, message)) => Cow::Owned(format!("from {from}: {}", message.trim())),
        None => Cow::Borrowed(text),
    }
}

impl Log {
    fn push(&mut self, item: Item) {
        self.items.push(item);
        if self.items.len() > MAX_ITEMS {
            self.items.drain(..self.items.len() - MAX_ITEMS);
            self.drawn = None;
        }
    }

    /// The agent was handed `text`.
    pub(super) fn input(&mut self, text: String) {
        self.version += 1;
        self.push(Item::Input(text));
    }

    /// Something the UI itself has to say in the agent's view.
    fn note(&mut self, text: String, role: Role) {
        self.version += 1;
        self.push(Item::Note(text, role));
    }

    pub(super) fn event(&mut self, event: &AgentEvent) {
        self.version += 1;
        if let Some(ended) = running::track(&mut self.tools, event) {
            self.push(Item::Tool(Box::new(Finished::from_ended(None, ended))));
        }
        match event {
            AgentEvent::MessageStart => {
                self.stream.clear();
                self.thinking.clear();
            }
            AgentEvent::Stream(StreamEvent::TextDelta { delta, .. }) => self.stream.push_str(delta),
            AgentEvent::Stream(StreamEvent::ThinkingDelta { delta, .. }) => self.thinking.push_str(delta),
            AgentEvent::Stream(StreamEvent::BlockEnd {
                block: Block::Thinking { .. },
                ..
            }) => {
                let thinking = std::mem::take(&mut self.thinking);
                if !thinking.trim().is_empty() {
                    self.push(Item::Thinking(thinking.trim().to_string()));
                }
            }
            AgentEvent::MessageEnd { message } => {
                self.stream.clear();
                self.thinking.clear();
                let said = message.text();
                if !said.trim().is_empty() {
                    self.push(Item::Reply(said));
                }
            }
            AgentEvent::Retry {
                attempt,
                model,
                stalled_secs,
            } => {
                self.stream.clear();
                self.thinking.clear();
                let line = crate::routing::retry_line(&model.to_string(), *attempt, *stalled_secs);
                self.push(Item::Note(line, Role::Muted));
            }
            AgentEvent::Rule { name } => {
                self.stream.clear();
                self.thinking.clear();
                self.push(Item::Note(
                    format!("rule {name} stopped the reply; retrying"),
                    Role::Muted,
                ));
            }
            AgentEvent::Idle { stop } => {
                self.stream.clear();
                self.thinking.clear();
                let how = match stop {
                    StopReason::Aborted => "stopped",
                    _ => "finished",
                };
                self.push(Item::Note(how.to_string(), Role::Muted));
            }
            AgentEvent::Failed { error } => {
                self.stream.clear();
                self.thinking.clear();
                self.push(Item::Note(format!("failed: {error}"), Role::Error));
            }
            _ => {}
        }
    }

    /// Whether what is drawn changes although nothing happens: a running call shows how long
    /// it has been running.
    fn ticks(&self) -> bool {
        !self.tools.is_empty()
    }

    fn draw_item(item: &Item, width: usize, look: Look<'_>, out: &mut Vec<Line>) {
        let theme = look.theme;
        match item {
            Item::Input(text) => out.extend(view::user_prompt(&handed(text), width, theme)),
            Item::Reply(text) => {
                out.push(String::new());
                out.extend(markdown::render(text, width, theme));
            }
            Item::Thinking(text) => {
                if !look.thinking {
                    return;
                }
                out.extend(view::thinking(text, width, theme));
            }
            Item::Tool(call) => {
                // Every card stands apart, as in the transcript.
                out.push(String::new());
                let call = view::Call {
                    agent: None,
                    name: &call.name,
                    input: &call.input,
                    output: &call.output,
                    elapsed: Some(call.elapsed),
                };
                out.extend(view::tool_card(call, look.show, width, theme));
            }
            Item::Note(text, role) => out.extend(view::styled(text, *role, width, theme)),
        }
    }

    /// Everything as rows `width` wide: what is finished, then what is in flight. The finished
    /// part is laid out once per width and added to as items arrive.
    pub(super) fn rows(&mut self, width: usize, look: Look<'_>) -> Vec<Line> {
        let (mut covered, mut lines) = match self.drawn.take() {
            Some((drawn, covered, lines)) if drawn == width && covered <= self.items.len() => {
                (covered, lines)
            }
            _ => (0, Vec::new()),
        };
        for item in &self.items[covered..] {
            Log::draw_item(item, width, look, &mut lines);
        }
        covered = self.items.len();
        let mut rows = lines.clone();
        self.drawn = Some((width, covered, lines));

        let theme = look.theme;
        if !self.stream.trim().is_empty() {
            rows.push(String::new());
            rows.extend(markdown::render(&self.stream, width, theme));
        } else if look.thinking && !self.thinking.trim().is_empty() {
            let wrapped = view::thinking(&self.thinking, width, theme);
            let from = wrapped.len().saturating_sub(THINKING_ROWS);
            rows.extend(wrapped.into_iter().skip(from));
        }
        for tool in &self.tools {
            rows.push(String::new());
            rows.extend(view::tool_running(
                &tool.name,
                &tool.input,
                &tool.tail,
                tool.started.elapsed(),
                look.show.redact,
                width,
                theme,
            ));
        }
        rows
    }
}

/// The work of every sub-agent, and where the keyboard is in the roster.
#[derive(Default)]
pub(super) struct Fleet {
    logs: HashMap<String, Log>,
    /// The row of the roster the keyboard is on; `None` while it is in the input.
    pub focus: Option<usize>,
}

impl Fleet {
    pub(super) fn log(&mut self, agent: &str) -> &mut Log {
        self.logs.entry(agent.to_string()).or_default()
    }

    /// The session changed: its agents went with it.
    pub(super) fn clear(&mut self) {
        self.logs.clear();
        self.focus = None;
    }
}

/// What a key did in the popup of an agent.
pub enum PaneKey {
    Pending,
    Close,
    /// Send this to the agent; the popup stays.
    Send(String),
    /// Stop the agent; the popup stays.
    Stop,
}

/// One agent's work in a popup: the rows of it, a place in them, and a line to type to it.
pub struct Pane {
    pub agent: String,
    lines: Vec<Line>,
    /// The version of the log and the width `lines` was drawn from.
    stamp: Option<(u64, usize)>,
    /// Rows scrolled up from the newest.
    up: Cell<usize>,
    /// Rows of the transcript the last render showed: the distance of a page.
    page: Cell<usize>,
    editor: Editor,
}

impl Pane {
    pub fn new(agent: &str) -> Pane {
        Pane {
            agent: agent.to_string(),
            lines: Vec::new(),
            stamp: None,
            up: Cell::new(0),
            page: Cell::new(10),
            editor: Editor::new(),
        }
    }

    pub fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    /// Takes the agent's work as it is now. A place scrolled back to stays where it is while
    /// rows are added below it.
    pub(super) fn set_lines(&mut self, lines: Vec<Line>) {
        if self.up.get() > 0 {
            let added = lines.len().saturating_sub(self.lines.len());
            self.up.set(self.up.get() + added);
        }
        self.lines = lines;
    }

    pub fn on_key(&mut self, key: &KeyEvent) -> PaneKey {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let empty = self.editor.is_empty();
        let page = self.page.get().max(1);
        match key.code {
            KeyCode::Esc => return PaneKey::Close,
            KeyCode::Char('c') if ctrl => return PaneKey::Close,
            KeyCode::Char('x') if ctrl => return PaneKey::Stop,
            KeyCode::PageUp => self.up.set(self.up.get().saturating_add(page)),
            KeyCode::PageDown => self.up.set(self.up.get().saturating_sub(page)),
            // With nothing typed the arrows move in the transcript.
            KeyCode::Up if empty => self.up.set(self.up.get().saturating_add(1)),
            KeyCode::Down if empty => self.up.set(self.up.get().saturating_sub(1)),
            KeyCode::Home if empty => self.up.set(usize::MAX),
            KeyCode::End if empty => self.up.set(0),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => self.editor.insert_newline(),
            KeyCode::Enter => {
                if !self.editor.text().trim().is_empty() {
                    // What is sent is read where it lands: at the newest end.
                    self.up.set(0);
                    return PaneKey::Send(self.editor.take());
                }
            }
            _ => {
                self.editor.handle_key(key);
            }
        }
        PaneKey::Pending
    }

    pub fn on_paste(&mut self, pasted: &str) {
        self.editor.insert_str(pasted);
    }

    /// The transcript above a rule and the input under it, filling `room` rows.
    pub fn widget(&self, width: usize, room: usize, prompt: &str, theme: &Theme) -> Widget {
        let input = self
            .editor
            .view(width, INPUT_ROWS.min((room / 3).max(1)), theme, prompt);
        let rows = room.saturating_sub(input.lines.len() + 1);
        let total = self.lines.len();
        // A place past the first row is the first row.
        let up = self.up.get().min(total.saturating_sub(rows));
        self.up.set(up);
        self.page.set(rows.saturating_sub(1).max(1));
        let end = total - up;
        let start = end.saturating_sub(rows);
        let mut lines: Vec<Line> = self.lines[start..end].to_vec();
        // A short transcript starts at the top; the input stays at the bottom.
        lines.resize(rows, String::new());
        let scroll = (total > rows && rows > 0).then_some((
            0,
            Scroll {
                offset: start,
                visible: rows,
                total,
            },
        ));
        if room > input.lines.len() {
            let rule = theme.glyphs().border().horizontal.repeat(width);
            lines.push(theme.paint(Role::BorderMuted, &rule));
        }
        let cursor = Some((
            lines.len() + usize::from(input.cursor.0),
            usize::from(input.cursor.1),
        ));
        lines.extend(input.lines);
        Widget {
            lines,
            cursor,
            scroll,
        }
    }

    pub fn hints(&self) -> Vec<Hint> {
        vec![
            Hint::new("enter", "send"),
            Hint::new("pgup/pgdn", "scroll"),
            Hint::new("ctrl+x", "stop"),
            Hint::new("esc", "close"),
        ]
    }
}

/// `@name text` at the start of what was typed: the name, and the text after it. The name is
/// one an agent or a definition can have; anything else after `@` is a file.
pub(super) fn mention(typed: &str) -> Option<(&str, &str)> {
    let rest = typed.strip_prefix('@')?;
    let (name, text) = match rest.split_once(char::is_whitespace) {
        Some((name, text)) => (name, text.trim()),
        None => (rest, ""),
    };
    let named = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    named.then_some((name, text))
}

impl Tui {
    /// The sub-agents the roster shows: those queued, working or waiting for an approval.
    pub(super) fn fleet_rows(&self) -> Vec<MemberInfo> {
        self.backend
            .roster()
            .into_iter()
            .filter(|member| member.id != LEAD && member.state.is_active())
            .collect()
    }

    /// The row of the roster the keyboard is on, when it is in the roster and that row is
    /// still there.
    pub(super) fn fleet_focus(&mut self, rows: usize) -> Option<usize> {
        let focus = self.fleet.focus?;
        if rows == 0 {
            self.fleet.focus = None;
            return None;
        }
        // An agent above the selection finished: the selection stays on a row.
        let focus = focus.min(rows - 1);
        self.fleet.focus = Some(focus);
        Some(focus)
    }

    /// What the keys do while the keyboard is in the roster, under its rows.
    pub(super) fn fleet_hint(&self, width: usize) -> Line {
        let separator = self.theme.glyph(Glyph::Separator);
        let arrows = format!(
            "{}{}",
            self.theme.glyph(Glyph::MoreUp),
            self.theme.glyph(Glyph::MoreDown)
        );
        let hint = [
            format!("{arrows} move"),
            "enter open".to_string(),
            "x stop".to_string(),
            "esc back".to_string(),
        ]
        .join(separator);
        text::truncate(
            &format!("  {}", self.theme.paint(Role::Muted, &hint)),
            width,
            self.theme.glyph(Glyph::Ellipsis),
        )
    }

    /// `down` in an empty input: the keyboard goes to the roster, when it has rows.
    pub(super) fn enter_fleet(&mut self) -> bool {
        if self.fleet_rows().is_empty() {
            return false;
        }
        self.fleet.focus = Some(0);
        true
    }

    /// A key while the keyboard is in the roster. False when the key is not the roster's: the
    /// keyboard is back in the input, which takes it.
    pub(super) async fn fleet_key(&mut self, key: &KeyEvent) -> bool {
        let rows = self.fleet_rows();
        let Some(focus) = self.fleet_focus(rows.len()) else {
            return false;
        };
        let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        match key.code {
            // Up from the first row is the way back, as down was the way in.
            KeyCode::Up if plain && focus == 0 => self.fleet.focus = None,
            KeyCode::Up if plain => self.fleet.focus = Some(focus - 1),
            KeyCode::Down if plain => self.fleet.focus = Some((focus + 1).min(rows.len() - 1)),
            KeyCode::Esc => self.fleet.focus = None,
            KeyCode::Enter if plain => {
                self.fleet.focus = None;
                self.open_agent_pane(&rows[focus].id);
            }
            KeyCode::Char('x') if plain => {
                let id = rows[focus].id.clone();
                match self.backend.stop_agent(&id).await {
                    Ok(stopped) => self.info(&format!("stopped: {}", stopped.join(", "))),
                    Err(why) => self.fail(&why),
                }
            }
            _ => {
                self.fleet.focus = None;
                return false;
            }
        }
        true
    }

    /// Opens the popup that shows `id` at work.
    pub(super) fn open_agent_pane(&mut self, id: &str) {
        if !self.backend.roster().iter().any(|member| member.id == id) {
            return self.fail(&format!("no agent named `{id}`"));
        }
        // Laid out afresh: the theme or the symbols may have changed since it was last open.
        self.fleet.log(id).drawn = None;
        self.overlay = Some(Overlay::pane(Pane::new(id), Action::AgentPane));
    }

    /// Brings the open popup of an agent up to date before it is drawn `width` columns wide
    /// inside its box: its title says how the agent stands, its rows are the agent's work.
    pub(super) fn refresh_pane(&mut self, width: usize) {
        let Some(mut overlay) = self.overlay.take() else {
            return;
        };
        if let Kind::Pane(pane) = &mut overlay.kind {
            let separator = self.theme.glyph(Glyph::Separator);
            let member = self
                .backend
                .roster()
                .into_iter()
                .find(|member| member.id == pane.agent);
            overlay.title = match member {
                Some(member) => [
                    member.id.clone(),
                    member.def.clone(),
                    member.state.to_string(),
                    format_elapsed(member.elapsed),
                    format!("{} tokens", format_tokens(member.tokens)),
                ]
                .join(separator),
                None => format!("{}{separator}gone", pane.agent),
            };
            let cwd = self.local_cwd();
            let look = Look {
                theme: &self.theme,
                show: view::Show {
                    expanded: self.expand_tools,
                    redact: self.env.config.tui.redact,
                    cwd: cwd.as_deref(),
                },
                thinking: self.show_thinking,
            };
            let log = self.fleet.logs.entry(pane.agent.clone()).or_default();
            let stamp = Some((log.version, width));
            if pane.stamp != stamp || log.ticks() {
                let mut lines = log.rows(width, look);
                if lines.is_empty() {
                    lines = view::notice("nothing yet", width, &self.theme);
                }
                pane.set_lines(lines);
                pane.stamp = stamp;
            }
        }
        self.overlay = Some(overlay);
    }

    /// `ctrl+x` in the popup of an agent.
    pub(super) async fn pane_stop(&mut self) {
        let Some(Kind::Pane(pane)) = self.overlay.as_ref().map(|overlay| &overlay.kind) else {
            return;
        };
        let id = pane.agent.clone();
        if let Err(why) = self.backend.stop_agent(&id).await {
            self.fleet
                .log(&id)
                .note(format!("not stopped: {why}"), Role::Error);
        }
    }

    /// What was typed in the popup of an agent goes to it: a steer message while it works, a
    /// new prompt when it has stopped.
    pub(super) async fn pane_send(&mut self, typed: String) {
        let Some(Kind::Pane(pane)) = self.overlay.as_ref().map(|overlay| &overlay.kind) else {
            return;
        };
        let id = pane.agent.clone();
        if let Err(why) = self.backend.message_agent(&id, typed.trim()).await {
            // Said where the user is looking.
            self.fleet.log(&id).note(format!("not sent: {why}"), Role::Error);
        }
    }
}

#[cfg(test)]
mod tests {
    use azazel_agent::ToolOutput;
    use azazel_ai::Message;
    use crossterm::event::{KeyEventKind, KeyEventState};
    use serde_json::json;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|line| text::strip_ansi(line)).collect()
    }

    fn look(theme: &Theme) -> Look<'_> {
        Look {
            theme,
            show: view::Show::default(),
            thinking: true,
        }
    }

    fn said(text: &str) -> Message {
        Message {
            role: azazel_ai::Role::Assistant,
            content: vec![Block::text(text)],
            meta: None,
        }
    }

    fn text_delta(delta: &str) -> AgentEvent {
        AgentEvent::Stream(StreamEvent::TextDelta {
            index: 0,
            delta: delta.to_string(),
        })
    }

    #[test]
    fn a_log_shows_the_task_the_reply_as_it_is_written_and_the_calls() {
        let theme = Theme::dark();
        let mut log = Log::default();
        log.input("Find where the lexer drops a token.".to_string());
        log.event(&AgentEvent::AgentStart);
        log.event(&AgentEvent::MessageStart);
        log.event(&text_delta("Looking at "));
        log.event(&text_delta("the lexer."));
        let rows = plain(&log.rows(60, look(&theme)));
        let shown = rows.join("\n");
        assert!(shown.contains("Find where the lexer drops a token."), "{shown}");
        assert!(
            shown.ends_with("Looking at the lexer."),
            "the reply in flight is the last row: {shown}"
        );

        // The message ends: it is said once, not twice.
        log.event(&AgentEvent::MessageEnd {
            message: said("Looking at the lexer."),
        });
        log.event(&AgentEvent::ToolStart {
            call_id: "c1".to_string(),
            name: "grep".to_string(),
            input: json!({"pattern": "next_token"}),
        });
        let running = plain(&log.rows(60, look(&theme))).join("\n");
        assert_eq!(running.matches("Looking at the lexer.").count(), 1, "{running}");
        assert!(
            running.contains("grep") && running.contains("next_token"),
            "{running}"
        );
        assert!(log.ticks(), "a running call shows its time");

        log.event(&AgentEvent::ToolEnd {
            call_id: "c1".to_string(),
            output: ToolOutput::text("src/lex.rs:40: fn next_token()"),
            duration_ms: 120,
        });
        log.event(&AgentEvent::Idle {
            stop: StopReason::Stop,
        });
        let done = plain(&log.rows(60, look(&theme)));
        let shown = done.join("\n");
        assert!(shown.contains("src/lex.rs"), "{shown}");
        assert_eq!(done.last().map(String::as_str), Some("finished"));
        assert!(!log.ticks());

        // Another width lays everything out again, within that width.
        for width in [20, 34, 80] {
            for row in log.rows(width, look(&theme)) {
                assert!(text::width(&row) <= width, "width {width}: {row:?}");
            }
        }
    }

    #[test]
    fn the_finished_part_is_laid_out_once_and_added_to() {
        let theme = Theme::dark();
        let mut log = Log::default();
        log.input("task".to_string());
        let first = log.rows(40, look(&theme));
        assert!(matches!(&log.drawn, Some((40, 1, _))));
        log.event(&AgentEvent::MessageEnd {
            message: said("done"),
        });
        let second = log.rows(40, look(&theme));
        assert_eq!(second[..first.len()], first[..]);
        assert!(matches!(&log.drawn, Some((40, 2, _))));

        // Only the newest items are kept, and the rows follow.
        for n in 0..MAX_ITEMS + 10 {
            log.input(format!("message {n}"));
        }
        assert_eq!(log.items.len(), MAX_ITEMS);
        let rows = plain(&log.rows(40, look(&theme))).join("\n");
        assert!(!rows.contains("task"), "the oldest went");
        assert!(rows.contains(&format!("message {}", MAX_ITEMS + 9)));
    }

    #[test]
    fn a_message_of_another_agent_names_its_sender() {
        assert_eq!(
            handed("<agent-message from=\"lead\">\nlook at src/lex.rs\n</agent-message>"),
            "from lead: look at src/lex.rs"
        );
        assert_eq!(handed("look at src/lex.rs"), "look at src/lex.rs");
    }

    #[test]
    fn the_popup_follows_the_newest_rows_until_it_is_scrolled_back() {
        let theme = Theme::dark();
        let mut pane = Pane::new("scout-1");
        let numbered = |count: usize| (0..count).map(|n| format!("row {n}")).collect::<Vec<_>>();
        pane.set_lines(numbered(30));
        // 12 rows: 10 of transcript, the rule, one of input.
        let shown = plain(&pane.widget(40, 12, "> ", &theme).lines);
        assert_eq!(shown.len(), 12);
        assert_eq!((shown[0].as_str(), shown[9].as_str()), ("row 20", "row 29"));
        assert_eq!(shown[10], "─".repeat(40));
        assert_eq!(pane.widget(40, 12, "> ", &theme).cursor, Some((11, 2)));

        // New rows come into view while the view is at the end.
        pane.set_lines(numbered(32));
        assert_eq!(plain(&pane.widget(40, 12, "> ", &theme).lines)[9], "row 31");

        // Scrolled back, the view stays on the rows it shows while more arrive below.
        assert!(matches!(pane.on_key(&key(KeyCode::PageUp)), PaneKey::Pending));
        let back = plain(&pane.widget(40, 12, "> ", &theme).lines);
        assert_eq!((back[0].as_str(), back[9].as_str()), ("row 13", "row 22"));
        pane.set_lines(numbered(40));
        let still = plain(&pane.widget(40, 12, "> ", &theme).lines);
        assert_eq!((still[0].as_str(), still[9].as_str()), ("row 13", "row 22"));
        let scroll = pane.widget(40, 12, "> ", &theme).scroll.unwrap().1;
        assert_eq!((scroll.offset, scroll.visible, scroll.total), (13, 10, 40));

        // Home goes to the first row and no further; end returns to the newest.
        pane.on_key(&key(KeyCode::Home));
        assert_eq!(plain(&pane.widget(40, 12, "> ", &theme).lines)[0], "row 0");
        pane.on_key(&key(KeyCode::Up));
        assert_eq!(plain(&pane.widget(40, 12, "> ", &theme).lines)[0], "row 0");
        pane.on_key(&key(KeyCode::End));
        assert_eq!(plain(&pane.widget(40, 12, "> ", &theme).lines)[9], "row 39");

        // A short transcript starts at the top and leaves the input where it is.
        pane.set_lines(numbered(2));
        let short = plain(&pane.widget(40, 12, "> ", &theme).lines);
        assert_eq!(
            (short[0].as_str(), short[1].as_str(), short[2].as_str()),
            ("row 0", "row 1", "")
        );
        assert!(pane.widget(40, 12, "> ", &theme).scroll.is_none());
        for room in 1..8 {
            assert_eq!(pane.widget(40, room, "> ", &theme).lines.len(), room.max(1));
        }
    }

    #[test]
    fn typing_in_the_popup_sends_and_ctrl_x_stops() {
        let mut pane = Pane::new("scout-1");
        // Nothing typed: enter sends nothing.
        assert!(matches!(pane.on_key(&key(KeyCode::Enter)), PaneKey::Pending));
        for c in "x marks it".chars() {
            assert!(matches!(pane.on_key(&key(KeyCode::Char(c))), PaneKey::Pending));
        }
        assert!(matches!(pane.on_key(&key(KeyCode::Enter)), PaneKey::Send(text) if text == "x marks it"));
        assert!(pane.editor.is_empty(), "the line is ready for the next message");

        let ctrl_x = KeyEvent {
            modifiers: KeyModifiers::CONTROL,
            ..key(KeyCode::Char('x'))
        };
        assert!(matches!(pane.on_key(&ctrl_x), PaneKey::Stop));
        assert!(matches!(pane.on_key(&key(KeyCode::Esc)), PaneKey::Close));
    }

    #[test]
    fn a_mention_is_a_name_an_agent_can_have_at_the_start() {
        assert_eq!(mention("@scout-1 look again"), Some(("scout-1", "look again")));
        assert_eq!(
            mention("@scout   find the lexer\nplease"),
            Some(("scout", "find the lexer\nplease"))
        );
        assert_eq!(mention("@scout-1"), Some(("scout-1", "")));
        // A path, an address and text that only contains a mention are not one.
        assert_eq!(mention("@src/main.rs explain"), None);
        assert_eq!(mention("@README.md"), None);
        assert_eq!(mention("see @scout-1"), None);
        assert_eq!(mention("@ scout"), None);
    }
}
