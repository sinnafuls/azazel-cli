//! What an agent is doing, from the harness's own records: no model is asked. Read by the
//! `agent_status` tool, `agent://<id>/status` and the agent hub.

use std::path::PathBuf;
use std::time::Duration;

use azazel_ai::Usage;
use serde_json::Value;

use crate::routing::Routing;
use crate::team::MemberState;

/// Lines of what the agent said that a status shows.
const TEXT_LINES: usize = 5;
/// Characters of one such line.
const LINE_CHARS: usize = 200;

/// A tool call in progress.
#[derive(Debug, Clone, PartialEq)]
pub struct RunningTool {
    pub name: String,
    /// What it works on: the command, path, pattern or URL.
    pub detail: String,
    pub elapsed: Duration,
}

/// What an isolated agent has done in its worktree so far.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeProgress {
    pub path: PathBuf,
    /// `git status --short`, one line per path.
    pub changed: Vec<String>,
    /// `git log --oneline <base>..HEAD`: the commits it made.
    pub commits: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemberStatus {
    pub id: String,
    pub def: String,
    pub depth: u32,
    pub state: MemberState,
    pub elapsed: Duration,
    pub tool: Option<RunningTool>,
    /// The last lines of what it said, or is saying now.
    pub text: Vec<String>,
    /// Files its write, edit and patch calls changed, in the order it first changed them.
    pub files: Vec<String>,
    /// For an isolated agent: its worktree, or why git could not read it.
    pub worktree: Option<Result<WorktreeProgress, String>>,
    /// Input and output tokens used so far.
    pub tokens: u64,
    pub cost_usd: Option<f64>,
    /// What the questions it asked other agents on the side (`agent_send` with `ask`) cost.
    pub asked: Option<Usage>,
    /// What model routing made of its task, when it rated one.
    pub routing: Option<Routing>,
}

/// The paths a call of a file-changing tool names. Other tools change no file the harness
/// can name.
pub(crate) fn written_paths(tool: &str, input: &Value) -> Vec<String> {
    match tool {
        "write" | "edit" => input
            .get("path")
            .and_then(Value::as_str)
            .map(|path| vec![path.to_string()])
            .unwrap_or_default(),
        "patch" => {
            let text = input
                .get("patch_text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut paths = Vec::new();
            for line in text.lines() {
                let named = [
                    "*** Add File: ",
                    "*** Update File: ",
                    "*** Delete File: ",
                    "*** Move to: ",
                ]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix));
                if let Some(path) = named.map(str::trim).filter(|path| !path.is_empty())
                    && !paths.iter().any(|known| known == path)
                {
                    paths.push(path.to_string());
                }
            }
            paths
        }
        _ => Vec::new(),
    }
}

/// The last lines of `text` worth showing.
pub(crate) fn last_lines(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(TEXT_LINES)..]
        .iter()
        .map(|line| {
            let cut: String = line.chars().take(LINE_CHARS).collect();
            if cut.len() < line.len() {
                format!("{cut}…")
            } else {
                cut
            }
        })
        .collect()
}

fn money(usage_cost: Option<f64>) -> String {
    usage_cost
        .map(|cost| format!(" · ${cost:.4}"))
        .unwrap_or_default()
}

fn headline(status: &MemberStatus) -> String {
    format!(
        "{} [{}] {} · {}s · {} tokens{}",
        status.id,
        status.def,
        status.state,
        status.elapsed.as_secs(),
        status.tokens,
        money(status.cost_usd)
    )
}

fn tool_line(tool: &RunningTool) -> String {
    let detail = if tool.detail.is_empty() {
        String::new()
    } else {
        format!(" {}", tool.detail)
    };
    format!("{}{detail} ({}s)", tool.name, tool.elapsed.as_secs())
}

/// Everything known about one agent, for a reader.
pub fn status_text(status: &MemberStatus) -> String {
    let mut out = headline(status);
    if let Some(tool) = &status.tool {
        out.push_str(&format!("\nrunning: {}", tool_line(tool)));
    }
    if !status.text.is_empty() {
        out.push_str("\nlast said:");
        for line in &status.text {
            out.push_str(&format!("\n  {line}"));
        }
    }
    if !status.files.is_empty() {
        out.push_str(&format!("\nfiles changed: {}", status.files.join(", ")));
    }
    match &status.worktree {
        Some(Ok(worktree)) => {
            out.push_str(&format!("\nworktree: {}", worktree.path.display()));
            if worktree.changed.is_empty() {
                out.push_str("\n  no uncommitted changes");
            }
            for line in &worktree.changed {
                out.push_str(&format!("\n  {line}"));
            }
            if !worktree.commits.is_empty() {
                out.push_str("\ncommits:");
                for line in &worktree.commits {
                    out.push_str(&format!("\n  {line}"));
                }
            }
        }
        Some(Err(why)) => out.push_str(&format!("\nworktree: git could not read it: {why}")),
        None => {}
    }
    if let Some(asked) = &status.asked {
        out.push_str(&format!(
            "\nside questions it asked (btw): {} tokens{}",
            asked.input + asked.cache_read + asked.cache_write + asked.output,
            money(asked.cost_usd)
        ));
    }
    if let Some(routing) = &status.routing {
        out.push_str(&format!(
            "
{}",
            routing.line()
        ));
    }
    out
}

/// One line per agent: what it is doing now and what it has changed.
pub fn status_table(statuses: &[MemberStatus]) -> String {
    let mut out = String::new();
    for status in statuses {
        out.push_str(&"  ".repeat(status.depth as usize));
        out.push_str(&headline(status));
        if let Some(tool) = &status.tool {
            out.push_str(&format!(" · running: {}", tool_line(tool)));
        }
        let changed = match &status.worktree {
            Some(Ok(worktree)) => worktree.changed.len().max(status.files.len()),
            _ => status.files.len(),
        };
        if changed > 0 {
            out.push_str(&format!(" · {changed} files changed"));
        }
        if let Some(Ok(worktree)) = &status.worktree
            && !worktree.commits.is_empty()
        {
            out.push_str(&format!(" · {} commits", worktree.commits.len()));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_patch_names_every_file_it_touches_once() {
        let patch = "*** Begin Patch\n*** Add File: new.txt\n+hi\n*** Update File: a.rs\n*** Move to: b.rs\n\
                     @@\n-x\n+y\n*** Update File: a.rs\n*** Delete File: old.txt\n*** End Patch";
        assert_eq!(
            written_paths("patch", &json!({ "patch_text": patch })),
            ["new.txt", "a.rs", "b.rs", "old.txt"]
        );
        assert_eq!(
            written_paths("edit", &json!({ "path": "src/x.rs" })),
            ["src/x.rs"]
        );
        assert!(written_paths("read", &json!({ "path": "src/x.rs" })).is_empty());
    }
}
