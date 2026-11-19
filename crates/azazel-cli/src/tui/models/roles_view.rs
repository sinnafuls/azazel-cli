//! The Roles view of `/models`: every role with the model it starts on, where that comes from,
//! its thinking level and its place in the ctrl+p cycle, its fallbacks under it, and the cycle
//! itself at the foot.

use azazel_protocol::{RoleRow, RoleSource};
use azazel_tui::{Glyph, Line, Role, Scroll, Theme, text};

use super::list;
use super::screen::{Focus, Models, Placed, RoleLine, is_set};

/// The widest the column of role names gets.
const TAG_WIDTH: usize = 14;

/// Where a role's value comes from, as the view says it.
fn source(row: &RoleRow) -> &'static str {
    match row.source {
        RoleSource::Builtin => "built-in",
        RoleSource::Home => "home",
        RoleSource::Project => "project",
        RoleSource::ProjectLocal => "local",
        RoleSource::Session => "session",
        RoleSource::Unset => "",
    }
}

impl Models {
    /// Whether a role answers to the search: by its name or by a model in its chain.
    pub(super) fn role_matches(&self, row: &RoleRow) -> bool {
        let query = self.query.trim().to_lowercase();
        query.is_empty()
            || row.role.to_lowercase().contains(&query)
            || row
                .chain
                .iter()
                .chain(&row.resolved)
                .any(|spec| spec.to_lowercase().contains(&query))
    }

    /// The rows of the view: each role that answers to the search with its fallbacks, then
    /// the row that makes a new one.
    pub(super) fn role_lines(&self) -> Vec<RoleLine> {
        let mut lines = Vec::new();
        for (at, row) in self.data.roles.roles.iter().enumerate() {
            if !self.role_matches(row) {
                continue;
            }
            lines.push(RoleLine::Role(at));
            lines.extend((1..row.chain.len()).map(|entry| RoleLine::Fallback(at, entry)));
        }
        lines.push(RoleLine::New);
        lines
    }

    /// What a role starts on: its first model, or what it follows and what that comes to.
    fn first(&self, row: &RoleRow) -> (String, bool) {
        let arrow = self.glyphs.get(Glyph::Arrow);
        let resolved = row.resolved.first().map(|spec| list::bare(spec));
        match (row.source, row.chain.first()) {
            (RoleSource::Unset, _) | (_, None) => ("none".to_string(), true),
            (RoleSource::Builtin, Some(_)) => (
                format!("auto {arrow} {}", resolved.unwrap_or_else(|| "none".to_string())),
                true,
            ),
            (_, Some(first)) if first.starts_with('@') => (
                format!(
                    "{first} {arrow} {}",
                    resolved.unwrap_or_else(|| "none".to_string())
                ),
                false,
            ),
            (_, Some(first)) => (list::bare(first), false),
        }
    }

    /// `left` with `right` set against the right edge; the left side is cut to make room.
    pub(super) fn spread(left: &str, right: &str, width: usize, theme: &Theme) -> String {
        let right_width = text::width(right);
        let room = width.saturating_sub(right_width + usize::from(right_width > 0));
        let left = text::truncate(left, room, theme.glyph(Glyph::Ellipsis));
        let pad = " ".repeat(width.saturating_sub(text::width(&left) + right_width));
        format!("{left}{pad}{right}")
    }

