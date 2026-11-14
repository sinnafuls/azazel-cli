//! The state of the `/models` screen and what keys and the mouse do to it. It holds no
//! terminal and no session: the UI hands it keys and what the session answered, and carries
//! out the [`Request`]s it makes.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use azazel_ai::ThinkingLevel;
use azazel_protocol::{ModelRow, ProviderRow, RoleRow, RoleSource, Roles, SaveTo};
use azazel_tui::mouse::{Mouse, MouseKind};
use azazel_tui::{Editor, Glyph, Glyphs, Hint, Line, Role, Scroll, Theme, text};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::Request;
use super::list::{self, Hit, Ranker};
use super::strip::{self, Purpose, Strip};
use crate::settings::View;
use crate::tui::input::{FavouriteChange, favourites_changed};

/// Below this many columns of text the sidebar is left out and `tab` steps through the scopes.
pub(super) const NARROW: usize = 72;
/// How long the selection rests on a provider before its models are asked for.
const DEBOUNCE: Duration = Duration::from_millis(120);

/// What the screen shows, as the session told it.
#[derive(Debug, Clone)]
pub(crate) struct Data {
    /// The models of every provider.
    pub rows: Vec<ModelRow>,
    pub providers: Vec<ProviderRow>,
    pub roles: Roles,
    /// The model the session is on, as `provider/id`.
    pub current: Option<String>,
    /// The models chosen lately, latest first.
    pub recent: Vec<String>,
    /// What ctrl+p steps through: `tui.favourites`.
    pub cycle: Vec<String>,
}

pub(crate) use crate::settings::ModelsAt as Opening;

/// A part of the sidebar: what the body shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Scope {
    Roles,
    Recent,
    All,
    Provider(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Focus {
    Side,
    Body,
}

/// How far the list of a provider has come.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Fetch {
    Running,
    Done,
    Failed(String),
}

/// What the model picked in assign mode becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum How {
    /// The role's first model; its fallbacks stay.
    Set,
    /// A fallback after the others.
    Add,
    /// Entry `n` of the chain.
    Replace(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Assign {
    pub role: String,
    pub how: How,
    /// Whether the Roles view comes back once the model is picked.
    pub back: bool,
}

/// A row of the model list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Item {
    /// Index into the rows of the data.
    Model(usize),
    /// The line under the models roles use and those chosen lately.
    Rule,
}

/// A row of the Roles view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RoleLine {
    /// Index into the roles of the data.
    Role(usize),
    /// Entry `n` (1 or more) of a role's chain.
    Fallback(usize, usize),
    New,
}

/// A row of the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Side {
    /// `None` for the line between groups.
    pub scope: Option<Scope>,
    pub label: String,
    pub count: String,
    /// A provider without a credential.
    pub dim: bool,
    /// A search left nothing in it: arrows step over it.
    pub skip: bool,
}

/// What the screen asks for when a key has been handled.
pub(crate) enum Step {
    Pending,
    Close,
    Do(Request),
}

/// What the next answer about the roles is said as.
#[derive(Debug, Clone)]
enum Said {
    Role(String),
    Preset { name: String, saved: bool },
}

/// Where the last frame put things, for the mouse: rows and columns within the text of the box.
#[derive(Debug, Clone, Default)]
pub(super) struct Placed {
    /// The first row of the sidebar and the body, and how many there are.
    pub main: (usize, usize),
    /// The sidebar's width and its first entry in view; none when it is left out.
    pub side: Option<(usize, usize)>,
    /// The column the body starts at.
    pub body: usize,
    /// The first row of the list (or the Roles view, or the actions of a locked provider) in
    /// view, and how many rows it has.
    pub list: (usize, usize),
    /// The row of the strip and where its chips are.
    pub strip: Option<(usize, Vec<(usize, usize)>)>,
}

pub(crate) struct Models {
    pub(super) data: Data,
    pub(super) save: SaveTo,
    pub(super) scope: Scope,
    pub(super) focus: Focus,
    pub(super) query: String,
    pub(super) fetch: HashMap<String, Fetch>,
    pub(super) assign: Option<Assign>,
    pub(super) strip: Option<Strip>,
    /// What the last change came to, in its colour; shown until the next key.
    pub(super) status: Option<(Role, String)>,
    said: Option<Said>,
    ranker: Ranker,
    /// How each model matches the search; empty without one.
    hits: Vec<Option<Hit>>,
    /// The model list of the scope.
    pub(super) items: Vec<Item>,
    /// The rows of the Roles view.
    pub(super) lines: Vec<RoleLine>,
    pub(super) side: Vec<Side>,
    /// The selected row of the list, of the Roles view and of a locked provider's actions.
    pub(super) cursor: usize,
    pub(super) role_cursor: usize,
    pub(super) action: usize,
    pub(super) top: Cell<usize>,
    pub(super) side_top: Cell<usize>,
    /// Rows of the list the last frame showed: the distance of a page.
    pub(super) shown: Cell<usize>,
    pub(super) narrow: Cell<bool>,
    pub(super) placed: RefCell<Placed>,
    /// The frame of the spinner beside a provider whose list is being asked for.
    pub(super) frame: usize,
    /// A provider to ask for its models once the selection has rested on it.
    due: Option<(String, Instant)>,
    /// The symbols the screen's own messages are written with.
    pub(super) glyphs: Glyphs,
}

/// Whether a role has a value of its own rather than its built-in one.
pub(super) fn is_set(row: &RoleRow) -> bool {
    !matches!(row.source, RoleSource::Builtin | RoleSource::Unset)
}

/// The thinking level a chain entry is written with.
fn level_of(spec: &str) -> Option<ThinkingLevel> {
    match spec.parse::<azazel_ai::ModelSpec>() {
        Ok(azazel_ai::ModelSpec::Model(model)) => model.thinking,
        _ => None,
    }
}

impl Models {
    pub(crate) fn new(data: Data, opening: &Opening, asked: &BTreeSet<String>, glyphs: Glyphs) -> Models {
        let fetch = asked
            .iter()
            .map(|provider| (provider.clone(), Fetch::Done))
            .collect();
        let mut screen = Models {
            data,
            save: SaveTo::Home,
            scope: Scope::All,
            focus: Focus::Body,
            query: String::new(),
            fetch,
            assign: None,
            strip: None,
            status: None,
            said: None,
            ranker: Ranker::default(),
            hits: Vec::new(),
            items: Vec::new(),
            lines: Vec::new(),
            side: Vec::new(),
            cursor: 0,
            role_cursor: 0,
            action: 0,
            top: Cell::new(0),
            side_top: Cell::new(0),
            shown: Cell::new(10),
            narrow: Cell::new(false),
            placed: RefCell::default(),
            frame: 0,
            due: None,
            glyphs,
        };
        match (&opening.role, &opening.provider) {
            (Some(role), Some(provider)) => {
                screen.scope = Scope::Provider(provider.clone());
                screen.assign = Some(Assign {
                    role: role.clone(),
                    how: How::Set,
                    back: false,
                });
            }
            (Some(role), None) => {
                screen.scope = Scope::Roles;
                screen.refilter();
                screen.select_role(role);
            }
            (None, Some(provider)) => screen.scope = Scope::Provider(provider.clone()),
            (None, None) => {}
        }
        screen.refilter();
        if screen.scope != Scope::Roles {
            let current = screen.data.current.clone();
            screen.select_model(current.as_deref());
        }
        screen
    }

    /// `Models · saving to home`.
    pub(crate) fn title(&self) -> String {
        match self.save {
            SaveTo::Project => "Models · saving to project".to_string(),
            _ => "Models · saving to home".to_string(),
        }
    }

    // ------------------------------------------------------------ what the session said

