//! Turning session events into transcript lines.

use std::borrow::Cow;
use std::path::Path;
use std::time::Duration;

use azazel_agent::ToolOutput;
use azazel_ai::{Block, Message, Role as MessageRole};
use azazel_tui::components::{
    code, diff, format_duration, format_elapsed, format_tokens, markdown, tool_card,
};
use azazel_tui::{Card, CardState, Glyph, GlyphLevel, Line, Role, Theme, text};
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

/// The opening banner: block letters with their shadow in a quieter colour; thin letters of
/// plain ASCII when the terminal is narrower or the glyph level is `ascii`; nothing when even
/// those do not fit.
pub fn banner(width: usize, theme: &Theme) -> Vec<Line> {
    let wide = text::width(BANNER_WIDE[0]);
    let narrow = text::width(BANNER_NARROW[1]);
    let blocks = theme.glyphs().level() != GlyphLevel::Ascii;
    let mut lines = vec![String::new()];
    if blocks && width > wide + 2 {
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

/// What an `lsp` call asks: the action, then the symbol, the place or the file it is about.
fn lsp_summary(action: &str, input: &Value) -> String {
    let field = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or_default();
    let path = field("path");
    let place = match input.get("line").and_then(Value::as_u64) {
        Some(line) => match input.get("column").and_then(Value::as_u64) {
            Some(column) => format!("{path}:{line}:{column}"),
            None => format!("{path}:{line}"),
        },
        None => path.to_string(),
    };
    match action {
        "read_symbol" => format!("{action} {} in {path}", field("symbol")),
        "rename" => format!("{action} {place} to {}", field("new_name")),
        "rename_file" => format!("{action} {path} to {}", field("new_path")),
        "symbols" if !field("query").is_empty() => format!("{action} {}", field("query")),
        _ => format!("{action} {place}"),
    }
}

/// What an `ast_grep` call looks for, what it puts in its place, and whether it writes.
fn ast_grep_summary(pattern: &str, input: &Value) -> String {
    let paths: Vec<&str> = input
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let within = match paths.as_slice() {
        [] => String::new(),
        [one] => format!(" in {one}"),
        [first, rest @ ..] => format!(" in {first} and {} more", rest.len()),
    };
    match input.get("rewrite").and_then(Value::as_str) {
        Some(rewrite) => {
            let applied = input.get("apply").and_then(Value::as_bool) == Some(true);
            let verb = if applied { "replace" } else { "preview" };
            format!("{verb} {pattern} with {rewrite}{within}")
        }
        None => format!("{pattern}{within}"),
    }
}

/// What a `session_recall` call asks of the conversation: what it searches for and which entries
/// it reads.
fn recall_summary(input: &Value) -> Option<String> {
    let text = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    };
    let ids = |key: &str| -> Vec<&str> {
        input
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect()
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(query) = text("query") {
        parts.push(query.to_string());
    }
    if let Some(regex) = text("regex") {
        parts.push(format!("/{regex}/"));
    }
    if let [from, to] = ids("range").as_slice() {
        let end = |id: &str, word: &str| {
            if id.is_empty() {
                word.to_string()
            } else {
                id.to_string()
            }
        };
        parts.push(format!("{} to {}", end(from, "start"), end(to, "end")));
    }
    match ids("expand").as_slice() {
        [] => {}
        [one] => parts.push(format!("entry {one}")),
        many => parts.push(format!("{} entries", many.len())),
    }
    if input.get("touched").and_then(Value::as_bool) == Some(true) {
        parts.push("files touched".to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// What a tool call is about, on one line.
pub fn tool_summary(name: &str, input: &Value) -> String {
    let field = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_string);
    let summary = match name {
        "shell" => field("command"),
        "read" | "write" | "edit" | "plan_submit" => field("path"),
        "glob" | "grep" => field("pattern"),
        "web_fetch" => field("url"),
        "web_grep" => {
            field("pattern").map(|pattern| format!("{pattern} in {}", field("url").unwrap_or_default()))
        }
        "web_search" => {
            let queries = input.get("queries").and_then(Value::as_array);
            let all: Vec<&str> = input
                .get("query")
                .and_then(Value::as_str)
                .into_iter()
                .chain(queries.into_iter().flatten().filter_map(Value::as_str))
                .collect();
            (!all.is_empty()).then(|| all.join(" | "))
        }
        "lsp" => field("action").map(|action| lsp_summary(&action, input)),
        "ast_grep" => field("pattern").map(|pattern| ast_grep_summary(&pattern, input)),
        "memory_search" | "semantic_search" | "tool_search" => field("query"),
        "session_recall" => recall_summary(input),
        "skill" => field("name"),
        "run_code" => field("code").map(|code| {
            let lines = code.trim().lines().count();
            format!("Luau script, {lines} line{}", if lines == 1 { "" } else { "s" })
        }),
        "job" => Some(format!(
            "{} {}",
            field("action").unwrap_or_default(),
            field("id").unwrap_or_default()
        )),
        "ask_user" => field("question").map(|question| {
            match input.get("options").and_then(Value::as_array).map(Vec::len) {
                Some(1) => format!("{question} (1 option)"),
                Some(count) if count > 0 => format!("{question} ({count} options)"),
                _ => question,
            }
        }),
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

/// `text` as it is shown and exported: with what looks like a secret hidden when `redact`
/// (`tui.redact`) is on. The session and the model keep the text as it was.
pub fn redacted(text: String, redact: bool) -> String {
    if !redact {
        return text;
    }
    let masked = match text::redact(&text) {
        Cow::Owned(masked) => Some(masked),
        Cow::Borrowed(_) => None,
    };
    masked.unwrap_or(text)
}

/// The card of a call in flight: what it is about, the last lines it printed and how long it
/// has been running.
pub fn tool_running(
    name: &str,
    input: &Value,
    tail: &[String],
    elapsed: Duration,
    redact: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line> {
    let summary = redacted(tool_summary(name, input), redact);
    if !redact {
        return tool_card::running(name, &summary, tail, elapsed, width, theme);
    }
    let tail: Vec<Cow<'_, str>> = tail.iter().map(|line| text::redact(line)).collect();
    tool_card::running(name, &summary, &tail, elapsed, width, theme)
}

/// A finished tool call.
#[derive(Clone, Copy)]
pub struct Call<'a> {
    /// The sub-agent that made the call, when it is not the lead.
    pub agent: Option<&'a str>,
    pub name: &'a str,
    pub input: &'a Value,
    pub output: &'a ToolOutput,
    /// How long it took; unknown for a call read back from the session.
    pub elapsed: Option<Duration>,
}

impl Call<'_> {
    /// The name the call is shown under: `[agent] tool` for a sub-agent's.
    fn label(&self) -> Cow<'_, str> {
        match self.agent {
            Some(agent) => Cow::Owned(format!("[{agent}] {}", self.name)),
            None => Cow::Borrowed(self.name),
        }
    }
}

/// How tool calls are drawn.
#[derive(Clone, Copy, Default)]
pub struct Show<'a> {
    /// All of a long body (up to forty rows of output, a whole diff) rather than its start.
    pub expanded: bool,
    /// Hide what looks like a secret in what is shown (`tui.redact`).
    pub redact: bool,
    /// Where relative paths start; with it, the file a call names is a link to it.
    pub cwd: Option<&'a Path>,
}

/// The file a call read or changed, as an address to open, when `cwd` says where relative
/// paths start.
fn file_link(name: &str, input: &Value, cwd: Option<&Path>) -> Option<String> {
    if !matches!(name, "read" | "write" | "edit") {
        return None;
    }
    let path = input.get("path").and_then(Value::as_str)?;
    // An address (`https://`, `agent://`) is not a file.
    if path.contains("://") {
        return None;
    }
    Some(super::terminal::file_url(&cwd?.join(path)))
}

/// The rows of a `read` of a file: the number of the first, each without the `N<tab>` it came
/// with, and the line that says which part of the file this is. `None` for anything else `read`
/// returns.
fn numbered_lines(said: &str) -> Option<(usize, Vec<&str>, Option<&str>)> {
    let mut rows: Vec<&str> = said.split('\n').collect();
    let trailer = rows
        .last()
        .copied()
        .filter(|row| row.starts_with("[lines ") && row.ends_with(']'));
    if trailer.is_some() {
        rows.pop();
    }
    let mut lines = Vec::with_capacity(rows.len());
    let mut first = None;
    for row in rows {
        let (number, line) = row.split_once('\t')?;
        let number: usize = number.parse().ok()?;
        // Numbers that do not follow one another are not line numbers.
        if number != *first.get_or_insert(number) + lines.len() {
            return None;
        }
        lines.push(line);
    }
    Some((first?, lines, trailer))
}

/// The rows of a `read` of a directory (`name/`, `name<tab>N bytes`) and the line that says
/// entries were left out. `None` for anything else `read` returns.
fn directory_entries(said: &str) -> Option<(Vec<code::Entry<'_>>, Option<&str>)> {
    let mut rows: Vec<&str> = said.split('\n').collect();
    let trailer = rows
        .last()
        .copied()
        .filter(|row| row.starts_with('[') && row.ends_with(']') && row.contains(" entries shown"));
    if trailer.is_some() {
        rows.pop();
    }
    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let entry = match row.strip_suffix('/') {
            Some(name) if !name.is_empty() && !name.contains('\t') => code::Entry {
                name,
                dir: true,
                size: None,
            },
            Some(_) => return None,
            None => {
                let (name, size) = row.split_once('\t')?;
                code::Entry {
                    name,
                    dir: false,
                    size: Some(size.strip_suffix(" bytes")?.parse().ok()?),
                }
            }
        };
        entries.push(entry);
    }
    (!entries.is_empty()).then_some((entries, trailer))
}

/// Where `:<digits>:` first stands in a row of `grep`, as the end of the path, the line number
/// and the start of the text. The colon after a drive letter is part of the path.
fn match_row(row: &str) -> Option<(&str, u64, &str)> {
    let bytes = row.as_bytes();
    let drive = bytes.len() > 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    let mut from = if drive { 2 } else { 0 };
    while let Some(at) = row[from..].find(':').map(|at| at + from) {
        let digits = row[at + 1..].bytes().take_while(u8::is_ascii_digit).count();
        let end = at + 1 + digits;
        if digits > 0 && bytes.get(end) == Some(&b':') && at > 0 {
            return Some((&row[..at], row[at + 1..end].parse().ok()?, &row[end + 1..]));
        }
        from = at + 1;
    }
    None
}

/// A row of `grep` that belongs to `path`: a match (`path:N:text`) or, when the search asked
/// for `context`, a line of it (`path-N-text`).
fn row_of<'a>(row: &'a str, path: &str, context: bool) -> Option<code::Hit<'a>> {
    let rest = row.strip_prefix(path)?;
    let mark = rest
        .bytes()
        .next()
        .filter(|mark| *mark == b':' || (context && *mark == b'-'))?;
    let digits = rest[1..].bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || rest.as_bytes().get(1 + digits) != Some(&mark) {
        return None;
    }
    let line = rest[1..=digits].parse().ok()?;
    let text = &rest[digits + 2..];
    Some(match mark {
        b':' => code::Hit::Match { line, text },
        _ => code::Hit::Context { line, text },
    })
}

