//! The little of Markdown the docs use, read line by line: tables, headings, code spans, fenced
//! code and links.

use std::collections::BTreeMap;

/// One page, with the path it is named by in findings (`docs/tools.md`).
#[derive(Debug, Clone)]
pub struct Page {
    pub path: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Heading {
    /// 1-based.
    pub line: usize,
    pub level: usize,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Row {
    /// 1-based.
    pub line: usize,
    pub cells: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Table {
    /// The heading the table stands under, without its `#`s; empty before the first heading.
    pub heading: String,
    /// The nearest `## ` heading above the table.
    pub section: String,
    pub headers: Vec<String>,
    pub rows: Vec<Row>,
}

impl Page {
    pub fn new(path: &str, text: &str) -> Page {
        Page {
            path: path.to_string(),
            lines: text.lines().map(str::to_string).collect(),
        }
    }

    /// The lines with their 1-based numbers and whether each is inside a fenced code block (the
    /// fences themselves count as inside).
    pub fn numbered(&self) -> impl Iterator<Item = (usize, &str, bool)> {
        let mut fenced = false;
        self.lines.iter().enumerate().map(move |(index, line)| {
            let fence = line.trim_start().starts_with("```");
            let inside = fenced || fence;
            if fence {
                fenced = !fenced;
            }
            (index + 1, line.as_str(), inside)
        })
    }

    pub fn headings(&self) -> Vec<Heading> {
        self.numbered()
            .filter(|(_, _, fenced)| !fenced)
            .filter_map(|(line, text, _)| {
                let level = text.chars().take_while(|c| *c == '#').count();
                let rest = text[level..].strip_prefix(' ')?;
                (1..=6).contains(&level).then(|| Heading {
                    line,
                    level,
                    text: rest.trim().to_string(),
                })
            })
            .collect()
    }

    /// The anchors GitHub gives the headings of the page.
    pub fn anchors(&self) -> Vec<String> {
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        self.headings()
            .iter()
            .map(|heading| {
                let slug = slug(&heading.text);
                let count = seen.entry(slug.clone()).or_insert(0);
                let anchor = match *count {
                    0 => slug,
                    n => format!("{slug}-{n}"),
                };
                *count += 1;
                anchor
            })
            .collect()
    }

    pub fn tables(&self) -> Vec<Table> {
        let mut tables: Vec<Table> = Vec::new();
        let (mut heading, mut section) = (String::new(), String::new());
        let mut open = false;
        let lines: Vec<(usize, &str, bool)> = self.numbered().collect();
        for (index, (line, text, fenced)) in lines.iter().enumerate() {
            if *fenced {
                open = false;
                continue;
            }
            let level = text.chars().take_while(|c| *c == '#').count();
            if level > 0 && text[level..].starts_with(' ') {
                heading = text[level..].trim().to_string();
                if level <= 2 {
                    section = heading.clone();
                }
            }
            if !text.trim_start().starts_with('|') {
                open = false;
                continue;
            }
            let separator = |text: &str| {
                let inner = text.trim().trim_matches('|');
                !inner.is_empty() && inner.chars().all(|c| matches!(c, '-' | ':' | '|' | ' '))
            };
            if open {
                if let Some(table) = tables.last_mut() {
                    table.rows.push(Row {
                        line: *line,
                        cells: cells(text),
                    });
                }
            } else if lines.get(index + 1).is_some_and(|(_, next, _)| separator(next)) {
                tables.push(Table {
                    heading: heading.clone(),
                    section: section.clone(),
                    headers: cells(text),
                    rows: Vec::new(),
                });
            } else if separator(text) && !tables.is_empty() {
                open = true;
            }
        }
        tables
    }

    /// The paragraph that starts with `start`, as one line with the 1-based number of its first
    /// line.
    pub fn paragraph(&self, start: &str) -> Option<(usize, String)> {
        let first = self.lines.iter().position(|line| line.starts_with(start))?;
        let text: Vec<&str> = self.lines[first..]
            .iter()
            .take_while(|line| !line.trim().is_empty())
            .map(String::as_str)
            .collect();
        Some((first + 1, text.join(" ")))
    }

    /// Link targets outside code, with the 1-based line each is on.
    pub fn links(&self) -> Vec<(usize, String)> {
        let mut found = Vec::new();
        for (line, text, fenced) in self.numbered() {
            if fenced {
                continue;
            }
            let prose = without_code_spans(text);
            let mut rest = prose.as_str();
            while let Some(at) = rest.find("](") {
                let after = &rest[at + 2..];
                let Some(end) = after.find(')') else { break };
                // A title may follow the address: `[text](page.md "title")`.
                let target = after[..end].split_whitespace().next().unwrap_or_default();
                if !target.is_empty() {
                    found.push((line, target.to_string()));
                }
                rest = &after[end..];
            }
        }
        found
    }
}

/// The cells of a table row; `\|` is a bar inside a cell.
pub fn cells(line: &str) -> Vec<String> {
    let inner = line.trim();
    let inner = inner.strip_prefix('|').unwrap_or(inner);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut cells = vec![String::new()];
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                if let Some(cell) = cells.last_mut() {
                    cell.push('|');
                }
            }
            '|' => cells.push(String::new()),
            c => {
                if let Some(cell) = cells.last_mut() {
                    cell.push(c);
                }
            }
        }
    }
    cells.iter().map(|cell| cell.trim().to_string()).collect()
}

