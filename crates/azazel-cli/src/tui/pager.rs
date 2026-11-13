//! The full-screen viewer: the transcript of the session, or anything else worth reading with
//! search and copy (a diff, a changelog).
//!
//! It takes the alternate screen, so the transcript in the terminal's scrollback is untouched,
//! and it is the only place where the mouse is reported to azazel (for the wheel).

use std::collections::HashMap;
use std::io::Write;

use azazel_agent::ToolOutput;
use azazel_ai::{Block, Role as MessageRole};
use azazel_core::EntryKind;
use azazel_tui::components::diff;
use azazel_tui::components::pager::{self, Item, Outcome, Pager, WHEEL_ROWS};
use azazel_tui::{Role, Theme};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseEventKind};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use serde_json::Value;

use super::{Tui, clip, view};

/// One thing the viewer shows. It is rendered again whenever the width changes.
pub enum Source {
    /// One of your prompts. `[` and `]` jump between these.
    Prompt(String),
    /// Markdown: a reply, a changelog. Its fenced code blocks can be copied one by one.
    Markdown(String),
    /// The model's reasoning.
    Thinking(String),
    /// A finished tool call, shown as the card the transcript shows.
    Tool {
        name: String,
        input: Value,
        output: ToolOutput,
    },
    /// The unified diff of one file (`/diff`). `[` and `]` jump between these.
    Diff { file: String, text: String },
    /// A line of muted text: something the UI says, not the conversation.
    Note(String),
}

impl Source {
    fn item(&self, width: usize, theme: &Theme) -> Item {
        match self {
            Source::Prompt(text) => Item {
                lines: view::user_prompt(text, width, theme),
                source: text.clone(),
                anchor: true,
                blocks: Vec::new(),
            },
            Source::Markdown(text) => {
                let mut item = pager::markdown_item(text, width, theme);
                // A reply stands apart from what precedes it, as in the live transcript.
                item.lines.insert(0, String::new());
                for block in &mut item.blocks {
                    block.rows = block.rows.start + 1..block.rows.end + 1;
                }
                item
            }
            Source::Thinking(text) => Item {
                lines: view::thinking(text, width, theme),
                source: text.clone(),
                ..Item::default()
            },
            Source::Tool { name, input, output } => {
                let mut lines = vec![String::new()];
                lines.extend(view::tool_card(
                    name, input, output, None, true, None, width, theme,
                ));
                let said = output.text_content();
                Item {
                    lines,
                    source: format!(
                        "`{name}` {}\n\n```\n{}\n```",
                        view::tool_summary(name, input),
                        said.trim_end()
                    ),
                    ..Item::default()
                }
            }
            Source::Diff { file, text } => {
                let mut lines = vec![String::new()];
                lines.extend(view::styled(file, Role::Accent, width, theme));
                lines.extend(diff::render_unified(text, width, theme));
                Item {
                    lines,
                    source: text.clone(),
                    anchor: true,
                    blocks: Vec::new(),
                }
            }
            Source::Note(text) => Item {
                lines: view::notice(text, width, theme),
                source: text.clone(),
                ..Item::default()
            },
        }
    }
}

/// Where the viewer opens: a transcript at its newest end, a document at its start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    Top,
    Bottom,
}

fn items(sources: &[Source], width: usize, theme: &Theme) -> Vec<Item> {
    let width = pager::content_width(width);
    sources.iter().map(|source| source.item(width, theme)).collect()
}

