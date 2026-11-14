//! Model Context Protocol: server configuration, connections, and the tools, resources and
//! prompts servers contribute to a session; and azazel itself served to another MCP client.

mod catalog;
mod client;
pub mod config;
mod convert;
mod elicit;
mod ext;
mod handler;
pub mod import;
mod manager;
pub mod oauth;
pub mod serve;
pub mod sse;

pub use ext::{McpExt, TOOL_SEARCH, status_report};
pub use manager::{McpManager, Options, SearchHit, ServerStatus, State};

/// One segment of a proxied tool name: characters outside `[A-Za-z0-9_-]` become `_`.
pub fn name_segment(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The name a server's tool has for agents: `mcp__<server>__<tool>`.
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{}__{}", name_segment(server), name_segment(tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_keep_only_safe_characters() {
        assert_eq!(tool_name("echo", "add"), "mcp__echo__add");
        assert_eq!(tool_name("my server", "ns/tool.v2"), "mcp__my_server__ns_tool_v2");
        assert_eq!(tool_name("déjà-vu", "read_file"), "mcp__d_j_-vu__read_file");
    }
}
