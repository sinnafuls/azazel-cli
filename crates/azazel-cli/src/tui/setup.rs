//! `/terminal-setup`, which makes Shift+Enter add a line in terminals that send it as plain
//! Enter, and the key conflicts `/keys` reports.
//!
//! The fix is a key binding of the terminal that types a line feed for Shift+Enter. A line feed
//! is `ctrl+j` (a Windows console hands it to a program as `ctrl+enter`), and both are keys of
//! `newline` out of the box, so nothing changes in azazel. Where
//! the terminal keeps its bindings in a file that can be edited safely (VS Code, Windows
//! Terminal, Alacritty) the binding is added there, after a copy of the file was put beside
//! it; elsewhere the binding is shown for the user to put in.

use std::path::{Path, PathBuf};

use azazel_tui::keys::ambiguous;
use azazel_tui::term::Terminal;
use azazel_tui::{KeyCombo, Keymap, SelectItem};

use super::Tui;
use super::overlay::{Action, Overlay};

/// What `/terminal-setup` found.
pub(super) enum Plan {
    /// Shift+Enter reaches azazel as a key of its own; says how.
    Works(String),
    /// This file of the terminal has the binding already.
    Present(PathBuf),
    /// The binding can be added to a file of the terminal.
    Write(Change),
    /// The binding has to be put in by hand: where, and what.
    Show { place: String, snippet: String },
    /// Not a terminal there is a fix for.
    Unknown,
}

/// A file of the terminal with the binding added.
pub(super) struct Change {
    pub file: PathBuf,
    /// The whole file as it will be.
    pub updated: String,
    /// What was added, to show.
    pub snippet: String,
}

impl Change {
    /// Where the file as it is now is kept.
    fn backup(&self) -> PathBuf {
        let mut name = self.file.file_name().unwrap_or_default().to_os_string();
        name.push(".azazel-backup");
        self.file.with_file_name(name)
    }

    /// Writes the file, after copying what is there now beside it. Returns the copy, when
    /// there was something to copy.
    fn apply(&self) -> std::io::Result<Option<PathBuf>> {
        let backup = self.file.is_file().then(|| self.backup());
        if let Some(backup) = &backup {
            std::fs::copy(&self.file, backup)?;
        }
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.file, &self.updated)?;
        Ok(backup)
    }
}

/// The binding for VS Code's `keybindings.json`.
const VSCODE_BINDING: [&str; 6] = [
    "{",
    "    \"key\": \"shift+enter\",",
    "    \"command\": \"workbench.action.terminal.sendSequence\",",
    "    \"args\": { \"text\": \"\\n\" },",
    "    \"when\": \"terminalFocus\"",
    "}",
];
/// The binding for the `actions` of Windows Terminal's `settings.json`.
const WINDOWS_TERMINAL_BINDING: [&str; 1] =
    ["{ \"command\": { \"action\": \"sendInput\", \"input\": \"\\n\" }, \"keys\": \"shift+enter\" }"];
/// The binding for `alacritty.toml`.
const ALACRITTY_BINDING: &str = "[[keyboard.bindings]]\nkey = \"Return\"\nmods = \"Shift\"\nchars = \"\\n\"";

/// What to do about Shift+Enter in `terminal`. `enhanced` says that the terminal reports keys
/// the enhanced way, `var` reads an environment variable, `home` is the home directory.
pub(super) fn plan(
    terminal: Terminal,
    enhanced: bool,
    var: &dyn Fn(&str) -> Option<String>,
    home: Option<&Path>,
) -> Plan {
    let show = |place: &str, snippet: &str| Plan::Show {
        place: place.to_string(),
        snippet: snippet.to_string(),
    };
    if enhanced {
        return Plan::Works(format!(
            "{} reports Shift+Enter as a key of its own",
            terminal.name()
        ));
    }
    match terminal {
        // To a Windows program these two hand every key over with its modifiers.
        Terminal::WindowsTerminal | Terminal::WezTerm if cfg!(windows) => Plan::Works(format!(
            "{} reports Shift+Enter as a key of its own",
            terminal.name()
        )),
        Terminal::WindowsTerminal => windows_terminal(),
        Terminal::VsCode => vscode(var, home),
        Terminal::Alacritty => alacritty(var, home),
        Terminal::Kitty => show("kitty.conf", "map shift+enter send_text all \\n"),
        Terminal::Ghostty => show("Ghostty's config file", "keybind = shift+enter=text:\\n"),
        Terminal::WezTerm => show(
            "wezterm.lua, before the configuration is returned",
            "config.keys = {\n  { key = 'Enter', mods = 'SHIFT', action = wezterm.action.SendString '\\n' },\n}",
        ),
        Terminal::Iterm2 => show(
            "iTerm2 > Settings > Profiles > Keys > Key Mappings > +",
            "Keyboard Shortcut: Shift+Enter\nAction: Send Text with \"vim\" Special Chars\nText: \\n",
        ),
        Terminal::AppleTerminal | Terminal::Other => Plan::Unknown,
    }
}

/// The plan for a file whose present content is `source`, given what `edit` makes of it.
fn file_plan(file: PathBuf, edit: Option<Edit>, snippet: String, place: &str) -> Plan {
    match edit {
        Some(Edit::Present) => Plan::Present(file),
        Some(Edit::Updated(updated)) => Plan::Write(Change {
            file,
            updated,
            snippet,
        }),
        // A file that cannot be made sense of is left alone.
        None => Plan::Show {
            place: place.to_string(),
            snippet,
        },
    }
}

// ------------------------------------------------------------ VS Code

