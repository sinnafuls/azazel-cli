//! Components turn state into [`Line`](crate::render::Line)s for a given width and theme. None of
//! them writes to the terminal, and every line they return fits the width they were given.

pub mod chips;
pub mod code;
pub mod composer;
pub mod diff;
pub mod editor;
pub mod markdown;
pub mod overlay;
pub mod pager;
pub mod roster;
pub mod select;
pub mod shimmer;
pub mod spinner;
pub mod status;
pub mod tabs;
pub mod todo;
pub mod tool_card;
pub mod vim;

use std::time::Duration;

use crate::text;

/// Compact duration: `42s`, `3m05s`, `1h02m`.
pub fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, secs % 3600 / 60)
    }
}

/// How long something took, with more precision the shorter it was: `85ms`, `1.4s`, `42s`,
/// `3m05s`.
pub fn format_duration(elapsed: Duration) -> String {
    let millis = elapsed.as_millis();
    if millis < 1000 {
        format!("{millis}ms")
    } else if millis < 10_000 {
        format!("{}.{}s", millis / 1000, millis % 1000 / 100)
    } else {
        format_elapsed(elapsed)
    }
}

/// Compact token count: `847`, `1.2k`, `3.4M`.
pub fn format_tokens(tokens: u64) -> String {
    if tokens < 1000 {
        return tokens.to_string();
    }
    // Tenths of a unit, rounded half up without overflowing at the top of the range.
    let tenths = tokens / 100 + u64::from(tokens % 100 >= 50);
    if tenths < 10_000 {
        return format!("{}.{}k", tenths / 10, tenths % 10);
    }
    let tenths = tokens / 100_000 + u64::from(tokens % 100_000 >= 50_000);
    format!("{}.{}M", tenths / 10, tenths % 10)
}

/// Untrusted text reduced to a single plain line.
pub(crate) fn one_line(s: &str) -> String {
    let clean = text::sanitize(s);
    if clean.contains('\n') {
        clean.replace('\n', " ")
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_switches_units_at_boundaries() {
        assert_eq!(format_elapsed(Duration::from_millis(900)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(42)), "42s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m00s");
        assert_eq!(format_elapsed(Duration::from_secs(185)), "3m05s");
        assert_eq!(format_elapsed(Duration::from_secs(3599)), "59m59s");
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1h00m");
        assert_eq!(format_elapsed(Duration::from_secs(3725)), "1h02m");
    }
}
