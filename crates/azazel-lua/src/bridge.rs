//! What a script registered, in the shapes the harness calls: tools, event handlers, slash
//! commands and key bindings.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{Access, AgentInfo, Tool, ToolCtx, ToolOutput};
use azazel_ai::{Block, Route, RouteRequest, VirtualModel};
use azazel_core::{
    CommandHandler, CommandOutcome, Event, EventHandler, ExtCtx, Level, Outcome, ShortcutHandler,
    SystemPartProvider, ToolWrap, Ui,
};
use mlua::Function;
use serde_json::{Map, Value, json};

use crate::convert::{agent_of, apply, event_table, to_lua, tool_input};
use crate::models::{route_table, routed};
use crate::script::{BUDGET, Run, Script, ToolDef, WrapDef, describe};

/// How long a cancelled tool call gets to wind down what it is waiting for.
const CANCEL_GRACE: Duration = Duration::from_secs(1);

/// A tool defined with `azazel.tool`.
pub(crate) struct LuaTool {
    pub script: Arc<Script>,
    pub def: ToolDef,
}

impl LuaTool {
    fn start(&self, input: Value, ctx: &ToolCtx) -> mlua::Result<Run<'_, mlua::Value>> {
        let input = match input {
            Value::Null => Value::Object(Map::new()),
            input => input,
        };
        let args = to_lua(self.script.lua(), &input)?;
        let context = self.script.ctx(&ctx.cwd, &ctx.session_id, Some(&ctx.agent))?;
        Ok(self
            .script
            .call(&self.def.execute, (args, context), None, Some(ctx.cancel.clone())))
    }

    fn output(&self, returned: mlua::Value) -> ToolOutput {
        match returned {
            mlua::Value::Nil => ToolOutput::text(""),
            mlua::Value::String(text) => ToolOutput::text(text.to_string_lossy()),
            mlua::Value::Boolean(value) => ToolOutput::text(value.to_string()),
            mlua::Value::Integer(value) => ToolOutput::text(value.to_string()),
            mlua::Value::Number(value) => ToolOutput::text(value.to_string()),
            mlua::Value::Table(table) => {
                let content = table.get::<Option<String>>("content");
                let is_error = table.get::<Option<bool>>("is_error");
                match (content, is_error) {
                    (Ok(Some(content)), Ok(is_error)) => ToolOutput {
                        content: vec![Block::text(content)],
                        is_error: is_error.unwrap_or(false),
                        details: None,
                    },
                    _ => ToolOutput::error(format!(
                        "the tool `{}` returned a table without a `content` string",
                        self.def.name
                    )),
                }
            }
            other => ToolOutput::error(format!(
                "the tool `{}` returned a {}; it must return a string or {{ content, is_error }}",
                self.def.name,
                other.type_name()
            )),
        }
    }
}

#[async_trait]
impl Tool for LuaTool {
    fn name(&self) -> &str {
        &self.def.name
    }

    fn description(&self) -> &str {
        &self.def.description
    }

    fn schema(&self) -> Value {
        self.def.schema.clone()
    }

    fn access(&self, _: &Value) -> Access {
        self.def.access
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let shared = &self.script.shared;
        if let Some(error) = shared.error() {
            return ToolOutput::error(format!("the extension `{}` is disabled: {error}", shared.id));
        }
        let mut run = match self.start(input, ctx) {
            Ok(run) => run,
            Err(e) => return ToolOutput::error(describe(&e)),
        };
        let finished = tokio::select! {
            returned = &mut run => Some(returned),
            _ = ctx.cancel.cancelled() => None,
        };
        let returned = match finished {
            Some(returned) => returned,
            None => match tokio::time::timeout(CANCEL_GRACE, &mut run).await {
                Ok(returned) => returned,
                Err(_) => return ToolOutput::error("cancelled"),
            },
        };
        match returned {
            Ok(returned) => self.output(returned),
            Err(e) => ToolOutput::error(describe(&e)),
        }
    }
}

/// Functions registered with `azazel.wrap`, run around the calls of one tool. The first error
/// disables their script, after which the tool runs as if it were not wrapped.
pub(crate) struct LuaWrap {
    pub script: Arc<Script>,
    pub def: WrapDef,
}

