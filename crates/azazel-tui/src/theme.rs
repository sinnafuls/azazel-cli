//! Colour themes: semantic roles mapped to terminal colours, with 256-colour downgrade.

use std::fmt::Write as _;
use std::str::FromStr;

use crate::glyphs::{Glyph, GlyphLevel, Glyphs};

/// Resets every SGR attribute.
pub const RESET: &str = "\x1b[0m";

/// What a colour is used for. Components ask for roles, never for concrete colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Text,
    Muted,
    Accent,
    Success,
    Warning,
    Error,
    /// Rules around the input and overlays, quote bars, table lines.
    Border,
    /// Background of code blocks.
    CodeBg,
    DiffAdd,
    DiffDel,
    /// Background of the band a user message is shown on.
    UserBg,
    /// Text on [`Role::UserBg`].
    UserText,
    /// Background of the selected row of a list.
    SelectedBg,
    /// Frames that should stay quiet, such as the card of a finished tool call.
    BorderMuted,
    /// Background of an added diff line.
    DiffAddBg,
    /// Background of a removed diff line.
    DiffDelBg,
    /// The model's reasoning.
    Thinking,
    /// Input rules while a `!` shell command is being typed.
    Bash,
    /// Input rules in permission mode `plan`.
    ModePlan,
    /// Input rules in permission mode `accept-edits`.
    ModeEdits,
    /// Input rules in permission mode `auto`.
    ModeAuto,
    /// Background inside a popup box.
    Surface,
    /// Text of the screen behind a popup, dimmed so the box stands out.
    Backdrop,
}

const ROLE_COUNT: usize = 23;

impl Role {
    pub const ALL: [Role; ROLE_COUNT] = [
        Role::Text,
        Role::Muted,
        Role::Accent,
        Role::Success,
        Role::Warning,
        Role::Error,
        Role::Border,
        Role::CodeBg,
        Role::DiffAdd,
        Role::DiffDel,
        Role::UserBg,
        Role::UserText,
        Role::SelectedBg,
        Role::BorderMuted,
        Role::DiffAddBg,
        Role::DiffDelBg,
        Role::Thinking,
        Role::Bash,
        Role::ModePlan,
        Role::ModeEdits,
        Role::ModeAuto,
        Role::Surface,
        Role::Backdrop,
    ];

    /// The key naming this role in a theme file.
    pub fn name(self) -> &'static str {
        match self {
            Role::Text => "text",
            Role::Muted => "muted",
            Role::Accent => "accent",
            Role::Success => "success",
            Role::Warning => "warning",
            Role::Error => "error",
            Role::Border => "border",
            Role::CodeBg => "code_bg",
            Role::DiffAdd => "diff_add",
            Role::DiffDel => "diff_del",
            Role::UserBg => "user_bg",
            Role::UserText => "user_text",
            Role::SelectedBg => "selected_bg",
            Role::BorderMuted => "border_muted",
            Role::DiffAddBg => "diff_add_bg",
            Role::DiffDelBg => "diff_del_bg",
            Role::Thinking => "thinking",
            Role::Bash => "bash",
            Role::ModePlan => "mode_plan",
            Role::ModeEdits => "mode_edits",
            Role::ModeAuto => "mode_auto",
            Role::Surface => "surface",
            Role::Backdrop => "backdrop",
        }
    }

    pub fn from_name(name: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|role| role.name() == name)
    }

    /// Whether the role colours the background of a cell rather than its text.
    pub fn is_background(self) -> bool {
        matches!(
            self,
            Role::CodeBg
                | Role::UserBg
                | Role::SelectedBg
                | Role::DiffAddBg
                | Role::DiffDelBg
                | Role::Surface
        )
    }

    fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color {
    /// The terminal's own foreground or background.
    Default,
    /// An xterm palette index.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid colour `{0}`: expected \"#rrggbb\", a palette index 0-255 or an ANSI colour name")]
pub struct ColorParseError(String);

const ANSI_NAMES: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];

