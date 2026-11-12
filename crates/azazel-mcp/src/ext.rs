//! What MCP adds to a session: proxied tools, `tool_search`, `list_mcp_resources`, the `mcp://`
//! scheme, prompt commands, `/mcp`, and the `mcp` part of the system prompt.

use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, AgentInfo, Tool, ToolCtx, ToolOutput, ToolSource};
use azazel_ai::Block;
use azazel_core::commands::split_args;
use azazel_core::{
    Command, CommandHandler, CommandOutcome, CommandSource, Event, EventHandler, EventKind, ExtCtx, ExtError,
    Extension, Level, McpRegistrar, McpServers, Outcome, Registrar, Scheme, SystemPartProvider, Ui,
};
use rmcp::model::{JsonObject, Prompt};
use serde_json::{Value, json};

use crate::manager::{McpManager, State};

/// Hits `tool_search` returns when the caller does not say how many.
const DEFAULT_SEARCH_LIMIT: usize = 8;
const MAX_SEARCH_LIMIT: usize = 25;

struct ToolSearchTool(McpManager);

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "tool_search"
    }

    fn description(&self) -> &str {
        "Find tools provided by MCP servers. There are too many to list up front: describe what you need and \
         the best matches are returned with their input schemas. Tools found this way can be called by name \
         from your next step on."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What the tool should do, in a few words" },
                "limit": { "type": "integer", "description": "Most results to return (default 8)" }
            },
            "required": ["query"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let Some(query) = input
            .get("query")
            .and_then(Value::as_str)
            .filter(|q| !q.trim().is_empty())
        else {
            return ToolOutput::error("`query` must be a non-empty string");
        };
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_SEARCH_LIMIT, |n| (n as usize).clamp(1, MAX_SEARCH_LIMIT));
        let hits = self.0.search(query, limit);
        if hits.is_empty() {
            return ToolOutput::text(format!("no MCP tool matches `{query}`; try other words"));
        }
        let mut out = String::new();
        for hit in &hits {
            out.push_str(&format!(
                "## {}\n{}\ninput schema: {}\n\n",
                hit.name, hit.description, hit.schema
            ));
        }
        out.push_str("These tools can be called by name from your next step on.");
        ToolOutput::text(out)
    }
}

struct ResourcesTool(McpManager);

#[async_trait]
impl Tool for ResourcesTool {
    fn name(&self) -> &str {
        "list_mcp_resources"
    }

    fn description(&self) -> &str {
        "List the resources MCP servers offer, one per line as `mcp://<server>/<uri>  name`. Read one by \
         passing its `mcp://` address to the read tool."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "description": "Only this server; omit for all connected servers" }
            }
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let server = input
            .get("server")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        match self.0.list_resources(server).await {
            Ok(listing) => ToolOutput::text(listing),
            Err(why) => ToolOutput::error(why),
        }
    }
}

impl ToolSource for McpManager {
    fn tools(&self, _: &AgentInfo) -> Vec<Arc<dyn Tool>> {
        let mut tools = McpManager::tools(self);
        if self.deferring() {
            tools.push(Arc::new(ToolSearchTool(self.clone())));
        }
        tools
    }
}

/// `mcp://<server>/<resource uri>` for the `read` tool.
struct McpScheme(McpManager);

#[async_trait]
impl Scheme for McpScheme {
    fn scheme(&self) -> &str {
        "mcp"
    }

    async fn read(&self, rest: &str, ctx: &ToolCtx) -> Result<Vec<Block>, String> {
        let (server, uri) = rest
            .split_once('/')
            .filter(|(server, uri)| !server.is_empty() && !uri.is_empty())
            .ok_or("expected mcp://<server>/<resource uri>")?;
        self.0.read_resource(server, uri, ctx).await
    }
}

struct McpPart(McpManager);

#[async_trait]
impl SystemPartProvider for McpPart {
    async fn render(&self, _: &AgentInfo) -> Option<String> {
        self.0.instructions()
    }
}

/// Assigns what follows a prompt command to the prompt's arguments: `key=value` by name, anything
/// else by position.
fn prompt_arguments(prompt: &Prompt, args: &str) -> Result<JsonObject, String> {
    let declared: Vec<&str> = prompt
        .arguments
        .iter()
        .flatten()
        .map(|argument| argument.name.as_str())
        .collect();
    let mut values = JsonObject::new();
    let mut positional = declared.iter();
    for word in split_args(args) {
        let named = word.split_once('=').filter(|(key, _)| declared.contains(key));
        let (key, value) = match named {
            Some((key, value)) => (key.to_string(), value.to_string()),
            None => loop {
                match positional.next() {
                    Some(name) if values.contains_key(*name) => continue,
                    Some(name) => break (name.to_string(), word),
                    None => {
                        return Err(format!(
                            "too many arguments; `{}` takes: {}",
                            prompt.name,
                            declared.join(", ")
                        ));
                    }
                }
            },
        };
        values.insert(key, Value::String(value));
    }
    let missing: Vec<&str> = prompt
        .arguments
        .iter()
        .flatten()
        .filter(|argument| argument.required == Some(true) && !values.contains_key(&argument.name))
        .map(|argument| argument.name.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(format!("missing: {}", missing.join(", ")));
    }
    Ok(values)
}