/// The directory name of the editor whose terminal this is: VS Code itself or one built from
/// it. Its variables carry the path of the program.
fn vscode_product(var: &dyn Fn(&str) -> Option<String>) -> &'static str {
    let hints = [
        "VSCODE_GIT_ASKPASS_NODE",
        "VSCODE_GIT_ASKPASS_MAIN",
        "TERM_PROGRAM_VERSION",
    ]
    .iter()
    .filter_map(|name| var(name))
    .collect::<Vec<_>>()
    .join(" ")
    .to_lowercase();
    if hints.contains("cursor") {
        "Cursor"
    } else if hints.contains("windsurf") {
        "Windsurf"
    } else if hints.contains("codium") {
        "VSCodium"
    } else if hints.contains("insider") {
        "Code - Insiders"
    } else {
        "Code"
    }
}

/// Where the editor keeps the user's settings on this machine.
fn vscode_user_dir(var: &dyn Fn(&str) -> Option<String>, home: Option<&Path>) -> Option<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(var("APPDATA")?)
    } else if cfg!(target_os = "macos") {
        home?.join("Library").join("Application Support")
    } else {
        match var("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => home?.join(".config"),
        }
    };
    Some(base.join(vscode_product(var)).join("User"))
}

fn vscode(var: &dyn Fn(&str) -> Option<String>, home: Option<&Path>) -> Plan {
    let snippet = VSCODE_BINDING.join("\n");
    let place = "keybindings.json of the VS Code window this terminal is in \
                 (Command Palette > Preferences: Open Keyboard Shortcuts (JSON))";
    // With the window on another machine (SSH, WSL, a container), so is its key bindings file.
    let elsewhere = [
        "SSH_CONNECTION",
        "WSL_DISTRO_NAME",
        "REMOTE_CONTAINERS",
        "CODESPACES",
    ]
    .iter()
    .any(|name| var(name).is_some_and(|value| !value.is_empty()));
    let dir = vscode_user_dir(var, home).filter(|dir| !elsewhere && dir.is_dir());
    let Some(dir) = dir else {
        return Plan::Show {
            place: place.to_string(),
            snippet,
        };
    };
    let file = dir.join("keybindings.json");
    let source = std::fs::read_to_string(&file).unwrap_or_default();
    file_plan(file, vscode_edit(&source), snippet, place)
}

/// What becomes of a file that gets a binding added.
#[derive(Debug, PartialEq, Eq)]
enum Edit {
    /// It has the binding.
    Present,
    Updated(String),
}

/// `keybindings.json` with the binding. `None` when the file is not the list it should be.
fn vscode_edit(source: &str) -> Option<Edit> {
    let bound = |value: &serde_json::Value| {
        value.as_array().is_some_and(|bindings| {
            bindings.iter().any(|binding| {
                binding["key"] == "shift+enter"
                    && binding["command"] == "workbench.action.terminal.sendSequence"
            })
        })
    };
    if tokens(source)?.is_empty() {
        // Nothing but comments, if anything: they stay above the list.
        let above = match source {
            "" => "",
            source if source.ends_with('\n') => "",
            _ => "\n",
        };
        return Some(Edit::Updated(format!(
            "{source}{above}[\n    {}\n]\n",
            VSCODE_BINDING.join("\n    ")
        )));
    }
    if bound(&parsed(source)?) {
        return Some(Edit::Present);
    }
    let updated = insert_into_list(source, None, &VSCODE_BINDING)?;
    // Only a file that still reads as it should, with the binding in it, is written.
    bound(&parsed(&updated)?).then_some(Edit::Updated(updated))
}

// ------------------------------------------------------------ Windows Terminal

/// From WSL: the settings of the one Windows Terminal installed for the one user who has it.
fn windows_terminal() -> Plan {
    let snippet = WINDOWS_TERMINAL_BINDING.join("\n");
    let place = "the \"actions\" list of Windows Terminal's settings.json (Settings > Open JSON file)";
    let found: Vec<PathBuf> = std::fs::read_dir("/mnt/c/Users")
        .into_iter()
        .flatten()
        .flatten()
        .map(|user| user.path().join("AppData/Local/Packages"))
        .flat_map(|packages| {
            [
                "Microsoft.WindowsTerminal_8wekyb3d8bbwe",
                "Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe",
            ]
            .map(|package| packages.join(package).join("LocalState/settings.json"))
        })
        .filter(|file| file.is_file())
        .collect();
    let [file] = found.as_slice() else {
        return Plan::Show {
            place: place.to_string(),
            snippet,
        };
    };
    let edit = std::fs::read_to_string(file)
        .ok()
        .and_then(|source| windows_terminal_edit(&source));
    file_plan(file.clone(), edit, snippet, place)
}

/// Windows Terminal's `settings.json` with the binding among its `actions`.
fn windows_terminal_edit(source: &str) -> Option<Edit> {
    let bound = |value: &serde_json::Value| {
        ["actions", "keybindings"].iter().any(|list| {
            value[*list].as_array().is_some_and(|bindings| {
                bindings.iter().any(|binding| match &binding["keys"] {
                    serde_json::Value::String(keys) => keys == "shift+enter",
                    serde_json::Value::Array(keys) => keys.iter().any(|key| key == "shift+enter"),
                    _ => false,
                })
            })
        })
    };
    let settings = parsed(source)?;
    if !settings.is_object() {
        return None;
    }
    if bound(&settings) {
        return Some(Edit::Present);
    }
    let updated = insert_into_list(source, Some("actions"), &WINDOWS_TERMINAL_BINDING)?;
    bound(&parsed(&updated)?).then_some(Edit::Updated(updated))
}

// ------------------------------------------------------------ Alacritty

