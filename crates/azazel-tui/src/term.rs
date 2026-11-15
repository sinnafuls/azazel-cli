//! What the terminal azazel runs in can do, worked out once from the environment, and the escape
//! sequences for the extras (title, progress, notifications, links, mouse reporting), wrapped for
//! tmux when needed.

use std::io::{self, Write};

use crossterm::event::{KeyCode, KeyModifiers};

use crate::keys::KeyCombo;

/// Inline image protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Images {
    None,
    Kitty,
    Iterm,
    Sixel,
}

impl Images {
    /// `tui.images`: `auto` keeps what was detected, `off` turns images off, a protocol name forces it.
    pub fn from_setting(setting: &str, detected: Images) -> Result<Images, String> {
        match setting.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(detected),
            "off" | "none" => Ok(Images::None),
            "kitty" => Ok(Images::Kitty),
            "iterm" | "iterm2" => Ok(Images::Iterm),
            "sixel" => Ok(Images::Sixel),
            other => Err(format!(
                "tui.images: `{other}` is not one of auto, off, kitty, iterm, sixel"
            )),
        }
    }
}

/// Desktop notifications the terminal itself can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notify {
    None,
    /// iTerm2, WezTerm.
    Osc9,
    /// Ghostty and others that read `OSC 777;notify`.
    Osc777,
    /// kitty.
    Osc99,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Caps {
    /// Inside tmux: OSC sequences need pass-through.
    pub tmux: bool,
    /// Inside tmux, screen or zellij, which keep a scrollback of their own: erasing the
    /// terminal's (`CSI 3 J`) does nothing there.
    pub multiplexer: bool,
    pub images: Images,
    /// OSC 8 hyperlinks.
    pub links: bool,
    /// OSC 52 clipboard writes.
    pub clipboard_osc52: bool,
    /// Focus reporting (`CSI ? 1004 h`).
    pub focus: bool,
    /// OSC 9;4 progress.
    pub progress: bool,
    /// The terminal drops a progress state it has not heard about for a while (Ghostty, after
    /// fifteen seconds), so it has to be sent again while it lasts.
    pub progress_expires: bool,
    pub notify: Notify,
    /// Pixel size of one cell, `(width, height)`.
    pub cell_px: (u16, u16),
}

impl Caps {
    /// From environment variables (`var` returns a variable's value).
    pub fn detect(var: &dyn Fn(&str) -> Option<String>) -> Caps {
        let has = |name: &str| var(name).is_some_and(|v| !v.is_empty());
        let term = var("TERM").unwrap_or_default();
        let program = var("TERM_PROGRAM").unwrap_or_default();
        let tmux = has("TMUX") || term.starts_with("tmux") || term.starts_with("screen");
        let kitty = has("KITTY_WINDOW_ID") || term == "xterm-kitty";
        let iterm = program == "iTerm.app";
        let wezterm = has("WEZTERM_EXECUTABLE") || program == "WezTerm";
        let ghostty = has("GHOSTTY_RESOURCES_DIR") || program == "ghostty";
        let windows_terminal = has("WT_SESSION");
        let vte = has("VTE_VERSION");
        let warp = program == "WarpTerminal";
        let images = if kitty || ghostty {
            Images::Kitty
        } else if iterm || wezterm {
            Images::Iterm
        } else {
            // Windows Terminal draws Sixel since 1.22; that is confirmed by a device query before use.
            Images::None
        };
        let notify = if kitty {
            Notify::Osc99
        } else if iterm || wezterm {
            Notify::Osc9
        } else if ghostty {
            Notify::Osc777
        } else {
            Notify::None
        };
        let multiplexer = tmux || has("STY") || has("ZELLIJ");
        Caps {
            tmux,
            multiplexer,
            // Images under tmux need a forced protocol (`tui.images`).
            images: if tmux { Images::None } else { images },
            links: if tmux {
                // tmux passes hyperlinks on from 3.4; inside tmux the version variable is its own.
                program == "tmux" && tmux_has_links(&var("TERM_PROGRAM_VERSION").unwrap_or_default())
            } else {
                !warp && (kitty || iterm || wezterm || ghostty || windows_terminal || vte)
            },
            clipboard_osc52: true,
            focus: true,
            progress: windows_terminal || ghostty || wezterm || program == "iTerm.app",
            progress_expires: ghostty,
            notify,
            cell_px: (10, 20),
        }
    }

    /// `body` as an OSC sequence (`ESC ] body ST`), wrapped in tmux pass-through when inside tmux.
    pub fn osc(&self, body: &str) -> String {
        let sequence = format!("\x1b]{body}\x1b\\");
        if self.tmux {
            passthrough(&sequence)
        } else {
            sequence
        }
    }

    /// The window title.
    pub fn title(&self, text: &str) -> String {
        // The title is read by tmux itself (it sets the pane title), so it is not wrapped.
        format!("\x1b]0;{}\x07", clean(text))
    }

    /// Progress: `None` clears it.
    pub fn progress(&self, state: Option<Progress>) -> String {
        if !self.progress {
            return String::new();
        }
        let body = match state {
            None => "9;4;0;0".to_string(),
            Some(Progress::Working) => "9;4;3;0".to_string(),
            Some(Progress::Waiting) => "9;4;4;0".to_string(),
            Some(Progress::Failed) => "9;4;2;100".to_string(),
        };
        self.osc(&body)
    }

