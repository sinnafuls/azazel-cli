//! What this client does when a server calls back: roots, sampling, elicitation, progress,
//! logging and list-change notices.

// rmcp marks roots and sampling as deprecated (SEP-2577); deployed servers still rely on both.
#![allow(deprecated)]

use std::sync::Weak;

use azazel_ai::{Block, Message, ModelSpec, Request, Role, Router, StopReason, StreamEvent, SystemPart};
use azazel_core::{Level, Ui, UiError};
use futures::StreamExt;
use rmcp::model::{
    ClientCapabilities, ClientConfig, CreateMessageRequestParams, CreateMessageResult, ElicitRequestParams,
    ElicitResult, ElicitationAction, ElicitationCapability, FormElicitationCapability, Implementation,
    ListRootsResult, LoggingLevel, LoggingMessageNotificationParam, ProgressNotificationParam, Root,
    RootsCapabilities, SamplingCapability, SamplingMessage, SamplingMessageContentBlock,
};
use rmcp::service::{NotificationContext, RequestContext, RoleClient};
use rmcp::{ClientHandler, ErrorData};

use crate::elicit::{self, Answer};
use crate::manager::Inner;

/// The role whose model answers sampling requests.
const SAMPLING_ROLE: &str = "smol";

#[derive(Clone)]
pub(crate) struct Handler {
    pub server: String,
    pub inner: Weak<Inner>,
}

