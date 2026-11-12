//! What you typed in a project, kept across its sessions in `projects/<slug>/history.jsonl`:
//! one `{ts, session, text}` object per line, oldest first. The editor's up and down keys and
//! `ctrl+r` read it.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use azazel_core::Paths;
use serde::{Deserialize, Serialize};

/// The file is cut back to [`KEEP`] lines once it holds more than this many.
const TRIM_ABOVE: usize = 6000;
const KEEP: usize = 5000;
/// A file smaller than this cannot hold [`TRIM_ABOVE`] lines, so its lines are not counted.
const COUNT_FROM_BYTES: u64 = 256 * 1024;
/// How many of the newest entries the editor starts with.
pub const SEED: usize = 1000;

/// One line of the history file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// When it was sent.
    pub ts: String,
    /// Id of the session it was sent in.
    pub session: String,
    /// The prompt as typed; a shell line keeps its `!`.
    pub text: String,
}

pub fn file(paths: &Paths) -> PathBuf {
    paths.project_data.join("history.jsonl")
}

/// Adds one entry, and cuts the file back to its newest [`KEEP`] lines when it has grown past
/// [`TRIM_ABOVE`].
pub fn append(file: &Path, session: &str, text: &str) -> io::Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let record = Record {
        ts: jiff::Timestamp::now().to_string(),
        session: session.to_string(),
        text: text.to_string(),
    };
    let mut out = std::fs::File::options().create(true).append(true).open(file)?;
    writeln!(out, "{}", serde_json::to_string(&record)?)?;
    drop(out);
    if std::fs::metadata(file)?.len() >= COUNT_FROM_BYTES {
        trim(file, TRIM_ABOVE, KEEP)?;
    }
    Ok(())
}

/// Keeps the newest `keep` lines when the file holds more than `above`.
fn trim(file: &Path, above: usize, keep: usize) -> io::Result<()> {
    let text = std::fs::read_to_string(file)?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= above {
        return Ok(());
    }
    let mut kept = lines[lines.len() - keep..].join("\n");
    kept.push('\n');
    // Replaced in one step, so that a second azazel in this project never reads half a file.
    let temp = file.with_extension(format!("jsonl.{}.tmp", std::process::id()));
    std::fs::write(&temp, kept)?;
    std::fs::rename(&temp, file).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// Every entry, oldest first. Lines that do not parse are skipped.
pub fn load(file: &Path) -> Vec<Record> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The newest entries first, each text once (its newest use), at most `limit`.
pub fn newest_first(records: Vec<Record>, limit: usize) -> Vec<Record> {
    let mut seen = std::collections::HashSet::new();
    records
        .into_iter()
        .rev()
        .filter(|record| seen.insert(record.text.clone()))
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_appended_and_read_back_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("project/history.jsonl");
        assert!(load(&file).is_empty());
        append(&file, "s1", "fix the parser").unwrap();
        append(&file, "s1", "!cargo test").unwrap();
        append(&file, "s2", "two\nlines").unwrap();
        append(&file, "s2", "   ").unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();
        append(&file, "s2", "fix the parser").unwrap();

        let records = load(&file);
        let texts: Vec<&str> = records.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(
            texts,
            ["fix the parser", "!cargo test", "two\nlines", "fix the parser"]
        );
        assert_eq!(records[2].session, "s2");
        assert!(records[0].ts.parse::<jiff::Timestamp>().is_ok());

        // Newest first, and a prompt sent twice is listed once, where it was last used.
        let newest = newest_first(records, 10);
        let texts: Vec<(&str, &str)> = newest
            .iter()
            .map(|r| (r.text.as_str(), r.session.as_str()))
            .collect();
        assert_eq!(
            texts,
            [
                ("fix the parser", "s2"),
                ("two\nlines", "s2"),
                ("!cargo test", "s1")
            ]
        );
        assert_eq!(newest_first(load(&file), 2).len(), 2);
    }

    #[test]
    fn a_long_file_is_cut_back_to_its_newest_lines() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("history.jsonl");
        for n in 0..12 {
            append(&file, "s", &format!("prompt {n}")).unwrap();
        }
        // Not past the mark: nothing is cut.
        trim(&file, 12, 5).unwrap();
        assert_eq!(load(&file).len(), 12);
        trim(&file, 10, 5).unwrap();
        let kept: Vec<String> = load(&file).into_iter().map(|r| r.text).collect();
        assert_eq!(
            kept,
            ["prompt 7", "prompt 8", "prompt 9", "prompt 10", "prompt 11"]
        );
        // The file still takes new entries after a cut.
        append(&file, "s", "prompt 12").unwrap();
        assert_eq!(load(&file).len(), 6);
    }
}
