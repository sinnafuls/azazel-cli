//! `azazel stats`: what your sessions cost and did, from the session indexes of every project.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use azazel_ai::Usage;
use azazel_core::session::SessionSummary;
use azazel_core::{Paths, SessionStore};
use serde_json::json;

use crate::print::{EXIT_OK, EXIT_USAGE};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Day {
    pub sessions: usize,
    pub prompts: usize,
    pub tokens: u64,
    pub cost: f64,
}

#[derive(Debug, Default)]
pub struct Stats {
    pub days: BTreeMap<String, Day>,
    pub models: BTreeMap<String, Usage>,
    pub projects: BTreeMap<String, Day>,
    pub tools: BTreeMap<String, u32>,
    /// The session with the most messages: `(label, messages)`.
    pub longest: Option<(String, usize)>,
}

fn tokens(usage: &Usage) -> u64 {
    usage.input + usage.output + usage.cache_read + usage.cache_write
}

/// The local date of a stored timestamp, `None` when it does not parse.
fn local_date(timestamp: &str) -> Option<jiff::civil::Date> {
    let instant = timestamp.parse::<jiff::Timestamp>().ok()?;
    Some(instant.to_zoned(jiff::tz::TimeZone::system()).date())
}

/// Adds up the sessions last used on or after `since`, each under `(project, summary)`.
pub fn collect(sessions: &[(String, SessionSummary)], since: jiff::civil::Date) -> Stats {
    let mut stats = Stats::default();
    for (project, s) in sessions {
        let Some(date) = local_date(&s.updated) else {
            continue;
        };
        if date < since {
            continue;
        }
        let cost = s.usage.cost_usd.unwrap_or(0.0);
        for row in [
            stats.days.entry(date.to_string()).or_default(),
            stats.projects.entry(project.clone()).or_default(),
        ] {
            row.sessions += 1;
            row.prompts += s.prompts;
            row.tokens += tokens(&s.usage);
            row.cost += cost;
        }
        for (model, usage) in &s.models {
            stats.models.entry(model.clone()).or_default().add(usage);
        }
        for (tool, calls) in &s.tools {
            *stats.tools.entry(tool.clone()).or_default() += calls;
        }
        if stats.longest.as_ref().is_none_or(|(_, most)| s.messages > *most) {
            stats.longest = Some((crate::sessions::label(s), s.messages));
        }
    }
    stats
}

/// One character per day from `since` to `today`, taller for costlier days.
pub fn spark(days: &BTreeMap<String, Day>, since: jiff::civil::Date, today: jiff::civil::Date) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let mut values = Vec::new();
    let mut day = since;
    while day <= today {
        values.push(days.get(&day.to_string()).map_or(0.0, |d| d.cost));
        match day.tomorrow() {
            Ok(next) => day = next,
            Err(_) => break,
        }
    }
    let max = values.iter().copied().fold(0.0_f64, f64::max);
    values
        .iter()
        .map(|&v| {
            if max <= 0.0 || v <= 0.0 {
                ' '
            } else {
                BARS[(((v / max) * 7.0).round() as usize).min(7)]
            }
        })
        .collect()
}

