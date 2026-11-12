//! Key combinations, the action keymap and keyboard-protocol negotiation.

use std::fmt;
use std::io::{self, Write};
use std::str::FromStr;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("invalid keybindings file: {0}")]
    Parse(String),
    #[error("unknown action `{0}`")]
    UnknownAction(String),
    #[error("unparsable key `{0}`")]
    InvalidKey(String),
    #[error("action `{0}` must be a list of key strings")]
    InvalidValue(String),
}

/// A key with its modifiers, such as `ctrl+o` or `shift+tab`.
///
/// Terminals report the same physical chord in several ways. [`KeyCombo::new`] folds them into one
/// form so that comparisons work: shift+tab is `BackTab` without the shift flag, letters are
/// lower-case with the shift flag, and shift is dropped for characters that already encode it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl KeyCombo {
    pub fn new(code: KeyCode, mut mods: KeyModifiers) -> KeyCombo {
        let code = match code {
            KeyCode::BackTab => {
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::BackTab
            }
            KeyCode::Tab if mods.contains(KeyModifiers::SHIFT) => {
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::BackTab
            }
            KeyCode::Char(c) if c.is_uppercase() => {
                mods.insert(KeyModifiers::SHIFT);
                KeyCode::Char(c.to_lowercase().next().unwrap_or(c))
            }
            KeyCode::Char(c) if !c.is_alphabetic() => {
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::Char(c)
            }
            other => other,
        };
        KeyCombo { code, mods }
    }

    pub fn from_event(event: &KeyEvent) -> KeyCombo {
        KeyCombo::new(event.code, event.modifiers)
    }

    /// Whether a key press or repeat is this combination.
    pub fn matches(&self, event: &KeyEvent) -> bool {
        event.kind != KeyEventKind::Release
            && KeyCombo::new(self.code, self.mods) == KeyCombo::from_event(event)
    }
}

impl FromStr for KeyCombo {
    type Err = KeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || KeyError::InvalidKey(s.to_string());
        let lower = s.trim().to_lowercase();
        // The key itself may be `+`, as in `ctrl++`.
        let (mods_part, key_part) = if lower == "+" {
            ("", "+")
        } else if let Some(rest) = lower.strip_suffix("++") {
            (rest, "+")
        } else {
            lower.rsplit_once('+').unwrap_or(("", lower.as_str()))
        };

        let mut mods = KeyModifiers::NONE;
        if !mods_part.is_empty() {
            for part in mods_part.split('+') {
                mods |= match part.trim() {
                    "ctrl" | "control" => KeyModifiers::CONTROL,
                    "alt" | "option" | "opt" => KeyModifiers::ALT,
                    "shift" => KeyModifiers::SHIFT,
                    "super" | "cmd" | "command" | "win" => KeyModifiers::SUPER,
                    "meta" => KeyModifiers::META,
                    "hyper" => KeyModifiers::HYPER,
                    _ => return Err(err()),
                };
            }
        }

        let code = match key_part.trim() {
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "space" => KeyCode::Char(' '),
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "insert" | "ins" => KeyCode::Insert,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdown" | "pgdn" => KeyCode::PageDown,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            other => {
                let function_key = other
                    .strip_prefix('f')
                    .and_then(|n| n.parse::<u8>().ok())
                    .filter(|n| (1..=24).contains(n));
                let mut chars = other.chars();
                match (function_key, chars.next(), chars.next()) {
                    (Some(n), _, _) => KeyCode::F(n),
                    (None, Some(c), None) => KeyCode::Char(c),
                    _ => return Err(err()),
                }
            }
        };
        Ok(KeyCombo::new(code, mods))
    }
}

impl fmt::Display for KeyCombo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let KeyCombo { code, mut mods } = KeyCombo::new(self.code, self.mods);
        if code == KeyCode::BackTab {
            mods.insert(KeyModifiers::SHIFT);
        }
        for (flag, name) in [
            (KeyModifiers::CONTROL, "ctrl"),
            (KeyModifiers::ALT, "alt"),
            (KeyModifiers::SHIFT, "shift"),
            (KeyModifiers::SUPER, "super"),
            (KeyModifiers::META, "meta"),
            (KeyModifiers::HYPER, "hyper"),
        ] {
            if mods.contains(flag) {
                write!(f, "{name}+")?;
            }
        }
        let name = match code {
            KeyCode::Enter => "enter",
            KeyCode::Esc => "esc",
            KeyCode::Tab | KeyCode::BackTab => "tab",
            KeyCode::Char(' ') => "space",
            KeyCode::Backspace => "backspace",
            KeyCode::Delete => "delete",
            KeyCode::Insert => "insert",
            KeyCode::Home => "home",
            KeyCode::End => "end",
            KeyCode::PageUp => "pageup",
            KeyCode::PageDown => "pagedown",
            KeyCode::Up => "up",
            KeyCode::Down => "down",
            KeyCode::Left => "left",
            KeyCode::Right => "right",
            KeyCode::F(n) => return write!(f, "f{n}"),
            KeyCode::Char(c) => return write!(f, "{c}"),
            other => {
                let name = format!("{other:?}").to_lowercase();
                return f.write_str(&name);
            }
        };
        f.write_str(name)
    }
}

