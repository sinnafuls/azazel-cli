//! The `/stats` popup: tokens and cost from the usage log, over a tab per window, grouped by
//! provider, project or agent.

use std::sync::{Arc, Mutex, PoisonError};

use azazel_tui::{Glyph, KeyCombo, Role, Theme, text};
use crossterm::event::{KeyCode, KeyModifiers};

use super::Tui;
use super::overlay::{Action, Overlay};
use crate::stats::{self, By, Report, Window, compact, money};

const TITLE: &str = "Stats";
const GROUPS: [By; 3] = [By::Provider, By::Project, By::Agent];
/// Cells of the bar beside a row, when the box is wide enough for one.
const BAR: usize = 10;

struct State {
    tab: usize,
    by: usize,
}

/// What the open box shows: a report per tab, and which tab and grouping are chosen.
pub struct Shown {
    tabs: Vec<(String, Report)>,
    state: Mutex<State>,
}

impl Shown {
    pub(super) fn new(tabs: Vec<(String, Report)>, tab: usize) -> Self {
        Shown {
            tabs,
            state: Mutex::new(State { tab, by: 0 }),
        }
    }

    /// The box's own keys, in the order [`keys`] lists them.
    pub(super) fn apply(&self, key: usize) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let count = self.tabs.len().max(1);
        match key {
            0 | 3 => state.tab = (state.tab + count - 1) % count,
            1 | 2 => state.tab = (state.tab + 1) % count,
            _ => state.by = (state.by + 1) % GROUPS.len(),
        }
    }

    fn page(&self, width: usize, theme: &Theme) -> Vec<String> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        page(&self.tabs, state.tab, GROUPS[state.by], width, theme)
    }
}

fn keys() -> Vec<(KeyCombo, &'static str)> {
    vec![
        (KeyCombo::new(KeyCode::Left, KeyModifiers::NONE), "tab"),
        (KeyCombo::new(KeyCode::Right, KeyModifiers::NONE), ""),
        (KeyCombo::new(KeyCode::Tab, KeyModifiers::NONE), ""),
        (KeyCombo::new(KeyCode::BackTab, KeyModifiers::SHIFT), ""),
        (KeyCombo::new(KeyCode::Char('g'), KeyModifiers::NONE), "group"),
    ]
}

/// The windows of the tabs; a number of days from `/stats 90` is the first.
pub fn windows(days: Option<u32>) -> Vec<(String, Window)> {
    let mut tabs = Vec::new();
    if let Some(days) = days {
        tabs.push((format!("{days} days"), Window::days(days)));
    }
    tabs.extend([
        ("Today".to_string(), Window::days(1)),
        ("7 days".to_string(), Window::days(7)),
        ("30 days".to_string(), Window::days(30)),
        ("Month".to_string(), Window::period(stats::Period::Month)),
        ("Year".to_string(), Window::period(stats::Period::Year)),
        ("All".to_string(), Window::period(stats::Period::All)),
    ]);
    tabs
}

/// The cells of one row of the table.
struct Cells<'a> {
    label: &'a str,
    /// How full the bar is, 0 to 1; `None` leaves its place blank (the header).
    share: Option<f64>,
    fresh: String,
    cache: String,
    cost: String,
}

fn row(cells: &Cells<'_>, bar: usize, width: usize, theme: &Theme, role: Role) -> String {
    let drawn = match (bar, cells.share) {
        (0, _) => String::new(),
        (bar, None) => format!(" {}", " ".repeat(bar)),
        (bar, Some(share)) => {
            let filled = ((share.clamp(0.0, 1.0) * bar as f64).ceil() as usize).min(bar);
            format!(
                " {}{}{}{}",
                theme.fg(Role::Accent),
                theme.glyph(Glyph::ProgressFull).repeat(filled),
                theme.fg(Role::BorderMuted),
                theme.glyph(Glyph::ProgressEmpty).repeat(bar - filled),
            )
        }
    };
    let used = if bar > 0 { bar + 1 } else { 0 } + 1 + 7 + 1 + 7 + 1 + 9;
    let room = width.saturating_sub(used).max(4);
    let label = text::truncate(cells.label, room, theme.glyph(Glyph::Ellipsis));
    format!(
        "{}{}{drawn} {}{:>7} {:>7} {:>9}",
        theme.fg(role),
        text::pad_right(&label, room),
        theme.fg(role),
        cells.fresh,
        cells.cache,
        cells.cost,
    )
}

