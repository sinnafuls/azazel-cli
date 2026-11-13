//! `/btw`: a question on the side. Its answer streams into a panel that stands in for the input
//! while the agent keeps working; nothing of it is written to the session.

use std::sync::atomic::{AtomicU64, Ordering};

use azazel_core::side::SideAnswer;
use azazel_tui::components::{composer, markdown};
use azazel_tui::{Editor, Glyph, Line, Role, Spinner, Theme, text};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio_util::sync::CancellationToken;

use super::{Tui, first_line};
use crate::side::{Exchange, dropped_note};

/// Rows of questions and answers the panel shows at once; the rest scrolls.
const MAX_CONTENT_ROWS: usize = 18;
/// Rows the follow-up being typed may take.
const MAX_INPUT_ROWS: usize = 4;
/// How the message that hands a side answer to the agent starts.
const SENT_NOTE: &str = "I asked this on the side: ";

/// Each question gets its own number, so that what a closed or replaced one still sends is ignored.
static NEXT: AtomicU64 = AtomicU64::new(1);

/// What the request behind a panel reports back.
pub(super) enum Update {
    Text {
        id: u64,
        delta: String,
    },
    Done {
        id: u64,
        result: Result<SideAnswer, String>,
    },
}

#[derive(Debug, PartialEq)]
enum State {
    Answering,
    Answered,
    Failed(String),
}

/// What a key pressed in the panel asks the UI to do.
#[derive(Debug, PartialEq)]
pub(super) enum Pressed {
    Nothing,
    Close,
    Copy(String),
    /// Hand the answer to the agent.
    Send(Exchange),
    /// Ask this follow-up.
    Ask(String),
}

pub(super) struct Panel {
    id: u64,
    /// What was asked and answered before, oldest first.
    earlier: Vec<Exchange>,
    question: String,
    answer: String,
    state: State,
    /// Said under the answer: the model tried to use tools.
    note: Option<String>,
    /// A follow-up question being typed.
    follow_up: Option<Editor>,
    /// Rows scrolled up from the newest row.
    scroll: usize,
    /// How far `scroll` could go when the panel was last drawn.
    max_scroll: usize,
    cancel: CancellationToken,
}

impl Panel {
    fn new(id: u64, earlier: Vec<Exchange>, question: String, cancel: CancellationToken) -> Panel {
        Panel {
            id,
            earlier,
            question,
            answer: String::new(),
            state: State::Answering,
            note: None,
            follow_up: None,
            scroll: 0,
            max_scroll: 0,
            cancel,
        }
    }

    fn apply(&mut self, update: Update) {
        match update {
            Update::Text { id, delta } if id == self.id => self.answer.push_str(&delta),
            Update::Done { id, result } if id == self.id => match result {
                Ok(answer) => {
                    self.note = dropped_note(&answer.dropped_tools);
                    self.answer = answer.text;
                    self.state = State::Answered;
                }
                Err(why) => self.state = State::Failed(why),
            },
            // From a question that was closed or replaced since.
            _ => {}
        }
    }

    /// The exchanges a follow-up is asked after: a failed one is left out.
    fn exchanges(&self) -> Vec<Exchange> {
        let mut all = self.earlier.clone();
        if self.state == State::Answered && !self.answer.trim().is_empty() {
            all.push(Exchange {
                question: self.question.clone(),
                answer: self.answer.clone(),
            });
        }
        all
    }

    pub(super) fn on_paste(&mut self, pasted: &str) {
        if let Some(editor) = &mut self.follow_up {
            editor.insert_str(pasted);
        }
    }

