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
pub mod services;
pub mod sse;
pub mod stall;
pub mod stream;
pub mod substitute;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
pub mod tls;
pub mod types;
pub mod wire;

pub use catalog::{Catalog, Cost, ModelInfo, ModelOverride};
pub use credentials::{Account, Auth, Credential, Credentials, DEFAULT_ACCOUNT};
pub use dispatch::{Route, RouteReason, RouteRequest, VIRTUAL, VirtualModel, VirtualModels, is_virtual};
pub use error::AiError;
pub use limits::{AccountLimits, LimitBlock, LimitStore, Window};
pub use models_file::ModelsFile;
pub use oauth::{LoginUi, OAuthError, OAuthFlow, OAuthSettings};
pub use provider::Provider;
pub use providers::presets::{AuthStyle, Discovery, ProviderConfig, ProviderSettings};
pub use router::{RetryPolicy, RouteEvent, RouteState, Router};
pub use services::{Service, ServiceStore};
pub use stall::{StallLimits, Timeouts};
pub use stream::{EventStream, StreamEvent};
pub use types::{
    AssistantMeta, Block, HostedTool, INVALID_JSON_KEY, Message, ModelRef, ModelSpec, ParseError, Request,
    Role, StopReason, SystemPart, ThinkingLevel, Tier, ToolSpec, Usage, WebSource, WireApi,
};
