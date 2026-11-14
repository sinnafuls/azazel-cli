//! The colours of the built-in themes. A theme is a family: one palette, or a dark and a light
//! one. The family's own name follows the terminal's background; `<name>-dark` and
//! `<name>-light` pin one palette. Each palette names where its colours come from; the roles
//! the source has no colour for (diff backgrounds, the popup backdrop) are mixed from its own
//! colours.

use super::{ROLE_COUNT, Role, Theme};

/// Every role's colour as `0xrrggbb`, in the order of [`Role::ALL`].
type Palette = [u32; ROLE_COUNT];

/// One palette of a family, under the name that pins it and older names it is known by.
pub(super) struct Variant {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub make: fn() -> Theme,
}

pub(super) struct Family {
    pub name: &'static str,
    pub dark: Option<Variant>,
    pub light: Option<Variant>,
}

impl Family {
    /// The palettes of the family, dark first.
    pub fn variants(&self) -> impl Iterator<Item = (&Variant, bool)> {
        let dark = self.dark.iter().map(|variant| (variant, false));
        dark.chain(self.light.iter().map(|variant| (variant, true)))
    }

    /// Whether the family has a palette for each kind of background.
    pub fn has_both(&self) -> bool {
        self.dark.is_some() && self.light.is_some()
    }
}

fn from(palette: &Palette) -> Theme {
    let mut theme = Theme::dark();
    for (role, rgb) in Role::ALL.into_iter().zip(palette) {
        let [_, r, g, b] = rgb.to_be_bytes();
        theme.set_color(role, super::Color::Rgb(r, g, b));
    }
    theme
}

const fn variant(name: &'static str, make: fn() -> Theme) -> Option<Variant> {
    Some(Variant {
        name,
        aliases: &[],
        make,
    })
}