struct PromptCommand {
    manager: McpManager,
    server: String,
    prompt: Prompt,
}

#[async_trait]
impl CommandHandler for PromptCommand {
    async fn run(&self, args: &str, _: &ExtCtx) -> CommandOutcome {
        let arguments = match prompt_arguments(&self.prompt, args) {
            Ok(arguments) => arguments,
            Err(why) => return CommandOutcome::Error(why),
        };
        match self
            .manager
            .prompt_text(&self.server, &self.prompt.name, arguments)
            .await
        {
            Ok(text) => CommandOutcome::Prompt { text, model: None },
            Err(why) => CommandOutcome::Error(why),
        }
    }
}

impl McpRegistrar for McpManager {
    fn add_server(&self, name: &str, entry: Value) -> Result<(), String> {
        let base = self.base_dir();
        let config =
            crate::config::server_from_entry(name, entry, &base, &|variable| std::env::var(variable).ok())?;
        self.add(config)
    }
}

impl CommandSource for McpManager {
    fn commands(&self) -> Vec<Command> {
        self.prompts()
            .into_iter()
            .map(|(server, prompt)| {
                let hint: Vec<String> = prompt
                    .arguments
                    .iter()
                    .flatten()
                    .map(|a| {
                        if a.required == Some(true) {
                            format!("<{}>", a.name)
                        } else {
                            format!("[{}]", a.name)
                        }
                    })
                    .collect();
                Command {
                    name: format!(
                        "mcp:{}:{}",
                        crate::name_segment(&server),
                        crate::name_segment(&prompt.name)
                    ),
                    description: prompt
                        .description
                        .clone()
                        .unwrap_or_else(|| format!("Prompt from MCP server {server}")),
                    argument_hint: (!hint.is_empty()).then(|| hint.join(" ")),
                    handler: Arc::new(PromptCommand {
                        manager: self.clone(),
                        server,
                        prompt,
                    }),
                }
            })
            .collect()
    }
}

/// The state of every server, one per line, then the entries that could not be used.
pub fn status_report(manager: &McpManager) -> String {
    let mut out = String::new();
    for server in manager.status() {
        out.push_str(&format!("{}  {}", server.name, server.state));
        if server.state == State::Ready {
            out.push_str(&format!(" · {} tools · {} prompts", server.tools, server.prompts));
        }
        out.push_str(&format!(
            "\n  {}\n  from {}\n",
            server.target,
            server.source.display()
        ));
    }
    for (what, why) in manager.problems() {
        out.push_str(&format!("{what}  not loaded: {why}\n"));
    }
    if manager
        .status()
        .iter()
        .any(|server| matches!(server.state, State::WaitingApproval | State::Rejected))
    {
        out.push_str(
            "a server this project defines starts once you approve it: `azazel mcp approve <name>`, or \
             /mcp approve <name> in a session\n",
        );
    }
    if out.is_empty() {
        out.push_str("no MCP servers are configured; add one with `azazel mcp add <name> -- <command>`\n");
    }
    out
}

struct McpCommand(McpManager);

#[async_trait]
impl CommandHandler for McpCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        let words = split_args(args);
        let done = |result: Result<String, String>| match result {
            Ok(shown) => CommandOutcome::Show(shown),
            Err(why) => CommandOutcome::Error(why),
        };
        match words.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            [] | ["list"] => CommandOutcome::Show(status_report(&self.0)),
            ["reconnect", name] => done(self.0.reconnect(name).map(|()| format!("reconnecting {name}"))),
            ["enable", name] => done(
                self.0
                    .set_enabled(name, true)
                    .map(|()| format!("{name} is enabled")),
            ),
            ["disable", name] => done(
                self.0
                    .set_enabled(name, false)
                    .map(|()| format!("{name} is disabled")),
            ),
            ["approve", name] => done(
                self.0
                    .decide(&[(name.to_string(), true)])
                    .map(|_| format!("{name} is approved")),
            ),
            ["reject", name] => done(
                self.0
                    .decide(&[(name.to_string(), false)])
                    .map(|_| format!("{name} is rejected and will not start")),
            ),
            ["login", name] => {
                let ui = ctx.services.get::<Ui>();
                let show = move |url: &str| match &ui {
                    Some(ui) => {
                        ui.0.notify(Level::Info, &format!("Open this address to log in:\n{url}"))
                    }
                    None => tracing::info!("MCP login address: {url}"),
                };
                done(
                    self.0
                        .login(name, &show)
                        .await
                        .map(|()| format!("logged in to {name}")),
                )
            }
            ["logout", name] => done(self.0.logout(name).map(|removed| {
                if removed {
                    format!("logged out of {name}")
                } else {
                    format!("no login was stored for {name}")
                }
            })),
            _ => CommandOutcome::Error(
                "usage: /mcp [list | reconnect|enable|disable|approve|reject|login|logout <server>]".to_string(),
            ),
        }
    }
}

