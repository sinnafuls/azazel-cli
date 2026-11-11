//! A stand-in language server for tests, speaking JSON-RPC with `Content-Length` framing on its
//! standard streams.
//!
//! - `initialize` is answered with the capabilities below.
//! - After `didOpen` and `didChange` it publishes one error at the start of the document whose
//!   message names the document's size: `fake error: <bytes> bytes`.
//! - `definition` answers line 3, columns 5 to 9 of the same document; `references` adds the
//!   first three columns of line 1.
//! - `hover` echoes the position it was asked about, as the protocol counts it.
//! - `documentSymbol` answers a function `main` containing a variable `inner`.
//! - `rename` replaces the three characters at the position with the new name.

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

fn main() -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut out = io::stdout().lock();
    while let Some(message) = read(&mut input) {
        let method = message["method"].as_str().unwrap_or_default();
        let params = &message["params"];
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
                    "renameProvider": true
                },
                "serverInfo": { "name": "fake-lsp" }
            }),
            "textDocument/didOpen" | "textDocument/didChange" => {
                let text = if method.ends_with("didOpen") {
                    &params["textDocument"]["text"]
                } else {
                    &params["contentChanges"][0]["text"]
                };
                let bytes = text.as_str().map_or(0, str::len);
                let diagnostic = json!({
                    "range": range(0, 0, 1),
                    "severity": 1,
                    "source": "fake",
                    "message": format!("fake error: {bytes} bytes")
                });
                send(
                    &mut out,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": { "uri": uri, "diagnostics": [diagnostic] }
                    }),
                )?;
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
            "textDocument/documentSymbol" => json!([{
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
            }]),
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
