//! `/context` and `azazel context`: what the lead's last request was made of, by category.
//!
//! The numbers come from the request itself as the agent kept it (or, for a session that is not
//! running, as its `context.json` holds them); nothing is built again to count it.

use std::fmt::Write as _;

use azazel_agent::{Category, CategoryKind, ContextReport};
use azazel_core::{Paths, SessionStore};
use azazel_tui::components::format_tokens;

use crate::app::{Active, App};
use crate::print::{EXIT_FAILED, EXIT_OK};

/// Cells of a category's bar where it is drawn as text.
pub const BAR_CELLS: usize = 20;

/// What a category is called.
pub fn label(category: &Category) -> String {
    let name = category.name.as_str();
    match category.kind {
        CategoryKind::System => match name {
            "" => "system prompt: other".to_string(),
            "context_files" => "AGENTS and context files".to_string(),
            "skills" => "skill index".to_string(),
            "rule_files" => "rules on request".to_string(),
            "memory" => "memory".to_string(),
            name => format!("system prompt: {name}"),
        },
        CategoryKind::Tools if name.is_empty() => "tool schemas".to_string(),
        CategoryKind::Tools => format!("tool schemas: MCP {name}"),
        CategoryKind::Messages => match name {
            "user" => "user messages".to_string(),
            "assistant" => "assistant messages".to_string(),
            "tool_calls" => "tool calls".to_string(),
            name => name.to_string(),
        },
        CategoryKind::ToolResults => "tool results".to_string(),
        CategoryKind::Images => "images".to_string(),
    }
}

/// How many things a category counts, in words: `3 parts`, `41 tools`, `1 image`.
pub fn counted(category: &Category) -> String {
    let noun = match category.kind {
        CategoryKind::System => "part",
        CategoryKind::Tools => "tool",
        CategoryKind::Messages if category.name == "tool_calls" => "call",
        CategoryKind::Messages => "block",
        CategoryKind::ToolResults => "result",
        CategoryKind::Images => "image",
    };
    let plural = if category.items == 1 { "" } else { "s" };
    format!("{} {noun}{plural}", category.items)
}

/// A category's share of the request in whole percent, rounded.
pub fn percent(category: &Category, report: &ContextReport) -> u64 {
    if report.tokens == 0 {
        return 0;
    }
    (category.tokens * 100 + report.tokens / 2) / report.tokens
}

/// Cells of a bar of `cells` that a category fills: its share of the request, and at least one
/// cell for a category that holds anything.
pub fn bar_cells(category: &Category, report: &ContextReport, cells: usize) -> usize {
    if report.tokens == 0 || category.tokens == 0 {
        return 0;
    }
    let filled = (category.tokens * cells as u64 + report.tokens / 2) / report.tokens;
    (filled as usize).clamp(1, cells)
}

/// The lines above the categories: which request this is, and how full it makes the window.
pub fn headline(report: &ContextReport) -> Vec<String> {
    let mut first = format!("request {} of {}", report.request, report.agent);
    if let Some(model) = &report.model {
        let _ = write!(first, " · {model}");
    }
    let size = if report.window == 0 {
        format!("{} tokens", format_tokens(report.tokens))
    } else {
        format!(
            "{} of {} tokens ({}%)",
            format_tokens(report.tokens),
            format_tokens(report.window),
            report.tokens * 100 / report.window
        )
    };
    let how = match report.reported {
        Some(_) => "as the provider counted them; each category is its estimate scaled to that",
        None => "estimated: the provider has counted nothing for this request",
    };
    vec![first, format!("{size}, {how}")]
}

/// The breakdown as plain text: one row per category with its tokens, share and a bar.
pub fn render(report: &ContextReport) -> String {
    let labels: Vec<String> = report.categories.iter().map(label).collect();
    let wide = labels
        .iter()
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = headline(report).join("\n");
    for (category, label) in report.categories.iter().zip(&labels) {
        let filled = bar_cells(category, report, BAR_CELLS);
        let _ = write!(
            out,
            "\n{label:<wide$}  {:>6} {:>3}%  {}{}  {}",
            format_tokens(category.tokens),
            percent(category, report),
            "#".repeat(filled),
            ".".repeat(BAR_CELLS - filled),
            counted(category),
        );
    }
    out
}

