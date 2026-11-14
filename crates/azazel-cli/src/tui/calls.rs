//! Finished tool calls on their way into the transcript.
//!
//! In the card style each call is written as its card the moment it ends, with a blank row
//! above it. In the compact style (`tui.tool_style = "compact"`) a call is one row, and calls
//! that end one after another stand together as a tree under a row that counts them. The calls
//! of a group wait in the live region until something else is said (the reply goes on, the run
//! ends) and are written then, all at once. Calls that are still running stand in the same
//! tree, each with a spinner and the last lines it printed.
//!
//! Every call written is kept with what it returned in the [history](super::history), so that
//! `ctrl+o` can draw the transcript again with all of their output, or less.

use std::time::Duration;

use azazel_agent::ToolOutput;
use azazel_ai::Block;
use azazel_core::config::ToolStyle;
use azazel_tui::{HistoryItem, Line};
use serde_json::Value;

use super::history::Said;
use super::running::{Ended, RunningTool};
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
    pub(super) fn from_ended(agent: Option<String>, ended: Ended) -> Finished {
        Finished {
            agent,
            name: ended.tool.name,
            input: ended.tool.input,
            output: ended.output,
            elapsed: ended.elapsed,
        }
    }

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
    /// Where in the history the group written last is, which `ctrl+o` shows as full cards
    /// where the transcript cannot be drawn again.
    pub last: Option<usize>,
}

impl Tui {
    /// Whether a call that ends now is shown as a row rather than a card.
    pub(super) fn compact_calls(&self) -> bool {
        self.env.config.tui.tool_style == ToolStyle::Compact && !self.expand_tools
    }

    fn draw_calls<'a>(
        &self,
        calls: impl IntoIterator<Item = &'a Finished>,
        width: usize,
    ) -> Vec<view::Drawn> {
        let cwd = self.local_cwd();
        let show = view::Show {
            expanded: false,
            redact: self.env.config.tui.redact,
            cwd: cwd.as_deref(),
        };
        calls
            .into_iter()
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

    /// A group of finished calls as transcript rows: a tree of compact rows, set apart from
    /// what precedes it unless that is another group (`after_rows`), or a card for each call,
    /// each with a blank row above it.
    pub(super) fn draw_group<'a>(
        &self,
        calls: impl IntoIterator<Item = &'a Finished> + Clone,
        width: usize,
        after_rows: &mut bool,
        out: &mut Vec<HistoryItem>,
    ) {
        if self.compact_calls() {
            if !*after_rows {
                out.push(String::new().into());
            }
            let rows = view::tool_rows(&self.draw_calls(calls, width), width, &self.theme);
            out.extend(rows.into_iter().map(HistoryItem::Line));
            *after_rows = true;
            return;
        }
        for call in calls {
            out.push(String::new().into());
            out.extend(self.call_card(call, width).into_iter().map(HistoryItem::Line));
        }
        *after_rows = false;
    }

    /// A tool call ended: its card goes to the transcript, or its row joins the group.
    pub(super) fn tool_finished(&mut self, done: Finished) {
        let pictured = done
            .output
            .content
            .iter()
            .any(|block| matches!(block, Block::Image { .. }));
        // A picture stands right under the call it came from.
        let pictures = match pictured {
            true => self.pictures(&done.output),
            false => Vec::new(),
        };
        if !self.compact_calls() {
            self.flush_calls();
        }
        self.calls.waiting.push(done);
        self.calls.drawn = None;
        // Outside a run nothing follows that would end the group.
        if !self.compact_calls() || !self.busy || pictured {
            self.flush_calls();
        }
        if !pictures.is_empty() {
            self.keep(Said::Drawn, pictures);
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
        let mut rows = Vec::new();
        let mut after_rows = self.after_rows;
        self.draw_group(&calls, width, &mut after_rows, &mut rows);
        self.after_rows = after_rows;
        self.transcript.extend(rows);
        self.calls.last = Some(self.history.end());
        self.history.push(Said::Calls(calls), width, Vec::new());
    }

    /// Reads the waiting calls for `width` once, so that a frame only lays their rows out.
    pub(super) fn fit_calls(&mut self, width: usize) {
        let fits = matches!(&self.calls.drawn, Some((drawn, _)) if *drawn == width);
        if !fits && !self.calls.waiting.is_empty() {
            self.calls.drawn = Some((width, self.draw_calls(&self.calls.waiting, width)));
        }
    }

    /// The calls in flight of the lead and of the followed sub-agent, the sub-agent's marked
    /// with its name.
    fn in_flight_calls(&self) -> impl Iterator<Item = (String, &RunningTool)> {
        let lead = self.tools.iter().map(|tool| (tool.name.clone(), tool));
        let agent = self.follow.as_deref().unwrap_or_default();
        let followed = self
            .followed_tools
            .iter()
            .map(move |tool| (format!("[{agent}] {}", tool.name), tool));
        lead.chain(followed)
    }

    /// What tool calls there are to show above the input: in the compact style the group that
    /// is still growing with the calls in flight at its end, else a card per call in flight,
    /// each with a blank row above it.
    pub(super) fn live_calls(&self, width: usize) -> Vec<Line> {
        let redact = self.env.config.tui.redact;
        if !self.compact_calls() {
            let mut lines = Vec::new();
            for (label, tool) in self.in_flight_calls() {
                lines.push(String::new());
                lines.extend(view::tool_running(
                    &label,
                    &tool.input,
                    &tool.tail,
                    tool.started.elapsed(),
                    redact,
                    width,
                    &self.theme,
                ));
            }
            return lines;
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
            .in_flight_calls()
            .map(|(label, tool)| {
                let elapsed = tool.started.elapsed();
                view::running_row(&label, &tool.input, &tool.tail, elapsed, redact, &self.theme)
            })
            .collect();
        if finished.is_empty() && running.is_empty() {
            return Vec::new();
        }
        let mut lines = Vec::new();
        if !self.after_rows {
            lines.push(String::new());
        }
        lines.extend(view::tool_rows(
            finished.iter().chain(&running),
            width,
            &self.theme,
        ));
        lines
    }

    /// The look of tool calls changed (style, redaction, theme): what waits is drawn again, or
    /// written when calls are no longer compact.
    pub(super) fn restyle_calls(&mut self) {
        self.calls.drawn = None;
        if !self.compact_calls() {
            self.flush_calls();
        }
    }

    /// `ctrl+o` turned full tool output on where the transcript cannot be drawn again: in the
    /// compact style the calls of the group on screen are written as the cards they stand for.
    pub(super) fn expand_calls(&mut self) {
        if self.env.config.tui.tool_style != ToolStyle::Compact {
            return;
        }
        if !self.calls.waiting.is_empty() {
            return self.flush_calls();
        }
        let width = self.width();
        let mut rows = Vec::new();
        let mut after_rows = self.after_rows;
        if let Some(Said::Calls(calls)) = self.calls.last.and_then(|at| self.history.said(at)) {
            self.draw_group(calls, width, &mut after_rows, &mut rows);
        }
        // Written again, not said again: the history has these calls already.
        self.calls.last = None;
        self.after_rows = after_rows;
        self.transcript.extend(rows);
    }

    /// The conversation moved: what waits is written, and the last group is not this
    /// conversation's any more.
    pub(super) fn forget_calls(&mut self) {
        self.flush_calls();
        self.calls.last = None;
    }
}