    /// The providers whose lists are to be asked for now: those that can be used and were not
    /// asked yet. They are marked as being asked.
    pub(crate) fn start_refreshes(&mut self) -> Vec<String> {
        let wanted: Vec<String> = self
            .data
            .providers
            .iter()
            .filter(|provider| provider.usable && !self.fetch.contains_key(&provider.id))
            .map(|provider| provider.id.clone())
            .collect();
        for provider in &wanted {
            self.fetch.insert(provider.clone(), Fetch::Running);
        }
        self.refilter();
        wanted
    }

    /// Moves the spinner on. Returns the provider whose models are due to be asked for: the
    /// selection has rested on it long enough.
    pub(crate) fn tick(&mut self, now: Instant, frame: usize) -> Option<String> {
        if frame != self.frame && self.fetch.values().any(|fetch| *fetch == Fetch::Running) {
            self.frame = frame;
            self.side = self.side_entries();
        }
        self.frame = frame;
        let (provider, at) = self.due.as_ref()?;
        if now < *at {
            return None;
        }
        let provider = provider.clone();
        self.due = None;
        self.fetch.insert(provider.clone(), Fetch::Running);
        self.refilter();
        Some(provider)
    }

    /// A provider listed its models, or could not. The selection stays where it was.
    pub(crate) fn refreshed(&mut self, provider: &str, result: Result<Vec<ModelRow>, String>) {
        // Taken before the rows change: the list's entries point into the rows as they were.
        let selected = self.selected_model().map(|row| row.model.clone());
        match result {
            Ok(rows) => {
                if let Some(entry) = self.data.providers.iter_mut().find(|p| p.id == provider) {
                    entry.models = rows.len();
                }
                let providers = &self.data.providers;
                let place = |id: &str| providers.iter().position(|p| p.id == id);
                let mut kept: Vec<ModelRow> = std::mem::take(&mut self.data.rows)
                    .into_iter()
                    .filter(|row| row.provider != provider)
                    .collect();
                kept.extend(rows);
                kept.sort_by_key(|row| place(&row.provider).unwrap_or(usize::MAX));
                self.data.rows = kept;
                self.fetch.insert(provider.to_string(), Fetch::Done);
            }
            Err(why) => {
                self.fetch.insert(provider.to_string(), Fetch::Failed(why));
            }
        }
        self.items.clear();
        self.refilter();
        self.select_model(selected.as_deref());
        self.settle_cursor();
    }

    /// The session's models, providers and roles, read again after a sign-in; the screen goes
    /// to `provider`.
    pub(crate) fn reload(&mut self, data: Data, provider: Option<&str>) {
        let selected = self.selected_model().map(|row| row.model.clone());
        self.data = data;
        // The list's entries point into the rows that were just replaced.
        self.items.clear();
        if let Some(provider) = provider {
            self.fetch.remove(provider);
            self.scope = Scope::Provider(provider.to_string());
            self.focus = Focus::Body;
            self.cursor = 0;
            self.refilter();
            return;
        }
        self.refilter();
        self.select_model(selected.as_deref());
        self.settle_cursor();
    }

    /// The roles as a change left them, or why the change was refused. `current` is the model
    /// the session is on now.
    pub(crate) fn roles_changed(&mut self, result: Result<Roles, String>, current: Option<String>) {
        let said = self.said.take();
        let roles = match result {
            Ok(roles) => roles,
            Err(why) => {
                self.strip = None;
                self.status = Some((Role::Error, why));
                return;
            }
        };
        self.data.roles = roles;
        if current.is_some() {
            self.data.current = current;
        }
        self.status = match said {
            Some(Said::Role(role)) => Some(self.change_line(&role)),
            Some(Said::Preset { name, saved: true }) => Some((
                Role::Success,
                format!("saved the roles as preset {name} (home config)"),
            )),
            Some(Said::Preset { name, saved: false }) => {
                Some((Role::Success, format!("preset {name} is in use for this session")))
            }
            None => None,
        };
        self.refilter();
    }

    /// What a change of `role` came to, as the status row says it.
    fn change_line(&self, role: &str) -> (Role, String) {
        let place = match self.save {
            SaveTo::Project => "project config",
            _ => "home config",
        };
        let arrow = format!(" {} ", self.glyphs.get(Glyph::Arrow));
        let Some(row) = self.role(role) else {
            return (Role::Success, format!("{role} is gone ({place})"));
        };
        let chain = match row.chain.is_empty() {
            true => "nothing".to_string(),
            false => row.chain.join(&arrow),
        };
        let line = match row.source {
            RoleSource::Builtin => format!("{role} is back on its built-in value: {chain} ({place})"),
            RoleSource::Unset => format!("{role} has no model now ({place})"),
            _ => format!("{role}{arrow}{chain} ({place})"),
        };
        let shadowed = match (self.save, row.source) {
            (SaveTo::Home, RoleSource::Project | RoleSource::ProjectLocal) => Some("the project's"),
            (SaveTo::Project, RoleSource::ProjectLocal) => Some("the project's local"),
            _ => None,
        };
        match shadowed {
            Some(file) => (
                Role::Warning,
                format!("{line}; {file} config sets {role} too, and wins here"),
            ),
            None => (Role::Success, line),
        }
    }

    pub(crate) fn say(&mut self, role: Role, text: &str) {
        self.status = Some((role, text.to_string()));
    }

    // ------------------------------------------------------------ what is shown

    pub(super) fn role(&self, name: &str) -> Option<&RoleRow> {
        self.data.roles.roles.iter().find(|row| row.role == name)
    }

    pub(super) fn provider(&self, id: &str) -> Option<&ProviderRow> {
        self.data.providers.iter().find(|provider| provider.id == id)
    }

    /// Whether the body shows a provider without a credential.
    pub(super) fn locked(&self) -> Option<&ProviderRow> {
        match &self.scope {
            Scope::Provider(id) => self.provider(id).filter(|provider| !provider.usable),
            _ => None,
        }
    }

    pub(super) fn matched(&self, index: usize) -> bool {
        self.query.trim().is_empty() || self.hits.get(index).is_some_and(Option::is_some)
    }

    /// The models of the data that roles name first, in the order of the roles.
    fn assigned(&self) -> Vec<String> {
        let mut assigned: Vec<String> = Vec::new();
        for row in self.data.roles.roles.iter().filter(|row| is_set(row)) {
            if let Some(first) = row.chain.first().filter(|spec| !spec.starts_with('@')) {
                let first = list::bare(first);
                if !assigned.contains(&first) {
                    assigned.push(first);
                }
            }
        }
        assigned
    }

    /// The roles whose chain names `model` first.
    pub(super) fn tags(&self, model: &str) -> Vec<String> {
        self.data
            .roles
            .roles
            .iter()
            .filter(|row| is_set(row) && row.chain.first().is_some_and(|first| list::bare(first) == model))
            .map(|row| row.role.clone())
            .collect()
    }

    /// The roles whose first usable model is `model`, each with the level it thinks at.
    pub(super) fn used_by(&self, model: &str) -> Vec<(String, Option<String>)> {
        self.data
            .roles
            .roles
            .iter()
            .filter(|row| {
                row.resolved
                    .first()
                    .is_some_and(|first| list::bare(first) == model)
            })
            .map(|row| (row.role.clone(), row.thinking.clone()))
            .collect()
    }

