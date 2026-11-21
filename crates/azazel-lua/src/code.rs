//! The Luau engine behind `eval`'s `luau` language: a script that calls the harness's tools, so an
//! agent can do a job of several steps in one call and get back only what it asks for.

use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, ToolOutput, ToolSource, UpdateSink};
use azazel_core::Post;
use azazel_core::config::CodeFirstListing;
use azazel_core::hooks::Wake;
use azazel_core::scripts::{SavedScript, Scripts};
use azazel_core::toolcall::{Background, Deferral, Luau, LuauEngine, deferring};
use azazel_core::transcript::activity;
use azazel_core::{
    ApprovalDecision, ApprovalRequest, ArtifactStore, EntryKind, ExtError, Extension, Registrar,
    SessionStore, ToolCaller, Ui, toolsearch,
};
use azazel_team::{MemberState, Team};
use azazel_tools::{Jobs, TaskEnd};
use futures::StreamExt;
use mlua::chunk::ChunkMode;
use mlua::{Function, Lua, MultiValue, Table, VmState};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::convert::{from_lua, to_lua, tool_input};
use crate::replay::{Journal, Kind, Lookup, Outcome, Site};
use crate::script::{describe, locked};

/// The tool a script runs in; it cannot call itself.
const EVAL: &str = "eval";
/// The description's list of callable tool names stays within about 500 tokens.
const LISTING_CHARS: usize = 2_000;
/// With `code_first_listing = "signatures"` the tools that are only callable from a script are
/// listed as signatures, within about 1.5k tokens; the rest of them by name.
const CODE_FIRST_LISTING_CHARS: usize = 6_000;
/// What `print` keeps; a script that prints more is told so.
const MAX_PRINTED_BYTES: usize = 1024 * 1024;
/// How long a cancelled script gets to wind down the tool call it is waiting for.
const CANCEL_GRACE: Duration = Duration::from_secs(2);
/// The tool that starts agents; a script reaches it through `agents.run`.
const TASK: &str = "task";
/// The tool that finds tools kept back until they are asked for (those of MCP servers, when
/// there are many) and makes what it finds callable.
const TOOL_SEARCH: &str = "tool_search";
/// How deep saved scripts may run one another with `tools.run`; the `eval` script is level 0.
const MAX_SCRIPT_DEPTH: usize = 3;
/// Tools `tools.search` returns unless told otherwise, and the most it returns.
const SEARCH_LIMIT: usize = 8;
const MAX_SEARCH_LIMIT: usize = 25;
/// How many calls `parallel` runs at once where no team says (`team.max_parallel`'s default).
const PARALLEL: usize = 4;
/// The kind of the session entries `store` writes.
// The name is the one from before `run_code` became part of `eval`, so earlier sessions still load.
const STORE_ENTRY: &str = "run_code_store";
/// The longest key `store` takes, and the most JSON one stored value may be.
const MAX_KEY_BYTES: usize = 200;
const MAX_STORED_BYTES: usize = 256 * 1024;

const INTRO: &str = "`language: \"luau\"` runs a script that calls your other tools, to do a job of several steps in one call: \
     read a dozen files and return only the lines that matter, search and then filter, make the same \
     edit in many places. `tools.<name>{ ... }` calls a tool with the arguments you would give it \
     yourself and returns its result as a string; a call that fails or is refused raises an error, \
     which `pcall` catches. Every call is checked against the permission rules like one you make \
     directly. What the script prints and the value it returns are the result; nothing else comes \
     back. `json.decode` and `json.encode` are there. `parallel{ f, g, ... }` runs functions at the \
     same time and returns what each returned, in the same places. `agents.run(agent, prompt, { \
     model?, isolation?, schema? })` starts an agent as `task` does and returns its report, or with \
     a `schema` the value it reported. `classify(question, { \"a\", \"b\", ... }, { model? })` has a \
     small model pick one choice and returns `{ choice, index, confidence, reason }`. `store(key, \
     value)` keeps a JSON value in the session for later scripts; `load(key)` reads it. \
     `tools.search(query)` finds tools not listed below and the user's saved scripts, which \
     `tools.run(name, input)` runs; `tools.describe(name)` returns a tool's description and parameters. Limits: 120 s of script time (waiting for a tool, an agent or a \
     model does not count), 50 calls of tools, agents and `classify` together, 64 KB of result. \
     In the background a call that needs the user's approval pauses the script; once approved it runs again from the start with earlier results replayed, so make the same calls in the same order and wrap os.time or math.random in `step(name, fn)`. \
     `eval` and the agent tools cannot be called through `tools`.";

/// What bounds one script.
#[derive(Debug, Clone, Copy)]
pub struct CodeLimits {
    /// Running time of the script's own code; waiting for a tool does not count.
    pub script_time: Duration,
    pub tool_calls: usize,
    /// Bytes of result returned; a longer one is stored as an artifact.
    pub result_bytes: usize,
}

impl Default for CodeLimits {
    fn default() -> Self {
        CodeLimits {
            script_time: Duration::from_secs(120),
            tool_calls: 50,
            result_bytes: 64 * 1024,
        }
    }
}

/// Registers the Luau engine `eval` runs scripts with.
#[derive(Default)]
pub struct CodeExt {
    pub limits: CodeLimits,
}

impl Extension for CodeExt {
    fn id(&self) -> &str {
        "code"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.service(Arc::new(Luau(Arc::new(LuauEval { limits: self.limits }))));
        Ok(())
    }
}

/// Tools a script cannot call: itself, and those that start and steer other agents.
fn blocked(name: &str) -> bool {
    name == EVAL || azazel_team::ORCHESTRATION_TOOLS.contains(&name)
}

/// The type of one argument as a script author reads it.
fn type_name(schema: &Value) -> String {
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && options.len() <= 4
        && options.iter().all(Value::is_string)
    {
        let quoted: Vec<String> = options.iter().map(Value::to_string).collect();
        return quoted.join("|");
    }
    match schema.get("type") {
        Some(Value::String(name)) => name.clone(),
        Some(Value::Array(names)) => {
            let names: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
            names.join("|")
        }
        _ => "any".to_string(),
    }
}

/// `name{ arg: type, optional?: type }: what it does`, on one line.
fn signature(tool: &dyn Tool) -> String {
    let schema = tool.schema();
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let arguments: Vec<String> = schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(name, property)| {
            let mark = if required.contains(&name.as_str()) {
                ""
            } else {
                "?"
            };
            format!("{name}{mark}: {}", type_name(property))
        })
        .collect();
    format!(
        "- {}{{ {} }}: {}",
        tool.name(),
        arguments.join(", "),
        first_sentence(tool.description())
    )
}

/// What a tool does, in the first sentence of its description.
fn first_sentence(description: &str) -> String {
    let sentence = description
        .split_once(". ")
        .map_or(description, |(first, _)| first)
        .lines()
        .next()
        .unwrap_or_default();
    sentence.trim_end_matches('.').chars().take(140).collect()
}

