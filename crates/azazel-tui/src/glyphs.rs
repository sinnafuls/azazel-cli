//! The symbols the UI draws with, at three levels: `nerd` (icons from a Nerd Font), `unicode`
//! (symbols every modern terminal font has) and `ascii` (plain ASCII, for terminals and fonts
//! that have neither). Components ask the theme for a [`Glyph`] instead of writing a literal, so
//! one setting changes every icon, frame and tree line at once.
//!
//! Box-drawing, tree and progress glyphs are the same at `nerd` and `unicode`; only icons
//! differ. Every single symbol fills one terminal cell, or is a short ASCII word at the `ascii`
//! level. An `ascii` icon that would only repeat the word beside it is empty.

use std::fmt;
use std::str::FromStr;

/// How rich the symbols may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GlyphLevel {
    Nerd,
    #[default]
    Unicode,
    Ascii,
}

impl GlyphLevel {
    pub const ALL: [GlyphLevel; 3] = [GlyphLevel::Unicode, GlyphLevel::Nerd, GlyphLevel::Ascii];

    pub fn name(self) -> &'static str {
        match self {
            GlyphLevel::Nerd => "nerd",
            GlyphLevel::Unicode => "unicode",
            GlyphLevel::Ascii => "ascii",
        }
    }

    /// The level to use: `AZAZEL_GLYPHS` when it names one, else `setting` (the `tui.glyphs`
    /// value). `auto` is `unicode`, or `ascii` when the locale variables say the terminal's
    /// encoding is not UTF-8. Fonts are never probed: a missing Nerd Font cannot be detected.
    pub fn detect(setting: &str, env: impl Fn(&str) -> Option<String>) -> GlyphLevel {
        if let Some(level) = env("AZAZEL_GLYPHS").and_then(|value| value.parse().ok()) {
            return level;
        }
        match setting.parse() {
            Ok(level) => level,
            Err(_) => auto(&env),
        }
    }
}

/// `unicode` unless the first locale variable that is set names an encoding other than UTF-8.
fn auto(env: &impl Fn(&str) -> Option<String>) -> GlyphLevel {
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| env(name).filter(|value| !value.trim().is_empty()));
    match locale {
        Some(value) => {
            let value = value.to_ascii_lowercase();
            if value.contains("utf-8") || value.contains("utf8") {
                GlyphLevel::Unicode
            } else {
                GlyphLevel::Ascii
            }
        }
        None => GlyphLevel::Unicode,
    }
}

impl fmt::Display for GlyphLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown glyph set `{0}`: nerd, unicode, ascii or auto")]
pub struct UnknownGlyphLevel(pub String);

impl FromStr for GlyphLevel {
    type Err = UnknownGlyphLevel;

    /// `nerd`, `unicode` or `ascii`. `auto` is an error here, so callers can tell "decide for
    /// me" from a choice.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.trim().to_ascii_lowercase().as_str() {
            "nerd" => Ok(GlyphLevel::Nerd),
            "unicode" => Ok(GlyphLevel::Unicode),
            "ascii" => Ok(GlyphLevel::Ascii),
            _ => Err(UnknownGlyphLevel(text.to_string())),
        }
    }
}