fn alacritty(var: &dyn Fn(&str) -> Option<String>, home: Option<&Path>) -> Plan {
    let place = "alacritty.toml";
    let mut dirs: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        dirs.extend(var("APPDATA").map(|dir| PathBuf::from(dir).join("alacritty")));
    } else {
        dirs.extend(
            var("XDG_CONFIG_HOME")
                .filter(|dir| !dir.is_empty())
                .map(|dir| PathBuf::from(dir).join("alacritty")),
        );
        dirs.extend(home.map(|home| home.join(".config").join("alacritty")));
    }
    let existing = dirs
        .iter()
        .map(|dir| dir.join("alacritty.toml"))
        .chain(home.map(|home| home.join(".alacritty.toml")))
        .find(|file| file.is_file());
    // An Alacritty that is still set up in YAML would not read a TOML file put beside it.
    let legacy = dirs.iter().any(|dir| dir.join("alacritty.yml").is_file());
    let file = match (existing, dirs.first()) {
        (Some(file), _) => file,
        (None, Some(dir)) if !legacy => dir.join("alacritty.toml"),
        _ => {
            return Plan::Show {
                place: place.to_string(),
                snippet: ALACRITTY_BINDING.to_string(),
            };
        }
    };
    let source = std::fs::read_to_string(&file).unwrap_or_default();
    file_plan(
        file,
        alacritty_edit(&source),
        ALACRITTY_BINDING.to_string(),
        place,
    )
}

fn item_text(item: Option<&toml_edit::Item>) -> Option<&str> {
    item.and_then(toml_edit::Item::as_str)
}

fn value_text(value: Option<&toml_edit::Value>) -> Option<&str> {
    value.and_then(toml_edit::Value::as_str)
}

/// `alacritty.toml` with the binding among its `keyboard.bindings`.
fn alacritty_edit(source: &str) -> Option<Edit> {
    let mut document: toml_edit::DocumentMut = source.parse().ok()?;
    let ours = |key: Option<&str>, mods: Option<&str>| {
        key.is_some_and(|key| key.eq_ignore_ascii_case("return") || key.eq_ignore_ascii_case("enter"))
            && mods.is_some_and(|mods| mods.eq_ignore_ascii_case("shift"))
    };
    let appended = || {
        let gap = match source {
            "" => "",
            source if source.ends_with('\n') => "\n",
            _ => "\n\n",
        };
        Edit::Updated(format!("{source}{gap}{ALACRITTY_BINDING}\n"))
    };
    let bindings = document
        .get_mut("keyboard")
        .and_then(|keyboard| keyboard.get_mut("bindings"));
    match bindings {
        None => Some(appended()),
        Some(toml_edit::Item::ArrayOfTables(tables)) => {
            let present = tables
                .iter()
                .any(|table| ours(item_text(table.get("key")), item_text(table.get("mods"))));
            Some(if present { Edit::Present } else { appended() })
        }
        // Written as one list of inline tables: a `[[keyboard.bindings]]` after it would be
        // an error, so the binding joins the list.
        Some(toml_edit::Item::Value(toml_edit::Value::Array(list))) => {
            let present = list
                .iter()
                .filter_map(toml_edit::Value::as_inline_table)
                .any(|table| ours(value_text(table.get("key")), value_text(table.get("mods"))));
            if present {
                return Some(Edit::Present);
            }
            let mut binding = toml_edit::InlineTable::new();
            binding.insert("key", "Return".into());
            binding.insert("mods", "Shift".into());
            binding.insert("chars", "\n".into());
            list.push(binding);
            Some(Edit::Updated(document.to_string()))
        }
        Some(_) => None,
    }
}

// ------------------------------------------------------------ JSON with comments

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Open,
    Close,
    Comma,
    Colon,
    Text,
    Scalar,
}

/// One piece of a JSON file that may have comments and trailing commas, as these terminals'
/// files do: its kind and where it is.
#[derive(Debug, Clone, Copy)]
struct Token {
    kind: Kind,
    start: usize,
    end: usize,
}

/// The pieces of `source`, comments and white space left out. `None` when a string or a
/// comment does not end.
fn tokens(source: &str) -> Option<Vec<Token>> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        let kind = match bytes[at] {
            byte if byte.is_ascii_whitespace() => {
                at += 1;
                continue;
            }
            // A byte order mark.
            0xEF if bytes[at..].starts_with(&[0xEF, 0xBB, 0xBF]) => {
                at += 3;
                continue;
            }
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                at = source[at..]
                    .find('\n')
                    .map_or(bytes.len(), |line_end| at + line_end);
                continue;
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at = at + 2 + source[at + 2..].find("*/")? + 2;
                continue;
            }
            b'{' | b'[' => Kind::Open,
            b'}' | b']' => Kind::Close,
            b',' => Kind::Comma,
            b':' => Kind::Colon,
            b'"' => {
                at += 1;
                loop {
                    match bytes.get(at)? {
                        b'\\' => at += 2,
                        b'"' => break,
                        _ => at += 1,
                    }
                }
                Kind::Text
            }
            _ => {
                let ends = |byte: u8| byte.is_ascii_whitespace() || b",:{}[]/\"".contains(&byte);
                while bytes.get(at + 1).is_some_and(|next| !ends(*next)) {
                    at += 1;
                }
                Kind::Scalar
            }
        };
        at += 1;
        out.push(Token { kind, start, end: at });
    }
    Some(out)
}

/// `source` read as JSON, its comments and trailing commas forgiven.
fn parsed(source: &str) -> Option<serde_json::Value> {
    let tokens = tokens(source)?;
    let mut plain = String::with_capacity(source.len());
    for (index, token) in tokens.iter().enumerate() {
        let before_close = tokens.get(index + 1).is_some_and(|next| next.kind == Kind::Close);
        if token.kind != Kind::Comma || !before_close {
            plain.push_str(&source[token.start..token.end]);
        }
    }
    serde_json::from_str(&plain).ok()
}

