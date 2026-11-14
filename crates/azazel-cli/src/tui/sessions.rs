//! Commands about sessions: `/name`, `/session`, the `/resume` and `/open` pickers and their
//! keys, labels and filters in `/tree`, `/retry`, `/handoff`, `/search`, `/consult`, and the
//! prompt history that is kept across sessions.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use azazel_ai::{Message, Role as MessageRole};
use azazel_core::names;
use azazel_core::paths::canonical;
use azazel_core::session::{Entry, SessionSummary};
use azazel_core::side::{Side, SideAnswer, SideQuestion};
use azazel_core::{EntryKind, SessionStore};
use azazel_tui::{Glyph, KeyCombo, SelectItem, text};
use crossterm::event::{KeyCode, KeyModifiers};
use tokio_util::sync::CancellationToken;

use super::backend::Go;
use super::overlay::{Action, Kind, Overlay};
use super::pager::{self, Source, Start};
use super::{Internal, Tui, first_line, view};
use crate::history;
use crate::retry::{self, HANDOFF_NOTE};
use crate::sessions::local_time;

/// Messages shown when `tab` previews a session in a picker.
const PREVIEW_MESSAGES: usize = 6;
/// Entries `ctrl+r` lists.
const HISTORY_ROWS: usize = 2000;
/// Rows of a reply shown in the panel while it arrives.
const PANEL_ROWS: usize = 12;
/// Longest session name taken from `/name`.
const MAX_NAME_CHARS: usize = 80;

const HANDOFF_PROMPT: &str = "Write a hand-off brief for whoever continues this work in a new session that has \
none of this conversation. Use these headings: Goal, State, Decisions, Files touched, Next steps, Open questions. \
Be specific: name files, commands and results, and say what was tried and did not work. Leave out what no longer \
matters. Reply with the brief only.";

const CONSULT_PROMPT: &str = "The question below is asked from another session, about the saved conversation \
above. Answer from that conversation only, and say so when it does not hold the answer.";

/// What a session picker is for.
#[derive(Debug, Clone, PartialEq)]
pub enum PickFor {
    Resume,
    /// `/open`: the sessions of every project.
    Open,
    /// `/consult`, with the question when it was typed with the command.
    Consult(Option<String>),
}

/// Which entries `/tree` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TreeFilter {
    #[default]
    All,
    Labelled,
    Prompts,
}

/// What a request on the side is for.
pub enum SideWhat {
    Consult {
        /// The question as typed.
        question: String,
        /// What the consulted session is called.
        session: String,
    },
    Handoff,
}

/// A request on the side whose reply is arriving in the panel.
pub struct SideRun {
    id: u64,
    cancel: CancellationToken,
    /// The reply so far.
    text: String,
    what: SideWhat,
}

