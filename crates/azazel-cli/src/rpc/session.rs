//! The session commands of RPC mode: reporting the session, moving between sessions and within
//! one, and writing it out.

use std::collections::HashSet;
use std::sync::Arc;

use azazel_agent::{AgentCmd, UserInput};
use azazel_ai::Role;
use azazel_ai::providers::presets;
use azazel_core::session::{Entry, SessionSummary};
use azazel_core::{EntryKind, SessionStore, StartSource};
use azazel_protocol::{
    CommandRow, ExportFormat, Exported, Member, MessageRow, RewindWhat, SessionCopy, SessionRow, State, Todo,
    TreeRow, Usage,
};
use azazel_team::{Goals, MemberInfo, MemberState};
use azazel_tools::{TodoStatus, TodoStore};
use serde_json::Value;

use super::{Session, data};
use crate::app::Open;
use crate::export::{Format, default_file_name, render};
use crate::tui::{REWIND_NOTE, is_note};

/// Matching lines reported per session by `list_sessions` with a query.
const MATCHES_PER_SESSION: usize = 5;
/// How much of a message a tree row shows.
const ROW_TEXT_CHARS: usize = 200;

fn member(member: &MemberInfo) -> Member {
    Member {
        id: member.id.clone(),
        def: member.def.clone(),
        parent: member.parent.clone(),
        depth: member.depth,
        state: member.state.label().to_string(),
        elapsed_ms: member.elapsed.as_millis() as u64,
        tokens: member.tokens,
        cost_usd: member.cost_usd,
        activity: match &member.state {
            MemberState::Running { activity } => Some(activity.clone()),
            _ => None,
        },
    }
}

fn usage(usage: &azazel_ai::Usage) -> Usage {
    Usage {
        input: usage.input,
        output: usage.output,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
        reasoning: usage.reasoning,
        cost_usd: usage.cost_usd,
    }
}

fn session_row(summary: SessionSummary, matches: Vec<String>) -> SessionRow {
    SessionRow {
        path: summary.path.display().to_string(),
        id: summary.id,
        name: summary.name,
        created: summary.timestamp,
        updated: summary.updated,
        first_prompt: summary.first_prompt,
        messages: summary.messages as u64,
        prompts: summary.prompts as u64,
        pinned: summary.pinned,
        usage: usage(&summary.usage),
        matches,
    }
}