/// The rows of the box for tab `tab` grouped by `by`.
pub fn page(tabs: &[(String, Report)], tab: usize, by: By, width: usize, theme: &Theme) -> Vec<String> {
    let mut lines = Vec::new();
    let tab_row: Vec<String> = tabs
        .iter()
        .enumerate()
        .map(|(i, (name, _))| match i == tab {
            true => theme.paint(Role::Accent, &format!("[{name}]")),
            false => theme.paint(Role::Muted, &format!(" {name} ")),
        })
        .collect();
    lines.push(tab_row.join(""));
    let Some((_, report)) = tabs.get(tab) else {
        return lines;
    };
    let t = &report.total;
    lines.push(String::new());
    lines.push(format!(
        "{}{} requests{sep}{} fresh tokens{sep}{} cache read{sep}{}",
        theme.fg(Role::Text),
        t.requests,
        compact(t.fresh()),
        compact(t.cache_read),
        money(t, "≈"),
        sep = theme.glyph(Glyph::Separator),
    ));
    let (bucket, values) = stats::chart(report);
    let counts: Vec<u64> = values.iter().map(|(_, v)| *v).collect();
    let room = width.saturating_sub(2).max(1);
    // More buckets than cells: the newest ones are the ones worth seeing.
    let shown = &counts[counts.len().saturating_sub(room)..];
    lines.push(format!(
        "{}{}",
        theme.fg(Role::Accent),
        stats::spark(shown, theme.glyph(Glyph::Spark))
    ));
    lines.push(theme.paint(Role::Muted, &format!("fresh tokens per {}", bucket.name())));
    lines.push(String::new());
    let table = stats::rows(report, by);
    if table.is_empty() {
        lines.push(theme.paint(Role::Muted, "No requests in this window."));
        return lines;
    }
    let title = match by {
        By::Provider => "Provider / model",
        By::Model => "Model",
        By::Project => "Project",
        By::Agent => "Agent",
    };
    let bar = if width >= 64 { BAR } else { 0 };
    let head = Cells {
        label: title,
        share: None,
        fresh: "fresh".into(),
        cache: "cache".into(),
        cost: "cost".into(),
    };
    lines.push(row(&head, bar, width, theme, Role::Muted));
    let top = table.iter().map(|r| r.totals.fresh()).max().unwrap_or(0).max(1);
    for r in &table {
        let label = format!("{}{}", "  ".repeat(r.depth), r.label);
        let role = if r.depth == 0 { Role::Text } else { Role::Muted };
        let cells = Cells {
            label: &label,
            share: Some(r.totals.fresh() as f64 / top as f64),
            fresh: compact(r.totals.fresh()),
            cache: compact(r.totals.cache_read),
            cost: money(&r.totals, "≈"),
        };
        lines.push(row(&cells, bar, width, theme, role));
    }
    if t.login_cost > 0.0 {
        lines.push(String::new());
        lines.push(theme.paint(
            Role::Muted,
            "≈ API price of requests made through a subscription login.",
        ));
    }
    lines
}