impl SideRun {
    /// Stops the request; what arrives of it afterwards is dropped.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

/// What comes back from work this module started in the background.
pub(super) enum SideEvent {
    Delta {
        run: u64,
        text: String,
    },
    Done {
        run: u64,
        result: Result<SideAnswer, String>,
    },
    /// The name the namer suggests for the open session.
    Named(Result<String, String>),
}

/// What the keys of a session picker do. It is shown above the list: the rule below the list
/// has no room for five keys.
const SESSION_KEYS: &str =
    "ctrl+r rename · ctrl+d delete · ctrl+p pin or unpin · ctrl+n named only · tab preview";
const TREE_KEYS: &str = "alt+l labels an entry · ctrl+l labelled only · ctrl+u your prompts only";

/// The keys of a session picker, in the order [`Tui::session_hotkey`] reads them.
fn session_keys() -> Vec<(KeyCombo, &'static str)> {
    let ctrl = |c| KeyCombo::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    vec![
        (ctrl('r'), ""),
        (ctrl('d'), ""),
        (ctrl('p'), ""),
        (ctrl('n'), ""),
        (KeyCombo::new(KeyCode::Tab, KeyModifiers::NONE), ""),
    ]
}

/// The keys of `/tree`: label, labelled only, your prompts only. The list filters as you type,
/// so they are chords.
fn tree_keys() -> Vec<(KeyCombo, &'static str)> {
    vec![
        (KeyCombo::new(KeyCode::Char('l'), KeyModifiers::ALT), ""),
        (KeyCombo::new(KeyCode::Char('l'), KeyModifiers::CONTROL), ""),
        (KeyCombo::new(KeyCode::Char('u'), KeyModifiers::CONTROL), ""),
    ]
}

/// The sessions a picker lists: pinned ones first, then the most recently used. `open` is the
/// session in use; it is left out while it is still empty, and when `with_open` is off.
/// Archived sessions are never listed.
fn picker_rows(
    mut sessions: Vec<SessionSummary>,
    named_only: bool,
    open: &str,
    with_open: bool,
) -> Vec<SessionSummary> {
    sessions.retain(|session| {
        !session.archived
            && (!named_only || session.name.is_some())
            && (session.id != open || (with_open && session.messages > 0))
    });
    // The list arrives most recently used first; the sort keeps that order within each group.
    sessions.sort_by_key(|session| !session.pinned);
    sessions
}

/// What a session is called in pickers: its name, else its first prompt.
fn session_label(session: &SessionSummary) -> String {
    first_line(
        session
            .name
            .as_deref()
            .or(session.first_prompt.as_deref())
            .unwrap_or("(no prompt yet)"),
    )
}

/// `background` is the id of the background session that has the session open, if one does;
/// `place`, the directory it works in, when the picker lists those of every project.
fn session_item(
    session: &SessionSummary,
    open: bool,
    background: Option<&str>,
    place: Option<String>,
) -> SelectItem {
    let mut detail = Vec::new();
    detail.extend(place);
    if session.pinned {
        detail.push("pinned".to_string());
    }
    if open {
        detail.push("open now".to_string());
    }
    if let Some(id) = background {
        detail.push(format!("in the background: azazel attach {id}"));
    }
    detail.push(local_time(&session.updated));
    detail.push(format!("{} messages", session.messages));
    SelectItem::new(session_label(session), detail.join(" · "))
}

/// One row of `/tree`.
#[derive(Debug, Clone, PartialEq)]
struct TreeRow {
    id: String,
    is_user: bool,
    said: String,
    label: Option<String>,
    on_branch: bool,
}

/// The messages `/tree` lists, newest first: what you and the agent said, without tool results.
fn tree_rows(
    entries: Vec<Entry>,
    branch: &HashSet<String>,
    labels: &HashMap<String, String>,
    filter: TreeFilter,
) -> Vec<TreeRow> {
    entries
        .into_iter()
        .rev()
        .filter_map(|entry| {
            let EntryKind::Message { message } = entry.kind else {
                return None;
            };
            let said = message.text();
            if message.role == MessageRole::Tool || said.trim().is_empty() {
                return None;
            }
            let row = TreeRow {
                is_user: message.role == MessageRole::User,
                label: labels.get(&entry.id).cloned(),
                on_branch: branch.contains(&entry.id),
                id: entry.id,
                said,
            };
            let shown = match filter {
                TreeFilter::All => true,
                TreeFilter::Labelled => row.label.is_some(),
                TreeFilter::Prompts => row.is_user,
            };
            shown.then_some(row)
        })
        .collect()
}

/// A prompt as it was typed: without the `<file>` blocks its `@` mentions were expanded into.
/// Sending it again expands them again, from the files as they are now.
/// A session name as typed: one line, not too long. `None` when nothing is left.
fn clean_name(typed: &str) -> Option<String> {
    let line = typed.lines().next().unwrap_or_default().trim();
    (!line.is_empty()).then(|| text::truncate(line, MAX_NAME_CHARS, "…"))
}

/// A conversation as context for a request on the side. A reply that was cut off while it was
/// calling tools has no results to go with the calls, and a provider rejects that; it is left out.
fn closed_context(session: &SessionStore) -> Vec<Message> {
    let mut messages: Vec<Message> = session
        .build_context(None)
        .into_iter()
        .map(|entry| entry.message)
        .collect();
    while messages
        .last()
        .is_some_and(|last| last.role == MessageRole::Assistant && last.tool_calls().next().is_some())
    {
        messages.pop();
    }
    messages
}

/// The last `rows` rows of `said`, wrapped to `width`.
fn tail_rows(said: &str, width: usize, rows: usize) -> Vec<String> {
    let mut lines = wrapped(said, width);
    let from = lines.len().saturating_sub(rows);
    lines.drain(..from);
    lines
}

fn wrapped(said: &str, width: usize) -> Vec<String> {
    said.lines()
        .flat_map(|line| {
            if line.is_empty() {
                vec![String::new()]
            } else {
                text::wrap(line, width.max(1))
            }
        })
        .collect()
}

impl Tui {
    // ------------------------------------------------------------ /name and /session

    /// `/name <text>` names the session; `/name` alone asks the namer for a name.
    pub(super) async fn name_command(&mut self, args: &str) {
        if let Some(name) = clean_name(args) {
            return self.set_name(name).await;
        }
        let Some(local) = self.backend.local() else {
            return self.fail("give the session a name with /name <text>");
        };
        let Some(side) = local.app.registry.services.get::<Side>() else {
            return self.fail("no model can be asked for a name; give one with /name <text>");
        };
        let (agent, session) = (local.active.lead.info().clone(), local.active.session.clone());
        let done = self.internal_tx.clone();
        self.notice = Some("asking for a name…".to_string());
        tokio::spawn(async move {
            let named = names::suggest(&side, &agent, &session).await;
            let _ = done.send(Internal::Side(SideEvent::Named(named)));
        });
    }

    async fn set_name(&mut self, name: String) {
        match self.backend.set_name(name.clone()).await {
            Ok(()) => self.info(&format!("session named: {name}")),
            Err(why) => self.fail(&why),
        }
        self.sync().await;
    }

    // ------------------------------------------------------------ session pickers

    /// `/resume`: the sessions of this project to pick from.
    pub(super) async fn open_resume(&mut self) {
        if self.idle_to("switch sessions") {
            self.open_sessions(PickFor::Resume, false, None, None).await;
        }
    }

    /// `/consult [question]`: pick a saved session and ask it something.
    pub(super) async fn open_consult(&mut self, question: &str) {
        if !self.here_only("/consult") {
            return;
        }
        let question = (!question.trim().is_empty()).then(|| question.trim().to_string());
        self.open_sessions(PickFor::Consult(question), false, None, None)
            .await;
    }

