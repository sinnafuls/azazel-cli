//! What the status line says, from the facts of the session.
//!
//! ```text
//! ask · ⑂ main · ~/src/app · ◫ 42% [━━━╸────] · ↯ 78 tok/s · $0.42 · ⇶ 2 agents · ⚑ goal      ⬢ model · high
//! ```
//!
//! The left side is in the order of what matters most; when the terminal is too narrow its
//! segments are dropped from the end, so the mode is the last to go.

use azazel_core::PermissionMode;
use azazel_tui::components::status;
use azazel_tui::{Glyph, Line, Role, Theme};

use super::backend::Billing;
use crate::usage::NEARLY_FULL;

/// The widest the working directory is shown, as a share of the line.
const PATH_SHARE: usize = 3;
/// The working directory keeps at least this many columns before it is shortened.
const PATH_MIN: usize = 16;

pub(super) struct StatusLine<'a> {
    pub mode: PermissionMode,
    pub coordinator: bool,
    pub branch: Option<&'a str>,
    /// The working directory, with `~` for the home directory.
    pub cwd: &'a str,
    /// The share of the context window in use, in percent.
    pub context: Option<u64>,
    /// Output tokens per second of the lead.
    pub rate: Option<f64>,
    /// The lead is at work: the rate is of now, not of the turn that ended.
    pub busy: bool,
    /// What the lead has cost, when that is known.
    pub cost: Option<f64>,
    /// The cost from which it is shown as a warning; 0 never warns.
    pub warn_cost: f64,
    pub billing: &'a Billing,
    /// Agents at work beside the lead.
    pub agents: usize,
    pub goal: Option<&'a str>,
    /// The model the lead answers with; empty when there is none.
    pub model: &'a str,
    pub tier: Option<&'a str>,
    pub thinking: &'a str,
}

impl StatusLine<'_> {
    pub fn render(&self, width: usize, theme: &Theme) -> Line {
        let muted = |text: &str| theme.paint(Role::Muted, text);
        let labelled = |glyph: Glyph, text: &str| status::labelled(glyph, text, theme);
        let mode = self.mode.to_string();
        let mut left = vec![match self.mode {
            PermissionMode::Plan => theme.paint(Role::ModePlan, &labelled(Glyph::ModePlan, &mode)),
            PermissionMode::Ask => muted(&mode),
            PermissionMode::AcceptEdits => theme.paint(Role::ModeEdits, &labelled(Glyph::ModeEdits, &mode)),
            PermissionMode::Auto => theme.paint(Role::ModeAuto, &labelled(Glyph::ModeAuto, &mode)),
        }];
        if self.coordinator {
            left.push(theme.paint(Role::Accent, "coordinator"));
        }
        if let Some(branch) = self.branch {
            left.push(muted(&labelled(Glyph::Branch, branch)));
        }
        let room = (width / PATH_SHARE).max(PATH_MIN);
        left.push(muted(&status::shorten_path(self.cwd, room, theme)));
        // Where the context segment is, to give it its bar once the rest is known to fit.
        let mut context = None;
        if let Some(percent) = self.context {
            let role = status::context_role(percent).unwrap_or(Role::Muted);
            context = Some((left.len(), percent));
            left.push(theme.paint(role, &labelled(Glyph::Context, &format!("{percent}%"))));
        }
        if let Some(rate) = self.rate.and_then(|rate| status::rate(rate, theme)) {
            let role = if self.busy { Role::Accent } else { Role::Muted };
            left.push(theme.paint(role, &rate));
        }
        match self.billing {
            // Tokens of a subscription are not paid for one by one: what counts is how much
            // of its usage window is left.
            Billing::Subscription { window } => {
                if let Some((name, used)) = window {
                    let percent = (used * 100.0).round().clamp(0.0, 999.0) as u32;
                    let role = if *used >= NEARLY_FULL {
                        Role::Warning
                    } else {
                        Role::Muted
                    };
                    left.push(theme.paint(role, &labelled(Glyph::Usage, &format!("{percent}% {name}"))));
                }
            }
            Billing::Metered => {
                if let Some(cost) = self.cost {
                    let role = if self.warn_cost > 0.0 && cost >= self.warn_cost {
                        Role::Warning
                    } else {
                        Role::Muted
                    };
                    left.push(theme.paint(role, &status::cost(cost, theme)));
                }
            }
        }
        match self.agents {
            0 => {}
            1 => left.push(theme.paint(Role::Accent, &labelled(Glyph::Agents, "1 agent"))),
            n => left.push(theme.paint(Role::Accent, &labelled(Glyph::Agents, &format!("{n} agents")))),
        }
        if let Some(goal) = self.goal {
            left.push(theme.paint(Role::Accent, &labelled(Glyph::Goal, goal)));
        }

        let separator = theme.glyph(Glyph::Separator);
        let model = if self.model.is_empty() {
            "no model"
        } else {
            self.model
        };
        let tier = match self.tier {
            Some(tier) => format!("{tier}{separator}"),
            None => String::new(),
        };
        let right = format!(
            "{}{}{}",
            theme.paint(Role::Accent, &theme.glyphs().prefix(Glyph::Model)),
            muted(&format!("{model}{separator}{tier}")),
            theme.paint(Role::Thinking, self.thinking),
        );
        // The bar is an ornament: it is drawn where it pushes nothing else off the line.
        if let Some((at, percent)) = context.filter(|_| width >= status::BAR_MIN_WIDTH) {
            let plain = left[at].clone();
            left[at] = format!("{plain} {}", status::context_bar(percent, theme));
            if !status::fits(&left, &right, width, theme) {
                left[at] = plain;
            }
        }
        status::render(&left, &right, width, theme)
    }
}

