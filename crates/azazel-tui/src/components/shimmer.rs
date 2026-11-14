//! The working indicator: a band of light that passes over a short text.
//!
//! Each cell of the text has a brightness between 0 and 1 that depends on how far the band is;
//! the brightness falls into one of three tiers, and cells of one tier that follow each other
//! share one colour sequence, so a frame costs a handful of escapes however long the text is.
//!
//! - `shimmer`: a cosine band that enters ten cells before the text, moves one cell per tick
//!   and leaves at its end, over and over.
//! - `kitt`: one bright cell that runs to the end of the text and back, with a fading trail.
//! - `static`: the text as it is.

use std::time::Duration;

use crate::text::{self, Segment};
use crate::theme::{Color, RESET, Role, Theme};

/// Time between two frames of the indicator. A redraw timer this fast is only needed while the
/// indicator is visible and moving.
pub const INTERVAL: Duration = Duration::from_millis(33);

/// Cells on each side of the band's centre that it lights.
const BAND: f64 = 6.0;
/// Cells the band travels before it reaches the text.
const LEAD_IN: u64 = 10;
/// Cells behind the head of `kitt` that still glow.
const TRAIL: u64 = 7;
/// Brightness from which a cell is drawn in the high tier.
const HIGH: f64 = 0.65;
/// Brightness from which a cell is drawn in the middle tier.
const MID: f64 = 0.22;
/// Ticks a phrase stays when nothing moves over it.
const STATIC_DWELL: u64 = 90;

const BOLD: &str = "\x1b[1m";

/// How the indicator moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Shimmer,
    Kitt,
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    Low,
    Mid,
    High,
}

/// The tick reached after `elapsed` time; ticks advance once per [`INTERVAL`].
pub fn tick_at(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis() / INTERVAL.as_millis()).unwrap_or(u64::MAX)
}

/// Ticks one pass over a text `cells` wide takes.
pub fn period(cells: usize, mode: Mode) -> u64 {
    let cells = u64::try_from(cells).unwrap_or(u64::MAX / 4);
    match mode {
        // In from the lead-in, across the text, and out until the band has left it.
        Mode::Shimmer => LEAD_IN + cells + BAND as u64,
        // There and back again.
        Mode::Kitt => 2 * cells.saturating_sub(1).max(1),
        Mode::Static => STATIC_DWELL,
    }
}

/// The phrase to show at tick `phase` and the tick within its pass. Every phrase gets one
/// pass, then the next one is shown; `None` when there are no phrases.
pub fn phrase(phrases: &[String], phase: u64, mode: Mode) -> Option<(&str, u64)> {
    let periods: Vec<u64> = phrases
        .iter()
        .map(|phrase| period(text::width(phrase), mode))
        .collect();
    let total: u64 = periods.iter().sum();
    let mut left = phase.checked_rem(total)?;
    for (phrase, period) in phrases.iter().zip(periods) {
        if left < period {
            return Some((phrase, left));
        }
        left -= period;
    }
    None
}

/// How bright the cell at column `cell` of a text `cells` wide is at tick `phase`.
fn brightness(cell: u64, cells: u64, phase: u64, mode: Mode) -> f64 {
    match mode {
        Mode::Static => 0.0,
        Mode::Shimmer => {
            let centre = (phase % period(cells as usize, mode)) as f64 - LEAD_IN as f64;
            let distance = (cell as f64 - centre).abs();
            if distance >= BAND {
                0.0
            } else {
                0.5 * (1.0 + (std::f64::consts::PI * distance / BAND).cos())
            }
        }
        Mode::Kitt => {
            let span = cells.saturating_sub(1).max(1);
            let step = phase % (2 * span);
            // At either end the trail still points where the head came from.
            let (head, forward) = if (1..=span).contains(&step) {
                (step, true)
            } else {
                ((2 * span - step) % (2 * span), false)
            };
            // The trail is on the side the head came from.
            let behind = if forward {
                head.checked_sub(cell)
            } else {
                cell.checked_sub(head)
            };
            match behind {
                Some(0) => 1.0,
                Some(distance) if distance <= TRAIL => (1.0 - distance as f64 / TRAIL as f64).powi(2),
                _ => 0.0,
            }
        }
    }
}

