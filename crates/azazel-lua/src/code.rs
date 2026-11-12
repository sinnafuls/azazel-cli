//! `run_code`: a Luau script that calls the harness's tools, so an agent can do a job of several
//! steps in one call and get back only what it asks for.

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_agent::{Access, AgentInfo, Services, Tool, ToolCtx, ToolOutput, ToolSource};
use azazel_core::transcript::activity;
use azazel_core::{ArtifactStore, ExtError, Extension, Registrar, ToolCaller};
use mlua::chunk::ChunkMode;
use mlua::{Lua, MultiValue, Table, VmState};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::convert::{from_lua, tool_input};
use crate::script::{describe, locked};

pub const NAME: &str = "run_code";
/// The description's list of callable tools stays within about 3,000 tokens.
const LISTING_CHARS: usize = 12_000;
/// What `print` keeps; a script that prints more is told so.
const MAX_PRINTED_BYTES: usize = 1024 * 1024;
/// How long a cancelled script gets to wind down the tool call it is waiting for.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

const INTRO: &str = "Run a Luau script that calls your other tools, to do a job of several steps in one call: \
     read a dozen files and return only the lines that matter, search and then filter, make the same \
     edit in many places. `tools.<name>{ ... }` calls a tool with the arguments you would give it \
     yourself and returns its result as a string; a call that fails or is refused raises an error, \
     which `pcall` catches. Every call is checked against the permission rules like one you make \
     directly. What the script prints and the value it returns are the result; nothing else comes \
     back. `json.decode` and `json.encode` are there. Limits: 120 s of script time (waiting for a \
     tool does not count), 50 tool calls, 64 KB of result. `run_code`, `task` and the other agent \
     tools cannot be called from a script.";

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

/// Registers the `run_code` tool.
#[derive(Default)]
pub struct CodeExt {
    pub limits: CodeLimits,
}

impl Extension for CodeExt {
    fn id(&self) -> &str {
        "code"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.tool(Arc::new(RunCodeTool {
            limits: self.limits,
            services: r.services().clone(),
            description: OnceLock::new(),
        }));
        Ok(())
    }
}

/// Tools a script cannot call: itself, and those that start and steer other agents.
fn blocked(name: &str) -> bool {
    name == NAME || azazel_team::ORCHESTRATION_TOOLS.contains(&name)
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
            let mark = if required.contains(&name.as_str()) { "" } else { "?" };
            format!("{name}{mark}: {}", type_name(property))
        })
        .collect();
    let description = tool.description();
    let sentence = description
        .split_once(". ")
        .map_or(description, |(first, _)| first)
        .lines()
        .next()
        .unwrap_or_default();
    let sentence: String = sentence.trim_end_matches('.').chars().take(140).collect();
    format!("- {}{{ {} }}: {sentence}", tool.name(), arguments.join(", "))
}

/// The description with the tools a script can call: one line each while they fit the budget,
/// the rest by name.
fn describe_tools(tools: &[Arc<dyn Tool>]) -> String {
    let mut out = format!("{INTRO}\n\nCallable tools:\n");
    let mut unlisted: Vec<&str> = Vec::new();
    for tool in tools.iter().filter(|tool| !blocked(tool.name())) {
        let line = signature(tool.as_ref());
        if unlisted.is_empty() && out.len() + line.len() < LISTING_CHARS {
            out.push_str(&line);
            out.push('\n');
        } else {
            unlisted.push(tool.name());
        }
    }
    if !unlisted.is_empty() {
        let mut names = unlisted.join(", ");
        if names.len() > 2000 {
            let mut end = 2000;
            while !names.is_char_boundary(end) {
                end -= 1;
            }
            names.truncate(end);
            names.push('…');
        }
        out.push_str(&format!("Also callable, with the arguments their own descriptions give: {names}\n"));
    }
    out.push_str(
        "Any other tool you have is callable by name in the same way; a name that is not an identifier \
         is written `tools[\"the-name\"]{ ... }`.",
    );
    out
}

pub struct RunCodeTool {
    limits: CodeLimits,
    services: Arc<Services>,
    /// Built on first use, when every extension has registered its tools, and then kept: a
    /// description that changed between requests would break the provider's prompt cache.
    description: OnceLock<String>,
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
}

impl Run {
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
        let number = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if number > self.limits.tool_calls {
            return Err(format!(
                "the limit of {} tool calls per script is reached",
                self.limits.tool_calls
            ));
        }
        self.ctx.update.send(activity(name, &input));
        let ctx = ToolCtx {
            call_id: format!("{}.{number}", self.ctx.call_id),
            ..self.ctx.clone()
        };
        let output = self.caller.call(&ctx, name, input).await;
        let text = output.text_content();
        if output.is_error {
            Err(format!("{name}: {text}"))
        } else {
            Ok(text)
        }
    }
}

/// `tools`: indexing it with a tool's name gives a function that calls the tool. A failure is
/// raised from Luau, so that the script gets a plain string naming the line of the call.
const TOOLS: &str = r#"
local call = ...
return setmetatable({}, {
    __index = function(_, name)
        return function(args)
            local ok, text = call(name, args)
            if not ok then
                error(text, 2)
            end
            return text
        end
    end,
})
"#;

