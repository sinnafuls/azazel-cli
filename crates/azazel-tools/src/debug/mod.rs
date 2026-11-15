//! The `debug` tool: a debugger for agents over the Debug Adapter Protocol. One session per agent;
//! the adapter for the program's language is found on `PATH` when the session starts.

mod adapters;
mod dap;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::agent::ABORTED_TEXT;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::config::DebugAdapterConfig;
use azazel_core::{Event, EventHandler, EventKind, ExtCtx, ExtError, Extension, Outcome, Registrar};
use serde::Deserialize;
use serde_json::{Value, json};

use self::adapters::{Family, launch_arguments, resolve};
use self::dap::{Dap, Stop, Wait};
use crate::util::{parse_input, resolve_path};

pub const NAME: &str = "debug";

/// Frames listed when the program is stopped.
const MAX_FRAMES: usize = 12;
/// Locals listed of the top frame.
const MAX_LOCALS: usize = 30;
/// Characters of one value.
const MAX_VALUE: usize = 160;
/// Characters of program output shown in the result of a step or a continue.
const MAX_STEP_OUTPUT: usize = 1500;
/// Characters of program output the `output` action returns.
const MAX_OUTPUT: usize = 8000;
/// How long the adapter gets to say it is ready for breakpoints.
const INITIALIZED_WAIT: Duration = Duration::from_secs(15);

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
struct Breakpoint {
    line: u32,
    condition: Option<String>,
}

struct Session {
    dap: Dap,
    cwd: PathBuf,
    program: String,
    /// The breakpoints of each file; the adapter wants all of a file's at once.
    breakpoints: Mutex<BTreeMap<PathBuf, Vec<Breakpoint>>>,
}

/// The debugging sessions, one per agent, and what starts them. A service of the `debug` extension.
pub struct DebugManager {
    adapters: BTreeMap<String, DebugAdapterConfig>,
    timeout: Duration,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl DebugManager {
    pub fn new(ext: &DebugExt) -> Self {
        DebugManager {
            adapters: ext.adapters.clone(),
            timeout: ext.timeout,
            sessions: Mutex::default(),
        }
    }

    fn session(&self, agent: &str) -> Option<Arc<Session>> {
        locked(&self.sessions).get(agent).cloned()
    }

    /// Whether `agent` has a session.
    pub fn has_session(&self, agent: &str) -> bool {
        locked(&self.sessions).contains_key(agent)
    }

    /// Ends the session of `agent`, if it has one.
    pub async fn end(&self, agent: &str) {
        let session = locked(&self.sessions).remove(agent);
        if let Some(session) = session {
            session.dap.shutdown().await;
        }
    }

    /// Ends every session.
    pub async fn shutdown(&self) {
        let all: Vec<Arc<Session>> = locked(&self.sessions).drain().map(|(_, s)| s).collect();
        for session in all {
            session.dap.shutdown().await;
        }
    }
}

#[derive(Deserialize)]
struct BreakpointInput {
    file: String,
    line: u32,
    #[serde(default)]
    condition: Option<String>,
}

#[derive(Deserialize)]
struct Input {
    action: String,
    program: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    language: Option<String>,
    #[serde(default)]
    stop_on_entry: bool,
    #[serde(default)]
    breakpoints: Vec<BreakpointInput>,
    file: Option<String>,
    line: Option<u32>,
    condition: Option<String>,
    #[serde(default)]
    clear: bool,
    kind: Option<String>,
    expression: Option<String>,
    frame: Option<usize>,
    timeout_secs: Option<u64>,
}

pub struct DebugTool {
    manager: Arc<DebugManager>,
}

/// `text` cut to `limit` characters.
fn short(text: &str, limit: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}… ({} more characters)", text.chars().count() - limit)
}

/// The last `limit` characters of `text`, noting what was left out.
fn tail(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().skip(count - limit).collect();
    format!("… ({} earlier characters left out)\n{kept}", count - limit)
}

/// `path` relative to `cwd` when it lies under it.
fn display_path(path: &str, cwd: &Path) -> String {
    Path::new(path)
        .strip_prefix(cwd)
        .map_or_else(|_| path.to_string(), |rest| rest.display().to_string())
}