    /// Opens a session picker with the row of session `select` selected and `note` above the
    /// list. What was typed into the filter of the picker it replaces is kept.
    pub(super) async fn open_sessions(
        &mut self,
        purpose: PickFor,
        named_only: bool,
        select: Option<&str>,
        note: Option<String>,
    ) {
        let filter = match self.overlay.as_ref() {
            Some(Overlay {
                kind: Kind::Select { filter, .. },
                action: Action::Sessions { .. },
                ..
            }) => filter.clone(),
            _ => String::new(),
        };
        let open = self.session.id();
        let everywhere = purpose == PickFor::Open;
        let listed = match everywhere {
            true => super::places::every_session(&self.env.paths),
            false => match self.backend.sessions(None).await {
                Ok(listed) => listed.into_iter().map(|(session, _)| session).collect(),
                Err(why) => return self.fail(&why),
            },
        };
        let rows = picker_rows(listed, named_only, &open, !matches!(purpose, PickFor::Consult(_)));
        if rows.is_empty() {
            self.overlay = None;
            if let Some(note) = note {
                self.info(&note);
            }
            return self.info(match (named_only, everywhere) {
                (true, false) => "no session of this project has a name",
                (true, true) => "no session has a name",
                (false, false) => "this project has no other sessions",
                (false, true) => "there are no other sessions",
            });
        }
        let home = &self.env.paths.home;
        let items = rows
            .iter()
            .map(|session| {
                // The open session says so itself, wherever it runs.
                let background = Some(&session.id)
                    .filter(|id| **id != open)
                    .and_then(|id| crate::running::host_of(home, id));
                let place = everywhere.then(|| super::places::shown_dir(&session.cwd));
                session_item(session, session.id == open, background.as_deref(), place)
            })
            .collect();
        let mut body = vec![SESSION_KEYS.to_string()];
        if named_only {
            body.push("Named sessions only; ctrl+n shows all of them.".to_string());
        }
        body.extend(note);
        let title = match purpose {
            PickFor::Resume => "Resume a session",
            PickFor::Open => "Open a session of any project",
            PickFor::Consult(_) => "Ask a saved session",
        };
        let selected = select.and_then(|id| rows.iter().position(|session| session.id == id));
        let action = Action::Sessions {
            rows,
            purpose,
            named_only,
        };
        let mut overlay = Overlay::select(title, body, items, true, action).with_keys(session_keys());
        if let Kind::Select {
            list, filter: typed, ..
        } = &mut overlay.kind
        {
            list.set_filter(&filter);
            *typed = filter;
            // The cursor counts rows as shown, which a filter reorders: step to the wanted one.
            if let Some(wanted) = selected {
                for row in 0..list.len() {
                    list.set_cursor(row);
                    if list.selected() == Some(wanted) {
                        break;
                    }
                }
                if list.selected() != Some(wanted) {
                    list.set_cursor(0);
                }
            }
        }
        self.overlay = Some(overlay);
    }

    /// Appends to a session of a picker: the open one through its store, another through its file.
    fn write_to(&self, session: &SessionSummary, kind: EntryKind) -> std::io::Result<()> {
        if session.id == self.session.id() {
            self.session.append(kind);
        } else {
            SessionStore::open(&session.path)?.append(kind);
        }
        Ok(())
    }

    /// The keys of the session pickers and of `/tree`. False when the open overlay is neither.
    pub(super) async fn session_hotkey(&mut self, index: usize, key: usize) -> bool {
        // What the key acts on is copied out first: acting on it replaces the overlay.
        enum Target {
            Session(Box<SessionSummary>, PickFor, bool),
            Tree(TreeRowRef, TreeFilter),
        }
        type TreeRowRef = (String, String);
        let target = match self.overlay.as_ref().map(|overlay| &overlay.action) {
            Some(Action::Sessions {
                rows,
                purpose,
                named_only,
            }) => rows
                .get(index)
                .map(|row| Target::Session(Box::new(row.clone()), purpose.clone(), *named_only)),
            Some(Action::Tree(rows, filter)) => rows
                .get(index)
                .map(|(id, _, said)| Target::Tree((id.clone(), said.clone()), *filter)),
            _ => return false,
        };
        match target {
            Some(Target::Session(row, purpose, named_only)) => {
                self.picker_key(key, *row, purpose, named_only).await
            }
            Some(Target::Tree((id, said), filter)) => match key {
                0 => {
                    let current = self.session.labels().remove(&id);
                    let body = vec![
                        format!("A label for: {}", first_line(&said)),
                        match current {
                            Some(label) => {
                                format!("It is labelled `{label}`. An empty label takes that away.")
                            }
                            None => "Labels show beside entries; ctrl+l lists the labelled ones.".to_string(),
                        },
                    ];
                    self.overlay = Some(Overlay::input(
                        "Label",
                        body,
                        Action::TreeLabel { entry: id, filter },
                    ));
                }
                1 => self.open_tree_with(toggled(filter, TreeFilter::Labelled), Some(&id)),
                _ => self.open_tree_with(toggled(filter, TreeFilter::Prompts), Some(&id)),
            },
            None => {}
        }
        true
    }

