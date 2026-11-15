//! `azazel stats` and the `/stats` panel: what was spent, from the usage ledger
//! (`~/.azazel/usage`) that every answered model request is written to.
//!
//! "Fresh" tokens are input, output and cache writes; cache reads are shown beside them, since a
//! cache read is cheap and repeats the same context on every request. The cost of a request is the
//! catalogue's API price; for a request made through a subscription login it is what the tokens
//! would have cost, not what was paid, and is marked `≈`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use azazel_core::Paths;
use azazel_core::ledger::{self, Line};
use clap::ValueEnum;
use jiff::civil::Date;
use serde_json::{Value, json};

use crate::print::{EXIT_OK, EXIT_USAGE};

/// The levels of a one-line chart, lowest first.
pub const SPARK: &str = "▁▂▃▄▅▆▇█";

/// The window `--period` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Period {
    /// Today
    Day,
    /// The last 7 days, today included
    Week,
    /// This calendar month
    Month,
    /// This calendar year
    Year,
    /// Everything in the log
    All,
}

/// What a table groups requests by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum By {
    /// Provider, with its models under it
    Provider,
    /// `provider/model`
    Model,
    /// The project the session ran in
    Project,
    /// `lead`, the agent definition of a sub-agent, or the purpose of a side request
    Agent,
}

/// The days a report covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub label: String,
    /// The first local day; `None` is all time.
    pub since: Option<Date>,
}

impl Window {
    pub fn today() -> Date {
        jiff::Zoned::now().date()
    }

    pub fn days(count: u32) -> Window {
        let today = Self::today();
        let since = today
            .checked_sub(jiff::Span::new().days(i64::from(count.max(1) - 1)))
            .unwrap_or(today);
        let label = match count {
            1 => "Today".to_string(),
            n => format!("Last {n} days"),
        };
        Window {
            label,
            since: Some(since),
        }
    }

    pub fn period(period: Period) -> Window {
        let today = Self::today();
        match period {
            Period::Day => Window::days(1),
            Period::Week => Window::days(7),
            Period::Month => Window {
                label: "This month".to_string(),
                since: Some(today.first_of_month()),
            },
            Period::Year => Window {
                label: "This year".to_string(),
                since: Some(today.first_of_year()),
            },
            Period::All => Window {
                label: "All time".to_string(),
                since: None,
            },
        }
    }

    pub fn since(date: Date) -> Window {
        Window {
            label: format!("Since {date}"),
            since: Some(date),
        }
    }
}

/// What a set of requests used.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Totals {
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// API-price estimate over the requests the catalogue could price.
    pub cost: f64,
    /// The part of `cost` that went through a subscription login.
    pub login_cost: f64,
}

impl Totals {
    fn add(&mut self, line: &Line) {
        self.requests += u64::from(line.requests);
        self.input += line.input;
        self.output += line.output;
        self.cache_read += line.cache_read;
        self.cache_write += line.cache_write;
        let cost = line.cost_usd.unwrap_or(0.0);
        self.cost += cost;
        if line.login {
            self.login_cost += cost;
        }
    }

    fn merge(&mut self, other: &Totals) {
        self.requests += other.requests;
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.cost += other.cost;
        self.login_cost += other.login_cost;
    }

    /// Tokens that were not read from a cache.
    pub fn fresh(&self) -> u64 {
        self.input + self.output + self.cache_write
    }

    fn json(&self) -> Value {
        json!({
            "requests": self.requests,
            "input": self.input,
            "output": self.output,
            "cache_read": self.cache_read,
            "cache_write": self.cache_write,
            "fresh": self.fresh(),
            "cost_usd": self.cost,
            "login_cost_usd": self.login_cost,
        })
    }
}

/// Everything the ledger says about one window.
#[derive(Debug, Clone)]
pub struct Report {
    pub window: Window,
    pub total: Totals,
    /// By local day.
    pub days: BTreeMap<Date, Totals>,
    /// By local hour; kept for a window of one day only.
    pub hours: Vec<Totals>,
    /// By `(provider, model)`.
    pub models: BTreeMap<(String, String), Totals>,
    pub projects: BTreeMap<String, Totals>,
    pub agents: BTreeMap<String, Totals>,
}

