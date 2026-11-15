//! `web_grep`: the lines of a fetched page that match a pattern.

use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{Value, json};

use super::fetch::{Fetcher, Format, domain_subject, lines as page_lines};
use crate::util::parse_input;

/// Matches returned when a call names no `limit`.
pub const DEFAULT_MATCHES: usize = 25;
/// The most matches one call returns.
pub const MAX_MATCHES: usize = 100;
/// The most lines of context on each side of a match.
pub const MAX_CONTEXT: usize = 10;
/// Characters of a line shown.
const LINE_CHARS: usize = 250;
/// Characters kept before the match when a long line is shown from its middle.
const LEAD_CHARS: usize = 60;

/// What a search of a page found.
#[derive(Debug, PartialEq, Eq)]
struct Found {
    /// `N: line` for a match, `N- line` for context, `--` between stretches that are apart.
    text: String,
    matches: usize,
    /// Lines the page has.
    lines: usize,
    /// The line of the first match left out because `limit` was reached.
    next: Option<usize>,
}

/// `line` in at most [`LINE_CHARS`] characters. A longer line is shown from the start, or around
/// the match at byte `focus` when that lies further in.
fn clip(line: &str, focus: Option<usize>) -> Cow<'_, str> {
    if line.len() <= LINE_CHARS {
        return Cow::Borrowed(line);
    }
    let total = line.chars().count();
    if total <= LINE_CHARS {
        return Cow::Borrowed(line);
    }
    let before = focus.map_or(0, |at| line[..at].chars().count());
    let start = if before + LEAD_CHARS <= LINE_CHARS {
        0
    } else {
        (before - LEAD_CHARS).min(total - LINE_CHARS)
    };
    let byte = |chars: usize| line.char_indices().nth(chars).map_or(line.len(), |(at, _)| at);
    let shown = &line[byte(start)..byte(start + LINE_CHARS)];
    let lead = if start > 0 { "…" } else { "" };
    let tail = if start + LINE_CHARS < total { "…" } else { "" };
    Cow::Owned(format!("{lead}{shown}{tail}"))
}

/// The first `limit` lines of `text` that match `regex`, each with `context` lines around it.
fn grep(text: &str, regex: &Regex, context: usize, limit: usize) -> Found {
    let lines: Vec<&str> = page_lines(text).collect();
    // (line index, byte at which the match starts)
    let mut hits: Vec<(usize, usize)> = Vec::new();
    let mut next = None;
    for (index, line) in lines.iter().enumerate() {
        let Some(found) = regex.find(line) else {
            continue;
        };
        if hits.len() == limit {
            next = Some(index + 1);
            break;
        }
        hits.push((index, found.start()));
    }

    let mut text = String::new();
    // Lines before this index are shown or passed.
    let mut shown = 0;
    for &(index, _) in &hits {
        let first = index.saturating_sub(context).max(shown);
        let last = (index + context).min(lines.len() - 1);
        if context > 0 && shown > 0 && first > shown {
            text.push_str("--\n");
        }
        for (at, line) in lines.iter().enumerate().take(last + 1).skip(first) {
            // A match inside another's context is still marked as one.
            let hit = hits.binary_search_by_key(&at, |hit| hit.0).ok();
            let (mark, focus) = match hit {
                Some(hit) => (':', Some(hits[hit].1)),
                None => ('-', None),
            };
            let line = clip(line, focus);
            let gap = if line.is_empty() { "" } else { " " };
            text.push_str(&format!("{}{mark}{gap}{line}\n", at + 1));
        }
        shown = shown.max(last + 1);
    }
    text.truncate(text.trim_end_matches('\n').len());
    Found {
        text,
        matches: hits.len(),
        lines: lines.len(),
        next,
    }
}

pub struct WebGrepTool {
    fetcher: Arc<Fetcher>,
}

impl WebGrepTool {
    pub fn new(fetcher: Arc<Fetcher>) -> Self {
        WebGrepTool { fetcher }
    }
}

#[derive(Deserialize)]
struct GrepInput {
    url: String,
    pattern: String,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    format: Format,
}

#[async_trait]
impl Tool for WebGrepTool {
    fn name(&self) -> &str {
        "web_grep"
    }