    /// A desktop notification through the terminal; empty when it has no such sequence.
    pub fn notification(&self, title: &str, body: &str) -> String {
        let (title, body) = (clean(title), clean(body));
        match self.notify {
            Notify::None => String::new(),
            Notify::Osc9 => self.osc(&format!("9;{title}: {body}")),
            Notify::Osc777 => self.osc(&format!("777;notify;{title};{body}")),
            Notify::Osc99 => {
                self.osc(&format!("99;i=azazel:d=0:p=title;{title}"))
                    + &self.osc(&format!("99;i=azazel:d=1:p=body;{body}"))
            }
        }
    }

    /// `text` as a hyperlink to `url` (OSC 8), or `text` alone when links are off.
    pub fn link(&self, url: &str, text: &str) -> String {
        if !self.links {
            return text.to_string();
        }
        hyperlink(url, text)
    }

    /// The capabilities after the terminal answered [`queries`]: the size of a cell when it
    /// said one, and Sixel when it listed it and nothing better was recognised.
    pub fn with_replies(mut self, replies: &Replies) -> Caps {
        if let Some(cell) = replies.cell_px {
            self.cell_px = cell;
        }
        if self.images == Images::None && replies.sixel && !self.tmux {
            self.images = Images::Sixel;
        }
        self
    }

    /// Copies `text` to the clipboard through OSC 52.
    pub fn clipboard(&self, text: &str) -> String {
        use base64::Engine as _;
        let data = base64::engine::general_purpose::STANDARD.encode(text);
        self.osc(&format!("52;c;{data}"))
    }
}

/// The terminal program azazel runs in, as far as the environment tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    WindowsTerminal,
    /// The terminal of VS Code and of the editors built from it.
    VsCode,
    Iterm2,
    Kitty,
    WezTerm,
    Alacritty,
    Ghostty,
    AppleTerminal,
    Other,
}

impl Terminal {
    /// From environment variables (`var` returns a variable's value). The editor comes first:
    /// a VS Code started from another terminal inherits that terminal's variables.
    pub fn detect(var: &dyn Fn(&str) -> Option<String>) -> Terminal {
        let has = |name: &str| var(name).is_some_and(|v| !v.is_empty());
        let term = var("TERM").unwrap_or_default();
        let program = var("TERM_PROGRAM").unwrap_or_default();
        if program == "vscode" {
            Terminal::VsCode
        } else if has("KITTY_WINDOW_ID") || term == "xterm-kitty" {
            Terminal::Kitty
        } else if program == "iTerm.app" {
            Terminal::Iterm2
        } else if has("WEZTERM_EXECUTABLE") || program == "WezTerm" {
            Terminal::WezTerm
        } else if has("GHOSTTY_RESOURCES_DIR") || program == "ghostty" {
            Terminal::Ghostty
        } else if has("ALACRITTY_WINDOW_ID") || has("ALACRITTY_SOCKET") || term == "alacritty" {
            Terminal::Alacritty
        } else if has("WT_SESSION") {
            Terminal::WindowsTerminal
        } else if program == "Apple_Terminal" {
            Terminal::AppleTerminal
        } else {
            Terminal::Other
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Terminal::WindowsTerminal => "Windows Terminal",
            Terminal::VsCode => "VS Code",
            Terminal::Iterm2 => "iTerm2",
            Terminal::Kitty => "kitty",
            Terminal::WezTerm => "WezTerm",
            Terminal::Alacritty => "Alacritty",
            Terminal::Ghostty => "Ghostty",
            Terminal::AppleTerminal => "Terminal",
            Terminal::Other => "this terminal",
        }
    }

    /// What the terminal does with `key` itself in its default setup, so that the key never
    /// reaches a program in it; `None` for a key it passes on.
    pub fn takes(self, key: &KeyCombo) -> Option<&'static str> {
        let key = KeyCombo::new(key.code, key.mods);
        let listed = |table: &[(&str, &'static str)]| {
            table
                .iter()
                .find(|(name, _)| name.parse::<KeyCombo>().is_ok_and(|taken| taken == key))
                .map(|(_, what)| *what)
        };
        match self {
            Terminal::WindowsTerminal => listed(WINDOWS_TERMINAL_KEYS),
            Terminal::VsCode => listed(VSCODE_KEYS).or_else(|| {
                listed(if cfg!(windows) {
                    &[("ctrl+v", "pastes")]
                } else {
                    &[]
                })
            }),
            Terminal::WezTerm => listed(WEZTERM_KEYS),
            Terminal::Alacritty => listed(ALACRITTY_KEYS).or_else(|| {
                listed(if cfg!(windows) {
                    &[("alt+enter", "toggles full screen")]
                } else {
                    &[]
                })
            }),
            Terminal::Ghostty => listed(GHOSTTY_KEYS),
            // kitty's own shortcuts are nearly all of `ctrl+shift`.
            Terminal::Kitty if key.mods == KeyModifiers::CONTROL | KeyModifiers::SHIFT => {
                Some("is among the ctrl+shift keys kitty keeps for its own shortcuts")
            }
            // On a Mac the option key types characters unless the terminal is told otherwise.
            Terminal::Iterm2 | Terminal::AppleTerminal
                if key.mods.contains(KeyModifiers::ALT) && matches!(key.code, KeyCode::Char(_)) =>
            {
                Some("types a character unless the option key is set to act as Meta (Esc+)")
            }
            _ => None,
        }
    }
}