/// `text` cut to at most `max` bytes, with `…` where it was cut.
fn cut(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

/// `mcp__<server>__<tool>` names of one server counted on one line: `mcp__<server> (N tools)`.
/// Other names stay as they are, in the order given.
fn grouped_names(tools: &[&Arc<dyn Tool>]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut servers: Vec<(String, usize)> = Vec::new();
    for tool in tools {
        let name = tool.name();
        match name.strip_prefix("mcp__").and_then(|rest| rest.split_once("__")) {
            Some((server, _)) => {
                let server = format!("mcp__{server}");
                match servers.iter_mut().find(|(known, _)| *known == server) {
                    Some((_, count)) => *count += 1,
                    None => servers.push((server, 1)),
                }
            }
            None => parts.push(name.to_string()),
        }
    }
    parts.extend(
        servers
            .into_iter()
            .map(|(server, count)| format!("{server} ({count} tool{})", if count == 1 { "" } else { "s" })),
    );
    parts.join(", ")
}

/// The description with the tools a script can call. The tools the model is offered are listed by
/// name only: their arguments are in its tool list, and `tools.describe` gives them for the rest.
/// Listing every signature here cost more than a thousand tokens of every request. The `hidden`
/// tools (code-first mode) are in no tool list: with [`CodeFirstListing::Names`] they are listed by
/// name, a server's MCP tools as one counted entry, and the model asks `tools.search` and
/// `tools.describe` for arguments; with `Signatures` their signatures are listed, as many as fit
/// in [`CODE_FIRST_LISTING_CHARS`], and the rest by name.
fn describe_tools(tools: &[Arc<dyn Tool>], hidden: &[Arc<dyn Tool>], listing: CodeFirstListing) -> String {
    let names = |tools: &[&Arc<dyn Tool>]| {
        cut(
            tools
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>()
                .join(", "),
            LISTING_CHARS,
        )
    };
    let is_hidden = |tool: &Arc<dyn Tool>| hidden.iter().any(|other| other.name() == tool.name());
    let offered: Vec<&Arc<dyn Tool>> = tools
        .iter()
        .filter(|tool| !blocked(tool.name()) && !is_hidden(tool))
        .collect();
    let mut out = format!("{INTRO}\n\nCallable tools: {}\n", names(&offered));
    let hidden: Vec<&Arc<dyn Tool>> = hidden.iter().filter(|tool| !blocked(tool.name())).collect();
    if !hidden.is_empty() && listing == CodeFirstListing::Names {
        out.push_str(&format!(
            "\nTools you call only from a script (they are not in your tool list; `tools.search(query)` \
             finds one and `tools.describe(name)` gives its arguments; an `mcp__<server>` entry stands for \
             that server's tools, named `mcp__<server>__<tool>`): {}\n",
            cut(grouped_names(&hidden), LISTING_CHARS)
        ));
    } else if !hidden.is_empty() {
        out.push_str("\nTools you call only from a script (they are not in your tool list):\n");
        let mut used = 0;
        let mut listed = 0;
        for tool in &hidden {
            let line = signature(tool.as_ref());
            if used + line.len() + 1 > CODE_FIRST_LISTING_CHARS {
                break;
            }
            used += line.len() + 1;
            listed += 1;
            out.push_str(&line);
            out.push('\n');
        }
        if listed < hidden.len() {
            out.push_str(&format!("More, by name: {}\n", names(&hidden[listed..])));
        }
    }
    out.push_str(
        "Any other tool you have is callable by name in the same way; a name that is not an identifier \
         is written `tools[\"the-name\"]{ ... }`.",
    );
    out
}

/// Runs the scripts of `eval`. `eval` keeps the description it gets from `describe`: one that
/// changed between requests would break the provider's prompt cache.
pub struct LuauEval {
    limits: CodeLimits,
}

/// What bounds the script code running right now.
#[derive(Default)]
struct Limit {
    deadline: Option<Instant>,
}

/// One run of a script.
struct Run {
    ctx: ToolCtx,
    caller: Arc<ToolCaller>,
    limits: CodeLimits,
    calls: AtomicUsize,
    printed: Mutex<String>,
    /// Bytes `print` dropped because the output was already too long.
    dropped: AtomicUsize,
    limit: Mutex<Limit>,
    /// How many calls `parallel` runs at once.
    width: usize,
    /// Held shared by a call that may run beside others and alone by one that may not, so the
    /// calls of a `parallel` keep the order an agent's own calls would.
    turns: tokio::sync::RwLock<()>,
    /// What the earlier passes of a background run that can pause did; `None` in the foreground
    /// and in a run that cannot pause.
    journal: Option<Arc<Journal>>,
    /// What the gate does at a call that needs approval; set in every background run.
    deferral: Option<Arc<Deferral>>,
    pass: PassState,
}

/// What happens in one pass of a script that can pause.
#[derive(Default)]
struct PassState {
    /// A call paused the run or the run diverged: no call may start any more.
    stopping: AtomicBool,
    /// Calls that were started and have not settled.
    in_flight: AtomicUsize,
    settled: tokio::sync::Notify,
    paused: tokio::sync::Notify,
    pending: Mutex<Option<(Site, ApprovalRequest)>>,
    diverged: Mutex<Option<String>>,
    /// The places of the calls this pass made.
    seen: Mutex<BTreeSet<Site>>,
}

/// Where the script is among the branches of `parallel`: calls take their places from it.
struct Branch {
    path: Vec<u32>,
    next: AtomicU32,
}

impl Branch {
    fn new(path: Vec<u32>) -> Arc<Branch> {
        Arc::new(Branch {
            path,
            next: AtomicU32::new(0),
        })
    }
}

tokio::task_local! {
    static BRANCH: Arc<Branch>;
    /// Set while the function of a `step` runs.
    static IN_STEP: ();
}

/// The place of the next call of the branch the code asking runs in.
fn next_site() -> Site {
    BRANCH
        .try_with(|branch| {
            let mut path = branch.path.clone();
            path.push(branch.next.fetch_add(1, Ordering::SeqCst) + 1);
            Site(path)
        })
        .unwrap_or_else(|_| Site(vec![0]))
}

fn in_step() -> bool {
    IN_STEP.try_with(|_| ()).is_ok()
}

fn runtime(message: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(message)
}

impl Run {
    fn new(
        ctx: ToolCtx,
        caller: Arc<ToolCaller>,
        limits: CodeLimits,
        width: usize,
        journal: Option<Arc<Journal>>,
        deferral: Option<Arc<Deferral>>,
    ) -> Run {
        Run {
            ctx,
            caller,
            limits,
            calls: AtomicUsize::new(0),
            printed: Mutex::default(),
            dropped: AtomicUsize::new(0),
            limit: Mutex::default(),
            width,
            turns: tokio::sync::RwLock::default(),
            journal,
            deferral,
            pass: PassState::default(),
        }
    }

    /// Ends this pass without a result: the call that asked never returns, and no other starts.
    async fn halt(&self) -> Result<Value, String> {
        self.pass.stopping.store(true, Ordering::SeqCst);
        self.pass.paused.notify_one();
        std::future::pending().await
    }

    /// Makes one call whose result steers the script: calls `live` with the call's place, or, in a
    /// run that can pause, returns what an earlier pass recorded for the place. A call that the
    /// gate wants approved ends the pass here. `counts` takes one of the script's calls.
    async fn recorded<F, Fut>(&self, kind: Kind, args: Value, counts: bool, live: F) -> Result<Value, String>
    where
        F: FnOnce(Site) -> Fut,
        Fut: Future<Output = Result<Value, String>>,
    {
        if in_step() {
            return Err(format!("step: {kind} cannot be called inside a step"));
        }
        let site = next_site();
        if counts {
            self.count()?;
        }
        let Some(journal) = &self.journal else {
            return live(site).await;
        };
        locked(&self.pass.seen).insert(site.clone());
        match journal.lookup(&site, &kind, &args) {
            Lookup::Replay(Outcome::Ok(value)) => return Ok(value),
            Lookup::Replay(Outcome::Err(text)) => return Err(text),
            Lookup::Diverged(text) => {
                locked(&self.pass.diverged).get_or_insert(text);
                return self.halt().await;
            }
            Lookup::Live | Lookup::Approved => {}
        }
        if self.pass.stopping.load(Ordering::SeqCst) {
            return self.halt().await;
        }
        journal.begin(site.clone(), kind, args);
        self.pass.in_flight.fetch_add(1, Ordering::SeqCst);
        let result = live(site.clone()).await;
        self.pass.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.pass.settled.notify_one();
        let call_id = format!("{}.{site}", self.ctx.call_id);
        if let Some(request) = self.deferral.as_ref().and_then(|d| d.take_pending(&call_id)) {
            journal.pend(&site);
            locked(&self.pass.pending).get_or_insert((site, request));
            return self.halt().await;
        }
        let outcome = match &result {
            Ok(value) => Outcome::Ok(value.clone()),
            Err(text) => Outcome::Err(text.clone()),
        };
        if let Err(why) = journal.applied(&site, outcome)
            && let Some(deferral) = &self.deferral
        {
            deferral.block(why);
        }
        result
    }

    /// Called by the VM at every loop iteration and function call of the script.
    fn interrupt(&self) -> mlua::Result<VmState> {
        if self.ctx.cancel.is_cancelled() {
            return Err(mlua::Error::runtime("cancelled"));
        }
        if locked(&self.limit)
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            let allowed = self.limits.script_time;
            let allowed = match allowed.as_secs() {
                0 => format!("{} ms", allowed.as_millis()),
                secs => format!("{secs} s"),
            };
            return Err(mlua::Error::runtime(format!(
                "the script used its {allowed} of running time without finishing"
            )));
        }
        Ok(VmState::Continue)
    }

    fn print(&self, values: MultiValue) -> mlua::Result<()> {
        let mut line = String::new();
        for (i, value) in values.iter().enumerate() {
            if i > 0 {
                line.push('\t');
            }
            line.push_str(&value.to_string()?);
        }
        line.push('\n');
        let mut printed = locked(&self.printed);
        if printed.len() + line.len() > MAX_PRINTED_BYTES {
            self.dropped.fetch_add(line.len(), Ordering::Relaxed);
        } else {
            printed.push_str(&line);
        }
        Ok(())
    }

    /// Calls tool `name` for the agent that runs the script, through the gate. `Err` is what
    /// the script is told when the call fails or is not made.
    async fn call_tool(&self, lua: &Lua, name: &str, args: mlua::Value) -> Result<String, String> {
        if blocked(name) {
            return Err(format!("`{name}` cannot be called from a script"));
        }
        let input = tool_input(lua, args).map_err(|e| format!("tools.{name}: {}", describe(&e)))?;
        let live = input.clone();
        let text = self
            .recorded(Kind::Tool(name.to_string()), input, true, |site| async move {
                self.gated(name, live, &site)
                    .await
                    .map(|output| Value::String(output.text_content()))
            })
            .await?;
        Ok(text.as_str().unwrap_or_default().to_string())
    }

    /// Takes one of the script's calls; `Err` when it has used them up.
    fn count(&self) -> Result<usize, String> {
        let number = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if number > self.limits.tool_calls {
            return Err(format!(
                "the limit of {} tool calls per script is reached",
                self.limits.tool_calls
            ));
        }
        Ok(number)
    }

    /// One call of tool `name` through the gate, under the call id of its place. A call that
    /// may run beside others does; one that may not (an edit, a command) waits until no other
    /// call of the script is running.
    async fn gated(&self, name: &str, input: Value, site: &Site) -> Result<ToolOutput, String> {
        self.ctx.update.send(activity(name, &input));
        let ctx = ToolCtx {
            call_id: format!("{}.{site}", self.ctx.call_id),
            ..self.ctx.clone()
        };
        let shares = self
            .caller
            .available(&ctx)
            .iter()
            .find(|tool| tool.name() == name)
            .is_none_or(|tool| tool.concurrency_safe(&input));
        let output = if shares {
            let _beside = self.turns.read().await;
            self.caller.call(&ctx, name, input).await
        } else {
            let _alone = self.turns.write().await;
            self.caller.call(&ctx, name, input).await
        };
        if output.is_error {
            Err(format!("{name}: {}", output.text_content()))
        } else {
            Ok(output)
        }
    }

    /// The tools a script of this agent can call.
    fn callable(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools = self.caller.available(&self.ctx);
        tools.retain(|tool| !blocked(tool.name()));
        tools
    }

    /// `tools.search`: the callable tools that match `query`, best first, as `{ name,
    /// description }` with the first sentence of each description.
    async fn search_tools(
        &self,
        lua: &Lua,
        query: String,
        limit: Option<usize>,
    ) -> mlua::Result<mlua::Value> {
        if query.trim().is_empty() {
            return Err(runtime("tools.search: the query is empty"));
        }
        let limit = limit.unwrap_or(SEARCH_LIMIT).clamp(1, MAX_SEARCH_LIMIT);
        // What `tool_search` finds becomes callable, and is then among the tools ranked here.
        let held_back = self
            .caller
            .available(&self.ctx)
            .iter()
            .any(|tool| tool.name() == TOOL_SEARCH);
        if held_back {
            let site = next_site();
            if self.count().is_ok() {
                let _ = self
                    .gated(TOOL_SEARCH, json!({ "query": query, "limit": limit }), &site)
                    .await;
            }
        }
        let tools = self.callable();
        let saved = self.saved();
        let mut documents: Vec<(String, String)> = tools
            .iter()
            .map(|tool| (tool.name().to_string(), tool.description().to_string()))
            .collect();
        documents.extend(saved.iter().map(|script| {
            let input = script.input.as_deref().unwrap_or_default();
            (script.name.clone(), format!("{} {input}", script.description))
        }));
        let hits: Vec<Value> = toolsearch::rank(&documents, &query, limit)
            .into_iter()
            .filter_map(|index| {
                match tools.get(index) {
                Some(tool) => {
                    Some(json!({ "name": tool.name(), "description": first_sentence(tool.description()) }))
                }
                None => saved.get(index - tools.len()).map(|script| {
                    json!({ "name": script.name, "description": script.description, "kind": "script" })
                }),
            }
            })
            .collect();
        to_lua(lua, &Value::Array(hits))
    }

    /// The saved scripts `tools.run` can run.
    fn saved(&self) -> Vec<SavedScript> {
        self.ctx
            .services
            .get::<Scripts>()
            .map(|scripts| scripts.list())
            .unwrap_or_default()
    }

    /// `tools.describe`: everything about one callable tool or saved script, or nil when there
    /// is none of that name.
    fn describe_tool(&self, lua: &Lua, name: &str) -> mlua::Result<mlua::Value> {
        let Some(tool) = self.callable().into_iter().find(|tool| tool.name() == name) else {
            let Some(script) = self.saved().into_iter().find(|script| script.name == name) else {
                return Ok(mlua::Value::Nil);
            };
            let described = json!({
                "kind": "script",
                "name": script.name,
                "description": script.description,
                "input": script.input,
                "signature": format!("tools.run(\"{}\", input)", script.name),
            });
            return to_lua(lua, &described);
        };
        let described = json!({
            "name": tool.name(),
            "description": tool.description(),
            "parameters": tool.schema(),
            "signature": signature(tool.as_ref()).trim_start_matches("- "),
        });
        to_lua(lua, &described)
    }

    /// `tools.run`: runs the saved script `name` in a VM of its own with `input` as its global
    /// `input`, and returns `(value, nil)`, or `(nil, message)` when it cannot be run or fails.
    /// Its tool calls pass the gate and count against the limits like the caller's own.
    async fn run_saved(
        self: &Arc<Self>,
        lua: &Lua,
        depth: usize,
        name: String,
        input: mlua::Value,
    ) -> mlua::Result<(mlua::Value, mlua::Value)> {
        let failed = |message: String| {
            Ok((
                mlua::Value::Nil,
                mlua::Value::String(lua.create_string(&message)?),
            ))
        };
        if depth >= MAX_SCRIPT_DEPTH {
            return failed(format!(
                "tools.run: scripts may run others only {MAX_SCRIPT_DEPTH} levels deep"
            ));
        }
        let Some(script) = self.ctx.services.get::<Scripts>().and_then(|s| s.find(&name)) else {
            return failed(format!("tools.run: there is no saved script `{name}`"));
        };
        let input = match input {
            mlua::Value::Nil => Value::Null,
            other => match from_lua(lua, other) {
                Ok(value) => value,
                Err(e) => return failed(format!("tools.run: the input: {}", describe(&e))),
            },
        };
        match self.nested(script.source, &name, input, depth + 1).await {
            Ok(value) => Ok((to_lua(lua, &value)?, mlua::Value::Nil)),
            Err(message) => failed(format!("tools.run: `{name}`: {message}")),
        }
    }

    /// Runs `code` in a new VM at nesting level `depth`, inside the running script's time and
    /// call limits; returns the first value it returned, as JSON.
    async fn nested(
        self: &Arc<Self>,
        code: String,
        name: &str,
        input: Value,
        depth: usize,
    ) -> Result<Value, String> {
        let lua = Lua::new();
        install(&lua, self, depth).map_err(|e| describe(&e))?;
        let input = to_lua(&lua, &input).map_err(|e| describe(&e))?;
        lua.globals().raw_set("input", input).map_err(|e| describe(&e))?;
        lua.sandbox(true).map_err(|e| describe(&e))?;
        let watched = self.clone();
        lua.set_interrupt(move |_| watched.interrupt());
        let function = lua
            .load(code)
            .set_name(format!("={name}"))
            .set_mode(ChunkMode::Text)
            .into_function()
            .map_err(|e| describe(&e))?;
        let returned = function
            .call_async::<MultiValue>(())
            .await
            .map_err(|e| describe(&e))?;
        match returned.into_iter().next() {
            Some(value) => from_lua(&lua, value).map_err(|e| describe(&e)),
            None => Ok(Value::Null),
        }
    }

    /// `agents.run`: starts one agent through the `task` tool, so the call is checked like
    /// one the agent makes itself, waits for it, and returns `(result, id)`: the value of a
    /// typed result, else the report.
    async fn run_agent(
        &self,
        lua: &Lua,
        (def, prompt, options): (String, String, Option<Table>),
    ) -> mlua::Result<(mlua::Value, String)> {
        const NAME: &str = "agents.run";
        let mut agent = json!({ "agent": def, "prompt": prompt });
        if let Some(options) = options {
            let options = from_lua(lua, mlua::Value::Table(options))
                .map_err(|e| runtime(format!("{NAME}: the options: {}", describe(&e))))?;
            for (key, value) in options.as_object().into_iter().flatten() {
                if !["model", "isolation", "schema", "name"].contains(&key.as_str()) {
                    return Err(runtime(format!(
                        "{NAME}: there is no option `{key}`; the options are model, isolation, schema and name"
                    )));
                }
                agent[key.as_str()] = value.clone();
            }
        }
        let offered = self
            .caller
            .available(&self.ctx)
            .iter()
            .any(|tool| tool.name() == TASK);
        let team = self.ctx.services.get::<Team>().filter(|_| offered);
        let Some(team) = team else {
            return Err(runtime(format!("{NAME}: this agent cannot start agents")));
        };
        let recorded = self
            .recorded(Kind::Agent, agent.clone(), true, |site| async move {
                let output = self
                    .gated(TASK, json!({ "agents": [agent], "wait": true }), &site)
                    .await
                    .map_err(|why| why.trim_start_matches("task: ").to_string())?;
                let finished = output
                    .details
                    .as_ref()
                    .and_then(|details| details.pointer("/agents/0/id"))
                    .and_then(Value::as_str)
                    .and_then(|id| team.result(id))
                    .ok_or_else(|| "the agent left no result".to_string())?;
                if finished.state != MemberState::Done {
                    return Err(format!(
                        "agent `{}` {}: {}",
                        finished.id, finished.state, finished.report
                    ));
                }
                let value = match finished.json.as_deref().map(serde_json::from_str::<Value>) {
                    Some(Ok(value)) => value,
                    _ => Value::String(finished.report),
                };
                Ok(json!([value, finished.id]))
            })
            .await
            .map_err(|why| runtime(format!("{NAME}: {why}")))?;
        let id = recorded[1].as_str().unwrap_or_default().to_string();
        Ok((to_lua(lua, &recorded[0])?, id))
    }

    /// `parallel`: runs every function of `calls` at once, at most `width` at a time, and
    /// returns what each returned first under the key its function had. All of them are
    /// waited for; the first failure is then raised.
    async fn parallel(&self, lua: &Lua, calls: Table) -> mlua::Result<Table> {
        if in_step() {
            return Err(runtime("step: parallel cannot be called inside a step"));
        }
        // The branches take their calls' places from this one's, so that a replay finds each call
        // where it was whatever order the branches ran in.
        let site = next_site();
        let mut keys = Vec::new();
        let mut functions: Vec<Function> = Vec::new();
        for pair in calls.pairs::<mlua::Value, mlua::Value>() {
            let (key, value) = pair?;
            let mlua::Value::Function(function) = value else {
                return Err(runtime(format!(
                    "parallel: every entry must be a function; entry `{}` is not",
                    key.to_string()?
                )));
            };
            keys.push(key);
            functions.push(function);
        }
        let results: Vec<mlua::Result<mlua::Value>> =
            futures::stream::iter(functions.into_iter().enumerate().map(|(i, function)| {
                let mut path = site.0.clone();
                path.push(i as u32 + 1);
                BRANCH.scope(Branch::new(path), async move {
                    function.call_async::<mlua::Value>(()).await
                })
            }))
            .buffered(self.width)
            .collect()
            .await;
        let out = lua.create_table()?;
        let mut failure = None;
        for (key, result) in keys.into_iter().zip(results) {
            match result {
                Ok(value) => out.raw_set(key, value)?,
                Err(e) if failure.is_none() => {
                    failure = Some(format!(
                        "parallel: entry `{}`: {}",
                        key.to_string()?,
                        describe(&e)
                    ));
                }
                Err(_) => {}
            }
        }
        match failure {
            Some(failure) => Err(runtime(failure)),
            None => Ok(out),
        }
    }

    fn session(&self, function: &str) -> mlua::Result<Arc<SessionStore>> {
        self.ctx
            .services
            .get::<SessionStore>()
            .ok_or_else(|| runtime(format!("{function}: no session is open")))
    }

    /// `store`: records `value` under `key` where the conversation is now, so that later
    /// scripts on this branch of the session find it. `nil` forgets the key.
    async fn store(&self, lua: &Lua, key: String, value: mlua::Value) -> mlua::Result<()> {
        const NAME: &str = "store";
        if key.is_empty() || key.len() > MAX_KEY_BYTES {
            return Err(runtime(format!(
                "{NAME}: the key must be 1 to {MAX_KEY_BYTES} bytes long"
            )));
        }
        let value = match value {
            mlua::Value::Nil => Value::Null,
            value => {
                from_lua(lua, value).map_err(|e| runtime(format!("{NAME}: `{key}`: {}", describe(&e))))?
            }
        };
        let size = serde_json::to_string(&value).map_or(0, |text| text.len());
        if size > MAX_STORED_BYTES {
            return Err(runtime(format!(
                "{NAME}: the value of `{key}` is {size} bytes of JSON; one value may be {MAX_STORED_BYTES}. Keep \
                 large data in a file and store its path"
            )));
        }
        // On a replay the call is not repeated: the session gets the entry once.
        self.recorded(Kind::Store, json!([key, value]), false, |_| async {
            self.session(NAME)
                .map_err(|e| describe(&e))?
                .append(EntryKind::Custom {
                    custom_type: STORE_ENTRY.to_string(),
                    data: json!({ "key": key, "value": value }),
                });
            Ok(Value::Null)
        })
        .await
        .map_err(runtime)?;
        Ok(())
    }

    /// `load`: what was last stored under `key` on this branch of the session, or nil.
    async fn load(&self, lua: &Lua, key: &str) -> mlua::Result<mlua::Value> {
        let stored = self
            .recorded(Kind::Load, json!(key), false, |_| async {
                let session = self.session("load").map_err(|e| describe(&e))?;
                let stored = session.latest(None, |entry| match &entry.kind {
                    EntryKind::Custom { custom_type, data }
                        if custom_type == STORE_ENTRY
                            && data.get("key").and_then(Value::as_str) == Some(key) =>
                    {
                        Some(data.get("value").cloned().unwrap_or(Value::Null))
                    }
                    _ => None,
                });
                Ok(stored.unwrap_or(Value::Null))
            })
            .await
            .map_err(runtime)?;
        to_lua(lua, &stored)
    }

    /// `step`: runs `function` and returns its value; in a background run the value is kept, so
    /// that when the script runs again after an approval it is the same one.
    async fn step(&self, lua: &Lua, name: String, function: Function) -> mlua::Result<mlua::Value> {
        let value = self
            .recorded(Kind::Step(name.clone()), Value::Null, false, |_| async {
                let returned = IN_STEP
                    .scope((), function.call_async::<mlua::Value>(()))
                    .await
                    .map_err(|e| describe(&e))?;
                from_lua(lua, returned).map_err(|_| {
                    format!("step `{name}`: the value must be nil, a boolean, a number, a string or a table")
                })
            })
            .await
            .map_err(runtime)?;
        to_lua(lua, &value)
    }
}

