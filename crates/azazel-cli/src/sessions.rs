//! `azazel sessions ...` and `azazel export`: the sessions of this project (or of every project)
//! from the shell, bringing one in from a file, and the retention prune that runs at start.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use azazel_core::paths::canonical;
use azazel_core::session::{SessionSummary, parse_lines};
use azazel_core::{Config, LoadOptions, Paths, SessionStore, TrustStore};
use serde_json::json;

use crate::cli::SessionsAction;
use crate::export::{Format, default_file_name, has_messages, render, sanitized};
use crate::print::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};

/// How long fetching a session to import may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// The session directories to look in: this project's, or every project's.
pub(crate) fn session_dirs(paths: &Paths, all_projects: bool) -> Vec<PathBuf> {
    if !all_projects {
        return vec![paths.sessions_dir()];
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(paths.home.join("projects"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|project| project.path().join("sessions"))
        .filter(|dir| dir.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// `2026-11-12 14:05`, in local time; the text as it is when it does not parse.
pub(crate) fn local_time(timestamp: &str) -> String {
    timestamp
        .parse::<jiff::Timestamp>()
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| timestamp.to_string())
}

/// What a session is called in lists: its name, else its first prompt.
pub fn label(summary: &SessionSummary) -> String {
    let text = summary
        .name
        .clone()
        .or_else(|| summary.first_prompt.clone())
        .unwrap_or_else(|| "(empty)".to_string());
    let line = text.lines().next().unwrap_or_default();
    azazel_tui::text::truncate(line, 60, "...")
}

fn summary_json(summary: &SessionSummary) -> serde_json::Value {
    json!({
        "id": summary.id,
        "path": summary.path,
        "name": summary.name,
        "first_prompt": summary.first_prompt,
        "created": summary.timestamp,
        "updated": summary.updated,
        "messages": summary.messages,
        "pinned": summary.pinned,
        "archived": summary.archived,
        "cwd": summary.cwd,
        "cost_usd": summary.usage.cost_usd,
    })
}

/// A session given as a path to its file or as an id prefix in `dirs`.
fn find(paths: &Paths, dirs: &[PathBuf], wanted: &str) -> Result<SessionSummary, String> {
    let file = paths.cwd.join(wanted);
    if file.is_file() {
        let dir = file.parent().unwrap_or(Path::new("."));
        return SessionStore::list(dir)
            .into_iter()
            .find(|s| canonical(&s.path) == canonical(&file))
            .ok_or_else(|| format!("{} is not a session file", file.display()));
    }
    let matches: Vec<SessionSummary> = dirs
        .iter()
        .flat_map(|dir| SessionStore::list(dir))
        .filter(|s| s.id.starts_with(wanted))
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().expect("one match")),
        0 => Err(format!("no session with id `{wanted}`")),
        n => Err(format!("`{wanted}` matches {n} sessions; give more of the id")),
    }
}

/// A duration such as `30d`, `12h` or `2w`.
pub fn parse_age(text: &str) -> Result<jiff::SignedDuration, String> {
    let text = text.trim();
    let split = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let n: i64 = number
        .parse()
        .map_err(|_| format!("`{text}`: expected a number and d, h or w, as in 30d"))?;
    let hours = match unit {
        "h" => n,
        "d" | "" => n * 24,
        "w" => n * 24 * 7,
        _ => return Err(format!("`{text}`: the unit is d (days), h (hours) or w (weeks)")),
    };
    Ok(jiff::SignedDuration::from_hours(hours))
}

fn confirm(question: &str, yes: bool) -> Result<bool, String> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err("nobody to confirm: pass --yes".to_string());
    }
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