/// `path` as the status line names it: `~` stands for `home`, with forward slashes below it.
pub(super) fn display_path(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.to_string_lossy().replace('\\', "/")),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use azazel_tui::{GlyphLevel, text};

    use super::*;

    const METERED: Billing = Billing::Metered;

    fn full(billing: &Billing) -> StatusLine<'_> {
        StatusLine {
            mode: PermissionMode::Plan,
            coordinator: false,
            branch: Some("main"),
            cwd: "~/src/app",
            context: Some(42),
            rate: Some(78.3),
            busy: true,
            cost: Some(0.42),
            warn_cost: 0.0,
            billing,
            agents: 2,
            goal: Some("goal active, attempt 1"),
            model: "anthropic/claude-opus-5-5",
            tier: Some("priority"),
            thinking: "high",
        }
    }

    fn shown(line: &StatusLine, width: usize, theme: &Theme) -> String {
        text::strip_ansi(&line.render(width, theme))
    }

    /// `left` and `right` with as many spaces between them as make the line `width` wide.
    fn sides(left: &str, right: &str, width: usize) -> String {
        let gap = width - text::width(left) - text::width(right);
        format!("{left}{}{right}", " ".repeat(gap))
    }

    #[test]
    fn every_fact_has_its_place_and_the_bar_needs_a_wide_terminal() {
        let theme = Theme::dark();
        let line = full(&METERED);
        assert_eq!(
            shown(&line, 160, &theme),
            sides(
                "⏸ plan · ⑂ main · ~/src/app · ◫ 42% [━━━╸────] · ↯ 78 tok/s · $0.42 · ⇶ 2 agents · \
                 ⚑ goal active, attempt 1",
                "⬢ anthropic/claude-opus-5-5 · priority · high",
                160
            )
        );
        // Under a hundred columns the percentage stands alone, and above it the bar is only
        // drawn where nothing has to leave the line for it.
        let short = StatusLine {
            agents: 0,
            goal: None,
            tier: None,
            ..full(&METERED)
        };
        assert!(shown(&short, 99, &theme).contains("◫ 42% · ↯ 78 tok/s · $0.42  "));
        assert!(shown(&short, 100, &theme).contains("◫ 42% · ↯ 78 tok/s · $0.42  "));
        assert!(shown(&short, 110, &theme).contains("◫ 42% [━━━╸────] · ↯ 78 tok/s · $0.42  "));

        let ascii = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        assert_eq!(
            shown(&line, 160, &ascii),
            sides(
                "|| plan | git: main | ~/src/app | ctx 42% [===>----] | 78 tok/s | $0.42 | 2 agents | \
                 goal active, attempt 1",
                "anthropic/claude-opus-5-5 | priority | high",
                160
            )
        );
        let nerd = shown(&line, 160, &Theme::dark().with_glyphs(GlyphLevel::Nerd));
        assert!(nerd.starts_with("\u{f04c} plan · \u{e0a0} main · "), "{nerd:?}");
    }

    #[test]
    fn segments_go_from_the_end_and_the_mode_and_the_model_go_last() {
        let theme = Theme::dark();
        let line = full(&METERED);
        // What is still on the left at each width, by what marks each segment.
        let marks = ["plan", "⑂", "~/", "◫", "↯", "$", "⇶", "⚑"];
        let mut kept = marks.len();
        let mut right_whole = true;
        for width in (20..=200).rev() {
            let row = shown(&line, width, &theme);
            assert!(text::width(&row) <= width, "width {width}: {row:?}");
            let present: Vec<bool> = marks.iter().map(|mark| row.contains(mark)).collect();
            let count = present.iter().filter(|here| **here).count();
            // Whatever is shown is a prefix of the order: nothing in the middle is skipped.
            assert!(
                present.iter().take(count).all(|here| *here),
                "width {width}: {row:?}"
            );
            assert!(
                count <= kept,
                "width {width} shows more than a wider line: {row:?}"
            );
            kept = count;
            assert!(row.starts_with("⏸ plan"), "width {width}: {row:?}");
            // The model is whole for as long as the left side still has a segment to give up.
            let whole = row.ends_with("⬢ anthropic/claude-opus-5-5 · priority · high");
            if count > 1 {
                assert!(whole, "width {width}: {row:?}");
            }
            assert!(whole || !right_whole || count == 1, "width {width}: {row:?}");
            right_whole = whole;
        }
        assert_eq!(kept, 1, "at 20 columns only the mode is left");
        assert_eq!(shown(&line, 200, &theme).matches(" · ").count(), 9);
    }

    #[test]
    fn no_line_is_wider_than_the_terminal_at_any_glyph_level() {
        let subscription = Billing::Subscription {
            window: Some(("5h".to_string(), 0.61)),
        };
        let mut long = full(&subscription);
        long.cwd = "~/projects/a-rather-long-directory/with/several/levels/below/it";
        long.coordinator = true;
        for level in GlyphLevel::ALL {
            let theme = Theme::dark().with_glyphs(level);
            for width in 0..=200 {
                for line in [&full(&METERED), &long] {
                    let row = shown(line, width, &theme);
                    assert!(text::width(&row) <= width, "{level} at {width}: {row:?}");
                    if level == GlyphLevel::Ascii {
                        assert!(row.is_ascii(), "{level} at {width}: {row:?}");
                    }
                }
            }
        }
        // A long directory keeps its end and no more than a third of the line.
        let row = shown(&long, 120, &Theme::dark());
        assert!(
            row.starts_with("⏸ plan · coordinator · ⑂ main · …/with/several/levels/below/it · ◫ 42%  "),
            "{row:?}"
        );
        let row = shown(&long, 200, &Theme::dark());
        assert!(
            row.contains(" · ~/projects/a-rather-long-directory/with/several/levels/below/it · "),
            "{row:?}"
        );
    }

    #[test]
    fn a_subscription_shows_its_usage_window_and_never_a_cost() {
        let theme = Theme::dark();
        let subscription = Billing::Subscription {
            window: Some(("5h".to_string(), 0.61)),
        };
        let line = full(&subscription);
        let row = shown(&line, 160, &theme);
        assert!(row.contains(" · ↯ 78 tok/s · ◔ 61% 5h · ⇶ 2 agents"), "{row:?}");
        assert!(!row.contains('$'), "{row:?}");
        // It is quiet until the window is nearly full.
        let painted = line.render(160, &theme);
        assert!(painted.contains(&theme.paint(Role::Muted, "◔ 61% 5h")));
        let nearly = Billing::Subscription {
            window: Some(("7d".to_string(), 0.8)),
        };
        let painted = full(&nearly).render(160, &theme);
        assert!(painted.contains(&theme.paint(Role::Warning, "◔ 80% 7d")));

        // No window is known yet: still no cost.
        let unknown = Billing::Subscription { window: None };
        let row = shown(&full(&unknown), 160, &theme);
        assert!(
            row.contains("tok/s · ⇶ 2 agents") && !row.contains('$'),
            "{row:?}"
        );
    }

    #[test]
    fn a_key_shows_the_cost_once_it_is_known_and_warns_past_the_budget() {
        let theme = Theme::dark();
        let mut line = full(&METERED);
        let painted = line.render(160, &theme);
        assert!(painted.contains(&theme.paint(Role::Muted, "$0.42")));
        line.warn_cost = 0.4;
        let painted = line.render(160, &theme);
        assert!(painted.contains(&theme.paint(Role::Warning, "$0.42")));
        line.warn_cost = 0.5;
        assert!(
            line.render(160, &theme)
                .contains(&theme.paint(Role::Muted, "$0.42"))
        );
        line.cost = Some(12.345);
        assert!(shown(&line, 160, &theme).contains(" · $12.3 · "));
        line.cost = None;
        assert!(!shown(&line, 160, &theme).contains('$'));
    }

    #[test]
    fn the_rate_is_bright_while_the_lead_works_and_quiet_afterwards() {
        let theme = Theme::dark();
        let mut line = full(&METERED);
        assert!(
            line.render(160, &theme)
                .contains(&theme.paint(Role::Accent, "↯ 78 tok/s"))
        );
        line.busy = false;
        assert!(
            line.render(160, &theme)
                .contains(&theme.paint(Role::Muted, "↯ 78 tok/s"))
        );
        line.rate = Some(0.4);
        assert!(!shown(&line, 160, &theme).contains("tok/s"));
        line.rate = None;
        assert!(!shown(&line, 160, &theme).contains("tok/s"));
    }

    #[test]
    fn modes_and_missing_facts() {
        let theme = Theme::dark();
        let bare = StatusLine {
            mode: PermissionMode::Ask,
            coordinator: true,
            branch: None,
            cwd: "~",
            context: None,
            rate: None,
            busy: false,
            cost: None,
            warn_cost: 0.0,
            billing: &METERED,
            agents: 1,
            goal: None,
            model: "",
            tier: None,
            thinking: "off",
        };
        assert_eq!(
            shown(&bare, 60, &theme),
            sides("ask · coordinator · ~ · ⇶ 1 agent", "⬢ no model · off", 60)
        );
        let mode = |mode| {
            shown(
                &StatusLine {
                    mode,
                    agents: 0,
                    coordinator: false,
                    ..full(&METERED)
                },
                30,
                &theme,
            )
        };
        assert!(mode(PermissionMode::AcceptEdits).starts_with("⏵ accept-edits"));
        assert!(mode(PermissionMode::Auto).starts_with("⏵⏵ auto"));
    }

    #[test]
    fn the_home_directory_is_a_tilde() {
        let home = std::path::Path::new("/home/ann");
        let at = |path: &str| display_path(std::path::Path::new(path), Some(home));
        assert_eq!(at("/home/ann"), "~");
        assert_eq!(at("/home/ann/src/app"), "~/src/app");
        assert_eq!(
            at("/srv/app"),
            std::path::Path::new("/srv/app").display().to_string()
        );
        assert_eq!(display_path(home, None), home.display().to_string());
    }
}
