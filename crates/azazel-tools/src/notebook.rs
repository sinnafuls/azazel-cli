//! A Jupyter notebook for `read`, cell by cell: source, then what the cell printed.

use base64::Engine as _;
use serde_json::Value;

/// Characters of one text output that are shown.
const MAX_OUTPUT_CHARS: usize = 2000;
/// Characters of one cell's source that are shown.
const MAX_SOURCE_CHARS: usize = 100_000;

#[derive(Debug)]
pub struct Rendered {
    pub text: String,
    /// The first image among the outputs of the cells shown: its MIME type and bytes.
    pub image: Option<(String, Vec<u8>)>,
}

/// A notebook field that is one string or a list of lines.
fn joined(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(lines)) => lines.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

/// The cells of a notebook: `cells` since format 4, one list per worksheet before that.
fn cells(notebook: &Value) -> Option<Vec<&Value>> {
    if let Some(cells) = notebook.get("cells").and_then(Value::as_array) {
        return Some(cells.iter().collect());
    }
    let sheets = notebook.get("worksheets")?.as_array()?;
    Some(
        sheets
            .iter()
            .filter_map(|sheet| sheet.get("cells").and_then(Value::as_array))
            .flatten()
            .collect(),
    )
}

fn cut(text: &str, max: usize, what: &str) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!(
            "{}\n[{what} cut: {} more characters]",
            &text[..end],
            text[end..].chars().count()
        ),
        None => text.to_string(),
    }
}

/// `text` without the colour codes tracebacks carry.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for end in chars.by_ref() {
                if end.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

struct CellOutput {
    lines: Vec<String>,
    images: Vec<(String, Vec<u8>)>,
}

/// What one code cell produced: its text outputs, cut, and a line naming each image.
fn outputs(cell: &Value, number: usize) -> CellOutput {
    let mut found = CellOutput {
        lines: Vec::new(),
        images: Vec::new(),
    };
    let list = cell.get("outputs").and_then(Value::as_array);
    for output in list.into_iter().flatten() {
        let kind = output
            .get("output_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "stream" => {
                let text = joined(output.get("text"));
                if !text.trim().is_empty() {
                    found.lines.push(cut(text.trim_end(), MAX_OUTPUT_CHARS, "output"));
                }
            }
            "error" | "pyerr" => {
                let name = output.get("ename").and_then(Value::as_str).unwrap_or("error");
                let value = output.get("evalue").and_then(Value::as_str).unwrap_or_default();
                let trace: Vec<&str> = output
                    .get("traceback")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                let text = format!("{name}: {value}\n{}", plain(&trace.join("\n")));
                found.lines.push(cut(text.trim_end(), MAX_OUTPUT_CHARS, "output"));
            }
            _ => {
                // Format 4 keeps the representations under `data` by MIME type; older formats
                // keep them beside `output_type` under short names.
                let data = output.get("data").unwrap_or(output);
                let text = joined(data.get("text/plain").or_else(|| data.get("text")));
                let mut others = Vec::new();
                for (key, value) in data.as_object().into_iter().flatten() {
                    let mime = match key.as_str() {
                        "png" => "image/png",
                        "jpeg" => "image/jpeg",
                        other => other,
                    };
                    if mime.starts_with("image/") && mime != "image/svg+xml" {
                        let encoded: String = joined(Some(value)).split_whitespace().collect();
                        match base64::engine::general_purpose::STANDARD.decode(encoded) {
                            Ok(bytes) => {
                                found.lines.push(format!(
                                    "[image output: cell {number} image {}, {mime}, {} bytes]",
                                    found.images.len() + 1,
                                    bytes.len()
                                ));
                                found.images.push((mime.to_string(), bytes));
                            }
                            Err(_) => found.lines.push(format!("[image output: {mime}, not readable]")),
                        }
                    } else if mime.contains('/') && mime != "text/plain" {
                        others.push(mime.to_string());
                    }
                }
                if !text.trim().is_empty() {
                    found.lines.push(cut(text.trim_end(), MAX_OUTPUT_CHARS, "output"));
                } else if found.images.is_empty() && !others.is_empty() {
                    found
                        .lines
                        .push(format!("[output not shown: {}]", others.join(", ")));
                }
            }
        }
    }
    found
}