    fn on_key(&mut self, key: &KeyEvent) -> Pressed {
        if key.kind == KeyEventKind::Release {
            return Pressed::Nothing;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let leave = key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c'));
        if let Some(editor) = &mut self.follow_up {
            if leave {
                self.follow_up = None;
            } else if key.code == KeyCode::Enter {
                let question = editor.text().trim().to_string();
                if !question.is_empty() {
                    self.follow_up = None;
                    return Pressed::Ask(question);
                }
            } else {
                editor.handle_key(key);
            }
            return Pressed::Nothing;
        }
        if leave {
            return Pressed::Close;
        }
        let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        let has_answer = !self.answer.trim().is_empty();
        match key.code {
            KeyCode::Char('c') if plain && has_answer => return Pressed::Copy(self.answer.clone()),
            KeyCode::Char('s') if plain && has_answer && self.state == State::Answered => {
                return Pressed::Send(Exchange {
                    question: self.question.clone(),
                    answer: self.answer.clone(),
                });
            }
            // A follow-up goes out after this answer, so it waits for it.
            KeyCode::Char('f') if plain && self.state != State::Answering => {
                self.follow_up = Some(Editor::new());
            }
            KeyCode::Up => self.scroll = (self.scroll + 1).min(self.max_scroll),
            KeyCode::Down => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageUp => self.scroll = (self.scroll + MAX_CONTENT_ROWS / 2).min(self.max_scroll),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(MAX_CONTENT_ROWS / 2),
            KeyCode::Home => self.scroll = self.max_scroll,
            KeyCode::End => self.scroll = 0,
            _ => {}
        }
        Pressed::Nothing
    }

    /// Every row of the side conversation, newest last.
    fn content(&self, width: usize, theme: &Theme) -> Vec<Line> {
        let mut lines = Vec::new();
        let question = |lines: &mut Vec<Line>, asked: &str| {
            let mark = theme.paint(Role::Accent, &theme.glyphs().prefix(Glyph::Prompt));
            for (at, row) in text::wrap(&text::sanitize(asked), width.saturating_sub(2).max(1))
                .into_iter()
                .enumerate()
            {
                let lead = if at == 0 { mark.as_str() } else { "  " };
                lines.push(format!("{lead}{}", theme.paint(Role::Muted, &row)));
            }
        };
        for exchange in &self.earlier {
            question(&mut lines, &exchange.question);
            lines.extend(markdown::render(&exchange.answer, width, theme));
            lines.push(String::new());
        }
        question(&mut lines, &self.question);
        if self.answer.trim().is_empty() {
            if self.state == State::Answering {
                let waiting = format!("waiting for the answer{}", theme.glyph(Glyph::Ellipsis));
                lines.push(theme.paint(Role::Muted, &waiting));
            } else if self.state == State::Answered && self.note.is_none() {
                lines.push(theme.paint(Role::Muted, "(the answer was empty)"));
            }
        } else {
            lines.extend(markdown::render(&self.answer, width, theme));
        }
        if let State::Failed(why) = &self.state {
            let failed = format!("no answer: {why}");
            lines.extend(text::wrap(
                &theme.paint(Role::Error, &text::sanitize(&failed)),
                width,
            ));
        }
        if let Some(note) = &self.note {
            lines.extend(text::wrap(&theme.paint(Role::Warning, note), width));
        }
        lines
    }

    /// The panel's rows, at most `max_rows`, and the cursor while a follow-up is typed.
    pub(super) fn render(
        &mut self,
        width: usize,
        max_rows: usize,
        theme: &Theme,
    ) -> (Vec<Line>, Option<(u16, u16)>) {
        let input = self.follow_up.as_ref().map(|editor| {
            let prompt = theme.paint(Role::Accent, &theme.glyphs().prefix(Glyph::Prompt));
            editor.view(width, MAX_INPUT_ROWS, theme, &prompt)
        });
        // Two rules, and a third above the follow-up.
        let chrome = 2 + input.as_ref().map_or(0, |view| view.lines.len() + 1);
        let room = max_rows.saturating_sub(chrome).clamp(1, MAX_CONTENT_ROWS);
        let content = self.content(width, theme);
        self.max_scroll = content.len().saturating_sub(room);
        self.scroll = self.scroll.min(self.max_scroll);
        let end = content.len() - self.scroll;
        let start = end.saturating_sub(room);

        let more = |arrow: &str, hidden: usize| (hidden > 0).then(|| format!("{arrow} {hidden} more"));
        let title = theme.paint(Role::Accent, "btw");
        let mut lines = vec![composer::rule(
            width,
            Some(&title),
            more(theme.glyph(Glyph::MoreUp), start).as_deref(),
            Role::Border,
            theme,
        )];
        lines.extend(content[start..end].iter().cloned());
        let mut cursor = None;
        let hints = |hints: &[&str]| hints.join(theme.glyph(Glyph::Separator));
        let hint = match (&input, &self.state) {
            (Some(_), _) => hints(&["enter ask", "esc back"]),
            (None, State::Answering) => hints(&[
                &format!("{} answering", Spinner::now_in(theme.glyphs())),
                "esc close",
            ]),
            (None, State::Answered) if self.answer.trim().is_empty() => hints(&["f follow-up", "esc close"]),
            (None, State::Answered) => hints(&[
                "c copy",
                "s send to the agent",
                "f follow-up",
                &format!(
                    "{}{} scroll",
                    theme.glyph(Glyph::MoreUp),
                    theme.glyph(Glyph::MoreDown)
                ),
                "esc close",
            ]),
            (None, State::Failed(_)) => hints(&["f ask again", "esc close"]),
        };
        if let Some(view) = input {
            let label = theme.paint(Role::Muted, "follow-up");
            lines.push(composer::rule(width, Some(&label), None, Role::Border, theme));
            cursor = Some((lines.len() as u16 + view.cursor.0, view.cursor.1));
            lines.extend(view.lines);
        }
        let hint = theme.paint(Role::Muted, &hint);
        lines.push(composer::rule(
            width,
            Some(&hint),
            more(theme.glyph(Glyph::MoreDown), self.scroll).as_deref(),
            Role::Border,
            theme,
        ));
        (lines, cursor)
    }
}

impl Tui {
    /// `/btw <question>`: asks about the session without interrupting it.
    pub(super) fn btw_command(&mut self, args: &str) {
        if args.is_empty() {
            return self.fail("usage: /btw <question>");
        }
        self.ask_on_the_side(args.to_string(), Vec::new());
    }