impl FromStr for Color {
    type Err = ColorParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ColorParseError(s.to_string());
        let lower = s.trim().to_ascii_lowercase();
        if let Some(hex) = lower.strip_prefix('#') {
            if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(err());
            }
            let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| err());
            return Ok(Color::Rgb(channel(0)?, channel(2)?, channel(4)?));
        }
        if lower.bytes().all(|b| b.is_ascii_digit()) && !lower.is_empty() {
            return lower.parse::<u8>().map(Color::Indexed).map_err(|_| err());
        }
        let name = lower.replace(['-', ' '], "_");
        if name == "default" {
            return Ok(Color::Default);
        }
        if matches!(name.as_str(), "gray" | "grey") {
            return Ok(Color::Indexed(8));
        }
        let (base, offset) = match name.strip_prefix("bright_") {
            Some(rest) => (rest, 8),
            None => (name.as_str(), 0),
        };
        ANSI_NAMES
            .iter()
            .position(|candidate| *candidate == base)
            .and_then(|i| u8::try_from(i + offset).ok())
            .map(Color::Indexed)
            .ok_or_else(err)
    }
}

const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// RGB values xterm assigns to the sixteen base colours.
const BASE_COLORS: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

fn distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let d = |x: u8, y: u8| u32::from(x.abs_diff(y)).pow(2);
    d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)
}

fn nearest_level(v: u8) -> u8 {
    let mut best = 0u8;
    for (i, level) in (0u8..).zip(CUBE_LEVELS) {
        if level.abs_diff(v) < CUBE_LEVELS[usize::from(best)].abs_diff(v) {
            best = i;
        }
    }
    best
}

/// The xterm-256 palette entry closest to an RGB colour, chosen from the 6x6x6 cube and the
/// greyscale ramp (the sixteen base colours are skipped because terminals remap them).
pub fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    let (ri, gi, bi) = (nearest_level(r), nearest_level(g), nearest_level(b));
    let cube = (
        CUBE_LEVELS[usize::from(ri)],
        CUBE_LEVELS[usize::from(gi)],
        CUBE_LEVELS[usize::from(bi)],
    );
    let cube_index = 16 + 36 * ri + 6 * gi + bi;
    let mut best = (distance((r, g, b), cube), cube_index);
    for step in 0u8..24 {
        let level = 8 + 10 * step;
        let d = distance((r, g, b), (level, level, level));
        if d < best.0 {
            best = (d, 232 + step);
        }
    }
    best.1
}

impl Color {
    /// Approximate RGB value; `None` for the terminal default.
    pub fn to_rgb(self) -> Option<(u8, u8, u8)> {
        match self {
            Color::Default => None,
            Color::Rgb(r, g, b) => Some((r, g, b)),
            Color::Indexed(n) => Some(match n {
                0..=15 => BASE_COLORS[usize::from(n)],
                16..=231 => {
                    let n = n - 16;
                    let level = |i: u8| CUBE_LEVELS[usize::from(i)];
                    (level(n / 36), level(n / 6 % 6), level(n % 6))
                }
                232..=255 => {
                    let level = 8 + 10 * (n - 232);
                    (level, level, level)
                }
            }),
        }
    }
}

/// How many colours the terminal can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorDepth {
    #[default]
    TrueColor,
    Ansi256,
}