/// `tools`: indexing it with a tool's name gives a function that calls the tool. A failure is
/// raised from Luau, so that the script gets a plain string naming the line of the call.
/// `tools.search`, `tools.describe` and `tools.run` take a string; called with a table they are the
/// tools of those names, if there are any.
const TOOLS: &str = r#"
local call, search, describe, run = ...
local finders = { search = search, describe = describe, run = run }
return setmetatable({}, {
    __index = function(_, name)
        local find = finders[name]
        return function(args, ...)
            if find ~= nil and type(args) ~= "table" then
                return find(args, ...)
            end
            local ok, text = call(name, args)
            if not ok then
                error(text, 2)
            end
            return text
        end
    end,
})
"#;

/// The globals of a script: `tools`, `agents`, `parallel`, `classify`, `store`, `load`, `print`
/// and `json`.
fn install(lua: &Lua, run: &Arc<Run>, depth: usize) -> mlua::Result<()> {
    // Luau's `require` reads source files from disk; a script reaches files through `tools` only.
    lua.globals().raw_set("require", mlua::Nil)?;
    let state = run.clone();
    let call = lua.create_async_function(move |lua, (name, args): (String, mlua::Value)| {
        let state = state.clone();
        async move {
            Ok(match state.call_tool(&lua, &name, args).await {
                Ok(text) => (true, text),
                Err(why) => (false, why),
            })
        }
    })?;
    let state = run.clone();
    let search = lua.create_async_function(move |lua, (query, limit): (String, Option<usize>)| {
        let state = state.clone();
        async move { state.search_tools(&lua, query, limit).await }
    })?;
    let state = run.clone();
    let describe_tool = lua.create_function(move |lua, name: String| state.describe_tool(lua, &name))?;
    let state = run.clone();
    let run_saved = lua.create_async_function(move |lua, (name, input): (String, mlua::Value)| {
        let state = state.clone();
        async move { state.run_saved(&lua, depth, name, input).await }
    })?;
    let tools: Table = lua
        .load(TOOLS)
        .set_name("=tools")
        .call((call, search, describe_tool, run_saved))?;
    tools.set_readonly(true);
    lua.globals().set("tools", tools)?;

    let agents = lua.create_table()?;
    let state = run.clone();
    let start = lua.create_async_function(move |lua, args: (String, String, Option<Table>)| {
        let state = state.clone();
        async move { state.run_agent(&lua, args).await }
    })?;
    agents.set("run", start)?;
    agents.set_readonly(true);
    lua.globals().set("agents", agents)?;

    let state = run.clone();
    let parallel = lua.create_async_function(move |lua, calls: Table| {
        let state = state.clone();
        async move { state.parallel(&lua, calls).await }
    })?;
    lua.globals().set("parallel", parallel)?;

    let state = run.clone();
    let classify = lua.create_async_function(move |lua, args: (String, Vec<String>, Option<Table>)| {
        let state = state.clone();
        async move {
            let options = match &args.2 {
                Some(options) => from_lua(&lua, mlua::Value::Table(options.clone())).unwrap_or(Value::Null),
                None => Value::Null,
            };
            let recorded = json!([args.0, args.1, options]);
            let answer = state
                .recorded(Kind::Classify, recorded, true, |_| async {
                    state.ctx.update.send("classify".to_string());
                    let (services, agent, cancel) =
                        (&state.ctx.services, &state.ctx.agent, state.ctx.cancel.clone());
                    let table = crate::models::judge(&lua, services, agent, "classify", args, cancel)
                        .await
                        .map_err(|e| describe(&e))?;
                    from_lua(&lua, mlua::Value::Table(table)).map_err(|e| describe(&e))
                })
                .await
                .map_err(runtime)?;
            to_lua(&lua, &answer)
        }
    })?;
    lua.globals().set("classify", classify)?;

    let state = run.clone();
    let store = lua.create_async_function(move |lua, (key, value): (String, mlua::Value)| {
        let state = state.clone();
        async move { state.store(&lua, key, value).await }
    })?;
    lua.globals().set("store", store)?;
    let state = run.clone();
    let load = lua.create_async_function(move |lua, key: String| {
        let state = state.clone();
        async move { state.load(&lua, &key).await }
    })?;
    lua.globals().set("load", load)?;
    let state = run.clone();
    let step = lua.create_async_function(move |lua, (name, function): (String, Function)| {
        let state = state.clone();
        async move { state.step(&lua, name, function).await }
    })?;
    lua.globals().set("step", step)?;

    let state = run.clone();
    let print = lua.create_function(move |_, values: MultiValue| state.print(values))?;
    lua.globals().set("print", print)?;
    lua.globals().set("json", crate::api::json_table(lua)?)
}

