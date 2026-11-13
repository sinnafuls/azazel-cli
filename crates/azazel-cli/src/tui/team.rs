//! The team in the terminal UI: the roster of working agents, the agent hub, and following one
//! agent's work in the transcript.

use std::time::{Duration, Instant};

use azazel_agent::AgentEvent;
use azazel_team::{LEAD, MemberState, TeamEvent};
use azazel_tui::components::{format_elapsed, format_tokens, markdown, roster};
use azazel_tui::{Line, Role, RosterRow, RosterState, SelectItem};
use tokio::sync::broadcast;

use super::overlay::{Action, Overlay};
use super::{RunningTool, Tui, view};

/// The next team event; never resolves when there is no team. A reader that fell behind is told
/// to look at the roster again.
pub(super) async fn next_event(events: &mut Option<broadcast::Receiver<TeamEvent>>) -> TeamEvent {
    loop {
        let Some(receiver) = events else {
            return std::future::pending().await;
        };
        match receiver.recv().await {
            Ok(event) => return event,
            Err(broadcast::error::RecvError::Lagged(_)) => return TeamEvent::Roster,
            Err(broadcast::error::RecvError::Closed) => *events = None,
        }
    }
}

impl Tui {
    /// How many sub-agents are queued, working or waiting for an approval.
    pub(super) fn active_agents(&self) -> usize {
        self.backend
            .roster()
            .iter()
            .filter(|member| member.id != LEAD && member.state.is_active())
            .count()
    }

    /// Whether a sub-agent is at work, so that the roster's clocks need redrawing.
    pub(super) fn agents_active(&self) -> bool {
        self.active_agents() > 0
    }

    /// One line per sub-agent that is queued, working or waiting for an approval. While the
    /// keyboard is in the roster the selected row is marked, and a line under the rows says
    /// what the keys do.
    pub(super) fn roster_lines(&mut self, width: usize) -> Vec<Line> {
        let members = self.fleet_rows();
        let focus = self.fleet_focus(members.len());
        let rows: Vec<RosterRow> = members
            .into_iter()
            .map(|member| {
                let (state, activity) = match &member.state {
                    MemberState::Queued => (RosterState::Queued, "queued".to_string()),
                    MemberState::WaitingApproval => {
                        (RosterState::Waiting, "waiting for your approval".to_string())
                    }
                    MemberState::Running { activity } => (RosterState::Running, activity.clone()),
                    other => (RosterState::Idle, other.label().to_string()),
                };
                RosterRow {
                    id: member.id,
                    activity,
                    elapsed: member.elapsed,
                    tokens: member.tokens,
                    state,
                }
            })
            .collect();
        let mut lines = roster::render(&rows, focus, width, &self.theme);
        if focus.is_some() {
            lines.push(self.fleet_hint(width));
        }
        lines
    }

    pub(super) fn on_team(&mut self, event: TeamEvent) {
        let (id, event) = match event {
            TeamEvent::Agent { id, event } => (id, event),
            TeamEvent::Input { id, text } => return self.fleet.log(&id).input(text),
            // The roster is read afresh for every frame.
            TeamEvent::Roster => return,
        };
        // Kept for the fleet view, whether the agent is followed or not.
        self.fleet.log(&id).event(&event);
        // A provider moving to another account concerns every agent, whoever ran into the limit.
        if let AgentEvent::Account {
            provider,
            from,
            to,
            until,
            limit,
        } = &*event
        {
            let line = crate::accounts::switch_line(provider, from, to, *until, limit);
            return self.info(&line);
        }
        if self.follow.as_deref() != Some(id.as_str()) {
            return;
        }
        let width = self.width();
        match *event {
            AgentEvent::MessageEnd { message } => {
                let said = message.text();
                if !said.trim().is_empty() {
                    let mut lines = view::styled(&format!("[{id}]"), Role::Accent, width, &self.theme);
                    lines.extend(markdown::render(&said, width, &self.theme));
                    self.say(lines);
                }
            }
            AgentEvent::ToolStart { call_id, name, input } => {
                self.followed_tools.push(RunningTool {
                    call_id,
                    name,
                    input,
                    tail: Vec::new(),
                    started: Instant::now(),
                });
            }
            AgentEvent::ToolEnd {
                call_id,
                output,
                duration_ms,
            } => {
                if let Some(at) = self
                    .followed_tools
                    .iter()
                    .position(|tool| tool.call_id == call_id)
                {
                    let tool = self.followed_tools.remove(at);
                    self.tool_finished(super::calls::Finished {
                        agent: Some(id),
                        name: tool.name,
                        input: tool.input,
                        output,
                        elapsed: Duration::from_millis(duration_ms),
                    });
                }
            }
            AgentEvent::Idle { .. } => self.info(&format!("[{id}] finished")),
            AgentEvent::Failed { error } => self.fail(&format!("[{id}] failed: {error}")),
            _ => {}
        }
    }

