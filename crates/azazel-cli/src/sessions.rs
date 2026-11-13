//! `azazel sessions ...` and `azazel export`: the sessions of this project (or of every project)
//! from the shell, and the retention prune that runs at start.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use azazel_core::paths::canonical;
use azazel_core::session::SessionSummary;
use azazel_core::{Config, LoadOptions, Paths, SessionStore, TrustStore};
use serde_json::json;

use crate::cli::SessionsAction;
use crate::export::{Format, default_file_name, has_messages, render};
use crate::print::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};

/// The session directories to look in: this project's, or every project's.
fn session_dirs(paths: &Paths, all_projects: bool) -> Vec<PathBuf> {
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

pub fn run(action: &SessionsAction, paths: &Paths) -> u8 {
    match action {
        SessionsAction::List { all_projects, json } => {
            let sessions: Vec<SessionSummary> = session_dirs(paths, *all_projects)
                .iter()
                .flat_map(|dir| SessionStore::list(dir))
                .collect();
            for s in &sessions {
                if *json {
                    println!("{}", summary_json(s));
                } else {
                    let cost = s.usage.cost_usd.map(|c| format!("${c:.2}")).unwrap_or_default();
                    println!(
                        "{} {}  {:<60} {:>4} msgs {:>7}{}",
                        &s.id[..s.id.len().min(8)],
                        local_time(&s.updated),
                        label(s),
                        s.messages,
                        cost,
                        if s.pinned { "  pinned" } else { "" }
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
        } => export(paths, session.as_deref(), *format, file.as_deref()),
    }
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

/// `azazel export [ID] [--format] [-o FILE]`: the latest session of this project when no id is
/// given; the format from `--format`, else the file's extension, else Markdown.
pub fn export(paths: &Paths, session: Option<&str>, format: Option<Format>, output: Option<&Path>) -> u8 {
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
    let text = render(&store, format, redact_exports(paths));
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
mod tests {
    use super::*;

    #[test]
    fn ages_parse_in_hours_days_and_weeks() {
        assert_eq!(parse_age("12h").unwrap().as_hours(), 12);
        assert_eq!(parse_age("30d").unwrap().as_hours(), 720);
        assert_eq!(parse_age("2w").unwrap().as_hours(), 336);
        assert_eq!(parse_age("7").unwrap().as_hours(), 168);
        assert!(parse_age("d").is_err());
        assert!(parse_age("3y").unwrap_err().contains("unit"));
    }
}