// `ctrl+v` is not among them: Windows Terminal pastes text with it, and with nothing but an image
// on the clipboard it lets the key's release through, which azazel takes for the key.
const WINDOWS_TERMINAL_KEYS: &[(&str, &str)] = &[
    ("ctrl+shift+v", "pastes"),
    ("shift+insert", "pastes"),
    ("ctrl+shift+c", "copies"),
    ("ctrl+insert", "copies"),
    ("alt+enter", "toggles full screen"),
    ("f11", "toggles full screen"),
    ("ctrl+shift+t", "opens a tab"),
    ("ctrl+shift+w", "closes the pane"),
    ("ctrl+shift+n", "opens a window"),
    ("ctrl+shift+d", "duplicates the tab"),
    ("ctrl+shift+p", "opens its command palette"),
    ("ctrl+shift+f", "searches"),
    ("ctrl+shift+a", "selects all"),
    ("ctrl+shift+m", "enters mark mode"),
    ("ctrl+tab", "switches tabs"),
    ("ctrl+shift+tab", "switches tabs"),
    ("ctrl+=", "zooms in"),
    ("ctrl+-", "zooms out"),
    ("ctrl+0", "resets the zoom"),
    ("ctrl+,", "opens its settings"),
    ("alt+shift+d", "splits the pane"),
    ("ctrl+shift+up", "scrolls"),
    ("ctrl+shift+down", "scrolls"),
    ("ctrl+shift+pageup", "scrolls"),
    ("ctrl+shift+pagedown", "scrolls"),
    ("ctrl+shift+home", "scrolls"),
    ("ctrl+shift+end", "scrolls"),
    ("alt+f4", "closes the window"),
];

const VSCODE_KEYS: &[(&str, &str)] = &[
    ("ctrl+p", "opens Quick Open"),
    ("ctrl+e", "opens Quick Open"),
    ("ctrl+shift+p", "opens the Command Palette"),
    ("f1", "opens the Command Palette"),
    ("ctrl+j", "toggles the panel"),
    ("ctrl+`", "toggles the terminal"),
    ("ctrl+f", "searches the terminal"),
    ("ctrl+g", "goes to a recent directory"),
    ("ctrl+k", "starts a two-key shortcut"),
    ("ctrl+shift+c", "copies"),
    ("ctrl+shift+v", "pastes"),
    ("ctrl+shift+f", "searches the files"),
    ("ctrl+shift+e", "shows the explorer"),
    ("ctrl+pageup", "switches terminals"),
    ("ctrl+pagedown", "switches terminals"),
    ("ctrl+tab", "switches editors"),
    ("ctrl+,", "opens the settings"),
    ("ctrl+=", "zooms in"),
    ("ctrl+-", "zooms out"),
    ("f11", "toggles full screen"),
];

const WEZTERM_KEYS: &[(&str, &str)] = &[
    ("ctrl+shift+c", "copies"),
    ("ctrl+shift+v", "pastes"),
    ("ctrl+insert", "copies"),
    ("shift+insert", "pastes"),
    ("alt+enter", "toggles full screen"),
    ("ctrl+shift+t", "opens a tab"),
    ("ctrl+shift+w", "closes the tab"),
    ("ctrl+shift+n", "opens a window"),
    ("ctrl+shift+p", "opens its command palette"),
    ("ctrl+shift+f", "searches"),
    ("ctrl+shift+k", "clears the scrollback"),
    ("ctrl+shift+l", "opens its debug overlay"),
    ("ctrl+shift+r", "reloads its configuration"),
    ("ctrl+shift+u", "selects a character"),
    ("ctrl+shift+x", "enters copy mode"),
    ("ctrl+shift+z", "zooms the pane"),
    ("ctrl+tab", "switches tabs"),
    ("ctrl+shift+tab", "switches tabs"),
    ("ctrl+pageup", "switches tabs"),
    ("ctrl+pagedown", "switches tabs"),
    ("shift+pageup", "scrolls"),
    ("shift+pagedown", "scrolls"),
    ("ctrl+=", "zooms in"),
    ("ctrl+-", "zooms out"),
    ("ctrl+0", "resets the zoom"),
];

const ALACRITTY_KEYS: &[(&str, &str)] = &[
    ("ctrl+shift+c", "copies"),
    ("ctrl+shift+v", "pastes"),
    ("shift+insert", "pastes the selection"),
    ("ctrl+shift+f", "searches"),
    ("ctrl+shift+b", "searches backwards"),
    ("shift+pageup", "scrolls"),
    ("shift+pagedown", "scrolls"),
    ("shift+home", "scrolls"),
    ("shift+end", "scrolls"),
    ("ctrl+=", "zooms in"),
    ("ctrl+-", "zooms out"),
    ("ctrl+0", "resets the zoom"),
];