/// The report as `azazel context --json` and `/context json` print it: the breakdown with each
/// category's label beside its name.
pub fn json(report: &ContextReport) -> serde_json::Value {
    let mut value = serde_json::to_value(report).unwrap_or_default();
    let rows = value
        .get_mut("categories")
        .and_then(serde_json::Value::as_array_mut);
    for (row, category) in rows.into_iter().flatten().zip(&report.categories) {
        row["label"] = label(category).into();
    }
    value
}

/// `/context` as a report. With no argument the breakdown as text; `json` is the same as one
/// JSON object; `text <request> <category>` is the exact text of one category of that request.
pub fn report(app: &App, active: &Active, args: &str) -> Result<String, String> {
    let kept = active.lead.last_request();
    let mut words = args.split_whitespace();
    match words.next() {
        Some("text") => {
            let numbers: Vec<usize> = words.filter_map(|word| word.parse().ok()).collect();
            let [request, category] = numbers.as_slice() else {
                return Err("usage: /context text <request> <category>".to_string());
            };
            let record = kept.ok_or(
                "the text of a request is kept only while the session that sent it runs; send a prompt first",
            )?;
            if record.seq as usize != *request {
                return Err(format!(
                    "request {request} is gone: the agent has sent request {} since; open /context again",
                    record.seq
                ));
            }
            record
                .text(*category)
                .ok_or_else(|| format!("request {request} has no category {category}"))
        }
        other => {
            // A session opened again has sent nothing yet in this process: what its file holds
            // of its last request still says what it was made of.
            let report = kept
                .map(|record| record.report())
                .or_else(|| SessionStore::last_request(&active.session.path()));
            match (other, report) {
                (Some("json"), Some(report)) => Ok(json(&report).to_string()),
                (Some("json"), None) => Err("no request has been sent in this session yet".to_string()),
                (Some(word), _) => Err(format!(
                    "unknown argument `{word}`: /context, /context json or /context text <request> <category>"
                )),
                (None, Some(report)) => Ok(render(&report)),
                (None, None) => Ok(before_any_request(app, active)),
            }
        }
    }
}

/// What can be said before the lead has sent a request.
fn before_any_request(app: &App, active: &Active) -> String {
    use azazel_agent::ToolSource;
    let messages = active.session.build_context(None).len();
    let tools = app.registry.tools.tools(active.lead.info()).len();
    format!(
        "no request has been sent in this session yet\nmessages waiting to be sent: {messages}\ntools offered: {tools}"
    )
}