impl Report {
    fn new(window: Window) -> Self {
        let hours = match window.since == Some(Window::today()) {
            true => vec![Totals::default(); 24],
            false => Vec::new(),
        };
        Report {
            window,
            total: Totals::default(),
            days: BTreeMap::new(),
            hours,
            models: BTreeMap::new(),
            projects: BTreeMap::new(),
            agents: BTreeMap::new(),
        }
    }

    fn add(&mut self, line: &Line, date: Date, hour: i8) {
        self.total.add(line);
        self.days.entry(date).or_default().add(line);
        if let Some(slot) = usize::try_from(hour).ok().and_then(|h| self.hours.get_mut(h)) {
            slot.add(line);
        }
        self.models
            .entry((line.provider.clone(), line.model.clone()))
            .or_default()
            .add(line);
        self.projects.entry(line.project.clone()).or_default().add(line);
        self.agents.entry(agent_key(line)).or_default().add(line);
    }
}

/// Who asked, as a group: `lead`, the definition of a sub-agent (`scout`, not `scout-3`), or the
/// purpose of a side request.
fn agent_key(line: &Line) -> String {
    if let Some(def) = line.def.as_deref().filter(|def| !def.is_empty()) {
        return def.to_string();
    }
    match line.agent.rsplit_once('-') {
        Some((name, number)) if !name.is_empty() && number.chars().all(|c| c.is_ascii_digit()) => {
            name.to_string()
        }
        _ => line.agent.clone(),
    }
}

/// Reads the ledger once and fills a report for each window.
pub fn collect(dir: &Path, windows: &[Window]) -> Vec<Report> {
    let mut reports: Vec<Report> = windows.iter().cloned().map(Report::new).collect();
    let earliest = windows.iter().map(|w| w.since).min().flatten();
    let all_time = windows.iter().any(|w| w.since.is_none());
    let since_month = earliest
        .filter(|_| !all_time)
        .map(|date| format!("{:04}-{:02}", date.year(), date.month()));
    let zone = jiff::tz::TimeZone::system();
    ledger::read(dir, since_month.as_deref(), |line| {
        let Ok(at) = line.at.parse::<jiff::Timestamp>() else {
            return;
        };
        let local = at.to_zoned(zone.clone());
        let date = local.date();
        for report in &mut reports {
            if report.window.since.is_none_or(|since| date >= since) {
                report.add(&line, date, local.hour());
            }
        }
    });
    reports
}

/// How the chart of a report is cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Hour,
    Day,
    Week,
    Month,
}

impl Bucket {
    pub fn name(self) -> &'static str {
        match self {
            Bucket::Hour => "hour",
            Bucket::Day => "day",
            Bucket::Week => "week",
            Bucket::Month => "month",
        }
    }

    fn start(self, date: Date) -> Date {
        match self {
            Bucket::Hour | Bucket::Day => date,
            Bucket::Week => date
                .checked_sub(jiff::Span::new().days(i64::from(date.weekday().to_monday_zero_offset())))
                .unwrap_or(date),
            Bucket::Month => date.first_of_month(),
        }
    }

    fn next(self, start: Date) -> Option<Date> {
        match self {
            Bucket::Hour | Bucket::Day => start.tomorrow().ok(),
            Bucket::Week => start.checked_add(jiff::Span::new().days(7)).ok(),
            Bucket::Month => start.checked_add(jiff::Span::new().months(1)).ok(),
        }
    }
}

/// Fresh tokens per bucket, from the first bucket with a day in the window to today, gaps
/// included, and the name of the bucket.
pub fn chart(report: &Report) -> (Bucket, Vec<(String, u64)>) {
    if !report.hours.is_empty() {
        let values = report
            .hours
            .iter()
            .enumerate()
            .map(|(hour, totals)| (format!("{hour:02}:00"), totals.fresh()))
            .collect();
        return (Bucket::Hour, values);
    }
    let today = Window::today();
    let first = report
        .window
        .since
        .or_else(|| report.days.keys().next().copied())
        .unwrap_or(today);
    let span = today.since(first).map_or(0, |span| span.get_days()).max(0);
    let bucket = match span {
        0..=62 => Bucket::Day,
        63..=400 => Bucket::Week,
        _ => Bucket::Month,
    };
    let mut sums: BTreeMap<Date, u64> = BTreeMap::new();
    for (day, totals) in &report.days {
        *sums.entry(bucket.start(*day)).or_default() += totals.fresh();
    }
    let mut values = Vec::new();
    let mut at = Some(bucket.start(first));
    while let Some(start) = at.filter(|start| *start <= today) {
        values.push((start.to_string(), sums.get(&start).copied().unwrap_or(0)));
        at = bucket.next(start);
    }
    (bucket, values)
}