impl Tui {
    /// `/stats [days]`: opens the box on the usage log. The history of the session files is
    /// imported first the first time, which can take a moment.
    pub(super) async fn open_stats(&mut self, args: &str) {
        let days = match args.trim() {
            "" => None,
            text => match stats::parse_days(text) {
                Some(days) => Some(days),
                None => return self.fail("usage: /stats [days]"),
            },
        };
        let Some(local) = self.backend.local() else {
            return self.other_command("stats", args);
        };
        let home = local.app.paths.home.clone();
        if azazel_core::ledger::backfilling(&home) {
            self.info("building the usage history from your sessions; /stats shows it in a moment");
            return;
        }
        let tabs = windows(days);
        let read = move || {
            azazel_core::ledger::ensure_backfilled(&home);
            let windows: Vec<Window> = tabs.iter().map(|(_, w)| w.clone()).collect();
            let reports = stats::collect(&azazel_core::ledger::dir(&home), &windows);
            tabs.into_iter()
                .map(|(name, _)| name)
                .zip(reports)
                .collect::<Vec<_>>()
        };
        let Ok(reports) = tokio::task::spawn_blocking(read).await else {
            return self.fail("could not read the usage log");
        };
        // A number of days is the first tab; without one the box opens on 30 days.
        let tab = if days.is_some() { 0 } else { 2 };
        let shown = Arc::new(Shown::new(reports, tab));
        let drawn = shown.clone();
        let rows = Box::new(move |width: usize, theme: &Theme| drawn.page(width, theme));
        self.overlay = Some(Overlay::report(TITLE, rows, Action::Stats(shown)).with_keys(keys()));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use crossterm::event::KeyEvent;

    use super::*;
    use crate::tui::overlay::Outcome;

    fn report(label: &str, line: azazel_core::ledger::Line) -> (String, Report) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(format!("{}.jsonl", &line.at[..7])),
            format!("{}\n", serde_json::to_string(&line).unwrap()),
        )
        .unwrap();
        let all = Window {
            label: label.to_string(),
            since: None,
        };
        (
            label.to_string(),
            stats::collect(dir.path(), &[all]).pop().unwrap(),
        )
    }

    fn line(provider: &str, project: &str, agent: &str, fresh: u64) -> azazel_core::ledger::Line {
        azazel_core::ledger::Line {
            at: "2026-04-10T12:00:00Z".into(),
            provider: provider.into(),
            model: "m".into(),
            project: project.into(),
            agent: agent.into(),
            input: fresh,
            requests: 1,
            ..Default::default()
        }
    }

    fn drawn(overlay: &Overlay) -> String {
        overlay
            .render(80, 30, &Theme::dark())
            .0
            .iter()
            .map(|l| text::strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A key as the UI loop handles it: the box's own keys act on what it shows.
    fn press(overlay: &mut Overlay, shown: &Shown, code: KeyCode) {
        let mods = if code == KeyCode::BackTab {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        if let Outcome::Hotkey { key, .. } = overlay.on_key(&KeyEvent::new(code, mods)) {
            shown.apply(key);
        }
    }

    #[test]
    fn arrows_and_tab_change_the_window_and_g_the_grouping() {
        let tabs = vec![
            report("Today", line("anthropic", "web-0123abcd", "lead", 100)),
            report("Year", line("openai", "cli-89abcdef", "scout", 300)),
        ];
        let shown = Arc::new(Shown::new(tabs, 0));
        let drawn_by = shown.clone();
        let rows = Box::new(move |width: usize, theme: &Theme| drawn_by.page(width, theme));
        let mut overlay = Overlay::report(TITLE, rows, Action::Stats(shown.clone())).with_keys(keys());

        let first = drawn(&overlay);
        assert!(
            first.contains("[Today]") && first.contains("anthropic"),
            "{first}"
        );
        assert!(!first.contains("openai"), "{first}");

        press(&mut overlay, &shown, KeyCode::Right);
        let second = drawn(&overlay);
        assert!(second.contains("[Year]") && second.contains("openai"), "{second}");
        // Tab goes on and wraps round to the first.
        press(&mut overlay, &shown, KeyCode::Tab);
        assert!(drawn(&overlay).contains("[Today]"));
        press(&mut overlay, &shown, KeyCode::Left);
        assert!(drawn(&overlay).contains("[Year]"));

        // Provider, then project, then agent.
        assert!(drawn(&overlay).contains("Provider / model"));
        press(&mut overlay, &shown, KeyCode::Char('g'));
        let projects = drawn(&overlay);
        assert!(
            projects.contains("Project") && projects.contains("cli"),
            "{projects}"
        );
        press(&mut overlay, &shown, KeyCode::Char('g'));
        let agents = drawn(&overlay);
        assert!(agents.contains("Agent") && agents.contains("scout"), "{agents}");
    }
}
