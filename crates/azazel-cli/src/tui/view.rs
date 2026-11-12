//! Turning session events into transcript lines.

use std::time::Duration;

use azazel_agent::ToolOutput;
use azazel_ai::{Block, Message, Role as MessageRole};
use azazel_tui::components::{diff, format_duration, markdown, tool_card};
use azazel_tui::{Card, CardState, Line, Role, Theme, text};
use serde_json::Value;

/// Output lines shown in the card of a finished tool call unless expanded.
const COLLAPSED_LINES: usize = 3;
const EXPANDED_LINES: usize = 40;
const COLLAPSED_DIFF_LINES: usize = 16;
/// How the result of a call that was interrupted or refused, rather than failed, starts.
const NOT_RUN: [&str; 2] = ["aborted by user", "the user denied this call"];

/// `text` wrapped to `width` and painted with `role`.
pub fn styled(text_in: &str, role: Role, width: usize, theme: &Theme) -> Vec<Line> {
    let mut lines = Vec::new();
    for raw in text::sanitize(text_in).lines() {
        if raw.is_empty() {
            lines.push(String::new());
            continue;
        }
        lines.extend(text::wrap(&theme.paint(role, raw), width));
    }
    lines
}

pub fn notice(text_in: &str, width: usize, theme: &Theme) -> Vec<Line> {
    styled(text_in, Role::Muted, width, theme)
}

pub fn warning(text_in: &str, width: usize, theme: &Theme) -> Vec<Line> {
    styled(text_in, Role::Warning, width, theme)
}

pub fn error(text_in: &str, width: usize, theme: &Theme) -> Vec<Line> {
    styled(text_in, Role::Error, width, theme)
}

/// The name in block letters, shown when the UI opens.
const BANNER_WIDE: [&str; 6] = [
    " █████╗ ███████╗ █████╗ ███████╗███████╗██╗     ",
    "██╔══██╗╚══███╔╝██╔══██╗╚══███╔╝██╔════╝██║     ",
    "███████║  ███╔╝ ███████║  ███╔╝ █████╗  ██║     ",
    "██╔══██║ ███╔╝  ██╔══██║ ███╔╝  ██╔══╝  ██║     ",
    "██║  ██║███████╗██║  ██║███████╗███████╗███████╗",
    "╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝╚══════╝╚══════╝╚══════╝",
];
/// The same in thin letters, for narrower terminals.
const BANNER_NARROW: [&str; 5] = [
    "                                __",
    "  ____ _____  ____ _____  ___  / /",
    " / __ `/_  / / __ `/_  / / _ \\/ / ",
    "/ /_/ / / /_/ /_/ / / /_/  __/ /  ",
    "\\__,_/ /___/\\__,_/ /___/\\___/_/   ",
];
const TAGLINE: &str = "a team of coding agents in your terminal";

/// The opening banner: block letters with their shadow in a quieter colour, thin letters when the
/// terminal is narrower, nothing when even those do not fit.
pub fn banner(width: usize, theme: &Theme) -> Vec<Line> {
    let wide = text::width(BANNER_WIDE[0]);
    let narrow = text::width(BANNER_NARROW[1]);
    let mut lines = vec![String::new()];
    if width > wide + 2 {
        for row in BANNER_WIDE {
            lines.push(format!(" {}", two_tone(row, theme)));
        }
    } else if width > narrow + 2 {
        for row in BANNER_NARROW {
            lines.push(format!(" {}", theme.paint(Role::Accent, row.trim_end())));
        }
    } else {
        return Vec::new();
    }
    if width > text::width(TAGLINE) + 2 {
        lines.push(format!(" {}", theme.paint(Role::Muted, TAGLINE)));
    }
    lines.push(String::new());
    lines
}