fn field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

impl Session {
    async fn set_breakpoints(&self, file: &Path) -> Result<Vec<String>, String> {
        let wanted = locked(&self.breakpoints).get(file).cloned().unwrap_or_default();
        let entries: Vec<Value> = wanted
            .iter()
            .map(|b| {
                let mut entry = json!({ "line": b.line });
                if let Some(condition) = &b.condition {
                    entry["condition"] = condition.clone().into();
                }
                entry
            })
            .collect();
        let answer = self
            .dap
            .request(
                "setBreakpoints",
                json!({
                    "source": { "path": file.to_string_lossy() },
                    "breakpoints": entries,
                    "lines": wanted.iter().map(|b| b.line).collect::<Vec<_>>(),
                }),
            )
            .await?;
        let shown = display_path(&file.to_string_lossy(), &self.cwd);
        let results = answer.get("breakpoints").and_then(Value::as_array);
        Ok(wanted
            .iter()
            .enumerate()
            .map(|(index, b)| {
                let result = results.and_then(|r| r.get(index));
                let verified = result.and_then(|r| r.get("verified")).and_then(Value::as_bool);
                let line = result
                    .and_then(|r| r.get("line"))
                    .and_then(Value::as_u64)
                    .map_or(b.line, |l| l as u32);
                let mut text = format!("{shown}:{line}");
                if line != b.line {
                    text.push_str(&format!(" (moved from line {})", b.line));
                }
                if let Some(condition) = &b.condition {
                    text.push_str(&format!(" if {condition}"));
                }
                if verified == Some(false) {
                    let why = result
                        .and_then(|r| field(r, "message"))
                        .unwrap_or("not bound yet");
                    text.push_str(&format!(" [unverified: {why}]"));
                }
                text
            })
            .collect())
    }

    /// The thread to act on: the one that stopped, else the first.
    async fn thread(&self) -> Result<i64, String> {
        if let Some(thread) = self.dap.stopped().and_then(|s| s.thread) {
            return Ok(thread);
        }
        let threads = self.dap.request("threads", json!({})).await?;
        threads
            .get("threads")
            .and_then(Value::as_array)
            .and_then(|all| all.first())
            .and_then(|t| t.get("id"))
            .and_then(Value::as_i64)
            .ok_or_else(|| "the program has no threads".to_string())
    }

