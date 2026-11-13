//! A stand-in language server for tests, speaking JSON-RPC with `Content-Length` framing on its
//! standard streams.
//!
//! - `initialize` is answered with the capabilities below, which include being asked before a
//!   file is renamed and told after.
//! - After `didOpen` and `didChange` it publishes one error at the start of the document whose
//!   message names the document's size: `fake error: <bytes> bytes`, and one warning
//!   (`fake warning`) at the start of every line that contains `WARN`.
//! - `definition` answers line 3, columns 5 to 9 of the same document; `references` adds the
//!   first three columns of line 1.
//! - `hover` echoes the position it was asked about, as the protocol counts it.
//! - `documentSymbol` answers the document's symbols, nested, read from its text: a line
//!   `fn <name>` or `class <name>` opens a function or a class that runs to the next line that
//!   is `end`, and a line `let <name>` is a variable. A function inside a class is a method.
//!   A document with none of these has a function `main` on line 1 containing a variable
//!   `inner` on line 3.
//! - `rename` replaces the three characters at the position with the new name.
//! - `workspace/willRenameFiles` answers an edit that replaces, in every open document, each
//!   `use <old name>` with `use <new name>`, the names being the files' names without extension.
//!
//! With `--log <file>` every message it receives adds one line to `<file>`: the method, a
//! space, and the parameters as JSON, where a document's `text` is replaced by its length.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

fn read(input: &mut impl BufRead) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; length?];
    input.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn send(out: &mut impl Write, message: &Value) -> io::Result<()> {
    let body = message.to_string();
    write!(out, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    out.flush()
}

fn range(line: u64, from: u64, to: u64) -> Value {
    json!({ "start": { "line": line, "character": from }, "end": { "line": line, "character": to } })
}

fn units(text: &str) -> u64 {
    text.encode_utf16().count() as u64
}

/// One line of the log: the method and its parameters, without the text of documents.
fn log(file: Option<&str>, method: &str, params: &Value) {
    let Some(file) = file else {
        return;
    };
    let mut params = params.clone();
    if let Some(text) = params.pointer_mut("/textDocument/text") {
        *text = text.as_str().map_or(0, str::len).into();
    }
    if let Some(text) = params.pointer_mut("/contentChanges/0/text") {
        *text = text.as_str().map_or(0, str::len).into();
    }
    if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
        let _ = writeln!(log, "{method} {params}");
    }
}

/// A symbol being read: its protocol form without `range.end` settled, and its children.
struct Open {
    symbol: Value,
    children: Vec<Value>,
}

fn close(mut open: Open, line: u64, length: u64) -> Value {
    open.symbol["range"]["end"] = json!({ "line": line, "character": length });
    if !open.children.is_empty() {
        open.symbol["children"] = Value::Array(open.children);
    }
    open.symbol
}

/// The symbols of `text`, nested.
fn symbols(text: &str) -> Vec<Value> {
    let mut top = Vec::new();
    let mut stack: Vec<Open> = Vec::new();
    let mut last = (0, 0);
    for (number, line) in text.lines().enumerate() {
        let number = number as u64;
        last = (number, units(line));
        let indent = units(line) - units(line.trim_start());
        let words: Vec<&str> = line.split_whitespace().collect();
        let mut add = |symbol: Value, stack: &mut Vec<Open>| match stack.last_mut() {
            Some(parent) => parent.children.push(symbol),
            None => top.push(symbol),
        };
        match words.as_slice() {
            [word @ ("fn" | "class" | "let"), name, ..] => {
                let from = indent + units(word) + 1;
                let selection = range(number, from, from + units(name));
                let kind = match *word {
                    "class" => 5,
                    "let" => 13,
                    _ if stack.last().is_some_and(|parent| parent.symbol["kind"] == 5) => 6,
                    _ => 12,
                };
                let symbol = json!({
                    "name": name,
                    "kind": kind,
                    "range": range(number, indent, units(line)),
                    "selectionRange": selection
                });
                if *word == "let" {
                    add(symbol, &mut stack);
                } else {
                    stack.push(Open {
                        symbol,
                        children: Vec::new(),
                    });
                }
            }
            ["end"] => {
                if let Some(open) = stack.pop() {
                    let symbol = close(open, number, units(line));
                    add(symbol, &mut stack);
                }
            }
            _ => {}
        }
    }
    // What was never closed runs to the end of the document.
    while let Some(open) = stack.pop() {
        let symbol = close(open, last.0, last.1);
        match stack.last_mut() {
            Some(parent) => parent.children.push(symbol),
            None => top.push(symbol),
        }
    }
    if top.is_empty() {
        top.push(json!({
            "name": "main",
            "kind": 12,
            "range": range(0, 0, 3),
            "selectionRange": range(0, 0, 3),
            "children": [{
                "name": "inner",
                "kind": 13,
                "range": range(2, 4, 8),
                "selectionRange": range(2, 4, 8)
            }]
        }));
    }
    top
}