const fn aka(name: &'static str, aliases: &'static [&'static str], make: fn() -> Theme) -> Option<Variant> {
    Some(Variant { name, aliases, make })
}

/// The built-in themes in the order pickers list them. `auto` is the plain pair: `dark` and
/// `light`, which leave the text in the terminal's own colour.
pub(super) const FAMILIES: &[Family] = &[
    Family {
        name: "auto",
        dark: variant("dark", Theme::dark),
        light: variant("light", Theme::light),
    },
    Family {
        name: "dark-daltonized",
        dark: variant("dark-daltonized", Theme::dark_daltonized),
        light: None,
    },
    Family {
        name: "catppuccin",
        dark: aka("catppuccin-dark", &["catppuccin-mocha"], || {
            from(&CATPPUCCIN_MOCHA)
        }),
        light: aka("catppuccin-light", &["catppuccin-latte"], || {
            from(&CATPPUCCIN_LATTE)
        }),
    },
    Family {
        name: "tokyo-night",
        dark: variant("tokyo-night-dark", || from(&TOKYO_NIGHT)),
        light: aka("tokyo-night-light", &["tokyo-night-day"], || {
            from(&TOKYO_NIGHT_DAY)
        }),
    },
    Family {
        name: "gruvbox",
        dark: variant("gruvbox-dark", || from(&GRUVBOX_DARK)),
        light: variant("gruvbox-light", || from(&GRUVBOX_LIGHT)),
    },
    Family {
        name: "dracula",
        dark: variant("dracula-dark", || from(&DRACULA)),
        light: aka("dracula-light", &["alucard"], || from(&ALUCARD)),
    },
    Family {
        name: "nord",
        dark: variant("nord", || from(&NORD)),
        light: None,
    },
    Family {
        name: "rose-pine",
        dark: aka("rose-pine-dark", &["rose-pine-main"], || from(&ROSE_PINE)),
        light: aka("rose-pine-light", &["rose-pine-dawn"], || from(&ROSE_PINE_DAWN)),
    },
    Family {
        name: "kanagawa",
        dark: aka("kanagawa-dark", &["kanagawa-wave"], || from(&KANAGAWA_WAVE)),
        light: aka("kanagawa-light", &["kanagawa-lotus"], || from(&KANAGAWA_LOTUS)),
    },
    Family {
        name: "one",
        dark: variant("one-dark", || from(&ONE_DARK)),
        light: variant("one-light", || from(&ONE_LIGHT)),
    },
    Family {
        name: "solarized",
        dark: variant("solarized-dark", || from(&SOLARIZED_DARK)),
        light: variant("solarized-light", || from(&SOLARIZED_LIGHT)),
    },
    Family {
        name: "everforest",
        dark: variant("everforest-dark", || from(&EVERFOREST_DARK)),
        light: variant("everforest-light", || from(&EVERFOREST_LIGHT)),
    },
    Family {
        name: "github",
        dark: variant("github-dark", || from(&GITHUB_DARK)),
        light: variant("github-light", || from(&GITHUB_LIGHT)),
    },
    Family {
        name: "ayu",
        dark: variant("ayu-dark", || from(&AYU_DARK)),
        light: variant("ayu-light", || from(&AYU_LIGHT)),
    },
    Family {
        name: "monokai",
        dark: variant("monokai", || from(&MONOKAI)),
        light: None,
    },
];

/// Catppuccin Mocha, <https://catppuccin.com/palette>.
const CATPPUCCIN_MOCHA: Palette = [
    0xcdd6f4, 0xa6adc8, 0xcba6f7, 0xa6e3a1, 0xf9e2af, 0xf38ba8, 0x7f849c, 0x181825, 0xa6e3a1, 0xf38ba8,
    0x313244, 0xcdd6f4, 0x45475a, 0x585b70, 0x2c4433, 0x4d2a35, 0xb4befe, 0xf5c2e7, 0x89dceb, 0xa6e3a1,
    0xfab387, 0x1e1e2e, 0x585b70,
];

/// Catppuccin Latte, <https://catppuccin.com/palette>.
const CATPPUCCIN_LATTE: Palette = [
    0x4c4f69, 0x6c6f85, 0x8839ef, 0x40a02b, 0xdf8e1d, 0xd20f39, 0x8c8fa1, 0xe6e9ef, 0x40a02b, 0xd20f39,
    0xccd0da, 0x4c4f69, 0xbcc0cc, 0xacb0be, 0xc9dfc9, 0xe9bfcc, 0x7287fd, 0xea76cb, 0x04a5e5, 0x40a02b,
    0xfe640b, 0xeff1f5, 0x9ca0b0,
];

/// Tokyo Night, <https://github.com/folke/tokyonight.nvim> (`night`).
const TOKYO_NIGHT: Palette = [
    0xc0caf5, 0xa9b1d6, 0xbb9af7, 0x9ece6a, 0xe0af68, 0xf7768e, 0x565f89, 0x16161e, 0x9ece6a, 0xf7768e,
    0x292e42, 0xc0caf5, 0x283457, 0x3b4261, 0x243e33, 0x4b2a37, 0x7aa2f7, 0x7dcfff, 0x7dcfff, 0x9ece6a,
    0xff9e64, 0x1a1b26, 0x545c7e,
];

/// Tokyo Night Day, <https://github.com/folke/tokyonight.nvim> (`extras/kitty/tokyonight_day.conf`).
const TOKYO_NIGHT_DAY: Palette = [
    0x3760bf, 0x6172b0, 0x9854f1, 0x587539, 0x8c6c3e, 0xf52a65, 0x8990b3, 0xd0d5e3, 0x587539, 0xf52a65,
    0xd0d5e3, 0x3760bf, 0xb7c1e3, 0xa8aecb, 0xc3cac1, 0xe5baca, 0x2e7de9, 0x007197, 0x007197, 0x587539,
    0xb15c00, 0xe1e2e7, 0xa1a6c5,
];

/// Gruvbox dark, <https://github.com/morhetz/gruvbox>.
const GRUVBOX_DARK: Palette = [
    0xebdbb2, 0xa89984, 0xfabd2f, 0xb8bb26, 0xfe8019, 0xfb4934, 0x7c6f64, 0x1d2021, 0xb8bb26, 0xfb4934,
    0x3c3836, 0xebdbb2, 0x504945, 0x504945, 0x32361a, 0x4a2020, 0xd3869b, 0x8ec07c, 0x83a598, 0xb8bb26,
    0xfe8019, 0x282828, 0x665c54,
];

/// Gruvbox light, <https://github.com/morhetz/gruvbox>.
const GRUVBOX_LIGHT: Palette = [
    0x3c3836, 0x7c6f64, 0xb57614, 0x79740e, 0xaf3a03, 0x9d0006, 0x928374, 0xf2e5bc, 0x79740e, 0x9d0006,
    0xebdbb2, 0x3c3836, 0xd5c4a1, 0xd5c4a1, 0xded69e, 0xe6bc9d, 0x8f3f71, 0x427b58, 0x076678, 0x79740e,
    0xaf3a03, 0xfbf1c7, 0xa89984,
];

/// Dracula Classic, <https://draculatheme.com/spec>.
const DRACULA: Palette = [
    0xf8f8f2, 0x8b95c1, 0xbd93f9, 0x50fa7b, 0xf1fa8c, 0xff5555, 0x6272a4, 0x21222c, 0x50fa7b, 0xff5555,
    0x44475a, 0xf8f8f2, 0x44475a, 0x424450, 0x2c4a3a, 0x4f2b35, 0xff79c6, 0x8be9fd, 0x8be9fd, 0x50fa7b,
    0xffb86c, 0x282a36, 0x6272a4,
];

/// Alucard Classic, Dracula's light theme, <https://draculatheme.com/spec>.
const ALUCARD: Palette = [
    0x1f1f1f, 0x6c664b, 0x644ac9, 0x14710a, 0x846e15, 0xcb3a2a, 0x6c664b, 0xefeddc, 0x14710a, 0xcb3a2a,
    0xdedccf, 0x1f1f1f, 0xcfcfde, 0xceccc0, 0xcbddba, 0xf4d1c1, 0x644ac9, 0xa3144d, 0x036a96, 0x14710a,
    0xa34d14, 0xfffbeb, 0xbcbab3,
];

/// Nord, <https://www.nordtheme.com/docs/colors-and-palettes>. Nord has no light theme.
const NORD: Palette = [
    0xeceff4, 0xa3acbd, 0x88c0d0, 0xa3be8c, 0xebcb8b, 0xbf616a, 0x4c566a, 0x3b4252, 0xa3be8c, 0xbf616a,
    0x3b4252, 0xeceff4, 0x434c5e, 0x434c5e, 0x485251, 0x4e3e49, 0xb48ead, 0x8fbcbb, 0x81a1c1, 0xa3be8c,
    0xd08770, 0x2e3440, 0x4c566a,
];

/// Rosé Pine (main), <https://rosepinetheme.com/palette>.
const ROSE_PINE: Palette = [
    0xe0def4, 0x908caa, 0xc4a7e7, 0x9ccfd8, 0xf6c177, 0xeb6f92, 0x6e6a86, 0x1f1d2e, 0x9ccfd8, 0xeb6f92,
    0x26233a, 0xe0def4, 0x403d52, 0x26233a, 0x1f3a3f, 0x3d2335, 0xebbcba, 0xeb6f92, 0x31748f, 0x9ccfd8,
    0xf6c177, 0x191724, 0x6e6a86,
];

/// Rosé Pine Dawn, <https://rosepinetheme.com/palette>.
const ROSE_PINE_DAWN: Palette = [
    0x464261, 0x797593, 0x907aa9, 0x286983, 0xea9d34, 0xb4637a, 0x9893a5, 0xf2e9e1, 0x286983, 0xb4637a,
    0xf2e9e1, 0x464261, 0xdfdad9, 0xdfdad9, 0xccd5d6, 0xebd4d4, 0xd7827e, 0xb4637a, 0x56949f, 0x286983,
    0xea9d34, 0xfaf4ed, 0xcecacd,
];

/// Kanagawa Wave, <https://github.com/rebelot/kanagawa.nvim> (`lua/kanagawa/colors.lua`).
const KANAGAWA_WAVE: Palette = [
    0xdcd7ba, 0x727169, 0x957fb8, 0x98bb6c, 0xe6c384, 0xe46876, 0x54546d, 0x16161d, 0x98bb6c, 0xe46876,
    0x2a2a37, 0xdcd7ba, 0x2d4f67, 0x363646, 0x2b3328, 0x43242b, 0x938aa9, 0x7fb4ca, 0x7e9cd8, 0x98bb6c,
    0xffa066, 0x1f1f28, 0x54546d,
];

/// Kanagawa Lotus, <https://github.com/rebelot/kanagawa.nvim> (`lua/kanagawa/colors.lua`).
const KANAGAWA_LOTUS: Palette = [
    0x545464, 0x716e61, 0x624c83, 0x6f894e, 0x77713f, 0xc84053, 0x8a8980, 0xe7dba0, 0x6f894e, 0xc84053,
    0xe5ddb0, 0x545464, 0xc9cbd1, 0xd5cea3, 0xb7d0ae, 0xd9a594, 0x766b90, 0xb35b79, 0x4d699b, 0x6f894e,
    0xcc6d00, 0xf2ecbc, 0xa09cac,
];

/// Atom One Dark, <https://github.com/atom/atom/tree/master/packages/one-dark-syntax>.
const ONE_DARK: Palette = [
    0xabb2bf, 0x7f848e, 0xc678dd, 0x98c379, 0xe5c07b, 0xe06c75, 0x5c6370, 0x21252b, 0x98c379, 0xe06c75,
    0x2c313a, 0xabb2bf, 0x3e4451, 0x3b4048, 0x414d43, 0x503a42, 0x61afef, 0x56b6c2, 0x61afef, 0x98c379,
    0xd19a66, 0x282c34, 0x5c6370,
];

/// Atom One Light, <https://github.com/atom/atom/tree/master/packages/one-light-syntax>.
const ONE_LIGHT: Palette = [
    0x383a42, 0x696c77, 0xa626a4, 0x50a14f, 0xc18401, 0xe45649, 0xa0a1a7, 0xf0f0f1, 0x50a14f, 0xe45649,
    0xe5e5e6, 0x383a42, 0xdbdbdc, 0xd3d3d4, 0xd5e6d4, 0xf5d6d3, 0x4078f2, 0x0184bc, 0x4078f2, 0x50a14f,
    0x986801, 0xfafafa, 0xa0a1a7,
];

/// Solarized dark, <https://ethanschoonover.com/solarized/>. Text is `base1`, the emphasised tone, for contrast.
const SOLARIZED_DARK: Palette = [
    0x93a1a1, 0x839496, 0x6c71c4, 0x859900, 0xb58900, 0xdc322f, 0x586e75, 0x073642, 0x859900, 0xdc322f,
    0x073642, 0x93a1a1, 0x0a4a5a, 0x094555, 0x1d432a, 0x302d34, 0x268bd2, 0xd33682, 0x2aa198, 0x859900,
    0xcb4b16, 0x002b36, 0x586e75,
];

/// Solarized light, <https://ethanschoonover.com/solarized/>. Text is `base01`, the emphasised tone, for contrast.
const SOLARIZED_LIGHT: Palette = [
    0x586e75, 0x657b83, 0x6c71c4, 0x859900, 0xb58900, 0xdc322f, 0x93a1a1, 0xeee8d5, 0x859900, 0xdc322f,
    0xeee8d5, 0x586e75, 0xe4ddc8, 0xe9e2cb, 0xe3e2b1, 0xf6cbbb, 0x268bd2, 0xd33682, 0x2aa198, 0x859900,
    0xcb4b16, 0xfdf6e3, 0x93a1a1,
];

/// Everforest dark (medium), <https://github.com/sainnhe/everforest>.
const EVERFOREST_DARK: Palette = [
    0xd3c6aa, 0x9da9a0, 0xd699b6, 0xa7c080, 0xdbbc7f, 0xe67e80, 0x7a8478, 0x232a2e, 0xa7c080, 0xe67e80,
    0x343f44, 0xd3c6aa, 0x475258, 0x3d484d, 0x425047, 0x514045, 0x7fbbb3, 0x83c092, 0x7fbbb3, 0xa7c080,
    0xe69875, 0x2d353b, 0x7a8478,
];

/// Everforest light (medium), <https://github.com/sainnhe/everforest>.
const EVERFOREST_LIGHT: Palette = [
    0x5c6a72, 0x829181, 0xdf69ba, 0x8da101, 0xdfa000, 0xf85552, 0xa6b0a0, 0xefebd4, 0x8da101, 0xf85552,
    0xf4f0d9, 0x5c6a72, 0xe6e2cc, 0xe0dcc7, 0xf0f1d2, 0xfde3da, 0x3a94c5, 0x35a77c, 0x3a94c5, 0x8da101,
    0xf57d26, 0xfdf6e3, 0xbdc3af,
];

/// GitHub dark, Primer colours, <https://primer.style/foundations/primitives/color>.
const GITHUB_DARK: Palette = [
    0xe6edf3, 0x9198a1, 0x4493f8, 0x3fb950, 0xd29922, 0xf85149, 0x3d444d, 0x151b23, 0x3fb950, 0xf85149,
    0x212830, 0xe6edf3, 0x262c36, 0x2a313c, 0x1d3a28, 0x4a1f22, 0xab7df8, 0xdb61a2, 0x4493f8, 0x3fb950,
    0xdb6d28, 0x0d1117, 0x656c76,
];

/// GitHub light, Primer colours, <https://primer.style/foundations/primitives/color>.
const GITHUB_LIGHT: Palette = [
    0x1f2328, 0x59636e, 0x0969da, 0x1a7f37, 0x9a6700, 0xd1242f, 0xd1d9e0, 0xf6f8fa, 0x1a7f37, 0xd1242f,
    0xeff2f5, 0x1f2328, 0xdde4ec, 0xe3e7ec, 0xdafbe1, 0xffebe9, 0x8250df, 0xbf3989, 0x0969da, 0x1a7f37,
    0xbc4c00, 0xffffff, 0xafb8c1,
];

/// Ayu dark, <https://github.com/ayu-theme/ayu-colors>.
const AYU_DARK: Palette = [
    0xbfbdb6, 0x8a8f99, 0xe6b450, 0x7fd962, 0xffb454, 0xf26d78, 0x565b66, 0x0b0e14, 0x7fd962, 0xf26d78,
    0x131721, 0xbfbdb6, 0x1f2430, 0x1b1f29, 0x263c28, 0x3f242c, 0xd2a6ff, 0x95e6cb, 0x59c2ff, 0x7fd962,
    0xff8f40, 0x0d1017, 0x565b66,
];

/// Ayu light, <https://github.com/ayu-theme/ayu-colors>.
const AYU_LIGHT: Palette = [
    0x5c6166, 0x787b80, 0xfa8d3e, 0x6cbf43, 0xf2ae49, 0xe65050, 0xa6aab0, 0xf3f4f5, 0x6cbf43, 0xe65050,
    0xeceef0, 0x5c6166, 0xe1e4e8, 0xe6e8eb, 0xdcefd3, 0xf7d6d6, 0xa37acc, 0x4cbf99, 0x399ee6, 0x6cbf43,
    0xfa8d3e, 0xfcfcfc, 0xb9bcc0,
];

/// Monokai, Wimer Hazenberg's original colours, <https://monokai.pro/>. Monokai has no light theme.
const MONOKAI: Palette = [
    0xf8f8f2, 0xa59f85, 0xae81ff, 0xa6e22e, 0xe6db74, 0xf92672, 0x75715e, 0x1e1f1c, 0xa6e22e, 0xf92672,
    0x3e3d32, 0xf8f8f2, 0x49483e, 0x3e3d32, 0x435125, 0x552834, 0x66d9ef, 0xfd971f, 0x66d9ef, 0xa6e22e,
    0xfd971f, 0x272822, 0x75715e,
];
