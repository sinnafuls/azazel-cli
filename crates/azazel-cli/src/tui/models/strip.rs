//! The strip at the foot of `/models`: a row of choices (the roles a model can take, the
//! thinking levels, the presets) or a name being typed.

use azazel_ai::ThinkingLevel;
use azazel_tui::{Editor, Glyph, Line, Role, Theme, text};
use crossterm::event::{KeyCode, KeyEvent};

use super::screen::How;
use crate::profiles::edit::Edit;

/// What a name typed into the strip is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Purpose {
    NewRole,
    Preset,
    /// A profile of the roles the session has now.
    NewProfile,
    /// A copy of the profile the screen has in `profile_target`.
    CopyProfile,
    /// A new name for the profile in `profile_target`.
    RenameProfile,
}

pub(super) enum Strip {
    /// The roles `model` can take; `held` marks those it is the model of already. The chip
    /// after them asks which role it is to be a fallback for.
    Roles {
        model: String,
        roles: Vec<String>,
        held: Vec<bool>,
        at: usize,
    },
    /// The roles `model` can be added to as a fallback.
    Fallback {
        model: String,
        roles: Vec<String>,
        at: usize,
    },
    /// The thinking level of entry `entry` of `role`'s chain: `None` inherits the session's.
    Thinking {
        role: String,
        entry: usize,
        options: Vec<Option<ThinkingLevel>>,
        at: usize,
        /// The profile whose chain it is, when it is not the session's roles.
        profile: Option<String>,
    },
    /// The roles of `profile` to choose one from, to give it a model (`how`). `copy` when the
    /// profile is a built-in one that the change turns into a copy of the user's own.
    ProfileRoles {
        profile: String,
        roles: Vec<String>,
        how: How,
        copy: bool,
        at: usize,
    },
    /// Asks before `edit` is made: the first chip keeps things as they are, the second (`yes`)
    /// does it.
    Confirm {
        question: String,
        yes: String,
        edit: Edit,
        at: usize,
    },
    /// The saved presets, to load one.
    Presets { names: Vec<String>, at: usize },
    /// A name being typed.
    Name {
        purpose: Purpose,
        editor: Box<Editor>,
        error: Option<String>,
    },
}

/// What a key did to the strip.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Key {
    Pending,
    /// Esc: the strip closes and nothing is done.
    Close,
    /// Enter on the choice at this index, or on the typed name.
    Chosen(usize),
    Typed(String),
}

/// The strip as one row, and where on it each choice is, as columns `start..end`.
pub(super) struct Drawn {
    pub line: Line,
    pub chips: Vec<(usize, usize)>,
    /// The typing cursor's column, while a name is typed.
    pub cursor: Option<usize>,
}

impl Strip {
    /// The words of the choices.
    fn labels(&self, theme: &Theme) -> Vec<String> {
        match self {
            Strip::Roles { roles, held, .. } => {
                let mut labels: Vec<String> = roles
                    .iter()
                    .zip(held)
                    .map(|(role, held)| match held {
                        true => format!("{role} {}", theme.glyph(Glyph::Success)),
                        false => role.clone(),
                    })
                    .collect();
                labels.push(format!("+ fallback for{}", theme.glyph(Glyph::Ellipsis)));
                labels
            }
            Strip::Fallback { roles, .. } | Strip::ProfileRoles { roles, .. } => roles.clone(),
            Strip::Thinking { options, .. } => options
                .iter()
                .map(|option| match option {
                    Some(level) => level.to_string(),
                    None => "inherit".to_string(),
                })
                .collect(),
            Strip::Presets { names, .. } => names.clone(),
            Strip::Confirm { yes, .. } => vec!["keep".to_string(), yes.clone()],
            Strip::Name { .. } => Vec::new(),
        }
    }