/// A value the script returned, as text: a string as it is, a table as JSON.
fn returned_text(lua: &Lua, value: mlua::Value) -> Option<String> {
    match value {
        mlua::Value::Nil => None,
        mlua::Value::String(text) => Some(text.to_string_lossy()),
        mlua::Value::Table(_) => match from_lua(lua, value.clone()) {
            Ok(json) => serde_json::to_string_pretty(&json).ok(),
            Err(_) => value.to_string().ok(),
        },
        other => other.to_string().ok(),
    }
}

/// The script, driven to completion by awaiting it. Its own code runs only inside `poll`, under
/// what is left of its running time.
struct Script<'a> {
    call: mlua::function::AsyncCallFuture<MultiValue>,
    run: &'a Run,
    left: Duration,
}

impl Future for Script<'_> {
    type Output = mlua::Result<MultiValue>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let started = Instant::now();
        locked(&this.run.limit).deadline = Some(started + this.left);
        let polled = Pin::new(&mut this.call).poll(cx);
        locked(&this.run.limit).deadline = None;
        this.left = this.left.saturating_sub(started.elapsed());
        polled
    }
}

/// How one pass of a script ended.
enum PassEnd {
    Finished(String),
    Failed(String),
    /// A call needs the user's approval: the pass stopped there.
    Paused {
        site: Site,
        request: ApprovalRequest,
        printed: String,
    },
    /// The script did not repeat its earlier calls.
    Diverged(String),
}