/// The white space the line of `at` starts with.
fn line_indent(source: &str, at: usize) -> &str {
    let line = &source[source[..at].rfind('\n').map_or(0, |end| end + 1)..];
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

/// `source` with `entry` (its lines) added at the end of a list: the list the file is when
/// `key` is `None`, else the list of that name in the object the file is, which is made when
/// the object has none. Everything else in the file, comments included, stays as it is.
fn insert_into_list(source: &str, key: Option<&str>, entry: &[&str]) -> Option<String> {
    const STEP: &str = "    ";
    let tokens = tokens(source)?;
    let is = |token: &Token, text: &str| &source[token.start..token.end] == text;
    let first = tokens.first()?;
    let open = match key {
        None => is(first, "[").then_some(0)?,
        Some(key) => {
            if !is(first, "{") {
                return None;
            }
            let mut depth = 0usize;
            let mut found = None;
            for (index, token) in tokens.iter().enumerate() {
                match token.kind {
                    Kind::Open => depth += 1,
                    Kind::Close => depth = depth.checked_sub(1)?,
                    Kind::Text
                        if depth == 1
                            && source[token.start + 1..token.end - 1] == *key
                            && tokens.get(index + 1).is_some_and(|next| next.kind == Kind::Colon) =>
                    {
                        // Something else under that name is not ours to change.
                        tokens.get(index + 2).filter(|list| is(list, "["))?;
                        found = Some(index + 2);
                        break;
                    }
                    _ => {}
                }
            }
            match found {
                Some(open) => open,
                None => {
                    // No such list yet: it becomes the first member of the object.
                    let inner = format!("{}{STEP}", line_indent(source, first.start));
                    let lines = entry.join(&format!("\n{inner}{STEP}"));
                    let comma = match tokens.get(1) {
                        Some(next) if next.kind != Kind::Close => ",",
                        _ => "",
                    };
                    let member = format!("\n{inner}\"{key}\": [\n{inner}{STEP}{lines}\n{inner}]{comma}");
                    let mut updated = source.to_string();
                    updated.insert_str(first.end, &member);
                    // An object that was `{}` on one line closes on a line of its own now.
                    let close = tokens.get(1)?.start;
                    if comma.is_empty() && source[first.end..close].trim().is_empty() {
                        let after = first.end + member.len();
                        updated.replace_range(after..close + member.len(), "\n");
                    }
                    return Some(updated);
                }
            }
        }
    };
    // The end of the list.
    let mut depth = 0usize;
    let mut close = None;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        match token.kind {
            Kind::Open => depth += 1,
            Kind::Close => {
                depth -= 1;
                if depth == 0 {
                    close = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let base = line_indent(source, tokens[open].start);
    let mut updated = source.to_string();
    if close == open + 1 {
        let inner = format!("{base}{STEP}");
        let lines = entry.join(&format!("\n{inner}"));
        let (from, to) = (tokens[open].end, tokens[close].start);
        if source[from..to].trim().is_empty() {
            updated.replace_range(from..to, &format!("\n{inner}{lines}\n{base}"));
            return Some(updated);
        }
        // A list with nothing but comments in it keeps them: the entry goes above the line
        // the list closes on.
        let line = source[..to].rfind('\n').map_or(0, |end| end + 1);
        if source[line..to].trim().is_empty() {
            updated.insert_str(line, &format!("{inner}{lines}\n"));
        } else {
            updated.insert_str(to, &format!("\n{inner}{lines}\n{base}"));
        }
        return Some(updated);
    }
    // The entries that are there say how far in an entry goes.
    let head = tokens[open + 1].start;
    let own_line = source[tokens[open].end..head].contains('\n');
    let inner = if own_line {
        line_indent(source, head).to_string()
    } else {
        format!("{base}{STEP}")
    };
    let last = tokens[close - 1];
    let lines = entry.join(&format!("\n{inner}"));
    // A comment after the last entry on its line stays with that entry: the new one starts
    // on the line below, unless the list closes on this line or a comment runs past it.
    let line_end = source[last.end..]
        .find('\n')
        .map_or(source.len(), |end| last.end + end);
    let at = match line_end <= tokens[close].start && !source[last.end..line_end].contains("/*") {
        true => line_end,
        false => last.end,
    };
    updated.insert_str(at, &format!("\n{inner}{lines}"));
    if last.kind != Kind::Comma {
        updated.insert(last.end, ',');
    }
    Some(updated)
}

// ------------------------------------------------------------ key conflicts

/// What is wrong with the key bindings as they are: keys given to two actions, and keys of an
/// action that the terminal is known to keep for itself or cannot tell from another key.
/// `plain_keys` says that the terminal reports keys the old way, where some look alike;
/// `shift_enter_set_up` that the terminal has the binding `/terminal-setup` makes.
pub(super) fn key_conflicts(
    keymap: &Keymap,
    terminal: Terminal,
    tmux: bool,
    plain_keys: bool,
    shift_enter_set_up: bool,
) -> Vec<String> {
    let mut found: Vec<String> = keymap
        .duplicates()
        .into_iter()
        .map(|(key, actions)| {
            format!(
                "{key} is bound to {}: {} gets it",
                actions.join(" and "),
                actions[0]
            )
        })
        .collect();
    let shift_enter: KeyCombo = KeyCombo::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::SHIFT,
    );
    let prefix = KeyCombo::new(
        crossterm::event::KeyCode::Char('b'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    let name = terminal.name();
    let actions: Vec<&str> = keymap.actions().collect();
    for action in actions {
        let keys = keymap.keys_for(action);
        let trouble = |key: &KeyCombo| -> Option<String> {
            if let Some(what) = terminal.takes(key) {
                return Some(format!("does not reach azazel: {name} {what}"));
            }
            if tmux && *key == prefix {
                return Some("does not reach azazel: tmux takes it as its prefix key".to_string());
            }
            // VS Code's terminal sends Enter with a modifier as Enter on every system, and so
            // does Alacritty to a Windows program.
            let enter_only =
                terminal == Terminal::VsCode || (cfg!(windows) && terminal == Terminal::Alacritty);
            let plain = plain_keys || (enter_only && key.code == crossterm::event::KeyCode::Enter);
            let why = ambiguous(key).filter(|_| plain)?;
            if *key == shift_enter {
                (!shift_enter_set_up).then(|| format!("{why} in {name}; /terminal-setup can change that"))
            } else {
                Some(format!("{why} in {name}"))
            }
        };
        let fine: Vec<String> = keys
            .iter()
            .filter(|key| trouble(key).is_none())
            .map(ToString::to_string)
            .collect();
        for key in keys {
            if let Some(trouble) = trouble(key) {
                let instead = match fine.is_empty() {
                    true => String::new(),
                    false => format!(" ({} works)", fine.join(", ")),
                };
                found.push(format!("{key} ({action}) {trouble}{instead}"));
            }
        }
    }
    found
}

impl Tui {
    /// The terminal program the UI is in.
    fn terminal_program() -> Terminal {
        Terminal::detect(&|name| std::env::var(name).ok())
    }

    fn setup_plan(&self) -> Plan {
        let home = std::env::home_dir();
        plan(
            Self::terminal_program(),
            self.guard.enhanced_keys,
            &|name| std::env::var(name).ok(),
            home.as_deref(),
        )
    }

    /// `/terminal-setup`: says whether Shift+Enter adds a line in this terminal, and offers
    /// the key binding that makes it when it does not.
    pub(super) fn terminal_setup(&mut self) {
        let name = Self::terminal_program().name();
        // What the line feed the terminal will type arrives as.
        let (code, line_feed) = match cfg!(windows) {
            true => (crossterm::event::KeyCode::Enter, "ctrl+enter"),
            false => (crossterm::event::KeyCode::Char('j'), "ctrl+j"),
        };
        let arrives = KeyCombo::new(code, crossterm::event::KeyModifiers::CONTROL);
        if !self.keymap.keys_for("newline").contains(&arrives) {
            return self.warn(&format!(
                "the fix has the terminal type a line feed for Shift+Enter, which arrives as {line_feed}, and \
                 your key bindings no longer give that key to `newline`: add \"{line_feed}\" to `newline` in \
                 the key bindings file first"
            ));
        }
        match self.setup_plan() {
            Plan::Works(how) => self.info(&format!(
                "{how}: Shift+Enter adds a line, nothing to set up. Ctrl+J adds one too."
            )),
            Plan::Present(file) => self.info(&format!(
                "{} already has a Shift+Enter binding for the terminal: nothing to change",
                file.display()
            )),
            Plan::Show { place, snippet } => self.info(&format!(
                "{name} sends Shift+Enter as plain Enter. To have it add a line, put this in {place}:\n\n\
                 {snippet}\n\nUntil then Ctrl+J adds a line."
            )),
            Plan::Unknown => self.info(
                "azazel has no Shift+Enter fix for this terminal. Ctrl+J adds a line everywhere; Shift+Enter \
                 does where the terminal reports keys the enhanced way (the kitty keyboard protocol).",
            ),
            Plan::Write(change) => {
                let mut body = vec![
                    format!("{name} sends Shift+Enter as plain Enter. With this key binding it adds a line:"),
                    String::new(),
                ];
                body.extend(change.snippet.lines().map(str::to_string));
                body.push(String::new());
                body.push(format!(
                    "It goes into {}. The file as it is now is kept as {}.",
                    change.file.display(),
                    change.backup().file_name().unwrap_or_default().to_string_lossy()
                ));
                let items = ["Add the key binding", "Show it and change nothing"]
                    .iter()
                    .map(|label| SelectItem::new(*label, ""))
                    .collect();
                self.overlay = Some(Overlay::select(
                    "Terminal setup",
                    body,
                    items,
                    false,
                    Action::TerminalSetup(Box::new(change)),
                ));
            }
        }
    }

    /// The answer to `/terminal-setup`'s question.
    pub(super) fn finish_terminal_setup(&mut self, change: &Change, choice: usize) {
        if choice != 0 {
            return self.info(&format!(
                "For Shift+Enter to add a line, put this in {}:\n\n{}",
                change.file.display(),
                change.snippet
            ));
        }
        match change.apply() {
            Ok(backup) => {
                let kept = match backup {
                    Some(backup) => format!(" (the file as it was: {})", backup.display()),
                    None => String::new(),
                };
                self.info(&format!(
                    "added the Shift+Enter binding to {}{kept}. The terminal reads it by itself; \
                     Shift+Enter adds a line from now on.",
                    change.file.display()
                ));
            }
            Err(e) => self.fail(&format!("could not write {}: {e}", change.file.display())),
        }
    }

    /// The conflicts `/keys` lists under the bindings.
    pub(super) fn key_conflicts(&self) -> Vec<String> {
        let plain_keys = !cfg!(windows) && !self.guard.enhanced_keys;
        key_conflicts(
            &self.keymap,
            Self::terminal_program(),
            self.caps.tmux,
            plain_keys,
            matches!(self.setup_plan(), Plan::Present(_)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, value)| value.clone())
        }
    }

    fn updated(edit: Option<Edit>) -> String {
        match edit {
            Some(Edit::Updated(updated)) => updated,
            other => panic!("expected an updated file, got {other:?}"),
        }
    }

    #[test]
    fn the_binding_joins_a_vscode_keybindings_file_and_nothing_else_changes() {
        // No file, or an empty one: the file is the binding.
        let fresh = updated(vscode_edit(""));
        assert_eq!(
            fresh,
            "[\n    {\n        \"key\": \"shift+enter\",\n        \"command\": \
             \"workbench.action.terminal.sendSequence\",\n        \"args\": { \"text\": \"\\n\" },\n        \
             \"when\": \"terminalFocus\"\n    }\n]\n"
        );
        let commented = updated(vscode_edit("// Place your key bindings in this file"));
        assert!(commented.starts_with("// Place your key bindings in this file\n[\n    {\n"));
        assert_eq!(parsed(&commented).unwrap()[0]["key"], "shift+enter");
        let listed = updated(vscode_edit("// Place your key bindings in this file\n[]\n"));
        assert_eq!(
            listed.replace("// Place your key bindings in this file\n", ""),
            fresh
        );
        // What the terminal is sent is a line feed.
        let bindings: serde_json::Value = serde_json::from_str(&fresh).unwrap();
        assert_eq!(bindings[0]["args"]["text"], "\n");
        // Run again, there is nothing to add.
        assert_eq!(vscode_edit(&fresh), Some(Edit::Present));

        // A file with comments, a trailing comma and bindings of its own keeps all of them.
        let theirs = "// Place your key bindings in this file to override the defaults\n[\n  {\n    \"key\": \
                      \"ctrl+k ctrl+t\", // a comment with ] and \" in it\n    \"command\": \
                      \"workbench.action.selectTheme\"\n  },\n  /* another */\n]\n";
        let with = updated(vscode_edit(theirs));
        assert!(with.starts_with(&theirs[..theirs.rfind("  },").unwrap() + 4]));
        assert!(with.ends_with("\n  /* another */\n]\n"), "{with}");
        assert!(
            with.contains("  },\n  {\n      \"key\": \"shift+enter\","),
            "{with}"
        );
        let read = parsed(&with).unwrap();
        assert_eq!(read.as_array().unwrap().len(), 2);
        assert_eq!(read[0]["command"], "workbench.action.selectTheme");
        assert_eq!(read[1]["key"], "shift+enter");
        assert_eq!(vscode_edit(&with), Some(Edit::Present));

        // Without a trailing comma one is put after the last binding; on one line too.
        let tight = "[{\"key\": \"f5\", \"command\": \"x\"}]";
        let with = updated(vscode_edit(tight));
        assert!(
            with.starts_with("[{\"key\": \"f5\", \"command\": \"x\"},\n    {\n"),
            "{with}"
        );
        assert_eq!(parsed(&with).unwrap().as_array().unwrap().len(), 2);

        // Comments stay where they are: after the last binding on its line, and in a list
        // that has nothing else.
        let with = updated(vscode_edit(
            "[\n  { \"key\": \"f5\", \"command\": \"x\" } // mine\n  // more below\n]\n",
        ));
        assert!(
            with.starts_with(
                "[\n  { \"key\": \"f5\", \"command\": \"x\" }, // mine\n  {\n      \"key\": \"shift+enter\","
            ),
            "{with}"
        );
        assert!(with.ends_with("  }\n  // more below\n]\n"), "{with}");
        let with = updated(vscode_edit(
            "[\n    // {\n    //   \"key\": \"an example\"\n    // }\n]\n",
        ));
        assert!(
            with.starts_with("[\n    // {\n    //   \"key\": \"an example\"\n    // }\n    {\n        \"key\": \"shift+enter\","),
            "{with}"
        );
        assert!(with.ends_with("    }\n]\n"), "{with}");
        let with = updated(vscode_edit("[ /* none yet */ ]"));
        assert!(with.starts_with("[ /* none yet */ \n    {\n"), "{with}");
        assert_eq!(parsed(&with).unwrap().as_array().unwrap().len(), 1);
        let with = updated(vscode_edit("[{\"key\": \"f5\"} /* one\n two */\n]"));
        assert!(with.starts_with("[{\"key\": \"f5\"},\n    {\n"), "{with}");
        assert!(with.ends_with("    } /* one\n two */\n]"), "{with}");

        // A file that is not a list of bindings is not touched.
        assert_eq!(vscode_edit("{\"key\": \"f5\"}"), None);
        assert_eq!(vscode_edit("[ \"unterminated"), None);
        assert_eq!(vscode_edit("[ /* never closed"), None);
    }

    #[test]
    fn the_binding_joins_the_actions_of_windows_terminal_settings() {
        let theirs = "{\n    \"$help\": \"https://aka.ms/terminal-documentation\",\n    \"actions\": \n    [\n        \
                      {\n            \"command\": { \"action\": \"copy\", \"singleLine\": false },\n            \
                      \"id\": \"User.copy\"\n        }\n    ],\n    \"profiles\": { \"list\": [] } // kept\n}\n";
        let with = updated(windows_terminal_edit(theirs));
        assert!(
            with.contains(
                "\"id\": \"User.copy\"\n        },\n        { \"command\": { \"action\": \"sendInput\", \
                 \"input\": \"\\n\" }, \"keys\": \"shift+enter\" }\n    ],"
            ),
            "{with}"
        );
        assert!(with.ends_with("\"profiles\": { \"list\": [] } // kept\n}\n"));
        let read = parsed(&with).unwrap();
        assert_eq!(read["actions"].as_array().unwrap().len(), 2);
        assert_eq!(read["actions"][1]["command"]["input"], "\n");
        assert_eq!(read["profiles"]["list"], serde_json::json!([]));
        assert_eq!(windows_terminal_edit(&with), Some(Edit::Present));

        // An empty list, and settings without the list at all.
        let with = updated(windows_terminal_edit(
            "{\n    \"actions\": [],\n    \"theme\": \"dark\"\n}\n",
        ));
        assert_eq!(parsed(&with).unwrap()["actions"][0]["keys"], "shift+enter");
        assert_eq!(parsed(&with).unwrap()["theme"], "dark");
        let with = updated(windows_terminal_edit("{\n    \"theme\": \"dark\"\n}\n"));
        assert!(
            with.starts_with("{\n    \"actions\": [\n        { \"command\""),
            "{with}"
        );
        assert_eq!(parsed(&with).unwrap()["theme"], "dark");
        let with = updated(windows_terminal_edit("{}"));
        assert_eq!(parsed(&with).unwrap()["actions"][0]["keys"], "shift+enter");
        assert!(with.ends_with("\n    ]\n}"), "{with}");
        let with = updated(windows_terminal_edit("{ // nothing set\n}\n"));
        assert!(with.ends_with("    ] // nothing set\n}\n"), "{with}");
        assert_eq!(parsed(&with).unwrap()["actions"][0]["keys"], "shift+enter");

        // A binding of Shift+Enter that is there, in either list and either form, is left.
        let old = "{ \"keybindings\": [ { \"command\": \"paste\", \"keys\": [\"shift+enter\"] } ] }";
        assert_eq!(windows_terminal_edit(old), Some(Edit::Present));
        assert_eq!(windows_terminal_edit("[]"), None);
        assert_eq!(windows_terminal_edit("{ \"actions\": 3 }"), None);
    }

    #[test]
    fn the_binding_joins_an_alacritty_configuration_in_the_form_it_uses() {
        assert_eq!(
            updated(alacritty_edit("")),
            "[[keyboard.bindings]]\nkey = \"Return\"\nmods = \"Shift\"\nchars = \"\\n\"\n"
        );
        let theirs = "[font]\nsize = 12.0\n\n[[keyboard.bindings]]\nkey = \"N\"\nmods = \"Control|Shift\"\naction = \
                      \"CreateNewWindow\"";
        let with = updated(alacritty_edit(theirs));
        assert!(with.starts_with(theirs));
        let read: toml::Table = with.parse().unwrap();
        let bindings = read["keyboard"]["bindings"].as_array().unwrap();
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[1]["chars"].as_str(), Some("\n"));
        assert_eq!(alacritty_edit(&with), Some(Edit::Present));

        // Bindings written as one list keep that form.
        let inline = "[keyboard]\nbindings = [\n  { key = \"N\", mods = \"Control|Shift\", action = \
                      \"CreateNewWindow\" },\n]\n";
        let with = updated(alacritty_edit(inline));
        let read: toml::Table = with.parse().unwrap();
        let bindings = read["keyboard"]["bindings"].as_array().unwrap();
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[1]["key"].as_str(), Some("Return"));
        assert_eq!(bindings[1]["chars"].as_str(), Some("\n"));
        assert_eq!(alacritty_edit(&with), Some(Edit::Present));

        assert_eq!(alacritty_edit("[keyboard]\nbindings = 3\n"), None);
        assert_eq!(alacritty_edit("not = [toml"), None);
    }

    #[test]
    fn the_plan_follows_the_terminal_and_what_its_files_hold() {
        let home = tempfile::tempdir().unwrap();
        let none = env(&[]);
        // A terminal that reports keys the enhanced way needs nothing, whichever it is.
        assert!(matches!(
            plan(Terminal::VsCode, true, &none, Some(home.path())),
            Plan::Works(_)
        ));
        assert!(matches!(
            plan(Terminal::Other, false, &none, Some(home.path())),
            Plan::Unknown
        ));
        // WezTerm hands a Windows program every key with its modifiers.
        assert_eq!(
            matches!(
                plan(Terminal::WezTerm, false, &none, Some(home.path())),
                Plan::Works(_)
            ),
            cfg!(windows)
        );
        let shown = match cfg!(windows) {
            true => vec![Terminal::Kitty, Terminal::Iterm2, Terminal::Ghostty],
            false => vec![
                Terminal::Kitty,
                Terminal::WezTerm,
                Terminal::Iterm2,
                Terminal::Ghostty,
            ],
        };
        for terminal in shown {
            let Plan::Show { snippet, .. } = plan(terminal, false, &none, Some(home.path())) else {
                panic!("{terminal:?} is shown its binding")
            };
            assert!(snippet.contains("\\n"), "{snippet}");
        }

        // VS Code: the user directory of the editor this is, found through its variables.
        let config = home.path().join("config");
        let user = config.join("Cursor").join("User");
        let vars = [
            ("APPDATA", config.to_str().unwrap()),
            ("XDG_CONFIG_HOME", config.to_str().unwrap()),
            ("VSCODE_GIT_ASKPASS_NODE", "/opt/Cursor/cursor"),
        ];
        let cursor = env(&vars);
        if !cfg!(target_os = "macos") {
            // No such directory on this machine: the binding is shown.
            assert!(matches!(
                plan(Terminal::VsCode, false, &cursor, Some(home.path())),
                Plan::Show { .. }
            ));
            std::fs::create_dir_all(&user).unwrap();
            std::fs::write(
                user.join("keybindings.json"),
                "[\n  { \"key\": \"f5\", \"command\": \"x\" }\n]\n",
            )
            .unwrap();
            let Plan::Write(change) = plan(Terminal::VsCode, false, &cursor, Some(home.path())) else {
                panic!("the binding can be written")
            };
            assert_eq!(change.file, user.join("keybindings.json"));
            let backup = change.apply().unwrap().unwrap();
            assert_eq!(backup, user.join("keybindings.json.azazel-backup"));
            assert_eq!(
                std::fs::read_to_string(&backup).unwrap(),
                "[\n  { \"key\": \"f5\", \"command\": \"x\" }\n]\n"
            );
            let written = std::fs::read_to_string(&change.file).unwrap();
            assert_eq!(parsed(&written).unwrap().as_array().unwrap().len(), 2);
            assert!(matches!(
                plan(Terminal::VsCode, false, &cursor, Some(home.path())),
                Plan::Present(_)
            ));
            // The window is on another machine: its file is not here.
            let mut remote = vars.to_vec();
            remote.push(("SSH_CONNECTION", "10.0.0.1 1 10.0.0.2 22"));
            assert!(matches!(
                plan(Terminal::VsCode, false, &env(&remote), Some(home.path())),
                Plan::Show { .. }
            ));
        }
        assert_eq!(vscode_product(&none), "Code");
        assert_eq!(
            vscode_product(&env(&[("TERM_PROGRAM_VERSION", "1.99.0-insider")])),
            "Code - Insiders"
        );

        // Alacritty: a new file where none is, unless it is set up the old way.
        let alacritty_dir = config.join("alacritty");
        let Plan::Write(change) = plan(Terminal::Alacritty, false, &cursor, Some(home.path())) else {
            panic!("a new configuration can be written")
        };
        assert_eq!(change.file, alacritty_dir.join("alacritty.toml"));
        assert_eq!(change.apply().unwrap(), None, "nothing was there to keep");
        assert!(matches!(
            plan(Terminal::Alacritty, false, &cursor, Some(home.path())),
            Plan::Present(_)
        ));
        std::fs::remove_file(&change.file).unwrap();
        std::fs::write(alacritty_dir.join("alacritty.yml"), "font:\n  size: 12\n").unwrap();
        assert!(matches!(
            plan(Terminal::Alacritty, false, &cursor, Some(home.path())),
            Plan::Show { .. }
        ));
    }

    #[test]
    fn key_conflicts_name_the_key_the_action_and_what_takes_it() {
        let keymap = Keymap::defaults();
        // Windows Terminal, which reports every key: only what it keeps for itself.
        let found = key_conflicts(&keymap, Terminal::WindowsTerminal, false, false, false);
        assert_eq!(
            found,
            [
                "alt+enter (follow_up) does not reach azazel: Windows Terminal toggles full screen",
                "ctrl+shift+v (paste_text) does not reach azazel: Windows Terminal pastes (alt+shift+v works)",
                "ctrl+shift+p (cycle_model_back) does not reach azazel: Windows Terminal opens its command \
                 palette (alt+shift+p, alt+shift+m works)",
            ]
        );

        // A terminal nothing is known about that reports keys the old way.
        let found = key_conflicts(&keymap, Terminal::Other, false, true, false);
        assert_eq!(
            found,
            [
                "shift+enter (newline) arrives as plain enter in this terminal; /terminal-setup can change that \
                 (ctrl+j works)",
                "ctrl+enter (newline) arrives as plain enter in this terminal (ctrl+j works)",
                "ctrl+shift+v (paste_text) arrives without shift, as the ctrl key of the same letter in this \
                 terminal (alt+shift+v works)",
                "ctrl+shift+p (cycle_model_back) arrives without shift, as the ctrl key of the same letter in this \
                 terminal (alt+shift+p, alt+shift+m works)",
            ]
        );
        assert!(key_conflicts(&keymap, Terminal::Other, false, false, false).is_empty());

        // VS Code keeps `ctrl+j` and sends Shift+Enter as Enter, until it is set up.
        let found = key_conflicts(&keymap, Terminal::VsCode, false, false, false);
        assert!(
            found.contains(
                &"shift+enter (newline) arrives as plain enter in VS Code; /terminal-setup can change that"
                    .to_string()
            ),
            "{found:#?}"
        );
        assert!(
            found.contains(&"ctrl+j (newline) does not reach azazel: VS Code toggles the panel".to_string())
        );
        assert!(found.contains(&"ctrl+enter (newline) arrives as plain enter in VS Code".to_string()));
        // It keeps `ctrl+p` too: the other key of the action is named.
        assert!(
            found.contains(
                &"ctrl+p (cycle_model) does not reach azazel: VS Code opens Quick Open (alt+m works)"
                    .to_string()
            ),
            "{found:#?}"
        );
        let set_up = key_conflicts(&keymap, Terminal::VsCode, false, false, true);
        assert!(
            !set_up.iter().any(|line| line.starts_with("shift+enter")),
            "{set_up:#?}"
        );
        assert!(
            set_up.contains(
                &"ctrl+j (newline) does not reach azazel: VS Code toggles the panel (shift+enter works)"
                    .to_string()
            ),
            "{set_up:#?}"
        );

        // tmux takes its prefix, and a key bound twice says who gets it.
        let mut keymap = Keymap::defaults();
        keymap
            .merge_toml("stash = [\"ctrl+o\"]\ntoggle_thinking = [\"ctrl+o\"]\n")
            .unwrap();
        let found = key_conflicts(&keymap, Terminal::Other, true, false, false);
        assert_eq!(
            found,
            [
                "ctrl+o is bound to toggle_thinking and stash: toggle_thinking gets it",
                "ctrl+b (background) does not reach azazel: tmux takes it as its prefix key",
            ]
        );
    }
}
