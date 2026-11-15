//! Opening the connection to one server: process or HTTP transport, then the MCP handshake.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use azazel_ai::Credentials;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientHandler, ServiceExt};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::config::{ServerConfig, Transport};
use crate::sse::SseError;
use crate::{oauth, sse};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// The server wants the user to log in first.
    NeedsAuth,
    /// Trying again cannot help (the command does not exist, the entry is malformed).
    Fatal(String),
    /// Worth another attempt later.
    Retry(String),
}

fn handshake_error(error: rmcp::service::ClientInitializeError) -> ConnectError {
    if error.is_authorization_required() {
        ConnectError::NeedsAuth
    } else {
        ConnectError::Retry(error.to_string())
    }
}

/// Starts `command` and speaks MCP over its stdin and stdout. Its stderr goes to the log.
async fn connect_process<H: ClientHandler>(
    server: &str,
    command: &str,
    args: &[String],
    env: &std::collections::BTreeMap<String, String>,
    cwd: &std::path::Path,
    handler: H,
) -> Result<RunningService<RoleClient, H>, ConnectError> {
    // Resolved by hand so that script shims (`npx.cmd`) are found on Windows too.
    let program = which::which_in(command, std::env::var_os("PATH"), cwd)
        .map_err(|_| ConnectError::Fatal(format!("`{command}` was not found on PATH")))?;
    // In a session of its own with no terminal, like every program azazel starts.
    let process = azazel_core::process::helper_command(program, |process| {
        process.args(args).envs(env).current_dir(cwd);
    });
    let (transport, stderr) = TokioChildProcess::builder(process)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ConnectError::Fatal(format!("could not start `{command}`: {e}")))?;
    if let Some(stderr) = stderr {
        let server = server.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!("mcp server {server} stderr: {line}");
            }
        });
    }
    handler.serve(transport).await.map_err(handshake_error)
}

fn header_map(
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<HashMap<HeaderName, HeaderValue>, ConnectError> {
    headers
        .iter()
        .map(|(name, value)| {
            let bad = |what: &str| ConnectError::Fatal(format!("header `{name}` has an invalid {what}"));
            let header = HeaderName::from_bytes(name.as_bytes()).map_err(|_| bad("name"))?;
            let value = HeaderValue::from_str(value).map_err(|_| bad("value"))?;
            Ok((header, value))
        })
        .collect()
}

/// The HTTP client for one server's requests.
pub(crate) fn http_client(config: &ServerConfig) -> Result<reqwest::Client, String> {
    azazel_ai::tls::client_builder()
        .connect_timeout(config.timeout)
        .build()
        .map_err(|e| format!("could not set up the HTTP client: {e}"))
}

/// Connects over streamable HTTP, with the stored OAuth tokens when the user has logged in. An
/// entry of `type: "sse"` falls back to the legacy SSE transport when that is refused.
async fn connect_http<H: ClientHandler + Clone>(
    config: &ServerConfig,
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
    legacy_sse: bool,
    handler: H,
    credentials: &Arc<Credentials>,
) -> Result<RunningService<RoleClient, H>, ConnectError> {
    let http = http_client(config).map_err(ConnectError::Fatal)?;
    let transport_config =
        StreamableHttpClientTransportConfig::with_uri(url).custom_headers(header_map(headers)?);
    let authorized = oauth::authorized_client(url, credentials.clone(), http.clone())
        .await
        .map_err(|e| ConnectError::Retry(format!("stored login could not be used: {e}")))?;
    let streamable = match authorized {
        Some(client) => {
            let transport = StreamableHttpClientTransport::with_client(client, transport_config);
            handler.clone().serve(transport).await
        }
        None => {
            let transport = StreamableHttpClientTransport::with_client(http.clone(), transport_config);
            handler.clone().serve(transport).await
        }
    };
    let refused = match streamable {
        Ok(running) => return Ok(running),
        Err(error) => handshake_error(error),
    };
    if !legacy_sse || refused == ConnectError::NeedsAuth {
        return Err(refused);
    }
    let transport = sse::connect(url, http, headers, config.timeout)
        .await
        .map_err(|error| match error {
            SseError::Unauthorized { .. } => ConnectError::NeedsAuth,
            other => ConnectError::Retry(format!("{other} (streamable HTTP was refused too)")),
        })?;
    handler.serve(transport).await.map_err(handshake_error)
}

/// Connects to the server `config` describes and completes the handshake.
pub async fn connect<H: ClientHandler + Clone>(
    config: &ServerConfig,
    handler: H,
    credentials: &Arc<Credentials>,
) -> Result<RunningService<RoleClient, H>, ConnectError> {
    let connecting = async {
        match &config.transport {
            Transport::Stdio {
                command,
                args,
                env,
                cwd,
            } => connect_process(&config.name, command, args, env, cwd, handler).await,
            Transport::Http {
                url,
                headers,
                legacy_sse,
            } => connect_http(config, url, headers, *legacy_sse, handler, credentials).await,
        }
    };
    match tokio::time::timeout(config.timeout, connecting).await {
        Ok(result) => result,
        Err(_) => Err(ConnectError::Retry(format!(
            "no answer to the handshake within {} s",
            config.timeout.as_secs()
        ))),
    }
}