    async fn frames(&self, thread: i64, levels: usize) -> Result<Vec<Value>, String> {
        let trace = self
            .dap
            .request(
                "stackTrace",
                json!({ "threadId": thread, "startFrame": 0, "levels": levels }),
            )
            .await?;
        Ok(trace
            .get("stackFrames")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    fn frame_line(&self, index: usize, frame: &Value) -> String {
        let name = field(frame, "name").unwrap_or("?");
        let place = match frame
            .get("source")
            .and_then(|s| field(s, "path").or_else(|| field(s, "name")))
        {
            Some(path) => {
                let line = frame.get("line").and_then(Value::as_u64).unwrap_or(0);
                format!("{}:{line}", display_path(path, &self.cwd))
            }
            None => "(no source)".to_string(),
        };
        format!("#{index} {name} at {place}")
    }

    /// Where the program stopped: why, the stack, and the locals of the top frame.
    async fn describe_stop(&self, stop: &Stop) -> String {
        let mut out = format!("stopped: {}", stop.reason);
        if !stop.description.is_empty() {
            out.push_str(&format!(" ({})", short(&stop.description, MAX_VALUE)));
        }
        let thread = match self.thread().await {
            Ok(thread) => thread,
            Err(error) => return format!("{out}\n{error}"),
        };
        out.push_str(&format!("\nthread {thread}"));
        let frames = match self.frames(thread, MAX_FRAMES + 1).await {
            Ok(frames) => frames,
            Err(error) => return format!("{out}\nno stack: {error}"),
        };
        for (index, frame) in frames.iter().take(MAX_FRAMES).enumerate() {
            out.push_str(&format!("\n{}", self.frame_line(index, frame)));
        }
        if frames.len() > MAX_FRAMES {
            out.push_str("\n… more frames");
        }
        if let Some(top) = frames.first().and_then(|f| f.get("id")).and_then(Value::as_i64) {
            match self.locals(top).await {
                Ok(locals) if locals.is_empty() => out.push_str("\nlocals: none"),
                Ok(locals) => out.push_str(&format!("\nlocals:\n{}", locals.join("\n"))),
                Err(error) => out.push_str(&format!("\nlocals unavailable: {error}")),
            }
        }
        out
    }

    async fn locals(&self, frame: i64) -> Result<Vec<String>, String> {
        let scopes = self.dap.request("scopes", json!({ "frameId": frame })).await?;
        let scopes = scopes
            .get("scopes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // The first scope is the locals; the ones after it (registers, globals) are not wanted.
        let Some(scope) = scopes.first() else {
            return Ok(Vec::new());
        };
        let reference = scope
            .get("variablesReference")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if reference == 0 {
            return Ok(Vec::new());
        }
        let variables = self
            .dap
            .request("variables", json!({ "variablesReference": reference }))
            .await?;
        // debugpy lists groups (`special variables`, `function variables`) beside the locals.
        let all: Vec<Value> = variables
            .get("variables")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|v| !field(v, "name").is_some_and(|name| name.ends_with(" variables")))
            .cloned()
            .collect();
        let mut lines: Vec<String> = all
            .iter()
            .take(MAX_LOCALS)
            .map(|v| {
                let name = field(v, "name").unwrap_or("?");
                let value = short(field(v, "value").unwrap_or_default(), MAX_VALUE);
                match field(v, "type").filter(|t| !t.is_empty()) {
                    Some(kind) => format!("  {name}: {} = {value}", short(kind, 60)),
                    None => format!("  {name} = {value}"),
                }
            })
            .collect();
        if all.len() > MAX_LOCALS {
            lines.push(format!("  … {} more", all.len() - MAX_LOCALS));
        }
        Ok(lines)
    }

    /// The result of waiting for the program: where it stopped, or why it did not.
    async fn after(&self, wait: Wait, limit: Duration) -> ToolOutput {
        let mut text = match wait {
            Wait::Stopped(stop) => self.describe_stop(&stop).await,
            Wait::Terminated(code) => match code {
                Some(code) => {
                    format!("the program exited with code {code}; the session is over (`stop` ends it)")
                }
                None => "the program ended; the session is over (`stop` ends it)".to_string(),
            },
            Wait::TimedOut => format!(
                "the program is still running after {} s and has not stopped; `pause` stops it, `state` \
                 with `timeout_secs` waits longer",
                limit.as_secs()
            ),
            Wait::Cancelled => return ToolOutput::error(ABORTED_TEXT),
        };
        let printed = self.dap.new_output();
        if !printed.trim().is_empty() {
            text.push_str(&format!(
                "\noutput:\n{}",
                tail(printed.trim_end(), MAX_STEP_OUTPUT)
            ));
        }
        ToolOutput::text(text)
    }
}

fn sanitized(error: String) -> ToolOutput {
    ToolOutput::error(error)
}

impl DebugTool {
    pub fn new(manager: Arc<DebugManager>) -> Self {
        DebugTool { manager }
    }

    fn limit(&self, input: &Input) -> Duration {
        input
            .timeout_secs
            .map_or(self.manager.timeout, Duration::from_secs)
            .max(Duration::from_secs(1))
    }

    async fn start(&self, input: Input, ctx: &ToolCtx) -> ToolOutput {
        let Some(program) = input.program.clone().filter(|p| !p.trim().is_empty()) else {
            return ToolOutput::error(
                "start needs `program`: the executable, test binary or script to debug",
            );
        };
        let cwd = input
            .cwd
            .as_deref()
            .map_or_else(|| ctx.cwd.clone(), |dir| resolve_path(&ctx.cwd, dir));
        let program_path = resolve_path(&cwd, &program);
        let (family, adapter) =
            match resolve(input.language.as_deref(), &program_path, &self.manager.adapters) {
                Ok(found) => found,
                Err(error) => return sanitized(error),
            };
        self.manager.end(&ctx.agent_id).await;
        let limit = self.limit(&input);
        let started = tokio::select! {
            started = self.begin(&input, family, &adapter, &cwd, &program_path, limit) => started,
            _ = ctx.cancel.cancelled() => return ToolOutput::error(ABORTED_TEXT),
        };
        let (session, notes, before) = match started {
            Ok(started) => started,
            Err(error) => return sanitized(adapters::with_hint(family, error)),
        };
        locked(&self.manager.sessions).insert(ctx.agent_id.clone(), session.clone());
        let wait = session.dap.wait_stop(before, limit, &ctx.cancel).await;
        let mut out = session.after(wait, limit).await;
        if !out.is_error {
            let mut text = format!("started {}\n", session.program);
            for note in notes {
                text.push_str(&format!("{note}\n"));
            }
            text.push_str(&out.text_content());
            out = ToolOutput::text(text);
        }
        out
    }

    /// Starts the adapter, sets the breakpoints and lets the program run; the program's session
    /// and the notes about its breakpoints.
    async fn begin(
        &self,
        input: &Input,
        family: Family,
        adapter: &dap_adapter::Adapter,
        cwd: &Path,
        program: &Path,
        limit: Duration,
    ) -> Result<(Arc<Session>, Vec<String>, u64), String> {
        let dap = Dap::start(adapter, cwd).await?;
        let session = Arc::new(Session {
            dap,
            cwd: cwd.to_path_buf(),
            program: display_path(&program.to_string_lossy(), cwd),
            breakpoints: Mutex::default(),
        });
        match self.handshake(&session, input, family, program, limit).await {
            Ok(notes) => {
                let before = 0;
                Ok((session, notes, before))
            }
            Err(error) => {
                let error = if session.dap.dead() {
                    session.dap.gone()
                } else {
                    error
                };
                session.dap.shutdown().await;
                Err(error)
            }
        }
    }

    async fn handshake(
        &self,
        session: &Session,
        input: &Input,
        family: Family,
        program: &Path,
        limit: Duration,
    ) -> Result<Vec<String>, String> {
        let dap = &session.dap;
        dap.request(
            "initialize",
            json!({
                "clientID": "azazel",
                "clientName": "azazel",
                "adapterID": family.adapter_id(),
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "pathFormat": "path",
                "supportsVariableType": true,
                "supportsRunInTerminalRequest": false,
            }),
        )
        .await?;
        let launch = launch_arguments(family, program, &input.args, &session.cwd, input.stop_on_entry);
        let mut launched = dap.send("launch", launch).await?;
        let mut launch_answer = None;
        // Adapters differ in whether the launch is answered before or after configuration is done.
        tokio::select! {
            ready = dap.wait_initialized(INITIALIZED_WAIT) => {
                if !ready && dap.dead() {
                    return Err(dap.gone());
                }
            }
            answer = &mut launched => {
                match answer {
                    Ok(Ok(body)) => launch_answer = Some(body),
                    Ok(Err(error)) => return Err(error),
                    Err(_) => return Err(dap.gone()),
                }
            }
        }
        dap.wait_initialized(INITIALIZED_WAIT).await;
        let mut notes = Vec::new();
        for wanted in &input.breakpoints {
            let file = resolve_path(&session.cwd, &wanted.file);
            locked(&session.breakpoints)
                .entry(file)
                .or_default()
                .push(Breakpoint {
                    line: wanted.line,
                    condition: wanted.condition.clone().filter(|c| !c.trim().is_empty()),
                });
        }
        let files: Vec<PathBuf> = locked(&session.breakpoints).keys().cloned().collect();
        for file in files {
            for line in session.set_breakpoints(&file).await? {
                notes.push(format!("breakpoint {line}"));
            }
        }
        dap.request_within("configurationDone", json!({}), limit.max(Duration::from_secs(20)))
            .await
            .or_else(|error| {
                // Adapters that do not announce the request have nothing to configure.
                if error.contains("not supported") || error.contains("unknown") {
                    Ok(Value::Null)
                } else {
                    Err(error)
                }
            })?;
        if launch_answer.is_none() {
            match tokio::time::timeout(limit.max(Duration::from_secs(20)), launched).await {
                Ok(Ok(Ok(_))) => {}
                Ok(Ok(Err(error))) => return Err(error),
                Ok(Err(_)) => return Err(dap.gone()),
                Err(_) => return Err("the debug adapter did not answer launch".to_string()),
            }
        }
        Ok(notes)
    }

    async fn call_action(&self, input: Input, ctx: &ToolCtx) -> ToolOutput {
        if input.action == "start" {
            return self.start(input, ctx).await;
        }
        let Some(session) = self.manager.session(&ctx.agent_id) else {
            return ToolOutput::error("no debug session: begin one with action `start`");
        };
        if input.action == "stop" {
            self.manager.end(&ctx.agent_id).await;
            let printed = session.dap.new_output();
            return ToolOutput::text(if printed.trim().is_empty() {
                "debug session ended".to_string()
            } else {
                format!(
                    "debug session ended\noutput:\n{}",
                    tail(printed.trim_end(), MAX_STEP_OUTPUT)
                )
            });
        }
        let limit = self.limit(&input);
        match input.action.as_str() {
            "break" => self.breakpoint(&session, &input, ctx).await,
            "continue" | "step" | "pause" => self.resume(&session, &input, limit, ctx).await,
            "state" => {
                self.state(&session, limit, ctx, input.timeout_secs.is_some())
                    .await
            }
            "eval" => self.eval(&session, &input).await,
            "output" => {
                let (all, cut) = session.dap.all_output();
                if all.trim().is_empty() {
                    return ToolOutput::text("the program has printed nothing");
                }
                let note = if cut { "(the start was dropped)\n" } else { "" };
                ToolOutput::text(format!("{note}{}", tail(all.trim_end(), MAX_OUTPUT)))
            }
            other => ToolOutput::error(format!(
                "unknown action `{other}`; use start, break, continue, step, pause, state, eval, output or stop"
            )),
        }
    }

    async fn breakpoint(&self, session: &Session, input: &Input, ctx: &ToolCtx) -> ToolOutput {
        let Some(file) = input.file.as_deref() else {
            return ToolOutput::error("break needs `file`");
        };
        let file = resolve_path(&ctx.cwd, file);
        {
            let mut all = locked(&session.breakpoints);
            if input.clear {
                match input.line {
                    Some(line) => {
                        if let Some(list) = all.get_mut(&file) {
                            list.retain(|b| b.line != line);
                        }
                    }
                    None => {
                        all.remove(&file);
                    }
                }
            } else {
                let Some(line) = input.line else {
                    return ToolOutput::error("break needs `line` (or `clear` to remove breakpoints)");
                };
                let list = all.entry(file.clone()).or_default();
                list.retain(|b| b.line != line);
                list.push(Breakpoint {
                    line,
                    condition: input.condition.clone().filter(|c| !c.trim().is_empty()),
                });
            }
        }
        match session.set_breakpoints(&file).await {
            Ok(lines) if lines.is_empty() => ToolOutput::text(format!(
                "no breakpoints in {}",
                display_path(&file.to_string_lossy(), &session.cwd)
            )),
            Ok(lines) => ToolOutput::text(format!("breakpoints:\n{}", lines.join("\n"))),
            Err(error) => ToolOutput::error(error),
        }
    }

    async fn resume(&self, session: &Session, input: &Input, limit: Duration, ctx: &ToolCtx) -> ToolOutput {
        let dap = &session.dap;
        if let Some(code) = dap.ended() {
            return session.after(Wait::Terminated(code), limit).await;
        }
        let thread = match session.thread().await {
            Ok(thread) => thread,
            Err(error) => return ToolOutput::error(error),
        };
        let before = dap.stops();
        let (command, arguments) = match input.action.as_str() {
            "continue" => ("continue", json!({ "threadId": thread })),
            "pause" => ("pause", json!({ "threadId": thread })),
            _ => {
                let command = match input.kind.as_deref().unwrap_or("over") {
                    "over" => "next",
                    "in" => "stepIn",
                    "out" => "stepOut",
                    other => {
                        return ToolOutput::error(format!("`kind` is over, in or out, not `{other}`"));
                    }
                };
                (command, json!({ "threadId": thread }))
            }
        };
        if command != "pause" {
            if dap.stopped().is_none() {
                return ToolOutput::error("the program is running; `pause` stops it");
            }
            dap.resumed();
        }
        if let Err(error) = dap.request(command, arguments).await {
            return ToolOutput::error(error);
        }
        let wait = dap.wait_stop(before, limit, &ctx.cancel).await;
        session.after(wait, limit).await
    }

    async fn state(&self, session: &Session, limit: Duration, ctx: &ToolCtx, wait: bool) -> ToolOutput {
        let dap = &session.dap;
        if let Some(stop) = dap.stopped() {
            return session.after(Wait::Stopped(stop), limit).await;
        }
        if let Some(code) = dap.ended() {
            return session.after(Wait::Terminated(code), limit).await;
        }
        if !wait {
            let printed = dap.new_output();
            let mut text = "the program is running".to_string();
            if !printed.trim().is_empty() {
                text.push_str(&format!(
                    "\noutput:\n{}",
                    tail(printed.trim_end(), MAX_STEP_OUTPUT)
                ));
            }
            return ToolOutput::text(text);
        }
        let waited = dap.wait_stop(dap.stops(), limit, &ctx.cancel).await;
        session.after(waited, limit).await
    }

    async fn eval(&self, session: &Session, input: &Input) -> ToolOutput {
        let Some(expression) = input.expression.as_deref().filter(|e| !e.trim().is_empty()) else {
            return ToolOutput::error("eval needs `expression`");
        };
        let Some(stop) = session.dap.stopped() else {
            return ToolOutput::error("the program is not stopped; evaluate when it is at a breakpoint");
        };
        let frame = input.frame.unwrap_or(0);
        let frames = match session.thread().await {
            Ok(thread) => match session.frames(thread, frame + 1).await {
                Ok(frames) => frames,
                Err(error) => return ToolOutput::error(error),
            },
            Err(error) => return ToolOutput::error(error),
        };
        let Some(id) = frames
            .get(frame)
            .and_then(|f| f.get("id"))
            .and_then(Value::as_i64)
        else {
            return ToolOutput::error(format!(
                "frame {frame} does not exist: the stack stopped at {} has {} frames",
                stop.reason,
                frames.len()
            ));
        };
        match session
            .dap
            .request(
                "evaluate",
                json!({ "expression": expression, "frameId": id, "context": "repl" }),
            )
            .await
        {
            Ok(answer) => {
                let value = short(field(&answer, "result").unwrap_or_default(), MAX_OUTPUT.min(2000));
                match field(&answer, "type").filter(|t| !t.is_empty()) {
                    Some(kind) => ToolOutput::text(format!("{expression}: {} = {value}", short(kind, 60))),
                    None => ToolOutput::text(format!("{expression} = {value}")),
                }
            }
            Err(error) => ToolOutput::error(format!("could not evaluate: {}", short(&error, 400))),
        }
    }
}

#[async_trait]
impl Tool for DebugTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "Debug a program with a debugger (Debug Adapter Protocol). One session at a time. `start` runs \
         `program` with `args` in `cwd` under the adapter for its `language` (rust, c, cpp, python or go; \
         guessed from the program when left out) and waits for it to stop or end; pass `breakpoints` \
         (`file`, `line`, `condition?`) to stop before it runs on, or `stop_on_entry`. Build the program \
         with debug info first; a test binary works too. `break` sets a breakpoint at `file` and `line` \
         (with `condition?`), or with `clear` removes them. `continue`, `step` (`kind`: over, in or out) \
         and `pause` wait up to `timeout_secs` for the next stop and report it. A stop is the reason, the \
         stack as `#n name at file:line` and the locals of the top frame; `state` reports it again. `eval` \
         evaluates `expression` in `frame` (0 is the top). `output` is what the program printed. `stop` \
         ends the session; it also ends when the agent does."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["start", "break", "continue", "step", "pause", "state", "eval", "output", "stop"]
                },
                "program": { "type": "string", "description": "start: the executable, test binary or script" },
                "args": { "type": "array", "items": { "type": "string" }, "description": "start: its arguments" },
                "cwd": { "type": "string", "description": "start: its working directory; the project's by default" },
                "language": {
                    "type": "string",
                    "description": "start: rust, c, cpp, python or go; or a language named in [debug.adapters]"
                },
                "stop_on_entry": { "type": "boolean", "description": "start: stop before the first line runs" },
                "breakpoints": {
                    "type": "array",
                    "description": "start: breakpoints to set before the program runs",
                    "items": {
                        "type": "object",
                        "properties": {
                            "file": { "type": "string" },
                            "line": { "type": "integer", "minimum": 1 },
                            "condition": { "type": "string" }
                        },
                        "required": ["file", "line"]
                    }
                },
                "file": { "type": "string", "description": "break: the source file" },
                "line": { "type": "integer", "minimum": 1, "description": "break: 1-based line" },
                "condition": { "type": "string", "description": "break: stop only when this expression is true" },
                "clear": { "type": "boolean", "description": "break: remove the breakpoint at `line`, or all of `file`'s" },
                "kind": { "type": "string", "enum": ["over", "in", "out"], "description": "step: over by default" },
                "expression": { "type": "string", "description": "eval: what to evaluate" },
                "frame": { "type": "integer", "minimum": 0, "description": "eval: stack frame, 0 is the top" },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "How long start, continue, step, pause and state wait for a stop"
                }
            },
            "required": ["action"]
        })
    }

    fn access(&self, input: &Value) -> Access {
        // Starting runs a program and an expression may call functions of it: both execute code.
        match input.get("action").and_then(Value::as_str) {
            Some("start" | "eval") => Access::Exec,
            _ => Access::Read,
        }
    }

    fn concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        let text = |key: &str| input.get(key).and_then(Value::as_str);
        match text("action") {
            // The command line the program is run with, as the permission rules see a shell command.
            Some("start") => {
                let mut line = text("program").unwrap_or_default().to_string();
                for arg in input.get("args").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(arg) = arg.as_str() {
                        line.push(' ');
                        line.push_str(arg);
                    }
                }
                vec![line]
            }
            Some("eval") => vec![format!("debug eval {}", text("expression").unwrap_or_default())],
            _ => Vec::new(),
        }
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        tokio::select! {
            output = self.call_action(input, ctx) => output,
            _ = ctx.cancel.cancelled() => ToolOutput::error(ABORTED_TEXT),
        }
    }
}