/// Solid blocks in the accent colour, the box-drawing shadow muted.
fn two_tone(row: &str, theme: &Theme) -> String {
    let mut out = String::new();
    let mut run = String::new();
    let mut solid = None;
    for c in row.trim_end().chars() {
        let this = c == '█';
        if solid.is_some_and(|was| was != this) {
            let role = if solid == Some(true) {
                Role::Accent
            } else {
                Role::Muted
            };
            out.push_str(&theme.paint(role, &run));
            run.clear();
        }
        solid = Some(this);
        run.push(c);
    }
    if !run.is_empty() {
        let role = if solid == Some(true) {
            Role::Accent
        } else {
            Role::Muted
        };
        out.push_str(&theme.paint(role, &run));
    }
    out
}

/// A user prompt: a blank row, then the text on a band that spans the whole width.
pub fn user_prompt(prompt: &str, width: usize, theme: &Theme) -> Vec<Line> {
    let mut lines = vec![String::new()];
    let inner = width.saturating_sub(2).max(1);
    for raw in text::sanitize(prompt).lines() {
        for row in text::wrap(raw, inner) {
            lines.push(theme.band(Role::UserBg, Role::UserText, &format!(" {row}"), width));
        }
    }
    lines
}

/// The model's reasoning, set apart from what it says.
pub fn thinking(text_in: &str, width: usize, theme: &Theme) -> Vec<Line> {
    let style = format!("\x1b[3m{}", theme.fg(Role::Thinking));
    let mut lines = Vec::new();
    for raw in text::sanitize(text_in).lines() {
        lines.extend(text::wrap(&format!("{style}{raw}"), width));
    }
    lines
}

/// What a tool call is about, on one line.
pub fn tool_summary(name: &str, input: &Value) -> String {
    let field = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_string);
    let summary = match name {
        "shell" => field("command"),
        "read" | "write" | "edit" | "plan_submit" => field("path"),
        "glob" | "grep" => field("pattern"),
        "web_fetch" => field("url"),
        "web_search" | "memory_search" | "semantic_search" | "tool_search" => field("query"),
        "skill" => field("name"),
        "job" => Some(format!(
            "{} {}",
            field("action").unwrap_or_default(),
            field("id").unwrap_or_default()
        )),
        "todo" => input
            .get("items")
            .and_then(Value::as_array)
            .map(|items| format!("{} items", items.len())),
        "task" => input.get("agents").and_then(Value::as_array).map(|agents| {
            agents
                .iter()
                .filter_map(|a| a.get("agent").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(", ")
        }),
        _ => None,
    };
    let summary = summary.unwrap_or_else(|| match input {
        Value::Object(map) if map.is_empty() => String::new(),
        other => other.to_string(),
    });
    summary.lines().next().unwrap_or_default().to_string()
}

fn output_text(output: &ToolOutput) -> String {
    let mut out = String::new();
    for block in &output.content {
        let piece = match block {
            Block::Text { text } => text.as_str(),
            Block::Image { .. } => "[image]",
            _ => continue,
        };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(piece);
    }
    out
}

/// A diff without the two lines that name its file, which the header of the card already does.
/// A diff that covers several files keeps them all.
fn without_file_header(diff: &str) -> &str {
    let mut rest = diff;
    for marker in ["--- ", "+++ "] {
        match rest.strip_prefix(marker).and_then(|named| named.split_once('\n')) {
            Some((_, after)) => rest = after,
            None => return diff,
        }
    }
    if rest.contains("\n+++ ") { diff } else { rest }
}

/// What a command printed, without the last line when that only repeats its exit code.
fn without_exit_line(said: &str) -> &str {
    let trimmed = said.trim_end();
    let (body, last) = trimmed.rsplit_once('\n').unwrap_or(("", trimmed));
    if last.starts_with("exit code:") {
        body
    } else {
        said
    }
}

