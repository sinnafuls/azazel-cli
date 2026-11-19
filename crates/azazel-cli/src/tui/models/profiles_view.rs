//! The Profiles page of `/models`: every profile, built-in and saved, with where it comes from,
//! whether it can be used and whether it is enabled, and the keys that switch to one, make,
//! copy, rename, edit and delete them. It only says what is to be done: the UI carries out a
//! [`Request`] and hands the answer back to [`Models::profiles_changed`].

use azazel_protocol::Roles;
use azazel_tui::{Editor, Glyph, Hint, Line, Role, Scroll, Theme, text};
use crossterm::event::{KeyCode, KeyEvent};

use super::Request;
use super::list;
use super::screen::{Assign, Focus, How, Models, Placed, Scope};
use super::strip::{Purpose, Strip};
use crate::profiles::edit::Edit;
use crate::profiles::{Entry, ProfileSource};

/// The widest the column of profile names gets.
const NAME_WIDTH: usize = 18;
/// Wide enough for "built-in, edited".
const SOURCE_WIDTH: usize = 16;

fn source_label(entry: &Entry) -> &'static str {
    match (entry.profile.source, entry.builtin) {
        (ProfileSource::Builtin, _) => "built-in",
        (ProfileSource::User, true) => "built-in, edited",
        (ProfileSource::User, false) => "yours",
        (ProfileSource::Project, _) => "project",
    }
}

impl Models {
    pub(super) fn selected_profile(&self) -> Option<&Entry> {
        let at = *self.profile_items.get(self.profile_cursor)?;
        self.data.profiles.get(at)
    }

    pub(super) fn profile_matches(&self, entry: &Entry) -> bool {
        let query = self.query.trim().to_lowercase();
        query.is_empty()
            || entry.profile.name.to_lowercase().contains(&query)
            || source_label(entry).contains(&query)
            || entry
                .profile
                .roles
                .values()
                .flatten()
                .any(|spec| spec.to_string().to_lowercase().contains(&query))
    }

    /// The profiles that answer to the search; the cursor stays on its profile when it is
    /// still there.
    pub(super) fn refilter_profiles(&mut self) {
        let kept = self.selected_profile().map(|entry| entry.profile.name.clone());
        self.profile_items = (0..self.data.profiles.len())
            .filter(|at| self.profile_matches(&self.data.profiles[*at]))
            .collect();
        self.profile_cursor = self
            .profile_cursor
            .min(self.profile_items.len().saturating_sub(1));
        if let Some(kept) = kept {
            self.select_profile(Some(&kept));
        }
    }

    pub(super) fn select_profile(&mut self, name: Option<&str>) {
        let Some(name) = name else { return };
        if let Some(at) = self
            .profile_items
            .iter()
            .position(|at| self.data.profiles[*at].profile.name == name)
        {
            self.profile_cursor = at;
        }
    }

    /// The chain of `role` in `profile` as written.
    pub(super) fn profile_chain(&self, profile: &str, role: &str) -> Vec<String> {
        self.data
            .profiles
            .iter()
            .find(|entry| entry.profile.name == profile)
            .and_then(|entry| entry.profile.roles.get(role))
            .map(|chain| chain.iter().map(ToString::to_string).collect())
            .unwrap_or_default()
    }

    /// The answer to a switch or an edit: what it came to, the profiles and roles as they are
    /// now, and the profile the session is on.
    pub(crate) fn profiles_changed(
        &mut self,
        result: Result<String, String>,
        entries: Vec<Entry>,
        current: Option<String>,
        roles: Option<Roles>,
    ) {
        match result {
            Ok(line) => self.status = Some((Role::Success, line)),
            Err(why) => {
                self.strip = None;
                self.status = Some((Role::Error, why));
            }
        }
        self.data.profiles = entries;
        self.data.profile = current;
        if let Some(roles) = roles {
            self.data.roles = roles;
        }
        let pick = self.profile_pick.take();
        self.refilter();
        self.select_profile(pick.as_deref());
    }

    pub(super) fn profile_hints(&self) -> Vec<Hint> {
        let arrows = format!(
            "{}{}",
            self.glyphs.get(Glyph::MoreUp),
            self.glyphs.get(Glyph::MoreDown)
        );
        let mut hints = vec![Hint::new(arrows, "move"), Hint::new("enter", "switch")];
        let Some(entry) = self.selected_profile() else {
            hints.push(Hint::new("n", "new"));
            return hints;
        };
        hints.push(Hint::new("n", "new"));
        hints.push(Hint::new("c", "copy"));
        hints.push(Hint::new("e", "edit role"));
        hints.push(Hint::new("f", "fallback"));
        if !entry.builtin {
            hints.push(Hint::new("r", "rename"));
        }
        if entry.builtin {
            hints.push(Hint::new("d", if entry.enabled { "disable" } else { "enable" }));
        }
        match (entry.builtin, entry.profile.source) {
            (true, ProfileSource::User) => hints.push(Hint::new("x", "reset")),
            (false, _) => hints.push(Hint::new("x", "delete")),
            _ => {}
        }
        hints
    }

