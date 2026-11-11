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