fn default_bindings() -> Vec<(&'static str, Vec<KeyCombo>)> {
    let plain = |code| KeyCombo::new(code, KeyModifiers::NONE);
    let alt = |code| KeyCombo::new(code, KeyModifiers::ALT);
    let ctrl = |c| KeyCombo::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    vec![
        ("submit", vec![plain(KeyCode::Enter)]),
        ("follow_up", vec![alt(KeyCode::Enter)]),
        (
            "newline",
            vec![KeyCombo::new(KeyCode::Enter, KeyModifiers::SHIFT), ctrl('j')],
        ),
        ("abort", vec![plain(KeyCode::Esc)]),
        ("clear_or_exit", vec![ctrl('c')]),
        ("exit", vec![ctrl('d')]),
        ("cycle_mode", vec![plain(KeyCode::BackTab)]),
        ("agent_hub", vec![alt(KeyCode::Char('a'))]),
        ("toggle_tool_output", vec![ctrl('o')]),
        ("toggle_thinking", vec![ctrl('t')]),
        ("model_picker", vec![ctrl('l')]),
        ("history_search", vec![ctrl('r')]),
        ("paste_image", vec![ctrl('v')]),
        ("complete", vec![plain(KeyCode::Tab)]),
        ("external_editor", vec![ctrl('g')]),
        (
            "dequeue",
            vec![alt(KeyCode::Up), KeyCombo::new(KeyCode::Up, KeyModifiers::SHIFT)],
        ),
        ("stash", vec![ctrl('s')]),
        ("cycle_thinking", vec![alt(KeyCode::Char('t'))]),
        ("cycle_model", vec![alt(KeyCode::Char('m'))]),
        (
            "cycle_model_back",
            vec![KeyCombo::new(
                KeyCode::Char('m'),
                KeyModifiers::ALT | KeyModifiers::SHIFT,
            )],
        ),
        ("transcript", vec![plain(KeyCode::F(2))]),
        ("retry", vec![plain(KeyCode::F(5))]),
        ("background", vec![ctrl('b')]),
    ]
}

/// Action name to the key combinations that trigger it. When two actions share a key the one
/// defined first wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    bindings: Vec<(String, Vec<KeyCombo>)>,
}

impl Keymap {
    /// A keymap with no actions.
    pub fn empty() -> Keymap {
        Keymap { bindings: Vec::new() }
    }

    pub fn defaults() -> Keymap {
        let bindings = default_bindings()
            .into_iter()
            .map(|(action, combos)| (action.to_string(), combos))
            .collect();
        Keymap { bindings }
    }

    /// Defines an action or replaces its keys. Actions must exist before a keybindings file can
    /// rebind them, so register application-specific actions here first.
    pub fn bind(&mut self, action: &str, combos: impl IntoIterator<Item = KeyCombo>) {
        let combos: Vec<KeyCombo> = combos
            .into_iter()
            .map(|c| KeyCombo::new(c.code, c.mods))
            .collect();
        match self.bindings.iter_mut().find(|(name, _)| name == action) {
            Some((_, existing)) => *existing = combos,
            None => self.bindings.push((action.to_string(), combos)),
        }
    }