fn tier(brightness: f64) -> Tier {
    if brightness >= HIGH {
        Tier::High
    } else if brightness >= MID {
        Tier::Mid
    } else {
        Tier::Low
    }
}

/// `text` as the indicator shows it at tick `phase`: muted where the band is not, in the text
/// colour at its edge and bold in the accent colour at its centre. `accent` stands in for the
/// theme's accent, for a session with a colour of its own. Styling already in `text` is
/// dropped; the result is as wide as `text`.
pub fn sweep(text: &str, phase: u64, mode: Mode, accent: Option<Color>, theme: &Theme) -> String {
    if mode == Mode::Static {
        return theme.paint(Role::Muted, &text::strip_ansi(text));
    }
    let cells = u64::try_from(text::width(text)).unwrap_or(u64::MAX / 4);
    let style = |tier: Tier| match tier {
        Tier::Low => theme.fg(Role::Muted),
        Tier::Mid => theme.fg(Role::Text),
        Tier::High => format!(
            "{BOLD}{}",
            match accent {
                Some(color) => theme.fg_color(color),
                None => theme.fg(Role::Accent),
            }
        ),
    };
    let mut out = String::with_capacity(text.len() + 64);
    let mut open: Option<Tier> = None;
    let mut column = 0u64;
    for segment in text::segments(text) {
        let Segment::Grapheme(grapheme) = segment else {
            continue;
        };
        let now = tier(brightness(column, cells, phase, mode));
        if open != Some(now) {
            if open.is_some() {
                out.push_str(RESET);
            }
            out.push_str(&style(now));
            open = Some(now);
        }
        out.push_str(grapheme);
        column += u64::try_from(text::grapheme_width(grapheme)).unwrap_or(0);
    }
    if open.is_some() {
        out.push_str(RESET);
    }
    out
}

/// A colour of its own for the session called `name`: always the same for one name, as bright
/// and as saturated as every other, and never in the yellow-green or the cyan that are hard to
/// read on a dark background.
pub fn session_accent(name: &str) -> Color {
    // djb2.
    let hash = name.bytes().fold(5381u32, |hash, byte| {
        hash.wrapping_mul(33).wrapping_add(u32::from(byte))
    });
    // The hues from 94 to 138 and from 158 to 200 degrees are left out.
    let hue = match hash % 274 {
        step @ 0..94 => step,
        step @ 94..114 => 138 + (step - 94),
        step => 200 + (step - 114),
    };
    let (r, g, b) = oklch_to_srgb(0.75, 0.14, f64::from(hue));
    Color::Rgb(r, g, b)
}

