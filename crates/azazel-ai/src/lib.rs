//! Model layer: message and stream types, the `Provider` trait, wire APIs, the model catalogue,
//! credentials, and the retry/fallback `Router`.

pub mod catalog;
pub mod credentials;
pub mod error;
pub mod oauth;
pub mod provider;
pub mod providers;
pub mod router;
pub mod sse;
pub mod stream;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
pub mod types;
pub mod wire;

pub use catalog::{Catalog, Cost, ModelInfo, ModelOverride};
pub use credentials::{Auth, Credential, Credentials};
pub use error::AiError;
pub use oauth::{LoginUi, OAuthError, OAuthFlow, OAuthSettings};
pub use provider::Provider;
pub use providers::presets::{ProviderConfig, ProviderSettings};
pub use router::{RetryPolicy, RouteEvent, RouteState, Router};
pub use stream::{EventStream, StreamEvent};
pub use types::{
    AssistantMeta, Block, INVALID_JSON_KEY, Message, ModelRef, ModelSpec, ParseError, Request, Role,
    StopReason, SystemPart, ThinkingLevel, ToolSpec, Usage, WireApi,
};