/// How far ahead a line of context looks for the match that names its file.
const CONTEXT_AHEAD: usize = 32;

/// What `grep` returned as rows under the file each is from. Lines of context are looked for
/// only when the search asked for some: a file name may hold `-N-` too.
fn grep_hits(said: &str, context: bool) -> Vec<code::Hit<'_>> {
    let rows: Vec<&str> = said.split('\n').collect();
    let mut hits = Vec::with_capacity(rows.len() + 8);
    let mut current: Option<&str> = None;
    for (index, row) in rows.iter().enumerate() {
        if *row == "--" {
            hits.push(code::Hit::Gap);
            continue;
        }
        if let Some(hit) = current.and_then(|path| row_of(row, path, context)) {
            hits.push(hit);
            continue;
        }
        // A match in a file not seen yet names it; context before the first match of a file
        // takes the name from the match that follows.
        let found = match match_row(row) {
            Some((path, line, text)) => Some((path, code::Hit::Match { line, text })),
            None => rows[index + 1..]
                .iter()
                .take(CONTEXT_AHEAD)
                .filter_map(|later| match_row(later))
                .find_map(|(path, _, _)| Some((path, row_of(row, path, context)?))),
        };
        match found {
            Some((path, hit)) => {
                current = Some(path);
                hits.push(code::Hit::File(path));
                hits.push(hit);
            }
            None => hits.push(code::Hit::Note(row)),
        }
    }
    hits
}