struct EndSessions(Arc<DebugManager>);

#[async_trait]
impl EventHandler for EndSessions {
    async fn handle(&self, event: &mut Event, _: &ExtCtx) -> Outcome {
        match event {
            Event::AgentEnd { agent, .. } => self.0.end(&agent.id).await,
            _ => self.0.shutdown().await,
        }
        Outcome::Continue
    }
}

/// Registers the [`DebugManager`] service and the `debug` tool.
pub struct DebugExt {
    /// `debug.adapters`: the adapter to use for a language, instead of the one found on `PATH`.
    pub adapters: BTreeMap<String, DebugAdapterConfig>,
    /// `debug.timeout_secs`: how long a call waits for the program to stop.
    pub timeout: Duration,
}

impl Extension for DebugExt {
    fn id(&self) -> &str {
        "debug"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        let manager = Arc::new(DebugManager::new(self));
        r.service(manager.clone());
        r.tool(Arc::new(DebugTool::new(manager.clone())));
        let ends = Arc::new(EndSessions(manager));
        r.on(EventKind::AgentEnd, ends.clone());
        r.on(EventKind::SessionEnd, ends);
        Ok(())
    }
}

use self::dap as dap_adapter;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_values_are_cut_and_say_so() {
        assert_eq!(short("  abc  ", 10), "abc");
        assert_eq!(
            short(&"x".repeat(12), 10),
            format!("{}… (2 more characters)", "x".repeat(10))
        );
        assert!(tail("abcdef", 3).ends_with("def") && tail("abcdef", 3).contains("3 earlier"));
    }
}