/// A finished tool call as a card: its diff or the start of its output inside, how long it took
/// and how it ended in the bottom border.
pub fn tool_done(
    name: &str,
    input: &Value,
    output: &ToolOutput,
    elapsed: Duration,
    expanded: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line> {
    let said = output_text(output);
    let state = if !output.is_error {
        CardState::Success
    } else if NOT_RUN.iter().any(|start| said.starts_with(start)) {
        CardState::Warning
    } else {
        CardState::Error
    };
    let inner = tool_card::inner_width(width).max(1);
    let details = output.details.as_ref();
    let diff_text = details
        .and_then(|d| d.get("diff"))
        .and_then(Value::as_str)
        .map(without_file_header)
        .unwrap_or_default();
    let (mut body, limit): (Vec<Line>, usize) = if !diff_text.is_empty() && !output.is_error {
        let limit = if expanded {
            usize::MAX
        } else {
            COLLAPSED_DIFF_LINES
        };
        (diff::render_unified(diff_text, inner, theme), limit)
    } else {
        let limit = if expanded || output.is_error {
            EXPANDED_LINES
        } else {
            COLLAPSED_LINES
        };
        let role = match state {
            CardState::Error => Role::Error,
            CardState::Warning => Role::Warning,
            _ => Role::Muted,
        };
        // The border of the card says how a command ended; its output need not repeat it.
        let exit = details.and_then(|d| d.get("exit_code")).and_then(Value::as_i64);
        let said = if exit.is_some() {
            without_exit_line(&said)
        } else {
            &said
        };
        let rows = text::sanitize(said)
            .lines()
            .map(|l| theme.paint(role, &text::truncate(l, inner, "…")))
            .collect();
        (rows, limit)
    };
    let hidden = body.len().saturating_sub(limit);
    body.truncate(limit);
    if hidden > 0 {
        let more = format!("… {hidden} more lines (ctrl+o to expand)");
        body.push(theme.paint(Role::Muted, &more));
    }

    let mut facts = vec![format_duration(elapsed)];
    if details.and_then(|d| d.get("timed_out")).and_then(Value::as_bool) == Some(true) {
        facts.push("timed out".to_string());
    } else if let Some(code) = details.and_then(|d| d.get("exit_code")).and_then(Value::as_i64) {
        facts.push(format!("exit {code}"));
    }
    tool_card::render(
        &Card {
            state,
            name,
            summary: &tool_summary(name, input),
            body: &body,
            facts: &facts,
        },
        width,
        theme,
    )
}

/// A stored message as transcript lines, for showing where a resumed session left off.
pub fn recap(message: &Message, width: usize, theme: &Theme) -> Vec<Line> {
    let body = message.text();
    if body.trim().is_empty() {
        return Vec::new();
    }
    match message.role {
        MessageRole::User => user_prompt(&body, width, theme),
        MessageRole::Assistant => markdown::render(&body, width, theme),
        MessageRole::Tool => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| text::strip_ansi(l)).collect()
    }

    #[test]
    fn summaries_pick_the_telling_field() {
        assert_eq!(
            tool_summary("shell", &json!({"command": "cargo test\n  --all"})),
            "cargo test"
        );
        assert_eq!(
            tool_summary("edit", &json!({"path": "src/a.rs", "edits": []})),
            "src/a.rs"
        );
        assert_eq!(
            tool_summary("grep", &json!({"pattern": "fn main", "path": "src"})),
            "fn main"
        );
        assert_eq!(tool_summary("todo", &json!({"items": [1, 2]})), "2 items");
        assert_eq!(
            tool_summary(
                "task",
                &json!({"agents": [{"agent": "scout"}, {"agent": "tester"}]})
            ),
            "scout, tester"
        );
        assert_eq!(tool_summary("mcp__x__y", &json!({"a": 1})), "{\"a\":1}");
        assert_eq!(tool_summary("agent_list", &json!({})), "");
    }

    #[test]
    fn user_prompt_is_a_band_across_the_width() {
        let theme = Theme::dark();
        let lines = user_prompt("one two three four", 10, &theme);
        assert_eq!(plain(&lines), ["", " one two  ", " three    ", " four     "]);
        for line in &lines[1..] {
            assert!(line.starts_with(&theme.bg(Role::UserBg)), "{line:?}");
        }
    }

    /// The rows of a card without its side borders and padding.
    fn inside(rows: &[String]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                let row = row.strip_prefix("│ ").unwrap_or(row);
                row.strip_suffix(" │").unwrap_or(row).trim_end().to_string()
            })
            .collect()
    }

    #[test]
    fn finished_tool_is_a_card_with_a_bounded_body() {
        let theme = Theme::dark();
        let long = ToolOutput::text(
            (1..=10)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .with_details(json!({"exit_code": 0, "timed_out": false}));
        let input = json!({"command": "seq 10"});
        let collapsed = plain(&tool_done(
            "shell",
            &input,
            &long,
            Duration::from_millis(2040),
            false,
            44,
            &theme,
        ));
        assert!(collapsed[0].starts_with("╭─── ✔ shell seq 10 ─"), "{collapsed:?}");
        assert!(collapsed[0].ends_with('╮'));
        assert_eq!(
            inside(&collapsed[1..5]),
            ["line 1", "line 2", "line 3", "… 7 more lines (ctrl+o to expand)"]
        );
        assert!(collapsed[5].starts_with("╰── 2.0s · exit 0 ─"), "{collapsed:?}");
        assert_eq!(collapsed.len(), 6);
        for row in &collapsed {
            assert_eq!(text::width(row), 44, "{row:?}");
        }
        let expanded = tool_done("shell", &input, &long, Duration::from_secs(2), true, 44, &theme);
        assert_eq!(expanded.len(), 12);

        // The exit code is in the border, so the line of output that repeats it is left out.
        let ran = ToolOutput::text("built\nexit code: 3").with_details(json!({"exit_code": 3}));
        let shown = plain(&tool_done(
            "shell",
            &input,
            &ran,
            Duration::ZERO,
            false,
            44,
            &theme,
        ));
        assert_eq!(inside(&shown[1..shown.len() - 1]), ["built"]);
        assert!(shown[2].starts_with("╰── 0ms · exit 3 ─"), "{shown:?}");

        let failed = ToolOutput::error("boom\nwhy");
        let shown = plain(&tool_done(
            "shell",
            &input,
            &failed,
            Duration::ZERO,
            false,
            44,
            &theme,
        ));
        assert!(shown[0].starts_with("╭─── ✘ shell"), "{shown:?}");
        assert_eq!(inside(&shown[1..3]), ["boom", "why"]);
        assert!(shown[3].starts_with("╰── 0ms ─"), "{shown:?}");

        // Interrupted or refused is not the same as failed.
        let aborted = ToolOutput::error("aborted by user");
        let shown = plain(&tool_done(
            "shell",
            &input,
            &aborted,
            Duration::ZERO,
            false,
            44,
            &theme,
        ));
        assert!(shown[0].starts_with("╭─── ⚠ shell"), "{shown:?}");

        let edited = ToolOutput::text("edited a.rs")
            .with_details(json!({"diff": "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n"}));
        let shown = plain(&tool_done(
            "edit",
            &json!({"path": "a.rs"}),
            &edited,
            Duration::ZERO,
            false,
            44,
            &theme,
        ));
        let rows = inside(&shown);
        assert!(rows.contains(&"1 -old".to_string()), "{rows:?}");
        assert!(rows.contains(&"1 +new".to_string()), "{rows:?}");
        assert!(!shown.iter().any(|l| l.contains("edited a.rs")));
        // The card's header names the file; the diff does not repeat it.
        assert!(shown[0].starts_with("╭─── ✔ edit a.rs ─"), "{shown:?}");
        assert_eq!(rows[1], "@@ -1 +1 @@");
        let two_files = "--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n+y\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-x\n+y\n";
        assert_eq!(without_file_header(two_files), two_files);
        for line in &shown {
            assert_eq!(text::width(line), 44);
        }
    }
}