    fn lead(&self, theme: &Theme) -> String {
        let arrow = theme.glyph(Glyph::Arrow);
        let short = |model: &str| model.split_once('/').map_or(model, |(_, id)| id).to_string();
        match self {
            Strip::Roles { model, .. } => format!("{} {arrow}", short(model)),
            Strip::Fallback { model, .. } => format!("{} as a fallback for", short(model)),
            Strip::Thinking { role, entry: 0, .. } => format!("thinking for {role}:"),
            Strip::Thinking { role, .. } => format!("thinking for this fallback of {role}:"),
            Strip::ProfileRoles {
                profile, how, copy, ..
            } => {
                let what = match how {
                    How::Add => "add a fallback to",
                    _ => "model of",
                };
                let own = match copy {
                    true => " (saves your own copy of the built-in profile)",
                    false => "",
                };
                format!("{what} a role of {profile}{own}:")
            }
            Strip::Confirm { question, .. } => question.clone(),
            Strip::Presets { .. } => "load preset:".to_string(),
            Strip::Name {
                purpose: Purpose::NewRole,
                ..
            } => "new role:".to_string(),
            Strip::Name {
                purpose: Purpose::Preset,
                ..
            } => "save the roles as preset:".to_string(),
            Strip::Name {
                purpose: Purpose::NewProfile,
                ..
            } => "new profile from the roles in use, named:".to_string(),
            Strip::Name {
                purpose: Purpose::CopyProfile,
                ..
            } => "copy as:".to_string(),
            Strip::Name {
                purpose: Purpose::RenameProfile,
                ..
            } => "rename to:".to_string(),
        }
    }

    fn at_mut(&mut self) -> Option<&mut usize> {
        match self {
            Strip::Roles { at, .. }
            | Strip::Fallback { at, .. }
            | Strip::ProfileRoles { at, .. }
            | Strip::Thinking { at, .. }
            | Strip::Confirm { at, .. }
            | Strip::Presets { at, .. } => Some(at),
            Strip::Name { .. } => None,
        }
    }

    pub(super) fn at(&self) -> Option<usize> {
        match self {
            Strip::Roles { at, .. }
            | Strip::Fallback { at, .. }
            | Strip::ProfileRoles { at, .. }
            | Strip::Thinking { at, .. }
            | Strip::Confirm { at, .. }
            | Strip::Presets { at, .. } => Some(*at),
            Strip::Name { .. } => None,
        }
    }

    /// How many choices there are.
    fn len(&self) -> usize {
        match self {
            Strip::Roles { roles, .. } => roles.len() + 1,
            Strip::Fallback { roles, .. } | Strip::ProfileRoles { roles, .. } => roles.len(),
            Strip::Thinking { options, .. } => options.len(),
            Strip::Presets { names, .. } => names.len(),
            Strip::Confirm { .. } => 2,
            Strip::Name { .. } => 0,
        }
    }

    /// Selects choice `index`, when there is one.
    pub(super) fn select(&mut self, index: usize) -> bool {
        let count = self.len();
        match self.at_mut() {
            Some(at) if index < count => {
                *at = index;
                true
            }
            _ => false,
        }
    }

    pub(super) fn on_key(&mut self, key: &KeyEvent) -> Key {
        if key.code == KeyCode::Esc {
            return Key::Close;
        }
        if let Strip::Name { editor, error, .. } = self {
            if key.code == KeyCode::Enter {
                return Key::Typed(editor.text().trim().to_string());
            }
            *error = None;
            editor.handle_key(key);
            return Key::Pending;
        }
        let count = self.len();
        let Some(at) = self.at_mut() else {
            return Key::Pending;
        };
        match key.code {
            KeyCode::Enter => return Key::Chosen(*at),
            KeyCode::Left | KeyCode::Up | KeyCode::BackTab => *at = at.saturating_sub(1),
            KeyCode::Right | KeyCode::Down | KeyCode::Tab => *at = (*at + 1).min(count.saturating_sub(1)),
            KeyCode::Home => *at = 0,
            KeyCode::End => *at = count.saturating_sub(1),
            _ => {}
        }
        Key::Pending
    }

    pub(super) fn on_paste(&mut self, pasted: &str) {
        if let Strip::Name { editor, .. } = self {
            editor.insert_str(pasted.lines().next().unwrap_or_default());
        }
    }

    /// Says why the typed name was not taken; the strip stays open.
    pub(super) fn refuse(&mut self, why: String) {
        if let Strip::Name { error, .. } = self {
            *error = Some(why);
        }
    }