fn compact(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

pub fn render(stats: &Stats, since: jiff::civil::Date, today: jiff::civil::Date) -> String {
    let total = stats.days.values().fold(Day::default(), |mut sum, d| {
        sum.sessions += d.sessions;
        sum.prompts += d.prompts;
        sum.tokens += d.tokens;
        sum.cost += d.cost;
        sum
    });
    let mut out = format!(
        "Since {since}: {} sessions, {} prompts, {} tokens, ${:.2}\ncost per day  |{}|\n",
        total.sessions,
        total.prompts,
        compact(total.tokens),
        total.cost,
        spark(&stats.days, since, today)
    );
    let mut models: Vec<(&String, &Usage)> = stats.models.iter().collect();
    models.sort_by_key(|(_, usage)| std::cmp::Reverse(tokens(usage)));
    if !models.is_empty() {
        out.push_str("\nModels\n");
        for (model, usage) in models.iter().take(10) {
            let cost = usage
                .cost_usd
                .map(|c| format!("${c:.2}"))
                .unwrap_or_else(|| "-".to_string());
            let _ = writeln!(
                out,
                "  {model:<44} {:>8} tokens {cost:>9}",
                compact(tokens(usage))
            );
        }
    }
    let mut projects: Vec<(&String, &Day)> = stats.projects.iter().collect();
    projects.sort_by(|a, b| {
        b.1.cost
            .total_cmp(&a.1.cost)
            .then(b.1.sessions.cmp(&a.1.sessions))
    });
    if !projects.is_empty() {
        out.push_str("\nProjects\n");
        for (project, day) in projects.iter().take(10) {
            let _ = writeln!(
                out,
                "  {project:<44} {:>4} sessions ${:.2}",
                day.sessions, day.cost
            );
        }
    }
    let mut tools: Vec<(&String, &u32)> = stats.tools.iter().collect();
    tools.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    if !tools.is_empty() {
        out.push_str("\nTools\n");
        for (tool, calls) in tools.iter().take(10) {
            let _ = writeln!(out, "  {tool:<24} {calls:>6}");
        }
    }
    if let Some((label, messages)) = &stats.longest {
        let _ = writeln!(out, "\nLongest session: {label} ({messages} messages)");
    }
    out
}

fn json(stats: &Stats) -> serde_json::Value {
    let day = |d: &Day| json!({ "sessions": d.sessions, "prompts": d.prompts, "tokens": d.tokens, "cost_usd": d.cost });
    json!({
        "days": stats.days.iter().map(|(k, d)| (k.clone(), day(d))).collect::<serde_json::Map<_, _>>(),
        "projects": stats.projects.iter().map(|(k, d)| (k.clone(), day(d))).collect::<serde_json::Map<_, _>>(),
        "models": stats.models,
        "tools": stats.tools,
        "longest": stats.longest.as_ref().map(|(label, messages)| json!({ "session": label, "messages": messages })),
    })
}

/// Every session of every project, with the project's folder name.
fn all_sessions(paths: &Paths) -> Vec<(String, SessionSummary)> {
    let mut out = Vec::new();
    for project in std::fs::read_dir(paths.home.join("projects"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = project.file_name().to_string_lossy().into_owned();
        for summary in SessionStore::list(&project.path().join("sessions")) {
            out.push((name.clone(), summary));
        }
    }
    out
}

/// The first of the last `days` days, today included, and today.
fn span(days: u32) -> (jiff::civil::Date, jiff::civil::Date) {
    let today = jiff::Zoned::now().date();
    let since = today
        .checked_sub(jiff::Span::new().days(i64::from(days.max(1) - 1)))
        .unwrap_or(today);
    (since, today)
}

/// What `azazel stats` prints for the last `days` days; `/stats` shows the same.
pub fn text(paths: &Paths, days: u32) -> String {
    let (since, today) = span(days);
    render(&collect(&all_sessions(paths), since), since, today)
}

/// `azazel stats [--days N] [--json]`.
pub fn run(paths: &Paths, days: u32, as_json: bool) -> u8 {
    if days == 0 {
        eprintln!("azazel: --days must be at least 1");
        return EXIT_USAGE;
    }
    if as_json {
        let (since, _) = span(days);
        println!("{}", json(&collect(&all_sessions(paths), since)));
    } else {
        print!("{}", text(paths, days));
    }
    EXIT_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(updated: &str, messages: usize, cost: f64, model: &str, tool: &str) -> SessionSummary {
        let usage = Usage {
            input: 100,
            output: 50,
            cost_usd: Some(cost),
            ..Usage::default()
        };
        SessionSummary {
            path: "x.jsonl".into(),
            id: format!("id-{updated}"),
            timestamp: updated.into(),
            updated: updated.into(),
            name: Some(format!("work {messages}")),
            first_prompt: None,
            messages,
            pinned: false,
            usage,
            prompts: messages / 2,
            models: BTreeMap::from([(model.to_string(), usage)]),
            tools: BTreeMap::from([(tool.to_string(), 3)]),
            cwd: String::new(),
            archived: false,
        }
    }

    #[test]
    fn days_models_projects_and_tools_add_up_within_the_window() {
        let today = jiff::Zoned::now().date();
        let noon = |date: jiff::civil::Date| {
            date.at(12, 0, 0, 0)
                .to_zoned(jiff::tz::TimeZone::system())
                .unwrap()
                .timestamp()
                .to_string()
        };
        let yesterday = today.yesterday().unwrap();
        let long_ago = today.checked_sub(jiff::Span::new().days(40)).unwrap();
        let sessions = vec![
            (
                "a".to_string(),
                summary(&noon(today), 10, 1.0, "anthropic/fable", "read"),
            ),
            (
                "a".to_string(),
                summary(&noon(yesterday), 4, 3.0, "openai/sol", "read"),
            ),
            (
                "b".to_string(),
                summary(&noon(today), 2, 0.5, "anthropic/fable", "shell"),
            ),
            ("b".to_string(), summary(&noon(long_ago), 99, 9.0, "x/y", "edit")),
        ];
        let since = today.checked_sub(jiff::Span::new().days(29)).unwrap();
        let stats = collect(&sessions, since);
        assert_eq!(stats.days.len(), 2);
        assert_eq!(stats.days[&today.to_string()].sessions, 2);
        assert_eq!(stats.days[&today.to_string()].cost, 1.5);
        assert_eq!(stats.projects["a"].sessions, 2);
        assert_eq!(stats.models["anthropic/fable"].input, 200);
        assert!(!stats.models.contains_key("x/y"));
        assert_eq!(stats.tools["read"], 6);
        assert_eq!(stats.longest, Some(("work 10".to_string(), 10)));

        let line = spark(&stats.days, since, today);
        assert_eq!(line.chars().count(), 30);
        assert!(line.ends_with("█▅"), "{line:?}");
        let text = render(&stats, since, today);
        assert!(text.contains("3 sessions, 8 prompts"), "{text}");
        assert!(text.contains("$4.50"), "{text}");
    }
}