impl LuaWrap {
    /// Runs `before(input, ctx)` and applies what it returned: `{ input }` replaces the input,
    /// `{ block = true, reason }` refuses the call, `{ content, is_error }` answers it.
    async fn enter(
        &self,
        function: &Function,
        input: &mut Value,
        ctx: &ToolCtx,
    ) -> mlua::Result<Option<ToolOutput>> {
        let lua = self.script.lua();
        let context = self.script.ctx(&ctx.cwd, &ctx.session_id, Some(&ctx.agent))?;
        let returned: mlua::Value = self
            .script
            .call(
                function,
                (to_lua(lua, input)?, context),
                Some(BUDGET),
                Some(ctx.cancel.clone()),
            )
            .await?;
        let returned = match returned {
            mlua::Value::Nil => return Ok(None),
            mlua::Value::Table(returned) => returned,
            other => {
                return Err(mlua::Error::runtime(format!(
                    "`before` returned a {}; it returns nil or a table",
                    other.type_name()
                )));
            }
        };
        if returned.get::<Option<bool>>("block")? == Some(true) {
            let reason = returned.get::<Option<String>>("reason")?.unwrap_or_else(|| {
                format!(
                    "`{}` was blocked by the extension `{}`",
                    self.def.tool, self.script.shared.id
                )
            });
            return Ok(Some(ToolOutput::error(reason)));
        }
        if let Some(content) = returned.get::<Option<String>>("content")? {
            return Ok(Some(ToolOutput {
                content: vec![Block::text(content)],
                is_error: returned.get::<Option<bool>>("is_error")?.unwrap_or(false),
                details: None,
            }));
        }
        let replacement: mlua::Value = returned.get("input")?;
        if !replacement.is_nil() {
            *input = tool_input(lua, replacement)?;
        }
        Ok(None)
    }

    /// Runs `after(result, input, ctx)` and applies what it returned: a string is the new
    /// content, a table changes `content` and `is_error`.
    async fn leave(
        &self,
        function: &Function,
        input: &Value,
        output: &mut ToolOutput,
        ctx: &ToolCtx,
    ) -> mlua::Result<()> {
        let lua = self.script.lua();
        let context = self.script.ctx(&ctx.cwd, &ctx.session_id, Some(&ctx.agent))?;
        let result = json!({ "content": output.text_content(), "is_error": output.is_error });
        let returned: mlua::Value = self
            .script
            .call(
                function,
                (to_lua(lua, &result)?, to_lua(lua, input)?, context),
                Some(BUDGET),
                Some(ctx.cancel.clone()),
            )
            .await?;
        match returned {
            mlua::Value::Nil => {}
            mlua::Value::String(content) => output.content = vec![Block::text(content.to_string_lossy())],
            mlua::Value::Table(returned) => {
                if let Some(content) = returned.get::<Option<String>>("content")? {
                    output.content = vec![Block::text(content)];
                }
                if let Some(is_error) = returned.get::<Option<bool>>("is_error")? {
                    output.is_error = is_error;
                }
            }
            other => {
                return Err(mlua::Error::runtime(format!(
                    "`after` returned a {}; it returns nil, a string or a table",
                    other.type_name()
                )));
            }
        }
        Ok(())
    }

    fn failed(&self, which: &str, error: &mlua::Error) {
        self.script.shared.disable(format!(
            "its `{which}` around `{}` failed: {}",
            self.def.tool,
            describe(error)
        ));
    }
}

#[async_trait]
impl ToolWrap for LuaWrap {
    async fn before(&self, input: &mut Value, ctx: &ToolCtx) -> Result<(), ToolOutput> {
        let Some(function) = &self.def.before else {
            return Ok(());
        };
        if self.script.shared.error().is_some() {
            return Ok(());
        }
        match self.enter(function, input, ctx).await {
            Ok(None) => Ok(()),
            Ok(Some(answer)) => Err(answer),
            // The user stopped the call: the tool sees the same token and ends at once.
            Err(_) if ctx.cancel.is_cancelled() => Ok(()),
            Err(e) => {
                self.failed("before", &e);
                Ok(())
            }
        }
    }

    async fn after(&self, input: &Value, output: &mut ToolOutput, ctx: &ToolCtx) {
        let Some(function) = &self.def.after else {
            return;
        };
        if self.script.shared.error().is_some() {
            return;
        }
        if let Err(e) = self.leave(function, input, output, ctx).await
            && !ctx.cancel.is_cancelled()
        {
            self.failed("after", &e);
        }
    }
}

/// A handler registered with `azazel.on`. The first error disables its script.
pub(crate) struct LuaHandler {
    pub script: Arc<Script>,
    /// The event's name as scripts write it.
    pub event: &'static str,
    pub function: Function,
}

impl LuaHandler {
    async fn run(&self, event: &mut Event, ctx: &ExtCtx) -> mlua::Result<Outcome> {
        let lua = self.script.lua();
        let table = event_table(lua, self.event, event)?;
        let context = self.script.ctx(&ctx.cwd, &ctx.session_id, agent_of(event))?;
        let returned: mlua::Value = self
            .script
            .call(&self.function, (table, context), Some(BUDGET), None)
            .await?;
        match returned {
            mlua::Value::Nil => Ok(Outcome::Continue),
            mlua::Value::Table(returned) => apply(lua, event, &returned, &self.script.shared.id),
            other => {
                tracing::warn!(
                    script = %self.script.shared.id,
                    "the `{}` handler returned a {}; only nil or a table has an effect",
                    self.event,
                    other.type_name()
                );
                Ok(Outcome::Continue)
            }
        }
    }
}