/// Shows `sources` full screen under `title` until the user leaves with `q` or `esc`.
///
/// This is the entry point for anything that wants a scrollable, searchable view: `/transcript`,
/// `/diff`, `/changelog`. The agent keeps working meanwhile; a request that needs the user
/// (an approval, a question) is announced in the viewer's last row and shown on return.
pub(super) async fn show(tui: &mut Tui, title: &str, sources: Vec<Source>, start: Start) {
    let (mut width, mut height) = tui.renderer.size();
    let mut view = Pager::new(title, items(&sources, usize::from(width), &tui.theme));
    if start == Start::Top {
        view.to_top();
    }
    let _ = tui.renderer.suspend();
    let _ = crossterm::execute!(tui.renderer.get_mut(), EnterAlternateScreen, EnableMouseCapture);
    loop {
        if draw(tui, &mut view, width, height).is_err() {
            break;
        }
        tokio::select! {
            event = super::next_terminal(&mut tui.terminal) => match event {
                Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => match view.on_key(&key) {
                    Outcome::Pending => {}
                    Outcome::Close => break,
                    Outcome::Copy { text, what } => {
                        let how = clip::copy(&text, tui.renderer.get_mut());
                        view.set_notice(format!("copied {what} ({}) to {how}", clip::size(&text)));
                    }
                    Outcome::Open(link) => {
                        crate::login::open_in_browser(&link);
                        view.set_notice(format!("opening {link}"));
                    }
                },
                Some(Ok(Event::Mouse(mouse))) => match mouse.kind {
                    MouseEventKind::ScrollUp => view.scroll(-WHEEL_ROWS),
                    MouseEventKind::ScrollDown => view.scroll(WHEEL_ROWS),
                    _ => {}
                },
                Some(Ok(Event::Paste(pasted))) => view.paste(&pasted),
                Some(Ok(Event::FocusGained)) => tui.attention.focus(true),
                Some(Ok(Event::FocusLost)) => tui.attention.focus(false),
                Some(Ok(Event::Resize(columns, rows))) => {
                    (width, height) = (columns, rows);
                    tui.renderer.resize(columns, rows);
                    tui.floor.resize(rows);
                    view.set_items(items(&sources, usize::from(width), &tui.theme));
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            request = tui.requests.recv() => {
                if let Some(request) = request {
                    tui.waiting.push_back(request);
                    view.set_notice("the agent is waiting for you: q returns to it");
                }
            }
        }
    }
    // The viewer hid the cursor; the inline renderer believes it is showing.
    let _ = tui.renderer.get_mut().write_all(b"\x1b[?25h");
    let _ = crossterm::execute!(tui.renderer.get_mut(), DisableMouseCapture, LeaveAlternateScreen);
}

/// Paints one screen of the viewer.
fn draw(tui: &mut Tui, view: &mut Pager, width: u16, height: u16) -> std::io::Result<()> {
    let (columns, rows) = (usize::from(width), usize::from(height));
    let lines = view.render(columns, rows, &tui.theme);
    // One synchronized update from the top-left corner; each row is erased to its end first.
    let mut frame = String::from("\x1b[?2026h\x1b[H");
    for (row, line) in lines.iter().enumerate() {
        if row > 0 {
            frame.push_str("\r\n");
        }
        frame.push_str("\x1b[2K");
        frame.push_str(line);
        frame.push_str("\x1b[0m");
    }
    frame.push_str("\x1b[J");
    match view.typing_cursor(columns, rows) {
        Some((row, column)) => frame.push_str(&format!("\x1b[{};{}H\x1b[?25h", row + 1, column + 1)),
        None => frame.push_str("\x1b[?25l"),
    }
    frame.push_str("\x1b[?2026l");
    let out = tui.renderer.get_mut();
    out.write_all(frame.as_bytes())?;
    out.flush()
}

impl Tui {
    /// The session branch as the viewer shows it: prompts, replies, tool calls with their
    /// results, and thinking when it is shown in the transcript too.
    fn transcript_sources(&self) -> Vec<Source> {
        let branch = self.active.session.branch(None);
        let mut results: HashMap<String, ToolOutput> = HashMap::new();
        for entry in &branch {
            let EntryKind::Message { message } = &entry.kind else {
                continue;
            };
            for block in &message.content {
                if let Block::ToolResult {
                    call_id,
                    content,
                    is_error,
                } = block
                {
                    results.insert(
                        call_id.clone(),
                        ToolOutput {
                            content: content.clone(),
                            is_error: *is_error,
                            details: None,
                        },
                    );
                }
            }
        }
        let mut sources = Vec::new();
        for entry in branch {
            match entry.kind {
                EntryKind::Message { message } => match message.role {
                    MessageRole::User => {
                        let text = message.text();
                        let images = message
                            .content
                            .iter()
                            .filter(|block| matches!(block, Block::Image { .. }))
                            .count();
                        if super::slash::is_note(&text) {
                            sources.push(Source::Note(text));
                        } else if !text.trim().is_empty() || images > 0 {
                            let attached = match images {
                                0 => String::new(),
                                1 => "\n[1 image]".to_string(),
                                n => format!("\n[{n} images]"),
                            };
                            sources.push(Source::Prompt(format!("{}{attached}", text.trim_end())));
                        }
                    }
                    MessageRole::Assistant => {
                        for block in message.content {
                            match block {
                                Block::Text { text } if !text.trim().is_empty() => {
                                    sources.push(Source::Markdown(text));
                                }
                                Block::Thinking {
                                    text,
                                    redacted: false,
                                    ..
                                } if self.show_thinking && !text.trim().is_empty() => {
                                    sources.push(Source::Thinking(text));
                                }
                                Block::ToolCall { id, name, input, .. } => {
                                    let output = results
                                        .remove(&id)
                                        .unwrap_or_else(|| ToolOutput::error("(no result was recorded)"));
                                    sources.push(Source::Tool { name, input, output });
                                }
                                _ => {}
                            }
                        }
                    }
                    MessageRole::Tool => {}
                },
                EntryKind::Compaction { summary, .. } => {
                    sources.push(Source::Note(
                        "the conversation up to here was summarised for the model:".to_string(),
                    ));
                    sources.push(Source::Markdown(summary));
                }
                EntryKind::ModelChange { chain } => {
                    let models: Vec<String> = chain.iter().map(ToString::to_string).collect();
                    sources.push(Source::Note(format!("model: {}", models.join(", "))));
                }
                _ => {}
            }
        }
        sources
    }

    /// `f2`, `/transcript`: the whole conversation of this branch, full screen.
    pub(super) async fn open_transcript(&mut self) {
        let sources = self.transcript_sources();
        if sources.is_empty() {
            return self.info("this session has no messages yet");
        }
        let title = match self.active.session.name() {
            Some(name) => format!("Transcript · {name}"),
            None => "Transcript".to_string(),
        };
        show(self, &title, sources, Start::Bottom).await;
    }
}