/// One character per value, taller for larger ones, from the levels of `levels`.
pub fn spark(values: &[u64], levels: &str) -> String {
    let levels: Vec<char> = levels.chars().collect();
    let max = values.iter().copied().max().unwrap_or(0);
    let top = levels.len().saturating_sub(1);
    values
        .iter()
        .map(|&v| match (max, v) {
            (0, _) | (_, 0) => ' ',
            _ => levels[((v as f64 / max as f64) * top as f64).round() as usize],
        })
        .collect()
}

/// A row of a table.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub label: String,
    /// 1 for a model under its provider.
    pub depth: usize,
    pub totals: Totals,
}

fn sorted(map: impl IntoIterator<Item = (String, Totals)>, depth: usize) -> Vec<Row> {
    let mut rows: Vec<Row> = map
        .into_iter()
        .map(|(label, totals)| Row { label, depth, totals })
        .collect();
    rows.sort_by(|a, b| {
        b.totals
            .fresh()
            .cmp(&a.totals.fresh())
            .then(b.totals.requests.cmp(&a.totals.requests))
            .then(a.label.cmp(&b.label))
    });
    rows
}

/// The rows of the table for `by`, largest first.
pub fn rows(report: &Report, by: By) -> Vec<Row> {
    match by {
        By::Provider => {
            let mut providers: BTreeMap<&str, Totals> = BTreeMap::new();
            for ((provider, _), totals) in &report.models {
                providers.entry(provider).or_default().merge(totals);
            }
            let mut out = Vec::new();
            for provider in sorted(providers.into_iter().map(|(k, v)| (k.to_string(), v)), 0) {
                let models = report
                    .models
                    .iter()
                    .filter(|((p, _), _)| *p == provider.label)
                    .map(|((_, model), totals)| (model.clone(), *totals));
                let children = sorted(models, 1);
                out.push(provider);
                out.extend(children);
            }
            out
        }
        By::Model => sorted(
            report
                .models
                .iter()
                .map(|((provider, model), totals)| (format!("{provider}/{model}"), *totals)),
            0,
        ),
        By::Project => sorted(report.projects.iter().map(|(k, v)| (project_label(k), *v)), 0),
        By::Agent => sorted(report.agents.iter().map(|(k, v)| (k.clone(), *v)), 0),
    }
}

/// A project's folder name without the hash that ends it.
pub fn project_label(slug: &str) -> String {
    match slug.rsplit_once('-') {
        Some((name, hash))
            if hash.len() == 8 && hash.chars().all(|c| c.is_ascii_hexdigit()) && !name.is_empty() =>
        {
            name.to_string()
        }
        _ if slug.is_empty() => "(none)".to_string(),
        _ => slug.to_string(),
    }
}

