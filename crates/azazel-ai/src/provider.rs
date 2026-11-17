//! The provider abstraction. Routing, pricing and credentials live elsewhere on purpose.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::catalog::ModelInfo;
use crate::error::AiError;
use crate::stream::EventStream;
use crate::types::Request;

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;

    /// Whether the provider searches the web itself when a request asks for it
    /// ([`crate::types::HostedTool::WebSearch`]). A provider that says no is never sent such a
    /// request.
    fn hosts_web_search(&self) -> bool {
        false
    }

    /// Whether the provider searches for tools itself when `model` is asked, so a tool marked
    /// [`crate::types::ToolSpec::defer`] stays out of the prompt until the model looks for it.
    /// A provider that says no is sent every tool in full.
    fn searches_tools(&self, _model: &ModelInfo) -> bool {
        false
    }

    /// Starts one response. An `Err` means nothing was streamed; failures after the first event
    /// arrive as `StreamEvent::Error`.
    async fn stream(
        &self,
        model: &ModelInfo,
        req: &Request,
        cancel: CancellationToken,
    ) -> Result<EventStream, AiError>;

    async fn list_models(&self) -> Result<Vec<ModelInfo>, AiError>;
}
