//! The sidebar of `/models`: the roles, the models chosen lately, every model, then the
//! providers that can be used and, dimmed, those without a credential.

use azazel_tui::{Glyph, Line, Role, Theme, text};

use super::list;
use super::screen::{Fetch, Focus, Models, Placed, Scope, Side, is_set};

/// The narrowest and widest the sidebar gets.
const MIN_WIDTH: usize = 18;
const MAX_WIDTH: usize = 24;

impl Models {
    /// The rows of the sidebar for the search and the data as they are now. With a search,
    /// every entry counts its matches, and those with none are stepped over.
    pub(super) fn side_entries(&self) -> Vec<Side> {
        let searching = !self.query.trim().is_empty();
        let rows = &self.data.rows;
        let matched = |at: usize| self.matched(at);
        let mut side = Vec::new();

        let roles = &self.data.roles.roles;
        let (roles_label, roles_left) = match searching {
            true => {
                let found = roles.iter().filter(|row| self.role_matches(row)).count();
                (found.to_string(), found == 0)
            }
            false => {
                let set = roles.iter().filter(|row| is_set(row)).count();
                (format!("{set}/{}", roles.len()), false)
            }
        };
        side.push(Side {
            scope: Some(Scope::Roles),
            label: "Roles".to_string(),
            count: roles_label,
            dim: false,
            skip: roles_left,
        });
        let profiles = &self.data.profiles;
        let (profiles_label, profiles_left) = match searching {
            true => {
                let found = profiles
                    .iter()
                    .filter(|entry| self.profile_matches(entry))
                    .count();
                (found.to_string(), found == 0)
            }
            false => (profiles.len().to_string(), false),
        };
        side.push(Side {
            scope: Some(Scope::Profiles),
            label: "Profiles".to_string(),
            count: profiles_label,
            dim: false,
            skip: profiles_left,
        });
        let recent = self
            .data
            .recent
            .iter()
            .filter_map(|spec| {
                let spec = list::bare(spec);
                rows.iter().position(|row| row.model == spec && row.usable)
            })
            .filter(|at| matched(*at))
            .count();
        side.push(Side {
            scope: Some(Scope::Recent),
            label: "Recent".to_string(),
            count: recent.to_string(),
            dim: false,
            skip: searching && recent == 0,
        });
        let all = (0..rows.len())
            .filter(|at| rows[*at].usable && matched(*at))
            .count();
        side.push(Side {
            scope: Some(Scope::All),
            label: "All models".to_string(),
            count: all.to_string(),
            dim: false,
            skip: searching && all == 0,
        });

        let spinner = self.glyphs_spinner();
        for usable in [true, false] {
            side.push(Side {
                scope: None,
                label: String::new(),
                count: String::new(),
                dim: false,
                skip: true,
            });
            for provider in self.data.providers.iter().filter(|p| p.usable == usable) {
                let found = (0..rows.len())
                    .filter(|at| rows[*at].provider == provider.id && matched(*at))
                    .count();
                let count = match self.fetch.get(&provider.id) {
                    Some(Fetch::Running) => spinner.to_string(),
                    _ if !usable && !searching => String::new(),
                    _ if searching => found.to_string(),
                    _ => provider.models.to_string(),
                };
                side.push(Side {
                    scope: Some(Scope::Provider(provider.id.clone())),
                    label: text::sanitize(&provider.name),
                    count,
                    dim: !usable,
                    skip: searching && found == 0,
                });
            }
        }
        side
    }

    /// The frame of the spinner beside a provider being asked for its models.
    fn glyphs_spinner(&self) -> &'static str {
        let frames = self.glyphs.spinner();
        frames[self.frame % frames.len()]
    }

    /// As wide as its widest entry, between the narrowest and the widest a sidebar gets.
    pub(super) fn side_width(&self) -> usize {
        let widest = self
            .side
            .iter()
            .map(|entry| list::RAIL + text::width(&entry.label) + 1 + text::width(&entry.count).max(3))
            .max()
            .unwrap_or(MIN_WIDTH);
        widest.clamp(MIN_WIDTH, MAX_WIDTH)
    }

    /// The sidebar in `rows` rows of `width` columns, scrolled to keep the scope in view.
    pub(super) fn side_rows(
        &self,
        width: usize,
        rows: usize,
        theme: &Theme,
        placed: &mut Placed,
    ) -> Vec<Line> {
        let at = self
            .side
            .iter()
            .position(|entry| entry.scope.as_ref() == Some(&self.scope))
            .unwrap_or(0);
        let total = self.side.len();
        let visible = rows.min(total);
        let mut top = self.side_top.get().min(total - visible);
        if at < top {
            top = at;
        } else if at >= top + visible {
            top = at + 1 - visible;
        }
        self.side_top.set(top);
        placed.side = Some((width, top));
        let ellipsis = theme.glyph(Glyph::Ellipsis);
        let rule = theme.glyph(Glyph::ProgressEmpty);
        self.side[top..top + visible]
            .iter()
            .enumerate()
            .map(|(offset, entry)| {
                let Some(scope) = &entry.scope else {
                    return theme.paint(Role::BorderMuted, &rule.repeat(width.saturating_sub(list::RAIL)));
                };
                let room = width.saturating_sub(list::RAIL);
                let count_width = text::width(&entry.count);
                let label = text::truncate(&entry.label, room.saturating_sub(count_width + 1), ellipsis);
                let pad = " ".repeat(room.saturating_sub(text::width(&label) + count_width));
                let chosen = top + offset == at && *scope == self.scope;
                let (label, count) = match (entry.dim || entry.skip, chosen && self.focus == Focus::Body) {
                    (_, true) => (
                        theme.paint(Role::Accent, &format!("\x1b[1m{label}")),
                        entry.count.clone(),
                    ),
                    (true, false) => (theme.paint(Role::Muted, &label), entry.count.clone()),
                    (false, false) => (label, entry.count.clone()),
                };
                let body = format!("{label}{pad}{}", theme.paint(Role::Muted, &count));
                list::rail_row(&body, chosen && self.focus == Focus::Side, width, theme)
            })
            .collect()
    }
}