    /// The view in `rows` rows of `width` columns: the rows, a blank one, and the ctrl+p cycle.
    pub(super) fn roles_rows(
        &self,
        width: usize,
        rows: usize,
        theme: &Theme,
        placed: &mut Placed,
    ) -> (Vec<Line>, Option<(usize, Scroll)>) {
        let foot = if rows >= 4 { 2 } else { 0 };
        let list_rows = rows - foot;
        placed.list = (2, list_rows);
        let total = self.lines.len();
        let visible = list_rows.min(total);
        let mut top = self.top.get().min(total - visible);
        if self.role_cursor < top {
            top = self.role_cursor;
        } else if self.role_cursor >= top + visible {
            top = self.role_cursor + 1 - visible;
        }
        self.top.set(top);
        self.shown.set(visible);
        let tag_width = self
            .data
            .roles
            .roles
            .iter()
            .map(|row| text::width(&row.role))
            .max()
            .unwrap_or(0)
            .min(TAG_WIDTH);
        let room = width.saturating_sub(list::RAIL);
        let selected = self.focus == Focus::Body || self.narrow.get();
        let mut lines: Vec<Line> = self.lines[top..top + visible]
            .iter()
            .enumerate()
            .map(|(offset, line)| {
                let body = match *line {
                    RoleLine::Role(at) => self.role_row(&self.data.roles.roles[at], tag_width, room, theme),
                    RoleLine::Fallback(at, entry) => {
                        let row = &self.data.roles.roles[at];
                        let spec = &row.chain[entry];
                        let right = match spec.parse::<azazel_ai::ModelSpec>() {
                            Ok(azazel_ai::ModelSpec::Model(model)) => model
                                .thinking
                                .map(|level| format!("{} {level}", theme.glyph(Glyph::RadioOn)))
                                .unwrap_or_default(),
                            _ => String::new(),
                        };
                        let left = format!(
                            "{}{} {}",
                            " ".repeat(tag_width + 3),
                            theme.glyph(Glyph::Fallback),
                            list::bare(spec)
                        );
                        Self::spread(&left, &theme.paint(Role::Muted, &right), room, theme)
                    }
                    RoleLine::New => theme.paint(
                        Role::Muted,
                        &format!("+ New role{}", theme.glyph(Glyph::Ellipsis)),
                    ),
                };
                list::rail_row(&body, selected && top + offset == self.role_cursor, width, theme)
            })
            .collect();
        lines.resize(list_rows, String::new());
        if foot > 0 {
            lines.push(String::new());
            lines.push(self.cycle_line(width, theme));
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

    fn role_row(&self, row: &RoleRow, tag_width: usize, room: usize, theme: &Theme) -> String {
        let dot = match is_set(row) {
            true => theme.paint(Role::Accent, theme.glyph(Glyph::Bullet)),
            false => theme.paint(Role::Muted, theme.glyph(Glyph::Hollow)),
        };
        let tag = text::pad_right(
            &text::truncate(&row.role, tag_width, theme.glyph(Glyph::Ellipsis)),
            tag_width,
        );
        let (first, follows) = self.first(row);
        let first = match follows {
            true => theme.paint(Role::Muted, &first),
            false => first,
        };
        let mut right: Vec<String> = Vec::new();
        if let Some(level) = &row.thinking {
            right.push(format!("{} {level}", theme.glyph(Glyph::RadioOn)));
        }
        let spec = format!("@{}", row.role);
        if let Some(at) = self.data.cycle.iter().position(|entry| *entry == spec) {
            right.push(format!("{} {}", theme.glyph(Glyph::Cycle), at + 1));
        }
        right.push(format!("{:>8}", source(row)));
        let right = theme.paint(Role::Muted, &right.join("  "));
        Self::spread(&format!("{dot} {tag}  {first}"), &right, room, theme)
    }

    /// What ctrl+p steps through, with the selected role's entry in the accent.
    fn cycle_line(&self, width: usize, theme: &Theme) -> Line {
        if self.data.cycle.is_empty() {
            return theme.paint(
                Role::Muted,
                &text::truncate(
                    "ctrl+p steps through the roles that have models; c on a role makes a cycle of your own",
                    width,
                    theme.glyph(Glyph::Ellipsis),
                ),
            );
        }
        let selected = match self.selected_line() {
            Some(RoleLine::Role(at) | RoleLine::Fallback(at, _)) => {
                Some(format!("@{}", self.data.roles.roles[at].role))
            }
            _ => None,
        };
        let step = format!(" {} ", theme.glyph(Glyph::Step));
        let entries: Vec<String> = self
            .data
            .cycle
            .iter()
            .map(|entry| match Some(entry) == selected.as_ref() {
                true => theme.paint(Role::Accent, &format!("\x1b[1m{}", text::sanitize(entry))),
                false => text::sanitize(entry),
            })
            .collect();
        let line = format!(
            "{}{}",
            theme.paint(Role::Muted, "ctrl+p cycle: "),
            entries.join(&step)
        );
        text::truncate(&line, width, theme.glyph(Glyph::Ellipsis))
    }
}