const GHOSTTY_KEYS: &[(&str, &str)] = &[
    ("ctrl+shift+c", "copies"),
    ("ctrl+shift+v", "pastes"),
    ("ctrl+shift+t", "opens a tab"),
    ("ctrl+shift+w", "closes the surface"),
    ("ctrl+shift+n", "opens a window"),
    ("ctrl+shift+o", "splits the window"),
    ("ctrl+shift+e", "splits the window"),
    ("ctrl+enter", "toggles full screen"),
    ("ctrl+tab", "switches tabs"),
    ("ctrl+shift+tab", "switches tabs"),
    ("shift+pageup", "scrolls"),
    ("shift+pagedown", "scrolls"),
    ("ctrl+=", "zooms in"),
    ("ctrl+-", "zooms out"),
    ("ctrl+0", "resets the zoom"),
];

/// Whether the terminal re-wraps what is on screen when its window gets narrower. Nearly all
/// do; xterm, rxvt, st, GNU screen and the Linux console cut the rows instead.
pub fn rewraps(var: &dyn Fn(&str) -> Option<String>) -> bool {
    let has = |name: &str| var(name).is_some_and(|v| !v.is_empty());
    let term = var("TERM").unwrap_or_default();
    let cuts = has("XTERM_VERSION")
        || term == "linux"
        || term.starts_with("rxvt")
        || term.starts_with("st-")
        || term == "st"
        || (term.starts_with("screen") && !has("TMUX"));
    !cuts
}

/// `text` as an OSC 8 hyperlink to `url`. Not wrapped for tmux, which keeps track of links
/// itself.
pub fn hyperlink(url: &str, text: &str) -> String {
    format!("{}{text}{}", link_start(url), crate::text::LINK_END)
}

/// The sequence that makes what follows a hyperlink to `url`, until [`crate::text::LINK_END`].
pub fn link_start(url: &str) -> String {
    format!("\x1b]8;;{}\x1b\\", clean(url))
}

/// Whether a tmux of this version (`3.4`, `3.5a`, `next-3.6`) passes hyperlinks on.
fn tmux_has_links(version: &str) -> bool {
    let mut numbers = version
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .split(|c: char| !c.is_ascii_digit())
        .map(|part| part.parse::<u32>().unwrap_or(0));
    let (major, minor) = (numbers.next().unwrap_or(0), numbers.next().unwrap_or(0));
    (major, minor) >= (3, 4)
}

/// Asks the terminal for the size of a cell in pixels (`CSI 16 t`).
const CELL_QUERY: &str = "\x1b[16t";
/// Asks the terminal for its background colour (`OSC 11 ; ?`).
const BACKGROUND_QUERY: &str = "\x1b]11;?\x1b\\";
/// Asks the terminal for its device attributes (`CSI c`). Nearly every terminal answers this
/// one, so its reply also tells that no answer to an earlier question is coming.
const ATTRIBUTES_QUERY: &str = "\x1b[c";

/// What to send a terminal to learn the size of a cell (`cell`) and its background colour
/// (`background`): those questions, then the one for its device attributes, which ends the
/// replies.
pub fn queries(cell: bool, background: bool) -> String {
    let mut out = String::new();
    if background {
        out.push_str(BACKGROUND_QUERY);
    }
    if cell {
        out.push_str(CELL_QUERY);
    }
    out.push_str(ATTRIBUTES_QUERY);
    out
}

/// What a terminal answered to [`queries`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Replies {
    /// `(width, height)` of one cell in pixels.
    pub cell_px: Option<(u16, u16)>,
    /// The device attributes list Sixel graphics (attribute 4).
    pub sixel: bool,
    /// The background colour, as red, green and blue.
    pub background: Option<(u8, u8, u8)>,
    /// The device attributes arrived: nothing more is coming.
    pub complete: bool,
}

impl Replies {
    /// Reads the replies out of what the terminal sent; anything else in `input` is skipped.
    pub fn parse(input: &[u8]) -> Replies {
        let mut replies = Replies {
            background: background_reply(input),
            ..Replies::default()
        };
        let mut rest = input;
        while let Some(start) = rest.windows(2).position(|pair| pair == b"\x1b[") {
            rest = &rest[start + 2..];
            let Some(end) = rest.iter().position(|b| (0x40..=0x7e).contains(b)) else {
                break;
            };
            let (body, terminator) = (&rest[..end], rest[end]);
            rest = &rest[end + 1..];
            let Ok(body) = std::str::from_utf8(body) else {
                continue;
            };
            let numbers =
                |text: &str| -> Vec<u16> { text.split(';').map(|part| part.parse().unwrap_or(0)).collect() };
            match (terminator, body.strip_prefix('?')) {
                // `CSI ? 62 ; 4 ; 6 c`: the first number is the device class, the rest features.
                (b'c', Some(attributes)) => {
                    replies.sixel = numbers(attributes).iter().skip(1).any(|n| *n == 4);
                    replies.complete = true;
                }
                // `CSI 6 ; height ; width t`.
                (b't', None) => {
                    if let [6, height, width] = numbers(body)[..]
                        && height > 0
                        && width > 0
                    {
                        replies.cell_px = Some((width, height));
                    }
                }
                _ => {}
            }
        }
        replies
    }