/// What `web_grep` returned (`N: line`, `N- line`, `--`) as rows of one page.
fn web_grep_hits(said: &str) -> Vec<code::Hit<'_>> {
    said.split('\n')
        .map(|row| {
            if row == "--" {
                return code::Hit::Gap;
            }
            let digits = row.bytes().take_while(u8::is_ascii_digit).count();
            let text = row.get(digits + 1..).unwrap_or_default();
            let text = text.strip_prefix(' ').unwrap_or(text);
            match (row[..digits].parse(), row.as_bytes().get(digits)) {
                (Ok(line), Some(b':')) => code::Hit::Match { line, text },
                (Ok(line), Some(b'-')) => code::Hit::Context { line, text },
                _ => code::Hit::Note(row),
            }
        })
        .collect()
}

/// The text a search pattern stands for when it is plain text with its punctuation escaped
/// (`main\(\)`); the pattern as written otherwise. Found lines are searched for this text, so a
/// pattern that is more than that marks nothing, and none is ever compiled here.
fn literal_of(pattern: &str) -> Cow<'_, str> {
    if !pattern.contains('\\') {
        return Cow::Borrowed(pattern);
    }
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(escaped) if escaped.is_ascii_punctuation() => out.push(escaped),
                _ => return Cow::Borrowed(pattern),
            },
            _ => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// The first `limit` of `rows`, without a file or directory name left alone at the end, and
/// how many rows that leaves out.
fn first_rows<T>(rows: &[T], limit: usize, heads: impl Fn(&T) -> bool) -> (&[T], usize) {
    let mut shown = &rows[..rows.len().min(limit)];
    if shown.len() < rows.len()
        && let [rest @ .., last] = shown
        && heads(last)
    {
        shown = rest;
    }
    (shown, rows.len() - shown.len())
}

/// The line a tool added after its rows (which part of the file this is, that a limit was
/// reached), muted.
fn trailer_row(trailer: &str, inner: usize, theme: &Theme) -> Line {
    let cut = text::truncate(&text::sanitize(trailer), inner, theme.glyph(Glyph::Ellipsis));
    theme.paint(Role::Muted, &cut)
}

/// The body of a call that read or searched files, drawn for what it is: a numbered listing, a
/// tree, hits under their file. Rows beyond `limit` are counted, not drawn; the line the tool
/// closes with is shown when no row before it is left out. `None` when the output is not of a
/// shape known here.
fn shaped_body(
    call: &Call<'_>,
    said: &str,
    limit: usize,
    inner: usize,
    theme: &Theme,
) -> Option<(Vec<Line>, usize)> {
    let field = |key: &str| call.input.get(key).and_then(Value::as_str);
    let flag = |key: &str| call.input.get(key).and_then(Value::as_bool) == Some(true);
    let files = |rows: &[&str]| {
        let (paths, trailer) = match rows {
            [paths @ .., last] if last.starts_with('[') => (paths, Some(*last)),
            paths => (paths, None),
        };
        let grouped = code::by_directory(paths.iter().copied());
        let (shown, hidden) = first_rows(&grouped, limit, |row| matches!(row, code::Found::Dir(_)));
        let mut lines = code::found(shown, inner, theme);
        lines.extend(
            trailer
                .filter(|_| hidden == 0)
                .map(|row| trailer_row(row, inner, theme)),
        );
        (lines, hidden)
    };
    let needle = |literal: bool| {
        let pattern = field("pattern")?;
        Some(match literal {
            true => Cow::Borrowed(pattern),
            false => literal_of(pattern),
        })
    };
    let hits = |hits: Vec<code::Hit<'_>>, text: Option<Cow<'_, str>>| {
        let (shown, hidden) = first_rows(&hits, limit, |row| matches!(row, code::Hit::File(_)));
        let needle = text.as_deref().map(|text| code::Needle {
            text,
            ignore_case: flag("ignore_case"),
        });
        (code::hits(shown, needle, inner, theme), hidden)
    };
    match call.name {
        "read" => {
            if let Some((entries, trailer)) = directory_entries(said) {
                let shown = &entries[..entries.len().min(limit)];
                let more = shown.len() < entries.len() || trailer.is_some();
                let mut lines = code::tree(shown, more, inner, theme);
                let hidden = entries.len() - shown.len();
                lines.extend(
                    trailer
                        .filter(|_| hidden == 0)
                        .map(|row| trailer_row(row, inner, theme)),
                );
                return Some((lines, hidden));
            }
            let (first, rows, trailer) = numbered_lines(said)?;
            let shown = &rows[..rows.len().min(limit)];
            let path = field("path").unwrap_or_default();
            let mut lines = code::listing(&shown.join("\n"), path, first, inner, theme);
            let hidden = rows.len() - shown.len();
            lines.extend(
                trailer
                    .filter(|_| hidden == 0)
                    .map(|row| trailer_row(row, inner, theme)),
            );
            Some((lines, hidden))
        }
        "glob" if said != "no files match" => Some(files(&said.split('\n').collect::<Vec<_>>())),
        "grep" if said != "no matches" => match field("mode") {
            Some("files") => Some(files(&said.split('\n').collect::<Vec<_>>())),
            Some("count") => None,
            _ => {
                let context = call.input.get("context").and_then(Value::as_u64).unwrap_or(0) > 0;
                Some(hits(grep_hits(said, context), needle(false)))
            }
        },
        "web_grep" if !said.starts_with("no matches for ") => {
            Some(hits(web_grep_hits(said), needle(flag("literal"))))
        }
        _ => None,
    }
}

/// How many characters a body that was cut short has in all: `812 chars`, `4.1k chars`.
fn size_fact(said: &str) -> String {
    format!("{} chars", format_tokens(said.chars().count() as u64))
}

/// A tool call read for display: what its card or its compact row is made of.
pub struct Drawn {
    state: CardState,
    name: String,
    summary: String,
    link: Option<String>,
    body: Vec<Line>,
    facts: Vec<String>,
}

impl Drawn {
    fn card(&self) -> Card<'_> {
        Card {
            state: self.state,
            name: &self.name,
            summary: &self.summary,
            link: self.link.as_deref(),
            body: &self.body,
            facts: &self.facts,
        }
    }
}