    async fn picker_key(&mut self, key: usize, row: SessionSummary, purpose: PickFor, named_only: bool) {
        let label = session_label(&row);
        let open = row.id == self.session.id();
        // Renaming, deleting, pinning and previewing work on the session files themselves.
        if key != 3 && self.backend.local().is_none() {
            let note = "Only a session that runs in this process can rename, delete, pin or preview others."
                .to_string();
            return self
                .open_sessions(purpose, named_only, Some(&row.id), Some(note))
                .await;
        }
        match key {
            0 => {
                let body = vec![format!("A new name for: {label}")];
                self.overlay = Some(Overlay::input(
                    "Rename the session",
                    body,
                    Action::SessionRename(Box::new(row), purpose, named_only),
                ));
            }
            1 if open => {
                let note = "The open session cannot be deleted; switch to another one first.".to_string();
                self.open_sessions(purpose, named_only, Some(&row.id), Some(note))
                    .await;
            }
            1 => {
                let items = vec![
                    SelectItem::new("Delete it", "the session and its agents' transcripts"),
                    SelectItem::new("Keep it", ""),
                ];
                let body = vec![
                    label,
                    format!(
                        "{} messages · last used {}",
                        row.messages,
                        local_time(&row.updated)
                    ),
                    "A deleted session cannot be brought back.".to_string(),
                ];
                // A stray Enter must not delete anything: start on "Keep it".
                self.overlay = Some(
                    Overlay::select(
                        "Delete this session?",
                        body,
                        items,
                        false,
                        Action::SessionDelete(Box::new(row), purpose, named_only),
                    )
                    .starting_at(1),
                );
            }
            2 => {
                let pinned = !row.pinned;
                let note = match self.write_to(&row, EntryKind::Pin { pinned }) {
                    Ok(()) if pinned => format!("Pinned: {label}"),
                    Ok(()) => format!("Unpinned: {label}"),
                    Err(e) => format!("Could not write to {}: {e}", row.path.display()),
                };
                self.open_sessions(purpose, named_only, Some(&row.id), Some(note))
                    .await;
            }
            3 => {
                self.open_sessions(purpose, !named_only, Some(&row.id), None)
                    .await
            }
            _ => {
                let width = self.width();
                let mut body = vec![
                    SESSION_KEYS.to_string(),
                    format!("{label} · its last {PREVIEW_MESSAGES} messages:"),
                ];
                let messages = SessionStore::preview(&row.path, PREVIEW_MESSAGES);
                if messages.is_empty() {
                    body.push("(no messages yet)".to_string());
                }
                for (role, said) in messages {
                    let who = if role == MessageRole::User { "you" } else { "agent" };
                    let line = format!("{who}: {}", said.lines().next().unwrap_or_default());
                    body.push(text::truncate(
                        &line,
                        width.saturating_sub(1),
                        self.theme.glyph(Glyph::Ellipsis),
                    ));
                }
                if let Some(overlay) = &mut self.overlay {
                    overlay.body = body;
                }
            }
        }
    }