/// The globals of a script: `tools`, `print` and `json`.
fn install(lua: &Lua, run: &Arc<Run>) -> mlua::Result<()> {
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
    let tools: Table = lua.load(TOOLS).set_name("=tools").call(call)?;
    tools.set_readonly(true);
    lua.globals().set("tools", tools)?;

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

/// Runs `code` in a fresh sandboxed VM and returns what it printed and returned, or its error
/// after what it printed.
async fn execute(run: Arc<Run>, code: String) -> Result<String, String> {
    let lua = Lua::new();
    install(&lua, &run).map_err(|e| describe(&e))?;
    lua.sandbox(true).map_err(|e| describe(&e))?;
    let watched = run.clone();
    lua.set_interrupt(move |_| watched.interrupt());
    let function = lua
        .load(code)
        .set_name("=run_code")
        .set_mode(ChunkMode::Text)
        .into_function()
        .map_err(|e| describe(&e))?;

    let mut script = Script {
        call: function.call_async(()),
        run: &run,
        left: run.limits.script_time,
    };
    let cancel: CancellationToken = run.ctx.cancel.clone();
    let finished = tokio::select! {
        returned = &mut script => Some(returned),
        _ = cancel.cancelled() => None,
    };
    let returned = match finished {
        Some(returned) => returned,
        None => match tokio::time::timeout(CANCEL_GRACE, &mut script).await {
            Ok(returned) => returned,
            Err(_) => Err(mlua::Error::runtime("cancelled")),
        },
    };
    drop(script);

    let mut out = std::mem::take(&mut *locked(&run.printed));
    let dropped = run.dropped.load(Ordering::Relaxed);
    if dropped > 0 {
        out.push_str(&format!("[{dropped} more bytes of printed output were dropped]\n"));
    }
    match returned {
        Ok(values) => {
            for value in values {
                if let Some(text) = returned_text(&lua, value) {
                    out.push_str(&text);
                    out.push('\n');
                }
            }
            Ok(out.trim_end().to_string())
        }
        Err(e) => {
            out.push_str(&format!("script error: {}", describe(&e)));
            Err(out)
        }
    }
}

impl RunCodeTool {
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
            .and_then(|store| store.save(&format!("{}-code.txt", ctx.call_id), text.as_bytes()).ok())
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
impl Tool for RunCodeTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        if let Some(built) = self.description.get() {
            return built;
        }
        // Before the tool caller exists the extensions are still loading; nothing is kept yet.
        let Some(caller) = self.services.get::<ToolCaller>() else {
            return INTRO;
        };
        let anyone = AgentInfo {
            id: "lead".to_string(),
            def: "lead".to_string(),
            parent: None,
            depth: 0,
            session_id: String::new(),
        };
        self.description
            .get_or_init(|| describe_tools(&caller.tools.tools(&anyone)))
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {"type": "string", "description": "The Luau script"}
            },
            "required": ["code"]
        })
    }

    /// The script itself touches nothing; each tool call it makes is judged on its own.
    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    fn concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(code) = input.get("code").and_then(Value::as_str) else {
            return ToolOutput::error("invalid arguments: `code` must be a string");
        };
        let Some(caller) = ctx.services.get::<ToolCaller>() else {
            return ToolOutput::error("scripts cannot call tools in this session");
        };
        let run = Arc::new(Run {
            ctx: ctx.clone(),
            caller,
            limits: self.limits,
            calls: AtomicUsize::new(0),
            printed: Mutex::default(),
            dropped: AtomicUsize::new(0),
            limit: Mutex::default(),
        });
        // On a thread of its own: a script that computes for a while must not hold up the
        // threads every other agent and the interface run on.
        let runtime = tokio::runtime::Handle::current();
        let (state, code) = (run.clone(), code.to_string());
        let finished = tokio::task::spawn_blocking(move || runtime.block_on(execute(state, code))).await;
        let calls = run.calls.load(Ordering::SeqCst).min(self.limits.tool_calls);
        let details = json!({ "tool_calls": calls });
        match finished {
            Ok(Ok(text)) if text.is_empty() => {
                ToolOutput::text("(the script printed and returned nothing)").with_details(details)
            }
            Ok(Ok(text)) => ToolOutput::text(self.bounded(ctx, text)).with_details(details),
            Ok(Err(text)) => ToolOutput::error(self.bounded(ctx, text)).with_details(details),
            Err(_) => ToolOutput::error("the script could not be run"),
        }
    }
}

#[cfg(test)]
mod tests {
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
            shown(NAME, "Run a script.", &schema),
        ];
        let listed = describe_tools(&tools);
        assert!(listed.contains("\n- read{  }: Read a file\n"), "{listed}");
        assert!(!listed.contains("- task") && !listed.contains("- run_code"), "{listed}");
        assert!(!listed.contains("Also callable"), "{listed}");

        // Past the budget the tools are still named, so the model knows they are there.
        let long = "x".repeat(130);
        for i in 0..120 {
            tools.push(shown(&format!("mcp__server__tool_{i}"), &long, &schema));
        }
        let listed = describe_tools(&tools);
        assert!(listed.len() < LISTING_CHARS + 2600, "{}", listed.len());
        assert!(listed.contains("\n- mcp__server__tool_0{  }: xxx"), "{listed}");
        assert!(
            listed.contains("Also callable, with the arguments their own descriptions give: mcp__server__tool_"),
            "{listed}"
        );
        assert!(listed.contains("mcp__server__tool_119"), "{listed}");
    }
}
