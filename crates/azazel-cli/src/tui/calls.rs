//! Finished tool calls on their way into the transcript.
//!
//! In the card style each call is written as its card the moment it ends. In the compact style
//! (`tui.tool_style = "compact"`) a call is one row, and calls that end one after another stand
//! together as a tree under a row that counts them. What is in the terminal's scrollback cannot
//! be drawn again, so the calls of a group wait in the live region until something else is said
//! (the reply goes on, the run ends) and are written then, all at once. Calls that are still
//! running stand in the same tree, each with a spinner and the last lines it printed.

use std::time::Duration;

use azazel_agent::ToolOutput;
use azazel_ai::Block;
use azazel_core::config::ToolStyle;
use azazel_tui::Line;
use serde_json::Value;

use super::{Tui, view};

/// A tool call that ended.
pub(super) struct Finished {
    /// The sub-agent that made it, when it is not the lead.
    pub agent: Option<String>,
    pub name: String,
    pub input: Value,
    pub output: ToolOutput,
    pub elapsed: Duration,
}

impl Finished {
    fn call(&self) -> view::Call<'_> {
        view::Call {
            agent: self.agent.as_deref(),
            name: &self.name,
            input: &self.input,
            output: &self.output,
            elapsed: Some(self.elapsed),
        }
    }
}

#[derive(Default)]
pub(super) struct Calls {
    /// The calls of the group that is still growing, shown in the live region.
    waiting: Vec<Finished>,
    /// `waiting` read for display, and the width that was for: a frame only lays the rows out.
    drawn: Option<(usize, Vec<view::Drawn>)>,
    /// The calls of the group written last, which `ctrl+o` shows as full cards.
    last: Vec<Finished>,
}

impl Tui {
    /// Whether a call that ends now is shown as a row rather than a card.
    fn compact_calls(&self) -> bool {
        self.env.config.tui.tool_style == ToolStyle::Compact && !self.expand_tools
    }

    fn draw_calls(&self, calls: &[Finished], width: usize) -> Vec<view::Drawn> {
        let cwd = self.local_cwd();
        let show = view::Show {
            expanded: false,
            redact: self.env.config.tui.redact,
            cwd: cwd.as_deref(),
        };
        calls
            .iter()
            .map(|call| view::tool_row(call.call(), show, width, &self.theme))
            .collect()
    }

    fn call_card(&self, call: &Finished, width: usize) -> Vec<Line> {
        let cwd = self.local_cwd();
        let show = view::Show {
            expanded: self.expand_tools,
            redact: self.env.config.tui.redact,
            cwd: cwd.as_deref(),
        };
        view::tool_card(call.call(), show, width, &self.theme)
    }

    /// A tool call ended: its card goes to the transcript, or its row joins the group.
    pub(super) fn tool_finished(&mut self, done: Finished) {
        let pictured = done
            .output
            .content
            .iter()
            .any(|block| matches!(block, Block::Image { .. }));
        if !self.compact_calls() {
            self.flush_calls();
            let lines = self.call_card(&done, self.width());
            self.say_card(lines);
            return self.show_images(&done.output);
        }
        self.calls.waiting.push(done);
        self.calls.drawn = None;
        // Outside a run nothing follows that would end the group, and a picture stands right
        // under the call it came from.
        if !self.busy || pictured {
            self.flush_calls();
        }
        if pictured && let Some(done) = self.calls.last.pop() {
            self.show_images(&done.output);
            self.calls.last.push(done);
        }
    }

    /// Writes the calls that wait to the transcript: the group ends here.
    pub(super) fn flush_calls(&mut self) {
        if self.calls.waiting.is_empty() {
            return;
        }
        let calls = std::mem::take(&mut self.calls.waiting);
        self.calls.drawn = None;
        let width = self.width();
        let lines = view::tool_rows(&self.draw_calls(&calls, width), width, &self.theme);
        self.say_card(lines);
        self.calls.last = calls;
    }

    /// Reads the waiting calls for `width` once, so that a frame only lays their rows out.
    pub(super) fn fit_calls(&mut self, width: usize) {
        let fits = matches!(&self.calls.drawn, Some((drawn, _)) if *drawn == width);
        if !fits && !self.calls.waiting.is_empty() {
            self.calls.drawn = Some((width, self.draw_calls(&self.calls.waiting, width)));
        }
    }

    /// What tool calls there are to show above the input: in the compact style the group that
    /// is still growing with the calls in flight at its end, else a card per call in flight.
    pub(super) fn live_calls(&self, width: usize) -> Vec<Line> {
        let redact = self.env.config.tui.redact;
        if !self.compact_calls() {
            return self
                .tools
                .iter()
                .flat_map(|tool| {
                    view::tool_running(
                        &tool.name,
                        &tool.input,
                        &tool.tail,
                        tool.started.elapsed(),
                        redact,
                        width,
                        &self.theme,
                    )
                })
                .collect();
        }
        let fresh;
        let finished = match &self.calls.drawn {
            Some((drawn, rows)) if *drawn == width => rows,
            _ => {
                fresh = self.draw_calls(&self.calls.waiting, width);
                &fresh
            }
        };
        let running: Vec<view::Drawn> = self
            .tools
            .iter()
            .map(|tool| {
                let elapsed = tool.started.elapsed();
                view::running_row(&tool.name, &tool.input, &tool.tail, elapsed, redact, &self.theme)
            })
            .collect();
        view::tool_rows(finished.iter().chain(&running), width, &self.theme)
    }

    /// The look of tool calls changed (style, redaction, theme): what waits is drawn again, or
    /// written when calls are no longer compact.
    pub(super) fn restyle_calls(&mut self) {
        self.calls.drawn = None;
        if !self.compact_calls() {
            self.flush_calls();
        }
    }

    /// `ctrl+o` turned full tool output on: in the compact style the calls of the group on
    /// screen are written as the cards they stand for.
    pub(super) fn expand_calls(&mut self) {
        if self.env.config.tui.tool_style != ToolStyle::Compact {
            return;
        }
        let calls = match self.calls.waiting.is_empty() {
            true => std::mem::take(&mut self.calls.last),
            false => std::mem::take(&mut self.calls.waiting),
        };
        self.calls.drawn = None;
        let width = self.width();
        for call in &calls {
            let lines = self.call_card(call, width);
            self.say_card(lines);
        }
    }

    /// The conversation moved: what waits is written, and the last group is not this
    /// conversation's any more.
    pub(super) fn forget_calls(&mut self) {
        self.flush_calls();
        self.calls.last.clear();
    }
}