    fn description(&self) -> &str {
        "Search the page at an http or https URL for a regular expression, line by line, without \
         reading the page. Returns `N: line` for each matching line and `N- line` for `context` lines \
         around it; `N` is the line number `web_fetch` takes as `offset`. `literal` searches for the \
         text as written, `ignore_case` ignores case, `limit` is the number of matches (default 25, at \
         most 100). The page comes from the same 15-minute cache as `web_fetch`, in the same `format` \
         (default `markdown`)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "pattern": {"type": "string", "description": "Regular expression, matched within one line"},
                "ignore_case": {"type": "boolean"},
                "literal": {"type": "boolean", "description": "Treat pattern as plain text"},
                "context": {"type": "integer", "minimum": 0, "maximum": MAX_CONTEXT, "description": "Lines shown before and after each match"},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_MATCHES},
                "format": {"type": "string", "enum": ["markdown", "text", "raw"]}
            },
            "required": ["url", "pattern"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Net
    }

    fn replay_safe(&self, _: &Value) -> bool {
        true
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        let url = input.get("url").and_then(Value::as_str);
        url.and_then(domain_subject).into_iter().collect()
    }

    fn concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: GrepInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        if input.pattern.is_empty() {
            return ToolOutput::error("pattern is empty");
        }
        let source = if input.literal {
            Cow::Owned(regex::escape(&input.pattern))
        } else {
            Cow::Borrowed(input.pattern.as_str())
        };
        let regex = match RegexBuilder::new(&source)
            .case_insensitive(input.ignore_case)
            .build()
        {
            Ok(regex) => regex,
            Err(error) => {
                return ToolOutput::error(format!(
                    "invalid pattern {:?}: {error}\nset literal=true to search for the text as written",
                    input.pattern
                ));
            }
        };
        let page = match self.fetcher.fetch(&input.url, input.format, ctx).await {
            Ok(page) => page,
            Err(error) => return ToolOutput::error(error),
        };
        let context = input.context.unwrap_or(0).min(MAX_CONTEXT);
        let limit = input.limit.unwrap_or(DEFAULT_MATCHES).clamp(1, MAX_MATCHES);
        let found = grep(&page, &regex, context, limit);
        let details = json!({"matches": found.matches, "lines": found.lines});
        let mut text = found.text;
        if found.matches == 0 {
            text = format!(
                "no matches for {:?} ({} lines scanned)",
                input.pattern, found.lines
            );
        } else if let Some(next) = found.next {
            text.push_str(&format!(
                "\n[stopped at {limit} matches; the next is on line {next}: narrow the pattern or raise \
                 limit (at most {MAX_MATCHES})]"
            ));
        }
        ToolOutput::text(text).with_details(details)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn find(text: &str, pattern: &str, context: usize, limit: usize) -> Found {
        grep(text, &Regex::new(pattern).unwrap(), context, limit)
    }

    const PAGE: &str = "alpha\nbeta one\ngamma\ndelta\nbeta two\nepsilon\nzeta\neta\ntheta\nbeta three\n";

    #[test]
    fn context_lines_are_marked_and_stretches_that_touch_are_joined() {
        // Lines 1-3 and 4-6 touch, so they are one stretch; line 9 starts another.
        assert_eq!(
            find(PAGE, "beta", 1, 25).text,
            "1- alpha\n2: beta one\n3- gamma\n4- delta\n5: beta two\n6- epsilon\n--\n9- theta\n10: beta three"
        );
        // A match inside the context of another keeps its mark and is not shown twice; an empty
        // line is its number alone.
        assert_eq!(
            find("a\nhit 1\nhit 2\n\nc\n", "hit", 2, 25).text,
            "1- a\n2: hit 1\n3: hit 2\n4-\n5- c"
        );
    }

    #[test]
    fn the_limit_stops_the_search_and_names_the_next_match() {
        let found = find(PAGE, "beta", 1, 2);
        assert_eq!(
            found.text,
            "1- alpha\n2: beta one\n3- gamma\n4- delta\n5: beta two\n6- epsilon"
        );
        assert_eq!((found.matches, found.next), (2, Some(10)));
        // Exactly as many matches as the limit is not a stop.
        assert_eq!(find(PAGE, "beta", 0, 3).next, None);
    }

    #[test]
    fn long_lines_are_cut_and_a_match_far_in_stays_visible() {
        let filler = "é".repeat(400);
        let line = format!("{filler}NEEDLE{filler}");
        let found = find(&format!("{line}\n{filler}\n"), "NEEDLE", 1, 25);
        let shown: Vec<&str> = found.text.lines().collect();
        let (number, match_line) = shown[0].split_once(": ").unwrap();
        assert_eq!(number, "1");
        assert!(
            match_line.starts_with('…') && match_line.ends_with('…'),
            "{match_line}"
        );
        assert_eq!(match_line.chars().count(), LINE_CHARS + 2);
        assert_eq!(
            match_line.find("NEEDLE"),
            Some("…".len() + "é".len() * LEAD_CHARS)
        );
        // A context line is cut from its start.
        assert_eq!(shown[1], format!("2- {}…", "é".repeat(LINE_CHARS)));

        // A match near the start, and one at the very end of a long line.
        let early = format!("NEEDLE{filler}");
        assert_eq!(
            clip(&early, Some(0)),
            format!("NEEDLE{}…", "é".repeat(LINE_CHARS - 6))
        );
        let late = format!("{filler}NEEDLE");
        let end = clip(&late, Some(filler.len()));
        assert!(end.starts_with('…') && end.ends_with("NEEDLE"), "{end}");
        assert_eq!(end.chars().count(), LINE_CHARS + 1);
        assert_eq!(clip("short", Some(2)), "short");
    }
}