    pub(super) fn profiles_key(&mut self, key: &KeyEvent) -> super::screen::Step {
        use super::screen::Step;
        let count = self.profile_items.len();
        let page = self.shown.get().max(1);
        match key.code {
            KeyCode::Up => self.profile_cursor = self.profile_cursor.saturating_sub(1),
            KeyCode::Down => self.profile_cursor = (self.profile_cursor + 1).min(count.saturating_sub(1)),
            KeyCode::PageUp => self.profile_cursor = self.profile_cursor.saturating_sub(page),
            KeyCode::PageDown => {
                self.profile_cursor = (self.profile_cursor + page).min(count.saturating_sub(1));
            }
            KeyCode::Home => self.profile_cursor = 0,
            KeyCode::End => self.profile_cursor = count.saturating_sub(1),
            KeyCode::Left if !self.narrow.get() => self.focus = Focus::Side,
            KeyCode::Enter => return self.switch_profile(),
            KeyCode::Char('n') => return self.ask_profile_name(Purpose::NewProfile),
            KeyCode::Char('c') => return self.copy_profile(),
            KeyCode::Char('r') => return self.rename_profile(),
            KeyCode::Char('e') => return self.pick_profile_role(How::Set),
            KeyCode::Char('f') => return self.pick_profile_role(How::Add),
            KeyCode::Char('d') => return self.toggle_profile(),
            KeyCode::Delete | KeyCode::Char('x') => return self.delete_profile(),
            _ => {
                self.search_key(key);
            }
        }
        Step::Pending
    }

    /// Asks the session to move to the selected profile.
    pub(super) fn switch_profile(&mut self) -> super::screen::Step {
        use super::screen::Step;
        let Some(entry) = self.selected_profile() else {
            return Step::Pending;
        };
        let name = entry.profile.name.clone();
        if !entry.enabled {
            self.warn(format!("{name} is disabled: d enables it"));
            return Step::Pending;
        }
        if !entry.usability.usable {
            let logins = entry.usability.missing_logins.join(", ");
            self.warn(format!("{name} needs a login: {logins} (azazel login)"));
            return Step::Pending;
        }
        Step::Do(Request::SwitchProfile(name))
    }

    fn warn(&mut self, line: String) {
        self.status = Some((Role::Warning, line));
    }

    /// Whether profiles can be changed here; says why not when they cannot.
    fn profiles_open(&mut self) -> bool {
        if !self.data.profiles_editable {
            self.warn(
                "this session runs elsewhere: change its profiles on the machine it runs on".to_string(),
            );
        }
        self.data.profiles_editable
    }

    pub(super) fn edit_profile(&mut self, edit: Edit) -> super::screen::Step {
        use super::screen::Step;
        if !self.profiles_open() {
            return Step::Pending;
        }
        if let Edit::New { name, .. } | Edit::Rename { to: name, .. } = &edit {
            self.profile_pick = Some(name.clone());
        }
        Step::Do(Request::EditProfile(edit))
    }

    fn ask_profile_name(&mut self, purpose: Purpose) -> super::screen::Step {
        if self.profiles_open() {
            self.strip = Some(Strip::Name {
                purpose,
                editor: Box::new(Editor::new()),
                error: None,
            });
        }
        super::screen::Step::Pending
    }

    fn copy_profile(&mut self) -> super::screen::Step {
        self.profile_target = self.selected_profile().map(|entry| entry.profile.name.clone());
        match self.profile_target {
            Some(_) => self.ask_profile_name(Purpose::CopyProfile),
            None => super::screen::Step::Pending,
        }
    }

    fn rename_profile(&mut self) -> super::screen::Step {
        let Some(entry) = self.selected_profile() else {
            return super::screen::Step::Pending;
        };
        if entry.builtin {
            let line = format!(
                "{} is built in and keeps its name: c copies it",
                entry.profile.name
            );
            self.warn(line);
            return super::screen::Step::Pending;
        }
        self.profile_target = Some(entry.profile.name.clone());
        self.ask_profile_name(Purpose::RenameProfile)
    }

