//! The settings screen: every setting on pages behind a row of tabs, a search across all of
//! them, and the editors of a value, all inside one box. It holds no terminal: the UI gives it
//! keys and asks it for rows.

use std::cell::Cell;

use azazel_core::settings::{Kind, NOT_EDITABLE, Page, SETTINGS, Setting};
use azazel_core::{Level, PermissionMode};
use azazel_tui::components::tabs;
use azazel_tui::{Editor, Glyph, Hint, Line, RESET, Role, Scroll, SelectItem, SelectList, Tab, Theme, text};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use toml::Value;

use super::store::{Scope, Store, parse_entry, parse_value};

/// The most rows of settings in view at once.
const LIST_ROWS: usize = 16;
/// Rows under the list: what the selected setting does, in two rows, and where it stands.
const FOOT_ROWS: usize = 3;
/// The widest the column of names gets.
const LABEL_WIDTH: usize = 34;
/// Columns between two columns of a row.
const GAP: usize = 2;
/// Columns before the name: the rail of the selected row and a space.
const RAIL: usize = 2;
/// A value that would get fewer columns than this takes those of the source instead.
const MIN_VALUE: usize = 8;

/// What the screen hands back to the command that opened it: it is closed, or something that
/// takes prompts of its own is to run before it opens again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Request {
    Close,
    Subscription,
    ApiKey,
    SignOut,
    /// Choose the model of this role.
    Role(&'static Setting),
}

/// What the Accounts page shows of the credentials there are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Accounts {
    /// `(provider, where its credential comes from)`.
    pub signed_in: Vec<(String, String)>,
    /// Something is stored in `auth.json` that can be forgotten.
    pub stored: bool,
}

enum Row {
    Heading(String),
    Setting(&'static Setting),
    /// Something that asks questions of its own: signing in, storing a key.
    Action {
        request: Request,
        label: &'static str,
        detail: &'static str,
        /// One sentence about what it does.
        about: &'static str,
    },
    /// A provider that has a credential, and where it comes from.
    Credential {
        provider: String,
        source: String,
    },
    /// A key that only the config file edits, and why.
    Fixed {
        path: &'static str,
        why: &'static str,
    },
}

impl Row {
    fn selectable(&self) -> bool {
        !matches!(self, Row::Heading(_))
    }

    /// What tells the row from every other, to stay on it when the rows are built again.
    fn name(&self) -> &str {
        match self {
            Row::Heading(name) => name,
            Row::Setting(setting) => setting.path,
            Row::Action { label, .. } => label,
            Row::Credential { provider, .. } => provider,
            Row::Fixed { path, .. } => path,
        }
    }
}

/// A value being edited.
enum Edit {
    /// One of a list of values. Moving through them shows each in the running UI when the
    /// setting is one that can change at once.
    Choice {
        setting: &'static Setting,
        options: Vec<String>,
        list: SelectList,
    },
    /// The question before bypass becomes the mode every session starts in; "No" comes first.
    Confirm {
        setting: &'static Setting,
        value: Value,
        list: SelectList,
    },
    /// A number or a text.
    Text {
        setting: &'static Setting,
        editor: Editor,
        error: Option<String>,
    },
    /// The entries of a list or a table. `at` is the entry in view; one past the last is the
    /// row that adds one.
    Entries {
        setting: &'static Setting,
        entries: Vec<String>,
        at: usize,
        /// The entry being typed: a new one, or the one at this index.
        typing: Option<(Option<usize>, Editor)>,
        error: Option<String>,
        scroll: Cell<usize>,
    },
}

impl Edit {
    fn setting(&self) -> &'static Setting {
        match self {
            Edit::Choice { setting, .. }
            | Edit::Confirm { setting, .. }
            | Edit::Text { setting, .. }
            | Edit::Entries { setting, .. } => setting,
        }
    }
}

/// What a row shows, column by column.
struct Cells {
    label: String,
    value: String,
    /// The colour of the value.
    role: Role,
    /// The value is not the default.
    changed: bool,
    /// Where the value comes from.
    note: &'static str,
}

/// The widths of the columns of a list of rows.
struct Columns {
    label: usize,
    marker: bool,
    note: usize,
}

/// The screen as rows, for the box it is drawn in.
pub(crate) struct View {
    pub lines: Vec<Line>,
    /// The typing cursor as `(row, column)` within `lines`.
    pub cursor: Option<(usize, usize)>,
    /// Where the list is when it does not fit, and the row of `lines` it starts at.
    pub scroll: Option<(usize, Scroll)>,
}

/// The rows of the list or of an editor, and what the box needs to know about them.
struct Body {
    lines: Vec<Line>,
    cursor: Option<(usize, usize)>,
    scroll: Option<Scroll>,
}

impl Body {
    fn plain(lines: Vec<Line>) -> Body {
        Body {
            lines,
            cursor: None,
            scroll: None,
        }
    }
}

pub(crate) struct Screen {
    store: Store,
    /// The themes there are to choose from.
    themes: Vec<String>,
    accounts: Accounts,
    /// The page in view, as an index into [`Page::ALL`].
    page: usize,
    /// The row each page was left on.
    cursors: [usize; Page::ALL.len()],
    /// What is searched for across all pages; `None` shows the page.
    search: Option<String>,
    rows: Vec<Row>,
    /// The selected row, as an index into `rows`.
    cursor: usize,
    scroll: Cell<usize>,
    /// Rows of the list the last render showed: the distance of a page movement.
    shown_rows: Cell<usize>,
    edit: Option<Edit>,
    /// What the last change came to, shown in place of the description until the next key.
    notice: Option<(Level, String)>,
    request: Option<Request>,
}

/// Text from a config file as one plain line.
fn one_line(raw: &str) -> String {
    text::sanitize(raw).replace('\n', " ")
}

/// `left` and `right` at the two ends of a row. The right side is left out where both do not
/// fit with a gap between them.
fn spread(left: &str, right: &str, width: usize, ellipsis: &str) -> Line {
    let (left_width, right_width) = (text::width(left), text::width(right));
    if right.is_empty() || left_width + GAP + right_width > width {
        return text::truncate(left, width, ellipsis);
    }
    format!("{left}{}{right}", " ".repeat(width - left_width - right_width))
}

impl Screen {
    pub(crate) fn new(store: Store, themes: Vec<String>) -> Screen {
        let mut screen = Screen {
            store,
            themes,
            accounts: Accounts::default(),
            page: 0,
            cursors: [0; Page::ALL.len()],
            search: None,
            rows: Vec::new(),
            cursor: 0,
            scroll: Cell::new(0),
            shown_rows: Cell::new(LIST_ROWS),
            edit: None,
            notice: None,
            request: None,
        };
        screen.rebuild();
        screen
    }

    pub(crate) fn store(&self) -> &Store {
        &self.store
    }

    /// What the screen asked for when it gave up the keyboard.
    pub(crate) fn take_request(&mut self) -> Request {
        self.request.take().unwrap_or(Request::Close)
    }

    /// Brings the screen up to date with what was done while it was closed: the credentials
    /// there are now, and the values of every layer.
    pub(crate) fn refresh(&mut self, accounts: Accounts) {
        self.accounts = accounts;
        if let Err(why) = self.store.reload() {
            self.notice = Some((Level::Error, why));
        }
        self.rebuild();
    }

    /// Says what something done outside the screen came to.
    pub(crate) fn say(&mut self, level: Level, text: &str) {
        self.notice = Some((level, text.to_string()));
    }

    /// Opens the page a setting is on, with the setting selected.
    pub(crate) fn go_to(&mut self, path: &str) {
        let Some(setting) = SETTINGS.iter().find(|setting| setting.path == path) else {
            return;
        };
        // The row a search stands on is no row of the page it started from.
        if self.search.take().is_none() {
            self.cursors[self.page] = self.cursor;
        }
        self.show_page(
            Page::ALL
                .iter()
                .position(|page| *page == setting.page)
                .unwrap_or(0),
        );
        if let Some(at) = self.rows.iter().position(|row| row.name() == path) {
            self.cursor = at;
        }
    }

    // ------------------------------------------------------------ rows

    fn page(&self) -> Page {
        Page::ALL[self.page.min(Page::ALL.len() - 1)]
    }

    fn page_rows(&self, page: Page) -> Vec<Row> {
        let mut rows = Vec::new();
        if page == Page::Accounts {
            rows.push(Row::Action {
                request: Request::Subscription,
                label: "Sign in with a subscription",
                detail: "Claude, ChatGPT and other accounts",
                about: "Sign in with the account of a provider that has one; what it allows is used \
                        instead of paying by the token.",
            });
            rows.push(Row::Action {
                request: Request::ApiKey,
                label: "Add an API key",
                detail: "stored in auth.json",
                about: "Store the API key of a provider in auth.json; a key in the environment needs \
                        no storing.",
            });
            if self.accounts.stored {
                rows.push(Row::Action {
                    request: Request::SignOut,
                    label: "Sign out or forget a key",
                    detail: "removes it from auth.json",
                    about: "Remove a stored login or key from auth.json.",
                });
            }
            if !self.accounts.signed_in.is_empty() {
                rows.push(Row::Heading("Credentials".to_string()));
                rows.extend(
                    self.accounts
                        .signed_in
                        .iter()
                        .map(|(provider, source)| Row::Credential {
                            provider: provider.clone(),
                            source: source.clone(),
                        }),
                );
            }
        }
        let mut group = "";
        for setting in SETTINGS.iter().filter(|setting| setting.page == page) {
            if setting.group != group {
                group = setting.group;
                // A page with one group named like the page needs no heading.
                if group != page.title() {
                    rows.push(Row::Heading(group.to_string()));
                }
            }
            rows.push(Row::Setting(setting));
        }
        if page == Page::Advanced {
            rows.push(Row::Heading("Only in the config file".to_string()));
            rows.extend(NOT_EDITABLE.iter().map(|(path, why)| Row::Fixed { path, why }));
        }
        rows
    }

    /// Whether a row has every word of what is searched for, in its name, its key, its page or
    /// what it says it does.
    fn matches(&self, page: Page, row: &Row, words: &[String]) -> bool {
        let hay = match row {
            Row::Heading(_) => return false,
            Row::Setting(setting) => format!(
                "{} {} {} {} {} {}",
                page.title(),
                setting.group,
                setting.label,
                setting.path,
                setting.description,
                self.store.shown(setting),
            ),
            Row::Action {
                label, detail, about, ..
            } => format!("{} {label} {detail} {about}", page.title()),
            Row::Credential { provider, source } => format!("{} {provider} {source}", page.title()),
            Row::Fixed { path, why } => format!("{} {path} {why}", page.title()),
        }
        .to_lowercase();
        words.iter().all(|word| hay.contains(word))
    }

