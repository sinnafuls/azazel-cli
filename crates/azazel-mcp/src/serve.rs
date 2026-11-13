//! Serving azazel to another MCP client (`azazel mcp serve`): the tool `azazel_prompt`, which
//! runs a session to the end of one prompt, and azazel's read-only tools under their own names.
//!
//! The protocol side lives here. Running sessions and tools is the [`Host`]'s part, which the
//! command line implements.

use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Tool, ToolOutput};
use azazel_ai::Block;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ElicitRequestParams,
    ElicitationAction, ElicitationSchema, Implementation, JsonObject, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, ToolAnnotations,
};
use rmcp::service::{ElicitationMode, Peer, RequestContext};
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// The tool that runs a prompt.
pub const PROMPT_TOOL: &str = "azazel_prompt";
/// The tools served under their own names: those that read and change nothing.
pub const READ_ONLY_TOOLS: [&str; 5] = ["read", "glob", "grep", "web_search", "web_fetch"];

const PROMPT_DESCRIPTION: &str = "Hands a task to azazel, a coding agent with its own tools, and returns its \
final reply when it has finished. The reply ends with the session id: pass it back as `session` (with the \
same `cwd`) to continue that conversation.";

/// The arguments of [`PROMPT_TOOL`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptRequest {
    pub prompt: String,
    /// The directory to work in; the server's own when left out.
    pub cwd: Option<String>,
    /// The permission mode: `plan`, `ask`, `accept-edits` or `auto`.
    pub mode: Option<String>,
    pub model: Option<String>,
    /// A session to continue, by id.
    pub session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptReply {
    pub reply: String,
    pub session_id: String,
}

/// Asks the user of the MCP client to allow something.
#[async_trait]
pub trait Asker: Send + Sync {
    /// The user's answer; `None` when the client cannot ask its user.
    async fn allow(&self, question: &str) -> Option<bool>;
}

/// What runs the sessions and the tools.
#[async_trait]
pub trait Host: Send + Sync + 'static {
    /// The tools to serve besides [`PROMPT_TOOL`].
    fn tools(&self) -> Vec<Arc<dyn Tool>>;

    /// Calls one of [`Host::tools`]. A permission the call needs is asked for through `asker`.
    async fn call(
        &self,
        tool: &str,
        input: Value,
        asker: Arc<dyn Asker>,
        cancel: CancellationToken,
    ) -> ToolOutput;

    /// Runs a session to the end of one prompt. `Err` is why it could not, or how it failed.
    async fn prompt(
        &self,
        request: PromptRequest,
        asker: Arc<dyn Asker>,
        cancel: CancellationToken,
    ) -> Result<PromptReply, String>;
}

/// Asks through MCP elicitation, when the client supports its form mode.
struct ClientAsker(Peer<RoleServer>);

#[async_trait]
impl Asker for ClientAsker {
    async fn allow(&self, question: &str) -> Option<bool> {
        if !self
            .0
            .supported_elicitation_modes()
            .contains(&ElicitationMode::Form)
        {
            return None;
        }
        let requested_schema = ElicitationSchema::builder().required_bool("allow").build().ok()?;
        let asked = self
            .0
            .create_elicitation(ElicitRequestParams::FormElicitationParams {
                meta: None,
                message: question.to_string(),
                requested_schema,
            })
            .await;
        match asked {
            Ok(answer) if answer.action == ElicitationAction::Accept => Some(
                answer
                    .content
                    .as_ref()
                    .and_then(|content| content.get("allow"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
            // Declining or dismissing the question refuses what it asked for.
            Ok(_) => Some(false),
            Err(e) => {
                tracing::warn!("the MCP client did not answer a permission question: {e}");
                None
            }
        }
    }
}

struct Server {
    host: Arc<dyn Host>,
}

fn object(value: Value) -> Arc<JsonObject> {
    Arc::new(value.as_object().cloned().unwrap_or_default())
}

fn prompt_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "prompt": { "type": "string", "description": "What azazel should do" },
            "cwd": {
                "type": "string",
                "description": "The directory to work in; the server's working directory when left out"
            },
            "mode": {
                "type": "string",
                "enum": ["plan", "ask", "accept-edits", "auto"],
                "description": "The permission mode; the configured one when left out"
            },
            "model": {
                "type": "string",
                "description": "The model, as provider/model or @role; the configured one when left out"
            },
            "session": {
                "type": "string",
                "description": "The id of an earlier azazel_prompt session to continue"
            }
        },
        "required": ["prompt"]
    })
}

fn blocks(content: Vec<Block>) -> Vec<ContentBlock> {
    content
        .into_iter()
        .filter_map(|block| match block {
            Block::Text { text } => Some(ContentBlock::text(text)),
            Block::Image { mime, data } => Some(ContentBlock::image(data, mime)),
            _ => None,
        })
        .collect()
}

fn result(output: ToolOutput) -> CallToolResult {
    if output.is_error {
        CallToolResult::error(blocks(output.content))
    } else {
        CallToolResult::success(blocks(output.content))
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("azazel", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "azazel is a coding agent. `azazel_prompt` hands it a task and returns its reply; the other \
                 tools read files and the web without changing anything.",
            )
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools = vec![rmcp::model::Tool::new(
            PROMPT_TOOL,
            PROMPT_DESCRIPTION,
            object(prompt_schema()),
        )];
        for tool in self.host.tools() {
            tools.push(
                rmcp::model::Tool::new(
                    tool.name().to_string(),
                    tool.description().to_string(),
                    object(tool.schema()),
                )
                .with_annotations(ToolAnnotations::new().read_only(true)),
            );
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arguments = request.arguments.unwrap_or_default();
        let asker: Arc<dyn Asker> = Arc::new(ClientAsker(context.peer.clone()));
        let name = request.name.as_ref();
        if name == PROMPT_TOOL {
            let text = |key: &str| {
                arguments
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
            };
            let Some(prompt) = text("prompt") else {
                return Err(ErrorData::invalid_params("`prompt` must be a string", None));
            };
            let request = PromptRequest {
                prompt,
                cwd: text("cwd"),
                mode: text("mode"),
                model: text("model"),
                session: text("session"),
            };
            let outcome = self.host.prompt(request, asker, context.ct.clone()).await;
            return Ok(match outcome {
                Ok(done) => {
                    let mut result = CallToolResult::success(vec![
                        ContentBlock::text(done.reply.clone()),
                        ContentBlock::text(format!("session: {}", done.session_id)),
                    ]);
                    result.structured_content =
                        Some(json!({ "reply": done.reply, "session_id": done.session_id }));
                    result
                }
                Err(why) => CallToolResult::error(vec![ContentBlock::text(why)]),
            }
            .into());
        }
        if !self.host.tools().iter().any(|tool| tool.name() == name) {
            return Err(ErrorData::invalid_params(format!("unknown tool `{name}`"), None));
        }
        let output = self
            .host
            .call(name, Value::Object(arguments), asker, context.ct.clone())
            .await;
        Ok(result(output).into())
    }
}

/// Serves `host` on standard input and output until the client goes away.
pub async fn serve_stdio(host: Arc<dyn Host>) -> Result<(), String> {
    let service = Server { host }
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| format!("the MCP handshake failed: {e}"))?;
    service.waiting().await.map_err(|e| e.to_string())?;
    Ok(())
}
