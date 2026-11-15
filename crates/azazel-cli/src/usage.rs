//! `/usage` and `azazel usage`: what each subscription has left of its usage windows.
//!
//! The provider is asked (see [`azazel_ai::providers::usage`]) at most every five minutes per
//! account when somebody looks, and in the background while a session is open; the answers are
//! kept in `cache/limits.json`. When it cannot be asked, its last answer or what the headers of
//! this session's responses said about the account is shown instead.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use azazel_ai::credentials::{account_key, now_ms};
use azazel_ai::limits::time_left;
use azazel_ai::providers::http::http_client;
use azazel_ai::providers::resets::{self, Desk};
use azazel_ai::providers::usage::{self, Spend, UsageReport};
use azazel_ai::{Account, Credentials, LimitBlock, ProviderConfig, Window};
use azazel_core::{Command, CommandHandler, CommandOutcome, ExtCtx, ExtError, Extension, Registrar};
use azazel_tui::components::overlay::Frame;
use azazel_tui::{Glyph, Line, Role, Theme, modal, text};
use serde_json::json;

use crate::accounts::clock;
use crate::print::{EXIT_FAILED, EXIT_OK};

/// An answer of a provider is good for this long when somebody looks.
pub const FRESH_SECS: u64 = 5 * 60;
/// The background does not ask more often than this, whatever `usage.poll_secs` says.
const POLL_MIN_SECS: u64 = 60;
/// An answer kept by an earlier run says something about now only when it is younger than
/// this.
const KEPT_SECS: u64 = 60 * 60;
/// The cells of a window's bar, and the fewest worth drawing.
const BAR: usize = 20;
const BAR_MIN: usize = 6;
/// The widest the column of window names gets.
const NAMES: usize = 18;
/// What the rows under an account start with.
const INDENT: &str = "     ";
/// The share of a window from which the status line and the bars warn about it.
pub const NEARLY_FULL: f32 = 0.8;
const NO_LOGIN: &str = "no subscription is signed in: usage is reported for Claude and ChatGPT logins \
                        (azazel login anthropic, azazel login openai-codex)";

/// Where the usage of the stored logins is asked and kept.
#[derive(Clone)]
pub struct Source {
    /// The providers that take a key or have a login.
    pub providers: Vec<ProviderConfig>,
    pub credentials: Arc<Credentials>,
    /// `cache/limits.json`.
    pub cache_file: PathBuf,
    /// Where banked resets are read and spent, and the journal of the ones that were.
    pub desk: Desk,
}

impl Source {
    pub fn of(
        config: &azazel_core::Config,
        credentials: Arc<Credentials>,
        paths: &azazel_core::Paths,
    ) -> Result<Source, String> {
        Ok(Source {
            providers: crate::login::key_providers(config)?,
            credentials,
            cache_file: cache_file(paths),
            desk: Desk::new(crate::resets::journal_dir(paths)),
        })
    }
}

/// What is known about one account.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub provider: String,
    pub account: Account,
    /// Requests of this run to the provider are made with this account.
    pub in_use: bool,
    /// What the provider answered: now, or the last time it could be asked.
    pub report: Option<UsageReport>,
    /// What response headers said, when the provider has never answered.
    pub seen: Vec<Window>,
    /// Why the provider could not be asked.
    pub problem: Option<String>,
    /// Until when the account is left alone, and for which models.
    pub blocks: Vec<LimitBlock>,
}

impl Entry {
    /// The windows to show: the provider's answer, else what headers said.
    fn windows(&self) -> &[Window] {
        match &self.report {
            Some(report) => &report.windows,
            None => &self.seen,
        }
    }
}

/// Which accounts the providers are asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// None: only what is already known is shown.
    Known,
    /// Those without an answer from the last five minutes.
    Stale,
    /// All of them.
    All,
}

/// The usage of every stored login of the providers that report it, in provider order.
pub async fn collect(source: &Source, ask: Ask) -> Vec<Entry> {
    let client = http_client();
    let credentials = &source.credentials;
    let limits = credentials.limits();
    let cache = usage::cached(&source.cache_file);
    let now = now_ms() / 1000;
    let mut entries = Vec::new();
    for provider in source
        .providers
        .iter()
        .filter(|provider| usage::supported(provider))
    {
        // A key from the environment or the configuration answers whatever is stored here.
        let selected = credentials.selected(&provider.id).filter(|_| {
            credentials
                .source(provider)
                .is_some_and(|from| from.starts_with("auth.json"))
        });
        for account in credentials.accounts(&provider.id) {
            if !account.login {
                continue;
            }
            let label = account.label.as_str();
            let kept = cache.get(&account_key(&provider.id, label)).cloned();
            let fresh = kept
                .as_ref()
                .is_some_and(|report| now.saturating_sub(report.fetched_at) < FRESH_SECS);
            let (report, problem) = match ask {
                Ask::Known => (kept, None),
                Ask::Stale if fresh => {
                    // A run that has heard nothing about the account yet starts from this.
                    if let Some(report) = kept.as_ref()
                        && limits.windows(&provider.id, label).is_empty()
                    {
                        limits.record_report(&provider.id, label, &report.windows, report.fetched_at);
                    }
                    (kept, None)
                }
                Ask::Stale | Ask::All => match fetch(source, &client, provider, label, true).await {
                    Ok(report) => (Some(report), None),
                    Err(why) => (kept, Some(why)),
                },
            };
            // The resets an account holds take a question of their own, asked when somebody
            // looks and the last answer is older than five minutes.
            let report = match report {
                Some(mut report)
                    if ask != Ask::Known && problem.is_none() && asks_resets(provider, &report, now) =>
                {
                    match source.desk.offers(provider, credentials, label, true).await {
                        Ok(offers) => report.resets = offers.shown,
                        Err(why) => {
                            tracing::debug!("resets of {} ({label}) were not read: {why}", provider.id);
                            // What the account holds is not known; what the usage report
                            // itself counted stays.
                            if resets::asked_apart(provider) {
                                report.resets.clear();
                            }
                        }
                    }
                    // Asked, with or without an answer: not again for five minutes.
                    report.resets_asked_at = Some(now);
                    usage::keep(&source.cache_file, &report);
                    Some(report)
                }
                report => report,
            };
            let seen = match report {
                Some(_) => Vec::new(),
                None => limits.windows(&provider.id, label),
            };
            entries.push(Entry {
                provider: provider.id.clone(),
                in_use: selected.as_deref() == Some(label),
                blocks: limits.blocks(&provider.id, label),
                account,
                report,
                seen,
                problem,
            });
        }
    }
    entries
}