    /// A name was typed for a new profile, a copy or a rename.
    pub(super) fn profile_name_typed(&mut self, purpose: Purpose, name: String) -> super::screen::Step {
        if let Err(why) = crate::routing::profile_name(&name) {
            if let Some(strip) = &mut self.strip {
                strip.refuse(why);
            }
            return super::screen::Step::Pending;
        }
        self.strip = None;
        let target = self.profile_target.take();
        let edit = match (purpose, target) {
            (Purpose::CopyProfile, from) => Edit::New { name, from },
            (Purpose::RenameProfile, Some(from)) => Edit::Rename { from, to: name },
            _ => Edit::New { name, from: None },
        };
        self.edit_profile(edit)
    }

    /// `e` and `f`: which role of the selected profile.
    fn pick_profile_role(&mut self, how: How) -> super::screen::Step {
        let Some(entry) = self.selected_profile() else {
            return super::screen::Step::Pending;
        };
        let name = entry.profile.name.clone();
        if entry.profile.source == ProfileSource::Project {
            self.warn(format!("{name} comes from the project's config: change it there"));
            return super::screen::Step::Pending;
        }
        let copy = entry.builtin && entry.profile.source == ProfileSource::Builtin;
        let mut roles: Vec<String> = self.data.roles.roles.iter().map(|row| row.role.clone()).collect();
        for role in entry.profile.roles.keys() {
            if !roles.contains(role) {
                roles.push(role.clone());
            }
        }
        if self.profiles_open() {
            self.strip = Some(Strip::ProfileRoles {
                profile: name,
                roles,
                how,
                copy,
                at: 0,
            });
        }
        super::screen::Step::Pending
    }

    /// Opens the model list to choose a model of `role` for `profile`.
    pub(super) fn start_assign_profile(&mut self, profile: &str, role: &str, how: How) {
        let now = match how {
            How::Set => self.profile_chain(profile, role).first().cloned(),
            _ => None,
        };
        self.assign = Some(Assign {
            role: role.to_string(),
            how,
            back: true,
            profile: Some(profile.to_string()),
        });
        self.query.clear();
        self.scope = Scope::All;
        self.focus = Focus::Body;
        self.cursor = 0;
        self.top.set(0);
        self.refilter();
        self.select_model(now.as_deref());
    }

