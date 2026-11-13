//! Model layer: message and stream types, the `Provider` trait, wire APIs, the model catalogue,
//! credentials, and the retry/fallback `Router`.

pub mod catalog;
pub mod credentials;
pub mod dispatch;
pub mod error;
pub mod limits;
pub mod models_file;
pub mod oauth;
pub mod provider;
pub mod providers;
pub mod router;
pub mod sse;
pub mod stall;
pub mod stream;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
pub mod types;
pub mod wire;

pub use catalog::{Catalog, Cost, ModelInfo, ModelOverride};
pub use credentials::{Account, Auth, Credential, Credentials, DEFAULT_ACCOUNT};
pub use dispatch::{Route, RouteReason, RouteRequest, VIRTUAL, VirtualModel, VirtualModels, is_virtual};
pub use error::AiError;
pub use limits::{AccountLimits, LimitStore, Window};
pub use models_file::ModelsFile;
pub use oauth::{LoginUi, OAuthError, OAuthFlow, OAuthSettings};
pub use provider::Provider;
pub use providers::presets::{AuthStyle, Discovery, ProviderConfig, ProviderSettings};
pub use router::{RetryPolicy, RouteEvent, RouteState, Router};
pub use stall::StallLimits;
pub use stream::{EventStream, StreamEvent};
pub use types::{
    AssistantMeta, Block, HostedTool, INVALID_JSON_KEY, Message, ModelRef, ModelSpec, ParseError, Request,
    Role, StopReason, SystemPart, ThinkingLevel, Tier, ToolSpec, Usage, WebSource, WireApi,
};