/// A VM with the globals of a script, and `code` loaded as its function.
fn prepare(run: &Arc<Run>, code: String) -> Result<(Lua, Function), String> {
    let lua = Lua::new();
    install(&lua, run, 0).map_err(|e| describe(&e))?;
    lua.sandbox(true).map_err(|e| describe(&e))?;
    let watched = run.clone();
    lua.set_interrupt(move |_| watched.interrupt());
    let function = lua
        .load(code)
        .set_name("=eval")
        .set_mode(ChunkMode::Text)
        .into_function()
        .map_err(|e| describe(&e))?;
    Ok((lua, function))
}

/// Runs `code` in a fresh sandboxed VM and returns what it printed and returned, or its error
/// after what it printed.
async fn execute(run: Arc<Run>, code: String) -> PassEnd {
    BRANCH.scope(Branch::new(Vec::new()), run_pass(run, code)).await
}

async fn run_pass(run: Arc<Run>, code: String) -> PassEnd {
    let (lua, function) = match prepare(&run, code) {
        Ok(prepared) => prepared,
        Err(why) => return PassEnd::Failed(why),
    };
    let mut script = Script {
        call: function.call_async(()),
        run: &run,
        left: run.limits.script_time,
    };
    enum Woke {
        Returned(mlua::Result<MultiValue>),
        Cancelled,
        Halted,
    }
    let cancel: CancellationToken = run.ctx.cancel.clone();
    let woke = tokio::select! {
        returned = &mut script => Woke::Returned(returned),
        _ = cancel.cancelled() => Woke::Cancelled,
        _ = run.pass.paused.notified() => Woke::Halted,
    };
    let returned = match woke {
        Woke::Returned(returned) => Some(returned),
        Woke::Cancelled => Some(match tokio::time::timeout(CANCEL_GRACE, &mut script).await {
            Ok(returned) => returned,
            Err(_) => Err(mlua::Error::runtime("cancelled")),
        }),
        // The calls that are running settle (and are recorded) before the VM is dropped: the
        // script is polled until then, so that a sibling's side effect is not repeated.
        Woke::Halted => loop {
            if run.pass.in_flight.load(Ordering::SeqCst) == 0 {
                break None;
            }
            tokio::select! {
                returned = &mut script => break Some(returned),
                _ = run.pass.settled.notified() => {}
                _ = cancel.cancelled() => break None,
            }
        },
    };
    drop(script);

    let mut out = std::mem::take(&mut *locked(&run.printed));
    let dropped = run.dropped.load(Ordering::Relaxed);
    if dropped > 0 {
        out.push_str(&format!(
            "[{dropped} more bytes of printed output were dropped]\n"
        ));
    }
    if let Some(text) = locked(&run.pass.diverged).take() {
        return PassEnd::Diverged(text);
    }
    if let Some((site, request)) = locked(&run.pass.pending).take() {
        return PassEnd::Paused {
            site,
            request,
            printed: out,
        };
    }
    match returned {
        Some(Ok(values)) => {
            if let Some(text) = run
                .journal
                .as_ref()
                .and_then(|journal| journal.unreached(&locked(&run.pass.seen)))
            {
                return PassEnd::Diverged(text);
            }
            for value in values {
                if let Some(text) = returned_text(&lua, value) {
                    out.push_str(&text);
                    out.push('\n');
                }
            }
            PassEnd::Finished(out.trim_end().to_string())
        }
        Some(Err(e)) => {
            out.push_str(&format!("script error: {}", describe(&e)));
            PassEnd::Failed(out)
        }
        None => {
            out.push_str("script error: cancelled");
            PassEnd::Failed(out)
        }
    }
}