    /// What the overlays of this module do when they are answered.
    pub(super) async fn finish_session_overlay(&mut self, action: Action, choice: usize, typed: String) {
        match action {
            Action::Sessions { rows, purpose, .. } => {
                let Some(row) = rows.into_iter().nth(choice) else {
                    return;
                };
                match purpose {
                    PickFor::Resume => self.resume_file(row.path).await,
                    PickFor::Open => self.open_anywhere(row).await,
                    PickFor::Consult(Some(question)) => self.consult(&row, &question),
                    PickFor::Consult(None) => {
                        let body = vec![format!("What do you want to know from: {}", session_label(&row))];
                        self.overlay = Some(Overlay::input(
                            "Ask a saved session",
                            body,
                            Action::ConsultQuestion(Box::new(row)),
                        ));
                    }
                }
            }
            Action::SessionRename(row, purpose, named_only) => {
                let note = match clean_name(&typed) {
                    Some(name) => match self.write_to(&row, EntryKind::SessionInfo { name: name.clone() }) {
                        Ok(()) => Some(format!("Renamed to: {name}")),
                        Err(e) => Some(format!("Could not write to {}: {e}", row.path.display())),
                    },
                    None => None,
                };
                self.open_sessions(purpose, named_only, Some(&row.id), note).await;
            }
            Action::SessionDelete(row, purpose, named_only) => {
                let note = if choice != 0 {
                    None
                } else {
                    Some(match SessionStore::delete(&row) {
                        Ok(()) => format!("Deleted: {}", session_label(&row)),
                        Err(e) => format!("Could not delete {}: {e}", row.path.display()),
                    })
                };
                self.open_sessions(purpose, named_only, Some(&row.id), note).await;
            }
            Action::TreeLabel { entry, filter } => {
                if let Err(why) = self.backend.set_label(&entry, typed.trim()).await {
                    self.fail(&why);
                }
                self.sync().await;
                self.open_tree_with(filter, Some(&entry));
            }
            Action::ConsultQuestion(row) => self.consult(&row, &typed),
            // The one row of the panel while a reply arrives is "Stop".
            Action::Side(run) => run.cancel(),
            Action::SideAnswer {
                title,
                question,
                answer,
            } => match choice {
                1 => self.copy_text(&answer, "the answer"),
                2 => {
                    let sources = vec![Source::Prompt(question), Source::Markdown(answer)];
                    pager::show(self, &title, sources, Start::Top).await;
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Switches to the session in `path`, on the model and thinking level it last used.
    pub(super) async fn resume_file(&mut self, path: PathBuf) {
        // A session in another process names its files as its own machine does.
        let same = match self.backend.local() {
            Some(_) => canonical(&path) == canonical(&self.session.path()),
            None => path == self.session.path(),
        };
        if same {
            return self.info("that session is the one that is open");
        }
        if !self.idle_to("switch sessions") {
            return;
        }
        let before = self.backend.status();
        if !self.go(Go::File(path)).await {
            return;
        }
        // The session came back on what it last used; say so when that is a change.
        let now = self.backend.status();
        if (&now.chain, now.thinking) != (&before.chain, before.thinking) {
            let chain: Vec<String> = now.chain.iter().map(ToString::to_string).collect();
            self.info(&format!(
                "model: {} · thinking: {}, as this session last used them",
                chain.join(", "),
                now.thinking
            ));
        }
    }

    /// `/search <query>`: the sessions of this project whose messages hold `query`; choosing a
    /// result resumes its session.
    pub(super) async fn search_command(&mut self, query: &str) {
        if query.trim().is_empty() {
            return self.fail("usage: /search <text>");
        }
        let found = match self.backend.sessions(Some(query.trim())).await {
            Ok(found) => found,
            Err(why) => return self.fail(&why),
        };
        if found.is_empty() {
            return self.info(&format!("no session of this project mentions `{}`", query.trim()));
        }
        let mut items = Vec::new();
        let mut paths = Vec::new();
        for (session, lines) in &found {
            let detail = format!("{} · {}", local_time(&session.updated), session_label(session));
            for line in lines {
                items.push(SelectItem::new(first_line(line), detail.as_str()));
                paths.push(session.path.clone());
            }
        }
        let body = vec![format!(
            "`{}` in {} session{}; choosing a line resumes its session.",
            query.trim(),
            found.len(),
            if found.len() == 1 { "" } else { "s" }
        )];
        self.overlay = Some(Overlay::select(
            "Search sessions",
            body,
            items,
            true,
            Action::Resume(paths),
        ));
    }

    // ------------------------------------------------------------ /tree

    pub(super) fn open_tree(&mut self) {
        if self.idle_to("move within the session") {
            self.open_tree_with(TreeFilter::All, None);
        }
    }

    /// Opens `/tree` listing what `filter` lets through, with entry `select` selected.
    fn open_tree_with(&mut self, filter: TreeFilter, select: Option<&str>) {
        let session = &self.session;
        let branch: HashSet<String> = session.branch(None).into_iter().map(|entry| entry.id).collect();
        let rows = tree_rows(session.entries(), &branch, &session.labels(), filter);
        if rows.is_empty() {
            self.overlay = None;
            return self.info(match filter {
                TreeFilter::All => "this session has no messages yet",
                TreeFilter::Labelled => "no entry has a label yet; alt+l in /tree gives one",
                TreeFilter::Prompts => "you have not sent a prompt in this session yet",
            });
        }
        let on_branch = self.theme.glyph(Glyph::Bullet);
        let flag = self.theme.glyphs().prefix(Glyph::Label);
        let items = rows
            .iter()
            .map(|row| {
                let marker = if row.on_branch { on_branch } else { " " };
                let who = if row.is_user { "you" } else { "agent" };
                let label = row
                    .label
                    .as_ref()
                    .map(|label| format!("{flag}{label}"))
                    .unwrap_or_default();
                SelectItem::new(format!("{marker} {who}: {}", first_line(&row.said)), label)
            })
            .collect();
        let mut body = vec![
            format!(
                "{on_branch} marks the current branch. Choosing one of your prompts goes back to before it and \
                 puts it in the editor; choosing a reply continues from it."
            ),
            TREE_KEYS.to_string(),
        ];
        match filter {
            TreeFilter::All => {}
            TreeFilter::Labelled => body.push("Labelled entries only; ctrl+l shows all of them.".to_string()),
            TreeFilter::Prompts => body.push("Your prompts only; ctrl+u shows everything.".to_string()),
        }
        let start = select
            .and_then(|id| rows.iter().position(|row| row.id == id))
            .unwrap_or(0);
        let rows = rows
            .into_iter()
            .map(|row| (row.id, row.is_user, row.said))
            .collect();
        self.overlay = Some(
            Overlay::select("Session tree", body, items, true, Action::Tree(rows, filter))
                .with_keys(tree_keys())
                .starting_at(start),
        );
    }

    // ------------------------------------------------------------ /retry

    /// `/retry`: goes back to before your last prompt and sends it again. The first attempt
    /// stays in the session as another branch.
    pub(super) async fn retry(&mut self) {
        if !self.idle_to("retry") {
            return;
        }
        let Some(again) = retry::last_prompt(&self.session, &self.backend.cwd()) else {
            return self.info("there is no prompt to send again yet");
        };
        let prompt = again.text.clone();
        match self.backend.retry(again).await {
            Ok(moved) => {
                self.moved(moved, false).await;
                let lines = view::user_prompt(&prompt, self.width(), &self.theme);
                self.say(lines);
            }
            Err(why) => {
                self.sync().await;
                self.fail(&why);
            }
        }
    }

    // ------------------------------------------------------------ requests on the side

    /// Sends a request outside the conversation and opens the panel its reply arrives in.
    fn start_side(
        &mut self,
        what: SideWhat,
        title: String,
        context: Vec<Message>,
        question: String,
        purpose: &'static str,
    ) {
        let Some(local) = self.backend.local() else {
            return self.fail("this works only in a session that runs in this process");
        };
        let Some(side) = local.app.registry.services.get::<Side>() else {
            return self.fail("no model can be asked in this session");
        };
        static RUNS: AtomicU64 = AtomicU64::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        let agent = local.active.lead.info().clone();
        let chain = local.active.lead.status().chain;
        let (events, stop) = (self.internal_tx.clone(), cancel.clone());
        tokio::spawn(async move {
            let deltas = events.clone();
            let mut on_text = move |delta: &str| {
                let _ = deltas.send(Internal::Side(SideEvent::Delta {
                    run,
                    text: delta.to_string(),
                }));
            };
            let asked = SideQuestion {
                agent: &agent,
                context,
                question,
                chain,
                purpose,
                // The lead's prompt and tools go along, so the context reads as it did there.
                framed: true,
            };
            let result = side.ask(asked, &mut on_text, stop).await;
            let _ = events.send(Internal::Side(SideEvent::Done { run, result }));
        });
        let action = Action::Side(SideRun {
            id: run,
            cancel,
            text: String::new(),
            what,
        });
        self.overlay = Some(Overlay::select(
            title,
            vec!["Waiting for the model…".to_string()],
            vec![SelectItem::new("Stop", "")],
            false,
            action,
        ));
    }

    pub(super) async fn on_side(&mut self, event: SideEvent) {
        match event {
            SideEvent::Named(named) => {
                self.notice = None;
                match named {
                    Ok(name) => self.set_name(name).await,
                    Err(why) => self.fail(&format!(
                        "no name was suggested: {why}; give one with /name <text>"
                    )),
                }
            }
            SideEvent::Delta { run, text } => {
                let (width, height) = self.renderer.size();
                let rows = PANEL_ROWS.min(usize::from(height).saturating_sub(8)).max(1);
                if let Some(Overlay {
                    action: Action::Side(side),
                    body,
                    ..
                }) = &mut self.overlay
                    && side.id == run
                {
                    side.text.push_str(&text);
                    *body = tail_rows(&side.text, usize::from(width), rows);
                }
            }
            SideEvent::Done { run, result } => {
                // A panel that was closed dropped its request: nothing is done with the reply.
                let ours = matches!(
                    &self.overlay,
                    Some(Overlay { action: Action::Side(side), .. }) if side.id == run
                );
                let Some(Overlay {
                    action: Action::Side(side),
                    ..
                }) = self.overlay.take_if(|_| ours)
                else {
                    return;
                };
                let answer = match result {
                    Ok(answer) => answer,
                    Err(why) => return self.fail(&format!("the request failed: {why}")),
                };
                if answer.text.trim().is_empty() {
                    let tools = answer.dropped_tools.join(", ");
                    return self.fail(&if tools.is_empty() {
                        "the model gave no answer".to_string()
                    } else {
                        format!("the model tried to use {tools} instead of answering; ask again")
                    });
                }
                match side.what {
                    SideWhat::Handoff => self.finish_handoff(answer.text.trim()).await,
                    SideWhat::Consult { question, session } => {
                        let items = ["Close", "Copy the answer", "Read it full screen"]
                            .iter()
                            .map(|label| SelectItem::new(*label, ""))
                            .collect();
                        self.overlay = Some(
                            Overlay::select(
                                format!("Answer from {session}"),
                                vec![answer.text.trim().to_string()],
                                items,
                                false,
                                Action::SideAnswer {
                                    title: format!("Answer from {session}"),
                                    question,
                                    answer: answer.text.trim().to_string(),
                                },
                            )
                            .markdown(),
                        );
                    }
                }
            }
        }
    }

    /// Asks the saved session `row` a question. The answer is shown in a panel and stored in
    /// neither session.
    fn consult(&mut self, row: &SessionSummary, question: &str) {
        let question = question.trim();
        if question.is_empty() {
            return self.info("no question was asked");
        }
        let saved = match SessionStore::open(&row.path) {
            Ok(saved) => saved,
            Err(e) => return self.fail(&format!("could not read {}: {e}", row.path.display())),
        };
        let context = closed_context(&saved);
        if context.is_empty() {
            return self.info("that session has no messages to ask about");
        }
        let name = session_label(row);
        self.start_side(
            SideWhat::Consult {
                question: question.to_string(),
                session: name.clone(),
            },
            format!("Asking {name}"),
            context,
            format!("{CONSULT_PROMPT}\n\n{question}"),
            "consult",
        );
    }

    /// `/handoff [focus]`: has the model write a brief of this session, then opens a new session
    /// that starts from the brief.
    pub(super) fn handoff(&mut self, focus: &str) {
        if !self.here_only("/handoff") || !self.idle_to("hand the work off") {
            return;
        }
        let context = closed_context(&self.session);
        if context.is_empty() {
            return self.info("there is nothing to hand off yet");
        }
        let mut question = HANDOFF_PROMPT.to_string();
        if !focus.trim().is_empty() {
            question.push_str(&format!("\n\nPut the weight on: {}", focus.trim()));
        }
        self.start_side(
            SideWhat::Handoff,
            "Writing the hand-off brief".to_string(),
            context,
            question,
            "handoff",
        );
    }

    async fn finish_handoff(&mut self, brief: &str) {
        let Some(local) = self.backend.local() else {
            return;
        };
        let old = local.active.session.clone();
        let made = SessionStore::create(
            &local.app.paths.sessions_dir(),
            &local.app.paths.cwd,
            Some(&old.path()),
        );
        let new = match made {
            Ok(new) => new,
            Err(e) => return self.fail(&format!("could not start the new session: {e}")),
        };
        let (old_id, new_id) = (old.id(), new.id());
        // The brief is the new session's first message. Nothing answers it until you write.
        new.append(EntryKind::Message {
            message: Message::user(format!("{HANDOFF_NOTE}{old_id}:\n\n{brief}")),
        });
        if let Some(name) = old.name() {
            new.append(EntryKind::SessionInfo {
                name: text::truncate(&format!("{name} (continued)"), MAX_NAME_CHARS, "…"),
            });
        }
        // Read back from its file, like any session that was not open here.
        let file = new.path();
        drop(new);
        if !self.go(Go::File(file)).await {
            return;
        }
        self.info(&format!(
            "handed off: session {old_id} continues as {new_id}. The brief is its first message; \
             send a message to start from it."
        ));
    }

    // ------------------------------------------------------------ prompt history

    /// Adds what was just sent to the project's history file.
    pub(super) fn remember(&mut self, sent: &str) {
        // The history is this project's; a session elsewhere is not part of it.
        if !self
            .backend
            .local()
            .is_some_and(|local| local.app.keeps_sessions())
        {
            return;
        }
        let file = history::file(&self.env.paths);
        if let Err(e) = history::append(&file, &self.session.id(), sent) {
            tracing::debug!("could not write {}: {e}", file.display());
        }
    }

    /// Puts the project's newest prompts behind the editor's up key.
    pub(super) fn seed_history(&mut self) {
        if self.backend.local().is_none() {
            return;
        }
        let records = history::load(&history::file(&self.env.paths));
        let from = records.len().saturating_sub(history::SEED);
        for record in &records[from..] {
            self.editor.push_history(record.text.clone());
        }
    }

    /// `ctrl+r`: what you sent in this project, newest first, with the session it was sent in.
    pub(super) fn open_history(&mut self) {
        let open = self.session.id();
        let (names, records) = match self.backend.local() {
            Some(local) => (
                SessionStore::list(&local.app.paths.sessions_dir())
                    .into_iter()
                    .filter_map(|session| Some((session.id, session.name?)))
                    .collect::<HashMap<String, String>>(),
                history::newest_first(history::load(&history::file(&self.env.paths)), HISTORY_ROWS),
            ),
            None => (HashMap::new(), Vec::new()),
        };
        let mut rows: Vec<(String, String)> = records
            .into_iter()
            .map(|record| {
                let detail = if record.session == open {
                    "this session".to_string()
                } else {
                    names.get(&record.session).cloned().unwrap_or_default()
                };
                (record.text, detail)
            })
            .collect();
        // What the file does not hold: prompts of a run that keeps nothing, and those of a
        // session from before the history was kept.
        let mut seen: HashSet<String> = rows.iter().map(|(sent, _)| sent.clone()).collect();
        let earlier = self.prompts.iter().rev().cloned().chain(
            self.session
                .branch(None)
                .into_iter()
                .rev()
                .filter_map(|entry| match entry.kind {
                    EntryKind::Message { message } if message.role == MessageRole::User => {
                        Some(retry::typed(&message.text()).to_string())
                    }
                    _ => None,
                })
                .filter(|sent| retry::is_prompt(sent)),
        );
        for sent in earlier {
            if !sent.trim().is_empty() && seen.insert(sent.clone()) {
                rows.push((sent, "this session".to_string()));
            }
        }
        if rows.is_empty() {
            self.notice = Some("no earlier prompts".to_string());
            return;
        }
        let items = rows
            .iter()
            .map(|(sent, detail)| SelectItem::new(first_line(sent), detail.as_str()))
            .collect();
        let prompts = rows.into_iter().map(|(sent, _)| sent).collect();
        self.overlay = Some(Overlay::select(
            "Prompt history",
            Vec::new(),
            items,
            true,
            Action::History(prompts),
        ));
    }
}

/// `filter` switched on, or off again when it is the one in effect.
fn toggled(current: TreeFilter, filter: TreeFilter) -> TreeFilter {
    if current == filter {
        TreeFilter::All
    } else {
        filter
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use azazel_ai::Block;
    use std::collections::BTreeMap;

    use azazel_ai::Usage;

    use super::*;

    fn summary(id: &str, name: Option<&str>, pinned: bool, messages: usize) -> SessionSummary {
        SessionSummary {
            path: PathBuf::from(format!("{id}.jsonl")),
            id: id.to_string(),
            timestamp: "2026-11-12T10:00:00Z".to_string(),
            updated: "2026-11-12T11:00:00Z".to_string(),
            name: name.map(str::to_string),
            first_prompt: Some(format!("prompt of {id}")),
            messages,
            pinned,
            usage: Usage::default(),
            prompts: 1,
            models: BTreeMap::new(),
            tools: BTreeMap::new(),
            cwd: "/work/app".to_string(),
            archived: false,
        }
    }

    fn ids(rows: &[SessionSummary]) -> Vec<&str> {
        rows.iter().map(|row| row.id.as_str()).collect()
    }

    #[test]
    fn pickers_list_pinned_sessions_first_and_keep_the_order_of_use() {
        let listed = || {
            vec![
                summary("newest", None, false, 4),
                summary("open", Some("the open one"), false, 2),
                summary("pinned", Some("kept"), true, 9),
                summary("oldest", None, false, 1),
            ]
        };
        assert_eq!(
            ids(&picker_rows(listed(), false, "open", true)),
            ["pinned", "newest", "open", "oldest"]
        );
        // `/consult` asks other sessions; named only leaves out the rest.
        assert_eq!(
            ids(&picker_rows(listed(), false, "open", false)),
            ["pinned", "newest", "oldest"]
        );
        assert_eq!(
            ids(&picker_rows(listed(), true, "open", true)),
            ["pinned", "open"]
        );
        // The session just started has nothing to resume, rename or pin yet.
        let mut fresh = listed();
        fresh[1].messages = 0;
        assert_eq!(
            ids(&picker_rows(fresh, false, "open", true)),
            ["pinned", "newest", "oldest"]
        );
        // An archived session is in no picker.
        let mut archived = listed();
        archived[0].archived = true;
        assert_eq!(
            ids(&picker_rows(archived, false, "open", true)),
            ["pinned", "open", "oldest"]
        );

        let item = session_item(&summary("pinned", Some("kept"), true, 9), true, None, None);
        assert_eq!(item.label, "kept");
        assert!(item.detail.starts_with("pinned · open now · "), "{}", item.detail);
        assert!(item.detail.ends_with("9 messages"), "{}", item.detail);
        // `/open` says where each session works.
        let item = session_item(&summary("y", None, false, 3), false, None, Some("~/app".into()));
        assert!(item.detail.starts_with("~/app · "), "{}", item.detail);
        let item = session_item(&summary("x", None, false, 1), false, Some("ab12"), None);
        assert_eq!(item.label, "prompt of x");
        assert!(
            item.detail
                .starts_with("in the background: azazel attach ab12 · "),
            "{}",
            item.detail
        );
    }

    #[test]
    fn picker_keys_are_named_above_the_list_and_stay_out_of_the_hint_line() {
        let rows = vec![SelectItem::new("a session", "")];
        let overlay = Overlay::select(
            "Resume a session",
            vec![SESSION_KEYS.to_string()],
            rows,
            true,
            Action::Mode,
        )
        .with_keys(session_keys());
        let (lines, _) = overlay.render(100, 20, &azazel_tui::Theme::dark());
        let shown: Vec<String> = lines.iter().map(|line| text::strip_ansi(line)).collect();
        assert!(
            shown[1].starts_with("│ ctrl+r rename · ctrl+d delete"),
            "{shown:?}"
        );
        let hint = shown.last().unwrap();
        assert!(
            hint.starts_with("╰─ type to filter · ↑↓ move · enter pick · esc cancel ─"),
            "{hint}"
        );
        // Both lists filter as you type: none of their keys may be a plain letter.
        for (combo, _) in session_keys().iter().chain(&tree_keys()) {
            let plain_letter = matches!(combo.code, KeyCode::Char(_))
                && !combo.mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            assert!(!plain_letter, "{combo}");
        }
    }

    #[test]
    fn the_tree_lists_messages_newest_first_with_labels_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), Path::new("/w"), None).unwrap();
        let say = |role: MessageRole, said: &str| {
            let mut message = Message::user(said);
            message.role = role;
            store.append(EntryKind::Message { message })
        };
        let first = say(MessageRole::User, "first prompt");
        let reply = say(MessageRole::Assistant, "first reply");
        say(MessageRole::Tool, "tool output");
        // Another attempt from the same prompt.
        store.set_leaf(Some(&first));
        let second = say(MessageRole::Assistant, "second reply");
        store.append(EntryKind::Label {
            target_id: reply.clone(),
            label: "dead end".to_string(),
        });
        let branch: HashSet<String> = store.branch(None).into_iter().map(|e| e.id).collect();
        let rows = |filter| tree_rows(store.entries(), &branch, &store.labels(), filter);

        let all = rows(TreeFilter::All);
        let shown: Vec<(&str, bool, bool)> = all
            .iter()
            .map(|row| (row.said.as_str(), row.is_user, row.on_branch))
            .collect();
        assert_eq!(
            shown,
            [
                ("second reply", false, true),
                ("first reply", false, false),
                ("first prompt", true, true)
            ]
        );
        assert_eq!(all[1].label.as_deref(), Some("dead end"));
        assert_eq!(all[0].id, second);
        let labelled = rows(TreeFilter::Labelled);
        assert_eq!(labelled.len(), 1);
        assert_eq!(labelled[0].id, reply);
        let prompts = rows(TreeFilter::Prompts);
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].id, first);

        assert_eq!(
            toggled(TreeFilter::All, TreeFilter::Labelled),
            TreeFilter::Labelled
        );
        assert_eq!(
            toggled(TreeFilter::Labelled, TreeFilter::Labelled),
            TreeFilter::All
        );
        assert_eq!(
            toggled(TreeFilter::Labelled, TreeFilter::Prompts),
            TreeFilter::Prompts
        );
    }