/// The code spans of a line of prose, without their backticks.
pub fn code_spans(text: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        let ticks = rest[start..].chars().take_while(|c| *c == '`').count();
        let body = &rest[start + ticks..];
        let fence = "`".repeat(ticks);
        let Some(end) = body.find(&fence) else { break };
        spans.push(body[..end].trim());
        rest = &body[end + ticks..];
    }
    spans
}

/// `text` with its code spans blanked out.
fn without_code_spans(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        let ticks = rest[start..].chars().take_while(|c| *c == '`').count();
        let body = &rest[start + ticks..];
        let Some(end) = body.find(&"`".repeat(ticks)) else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push(' ');
        rest = &body[end + ticks..];
    }
    out.push_str(rest);
    out
}

/// The anchor GitHub makes of a heading: lower case, formatting and punctuation dropped, spaces
/// to hyphens.
pub fn slug(heading: &str) -> String {
    // A link in a heading counts by its text.
    let mut text = String::new();
    let mut rest = heading;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find("](") else {
            break;
        };
        let Some(end) = rest[open + close..].find(')') else {
            break;
        };
        text.push_str(&rest[..open]);
        text.push_str(&rest[open + 1..open + close]);
        rest = &rest[open + close + end + 1..];
    }
    text.push_str(rest);
    text.to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_read_with_their_heading_and_escaped_bars() {
        let page = Page::new(
            "docs/x.md",
            "# Title\n\n## Keys\n\n| Key | Meaning |\n|---|---|\n| `a\\|b` | one |\n| | two |\n\ntext\n\n```\n| not | a table |\n|---|---|\n| x | y |\n```\n\n### Sub\n\n| A |\n|:--|\n| 1 |\n",
        );
        let tables = page.tables();
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[0].heading, "Keys");
        assert_eq!(tables[0].headers, ["Key", "Meaning"]);
        assert_eq!(tables[0].rows[0].cells, ["`a|b`", "one"]);
        assert_eq!(tables[0].rows[0].line, 7);
        assert_eq!(tables[0].rows[1].cells, ["", "two"]);
        assert_eq!(tables[1].heading, "Sub");
        assert_eq!(tables[1].section, "Keys");
        assert_eq!(tables[1].rows.len(), 1);
    }

    #[test]
    fn code_spans_keep_what_is_between_any_number_of_backticks() {
        assert_eq!(code_spans("`a`, `b c` and `` !`cmd` ``"), ["a", "b c", "!`cmd`"]);
        assert_eq!(code_spans("no code, one ` stray"), Vec::<&str>::new());
    }

    #[test]
    fn anchors_follow_github() {
        let page = Page::new(
            "docs/x.md",
            "# The `[tools]` table\n## What's new?\n## Keys\n### Keys\n## [Unreleased]\n## See [the team](team.md)\n```\n# not a heading\n```\n",
        );
        assert_eq!(
            page.anchors(),
            [
                "the-tools-table",
                "whats-new",
                "keys",
                "keys-1",
                "unreleased",
                "see-the-team"
            ]
        );
    }

    #[test]
    fn links_outside_code_are_found() {
        let page = Page::new(
            "docs/x.md",
            "See [a](a.md#top) and [b](../b.md \"B\"), not `[c](c.md)`.\n```\n[d](d.md)\n```\n",
        );
        let targets: Vec<String> = page.links().into_iter().map(|(_, target)| target).collect();
        assert_eq!(targets, ["a.md#top", "../b.md"]);
    }

    #[test]
    fn a_paragraph_is_joined_up_to_the_blank_line() {
        let page = Page::new("docs/x.md", "intro\n\nGated settings: `a`,\n`b`.\n\nmore `c`\n");
        assert_eq!(
            page.paragraph("Gated settings:"),
            Some((3, "Gated settings: `a`, `b`.".to_string()))
        );
    }
}