impl ColorDepth {
    /// Decides from the value of the `COLORTERM` environment variable.
    pub fn detect(colorterm: Option<&str>) -> ColorDepth {
        match colorterm.map(|value| value.trim().to_ascii_lowercase()) {
            Some(value) if value == "truecolor" || value == "24bit" => ColorDepth::TrueColor,
            _ => ColorDepth::Ansi256,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ThemeError {
    #[error("invalid theme file: {0}")]
    Parse(String),
    #[error("unknown theme key `{0}`")]
    UnknownKey(String),
    #[error("invalid colour for `{key}`: {reason}")]
    InvalidColor { key: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    colors: [Color; ROLE_COUNT],
    depth: ColorDepth,
    /// The terminal follows OSC 8 hyperlinks.
    links: bool,
    glyphs: Glyphs,
}

impl Theme {
    /// Violet accent and indigo rules on the terminal's own dark background.
    pub fn dark() -> Theme {
        Theme {
            colors: [
                Color::Default,            // text
                Color::Rgb(138, 143, 163), // muted
                Color::Rgb(180, 142, 245), // accent
                Color::Rgb(126, 198, 153), // success
                Color::Rgb(229, 192, 123), // warning
                Color::Rgb(240, 113, 120), // error
                Color::Rgb(107, 116, 168), // border
                Color::Rgb(30, 34, 42),    // code_bg
                Color::Rgb(126, 198, 153), // diff_add
                Color::Rgb(240, 113, 120), // diff_del
                Color::Rgb(43, 48, 74),    // user_bg
                Color::Rgb(229, 231, 245), // user_text
                Color::Rgb(52, 56, 82),    // selected_bg
                Color::Rgb(74, 80, 100),   // border_muted
                Color::Rgb(34, 92, 43),    // diff_add_bg
                Color::Rgb(122, 41, 54),   // diff_del_bg
                Color::Rgb(150, 142, 190), // thinking
                Color::Rgb(253, 93, 177),  // bash
                Color::Rgb(97, 175, 239),  // mode_plan
                Color::Rgb(126, 198, 153), // mode_edits
                Color::Rgb(240, 160, 80),  // mode_auto
                Color::Rgb(27, 30, 40),    // surface
                Color::Rgb(78, 82, 100),   // backdrop
            ],
            depth: ColorDepth::TrueColor,
            links: false,
            glyphs: Glyphs::new(GlyphLevel::Unicode),
        }
    }

    pub fn light() -> Theme {
        Theme {
            colors: [
                Color::Default,            // text
                Color::Rgb(110, 119, 129), // muted
                Color::Rgb(111, 66, 193),  // accent
                Color::Rgb(26, 127, 55),   // success
                Color::Rgb(154, 103, 0),   // warning
                Color::Rgb(207, 34, 46),   // error
                Color::Rgb(140, 149, 190), // border
                Color::Rgb(240, 242, 245), // code_bg
                Color::Rgb(26, 127, 55),   // diff_add
                Color::Rgb(207, 34, 46),   // diff_del
                Color::Rgb(230, 233, 247), // user_bg
                Color::Rgb(31, 35, 53),    // user_text
                Color::Rgb(221, 226, 242), // selected_bg
                Color::Rgb(200, 206, 216), // border_muted
                Color::Rgb(218, 251, 225), // diff_add_bg
                Color::Rgb(255, 235, 233), // diff_del_bg
                Color::Rgb(110, 100, 150), // thinking
                Color::Rgb(191, 57, 137),  // bash
                Color::Rgb(9, 105, 218),   // mode_plan
                Color::Rgb(26, 127, 55),   // mode_edits
                Color::Rgb(188, 76, 0),    // mode_auto
                Color::Rgb(250, 250, 253), // surface
                Color::Rgb(180, 186, 198), // backdrop
            ],
            depth: ColorDepth::TrueColor,
            links: false,
            glyphs: Glyphs::new(GlyphLevel::Unicode),
        }
    }

    /// The names of the themes built into azazel, for pickers and error messages.
    pub const BUILTINS: [&'static str; 6] = [
        "dark",
        "light",
        "catppuccin-mocha",
        "tokyo-night",
        "gruvbox-dark",
        "dark-daltonized",
    ];

    /// A theme built into azazel, by name.
    pub fn builtin(name: &str) -> Option<Theme> {
        let theme = match name {
            "dark" => Theme::dark(),
            "light" => Theme::light(),
            "catppuccin-mocha" => Theme::dark().with_colors(&[
                (Role::Text, 0xcdd6f4),
                (Role::Muted, 0xa6adc8),
                (Role::Accent, 0xcba6f7),
                (Role::Success, 0xa6e3a1),
                (Role::Warning, 0xf9e2af),
                (Role::Error, 0xf38ba8),
                (Role::Border, 0x7f849c),
                (Role::CodeBg, 0x181825),
                (Role::DiffAdd, 0xa6e3a1),
                (Role::DiffDel, 0xf38ba8),
                (Role::UserBg, 0x313244),
                (Role::UserText, 0xcdd6f4),
                (Role::SelectedBg, 0x45475a),
                (Role::BorderMuted, 0x585b70),
                (Role::DiffAddBg, 0x2c4433),
                (Role::DiffDelBg, 0x4d2a35),
                (Role::Thinking, 0xb4befe),
                (Role::Bash, 0xf5c2e7),
                (Role::ModePlan, 0x89dceb),
                (Role::ModeEdits, 0xa6e3a1),
                (Role::ModeAuto, 0xfab387),
                (Role::Surface, 0x1e1e2e),
                (Role::Backdrop, 0x585b70),
            ]),
            "tokyo-night" => Theme::dark().with_colors(&[
                (Role::Text, 0xc0caf5),
                (Role::Muted, 0xa9b1d6),
                (Role::Accent, 0xbb9af7),
                (Role::Success, 0x9ece6a),
                (Role::Warning, 0xe0af68),
                (Role::Error, 0xf7768e),
                (Role::Border, 0x565f89),
                (Role::CodeBg, 0x16161e),
                (Role::DiffAdd, 0x9ece6a),
                (Role::DiffDel, 0xf7768e),
                (Role::UserBg, 0x292e42),
                (Role::UserText, 0xc0caf5),
                (Role::SelectedBg, 0x283457),
                (Role::BorderMuted, 0x3b4261),
                (Role::DiffAddBg, 0x243e33),
                (Role::DiffDelBg, 0x4b2a37),
                (Role::Thinking, 0x7aa2f7),
                (Role::Bash, 0x7dcfff),
                (Role::ModePlan, 0x7dcfff),
                (Role::ModeEdits, 0x9ece6a),
                (Role::ModeAuto, 0xff9e64),
                (Role::Surface, 0x1a1b26),
                (Role::Backdrop, 0x545c7e),
            ]),
            "gruvbox-dark" => Theme::dark().with_colors(&[
                (Role::Text, 0xebdbb2),
                (Role::Muted, 0xa89984),
                (Role::Accent, 0xfabd2f),
                (Role::Success, 0xb8bb26),
                (Role::Warning, 0xfe8019),
                (Role::Error, 0xfb4934),
                (Role::Border, 0x7c6f64),
                (Role::CodeBg, 0x1d2021),
                (Role::DiffAdd, 0xb8bb26),
                (Role::DiffDel, 0xfb4934),
                (Role::UserBg, 0x3c3836),
                (Role::UserText, 0xebdbb2),
                (Role::SelectedBg, 0x504945),
                (Role::BorderMuted, 0x504945),
                (Role::DiffAddBg, 0x32361a),
                (Role::DiffDelBg, 0x4a2020),
                (Role::Thinking, 0xd3869b),
                (Role::Bash, 0x8ec07c),
                (Role::ModePlan, 0x83a598),
                (Role::ModeEdits, 0xb8bb26),
                (Role::ModeAuto, 0xfe8019),
                (Role::Surface, 0x282828),
                (Role::Backdrop, 0x665c54),
            ]),
            // Red and green carry no meaning a colour-blind reader could miss: success and
            // additions are blue, failures and removals orange.
            "dark-daltonized" => Theme::dark().with_colors(&[
                (Role::Success, 0x6cb6ff),
                (Role::Error, 0xf0a050),
                (Role::DiffAdd, 0x6cb6ff),
                (Role::DiffDel, 0xf0a050),
                (Role::DiffAddBg, 0x1f3a5c),
                (Role::DiffDelBg, 0x5c3a12),
                (Role::ModeEdits, 0x6cb6ff),
            ]),
            _ => return None,
        };
        Some(theme)
    }

    /// The theme with each listed role set to a `0xrrggbb` colour.
    fn with_colors(mut self, colors: &[(Role, u32)]) -> Theme {
        for &(role, rgb) in colors {
            let [_, r, g, b] = rgb.to_be_bytes();
            self.colors[role.index()] = Color::Rgb(r, g, b);
        }
        self
    }

    /// Parses a theme file: a flat table of role name to colour (`"#rrggbb"`, an integer 0-255
    /// or an ANSI colour name). Roles that are absent keep the colour of [`Theme::dark`], or of
    /// [`Theme::light`] when the file sets a light `code_bg`, so a file written before a role
    /// existed still gets colours that suit its background.
    pub fn from_toml(source: &str) -> Result<Theme, ThemeError> {
        let table: toml::Table = source
            .parse()
            .map_err(|e: toml::de::Error| ThemeError::Parse(e.to_string()))?;
        let mut set: Vec<(Role, Color)> = Vec::with_capacity(table.len());
        for (key, value) in &table {
            let role = Role::from_name(key).ok_or_else(|| ThemeError::UnknownKey(key.clone()))?;
            let invalid = |reason: String| ThemeError::InvalidColor {
                key: key.clone(),
                reason,
            };
            let color = match value {
                toml::Value::String(text) => text.parse::<Color>().map_err(|e| invalid(e.to_string()))?,
                toml::Value::Integer(n) => u8::try_from(*n)
                    .map(Color::Indexed)
                    .map_err(|_| invalid(format!("palette index {n} is outside 0-255")))?,
                other => {
                    let found = other.type_str();
                    return Err(invalid(format!("expected a string or an integer, found {found}")));
                }
            };
            set.push((role, color));
        }
        let light = set
            .iter()
            .any(|(role, color)| *role == Role::CodeBg && is_light_color(*color));
        let mut theme = if light { Theme::light() } else { Theme::dark() };
        for (role, color) in set {
            theme.colors[role.index()] = color;
        }
        Ok(theme)
    }

    /// Returns the theme with its output colour depth replaced.
    pub fn with_depth(mut self, depth: ColorDepth) -> Theme {
        self.depth = depth;
        self
    }

    pub fn depth(&self) -> ColorDepth {
        self.depth
    }

    /// Returns the theme with hyperlinks on or off: whether [`Theme::link`] makes text
    /// clickable.
    pub fn with_links(mut self, links: bool) -> Theme {
        self.links = links;
        self
    }

    pub fn links(&self) -> bool {
        self.links
    }

    /// Returns the theme drawing with the symbols of `level`.
    pub fn with_glyphs(mut self, level: GlyphLevel) -> Theme {
        self.glyphs = Glyphs::new(level);
        self
    }

    pub fn glyphs(&self) -> Glyphs {
        self.glyphs
    }

    /// One symbol at this theme's glyph level.
    pub fn glyph(&self, glyph: Glyph) -> &'static str {
        self.glyphs.get(glyph)
    }

    /// `text` as a hyperlink to `url` when the terminal follows them, else `text` alone.
    pub fn link(&self, url: &str, text: &str) -> String {
        if self.links {
            crate::term::hyperlink(url, text)
        } else {
            text.to_string()
        }
    }

    pub fn color(&self, role: Role) -> Color {
        self.colors[role.index()]
    }

    pub fn set_color(&mut self, role: Role, color: Color) {
        self.colors[role.index()] = color;
    }

    /// Whether the theme is meant for a light terminal background, judged by its code background.
    pub fn is_light(&self) -> bool {
        is_light_color(self.color(Role::CodeBg))
    }

    /// SGR sequence selecting `color` as the foreground at this theme's colour depth.
    pub fn fg_color(&self, color: Color) -> String {
        self.sgr(color, false)
    }

    /// SGR sequence selecting `color` as the background at this theme's colour depth.
    pub fn bg_color(&self, color: Color) -> String {
        self.sgr(color, true)
    }

    /// SGR prefix that sets the foreground to the role's colour.
    pub fn fg(&self, role: Role) -> String {
        self.fg_color(self.color(role))
    }

    /// SGR prefix that sets the background to the role's colour.
    pub fn bg(&self, role: Role) -> String {
        self.bg_color(self.color(role))
    }

    /// Wraps `text` in the role's colour (as a background for the roles that are backgrounds,
    /// as a foreground otherwise) followed by [`RESET`]. Text is returned unchanged when it is
    /// empty or the role uses the terminal default.
    pub fn paint(&self, role: Role, text: &str) -> String {
        if text.is_empty() || self.color(role) == Color::Default {
            return text.to_string();
        }
        let prefix = if role.is_background() {
            self.bg(role)
        } else {
            self.fg(role)
        };
        format!("{prefix}{text}{RESET}")
    }

    /// A full-width band: `text` on the background `bg` in the foreground `fg`, padded with
    /// spaces to `width` columns. `text` must not reset styles itself. Text wider than `width`
    /// is cut.
    pub fn band(&self, bg: Role, fg: Role, text: &str, width: usize) -> String {
        let text = crate::text::truncate(text, width, "");
        let padded = crate::text::pad_right(text.strip_suffix(RESET).unwrap_or(&text), width);
        format!("{}{}{padded}{RESET}", self.bg(bg), self.fg(fg))
    }

    fn sgr(&self, color: Color, background: bool) -> String {
        let mut out = String::from("\x1b[");
        let color = match (color, self.depth) {
            (Color::Rgb(r, g, b), ColorDepth::Ansi256) => Color::Indexed(nearest_256(r, g, b)),
            (other, _) => other,
        };
        // Writing to a String cannot fail.
        let _ = match color {
            Color::Default => write!(out, "{}", if background { 49 } else { 39 }),
            Color::Indexed(n @ 0..=7) => write!(out, "{}", u16::from(n) + if background { 40 } else { 30 }),
            Color::Indexed(n @ 8..=15) => {
                write!(out, "{}", u16::from(n) - 8 + if background { 100 } else { 90 })
            }
            Color::Indexed(n) => write!(out, "{};5;{n}", if background { 48 } else { 38 }),
            Color::Rgb(r, g, b) => write!(out, "{};2;{r};{g};{b}", if background { 48 } else { 38 }),
        };
        out.push('m');
        out
    }
}

fn is_light_color(color: Color) -> bool {
    color
        .to_rgb()
        .is_some_and(|(r, g, b)| 299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b) > 128_000)
}

impl Default for Theme {
    fn default() -> Self {
        Theme::dark()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colour_forms() {
        assert_eq!("#ff8000".parse::<Color>(), Ok(Color::Rgb(255, 128, 0)));
        assert_eq!("#FF8000".parse::<Color>(), Ok(Color::Rgb(255, 128, 0)));
        assert_eq!("42".parse::<Color>(), Ok(Color::Indexed(42)));
        assert_eq!("255".parse::<Color>(), Ok(Color::Indexed(255)));
        assert_eq!("red".parse::<Color>(), Ok(Color::Indexed(1)));
        assert_eq!("Bright-Blue".parse::<Color>(), Ok(Color::Indexed(12)));
        assert_eq!("grey".parse::<Color>(), Ok(Color::Indexed(8)));
        assert_eq!("default".parse::<Color>(), Ok(Color::Default));
    }

    #[test]
    fn rejects_malformed_colours() {
        for bad in [
            "#fff", "#gggggg", "256", "-1", "reddish", "", "#ff80000", "bright_",
        ] {
            assert!(bad.parse::<Color>().is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn detects_colour_depth() {
        assert_eq!(ColorDepth::detect(Some("truecolor")), ColorDepth::TrueColor);
        assert_eq!(ColorDepth::detect(Some("24bit")), ColorDepth::TrueColor);
        assert_eq!(ColorDepth::detect(Some("TrueColor")), ColorDepth::TrueColor);
        assert_eq!(ColorDepth::detect(Some("yes")), ColorDepth::Ansi256);
        assert_eq!(ColorDepth::detect(Some("")), ColorDepth::Ansi256);
        assert_eq!(ColorDepth::detect(None), ColorDepth::Ansi256);
    }

    #[test]
    fn downgrades_known_rgb_values() {
        assert_eq!(nearest_256(0, 0, 0), 16);
        assert_eq!(nearest_256(255, 255, 255), 231);
        assert_eq!(nearest_256(255, 0, 0), 196);
        assert_eq!(nearest_256(0, 255, 0), 46);
        assert_eq!(nearest_256(0, 0, 255), 21);
        assert_eq!(nearest_256(95, 135, 175), 67);
        assert_eq!(nearest_256(128, 128, 128), 244);
        assert_eq!(nearest_256(8, 8, 8), 232);
        assert_eq!(nearest_256(250, 100, 100), 203);
    }

    #[test]
    fn palette_round_trips_through_rgb() {
        for n in 16..=255u8 {
            let (r, g, b) = Color::Indexed(n)
                .to_rgb()
                .expect("indexed colours have an RGB value");
            assert_eq!(nearest_256(r, g, b), n, "palette entry {n}");
        }
    }

    #[test]
    fn sgr_sequences_follow_depth() {
        let mut theme = Theme::dark();
        theme.set_color(Role::Accent, Color::Rgb(255, 0, 0));
        assert_eq!(theme.fg(Role::Accent), "\x1b[38;2;255;0;0m");
        assert_eq!(theme.bg(Role::Accent), "\x1b[48;2;255;0;0m");
        let theme = theme.with_depth(ColorDepth::Ansi256);
        assert_eq!(theme.fg(Role::Accent), "\x1b[38;5;196m");
        assert_eq!(theme.bg(Role::Accent), "\x1b[48;5;196m");
    }

    #[test]
    fn sgr_sequences_for_palette_and_default() {
        let theme = Theme::dark();
        assert_eq!(theme.fg_color(Color::Default), "\x1b[39m");
        assert_eq!(theme.bg_color(Color::Default), "\x1b[49m");
        assert_eq!(theme.fg_color(Color::Indexed(1)), "\x1b[31m");
        assert_eq!(theme.bg_color(Color::Indexed(7)), "\x1b[47m");
        assert_eq!(theme.fg_color(Color::Indexed(9)), "\x1b[91m");
        assert_eq!(theme.bg_color(Color::Indexed(15)), "\x1b[107m");
        assert_eq!(theme.fg_color(Color::Indexed(200)), "\x1b[38;5;200m");
    }

    #[test]
    fn paint_wraps_text_and_skips_default() {
        let mut theme = Theme::dark();
        theme.set_color(Role::Error, Color::Indexed(1));
        assert_eq!(theme.paint(Role::Error, "boom"), "\x1b[31mboom\x1b[0m");
        assert_eq!(theme.paint(Role::Error, ""), "");
        assert_eq!(theme.color(Role::Text), Color::Default);
        assert_eq!(theme.paint(Role::Text, "plain"), "plain");
        theme.set_color(Role::CodeBg, Color::Indexed(236));
        assert_eq!(theme.paint(Role::CodeBg, "x"), "\x1b[48;5;236mx\x1b[0m");
    }

    #[test]
    fn toml_overrides_and_inherits_from_dark() {
        let theme = Theme::from_toml("accent = \"#102030\"\nerror = 9\nmuted = \"bright_black\"\n")
            .expect("valid theme");
        assert_eq!(theme.color(Role::Accent), Color::Rgb(16, 32, 48));
        assert_eq!(theme.color(Role::Error), Color::Indexed(9));
        assert_eq!(theme.color(Role::Muted), Color::Indexed(8));
        let dark = Theme::dark();
        for role in [
            Role::Text,
            Role::Success,
            Role::Warning,
            Role::Border,
            Role::CodeBg,
            Role::DiffAdd,
        ] {
            assert_eq!(theme.color(role), dark.color(role), "{role:?} should inherit");
        }
        assert_eq!(Theme::from_toml(""), Ok(dark));
    }

    #[test]
    fn toml_with_a_light_code_background_inherits_from_light() {
        let theme = Theme::from_toml("code_bg = \"#f5f5f5\"\naccent = \"#102030\"\n").expect("valid theme");
        let light = Theme::light();
        assert_eq!(theme.color(Role::Accent), Color::Rgb(16, 32, 48));
        for role in [Role::UserBg, Role::DiffAddBg, Role::SelectedBg, Role::Muted] {
            assert_eq!(theme.color(role), light.color(role), "{role:?} should inherit");
        }
    }

    #[test]
    fn background_roles_paint_the_background() {
        let mut theme = Theme::dark();
        for role in Role::ALL {
            theme.set_color(role, Color::Indexed(200));
            let painted = theme.paint(role, "x");
            let expected = if role.is_background() {
                "\x1b[48;5;200mx\x1b[0m"
            } else {
                "\x1b[38;5;200mx\x1b[0m"
            };
            assert_eq!(painted, expected, "{role:?}");
        }
    }

    #[test]
    fn band_fills_the_width() {
        let mut theme = Theme::dark();
        theme.set_color(Role::UserBg, Color::Indexed(236));
        theme.set_color(Role::UserText, Color::Indexed(15));
        assert_eq!(
            theme.band(Role::UserBg, Role::UserText, " hi", 6),
            "\x1b[48;5;236m\x1b[97m hi   \x1b[0m"
        );
        assert_eq!(
            crate::text::width(&theme.band(Role::UserBg, Role::UserText, "too long", 4)),
            4
        );
    }

    #[test]
    fn toml_accepts_every_role_name() {
        let entries: Vec<String> = Role::ALL
            .iter()
            .map(|role| format!("{} = 7", role.name()))
            .collect();
        let source = entries.join("\n");
        let theme = Theme::from_toml(&source).expect("all role names are valid keys");
        for role in Role::ALL {
            assert_eq!(theme.color(role), Color::Indexed(7));
        }
    }

    #[test]
    fn toml_errors_name_the_problem() {
        assert_eq!(
            Theme::from_toml("acent = \"red\""),
            Err(ThemeError::UnknownKey("acent".to_string()))
        );
        match Theme::from_toml("accent = \"#12\"") {
            Err(ThemeError::InvalidColor { key, .. }) => assert_eq!(key, "accent"),
            other => panic!("expected InvalidColor, got {other:?}"),
        }
        match Theme::from_toml("accent = 300") {
            Err(ThemeError::InvalidColor { key, reason }) => {
                assert_eq!(key, "accent");
                assert!(reason.contains("300"));
            }
            other => panic!("expected InvalidColor, got {other:?}"),
        }
        assert!(matches!(
            Theme::from_toml("accent = true"),
            Err(ThemeError::InvalidColor { .. })
        ));
        assert!(matches!(Theme::from_toml("accent = "), Err(ThemeError::Parse(_))));
    }

    #[test]
    fn every_builtin_loads_and_sets_every_role() {
        for name in Theme::BUILTINS {
            let theme = Theme::builtin(name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(theme.is_light(), name == "light", "{name}");
            // A file listing every role of a built-in reproduces it exactly.
            let file: String = Role::ALL
                .iter()
                .map(|role| match theme.color(*role) {
                    Color::Rgb(r, g, b) => format!("{} = \"#{r:02x}{g:02x}{b:02x}\"\n", role.name()),
                    Color::Indexed(n) => format!("{} = {n}\n", role.name()),
                    Color::Default => format!("{} = \"default\"\n", role.name()),
                })
                .collect();
            assert_eq!(Theme::from_toml(&file).unwrap(), theme, "{name}");
        }
        assert!(Theme::builtin("nope").is_none());
    }

    #[test]
    fn glyphs_follow_the_level() {
        let theme = Theme::dark();
        assert_eq!(theme.glyph(Glyph::Success), "✔");
        assert_eq!(theme.with_glyphs(GlyphLevel::Ascii).glyph(Glyph::Success), "[+]");
    }

    #[test]
    fn lightness_follows_code_background() {
        assert!(!Theme::dark().is_light());
        assert!(Theme::light().is_light());
    }
}