    /// Every agent of the session; choosing one shows its work in the transcript from then on.
    pub(super) fn open_agent_hub(&mut self) {
        let members = self.backend.roster();
        if members.is_empty() {
            return self.info("this session has no team");
        }
        let items = members
            .iter()
            .map(|member| {
                let mut detail = format!("{} · {}", member.def, member.state);
                if member.id != LEAD {
                    detail.push_str(&format!(" · {}", format_elapsed(member.elapsed)));
                }
                detail.push_str(&format!(" · {} tokens", format_tokens(member.tokens)));
                let followed = if self.follow.as_deref() == Some(member.id.as_str()) {
                    " (following)"
                } else {
                    ""
                };
                SelectItem::new(format!("{}{followed}", member.id), detail)
            })
            .collect();
        let body = vec!["enter shows the agent's work here; o opens it in a popup".to_string()];
        let ids = members.into_iter().map(|member| member.id).collect();
        self.overlay = Some(
            Overlay::select("Agents", body, items, false, Action::Agents(ids))
                .with_keys(super::panels::hub_keys()),
        );
    }

    /// Acts on one of the agent hub's own keys. `id` is the agent the selection is on.
    pub(super) async fn hub_key(&mut self, key: char, id: &str) {
        match key {
            'm' => {
                let body = vec![format!("To {id}. It sees the message before its next step.")];
                self.overlay = Some(Overlay::input(
                    "Message",
                    body,
                    Action::AgentMessage(id.to_string()),
                ));
            }
            'o' => self.open_agent_pane(id),
            's' => {
                self.overlay = None;
                match self.backend.stop_agent(id).await {
                    Ok(stopped) => self.info(&format!("stopped: {}", stopped.join(", "))),
                    Err(why) => self.fail(&why),
                }
            }
            'S' => {
                self.overlay = None;
                match self.backend.stop_agent("all").await {
                    Ok(stopped) if stopped.is_empty() => self.info("no agent is running"),
                    Ok(stopped) => self.info(&format!("stopped: {}", stopped.join(", "))),
                    Err(why) => self.fail(&why),
                }
            }
            _ => {}
        }
    }

    /// Shows `id`'s work in the transcript from now on; the lead's own work is always shown.
    pub(super) fn follow_agent(&mut self, id: &str) {
        self.followed_tools.clear();
        if id == LEAD {
            if self.follow.take().is_some() {
                self.info("no longer following a sub-agent");
            }
        } else {
            self.follow = Some(id.to_string());
            self.info(&format!(
                "following {id}: what it says and does appears here, marked [{id}]"
            ));
        }
    }

    /// `@name text` at the prompt. True when `name` is an agent or a definition and the line
    /// was dealt with: sent to the agent, or an agent started for it. `@name` alone opens the
    /// agent's popup.
    pub(super) async fn mention(&mut self, shown: &str, name: &str, text: &str) -> bool {
        if text.is_empty() {
            let known = self
                .backend
                .roster()
                .iter()
                .any(|member| member.id == name && member.id != LEAD);
            if known {
                self.open_agent_pane(name);
            }
            return known;
        }
        let Some(mentioned) = self.backend.mention(name, text).await else {
            return false;
        };
        let lines = view::user_prompt(shown, self.width(), &self.theme);
        self.say(lines);
        match mentioned {
            Ok(mentioned) if mentioned.started => self.info(&format!(
                "started {}; its report goes to the lead when it is done",
                mentioned.agent
            )),
            Ok(mentioned) => self.info(&format!("sent to: {}", mentioned.agent)),
            Err(why) => self.fail(&why),
        }
        true
    }

    /// What `@` completes at the start of a prompt before the files: the sub-agents, then the
    /// definitions an agent can be started from.
    pub(super) fn mention_items(&self) -> Vec<SelectItem> {
        let mut items: Vec<SelectItem> = self
            .backend
            .roster()
            .into_iter()
            .filter(|member| member.id != LEAD)
            .map(|member| SelectItem::new(member.id, format!("agent, {}", member.state.label())))
            .collect();
        items.extend(
            self.backend
                .agent_defs()
                .into_iter()
                .map(|(name, about)| SelectItem::new(name, format!("start an agent: {about}"))),
        );
        items
    }

    pub(super) async fn message_agent(&mut self, id: &str, message: &str) {
        if message.trim().is_empty() {
            return;
        }
        match self.backend.message_agent(id, message.trim()).await {
            Ok(reached) => self.info(&format!("sent to: {}", reached.join(", "))),
            Err(why) => self.fail(&why),
        }
    }
}