impl LuauEval {
    /// `text` within the result limit; the whole of a longer one goes to an artifact.
    fn bounded(&self, ctx: &ToolCtx, mut text: String) -> String {
        let max = self.limits.result_bytes;
        if text.len() <= max {
            return text;
        }
        let total = text.len();
        let stored = ctx
            .services
            .get::<ArtifactStore>()
            .and_then(|store| {
                store
                    .save(&format!("{}-code.txt", ctx.call_id), text.as_bytes())
                    .ok()
            })
            .and_then(|path| path.file_stem().map(|stem| stem.to_string_lossy().into_owned()));
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(&format!("\n[result cut: {total} bytes in total]"));
        if let Some(id) = stored {
            text.push_str(&format!(" full result: artifact://{id}"));
        }
        text
    }
}

#[async_trait]
impl LuauEngine for LuauEval {
    fn describe(&self, services: &Services, hidden: &[Arc<dyn Tool>]) -> String {
        let anyone = AgentInfo {
            id: "lead".to_string(),
            def: "lead".to_string(),
            parent: None,
            depth: 0,
            session_id: String::new(),
        };
        let tools = services
            .get::<ToolCaller>()
            .map(|caller| caller.tools.tools(&anyone))
            .unwrap_or_default();
        let listing = services
            .get::<CodeFirstListing>()
            .map_or_else(CodeFirstListing::default, |listing| *listing);
        describe_tools(&tools, hidden, listing)
    }

