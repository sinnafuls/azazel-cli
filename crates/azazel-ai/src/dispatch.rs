//! Virtual models: names under the provider `virtual` that stand for no model of their own.
//! Each picks, for every request made with it, the models the request goes to.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};

use async_trait::async_trait;

use crate::error::AiError;
use crate::types::{ModelRef, ModelSpec, Request, Role, ThinkingLevel};

/// The provider id every virtual model is listed under: `virtual/<name>`.
pub const VIRTUAL: &str = "virtual";

/// Why a request is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteReason {
    /// The first request after something the user wrote.
    User,
    /// Any other request of an agent's turn, such as the one after tool results.
    Continuation,
    /// The request is sent again after an attempt that failed.
    Retry,
    /// A request from outside an agent's conversation: a summary, a side question.
    Direct,
}

impl RouteReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RouteReason::User => "user",
            RouteReason::Continuation => "continuation",
            RouteReason::Retry => "retry",
            RouteReason::Direct => "direct",
        }
    }
}

/// What a virtual model is told about the request it picks a model for.
pub struct RouteRequest<'a> {
    /// The virtual model's name, without `virtual/`.
    pub name: &'a str,
    pub reason: RouteReason,
    /// The thinking level the request asks for.
    pub thinking: ThinkingLevel,
    /// The model that wrote the latest reply of the conversation.
    pub previous: Option<ModelRef>,
    /// For a retry: the model that failed and how.
    pub failed: Option<(&'a ModelRef, &'a AiError)>,
    pub request: &'a Request,
}

impl RouteRequest<'_> {
    /// The text of the latest message the user wrote.
    pub fn last_user_text(&self) -> Option<String> {
        self.request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| message.text())
    }
}

/// Where a virtual model sends one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// A fallback chain like any other: models and roles, first choice first.
    pub chain: Vec<ModelSpec>,
    /// The thinking level for the models of `chain` that name none of their own; `None` leaves
    /// them the level the request asks for.
    pub thinking: Option<ThinkingLevel>,
}

/// Picks the models of one request. `Err` says why no model could be picked.
#[async_trait]
pub trait VirtualModel: Send + Sync {
    async fn route(&self, request: &RouteRequest<'_>) -> Result<Route, String>;
}

/// The virtual models of the process, by name. Shared by the router and by whoever registers
/// them, so that one registered while the process runs is routed through at once.
#[derive(Default)]
pub struct VirtualModels {
    models: RwLock<BTreeMap<String, Registered>>,
}

/// One registered virtual model and the extension it belongs to.
struct Registered {
    owner: String,
    model: Arc<dyn VirtualModel>,
}

impl VirtualModels {
    /// Registers `virtual/<name>` for `owner`, in place of an earlier one of that name.
    pub fn add(&self, owner: &str, name: &str, model: Arc<dyn VirtualModel>) {
        self.models
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                name.to_string(),
                Registered {
                    owner: owner.to_string(),
                    model,
                },
            );
    }

    /// Removes everything `owner` registered.
    pub fn remove_owner(&self, owner: &str) {
        self.models
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|_, registered| registered.owner != owner);
    }

    /// The registered names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.models
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn VirtualModel>> {
        self.models
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .map(|registered| registered.model.clone())
    }
}

/// Whether `model` is a virtual one.
pub fn is_virtual(model: &ModelRef) -> bool {
    model.provider == VIRTUAL
}