/// The name of the file at `uri` without its directories and its extension.
fn stem(uri: &str) -> &str {
    let name = uri.rsplit('/').next().unwrap_or(uri);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// The edits that turn every `use <old>` of the open documents into `use <new>`.
fn renamed(documents: &BTreeMap<String, String>, old: &str, new: &str) -> Value {
    let wanted = format!("use {old}");
    let mut changes = serde_json::Map::new();
    for (uri, text) in documents {
        let mut edits = Vec::new();
        for (number, line) in text.lines().enumerate() {
            if let Some(at) = line.find(&wanted) {
                let from = units(&line[..at]) + 4;
                edits.push(json!({ "range": range(number as u64, from, from + units(old)), "newText": new }));
            }
        }
        if !edits.is_empty() {
            changes.insert(uri.clone(), Value::Array(edits));
        }
    }
    json!({ "changes": changes })
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let log_file = args
        .iter()
        .position(|arg| arg == "--log")
        .and_then(|at| args.get(at + 1))
        .map(String::as_str);
    let mut documents: BTreeMap<String, String> = BTreeMap::new();
    let mut input = io::stdin().lock();
    let mut out = io::stdout().lock();
    while let Some(message) = read(&mut input) {
        let method = message["method"].as_str().unwrap_or_default();
        let params = &message["params"];
        log(log_file, method, params);
        let uri = &params["textDocument"]["uri"];
        let position = &params["position"];
        let result = match method {
            "initialize" => json!({
                "capabilities": {
                    "textDocumentSync": 1,
                    "definitionProvider": true,
                    "referencesProvider": true,
                    "hoverProvider": true,
                    "documentSymbolProvider": true,
                    "renameProvider": true,
                    "workspace": {
                        "fileOperations": {
                            "willRename": { "filters": [{ "pattern": { "glob": "**/*" } }] },
                            "didRename": { "filters": [{ "pattern": { "glob": "**/*" } }] }
                        }
                    }
                },
                "serverInfo": { "name": "fake-lsp" }
            }),
            "textDocument/didOpen" | "textDocument/didChange" => {
                let text = if method.ends_with("didOpen") {
                    &params["textDocument"]["text"]
                } else {
                    &params["contentChanges"][0]["text"]
                };
                let text = text.as_str().unwrap_or_default();
                documents.insert(uri.as_str().unwrap_or_default().to_string(), text.to_string());
                let mut diagnostics = vec![json!({
                    "range": range(0, 0, 1),
                    "severity": 1,
                    "source": "fake",
                    "message": format!("fake error: {} bytes", text.len())
                })];
                for (number, line) in text.lines().enumerate() {
                    if line.contains("WARN") {
                        diagnostics.push(json!({
                            "range": range(number as u64, 0, 1),
                            "severity": 2,
                            "source": "fake",
                            "message": "fake warning"
                        }));
                    }
                }
                send(
                    &mut out,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": { "uri": uri, "diagnostics": diagnostics }
                    }),
                )?;
                continue;
            }
            "textDocument/didClose" => {
                documents.remove(uri.as_str().unwrap_or_default());
                continue;
            }
            "textDocument/definition" => json!({ "uri": uri, "range": range(2, 4, 8) }),
            "textDocument/references" => json!([
                { "uri": uri, "range": range(0, 0, 3) },
                { "uri": uri, "range": range(2, 4, 8) }
            ]),
            "textDocument/hover" => json!({
                "contents": {
                    "kind": "markdown",
                    "value": format!("fake hover at {}:{}", position["line"], position["character"])
                }
            }),
            "textDocument/documentSymbol" => {
                let text = documents.get(uri.as_str().unwrap_or_default());
                Value::Array(symbols(text.map_or("", String::as_str)))
            }
            "textDocument/rename" => {
                let line = position["line"].as_u64().unwrap_or(0);
                let from = position["character"].as_u64().unwrap_or(0);
                let mut changes = serde_json::Map::new();
                changes.insert(
                    uri.as_str().unwrap_or_default().to_string(),
                    json!([{ "range": range(line, from, from + 3), "newText": params["newName"] }]),
                );
                json!({ "changes": changes })
            }
            "workspace/willRenameFiles" => {
                let file = &params["files"][0];
                let old = stem(file["oldUri"].as_str().unwrap_or_default());
                let new = stem(file["newUri"].as_str().unwrap_or_default());
                renamed(&documents, old, new)
            }
            "shutdown" => Value::Null,
            "exit" => break,
            _ => {
                if let Some(id) = message.get("id") {
                    let error = json!({ "code": -32601, "message": format!("{method} is not supported") });
                    send(&mut out, &json!({ "jsonrpc": "2.0", "id": id, "error": error }))?;
                }
                continue;
            }
        };
        if let Some(id) = message.get("id") {
            send(&mut out, &json!({ "jsonrpc": "2.0", "id": id, "result": result }))?;
        }
    }
    Ok(())
}