    #[test]
    fn a_prompt_is_sent_again_as_typed_and_names_are_one_short_line() {
        let sent = "review @src/lib.rs please\n\n<file path=\"src/lib.rs\">\nfn main() {}\n</file>";
        assert_eq!(retry::typed(sent), "review @src/lib.rs please");
        assert_eq!(
            retry::typed("no mentions\n\nsecond paragraph"),
            "no mentions\n\nsecond paragraph"
        );

        assert_eq!(
            clean_name("  parser rework \nmore").as_deref(),
            Some("parser rework")
        );
        assert_eq!(clean_name("   "), None);
        assert_eq!(
            clean_name(&"x".repeat(200)).unwrap().chars().count(),
            MAX_NAME_CHARS
        );
    }

    #[test]
    fn side_context_drops_a_reply_cut_off_in_its_tool_calls() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), Path::new("/w"), None).unwrap();
        store.append(EntryKind::Message {
            message: Message::user("list the files"),
        });
        let mut reply = Message::user("looking");
        reply.role = MessageRole::Assistant;
        store.append(EntryKind::Message {
            message: reply.clone(),
        });
        reply.content.push(Block::ToolCall {
            id: "c1".to_string(),
            name: "glob".to_string(),
            input: serde_json::json!({ "pattern": "*" }),
            signature: None,
        });
        store.append(EntryKind::Message { message: reply });
        let context = closed_context(&store);
        assert_eq!(context.len(), 2);
        assert_eq!(context[1].text(), "looking");
    }

    #[test]
    fn a_streamed_reply_shows_its_last_rows() {
        let said = "one\n\ntwo two two two\nthree";
        assert_eq!(wrapped(said, 80), ["one", "", "two two two two", "three"]);
        assert_eq!(tail_rows(said, 80, 2), ["two two two two", "three"]);
        // Wrapped rows count, not lines of text.
        let narrow = tail_rows(said, 8, 3);
        assert_eq!(narrow.len(), 3);
        assert_eq!(narrow[2], "three");
        assert!(tail_rows("", 80, 5).is_empty());
    }
}