#[async_trait]
impl EventHandler for LuaHandler {
    async fn handle(&self, ev: &mut Event, ctx: &ExtCtx) -> Outcome {
        let shared = &self.script.shared;
        if shared.error().is_some() {
            return Outcome::Continue;
        }
        match self.run(ev, ctx).await {
            Ok(outcome) => outcome,
            Err(e) => {
                shared.disable(format!("its `{}` handler failed: {}", self.event, describe(&e)));
                Outcome::Continue
            }
        }
    }
}

/// A slash command registered with `azazel.command`.
pub(crate) struct LuaCommand {
    pub script: Arc<Script>,
    pub name: String,
    pub function: Function,
}

#[async_trait]
impl CommandHandler for LuaCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        let shared = &self.script.shared;
        if let Some(error) = shared.error() {
            return CommandOutcome::Error(format!("the extension `{}` is disabled: {error}", shared.id));
        }
        let call = self.script.ctx(&ctx.cwd, &ctx.session_id, None).map(|context| {
            self.script
                .call::<mlua::Value>(&self.function, (args, context), Some(BUDGET), None)
        });
        let returned = match call {
            Ok(call) => call.await,
            Err(e) => Err(e),
        };
        match returned {
            Ok(mlua::Value::String(text)) => CommandOutcome::Show(text.to_string_lossy()),
            Ok(_) => CommandOutcome::Done,
            Err(e) => CommandOutcome::Error(format!("/{}: {}", self.name, describe(&e))),
        }
    }
}

/// A part of the system prompt registered with `azazel.prompt`. The first error disables its
/// script.
pub(crate) struct LuaPrompt {
    pub script: Arc<Script>,
    pub name: String,
    pub function: Function,
    /// The directory the session works in.
    pub cwd: PathBuf,
}

#[async_trait]
impl SystemPartProvider for LuaPrompt {
    async fn render(&self, agent: &AgentInfo) -> Option<String> {
        let shared = &self.script.shared;
        if shared.error().is_some() {
            return None;
        }
        let call = self
            .script
            .ctx(&self.cwd, &agent.session_id, Some(agent))
            .map(|context| {
                self.script
                    .call::<mlua::Value>(&self.function, context, Some(BUDGET), None)
            });
        let returned = match call {
            Ok(call) => call.await,
            Err(e) => Err(e),
        };
        match returned {
            Ok(mlua::Value::String(text)) => {
                let text = text.to_string_lossy();
                (!text.trim().is_empty()).then_some(text)
            }
            Ok(mlua::Value::Nil) => None,
            Ok(other) => {
                tracing::warn!(
                    script = %shared.id,
                    "the prompt part `{}` returned a {}; only a string or nil has an effect",
                    self.name,
                    other.type_name()
                );
                None
            }
            Err(e) => {
                shared.disable(format!(
                    "its prompt part `{}` failed: {}",
                    self.name,
                    describe(&e)
                ));
                None
            }
        }
    }
}

/// A virtual model registered with `azazel.router`: `function` picks the model of each request
/// made with `virtual/<name>`. A failure fails that request and is shown with it.
pub(crate) struct LuaRouter {
    pub script: Arc<Script>,
    pub function: Function,
}

#[async_trait]
impl VirtualModel for LuaRouter {
    async fn route(&self, request: &RouteRequest<'_>) -> Result<Route, String> {
        let shared = &self.script.shared;
        if let Some(error) = shared.error() {
            return Err(format!("the extension `{}` is disabled: {error}", shared.id));
        }
        let asked = route_table(self.script.lua(), request).map_err(|e| describe(&e))?;
        let returned: mlua::Value = self
            .script
            .call(&self.function, asked, Some(BUDGET), None)
            .await
            .map_err(|e| describe(&e))?;
        routed(returned)
    }
}

/// A key binding registered with `azazel.shortcut`.
pub(crate) struct LuaShortcut {
    pub script: Arc<Script>,
    pub key: String,
    pub function: Function,
}

#[async_trait]
impl ShortcutHandler for LuaShortcut {
    async fn run(&self, ctx: &ExtCtx) {
        let shared = &self.script.shared;
        if shared.error().is_some() {
            return;
        }
        let call = self.script.ctx(&ctx.cwd, &ctx.session_id, None).map(|context| {
            self.script
                .call::<()>(&self.function, context, Some(BUDGET), None)
        });
        let ran = match call {
            Ok(call) => call.await,
            Err(e) => Err(e),
        };
        if let Err(e) = ran {
            let text = format!("{} ({}): {}", self.key, shared.id, describe(&e));
            match ctx.services.get::<Ui>() {
                Some(ui) => ui.0.notify(Level::Error, &text),
                None => tracing::error!("{text}"),
            }
        }
    }
}