pub async fn run(action: &SessionsAction, paths: &Paths) -> u8 {
    match action {
        SessionsAction::List {
            all_projects,
            all,
            json,
        } => {
            let sessions: Vec<SessionSummary> = session_dirs(paths, *all_projects)
                .iter()
                .flat_map(|dir| SessionStore::list(dir))
                .filter(|session| *all || !session.archived)
                .collect();
            for s in &sessions {
                if *json {
                    println!("{}", summary_json(s));
                } else {
                    let cost = s.usage.cost_usd.map(|c| format!("${c:.2}")).unwrap_or_default();
                    println!(
                        "{} {}  {:<60} {:>4} msgs {:>7}{}{}",
                        &s.id[..s.id.len().min(8)],
                        local_time(&s.updated),
                        label(s),
                        s.messages,
                        cost,
                        if s.pinned { "  pinned" } else { "" },
                        if s.archived { "  archived" } else { "" }
                    );
                }
            }
            if sessions.is_empty() && !json {
                eprintln!("no sessions");
            }
            EXIT_OK
        }
        SessionsAction::Search {
            query,
            all_projects,
            json,
        } => {
            let mut found = 0;
            for dir in session_dirs(paths, *all_projects) {
                for (summary, lines) in SessionStore::search(&dir, query, 3) {
                    found += 1;
                    for line in lines {
                        if *json {
                            println!(
                                "{}",
                                json!({ "id": summary.id, "path": summary.path, "line": line })
                            );
                        } else {
                            println!(
                                "{} · {} · {}",
                                local_time(&summary.updated),
                                label(&summary),
                                azazel_tui::text::truncate(&line, 100, "...")
                            );
                        }
                    }
                }
            }
            if found == 0 && !json {
                eprintln!("nothing matches `{query}`");
            }
            EXIT_OK
        }
        SessionsAction::Delete { session, yes } => {
            let dirs = session_dirs(paths, true);
            let summary = match find(paths, &dirs, session) {
                Ok(summary) => summary,
                Err(e) => {
                    eprintln!("azazel: {e}");
                    return EXIT_FAILED;
                }
            };
            match confirm(
                &format!("Delete session {} ({})?", summary.id, label(&summary)),
                *yes,
            ) {
                Ok(true) => {}
                Ok(false) => return EXIT_OK,
                Err(e) => {
                    eprintln!("azazel: {e}");
                    return EXIT_USAGE;
                }
            }
            match SessionStore::delete(&summary) {
                Ok(()) => {
                    println!("deleted {}", summary.path.display());
                    EXIT_OK
                }
                Err(e) => {
                    eprintln!("azazel: could not delete {}: {e}", summary.path.display());
                    EXIT_FAILED
                }
            }
        }
        SessionsAction::Prune {
            older_than,
            all_projects,
            dry_run,
        } => {
            let age = match parse_age(older_than) {
                Ok(age) => age,
                Err(e) => {
                    eprintln!("azazel: --older-than {e}");
                    return EXIT_USAGE;
                }
            };
            let cutoff = jiff::Timestamp::now() - age;
            let mut count = 0;
            for dir in session_dirs(paths, *all_projects) {
                for s in SessionStore::prune(&dir, cutoff, None, *dry_run) {
                    count += 1;
                    println!(
                        "{} {} {}  {}",
                        if *dry_run { "would delete" } else { "deleted" },
                        &s.id[..s.id.len().min(8)],
                        local_time(&s.updated),
                        label(&s)
                    );
                }
            }
            println!(
                "{count} session(s) {}; pinned sessions are kept",
                if *dry_run { "would be deleted" } else { "deleted" }
            );
            EXIT_OK
        }
        SessionsAction::Export {
            session,
            format,
            file,
            sanitize,
        } => export(paths, session.as_deref(), *format, file.as_deref(), *sanitize),
        SessionsAction::Import { source, project } => import(paths, source, project.as_deref()).await,
    }
}

/// The text of what `source` names: a file, or an `http(s)://` address.
async fn read_source(paths: &Paths, source: &str) -> Result<String, String> {
    if !(source.starts_with("https://") || source.starts_with("http://")) {
        let file = paths.cwd.join(source);
        return std::fs::read_to_string(&file).map_err(|e| format!("cannot read {}: {e}", file.display()));
    }
    let client = azazel_ai::tls::client_builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(source)
        .send()
        .await
        .map_err(|e| format!("cannot fetch {source}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("cannot fetch {source}: the server answered {status}"));
    }
    response
        .text()
        .await
        .map_err(|e| format!("cannot fetch {source}: {e}"))
}

/// `azazel sessions import <file|URL> [--project DIR]`: the entries of a session file, or of an
/// export as JSON lines, become a new session of the project. Nothing in them is changed.
pub async fn import(paths: &Paths, source: &str, project: Option<&Path>) -> u8 {
    let target = match project {
        Some(dir) => {
            let dir = paths.cwd.join(dir);
            if !dir.is_dir() {
                eprintln!("azazel: --project {} is not a directory", dir.display());
                return EXIT_USAGE;
            }
            let mut target = Paths::discover(&dir, Some(&paths.home.to_string_lossy()));
            target.sessions_override = paths.sessions_override.clone();
            target
        }
        None => paths.clone(),
    };
    let text = match read_source(paths, source).await {
        Ok(text) => text,
        Err(e) => {
            eprintln!("azazel: {e}");
            return EXIT_FAILED;
        }
    };
    let (header, entries, skipped) = parse_lines(&text);
    if entries.is_empty() {
        eprintln!(
            "azazel: {source} holds no session: it takes a session file, or the JSON lines `azazel export --format \
             jsonl` writes"
        );
        return EXIT_FAILED;
    }
    let count = entries.len();
    // It keeps the time it began, so it sorts among the others where it belongs.
    let created = header
        .map(|header| header.timestamp)
        .into_iter()
        .chain(entries.first().map(|entry| entry.timestamp.clone()))
        .find_map(|stamp| stamp.parse::<jiff::Timestamp>().ok())
        .unwrap_or_else(jiff::Timestamp::now);
    let store = match SessionStore::import(&target.sessions_dir(), &target.cwd, created, entries) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("azazel: could not write the session: {e}");
            return EXIT_FAILED;
        }
    };
    let id = store.id();
    println!(
        "imported {count} entries as session {} of {}",
        &id[..id.len().min(8)],
        target.project_root.display()
    );
    if skipped > 0 {
        println!("{skipped} line(s) were not entries of a session and were left out");
    }
    EXIT_OK
}