/// The name an entry's kind has in the session file.
fn kind_name(kind: &EntryKind) -> &'static str {
    match kind {
        EntryKind::Message { .. } => "message",
        EntryKind::ModelChange { .. } => "model_change",
        EntryKind::ModeChange { .. } => "mode_change",
        EntryKind::ContextEdit { .. } => "context_edit",
        EntryKind::Compaction { .. } => "compaction",
        EntryKind::Checkpoint { .. } => "checkpoint",
        EntryKind::Agent { .. } => "agent",
        EntryKind::Goal { .. } => "goal",
        EntryKind::Label { .. } => "label",
        EntryKind::Custom { .. } => "custom",
        EntryKind::SessionInfo { .. } => "session_info",
        EntryKind::ThinkingChange { .. } => "thinking_change",
        EntryKind::AgentChange { .. } => "agent_change",
        EntryKind::Pin { .. } => "pin",
        EntryKind::Archive { .. } => "archive",
        EntryKind::RunStarted { .. } => "run_started",
        EntryKind::RequestStarted { .. } => "request_started",
        EntryKind::ToolStarted { .. } => "tool_started",
        EntryKind::RunEnded { .. } => "run_ended",
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// The first line of `text` that says something, cut to a row's length.
fn row_text(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    line.trim().chars().take(ROW_TEXT_CHARS).collect()
}

fn tree_row(entry: Entry, current: &HashSet<String>) -> TreeRow {
    let (role, text, target_id) = match &entry.kind {
        EntryKind::Message { message } => (
            Some(role_name(message.role).to_string()),
            Some(row_text(&message.text())),
            None,
        ),
        EntryKind::Label { target_id, label } => (None, Some(label.clone()), Some(target_id.clone())),
        EntryKind::SessionInfo { name } => (None, Some(name.clone()), None),
        EntryKind::Checkpoint { turn_entry_id, .. } => (None, None, Some(turn_entry_id.clone())),
        _ => (None, None, None),
    };
    TreeRow {
        current: current.contains(&entry.id),
        kind: kind_name(&entry.kind).to_string(),
        id: entry.id,
        parent_id: entry.parent_id,
        timestamp: entry.timestamp,
        role,
        text,
        target_id,
    }
}

/// The checkpoint a rewind goes back to: the entry itself when `entry_id` names a checkpoint,
/// else the checkpoint taken before the prompt it names. Returns the checkpoint's entry, its
/// commit, and the prompt that followed it.
fn rewind_point(branch: &[Entry], entry_id: &str) -> Result<(Entry, String, String), String> {
    let at = branch
        .iter()
        .position(|entry| entry.id == entry_id)
        .ok_or_else(|| format!("no entry `{entry_id}` on the current branch"))?;
    let is_prompt = |entry: &Entry| match &entry.kind {
        EntryKind::Message { message } => message.role == Role::User && !is_note(&message.text()),
        _ => false,
    };
    let checkpoint_at = match &branch[at].kind {
        EntryKind::Checkpoint { .. } => at,
        _ if is_prompt(&branch[at]) => branch[..at]
            .iter()
            .rposition(|entry| matches!(entry.kind, EntryKind::Checkpoint { .. }) || is_prompt(entry))
            .filter(|before| matches!(branch[*before].kind, EntryKind::Checkpoint { .. }))
            .ok_or("no checkpoint was taken before that prompt")?,
        _ => return Err(format!("`{entry_id}` is neither a checkpoint nor a prompt")),
    };
    let EntryKind::Checkpoint { commit, .. } = &branch[checkpoint_at].kind else {
        return Err(format!("`{entry_id}` is not a checkpoint"));
    };
    // A checkpoint taken by a rewind is not followed by a prompt of its own.
    let prompt = branch[checkpoint_at + 1..]
        .iter()
        .take_while(|entry| !matches!(entry.kind, EntryKind::Checkpoint { .. }))
        .find(|entry| is_prompt(entry))
        .and_then(|entry| match &entry.kind {
            EntryKind::Message { message } => Some(message.text()),
            _ => None,
        })
        .unwrap_or_default();
    Ok((branch[checkpoint_at].clone(), commit.clone(), prompt))
}

impl Session {
    pub(super) fn roster(&self) -> Vec<Member> {
        self.team
            .as_ref()
            .map(|team| team.list().iter().map(member).collect())
            .unwrap_or_default()
    }

    pub(super) fn state(&self) -> State {
        let status = self.active.lead.status();
        let (services, router) = (&self.app.registry.services, &self.app.router);
        State {
            session_id: self.active.session.id(),
            session_file: self.active.session.path().display().to_string(),
            name: self.active.session.name(),
            cwd: self.app.paths.cwd.display().to_string(),
            mode: self.app.permissions.mode().to_string(),
            busy: self.busy(),
            turn: status.turn,
            model: status.model.map(|model| model.to_string()),
            via: status.via.map(|via| via.to_string()),
            virtual_models: self.app.virtual_models(),
            chain: status.chain.iter().map(ToString::to_string).collect(),
            thinking: status.thinking.to_string(),
            tier: self.app.router.tier().map(|tier| tier.as_str().to_string()),
            context_tokens: status.context_tokens,
            context_window: status.context_window,
            usage: usage(&status.usage),
            requests: status.requests,
            agents: self.roster(),
            branch: azazel_core::prompt::git_branch(&self.app.paths.project_root),
            coordinator: self.app.coordinator.is_on(),
            agent: self.app.lead_agent.name(),
            primary_agents: self
                .app
                .lead_agent
                .primaries()
                .iter()
                .map(|def| azazel_protocol::PrimaryAgent {
                    name: def.name.clone(),
                    description: def.description.clone(),
                    color: def.color.clone(),
                })
                .collect(),
            goal: services
                .get::<Goals>()
                .and_then(|goals| goals.current())
                .filter(|goal| goal.status.is_open())
                .map(|goal| format!("goal {}, attempt {}", goal.status.label(), goal.iteration + 1)),
            todo: services
                .get::<TodoStore>()
                .map(|store| store.get(&self.active.lead.id))
                .unwrap_or_default()
                .into_iter()
                .map(|item| Todo {
                    text: item.text,
                    status: match item.status {
                        TodoStatus::Pending => "pending",
                        TodoStatus::InProgress => "in_progress",
                        TodoStatus::Done => "done",
                    }
                    .to_string(),
                })
                .collect(),
            roles: router
                .roles()
                .keys()
                .filter_map(|role| {
                    let first = router.resolve(&[azazel_ai::ModelSpec::Role(role.clone())]);
                    Some((role.clone(), first.first()?.to_string()))
                })
                .collect(),
            // A run that is being gone on with has no end in the file yet either.
            interrupted: (!self.busy())
                .then(|| self.active.session.interrupted_run(None))
                .flatten()
                .map(|run| run.describe()),
        }
    }

    /// The open session as its file holds it.
    pub(super) fn session_copy(&self) -> Result<Value, String> {
        let session = &self.active.session;
        data(SessionCopy {
            path: session.path().display().to_string(),
            header: serde_json::to_value(session.header()).map_err(|e| e.to_string())?,
            entries: session
                .entries()
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?,
            leaf: session.leaf(),
        })
    }

    /// What `run_command` takes: the commands of extensions, the reports, and the skills.
    pub(super) fn commands(&self) -> Vec<CommandRow> {
        let mut rows: Vec<CommandRow> = crate::reports::REPORTS
            .iter()
            .map(|(name, description)| CommandRow {
                name: name.to_string(),
                description: description.to_string(),
            })
            .collect();
        for command in self.app.registry.commands() {
            if rows.iter().any(|row| row.name == command.name) {
                continue;
            }
            rows.push(CommandRow {
                description: match &command.argument_hint {
                    Some(hint) => format!("{} {hint}", command.description),
                    None => command.description,
                },
                name: command.name,
            });
        }
        for skill in self.app.registry.skills.list() {
            if skill.user_invocable {
                rows.push(CommandRow {
                    name: format!("skill:{}", skill.name),
                    description: skill.description,
                });
            }
        }
        rows
    }

    /// Replaces the open session with another one, keeping the model selection. When the other
    /// one cannot be opened the session that was open is opened again.
    async fn swap(&mut self, how: Open, source: StartSource) -> Result<(), String> {
        // What the lead said last goes out before it is left behind.
        while let Ok(event) = self.events.try_recv() {
            self.on_event(event);
        }
        let status = self.active.lead.status();
        self.app.close(&self.active).await;
        self.working = false;
        self.restore_chain = None;
        let (opened, problem) = match self.app.open(how, status.chain.clone(), status.thinking).await {
            Ok(active) => (Ok(active), None),
            Err(e) => {
                let previous = Open::Store(self.active.session.clone());
                (
                    self.app.open(previous, status.chain, status.thinking).await,
                    Some(format!("could not open the session: {e:#}")),
                )
            }
        };
        match opened {
            Ok(active) => {
                self.events = active.lead.subscribe();
                self.active = active;
            }
            Err(e) => {
                return Err(format!(
                    "{}; the session that was open could not be opened again either: {e:#}",
                    problem.unwrap_or_default()
                ));
            }
        }
        match problem {
            Some(problem) => Err(problem),
            None => {
                self.app.start(source).await;
                Ok(())
            }
        }
    }

    pub(super) async fn new_session(&mut self, name: Option<String>) -> Result<Value, String> {
        self.idle_to("start a new session")?;
        self.swap(Open::New, StartSource::Clear).await?;
        if let Some(name) = name.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
            self.active.session.append(EntryKind::SessionInfo {
                name: name.to_string(),
            });
        }
        data(self.state())
    }

    pub(super) async fn resume_session(
        &mut self,
        session_id: Option<String>,
        path: Option<String>,
    ) -> Result<Value, String> {
        let wanted = path
            .or(session_id)
            .ok_or("`session_id` or `path` must name the session to resume")?;
        let file = self.app.find_session(&wanted).map_err(|e| format!("{e:#}"))?;
        self.idle_to("resume another session")?;
        self.swap(Open::File(file), StartSource::Resume).await?;
        data(self.state())
    }

    pub(super) async fn fork_session(&mut self, entry_id: Option<String>) -> Result<Value, String> {
        self.idle_to("fork the session")?;
        let session = &self.active.session;
        if let Some(id) = &entry_id
            && session.entry(id).is_none()
        {
            return Err(format!("no entry `{id}` in this session"));
        }
        let fork = session
            .fork(&self.app.paths.sessions_dir(), entry_id.as_deref())
            .map_err(|e| format!("could not fork the session: {e}"))?;
        self.swap(Open::Store(Arc::new(fork)), StartSource::Resume)
            .await?;
        data(self.state())
    }

    pub(super) fn list_sessions(&self, query: Option<&str>) -> Result<Value, String> {
        let dir = self.app.paths.sessions_dir();
        let rows: Vec<SessionRow> = match query.map(str::trim).filter(|query| !query.is_empty()) {
            Some(query) => SessionStore::search(&dir, query, MATCHES_PER_SESSION)
                .into_iter()
                .map(|(summary, matches)| session_row(summary, matches))
                .collect(),
            None => SessionStore::list(&dir)
                .into_iter()
                .map(|summary| session_row(summary, Vec::new()))
                .collect(),
        };
        data(rows)
    }

    pub(super) fn messages(&self, leaf: Option<&str>) -> Result<Value, String> {
        let session = &self.active.session;
        if let Some(leaf) = leaf
            && session.entry(leaf).is_none()
        {
            return Err(format!("no entry `{leaf}` in this session"));
        }
        let rows: Vec<MessageRow> = session
            .branch(leaf)
            .into_iter()
            .filter_map(|entry| match entry.kind {
                EntryKind::Message { message } => Some(MessageRow {
                    entry_id: entry.id,
                    timestamp: entry.timestamp,
                    message: serde_json::to_value(message).ok()?,
                }),
                _ => None,
            })
            .collect();
        data(rows)
    }

    pub(super) fn tree(&self) -> Vec<TreeRow> {
        let session = &self.active.session;
        let mut current: HashSet<String> = session.branch(None).into_iter().map(|entry| entry.id).collect();
        let entries = session.entries();
        // What a run noted hangs from the branch rather than being on it.
        for entry in &entries {
            if entry.kind.is_journal()
                && entry
                    .parent_id
                    .as_ref()
                    .is_some_and(|parent| current.contains(parent))
            {
                current.insert(entry.id.clone());
            }
        }
        entries
            .into_iter()
            .map(|entry| tree_row(entry, &current))
            .collect()
    }

    /// Moves the session to another entry of its tree, or with `before` to the entry before
    /// it, and restarts the lead from there.
    pub(super) async fn set_leaf(&mut self, entry_id: &str, before: bool) -> Result<Value, String> {
        self.idle_to("move within the session")?;
        let session = self.active.session.clone();
        let entry = session
            .entry(entry_id)
            .ok_or_else(|| format!("no entry `{entry_id}` in this session"))?;
        let left = session.leaf();
        let target = if before { entry.parent_id } else { Some(entry.id) };
        if !session.set_leaf(target.as_deref()) {
            return Err(format!("no entry `{entry_id}` in this session"));
        }
        self.swap(Open::Store(session), StartSource::Resume).await?;
        // What happened on the branch that was left is written up for the agent.
        crate::branch::summarise_left(&self.app, &self.active, left.as_deref());
        data(self.state())
    }

    /// Goes back to before the last prompt and sends it again. The earlier attempt stays in the
    /// file as a branch.
    pub(super) async fn retry(&mut self) -> Result<Value, String> {
        self.idle_to("send the last prompt again")?;
        let session = self.active.session.clone();
        let again = crate::retry::last_prompt(&session, &self.app.paths.cwd)
            .ok_or("there is no prompt to send again")?;
        session.set_leaf(again.parent.as_deref());
        self.swap(Open::Store(session), StartSource::Resume).await?;
        self.submit(again.text, again.images).await?;
        Ok(Value::Null)
    }

    /// Goes back to a checkpoint: the files, the conversation, or both.
    pub(super) async fn rewind(&mut self, entry_id: &str, what: RewindWhat) -> Result<Value, String> {
        self.idle_to("rewind")?;
        let session = self.active.session.clone();
        let (checkpoint, commit, prompt) = rewind_point(&session.branch(None), entry_id)?;
        let (code, conversation) = match what {
            RewindWhat::Code => (true, false),
            RewindWhat::Conversation => (false, true),
            RewindWhat::Both => (true, true),
        };
        let undo = if code {
            let before = self.active.checkpoints.restore(&commit).await;
            Some(before.map_err(|e| format!("could not restore the files: {e}"))?)
        } else {
            None
        };
        if conversation {
            session.set_leaf(checkpoint.parent_id.as_deref());
        }
        // The files as they were just now stay reachable, so the rewind can itself be rewound.
        if let Some(commit) = undo {
            let turn_entry_id = session.leaf().unwrap_or_default();
            session.append(EntryKind::Checkpoint {
                turn_entry_id,
                commit,
            });
        }
        if conversation {
            self.swap(Open::Store(session), StartSource::Resume).await?;
        } else {
            let target = match row_text(&prompt) {
                first if first.is_empty() => "before my previous rewind".to_string(),
                first => format!("before this prompt: {first}"),
            };
            let note = format!(
                "{REWIND_NOTE} to how they were {target}\nFiles you read or changed since then may differ \
                 now; read them again before editing."
            );
            self.active.lead.send(AgentCmd::Append(UserInput::text(note)));
        }
        Ok(Value::Null)
    }

    pub(super) fn export(&self, format: Option<ExportFormat>, path: Option<&str>) -> Result<Value, String> {
        let session = &self.active.session;
        let given = format.map(|format| match format {
            ExportFormat::Markdown => Format::Markdown,
            ExportFormat::Html => Format::Html,
            ExportFormat::Jsonl => Format::Jsonl,
        });
        let cwd = &self.app.paths.cwd;
        let (file, format) = match path {
            Some(path) => {
                let file = cwd.join(path);
                let format = given
                    .or_else(|| Format::from_path(&file))
                    .unwrap_or(Format::Markdown);
                (file, format)
            }
            None => {
                let format = given.unwrap_or(Format::Markdown);
                (cwd.join(default_file_name(session, format)), format)
            }
        };
        std::fs::write(&file, render(session, format, self.app.config.tui.redact))
            .map_err(|e| format!("could not write {}: {e}", file.display()))?;
        data(Exported {
            path: file.display().to_string(),
        })
    }

    /// The models of the providers there is a credential for, as `azazel models` lists them.
    /// Where role changes over RPC land: this session, whose lead follows `default`.
    pub(super) fn roles_place(&self) -> crate::roles::Place<'_> {
        crate::roles::Place::of(&self.app, Some((&self.active.lead, Some(&*self.active.session))))
    }

    pub(super) fn models(&self) -> Result<Value, String> {
        let providers = presets::resolve(&self.app.config.providers)?;
        let listed = crate::models::listed(
            self.app.router.catalog(),
            &providers,
            &self.app.credentials,
            None,
            self.app.provider.as_deref(),
            false,
        );
        Ok(Value::Array(
            listed.into_iter().map(crate::models::model_json).collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use azazel_ai::Message;

    use super::*;

    fn entry(id: &str, kind: EntryKind) -> Entry {
        Entry {
            id: id.to_string(),
            parent_id: None,
            timestamp: String::new(),
            kind,
        }
    }

    fn checkpoint(id: &str, commit: &str) -> Entry {
        entry(
            id,
            EntryKind::Checkpoint {
                turn_entry_id: String::new(),
                commit: commit.to_string(),
            },
        )
    }

    fn prompt(id: &str, text: &str) -> Entry {
        entry(
            id,
            EntryKind::Message {
                message: Message::user(text),
            },
        )
    }

    #[test]
    fn a_rewind_is_named_by_its_checkpoint_or_by_the_prompt_after_it() {
        let branch = [
            prompt("p0", "before checkpoints were on"),
            checkpoint("k1", "aaa"),
            prompt("p1", "first"),
            checkpoint("k2", "bbb"),
            prompt("p2", "second"),
            // Taken by a rewind: no prompt follows it.
            checkpoint("k3", "ccc"),
        ];
        let point =
            |id: &str| rewind_point(&branch, id).map(|(entry, commit, prompt)| (entry.id, commit, prompt));
        assert_eq!(point("k2"), Ok(("k2".into(), "bbb".into(), "second".into())));
        assert_eq!(point("p2"), Ok(("k2".into(), "bbb".into(), "second".into())));
        assert_eq!(point("p1"), Ok(("k1".into(), "aaa".into(), "first".into())));
        assert_eq!(point("k3"), Ok(("k3".into(), "ccc".into(), String::new())));
        assert_eq!(
            point("p0"),
            Err("no checkpoint was taken before that prompt".to_string())
        );
        assert_eq!(
            point("nope"),
            Err("no entry `nope` on the current branch".to_string())
        );
    }

    #[test]
    fn tree_rows_say_what_an_entry_is() {
        let current: HashSet<String> = ["m1".to_string()].into();
        let row = tree_row(prompt("m1", "\n  fix the bug  \nand more"), &current);
        assert_eq!(
            (
                row.kind.as_str(),
                row.role.as_deref(),
                row.text.as_deref(),
                row.current
            ),
            ("message", Some("user"), Some("fix the bug"), true)
        );
        let row = tree_row(checkpoint("k1", "aaa"), &current);
        assert_eq!(
            (row.kind.as_str(), row.role, row.current),
            ("checkpoint", None, false)
        );
    }
}