/// Asks `provider` for the usage of the login stored as `account` and keeps the answer: in
/// the limit store, in `cache/limits.json`, and in the journal of resets, where an answer
/// that shows room settles a redemption whose outcome was open. What was last heard about
/// the account's resets stays with the answer when the usage report itself cannot say.
pub async fn fetch(
    source: &Source,
    client: &reqwest::Client,
    provider: &ProviderConfig,
    account: &str,
    renew: bool,
) -> Result<UsageReport, String> {
    let mut report = usage::fetch(client, provider, &source.credentials, account, renew).await?;
    if resets::asked_apart(provider)
        && let Some(kept) = usage::cached(&source.cache_file).remove(&account_key(&provider.id, account))
    {
        report.resets = kept.resets;
        report.resets_asked_at = kept.resets_asked_at;
    }
    usage::keep(&source.cache_file, &report);
    source.desk.journal().settle(&report);
    Ok(report)
}

/// Whether the provider is to be asked which resets the account of `report` holds: it tells
/// them apart from the usage report (Claude), or the report counts some without saying when
/// they lapse (ChatGPT), and the last answer is older than five minutes.
fn asks_resets(provider: &ProviderConfig, report: &UsageReport, now: u64) -> bool {
    let stale = report
        .resets_asked_at
        .is_none_or(|at| now.saturating_sub(at) >= FRESH_SECS);
    match resets::asked_apart(provider) {
        true => stale,
        false => stale && report.resets.iter().any(|offer| offer.available > 0),
    }
}

/// The resets of `report` that are still worth showing at `now`: an answer about them that
/// is older than an hour says nothing about what can be spent.
fn shown_resets(report: &UsageReport, now: u64) -> &[azazel_ai::providers::usage::ResetOffer] {
    let asked_at = report.resets_asked_at.unwrap_or(report.fetched_at);
    match now.saturating_sub(asked_at) < KEPT_SECS {
        true => &report.resets,
        false => &[],
    }
}

/// An e-mail address with all but the first letter of its name hidden: `j***@example.com`.
pub fn redact_email(email: &str) -> String {
    let (name, domain) = match email.split_once('@') {
        Some((name, domain)) => (name, Some(domain)),
        None => (email, None),
    };
    let first: String = name.chars().take(1).collect();
    match domain {
        Some(domain) => format!("{first}***@{domain}"),
        None => format!("{first}***"),
    }
}

/// The e-mail address of an account as it is shown: hidden but for its first letter and its
/// domain when `redact` says so.
pub fn shown_email(account: &Account, redact: bool) -> Option<String> {
    let email = account.email.as_deref()?;
    Some(match redact {
        true => redact_email(email),
        false => email.to_string(),
    })
}

/// The label of an account with its e-mail address behind it when the login has one.
pub fn describe(account: &Account, redact: bool) -> String {
    match shown_email(account, redact) {
        Some(email) => format!("{} ({email})", account.label),
        None if account.login => account.label.clone(),
        None => format!("{} (API key)", account.label),
    }
}

/// A window as a row names it: `5h`, `7d`, `7d Fable` for the weekly window of the Fable
/// models.
pub fn window_name(window: &Window) -> String {
    match &window.scope {
        Some(scope) => {
            let length = window.name.split('-').next().unwrap_or(&window.name);
            // A window that is not named by its length is named by what it counts alone.
            match length.starts_with(|c: char| c.is_ascii_digit()) {
                true => format!("{length} {scope}"),
                false => scope.clone(),
            }
        }
        None => window.name.clone(),
    }
}

/// How the views are to show what they have.
#[derive(Debug, Clone, Copy)]
pub struct View {
    /// Unix seconds.
    pub now: u64,
    /// Hide e-mail addresses but for their first letter and their domain.
    pub redact: bool,
}

/// `$12.50`, or `12.50 EUR` for another currency.
fn money(amount: f64, currency: &str) -> String {
    match currency {
        "USD" => format!("${amount:.2}"),
        other => format!("{amount:.2} {other}"),
    }
}

fn spend_text(spend: &Spend) -> String {
    match spend.limit {
        Some(limit) => format!(
            "{} of {} used",
            money(spend.used, &spend.currency),
            money(limit, &spend.currency)
        ),
        None => format!("{} used", money(spend.used, &spend.currency)),
    }
}

/// The colour of a window by how full it is.
fn level(window: &Window) -> Role {
    if window.exhausted() || window.used >= 1.0 {
        Role::Error
    } else if window.used >= NEARLY_FULL {
        Role::Warning
    } else {
        Role::Accent
    }
}