    /// Whether the terminal's background is a light one; `None` when it did not say.
    pub fn light_background(&self) -> Option<bool> {
        let (red, green, blue) = self.background?;
        // How bright the colour looks: green counts most, blue least.
        let luminance = 0.2126 * f64::from(red) + 0.7152 * f64::from(green) + 0.0722 * f64::from(blue);
        Some(luminance > 127.5)
    }
}

/// The colour in a reply to [`BACKGROUND_QUERY`], `OSC 11 ; rgb:RRRR/GGGG/BBBB`, each part one
/// to four hexadecimal digits. The escape character in front of the reply is not looked for: a
/// Windows console delivers the reply without it.
fn background_reply(input: &[u8]) -> Option<(u8, u8, u8)> {
    const START: &[u8] = b"]11;rgb:";
    let at = input.windows(START.len()).position(|window| window == START)?;
    let rest = &input[at + START.len()..];
    let end = rest
        .iter()
        .position(|byte| !byte.is_ascii_hexdigit() && *byte != b'/')
        .unwrap_or(rest.len());
    let mut parts = std::str::from_utf8(&rest[..end]).ok()?.split('/');
    let mut part = || -> Option<u8> {
        let digits = parts.next().filter(|digits| (1..=4).contains(&digits.len()))?;
        let value = u32::from_str_radix(digits, 16).ok()?;
        // `f` is full, as `ff` and `ffff` are.
        let full = (1u32 << (4 * digits.len())) - 1;
        u8::try_from((value * 255 + full / 2) / full).ok()
    };
    let colour = (part()?, part()?, part()?);
    parts.next().is_none().then_some(colour)
}

/// The terminal bell: never wrapped, so tmux can flag the window.
pub const BELL: &str = "\x07";
/// Saves and restores the window title around a run.
pub const PUSH_TITLE: &str = "\x1b[22;0t";
pub const POP_TITLE: &str = "\x1b[23;0t";
pub const FOCUS_ON: &str = "\x1b[?1004h";
pub const FOCUS_OFF: &str = "\x1b[?1004l";
/// Has the terminal report the mouse: presses (`?1000h`), drags (`?1002h`) and moves
/// (`?1003h`), in SGR coordinates (`?1006h`), which no column is too far for.
pub const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h";
/// Undoes [`MOUSE_ON`], in reverse order.
pub const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

/// Mouse reporting, turned on and off through one owner. While the terminal reports the mouse
/// its own selection and wheel do not work, so reporting is never left on: dropping the owner,
/// also while a panic unwinds, turns it off.
pub struct MouseCapture<W: Write> {
    out: W,
    on: bool,
    /// A Windows console reports the mouse when its input mode says so, not after a sequence.
    #[cfg(windows)]
    console: bool,
}

impl<W: Write> MouseCapture<W> {
    /// Reporting asked for with [`MOUSE_ON`] and ended with [`MOUSE_OFF`], written to `out`.
    pub fn sequences(out: W) -> Self {
        MouseCapture {
            out,
            on: false,
            #[cfg(windows)]
            console: false,
        }
    }

    /// Reporting as the platform asks for it: the input mode of the console on Windows, the
    /// sequences written to `out` elsewhere.
    pub fn native(out: W) -> Self {
        MouseCapture {
            out,
            on: false,
            #[cfg(windows)]
            console: true,
        }
    }

    pub fn is_on(&self) -> bool {
        self.on
    }

    /// Turns reporting on or off; nothing happens when it already is.
    pub fn set(&mut self, on: bool) -> io::Result<()> {
        if self.on == on {
            return Ok(());
        }
        self.on = on;
        #[cfg(windows)]
        if self.console {
            use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
            return match on {
                true => crossterm::execute!(self.out, EnableMouseCapture),
                false => crossterm::execute!(self.out, DisableMouseCapture),
            };
        }
        let sequence = if on { MOUSE_ON } else { MOUSE_OFF };
        self.out.write_all(sequence.as_bytes())?;
        self.out.flush()
    }
}

