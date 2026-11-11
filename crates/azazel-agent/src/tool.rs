//! The tool abstraction and what a tool receives when it runs.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};

use async_trait::async_trait;
use azazel_ai::{Block, ToolSpec};
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::event::{AgentId, AgentInfo};

/// What a call touches; permission modes key off this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Read,
    Write,
    Exec,
    Net,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema of the input object.
    fn schema(&self) -> Value;
    fn access(&self, input: &Value) -> Access;
    /// The strings permission rules are matched against: command lines, paths, URLs.
    fn subjects(&self, input: &Value) -> Vec<String>;
    /// Whether this call may run at the same time as other calls of the same assistant message.
    fn concurrency_safe(&self, input: &Value) -> bool {
        matches!(self.access(input), Access::Read)
    }
    /// Something the user should know before approving this call (that it may cost money, for
    /// example); shown with the approval prompt.
    fn approval_note(&self, _input: &Value) -> Option<String> {
        None
    }
    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput;
}

pub fn tool_spec(tool: &dyn Tool) -> ToolSpec {
    ToolSpec {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        schema: tool.schema(),
    }
}

/// The tools an agent may use, asked again before every request so the set can change mid-session.
pub trait ToolSource: Send + Sync {
    fn tools(&self, agent: &AgentInfo) -> Vec<Arc<dyn Tool>>;
}

impl ToolSource for Vec<Arc<dyn Tool>> {
    fn tools(&self, _agent: &AgentInfo) -> Vec<Arc<dyn Tool>> {
        self.clone()
    }
}

#[derive(Clone)]
pub struct ToolCtx {
    pub agent: AgentInfo,
    pub agent_id: AgentId,
    pub session_id: String,
    pub call_id: String,
    pub cwd: PathBuf,
    /// Fires when the user aborts; long-running tools must return promptly once it does.
    pub cancel: CancellationToken,
    pub update: UpdateSink,
    pub services: Arc<Services>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ToolOutput {
    /// `Text` and `Image` blocks.
    pub content: Vec<Block>,
    pub is_error: bool,
    /// Structured data for the UI (diffs, exit codes); never sent to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        ToolOutput {
            content: vec![Block::text(text)],
            is_error: false,
            details: None,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        ToolOutput {
            content: vec![Block::text(text)],
            is_error: true,
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// The text blocks joined with newlines.
    pub fn text_content(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let Block::Text { text } = block {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }
}

/// Progress text from a running tool, shown live and never sent to the model.
#[derive(Clone, Default)]
pub struct UpdateSink {
    sink: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

impl UpdateSink {
    pub fn new(sink: impl Fn(String) + Send + Sync + 'static) -> Self {
        UpdateSink {
            sink: Some(Arc::new(sink)),
        }
    }

    pub fn send(&self, text: impl Into<String>) {
        if let Some(sink) = &self.sink {
            sink(text.into());
        }
    }
}

/// Stores tool output that is too large to return and gives back a URI the model can read.
pub trait ArtifactSink: Send + Sync {
    fn spill(&self, call_id: &str, text: &str) -> Option<String>;
}

/// Application services keyed by type, so tools can reach them without this crate knowing them.
/// Trait-object services are registered behind a concrete wrapper type.
#[derive(Default)]
pub struct Services {
    map: RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl Services {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces any service of the same type.
    pub fn insert<T: Send + Sync + 'static>(&self, service: Arc<T>) {
        self.map
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(TypeId::of::<T>(), service);
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        let service = self
            .map
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&TypeId::of::<T>())
            .cloned()?;
        service.downcast::<T>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counter(u32);
    struct Label(&'static str);

    #[test]
    fn services_are_keyed_by_type_and_replaceable() {
        let services = Services::new();
        assert!(services.get::<Counter>().is_none());
        services.insert(Arc::new(Counter(1)));
        services.insert(Arc::new(Label("x")));
        services.insert(Arc::new(Counter(2)));
        assert_eq!(services.get::<Counter>().map(|c| c.0), Some(2));
        assert_eq!(services.get::<Label>().map(|l| l.0), Some("x"));
    }

    #[test]
    fn text_content_joins_text_blocks_and_skips_images() {
        let output = ToolOutput {
            content: vec![
                Block::text("a"),
                Block::Image {
                    mime: "image/png".into(),
                    data: String::new(),
                },
                Block::text("b"),
            ],
            is_error: false,
            details: None,
        };
        assert_eq!(output.text_content(), "a\nb");
    }
}