/// `tui.redact` as the configuration has it, for an export made outside a session. On unless
/// it is turned off; the project's own configuration counts when the project is trusted.
fn redact_exports(paths: &Paths) -> bool {
    let options = LoadOptions {
        trusted: TrustStore::new(paths.trust_file())
            .decision(&paths.project_root)
            .unwrap_or(false),
        overrides: toml::Table::new(),
        env: std::env::vars().collect(),
    };
    Config::load(paths, &options).map_or(true, |loaded| loaded.config.tui.redact)
}

/// The session a command is about: the one given as a path to its file or as the start of its
/// id, in any project; the latest session of this project when none is given.
pub(crate) fn pick(paths: &Paths, session: Option<&str>) -> Result<SessionSummary, String> {
    match session {
        Some(wanted) => find(paths, &session_dirs(paths, true), wanted),
        None => SessionStore::list(&paths.sessions_dir())
            .into_iter()
            .next()
            .ok_or_else(|| "this project has no session yet".to_string()),
    }
}

/// `azazel export [ID] [--format] [-o FILE] [--sanitize]`: the latest session of this project
/// when no id is given; the format from `--format`, else the file's extension, else Markdown.
pub fn export(
    paths: &Paths,
    session: Option<&str>,
    format: Option<Format>,
    output: Option<&Path>,
    sanitize: bool,
) -> u8 {
    let summary = pick(paths, session);
    let store = match summary.and_then(|s| SessionStore::open(&s.path).map_err(|e| e.to_string())) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("azazel: {e}");
            return EXIT_FAILED;
        }
    };
    if !has_messages(&store) {
        eprintln!("azazel: session {} has no messages", store.id());
        return EXIT_FAILED;
    }
    let format = format
        .or_else(|| output.and_then(Format::from_path))
        .unwrap_or(Format::Markdown);
    let text = match sanitize {
        true => render(&sanitized(&store), format, true),
        false => render(&store, format, redact_exports(paths)),
    };
    match output {
        Some(path) if path == Path::new("-") => {
            print!("{text}");
            EXIT_OK
        }
        Some(path) => write(&paths.cwd.join(path), &text),
        None => write(&paths.cwd.join(default_file_name(&store, format)), &text),
    }
}

fn write(path: &Path, text: &str) -> u8 {
    match std::fs::write(path, text) {
        Ok(()) => {
            println!("wrote {}", path.display());
            EXIT_OK
        }
        Err(e) => {
            eprintln!("azazel: could not write {}: {e}", path.display());
            EXIT_FAILED
        }
    }
}

/// With `session.retention_days` set, deletes this project's old unpinned sessions, at most once
/// a day (the stamp is `cache/prune.json`). `keep` (the open session) is never deleted. Runs on a
/// background thread; never fails the start.
pub fn prune_in_background(paths: &Paths, retention_days: u32, keep: String) {
    if retention_days == 0 {
        return;
    }
    let stamp = paths.cache_dir().join("prune.json");
    let today = jiff::Zoned::now().date().to_string();
    let last = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|v| v.get("date").and_then(|d| d.as_str()).map(str::to_string));
    if last.as_deref() == Some(today.as_str()) {
        return;
    }
    let dir = paths.sessions_dir();
    std::thread::spawn(move || {
        let cutoff =
            jiff::Timestamp::now() - jiff::SignedDuration::from_hours(i64::from(retention_days) * 24);
        let gone = SessionStore::prune(&dir, cutoff, Some(&keep), false);
        if !gone.is_empty() {
            tracing::info!(
                "retention: deleted {} session(s) older than {retention_days} days",
                gone.len()
            );
        }
        if let Some(parent) = stamp.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&stamp, json!({ "date": today }).to_string());
    });
}

#[cfg(test)]
mod tests {}
