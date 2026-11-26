//! The team in the terminal UI: the roster of working agents, the agent hub, and following one
//! agent's work in the transcript.

use std::time::Instant;

use azazel_agent::AgentEvent;
use azazel_team::{LEAD, MemberInfo, MemberState, TeamEvent};
use azazel_tui::components::{format_elapsed, format_tokens, markdown, roster};
use azazel_tui::{Glyph, Line, Role, RosterRow, RosterState, SelectItem, text};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use tokio::sync::broadcast;

use super::overlay::{Action, Kind, Overlay};
use super::{Tui, running, view};

/// Rows of agents shown above the input before the rest are counted as `… N more`.
const ROSTER_ROWS: usize = 3;

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
        self.roster
            .iter()
            .filter(|member| member.id != LEAD && member.state.is_active())
            .count()
    }

    /// Whether a sub-agent is at work, so that the roster's clocks need redrawing. Reads the
    /// roster of the last frame, which is drawn before every wait.
    pub(super) fn agents_active(&self) -> bool {
        self.active_agents() > 0
    }

    /// A header that counts the sub-agents at work, then one line for each of the first three
    /// that are queued, working or waiting for an approval, and `… N more` for the others.
    /// While the keyboard is in the roster every row is shown, the selected one marked, and a
    /// line under the rows says what the keys do.
    pub(super) fn roster_lines(&mut self, width: usize) -> Vec<Line> {
        let members = active_members(&self.roster);
        if members.is_empty() {
            return Vec::new();
        }
        let focus = self.fleet_focus(members.len());
        let header = self.roster_header(&members, width);
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
                    def: member.def,
                    activity,
                    elapsed: member.elapsed,
                    tokens: member.tokens,
                    state,
                }
            })
            .collect();
        let mut lines = vec![header];
        if focus.is_some() {
            lines.extend(roster::render(&rows, focus, width, &self.theme));
            lines.push(self.fleet_hint(width));
        } else {
            lines.extend(roster::collapsed(&rows, ROSTER_ROWS, width, &self.theme));
        }
        lines
    }

    /// `Agents · 2 running · 1 queued`, with the key of the hub at the right end when there
    /// is room for it.
    fn roster_header(&self, members: &[MemberInfo], width: usize) -> Line {
        let theme = &self.theme;
        let separator = theme.glyph(Glyph::Separator);
        let queued = members
            .iter()
            .filter(|member| member.state == MemberState::Queued)
            .count();
        let mut counts = format!("{separator}{} running", members.len() - queued);
        if queued > 0 {
            counts.push_str(&format!("{separator}{queued} queued"));
        }
        let left = format!(
            "{}{}",
            theme.paint(Role::Accent, "\x1b[1mAgents\x1b[22m"),
            theme.paint(Role::Muted, &counts)
        );
        let right = match self.keymap.keys_for("agent_hub").first() {
            Some(key) => theme.paint(Role::Muted, &format!("{key} hub")),
            None => String::new(),
        };
        let gap = width.saturating_sub(text::width(&left) + text::width(&right));
        if right.is_empty() || gap < 2 {
            return text::truncate(&left, width, theme.glyph(Glyph::Ellipsis));
        }
        format!("{left}{}{right}", " ".repeat(gap))
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
        if let Some(ended) = running::track(&mut self.followed_tools, &event) {
            self.tool_finished(super::calls::Finished::from_ended(Some(id.clone()), ended));
        }
        match *event {
            AgentEvent::MessageEnd { message } => {
                let said = message.text();
                if !said.trim().is_empty() {
                    let mut lines = view::styled(&format!("[{id}]"), Role::Accent, width, &self.theme);
                    lines.extend(markdown::render(&said, width, &self.theme));
                    self.say(lines);
                }
            }
            AgentEvent::Idle { .. } => self.info(&format!("[{id}] finished")),
            AgentEvent::Failed { error } => self.fail(&format!("[{id}] failed: {error}")),
            _ => {}
        }
    }

    /// `left` twice in an empty input opens the hub, when the session has sub-agents. Any other
    /// key starts the count again. True when the key opened the hub.
    pub(super) fn hub_tap(&mut self, key: &KeyEvent) -> bool {
        let empty = self.editor.is_empty() && self.completion.is_none();
        if key.code != KeyCode::Left || !key.modifiers.is_empty() || !empty {
            self.fleet.taps.reset();
            return false;
        }
        if key.kind == KeyEventKind::Repeat || !self.fleet.taps.press(Instant::now()) {
            return false;
        }
        self.read_recorded();
        let any =
            self.backend.roster().iter().any(|member| member.id != LEAD) || !self.fleet.recorded.is_empty();
        if any {
            self.open_hub_at(None);
        }
        any
    }

    /// The agents the session recorded as finished, earlier processes of it included.
    fn read_recorded(&mut self) {
        let days = self.env.config.team.agent_retention_days;
        let since = (days > 0)
            .then(|| jiff::Timestamp::now() - jiff::SignedDuration::from_hours(i64::from(days) * 24));
        self.fleet.recorded = self
            .session
            .with_entries(|entries| super::fleet::recorded_agents(entries, since));
    }

    /// Every sub-agent of the session, running and finished; `enter` opens one's chat.
    pub(super) fn open_agent_hub(&mut self) {
        self.read_recorded();
        self.open_hub_at(None);
    }

    /// Opens the hub with the selection on `at`, when it is there.
    pub(super) fn open_hub_at(&mut self, at: Option<&str>) {
        let (ids, items) = self.hub_rows();
        if ids.is_empty() {
            return self.info("this session has no sub-agents yet");
        }
        let index = at.and_then(|at| ids.iter().position(|id| id == at)).unwrap_or(0);
        self.fleet.taps.reset();
        self.overlay = Some(
            Overlay::select("Agents", Vec::new(), items, false, Action::Agents(ids))
                .with_keys(super::panels::hub_keys())
                .with_enter("open")
                .starting_at(index),
        );
    }

    /// The hub's rows as they stand now: the agents at work first, then the finished agents of
    /// the team, newest first, then those only the session's record knows, newest first.
    fn hub_rows(&self) -> (Vec<String>, Vec<SelectItem>) {
        let separator = self.theme.glyph(Glyph::Separator);
        let mut ids = Vec::new();
        let mut items = Vec::new();
        let members = self.backend.roster();
        for member in hub_order(&members) {
            let doing = match &member.state {
                MemberState::Running { activity } if !activity.is_empty() => Some(activity.clone()),
                _ => self.fleet.task(&member.id),
            };
            let mut parts = vec![member.state.label().to_string(), member.def.clone()];
            if let Some(parent) = &member.parent {
                parts.push(format!("from {parent}"));
            }
            parts.push(format_elapsed(member.elapsed));
            parts.push(format!("{} tokens", format_tokens(member.tokens)));
            if let Some(cost) = member.cost_usd {
                parts.push(format!("${cost:.2}"));
            }
            parts.extend(doing);
            items.push(SelectItem::new(self.hub_label(&member.id), parts.join(separator)));
            ids.push(member.id.clone());
        }
        for agent in &self.fleet.recorded {
            if members.iter().any(|member| member.id == agent.id) {
                continue;
            }
            let mut parts = vec![agent.status.clone(), agent.def.clone(), "earlier".to_string()];
            parts.extend(self.fleet.task(&agent.id));
            items.push(SelectItem::new(self.hub_label(&agent.id), parts.join(separator)));
            ids.push(agent.id.clone());
        }
        (ids, items)
    }

    fn hub_label(&self, id: &str) -> String {
        match self.follow.as_deref() == Some(id) {
            true => format!("{id} (following)"),
            false => id.to_string(),
        }
    }

    /// Brings the open hub up to date before it is drawn; the selection stays on its agent.
    pub(super) fn refresh_hub(&mut self) {
        let Some(overlay) = &self.overlay else {
            return;
        };
        let (Action::Agents(ids), Kind::Select { list, .. }) = (&overlay.action, &overlay.kind) else {
            return;
        };
        let selected = list.selected().and_then(|index| ids.get(index)).cloned();
        let (ids, items) = self.hub_rows();
        let at = selected.and_then(|selected| ids.iter().position(|id| *id == selected));
        if let Some(overlay) = &mut self.overlay
            && let Kind::Select { list, .. } = &mut overlay.kind
        {
            list.set_items(items);
            if let Some(at) = at {
                list.set_cursor(at);
            }
            overlay.action = Action::Agents(ids);
        }
    }

    /// Acts on one of the agent hub's own keys (see [`super::panels::HUB_ACTIONS`]). `id` is
    /// the agent the selection is on.
    pub(super) async fn hub_key(&mut self, action: &str, id: &str) {
        if action != "back" {
            self.fleet.taps.reset();
        }
        match action {
            "down" | "up" => {
                if let Some(overlay) = &mut self.overlay
                    && let Kind::Select { list, .. } = &mut overlay.kind
                {
                    match action {
                        "down" => list.move_down(),
                        _ => list.move_up(),
                    }
                }
            }
            "back" => {
                if self.fleet.taps.press(Instant::now()) {
                    self.overlay = None;
                }
            }
            "follow" => {
                self.overlay = None;
                match self.follow.as_deref() == Some(id) {
                    true => self.follow_agent(LEAD),
                    false => self.follow_agent(id),
                }
            }
            "message" => {
                let body = vec![format!("To {id}. It sees the message before its next step.")];
                self.overlay = Some(Overlay::input(
                    "Message",
                    body,
                    Action::AgentMessage(id.to_string()),
                ));
            }
            "stop" => match self.backend.stop_agent(id).await {
                Ok(stopped) if stopped.is_empty() => self.info(&format!("{id} is not running")),
                Ok(stopped) => self.info(&format!("stopped: {}", stopped.join(", "))),
                Err(why) => self.fail(&why),
            },
            "stop all" => match self.backend.stop_agent("all").await {
                Ok(stopped) if stopped.is_empty() => self.info("no agent is running"),
                Ok(stopped) => self.info(&format!("stopped: {}", stopped.join(", "))),
                Err(why) => self.fail(&why),
            },
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

/// The sub-agents of `members` in the hub's order: those at work (running, queued, waiting for
/// an approval) first, in the order they started, then the finished ones, newest first.
/// The sub-agents the roster shows: those queued, working or waiting for an approval.
pub(super) fn active_members(roster: &[MemberInfo]) -> Vec<MemberInfo> {
    roster
        .iter()
        .filter(|member| member.id != LEAD && member.state.is_active())
        .cloned()
        .collect()
}

fn hub_order(members: &[MemberInfo]) -> Vec<&MemberInfo> {
    let agents = members.iter().filter(|member| member.id != LEAD);
    let (mut active, mut finished): (Vec<_>, Vec<_>) = agents.partition(|member| member.state.is_active());
    finished.reverse();
    active.append(&mut finished);
    active
}

#[cfg(test)]
mod hub_tests {
    use std::time::Duration;

    use azazel_team::{MemberInfo, MemberState};

    fn member(id: &str, state: MemberState) -> MemberInfo {
        MemberInfo {
            id: id.to_string(),
            def: "coder".to_string(),
            parent: Some("lead".to_string()),
            depth: 1,
            state,
            elapsed: Duration::ZERO,
            tokens: 0,
            cost_usd: None,
        }
    }

    #[test]
    fn agents_at_work_come_first_and_finished_ones_newest_first() {
        let running = || MemberState::Running {
            activity: String::new(),
        };
        let members = [
            member("lead", MemberState::Idle),
            member("old", MemberState::Done),
            member("a", running()),
            member("failed", MemberState::Failed("x".into())),
            member("q", MemberState::Queued),
            member("new", MemberState::Done),
        ];
        let order: Vec<&str> = super::hub_order(&members).iter().map(|m| m.id.as_str()).collect();
        assert_eq!(order, ["a", "q", "new", "failed", "old"]);
    }
}
