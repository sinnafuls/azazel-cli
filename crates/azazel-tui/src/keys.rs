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
        // `ctrl+j` is a line feed, which is what a terminal can be told to type for
        // `shift+enter`; a Windows console hands a line feed over as `ctrl+enter`.
        (
            "newline",
            vec![
                KeyCombo::new(KeyCode::Enter, KeyModifiers::SHIFT),
                ctrl('j'),
                KeyCombo::new(KeyCode::Enter, KeyModifiers::CONTROL),
            ],
        ),
        ("abort", vec![plain(KeyCode::Esc)]),
        ("clear_or_exit", vec![ctrl('c')]),
        ("exit", vec![ctrl('d')]),
        ("cycle_mode", vec![plain(KeyCode::BackTab)]),
        ("agent_hub", vec![alt(KeyCode::Char('a'))]),
        ("toggle_tool_output", vec![ctrl('o')]),
        ("toggle_thinking", vec![ctrl('t')]),
        ("model_picker", vec![ctrl('l'), alt(KeyCode::Char('p'))]),
        ("history_search", vec![ctrl('r')]),
        // Windows Terminal and others paste with `ctrl+v` themselves, and with an image on the
        // clipboard that pastes nothing: `alt+v` reaches azazel there.
        ("paste_image", vec![ctrl('v'), alt(KeyCode::Char('v'))]),
        (
            "paste_text",
            vec![
                KeyCombo::new(KeyCode::Char('v'), KeyModifiers::CONTROL | KeyModifiers::SHIFT),
                KeyCombo::new(KeyCode::Char('v'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            ],
        ),
        ("complete", vec![plain(KeyCode::Tab)]),
        ("external_editor", vec![ctrl('g')]),
        (
            "dequeue",
            vec![alt(KeyCode::Up), KeyCombo::new(KeyCode::Up, KeyModifiers::SHIFT)],
        ),
        ("stash", vec![ctrl('s')]),
        ("cycle_thinking", vec![alt(KeyCode::Char('t'))]),
        ("cycle_model", vec![ctrl('p'), alt(KeyCode::Char('m'))]),
        // A terminal without the enhanced key reports sends `ctrl+shift+p` as `ctrl+p`, and
        // several terminals keep it for their command palette: `alt+shift+p` always arrives.
        (
            "cycle_model_back",
            vec![
                KeyCombo::new(KeyCode::Char('p'), KeyModifiers::CONTROL | KeyModifiers::SHIFT),
                KeyCombo::new(KeyCode::Char('p'), KeyModifiers::ALT | KeyModifiers::SHIFT),
                KeyCombo::new(KeyCode::Char('m'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            ],
        ),
        ("transcript", vec![plain(KeyCode::F(2))]),
        ("retry", vec![plain(KeyCode::F(5))]),
        ("background", vec![ctrl('b')]),
        ("terminal", vec![plain(KeyCode::F(3))]),
        ("voice", vec![ctrl(' ')]),
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

    /// Keys bound to more than one action, each with those actions in the order they are
    /// defined: the first is the one the key triggers, the others never hear of it.
    pub fn duplicates(&self) -> Vec<(KeyCombo, Vec<&str>)> {
        let mut found: Vec<(KeyCombo, Vec<&str>)> = Vec::new();
        for (action, combos) in &self.bindings {
            for combo in combos {
                match found.iter_mut().find(|(key, _)| key == combo) {
                    Some((_, actions)) if actions.contains(&action.as_str()) => {}
                    Some((_, actions)) => actions.push(action),
                    None => found.push((*combo, vec![action])),
                }
            }
        }
        found.retain(|(_, actions)| actions.len() > 1);
        found
    }
}

/// Why a terminal that does not report keys the enhanced way (the kitty keyboard protocol)
/// cannot tell `key` from another key; `None` when the key arrives as itself. Windows consoles
/// report every key with its modifiers and are not meant here.
pub fn ambiguous(key: &KeyCombo) -> Option<&'static str> {
    let KeyCombo { code, mods } = KeyCombo::new(key.code, key.mods);
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let shift = mods.contains(KeyModifiers::SHIFT);
    match code {
        KeyCode::Enter if ctrl || shift => Some("arrives as plain enter"),
        KeyCode::Tab if ctrl => Some("arrives as plain tab"),
        KeyCode::Char(c) if ctrl && shift && c.is_ascii_alphabetic() => {
            Some("arrives without shift, as the ctrl key of the same letter")
        }
        KeyCode::Char('i') if ctrl => Some("is the same key as tab"),
        KeyCode::Char('m') if ctrl => Some("is the same key as enter"),
        KeyCode::Char('[') if ctrl => Some("is the same key as esc"),
        _ => None,
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap::defaults()
    }
}

/// What the characters after an `esc` make of themselves; see [`decode_escape`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Escape {
    /// A sequence that has begun and has not ended: wait for more characters.
    Incomplete,
    /// A complete sequence for this key.
    Key(KeyEvent),
    /// A complete sequence that names no key azazel knows. It is dropped, never typed.
    Ignored,
    /// Not a key sequence: the `esc` was a key of its own and these characters are typed text.
    Invalid,
}

/// Longest sequence looked at; a longer one is not a key report.
const MAX_ESCAPE: usize = 40;

/// Decodes the characters that follow an `esc` when a console hands a terminal's key report over
/// as separate key presses (the Windows console does; the terminals of other systems are read by
/// crossterm, which decodes them itself). Understands the kitty form `CSI code ; modifiers u`, the
/// xterm forms of the arrows, home, end, the function keys and `shift+tab`, with their
/// modifiers, and `SS3` keys. Any other complete `CSI` or `SS3` sequence is [`Escape::Ignored`].
pub fn decode_escape(after_esc: &str) -> Escape {
    let mut chars = after_esc.chars();
    let lead = chars.next();
    let rest: Vec<char> = chars.collect();
    match lead {
        None => Escape::Incomplete,
        Some('[') => decode_csi(&rest),
        Some('O') => match rest.as_slice() {
            [] => Escape::Incomplete,
            [last] => match xterm_key(*last) {
                Some(event) => Escape::Key(event),
                None if (' '..='~').contains(last) => Escape::Ignored,
                None => Escape::Invalid,
            },
            _ => Escape::Invalid,
        },
        Some(_) => Escape::Invalid,
    }
}

fn decode_csi(body: &[char]) -> Escape {
    let Some(end) = body.iter().position(|c| ('@'..='~').contains(c)) else {
        // Parameters and intermediate bytes only so far.
        let well_formed = body.iter().all(|c| (' '..='?').contains(c));
        return if well_formed && body.len() < MAX_ESCAPE {
            Escape::Incomplete
        } else {
            Escape::Invalid
        };
    };
    if end + 1 != body.len() || !body[..end].iter().all(|c| (' '..='?').contains(c)) {
        return Escape::Invalid;
    }
    let final_byte = body[end];
    let params: String = body[..end].iter().collect();
    let fields: Vec<&str> = params.split(';').collect();
    let number = |field: Option<&&str>, default: u32| -> Option<u32> {
        match field.copied() {
            None | Some("") => Some(default),
            Some(text) => text.split(':').next().and_then(|n| n.parse().ok()),
        }
    };
    // `modifiers[:event]`; the event is 1 press, 2 repeat, 3 release.
    let modifiers_and_kind = |field: Option<&&str>| -> Option<(KeyModifiers, KeyEventKind)> {
        let mods = number(field, 1)?;
        let kind = match field.and_then(|f| f.split(':').nth(1)) {
            Some("2") => KeyEventKind::Repeat,
            Some("3") => KeyEventKind::Release,
            _ => KeyEventKind::Press,
        };
        Some((modifier_bits(mods), kind))
    };
    let decoded = match final_byte {
        'u' => {
            let code = fields.first().copied().unwrap_or_default();
            let mut parts = code.split(':');
            let first = parts.next().and_then(|n| n.parse::<u32>().ok());
            let shifted = parts.next().and_then(|n| n.parse::<u32>().ok());
            first
                .zip(modifiers_and_kind(fields.get(1)))
                .and_then(|(code, (mods, kind))| {
                    let key = kitty_key(code, shifted, mods)?;
                    Some(KeyEvent::new_with_kind(key.0, key.1, kind))
                })
        }
        '~' => number(fields.first(), 0)
            .zip(modifiers_and_kind(fields.get(1)))
            .and_then(|(n, (mods, kind))| {
                let code = match n {
                    1 | 7 => KeyCode::Home,
                    2 => KeyCode::Insert,
                    3 => KeyCode::Delete,
                    4 | 8 => KeyCode::End,
                    5 => KeyCode::PageUp,
                    6 => KeyCode::PageDown,
                    11..=15 => KeyCode::F((n - 10) as u8),
                    17..=21 => KeyCode::F((n - 11) as u8),
                    23 | 24 => KeyCode::F((n - 12) as u8),
                    _ => return None,
                };
                Some(KeyEvent::new_with_kind(code, mods, kind))
            }),
        'Z' => Some(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
        other => modifiers_and_kind(fields.get(1)).and_then(|(mods, kind)| {
            let key = xterm_key(other)?;
            Some(KeyEvent::new_with_kind(key.code, mods, kind))
        }),
    };
    match decoded {
        Some(event) => Escape::Key(event),
        None => Escape::Ignored,
    }
}

/// The modifiers of the number a report carries: 1 plus the sum of shift 1, alt 2, ctrl 4,
/// super 8, hyper 16 and meta 32.
fn modifier_bits(number: u32) -> KeyModifiers {
    let bits = number.saturating_sub(1);
    let mut mods = KeyModifiers::NONE;
    for (bit, flag) in [
        (1, KeyModifiers::SHIFT),
        (2, KeyModifiers::ALT),
        (4, KeyModifiers::CONTROL),
        (8, KeyModifiers::SUPER),
        (16, KeyModifiers::HYPER),
        (32, KeyModifiers::META),
    ] {
        if bits & bit != 0 {
            mods |= flag;
        }
    }
    mods
}

/// The key an xterm report ends in (`A` is up, `H` is home, `P` is f1), with no modifiers.
fn xterm_key(final_byte: char) -> Option<KeyEvent> {
    let code = match final_byte {
        'A' => KeyCode::Up,
        'B' => KeyCode::Down,
        'C' => KeyCode::Right,
        'D' => KeyCode::Left,
        'H' => KeyCode::Home,
        'F' => KeyCode::End,
        'P' => KeyCode::F(1),
        'Q' => KeyCode::F(2),
        'R' => KeyCode::F(3),
        'S' => KeyCode::F(4),
        _ => return None,
    };
    Some(KeyEvent::new(code, KeyModifiers::NONE))
}

/// A key of the kitty form: its code point (and the one shift makes of it) and modifiers.
fn kitty_key(code: u32, shifted: Option<u32>, mods: KeyModifiers) -> Option<(KeyCode, KeyModifiers)> {
    let named = match code {
        13 => Some(KeyCode::Enter),
        9 if mods.contains(KeyModifiers::SHIFT) => Some(KeyCode::BackTab),
        9 => Some(KeyCode::Tab),
        27 => Some(KeyCode::Esc),
        8 | 127 => Some(KeyCode::Backspace),
        _ => None,
    };
    if let Some(code) = named {
        return Some((code, mods));
    }
    // Other control characters and the private-use area (lock keys, keypad, media keys) name no
    // key azazel has.
    let c = char::from_u32(code).filter(|c| !c.is_control() && !('\u{e000}'..='\u{f8ff}').contains(c))?;
    let c = match (
        mods.contains(KeyModifiers::SHIFT),
        shifted.and_then(char::from_u32),
    ) {
        (true, Some(shifted)) => shifted,
        (true, None) => c.to_uppercase().next().unwrap_or(c),
        _ => c,
    };
    Some((KeyCode::Char(c), mods))
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
        let expected: [(&str, &[&str]); 26] = [
            ("submit", &["enter"]),
            ("follow_up", &["alt+enter"]),
            ("newline", &["shift+enter", "ctrl+j", "ctrl+enter"]),
            ("abort", &["esc"]),
            ("clear_or_exit", &["ctrl+c"]),
            ("exit", &["ctrl+d"]),
            ("cycle_mode", &["shift+tab"]),
            ("agent_hub", &["alt+a"]),
            ("toggle_tool_output", &["ctrl+o"]),
            ("toggle_thinking", &["ctrl+t"]),
            ("model_picker", &["ctrl+l", "alt+p"]),
            ("history_search", &["ctrl+r"]),
            ("paste_image", &["ctrl+v", "alt+v"]),
            ("paste_text", &["ctrl+shift+v", "alt+shift+v"]),
            ("complete", &["tab"]),
            ("external_editor", &["ctrl+g"]),
            ("dequeue", &["alt+up", "shift+up"]),
            ("stash", &["ctrl+s"]),
            ("cycle_thinking", &["alt+t"]),
            ("cycle_model", &["ctrl+p", "alt+m"]),
            (
                "cycle_model_back",
                &["ctrl+shift+p", "alt+shift+p", "alt+shift+m"],
            ),
            ("transcript", &["f2"]),
            ("retry", &["f5"]),
            ("background", &["ctrl+b"]),
            ("terminal", &["f3"]),
            ("voice", &["ctrl+space"]),
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
    fn keys_given_to_two_actions_are_reported_with_the_action_that_gets_them() {
        // Out of the box no key is bound twice.
        assert!(Keymap::defaults().duplicates().is_empty());

        // A file that gives one key to two actions it names leaves both with it.
        let mut keymap = Keymap::defaults();
        keymap
            .merge_toml(
                "stash = [\"ctrl+o\", \"f9\"]\ntoggle_thinking = [\"ctrl+o\"]\nretry = [\"f9\", \"f9\"]\n",
            )
            .expect("valid file");
        assert_eq!(
            keymap.duplicates(),
            [
                (combo("ctrl+o"), vec!["toggle_thinking", "stash"]),
                (combo("f9"), vec!["stash", "retry"]),
            ]
        );
        // The action defined first is the one the key triggers.
        assert_eq!(
            keymap.action_for(&press(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some("toggle_thinking")
        );
        // A key taken from an action the file does not name is not a duplicate.
        assert!(keymap.keys_for("toggle_tool_output").is_empty());
    }

    fn key_of(text: &str) -> KeyEvent {
        match decode_escape(text) {
            Escape::Key(event) => event,
            other => panic!("{text:?} decoded as {other:?}"),
        }
    }

    #[test]
    fn kitty_reports_decode_with_their_modifiers() {
        assert_eq!(key_of("[13;2u"), press(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(key_of("[13u"), press(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(key_of("[13;5u"), press(KeyCode::Enter, KeyModifiers::CONTROL));
        assert_eq!(key_of("[13;3u"), press(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(key_of("[97;5u"), press(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(
            key_of("[112;6u"),
            press(KeyCode::Char('P'), KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        );
        assert_eq!(key_of("[27u"), press(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(key_of("[9;2u"), press(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(key_of("[13;2:3u").kind, KeyEventKind::Release);
        // Shift+Enter in this form is the key the keymap calls `newline`.
        assert_eq!(Keymap::defaults().action_for(&key_of("[13;2u")), Some("newline"));
    }

    #[test]
    fn xterm_reports_decode_with_their_modifiers() {
        assert_eq!(key_of("[A"), press(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(key_of("[1;2A"), press(KeyCode::Up, KeyModifiers::SHIFT));
        assert_eq!(key_of("[1;3D"), press(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(key_of("[3~"), press(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(key_of("[15~"), press(KeyCode::F(5), KeyModifiers::NONE));
        assert_eq!(key_of("[24;5~"), press(KeyCode::F(12), KeyModifiers::CONTROL));
        assert_eq!(key_of("[Z"), press(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(key_of("OP"), press(KeyCode::F(1), KeyModifiers::NONE));
        assert_eq!(key_of("OH"), press(KeyCode::Home, KeyModifiers::NONE));
    }

    #[test]
    fn unknown_complete_sequences_are_swallowed_not_typed() {
        for text in ["[57358u", "[99999999;2u", "[1;2X", "[200~", "[?1;2c", "[0m", "OZ"] {
            assert_eq!(decode_escape(text), Escape::Ignored, "{text:?}");
        }
    }

    #[test]
    fn partial_sequences_wait_and_other_text_is_not_a_sequence() {
        for text in ["", "[", "[13", "[13;", "[13;2", "O", "[1;"] {
            assert_eq!(decode_escape(text), Escape::Incomplete, "{text:?}");
        }
        for text in ["x", "hello", "[13;2uX", "[\u{e9}", "OPQ", "\n"] {
            assert_eq!(decode_escape(text), Escape::Invalid, "{text:?}");
        }
        assert_eq!(decode_escape(&format!("[{}", "1;".repeat(40))), Escape::Invalid);
    }

    #[test]
    fn keys_a_plain_terminal_cannot_tell_apart_are_known() {
        for key in [
            "shift+enter",
            "ctrl+enter",
            "ctrl+shift+p",
            "ctrl+i",
            "ctrl+m",
            "ctrl+[",
            "ctrl+tab",
        ] {
            assert!(ambiguous(&combo(key)).is_some(), "{key}");
        }
        for key in [
            "enter",
            "alt+enter",
            "ctrl+p",
            "alt+shift+m",
            "shift+tab",
            "shift+up",
            "f5",
            "ctrl+j",
        ] {
            assert_eq!(ambiguous(&combo(key)), None, "{key}");
        }
    }
}