/// Reads a finished call for display with `inner` columns for its body. A card shows the start
/// of the output, drawn for what it is; a compact row only a diff, and says the rest in facts.
fn draw(call: &Call<'_>, show: Show<'_>, inner: usize, compact: bool, theme: &Theme) -> Drawn {
    let output = call.output;
    let hide = |text: String| redacted(text, show.redact);
    let said = hide(output_text(output));
    let state = if !output.is_error {
        CardState::Success
    } else if NOT_RUN.iter().any(|start| said.starts_with(start)) {
        CardState::Warning
    } else {
        CardState::Error
    };
    let details = output.details.as_ref();
    let exit = details.and_then(|d| d.get("exit_code")).and_then(Value::as_i64);
    let diff_text = details
        .and_then(|d| d.get("diff"))
        .and_then(Value::as_str)
        .filter(|_| !output.is_error)
        .map(without_file_header)
        .unwrap_or_default();
    let mut facts: Vec<String> = Vec::new();
    let (mut body, hidden) = if !diff_text.is_empty() {
        let limit = if show.expanded {
            usize::MAX
        } else {
            COLLAPSED_DIFF_LINES
        };
        let shown = match show.redact {
            true => text::redact(diff_text),
            false => Cow::Borrowed(diff_text),
        };
        let mut rows = diff::render_unified(&shown, inner, theme);
        let hidden = rows.len().saturating_sub(limit);
        rows.truncate(limit);
        (rows, hidden)
    } else if compact {
        // The row says how much there was, or what went wrong.
        let first = said.lines().find(|line| !line.trim().is_empty());
        match (state, first) {
            (CardState::Success, _) => match said.lines().count() {
                0 => {}
                1 => facts.push("1 line".to_string()),
                lines => facts.push(format!("{lines} lines")),
            },
            (_, Some(why)) => facts.push(why.trim().to_string()),
            (_, None) => {}
        }
        (Vec::new(), 0)
    } else {
        let limit = if show.expanded || output.is_error {
            EXPANDED_LINES
        } else {
            COLLAPSED_LINES
        };
        // The border of the card says how a command ended; its output need not repeat it.
        let said = if exit.is_some() {
            without_exit_line(&said)
        } else {
            &said
        };
        let shaped = match state {
            CardState::Success => shaped_body(call, said, limit, inner, theme),
            _ => None,
        };
        let (rows, hidden) = shaped.unwrap_or_else(|| {
            let role = match state {
                CardState::Error => Role::Error,
                CardState::Warning => Role::Warning,
                _ => Role::Muted,
            };
            let clean = text::sanitize(said);
            let ellipsis = theme.glyph(Glyph::Ellipsis);
            let rows: Vec<Line> = clean
                .lines()
                .take(limit)
                .map(|line| theme.paint(role, &text::truncate(line, inner, ellipsis)))
                .collect();
            let hidden = clean.lines().count() - rows.len();
            (rows, hidden)
        });
        if hidden > 0 {
            facts.push(size_fact(said));
        }
        (rows, hidden)
    };
    if hidden > 0 {
        let more = format!(
            "{} {hidden} more line{} (ctrl+o to expand)",
            theme.glyph(Glyph::Ellipsis),
            if hidden == 1 { "" } else { "s" }
        );
        body.push(theme.paint(Role::Muted, &more));
    }

    // How long it took and how it ended come before what the body adds.
    let mut ended: Vec<String> = call.elapsed.map(format_duration).into_iter().collect();
    if details.and_then(|d| d.get("timed_out")).and_then(Value::as_bool) == Some(true) {
        ended.push("timed out".to_string());
    } else if let Some(code) = exit {
        ended.push(format!("exit {code}"));
    }
    // In a row, the count of lines leads; what went wrong closes it.
    if compact && state == CardState::Success {
        facts.extend(ended);
    } else {
        facts.splice(0..0, ended);
    }
    Drawn {
        state,
        name: call.label().into_owned(),
        summary: hide(tool_summary(call.name, call.input)),
        link: file_link(call.name, call.input, show.cwd),
        body,
        facts,
    }
}

/// A finished tool call as a card: its diff or the start of its output inside, how long it took
/// and how it ended in the bottom border. What `read`, `grep` and `glob` return is drawn as a
/// listing, as hits under their file and as files under their directory.
pub fn tool_card(call: Call<'_>, show: Show<'_>, width: usize, theme: &Theme) -> Vec<Line> {
    let inner = tool_card::inner_width(width).max(1);
    tool_card::render(&draw(&call, show, inner, false, theme).card(), width, theme)
}

/// A finished tool call for the compact style, drawn for rows `width` columns wide: what it
/// was about and how it went on its row, and only a diff under it.
pub fn tool_row(call: Call<'_>, show: Show<'_>, width: usize, theme: &Theme) -> Drawn {
    let show = Show {
        expanded: false,
        ..show
    };
    let inner = tool_card::compact_inner_width(width).max(1);
    draw(&call, show, inner, true, theme)
}

/// A call in flight for the compact style: a spinner, how long it has been running and the
/// last lines it printed under its row.
pub fn running_row(
    name: &str,
    input: &Value,
    tail: &[String],
    elapsed: Duration,
    redact: bool,
    theme: &Theme,
) -> Drawn {
    let from = tail.len().saturating_sub(tool_card::MAX_OUTPUT_LINES);
    let body = tail[from..]
        .iter()
        .map(|line| {
            let line = match redact {
                true => text::redact(line),
                false => Cow::Borrowed(line.as_str()),
            };
            theme.paint(Role::Muted, &text::sanitize(&line).replace('\n', " "))
        })
        .collect();
    Drawn {
        state: CardState::Running,
        name: name.to_string(),
        summary: redacted(tool_summary(name, input), redact),
        link: None,
        body,
        facts: vec![format_elapsed(elapsed)],
    }
}