/// A sampling request as a request to our own model: text and images are kept, the rest dropped.
fn sampling_request(params: &CreateMessageRequestParams) -> Request {
    let messages = params
        .messages
        .iter()
        .map(|message| {
            let content = message
                .content
                .iter()
                .filter_map(|block| match block {
                    SamplingMessageContentBlock::Text(text) => Some(Block::text(text.text.clone())),
                    SamplingMessageContentBlock::Image(image) => Some(Block::Image {
                        mime: image.mime_type.clone(),
                        data: image.data.clone(),
                    }),
                    _ => None,
                })
                .collect();
            let role = match message.role {
                rmcp::model::Role::User => Role::User,
                rmcp::model::Role::Assistant => Role::Assistant,
            };
            Message {
                role,
                content,
                meta: None,
            }
        })
        .collect();
    let system = params
        .system_prompt
        .iter()
        .map(|text| SystemPart {
            text: text.clone(),
            name: String::new(),
            cache_boundary: false,
        })
        .collect();
    Request {
        system,
        messages,
        tools: Vec::new(),
        thinking: Default::default(),
        max_output: Some(params.max_tokens),
        temperature: params.temperature,
        cache_key: None,
        tier: None,
        hosted: Vec::new(),
        tail: Vec::new(),
    }
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        let mut roots = RootsCapabilities::default();
        roots.list_changed = Some(false);
        let mut capabilities = ClientCapabilities::default();
        capabilities.roots = Some(roots);
        capabilities.sampling = Some(SamplingCapability::default());
        capabilities.elicitation =
            Some(ElicitationCapability::new().with_form(FormElicitationCapability::new()));
        ClientConfig::new(
            capabilities,
            Implementation::new("azazel", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_roots(&self, _: RequestContext<RoleClient>) -> Result<ListRootsResult, ErrorData> {
        let Some(inner) = self.inner.upgrade() else {
            return Ok(ListRootsResult::default());
        };
        let roots = inner
            .roots
            .iter()
            .filter_map(|dir| {
                let uri = reqwest::Url::from_file_path(dir).ok()?;
                let root = Root::new(uri.to_string());
                Some(match dir.file_name() {
                    Some(name) => root.with_name(name.to_string_lossy()),
                    None => root,
                })
            })
            .collect();
        Ok(ListRootsResult::new(roots))
    }

    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, ErrorData> {
        let unavailable = || ErrorData::internal_error("no model is available for sampling", None);
        let inner = self.inner.upgrade().ok_or_else(unavailable)?;
        if !inner.sampling_allowed(&self.server).await {
            return Err(ErrorData::invalid_request(
                "the user did not allow this server to use the model",
                None,
            ));
        }
        let router = inner.services.get::<Router>().ok_or_else(unavailable)?;
        let request = sampling_request(&params);
        let chain = [ModelSpec::Role(SAMPLING_ROLE.to_string())];
        let (model, mut events) = router
            .stream(&chain, &request, context.ct.clone())
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        while let Some(event) = events.next().await {
            match event {
                StreamEvent::Done { message } => {
                    let stop = match message.meta.as_ref().map(|meta| meta.stop) {
                        Some(StopReason::Length) => CreateMessageResult::STOP_REASON_END_MAX_TOKEN,
                        _ => CreateMessageResult::STOP_REASON_END_TURN,
                    };
                    let reply = SamplingMessage::assistant_text(message.text());
                    return Ok(CreateMessageResult::new(reply, model.to_string()).with_stop_reason(stop));
                }
                StreamEvent::Error { error, .. } => {
                    return Err(ErrorData::internal_error(error.to_string(), None));
                }
                _ => {}
            }
        }
        Err(ErrorData::internal_error("the model returned nothing", None))
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        let decline = || Ok(ElicitResult::new(ElicitationAction::Decline));
        let Some(ui) = self.inner.upgrade().and_then(|inner| inner.services.get::<Ui>()) else {
            return decline();
        };
        match request {
            ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => {
                let schema = serde_json::to_value(&requested_schema)
                    .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
                Ok(
                    match elicit::fill(ui.0.as_ref(), &self.server, &message, &schema).await {
                        Answer::Accepted(content) => {
                            ElicitResult::new(ElicitationAction::Accept).with_content(content)
                        }
                        Answer::Declined => ElicitResult::new(ElicitationAction::Decline),
                        Answer::Cancelled => ElicitResult::new(ElicitationAction::Cancel),
                    },
                )
            }
            ElicitRequestParams::UrlElicitationParams { message, url, .. } => {
                let prompt = format!(
                    "{} asks: {message}\nOpen {url} and finish there. Done?",
                    self.server
                );
                Ok(match ui.0.confirm(&prompt).await {
                    Ok(true) => ElicitResult::new(ElicitationAction::Accept),
                    Ok(false) | Err(UiError::NoUser) => ElicitResult::new(ElicitationAction::Decline),
                    Err(UiError::Cancelled) => ElicitResult::new(ElicitationAction::Cancel),
                })
            }
            _ => decline(),
        }
    }

    async fn on_progress(&self, params: ProgressNotificationParam, _: NotificationContext<RoleClient>) {
        let Some(inner) = self.inner.upgrade() else { return };
        let Some(sink) = inner.progress_sink(&params.progress_token) else {
            return;
        };
        let amount = match params.total {
            Some(total) => format!("{}/{total}", params.progress),
            None => params.progress.to_string(),
        };
        sink.send(match params.message {
            Some(message) => format!("{message} ({amount})"),
            None => amount,
        });
    }

    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _: NotificationContext<RoleClient>,
    ) {
        let server = &self.server;
        let logger = params.logger.as_deref().unwrap_or("-");
        let data = &params.data;
        match params.level {
            LoggingLevel::Debug => tracing::debug!("mcp server {server} [{logger}]: {data}"),
            LoggingLevel::Info | LoggingLevel::Notice => {
                tracing::info!("mcp server {server} [{logger}]: {data}")
            }
            LoggingLevel::Warning => tracing::warn!("mcp server {server} [{logger}]: {data}"),
            _ => tracing::error!("mcp server {server} [{logger}]: {data}"),
        }
    }

    async fn on_tool_list_changed(&self, _: NotificationContext<RoleClient>) {
        self.reload();
    }

    async fn on_prompt_list_changed(&self, _: NotificationContext<RoleClient>) {
        self.reload();
    }
}

impl Handler {
    /// Fetches the server's tool and prompt lists again, off the notification path.
    fn reload(&self) {
        let (inner, server) = (self.inner.clone(), self.server.clone());
        tokio::spawn(async move {
            if let Some(inner) = inner.upgrade() {
                inner.reload(&server).await;
            }
        });
    }
}

/// Tells the user, through the UI bridge, what a server changed or needs.
pub(crate) fn notify(inner: &Inner, level: Level, text: &str) {
    if let Some(ui) = inner.services.get::<Ui>() {
        ui.0.notify(level, text);
    }
}
