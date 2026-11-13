//! Stands in for the programs schedules talk to, under whichever name it is copied to:
//!
//! - `schtasks`: every call is appended to `$FAKE_SCHED_DIR/calls.jsonl` as `{"args": […],
//!   "xml": "<the task, when one is created>"}`; `/Query` succeeds for a task that was created
//!   and not deleted since.
//! - `crontab`: `-l` prints `$FAKE_SCHED_DIR/crontab`, `-` replaces it with standard input.
//! - `launchctl`: every call is appended to `calls.jsonl`.
//! - `gh`: prints `$FAKE_SCHED_DIR/runs.json` (the answer to `gh run list --json …`).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value, json};

fn note(dir: &Path, call: &Value) {
    let noted = std::fs::File::options()
        .create(true)
        .append(true)
        .open(dir.join("calls.jsonl"))
        .and_then(|mut file| writeln!(file, "{call}"));
    if let Err(e) = noted {
        eprintln!("fake-sched: {e}");
    }
}

/// A task file as Task Scheduler reads it: UTF-16 with a byte order mark.
fn task_text(path: &str) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16_lossy(&units)
        .trim_start_matches('\u{feff}')
        .to_string()
}

fn after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let at = args.iter().position(|arg| arg.eq_ignore_ascii_case(flag))?;
    args.get(at + 1).map(String::as_str)
}

fn schtasks(dir: &Path, args: &[String]) -> ExitCode {
    let has = |flag: &str| args.iter().any(|arg| arg.eq_ignore_ascii_case(flag));
    if has("/Query") {
        let name = after(args, "/TN").unwrap_or_default();
        let calls = std::fs::read_to_string(dir.join("calls.jsonl")).unwrap_or_default();
        let mut known = false;
        for call in calls
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        {
            let words: Vec<&str> = call["args"]
                .as_array()
                .map(|args| args.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let named = words.windows(2).any(|pair| pair[0] == "/TN" && pair[1] == name);
            if named && words.contains(&"/Create") {
                known = true;
            }
            if named && words.contains(&"/Delete") {
                known = false;
            }
        }
        return match known {
            true => ExitCode::SUCCESS,
            false => {
                eprintln!("ERROR: The system cannot find the file specified.");
                ExitCode::FAILURE
            }
        };
    }
    let xml = after(args, "/XML").map(task_text);
    note(dir, &json!({ "args": args, "xml": xml }));
    println!("SUCCESS");
    ExitCode::SUCCESS
}

fn crontab(dir: &Path, args: &[String]) -> ExitCode {
    let table = dir.join("crontab");
    match args.first().map(String::as_str) {
        Some("-l") => match std::fs::read_to_string(&table) {
            Ok(text) => {
                print!("{text}");
                ExitCode::SUCCESS
            }
            Err(_) => {
                eprintln!("no crontab for this user");
                ExitCode::FAILURE
            }
        },
        Some("-") => {
            let mut text = String::new();
            let read = std::io::stdin().read_to_string(&mut text);
            match read.and_then(|_| std::fs::write(&table, text)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("crontab: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("usage: crontab -l | -");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let program = std::env::current_exe().unwrap_or_default();
    let name = program
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let dir = PathBuf::from(std::env::var_os("FAKE_SCHED_DIR").unwrap_or_default());
    let args: Vec<String> = std::env::args().skip(1).collect();
    match name.as_str() {
        "schtasks" => schtasks(&dir, &args),
        "crontab" => crontab(&dir, &args),
        "gh" => {
            let runs = std::fs::read_to_string(dir.join("runs.json")).unwrap_or_else(|_| "[]".to_string());
            println!("{runs}");
            ExitCode::SUCCESS
        }
        _ => {
            note(&dir, &json!({ "args": args }));
            ExitCode::SUCCESS
        }
    }
}
