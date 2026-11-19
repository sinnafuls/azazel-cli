//! `/profile`: the picker that moves the session to a profile, and the facts it needs.

use azazel_protocol::ProfileInfo;
use azazel_tui::{Glyph, SelectItem, Theme};

use super::Tui;
use super::overlay::{Action, Overlay};

/// What choosing a row does: the profile's name, and what it lacks when it cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Row {
    pub name: String,
    pub missing_logins: Vec<String>,
    /// The last row: opens the Profiles page of `/models` instead of switching.
    pub manage: bool,
}

/// What the source of a profile is called to a user.
fn source_label(source: &str) -> &str {
    match source {
        "builtin" => "built-in",
        "user" => "yours",
        other => other,
    }
}

/// The sentence that says what a profile needs before it can be used.
pub(super) fn login_hint(missing: &[String]) -> String {
    let logins: Vec<String> = missing.iter().map(|id| format!("`azazel login {id}`")).collect();
    format!("needs a login: run {}", logins.join(" and "))
}

/// The picker: one row for each profile, the current one marked, a profile that cannot be used
/// saying which login it lacks.
pub(super) fn picker(profiles: &[ProfileInfo], current: Option<&str>, theme: &Theme) -> Overlay {
    let mark = theme.glyph(Glyph::Bullet);
    let sep = theme.glyph(Glyph::Separator);
    let items = profiles
        .iter()
        .map(|profile| {
            let here = if current == Some(profile.name.as_str()) {
                format!("{mark} ")
            } else {
                String::new()
            };
            let source = source_label(&profile.source);
            let detail = if profile.usable {
                let models: Vec<String> = ["default", "slow", "smol"]
                    .into_iter()
                    .filter_map(|role| Some(format!("{role} {}", profile.roles.get(role)?)))
                    .collect();
                format!("{here}{source}{sep}{}", models.join(sep))
            } else {
                format!(
                    "{here}{source}{sep}needs login: {}",
                    profile.missing_logins.join(", ")
                )
            };
            SelectItem::new(profile.name.as_str(), detail)
        })
        .chain(std::iter::once(SelectItem::new(
            format!("Manage profiles{}", theme.glyph(Glyph::Ellipsis)),
            "make, copy, rename, edit, delete, disable (the Profiles page of /models)",
        )))
        .collect();
    let rows = profiles
        .iter()
        .map(|profile| Row {
            name: profile.name.clone(),
            missing_logins: if profile.usable {
                Vec::new()
            } else {
                profile.missing_logins.clone()
            },
            manage: false,
        })
        .chain(std::iter::once(Row {
            name: String::new(),
            missing_logins: Vec::new(),
            manage: true,
        }))
        .collect();
    let body = match current {
        Some(name) => vec![format!("current: {name}")],
        None => vec!["no profile is in use yet".to_string()],
    };
    let at = profiles
        .iter()
        .position(|profile| current == Some(profile.name.as_str()))
        .unwrap_or(0);
    Overlay::select("Profile", body, items, true, Action::Profile(rows)).starting_at(at)
}

impl Tui {
    /// `/profile` with no argument.
    pub(super) fn open_profile_picker(&mut self) {
        let (profiles, current) = self.backend.profiles();
        if profiles.is_empty() {
            return self.fail("this session has no profiles");
        }
        self.overlay = Some(picker(&profiles, current.as_deref(), &self.theme));
    }

    /// A row of the picker was chosen.
    pub(super) async fn choose_profile(&mut self, row: Option<&Row>) {
        let Some(row) = row else { return };
        if row.manage {
            let opening = super::models::Opening {
                profiles: true,
                ..Default::default()
            };
            return self.open_models(opening, None).await;
        }
        if !row.missing_logins.is_empty() {
            return self.fail(&format!(
                "profile {} {}",
                row.name,
                login_hint(&row.missing_logins)
            ));
        }
        match self.backend.set_profile(&row.name).await {
            Ok(()) => self.info(&format!("profile {} is in use for this session", row.name)),
            Err(why) => self.fail(&why),
        }
    }
}

#[cfg(test)]
mod tests {
    use azazel_tui::{GlyphLevel, text};

    use super::*;

    fn info(name: &str, source: &str, missing: &[&str]) -> ProfileInfo {
        ProfileInfo {
            name: name.to_string(),
            source: source.to_string(),
            usable: missing.is_empty(),
            missing_logins: missing.iter().map(ToString::to_string).collect(),
            unknown_providers: Vec::new(),
            roles: [("default", "openai/gpt-5"), ("smol", "openai/gpt-5-mini")]
                .into_iter()
                .map(|(role, model)| (role.to_string(), model.to_string()))
                .collect(),
        }
    }

    fn shown(theme: &Theme) -> Vec<String> {
        let profiles = [
            info("openai", "builtin", &[]),
            info("claude", "builtin", &["anthropic"]),
            info("mine", "user", &[]),
        ];
        let overlay = picker(&profiles, Some("openai"), theme);
        let (lines, _) = overlay.render(100, 20, theme);
        lines.iter().map(|line| text::strip_ansi(line)).collect()
    }

    #[test]
    fn the_picker_marks_the_current_profile_and_says_which_login_a_profile_lacks() {
        let rows = shown(&Theme::dark());
        let find = |name: &str| {
            rows.iter()
                .find(|row| {
                    row.starts_with(&format!("│ ▌ {name}")) || row.starts_with(&format!("│   {name}"))
                })
                .unwrap_or_else(|| panic!("no row for {name}: {rows:?}"))
                .clone()
        };
        assert!(find("openai").contains("● built-in · default openai/gpt-5 · smol openai/gpt-5-mini"));
        assert!(
            find("claude").contains("built-in · needs login: anthropic"),
            "{rows:?}"
        );
        assert!(!find("claude").contains('●'));
        assert!(find("mine").contains("yours · default"), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains("current: openai")));
    }

    #[test]
    fn the_picker_is_plain_ascii_in_the_ascii_symbol_set() {
        let rows = shown(&Theme::dark().with_glyphs(GlyphLevel::Ascii));
        assert!(rows.iter().all(|row| row.is_ascii()), "{rows:?}");
        assert!(
            rows.iter().any(|row| row.contains("* built-in | default")),
            "{rows:?}"
        );
    }

    #[test]
    fn a_login_hint_names_every_login_to_run() {
        assert_eq!(
            login_hint(&["a".to_string(), "b".to_string()]),
            "needs a login: run `azazel login a` and `azazel login b`"
        );
    }
}