pub fn compact(n: u64) -> String {
    match n {
        n if n >= 1_000_000_000 => format!("{:.1}G", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

/// The cost of `totals`; `approx` marks a figure that is an API price for subscription requests.
pub fn money(totals: &Totals, approx: &str) -> String {
    match totals.cost {
        cost if cost <= 0.0 => "-".to_string(),
        cost if totals.login_cost > 0.0 => format!("{approx}${cost:.2}"),
        cost => format!("${cost:.2}"),
    }
}

/// What `azazel stats` prints.
pub fn render(report: &Report, by: By) -> String {
    let t = &report.total;
    let mut out = format!(
        "{}: {} requests, {} fresh tokens (in {}, out {}, cache write {}), {} cache read, {}\n",
        report.window.label,
        t.requests,
        compact(t.fresh()),
        compact(t.input),
        compact(t.output),
        compact(t.cache_write),
        compact(t.cache_read),
        money(t, "≈"),
    );
    let (bucket, values) = chart(report);
    let counts: Vec<u64> = values.iter().map(|(_, v)| *v).collect();
    let _ = writeln!(
        out,
        "fresh tokens per {}  |{}|",
        bucket.name(),
        spark(&counts, SPARK)
    );
    let table = rows(report, by);
    if table.is_empty() {
        out.push_str("\nNo requests in this window.\n");
        return out;
    }
    let title = match by {
        By::Provider => "Provider / model",
        By::Model => "Model",
        By::Project => "Project",
        By::Agent => "Agent",
    };
    let _ = writeln!(
        out,
        "\n{title:<46} {:>8} {:>9} {:>10} {:>10}",
        "requests", "fresh", "cache read", "cost"
    );
    for row in &table {
        let label = format!("{}{}", "  ".repeat(row.depth), row.label);
        let label: String = label.chars().take(46).collect();
        let _ = writeln!(
            out,
            "{label:<46} {:>8} {:>9} {:>10} {:>10}",
            row.totals.requests,
            compact(row.totals.fresh()),
            compact(row.totals.cache_read),
            money(&row.totals, "≈"),
        );
    }
    if t.login_cost > 0.0 {
        out.push_str(
            "\n≈ API price of requests made through a subscription login: what the tokens would cost by the token, \
             not what you paid.\n",
        );
    }
    out
}

fn json(report: &Report, by: By) -> Value {
    let (bucket, values) = chart(report);
    json!({
        "window": report.window.label,
        "since": report.window.since.map(|d| d.to_string()),
        "total": report.total.json(),
        "bucket": bucket.name(),
        "buckets": values.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<serde_json::Map<_, _>>(),
        "by": format!("{by:?}").to_lowercase(),
        "groups": rows(report, by).iter().map(|row| {
            let mut group = row.totals.json();
            group["name"] = json!(row.label);
            group["depth"] = json!(row.depth);
            group
        }).collect::<Vec<_>>(),
    })
}

/// Imports the history of the session files the first time, and the ledger's directory.
fn ready(paths: &Paths) -> std::path::PathBuf {
    ledger::ensure_backfilled(&paths.home);
    ledger::dir(&paths.home)
}

/// What `/stats` shows in a session that is not in this process, and `azazel stats` prints.
pub fn text(paths: &Paths, window: Window, by: By) -> String {
    let dir = ready(paths);
    let reports = collect(&dir, &[window]);
    reports
        .first()
        .map(|report| render(report, by))
        .unwrap_or_default()
}

/// The window `/stats [days]` and `azazel stats --days` name.
pub fn parse_days(text: &str) -> Option<u32> {
    text.trim().parse::<u32>().ok().filter(|days| *days > 0)
}

pub struct Args {
    pub days: Option<u32>,
    pub period: Option<Period>,
    pub by: By,
    pub since: Option<String>,
    pub json: bool,
    pub rebuild: bool,
}

/// `azazel stats`.
pub fn run(paths: &Paths, args: &Args) -> u8 {
    let window = match (&args.since, args.days, args.period) {
        (Some(text), _, _) => match text.parse::<Date>() {
            Ok(date) => Window::since(date),
            Err(_) => {
                eprintln!("azazel: --since: `{text}` is not a date like 2026-03-01");
                return EXIT_USAGE;
            }
        },
        (None, Some(0), _) => {
            eprintln!("azazel: --days must be at least 1");
            return EXIT_USAGE;
        }
        (None, Some(days), _) => Window::days(days),
        (None, None, Some(period)) => Window::period(period),
        (None, None, None) => Window::days(30),
    };
    if args.rebuild {
        let imported = ledger::rebuild(&paths.home);
        eprintln!("imported {imported} requests from the session files");
    }
    let dir = ready(paths);
    let Some(report) = collect(&dir, &[window]).pop() else {
        return EXIT_OK;
    };
    if args.json {
        println!("{}", json(&report, args.by));
    } else {
        print!("{}", render(&report, args.by));
    }
    EXIT_OK
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn line(at: &str, provider: &str, model: &str, agent: &str, fresh: u64, login: bool) -> Line {
        Line {
            at: at.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            project: "proj-0123abcd".to_string(),
            session: "s".to_string(),
            agent: agent.to_string(),
            input: fresh,
            cache_read: 1000,
            cost_usd: Some(0.5),
            login,
            requests: 1,
            ..Line::default()
        }
    }

    fn write(dir: &Path, month: &str, lines: &[Line]) {
        std::fs::create_dir_all(dir).unwrap();
        let text: String = lines
            .iter()
            .map(|l| format!("{}\n", serde_json::to_string(l).unwrap()))
            .collect();
        std::fs::write(dir.join(format!("{month}.jsonl")), text).unwrap();
    }

    #[test]
    fn requests_add_up_by_provider_by_month_and_by_who_asked() {
        let dir = tempfile::tempdir().unwrap();
        // Noon UTC is the same calendar day in every time zone but the most extreme ones.
        write(
            dir.path(),
            "2026-03",
            &[
                line("2026-03-10T12:00:00Z", "anthropic", "fable", "lead", 100, true),
                line("2026-03-10T12:05:00Z", "anthropic", "sonnet", "scout-2", 50, true),
            ],
        );
        write(
            dir.path(),
            "2026-04",
            &[
                line("2026-04-10T12:00:00Z", "openai", "sol", "btw", 30, false),
                line("2026-04-11T12:00:00Z", "anthropic", "fable", "lead", 20, true),
            ],
        );
        let windows = [
            Window::since("2026-04-01".parse().unwrap()),
            Window {
                label: "All time".into(),
                since: None,
            },
        ];
        let reports = collect(dir.path(), &windows);

        let april = &reports[0];
        assert_eq!(april.total.requests, 2);
        assert_eq!(april.total.fresh(), 50);
        // The month file before the window is not counted.
        let by_provider = rows(april, By::Provider);
        let top: Vec<(&str, usize)> = by_provider.iter().map(|r| (r.label.as_str(), r.depth)).collect();
        assert_eq!(top, [("openai", 0), ("sol", 1), ("anthropic", 0), ("fable", 1)]);

        let all = &reports[1];
        assert_eq!(all.total.requests, 4);
        assert_eq!(all.total.fresh(), 200);
        assert_eq!(all.total.cache_read, 4000, "cache reads are not fresh tokens");
        let providers = rows(all, By::Provider);
        let anthropic = providers
            .iter()
            .find(|r| r.label == "anthropic" && r.depth == 0)
            .unwrap();
        assert_eq!((anthropic.totals.requests, anthropic.totals.fresh()), (3, 170));
        // Only the requests through a login are marked as an API price.
        assert_eq!(money(&anthropic.totals, "≈"), "≈$1.50");
        let openai = providers.iter().find(|r| r.label == "openai").unwrap();
        assert_eq!(money(&openai.totals, "≈"), "$0.50");
        // `scout-2` is a scout.
        let agents: Vec<String> = rows(all, By::Agent).into_iter().map(|r| r.label).collect();
        assert_eq!(agents, ["lead", "scout", "btw"]);
        assert_eq!(rows(all, By::Project)[0].label, "proj");
        // The chart goes from the first month to today, a bar per month.
        let (bucket, values) = chart(all);
        assert_eq!(bucket, Bucket::Week);
        assert_eq!(values.iter().map(|(_, v)| v).sum::<u64>(), 200);
    }

    #[test]
    fn the_text_reports_totals_and_a_note_for_subscription_requests() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "2026-04",
            &[line(
                "2026-04-10T12:00:00Z",
                "anthropic",
                "fable",
                "lead",
                100,
                true,
            )],
        );
        let report = collect(
            dir.path(),
            &[Window {
                label: "All time".into(),
                since: None,
            }],
        )
        .pop()
        .unwrap();
        let text = render(&report, By::Model);
        assert!(
            text.starts_with("All time: 1 requests, 100 fresh tokens"),
            "{text}"
        );
        assert!(
            text.contains("anthropic/fable") && text.contains("≈$0.50"),
            "{text}"
        );
        assert!(text.contains("subscription login"), "{text}");
    }
}