    fn start(&self, code: String, ctx: &ToolCtx, background: Background) -> ToolOutput {
        let max = background.max;
        let Some(caller) = ctx.services.get::<ToolCaller>() else {
            return ToolOutput::error("scripts cannot call tools in this session");
        };
        let Some(jobs) = ctx.services.get::<Jobs>() else {
            return ToolOutput::error("there is no job list in this session to run a script in");
        };
        let label = format!("Luau script, {} lines", code.lines().count());
        let job = jobs.start_task("lua", &ctx.agent.id, &label);
        let id = job.id().to_string();
        // The script has a token of its own: Esc or an abort of the turn leaves it running, and
        // `job kill`, the end of its agent or of the session stop it.
        let token = job.token();
        let progress = job.clone();
        let job_ctx = ToolCtx {
            cancel: token.clone(),
            update: UpdateSink::new(move |line| progress.append(&line)),
            ..ctx.clone()
        };
        let width = ctx
            .services
            .get::<Team>()
            .map_or(PARALLEL, |team| team.max_parallel());
        // A script pauses only when somebody can answer; otherwise what needs approval is refused.
        let ui = ctx.services.get::<Ui>().filter(|ui| ui.0.present());
        let pause = background.pause && ui.is_some();
        let deferral = Deferral::new(pause);
        let journal = pause.then(|| Arc::new(Journal::default()));
        let (engine, owner, limits) = (LuauEval { limits: self.limits }, ctx.clone(), self.limits);
        let job_id = id.clone();
        tokio::spawn(async move {
            let runtime = tokio::runtime::Handle::current();
            let mut timed_out = false;
            let mut paused = 0usize;
            // Time spent running passes; time spent waiting for an answer does not count.
            let mut spent = Duration::ZERO;
            let (mut text, mut end) = loop {
                let run = Arc::new(Run::new(
                    job_ctx.clone(),
                    caller.clone(),
                    limits,
                    width,
                    journal.clone(),
                    Some(deferral.clone()),
                ));
                let (state, scope, rt, source) = (run, deferral.clone(), runtime.clone(), code.clone());
                let started = Instant::now();
                let mut work = tokio::task::spawn_blocking(move || {
                    rt.block_on(deferring(scope, execute(state, source)))
                });
                let limit = async {
                    if max.is_zero() {
                        std::future::pending::<()>().await;
                    } else {
                        tokio::time::sleep(max.saturating_sub(spent)).await;
                    }
                };
                let finished = tokio::select! {
                    finished = &mut work => finished,
                    _ = limit => {
                        timed_out = true;
                        token.cancel();
                        (&mut work).await
                    }
                };
                spent += started.elapsed();
                let (site, mut request, printed) = match finished {
                    Ok(PassEnd::Finished(text)) => break (text, TaskEnd::Finished),
                    Ok(PassEnd::Failed(text)) | Ok(PassEnd::Diverged(text)) => break (text, TaskEnd::Failed),
                    Err(_) => break ("the script could not be run".to_string(), TaskEnd::Failed),
                    Ok(PassEnd::Paused {
                        site,
                        request,
                        printed,
                    }) => (site, request, printed),
                };
                if timed_out || token.is_cancelled() {
                    break (printed, TaskEnd::Killed);
                }
                let Some(ui) = ui.clone() else {
                    break (
                        format!("{} needed approval and nobody answered", request.tool),
                        TaskEnd::Failed,
                    );
                };
                paused += 1;
                let tool = request.tool.clone();
                let call_id = format!("{}.{site}", job_ctx.call_id);
                request.job = Some(job_id.clone());
                let earlier = journal.as_ref().map_or(0, |journal| journal.replayed());
                let replay = format!(
                    "approving runs the script again from the start; its {earlier} earlier calls are replayed, not repeated"
                );
                request.note = Some(match request.note.take() {
                    Some(note) => format!("{note}; {replay}"),
                    None => replay,
                });
                job.set_waiting(Some(format!("approval of {tool}")));
                let waited = async {
                    if background.pause_max.is_zero() {
                        std::future::pending::<()>().await;
                    } else {
                        tokio::time::sleep(background.pause_max).await;
                    }
                };
                let answer = tokio::select! {
                    answer = ui.0.approval(request) => Some(answer),
                    _ = token.cancelled() => None,
                    _ = waited => {
                        job.set_waiting(None);
                        break (
                            format!(
                                "waited {} s for approval of {tool} (tools.eval.background_pause_secs)",
                                background.pause_max.as_secs()
                            ),
                            TaskEnd::Failed,
                        );
                    }
                };
                job.set_waiting(None);
                match answer {
                    None => break (String::new(), TaskEnd::Killed),
                    Some(Some(
                        decision @ (ApprovalDecision::AllowOnce
                        | ApprovalDecision::AllowSession
                        | ApprovalDecision::Always),
                    )) => deferral.decide(&call_id, &tool, decision),
                    Some(Some(ApprovalDecision::Deny { feedback })) => {
                        let mut text = format!("the user denied {tool} (call {site})");
                        if let Some(feedback) = feedback.filter(|feedback| !feedback.trim().is_empty()) {
                            text.push_str(&format!(": {feedback}"));
                        }
                        break (text, TaskEnd::Failed);
                    }
                    Some(None) => {
                        break (
                            format!("{tool} needed approval and nobody answered"),
                            TaskEnd::Failed,
                        );
                    }
                }
            };
            if timed_out {
                end = TaskEnd::TimedOut;
                text.push_str(&format!(
                    "
stopped after {} (tools.eval.background_max_secs)",
                    shown_duration(max)
                ));
            } else if token.is_cancelled() {
                end = TaskEnd::Killed;
            }
            if text.trim().is_empty() {
                text = "(the script printed and returned nothing)".to_string();
            }
            job.append(&text);
            let state = match end {
                TaskEnd::Finished => "finished",
                TaskEnd::Failed => "failed",
                TaskEnd::TimedOut => "timed out",
                TaskEnd::Killed => "killed",
            };
            let text = engine.bounded(&owner, text);
            let label = match paused {
                0 => label,
                1 => format!("{label} (paused once)"),
                n => format!("{label} (paused {n} times)"),
            };
            wake_agent(
                &owner.services,
                &owner.agent,
                wake_notice(job.id(), &label, state, &text),
            );
            // After the wake: a run that waits for its background scripts (print mode) sees one
            // as running until its agent has been told.
            job.finish(end);
        });
        ToolOutput::text(format!(
            "started {id}; you will be woken with its result. `job` output and kill work on it."
        ))
        .with_details(json!({ "job": id }))
    }

    async fn run(&self, code: String, ctx: &ToolCtx) -> ToolOutput {
        let Some(caller) = ctx.services.get::<ToolCaller>() else {
            return ToolOutput::error("scripts cannot call tools in this session");
        };
        let width = ctx
            .services
            .get::<Team>()
            .map_or(PARALLEL, |team| team.max_parallel());
        let run = Arc::new(Run::new(ctx.clone(), caller, self.limits, width, None, None));
        // On a thread of its own: a script that computes for a while must not hold up the
        // threads every other agent and the interface run on.
        let runtime = tokio::runtime::Handle::current();
        let state = run.clone();
        let finished = tokio::task::spawn_blocking(move || runtime.block_on(execute(state, code))).await;
        let calls = run.calls.load(Ordering::SeqCst).min(self.limits.tool_calls);
        let details = json!({ "tool_calls": calls });
        match finished {
            Ok(PassEnd::Finished(text)) if text.is_empty() => {
                ToolOutput::text("(the script printed and returned nothing)").with_details(details)
            }
            Ok(PassEnd::Finished(text)) => ToolOutput::text(self.bounded(ctx, text)).with_details(details),
            Ok(PassEnd::Failed(text)) | Ok(PassEnd::Diverged(text)) => {
                ToolOutput::error(self.bounded(ctx, text)).with_details(details)
            }
            Ok(PassEnd::Paused { .. }) | Err(_) => ToolOutput::error("the script could not be run"),
        }
    }
}