/// The notebook in `bytes` from cell `offset` (1-based) for `limit` cells, stopping early once
/// the text passes `max_bytes`. `None` when `bytes` is not a notebook, so that the caller can
/// show it as the text it is.
pub fn render(
    bytes: &[u8],
    offset: Option<usize>,
    limit: Option<usize>,
    max_bytes: usize,
) -> Option<Result<Rendered, String>> {
    let notebook: Value = serde_json::from_slice(bytes).ok()?;
    let cells = cells(&notebook)?;
    let total = cells.len();
    if total == 0 {
        return Some(Ok(Rendered {
            text: "(the notebook has no cells)".to_string(),
            image: None,
        }));
    }
    let first = offset.unwrap_or(1).max(1);
    if first > total {
        return Some(Err(format!(
            "offset {first} is past the end of the notebook ({total} cells)"
        )));
    }
    let limit = limit.unwrap_or(usize::MAX).max(1);
    let mut text = String::new();
    let mut image = None;
    let mut last = first - 1;
    for (index, cell) in cells.iter().enumerate().skip(first - 1).take(limit) {
        let number = index + 1;
        let kind = cell.get("cell_type").and_then(Value::as_str).unwrap_or("unknown");
        let source = joined(cell.get("source").or_else(|| cell.get("input")));
        let mut block = format!(
            "# cell {number} [{kind}]\n{}\n",
            cut(source.trim_end(), MAX_SOURCE_CHARS, "source")
        );
        let produced = outputs(cell, number);
        if !produced.lines.is_empty() {
            block.push_str("--- output ---\n");
            for line in &produced.lines {
                block.push_str(line);
                block.push('\n');
            }
        }
        if !text.is_empty() && text.len() + block.len() > max_bytes {
            break;
        }
        text.push_str(&block);
        text.push('\n');
        if image.is_none() {
            image = produced.images.into_iter().next();
        }
        last = number;
    }
    let mut text = text.trim_end().to_string();
    if first > 1 || last < total {
        text.push_str(&format!("\n[cells {first}-{last} of {total}"));
        if last < total {
            text.push_str(&format!("; continue with offset={}", last + 1));
        }
        text.push(']');
    }
    Some(Ok(Rendered { text, image }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A 1x1 PNG.
    pub(crate) const PIXEL: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn notebook() -> Vec<u8> {
        let long = "x".repeat(MAX_OUTPUT_CHARS + 30);
        serde_json::to_vec(&json!({
            "nbformat": 4,
            "cells": [
                {"cell_type": "markdown", "source": ["# Title\n", "Some words."]},
                {
                    "cell_type": "code",
                    "source": "print('hi')\nplot()",
                    "outputs": [
                        {"output_type": "stream", "name": "stdout", "text": ["hi\n"]},
                        {"output_type": "display_data", "data": {
                            "text/plain": "<Figure>",
                            "image/png": PIXEL
                        }}
                    ]
                },
                {
                    "cell_type": "code",
                    "source": ["1/0"],
                    "outputs": [
                        {"output_type": "error", "ename": "ZeroDivisionError", "evalue": "division by zero",
                         "traceback": ["\u{1b}[0;31mTraceback\u{1b}[0m", "line 1"]},
                        {"output_type": "stream", "text": long}
                    ]
                },
                {"cell_type": "code", "source": "x = 1", "outputs": []}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn cells_show_their_kind_source_text_output_and_name_their_images() {
        let shown = render(&notebook(), None, None, usize::MAX).unwrap().unwrap();
        let expected_start = "# cell 1 [markdown]\n# Title\nSome words.\n\n\
                              # cell 2 [code]\nprint('hi')\nplot()\n--- output ---\nhi\n\
                              [image output: cell 2 image 1, image/png, 70 bytes]\n<Figure>\n\n\
                              # cell 3 [code]\n1/0\n--- output ---\n\
                              ZeroDivisionError: division by zero\nTraceback\nline 1\n";
        assert!(shown.text.starts_with(expected_start), "{}", shown.text);
        assert!(
            shown
                .text
                .ends_with("[output cut: 30 more characters]\n\n# cell 4 [code]\nx = 1"),
            "{}",
            shown.text
        );
        let (mime, bytes) = shown.image.unwrap();
        assert_eq!((mime.as_str(), bytes.len()), ("image/png", 70));
    }

    #[test]
    fn offset_and_limit_count_cells() {
        let part = render(&notebook(), Some(2), Some(1), usize::MAX)
            .unwrap()
            .unwrap();
        assert!(part.text.starts_with("# cell 2 [code]\n"), "{}", part.text);
        assert!(
            part.text.ends_with("\n[cells 2-2 of 4; continue with offset=3]"),
            "{}",
            part.text
        );
        let tail = render(&notebook(), Some(4), None, usize::MAX).unwrap().unwrap();
        assert_eq!(tail.text, "# cell 4 [code]\nx = 1\n[cells 4-4 of 4]");
        assert!(tail.image.is_none());
        let past = render(&notebook(), Some(5), None, usize::MAX)
            .unwrap()
            .unwrap_err();
        assert_eq!(past, "offset 5 is past the end of the notebook (4 cells)");
        // The size bound ends a call early and names the cell to go on from.
        let small = render(&notebook(), None, None, 40).unwrap().unwrap();
        assert!(
            small.text.ends_with("\n[cells 1-1 of 4; continue with offset=2]"),
            "{}",
            small.text
        );
    }

    #[test]
    fn old_worksheets_are_read_and_other_json_is_not_a_notebook() {
        let old = json!({"nbformat": 3, "worksheets": [{"cells": [
            {"cell_type": "code", "input": ["a = 2\n"], "outputs": [
                {"output_type": "pyout", "text": ["2"], "png": PIXEL}
            ]}
        ]}]});
        let shown = render(&serde_json::to_vec(&old).unwrap(), None, None, usize::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(
            shown.text,
            "# cell 1 [code]\na = 2\n--- output ---\n[image output: cell 1 image 1, image/png, 70 bytes]\n2"
        );
        assert!(render(b"{\"a\": 1}", None, None, usize::MAX).is_none());
        assert!(render(b"not json", None, None, usize::MAX).is_none());
    }
}