    /// Sends `question` on the side and opens the panel its answer streams into. `earlier` is
    /// what was asked on the side before, for a follow-up.
    fn ask_on_the_side(&mut self, question: String, earlier: Vec<Exchange>) {
        if let Some(open) = self.btw.take() {
            open.cancel.cancel();
        }
        if !self.backend.has_model() {
            return self.fail("no model is set up yet: /settings adds one");
        }
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        let asked = self.backend.btw(
            id,
            question.clone(),
            earlier.clone(),
            self.internal_tx.clone(),
            cancel.clone(),
        );
        if let Err(why) = asked {
            return self.fail(&why);
        }
        self.btw = Some(Panel::new(id, earlier, question, cancel));
    }

    pub(super) fn on_btw(&mut self, update: Update) {
        if let Some(panel) = &mut self.btw {
            panel.apply(update);
        }
    }

    /// A key while the panel is open: all of them are the panel's.
    pub(super) async fn btw_key(&mut self, key: KeyEvent) {
        let Some(panel) = &mut self.btw else {
            return;
        };
        match panel.on_key(&key) {
            Pressed::Nothing => {}
            Pressed::Close => {
                if let Some(panel) = self.btw.take() {
                    panel.cancel.cancel();
                }
            }
            Pressed::Copy(answer) => self.copy_text(&answer, "the answer"),
            Pressed::Ask(question) => {
                let earlier = panel.exchanges();
                self.ask_on_the_side(question, earlier);
            }
            Pressed::Send(exchange) => {
                self.btw = None;
                self.send_side_answer(exchange).await;
            }
        }
    }