    /// The model was picked for a role of a profile: the page comes back and the profile is
    /// written, then its thinking levels are offered.
    pub(super) fn commit_profile(&mut self, assign: Assign, model: &str) -> super::screen::Step {
        let Some(profile) = assign.profile else {
            return super::screen::Step::Pending;
        };
        let role = assign.role;
        let mut chain = self.profile_chain(&profile, &role);
        let entry = match assign.how {
            How::Set if !chain.is_empty() => {
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
            How::Replace(_) => return super::screen::Step::Pending,
        };
        self.scope = Scope::Profiles;
        self.focus = Focus::Body;
        self.query.clear();
        self.refilter();
        self.select_profile(Some(&profile));
        self.strip = self.thinking_strip(&role, entry, model, Some(profile.clone()));
        self.edit_profile(Edit::SetChain {
            name: profile,
            role,
            chain: Some(chain),
        })
    }

    fn toggle_profile(&mut self) -> super::screen::Step {
        let Some(entry) = self.selected_profile() else {
            return super::screen::Step::Pending;
        };
        let name = entry.profile.name.clone();
        if !entry.builtin {
            self.warn(format!("{name} is one you saved: x deletes it"));
            return super::screen::Step::Pending;
        }
        let edit = match entry.enabled {
            true => Edit::Disable(name),
            false => Edit::Enable(name),
        };
        self.edit_profile(edit)
    }

    /// `x`: deletes a saved profile, or takes your version of a built-in one away; asks first.
    fn delete_profile(&mut self) -> super::screen::Step {
        let Some(entry) = self.selected_profile() else {
            return super::screen::Step::Pending;
        };
        let name = entry.profile.name.clone();
        let (question, yes) = match (entry.builtin, entry.profile.source) {
            (true, ProfileSource::User) => (format!("reset {name} to the built-in profile?"), "reset"),
            (true, _) => {
                self.warn(format!("{name} is built in: d hides it, e changes its roles"));
                return super::screen::Step::Pending;
            }
            (false, ProfileSource::Project) => {
                self.warn(format!("{name} comes from the project's config: change it there"));
                return super::screen::Step::Pending;
            }
            (false, _) => (format!("delete profile {name}?"), "delete"),
        };
        if self.profiles_open() {
            self.strip = Some(Strip::Confirm {
                question,
                yes: yes.to_string(),
                edit: Edit::Delete(name),
                at: 0,
            });
        }
        super::screen::Step::Pending
    }

    /// The page in `rows` rows of `width` columns: the profiles, then what the selected one
    /// holds and how it is kept.
    pub(super) fn profiles_rows(
        &self,
        width: usize,
        rows: usize,
        theme: &Theme,
        placed: &mut Placed,
    ) -> (Vec<Line>, Option<(usize, Scroll)>) {
        let foot = if rows >= 6 { 3 } else { 0 };
        let list_rows = rows - foot;
        placed.list = (2, list_rows);
        let total = self.profile_items.len();
        let visible = list_rows.min(total);
        let mut top = self.top.get().min(total - visible);
        if self.profile_cursor < top {
            top = self.profile_cursor;
        } else if self.profile_cursor >= top + visible {
            top = self.profile_cursor + 1 - visible;
        }
        self.top.set(top);
        self.shown.set(visible);
        let name_width = self
            .data
            .profiles
            .iter()
            .map(|entry| text::width(&entry.profile.name))
            .max()
            .unwrap_or(0)
            .min(NAME_WIDTH);
        let room = width.saturating_sub(list::RAIL);
        let selected = self.focus == Focus::Body || self.narrow.get();
        let mut lines: Vec<Line> = self.profile_items[top..top + visible]
            .iter()
            .enumerate()
            .map(|(offset, at)| {
                let body = self.profile_row(&self.data.profiles[*at], name_width, room, theme);
                list::rail_row(
                    &body,
                    selected && top + offset == self.profile_cursor,
                    width,
                    theme,
                )
            })
            .collect();
        if total == 0 {
            lines.push(theme.paint(Role::Muted, "no profile matches the search"));
        }
        lines.resize(list_rows, String::new());
        if foot > 0 {
            lines.push(String::new());
            lines.extend(self.profile_foot(width, theme));
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

    fn profile_row(&self, entry: &Entry, name_width: usize, room: usize, theme: &Theme) -> String {
        let current = self.data.profile.as_deref() == Some(entry.profile.name.as_str());
        let dot = match current {
            true => theme.paint(Role::Accent, theme.glyph(Glyph::Bullet)),
            false => " ".to_string(),
        };
        let name = text::pad_right(
            &text::truncate(&entry.profile.name, name_width, theme.glyph(Glyph::Ellipsis)),
            name_width,
        );
        let name = match entry.enabled {
            true => name,
            false => theme.paint(Role::Muted, &name),
        };
        let source = theme.paint(Role::Muted, &text::pad_right(source_label(entry), SOURCE_WIDTH));
        let state = match (entry.enabled, entry.usability.usable) {
            (false, _) => theme.paint(Role::Warning, "disabled"),
            (true, false) => theme.paint(
                Role::Warning,
                &format!("needs login: {}", entry.usability.missing_logins.join(", ")),
            ),
            (true, true) => theme.paint(Role::Success, "ready"),
        };
        let right = match current {
            true => theme.paint(Role::Accent, "in use"),
            false => String::new(),
        };
        Self::spread(&format!("{dot} {name}  {source}  {state}"), &right, room, theme)
    }

    /// Under the list: the roles of the selected profile, and how it is kept.
    fn profile_foot(&self, width: usize, theme: &Theme) -> Vec<Line> {
        let ellipsis = theme.glyph(Glyph::Ellipsis);
        let Some(entry) = self.selected_profile() else {
            return vec![String::new(), String::new()];
        };
        let sep = format!(" {} ", theme.glyph(Glyph::Separator));
        let roles: Vec<String> = entry
            .profile
            .roles
            .iter()
            .filter_map(|(role, chain)| {
                let first = chain.first()?;
                let more = match chain.len() {
                    0 | 1 => String::new(),
                    n => format!(" +{}", n - 1),
                };
                Some(format!("{role} {first}{more}"))
            })
            .collect();
        let name = &entry.profile.name;
        let note = match (entry.builtin, entry.profile.source) {
            (true, ProfileSource::Builtin) => format!(
                "Built in. Editing a role saves your own version as [profiles.{name}.roles]; d hides it from /profile and --profile."
            ),
            (true, ProfileSource::User) => {
                format!(
                    "Your own version of a built-in profile ([profiles.{name}.roles]); x resets it to the built-in."
                )
            }
            (false, ProfileSource::Project) => "From the project's config: change it there.".to_string(),
            (false, _) => format!("Saved in your home config as [profiles.{name}.roles]."),
            (true, ProfileSource::Project) => "From the project's config: change it there.".to_string(),
        };
        vec![
            text::truncate(&roles.join(&sep), width, ellipsis),
            theme.paint(Role::Muted, &text::truncate(&note, width, ellipsis)),
        ]
    }
}