    /// The strip in `width` columns: the lead in the muted colour, then the choices with the
    /// selected one on the selection band. A row of choices too long for the width starts
    /// late enough to show the selected one, after an ellipsis.
    pub(super) fn draw(&self, width: usize, theme: &Theme) -> Drawn {
        let lead = format!("{} ", self.lead(theme));
        let lead_width = text::width(&lead);
        let mut line = theme.paint(Role::Muted, &lead);
        if let Strip::Name { editor, error, .. } = self {
            let typed = text::sanitize(editor.text());
            let before = text::width(&text::sanitize(&editor.text()[..editor.cursor()]));
            line.push_str(&typed);
            if let Some(error) = error {
                line.push_str("  ");
                line.push_str(&theme.paint(Role::Error, error));
            }
            return Drawn {
                line: text::truncate(&line, width, ""),
                chips: Vec::new(),
                cursor: Some((lead_width + before).min(width.saturating_sub(1))),
            };
        }
        let labels = self.labels(theme);
        let at = self.at().unwrap_or(0);
        let widths: Vec<usize> = labels.iter().map(|label| text::width(label) + 2).collect();
        let ellipsis = format!("{} ", theme.glyph(Glyph::Ellipsis));
        // The first chip shown: as early as still leaves the selected one in view.
        let mut first = 0;
        while first < at {
            let skip = if first > 0 { text::width(&ellipsis) } else { 0 };
            let used: usize = lead_width + skip + widths[first..=at].iter().sum::<usize>();
            if used <= width {
                break;
            }
            first += 1;
        }
        let mut column = lead_width;
        if first > 0 {
            line.push_str(&theme.paint(Role::Muted, &ellipsis));
            column += text::width(&ellipsis);
        }
        let mut chips = vec![(0, 0); labels.len()];
        for (index, label) in labels.iter().enumerate().skip(first) {
            let chip = format!(" {label} ");
            if index == at {
                line.push_str(&theme.band(
                    Role::SelectedBg,
                    Role::Accent,
                    &format!("\x1b[1m{chip}"),
                    widths[index],
                ));
            } else {
                line.push_str(&chip);
            }
            chips[index] = (column, column + widths[index]);
            column += widths[index];
        }
        Drawn {
            line: text::truncate(&line, width, ""),
            chips,
            cursor: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use azazel_tui::GlyphLevel;
    use crossterm::event::KeyModifiers;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn roles(count: usize) -> Strip {
        let roles: Vec<String> = (0..count).map(|n| format!("role{n}")).collect();
        Strip::Roles {
            model: "a/model".into(),
            held: vec![false; count],
            roles,
            at: 0,
        }
    }

    #[test]
    fn arrows_choose_enter_picks_and_esc_closes() {
        let mut strip = roles(3);
        assert_eq!(strip.on_key(&key(KeyCode::Left)), Key::Pending);
        assert_eq!(strip.at(), Some(0));
        for _ in 0..10 {
            strip.on_key(&key(KeyCode::Right));
        }
        // Three roles and the fallback chip.
        assert_eq!(strip.on_key(&key(KeyCode::Enter)), Key::Chosen(3));
        assert_eq!(strip.on_key(&key(KeyCode::Esc)), Key::Close);

        let mut name = Strip::Name {
            purpose: Purpose::NewRole,
            editor: Box::new(Editor::new()),
            error: None,
        };
        for c in "critic ".chars() {
            name.on_key(&key(KeyCode::Char(c)));
        }
        assert_eq!(name.on_key(&key(KeyCode::Enter)), Key::Typed("critic".into()));
    }

    #[test]
    fn a_long_strip_keeps_the_selected_chip_in_view_and_says_where_each_chip_is() {
        for level in GlyphLevel::ALL {
            let theme = Theme::dark().with_glyphs(level);
            let mut strip = roles(20);
            strip.select(17);
            for width in 20..120 {
                let drawn = strip.draw(width, &theme);
                assert!(text::width(&drawn.line) <= width);
                let plain = text::strip_ansi(&drawn.line);
                let (start, end) = drawn.chips[17];
                if end <= width {
                    let chip: String = plain.chars().skip(start).take(end - start).collect();
                    assert_eq!(chip, " role17 ", "{width}: {plain}");
                }
            }
            let drawn = strip.draw(60, &theme);
            let plain = text::strip_ansi(&drawn.line);
            assert!(plain.contains(theme.glyph(Glyph::Ellipsis)), "{plain}");
            assert!(plain.contains("role17"), "{plain}");
            assert!(!plain.contains("role0 "), "{plain}");
        }
    }
}
