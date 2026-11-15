//! Reading GitHub through the `gh` program: issues and pull requests as text for `read`
//! (`issue://12`, `pr://12`), as context for a `#12` in a prompt, and as rows for the pickers.
//! Nothing here writes to GitHub.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;

/// How long a `gh` call may take.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// Where `gh` is installed from, for the message of a feature that needs it.
pub const INSTALL: &str = "https://cli.github.com";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Pr,
    Issue,
}

impl Kind {
    /// The `gh` subcommand and the scheme of the kind.
    pub fn noun(self) -> &'static str {
        match self {
            Kind::Pr => "pr",
            Kind::Issue => "issue",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Pr => "pull request",
            Kind::Issue => "issue",
        }
    }
}

/// Runs `gh <args>` in `cwd` and returns what it printed. `env` is added to its environment.
pub async fn run(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
    timeout: Duration,
) -> Result<String, String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .env("GH_PAGER", "cat")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.envs(env.iter().map(|(k, v)| (k, v)));
    let shown = format!("gh {}", args.join(" "));
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| format!("`{shown}` did not finish within {} s", timeout.as_secs()))?
        .map_err(|e| format!("cannot run `{shown}`: {e}"))?;
    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr);
        let why = why.trim();
        return Err(if why.is_empty() {
            format!("`{shown}` failed ({})", output.status)
        } else {
            format!("`{shown}` failed: {why}")
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
}

const ISSUE_FIELDS: &str = "number,title,state,author,labels,body,comments,url,createdAt";
const PR_FIELDS: &str = "number,title,state,author,labels,body,comments,url,createdAt,headRefName,baseRefName,\
                         isDraft,reviewDecision,statusCheckRollup,files";

/// The `gh` arguments that print one issue or pull request as JSON.
pub fn view_arguments(kind: Kind, number: &str) -> Vec<String> {
    let fields = match kind {
        Kind::Pr => PR_FIELDS,
        Kind::Issue => ISSUE_FIELDS,
    };
    [kind.noun(), "view", number, "--json", fields]
        .iter()
        .map(|word| word.to_string())
        .collect()
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn login(value: &Value) -> &str {
    value
        .get("author")
        .and_then(|author| author.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("ghost")
}

fn day(stamp: &str) -> &str {
    stamp.split('T').next().unwrap_or(stamp)
}

fn label_names(value: &Value) -> Vec<&str> {
    value
        .get("labels")
        .and_then(Value::as_array)
        .map(|labels| {
            labels
                .iter()
                .filter_map(|l| l.get("name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default()
}

/// A one-line account of the checks of a pull request: how many passed, failed or are still
/// running, with the names of the ones that failed.
fn checks_summary(rollup: &[Value]) -> String {
    let (mut passed, mut pending) = (0, 0);
    let mut failed: Vec<&str> = Vec::new();
    for check in rollup {
        let name = ["name", "context"]
            .iter()
            .map(|key| text(check, key))
            .find(|name| !name.is_empty())
            .unwrap_or("check");
        // A check run has a status and a conclusion, a commit status only a state.
        let outcome = ["conclusion", "state"]
            .iter()
            .map(|key| text(check, key))
            .find(|word| !word.is_empty())
            .unwrap_or_default()
            .to_ascii_uppercase();
        match outcome.as_str() {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => passed += 1,
            "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => {
                failed.push(name)
            }
            _ => pending += 1,
        }
    }
    let mut line = format!("{passed} passed, {} failed, {pending} pending", failed.len());
    if !failed.is_empty() {
        line.push_str(&format!(" ({})", failed.join(", ")));
    }
    line
}

/// An issue or pull request, from the JSON of [`view_arguments`], as text to read.
pub fn render_view(kind: Kind, json: &str) -> Result<String, String> {
    let value: Value =
        serde_json::from_str(json).map_err(|e| format!("gh printed something unexpected: {e}"))?;
    let number = value.get("number").and_then(Value::as_u64).unwrap_or_default();
    let mut out = format!(
        "#{number} {} [{}]\n",
        text(&value, "title"),
        text(&value, "state")
    );
    let url = text(&value, "url");
    if !url.is_empty() {
        out.push_str(&format!("{url}\n"));
    }
    out.push_str(&format!(
        "author: {}, opened {}\n",
        login(&value),
        day(text(&value, "createdAt"))
    ));
    let labels = label_names(&value);
    if !labels.is_empty() {
        out.push_str(&format!("labels: {}\n", labels.join(", ")));
    }
    if kind == Kind::Pr {
        let mut state = format!(
            "branch: {} into {}",
            text(&value, "headRefName"),
            text(&value, "baseRefName")
        );
        if value.get("isDraft").and_then(Value::as_bool) == Some(true) {
            state.push_str(", draft");
        }
        let review = text(&value, "reviewDecision");
        if !review.is_empty() {
            state.push_str(&format!(", review: {review}"));
        }
        out.push_str(&format!("{state}\n"));
        let rollup = value.get("statusCheckRollup").and_then(Value::as_array);
        match rollup {
            Some(checks) if !checks.is_empty() => {
                out.push_str(&format!("checks: {}\n", checks_summary(checks)));
            }
            _ => out.push_str("checks: none\n"),
        }
        let files = value
            .get("files")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        out.push_str(&format!("changed files ({}):\n", files.len()));
        for file in files {
            let count = |key| file.get(key).and_then(Value::as_u64).unwrap_or_default();
            out.push_str(&format!(
                "  {} +{} -{}\n",
                text(file, "path"),
                count("additions"),
                count("deletions")
            ));
        }
    }
    let body = text(&value, "body").trim();
    out.push('\n');
    out.push_str(if body.is_empty() { "(no description)" } else { body });
    let comments = value
        .get("comments")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if !comments.is_empty() {
        out.push_str(&format!("\n\ncomments ({}):", comments.len()));
        for comment in comments {
            out.push_str(&format!(
                "\n\n{}, {}:\n{}",
                login(comment),
                day(text(comment, "createdAt")),
                text(comment, "body").trim()
            ));
        }
    }
    Ok(out)
}

/// Which issues or pull requests a listing holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Open,
    Closed,
    All,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Open => "open",
            State::Closed => "closed",
            State::All => "all",
        }
    }

    /// The next one in the order `Tab` steps through.
    pub fn next(self) -> State {
        match self {
            State::Open => State::Closed,
            State::Closed => State::All,
            State::All => State::Open,
        }
    }
}

/// How many rows a picker asks for.
pub const PICKER_LIMIT: usize = 50;

/// The `gh` arguments that list issues or pull requests as JSON.
pub fn list_arguments(kind: Kind, state: State) -> Vec<String> {
    [
        kind.noun(),
        "list",
        "--state",
        state.word(),
        "--limit",
        &PICKER_LIMIT.to_string(),
        "--json",
        "number,title,author,labels,createdAt,state",
    ]
    .iter()
    .map(|word| word.to_string())
    .collect()
}

/// One row of a picker.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Item {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub labels: Vec<String>,
    pub state: String,
    /// The day it was opened, `2026-11-02`.
    pub opened: String,
}

/// The rows of the JSON of [`list_arguments`].
pub fn parse_list(json: &str) -> Result<Vec<Item>, String> {
    let rows: Vec<Value> =
        serde_json::from_str(json).map_err(|e| format!("gh printed something unexpected: {e}"))?;
    Ok(rows
        .iter()
        .map(|row| Item {
            number: row.get("number").and_then(Value::as_u64).unwrap_or_default(),
            title: text(row, "title").to_string(),
            author: login(row).to_string(),
            labels: label_names(row).iter().map(|l| l.to_string()).collect(),
            state: text(row, "state").to_string(),
            opened: day(text(row, "createdAt")).to_string(),
        })
        .collect())
}

/// What `#<n>` in a prompt can mean: the numbers of a prompt that are whole words made of
/// digits, in order, once each.
pub fn references(prompt: &str) -> Vec<u64> {
    let mut found: Vec<u64> = Vec::new();
    let chars: Vec<(usize, char)> = prompt.char_indices().collect();
    for (at, &(_, c)) in chars.iter().enumerate() {
        if c != '#' || (at > 0 && !chars[at - 1].1.is_whitespace() && !"([\"'".contains(chars[at - 1].1)) {
            continue;
        }
        let digits: String = chars[at + 1..]
            .iter()
            .map(|&(_, c)| c)
            .take_while(char::is_ascii_digit)
            .collect();
        let after = chars.get(at + 1 + digits.len()).map(|&(_, c)| c);
        let ends = after.is_none_or(|c| c.is_whitespace() || ",.;:!?)]\"'".contains(c));
        if digits.is_empty() || digits.len() > 9 || !ends {
            continue;
        }
        if let Ok(number) = digits.parse::<u64>()
            && number > 0
            && !found.contains(&number)
        {
            found.push(number);
        }
    }
    found
}

/// Issue or pull request `number`, as text, whichever it is; `None` when the repository of
/// `cwd` has neither (or is not on GitHub, or `gh` is not signed in).
pub async fn resolve(
    program: &Path,
    cwd: &Path,
    number: u64,
    env: &[(String, String)],
) -> Option<(Kind, String)> {
    for kind in [Kind::Issue, Kind::Pr] {
        let args = view_arguments(kind, &number.to_string());
        if let Ok(json) = run(program, &args, cwd, env, TIMEOUT).await
            && let Ok(text) = render_view(kind, &json)
        {
            return Some((kind, text));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_whole_word_numbers_are_references() {
        assert_eq!(references("fix #12 and (#7), see #12 again."), [12, 7]);
        for plain in ["issue#12", "#12abc", "#", "#0", "C# and #x", "a#1", "&#39;"] {
            assert!(references(plain).is_empty(), "{plain}");
        }
        assert_eq!(references("#3"), [3]);
    }

    #[test]
    fn a_pull_request_shows_its_branch_checks_and_files() {
        let json = r##"{"number":4,"title":"Add x","state":"OPEN","author":{"login":"ann"},
            "labels":[{"name":"bug"},{"name":"p1"}],"body":"Does x.","createdAt":"2026-11-02T10:00:00Z",
            "url":"https://github.com/o/r/pull/4","headRefName":"x","baseRefName":"main","isDraft":false,
            "reviewDecision":"APPROVED",
            "statusCheckRollup":[{"name":"build","conclusion":"SUCCESS"},{"name":"lint","conclusion":"FAILURE"},
                {"context":"ci/x","state":"PENDING"}],
            "files":[{"path":"a.rs","additions":3,"deletions":1}],
            "comments":[{"author":{"login":"bob"},"body":"Nice.","createdAt":"2026-11-03T09:00:00Z"}]}"##;
        let shown = render_view(Kind::Pr, json).unwrap();
        for wanted in [
            "#4 Add x [OPEN]",
            "labels: bug, p1",
            "branch: x into main, review: APPROVED",
            "checks: 1 passed, 1 failed, 1 pending (lint)",
            "a.rs +3 -1",
            "Does x.",
            "bob, 2026-11-03:\nNice.",
        ] {
            assert!(shown.contains(wanted), "{wanted} not in\n{shown}");
        }
        assert!(!render_view(Kind::Issue, json).unwrap().contains("branch:"));
    }
}