    /// Builds what every scope shows again, for the search, the scope and the data as they are
    /// now, staying on the selected rows.
    pub(super) fn refilter(&mut self) {
        let model = self.selected_model().map(|row| row.model.clone());
        let line = self.selected_line();
        let line_key = line.map(|line| match line {
            RoleLine::Role(at) => (self.data.roles.roles[at].role.clone(), 0),
            RoleLine::Fallback(at, entry) => (self.data.roles.roles[at].role.clone(), entry),
            RoleLine::New => (String::new(), usize::MAX),
        });

        let query = self.query.trim().to_string();
        self.hits = match query.is_empty() {
            true => Vec::new(),
            false => self.ranker.hits(&query, &self.data.rows),
        };
        self.side = self.side_entries();
        if self.scope_entry().is_none_or(|entry| entry.skip) {
            // A search left nothing where the screen was: the first scope of models that has
            // something, or the roles.
            let models =
                |entry: &&Side| !entry.skip && entry.scope.as_ref().is_some_and(|s| *s != Scope::Roles);
            let found = self.side.iter().find(models).or_else(|| {
                self.side
                    .iter()
                    .find(|entry| !entry.skip && entry.scope.is_some() && self.assign.is_none())
            });
            if let Some(first) = found {
                self.scope = first.scope.clone().unwrap_or(Scope::All);
            }
        }
        self.items = self.model_items();
        self.lines = self.role_lines();

        self.cursor = model
            .and_then(|model| {
                self.items
                    .iter()
                    .position(|item| matches!(item, Item::Model(at) if self.data.rows[*at].model == model))
            })
            .unwrap_or_else(|| self.cursor.min(self.items.len().saturating_sub(1)));
        self.settle_cursor();
        self.role_cursor = line_key
            .and_then(|(role, entry)| {
                self.lines.iter().position(|line| match *line {
                    RoleLine::Role(at) => entry == 0 && self.data.roles.roles[at].role == role,
                    RoleLine::Fallback(at, n) => n == entry && self.data.roles.roles[at].role == role,
                    RoleLine::New => entry == usize::MAX,
                })
            })
            .unwrap_or_else(|| self.role_cursor.min(self.lines.len().saturating_sub(1)));
    }

    fn scope_entry(&self) -> Option<&Side> {
        self.side
            .iter()
            .find(|entry| entry.scope.as_ref() == Some(&self.scope))
    }

    /// The model list of the scope: without a search, the models roles use and those chosen
    /// lately first, then a rule, then the others; with one, the matches best first.
    fn model_items(&self) -> Vec<Item> {
        let usable = |row: &ModelRow| row.usable;
        let base: Vec<usize> = match &self.scope {
            Scope::Roles => return Vec::new(),
            Scope::All => (0..self.data.rows.len())
                .filter(|&at| usable(&self.data.rows[at]))
                .collect(),
            Scope::Provider(id) => (0..self.data.rows.len())
                .filter(|&at| self.data.rows[at].provider == *id)
                .collect(),
            Scope::Recent => self
                .data
                .recent
                .iter()
                .filter_map(|spec| {
                    let spec = list::bare(spec);
                    self.data
                        .rows
                        .iter()
                        .position(|row| row.model == spec && row.usable)
                })
                .collect(),
        };
        if !self.query.trim().is_empty() {
            return list::ranked(&self.hits, &base)
                .into_iter()
                .map(Item::Model)
                .collect();
        }
        if self.scope == Scope::Recent {
            return base.into_iter().map(Item::Model).collect();
        }
        let mut first: Vec<String> = self.assigned();
        for recent in &self.data.recent {
            let recent = list::bare(recent);
            if !first.contains(&recent) {
                first.push(recent);
            }
        }
        let mut top: Vec<usize> = first
            .iter()
            .filter_map(|model| {
                base.iter()
                    .copied()
                    .find(|&at| self.data.rows[at].model == *model)
            })
            .collect();
        top.dedup();
        let rest: Vec<usize> = base.iter().copied().filter(|at| !top.contains(at)).collect();
        let mut items: Vec<Item> = top.into_iter().map(Item::Model).collect();
        if !items.is_empty() && !rest.is_empty() {
            items.push(Item::Rule);
        }
        items.extend(rest.into_iter().map(Item::Model));
        items
    }

    pub(super) fn selected_model(&self) -> Option<&ModelRow> {
        match self.items.get(self.cursor)? {
            Item::Model(at) => self.data.rows.get(*at),
            Item::Rule => None,
        }
    }

    pub(super) fn selected_line(&self) -> Option<RoleLine> {
        self.lines.get(self.role_cursor).copied()
    }

    /// Puts the cursor on the row of `model`, when the list has it.
    fn select_model(&mut self, model: Option<&str>) {
        let Some(model) = model.map(list::bare) else {
            return;
        };
        if let Some(at) = self
            .items
            .iter()
            .position(|item| matches!(item, Item::Model(at) if self.data.rows[*at].model == model))
        {
            self.cursor = at;
        }
    }

    fn select_role(&mut self, role: &str) {
        if let Some(at) = self
            .lines
            .iter()
            .position(|line| matches!(line, RoleLine::Role(at) if self.data.roles.roles[*at].role == role))
        {
            self.role_cursor = at;
        }
    }

    /// Keeps the cursor off the rule.
    fn settle_cursor(&mut self) {
        if matches!(self.items.get(self.cursor), Some(Item::Rule)) {
            self.cursor += 1;
        }
    }

    // ------------------------------------------------------------ keys