/// Starts the servers when the first session begins.
struct Starter(McpManager);

#[async_trait]
impl EventHandler for Starter {
    async fn handle(&self, _: &mut Event, _: &ExtCtx) -> Outcome {
        self.0.start();
        // Asked beside the session, not before it: the question needs the UI to be running.
        let manager = self.0.clone();
        tokio::spawn(async move { manager.ask_approvals().await });
        Outcome::Continue
    }
}

/// Holds a session's first request back briefly so servers that start fast are in it.
struct StartupWait(McpManager);

#[async_trait]
impl EventHandler for StartupWait {
    async fn handle(&self, _: &mut Event, _: &ExtCtx) -> Outcome {
        self.0.wait_startup().await;
        Outcome::Continue
    }
}

/// Registers everything MCP contributes. The manager is also available as a service.
pub struct McpExt {
    pub manager: McpManager,
}

impl Extension for McpExt {
    fn id(&self) -> &str {
        "mcp"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        let manager = self.manager.clone();
        r.service(Arc::new(manager.clone()));
        r.service(Arc::new(McpServers(Arc::new(manager.clone()))));
        r.tool_source(Arc::new(manager.clone()));
        r.tool(Arc::new(ResourcesTool(manager.clone())));
        r.scheme(Arc::new(McpScheme(manager.clone())));
        r.system_part("mcp", Arc::new(McpPart(manager.clone())));
        r.command_source(Arc::new(manager.clone()));
        r.command(Command {
            name: "mcp".to_string(),
            description: "MCP servers: state, reconnect, enable or disable, approve, log in".to_string(),
            argument_hint: Some("[reconnect|enable|disable|approve|reject|login|logout <server>]".to_string()),
            handler: Arc::new(McpCommand(manager.clone())),
        });
        r.on(EventKind::SessionStart, Arc::new(Starter(manager.clone())));
        r.on(EventKind::TurnStart, Arc::new(StartupWait(manager)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rmcp::model::PromptArgument;

    use super::*;

    fn prompt() -> Prompt {
        let arguments = vec![
            PromptArgument::new("name").with_required(true),
            PromptArgument::new("tone"),
            PromptArgument::new("lang"),
        ];
        Prompt::new("greet", Some("Greets"), Some(arguments))
    }

    fn text(arguments: &JsonObject) -> String {
        Value::Object(arguments.clone()).to_string()
    }

    #[test]
    fn prompt_arguments_fill_by_name_then_by_position() {
        let prompt = prompt();
        assert_eq!(
            text(&prompt_arguments(&prompt, "Ada").unwrap()),
            r#"{"name":"Ada"}"#
        );
        assert_eq!(
            text(&prompt_arguments(&prompt, "\"Ada Lovelace\" warm").unwrap()),
            r#"{"name":"Ada Lovelace","tone":"warm"}"#
        );
        // A named argument leaves its position to the next unnamed word.
        assert_eq!(
            text(&prompt_arguments(&prompt, "tone=dry Ada fr").unwrap()),
            r#"{"tone":"dry","name":"Ada","lang":"fr"}"#
        );
        // `x=y` is an ordinary word when `x` is not an argument of the prompt.
        assert_eq!(
            text(&prompt_arguments(&prompt, "a=b").unwrap()),
            r#"{"name":"a=b"}"#
        );
        assert_eq!(
            prompt_arguments(&prompt, "tone=dry").unwrap_err(),
            "missing: name"
        );
        assert!(
            prompt_arguments(&prompt, "a b c d")
                .unwrap_err()
                .starts_with("too many arguments")
        );
        assert!(
            prompt_arguments(&Prompt::new("plain", None::<String>, None), "")
                .unwrap()
                .is_empty()
        );
    }
}