    /// The rows of every page that match, under the name of their page.
    fn search_rows(&self, query: &str) -> Vec<Row> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut rows = Vec::new();
        for page in Page::ALL {
            let found: Vec<Row> = self
                .page_rows(page)
                .into_iter()
                .filter(|row| self.matches(page, row, &words))
                .collect();
            if !found.is_empty() {
                rows.push(Row::Heading(page.title().to_string()));
                rows.extend(found);
            }
        }
        rows
    }

    /// Builds the rows again, staying on the row that was selected when it is still there.
    fn rebuild(&mut self) {
        let on = self.rows.get(self.cursor).map(|row| row.name().to_string());
        self.rows = match &self.search {
            Some(query) => self.search_rows(query),
            None => self.page_rows(self.page()),
        };
        let same = on.and_then(|name| {
            self.rows
                .iter()
                .position(|row| row.selectable() && row.name() == name)
        });
        self.cursor = same.unwrap_or(self.cursor);
        self.settle();
    }

    /// Puts the cursor on a row that can be selected: where it is, else the next, else the last
    /// before it.
    fn settle(&mut self) {
        let last = self.rows.len().saturating_sub(1);
        let at = self.cursor.min(last);
        let after = (at..self.rows.len()).find(|index| self.rows[*index].selectable());
        let before = (0..at).rev().find(|index| self.rows[*index].selectable());
        self.cursor = after.or(before).unwrap_or(0);
    }

    /// Leaves the page in view for another, which comes back on the row it was left on.
    fn open_page(&mut self, page: usize) {
        self.cursors[self.page] = self.cursor;
        self.show_page(page);
    }

    /// Brings a page into view on the row it was left on.
    fn show_page(&mut self, page: usize) {
        self.page = page % Page::ALL.len();
        self.rows = self.page_rows(self.page());
        self.cursor = self.cursors[self.page];
        self.scroll.set(0);
        self.settle();
    }

    fn selected(&self) -> Option<&Row> {
        self.rows.get(self.cursor).filter(|row| row.selectable())
    }

    /// Moves the selection by one row that can be selected, around the ends.
    fn step(&mut self, forward: bool) {
        let count = self.rows.len();
        if count == 0 {
            return;
        }
        let mut at = self.cursor;
        for _ in 0..count {
            at = match forward {
                true => (at + 1) % count,
                false => (at + count - 1) % count,
            };
            if self.rows[at].selectable() {
                self.cursor = at;
                return;
            }
        }
    }

    /// Moves the selection by a page, stopping at the ends.
    fn leap(&mut self, forward: bool) {
        let distance = self.shown_rows.get().max(1);
        let last = self.rows.len().saturating_sub(1);
        self.cursor = match forward {
            true => (self.cursor + distance).min(last),
            false => self.cursor.saturating_sub(distance),
        };
        // A heading is stepped over: the last row of a page is never one.
        self.settle();
    }

    /// How many settings of a page are not on their default.
    fn changed_on(&self, page: Page) -> usize {
        SETTINGS
            .iter()
            .filter(|setting| setting.page == page && self.store.changed(setting))
            .count()
    }

    // ------------------------------------------------------------ keys

    /// Takes a key. True when the screen gives up the keyboard: it is closed, or a sign-in or
    /// a model picker is to run; [`Screen::take_request`] says which.
    pub(crate) fn on_key(&mut self, key: &KeyEvent) -> bool {
        self.notice = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.cancel_edit();
            return self.hand_over(Request::Close);
        }
        if let Some(edit) = self.edit.take() {
            self.edit = self.edit_key(edit, key);
            return false;
        }
        if self.search.is_some() {
            return self.search_key(key);
        }
        match key.code {
            KeyCode::Esc => return self.hand_over(Request::Close),
            KeyCode::Left | KeyCode::BackTab => self.open_page(self.page + Page::ALL.len() - 1),
            KeyCode::Right | KeyCode::Tab => self.open_page(self.page + 1),
            KeyCode::Up => self.step(false),
            KeyCode::Down => self.step(true),
            KeyCode::PageUp => self.leap(false),
            KeyCode::PageDown => self.leap(true),
            KeyCode::Home => {
                self.cursor = 0;
                self.settle();
            }
            KeyCode::End => {
                self.cursor = self.rows.len().saturating_sub(1);
                self.settle();
            }
            KeyCode::Enter | KeyCode::Char(' ') => return self.activate(),
            KeyCode::Char('r') if !ctrl => self.reset(),
            KeyCode::Char('p') if !ctrl => self.switch_scope(),
            KeyCode::Char('/') => {
                self.cursors[self.page] = self.cursor;
                self.search = Some(String::new());
                self.cursor = 0;
                self.scroll.set(0);
                self.rebuild();
            }
            _ => {}
        }
        false
    }

    fn hand_over(&mut self, request: Request) -> bool {
        self.request = Some(request);
        true
    }

    fn search_key(&mut self, key: &KeyEvent) -> bool {
        let typed = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => self.leave_search(),
            KeyCode::Up => self.step(false),
            KeyCode::Down | KeyCode::Tab => self.step(true),
            KeyCode::PageUp => self.leap(false),
            KeyCode::PageDown => self.leap(true),
            KeyCode::Enter => return self.activate(),
            KeyCode::Backspace => match self.search.as_mut().map(|query| query.pop()) {
                Some(Some(_)) => self.searched(),
                _ => self.leave_search(),
            },
            KeyCode::Char(c) if typed => {
                if let Some(query) = &mut self.search {
                    query.push(c);
                }
                self.searched();
            }
            _ => {}
        }
        false
    }

    /// The search text changed: the best place to be is the first match.
    fn searched(&mut self) {
        self.cursor = 0;
        self.scroll.set(0);
        self.rows = self.search_rows(self.search.as_deref().unwrap_or_default());
        self.settle();
    }

    /// Ends the search on the page of the setting it found, with that setting selected; with
    /// nothing found, on the page it started from.
    fn leave_search(&mut self) {
        let found = match self.selected() {
            Some(Row::Setting(setting)) => Some(setting.path),
            _ => None,
        };
        match found {
            Some(path) => self.go_to(path),
            None => {
                self.search = None;
                self.show_page(self.page);
            }
        }
    }

    pub(crate) fn on_paste(&mut self, pasted: &str) {
        let line = pasted.lines().next().unwrap_or_default();
        match &mut self.edit {
            Some(Edit::Text { editor, .. }) => editor.insert_str(line),
            Some(Edit::Entries {
                typing: Some((_, editor)),
                ..
            }) => editor.insert_str(line),
            Some(_) => {}
            None => {
                if let Some(query) = &mut self.search {
                    query.push_str(line);
                    self.searched();
                }
            }
        }
    }

    /// Enter on the selected row: flips a switch, opens the editor of a value, or hands over to
    /// what a row stands for.
    fn activate(&mut self) -> bool {
        let Some(row) = self.selected() else {
            return false;
        };
        match row {
            Row::Heading(_) => {}
            Row::Action { request, .. } => {
                let request = *request;
                return self.hand_over(request);
            }
            Row::Credential { provider, source } => {
                let line = format!("{provider} is used with {source}; `Sign out or forget a key` removes it");
                self.notice = Some((Level::Info, line));
            }
            Row::Fixed { path, .. } => {
                let line = format!("[{path}] is edited in {}", self.store.target().display());
                self.notice = Some((Level::Info, line));
            }
            Row::Setting(setting) => {
                let setting: &'static Setting = setting;
                match setting.kind {
                    Kind::Role => return self.hand_over(Request::Role(setting)),
                    Kind::Switch => {
                        let on = self.store.value(setting.path) == Some(&Value::Boolean(true));
                        self.save(setting, Value::Boolean(!on));
                    }
                    Kind::Choice(options) => {
                        let options = options.iter().map(ToString::to_string).collect();
                        self.edit = Some(self.choice(setting, options));
                    }
                    Kind::Theme => self.edit = Some(self.choice(setting, self.themes.clone())),
                    Kind::Number | Kind::Text => {
                        let mut editor = Editor::new();
                        match self.store.value(setting.path) {
                            Some(Value::String(text)) => editor.set_text(text),
                            Some(other) => editor.set_text(&other.to_string()),
                            None => {}
                        }
                        self.edit = Some(Edit::Text {
                            setting,
                            editor,
                            error: None,
                        });
                    }
                    Kind::List | Kind::Table => {
                        self.edit = Some(Edit::Entries {
                            setting,
                            entries: self.store.entries(setting),
                            at: 0,
                            typing: None,
                            error: None,
                            scroll: Cell::new(0),
                        });
                    }
                }
            }
        }
        false
    }

    /// The picker of a setting with a fixed set of values, starting on the one in effect.
    fn choice(&self, setting: &'static Setting, options: Vec<String>) -> Edit {
        let now = self.store.value(setting.path).and_then(Value::as_str);
        let default = setting.default_value();
        let default = default.as_ref().and_then(Value::as_str);
        let items = options
            .iter()
            .map(|option| {
                let detail = match (Some(option.as_str()) == now, Some(option.as_str()) == default) {
                    (true, true) => "in use, the default",
                    (true, false) => "in use",
                    (false, true) => "the default",
                    (false, false) => "",
                };
                SelectItem::new(option.as_str(), detail)
            })
            .collect();
        let mut list = SelectList::new(items);
        if let Some(at) = options.iter().position(|option| Some(option.as_str()) == now) {
            list.set_cursor(at);
        }
        Edit::Choice {
            setting,
            options,
            list,
        }
    }

    /// Saves a value and says what came of it.
    fn save(&mut self, setting: &'static Setting, value: Value) -> bool {
        let saved = self.store.save(setting, value);
        let ok = saved.is_ok();
        self.notice = Some(match saved {
            Ok(line) => (Level::Info, line),
            Err(why) => (Level::Error, why),
        });
        self.rebuild();
        ok
    }

    fn reset(&mut self) {
        let Some(Row::Setting(setting)) = self.selected() else {
            return;
        };
        let setting: &'static Setting = setting;
        self.notice = Some(match self.store.reset(setting) {
            Ok(line) => (Level::Info, line),
            Err(why) => (Level::Warn, why),
        });
        self.rebuild();
    }

    fn switch_scope(&mut self) {
        self.notice = Some(match self.store.toggle_scope() {
            Ok(_) => (
                Level::Info,
                format!("changes are saved in {}", self.store.target().display()),
            ),
            Err(why) => (Level::Warn, why),
        });
    }

    /// The running UI shows the value in effect again, after a choice was tried on it.
    fn unpreview(&self, setting: &Setting) {
        if let Some(value) = self.store.value(setting.path) {
            self.store.preview(setting, value);
        }
    }

    /// Leaves an editor without saving.
    fn cancel_edit(&mut self) {
        if let Some(Edit::Choice { setting, .. }) = self.edit.take() {
            self.unpreview(setting);
        }
    }

    /// A key for the editor that is open. Returns the editor, or `None` when it is done.
    fn edit_key(&mut self, mut edit: Edit, key: &KeyEvent) -> Option<Edit> {
        match &mut edit {
            Edit::Choice {
                setting,
                options,
                list,
            } => {
                let setting: &'static Setting = setting;
                let before = list.selected();
                match key.code {
                    KeyCode::Esc => {
                        self.unpreview(setting);
                        return None;
                    }
                    KeyCode::Up => list.move_up(),
                    KeyCode::Down | KeyCode::Tab => list.move_down(),
                    KeyCode::PageUp => list.page_up(),
                    KeyCode::PageDown => list.page_down(),
                    KeyCode::Enter => {
                        let chosen = list.selected().and_then(|index| options.get(index))?;
                        let value = Value::String(chosen.clone());
                        // Bypass as the mode every session starts in is confirmed, with "No"
                        // first.
                        if setting.path == "permissions.mode" && chosen == PermissionMode::Bypass.as_str() {
                            let list = SelectList::new(vec![
                                SelectItem::new("No", ""),
                                SelectItem::new("Yes, bypass permissions", "in every session"),
                            ]);
                            return Some(Edit::Confirm { setting, value, list });
                        }
                        self.save(setting, value);
                        // What was tried on the way may not be what applies now.
                        self.unpreview(setting);
                        return None;
                    }
                    _ => {}
                }
                if list.selected() != before
                    && let Some(option) = list.selected().and_then(|index| options.get(index))
                {
                    self.store.preview(setting, &Value::String(option.clone()));
                }
            }
            Edit::Confirm { setting, value, list } => match key.code {
                KeyCode::Esc => return None,
                KeyCode::Up => list.move_up(),
                KeyCode::Down | KeyCode::Tab => list.move_down(),
                KeyCode::Enter => {
                    if list.selected() == Some(1) {
                        self.save(setting, value.clone());
                    }
                    return None;
                }
                _ => {}
            },
            Edit::Text {
                setting,
                editor,
                error,
            } => match key.code {
                KeyCode::Esc => return None,
                KeyCode::Enter => {
                    let default = setting.default_value();
                    let saved = parse_value(setting.kind, editor.text(), default.as_ref())
                        .map_err(|why| format!("{}: {why}", setting.label))
                        .and_then(|value| self.store.save(setting, value));
                    match saved {
                        Ok(line) => {
                            self.notice = Some((Level::Info, line));
                            self.rebuild();
                            return None;
                        }
                        Err(why) => *error = Some(why),
                    }
                }
                _ => {
                    *error = None;
                    editor.handle_key(key);
                }
            },
            Edit::Entries { .. } => self.entries_key(&mut edit, key)?,
        }
        Some(edit)
    }

    /// A key for the editor of a list or a table. `None` closes it.
    fn entries_key(&mut self, edit: &mut Edit, key: &KeyEvent) -> Option<()> {
        let Edit::Entries {
            setting,
            entries,
            at,
            typing,
            error,
            ..
        } = edit
        else {
            return Some(());
        };
        let setting: &'static Setting = setting;
        let ordered = setting.kind == Kind::List;
        if let Some((index, editor)) = typing {
            match key.code {
                KeyCode::Esc => {
                    *typing = None;
                    *error = None;
                }
                KeyCode::Enter => {
                    let typed = editor.text().trim().to_string();
                    let done = match (typed.is_empty(), *index) {
                        // Nothing typed for a new entry adds nothing; for an old one it
                        // removes it.
                        (true, None) => Ok(None),
                        (true, Some(index)) => self.remove_entry(setting, entries, index).map(Some),
                        (false, index) => self.put_entry(setting, entries, index, &typed).map(Some),
                    };
                    match done {
                        Ok(line) => {
                            if let Some(line) = line {
                                self.notice = Some((Level::Info, line));
                            }
                            let added = index.is_none();
                            *entries = self.store.entries(setting);
                            *typing = None;
                            *error = None;
                            if added {
                                *at = entries.len();
                            }
                            *at = (*at).min(entries.len());
                            self.rebuild();
                        }
                        Err(why) => *error = Some(why),
                    }
                }
                _ => {
                    *error = None;
                    editor.handle_key(key);
                }
            }
            return Some(());
        }
        *error = None;
        // One past the last entry is the row that adds one.
        let rows = entries.len() + 1;
        match key.code {
            KeyCode::Esc => return None,
            KeyCode::Up => *at = (*at + rows - 1) % rows,
            KeyCode::Down | KeyCode::Tab => *at = (*at + 1) % rows,
            KeyCode::Home => *at = 0,
            KeyCode::End => *at = rows - 1,
            KeyCode::Char('a') => {
                *at = entries.len();
                *typing = Some((None, Editor::new()));
            }
            KeyCode::Enter => {
                let mut editor = Editor::new();
                let index = (*at < entries.len()).then_some(*at);
                if let Some(index) = index {
                    editor.set_text(&entries[index]);
                }
                *typing = Some((index, editor));
            }
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace if *at < entries.len() => {
                match self.remove_entry(setting, entries, *at) {
                    Ok(line) => {
                        self.notice = Some((Level::Info, line));
                        *entries = self.store.entries(setting);
                        *at = (*at).min(entries.len());
                        self.rebuild();
                    }
                    Err(why) => *error = Some(why),
                }
            }
            KeyCode::Char(c @ ('[' | ']')) if ordered && *at < entries.len() => {
                let to = match c {
                    '[' => at.checked_sub(1),
                    _ => Some(*at + 1).filter(|to| *to < entries.len()),
                };
                if let Some(to) = to {
                    let mut moved = entries.clone();
                    moved.swap(*at, to);
                    match self.save_list(setting, &moved) {
                        Ok(line) => {
                            self.notice = Some((Level::Info, line));
                            *entries = self.store.entries(setting);
                            *at = to.min(entries.len());
                            self.rebuild();
                        }
                        Err(why) => *error = Some(why),
                    }
                }
            }
            _ => {}
        }
        Some(())
    }

    fn save_list(&mut self, setting: &'static Setting, items: &[String]) -> Result<String, String> {
        let value = Value::Array(items.iter().map(|item| Value::String(item.clone())).collect());
        self.store.save(setting, value)
    }

    /// Adds what was typed as an entry, or puts it in place of the entry at `index`.
    fn put_entry(
        &mut self,
        setting: &'static Setting,
        entries: &[String],
        index: Option<usize>,
        typed: &str,
    ) -> Result<String, String> {
        if setting.kind == Kind::List {
            let mut items = entries.to_vec();
            match index {
                Some(index) => items[index] = typed.to_string(),
                None => items.push(typed.to_string()),
            }
            return self.save_list(setting, &items);
        }
        let fail = |why: String| format!("{}: {why}", setting.label);
        let (key, value) = parse_entry(typed).map_err(fail)?;
        let line = self.store.set_entry(setting, &key, value)?;
        // An entry typed over with another name moves: the old name goes.
        let old = index
            .and_then(|index| entries.get(index))
            .and_then(|entry| parse_entry(entry).ok())
            .map(|(old, _)| old)
            .filter(|old| *old != key);
        if let Some(old) = old {
            self.store.remove_entry(setting, &old)?;
        }
        Ok(line)
    }

    fn remove_entry(
        &mut self,
        setting: &'static Setting,
        entries: &[String],
        index: usize,
    ) -> Result<String, String> {
        if setting.kind == Kind::List {
            let mut items = entries.to_vec();
            items.remove(index);
            return self.save_list(setting, &items);
        }
        let fail = |why: String| format!("{}: {why}", setting.label);
        let (key, _) = parse_entry(&entries[index]).map_err(fail)?;
        self.store.remove_entry(setting, &key)
    }

    // ------------------------------------------------------------ drawing

    /// The key hints for the bottom edge of the box, for whatever has the keys now.
    pub(crate) fn hints(&self, theme: &Theme) -> Vec<Hint> {
        let arrows = format!("{}{}", theme.glyph(Glyph::MoreUp), theme.glyph(Glyph::MoreDown));
        match &self.edit {
            Some(Edit::Choice { .. }) | Some(Edit::Confirm { .. }) => vec![
                Hint::new(arrows, "move"),
                Hint::new("enter", "save"),
                Hint::new("esc", "cancel"),
            ],
            Some(Edit::Text { .. }) => vec![
                Hint::new("enter", "save"),
                Hint::new("", "empty resets"),
                Hint::new("esc", "cancel"),
            ],
            Some(Edit::Entries { typing: Some(_), .. }) => {
                vec![Hint::new("enter", "save"), Hint::new("esc", "back")]
            }
            Some(Edit::Entries { setting, .. }) => {
                let mut hints = vec![
                    Hint::new(arrows, "move"),
                    Hint::new("enter", "edit"),
                    Hint::new("a", "add"),
                    Hint::new("d", "remove"),
                ];
                if setting.kind == Kind::List {
                    hints.push(Hint::new("[ ]", "reorder"));
                }
                hints.push(Hint::new("esc", "back"));
                hints
            }
            None if self.search.is_some() => vec![
                Hint::new("type", "to search"),
                Hint::new(arrows, "move"),
                Hint::new("enter", "edit"),
                Hint::new("esc", "to its page"),
            ],
            None => {
                let mut hints = vec![
                    Hint::new("tab", "page"),
                    Hint::new(arrows, "move"),
                    Hint::new("enter", "edit"),
                    Hint::new("/", "search"),
                    Hint::new("r", "reset"),
                ];
                if self.store.can_project() {
                    let other = match self.store.scope() {
                        Scope::Home => "to project",
                        Scope::Project => "to home",
                    };
                    hints.push(Hint::new("p", other));
                }
                hints.push(Hint::new("esc", "close"));
                hints
            }
        }
    }

    /// The screen in at most `room` rows of `width` columns: the tabs, a row that says where one
    /// is, the settings or the editor of one, and what the selected setting does. It takes the
    /// same rows on every page, so the box does not change size between them.
    pub(crate) fn view(&self, width: usize, room: usize, prompt: &str, theme: &Theme) -> View {
        let total = room.clamp(1, LIST_ROWS + FOOT_ROWS + 4);
        // What there is no room for goes in this order: the blank rows, two of the three rows
        // under the list, the row under the tabs, the tabs.
        let gaps = total >= 14;
        let foot = match total {
            0..=5 => 0,
            6..=11 => 1,
            _ => FOOT_ROWS,
        };
        let lead = total >= 4;
        let tabs = total >= 2;
        let fixed = usize::from(tabs) + usize::from(lead) + 2 * usize::from(gaps) + foot;
        let list_rows = total.saturating_sub(fixed).max(1);

        let mut lines: Vec<Line> = Vec::with_capacity(total);
        let mut cursor = None;
        if tabs {
            lines.push(self.tabs_row(width, theme));
        }
        if lead {
            let (line, column) = self.lead_row(width, theme);
            cursor = column.map(|column| (lines.len(), column));
            lines.push(line);
        }
        if gaps {
            lines.push(String::new());
        }
        let first = lines.len();
        let body = match &self.edit {
            Some(edit) => self.editor(edit, width, list_rows, prompt, theme),
            None => self.list(width, list_rows, theme),
        };
        cursor = body.cursor.map(|(row, column)| (first + row, column)).or(cursor);
        let scroll = body.scroll.map(|scroll| (first, scroll));
        lines.extend(body.lines.into_iter().take(list_rows));
        lines.resize(first + list_rows, String::new());
        if gaps {
            lines.push(String::new());
        }
        lines.extend(self.foot(width, foot, theme));
        View {
            lines,
            cursor,
            scroll,
        }
    }

    fn tabs_row(&self, width: usize, theme: &Theme) -> Line {
        let searching = self.search.is_some();
        let tabs: Vec<Tab> = Page::ALL
            .iter()
            .map(|page| {
                let count = match searching {
                    true => self.found_on(*page),
                    false => self.changed_on(*page),
                };
                let badge = match count {
                    0 => String::new(),
                    count => count.to_string(),
                };
                Tab::new(page.title(), badge)
            })
            .collect();
        tabs::row(&tabs, self.tab_in_view(), width, theme)
    }

    /// The page the selection is on: the page in view, or while searching the page of the
    /// row that was found.
    fn tab_in_view(&self) -> usize {
        if self.search.is_none() {
            return self.page;
        }
        let title = self.rows[..self.cursor.min(self.rows.len())]
            .iter()
            .rev()
            .find_map(|row| match row {
                Row::Heading(title) => Some(title.as_str()),
                _ => None,
            });
        Page::ALL
            .iter()
            .position(|page| Some(page.title()) == title)
            .unwrap_or(self.page)
    }

    /// How many rows of the search are on a page.
    fn found_on(&self, page: Page) -> usize {
        let mut on = false;
        let mut count = 0;
        for row in &self.rows {
            match row {
                Row::Heading(title) => on = title == page.title(),
                _ if on => count += 1,
                _ => {}
            }
        }
        count
    }

    /// The row under the tabs: what is searched for, where an editor is, or what the page
    /// holds and where changes go. Also the column of the typing cursor while searching.
    fn lead_row(&self, width: usize, theme: &Theme) -> (Line, Option<usize>) {
        let ellipsis = theme.glyph(Glyph::Ellipsis);
        if let Some(edit) = &self.edit {
            let setting = edit.setting();
            let crumb = format!(
                "{} {} {}",
                theme.paint(Role::Muted, setting.page.title()),
                theme.paint(Role::Muted, theme.glyph(Glyph::Cursor)),
                theme.paint(Role::Accent, setting.label),
            );
            let right = match edit {
                Edit::Entries { .. } if Store::joined(setting.path) => theme.paint(
                    Role::Muted,
                    &format!("the {} config's own", self.store.scope().word()),
                ),
                _ => String::new(),
            };
            return (spread(&crumb, &right, width, ellipsis), None);
        }
        if let Some(query) = &self.search {
            let icon = theme.glyphs().prefix(Glyph::Search);
            let found = self.rows.iter().filter(|row| row.selectable()).count();
            let count = match found {
                1 => "1 match".to_string(),
                found => format!("{found} matches"),
            };
            let lead = text::width(&icon);
            let room = width.saturating_sub(lead + text::width(&count) + GAP + 1);
            let typed = one_line(query);
            // A long text shows its end.
            let mut shown = typed.as_str();
            while text::width(shown) > room {
                let mut chars = shown.chars();
                chars.next();
                shown = chars.as_str();
            }
            let left = format!("{}{shown}", theme.paint(Role::Accent, &icon));
            let column = (lead + text::width(shown)).min(width.saturating_sub(1));
            let line = spread(&left, &theme.paint(Role::Muted, &count), width, ellipsis);
            return (line, Some(column));
        }
        let scope = match self.store.scope() {
            Scope::Home => theme.paint(Role::Muted, "saves to the home config"),
            Scope::Project => theme.paint(Role::Warning, "saves to the project config"),
        };
        let lead = theme.paint(Role::Muted, self.page().lead());
        // The file changes go to matters more than the sentence about the page.
        let room = width.saturating_sub(text::width(&scope) + GAP);
        let lead = match room >= 12 {
            true => text::truncate(&lead, room, ellipsis),
            false => lead,
        };
        (spread(&lead, &scope, width, ellipsis), None)
    }

    fn cells(&self, row: &Row) -> Cells {
        match row {
            Row::Heading(name) => Cells {
                label: name.clone(),
                value: String::new(),
                role: Role::Muted,
                changed: false,
                note: "",
            },
            Row::Setting(setting) => {
                let changed = self.store.changed(setting);
                Cells {
                    label: setting.label.to_string(),
                    value: self.store.shown(setting),
                    role: match changed {
                        true => Role::Accent,
                        false => Role::Muted,
                    },
                    changed,
                    note: self
                        .store
                        .sources(setting.path)
                        .first()
                        .map_or("", |source| source.word()),
                }
            }
            Row::Action { label, detail, .. } => Cells {
                label: label.to_string(),
                value: detail.to_string(),
                role: Role::Muted,
                changed: false,
                note: "",
            },
            Row::Credential { provider, source } => Cells {
                label: provider.clone(),
                value: source.clone(),
                role: Role::Muted,
                changed: false,
                note: "",
            },
            Row::Fixed { path, .. } => Cells {
                label: format!("[{path}]"),
                value: match self.store.value(path) {
                    Some(Value::Table(entries)) if !entries.is_empty() => {
                        format!("{} set", entries.len())
                    }
                    Some(Value::Array(items)) if !items.is_empty() => format!("{} set", items.len()),
                    _ => "(none)".to_string(),
                },
                role: Role::Muted,
                changed: false,
                note: self
                    .store
                    .sources(path)
                    .first()
                    .map_or("", |source| source.word()),
            },
        }
    }

    /// One row of a list. The selected one is a band across the width with the rail before it.
    fn row_line(cells: &Cells, selected: bool, columns: &Columns, width: usize, theme: &Theme) -> Line {
        let ellipsis = theme.glyph(Glyph::Ellipsis);
        // The last column stays empty: a scrollbar may stand right after it.
        let room = width.saturating_sub(RAIL + 1);
        let label_width = columns.label.min(room);
        let marker_width = if columns.marker { 2 } else { 0 };
        let mut note_width = columns.note;
        let rest = |note: usize| {
            let note = if note > 0 { note + GAP } else { 0 };
            room.saturating_sub(label_width + GAP + marker_width + note)
        };
        if rest(note_width) < MIN_VALUE {
            note_width = 0;
        }
        let value_width = rest(note_width);
        let label = text::pad_right(
            &text::truncate(&one_line(&cells.label), label_width, ellipsis),
            label_width,
        );
        let value = text::strip_ansi(&text::truncate(&one_line(&cells.value), value_width, ellipsis));
        let value = text::pad_right(&value, value_width);
        let marker = match (columns.marker, cells.changed) {
            (false, _) => String::new(),
            (true, true) => format!(" {}", theme.glyph(Glyph::Bullet)),
            (true, false) => "  ".to_string(),
        };
        let note = match note_width {
            0 => String::new(),
            width => format!(
                "{}{}{}",
                " ".repeat(GAP),
                " ".repeat(width.saturating_sub(text::width(cells.note))),
                cells.note
            ),
        };
        let tail = format!(
            "{}{}{value}{}{marker}{}{note}",
            " ".repeat(GAP),
            theme.fg(cells.role),
            theme.fg(Role::Accent),
            theme.fg(Role::Muted),
        );
        if selected {
            // One band across the row: nothing in it may reset the background.
            let line = format!(
                "{}{}{} {}\x1b[1m{label}\x1b[22m{tail}",
                theme.bg(Role::SelectedBg),
                theme.fg(Role::Accent),
                theme.glyph(Glyph::Rail),
                theme.fg(Role::Text),
            );
            let cut = text::truncate(&line, width, "");
            let open = cut.strip_suffix(RESET).unwrap_or(&cut);
            format!("{}{RESET}", text::pad_right(open, width))
        } else {
            let line = format!("{}{label}{tail}{RESET}", " ".repeat(RAIL));
            text::truncate(&line, width, "")
        }
    }

    fn heading_line(name: &str, width: usize, theme: &Theme) -> Line {
        let line = format!(
            "\x1b[1m{}{}\x1b[22m{RESET}",
            theme.fg(Role::Muted),
            one_line(name)
        );
        text::truncate(&line, width, theme.glyph(Glyph::Ellipsis))
    }

    /// The rows of the page or of the search, as many as fit, scrolled to keep the selection in
    /// view.
    fn list(&self, width: usize, rows: usize, theme: &Theme) -> Body {
        let total = self.rows.len();
        if total == 0 {
            let none = match self.search {
                Some(_) => "  no setting matches",
                None => "  nothing here",
            };
            return Body::plain(vec![text::truncate(&theme.paint(Role::Muted, none), width, "")]);
        }
        let visible = rows.min(total);
        let mut top = self.scroll.get().min(total - visible);
        if self.cursor < top {
            top = self.cursor;
        } else if self.cursor >= top + visible {
            top = self.cursor + 1 - visible;
        }
        // The heading of the first row in view comes with it.
        if top > 0 && top == self.cursor && visible >= 2 && !self.rows[top - 1].selectable() {
            top -= 1;
        }
        self.scroll.set(top);
        self.shown_rows.set(visible);

        let cells: Vec<Cells> = self.rows.iter().map(|row| self.cells(row)).collect();
        let listed = || {
            self.rows
                .iter()
                .zip(&cells)
                .filter(|(row, _)| row.selectable())
                .map(|(_, cells)| cells)
        };
        let columns = Columns {
            label: listed()
                .map(|cells| text::width(&cells.label))
                .max()
                .unwrap_or(0)
                .min(LABEL_WIDTH)
                .min(width.saturating_sub(RAIL) / 2),
            marker: listed().any(|cells| cells.changed),
            note: listed().map(|cells| text::width(cells.note)).max().unwrap_or(0),
        };
        let lines = self
            .rows
            .iter()
            .zip(&cells)
            .enumerate()
            .skip(top)
            .take(visible)
            .map(|(index, (row, cells))| match row {
                Row::Heading(name) => Screen::heading_line(name, width, theme),
                _ => Screen::row_line(cells, index == self.cursor, &columns, width, theme),
            })
            .collect();
        Body {
            lines,
            cursor: None,
            scroll: (total > visible).then_some(Scroll {
                offset: top,
                visible,
                total,
            }),
        }
    }

    /// The editor of a value in the rows of the list.
    fn editor(&self, edit: &Edit, width: usize, rows: usize, prompt: &str, theme: &Theme) -> Body {
        let ellipsis = theme.glyph(Glyph::Ellipsis);
        match edit {
            Edit::Choice { list, .. } => {
                let view = list.view(width, rows, theme);
                Body {
                    lines: view.lines,
                    cursor: None,
                    scroll: view.scroll,
                }
            }
            Edit::Confirm { list, .. } => {
                let warning = "Start every session in bypass mode? Nothing is asked about and no safety \
                               guard applies: only your deny rules and the push guard are left.";
                let mut lines: Vec<Line> = text::wrap(warning, width)
                    .iter()
                    .map(|line| theme.paint(Role::Warning, line))
                    .collect();
                // The two answers stay in view whatever the warning needs.
                lines.truncate(rows.saturating_sub(3));
                if !lines.is_empty() {
                    lines.push(String::new());
                }
                lines.extend(
                    list.view(width, rows.saturating_sub(lines.len()).max(1), theme)
                        .lines,
                );
                Body::plain(lines)
            }
            Edit::Text {
                setting,
                editor,
                error,
            } => {
                let now = format!(
                    "now {} {} default {}",
                    self.store.shown(setting),
                    theme.glyph(Glyph::Separator).trim(),
                    Store::default_shown(setting),
                );
                let mut lines = Vec::new();
                if rows >= 3 {
                    lines.push(text::truncate(
                        &theme.paint(Role::Muted, &one_line(&now)),
                        width,
                        ellipsis,
                    ));
                }
                if rows >= 4 {
                    lines.push(String::new());
                }
                let view = editor.view(width, 1, theme, prompt);
                let cursor = Some((
                    lines.len() + usize::from(view.cursor.0),
                    usize::from(view.cursor.1),
                ));
                lines.extend(view.lines);
                if let Some(error) = error {
                    lines.push(text::truncate(
                        &theme.paint(Role::Error, &one_line(error)),
                        width,
                        ellipsis,
                    ));
                }
                Body {
                    lines,
                    cursor,
                    scroll: None,
                }
            }
            Edit::Entries {
                setting,
                entries,
                at,
                typing,
                error,
                scroll,
            } => {
                // What is typed keeps its row whatever the list needs.
                let below = usize::from(typing.is_some()) + usize::from(error.is_some());
                let room = match typing {
                    Some(_) => rows.saturating_sub(below),
                    None => rows.saturating_sub(below).max(1),
                };
                let add = match setting.kind {
                    Kind::Table => "+ add an entry: name = value",
                    _ => "+ add an entry",
                };
                let total = entries.len() + 1;
                let visible = room.min(total);
                let mut top = scroll.get().min(total - visible);
                if *at < top {
                    top = *at;
                } else if visible > 0 && *at >= top + visible {
                    top = *at + 1 - visible;
                }
                scroll.set(top);
                let columns = Columns {
                    label: width.saturating_sub(RAIL),
                    marker: false,
                    note: 0,
                };
                let mut lines: Vec<Line> = (top..top + visible)
                    .map(|index| {
                        let cells = Cells {
                            label: entries.get(index).cloned().unwrap_or_else(|| add.to_string()),
                            value: String::new(),
                            role: Role::Muted,
                            changed: false,
                            note: "",
                        };
                        Screen::row_line(&cells, index == *at, &columns, width, theme)
                    })
                    .collect();
                let list_scroll = (total > visible && visible > 0).then_some(Scroll {
                    offset: top,
                    visible,
                    total,
                });
                let mut cursor = None;
                if let Some((_, editor)) = typing {
                    let view = editor.view(width, 1, theme, prompt);
                    cursor = Some((
                        lines.len() + usize::from(view.cursor.0),
                        usize::from(view.cursor.1),
                    ));
                    lines.extend(view.lines);
                }
                if let Some(error) = error {
                    lines.push(text::truncate(
                        &theme.paint(Role::Error, &one_line(error)),
                        width,
                        ellipsis,
                    ));
                }
                Body {
                    lines,
                    cursor,
                    scroll: list_scroll,
                }
            }
        }
    }

    /// What the rows under the list say about a setting: what it does, then its key, its
    /// default and who sets it.
    fn about(&self, setting: &Setting, theme: &Theme) -> (String, String) {
        let separator = theme.glyph(Glyph::Separator);
        let who = match self.store.sources(setting.path).first() {
            Some(source) => source.phrase(),
            None => "not set".to_string(),
        };
        let mut meta = format!(
            "{}{separator}default {}{separator}{who}",
            setting.path,
            Store::default_shown(setting)
        );
        if !setting.live && setting.kind != Kind::Role {
            meta.push_str(separator);
            meta.push_str("applies to the next session");
        }
        (setting.description.to_string(), meta)
    }

    /// The rows under the list: what the last change came to or what the selected row does,
    /// then where the setting stands. Exactly `rows` rows.
    fn foot(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line> {
        if rows == 0 {
            return Vec::new();
        }
        let ellipsis = theme.glyph(Glyph::Ellipsis);
        let (description, meta) = match (&self.edit, self.selected()) {
            (Some(edit), _) => self.about(edit.setting(), theme),
            (None, Some(Row::Setting(setting))) => self.about(setting, theme),
            (None, Some(Row::Action { about, .. })) => (about.to_string(), String::new()),
            (None, Some(Row::Credential { provider, source })) => {
                (format!("{provider} is used with {source}."), String::new())
            }
            (None, Some(Row::Fixed { path, why })) => (why.to_string(), path.to_string()),
            (None, _) => (String::new(), String::new()),
        };
        let (role, said) = match &self.notice {
            Some((Level::Info, text)) => (Role::Success, text.as_str()),
            Some((Level::Warn, text)) => (Role::Warning, text.as_str()),
            Some((Level::Error, text)) => (Role::Error, text.as_str()),
            None => (Role::Text, description.as_str()),
        };
        // With one row there is the sentence alone; with more the last is where it stands.
        let text_rows = rows.saturating_sub(1).max(1);
        let mut wrapped = text::wrap(&one_line(said), width.max(1));
        if wrapped.len() > text_rows {
            let rest = wrapped.split_off(text_rows - 1).join(" ");
            wrapped.push(text::truncate(&rest, width, ellipsis));
        }
        let mut lines: Vec<Line> = wrapped
            .iter()
            .filter(|line| !line.is_empty())
            .map(|line| theme.paint(role, line))
            .collect();
        lines.resize(text_rows, String::new());
        if rows > text_rows {
            lines.push(text::truncate(
                &theme.paint(Role::Muted, &one_line(&meta)),
                width,
                ellipsis,
            ));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use azazel_core::LoadOptions;
    use azazel_tui::GlyphLevel;
    use crossterm::event::{KeyEventKind, KeyEventState};

    use super::super::store::tests::{Fixture, fixture};
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn press(screen: &mut Screen, code: KeyCode) -> bool {
        screen.on_key(&key(code))
    }

    fn type_text(screen: &mut Screen, typed: &str) {
        for c in typed.chars() {
            assert!(!press(screen, KeyCode::Char(c)));
        }
    }

    /// Empties the editor that is open, one character at a time.
    fn clear(screen: &mut Screen, characters: usize) {
        for _ in 0..characters {
            press(screen, KeyCode::Backspace);
        }
    }

    fn screen_on(fixture: &Fixture, options: LoadOptions) -> Screen {
        Screen::new(
            fixture.open(options),
            vec!["dark".into(), "light".into(), "tokyo-night".into()],
        )
    }

    fn screen(fixture: &Fixture) -> Screen {
        Screen::new(
            fixture.trusted(),
            vec!["dark".into(), "light".into(), "tokyo-night".into()],
        )
    }

    /// The screen as plain rows, without the padding of the selected row.
    fn rows(screen: &Screen, width: usize, room: usize) -> Vec<String> {
        screen
            .view(width, room, "> ", &Theme::dark())
            .lines
            .iter()
            .map(|line| text::strip_ansi(line).trim_end().to_string())
            .collect()
    }

    fn row_of<'a>(rows: &'a [String], label: &str) -> &'a str {
        rows.iter()
            .find(|row| row.trim_start_matches(['▌', ' ']).starts_with(label))
            .unwrap_or_else(|| panic!("no row starts with {label:?} in {rows:#?}"))
    }

    /// The cells of a row, without the spaces that line them up.
    fn cells(row: &str) -> Vec<&str> {
        row.split_whitespace().collect()
    }

    fn selected_name(screen: &Screen) -> String {
        screen.selected().map(Row::name).unwrap_or_default().to_string()
    }

    #[test]
    fn pages_are_tabs_and_each_keeps_the_row_it_was_left_on() {
        let f = fixture("", "");
        let mut screen = screen(&f);
        let shown = rows(&screen, 110, 30);
        assert_eq!(
            shown[0],
            "Accounts  Models  Appearance  Status line  Input  Permissions  Tools  Sessions  Usage  Advanced"
        );
        assert!(shown[1].starts_with("Subscriptions, API keys"), "{}", shown[1]);
        assert!(shown[1].ends_with("saves to the home config"), "{}", shown[1]);
        assert_eq!(selected_name(&screen), "Sign in with a subscription");

        press(&mut screen, KeyCode::Right);
        assert_eq!(screen.page(), Page::Models);
        assert_eq!(selected_name(&screen), "roles.default");
        press(&mut screen, KeyCode::Tab);
        assert_eq!(screen.page(), Page::Appearance);
        // The first row is a heading: the cursor starts under it.
        assert_eq!(selected_name(&screen), "tui.theme");
        press(&mut screen, KeyCode::Down);
        press(&mut screen, KeyCode::Down);
        assert_eq!(selected_name(&screen), "tui.indicator");
        press(&mut screen, KeyCode::Left);
        press(&mut screen, KeyCode::BackTab);
        assert_eq!(screen.page(), Page::Accounts);
        press(&mut screen, KeyCode::Left);
        assert_eq!(screen.page(), Page::Advanced);
        for _ in 0..3 {
            press(&mut screen, KeyCode::Right);
        }
        assert_eq!(screen.page(), Page::Appearance);
        assert_eq!(selected_name(&screen), "tui.indicator");

        // The tab in view is the only one set apart.
        let tabs = screen.view(110, 30, "> ", &Theme::dark()).lines[0].clone();
        let at = tabs.find("\x1b[1;4m").unwrap();
        assert!(tabs[at..].contains("Appearance") && !tabs[..at].contains("Appearance"));
        assert_eq!(tabs.matches("\x1b[1;4m").count(), 1);
    }

    #[test]
    fn the_cursor_moves_over_settings_and_never_stops_on_a_heading() {
        let f = fixture("", "");
        let mut screen = screen(&f);
        screen.go_to("tui.expand_tools");
        // The group ends here: the next row is the heading of the next group.
        press(&mut screen, KeyCode::Down);
        assert_eq!(selected_name(&screen), "notify.bell");
        press(&mut screen, KeyCode::Up);
        assert_eq!(selected_name(&screen), "tui.expand_tools");
        press(&mut screen, KeyCode::End);
        assert_eq!(selected_name(&screen), "notify.keep_awake");
        // Around the end to the first setting, not to the heading above it.
        press(&mut screen, KeyCode::Down);
        assert_eq!(selected_name(&screen), "tui.theme");
        press(&mut screen, KeyCode::Up);
        assert_eq!(selected_name(&screen), "notify.keep_awake");
        press(&mut screen, KeyCode::Home);
        assert_eq!(selected_name(&screen), "tui.theme");
        // A page down in a window of five rows moves by what was in view.
        rows(&screen, 100, 12);
        press(&mut screen, KeyCode::PageDown);
        let after = selected_name(&screen);
        assert_ne!(after, "tui.theme");
        assert!(screen.selected().is_some_and(Row::selectable));
        press(&mut screen, KeyCode::PageUp);
        assert_eq!(selected_name(&screen), "tui.theme");
        // Even where one row is in view, a page down gets past a heading.
        screen.go_to("tui.expand_tools");
        rows(&screen, 100, 1);
        press(&mut screen, KeyCode::PageDown);
        assert_eq!(selected_name(&screen), "notify.bell");
        press(&mut screen, KeyCode::Home);
        // The headings are in the list.
        let shown = rows(&screen, 100, 30);
        assert!(shown.contains(&"Interface".to_string()), "{shown:#?}");
        assert!(shown.contains(&"Notifications".to_string()), "{shown:#?}");
    }

    #[test]
    fn a_row_shows_its_value_whether_it_is_changed_and_who_sets_it() {
        let f = fixture(
            "[tui]\ntheme = \"light\"\npopups = true\n",
            "[tui]\nglyphs = \"ascii\"\n",
        );
        let mut screen = screen_on(
            &f,
            LoadOptions {
                trusted: true,
                env: vec![("AZAZEL_TUI__REDACT".to_string(), "false".to_string())],
                ..LoadOptions::default()
            },
        );
        screen.go_to("tui.theme");
        let shown = rows(&screen, 100, 30);
        assert_eq!(
            cells(row_of(&shown, "Theme")),
            ["▌", "Theme", "light", "●", "home"]
        );
        assert_eq!(
            cells(row_of(&shown, "Symbols")),
            ["Symbols", "ascii", "●", "project"]
        );
        assert_eq!(
            row_of(&shown, "Hide secrets").split_whitespace().last(),
            Some("env")
        );
        // On its default and set nowhere: no mark and no source. Set to its default in a file:
        // the source, and no mark.
        assert_eq!(
            cells(row_of(&shown, "Working indicator")),
            ["Working", "indicator", "shimmer"]
        );
        assert_eq!(
            row_of(&shown, "Menus and questions").split_whitespace().last(),
            Some("home")
        );
        assert!(!row_of(&shown, "Menus and questions").contains('●'));
        // The tab counts what is not on its default on the page: theme, symbols, redact.
        assert!(shown[0].contains("Appearance 3"), "{}", shown[0]);
        assert!(!shown[0].contains("Models 1"), "{}", shown[0]);

        // Under the list: what the setting does, then its key, its default and who sets it.
        let foot = &shown[shown.len() - 3..];
        assert!(foot[0].starts_with("The colours"), "{foot:#?}");
        assert_eq!(foot[2], "tui.theme · default dark · set in the home config");
        screen.go_to("tui.redact");
        let shown = rows(&screen, 100, 30);
        assert_eq!(
            shown.last().unwrap(),
            "tui.redact · default on · set by AZAZEL_TUI__REDACT"
        );
        screen.go_to("limits.max_turns");
        let shown = rows(&screen, 100, 30);
        assert_eq!(
            shown.last().unwrap(),
            "limits.max_turns · default 0 · not set · applies to the next session"
        );
    }

    #[test]
    fn enter_flips_a_switch_saves_it_and_the_ui_follows() {
        let f = fixture("", "");
        let mut screen = screen(&f);
        screen.go_to("tui.show_thinking");
        assert!(!press(&mut screen, KeyCode::Enter));
        assert!(f.home_text().contains("show_thinking = true"));
        assert_eq!(
            f.applied(),
            [("tui.show_thinking".to_string(), Value::Boolean(true))]
        );
        let shown = rows(&screen, 100, 30);
        assert!(cells(row_of(&shown, "Show the model's thinking")).ends_with(&["on", "●", "home"]));
        // What came of it stands where the description was, until the next key.
        assert!(
            shown.contains(&"tui.show_thinking = on".to_string()),
            "{shown:#?}"
        );
        press(&mut screen, KeyCode::Char(' '));
        assert!(!f.home_text().contains("show_thinking"));
        assert_eq!(f.applied().len(), 2);
        press(&mut screen, KeyCode::Up);
        assert!(
            !rows(&screen, 100, 30)
                .iter()
                .any(|row| row.starts_with("tui.show_thinking ="))
        );
        assert_eq!(
            *f.changes.lock().unwrap(),
            ["tui.show_thinking = on", "tui.show_thinking = off"]
        );
    }

    #[test]
    fn a_choice_is_tried_on_the_running_ui_and_kept_only_on_enter() {
        let f = fixture("", "");
        let mut screen = screen(&f);
        screen.go_to("tui.theme");
        press(&mut screen, KeyCode::Enter);
        let shown = rows(&screen, 100, 30);
        assert_eq!(shown[1], "Appearance ❯ Theme");
        assert!(row_of(&shown, "dark").contains("in use, the default"));
        assert!(shown.iter().any(|row| row.trim() == "tokyo-night"));
        // Moving shows each theme without saving it.
        press(&mut screen, KeyCode::Down);
        press(&mut screen, KeyCode::Down);
        let theme = |name: &str| ("tui.theme".to_string(), Value::String(name.to_string()));
        assert_eq!(f.applied(), [theme("light"), theme("tokyo-night")]);
        assert_eq!(f.home_text(), "");
        // Esc puts the UI back on what applies.
        press(&mut screen, KeyCode::Esc);
        assert_eq!(f.applied().last(), Some(&theme("dark")));
        assert_eq!(f.home_text(), "");
        assert_eq!(selected_name(&screen), "tui.theme");
        assert!(rows(&screen, 100, 30)[1].starts_with("Colours, symbols"));

        press(&mut screen, KeyCode::Enter);
        press(&mut screen, KeyCode::Down);
        assert!(!press(&mut screen, KeyCode::Enter));
        assert!(f.home_text().contains("theme = \"light\""));
        assert_eq!(f.applied().last(), Some(&theme("light")));
        let shown = rows(&screen, 100, 30);
        assert!(cells(row_of(&shown, "Theme")).ends_with(&["light", "●", "home"]));
        // The picker opens on the value in use.
        press(&mut screen, KeyCode::Enter);
        assert_eq!(
            cells(row_of(&rows(&screen, 100, 30), "light")),
            ["▌", "light", "in", "use"]
        );
        // Ctrl+C closes the screen from inside an editor, and takes back what was tried.
        press(&mut screen, KeyCode::Down);
        assert_eq!(f.applied().last(), Some(&theme("tokyo-night")));
        let ctrl_c = KeyEvent {
            modifiers: KeyModifiers::CONTROL,
            ..key(KeyCode::Char('c'))
        };
        assert!(screen.on_key(&ctrl_c));
        assert_eq!(screen.take_request(), Request::Close);
        assert_eq!(f.applied().last(), Some(&theme("light")));
    }

    #[test]
    fn a_number_that_does_not_parse_stays_in_its_editor_with_the_reason() {
        let f = fixture("", "");
        let mut screen = screen(&f);
        screen.go_to("limits.max_turns");
        press(&mut screen, KeyCode::Enter);
        let view = screen.view(100, 30, "> ", &Theme::dark());
        let shown = rows(&screen, 100, 30);
        assert!(shown.contains(&"now 0 · default 0".to_string()), "{shown:#?}");
        // The editor holds the value in use, with the cursor after it.
        let (row, column) = view.cursor.unwrap();
        assert_eq!(shown[row], "> 0");
        assert_eq!(column, 3);

        type_text(&mut screen, ".5");
        press(&mut screen, KeyCode::Enter);
        let shown = rows(&screen, 100, 30);
        assert!(
            shown.contains(&"Turn cap: expected a whole number that is not negative".to_string()),
            "{shown:#?}"
        );
        assert_eq!(f.home_text(), "");
        clear(&mut screen, 3);
        type_text(&mut screen, "12");
        press(&mut screen, KeyCode::Enter);
        assert!(f.home_text().contains("max_turns = 12"));
        let shown = rows(&screen, 100, 30);
        assert!(cells(row_of(&shown, "Turn cap")).ends_with(&["12", "●", "home"]));
        assert!(shown.contains(&"limits.max_turns = 12 (applies to the next session)".to_string()));

        // A fraction where the default has one; nothing typed for a text resets it.
        screen.go_to("limits.max_cost_usd");
        press(&mut screen, KeyCode::Enter);
        clear(&mut screen, 3);
        type_text(&mut screen, "2.5");
        press(&mut screen, KeyCode::Enter);
        assert!(f.home_text().contains("max_cost_usd = 2.5"));
        screen.go_to("editor.command");
        press(&mut screen, KeyCode::Enter);
        screen.on_paste("code --wait\nsecond line");
        press(&mut screen, KeyCode::Enter);
        assert!(f.home_text().contains("command = \"code --wait\""));
        assert_eq!(
            f.applied().last(),
            Some(&("editor.command".to_string(), Value::String("code --wait".into())))
        );
        press(&mut screen, KeyCode::Enter);
        clear(&mut screen, 11);
        press(&mut screen, KeyCode::Enter);
        assert!(!f.home_text().contains("command"));
        // Esc leaves a value as it is.
        press(&mut screen, KeyCode::Enter);
        type_text(&mut screen, "vi");
        press(&mut screen, KeyCode::Esc);
        assert!(!f.home_text().contains("command"));
        assert!(screen.edit.is_none());
    }

    #[test]
    fn a_list_is_edited_entry_by_entry_and_put_in_order() {
        let f = fixture("", "");
        let mut screen = screen(&f);
        screen.go_to("tui.indicator_text");
        press(&mut screen, KeyCode::Enter);
        let shown = rows(&screen, 100, 30);
        assert_eq!(shown[1], "Appearance ❯ Indicator phrases");
        assert_eq!(shown[3], "▌ + add an entry");

        // Enter on the row that adds, then the text.
        press(&mut screen, KeyCode::Enter);
        type_text(&mut screen, "thinking hard");
        press(&mut screen, KeyCode::Enter);
        press(&mut screen, KeyCode::Char('a'));
        type_text(&mut screen, "brewing, slowly");
        press(&mut screen, KeyCode::Enter);
        let list = |f: &Fixture| {
            let home: toml::Table = f.home_text().parse().unwrap();
            home["tui"]["indicator_text"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item.as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        // A comma is part of an entry here.
        assert_eq!(list(&f), ["thinking hard", "brewing, slowly"]);
        let shown = rows(&screen, 100, 30);
        assert_eq!(
            shown[3..6],
            ["  thinking hard", "  brewing, slowly", "▌ + add an entry"]
        );

        // `[` moves the entry up, `]` down.
        press(&mut screen, KeyCode::Up);
        press(&mut screen, KeyCode::Char('['));
        assert_eq!(list(&f), ["brewing, slowly", "thinking hard"]);
        assert_eq!(rows(&screen, 100, 30)[3], "▌ brewing, slowly");
        press(&mut screen, KeyCode::Char('['));
        press(&mut screen, KeyCode::Char(']'));
        assert_eq!(list(&f), ["thinking hard", "brewing, slowly"]);
        // Enter edits the entry; nothing typed removes it.
        press(&mut screen, KeyCode::Enter);
        clear(&mut screen, 8);
        press(&mut screen, KeyCode::Enter);
        assert_eq!(list(&f), ["thinking hard", "brewing"]);
        press(&mut screen, KeyCode::Enter);
        clear(&mut screen, 7);
        press(&mut screen, KeyCode::Enter);
        assert_eq!(list(&f), ["thinking hard"]);
        press(&mut screen, KeyCode::Up);
        press(&mut screen, KeyCode::Char('d'));
        assert!(!f.home_text().contains("indicator_text"), "{}", f.home_text());
        press(&mut screen, KeyCode::Esc);
        assert!(screen.edit.is_none());
        assert_eq!(selected_name(&screen), "tui.indicator_text");

        // An entry the configuration refuses stays in the editor with the reason.
        screen.go_to("advisor.model");
        press(&mut screen, KeyCode::Enter);
        press(&mut screen, KeyCode::Enter);
        type_text(&mut screen, "not a model");
        press(&mut screen, KeyCode::Enter);
        let shown = rows(&screen, 100, 30);
        assert!(
            shown.iter().any(|row| row.starts_with("Advisor model: ")),
            "{shown:#?}"
        );
        assert!(shown.contains(&"> not a model".to_string()), "{shown:#?}");
        assert!(!f.home_text().contains("advisor"));
    }

    #[test]
    fn a_table_takes_entries_as_a_name_and_a_value() {
        let f = fixture("", "");
        let mut screen = screen(&f);
        screen.go_to("tools.format");
        press(&mut screen, KeyCode::Enter);
        assert_eq!(rows(&screen, 100, 30)[3], "▌ + add an entry: name = value");
        press(&mut screen, KeyCode::Char('a'));
        type_text(&mut screen, "rustfmt");
        press(&mut screen, KeyCode::Enter);
        let shown = rows(&screen, 100, 30);
        assert!(
            shown.contains(&"Formatters: expected name = value".to_string()),
            "{shown:#?}"
        );
        clear(&mut screen, 7);
        type_text(&mut screen, "rs = rustfmt --edition 2024");
        press(&mut screen, KeyCode::Enter);
        assert!(f.home_text().contains("rs = \"rustfmt --edition 2024\""));
        assert_eq!(rows(&screen, 100, 30)[3], "  rs = rustfmt --edition 2024");

        // Typing another name over an entry moves it. There is no order to change.
        press(&mut screen, KeyCode::Up);
        press(&mut screen, KeyCode::Char('['));
        press(&mut screen, KeyCode::Enter);
        clear(&mut screen, 27);
        type_text(&mut screen, "rust = rustfmt");
        press(&mut screen, KeyCode::Enter);
        let home: toml::Table = f.home_text().parse().unwrap();
        let formatters = home["tools"]["format"].as_table().unwrap();
        assert_eq!(formatters.len(), 1);
        assert_eq!(formatters["rust"].as_str(), Some("rustfmt"));
        press(&mut screen, KeyCode::Char('d'));
        assert!(
            f.home_text().parse::<toml::Table>().unwrap()["tools"]["format"]
                .as_table()
                .unwrap()
                .is_empty()
        );
        press(&mut screen, KeyCode::Esc);
        let shown = rows(&screen, 100, 30);
        assert!(row_of(&shown, "Formatters").contains("(none)"));
    }

    #[test]
    fn search_finds_settings_on_every_page_and_esc_lands_on_the_one_selected() {
        let f = fixture("", "");
        let mut screen = screen(&f);
        press(&mut screen, KeyCode::Right);
        for _ in 0..3 {
            press(&mut screen, KeyCode::Down);
        }
        assert_eq!(selected_name(&screen), "roles.planner");
        press(&mut screen, KeyCode::Char('/'));
        type_text(&mut screen, "Secret");
        let shown = rows(&screen, 110, 40);
        // Every word must be in the name, the key, the page or the description, whatever the
        // case; the rows stand under the name of their page.
        assert_eq!(cells(&shown[1]), ["⌕", "Secret", "4", "matches"]);
        let listed: Vec<&str> = shown[3..]
            .iter()
            .map(|row| row.trim_start_matches(['▌', ' ']))
            .collect();
        let position = |label: &str| {
            listed
                .iter()
                .position(|row| row.starts_with(label))
                .unwrap_or_else(|| panic!("{label} is not in {listed:#?}"))
        };
        assert!(position("Appearance") < position("Hide secrets in what is shown"));
        assert!(position("Hide secrets") < position("Permissions"));
        assert!(position("Permissions") < position("Secret files"));
        assert!(position("Tools") < position("Redact secrets for the model"));
        assert!(position("Redact secrets") < position("Scan for committed secrets"));
        // The tabs count the matches of each page, and follow the selection.
        assert!(
            shown[0].contains("Appearance 1") && shown[0].contains("Tools 2"),
            "{}",
            shown[0]
        );
        assert_eq!(screen.tab_in_view(), 2);
        let view = screen.view(110, 40, "> ", &Theme::dark());
        assert_eq!(view.cursor, Some((1, 8)));

        // Enter edits the row that was found, without leaving the search.
        assert_eq!(selected_name(&screen), "tui.redact");
        press(&mut screen, KeyCode::Enter);
        assert!(f.home_text().contains("redact = false"));
        assert!(screen.search.is_some());
        press(&mut screen, KeyCode::Down);
        assert_eq!(selected_name(&screen), "permissions.secret_paths");
        assert_eq!(screen.tab_in_view(), 5);
        // Esc ends the search on the page of that setting, with it selected.
        press(&mut screen, KeyCode::Esc);
        assert!(screen.search.is_none());
        assert_eq!(screen.page(), Page::Permissions);
        assert_eq!(selected_name(&screen), "permissions.secret_paths");
        // The page the search started from is still on the row it was left on.
        for _ in 0..4 {
            press(&mut screen, KeyCode::Left);
        }
        assert_eq!(screen.page(), Page::Models);
        assert_eq!(selected_name(&screen), "roles.planner");
        for _ in 0..4 {
            press(&mut screen, KeyCode::Right);
        }
        assert_eq!(selected_name(&screen), "permissions.secret_paths");

        // Two words narrow it; a key matches too; nothing found leaves the page as it was.
        press(&mut screen, KeyCode::Char('/'));
        type_text(&mut screen, "tools secret");
        assert_eq!(
            rows(&screen, 110, 40)[1].split_whitespace().last(),
            Some("matches")
        );
        assert_eq!(screen.rows.iter().filter(|row| row.selectable()).count(), 2);
        clear(&mut screen, 12);
        type_text(&mut screen, "retry.max");
        assert_eq!(selected_name(&screen), "retry.max_attempts");
        clear(&mut screen, 9);
        screen.on_paste("zzzz\n");
        let shown = rows(&screen, 110, 40);
        assert!(shown[1].ends_with("0 matches"), "{}", shown[1]);
        assert_eq!(shown[3], "  no setting matches");
        assert!(!press(&mut screen, KeyCode::Enter));
        press(&mut screen, KeyCode::Esc);
        assert_eq!(screen.page(), Page::Permissions);
        assert_eq!(selected_name(&screen), "permissions.secret_paths");
        // Backspace on an empty search leaves it.
        press(&mut screen, KeyCode::Char('/'));
        press(&mut screen, KeyCode::Backspace);
        assert!(screen.search.is_none());
    }

    #[test]
    fn r_resets_a_row_and_p_switches_to_the_project_config_only_when_trusted() {
        let f = fixture("[tui]\ntheme = \"light\"\n", "");
        let mut untrusted = screen_on(&f, LoadOptions::default());
        untrusted.go_to("tui.theme");
        press(&mut untrusted, KeyCode::Char('p'));
        let shown = rows(&untrusted, 100, 30);
        assert!(
            shown.iter().any(|row| row.contains("only in a trusted project")),
            "{shown:#?}"
        );
        assert!(shown[1].ends_with("saves to the home config"));
        let hints: Vec<String> = untrusted
            .hints(&Theme::dark())
            .iter()
            .map(|hint| hint.key.clone())
            .collect();
        assert_eq!(hints, ["tab", "↑↓", "enter", "/", "r", "esc"]);

        let mut screen = screen(&f);
        screen.go_to("tui.theme");
        assert!(screen.hints(&Theme::dark()).iter().any(|hint| hint.key == "p"));
        press(&mut screen, KeyCode::Char('p'));
        assert!(rows(&screen, 100, 30)[1].ends_with("saves to the project config"));
        press(&mut screen, KeyCode::Enter);
        press(&mut screen, KeyCode::Down);
        press(&mut screen, KeyCode::Enter);
        assert!(f.project_text().contains("theme = \"tokyo-night\""));
        assert!(f.home_text().contains("theme = \"light\""));
        let shown = rows(&screen, 100, 30);
        assert!(
            cells(row_of(&shown, "Theme")).ends_with(&["tokyo-night", "●", "project"]),
            "{shown:#?}"
        );

        // `r` takes the key out of the file changes go to; the home config's value is back.
        press(&mut screen, KeyCode::Char('r'));
        assert!(!f.project_text().contains("theme"));
        let shown = rows(&screen, 100, 30);
        assert!(
            cells(row_of(&shown, "Theme")).ends_with(&["light", "●", "home"]),
            "{shown:#?}"
        );
        assert!(
            shown.contains(&"tui.theme reset: light, set in the home config".to_string()),
            "{shown:#?}"
        );
        press(&mut screen, KeyCode::Char('r'));
        assert!(
            rows(&screen, 100, 30)
                .iter()
                .any(|row| row.contains("nothing to reset in the project config")),
        );
        press(&mut screen, KeyCode::Char('p'));
        press(&mut screen, KeyCode::Char('r'));
        assert_eq!(f.home_text().trim(), "[tui]");
        assert!(!row_of(&rows(&screen, 100, 30), "Theme").contains('●'));
    }

    #[test]
    fn bypass_as_the_starting_mode_is_asked_about_with_no_first() {
        let f = fixture("", "");
        let mut screen = screen(&f);
        screen.go_to("permissions.mode");
        // The picker opens on `manual`; `bypass` is three rows down.
        let pick_bypass = |screen: &mut Screen| {
            press(screen, KeyCode::Enter);
            for _ in 0..3 {
                press(screen, KeyCode::Down);
            }
            press(screen, KeyCode::Enter);
        };
        pick_bypass(&mut screen);
        let shown = rows(&screen, 100, 30);
        assert!(
            shown[3].starts_with("Start every session in bypass mode?"),
            "{shown:#?}"
        );
        assert!(shown.contains(&"▌ No".to_string()), "{shown:#?}");
        // A stray Enter keeps the mode.
        press(&mut screen, KeyCode::Enter);
        assert_eq!(f.home_text(), "");
        assert!(screen.edit.is_none());

        pick_bypass(&mut screen);
        press(&mut screen, KeyCode::Down);
        press(&mut screen, KeyCode::Enter);
        assert!(f.home_text().contains("mode = \"bypass\""));
        // Any other mode is saved at once.
        press(&mut screen, KeyCode::Enter);
        press(&mut screen, KeyCode::Up);
        press(&mut screen, KeyCode::Enter);
        assert!(f.home_text().contains("mode = \"auto\""));
    }

    #[test]
    fn rows_that_need_prompts_of_their_own_hand_the_screen_back() {
        let f = fixture("", "");
        let mut screen = screen(&f);
        assert!(press(&mut screen, KeyCode::Enter));
        assert_eq!(screen.take_request(), Request::Subscription);
        press(&mut screen, KeyCode::Down);
        assert!(press(&mut screen, KeyCode::Enter));
        assert_eq!(screen.take_request(), Request::ApiKey);

        // Nothing is stored: there is nothing to sign out of, and no credential to list.
        let shown = rows(&screen, 100, 30);
        assert!(!shown.iter().any(|row| row.contains("Sign out")), "{shown:#?}");
        screen.refresh(Accounts {
            signed_in: vec![("anthropic".to_string(), "auth.json (login)".to_string())],
            stored: true,
        });
        let shown = rows(&screen, 100, 30);
        assert!(row_of(&shown, "anthropic").ends_with("auth.json (login)"));
        assert!(shown.contains(&"Credentials".to_string()));
        // The screen is where it was.
        assert_eq!(selected_name(&screen), "Add an API key");
        press(&mut screen, KeyCode::Down);
        assert!(press(&mut screen, KeyCode::Enter));
        assert_eq!(screen.take_request(), Request::SignOut);

        // A role takes its model from the picker.
        press(&mut screen, KeyCode::Right);
        assert!(press(&mut screen, KeyCode::Enter));
        assert_eq!(
            screen.take_request(),
            Request::Role(azazel_core::settings::setting("roles.default").unwrap())
        );
        screen.say(Level::Info, "the default model is now mock/lead");
        assert!(rows(&screen, 100, 30).contains(&"the default model is now mock/lead".to_string()));
        assert!(press(&mut screen, KeyCode::Esc));
        assert_eq!(screen.take_request(), Request::Close);
    }

    #[test]
    fn the_keys_only_the_file_edits_are_listed_with_the_reason() {
        let f = fixture(
            "[[hooks.stop]]\ncommand = \"notify-send done\"\n[[hooks.pre_tool_use]]\ncommand = \"check\"\n",
            "",
        );
        let mut screen = screen(&f);
        screen.go_to("telemetry.content");
        press(&mut screen, KeyCode::Down);
        assert_eq!(selected_name(&screen), "providers");
        let shown = rows(&screen, 100, 30);
        assert!(
            shown
                .iter()
                .any(|row| row.starts_with("Endpoints, headers and keys"))
        );
        for _ in 0..3 {
            press(&mut screen, KeyCode::Down);
        }
        assert_eq!(selected_name(&screen), "hooks");
        let shown = rows(&screen, 100, 30);
        assert_eq!(
            cells(row_of(&shown, "[hooks]")),
            ["▌", "[hooks]", "2", "set", "home"]
        );
        assert!(!press(&mut screen, KeyCode::Enter));
        press(&mut screen, KeyCode::Char('r'));
        assert!(f.home_text().contains("hooks.stop"));
    }

    #[test]
    fn the_screen_fits_every_size_in_every_state() {
        let f = fixture(
            "[tui]\ntheme = \"light\"\nindicator_text = [\"one\", \"two\", \"three\"]\n",
            "",
        );
        let mut states: Vec<Screen> = Vec::new();
        states.push(screen(&f));
        let mut models = screen(&f);
        press(&mut models, KeyCode::Right);
        press(&mut models, KeyCode::End);
        states.push(models);
        let mut searching = screen(&f);
        press(&mut searching, KeyCode::Char('/'));
        type_text(&mut searching, "the");
        states.push(searching);
        for (path, keys) in [
            ("tui.theme", vec![KeyCode::Enter, KeyCode::Down]),
            (
                "limits.max_turns",
                vec![KeyCode::Enter, KeyCode::Char('x'), KeyCode::Enter],
            ),
            ("tui.indicator_text", vec![KeyCode::Enter, KeyCode::Down]),
            (
                "tui.indicator_text",
                vec![KeyCode::Enter, KeyCode::Char('a'), KeyCode::Char('x')],
            ),
            (
                "permissions.mode",
                vec![
                    KeyCode::Enter,
                    KeyCode::End,
                    KeyCode::Up,
                    KeyCode::Up,
                    KeyCode::Enter,
                ],
            ),
        ] {
            let mut state = screen(&f);
            state.go_to(path);
            for code in keys {
                press(&mut state, code);
            }
            assert!(state.edit.is_some(), "{path}");
            states.push(state);
        }
        for level in [GlyphLevel::Unicode, GlyphLevel::Ascii] {
            let theme = Theme::dark().with_glyphs(level);
            for state in &states {
                for width in [1, 2, 3, 5, 8, 13, 20, 30, 44, 60, 79, 96, 120, 160] {
                    for room in (1..=14).chain([16, 20, 23, 30, 80]) {
                        let view = state.view(width, room, "> ", &theme);
                        // The same rows on every page, so the box keeps its size.
                        assert_eq!(view.lines.len(), room.min(23), "{width}x{room}");
                        for line in &view.lines {
                            assert!(text::width(line) <= width, "{width}x{room}: {line:?}");
                            if level == GlyphLevel::Ascii {
                                assert!(text::strip_ansi(line).is_ascii(), "{line:?}");
                            }
                        }
                        if let Some((row, column)) = view.cursor {
                            assert!(row < view.lines.len() && column <= width, "{width}x{room}");
                        }
                        if let Some((first, scroll)) = view.scroll {
                            assert!(first + scroll.visible <= view.lines.len(), "{width}x{room}");
                            assert!(scroll.total > scroll.visible);
                        }
                    }
                }
                assert!(!state.hints(&theme).is_empty());
            }
        }
        // A list longer than its rows scrolls with the selection and says where it is.
        let mut tools = screen(&f);
        tools.go_to("goal.max_nudges");
        let view = tools.view(100, 16, "> ", &Theme::dark());
        let (first, scroll) = view.scroll.unwrap();
        assert_eq!(first, 3);
        assert_eq!(scroll.offset + scroll.visible, scroll.total);
        let shown = rows(&tools, 100, 16);
        assert!(row_of(&shown, "Goal reminders").starts_with('▌'));
    }
}
