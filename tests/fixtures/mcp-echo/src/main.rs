//! A small MCP server over stdio for tests: tools `echo` and `add`, resource `mem://greeting`,
//! prompt `greet`. When `MCP_ECHO_PID_FILE` is set, the process id is written there at startup so
//! a test can kill the server. When `MCP_ECHO_STRICT` is set, an `initialize` asking for a
//! protocol version newer than 2025-11-25 is refused with -32022, as some deployed servers do.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorCode, GetPromptRequestParams,
    GetPromptResponse, GetPromptResult, InitializeRequestParams, InitializeResult, JsonObject,
    ListPromptsResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams, Prompt, PromptArgument,
    PromptMessage, ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    Resource, ResourceContents, Role, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Value, json};

const GREETING_URI: &str = "mem://greeting";

struct Echo;

fn schema(value: Value) -> Arc<JsonObject> {
    Arc::new(value.as_object().cloned().unwrap_or_default())
}

impl ServerHandler for Echo {
    fn get_info(&self) -> ServerConfig {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .enable_prompts()
            .build();
        ServerConfig::new(capabilities)
            .with_instructions("Echo fixture: `echo` returns its text, `add` sums two integers.")
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        let known = ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25);
        if std::env::var_os("MCP_ECHO_STRICT").is_some() && !known.contains(&request.protocol_version) {
            return Err(ErrorData::new(
                ErrorCode(-32022),
                format!("unsupported protocol version {}", request.protocol_version),
                None,
            ));
        }
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let echo = Tool::new(
            "echo",
            "Returns the text it was given",
            schema(json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            })),
        );
        let add = Tool::new(
            "add",
            "Adds two integers",
            schema(json!({
                "type": "object",
                "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
                "required": ["a", "b"]
            })),
        )
        .with_annotations(ToolAnnotations::new().read_only(true));
        Ok(ListToolsResult::with_all_items(vec![echo, add]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arguments = request.arguments.unwrap_or_default();
        let failed = |problem: &str| CallToolResult::error(vec![ContentBlock::text(problem)]);
        let result = match request.name.as_ref() {
            "echo" => match arguments.get("text").and_then(Value::as_str) {
                Some(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
                None => failed("`text` must be a string"),
            },
            "add" => {
                let number = |key: &str| arguments.get(key).and_then(Value::as_i64);
                match (number("a"), number("b")) {
                    (Some(a), Some(b)) => {
                        CallToolResult::success(vec![ContentBlock::text((a + b).to_string())])
                    }
                    _ => failed("`a` and `b` must be integers"),
                }
            }
            other => return Err(ErrorData::invalid_params(format!("unknown tool `{other}`"), None)),
        };
        Ok(result.into())
    }

    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(vec![Resource::new(
            GREETING_URI,
            "greeting",
        )]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        if request.uri == GREETING_URI {
            Ok(ReadResourceResult::new(vec![ResourceContents::text(
                "hello from the echo fixture",
                GREETING_URI,
            )])
            .into())
        } else {
            Err(ErrorData::resource_not_found(
                format!("no resource `{}`", request.uri),
                None,
            ))
        }
    }

    async fn list_prompts(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        let name = PromptArgument::new("name")
            .with_description("Who to greet")
            .with_required(true);
        let greet = Prompt::new("greet", Some("Greets someone by name"), Some(vec![name]));
        Ok(ListPromptsResult::with_all_items(vec![greet]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        if request.name != "greet" {
            return Err(ErrorData::invalid_params(
                format!("unknown prompt `{}`", request.name),
                None,
            ));
        }
        let name = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("there")
            .to_string();
        let message = PromptMessage::new_text(Role::User, format!("Say hello to {name}."));
        Ok(GetPromptResult::new(vec![message]).into())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(file) = std::env::var_os("MCP_ECHO_PID_FILE") {
        std::fs::write(file, std::process::id().to_string())?;
    }
    let service = Echo.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
