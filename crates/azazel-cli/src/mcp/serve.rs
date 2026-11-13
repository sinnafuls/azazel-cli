//! `azazel mcp serve`: azazel as an MCP server on standard input and output.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{AgentInfo, Tool, ToolCtx, ToolOutput, ToolSource, UpdateSink};
use azazel_core::{
    ApprovalDecision, ApprovalRequest, Extension, Level, Question, StartSource, ToolCaller, Ui, UiBridge,
    UiError,
};
use azazel_mcp::serve::{Asker, Host, PromptReply, PromptRequest, READ_ONLY_TOOLS, serve_stdio};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::app::App;
use crate::headless::{Ended, build_app, run_prompt};

/// The agent the read-only tools are called as. They belong to no session.
fn caller() -> AgentInfo {
    AgentInfo {
        id: "mcp".into(),
        def: "lead".into(),
        parent: None,
        depth: 0,
        session_id: String::new(),
    }
}

/// What agents ask of the user goes to the MCP client: a permission as an elicitation, anything
/// else is answered as it is when nobody is there.
struct ClientBridge(Arc<dyn Asker>);

#[async_trait]
impl UiBridge for ClientBridge {
    async fn ask(&self, _: &Question) -> Result<Vec<String>, UiError> {
        Err(UiError::NoUser)
    }

    async fn confirm(&self, _: &str) -> Result<bool, UiError> {
        Err(UiError::NoUser)
    }

    async fn select(&self, _: &str, _: &[String]) -> Result<usize, UiError> {
        Err(UiError::NoUser)
    }

    async fn input(&self, _: &str) -> Result<String, UiError> {
        Err(UiError::NoUser)
    }

    fn notify(&self, level: Level, text: &str) {
        match level {
            Level::Info => tracing::info!("{text}"),
            Level::Warn => tracing::warn!("{text}"),
            Level::Error => tracing::error!("{text}"),
        }
    }

    async fn approval(&self, request: ApprovalRequest) -> Option<ApprovalDecision> {
        let mut question = format!("azazel wants to use `{}`", request.tool);
        if !request.subjects.is_empty() {
            question.push_str(&format!(" on {}", request.subjects.join(", ")));
        }
        if let Some(note) = &request.note {
            question.push_str(&format!(" ({note})"));
        }
        question.push_str(". Allow it?");
        match self.0.allow(&question).await? {
            true => Some(ApprovalDecision::AllowOnce),
            false => Some(ApprovalDecision::Deny { feedback: None }),
        }
    }
}

struct Server {
    /// Serves the read-only tools, in the directory the server was started in.
    tools: App,
    extensions: Vec<Box<dyn Extension>>,
}

#[async_trait]
impl Host for Server {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools = self.tools.registry.tools.tools(&caller());
        tools.retain(|tool| READ_ONLY_TOOLS.contains(&tool.name()));
        tools
    }

    async fn call(
        &self,
        tool: &str,
        input: Value,
        asker: Arc<dyn Asker>,
        cancel: CancellationToken,
    ) -> ToolOutput {
        let services = &self.tools.registry.services;
        let Some(tools) = services.get::<ToolCaller>() else {
            return ToolOutput::error("tools cannot be called here");
        };
        // One client is connected, so the bridge of the latest call is the bridge of them all.
        services.insert(Arc::new(Ui(Arc::new(ClientBridge(asker)))));
        let agent = caller();
        let ctx = ToolCtx {
            agent_id: agent.id.clone(),
            session_id: agent.session_id.clone(),
            agent,
            call_id: format!("mcp-{}", uuid_like()),
            cwd: self.tools.paths.cwd.clone(),
            cancel,
            update: UpdateSink::default(),
            services: services.clone(),
        };
        tools.call(&ctx, tool, input).await
    }

    async fn prompt(
        &self,
        request: PromptRequest,
        asker: Arc<dyn Asker>,
        cancel: CancellationToken,
    ) -> Result<PromptReply, String> {
        let PromptRequest {
            prompt,
            cwd,
            mode,
            model,
            session,
        } = request;
        // A client is another program, often another agent: it cannot lift the guards for itself.
        if mode.as_deref() == Some("bypass") {
            return Err(
                "bypass mode is not available to an MCP client; the person who starts `azazel mcp serve` \
                 chooses it with `--mode bypass`"
                    .to_string(),
            );
        }
        let built = build_app(&self.extensions, |cli| {
            if let Some(cwd) = cwd {
                cli.cwd = Some(PathBuf::from(cwd));
            }
            if mode.is_some() {
                cli.mode = mode;
            }
            if let Some(model) = model {
                cli.model = vec![model];
            }
            cli.resume_latest = false;
            cli.fork = None;
            cli.session = session;
        })
        .await;
        let (app, cli) = built.map_err(|e| format!("{e:#}"))?;
        app.registry
            .services
            .insert(Arc::new(Ui(Arc::new(ClientBridge(asker)))));
        let opened = match app.requested_session(&cli) {
            Ok(how) => app.open(how, app.chain.clone(), app.thinking).await,
            Err(e) => Err(e),
        };
        let active = opened.map_err(|e| format!("{e:#}"))?;
        app.start(if active.resumed {
            StartSource::Resume
        } else {
            StartSource::Startup
        })
        .await;
        let ended = run_prompt(&app, &active, prompt, Vec::new(), &cancel, &mut |_, _| {}).await;
        let session_id = active.session.id();
        app.finish(&active).await;
        match ended {
            Ok(Ended::Idle { answer, .. }) => Ok(PromptReply {
                reply: answer,
                session_id,
            }),
            Ok(Ended::Failed(error)) => Err(format!("{error} (session: {session_id})")),
            Err(reason) => Err(format!("the prompt was rejected: {reason}")),
        }
    }
}

/// A call id no other call of this process has.
fn uuid_like() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CALLS: AtomicU64 = AtomicU64::new(0);
    CALLS.fetch_add(1, Ordering::Relaxed) + 1
}

/// Serves until the client closes the connection.
pub async fn run(extensions: Vec<Box<dyn Extension>>) -> u8 {
    // The tools need no model, so this is built the way the interactive UI is: a missing model
    // is an error only once a prompt is to be run.
    let tools = match build_app(&extensions, |cli| cli.print = false).await {
        Ok((app, _)) => app,
        Err(e) => {
            eprintln!("azazel: {e:#}");
            return crate::print::EXIT_FAILED;
        }
    };
    match serve_stdio(Arc::new(Server { tools, extensions })).await {
        Ok(()) => crate::print::EXIT_OK,
        Err(e) => {
            eprintln!("azazel: {e}");
            crate::print::EXIT_FAILED
        }
    }
}
