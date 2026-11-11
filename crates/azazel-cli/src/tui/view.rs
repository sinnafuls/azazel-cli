//! Turning session events into transcript lines.

use std::time::Duration;

use azazel_agent::ToolOutput;
use azazel_ai::{Block, Message, Role as MessageRole};
use azazel_tui::components::{diff, format_elapsed, markdown};
use azazel_tui::{Line, RESET, Role, Theme, text};
use serde_json::Value;

/// Output lines shown under a finished tool call unless expanded.
const COLLAPSED_LINES: usize = 3;
const EXPANDED_LINES: usize = 40;
const COLLAPSED_DIFF_LINES: usize = 16;

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

/// A user prompt: `› ` on the first row, aligned continuation rows, a blank row above.
pub fn user_prompt(prompt: &str, width: usize, theme: &Theme) -> Vec<Line> {
    let marker = theme.paint(Role::Accent, "›");
    let mut lines = vec![String::new()];
    let mut first = true;
    for raw in text::sanitize(prompt).lines() {
        for row in text::wrap(raw, width.saturating_sub(2).max(1)) {
            lines.push(if std::mem::take(&mut first) {
                format!("{marker} {row}")
            } else {
                format!("  {row}")
            });
        }
    }
    lines.push(String::new());
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

/// A finished tool call: a status line, then its diff or the start of its output.
pub fn tool_done(
    name: &str,
    input: &Value,
    output: &ToolOutput,
    elapsed: Duration,
    expanded: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line> {
    let bullet = theme.paint(
        if output.is_error {
            Role::Error
        } else {
            Role::Success
        },
        "●",
    );
    let time = theme.paint(Role::Muted, &format_elapsed(elapsed));
    let fixed = 2 + text::width(name) + 2 + text::width(&time);
    let summary = text::truncate(&tool_summary(name, input), width.saturating_sub(fixed + 1), "…");
    let mut lines = vec![format!(
        "{bullet} {}{name}{RESET} {summary}  {time}",
        theme.fg(Role::Accent)
    )];

    let indent = "  ";
    let inner = width.saturating_sub(indent.len()).max(1);
    let diff_text = output
        .details
        .as_ref()
        .and_then(|d| d.get("diff"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (body, limit, role): (Vec<Line>, usize, Option<Role>) = if !diff_text.is_empty() && !output.is_error {
        let limit = if expanded {
            usize::MAX
        } else {
            COLLAPSED_DIFF_LINES
        };
        (diff::render_unified(diff_text, inner, theme), limit, None)
    } else {
        let limit = if expanded || output.is_error {
            EXPANDED_LINES
        } else {
            COLLAPSED_LINES
        };
        let role = if output.is_error { Role::Error } else { Role::Muted };
        let raw: Vec<Line> = text::sanitize(&output_text(output))
            .lines()
            .map(|l| text::truncate(l, inner, "…"))
            .collect();
        (raw, limit, Some(role))
    };
    let hidden = body.len().saturating_sub(limit);
    for row in body.into_iter().take(limit) {
        lines.push(match role {
            Some(role) => format!("{indent}{}", theme.paint(role, &row)),
            None => format!("{indent}{row}"),
        });
    }
    if hidden > 0 {
        lines.push(format!(
            "{indent}{}",
            theme.paint(Role::Muted, &format!("… {hidden} more lines (ctrl+o expands)"))
        ));
    }
    lines
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
    fn user_prompt_wraps_under_the_marker() {
        let theme = Theme::dark();
        let lines = plain(&user_prompt("one two three four", 10, &theme));
        assert_eq!(lines, ["", "› one two", "  three", "  four", ""]);
    }

    #[test]
    fn finished_tool_shows_a_bounded_body() {
        let theme = Theme::dark();
        let long = ToolOutput::text(
            (1..=10)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let input = json!({"command": "seq 10"});
        let collapsed = plain(&tool_done(
            "shell",
            &input,
            &long,
            Duration::from_secs(2),
            false,
            60,
            &theme,
        ));
        assert_eq!(collapsed[0], "● shell seq 10  2s");
        assert_eq!(
            &collapsed[1..],
            [
                "  line 1",
                "  line 2",
                "  line 3",
                "  … 7 more lines (ctrl+o expands)"
            ]
        );
        let expanded = tool_done("shell", &input, &long, Duration::from_secs(2), true, 60, &theme);
        assert_eq!(expanded.len(), 11);

        let failed = ToolOutput::error("boom\nwhy");
        let shown = plain(&tool_done(
            "shell",
            &input,
            &failed,
            Duration::ZERO,
            false,
            60,
            &theme,
        ));
        assert_eq!(&shown[1..], ["  boom", "  why"]);

        let edited = ToolOutput::text("edited a.rs")
            .with_details(json!({"diff": "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n"}));
        let shown = plain(&tool_done(
            "edit",
            &json!({"path": "a.rs"}),
            &edited,
            Duration::ZERO,
            false,
            60,
            &theme,
        ));
        assert!(
            shown.iter().any(|l| l.trim_start().starts_with("-old")),
            "{shown:?}"
        );
        assert!(!shown.iter().any(|l| l.contains("edited a.rs")));
        for line in &shown {
            assert!(text::width(line) <= 60);
        }
    }
}
