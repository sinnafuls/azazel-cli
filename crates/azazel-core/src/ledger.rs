//! The usage ledger: one JSON line per answered model request, in `<home>/usage/YYYY-MM.jsonl`
//! (the local month of the request). It is kept apart from the sessions, so deleting sessions or
//! agent transcripts never takes the history of what was spent with it.

use std::collections::{BTreeMap, HashSet};
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use azazel_ai::{ModelRef, RequestOrigin, Role, Usage, UsageSink};
use serde::{Deserialize, Serialize};

use crate::session::{Entry, EntryKind, Header};

/// Present once the history of the session files has been imported.
const MARKER: &str = "backfilled";

fn is_false(value: &bool) -> bool {
    !*value
}

/// One request.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Line {
    /// When the request was answered, UTC.
    pub at: String,
    pub provider: String,
    pub model: String,
    /// The project's folder name under `<home>/projects`.
    pub project: String,
    pub session: String,
    /// `lead`, a sub-agent's id, or the purpose of a side request (`btw`, `memory`, ...).
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub def: Option<String>,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The catalogue's estimate at API prices; absent when the model has no price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// The request went through a subscription login, so the cost was not paid per token.
    pub login: bool,
    pub requests: u32,
    /// Imported from a session file, not written when the request was made.
    #[serde(skip_serializing_if = "is_false")]
    pub backfilled: bool,
    /// For an imported line: the entry of the session file it came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
}

impl Line {
    /// Tokens that were not read from a cache.
    pub fn fresh(&self) -> u64 {
        self.input + self.output + self.cache_write
    }
}

/// `<home>/usage`.
pub fn dir(home: &Path) -> PathBuf {
    home.join("usage")
}

/// The local `YYYY-MM` of a UTC timestamp.
fn month_of(at: &str) -> Option<String> {
    let instant = at.parse::<jiff::Timestamp>().ok()?;
    let date = instant.to_zoned(jiff::tz::TimeZone::system()).date();
    Some(format!("{:04}-{:02}", date.year(), date.month()))
}

/// Writes the ledger as the router reports requests.
pub struct Ledger {
    dir: PathBuf,
    project: String,
    /// Whether a provider is reached through a subscription login.
    login: Box<dyn Fn(&str) -> bool + Send + Sync>,
    warned: AtomicBool,
}

impl Ledger {
    pub fn new(home: &Path, project: &str, login: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        Ledger {
            dir: dir(home),
            project: project.to_string(),
            login: Box::new(login),
            warned: AtomicBool::new(false),
        }
    }

    fn append(&self, line: &Line) -> io::Result<()> {
        let month = month_of(&line.at).ok_or_else(|| io::Error::other("unreadable time"))?;
        std::fs::create_dir_all(&self.dir)?;
        let mut text = serde_json::to_string(line).map_err(io::Error::other)?;
        text.push('\n');
        // One `write` of the whole line, in append mode: lines of other processes never mix with it.
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(format!("{month}.jsonl")))?
            .write_all(text.as_bytes())
    }
}

impl UsageSink for Ledger {
    fn record(&self, model: &ModelRef, origin: Option<&RequestOrigin>, usage: &Usage) {
        let line = Line {
            at: jiff::Timestamp::now().to_string(),
            provider: model.provider.clone(),
            model: model.model.clone(),
            project: self.project.clone(),
            session: origin.map(|o| o.session.clone()).unwrap_or_default(),
            agent: origin.map_or_else(|| "other".to_string(), |o| o.agent.clone()),
            def: origin.and_then(|o| o.def.clone()),
            input: usage.input,
            output: usage.output,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
            cost_usd: usage.cost_usd,
            login: (self.login)(&model.provider),
            requests: 1,
            backfilled: false,
            entry: None,
        };
        if let Err(e) = self.append(&line)
            && !self.warned.swap(true, Ordering::Relaxed)
        {
            tracing::warn!("the usage ledger cannot be written: {e}");
        }
    }
}

/// The month files of `dir`, oldest first, from `since_month` (`YYYY-MM`) on.
fn month_files(dir: &Path, since_month: Option<&str>) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            path.extension().is_some_and(|ext| ext == "jsonl")
                && stem.len() == 7
                && since_month.is_none_or(|since| stem >= since)
        })
        .collect();
    files.sort();
    files
}

/// Calls `each` with every line of the ledger in `dir`, streaming; months before `since_month`
/// are not opened. Lines that do not parse are skipped.
pub fn read(dir: &Path, since_month: Option<&str>, mut each: impl FnMut(Line)) {
    for path in month_files(dir, since_month) {
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        for text in BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(line) = serde_json::from_str::<Line>(&text) {
                each(line);
            }
        }
    }
}

