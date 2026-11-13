//! The secret scan: `gitleaks` over the project, once per session and in the background. What it
//! finds joins the report the lead gets when it stops.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use azazel_core::config::CheckReport;
use serde_json::Value;
use tokio::task::JoinHandle;

/// How long the scan may take.
const TIMEOUT: Duration = Duration::from_secs(120);
/// Secrets kept from one scan.
const MAX_LEAKS: usize = 20;

/// A secret in the project: where it is and the rule that matched, never the secret.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Leak {
    pub file: String,
    pub line: u32,
    pub rule: String,
}

/// The program that scans the project for secrets, or why this session has no scan. `secrets` is
/// `tools.scan.secrets`, `report` is `tools.check_report` and `trusted` is whether the project is.
pub fn scanner(secrets: bool, report: CheckReport, trusted: bool) -> Result<PathBuf, &'static str> {
    if !secrets {
        return Err("off");
    }
    if report == CheckReport::Off {
        return Err("off, because `tools.check_report` is off and nothing would show what it found");
    }
    if !trusted {
        return Err("off, the project is not trusted");
    }
    which::which("gitleaks").map_err(|_| "off, gitleaks was not found on PATH")
}

#[derive(Default)]
struct State {
    started: bool,
    task: Option<JoinHandle<()>>,
    found: Vec<Leak>,
}

pub struct SecretScan {
    program: PathBuf,
    root: PathBuf,
    state: Mutex<State>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The findings in a gitleaks JSON report, sorted, each place once. A file inside `root` is
/// named from it.
fn parse(report: &str, root: &Path) -> Vec<Leak> {
    let report: Value = serde_json::from_str(report).unwrap_or_default();
    let mut leaks: Vec<Leak> = report
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let file = Path::new(item["File"].as_str()?);
            let file = file.strip_prefix(root).unwrap_or(file);
            Some(Leak {
                file: file.to_string_lossy().replace('\\', "/"),
                line: item["StartLine"].as_u64().and_then(|n| u32::try_from(n).ok())?,
                rule: item["RuleID"].as_str()?.to_string(),
            })
        })
        .collect();
    leaks.sort();
    leaks.dedup();
    leaks.truncate(MAX_LEAKS);
    leaks
}

/// Runs `program` over `root` and returns what it found.
async fn scan(program: &Path, root: &Path) -> Result<Vec<Leak>, String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let report = std::env::temp_dir().join(format!("azazel-gitleaks-{}-{stamp:x}.json", std::process::id()));
    let mut command = tokio::process::Command::new(program);
    command.args([
        "detect",
        "--no-banner",
        "--report-format",
        "json",
        "--report-path",
    ]);
    command.arg(&report).arg("--source").arg(root);
    // Without a repository there is no history to read; the files themselves are scanned.
    if !root.join(".git").exists() {
        command.arg("--no-git");
    }
    command
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let finished = tokio::time::timeout(TIMEOUT, command.status()).await;
    let written = std::fs::read_to_string(&report);
    let _ = std::fs::remove_file(&report);
    match finished {
        Err(_) => Err(format!("it did not finish within {} s", TIMEOUT.as_secs())),
        Ok(Err(e)) => Err(format!("it could not be started: {e}")),
        // 0: nothing found; 1: secrets found.
        Ok(Ok(status)) if matches!(status.code(), Some(0 | 1)) => {
            Ok(written.map(|report| parse(&report, root)).unwrap_or_default())
        }
        Ok(Ok(status)) => Err(format!("it failed ({status})")),
    }
}

impl SecretScan {
    pub fn new(program: PathBuf, root: PathBuf) -> SecretScan {
        SecretScan {
            program,
            root,
            state: Mutex::default(),
        }
    }

    /// Starts the scan the first time it is called.
    pub fn start(self: &Arc<Self>) {
        let mut state = locked(&self.state);
        if std::mem::replace(&mut state.started, true) {
            return;
        }
        let this = self.clone();
        state.task = Some(tokio::spawn(async move {
            match scan(&this.program, &this.root).await {
                Ok(leaks) => locked(&this.state).found = leaks,
                Err(why) => tracing::warn!("the secret scan found nothing: {why}"),
            }
        }));
    }

    /// What a finished scan found. Each finding is returned once.
    pub fn take(&self) -> Vec<Leak> {
        std::mem::take(&mut locked(&self.state).found)
    }

    /// Ends a scan that is still running.
    pub fn stop(&self) {
        if let Some(task) = locked(&self.state).task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_becomes_places_and_rules_without_the_secret() {
        let root = Path::new("/work/project");
        let report = r#"[
            {"RuleID": "generic-api-key", "File": "config/dev.env", "StartLine": 3, "Secret": "hunter2hunter2", "Match": "KEY=hunter2hunter2"},
            {"RuleID": "aws-access-token", "File": "/work/project/a/keys.txt", "StartLine": 1, "Secret": "AKIAIOSFODNN7EXAMPLE"},
            {"RuleID": "generic-api-key", "File": "config/dev.env", "StartLine": 3, "Secret": "hunter2hunter2"},
            {"RuleID": "no-line", "File": "x"}
        ]"#;
        let leaks = parse(report, root);
        let leak = |file: &str, line, rule: &str| Leak {
            file: file.to_string(),
            line,
            rule: rule.to_string(),
        };
        assert_eq!(
            leaks,
            [
                leak("a/keys.txt", 1, "aws-access-token"),
                leak("config/dev.env", 3, "generic-api-key")
            ]
        );
        assert!(!format!("{leaks:?}").contains("hunter2"));
        assert!(parse("not json", root).is_empty());
        assert!(parse("{}", root).is_empty());
    }
}