/// One window as a row `width` columns wide: its name in a column `names` wide, a bar, the
/// share used, when it resets (or for how long the provider refuses requests because of it),
/// and what it counts when that is not everything. A narrow row drops the note, then the time
/// of day, then the bar, so that the share stays.
fn window_row(window: &Window, names: usize, view: &View, width: usize, theme: &Theme) -> Line {
    let percent = (window.used * 100.0).round().clamp(0.0, 999.0) as u32;
    let role = level(window);
    let name = text::truncate(&window_name(window), names, theme.glyph(Glyph::Ellipsis));
    let mut row = format!(
        "{INDENT}{}{}",
        theme.fg(Role::Text),
        text::pad_right(&name, names)
    );
    let mut used = INDENT.len() + names;
    let room = width.saturating_sub(used + 2 + 4);
    let cells = match room {
        room if room > BAR => BAR,
        room if room > BAR_MIN => room - 1,
        _ => 0,
    };
    if cells > 0 {
        let filled = ((window.used.clamp(0.0, 1.0) * cells as f32).round() as usize).min(cells);
        let _ = write!(
            row,
            "  {}{}{}{}",
            theme.fg(role),
            theme.glyph(Glyph::ProgressFull).repeat(filled),
            theme.fg(Role::BorderMuted),
            theme.glyph(Glyph::ProgressEmpty).repeat(cells - filled),
        );
        used += 2 + cells;
        let _ = write!(row, " {}{percent:>3}%", theme.fg(role));
        used += 5;
    } else {
        let _ = write!(row, "  {}{percent:>3}%", theme.fg(role));
        used += 6;
    }
    // What follows the numbers, most wanted first, each in the longest form that still fits.
    // A window the provider refuses for says so in place of "resets".
    let (role, words) = match window.exhausted() {
        true => (Role::Error, "no requests for"),
        false => (Role::Muted, "resets in"),
    };
    let mut tail: Vec<(Role, Vec<String>)> = Vec::new();
    match window.resets_at.filter(|at| *at > view.now) {
        Some(at) => {
            let left = time_left(at - view.now);
            tail.push((
                role,
                vec![
                    format!("{words} {left} ({})", clock(at)),
                    format!("{words} {left}"),
                    left,
                ],
            ));
        }
        None if window.exhausted() => tail.push((role, vec!["no requests".to_string()])),
        None => {}
    }
    if let Some(scope) = &window.scope {
        tail.push((Role::Muted, vec![format!("{scope} only")]));
    }
    for (role, forms) in tail {
        let Some(form) = forms
            .into_iter()
            .find(|form| used + 2 + text::width(form) <= width)
        else {
            continue;
        };
        used += 2 + text::width(&form);
        let _ = write!(row, "  {}{form}", theme.fg(role));
    }
    row
}

/// The usage of each account as the rows of the popup, `width` columns wide: per provider its
/// accounts, the one in use marked; per account its plan, every window with a bar, the share
/// used, the reset time and what it counts, then extra usage, banked resets and until when
/// the account is left alone.
pub fn page(entries: &[Entry], view: &View, width: usize, theme: &Theme) -> Vec<Line> {
    let muted = |line: &str| theme.paint(Role::Muted, line);
    let mut lines: Vec<Line> = Vec::new();
    // Text that says what a row cannot: wrapped under the account it belongs to.
    let note = |lines: &mut Vec<Line>, role: Role, note: &str| {
        let room = width.saturating_sub(INDENT.len()).max(1);
        for part in text::wrap(&text::sanitize(note), room) {
            lines.push(format!("{INDENT}{}", theme.paint(role, &part)));
        }
    };
    if entries.is_empty() {
        lines.extend(text::wrap(NO_LOGIN, width.max(1)));
    }
    let names = entries
        .iter()
        .flat_map(|entry| entry.windows())
        .filter(|window| window.name != "overage")
        .map(|window| text::width(&window_name(window)))
        .max()
        .unwrap_or(0)
        .clamp(2, NAMES);
    let separator = theme.glyph(Glyph::Separator);
    let ellipsis = theme.glyph(Glyph::Ellipsis);
    let mut provider = None;
    for entry in entries {
        if provider != Some(&entry.provider) {
            if provider.is_some() {
                lines.push(String::new());
            }
            provider = Some(&entry.provider);
            lines.push(theme.paint(Role::Accent, &text::truncate(&entry.provider, width, ellipsis)));
        }
        let mark = match entry.in_use {
            true => theme.paint(Role::Accent, theme.glyph(Glyph::Bullet)),
            false => " ".to_string(),
        };
        let mut heading = format!(" {mark} {}{}", theme.fg(Role::Text), entry.account.label);
        let mut facts: Vec<String> = Vec::new();
        facts.extend(shown_email(&entry.account, view.redact));
        if let Some(plan) = entry.report.as_ref().and_then(|report| report.plan.as_deref()) {
            facts.push(format!("{plan} plan"));
        }
        if !facts.is_empty() {
            let _ = write!(heading, "  {}", muted(&facts.join(separator)));
        }
        if entry.in_use {
            let _ = write!(
                heading,
                "{}{}",
                muted(separator),
                theme.paint(Role::Accent, "in use")
            );
        }
        lines.push(text::truncate(&heading, width, ellipsis));

        let windows = entry.windows();
        for window in windows.iter().filter(|window| window.name != "overage") {
            lines.push(window_row(window, names, view, width, theme));
        }
        if let Some(report) = &entry.report {
            let extra = match (report.extra_usage, &report.spend) {
                (Some(true), Some(spend)) => Some(format!("extra usage on{separator}{}", spend_text(spend))),
                (Some(true), None) => Some("extra usage on".to_string()),
                (Some(false), _) => Some("extra usage off".to_string()),
                (None, _) => None,
            };
            if let Some(extra) = extra {
                note(&mut lines, Role::Muted, &extra);
            }
            for offer in shown_resets(report, view.now) {
                if let Some(text) = crate::resets::offer_text(offer, view.now) {
                    note(&mut lines, Role::Muted, &text);
                }
            }
        }
        if let Some(overage) = entry.seen.iter().find(|window| window.name == "overage") {
            note(
                &mut lines,
                Role::Muted,
                &format!("extra usage {}", overage.status),
            );
        }
        for block in &entry.blocks {
            let until = clock(block.until);
            let line = match &block.scope {
                Some(scope) => format!(
                    "{scope} models are not asked on this account until {until} ({})",
                    block.limit()
                ),
                None => format!("out of allowance until {until} ({})", block.limit()),
            };
            note(&mut lines, Role::Warning, &line);
        }
        let age = entry
            .report
            .as_ref()
            .map(|report| view.now.saturating_sub(report.fetched_at))
            .filter(|age| *age >= FRESH_SECS);
        match (&entry.problem, age, windows.is_empty()) {
            (Some(why), _, true) => note(&mut lines, Role::Warning, why),
            (Some(why), Some(age), false) => note(
                &mut lines,
                Role::Muted,
                &format!("as of {} ago ({why})", time_left(age)),
            ),
            (Some(why), None, false) if entry.report.is_none() => note(
                &mut lines,
                Role::Muted,
                &format!("from this session's responses ({why})"),
            ),
            (Some(why), None, false) => note(&mut lines, Role::Muted, why),
            (None, Some(age), false) => {
                note(&mut lines, Role::Muted, &format!("as of {} ago", time_left(age)))
            }
            (None, _, true) => note(&mut lines, Role::Muted, "the provider reported no usage windows"),
            (None, None, false) => {}
        }
    }
    lines
}