/// Calls of the compact style as rows: one alone, or several as a tree under a row that counts
/// them.
pub fn tool_rows<'a>(calls: impl IntoIterator<Item = &'a Drawn>, width: usize, theme: &Theme) -> Vec<Line> {
    let cards: Vec<Card<'_>> = calls.into_iter().map(Drawn::card).collect();
    tool_card::compact(&cards, width, theme)
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
            tool_summary("run_code", &json!({"code": "\nlocal a = 1\nreturn a\n"})),
            "Luau script, 2 lines"
        );
        assert_eq!(
            tool_summary("grep", &json!({"pattern": "fn main", "path": "src"})),
            "fn main"
        );
        assert_eq!(tool_summary("todo", &json!({"items": [1, 2]})), "2 items");
        let lsp = |input: Value| tool_summary("lsp", &input);
        assert_eq!(
            lsp(json!({"action": "outline", "path": "src/a.rs"})),
            "outline src/a.rs"
        );
        assert_eq!(
            lsp(json!({"action": "read_symbol", "path": "src/a.rs", "symbol": "Config.load"})),
            "read_symbol Config.load in src/a.rs"
        );
        assert_eq!(
            lsp(json!({"action": "definition", "path": "src/a.rs", "line": 12, "column": 20})),
            "definition src/a.rs:12:20"
        );
        assert_eq!(
            lsp(json!({"action": "rename", "path": "src/a.rs", "line": 8, "new_name": "load"})),
            "rename src/a.rs:8 to load"
        );
        assert_eq!(
            lsp(json!({"action": "rename_file", "path": "src/a.rs", "new_path": "src/b/mod.rs"})),
            "rename_file src/a.rs to src/b/mod.rs"
        );
        assert_eq!(
            lsp(json!({"action": "symbols", "path": "src/a.rs", "query": "parse"})),
            "symbols parse"
        );
        let ast_grep = |input: Value| tool_summary("ast_grep", &input);
        assert_eq!(
            ast_grep(json!({"action": "search", "pattern": "log($MSG)", "paths": ["src"]})),
            "log($MSG) in src"
        );
        assert_eq!(
            ast_grep(json!({"action": "replace", "pattern": "log($MSG)", "rewrite": "print($MSG)"})),
            "preview log($MSG) with print($MSG)"
        );
        assert_eq!(
            ast_grep(json!({
                "action": "replace", "pattern": "log($MSG)", "rewrite": "print($MSG)",
                "apply": true, "paths": ["src/a.ts", "src/b.ts", "src/c.ts"]
            })),
            "replace log($MSG) with print($MSG) in src/a.ts and 2 more"
        );
        assert_eq!(
            tool_summary(
                "ask_user",
                &json!({"question": "Which colour?", "options": ["red", "blue", "green"]})
            ),
            "Which colour? (3 options)"
        );
        assert_eq!(
            tool_summary("ask_user", &json!({"question": "Paint it?", "type": "confirm"})),
            "Paint it?"
        );
        let recall = |input: Value| tool_summary("session_recall", &input);
        assert_eq!(
            recall(json!({"query": "parser tests", "limit": 5})),
            "parser tests"
        );
        assert_eq!(
            recall(json!({"regex": "fn \\w+", "touched": true})),
            "/fn \\w+/ · files touched"
        );
        assert_eq!(recall(json!({"expand": ["1a2b3c4d"]})), "entry 1a2b3c4d");
        assert_eq!(
            recall(json!({"expand": ["1a2b3c4d", "5e6f7a8b"], "range": ["", "9c0d1e2f"]})),
            "start to 9c0d1e2f · 2 entries"
        );
        assert_eq!(
            tool_summary(
                "web_grep",
                &json!({"url": "https://docs.rs/tokio", "pattern": "select!"})
            ),
            "select! in https://docs.rs/tokio"
        );
        assert_eq!(tool_summary("web_search", &json!({"query": "one"})), "one");
        assert_eq!(
            tool_summary(
                "web_search",
                &json!({"query": "one", "queries": ["two", "three"]})
            ),
            "one | two | three"
        );
        assert_eq!(tool_summary("web_search", &json!({"queries": ["two"]})), "two");
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

    #[test]
    fn the_banner_is_block_letters_or_thin_ascii_by_glyph_level_and_width() {
        let unicode = Theme::dark();
        let wide = plain(&banner(100, &unicode));
        assert_eq!(wide.len(), 9, "{wide:?}");
        assert!(wide[1].contains("█████╗"), "{wide:?}");
        assert_eq!(wide[7], format!(" {TAGLINE}"));
        // The same letters with a Nerd Font; thin ones where the terminal is narrow.
        let nerd = Theme::dark().with_glyphs(GlyphLevel::Nerd);
        assert_eq!(plain(&banner(100, &nerd)), wide);
        let narrow = plain(&banner(45, &unicode));
        assert_eq!(narrow.len(), 8, "{narrow:?}");
        assert!(narrow[2].contains("____ _____"), "{narrow:?}");

        // Plain ASCII asks for thin letters however wide the terminal is.
        let ascii = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        assert_eq!(plain(&banner(100, &ascii)), narrow);
        for width in 0..120 {
            for line in plain(&banner(width, &ascii)) {
                assert!(line.is_ascii(), "width {width}: {line:?}");
                assert!(text::width(&line) <= width, "width {width}: {line:?}");
            }
            for line in banner(width, &unicode) {
                assert!(text::width(&line) <= width, "width {width}: {line:?}");
            }
        }
        assert!(banner(30, &unicode).is_empty(), "too narrow for either");
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

    const LEVELS: [GlyphLevel; 3] = [GlyphLevel::Unicode, GlyphLevel::Nerd, GlyphLevel::Ascii];

    fn call<'a>(name: &'a str, input: &'a Value, output: &'a ToolOutput) -> Call<'a> {
        Call {
            agent: None,
            name,
            input,
            output,
            elapsed: None,
        }
    }

    fn timed<'a>(name: &'a str, input: &'a Value, output: &'a ToolOutput, millis: u64) -> Call<'a> {
        Call {
            elapsed: Some(Duration::from_millis(millis)),
            ..call(name, input, output)
        }
    }

    const OPEN: Show<'static> = Show {
        expanded: true,
        redact: true,
        cwd: None,
    };
    const SHUT: Show<'static> = Show {
        expanded: false,
        redact: true,
        cwd: None,
    };

    /// Finished calls in the compact style.
    fn rows(calls: &[Call<'_>], show: Show<'_>, width: usize, theme: &Theme) -> Vec<Line> {
        let drawn: Vec<Drawn> = calls
            .iter()
            .map(|call| tool_row(*call, show, width, theme))
            .collect();
        tool_rows(&drawn, width, theme)
    }

    /// The rows inside the card of a call, without its frame.
    fn body(name: &str, input: Value, said: &str, show: Show<'_>) -> Vec<String> {
        let output = ToolOutput::text(said);
        let lines = plain(&tool_card(call(name, &input, &output), show, 64, &Theme::dark()));
        inside(&lines[1..lines.len() - 1])
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
        let collapsed = plain(&tool_card(timed("shell", &input, &long, 2040), SHUT, 44, &theme));
        assert!(collapsed[0].starts_with("╭─── ✔ shell seq 10 ─"), "{collapsed:?}");
        assert!(collapsed[0].ends_with('╮'));
        assert_eq!(
            inside(&collapsed[1..5]),
            ["line 1", "line 2", "line 3", "… 7 more lines (ctrl+o to expand)"]
        );
        // A body that was cut says how much there is of it.
        assert!(
            collapsed[5].starts_with("╰── 2.0s · exit 0 · 70 chars ─"),
            "{collapsed:?}"
        );
        assert_eq!(collapsed.len(), 6);
        for row in &collapsed {
            assert_eq!(text::width(row), 44, "{row:?}");
        }
        let expanded = plain(&tool_card(timed("shell", &input, &long, 2000), OPEN, 44, &theme));
        assert_eq!(expanded.len(), 12);
        assert!(expanded[11].starts_with("╰── 2.0s · exit 0 ─"), "{expanded:?}");

        // The exit code is in the border, so the line of output that repeats it is left out.
        let ran = ToolOutput::text("built\nexit code: 3").with_details(json!({"exit_code": 3}));
        let shown = plain(&tool_card(timed("shell", &input, &ran, 0), SHUT, 44, &theme));
        assert_eq!(inside(&shown[1..shown.len() - 1]), ["built"]);
        assert!(shown[2].starts_with("╰── 0ms · exit 3 ─"), "{shown:?}");

        let failed = ToolOutput::error("boom\nwhy");
        let shown = plain(&tool_card(timed("shell", &input, &failed, 0), SHUT, 44, &theme));
        assert!(shown[0].starts_with("╭─── ✘ shell"), "{shown:?}");
        assert_eq!(inside(&shown[1..3]), ["boom", "why"]);
        assert!(shown[3].starts_with("╰── 0ms ─"), "{shown:?}");

        // Interrupted or refused is not the same as failed.
        let aborted = ToolOutput::error("aborted by user");
        let shown = plain(&tool_card(timed("shell", &input, &aborted, 0), SHUT, 44, &theme));
        assert!(shown[0].starts_with("╭─── ⚠ shell"), "{shown:?}");

        let edited = ToolOutput::text("edited a.rs")
            .with_details(json!({"diff": "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n"}));
        let path = json!({"path": "a.rs"});
        let shown = plain(&tool_card(timed("edit", &path, &edited, 0), SHUT, 44, &theme));
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

        // A sub-agent's call names the agent, and is read like any other call of that tool.
        let read = ToolOutput::text("1\tone");
        let by_agent = Call {
            agent: Some("scout"),
            ..call("read", &path, &read)
        };
        let shown = plain(&tool_card(by_agent, SHUT, 44, &theme));
        assert!(shown[0].starts_with("╭─── ✔ [scout] read a.rs ─"), "{shown:?}");
        assert_eq!(inside(&shown[1..2]), ["  1 │ one"]);
    }

    #[test]
    fn a_file_that_was_read_is_a_numbered_listing() {
        let part = "4\tfn a() {\n5\t\tb();\n6\t}\n[lines 4-6 of 10; continue with offset=7]";
        let input = json!({"path": "src/a.rs", "offset": 4, "limit": 3});
        // The numbers are the file's own; the line that says which part this is closes the card.
        assert_eq!(
            body("read", input.clone(), part, SHUT),
            [
                "  4 │ fn a() {",
                "  5 │     b();",
                "  6 │ }",
                "[lines 4-6 of 10; continue with offset=7]"
            ]
        );
        // The code is coloured as Rust; the same rows under another name are not.
        let output = ToolOutput::text(part);
        let theme = Theme::dark();
        let uncoloured = format!("{} fn a() {{", theme.paint(Role::Border, "│"));
        let coloured = tool_card(call("read", &input, &output), SHUT, 64, &theme);
        assert!(!coloured[1].contains(&uncoloured), "{:?}", coloured[1]);
        let unnamed = json!({"path": "notes"});
        let bare = tool_card(call("read", &unnamed, &output), SHUT, 64, &theme);
        assert!(bare[1].contains(&uncoloured), "{:?}", bare[1]);

        let whole: String = (1..=5).map(|i| format!("{i}\tline {i}\n")).collect();
        assert_eq!(
            body("read", json!({"path": "a.txt"}), whole.trim_end(), SHUT),
            [
                "  1 │ line 1",
                "  2 │ line 2",
                "  3 │ line 3",
                "… 2 more lines (ctrl+o to expand)"
            ]
        );
        assert_eq!(
            body("read", json!({"path": "a.txt"}), whole.trim_end(), OPEN).len(),
            5
        );

        // Anything else `read` returns is shown as it came.
        for other in ["(empty file)", "1\tone\n3\tthree", "page 1\nsome text"] {
            let shown = body("read", json!({"path": "a.txt"}), other, OPEN);
            assert_eq!(
                shown,
                text::sanitize(other).lines().collect::<Vec<_>>(),
                "{other:?}"
            );
        }
        assert_eq!(
            numbered_lines("7\ta\n8\t\n9\tc\td"),
            Some((7, vec!["a", "", "c\td"], None))
        );
        assert_eq!(numbered_lines("7\ta\n9\tc"), None);
    }

    #[test]
    fn a_directory_that_was_read_is_a_tree() {
        let listing = "src/\ntarget/\nCargo.toml\t1234 bytes\nREADME.md\t99 bytes";
        let input = json!({"path": "."});
        assert_eq!(
            body("read", input.clone(), listing, OPEN),
            [
                "├─ src/",
                "├─ target/",
                "├─ Cargo.toml  1.2 KB",
                "└─ README.md   99 B"
            ]
        );
        // Cut short, the tree stays open and the rest is counted.
        assert_eq!(
            body("read", input.clone(), listing, SHUT),
            [
                "├─ src/",
                "├─ target/",
                "├─ Cargo.toml  1.2 KB",
                "… 1 more line (ctrl+o to expand)"
            ]
        );
        let capped = "a/\nb\t1 bytes\n[200 of 900 entries shown; use glob to narrow down]";
        assert_eq!(
            body("read", input.clone(), capped, OPEN),
            [
                "├─ a/",
                "├─ b   1 B",
                "[200 of 900 entries shown; use glob to narrow down]"
            ]
        );
        assert_eq!(
            body("read", input, "(empty directory)", OPEN),
            ["(empty directory)"]
        );
        // A file whose lines end in a slash is still a file.
        assert!(directory_entries("1\tsrc/\n2\tdocs/").is_none());
    }

    #[test]
    fn search_hits_stand_under_their_file() {
        let found = "src/a.rs-1-// top\nsrc/a.rs:2:fn main() {\nsrc/a.rs-3-}\n--\nsrc/a.rs:9:main();\n\
                     src/b-1.rs:4:call(main)\n[limit of 3 reached; narrow the search or raise limit]";
        let input = json!({"pattern": "main", "context": 1});
        assert_eq!(
            body("grep", input.clone(), found, OPEN),
            [
                "src/a.rs",
                "  1 │ // top",
                "  2 │ fn main() {",
                "  3 │ }",
                "  …",
                "  9 │ main();",
                "src/b-1.rs",
                "  4 │ call(main)",
                "[limit of 3 reached; narrow the search or raise limit]"
            ]
        );
        assert_eq!(
            body("grep", input.clone(), found, SHUT),
            [
                "src/a.rs",
                "  1 │ // top",
                "  2 │ fn main() {",
                "… 6 more lines (ctrl+o to expand)"
            ]
        );
        // What was searched for stands out where it is found as written.
        let theme = Theme::dark();
        let marked = format!("{}\x1b[1mmain", theme.fg(Role::Warning));
        let output = ToolOutput::text(found);
        let card = tool_card(call("grep", &input, &output), OPEN, 64, &theme);
        assert!(card[3].contains(&marked) && card[8].contains(&marked), "{card:?}");
        assert!(
            !card[2].contains("\x1b[1m"),
            "context is not marked: {:?}",
            card[2]
        );
        let escaped = json!({"pattern": "main\\(\\)"});
        let card = tool_card(call("grep", &escaped, &output), OPEN, 64, &theme);
        assert!(card[3].contains(&format!("{marked}()")), "{:?}", card[3]);
        let pattern = json!({"pattern": "ma\\w+"});
        let card = tool_card(call("grep", &pattern, &output), OPEN, 64, &theme);
        assert!(
            !card[3].contains("\x1b[1m"),
            "a pattern is not run: {:?}",
            card[3]
        );

        // Files and counts, and nothing found.
        assert_eq!(
            body(
                "grep",
                json!({"pattern": "x", "mode": "files"}),
                "src/a.rs\nsrc/b.rs\nc.rs",
                OPEN
            ),
            ["src/", "  a.rs", "  b.rs", "./", "  c.rs"]
        );
        assert_eq!(
            body(
                "grep",
                json!({"pattern": "x", "mode": "count"}),
                "src/a.rs:2\nc.rs:1",
                OPEN
            ),
            ["src/a.rs:2", "c.rs:1"]
        );
        assert_eq!(
            body("grep", json!({"pattern": "x"}), "no matches", OPEN),
            ["no matches"]
        );

        // A drive letter is part of the path; text may hold what looks like a line number.
        assert_eq!(
            match_row("C:\\work\\a.rs:12:let x = y:3:z;"),
            Some(("C:\\work\\a.rs", 12, "let x = y:3:z;"))
        );
        assert_eq!(match_row("no line number here: none"), None);
        // Without context asked for, a file named like a line of context is a file.
        assert_eq!(
            body(
                "grep",
                json!({"pattern": "x"}),
                "src/a:1:x\nsrc/a-2-b.rs:5:x",
                OPEN
            ),
            ["src/a", "  1 │ x", "src/a-2-b.rs", "  5 │ x"]
        );
        assert_eq!(literal_of("a\\.b\\(c"), "a.b(c");
        assert_eq!(literal_of("\\bword"), "\\bword");
    }

    #[test]
    fn found_files_and_page_hits_are_grouped_too() {
        assert_eq!(
            body(
                "glob",
                json!({"pattern": "*.rs"}),
                "src/tui/mod.rs\nsrc/main.rs\nsrc/tui/view.rs\nCargo.toml\n[1000 of 1200 files shown; narrow the pattern]",
                OPEN
            ),
            [
                "src/tui/",
                "  mod.rs",
                "  view.rs",
                "src/",
                "  main.rs",
                "./",
                "  Cargo.toml",
                "[1000 of 1200 files shown; narrow the pattern]"
            ]
        );
        // A directory is not left without a file under it where the card ends.
        assert_eq!(
            body("glob", json!({"pattern": "*"}), "a/1\na/2\nb/3", SHUT),
            ["a/", "  1", "  2", "… 2 more lines (ctrl+o to expand)"]
        );
        assert_eq!(
            body("glob", json!({"pattern": "*"}), "no files match", OPEN),
            ["no files match"]
        );

        let input = json!({"url": "https://docs.example", "pattern": "BETA", "ignore_case": true});
        let page = "1- alpha\n2: beta one\n3-\n--\n19: the beta";
        assert_eq!(
            body("web_grep", input.clone(), page, OPEN),
            ["  1 │ alpha", "  2 │ beta one", "  3 │", "  …", " 19 │ the beta"]
        );
        let output = ToolOutput::text(page);
        let theme = Theme::dark();
        let card = tool_card(call("web_grep", &input, &output), OPEN, 64, &theme);
        assert!(
            card[2].contains(&format!("{}\x1b[1mbeta", theme.fg(Role::Warning))),
            "{:?}",
            card[2]
        );
    }

    #[test]
    fn compact_calls_are_rows_and_several_are_a_tree() {
        let theme = Theme::dark();
        let read = ToolOutput::text("1\ta\n2\tb");
        let path = json!({"path": "src/a.rs"});
        let one = [timed("read", &path, &read, 3)];
        assert_eq!(
            plain(&rows(&one, SHUT, 60, &theme)),
            ["✔ read (src/a.rs) · 2 lines · 3ms"]
        );

        let edited = ToolOutput::text("edited a.rs")
            .with_details(json!({"diff": "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n"}));
        let failed = ToolOutput::error("\nboom\nexit code: 1").with_details(json!({"exit_code": 1}));
        let command = json!({"command": "cargo test"});
        let calls = [
            timed("read", &path, &read, 3),
            timed("edit", &path, &edited, 12),
            timed("shell", &command, &failed, 1400),
        ];
        let tree: Vec<String> = plain(&rows(&calls, SHUT, 60, &theme))
            .iter()
            .map(|row| row.trim_end().to_string())
            .collect();
        assert_eq!(
            tree,
            [
                "✘ 3 tool calls · 1 failed",
                "├─ ✔ read (src/a.rs) · 2 lines · 3ms",
                "├─ ✔ edit (src/a.rs) · 12ms",
                "│  @@ -1 +1 @@",
                "│  1 -old",
                "│  1 +new",
                "└─ ✘ shell (cargo test) · 1.4s · exit 1 · boom",
            ]
        );
        // A call still running joins the tree with a spinner, its clock and what it printed;
        // while one runs, the tree shows that.
        let tail: Vec<String> = (1..=7).map(|i| format!("line {i}")).collect();
        let live = running_row("shell", &command, &tail, Duration::from_secs(65), true, &theme);
        let done = tool_row(calls[0], SHUT, 60, &theme);
        let tree = plain(&tool_rows([&done, &live], 60, &theme));
        assert_eq!(tree.len(), 8, "{tree:?}");
        assert!(
            tree[0].ends_with(" 2 tool calls") && !tree[0].starts_with('✔'),
            "{tree:?}"
        );
        assert_eq!(tree[1], "├─ ✔ read (src/a.rs) · 2 lines · 3ms");
        assert!(
            tree[2].starts_with("└─ ") && tree[2].ends_with(" shell (cargo test) · 1m05s"),
            "{tree:?}"
        );
        assert_eq!(
            tree[3..],
            ["   line 3", "   line 4", "   line 5", "   line 6", "   line 7"]
        );

        // A diff under a call that stands alone is set in as far.
        let alone = plain(&rows(&calls[1..2], SHUT, 60, &theme));
        assert_eq!(alone[0], "✔ edit (src/a.rs) · 12ms");
        assert_eq!(alone[2].trim_end(), "   1 -old");
    }

    #[test]
    fn secrets_are_hidden_in_what_is_shown_only() {
        let theme = Theme::dark();
        let key = "sk-ant-api03-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6";
        let output = ToolOutput::text(format!("1\tANTHROPIC_API_KEY={key}\n2\tPORT=8080"));
        let input = json!({"path": ".env"});
        let shown = plain(&tool_card(call("read", &input, &output), SHUT, 64, &theme));
        assert_eq!(
            inside(&shown[1..3]),
            [
                "  1 │ ANTHROPIC_API_KEY=[redacted:anthropic-key]",
                "  2 │ PORT=8080"
            ]
        );
        assert!(
            output.text_content().contains(key),
            "the result itself is untouched"
        );
        let open = Show {
            redact: false,
            ..SHUT
        };
        let shown = plain(&tool_card(call("read", &input, &output), open, 100, &theme));
        assert!(shown[1].contains(key), "{shown:?}");

        // The command a call ran, its live output, a diff and a compact row are shown the same.
        let token = "Zq9xW3".repeat(4);
        let command = json!({"command": format!("curl -H 'Authorization: Bearer {token}' x")});
        let ran = ToolOutput::text("ok");
        let header = &plain(&tool_card(call("shell", &command, &ran), SHUT, 90, &theme))[0];
        assert!(
            header.contains("Bearer [redacted:bearer]") && !header.contains(&token),
            "{header}"
        );
        let tail = [format!("token={key}")];
        let live = plain(&tool_running(
            "shell",
            &command,
            &tail,
            Duration::ZERO,
            true,
            90,
            &theme,
        ));
        assert!(!live.iter().any(|row| row.contains(key)), "{live:?}");
        assert!(live[1].contains("token=[redacted:anthropic-key]"), "{live:?}");
        let live = plain(&tool_running(
            "shell",
            &command,
            &tail,
            Duration::ZERO,
            false,
            90,
            &theme,
        ));
        assert!(live[1].contains(key), "{live:?}");
        let edited = ToolOutput::text("edited .env").with_details(
            json!({"diff": format!("--- a/.env\n+++ b/.env\n@@ -1 +1 @@\n-KEY=\n+API_KEY={key}\n")}),
        );
        let waiting = running_row("shell", &command, &tail, Duration::ZERO, true, &theme);
        for shown in [
            tool_card(call("edit", &input, &edited), SHUT, 90, &theme),
            rows(&[call("edit", &input, &edited)], SHUT, 90, &theme),
            rows(&[call("shell", &command, &ran)], SHUT, 90, &theme),
            tool_rows([&waiting], 90, &theme),
        ] {
            let shown = plain(&shown);
            assert!(
                !shown.iter().any(|row| row.contains(key) || row.contains(&token)),
                "{shown:?}"
            );
        }
        assert_eq!(redacted(format!("x {key}"), true), "x [redacted:anthropic-key]");
        assert_eq!(redacted(format!("x {key}"), false), format!("x {key}"));
    }

    #[test]
    fn cards_and_rows_fit_every_width_at_every_glyph_level() {
        let read = ToolOutput::text("1\tfn main() {\n2\t\tprintln!(\"hello\");\n3\t}\n[lines 1-3 of 9]");
        let dir = ToolOutput::text("src/\nCargo.toml\t1234 bytes");
        let grep = ToolOutput::text("src/a.rs-1-// top\nsrc/a.rs:2:fn main() {\n--\nsrc/b.rs:4:main");
        let glob = ToolOutput::text("src/tui/mod.rs\nsrc/main.rs\nCargo.toml");
        let page = ToolOutput::text("1- alpha\n2: beta\n--\n9: beta");
        let edited = ToolOutput::text("edited")
            .with_details(json!({"diff": "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n"}));
        let failed = ToolOutput::error("boom").with_details(json!({"exit_code": 1}));
        let path = json!({"path": "src/main.rs"});
        let here = json!({"path": "."});
        let pattern = json!({"pattern": "main"});
        let web = json!({"url": "https://docs.example", "pattern": "beta"});
        let command = json!({"command": "cargo test --workspace"});
        let calls = [
            timed("read", &path, &read, 3),
            timed("read", &here, &dir, 1),
            timed("grep", &pattern, &grep, 8),
            timed("glob", &pattern, &glob, 8),
            timed("web_grep", &web, &page, 120),
            timed("edit", &path, &edited, 12),
            timed("shell", &command, &failed, 1400),
        ];
        for level in LEVELS {
            let theme = Theme::dark().with_glyphs(level);
            for width in 20..=160 {
                let mut rows = rows(&calls, SHUT, width, &theme);
                rows.extend(self::rows(&calls[..1], SHUT, width, &theme));
                let tail = ["Compiling azazel v0.3.0".to_string()];
                let live = running_row("shell", &command, &tail, Duration::from_secs(3), true, &theme);
                rows.extend(tool_rows([&live], width, &theme));
                for call in calls {
                    for show in [SHUT, OPEN] {
                        let card = tool_card(call, show, width, &theme);
                        for row in &card {
                            assert_eq!(text::width(row), width, "{level} {}: {row:?}", call.name);
                        }
                        rows.extend(card);
                    }
                }
                for row in &rows {
                    assert!(text::width(row) <= width, "{level}, width {width}: {row:?}");
                    if level == GlyphLevel::Ascii {
                        assert!(text::strip_ansi(row).is_ascii(), "width {width}: {row:?}");
                    }
                }
            }
        }
    }
}
