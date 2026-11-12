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
        self.team.as_ref().map_or(0, |team| {
            team.list()
                .iter()
                .filter(|member| member.id != LEAD && member.state.is_active())
                .count()
        })
    }

    /// Whether a sub-agent is at work, so that the roster's clocks need redrawing.
    pub(super) fn agents_active(&self) -> bool {
        self.active_agents() > 0
    }

    /// One line per sub-agent that is queued, working or waiting for an approval.
    pub(super) fn roster_lines(&self, width: usize) -> Vec<Line> {
        let Some(team) = &self.team else {
            return Vec::new();
        };
        let rows: Vec<RosterRow> = team
            .list()
            .into_iter()
            .filter(|member| member.id != LEAD && member.state.is_active())
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
        roster::render(&rows, width, &self.theme)
    }

    pub(super) fn on_team(&mut self, event: TeamEvent) {
        let TeamEvent::Agent { id, event } = event else {
            // The roster is read afresh for every frame.
            return;
        };
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
                    let lines = view::tool_done(
                        &format!("[{id}] {}", tool.name),
                        &tool.input,
                        &output,
                        Duration::from_millis(duration_ms),
                        self.expand_tools,
                        width,
                        &self.theme,
                    );
                    self.say_card(lines);
                }
            }
            AgentEvent::Idle { .. } => self.info(&format!("[{id}] finished")),
            AgentEvent::Failed { error } => self.fail(&format!("[{id}] failed: {error}")),
            _ => {}
        }
    }

    /// Every agent of the session; choosing one shows its work in the transcript from then on.
    pub(super) fn open_agent_hub(&mut self) {
        let Some(team) = &self.team else {
            return self.info("this session has no team");
        };
        let members = team.list();
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
        let body =
            vec!["enter: show this agent's work here · m: message · s: stop · S: stop all".to_string()];
        let ids = members.into_iter().map(|member| member.id).collect();
        self.overlay = Some(Overlay::select("Agents", body, items, false, Action::Agents(ids)));
    }

    /// Acts on a key the agent hub understands beyond moving and choosing. `id` is the agent the
    /// selection is on. Returns whether the key was one of them.
    pub(super) fn hub_key(&mut self, key: char, id: &str) -> bool {
        let Some(team) = self.team.clone() else {
            return false;
        };
        match key {
            'm' => {
                let body = vec![format!("To {id}. It sees the message before its next step.")];
                self.overlay = Some(Overlay::input(
                    "Message",
                    body,
                    Action::AgentMessage(id.to_string()),
                ));
            }
            's' => {
                self.overlay = None;
                match team.stop(id) {
                    Ok(stopped) => self.info(&format!("stopped: {}", stopped.join(", "))),
                    Err(why) => self.fail(&why),
                }
            }
            'S' => {
                self.overlay = None;
                let stopped = team.stop_all();
                if stopped.is_empty() {
                    self.info("no agent is running");
                } else {
                    self.info(&format!("stopped: {}", stopped.join(", ")));
                }
            }
            _ => return false,
        }
        true
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

    pub(super) fn message_agent(&mut self, id: &str, message: &str) {
        if message.trim().is_empty() {
            return;
        }
        let sent = match &self.team {
            Some(team) => team.send_from_user(id, message.trim()),
            None => Err("this session has no team".to_string()),
        };
        match sent {
            Ok(reached) => self.info(&format!("sent to: {}", reached.join(", "))),
            Err(why) => self.fail(&why),
        }
    }
}