    /// Applies a keybindings file of `action = ["key", …]` entries. Listed actions get exactly the
    /// listed keys, and those keys stop triggering every action the file does not mention. Nothing
    /// changes when the file has an error.
    pub fn merge_toml(&mut self, source: &str) -> Result<(), KeyError> {
        let table: toml::Table = source
            .parse()
            .map_err(|e: toml::de::Error| KeyError::Parse(e.to_string()))?;
        let mut updates: Vec<(usize, Vec<KeyCombo>)> = Vec::with_capacity(table.len());
        for (action, value) in &table {
            let index = self
                .bindings
                .iter()
                .position(|(name, _)| name == action)
                .ok_or_else(|| KeyError::UnknownAction(action.clone()))?;
            let toml::Value::Array(items) = value else {
                return Err(KeyError::InvalidValue(action.clone()));
            };
            let mut combos = Vec::with_capacity(items.len());
            for item in items {
                let toml::Value::String(text) = item else {
                    return Err(KeyError::InvalidValue(action.clone()));
                };
                combos.push(text.parse::<KeyCombo>()?);
            }
            updates.push((index, combos));
        }

        let mentioned: Vec<usize> = updates.iter().map(|(index, _)| *index).collect();
        for (index, combos) in updates {
            for (other, (_, existing)) in self.bindings.iter_mut().enumerate() {
                if !mentioned.contains(&other) {
                    existing.retain(|combo| !combos.contains(combo));
                }
            }
            self.bindings[index].1 = combos;
        }
        Ok(())
    }

    /// The action bound to a key press or repeat, if any. Key releases never trigger actions.
    pub fn action_for(&self, event: &KeyEvent) -> Option<&str> {
        if event.kind == KeyEventKind::Release {
            return None;
        }
        let pressed = KeyCombo::from_event(event);
        self.bindings
            .iter()
            .find(|(_, combos)| combos.contains(&pressed))
            .map(|(action, _)| action.as_str())
    }

    /// The keys bound to an action; empty for unknown or unbound actions.
    pub fn keys_for(&self, action: &str) -> &[KeyCombo] {
        let found = self.bindings.iter().find(|(name, _)| name == action);
        found.map(|(_, combos)| combos.as_slice()).unwrap_or_default()
    }

    pub fn actions(&self) -> impl Iterator<Item = &str> {
        self.bindings.iter().map(|(name, _)| name.as_str())
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap::defaults()
    }
}

// Kitty keyboard protocol: push and pop the "disambiguate escape codes" flag.
const PUSH_DISAMBIGUATE: &[u8] = b"\x1b[>1u";
const POP_KEYBOARD_FLAGS: &[u8] = b"\x1b[<1u";

/// Asks the terminal to report keys unambiguously (so that `shift+enter`, `ctrl+i` and `esc` can
/// be told apart) when it supports the kitty keyboard protocol. Returns whether it was enabled;
/// call [`disable_enhanced_keys`] before exit only in that case.
pub fn enable_enhanced_keys(out: &mut impl Write) -> io::Result<bool> {
    if !matches!(crossterm::terminal::supports_keyboard_enhancement(), Ok(true)) {
        return Ok(false);
    }
    out.write_all(PUSH_DISAMBIGUATE)?;
    out.flush()?;
    Ok(true)
}