    /// Hands a side answer to the agent: while it works, as a message it reads before its next
    /// request; when it is idle, as a prompt like any other.
    async fn send_side_answer(&mut self, exchange: Exchange) {
        let message = format!(
            "{SENT_NOTE}{}\n\nThe answer was:\n{}",
            exchange.question.trim(),
            exchange.answer.trim()
        );
        self.info(&format!(
            "sent the answer to the agent: {}",
            first_line(&exchange.question)
        ));
        if self.busy {
            self.backend.steer(message, Vec::new());
        } else {
            self.send_prompt(message).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use azazel_ai::{ModelRef, Usage};

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn panel() -> Panel {
        Panel::new(7, Vec::new(), "why two parsers?".into(), CancellationToken::new())
    }

    fn answered(text: &str, dropped: &[&str]) -> Update {
        Update::Done {
            id: 7,
            result: Ok(SideAnswer {
                text: text.to_string(),
                model: ModelRef::new("mock", "lead"),
                usage: Usage::default(),
                dropped_tools: dropped.iter().map(|tool| tool.to_string()).collect(),
            }),
        }
    }

    fn plain(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|line| text::strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_keys_wait_for_the_answer_they_act_on() {
        let mut panel = panel();
        // Nothing to copy or send yet, and a follow-up waits for the answer.
        for early in ['c', 's', 'f'] {
            assert_eq!(panel.on_key(&key(KeyCode::Char(early))), Pressed::Nothing);
        }
        assert!(panel.follow_up.is_none());
        panel.apply(Update::Text {
            id: 7,
            delta: "One is ".into(),
        });
        // An update of a question that was closed is not this panel's.
        panel.apply(Update::Text {
            id: 6,
            delta: "stale".into(),
        });
        assert_eq!(
            panel.on_key(&key(KeyCode::Char('c'))),
            Pressed::Copy("One is ".into())
        );
        assert_eq!(panel.on_key(&key(KeyCode::Char('s'))), Pressed::Nothing);

        panel.apply(answered("One is legacy.", &[]));
        let exchange = Exchange {
            question: "why two parsers?".into(),
            answer: "One is legacy.".into(),
        };
        assert_eq!(
            panel.on_key(&key(KeyCode::Char('s'))),
            Pressed::Send(exchange.clone())
        );
        assert_eq!(panel.exchanges(), [exchange]);
        assert_eq!(panel.on_key(&key(KeyCode::Esc)), Pressed::Close);
    }

    #[test]
    fn a_follow_up_is_typed_in_the_panel_and_esc_leaves_it_first() {
        let mut panel = panel();
        panel.apply(answered("One is legacy.", &[]));
        assert_eq!(panel.on_key(&key(KeyCode::Char('f'))), Pressed::Nothing);
        // Letters are text now, not commands; an empty question is not asked.
        assert_eq!(panel.on_key(&key(KeyCode::Enter)), Pressed::Nothing);
        for typed in "cs".chars() {
            assert_eq!(panel.on_key(&key(KeyCode::Char(typed))), Pressed::Nothing);
        }
        panel.on_paste(" which?");
        let (lines, cursor) = panel.render(60, 20, &Theme::dark());
        assert!(plain(&lines).contains("❯ cs which?"), "{}", plain(&lines));
        assert!(cursor.is_some());
        assert_eq!(panel.on_key(&key(KeyCode::Esc)), Pressed::Nothing);
        assert!(panel.follow_up.is_none());

        panel.on_key(&key(KeyCode::Char('f')));
        panel.on_paste("which one?");
        assert_eq!(
            panel.on_key(&key(KeyCode::Enter)),
            Pressed::Ask("which one?".into())
        );
    }

    #[test]
    fn the_panel_shows_the_newest_rows_and_scrolls_back() {
        let theme = Theme::dark();
        let mut panel = panel();
        let (lines, cursor) = panel.render(60, 20, &theme);
        let shown = plain(&lines);
        assert!(shown.contains("btw") && shown.contains("❯ why two parsers?"));
        assert!(shown.contains("waiting for the answer…") && shown.contains("answering · esc close"));
        assert_eq!(cursor, None);

        let long: String = (1..=40).map(|n| format!("line {n}\n\n")).collect();
        panel.apply(answered(&long, &["shell"]));
        let (lines, _) = panel.render(60, 12, &theme);
        assert_eq!(lines.len(), 12);
        let shown = plain(&lines);
        assert!(shown.contains("the model tried to use shell; ask in the main conversation"));
        assert!(shown.contains("line 40") && !shown.contains("line 1\n"));
        assert!(shown.contains("more") && shown.contains("s send to the agent"));

        panel.on_key(&key(KeyCode::Home));
        let (lines, _) = panel.render(60, 12, &theme);
        let shown = plain(&lines);
        assert!(shown.contains("❯ why two parsers?") && !shown.contains("line 40"));
        panel.on_key(&key(KeyCode::End));
        assert_eq!(panel.scroll, 0);
    }

    #[test]
    fn a_failed_question_can_be_asked_again_and_is_not_context() {
        let mut panel = panel();
        panel.apply(Update::Done {
            id: 7,
            result: Err("the provider is overloaded".into()),
        });
        let (lines, _) = panel.render(60, 20, &Theme::dark());
        let shown = plain(&lines);
        assert!(shown.contains("no answer: the provider is overloaded"));
        assert!(shown.contains("f ask again"));
        assert_eq!(panel.exchanges(), []);
        assert_eq!(panel.on_key(&key(KeyCode::Char('s'))), Pressed::Nothing);
        panel.on_key(&key(KeyCode::Char('f')));
        assert!(panel.follow_up.is_some());
    }
}
