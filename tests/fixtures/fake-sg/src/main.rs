//! A stand-in for the ast-grep program (`ast-grep`, `sg`) for tests. It is run as ast-grep is:
//!
//! ```text
//! fake-sg run --pattern=<p> [--rewrite=<r>] [--lang=<l>] --json=stream [paths...]
//! ```
//!
//! and prints what ast-grep prints with `--json=stream`: one match per line, as the JSON object
//! its documentation gives (<https://ast-grep.github.io/guide/tools/json.html>), with zero-based
//! lines, columns and byte offsets, `replacement` and `replacementOffsets` when there is a
//! `--rewrite`, and exit code 0 when something matched, 1 when nothing did.
//!
//! It knows no syntax. A pattern is text in which `$NAME` stands for one word (letters, digits,
//! `_` and `.`) and `$$$NAME` for anything up to the text that follows it in the pattern. Files
//! are searched last first, as a reminder that ast-grep prints in no promised order. `--lang`
//! limits the search to the files of that language (`rust` is `.rs`, `typescript` `.ts`,
//! `python` `.py`, anything else its own name), and an unknown one, `nope`, is refused the way
//! a real option error is. The pattern `FAIL` fails the way a pattern that cannot be parsed
//! does: a line on standard error and exit code 1. `--version` prints `ast-grep 0.0.0-fake`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Map, Value, json};

enum Part {
    Text(String),
    One(String),
    Many(String),
}

fn is_name(c: char) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.'
}

/// `pattern` as literal text and metavariables.
fn parts(pattern: &str) -> Vec<Part> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut rest = pattern;
    while let Some(c) = rest.chars().next() {
        let (dollars, many) = if rest.starts_with("$$$") {
            (3, true)
        } else {
            (1, false)
        };
        let name: String = rest[dollars.min(rest.len())..]
            .chars()
            .take_while(|c| is_name(*c))
            .collect();
        if c == '$' && !name.is_empty() {
            if !text.is_empty() {
                out.push(Part::Text(std::mem::take(&mut text)));
            }
            rest = &rest[dollars + name.len()..];
            out.push(if many { Part::Many(name) } else { Part::One(name) });
        } else {
            text.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    if !text.is_empty() {
        out.push(Part::Text(text));
    }
    out
}

/// A metavariable's name and the bytes of the file it stands for.
type Capture = (String, bool, usize, usize);

/// Where a match of `parts` that starts at byte `at` of `text` ends, and what it captured.
fn match_at(text: &str, at: usize, parts: &[Part]) -> Option<(usize, Vec<Capture>)> {
    let mut end = at;
    let mut captures = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        let rest = &text[end..];
        match part {
            Part::Text(literal) => {
                if !rest.starts_with(literal.as_str()) {
                    return None;
                }
                end += literal.len();
            }
            Part::One(name) => {
                let length: usize = rest.chars().take_while(|c| is_word(*c)).map(char::len_utf8).sum();
                if length == 0 {
                    return None;
                }
                captures.push((name.clone(), false, end, end + length));
                end += length;
            }
            Part::Many(name) => {
                let length = match parts.get(index + 1) {
                    Some(Part::Text(next)) => rest.find(next.as_str())?,
                    _ => rest.find('\n').unwrap_or(rest.len()),
                };
                captures.push((name.clone(), true, end, end + length));
                end += length;
            }
        }
    }
    (end > at).then_some((end, captures))
}

fn point(text: &str, offset: usize) -> Value {
    let before = &text[..offset];
    let line = before.matches('\n').count();
    let column = before.rsplit('\n').next().map_or(0, |last| last.chars().count());
    json!({ "line": line, "column": column })
}

fn range(text: &str, start: usize, end: usize) -> Value {
    json!({
        "byteOffset": { "start": start, "end": end },
        "start": point(text, start),
        "end": point(text, end)
    })
}

fn files_below(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        out.push(path.to_path_buf());
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if !hidden {
            files_below(&entry.path(), out);
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--version") {
        println!("ast-grep 0.0.0-fake");
        return ExitCode::SUCCESS;
    }
    let option = |name: &str| {
        args.iter()
            .find_map(|arg| arg.strip_prefix(name)?.strip_prefix('='))
            .map(str::to_string)
    };
    let (Some(pattern), true) = (
        option("--pattern"),
        args.first().is_some_and(|first| first == "run"),
    ) else {
        eprintln!("error: the following required arguments were not provided: --pattern <PATTERN>");
        return ExitCode::from(2);
    };
    if pattern == "FAIL" {
        eprintln!("Error: Cannot parse query as a valid pattern.");
        return ExitCode::from(1);
    }
    let rewrite = option("--rewrite");
    let extension = match option("--lang").as_deref() {
        Some("nope") => {
            eprintln!("error: invalid value 'nope' for '--lang <LANG>': nope is not supported!");
            return ExitCode::from(2);
        }
        Some("rust") => Some("rs".to_string()),
        Some("typescript") => Some("ts".to_string()),
        Some("python") => Some("py".to_string()),
        other => other.map(str::to_string),
    };
    let mut paths: Vec<&str> = args[1..]
        .iter()
        .filter(|arg| !arg.starts_with("--"))
        .map(String::as_str)
        .collect();
    if paths.is_empty() {
        paths.push(".");
    }
    let mut files = Vec::new();
    for path in paths {
        files_below(Path::new(path), &mut files);
    }
    files.sort();
    files.reverse();
    let parts = parts(&pattern);
    let mut any = false;
    for file in files {
        let wanted = extension
            .as_deref()
            .is_none_or(|wanted| file.extension().is_some_and(|own| own == wanted));
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        if !wanted {
            continue;
        }
        let mut at = 0;
        while at < text.len() {
            let Some((end, captures)) = match_at(&text, at, &parts) else {
                at += text[at..].chars().next().map_or(1, char::len_utf8);
                continue;
            };
            let line_start = text[..at].rfind('\n').map_or(0, |index| index + 1);
            let line_end = text[end..].find('\n').map_or(text.len(), |index| end + index);
            let (mut single, mut multi) = (Map::new(), Map::new());
            let mut replacement = rewrite.clone();
            for (name, many, from, to) in &captures {
                let captured = json!({ "text": &text[*from..*to], "range": range(&text, *from, *to) });
                let written = if *many {
                    multi.insert(name.clone(), json!([captured]));
                    format!("$$${name}")
                } else {
                    single.insert(name.clone(), captured);
                    format!("${name}")
                };
                replacement = replacement.map(|rewrite| rewrite.replace(&written, &text[*from..*to]));
            }
            let mut found = json!({
                "text": &text[at..end],
                "range": range(&text, at, end),
                "file": file.to_string_lossy().trim_start_matches("./").trim_start_matches(".\\"),
                "lines": &text[line_start..line_end],
                "language": "Fake",
                "metaVariables": { "single": single, "multi": multi, "transformed": {} }
            });
            if let Some(replacement) = replacement {
                found["replacement"] = replacement.into();
                found["replacementOffsets"] = json!({ "start": at, "end": end });
            }
            println!("{found}");
            any = true;
            at = end;
        }
    }
    if any { ExitCode::SUCCESS } else { ExitCode::from(1) }
}
