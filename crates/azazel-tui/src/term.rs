//! What the terminal azazel runs in can do, worked out once from the environment, and the escape
//! sequences for the extras (title, progress, notifications, links), wrapped for tmux when needed.

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
    pub images: Images,
    /// OSC 8 hyperlinks.
    pub links: bool,
    /// OSC 52 clipboard writes.
    pub clipboard_osc52: bool,
    /// Focus reporting (`CSI ? 1004 h`).
    pub focus: bool,
    /// OSC 9;4 progress.
    pub progress: bool,
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
        Caps {
            tmux,
            // Images under tmux need a forced protocol (`tui.images`).
            images: if tmux { Images::None } else { images },
            links: !warp && (kitty || iterm || wezterm || ghostty || windows_terminal || vte),
            clipboard_osc52: true,
            focus: true,
            progress: windows_terminal || ghostty || wezterm || program == "iTerm.app",
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
        format!("\x1b]8;;{}\x1b\\{text}\x1b]8;;\x1b\\", clean(url))
    }

    /// Copies `text` to the clipboard through OSC 52.
    pub fn clipboard(&self, text: &str) -> String {
        use base64::Engine as _;
        let data = base64::engine::general_purpose::STANDARD.encode(text);
        self.osc(&format!("52;c;{data}"))
    }
}

/// The terminal bell: never wrapped, so tmux can flag the window.
pub const BELL: &str = "\x07";
/// Saves and restores the window title around a run.
pub const PUSH_TITLE: &str = "\x1b[22;0t";
pub const POP_TITLE: &str = "\x1b[23;0t";
pub const FOCUS_ON: &str = "\x1b[?1004h";
pub const FOCUS_OFF: &str = "\x1b[?1004l";

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
    }
}