/// `azazel context [SESSION] [--json]`: the last request of a saved session, read from the
/// file beside it.
pub fn run(paths: &Paths, session: Option<&str>, as_json: bool) -> u8 {
    let found = crate::sessions::pick(paths, session).and_then(|summary| {
        SessionStore::last_request(&summary.path)
            .map(|report| (summary.id.clone(), report))
            .ok_or_else(|| {
                format!(
                    "session {} holds no record of a request: it has not sent one since azazel started keeping them",
                    summary.id
                )
            })
    });
    match found {
        Ok((id, report)) if as_json => {
            let mut value = json(&report);
            value["session"] = id.into();
            println!("{value}");
            EXIT_OK
        }
        Ok((id, report)) => {
            println!("session {id}\n{}", render(&report));
            EXIT_OK
        }
        Err(why) => {
            eprintln!("azazel: {why}");
            EXIT_FAILED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn category(kind: CategoryKind, name: &str, items: usize, tokens: u64) -> Category {
        Category {
            kind,
            name: name.to_string(),
            items,
            estimate: tokens,
            tokens,
        }
    }

    fn report(categories: Vec<Category>, reported: Option<u64>) -> ContextReport {
        let tokens = categories.iter().map(|c| c.tokens).sum();
        ContextReport {
            agent: "lead".into(),
            request: 7,
            model: Some("anthropic/claude-opus-5-5".into()),
            window: 200_000,
            estimate: tokens,
            reported,
            tokens,
            categories,
        }
    }

    fn sample() -> ContextReport {
        use CategoryKind::*;
        report(
            vec![
                category(System, "identity", 1, 400),
                category(System, "context_files", 1, 3_600),
                category(System, "skills", 1, 1_000),
                category(System, "", 2, 50),
                category(Tools, "", 24, 9_000),
                category(Tools, "github", 31, 16_000),
                category(Messages, "user", 3, 950),
                category(Messages, "tool_calls", 1, 0),
                category(ToolResults, "", 12, 18_000),
                category(Images, "", 1, 1_000),
            ],
            Some(50_000),
        )
    }

    #[test]
    fn categories_are_named_for_what_they_hold() {
        let report = sample();
        let labels: Vec<String> = report.categories.iter().map(label).collect();
        assert_eq!(
            labels,
            [
                "system prompt: identity",
                "AGENTS and context files",
                "skill index",
                "system prompt: other",
                "tool schemas",
                "tool schemas: MCP github",
                "user messages",
                "tool calls",
                "tool results",
                "images"
            ]
        );
        let counts: Vec<String> = report.categories.iter().map(counted).collect();
        assert_eq!(
            counts,
            [
                "1 part",
                "1 part",
                "1 part",
                "2 parts",
                "24 tools",
                "31 tools",
                "3 blocks",
                "1 call",
                "12 results",
                "1 image"
            ]
        );
    }

    #[test]
    fn a_bar_is_the_share_of_the_request_and_never_empty_for_something() {
        let report = sample();
        let bars: Vec<usize> = report
            .categories
            .iter()
            .map(|c| bar_cells(c, &report, BAR_CELLS))
            .collect();
        // 50 tokens of 50 000 still get a cell; nothing gets none; 36 % is seven cells of twenty.
        assert_eq!(bars, [1, 1, 1, 1, 4, 6, 1, 0, 7, 1]);
        let shares: Vec<u64> = report.categories.iter().map(|c| percent(c, &report)).collect();
        assert_eq!(shares, [1, 7, 2, 0, 18, 32, 2, 0, 36, 2]);
        for cells in 1..=60 {
            for category in &report.categories {
                let filled = bar_cells(category, &report, cells);
                assert!(filled <= cells);
                assert_eq!(filled == 0, category.tokens == 0);
            }
        }
        let empty = self::report(vec![category(CategoryKind::Images, "", 0, 0)], None);
        assert_eq!(bar_cells(&empty.categories[0], &empty, BAR_CELLS), 0);
        assert_eq!(percent(&empty.categories[0], &empty), 0);
    }

    #[test]
    fn the_text_report_has_a_row_per_category_under_the_size_of_the_request() {
        let text = render(&sample());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "request 7 of lead · anthropic/claude-opus-5-5");
        assert_eq!(
            lines[1],
            "50.0k of 200.0k tokens (25%), as the provider counted them; each category is its estimate scaled to that"
        );
        assert_eq!(lines.len(), 12);
        assert_eq!(
            lines[10],
            "tool results               18.0k  36%  #######.............  12 results"
        );
        assert_eq!(
            lines[9],
            "tool calls                     0   0%  ....................  1 call"
        );
        // Every row lines up under the widest label.
        let column = lines[2].find('%').unwrap();
        assert!(lines[2..].iter().all(|line| line.find('%') == Some(column)));

        let mut unanswered = sample();
        unanswered.reported = None;
        unanswered.model = None;
        unanswered.window = 0;
        let lines: Vec<String> = headline(&unanswered);
        assert_eq!(
            lines,
            [
                "request 7 of lead",
                "50.0k tokens, estimated: the provider has counted nothing for this request"
            ]
        );
    }

    #[test]
    fn the_json_form_keeps_the_numbers_and_adds_labels() {
        let report = sample();
        let value = json(&report);
        assert_eq!(value["tokens"], 50_000);
        assert_eq!(value["reported"], 50_000);
        assert_eq!(value["categories"][5]["kind"], "tools");
        assert_eq!(value["categories"][5]["name"], "github");
        assert_eq!(value["categories"][5]["label"], "tool schemas: MCP github");
        assert_eq!(value["categories"][8]["tokens"], 18_000);
        // Without the labels it reads back as the report it was made from.
        let back: ContextReport = serde_json::from_value(value).unwrap();
        assert_eq!(back, report);
    }
}