/// The sRGB colour of an OKLCH one. A colour the screen cannot show loses chroma, not
/// lightness or hue, until it can.
fn oklch_to_srgb(lightness: f64, chroma: f64, hue_degrees: f64) -> (u8, u8, u8) {
    let hue = hue_degrees.to_radians();
    let linear = |chroma: f64| {
        let (a, b) = (chroma * hue.cos(), chroma * hue.sin());
        let l = (lightness + 0.396_337_777_4 * a + 0.215_803_757_3 * b).powi(3);
        let m = (lightness - 0.105_561_345_8 * a - 0.063_854_172_8 * b).powi(3);
        let s = (lightness - 0.089_484_177_5 * a - 1.291_485_548 * b).powi(3);
        [
            4.076_741_662_1 * l - 3.307_711_591_3 * m + 0.230_969_929_2 * s,
            -1.268_438_004_6 * l + 2.609_757_401_1 * m - 0.341_319_396_5 * s,
            -0.004_196_086_3 * l - 0.703_418_614_7 * m + 1.707_614_701 * s,
        ]
    };
    let shown = |rgb: &[f64; 3]| rgb.iter().all(|channel| (0.0..=1.0).contains(channel));
    let mut rgb = linear(chroma);
    if !shown(&rgb) {
        let (mut low, mut high) = (0.0, chroma);
        for _ in 0..12 {
            let middle = (low + high) / 2.0;
            if shown(&linear(middle)) {
                low = middle;
            } else {
                high = middle;
            }
        }
        rgb = linear(low);
    }
    let encode = |channel: f64| {
        let channel = channel.clamp(0.0, 1.0);
        let gamma = if channel <= 0.003_130_8 {
            12.92 * channel
        } else {
            1.055 * channel.powf(1.0 / 2.4) - 0.055
        };
        (gamma * 255.0).round() as u8
    };
    (encode(rgb[0]), encode(rgb[1]), encode(rgb[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One letter per cell: `-` low, `+` middle, `#` high.
    fn tiers(cells: u64, phase: u64, mode: Mode) -> String {
        (0..cells)
            .map(|cell| match tier(brightness(cell, cells, phase, mode)) {
                Tier::Low => '-',
                Tier::Mid => '+',
                Tier::High => '#',
            })
            .collect()
    }

    #[test]
    fn the_shimmer_band_enters_crosses_and_leaves_one_cell_per_tick() {
        let at = |phase| tiers(20, phase, Mode::Shimmer);
        // Ten cells of lead-in: the band is still outside the text.
        assert_eq!(at(0), "-".repeat(20));
        assert_eq!(at(5), "-".repeat(20));
        // Its edge arrives first, then the bright centre.
        assert_eq!(at(6), "+-------------------");
        assert_eq!(at(8), "#++-----------------");
        assert_eq!(at(20), "------++#####++-----");
        assert_eq!(at(21), "-------++#####++----");
        assert_eq!(at(33), "-------------------+");
        // After 10 + 20 + 6 ticks it has left and the pass starts again.
        assert_eq!(at(35), "-".repeat(20));
        assert_eq!(period(20, Mode::Shimmer), 36);
        assert_eq!(at(36 + 20), at(20));
    }

    #[test]
    fn kitt_runs_to_the_end_and_back_with_a_trail_behind_it() {
        let at = |phase| tiers(12, phase, Mode::Kitt);
        assert_eq!(at(1), "##----------");
        assert_eq!(at(6), "---++##-----");
        assert_eq!(at(11), "--------++##");
        // On the way back the trail is on the other side.
        assert_eq!(at(12), "----------##");
        assert_eq!(at(16), "------##++--");
        assert_eq!(at(21), "-##++-------");
        assert_eq!(at(22), "##++--------");
        assert_eq!(period(12, Mode::Kitt), 22);
        assert_eq!(at(22 + 6), at(6));
        // The shortest texts still have a pass to make.
        assert_eq!(period(1, Mode::Kitt), 2);
        assert_eq!(period(0, Mode::Kitt), 2);
        assert_eq!(tiers(1, 0, Mode::Kitt), "#");
    }

    #[test]
    fn equal_tiers_share_one_colour_sequence() {
        let theme = Theme::dark();
        let line = sweep("running the tests now", 21, Mode::Shimmer, None, &theme);
        assert_eq!(text::strip_ansi(&line), "running the tests now");
        assert_eq!(tiers(21, 21, Mode::Shimmer), "-------++#####++-----");
        let (muted, plain, accent) = (
            theme.fg(Role::Muted),
            theme.fg(Role::Text),
            theme.fg(Role::Accent),
        );
        assert_eq!(
            line,
            format!(
                "{muted}running{RESET}{plain} t{RESET}{BOLD}{accent}he te{RESET}{plain}st{RESET}{muted}s now{RESET}"
            )
        );
        // Away from the text the whole of it is one run.
        assert_eq!(
            sweep("thinking", 0, Mode::Shimmer, None, &theme),
            format!("{muted}thinking{RESET}")
        );
    }

    #[test]
    fn a_session_colour_replaces_the_accent_of_the_bright_tier_only() {
        let theme = Theme::dark();
        let own = Color::Rgb(1, 2, 3);
        let line = sweep("thinking", 14, Mode::Shimmer, Some(own), &theme);
        assert!(
            line.contains(&format!("{BOLD}{}", theme.fg_color(own))),
            "{line:?}"
        );
        assert!(!line.contains(&theme.fg(Role::Accent)));
        assert!(line.contains(&theme.fg(Role::Text)), "the edge keeps its colour");
    }

    #[test]
    fn static_text_is_muted_and_wide_cells_count_as_they_are_shown() {
        let theme = Theme::dark();
        assert_eq!(
            sweep("working", 7, Mode::Static, None, &theme),
            theme.paint(Role::Muted, "working")
        );
        for phase in 0..60 {
            for mode in [Mode::Shimmer, Mode::Kitt, Mode::Static] {
                let line = sweep("日本語 \x1b[31mtext\x1b[0m", phase, mode, None, &theme);
                assert_eq!(text::strip_ansi(&line), "日本語 text", "{mode:?} at {phase}");
                assert_eq!(text::width(&line), 11);
            }
        }
        assert_eq!(sweep("", 3, Mode::Shimmer, None, &theme), "");
    }

    /// The hue of an sRGB colour in degrees, as OKLCH measures it, with its lightness.
    fn oklch_of(color: Color) -> (f64, f64) {
        let Color::Rgb(r, g, b) = color else {
            panic!("{color:?} is not an RGB colour");
        };
        let linear = |channel: u8| {
            let channel = f64::from(channel) / 255.0;
            if channel <= 0.040_45 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        let (r, g, b) = (linear(r), linear(g), linear(b));
        let l = (0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b).cbrt();
        let m = (0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b).cbrt();
        let s = (0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b).cbrt();
        let lightness = 0.210_454_255_3 * l + 0.793_617_785 * m - 0.004_072_046_8 * s;
        let a = 1.977_998_495_1 * l - 2.428_592_205 * m + 0.450_593_709_9 * s;
        let b = 0.025_904_037_1 * l + 0.782_771_766_2 * m - 0.808_675_766 * s;
        (b.atan2(a).to_degrees().rem_euclid(360.0), lightness)
    }

    #[test]
    fn a_session_keeps_its_colour_and_no_colour_is_hard_to_read() {
        assert_eq!(session_accent("fix-login"), session_accent("fix-login"));
        assert_ne!(session_accent("fix-login"), session_accent("fix-logout"));
        let mut seen = std::collections::HashSet::new();
        for index in 0..400 {
            let color = session_accent(&format!("session-{index}"));
            let (hue, lightness) = oklch_of(color);
            // Rounding to eight bits moves a hue by a degree or two at most.
            assert!(
                !(96.0..136.0).contains(&hue) && !(160.0..198.0).contains(&hue),
                "session-{index}: hue {hue:.0} of {color:?}"
            );
            assert!((lightness - 0.75).abs() < 0.01, "session-{index}: {lightness}");
            seen.insert(color);
        }
        assert!(seen.len() > 150, "only {} colours for 400 names", seen.len());
    }

    #[test]
    fn colours_outside_the_screen_lose_chroma_only() {
        // A chroma no screen has at this hue: the result is still a blue of that lightness.
        let (r, g, b) = oklch_to_srgb(0.75, 0.4, 260.0);
        let (hue, lightness) = oklch_of(Color::Rgb(r, g, b));
        assert!((hue - 260.0).abs() < 3.0, "hue {hue}");
        assert!((lightness - 0.75).abs() < 0.01, "lightness {lightness}");
        // Grey is grey.
        assert_eq!(oklch_to_srgb(1.0, 0.0, 0.0), (255, 255, 255));
        assert_eq!(oklch_to_srgb(0.0, 0.0, 0.0), (0, 0, 0));
    }
}
