//! What decides, at each due time of a schedule and before any model is asked, whether its
//! prompt runs: the `if` and `until` checks (shell commands judged by their exit code, what they
//! print and whether that changed), and the end of the schedule (a time, a number of runs).

use std::path::Path;
use std::time::Duration;

use azazel_core::process::{self, Shell};
use jiff::{Timestamp, Zoned};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use super::stamp;

/// How long a check may run before it counts as failed.
const CHECK_LIMIT: Duration = Duration::from_secs(60);
/// How much of what a check prints is looked at.
const OUTPUT_LIMIT: u64 = 1 << 20;
/// How long what a check printed is still read once it has exited: a program it left running
/// could hold the pipe open for ever.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(500);

fn is_false(value: &bool) -> bool {
    !*value
}

/// A shell command whose result says whether a schedule goes ahead (`if`) or stops (`until`).
/// It holds when every condition given holds: the exit code is `is` (0 when not given), what it
/// prints matches `matches`, and, with `changes`, it printed something else than the time
/// before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub run: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub changes: bool,
    /// For `changes`: a digest of what it printed the last time it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<String>,
}

impl Check {
    pub fn new(run: &str, is: Option<i32>, matches: Option<&str>, changes: bool) -> Result<Check, String> {
        let run = run.trim();
        if run.is_empty() {
            return Err("a check needs a command to run".to_string());
        }
        let matches = matches.map(str::to_string).filter(|text| !text.is_empty());
        if let Some(pattern) = &matches {
            pattern_of(pattern).map_err(|e| format!("`{pattern}` is not a regular expression: {e}"))?;
        }
        Ok(Check {
            run: run.to_string(),
            is,
            matches,
            changes,
            seen: None,
        })
    }

    /// Reads a check as the `schedule` tool is given it: `{run, is?, matches?, changes?}`.
    pub fn from_json(value: &serde_json::Value, name: &str) -> Result<Check, String> {
        let run = value
            .get("run")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("`{name}` needs `run`: the command to run"))?;
        let is = match value.get("is") {
            None | Some(serde_json::Value::Null) => None,
            Some(code) => Some(
                code.as_i64()
                    .and_then(|code| i32::try_from(code).ok())
                    .ok_or_else(|| format!("`{name}.is` is an exit code"))?,
            ),
        };
        let matches = value.get("matches").and_then(serde_json::Value::as_str);
        let changes = value
            .get("changes")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        Check::new(run, is, matches, changes)
    }
}

/// When a schedule stops by itself: at a moment, or after so many runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ends {
    #[serde(default, with = "stamp::optional", skip_serializing_if = "Option::is_none")]
    pub at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub times: Option<u32>,
}

impl Ends {
    /// `after` (a length of time from `now`), `at` (a moment, as `at` takes it) and `times` (a
    /// number of runs); `None` when none is given. With both `after` and `at`, the earlier counts.
    pub fn parse(
        after: Option<&str>,
        at: Option<&str>,
        times: Option<u32>,
        now: &Zoned,
    ) -> Result<Option<Ends>, String> {
        let after = after
            .filter(|text| !text.trim().is_empty())
            .map(|text| {
                let length =
                    jiff::SignedDuration::try_from(crate::loops::span(text)?).map_err(|e| e.to_string())?;
                now.timestamp().checked_add(length).map_err(|e| e.to_string())
            })
            .transpose()?;
        let at = at
            .filter(|text| !text.trim().is_empty())
            .map(|text| super::when::moment(text, now))
            .transpose()?;
        if times == Some(0) {
            return Err("`times` is how many runs: 1 at least".to_string());
        }
        let at = match (after, at) {
            (Some(after), Some(at)) => Some(after.min(at)),
            (one, other) => one.or(other),
        };
        Ok((at.is_some() || times.is_some()).then_some(Ends { at, times }))
    }
}

/// What running a check came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judged {
    pub held: bool,
    /// Why, in a few words: `exit 0`, `exit 1, not 0`, `printed the same as before`.
    pub why: String,
    /// The digest to keep for the next `changes`.
    pub seen: Option<String>,
}

/// `matches` as a regular expression whose `^` and `$` are the ends of a line.
fn pattern_of(pattern: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern).multi_line(true).build()
}