/// The message that wakes an agent when its background script has ended.
fn wake_notice(id: &str, label: &str, state: &str, text: &str) -> String {
    format!(
        "<job-exit id=\"{id}\">
The background script you started has ended: {label}
{text}
[{id}: {state}]
</job-exit>"
    )
}

/// Tells the agent that started a background script how it ended: a message to a sub-agent, or
/// the wake of the lead, as a `wake` hook does it.
fn wake_agent(services: &Services, agent: &AgentInfo, text: String) {
    let posted = services
        .get::<Post>()
        .is_some_and(|post| (post.0)(&agent.id, text.clone()));
    if !posted
        && agent.parent.is_none()
        && let Some(wake) = services.get::<Wake>()
    {
        (wake.0)(text);
    }
}

/// How long a script that has been stopped for time is said to have run.
fn shown_duration(max: Duration) -> String {
    match max.as_secs() {
        0 => format!("{} ms", max.as_millis()),
        secs => format!("{secs} s"),
    }
}

#[cfg(test)]
mod tests {
    use azazel_agent::Access;

    use super::*;

    struct Shown(String, String, Value);

    fn shown(name: &str, description: &str, schema: &Value) -> Arc<dyn Tool> {
        Arc::new(Shown(name.to_string(), description.to_string(), schema.clone()))
    }

    #[async_trait]
    impl Tool for Shown {
        fn name(&self) -> &str {
            &self.0
        }
        fn description(&self) -> &str {
            &self.1
        }
        fn schema(&self) -> Value {
            self.2.clone()
        }
        fn access(&self, _: &Value) -> Access {
            Access::Read
        }
        fn subjects(&self, _: &Value) -> Vec<String> {
            Vec::new()
        }
        async fn call(&self, _: Value, _: &ToolCtx) -> ToolOutput {
            ToolOutput::text("")
        }
    }

    #[test]
    fn a_signature_names_the_arguments_their_types_and_what_the_tool_does() {
        let read = shown(
            "read",
            "Read a file, or list a directory. Text comes back numbered.",
            &json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "limit": {"type": "integer"},
                    "format": {"type": "string", "enum": ["text", "raw"]},
                    "extra": {}
                },
                "required": ["path"]
            }),
        );
        assert_eq!(
            signature(read.as_ref()),
            "- read{ path: string, limit?: integer, format?: \"text\"|\"raw\", extra?: any }: \
             Read a file, or list a directory"
        );
    }

    #[test]
    fn the_listing_leaves_out_what_a_script_cannot_call_and_names_what_does_not_fit() {
        let schema = json!({"type": "object", "properties": {}});
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            shown("read", "Read a file.", &schema),
            shown("task", "Start agents.", &schema),
            shown(EVAL, "Run code.", &schema),
        ];
        let listed = describe_tools(&tools, &[], CodeFirstListing::Signatures);
        assert!(listed.contains("\nCallable tools: read\n"), "{listed}");
        assert!(!listed.contains("task,") && !listed.contains("eval,"), "{listed}");

        // Many tools are named up to the budget, and the description stays short.
        let long = "x".repeat(130);
        for i in 0..120 {
            tools.push(shown(&format!("mcp__server__tool_{i}"), &long, &schema));
        }
        let listed = describe_tools(&tools, &[], CodeFirstListing::Signatures);
        assert!(
            listed.len() < INTRO.len() + LISTING_CHARS + 400,
            "{}",
            listed.len()
        );
        assert!(listed.contains("read, mcp__server__tool_0, "), "{listed}");
        assert!(!listed.contains("xxx"), "{listed}");
    }

    #[test]
    fn hidden_tools_are_listed_as_signatures_within_the_budget_and_the_rest_by_name() {
        let schema =
            json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]});
        let tools: Vec<Arc<dyn Tool>> = vec![shown("read", "Read a file.", &schema)];
        let few: Vec<Arc<dyn Tool>> = vec![shown("glob", "Find files by pattern.", &schema)];
        let listed = describe_tools(
            &[tools[0].clone(), few[0].clone()],
            &few,
            CodeFirstListing::Signatures,
        );
        assert!(
            listed.contains(
                "
Callable tools: read
"
            ),
            "{listed}"
        );
        assert!(
            listed.contains(
                "- glob{ path: string }: Find files by pattern
"
            ),
            "{listed}"
        );

        let long = "y".repeat(130);
        let many: Vec<Arc<dyn Tool>> = (0..200)
            .map(|i| shown(&format!("mcp__s__tool_{i:03}"), &long, &schema))
            .collect();
        let listed = describe_tools(&many, &many, CodeFirstListing::Signatures);
        let block = listed
            .split(
                "(they are not in your tool list):
",
            )
            .nth(1)
            .unwrap();
        let (signatures, rest) = block.split_once("More, by name: ").unwrap();
        assert!(
            signatures.len() <= CODE_FIRST_LISTING_CHARS,
            "{}",
            signatures.len()
        );
        assert!(signatures.contains("- mcp__s__tool_000{"), "{signatures}");
        assert!(rest.contains("mcp__s__tool_199") || rest.contains('…'), "{rest}");
        assert!(!rest.contains("mcp__s__tool_000"), "{rest}");
    }

    #[test]
    fn hidden_tools_are_listed_by_name_with_mcp_servers_counted() {
        let schema = json!({"type": "object", "properties": {"path": {"type": "string"}}});
        let mut tools: Vec<Arc<dyn Tool>> = vec![shown("read", "Read a file.", &schema)];
        let mut hidden: Vec<Arc<dyn Tool>> = vec![shown("glob", "Find files.", &schema)];
        for i in 0..300 {
            hidden.push(shown(&format!("mcp__github__tool_{i}"), "Does a thing.", &schema));
        }
        hidden.push(shown("mcp__one__only", "Does a thing.", &schema));
        tools.extend(hidden.iter().cloned());
        let listed = describe_tools(&tools, &hidden, CodeFirstListing::Names);
        assert!(
            listed.contains(
                "Callable tools: read
"
            ),
            "{listed}"
        );
        assert!(
            listed.contains(
                "): glob, mcp__github (300 tools), mcp__one (1 tool)
"
            ),
            "{listed}"
        );
        assert!(listed.contains("tools.describe(name)"), "{listed}");
        assert!(!listed.contains("- glob{"), "{listed}");
        assert!(!listed.contains("mcp__github__tool_0"), "{listed}");
        // The listing does not grow with the number of tools of a server.
        assert!(listed.len() < INTRO.len() + 1_000, "{}", listed.len());
        // Both listings are deterministic: the description is part of the cached prompt.
        for listing in [CodeFirstListing::Names, CodeFirstListing::Signatures] {
            assert_eq!(
                describe_tools(&tools, &hidden, listing),
                describe_tools(&tools, &hidden, listing)
            );
        }
    }
}