/// Imports history once: when the marker is missing, [`import`]s the sessions of every project
/// under `home` and writes the marker. Returns how many requests were imported.
pub fn ensure_backfilled(home: &Path) -> usize {
    let usage = dir(home);
    if usage.join(MARKER).exists() || std::fs::create_dir_all(&usage).is_err() {
        return 0;
    }
    // Another azazel may be importing right now: only one does, the others leave it be.
    let lock = usage.join("backfill.lock");
    let taken = |lock: &Path| OpenOptions::new().write(true).create_new(true).open(lock).is_ok();
    if !taken(&lock) {
        let stale = std::fs::metadata(&lock)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age > std::time::Duration::from_secs(600));
        if !stale || std::fs::remove_file(&lock).is_err() || !taken(&lock) {
            return 0;
        }
    }
    let imported = match usage.join(MARKER).exists() {
        true => 0,
        false => rebuild(home),
    };
    let _ = std::fs::remove_file(&lock);
    imported
}

/// Whether the history import is running in some azazel process now.
pub fn backfilling(home: &Path) -> bool {
    let usage = dir(home);
    !usage.join(MARKER).exists() && usage.join("backfill.lock").exists()
}

/// The providers `home`'s credential file holds a sign-in for (`anthropic`, `openai-codex`, …):
/// imported replies of those count as subscription use.
fn login_providers(home: &Path) -> HashSet<String> {
    let credentials = azazel_ai::Credentials::new(home.join("auth.json"));
    let providers: HashSet<String> = credentials
        .load()
        .into_keys()
        .map(|key| key.split('#').next().unwrap_or_default().to_string())
        .collect();
    providers
        .into_iter()
        .filter(|provider| credentials.accounts(provider).iter().any(|account| account.login))
        .collect()
}

/// Imports what the ledger lacks from the session files, and writes the marker.
pub fn rebuild(home: &Path) -> usize {
    let usage = dir(home);
    let logins = login_providers(home);
    let imported = import(&home.join("projects"), &usage, &logins).unwrap_or_else(|e| {
        tracing::warn!("the usage history cannot be imported: {e}");
        0
    });
    if std::fs::create_dir_all(&usage).is_ok() {
        let _ = std::fs::write(usage.join(MARKER), jiff::Timestamp::now().to_string());
    }
    imported
}

/// Reads every session and sub-agent transcript under `projects` and appends to the ledger in
/// `dir` a line for each reply with usage, at the time of the reply. A reply already imported
/// (same session and entry) is not imported again, and nothing at or after the first line the
/// ledger itself wrote is, since those requests are in the ledger already. Replies of a provider
/// in `logins` are marked as subscription use.
pub fn import(projects: &Path, dir: &Path, logins: &HashSet<String>) -> io::Result<usize> {
    let mut live_from: Option<String> = None;
    let mut seen: HashSet<(String, String)> = HashSet::new();
    read(dir, None, |line| {
        if line.backfilled {
            seen.insert((line.session, line.entry.unwrap_or_default()));
        } else if live_from.as_ref().is_none_or(|first| line.at < *first) {
            live_from = Some(line.at);
        }
    });
    let mut by_month: BTreeMap<String, String> = BTreeMap::new();
    let mut count = 0;
    let mut take = |mut line: Line| {
        line.login = logins.contains(&line.provider);
        let key = (line.session.clone(), line.entry.clone().unwrap_or_default());
        if live_from.as_ref().is_some_and(|first| line.at >= *first) || !seen.insert(key) {
            return;
        }
        let Some(month) = month_of(&line.at) else {
            return;
        };
        if let Ok(text) = serde_json::to_string(&line) {
            let out = by_month.entry(month).or_default();
            out.push_str(&text);
            out.push('\n');
            count += 1;
        }
    };
    for project in std::fs::read_dir(projects)?.flatten() {
        let name = project.file_name().to_string_lossy().into_owned();
        let sessions = project.path().join("sessions");
        for file in std::fs::read_dir(&sessions).into_iter().flatten().flatten() {
            let path = file.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                let Some(session) = session_id(&path) else {
                    continue;
                };
                replies(&path, &name, &session, "lead", &mut take);
                let agents = sessions.join(&session).join(crate::paths::AGENT_TRANSCRIPTS);
                for transcript in std::fs::read_dir(agents).into_iter().flatten().flatten() {
                    let path = transcript.path();
                    if let (true, Some(agent)) = (
                        path.extension().is_some_and(|ext| ext == "jsonl"),
                        path.file_stem().and_then(|s| s.to_str()),
                    ) {
                        replies(&path, &name, &session, agent, &mut take);
                    }
                }
            }
        }
    }
    std::fs::create_dir_all(dir)?;
    for (month, text) in by_month {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{month}.jsonl")))?
            .write_all(text.as_bytes())?;
    }
    Ok(count)
}

/// The id in the header of a session file.
fn session_id(path: &Path) -> Option<String> {
    let first = BufReader::new(std::fs::File::open(path).ok()?)
        .lines()
        .next()?
        .ok()?;
    serde_json::from_str::<Header>(&first)
        .ok()
        .map(|header| header.id)
}