/// The usage of each account as plain text, for where there is no box to draw it in.
pub fn render(entries: &[Entry], view: &View) -> String {
    let mut out = String::new();
    for line in page(entries, view, 100, &Theme::dark()) {
        let _ = writeln!(out, "{}", text::strip_ansi(&line).trim_end());
    }
    out
}

/// The usage of each account in a box as wide as the popup of a window `columns` wide would
/// be, set in the middle of that window.
pub fn boxed(entries: &[Entry], view: &View, columns: usize, theme: &Theme) -> Vec<Line> {
    let width = modal::box_width(columns);
    let body = page(entries, view, Frame::inner_width(width).max(1), theme);
    let margin = " ".repeat(columns.saturating_sub(width) / 2);
    Frame::new("Usage", width)
        .render(&body, theme)
        .into_iter()
        .map(|line| format!("{margin}{line}"))
        .collect()
}

fn json_of(entries: &[Entry], view: &View) -> serde_json::Value {
    let window = |window: &Window| {
        json!({
            "name": window.name,
            "label": window.label(),
            "scope": window.scope,
            // To a tenth of a percent: the share is kept in single precision.
            "used": (f64::from(window.used) * 1000.0).round() / 1000.0,
            "resets_at": window.resets_at,
            "status": window.status,
        })
    };
    let rows: Vec<serde_json::Value> = entries
        .iter()
        .map(|entry| {
            let report = entry.report.as_ref();
            json!({
                "provider": entry.provider,
                "account": entry.account.label,
                "email": shown_email(&entry.account, view.redact),
                "in_use": entry.in_use,
                "plan": report.and_then(|report| report.plan.clone()),
                "windows": entry.windows().iter().map(window).collect::<Vec<_>>(),
                "extra_usage": report.and_then(|report| report.extra_usage),
                "spend": report.and_then(|report| report.spend.clone()),
                "resets": report.map(|report| shown_resets(report, view.now).to_vec()).unwrap_or_default(),
                "blocks": entry.blocks,
                "fetched_at": report.map(|report| report.fetched_at),
                "error": entry.problem,
            })
        })
        .collect();
    json!(rows)
}

/// `azazel usage [--json]`: a box in the middle of a terminal, plain text into a pipe, one
/// JSON array with `--json`.
pub async fn run(
    source: &Source,
    config: &azazel_core::Config,
    paths: &azazel_core::Paths,
    as_json: bool,
) -> u8 {
    let entries = collect(source, Ask::Stale).await;
    let view = View {
        now: now_ms() / 1000,
        redact: config.usage.redact_email,
    };
    let coloured = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    if as_json {
        println!("{}", json_of(&entries, &view));
    } else if coloured {
        let (theme, _) = crate::tui::terminal_theme(paths, config, None);
        let columns = crossterm::terminal::size().map_or(80, |(columns, _)| usize::from(columns));
        // The last column stays free: a line that fills it wraps in some terminals.
        for line in boxed(&entries, &view, columns.saturating_sub(1), &theme) {
            println!("{line}");
        }
    } else {
        print!("{}", render(&entries, &view));
    }
    // Nothing to show for any login is a failure; no login at all is not.
    let failed = !entries.is_empty() && entries.iter().all(|entry| entry.windows().is_empty());
    if failed { EXIT_FAILED } else { EXIT_OK }
}

/// The fullest window of an account that counts requests to `model`, for the status line: its
/// name (`5h`) and the share of it that is used. Extra usage is not a window that fills.
pub fn fullest(windows: &[Window], model: &str) -> Option<(String, f32)> {
    windows
        .iter()
        .filter(|window| window.name != "overage" && window.counts(model))
        .max_by(|a, b| a.used.total_cmp(&b.used))
        .map(|window| (window.name.clone(), window.used))
}