fn digest(output: &[u8]) -> String {
    Sha256::digest(output)
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Whether `check` holds for a command that exited with `code` (`None`: it was killed) and
/// printed `stdout`.
pub fn judge(check: &Check, code: Option<i32>, stdout: &[u8]) -> Judged {
    let seen = check.changes.then(|| digest(stdout));
    let fail = |why: String| Judged {
        held: false,
        why,
        seen: seen.clone(),
    };
    let wanted = check.is.unwrap_or(0);
    match code {
        Some(code) if code == wanted => {}
        Some(code) => return fail(format!("exit {code}, not {wanted}")),
        None => return fail("it was killed".to_string()),
    }
    if let Some(pattern) = &check.matches {
        let text = String::from_utf8_lossy(stdout);
        let found = pattern_of(pattern).is_ok_and(|re| re.is_match(&text));
        if !found {
            return fail(format!("printed nothing that matches `{pattern}`"));
        }
    }
    if check.changes {
        match &check.seen {
            None => return fail("first run: nothing to compare with yet".to_string()),
            Some(before) if Some(before) == seen.as_ref() => {
                return fail("printed the same as the time before".to_string());
            }
            Some(_) => {}
        }
    }
    let mut why = format!("exit {wanted}");
    if check.matches.is_some() {
        why.push_str(", matched");
    }
    if check.changes {
        why.push_str(", changed");
    }
    Judged {
        held: true,
        why,
        seen,
    }
}

/// Runs `command` with `shell` in `cwd`: its exit code (`None` when it was killed) and what it
/// printed on standard output.
async fn run(shell: &Shell, cwd: &Path, command: &str) -> Result<(Option<i32>, Vec<u8>), String> {
    let mut child = process::spawn(
        shell,
        command,
        cwd,
        &[],
        false,
        azazel_core::config::ProcessPriority::Normal,
    )
    .map_err(|e| format!("cannot run it: {e}"))?;
    let stdout = child.stdout().take();
    let stderr = child.stderr().take();
    let reader = tokio::spawn(async move {
        let mut out = Vec::new();
        if let Some(stdout) = stdout {
            let _ = stdout.take(OUTPUT_LIMIT).read_to_end(&mut out).await;
        }
        out
    });
    // Read and thrown away, so that a command that says much there is not stopped by a full pipe.
    tokio::spawn(async move {
        if let Some(mut stderr) = stderr {
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        }
    });
    let waited = tokio::time::timeout(CHECK_LIMIT, child.wait()).await;
    let status = match waited {
        Ok(status) => status.map_err(|e| e.to_string())?,
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(format!("it took longer than {}s", CHECK_LIMIT.as_secs()));
        }
    };
    let out = tokio::time::timeout(DRAIN_AFTER_EXIT, reader)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    Ok((status.code(), out))
}

/// Runs `check` and judges it. A command that cannot be run, or runs too long, does not hold.
pub async fn evaluate(shell: &Shell, cwd: &Path, check: &Check) -> Judged {
    match run(shell, cwd, &check.run).await {
        Ok((code, stdout)) => judge(check, code, &stdout),
        Err(why) => Judged {
            held: false,
            why,
            // A failure says nothing about what it prints: the next run compares with the last
            // output there was.
            seen: check.seen.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn check(is: Option<i32>, matches: Option<&str>, changes: bool) -> Check {
        Check::new("probe", is, matches, changes).unwrap()
    }

    #[test]
    fn a_check_holds_on_its_exit_code_and_what_it_prints() {
        let plain = check(None, None, false);
        assert!(judge(&plain, Some(0), b"").held);
        let failed = judge(&plain, Some(1), b"");
        assert_eq!((failed.held, failed.why.as_str()), (false, "exit 1, not 0"));
        assert!(!judge(&plain, None, b"").held);

        let three = check(Some(3), None, false);
        assert!(judge(&three, Some(3), b"").held && !judge(&three, Some(0), b"").held);

        let ready = check(None, Some(r"^status: (ready|done)$"), false);
        assert!(!judge(&ready, Some(0), b"status: waiting\n").held);
        // `^` and `$` are the ends of a line.
        assert!(judge(&ready, Some(0), b"build 12\nstatus: done\n").held);
        assert!(judge(&ready, Some(0), b"status: ready").held);
        // The exit code counts too.
        assert!(!judge(&ready, Some(2), b"status: ready").held);
    }

    #[test]
    fn a_change_is_counted_from_the_second_run_on() {
        let mut watch = check(None, None, true);
        let first = judge(&watch, Some(0), b"v1\n");
        assert!(!first.held, "{first:?}");
        watch.seen = first.seen;
        let same = judge(&watch, Some(0), b"v1\n");
        assert_eq!(
            (same.held, same.why.as_str()),
            (false, "printed the same as the time before")
        );
        watch.seen = same.seen;
        let moved = judge(&watch, Some(0), b"v2\n");
        assert_eq!((moved.held, moved.why.as_str()), (true, "exit 0, changed"));
        assert_ne!(moved.seen, watch.seen);
    }

    #[test]
    fn what_is_no_check_or_no_end_is_refused() {
        assert!(
            Check::new("  ", None, None, false)
                .unwrap_err()
                .contains("needs a command")
        );
        assert!(
            Check::new("true", None, Some("(unclosed"), false)
                .unwrap_err()
                .contains("not a regular expression")
        );
        let tool = serde_json::json!({ "run": "test -f done", "is": 1, "matches": "x", "changes": true });
        assert_eq!(
            Check::from_json(&tool, "if").unwrap(),
            Check {
                run: "test -f done".into(),
                is: Some(1),
                matches: Some("x".into()),
                changes: true,
                seen: None
            }
        );
        assert!(
            Check::from_json(&serde_json::json!({ "is": 0 }), "until")
                .unwrap_err()
                .contains("`until` needs `run`")
        );

        let now: Zoned = "2026-11-13T10:00:00+00:00[UTC]".parse().unwrap();
        assert_eq!(Ends::parse(None, None, None, &now), Ok(None));
        assert!(
            Ends::parse(None, None, Some(0), &now)
                .unwrap_err()
                .contains("1 at least")
        );
        // The earlier of the two moments.
        let ends = Ends::parse(Some("2h"), Some("2026-11-13 11:00"), Some(4), &now)
            .unwrap()
            .unwrap();
        assert_eq!(ends.at, Some("2026-11-13T11:00:00Z".parse().unwrap()));
        assert_eq!(ends.times, Some(4));
        let ends = Ends::parse(Some("1d"), None, None, &now).unwrap().unwrap();
        assert_eq!(ends.at, Some("2026-11-14T10:00:00Z".parse().unwrap()));
        assert!(
            Ends::parse(None, Some("2020-01-01 00:00"), None, &now)
                .unwrap_err()
                .contains("in the past")
        );
    }
}