    /// Takes a key; what the UI is to do comes back.
    pub(crate) fn on_key(&mut self, key: &KeyEvent) -> Step {
        self.status = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && key.code == KeyCode::Char('c') {
            return Step::Close;
        }
        if let Some(strip) = &mut self.strip {
            return match strip.on_key(key) {
                strip::Key::Pending => Step::Pending,
                strip::Key::Close => {
                    self.strip = None;
                    Step::Pending
                }
                strip::Key::Chosen(index) => self.strip_chosen(index),
                strip::Key::Typed(name) => self.name_typed(name),
            };
        }
        // Anywhere, also where letters search.
        if alt {
            return match key.code {
                KeyCode::Char('s') => self.ask_preset_name(),
                KeyCode::Char('l') => self.offer_presets(),
                KeyCode::Char('w') => self.switch_target(),
                _ => Step::Pending,
            };
        }
        match key.code {
            KeyCode::Esc => return self.escape(),
            KeyCode::F(5) => return self.refresh_selected(),
            KeyCode::Tab | KeyCode::BackTab if self.narrow.get() => {
                self.step_scope(key.code == KeyCode::Tab);
                return Step::Pending;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Side => Focus::Body,
                    Focus::Body => Focus::Side,
                };
                return Step::Pending;
            }
            _ => {}
        }
        if self.focus == Focus::Side && !self.narrow.get() {
            return self.side_key(key);
        }
        if self.scope == Scope::Roles {
            return self.roles_key(key);
        }
        if self.locked().is_some() {
            return self.locked_key(key);
        }
        self.list_key(key)
    }

    /// Esc, a step at a time: the strip, then assign mode, then the search, then the screen.
    fn escape(&mut self) -> Step {
        if let Some(assign) = self.assign.take() {
            if assign.back {
                self.scope = Scope::Roles;
                self.focus = Focus::Body;
                self.refilter();
                self.select_role(&assign.role);
            }
            return Step::Pending;
        }
        if !self.query.is_empty() {
            self.query.clear();
            self.refilter();
            return Step::Pending;
        }
        Step::Close
    }

    /// A letter for the search, or backspace.
    fn search_key(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => self.query.push(c),
            _ => return false,
        }
        self.searched();
        true
    }

    /// The search changed: the best place to be is the first match.
    fn searched(&mut self) {
        self.refilter();
        self.cursor = 0;
        self.role_cursor = 0;
        self.top.set(0);
        self.settle_cursor();
    }

    fn side_key(&mut self, key: &KeyEvent) -> Step {
        match key.code {
            KeyCode::Up => self.step_scope(false),
            KeyCode::Down => self.step_scope(true),
            KeyCode::Home => self.end_scope(false),
            KeyCode::End => self.end_scope(true),
            KeyCode::Right | KeyCode::Enter => self.focus = Focus::Body,
            _ => {
                self.search_key(key);
            }
        }
        Step::Pending
    }

    /// The next (or previous) scope that has something in it.
    pub(super) fn step_scope(&mut self, forward: bool) {
        let entries: Vec<&Scope> = self
            .side
            .iter()
            .filter(|entry| !entry.skip)
            .filter_map(|entry| entry.scope.as_ref())
            .collect();
        let Some(at) = entries.iter().position(|scope| **scope == self.scope) else {
            if let Some(first) = entries.first() {
                let first = (*first).clone();
                self.go_to_scope(first);
            }
            return;
        };
        let next = match forward {
            true => (at + 1) % entries.len(),
            false => (at + entries.len() - 1) % entries.len(),
        };
        let next = entries[next].clone();
        self.go_to_scope(next);
    }

    fn end_scope(&mut self, last: bool) {
        let mut entries = self
            .side
            .iter()
            .filter(|entry| !entry.skip)
            .filter_map(|entry| entry.scope.clone());
        let scope = match last {
            true => entries.next_back(),
            false => entries.next(),
        };
        if let Some(scope) = scope {
            self.go_to_scope(scope);
        }
    }

    /// Shows `scope` in the body. A provider whose models were not asked for this session is
    /// asked once the selection has rested on it; going to the Roles view ends assign mode.
    pub(super) fn go_to_scope(&mut self, scope: Scope) {
        if scope == self.scope {
            return;
        }
        if scope == Scope::Roles {
            self.assign = None;
        }
        self.due = match &scope {
            Scope::Provider(id)
                if self.provider(id).is_some_and(|p| p.usable) && !self.fetch.contains_key(id) =>
            {
                Some((id.clone(), Instant::now() + DEBOUNCE))
            }
            _ => None,
        };
        self.scope = scope;
        self.cursor = 0;
        self.action = 0;
        self.top.set(0);
        self.refilter();
    }

    fn list_key(&mut self, key: &KeyEvent) -> Step {
        let count = self.items.len();
        let page = self.shown.get().max(1);
        match key.code {
            KeyCode::Up => self.move_list(-1),
            KeyCode::Down => self.move_list(1),
            KeyCode::PageUp => self.move_list(-(page as isize)),
            KeyCode::PageDown => self.move_list(page as isize),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = count.saturating_sub(1),
            KeyCode::Left if !self.narrow.get() => self.focus = Focus::Side,
            KeyCode::Enter => return self.pick_model(),
            _ => {
                self.search_key(key);
            }
        }
        self.settle_cursor();
        Step::Pending
    }

    pub(super) fn move_list(&mut self, by: isize) {
        let count = self.items.len();
        if count == 0 {
            return;
        }
        let mut at = self.cursor.saturating_add_signed(by).min(count - 1);
        if matches!(self.items.get(at), Some(Item::Rule)) {
            at = match by < 0 {
                true => at.saturating_sub(1),
                false => (at + 1).min(count - 1),
            };
        }
        self.cursor = at;
    }

    fn locked_key(&mut self, key: &KeyEvent) -> Step {
        let actions = self.locked().map(locked_actions).unwrap_or_default();
        match key.code {
            KeyCode::Up => self.action = self.action.saturating_sub(1),
            KeyCode::Down => self.action = (self.action + 1).min(actions.len().saturating_sub(1)),
            KeyCode::Left if !self.narrow.get() => self.focus = Focus::Side,
            KeyCode::Enter => return self.run_action(),
            _ => {
                self.search_key(key);
            }
        }
        Step::Pending
    }

    /// Enter on a locked provider's page: its sign-in, or a key for it.
    pub(super) fn run_action(&mut self) -> Step {
        let Some(provider) = self.locked() else {
            return Step::Pending;
        };
        let id = provider.id.clone();
        match locked_actions(provider).get(self.action) {
            Some(Action::SignIn) => Step::Do(Request::SignIn(id)),
            Some(Action::AddKey) => Step::Do(Request::AddKey(id)),
            None => Step::Pending,
        }
    }

    fn roles_key(&mut self, key: &KeyEvent) -> Step {
        let count = self.lines.len();
        let page = self.shown.get().max(1);
        match key.code {
            KeyCode::Up => self.role_cursor = self.role_cursor.saturating_sub(1),
            KeyCode::Down => self.role_cursor = (self.role_cursor + 1).min(count.saturating_sub(1)),
            KeyCode::PageUp => self.role_cursor = self.role_cursor.saturating_sub(page),
            KeyCode::PageDown => self.role_cursor = (self.role_cursor + page).min(count.saturating_sub(1)),
            KeyCode::Home => self.role_cursor = 0,
            KeyCode::End => self.role_cursor = count.saturating_sub(1),
            KeyCode::Left if !self.narrow.get() => self.focus = Focus::Side,
            KeyCode::Enter => return self.pick_for_line(),
            KeyCode::Delete | KeyCode::Char('x') => return self.clear_line(),
            KeyCode::Char('t') => return self.thinking_for_line(),
            KeyCode::Char('f') => return self.add_fallback(),
            KeyCode::Char('[') => return self.reorder(false),
            KeyCode::Char(']') => return self.reorder(true),
            KeyCode::Char('c') => return self.toggle_cycle(),
            KeyCode::Char('n') => return self.ask_role_name(),
            KeyCode::Char('s') => return self.ask_preset_name(),
            KeyCode::Char('l') => return self.offer_presets(),
            KeyCode::Char('w') => return self.switch_target(),
            _ => {
                self.search_key(key);
            }
        }
        Step::Pending
    }

    // ------------------------------------------------------------ changes

    fn set_role(&mut self, role: &str, chain: Option<Vec<String>>) -> Step {
        self.said = Some(Said::Role(role.to_string()));
        Step::Do(Request::SetRole {
            role: role.to_string(),
            chain,
            save: self.save,
        })
    }

    /// The chain of `role` as it stands, built-in or set.
    fn chain(&self, role: &str) -> Vec<String> {
        self.role(role).map(|row| row.chain.clone()).unwrap_or_default()
    }

    /// Enter on a model: in assign mode it is the model picked; else the roles it can take.
    fn pick_model(&mut self) -> Step {
        let Some(model) = self.selected_model() else {
            return Step::Pending;
        };
        let (key, vision) = (model.model.clone(), model.vision);
        if let Some(assign) = self.assign.take() {
            return self.commit(assign, &key);
        }
        // The vision role is for what has to see images.
        let roles: Vec<String> = self
            .data
            .roles
            .roles
            .iter()
            .filter(|row| vision || row.role != "vision")
            .map(|row| row.role.clone())
            .collect();
        let held = roles.iter().map(|role| self.tags(&key).contains(role)).collect();
        self.strip = Some(Strip::Roles {
            model: key,
            roles,
            held,
            at: 0,
        });
        Step::Pending
    }

    /// Makes `model` what `assign` says, then offers its thinking levels.
    fn commit(&mut self, assign: Assign, model: &str) -> Step {
        let role = assign.role.clone();
        let mut chain = self.chain(&role);
        let set = self.role(&role).is_some_and(is_set);
        let entry = match assign.how {
            How::Set if set && !chain.is_empty() => {
                chain[0] = model.to_string();
                0
            }
            How::Set => {
                chain = vec![model.to_string()];
                0
            }
            How::Add => {
                chain.push(model.to_string());
                chain.len() - 1
            }
            How::Replace(n) if n < chain.len() => {
                chain[n] = model.to_string();
                n
            }
            How::Replace(_) => return Step::Pending,
        };
        if assign.back {
            self.scope = Scope::Roles;
            self.focus = Focus::Body;
            self.query.clear();
        }
        self.strip = self.thinking_strip(&role, entry, model);
        let step = self.set_role(&role, Some(chain));
        // The Roles view comes back on the role.
        self.refilter();
        self.select_role(&role);
        step
    }

    /// The thinking levels of `model` for entry `entry` of `role`: `inherit`, `off`, then the
    /// levels the model takes (every one when the catalogue names none); none for a model that
    /// does not think.
    fn thinking_strip(&self, role: &str, entry: usize, model: &str) -> Option<Strip> {
        let row = self.data.rows.iter().find(|row| row.model == list::bare(model))?;
        if !row.reasoning {
            return None;
        }
        let levels: Vec<ThinkingLevel> = match row.thinking_levels.is_empty() {
            true => ThinkingLevel::ALL.to_vec(),
            false => row
                .thinking_levels
                .iter()
                .filter_map(|level| level.parse().ok())
                .collect(),
        };
        let mut options = vec![None, Some(ThinkingLevel::Off)];
        options.extend(
            levels
                .into_iter()
                .filter(|level| *level != ThinkingLevel::Off)
                .map(Some),
        );
        let now = level_of(model);
        let at = options.iter().position(|option| *option == now).unwrap_or(0);
        Some(Strip::Thinking {
            role: role.to_string(),
            entry,
            options,
            at,
        })
    }

    fn strip_chosen(&mut self, index: usize) -> Step {
        let Some(strip) = self.strip.take() else {
            return Step::Pending;
        };
        match strip {
            Strip::Roles { model, roles, .. } => match roles.get(index) {
                Some(role) => {
                    let assign = Assign {
                        role: role.clone(),
                        how: How::Set,
                        back: false,
                    };
                    self.commit(assign, &model)
                }
                // The chip after the roles: which role it is to be a fallback for.
                None => {
                    self.strip = Some(Strip::Fallback { model, roles, at: 0 });
                    Step::Pending
                }
            },
            Strip::Fallback { model, roles, .. } => match roles.get(index) {
                Some(role) => {
                    let assign = Assign {
                        role: role.clone(),
                        how: How::Add,
                        back: false,
                    };
                    self.commit(assign, &model)
                }
                None => Step::Pending,
            },
            Strip::Thinking {
                role,
                entry,
                options,
                at,
            } => {
                let level = options.get(index).copied().flatten();
                let mut chain = self.chain(&role);
                let Some(spec) = chain.get_mut(entry) else {
                    return Step::Pending;
                };
                // Choosing what it is already changes nothing.
                if level_of(spec) == level && index == at {
                    return Step::Pending;
                }
                let model = list::bare(spec);
                *spec = match level {
                    Some(level) => format!("{model}:{level}"),
                    None => model,
                };
                self.set_role(&role, Some(chain))
            }
            Strip::Presets { names, .. } => match names.get(index) {
                Some(name) => {
                    self.said = Some(Said::Preset {
                        name: name.clone(),
                        saved: false,
                    });
                    Step::Do(Request::ApplyPreset(name.clone()))
                }
                None => Step::Pending,
            },
            Strip::Name { .. } => Step::Pending,
        }
    }

    fn name_typed(&mut self, name: String) -> Step {
        let Some(Strip::Name { purpose, .. }) = &self.strip else {
            return Step::Pending;
        };
        let purpose = *purpose;
        if !crate::routing::plain_name(&name) {
            if let Some(strip) = &mut self.strip {
                strip.refuse("letters, digits, - and _, starting with a letter".to_string());
            }
            return Step::Pending;
        }
        self.strip = None;
        match purpose {
            Purpose::Preset => {
                self.said = Some(Said::Preset {
                    name: name.clone(),
                    saved: true,
                });
                Step::Do(Request::SavePreset(name))
            }
            Purpose::NewRole if self.role(&name).is_some() => {
                self.status = Some((Role::Warning, format!("there is a role {name} already")));
                self.select_role(&name);
                Step::Pending
            }
            Purpose::NewRole => {
                self.start_assign(&name, How::Set);
                self.status = Some((Role::Accent, format!("pick the model of the new role {name}")));
                Step::Pending
            }
        }
    }

    /// Opens the model list to choose a model for `role`, with the model it has selected.
    fn start_assign(&mut self, role: &str, how: How) {
        let now = match how {
            How::Replace(n) => self.chain(role).get(n).cloned(),
            How::Set => self.role(role).and_then(|row| row.resolved.first().cloned()),
            How::Add => None,
        };
        self.assign = Some(Assign {
            role: role.to_string(),
            how,
            back: true,
        });
        self.query.clear();
        self.scope = Scope::All;
        self.focus = Focus::Body;
        self.cursor = 0;
        self.top.set(0);
        self.refilter();
        self.select_model(now.as_deref());
    }

    fn pick_for_line(&mut self) -> Step {
        match self.selected_line() {
            Some(RoleLine::Role(at)) => {
                let role = self.data.roles.roles[at].role.clone();
                self.start_assign(&role, How::Set);
            }
            Some(RoleLine::Fallback(at, entry)) => {
                let role = self.data.roles.roles[at].role.clone();
                self.start_assign(&role, How::Replace(entry));
            }
            Some(RoleLine::New) => return self.ask_role_name(),
            None => {}
        }
        Step::Pending
    }

    fn line_role(&self) -> Option<(String, usize)> {
        match self.selected_line()? {
            RoleLine::Role(at) => Some((self.data.roles.roles[at].role.clone(), 0)),
            RoleLine::Fallback(at, entry) => Some((self.data.roles.roles[at].role.clone(), entry)),
            RoleLine::New => None,
        }
    }

    fn clear_line(&mut self) -> Step {
        let Some((role, entry)) = self.line_role() else {
            return Step::Pending;
        };
        if entry == 0 {
            return self.set_role(&role, None);
        }
        let mut chain = self.chain(&role);
        if entry < chain.len() {
            chain.remove(entry);
        }
        self.set_role(&role, Some(chain))
    }

    fn thinking_for_line(&mut self) -> Step {
        let Some((role, entry)) = self.line_role() else {
            return Step::Pending;
        };
        let chain = self.chain(&role);
        let set = self.role(&role).is_some_and(is_set);
        match chain.get(entry) {
            Some(spec) if set && !spec.starts_with('@') => {
                let spec = spec.clone();
                self.strip = self.thinking_strip(&role, entry, &spec);
                if self.strip.is_none() {
                    self.status = Some((Role::Muted, format!("{} does not think", list::bare(&spec))));
                }
            }
            _ => {
                self.status = Some((
                    Role::Muted,
                    format!("{role} follows another role; enter gives it a model of its own"),
                ));
            }
        }
        Step::Pending
    }

    fn add_fallback(&mut self) -> Step {
        if let Some((role, _)) = self.line_role() {
            self.start_assign(&role, How::Add);
        }
        Step::Pending
    }

    /// `[` and `]`: the place of a role in the ctrl+p cycle, or of a fallback in its chain.
    fn reorder(&mut self, later: bool) -> Step {
        let Some((role, entry)) = self.line_role() else {
            return Step::Pending;
        };
        if entry == 0 {
            let spec = format!("@{role}");
            if !self.data.cycle.contains(&spec) {
                self.status = Some((Role::Muted, format!("c adds {role} to the ctrl+p cycle first")));
                return Step::Pending;
            }
            let change = match later {
                true => FavouriteChange::Later,
                false => FavouriteChange::Earlier,
            };
            return self.change_cycle(spec, change);
        }
        let mut chain = self.chain(&role);
        let other = match later {
            true => entry + 1,
            false => entry - 1,
        };
        // The first model stays first: fallbacks move among themselves.
        if other == 0 || other >= chain.len() {
            return Step::Pending;
        }
        chain.swap(entry, other);
        let step = self.set_role(&role, Some(chain));
        // The cursor goes with the entry.
        if let Some(at) = self.lines.iter().position(|line| {
            matches!(line, RoleLine::Fallback(at, n) if *n == other && self.data.roles.roles[*at].role == role)
        }) {
            self.role_cursor = at;
        }
        step
    }

    fn toggle_cycle(&mut self) -> Step {
        let Some((role, _)) = self.line_role() else {
            return Step::Pending;
        };
        let spec = format!("@{role}");
        let change = match self.data.cycle.contains(&spec) {
            true => FavouriteChange::Remove,
            false => FavouriteChange::Add,
        };
        self.change_cycle(spec, change)
    }

    fn change_cycle(&mut self, spec: String, change: FavouriteChange) -> Step {
        self.data.cycle = favourites_changed(std::mem::take(&mut self.data.cycle), &spec, change);
        let step = format!(" {} ", self.glyphs.get(Glyph::Step));
        self.status = Some((
            Role::Success,
            match self.data.cycle.is_empty() {
                true => {
                    "the ctrl+p cycle is empty: ctrl+p steps through the roles that have models".to_string()
                }
                false => format!("ctrl+p cycle: {} (home config)", self.data.cycle.join(&step)),
            },
        ));
        Step::Do(Request::Cycle { spec, change })
    }

    fn ask_role_name(&mut self) -> Step {
        self.strip = Some(Strip::Name {
            purpose: Purpose::NewRole,
            editor: Box::new(Editor::new()),
            error: None,
        });
        Step::Pending
    }

    fn ask_preset_name(&mut self) -> Step {
        self.strip = Some(Strip::Name {
            purpose: Purpose::Preset,
            editor: Box::new(Editor::new()),
            error: None,
        });
        Step::Pending
    }

    fn offer_presets(&mut self) -> Step {
        let names = self.data.roles.presets.clone();
        if names.is_empty() {
            self.status = Some((
                Role::Muted,
                "no preset is saved yet; s saves the roles as one".to_string(),
            ));
            return Step::Pending;
        }
        self.strip = Some(Strip::Presets { names, at: 0 });
        Step::Pending
    }

    /// `w`: changes go to the home config, or to the project's when it is trusted.
    fn switch_target(&mut self) -> Step {
        match self.save {
            SaveTo::Project => self.save = SaveTo::Home,
            _ if self.data.roles.project => self.save = SaveTo::Project,
            _ => {
                self.status = Some((
                    Role::Warning,
                    "this project is not trusted, so its config is not written (/trust yes trusts it)"
                        .to_string(),
                ));
            }
        }
        Step::Pending
    }

    /// F5: the selected provider's models, asked for again.
    fn refresh_selected(&mut self) -> Step {
        let provider = match &self.scope {
            Scope::Provider(id) => Some(id.clone()),
            _ => self.selected_model().map(|row| row.provider.clone()),
        };
        let Some(provider) = provider else {
            return Step::Pending;
        };
        if !self.provider(&provider).is_some_and(|p| p.usable) {
            self.status = Some((Role::Warning, format!("{provider} has no credential to ask with")));
            return Step::Pending;
        }
        if self.fetch.get(&provider) == Some(&Fetch::Running) {
            return Step::Pending;
        }
        self.fetch.insert(provider.clone(), Fetch::Running);
        self.refilter();
        Step::Do(Request::Refresh(provider))
    }

    pub(crate) fn on_paste(&mut self, pasted: &str) {
        if let Some(strip) = &mut self.strip {
            return strip.on_paste(pasted);
        }
        self.query.push_str(pasted.lines().next().unwrap_or_default());
        self.searched();
    }

    // ------------------------------------------------------------ mouse

    /// The wheel moves the selection of the part under it; a click selects, and a click on
    /// what is selected does what enter does. A chip of the strip is picked with one click.
    pub(crate) fn on_mouse(&mut self, mouse: Mouse) -> Step {
        // Row 0 is the top edge of the box and the text starts two columns in.
        let (Some(row), Some(column)) = (mouse.row.checked_sub(1), mouse.column.checked_sub(2)) else {
            return Step::Pending;
        };
        let placed = self.placed.borrow().clone();
        if let Some((at, chips)) = &placed.strip
            && row == *at
        {
            if mouse.kind != MouseKind::Press {
                return Step::Pending;
            }
            let Some(index) = chips.iter().position(|(from, to)| (*from..*to).contains(&column)) else {
                return Step::Pending;
            };
            if let Some(strip) = &mut self.strip
                && strip.select(index)
            {
                return self.strip_chosen(index);
            }
            return Step::Pending;
        }
        let (main, rows) = placed.main;
        let Some(offset) = row.checked_sub(main).filter(|offset| *offset < rows) else {
            return Step::Pending;
        };
        let in_side = placed.side.filter(|(width, _)| column < *width);
        if let Some((_, top)) = in_side {
            return self.side_mouse(mouse.kind, top + offset);
        }
        if column < placed.body {
            return Step::Pending;
        }
        let (list_row, list_rows) = placed.list;
        let Some(at) = row.checked_sub(list_row).filter(|at| *at < list_rows) else {
            return match mouse.kind {
                MouseKind::ScrollUp => self.wheel(-1),
                MouseKind::ScrollDown => self.wheel(1),
                _ => Step::Pending,
            };
        };
        match mouse.kind {
            MouseKind::ScrollUp => self.wheel(-1),
            MouseKind::ScrollDown => self.wheel(1),
            MouseKind::Press => self.body_click(at),
            MouseKind::Drag | MouseKind::Move => Step::Pending,
        }
    }

    fn side_mouse(&mut self, kind: MouseKind, index: usize) -> Step {
        match kind {
            MouseKind::ScrollUp => self.step_scope(false),
            MouseKind::ScrollDown => self.step_scope(true),
            MouseKind::Press => {
                let Some(scope) = self
                    .side
                    .get(index)
                    .filter(|entry| !entry.skip)
                    .and_then(|entry| entry.scope.clone())
                else {
                    return Step::Pending;
                };
                if self.focus == Focus::Side && scope == self.scope {
                    self.focus = Focus::Body;
                } else {
                    self.focus = Focus::Side;
                    self.go_to_scope(scope);
                }
            }
            MouseKind::Drag | MouseKind::Move => {}
        }
        Step::Pending
    }

    fn wheel(&mut self, by: isize) -> Step {
        if self.scope == Scope::Roles {
            self.role_cursor = self
                .role_cursor
                .saturating_add_signed(by)
                .min(self.lines.len().saturating_sub(1));
        } else if self.locked().is_none() {
            self.move_list(by);
        }
        Step::Pending
    }

    /// A click on row `at` of the list in view.
    fn body_click(&mut self, at: usize) -> Step {
        let again = self.focus == Focus::Body;
        self.focus = Focus::Body;
        if self.scope == Scope::Roles {
            let index = self.top.get() + at;
            if index >= self.lines.len() {
                return Step::Pending;
            }
            if again && index == self.role_cursor {
                return self.pick_for_line();
            }
            self.role_cursor = index;
            return Step::Pending;
        }
        if self.locked().is_some() {
            if again && at == self.action {
                return self.run_action();
            }
            self.action = at.min(
                self.locked()
                    .map_or(0, |p| locked_actions(p).len())
                    .saturating_sub(1),
            );
            return Step::Pending;
        }
        let index = self.top.get() + at;
        match self.items.get(index) {
            Some(Item::Model(_)) if again && index == self.cursor => self.pick_model(),
            Some(Item::Model(_)) => {
                self.cursor = index;
                Step::Pending
            }
            _ => Step::Pending,
        }
    }

    // ------------------------------------------------------------ drawing

    /// The screen as rows for a box `width` columns wide inside and `rows` tall: always that
    /// many, so the box does not change size as lists do.
    pub(crate) fn view(&self, width: usize, rows: usize, theme: &Theme) -> View {
        self.narrow.set(width < NARROW);
        let mut lines: Vec<Line> = Vec::with_capacity(rows);
        let mut placed = Placed::default();
        lines.push(text::truncate(&self.status_row(theme), width, ""));
        let (query, column) = self.query_row(width, theme);
        lines.push(query);
        let cursor = (self.strip.is_none() && (self.focus == Focus::Side || self.scope != Scope::Roles))
            .then_some((1, column));
        let main_rows = rows.saturating_sub(3).max(1);
        placed.main = (2, main_rows);
        let side_width = match self.narrow.get() {
            true => 0,
            false => self.side_width(),
        };
        let body_width = match side_width {
            0 => width,
            side => width.saturating_sub(side + 3).max(1),
        };
        placed.body = match side_width {
            0 => 0,
            side => side + 3,
        };
        let (body, scroll) = self.body(body_width, main_rows, theme, &mut placed);
        let side = match side_width {
            0 => Vec::new(),
            side => {
                placed.side = Some((side, 0));
                self.side_rows(side, main_rows, theme, &mut placed)
            }
        };
        let divider = theme.paint(Role::BorderMuted, theme.glyphs().border().vertical);
        for row in 0..main_rows {
            let right = body.get(row).cloned().unwrap_or_default();
            let line = match side_width {
                0 => right,
                width => {
                    let left = side.get(row).cloned().unwrap_or_default();
                    format!("{} {divider} {right}", text::pad_right(&left, width))
                }
            };
            lines.push(text::truncate(&line, width, ""));
        }
        let mut cursor = cursor;
        match &self.strip {
            Some(strip) => {
                let drawn = strip.draw(width, theme);
                placed.strip = Some((lines.len(), drawn.chips));
                if let Some(column) = drawn.cursor {
                    cursor = Some((lines.len(), column));
                }
                lines.push(drawn.line);
            }
            None => lines.push(String::new()),
        }
        lines.truncate(rows);
        let scroll = scroll.map(|(first, scroll)| (first + 2, scroll));
        *self.placed.borrow_mut() = placed;
        View {
            lines,
            cursor: cursor.filter(|(row, _)| *row < rows),
            scroll,
        }
    }

    /// The first row: what the last change came to, else what the body shows.
    fn status_row(&self, theme: &Theme) -> Line {
        if let Some((role, text)) = &self.status {
            return theme.paint(*role, &text::sanitize(text));
        }
        if let Some(assign) = &self.assign {
            let role = assign.role.to_uppercase();
            let text = match assign.how {
                How::Set => format!("Choosing the model of {role}: enter picks it, esc goes back"),
                How::Add => format!("Adding a fallback to {role}: enter picks it, esc goes back"),
                How::Replace(_) => format!("Replacing a fallback of {role}: enter picks it, esc goes back"),
            };
            return theme.paint(Role::Accent, &text);
        }
        let sep = theme.glyph(Glyph::Separator);
        let text = match &self.scope {
            Scope::Roles => {
                "Model roles: where each one's model comes from, its fallbacks and its thinking".to_string()
            }
            Scope::Recent => "Models chosen lately".to_string(),
            Scope::All => "Every model you have a credential for".to_string(),
            Scope::Provider(id) => {
                let count = self.provider(id).map_or(0, |p| p.models);
                match (self.fetch.get(id), self.provider(id).is_some_and(|p| p.usable)) {
                    (_, false) => format!("{id}{sep}no credential"),
                    (Some(Fetch::Running), _) => {
                        format!("{id}{sep}asking for its models{}", theme.glyph(Glyph::Ellipsis))
                    }
                    (Some(Fetch::Failed(why)), _) => {
                        format!(
                            "{id}{sep}{count} models from the catalogue; the list could not be asked for: {why}"
                        )
                    }
                    _ => format!("{id}{sep}{count} models"),
                }
            }
        };
        theme.paint(Role::Muted, &text)
    }

    /// The search row: the search symbol and what is typed, and how many rows of the scope
    /// match on the right. Also the column of the typing cursor.
    fn query_row(&self, width: usize, theme: &Theme) -> (Line, usize) {
        let icon = theme.glyphs().prefix(Glyph::Search);
        let (shown, total) = match &self.scope {
            Scope::Roles => (
                self.lines
                    .iter()
                    .filter(|line| matches!(line, RoleLine::Role(_)))
                    .count(),
                self.data.roles.roles.len(),
            ),
            _ => (
                self.items
                    .iter()
                    .filter(|item| matches!(item, Item::Model(_)))
                    .count(),
                self.scope_total(),
            ),
        };
        let count = format!("{shown}/{total}");
        let typed = text::sanitize(&self.query);
        let lead = text::width(&icon);
        let room = width.saturating_sub(lead + text::width(&count) + 2);
        let mut typed_shown = typed.as_str();
        while text::width(typed_shown) > room {
            let mut chars = typed_shown.chars();
            chars.next();
            typed_shown = chars.as_str();
        }
        let column = (lead + text::width(typed_shown)).min(width.saturating_sub(1));
        let left = format!("{}{typed_shown}", theme.paint(Role::Accent, &icon));
        let pad = width.saturating_sub(lead + text::width(typed_shown) + text::width(&count));
        let line = format!("{left}{}{}", " ".repeat(pad), theme.paint(Role::Muted, &count));
        (text::truncate(&line, width, ""), column)
    }

    fn scope_total(&self) -> usize {
        match &self.scope {
            Scope::Roles => self.data.roles.roles.len(),
            Scope::Recent => self.data.recent.len(),
            Scope::All => self.data.rows.iter().filter(|row| row.usable).count(),
            Scope::Provider(id) => self.data.rows.iter().filter(|row| row.provider == *id).count(),
        }
    }

    /// The body: the Roles view, a locked provider's page, or the model list with the detail
    /// of the selected model under it. Also where its scrollbar goes.
    fn body(
        &self,
        width: usize,
        rows: usize,
        theme: &Theme,
        placed: &mut Placed,
    ) -> (Vec<Line>, Option<(usize, Scroll)>) {
        if self.scope == Scope::Roles {
            return self.roles_rows(width, rows, theme, placed);
        }
        if let Some(provider) = self.locked() {
            return (self.locked_rows(provider, width, rows, theme, placed), None);
        }
        let selected = self.focus == Focus::Body || self.narrow.get();
        let detail_rows = if rows >= 6 { 3 } else { 0 };
        let list_rows = rows - detail_rows;
        placed.list = (2, list_rows);
        let models = self.items.iter().filter_map(|item| match item {
            Item::Model(at) => self.data.rows.get(*at),
            Item::Rule => None,
        });
        let columns = list::Columns::of(models, width);
        let total = self.items.len();
        if total == 0 {
            let mut lines = vec![theme.paint(Role::Muted, &self.empty_reason())];
            lines.resize(rows, String::new());
            return (lines, None);
        }
        let visible = list_rows.min(total);
        let mut top = self.top.get().min(total - visible);
        if self.cursor < top {
            top = self.cursor;
        } else if self.cursor >= top + visible {
            top = self.cursor + 1 - visible;
        }
        self.top.set(top);
        self.shown.set(visible);
        let current = self.data.current.as_deref().map(list::bare);
        let mut lines: Vec<Line> = self.items[top..top + visible]
            .iter()
            .enumerate()
            .map(|(offset, item)| match item {
                Item::Model(at) => {
                    let model = &self.data.rows[*at];
                    let tags = self.tags(&model.model);
                    let marks = list::Marks {
                        current: current.as_deref() == Some(model.model.as_str()),
                        tags: &tags,
                        selected: selected && top + offset == self.cursor,
                        dim: !model.usable,
                    };
                    list::row(model, &marks, &columns, width, theme)
                }
                Item::Rule => {
                    let rule = theme
                        .glyph(Glyph::ProgressEmpty)
                        .repeat(width.saturating_sub(list::RAIL));
                    format!(
                        "{}{}",
                        " ".repeat(list::RAIL),
                        theme.paint(Role::BorderMuted, &rule)
                    )
                }
            })
            .collect();
        lines.resize(list_rows, String::new());
        if detail_rows > 0 {
            lines.push(String::new());
            match self.selected_model() {
                Some(model) => lines.extend(list::detail(model, &self.used_by(&model.model), width, theme)),
                None => lines.resize(rows, String::new()),
            }
        }
        let scroll = (total > visible).then_some((
            0,
            Scroll {
                offset: top,
                visible,
                total,
            },
        ));
        (lines, scroll)
    }

    /// Why the list is empty.
    fn empty_reason(&self) -> String {
        if !self.query.trim().is_empty() {
            return format!("no model matches `{}`", text::sanitize(self.query.trim()));
        }
        match &self.scope {
            Scope::Recent => "no model was chosen lately".to_string(),
            Scope::All => {
                "no provider has a credential: pick one without a credential in the sidebar to add one"
                    .to_string()
            }
            Scope::Provider(id) => match self.fetch.get(id) {
                Some(Fetch::Running) => {
                    format!("asking {id} for its models{}", self.glyphs.get(Glyph::Ellipsis))
                }
                Some(Fetch::Failed(why)) => format!("{id} could not list its models: {why}"),
                Some(Fetch::Done) => format!("{id} lists no models"),
                None => format!("{id} has not been asked for its models yet (f5 asks now)"),
            },
            Scope::Roles => String::new(),
        }
    }

    /// The page of a provider without a credential: what to set, what can be done here, then
    /// the models its catalogue entry has, dimmed.
    fn locked_rows(
        &self,
        provider: &ProviderRow,
        width: usize,
        rows: usize,
        theme: &Theme,
        placed: &mut Placed,
    ) -> Vec<Line> {
        let mut lines = vec![theme.paint(
            Role::Warning,
            &format!("{} has no credential", text::sanitize(&provider.name)),
        )];
        let how = match (provider.env.is_empty(), provider.oauth) {
            (false, true) => format!(
                "Set {} in the environment, sign in, or add a key here.",
                provider.env.join(" or ")
            ),
            (false, false) => format!(
                "Set {} in the environment, or add a key here.",
                provider.env.join(" or ")
            ),
            (true, true) => "Sign in to use it.".to_string(),
            (true, false) => "Add a key for it in the config.".to_string(),
        };
        lines.extend(
            text::wrap(&how, width)
                .into_iter()
                .map(|line| theme.paint(Role::Muted, &line)),
        );
        lines.push(String::new());
        let actions = locked_actions(provider);
        placed.list = (2 + lines.len(), actions.len());
        let selected = self.focus == Focus::Body || self.narrow.get();
        for (at, action) in actions.iter().enumerate() {
            let label = format!("{} {}", theme.glyph(Glyph::Cursor), action.label());
            lines.push(list::rail_row(
                &label,
                selected && at == self.action,
                width,
                theme,
            ));
        }
        lines.push(String::new());
        let models: Vec<&ModelRow> = self
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Model(at) => self.data.rows.get(*at),
                Item::Rule => None,
            })
            .collect();
        lines.push(theme.paint(Role::Muted, &format!("{} models in the catalogue:", models.len())));
        let room = rows.saturating_sub(lines.len());
        for (index, model) in models.iter().enumerate().take(room) {
            if index + 1 == room && models.len() > room {
                let more = format!("{} {} more", theme.glyph(Glyph::Ellipsis), models.len() - index);
                lines.push(theme.paint(Role::Muted, &format!("  {more}")));
                break;
            }
            lines.push(theme.paint(Role::Muted, &format!("  {}", text::sanitize(&model.model))));
        }
        lines.truncate(rows);
        lines.resize(rows, String::new());
        lines
            .into_iter()
            .map(|line| text::truncate(&line, width, ""))
            .collect()
    }

    /// The key hints for the bottom edge of the box.
    pub(crate) fn hints(&self, theme: &Theme) -> Vec<Hint> {
        let arrows = format!("{}{}", theme.glyph(Glyph::MoreUp), theme.glyph(Glyph::MoreDown));
        let (right, left) = (theme.glyph(Glyph::Arrow), theme.glyph(Glyph::ArrowBack));
        let leave = match (&self.assign, self.query.is_empty()) {
            (Some(_), _) => "back",
            (None, false) => "clear search",
            (None, true) => "close",
        };
        let mut hints = Vec::new();
        match &self.strip {
            Some(Strip::Name { .. }) => {
                hints.push(Hint::new("enter", "save"));
                hints.push(Hint::new("esc", "cancel"));
                return hints;
            }
            Some(_) => {
                hints.push(Hint::new(format!("{left}{right}"), "choose"));
                hints.push(Hint::new("enter", "apply"));
                hints.push(Hint::new("esc", "back"));
                return hints;
            }
            None => {}
        }
        if self.focus == Focus::Side && !self.narrow.get() {
            hints.push(Hint::new(arrows, "move"));
            hints.push(Hint::new(right, "models"));
            hints.push(Hint::new("type", "to search"));
            hints.push(Hint::new("f5", "refresh"));
        } else if self.scope == Scope::Roles {
            hints.push(Hint::new("enter", "pick"));
            hints.push(Hint::new("t", "thinking"));
            hints.push(Hint::new("f", "fallback"));
            hints.push(Hint::new("x", "clear"));
            hints.push(Hint::new("[ ]", "reorder"));
            hints.push(Hint::new("c", "cycle"));
            hints.push(Hint::new("n", "new"));
            hints.push(Hint::new("s", "save preset"));
            hints.push(Hint::new("l", "load"));
            hints.push(Hint::new("w", "home/project"));
        } else if self.locked().is_some() {
            hints.push(Hint::new(arrows, "choose"));
            hints.push(Hint::new("enter", "run"));
        } else {
            hints.push(Hint::new(arrows, "move"));
            hints.push(Hint::new(
                "enter",
                match self.assign {
                    Some(_) => "pick",
                    None => "roles",
                },
            ));
            hints.push(Hint::new("type", "to search"));
            hints.push(Hint::new("f5", "refresh"));
        }
        if self.narrow.get() {
            hints.push(Hint::new("tab", "next list"));
        } else if self.focus == Focus::Body {
            hints.push(Hint::new(left, "lists"));
        }
        hints.push(Hint::new("esc", leave));
        hints
    }
}

/// What can be done on the page of a provider without a credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    SignIn,
    AddKey,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Action::SignIn => "Sign in (enter)",
            Action::AddKey => "Add API key (enter)",
        }
    }
}

pub(super) fn locked_actions(provider: &ProviderRow) -> Vec<Action> {
    let mut actions = Vec::new();
    if provider.oauth {
        actions.push(Action::SignIn);
    }
    if !provider.env.is_empty() {
        actions.push(Action::AddKey);
    }
    actions
}