/// Asks about the usage of every stored login in the background for as long as it lives.
pub struct Poller(tokio::task::JoinHandle<()>);

impl Drop for Poller {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts asking the providers about every stored login every `secs` seconds (at least once a
/// minute), so that the router knows which account has room before it sends a request. A
/// login is never renewed for this: an account whose token has run out is asked again once a
/// request has renewed it.
pub fn poll(source: Source, secs: u64) -> Poller {
    let every = secs.max(POLL_MIN_SECS);
    Poller(tokio::spawn(async move {
        let client = http_client();
        let mut heard = HashMap::new();
        loop {
            poll_once(&source, &client, every, &mut heard).await;
            tokio::time::sleep(Duration::from_secs(every)).await;
        }
    }))
}

/// One round of [`poll`]. `heard` is when the answer this run last took in about each account
/// was fetched. An answer another run kept in the meantime is taken in instead of asking
/// again, and so are the blocks other runs wrote.
async fn poll_once(source: &Source, client: &reqwest::Client, every: u64, heard: &mut HashMap<String, u64>) {
    let credentials = &source.credentials;
    let limits = credentials.limits();
    limits.reload();
    let cache = usage::cached(&source.cache_file);
    let now = now_ms() / 1000;
    for provider in source
        .providers
        .iter()
        .filter(|provider| usage::supported(provider))
    {
        for account in credentials.accounts(&provider.id) {
            if !account.login {
                continue;
            }
            let label = account.label.as_str();
            let key = account_key(&provider.id, label);
            let mut latest = heard.get(&key).copied().unwrap_or(0);
            if let Some(report) = cache.get(&key).filter(|report| report.fetched_at > latest) {
                latest = report.fetched_at;
                if now.saturating_sub(latest) < KEPT_SECS {
                    limits.record_report(&provider.id, label, &report.windows, latest);
                }
            }
            if now.saturating_sub(latest) >= every {
                match fetch(source, client, provider, label, false).await {
                    Ok(report) => latest = report.fetched_at,
                    Err(why) => tracing::debug!("usage of {key} was not fetched: {why}"),
                }
            }
            heard.insert(key, latest);
        }
    }
}

struct UsageCommand {
    source: Source,
    redact: bool,
}

#[async_trait]
impl CommandHandler for UsageCommand {
    async fn run(&self, _args: &str, _ctx: &ExtCtx) -> CommandOutcome {
        let entries = collect(&self.source, Ask::Stale).await;
        let view = View {
            now: now_ms() / 1000,
            redact: self.redact,
        };
        CommandOutcome::Show(render(&entries, &view).trim_end().to_string())
    }
}

struct StatsCommand {
    paths: azazel_core::Paths,
}

#[async_trait]
impl CommandHandler for StatsCommand {
    async fn run(&self, args: &str, _ctx: &ExtCtx) -> CommandOutcome {
        let days = match args.trim() {
            "" => 30,
            text => match crate::stats::parse_days(text) {
                Some(days) => days,
                None => return CommandOutcome::Error("usage: /stats [days]".to_string()),
            },
        };
        let paths = self.paths.clone();
        // Reading the usage log is file work.
        let read = move || {
            crate::stats::text(
                &paths,
                crate::stats::Window::days(days),
                crate::stats::By::Provider,
            )
        };
        match tokio::task::spawn_blocking(read).await {
            Ok(text) => CommandOutcome::Show(text.trim_end().to_string()),
            Err(e) => CommandOutcome::Error(format!("could not read the usage log: {e}")),
        }
    }
}

/// Registers `/usage` and `/stats`.
pub struct UsageExt {
    pub source: Source,
    /// `[usage] redact_email`.
    pub redact_email: bool,
    pub paths: azazel_core::Paths,
}

/// Where the answers of the providers are kept.
pub fn cache_file(paths: &azazel_core::Paths) -> PathBuf {
    paths.cache_dir().join("limits.json")
}

impl Extension for UsageExt {
    fn id(&self) -> &str {
        "usage"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.command(Command {
            name: "usage".to_string(),
            description: "What your Claude and ChatGPT subscriptions have left, and when each window resets"
                .to_string(),
            argument_hint: None,
            handler: Arc::new(UsageCommand {
                source: self.source.clone(),
                redact: self.redact_email,
            }),
        });
        r.command(Command {
            name: "stats".to_string(),
            description: "Tokens and cost per day, provider, model, project and agent, from the usage log"
                .to_string(),
            argument_hint: Some("[days]".to_string()),
            handler: Arc::new(StatsCommand {
                paths: self.paths.clone(),
            }),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeMap;

    use azazel_ai::Credential;
    use azazel_ai::providers::usage::ResetOffer;
    use azazel_tui::GlyphLevel;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const NOW: u64 = 1_000_000;

    fn window(name: &str, used: f32, resets_in: Option<u64>, status: &str) -> Window {
        Window::new(name, used, resets_in.map(|secs| NOW + secs), status)
    }

    fn account(label: &str, email: Option<&str>) -> Account {
        Account {
            label: label.to_string(),
            login: true,
            email: email.map(str::to_string),
            org: None,
        }
    }

    fn view() -> View {
        View {
            now: NOW,
            redact: true,
        }
    }

    /// Two Claude accounts and a ChatGPT one that could not be asked.
    fn entries() -> Vec<Entry> {
        vec![
            Entry {
                provider: "anthropic".into(),
                account: account("default", Some("jane@example.com")),
                in_use: true,
                report: Some(UsageReport {
                    provider: "anthropic".into(),
                    account: "default".into(),
                    plan: Some("max".into()),
                    windows: vec![
                        window("5h", 0.42, Some(2 * 3600 + 600), "allowed"),
                        window("7d", 1.0, Some(3 * 86_400), "rejected"),
                        window("7d-fable", 0.85, Some(3 * 86_400), "allowed").scoped("Fable"),
                    ],
                    extra_usage: Some(true),
                    spend: Some(Spend {
                        used: 12.5,
                        limit: Some(50.0),
                        currency: "USD".into(),
                    }),
                    resets: Vec::new(),
                    resets_asked_at: None,
                    fetched_at: NOW,
                }),
                seen: Vec::new(),
                problem: None,
                blocks: vec![LimitBlock {
                    until: NOW + 3 * 86_400,
                    since: NOW,
                    window: Some("7d".into()),
                    scope: None,
                }],
            },
            Entry {
                provider: "anthropic".into(),
                account: account("work", Some("j.doe@corp.example")),
                in_use: false,
                report: Some(UsageReport {
                    provider: "anthropic".into(),
                    account: "work".into(),
                    plan: None,
                    windows: vec![window("5h", 0.05, None, "allowed")],
                    extra_usage: Some(false),
                    spend: None,
                    resets: vec![ResetOffer::held("credits", 2)],
                    resets_asked_at: None,
                    // Asked twelve minutes ago, and not since.
                    fetched_at: NOW - 12 * 60,
                }),
                seen: Vec::new(),
                problem: Some("anthropic answered HTTP 500 when asked for its usage".into()),
                blocks: Vec::new(),
            },
            Entry {
                provider: "openai-codex".into(),
                account: account("default", None),
                in_use: true,
                report: None,
                seen: vec![
                    window("5h", 0.05, None, "allowed"),
                    window("overage", 0.0, None, "allowed"),
                ],
                problem: Some("could not reach openai-codex for its usage".into()),
                blocks: Vec::new(),
            },
        ]
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| text::strip_ansi(line).trim_end().to_string())
            .collect()
    }

    #[test]
    fn the_page_lists_each_account_with_a_bar_a_share_a_reset_and_a_scope_per_window() {
        let entries = entries();
        let lines = plain(&page(&entries, &view(), 96, &Theme::dark()));
        let week = clock(NOW + 3 * 86_400);
        let expected = [
            "anthropic".to_string(),
            // The account requests go to is marked; its address shows a letter and the domain.
            " ● default  j***@example.com · max plan · in use".to_string(),
            format!(
                "     5h        ━━━━━━━━────────────  42%  resets in 2 h 10 min ({})",
                clock(NOW + 2 * 3600 + 600)
            ),
            format!("     7d        ━━━━━━━━━━━━━━━━━━━━ 100%  no requests for 3 d ({week})"),
            format!("     7d Fable  ━━━━━━━━━━━━━━━━━───  85%  resets in 3 d ({week})  Fable only"),
            "     extra usage on · $12.50 of $50.00 used".to_string(),
            format!("     out of allowance until {week} (7-day limit)"),
            "   work  j***@corp.example".to_string(),
            "     5h        ━───────────────────   5%".to_string(),
            "     extra usage off".to_string(),
            "     2 banked resets".to_string(),
            "     as of 12 min ago (anthropic answered HTTP 500 when asked for its usage)".to_string(),
            String::new(),
            "openai-codex".to_string(),
            " ● default · in use".to_string(),
            "     5h        ━───────────────────   5%".to_string(),
            "     extra usage allowed".to_string(),
            "     from this session's responses (could not reach openai-codex for its usage)".to_string(),
        ];
        assert_eq!(lines, expected);

        // A full window is drawn in the error colour, one that is nearly full in the warning
        // colour, and the rest in the accent.
        let theme = Theme::dark();
        let coloured = page(&entries, &view(), 96, &theme);
        assert!(coloured[2].contains(&format!("{}━", theme.fg(Role::Accent))));
        assert!(coloured[3].contains(&format!("{}━", theme.fg(Role::Error))));
        assert!(coloured[4].contains(&format!("{}━", theme.fg(Role::Warning))));

        // With nobody signed in the page says how to.
        assert!(
            plain(&page(&[], &view(), 96, &Theme::dark()))[0].starts_with("no subscription is signed in")
        );
        assert!(render(&[], &view()).starts_with("no subscription is signed in"));
    }

    #[test]
    fn a_narrow_page_keeps_the_share_and_never_overflows() {
        let entries = entries();
        for level in [GlyphLevel::Unicode, GlyphLevel::Ascii] {
            let theme = Theme::dark().with_glyphs(level);
            for width in 20..=160 {
                let lines = plain(&page(&entries, &view(), width, &theme));
                for line in &lines {
                    assert!(text::width(line) <= width, "{width}: {line:?}");
                }
                // Whatever else is dropped, how full each window is stays.
                for share in [" 42%", "100%", " 85%", "  5%"] {
                    assert!(lines.iter().any(|line| line.contains(share)), "{width}: {share}");
                }
            }
        }
        let theme = Theme::dark();
        // The note goes first, then the time of day, then the words around the wait, then
        // the bar.
        let at = |width: usize| plain(&page(&entries, &view(), width, &theme))[4].clone();
        assert!(at(96).ends_with("Fable only"));
        assert_eq!(
            at(70),
            format!(
                "     7d Fable  ━━━━━━━━━━━━━━━━━───  85%  resets in 3 d ({})",
                clock(NOW + 3 * 86_400)
            )
        );
        assert_eq!(at(60), "     7d Fable  ━━━━━━━━━━━━━━━━━───  85%  resets in 3 d");
        assert_eq!(at(50), "     7d Fable  ━━━━━━━━━━━━━━━━━───  85%  3 d");
        assert_eq!(at(44), "     7d Fable  ━━━━━━━━━━━━━━━━━───  85%");
        assert_eq!(at(30), "     7d Fable  ━━━━━━━━━─  85%");
        assert_eq!(at(22), "     7d Fable   85%");

        // The box of `azazel usage` is as wide as the popup and sits in the middle.
        let framed = plain(&boxed(&entries, &view(), 120, &theme));
        assert!(framed[0].starts_with("          ╭─ Usage ─"), "{}", framed[0]);
        assert!(
            framed.iter().all(|line| text::width(line) == 10 + 100),
            "{framed:?}"
        );
        assert!(framed[2].contains("j***@example.com"));
    }

    #[test]
    fn e_mail_addresses_are_hidden_unless_the_setting_shows_them() {
        assert_eq!(redact_email("jane@example.com"), "j***@example.com");
        assert_eq!(redact_email("j@example.com"), "j***@example.com");
        assert_eq!(redact_email("élodie@exemple.fr"), "é***@exemple.fr");
        assert_eq!(redact_email("not-an-address"), "n***");
        assert_eq!(redact_email(""), "***");

        let entries = entries();
        let hidden = render(&entries, &view());
        assert!(hidden.contains("j***@example.com") && hidden.contains("j***@corp.example"));
        assert!(!hidden.contains("jane") && !hidden.contains("j.doe"), "{hidden}");
        let listed = json_of(&entries, &view());
        assert_eq!(listed[0]["email"], "j***@example.com");
        assert_eq!(listed[2]["email"], serde_json::Value::Null);

        let open = View {
            redact: false,
            ..view()
        };
        assert!(render(&entries, &open).contains("default  jane@example.com"));
        assert_eq!(json_of(&entries, &open)[1]["email"], "j.doe@corp.example");
        assert_eq!(describe(&entries[0].account, false), "default (jane@example.com)");
        assert_eq!(describe(&entries[0].account, true), "default (j***@example.com)");
    }

    #[test]
    fn the_json_names_every_window_with_its_scope_and_what_holds_the_account_back() {
        let listed = json_of(&entries(), &view());
        let first = &listed[0];
        assert_eq!((&first["in_use"], &first["plan"]), (&json!(true), &json!("max")));
        assert_eq!(
            first["windows"][2],
            json!({
                "name": "7d-fable", "label": "7-day Fable limit", "scope": "Fable",
                "used": 0.85, "resets_at": NOW + 3 * 86_400, "status": "allowed"
            })
        );
        assert_eq!(first["windows"][0]["scope"], serde_json::Value::Null);
        assert_eq!(
            first["spend"],
            json!({ "used": 12.5, "limit": 50.0, "currency": "USD" })
        );
        assert_eq!(first["blocks"][0]["until"], NOW + 3 * 86_400);
        assert_eq!(listed[1]["resets"][0]["available"], 2);
        assert_eq!(
            listed[1]["error"],
            "anthropic answered HTTP 500 when asked for its usage"
        );
        // Without an answer the windows are what headers said.
        assert_eq!(listed[2]["windows"][0]["used"], 0.05);
        assert_eq!(listed[2]["fetched_at"], serde_json::Value::Null);
    }

    #[test]
    fn the_status_line_names_the_fullest_window_that_counts_the_model() {
        let opus = "claude-opus-5-5";
        assert_eq!(
            fullest(&[window("5h", 0.12, None, "allowed")], opus),
            Some(("5h".to_string(), 0.12))
        );
        let windows = [
            window("5h", 0.84, None, "allowed"),
            window("7d", 0.91, None, "allowed"),
            window("7d-fable", 1.0, None, "rejected").scoped("Fable"),
            window("overage", 1.0, None, "allowed"),
        ];
        // The window of another model family is not this model's concern; extra usage is not
        // a window that fills.
        assert_eq!(fullest(&windows, opus), Some(("7d".to_string(), 0.91)));
        assert_eq!(
            fullest(&windows, "claude-fable-5-2"),
            Some(("7d-fable".to_string(), 1.0))
        );
        assert_eq!(fullest(&[window("overage", 1.0, None, "allowed")], opus), None);
        assert_eq!(fullest(&[], opus), None);
    }

    fn login(email: &str, access: &str, expires_in_ms: u64) -> Credential {
        Credential::OAuth {
            access: access.into(),
            refresh: "refresh".into(),
            expires: now_ms() + expires_in_ms,
            extra: BTreeMap::from([("email".to_string(), json!(email))]),
        }
    }

    /// A source whose two subscription providers are served by `server`.
    fn source(server: &MockServer, dir: &std::path::Path) -> Source {
        let credentials = Arc::new(Credentials::with_env(
            dir.join("auth.json"),
            Arc::new(|_: &str| None),
        ));
        let mut providers = crate::login::key_providers(&azazel_core::Config::default()).unwrap();
        for provider in &mut providers {
            match provider.id.as_str() {
                "openai-codex" => provider.base_url = format!("{}/backend-api/codex", server.uri()),
                "anthropic" => provider.base_url = server.uri(),
                _ => {}
            }
        }
        Source {
            providers,
            credentials,
            cache_file: dir.join("cache").join("limits.json"),
            desk: Desk::new(dir.join("resets")),
        }
    }

    fn chatgpt_answer() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": { "used_percent": 30, "limit_window_seconds": 18000, "reset_after_seconds": 900 },
                "secondary_window": { "used_percent": 9, "limit_window_seconds": 604800, "reset_after_seconds": 86400 }
            }
        }))
    }

    #[tokio::test]
    async fn an_answer_is_asked_for_once_and_then_read_from_the_cache() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/backend-api/wham/usage"))
            .respond_with(chatgpt_answer())
            .expect(3)
            .mount(&server)
            .await;
        // The Claude login's endpoint is down: its account is still listed, with the reason.
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let source = source(&server, dir.path());
        let credentials = &source.credentials;
        credentials
            .store("openai-codex", login("c@example.com", "access", 3_600_000))
            .unwrap();
        credentials
            .store("anthropic", login("a@example.com", "access", 3_600_000))
            .unwrap();
        credentials
            .store("anthropic#key", Credential::ApiKey { key: "k".into() })
            .unwrap();

        // Nothing is known before anybody asked, and looking at what is known asks nobody.
        let known = collect(&source, Ask::Known).await;
        assert!(
            known
                .iter()
                .all(|entry| entry.report.is_none() && entry.problem.is_none())
        );

        let first = collect(&source, Ask::Stale).await;
        // Logins only, in provider order; the stored key is not a subscription.
        let listed: Vec<(&str, &str, bool)> = first
            .iter()
            .map(|entry| {
                (
                    entry.provider.as_str(),
                    entry.account.label.as_str(),
                    entry.in_use,
                )
            })
            .collect();
        assert_eq!(
            listed,
            [("anthropic", "default", true), ("openai-codex", "default", true)]
        );
        assert!(first[0].report.is_none());
        assert!(first[0].problem.as_deref().unwrap().contains("HTTP 500"));
        let report = first[1].report.clone().unwrap();
        assert_eq!(report.plan.as_deref(), Some("plus"));
        assert_eq!(report.windows.len(), 2);

        // A second look within five minutes does not ask ChatGPT again and shows the same.
        let again = collect(&source, Ask::Stale).await;
        assert_eq!(again[1].report, Some(report.clone()));
        let shown = render(&again, &view());
        assert!(
            shown.contains("openai-codex\n ● default  c***@example.com · plus plan · in use"),
            "{shown}"
        );
        assert!(shown.contains("anthropic answered HTTP 500"), "{shown}");
        assert_eq!(collect(&source, Ask::Known).await[1].report, Some(report.clone()));

        // The refresh key asks whatever the age of the answer: the second request.
        let asked = collect(&source, Ask::All).await;
        assert!(asked[1].report.is_some() && asked[1].problem.is_none());

        // An answer older than five minutes is asked for again: the third request. When that
        // fails, the old answer is shown with the reason.
        let mut aged = report;
        aged.fetched_at -= FRESH_SECS + 1;
        usage::keep(&source.cache_file, &aged);
        let asked = collect(&source, Ask::Stale).await;
        assert!(asked[1].report.as_ref().unwrap().fetched_at > aged.fetched_at);
        // An account that cannot be asked shows its last answer, however old, with the reason.
        aged.provider = "anthropic".into();
        usage::keep(&source.cache_file, &aged);
        let failed = collect(&source, Ask::Stale).await;
        assert_eq!(failed[0].report, Some(aged));
        assert!(failed[0].problem.as_deref().unwrap().contains("HTTP 500"));
    }

    #[tokio::test]
    async fn the_background_asks_once_per_interval_and_never_for_a_login_that_needs_renewing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/backend-api/wham/usage"))
            .and(header("authorization", "Bearer good"))
            .respond_with(chatgpt_answer())
            .expect(1)
            .mount(&server)
            .await;
        // Asking with the token that is about to run out would mean renewing it first.
        Mock::given(method("GET"))
            .and(header("authorization", "Bearer running-out"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let source = source(&server, dir.path());
        let credentials = &source.credentials;
        credentials
            .store("openai-codex", login("c@example.com", "good", 3_600_000))
            .unwrap();
        credentials
            .store("openai-codex#work", login("w@example.com", "running-out", 5_000))
            .unwrap();
        credentials
            .store("openai-codex#key", Credential::ApiKey { key: "k".into() })
            .unwrap();
        let limits = credentials.limits();
        limits.persist(source.cache_file.clone());
        let client = http_client();
        let mut heard = HashMap::new();

        poll_once(&source, &client, 300, &mut heard).await;
        // The router now knows the windows of the account that could be asked about.
        let windows = limits.windows("openai-codex", "default");
        assert_eq!(
            windows
                .iter()
                .map(|w| (w.name.as_str(), w.used))
                .collect::<Vec<_>>(),
            [("5h", 0.3), ("7d", 0.09)]
        );
        assert!(limits.windows("openai-codex", "work").is_empty());
        assert!(limits.windows("openai-codex", "key").is_empty());
        // The answer is kept for `/usage` and for other runs.
        assert!(usage::cached(&source.cache_file).contains_key("openai-codex"));

        // A round inside the interval asks nobody: the one request the mock expects.
        poll_once(&source, &client, 300, &mut heard).await;

        // An answer another run kept in the meantime is taken in without asking; so are the
        // blocks it wrote.
        let mut theirs = usage::cached(&source.cache_file)["openai-codex"].clone();
        theirs.account = "work".into();
        theirs.windows[0].used = 0.77;
        usage::keep(&source.cache_file, &theirs);
        let other = azazel_ai::LimitStore::new();
        other.persist(source.cache_file.clone());
        other.block("openai-codex", "work", now_ms() / 1000 + 600);
        poll_once(&source, &client, 300, &mut heard).await;
        assert_eq!(limits.windows("openai-codex", "work")[0].used, 0.77);
        assert!(limits.blocked_until("openai-codex", "work").is_some());
    }
}