impl<W: Write> Drop for MouseCapture<W> {
    fn drop(&mut self) {
        let _ = self.set(false);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Working,
    Waiting,
    Failed,
}

/// tmux pass-through: `ESC P tmux; <sequence with every ESC doubled> ESC \`.
pub fn passthrough(sequence: &str) -> String {
    format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b"))
}

/// Text that is safe inside an OSC sequence: control characters (which would end or break it)
/// become spaces.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn terminals_are_recognised_from_their_variables() {
        let kitty = Caps::detect(&env(&[("KITTY_WINDOW_ID", "1"), ("TERM", "xterm-kitty")]));
        assert_eq!(
            (kitty.images, kitty.notify, kitty.links),
            (Images::Kitty, Notify::Osc99, true)
        );
        let wt = Caps::detect(&env(&[("WT_SESSION", "x")]));
        assert_eq!(
            (wt.images, wt.notify, wt.links, wt.progress),
            (Images::None, Notify::None, true, true)
        );
        let iterm_in_tmux = Caps::detect(&env(&[("TERM_PROGRAM", "iTerm.app"), ("TMUX", "/tmp/s,1,0")]));
        assert!(iterm_in_tmux.tmux);
        assert_eq!(iterm_in_tmux.images, Images::None);
        let warp = Caps::detect(&env(&[("TERM_PROGRAM", "WarpTerminal")]));
        assert!(!warp.links);
        // Inside tmux the program is tmux itself, and only a recent one passes links on.
        let tmux = |version: &str| {
            Caps::detect(&env(&[
                ("TMUX", "/tmp/s,1,0"),
                ("TERM_PROGRAM", "tmux"),
                ("TERM_PROGRAM_VERSION", version),
                ("KITTY_WINDOW_ID", "1"),
            ]))
        };
        assert!(tmux("3.4").links && tmux("3.5a").links && tmux("next-3.6").links);
        assert!(!tmux("3.3a").links && !tmux("").links);
        assert!(!iterm_in_tmux.links);
        assert!(Caps::detect(&env(&[("GHOSTTY_RESOURCES_DIR", "/x")])).progress_expires);
        assert!(!wt.progress_expires);
        assert_eq!(Images::from_setting("sixel", Images::None), Ok(Images::Sixel));
        assert_eq!(Images::from_setting("off", Images::Kitty), Ok(Images::None));
        assert!(Images::from_setting("jpeg", Images::None).is_err());
    }

    #[test]
    fn sequences_are_exact_and_wrapped_for_tmux() {
        let plain = Caps::detect(&env(&[("TERM_PROGRAM", "WezTerm")]));
        assert_eq!(plain.osc("9;hi"), "\x1b]9;hi\x1b\\");
        assert_eq!(plain.title("azazel · a\nb"), "\x1b]0;azazel · a b\x07");
        assert_eq!(plain.progress(Some(Progress::Working)), "\x1b]9;4;3;0\x1b\\");
        assert_eq!(plain.progress(None), "\x1b]9;4;0;0\x1b\\");
        assert_eq!(plain.notification("azazel", "done"), "\x1b]9;azazel: done\x1b\\");
        assert_eq!(
            plain.link("https://x.y", "x"),
            "\x1b]8;;https://x.y\x1b\\x\x1b]8;;\x1b\\"
        );
        assert_eq!(plain.clipboard("hi"), "\x1b]52;c;aGk=\x1b\\");

        let tmux = Caps {
            tmux: true,
            ..plain.clone()
        };
        assert_eq!(tmux.osc("9;hi"), "\x1bPtmux;\x1b\x1b]9;hi\x1b\x1b\\\x1b\\");
        let ghostty = Caps::detect(&env(&[("GHOSTTY_RESOURCES_DIR", "/x")]));
        assert_eq!(ghostty.notification("a", "b"), "\x1b]777;notify;a;b\x1b\\");
        let none = Caps::detect(&env(&[]));
        assert_eq!(none.progress(Some(Progress::Waiting)), "");
        assert_eq!(none.link("https://x.y", "x"), "x");
        assert_eq!(none.notification("a", "b"), "");

        // Every progress state, bare and through tmux.
        assert_eq!(plain.progress(Some(Progress::Waiting)), "\x1b]9;4;4;0\x1b\\");
        assert_eq!(plain.progress(Some(Progress::Failed)), "\x1b]9;4;2;100\x1b\\");
        assert_eq!(
            tmux.progress(Some(Progress::Working)),
            "\x1bPtmux;\x1b\x1b]9;4;3;0\x1b\x1b\\\x1b\\"
        );
        assert_eq!(
            tmux.notification("azazel", "done"),
            "\x1bPtmux;\x1b\x1b]9;azazel: done\x1b\x1b\\\x1b\\"
        );
        // The title is tmux's own to read, and the bell flags its window: neither is wrapped.
        assert_eq!(tmux.title("* azazel · x"), "\x1b]0;* azazel · x\x07");
        assert_eq!(
            tmux.clipboard("hi"),
            "\x1bPtmux;\x1b\x1b]52;c;aGk=\x1b\x1b\\\x1b\\"
        );
        let kitty = Caps::detect(&env(&[("KITTY_WINDOW_ID", "1")]));
        assert_eq!(
            kitty.notification("azazel", "needs you"),
            "\x1b]99;i=azazel:d=0:p=title;azazel\x1b\\\x1b]99;i=azazel:d=1:p=body;needs you\x1b\\"
        );
        // A link's address cannot end the sequence early.
        assert_eq!(
            hyperlink("https://x.y/\x1b\\evil\x07", "t"),
            "\x1b]8;;https://x.y/ \\evil \x1b\\t\x1b]8;;\x1b\\"
        );
        assert_eq!(
            (BELL, PUSH_TITLE, POP_TITLE, FOCUS_ON, FOCUS_OFF),
            ("\x07", "\x1b[22;0t", "\x1b[23;0t", "\x1b[?1004h", "\x1b[?1004l")
        );
        assert_eq!(queries(true, false), "\x1b[16t\x1b[c");
        assert_eq!(queries(false, true), "\x1b]11;?\x1b\\\x1b[c");
        assert_eq!(queries(true, true), "\x1b]11;?\x1b\\\x1b[16t\x1b[c");
        assert_eq!(queries(false, false), "\x1b[c");
    }

    #[test]
    fn replies_give_the_cell_size_and_sixel_support() {
        // Windows Terminal: cell size, then device attributes that list Sixel.
        let both = Replies::parse(b"\x1b[6;20;9t\x1b[?61;4;6;7;14;21;22;23;24;28;32;42c");
        assert_eq!(
            both,
            Replies {
                cell_px: Some((9, 20)),
                sixel: true,
                background: None,
                complete: true
            }
        );
        // A terminal that only answers the device attributes, without Sixel. The class number
        // (here 64) is not a feature, and neither is a 4 inside another number.
        let plain = Replies::parse(b"\x1b[?64;1;2;6;14;22c");
        assert_eq!((plain.cell_px, plain.sixel, plain.complete), (None, false, true));
        assert!(!Replies::parse(b"\x1b[?4;1;2c").sixel);
        // Typed keys around the replies are skipped; half a reply is not one.
        let noisy = Replies::parse(b"ab\x1b[6;18;8tq\x1b[?62;");
        assert_eq!(
            (noisy.cell_px, noisy.sixel, noisy.complete),
            (Some((8, 18)), false, false)
        );
        assert_eq!(Replies::parse(b"\x1b[6;0;0t\x1b[4;600;800t").cell_px, None);
        assert_eq!(Replies::parse(b""), Replies::default());

        let wt = Caps::detect(&env(&[("WT_SESSION", "x")])).with_replies(&both);
        assert_eq!((wt.images, wt.cell_px), (Images::Sixel, (9, 20)));
        // What was recognised from the environment is not replaced, and tmux stays without.
        let kitty = Caps::detect(&env(&[("KITTY_WINDOW_ID", "1")])).with_replies(&both);
        assert_eq!(kitty.images, Images::Kitty);
        let tmux = Caps::detect(&env(&[("WT_SESSION", "x"), ("TMUX", "1")])).with_replies(&both);
        assert_eq!(tmux.images, Images::None);
    }

    #[test]
    fn the_background_colour_tells_a_light_terminal_from_a_dark_one() {
        // xterm: sixteen bits a part, ended by the string terminator, then the device attributes.
        let white = Replies::parse(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?62;4c");
        assert_eq!(white.background, Some((255, 255, 255)));
        assert_eq!(
            (white.light_background(), white.complete, white.sixel),
            (Some(true), true, true)
        );
        // Eight bits a part, ended by a bell.
        let dark = Replies::parse(b"\x1b]11;rgb:1e/1e/2e\x07\x1b[6;20;9t\x1b[?61c");
        assert_eq!(dark.background, Some((0x1e, 0x1e, 0x2e)));
        assert_eq!(
            (dark.light_background(), dark.cell_px),
            (Some(false), Some((9, 20)))
        );
        // A Windows console hands the reply over without its escape character and terminator.
        let console = Replies::parse(b"]11;rgb:f0f0/f0f0/f0f0\x1b[?61c");
        assert_eq!(console.light_background(), Some(true));
        // One digit a part scales like the others; a saturated blue is dark, a yellow light.
        assert_eq!(
            Replies::parse(b"]11;rgb:f/0/8\x07").background,
            Some((255, 0, 136))
        );
        assert_eq!(
            Replies::parse(b"]11;rgb:0000/0000/ffff\x07").light_background(),
            Some(false)
        );
        assert_eq!(
            Replies::parse(b"]11;rgb:ffff/ffff/0000\x07").light_background(),
            Some(true)
        );
        // No answer, half an answer and something else are no colour.
        for other in [
            &b"\x1b[?62;4c"[..],
            b"]11;rgb:ffff/ffff",
            b"]11;rgb:ffff/ffff/ffff/ffff\x07",
            b"]11;rgb:fffff/00/00\x07",
            b"]11;rgb:zz/00/00\x07",
            b"]10;rgb:ffff/ffff/ffff\x07",
        ] {
            let replies = Replies::parse(other);
            assert_eq!(
                (replies.background, replies.light_background()),
                (None, None),
                "{other:?}"
            );
        }
    }

    #[test]
    fn the_terminal_program_is_told_from_its_variables() {
        let detect = |pairs: &[(&str, &str)]| Terminal::detect(&env(pairs));
        assert_eq!(detect(&[("WT_SESSION", "x")]), Terminal::WindowsTerminal);
        // VS Code started from a Windows Terminal shell still has that terminal's variable.
        assert_eq!(
            detect(&[("WT_SESSION", "x"), ("TERM_PROGRAM", "vscode")]),
            Terminal::VsCode
        );
        assert_eq!(detect(&[("TERM_PROGRAM", "iTerm.app")]), Terminal::Iterm2);
        assert_eq!(detect(&[("TERM", "xterm-kitty")]), Terminal::Kitty);
        assert_eq!(detect(&[("KITTY_WINDOW_ID", "3")]), Terminal::Kitty);
        assert_eq!(detect(&[("TERM_PROGRAM", "WezTerm")]), Terminal::WezTerm);
        assert_eq!(
            detect(&[("WEZTERM_EXECUTABLE", "/bin/wezterm")]),
            Terminal::WezTerm
        );
        assert_eq!(detect(&[("ALACRITTY_WINDOW_ID", "9")]), Terminal::Alacritty);
        assert_eq!(detect(&[("TERM", "alacritty")]), Terminal::Alacritty);
        assert_eq!(detect(&[("TERM_PROGRAM", "ghostty")]), Terminal::Ghostty);
        assert_eq!(
            detect(&[("TERM_PROGRAM", "Apple_Terminal")]),
            Terminal::AppleTerminal
        );
        assert_eq!(detect(&[("TERM", "xterm-256color")]), Terminal::Other);
        assert_eq!(detect(&[("WT_SESSION", "")]), Terminal::Other);
    }

    #[test]
    fn keys_a_terminal_keeps_for_itself_are_known() {
        let key = |text: &str| text.parse::<KeyCombo>().unwrap();
        // Every listed key is one the keymap could hold.
        for table in [
            WINDOWS_TERMINAL_KEYS,
            VSCODE_KEYS,
            WEZTERM_KEYS,
            ALACRITTY_KEYS,
            GHOSTTY_KEYS,
        ] {
            for (name, what) in table {
                assert_eq!(
                    key(name).to_string(),
                    *name,
                    "{name} is written the way keys are shown"
                );
                assert!(!what.is_empty());
            }
        }
        let wt = Terminal::WindowsTerminal;
        assert_eq!(wt.takes(&key("alt+enter")), Some("toggles full screen"));
        assert_eq!(wt.takes(&key("ctrl+v")), None);
        assert_eq!(wt.takes(&key("ctrl+shift+v")), Some("pastes"));
        assert_eq!(wt.takes(&key("alt+v")), None);
        assert_eq!(wt.takes(&key("shift+enter")), None);
        assert_eq!(Terminal::VsCode.takes(&key("ctrl+j")), Some("toggles the panel"));
        assert_eq!(Terminal::VsCode.takes(&key("ctrl+o")), None);
        assert!(Terminal::Kitty.takes(&key("ctrl+shift+v")).is_some());
        assert_eq!(Terminal::Kitty.takes(&key("ctrl+v")), None);
        assert!(Terminal::Iterm2.takes(&key("alt+m")).is_some());
        assert_eq!(Terminal::Iterm2.takes(&key("alt+enter")), None);
        assert_eq!(
            Terminal::WezTerm.takes(&key("alt+enter")),
            Some("toggles full screen")
        );
        assert_eq!(Terminal::Other.takes(&key("ctrl+v")), None);
    }

    #[test]
    fn terminals_that_cut_rows_instead_of_rewrapping_them_are_known() {
        assert!(rewraps(&env(&[("WT_SESSION", "x")])));
        assert!(rewraps(&env(&[("TERM", "xterm-256color")])));
        assert!(rewraps(&env(&[
            ("TERM", "screen-256color"),
            ("TMUX", "/tmp/s,1,0")
        ])));
        assert!(!rewraps(&env(&[
            ("TERM", "xterm"),
            ("XTERM_VERSION", "XTerm(390)")
        ])));
        assert!(!rewraps(&env(&[("TERM", "screen")])));
        assert!(!rewraps(&env(&[("TERM", "rxvt-unicode-256color")])));
        assert!(!rewraps(&env(&[("TERM", "st-256color")])));
        assert!(!rewraps(&env(&[("TERM", "linux")])));
    }

    /// What the capture writes, shared with the test while the capture owns its writer.
    #[derive(Clone, Default)]
    struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("not poisoned").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        /// The mouse mode a terminal is left in after everything written so far.
        fn mode(&self) -> (vt100::MouseProtocolMode, vt100::MouseProtocolEncoding) {
            let rows = std::num::NonZero::new(24).expect("not zero");
            let columns = std::num::NonZero::new(80).expect("not zero");
            let mut parser = vt100::Parser::new(rows, columns, 0);
            parser.process(&self.0.lock().expect("not poisoned"));
            let screen = parser.screen();
            (screen.mouse_protocol_mode(), screen.mouse_protocol_encoding())
        }
    }

    #[test]
    fn mouse_reporting_is_turned_on_and_off_and_never_left_on() {
        use vt100::{MouseProtocolEncoding as Encoding, MouseProtocolMode as Mode};
        let off = (Mode::None, Encoding::Default);

        let out = Shared::default();
        let mut capture = MouseCapture::sequences(out.clone());
        capture.set(true).expect("written");
        assert_eq!(out.mode(), (Mode::AnyMotion, Encoding::Sgr));
        capture.set(true).expect("written");
        assert_eq!(
            out.0.lock().expect("not poisoned").len(),
            MOUSE_ON.len(),
            "asked once"
        );
        capture.set(false).expect("written");
        assert_eq!(out.mode(), off);

        // Dropped while on, as when the UI ends.
        let out = Shared::default();
        let mut capture = MouseCapture::sequences(out.clone());
        capture.set(true).expect("written");
        drop(capture);
        assert_eq!(out.mode(), off);

        // A panic unwinds through the owner.
        let out = Shared::default();
        let writer = out.clone();
        let panicked = std::panic::catch_unwind(move || {
            let mut capture = MouseCapture::sequences(writer);
            capture.set(true).expect("written");
            panic!("the UI failed");
        });
        assert!(panicked.is_err());
        assert_eq!(out.mode(), off);
    }
}