/// Undoes [`enable_enhanced_keys`].
pub fn disable_enhanced_keys(out: &mut impl Write) -> io::Result<()> {
    out.write_all(POP_KEYBOARD_FLAGS)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn combo(s: &str) -> KeyCombo {
        s.parse().expect("test key string parses")
    }

    fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn parses_modifiers_and_named_keys() {
        assert_eq!(
            combo("ctrl+o"),
            KeyCombo {
                code: KeyCode::Char('o'),
                mods: KeyModifiers::CONTROL
            }
        );
        assert_eq!(
            combo("Alt+Enter"),
            KeyCombo {
                code: KeyCode::Enter,
                mods: KeyModifiers::ALT
            }
        );
        assert_eq!(
            combo("esc"),
            KeyCombo {
                code: KeyCode::Esc,
                mods: KeyModifiers::NONE
            }
        );
        assert_eq!(
            combo("F5"),
            KeyCombo {
                code: KeyCode::F(5),
                mods: KeyModifiers::NONE
            }
        );
        assert_eq!(
            combo("f"),
            KeyCombo {
                code: KeyCode::Char('f'),
                mods: KeyModifiers::NONE
            }
        );
        assert_eq!(
            combo("CTRL+SHIFT+P"),
            KeyCombo {
                code: KeyCode::Char('p'),
                mods: KeyModifiers::CONTROL | KeyModifiers::SHIFT
            }
        );
        assert_eq!(
            combo("ctrl++"),
            KeyCombo {
                code: KeyCode::Char('+'),
                mods: KeyModifiers::CONTROL
            }
        );
        assert_eq!(
            combo("+"),
            KeyCombo {
                code: KeyCode::Char('+'),
                mods: KeyModifiers::NONE
            }
        );
        assert_eq!(
            combo(" ctrl + space "),
            KeyCombo {
                code: KeyCode::Char(' '),
                mods: KeyModifiers::CONTROL
            }
        );
        assert_eq!(
            combo("cmd+pgdn"),
            KeyCombo {
                code: KeyCode::PageDown,
                mods: KeyModifiers::SUPER
            }
        );
        assert_eq!(combo("hyper+meta+x").to_string(), "meta+hyper+x");
    }

    #[test]
    fn rejects_unparsable_keys() {
        for bad in ["", "ctrl+", "mod+x", "ctrl+nope", "f99", "f0", "ctrl+alt"] {
            assert_eq!(
                bad.parse::<KeyCombo>(),
                Err(KeyError::InvalidKey(bad.to_string())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn display_round_trips() {
        for text in [
            "ctrl+o",
            "alt+enter",
            "shift+tab",
            "esc",
            "f5",
            "ctrl+shift+p",
            "ctrl+alt+delete",
            "space",
        ] {
            assert_eq!(combo(text).to_string(), text);
            assert_eq!(combo(&combo(text).to_string()), combo(text));
        }
        assert_eq!(combo("Control+Return").to_string(), "ctrl+enter");
    }

    #[test]
    fn shift_tab_is_back_tab() {
        let shift_tab = combo("shift+tab");
        assert_eq!(shift_tab.code, KeyCode::BackTab);
        assert!(shift_tab.matches(&press(KeyCode::BackTab, KeyModifiers::SHIFT)));
        assert!(shift_tab.matches(&press(KeyCode::BackTab, KeyModifiers::NONE)));
        assert!(shift_tab.matches(&press(KeyCode::Tab, KeyModifiers::SHIFT)));
        assert!(!shift_tab.matches(&press(KeyCode::Tab, KeyModifiers::NONE)));
        assert_eq!(combo("backtab"), shift_tab);
    }

    #[test]
    fn letters_match_regardless_of_reported_case() {
        let chord = combo("ctrl+shift+p");
        assert!(chord.matches(&press(
            KeyCode::Char('P'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(chord.matches(&press(KeyCode::Char('P'), KeyModifiers::CONTROL)));
        assert!(chord.matches(&press(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(!chord.matches(&press(KeyCode::Char('p'), KeyModifiers::CONTROL)));
        // Shift is already part of a punctuation character.
        assert!(combo("?").matches(&press(KeyCode::Char('?'), KeyModifiers::SHIFT)));
    }

    #[test]
    fn default_keymap_has_documented_bindings() {
        let keymap = Keymap::defaults();
        let expected: [(&str, &[&str]); 23] = [
            ("submit", &["enter"]),
            ("follow_up", &["alt+enter"]),
            ("newline", &["shift+enter", "ctrl+j"]),
            ("abort", &["esc"]),
            ("clear_or_exit", &["ctrl+c"]),
            ("exit", &["ctrl+d"]),
            ("cycle_mode", &["shift+tab"]),
            ("agent_hub", &["alt+a"]),
            ("toggle_tool_output", &["ctrl+o"]),
            ("toggle_thinking", &["ctrl+t"]),
            ("model_picker", &["ctrl+l"]),
            ("history_search", &["ctrl+r"]),
            ("paste_image", &["ctrl+v"]),
            ("complete", &["tab"]),
            ("external_editor", &["ctrl+g"]),
            ("dequeue", &["alt+up", "shift+up"]),
            ("stash", &["ctrl+s"]),
            ("cycle_thinking", &["alt+t"]),
            ("cycle_model", &["alt+m"]),
            ("cycle_model_back", &["alt+shift+m"]),
            ("transcript", &["f2"]),
            ("retry", &["f5"]),
            ("background", &["ctrl+b"]),
        ];
        assert_eq!(keymap.actions().count(), expected.len());
        for (action, keys) in expected {
            let shown: Vec<String> = keymap.keys_for(action).iter().map(ToString::to_string).collect();
            assert_eq!(shown, keys, "keys for {action}");
            for key in keys {
                let KeyCombo { code, mods } = combo(key);
                assert_eq!(
                    keymap.action_for(&press(code, mods)),
                    Some(action),
                    "action for {key}"
                );
            }
        }
    }

    #[test]
    fn action_lookup_distinguishes_modifiers_and_ignores_releases() {
        let keymap = Keymap::defaults();
        assert_eq!(
            keymap.action_for(&press(KeyCode::Enter, KeyModifiers::NONE)),
            Some("submit")
        );
        assert_eq!(
            keymap.action_for(&press(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some("newline")
        );
        assert_eq!(
            keymap.action_for(&press(KeyCode::Enter, KeyModifiers::ALT)),
            Some("follow_up")
        );
        assert_eq!(
            keymap.action_for(&press(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some("cycle_mode")
        );
        assert_eq!(
            keymap.action_for(&press(KeyCode::Tab, KeyModifiers::NONE)),
            Some("complete")
        );
        assert_eq!(
            keymap.action_for(&press(KeyCode::Char('x'), KeyModifiers::NONE)),
            None
        );
        // A modifier no binding names must not fall through to the unmodified key.
        assert_eq!(
            keymap.action_for(&press(KeyCode::Enter, KeyModifiers::META)),
            None
        );
        assert_eq!(
            keymap.action_for(&press(KeyCode::Enter, KeyModifiers::ALT | KeyModifiers::HYPER)),
            None
        );
        let release = KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release);
        assert_eq!(keymap.action_for(&release), None);
        let repeat = KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Repeat);
        assert_eq!(keymap.action_for(&repeat), Some("submit"));
        assert!(keymap.keys_for("no_such_action").is_empty());
    }

    #[test]
    fn merge_replaces_keys_and_takes_them_from_unmentioned_actions() {
        let mut keymap = Keymap::defaults();
        keymap
            .merge_toml("toggle_thinking = [\"ctrl+o\", \"f2\"]\nexit = []\n")
            .expect("valid file");
        assert_eq!(keymap.keys_for("toggle_thinking"), [combo("ctrl+o"), combo("f2")]);
        assert!(keymap.keys_for("toggle_tool_output").is_empty());
        assert!(keymap.keys_for("exit").is_empty());
        assert_eq!(
            keymap.action_for(&press(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some("toggle_thinking")
        );
        assert_eq!(
            keymap.action_for(&press(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            keymap.action_for(&press(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            None
        );
        // Untouched actions keep their defaults.
        assert_eq!(keymap.keys_for("submit"), [combo("enter")]);
    }

    #[test]
    fn merge_swaps_keys_between_two_mentioned_actions() {
        let mut keymap = Keymap::defaults();
        keymap
            .merge_toml("toggle_thinking = [\"ctrl+o\"]\ntoggle_tool_output = [\"ctrl+t\"]\n")
            .expect("valid file");
        assert_eq!(
            keymap.action_for(&press(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some("toggle_thinking")
        );
        assert_eq!(
            keymap.action_for(&press(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            Some("toggle_tool_output")
        );
    }

    #[test]
    fn merge_errors_name_the_offender_and_change_nothing() {
        let mut keymap = Keymap::defaults();
        let before = keymap.clone();
        assert_eq!(
            keymap.merge_toml("nope = [\"ctrl+o\"]"),
            Err(KeyError::UnknownAction("nope".to_string()))
        );
        assert_eq!(
            keymap.merge_toml("exit = [\"ctrl+q\"]\nsubmit = [\"ctrl+nope\"]"),
            Err(KeyError::InvalidKey("ctrl+nope".to_string()))
        );
        assert_eq!(
            keymap.merge_toml("submit = \"enter\""),
            Err(KeyError::InvalidValue("submit".to_string()))
        );
        assert_eq!(
            keymap.merge_toml("submit = [1]"),
            Err(KeyError::InvalidValue("submit".to_string()))
        );
        assert!(matches!(keymap.merge_toml("submit = ["), Err(KeyError::Parse(_))));
        assert_eq!(keymap, before);
    }

    #[test]
    fn custom_actions_can_be_bound_and_rebound() {
        let mut keymap = Keymap::defaults();
        keymap.bind(
            "open_notes",
            [KeyCombo {
                code: KeyCode::Char('N'),
                mods: KeyModifiers::ALT,
            }],
        );
        assert_eq!(
            keymap.action_for(&press(
                KeyCode::Char('n'),
                KeyModifiers::ALT | KeyModifiers::SHIFT
            )),
            Some("open_notes")
        );
        keymap
            .merge_toml("open_notes = [\"f9\"]")
            .expect("registered action can be rebound");
        assert_eq!(keymap.keys_for("open_notes"), [combo("f9")]);
    }

    #[test]
    fn disable_writes_the_pop_sequence() {
        let mut out = Vec::new();
        disable_enhanced_keys(&mut out).expect("writing to a Vec cannot fail");
        assert_eq!(out, b"\x1b[<1u");
    }
}
