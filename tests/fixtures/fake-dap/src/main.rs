//! A stand-in debug adapter for tests, speaking the Debug Adapter Protocol with `Content-Length`
//! framing on its standard streams.
//!
//! - `initialize` is answered; `launch` is followed by an `initialized` event and answered once
//!   `configurationDone` arrives, as real adapters do.
//! - `setBreakpoints` binds every line (a line 0 is moved to line 1). A breakpoint with a
//!   condition makes the stop at it carry the condition as its description.
//! - When configuration is done the program prints `hello` and stops at the first breakpoint
//!   (the smallest line), unless its file name contains `hang`, which never stops until `pause`,
//!   or there is no breakpoint, when it exits with code 3.
//! - `continue` prints `continued` and stops at the next breakpoint below the current line, or
//!   exits with code 0 when there is none. `next`, `stepIn` and `stepOut` stop one line on, with
//!   the reason `step`. `pause` stops with the reason `pause`.
//! - The stack has the frames `inner` (at the stop) and `main`. The locals of a frame are
//!   `x: int = 42`, `text: String` with a very long value, and `line` with the stop's line.
//! - `evaluate` knows `x` (42), `x + 1` (43) and `line`; anything else is an error.
//! - `disconnect` is answered and the program exits.

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
    io::Read::read_exact(input, &mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

struct Adapter {
    seq: i64,
    out: io::Stdout,
    file: String,
    /// `(line, condition)` of the breakpoints, sorted by line.
    breakpoints: Vec<(u64, Option<String>)>,
    line: u64,
    hangs: bool,
    launch_seq: Option<i64>,
}

impl Adapter {
    fn send(&mut self, mut message: Value) {
        self.seq += 1;
        message["seq"] = self.seq.into();
        let body = message.to_string();
        let mut out = self.out.lock();
        let _ = write!(out, "Content-Length: {}\r\n\r\n{body}", body.len());
        let _ = out.flush();
    }

    fn respond(&mut self, request: &Value, body: Value) {
        self.send(json!({
            "type": "response", "request_seq": request["seq"], "success": true,
            "command": request["command"], "body": body,
        }));
    }

    fn fail(&mut self, request: &Value, message: &str) {
        self.send(json!({
            "type": "response", "request_seq": request["seq"], "success": false,
            "command": request["command"], "message": message,
        }));
    }

    fn event(&mut self, event: &str, body: Value) {
        self.send(json!({ "type": "event", "event": event, "body": body }));
    }

    fn print(&mut self, text: &str) {
        self.event("output", json!({ "category": "stdout", "output": text }));
    }

    fn stop(&mut self, reason: &str, description: Option<String>) {
        let mut body = json!({ "reason": reason, "threadId": 1, "allThreadsStopped": true });
        if let Some(description) = description {
            body["description"] = description.into();
        }
        self.event("stopped", body);
    }

    fn stop_at_breakpoint(&mut self, line: u64) {
        self.line = line;
        let condition = self
            .breakpoints
            .iter()
            .find(|(at, _)| *at == line)
            .and_then(|(_, condition)| condition.clone());
        self.stop("breakpoint", condition.map(|c| format!("condition {c}")));
    }

    fn exit(&mut self, code: i64) {
        self.event("exited", json!({ "exitCode": code }));
        self.event("terminated", json!({}));
    }

    fn handle(&mut self, request: &Value) -> bool {
        let arguments = &request["arguments"];
        match request["command"].as_str().unwrap_or_default() {
            "initialize" => self.respond(request, json!({ "supportsConfigurationDoneRequest": true })),
            "launch" => {
                let program = arguments["program"].as_str().unwrap_or_default();
                self.hangs = program.contains("hang");
                self.launch_seq = request["seq"].as_i64();
                self.event("initialized", json!({}));
            }
            "setBreakpoints" => {
                self.file = arguments["source"]["path"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let asked = arguments["breakpoints"].as_array().cloned().unwrap_or_default();
                self.breakpoints = asked
                    .iter()
                    .map(|b| {
                        let line = b["line"].as_u64().unwrap_or(1).max(1);
                        (line, b["condition"].as_str().map(str::to_string))
                    })
                    .collect();
                self.breakpoints.sort();
                let bound: Vec<Value> = self
                    .breakpoints
                    .iter()
                    .map(|(line, _)| json!({ "verified": true, "line": line }))
                    .collect();
                self.respond(request, json!({ "breakpoints": bound }));
            }
            "configurationDone" => {
                self.respond(request, json!({}));
                let launch = json!({ "seq": self.launch_seq, "command": "launch" });
                self.respond(&launch, json!({}));
                self.print("hello\n");
                match (self.hangs, self.breakpoints.first().cloned()) {
                    (true, _) => {}
                    (false, Some((line, _))) => self.stop_at_breakpoint(line),
                    (false, None) => self.exit(3),
                }
            }
            "continue" => {
                self.respond(request, json!({ "allThreadsContinued": true }));
                self.print("continued\n");
                let next = self
                    .breakpoints
                    .iter()
                    .map(|(line, _)| *line)
                    .find(|line| *line > self.line);
                match next {
                    Some(line) => self.stop_at_breakpoint(line),
                    None => self.exit(0),
                }
            }
            "next" | "stepIn" | "stepOut" => {
                self.respond(request, json!({}));
                self.line += 1;
                self.stop("step", None);
            }
            "pause" => {
                self.respond(request, json!({}));
                self.stop("pause", None);
            }
            "threads" => self.respond(request, json!({ "threads": [{ "id": 1, "name": "main" }] })),
            "stackTrace" => {
                let source = json!({ "name": "main.fk", "path": self.file });
                let frames = json!([
                    { "id": 1000, "name": "inner", "source": source, "line": self.line, "column": 1 },
                    { "id": 1001, "name": "main", "source": source, "line": 3, "column": 1 },
                ]);
                self.respond(request, json!({ "stackFrames": frames, "totalFrames": 2 }));
            }
            "scopes" => self.respond(
                request,
                json!({ "scopes": [
                    { "name": "Locals", "variablesReference": 7, "expensive": false },
                    { "name": "Globals", "variablesReference": 8, "expensive": true },
                ] }),
            ),
            "variables" => {
                let variables = if arguments["variablesReference"] == 7 {
                    json!([
                        { "name": "x", "value": "42", "type": "int", "variablesReference": 0 },
                        { "name": "text", "value": "ab".repeat(400), "type": "String", "variablesReference": 0 },
                        { "name": "line", "value": self.line.to_string(), "type": "int", "variablesReference": 0 },
                    ])
                } else {
                    json!([{ "name": "GLOBAL", "value": "never shown", "variablesReference": 0 }])
                };
                self.respond(request, json!({ "variables": variables }));
            }
            "evaluate" => match arguments["expression"].as_str().unwrap_or_default().trim() {
                "x" => self.respond(
                    request,
                    json!({ "result": "42", "type": "int", "variablesReference": 0 }),
                ),
                "x + 1" => self.respond(
                    request,
                    json!({ "result": "43", "type": "int", "variablesReference": 0 }),
                ),
                "line" => {
                    let line = self.line.to_string();
                    self.respond(
                        request,
                        json!({ "result": line, "type": "int", "variablesReference": 0 }),
                    );
                }
                other => self.fail(request, &format!("cannot evaluate `{other}`")),
            },
            "disconnect" => {
                self.respond(request, json!({}));
                return false;
            }
            other => self.fail(request, &format!("{other} is not supported")),
        }
        true
    }
}

fn main() {
    let mut input = io::stdin().lock();
    let mut adapter = Adapter {
        seq: 0,
        out: io::stdout(),
        file: String::new(),
        breakpoints: Vec::new(),
        line: 0,
        hangs: false,
        launch_seq: None,
    };
    while let Some(request) = read(&mut input) {
        if !adapter.handle(&request) {
            break;
        }
    }
}