macro_rules! glyphs {
    ($($(#[$doc:meta])* $key:ident => $nerd:literal, $unicode:literal, $ascii:literal;)*) => {
        /// One symbol of the UI; see the module documentation.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Glyph {
            $($(#[$doc])* $key,)*
        }

        impl Glyph {
            pub const ALL: &'static [Glyph] = &[$(Glyph::$key,)*];
        }

        const NERD: &[&str] = &[$($nerd,)*];
        const UNICODE: &[&str] = &[$($unicode,)*];
        const ASCII: &[&str] = &[$($ascii,)*];
    };
}

glyphs! {
    /// Marks the input and the user's prompts.
    Prompt => "❯", "❯", ">";
    /// The bar beside the selected row of a list.
    Rail => "▌", "▌", ">";
    /// Points at the current item where there is no rail.
    Cursor => "\u{f054}", "❯", ">";
    Success => "\u{f00c}", "✔", "[+]";
    Fail => "\u{f00d}", "✘", "[-]";
    Running => "\u{f110}", "⟳", "[~]";
    Pending => "\u{f254}", "○", "[ ]";
    Warning => "\u{f12a}", "⚠", "[!]";
    Info => "\u{f129}", "ℹ", "[i]";
    /// Before the model name.
    Model => "\u{ec19}", "⬢", "";
    /// Before the share of the context window in use.
    Context => "\u{e70f}", "◫", "ctx";
    Cost => "\u{f155}", "$", "$";
    /// Before the output rate, `78 tok/s`.
    Rate => "\u{f0e7}", "↯", "";
    Tokens => "\u{e26b}", "◍", "tok";
    /// Before the fullest usage window of a subscription.
    Usage => "\u{f0e4}", "◔", "use";
    /// Before the count of running agents.
    Agents => "\u{f0c0}", "⇶", "";
    Goal => "\u{f024}", "⚑", "goal:";
    /// Before the label of an entry of the session tree.
    Label => "\u{f02b}", "⚑", "#";
    /// Marks a favourite model.
    Favourite => "\u{f005}", "★", "*";
    Branch => "\u{e0a0}", "⑂", "git:";
    Folder => "\u{f115}", "", "";
    File => "\u{f15b}", "", "";
    ModePlan => "\u{f04c}", "⏸", "||";
    ModeEdits => "\u{f04b}", "⏵", ">";
    ModeAuto => "\u{f04e}", "⏵⏵", ">>";
    /// Before a message waiting for the agent to finish.
    Queued => "\u{f017}", "⏵", ">";
    ToolRead => "\u{f15c}", "▤", "";
    ToolEdit => "\u{f044}", "✎", "";
    ToolWrite => "\u{f0219}", "✚", "";
    ToolShell => "\u{f120}", "❯", "$";
    ToolSearch => "\u{f002}", "⌕", "";
    ToolWeb => "\u{f0ac}", "⊕", "";
    ToolAgent => "\u{f0c0}", "⇶", "";
    Search => "\u{f002}", "⌕", "/";
    Thinking => "\u{f0eb}", "✻", "";
    /// Separates the parts of the status line.
    Separator => " · ", " · ", " | ";
    /// The `bar` separator of the status line.
    SeparatorBar => " │ ", " │ ", " | ";
    /// Ends a colour block on the left side of a `powerline` status line. Only a Nerd Font has
    /// it: at the other levels the blocks stand side by side.
    PowerRight => "\u{e0b0}", "", "";
    /// Starts a colour block on the right side of a `powerline` status line.
    PowerLeft => "\u{e0b2}", "", "";
    /// Before the count of todo items done.
    Tasks => "\u{f0ae}", "☑", "todo";
    /// Before the session's name.
    Session => "\u{f086}", "❖", "";
    Clock => "\u{f017}", "◷", "";
    /// Before the count of files changed since the last commit.
    Dirty => "\u{f040}", "±", "+-";
    /// Before the account a provider is used with.
    Account => "\u{f007}", "♙", "as";
    /// Before the share of the prompt that was read from the provider's cache.
    Cache => "\u{f1c0}", "≋", "cache";
    TreeMid => "├─", "├─", "|-";
    TreeLast => "└─", "└─", "`-";
    TreeLine => "│", "│", "|";
    ProgressFull => "━", "━", "=";
    /// The track of a vertical scrollbar.
    ScrollTrack => "│", "│", "|";
    /// The thumb of a vertical scrollbar.
    ScrollThumb => "┃", "┃", "#";
    ProgressEmpty => "─", "─", "-";
    /// The half-filled cell at the head of a progress bar.
    ProgressHead => "╸", "╸", ">";
    CheckOn => "\u{f14a}", "▣", "[x]";
    CheckOff => "\u{f096}", "□", "[ ]";
    RadioOn => "\u{f192}", "◉", "(o)";
    RadioOff => "\u{f10c}", "○", "( )";
    TodoPending => "\u{f10c}", "○", "[ ]";
    TodoActive => "\u{f192}", "◉", "[~]";
    TodoDone => "\u{f00c}", "✓", "[x]";
    Bullet => "●", "●", "*";
    MoreUp => "↑", "↑", "^";
    MoreDown => "↓", "↓", "v";
    Ellipsis => "…", "…", "...";
    /// Marks a model that can think before it answers.
    Reasons => "◐", "◐", "T";
    /// Marks a model that takes images.
    Vision => "\u{f06e}", "◫", "V";
    /// Before a model a role falls back to when the one before it fails.
    Fallback => "↳", "↳", "->";
    /// Leads from what something follows to what that comes to: `auto → model`.
    Arrow => "→", "→", "->";
    /// Points back, as in `← providers`.
    ArrowBack => "←", "←", "<-";
    /// A role that has no value of its own.
    Hollow => "○", "○", "o";
    /// Before the place of a role in the ctrl+p cycle.
    Cycle => "⟳", "⟳", "~";
    /// Between the steps of a sequence: `smol › default`.
    Step => "›", "›", ">";
}

/// The characters of a box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxChars {
    pub top_left: &'static str,
    pub top_right: &'static str,
    pub bottom_left: &'static str,
    pub bottom_right: &'static str,
    pub horizontal: &'static str,
    pub vertical: &'static str,
}

/// The characters of a table grid: corners and joints of the top, middle and bottom rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableChars {
    pub top: [&'static str; 3],
    pub middle: [&'static str; 3],
    pub bottom: [&'static str; 3],
    pub horizontal: &'static str,
    pub vertical: &'static str,
}

const ROUNDED: BoxChars = BoxChars {
    top_left: "╭",
    top_right: "╮",
    bottom_left: "╰",
    bottom_right: "╯",
    horizontal: "─",
    vertical: "│",
};

const ASCII_BOX: BoxChars = BoxChars {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    horizontal: "-",
    vertical: "|",
};

const SHARP_TABLE: TableChars = TableChars {
    top: ["┌", "┬", "┐"],
    middle: ["├", "┼", "┤"],
    bottom: ["└", "┴", "┘"],
    horizontal: "─",
    vertical: "│",
};

const ASCII_TABLE: TableChars = TableChars {
    top: ["+", "+", "+"],
    middle: ["+", "+", "+"],
    bottom: ["+", "+", "+"],
    horizontal: "-",
    vertical: "|",
};

const BULLETS: [&str; 3] = ["•", "◦", "▪"];
const ASCII_BULLETS: [&str; 3] = ["*", "-", "+"];

const BRAILLE: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ASCII_SPINNER: &[&str] = &["-", "\\", "|", "/"];

/// The symbol table of one [`GlyphLevel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Glyphs {
    level: GlyphLevel,
}

impl Glyphs {
    pub const fn new(level: GlyphLevel) -> Glyphs {
        Glyphs { level }
    }

    pub fn level(self) -> GlyphLevel {
        self.level
    }

    pub fn get(self, glyph: Glyph) -> &'static str {
        let table = match self.level {
            GlyphLevel::Nerd => NERD,
            GlyphLevel::Unicode => UNICODE,
            GlyphLevel::Ascii => ASCII,
        };
        table[glyph as usize]
    }

    /// The characters of a rounded box (`+-|` at `ascii`).
    pub fn border(self) -> BoxChars {
        match self.level {
            GlyphLevel::Ascii => ASCII_BOX,
            _ => ROUNDED,
        }
    }

    pub fn table(self) -> TableChars {
        match self.level {
            GlyphLevel::Ascii => ASCII_TABLE,
            _ => SHARP_TABLE,
        }
    }

    /// The frames of the working spinner.
    pub fn spinner(self) -> &'static [&'static str] {
        match self.level {
            GlyphLevel::Ascii => ASCII_SPINNER,
            _ => BRAILLE,
        }
    }

    /// The bullets of a list, one per nesting depth, used in turn.
    pub fn bullets(self) -> [&'static str; 3] {
        match self.level {
            GlyphLevel::Ascii => ASCII_BULLETS,
            _ => BULLETS,
        }
    }

    /// `glyph` followed by a space, or nothing when it is empty at this level, so a label can
    /// be written `format!("{}{label}", glyphs.prefix(Glyph::Branch))`.
    pub fn prefix(self, glyph: Glyph) -> String {
        match self.get(glyph) {
            "" => String::new(),
            icon => format!("{icon} "),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn the_environment_wins_then_the_setting_then_the_locale() {
        let level = GlyphLevel::detect("unicode", env(&[("AZAZEL_GLYPHS", "nerd")]));
        assert_eq!(level, GlyphLevel::Nerd);
        assert_eq!(GlyphLevel::detect("ascii", env(&[])), GlyphLevel::Ascii);
        assert_eq!(GlyphLevel::detect("auto", env(&[])), GlyphLevel::Unicode);
        let latin = env(&[("LANG", "de_DE.ISO-8859-1")]);
        assert_eq!(GlyphLevel::detect("auto", latin), GlyphLevel::Ascii);
        let utf8 = env(&[("LC_ALL", "en_US.UTF-8"), ("LANG", "C")]);
        assert_eq!(GlyphLevel::detect("auto", utf8), GlyphLevel::Unicode);
        // A value that names no level is ignored rather than trusted.
        let bogus = env(&[("AZAZEL_GLYPHS", "fancy")]);
        assert_eq!(GlyphLevel::detect("ascii", bogus), GlyphLevel::Ascii);
    }

    #[test]
    fn every_level_has_every_glyph_and_names_round_trip() {
        assert_eq!(NERD.len(), Glyph::ALL.len());
        assert_eq!(UNICODE.len(), Glyph::ALL.len());
        assert_eq!(ASCII.len(), Glyph::ALL.len());
        for level in GlyphLevel::ALL {
            assert_eq!(level.name().parse::<GlyphLevel>(), Ok(level));
        }
        assert!("auto".parse::<GlyphLevel>().is_err());
    }

    #[test]
    fn symbols_are_one_cell_and_ascii_is_ascii() {
        for &glyph in Glyph::ALL {
            for level in [GlyphLevel::Nerd, GlyphLevel::Unicode] {
                let symbol = Glyphs::new(level).get(glyph);
                // Pairs such as a tree joint, `⏵⏵` and the separator are several cells on
                // purpose; a single symbol must not be double width.
                if symbol.chars().count() == 1 {
                    assert_eq!(text::width(symbol), 1, "{glyph:?} at {level} is `{symbol}`");
                }
            }
            assert!(Glyphs::new(GlyphLevel::Ascii).get(glyph).is_ascii(), "{glyph:?}");
        }
        let ascii = Glyphs::new(GlyphLevel::Ascii);
        assert!(ascii.spinner().iter().all(|frame| frame.is_ascii()));
        assert!(ascii.bullets().iter().all(|bullet| bullet.is_ascii()));
        assert!(ascii.border().top_left.is_ascii() && ascii.table().middle[1].is_ascii());
    }
}