/// Calls `take` with a line for each assistant message with usage in a transcript.
fn replies(path: &Path, project: &str, session: &str, agent: &str, take: &mut impl FnMut(Line)) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    for text in BufReader::new(file).lines().map_while(Result::ok) {
        // Most lines are not replies; skipping them unparsed is most of the speed.
        if !text.contains("\"usage\"") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Entry>(&text) else {
            continue;
        };
        let EntryKind::Message { message } = entry.kind else {
            continue;
        };
        let Some(meta) = message.meta.filter(|_| message.role == Role::Assistant) else {
            continue;
        };
        let usage = meta.usage;
        take(Line {
            at: entry.timestamp,
            provider: meta.provider,
            model: meta.model,
            project: project.to_string(),
            session: session.to_string(),
            agent: agent.to_string(),
            def: None,
            input: usage.input,
            output: usage.output,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
            cost_usd: usage.cost_usd,
            login: false,
            requests: 1,
            backfilled: true,
            entry: Some(match agent {
                "lead" => entry.id,
                agent => format!("{agent}:{}", entry.id),
            }),
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn session_file(dir: &Path, id: &str, replies: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut text = format!(
            "{{\"type\":\"session\",\"version\":1,\"id\":\"{id}\",\"timestamp\":\"2026-03-01T10:00:00Z\",\"cwd\":\"x\",\"parentSession\":null}}\n"
        );
        for (entry, at) in replies {
            text.push_str(&format!(
                "{{\"id\":\"{entry}\",\"parentId\":null,\"timestamp\":\"{at}\",\"type\":\"message\",\"message\":{{\"role\":\"assistant\",\"content\":[],\"meta\":{{\"provider\":\"anthropic\",\"model\":\"fable\",\"api\":\"anthropic-messages\",\"usage\":{{\"input\":10,\"output\":5,\"cache_read\":100,\"cache_write\":0,\"reasoning\":0}},\"stop\":\"stop\"}}}}}}\n"
            ));
        }
        std::fs::write(dir.join(format!("{id}.jsonl")), text).unwrap();
    }

    fn lines(dir: &Path) -> Vec<Line> {
        let mut out = Vec::new();
        read(dir, None, |line| out.push(line));
        out
    }

    #[test]
    fn a_session_with_replies_on_two_days_lands_on_both_and_is_imported_once() {
        let home = tempfile::tempdir().unwrap();
        let sessions = home.path().join("projects").join("proj-1").join("sessions");
        session_file(
            &sessions,
            "s1",
            &[("e1", "2026-03-01T12:00:00Z"), ("e2", "2026-04-15T12:00:00Z")],
        );
        // A sub-agent of the session has a transcript of its own.
        session_file(
            &sessions.join("s1").join("agents"),
            "worker",
            &[("e1", "2026-04-15T13:00:00Z")],
        );

        assert_eq!(ensure_backfilled(home.path()), 3);
        assert_eq!(
            ensure_backfilled(home.path()),
            0,
            "the marker stops a second import"
        );
        // A forced rebuild imports nothing it already has.
        assert_eq!(rebuild(home.path()), 0);

        let mut all = lines(&dir(home.path()));
        all.sort_by(|a, b| a.at.cmp(&b.at));
        assert_eq!(all.len(), 3);
        assert!(
            all.iter()
                .all(|l| l.backfilled && l.project == "proj-1" && l.session == "s1")
        );
        assert_eq!(all[0].at, "2026-03-01T12:00:00Z");
        assert_eq!(all[1].at, "2026-04-15T12:00:00Z");
        assert_eq!(all[2].agent, "worker");
        let months: Vec<String> = month_files(&dir(home.path()), None)
            .iter()
            .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(months, ["2026-03", "2026-04"]);
    }

    #[test]
    fn requests_the_ledger_wrote_itself_are_not_imported_again() {
        let home = tempfile::tempdir().unwrap();
        let sessions = home.path().join("projects").join("proj-1").join("sessions");
        // Later than the request the ledger is about to write.
        let later = (jiff::Timestamp::now() + jiff::SignedDuration::from_hours(1)).to_string();
        session_file(&sessions, "s1", &[("e1", "2026-03-01T12:00:00Z"), ("e2", &later)]);
        let ledger = Ledger::new(home.path(), "proj-1", |_| true);
        let usage = Usage {
            input: 10,
            output: 5,
            cache_read: 100,
            ..Usage::default()
        };
        let origin = RequestOrigin {
            session: "s1".into(),
            agent: "lead".into(),
            def: Some("lead".into()),
        };
        ledger.record(&ModelRef::new("anthropic", "fable"), Some(&origin), &usage);
        assert_eq!(
            rebuild(home.path()),
            1,
            "only the reply from before the ledger is imported"
        );
        let all = lines(&dir(home.path()));
        assert_eq!(all.len(), 2);
        let live = all.iter().find(|l| !l.backfilled).unwrap();
        assert!(
            live.login && live.requests == 1 && live.fresh() == 15 && live.def.as_deref() == Some("lead")
        );
    }
}
