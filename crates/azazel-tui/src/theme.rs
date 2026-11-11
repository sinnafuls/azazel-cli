//! Colour themes: semantic roles mapped to terminal colours, with 256-colour downgrade.

use std::fmt::Write as _;
use std::str::FromStr;

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
    Border,
    /// Background of code blocks.
    CodeBg,
    DiffAdd,
    DiffDel,
}

impl Role {
    pub const ALL: [Role; 10] = [
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
        }
    }

    pub fn from_name(name: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|role| role.name() == name)
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
    colors: [Color; 10],
    depth: ColorDepth,
}

impl Theme {
    pub fn dark() -> Theme {
        Theme {
            colors: [
                Color::Default,
                Color::Rgb(127, 132, 142),
                Color::Rgb(110, 168, 254),
                Color::Rgb(126, 198, 153),
                Color::Rgb(229, 192, 123),
                Color::Rgb(240, 113, 120),
                Color::Rgb(88, 96, 112),
                Color::Rgb(30, 34, 42),
                Color::Rgb(126, 198, 153),
                Color::Rgb(240, 113, 120),
            ],
            depth: ColorDepth::TrueColor,
        }
    }

    pub fn light() -> Theme {
        Theme {
            colors: [
                Color::Default,
                Color::Rgb(110, 119, 129),
                Color::Rgb(9, 105, 218),
                Color::Rgb(26, 127, 55),
                Color::Rgb(154, 103, 0),
                Color::Rgb(207, 34, 46),
                Color::Rgb(175, 184, 193),
                Color::Rgb(240, 242, 245),
                Color::Rgb(26, 127, 55),
                Color::Rgb(207, 34, 46),
            ],
            depth: ColorDepth::TrueColor,
        }
    }

    /// Parses a theme file: a flat table of role name to colour (`"#rrggbb"`, an integer 0-255
    /// or an ANSI colour name). Roles that are absent keep the colour of [`Theme::dark`].
    pub fn from_toml(source: &str) -> Result<Theme, ThemeError> {
        let table: toml::Table = source
            .parse()
            .map_err(|e: toml::de::Error| ThemeError::Parse(e.to_string()))?;
        let mut theme = Theme::dark();
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

    pub fn color(&self, role: Role) -> Color {
        self.colors[role.index()]
    }

    pub fn set_color(&mut self, role: Role, color: Color) {
        self.colors[role.index()] = color;
    }

    /// Whether the theme is meant for a light terminal background, judged by its code background.
    pub fn is_light(&self) -> bool {
        self.color(Role::CodeBg)
            .to_rgb()
            .is_some_and(|(r, g, b)| 299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b) > 128_000)
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

    /// Wraps `text` in the role's colour (as a background for [`Role::CodeBg`], as a foreground
    /// otherwise) followed by [`RESET`]. Text is returned unchanged when it is empty or the role
    /// uses the terminal default.
    pub fn paint(&self, role: Role, text: &str) -> String {
        if text.is_empty() || self.color(role) == Color::Default {
            return text.to_string();
        }
        let prefix = if role == Role::CodeBg {
            self.bg(role)
        } else {
            self.fg(role)
        };
        format!("{prefix}{text}{RESET}")
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
    fn lightness_follows_code_background() {
        assert!(!Theme::dark().is_light());
        assert!(Theme::light().is_light());
    }
}
