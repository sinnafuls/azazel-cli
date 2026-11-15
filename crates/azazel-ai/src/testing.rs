//! A scripted provider for tests: replays canned responses and records every request.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::catalog::ModelInfo;
use crate::error::AiError;
use crate::provider::Provider;
use crate::stream::{EventStream, StreamEvent};
use crate::types::{AssistantMeta, Block, Message, Request, StopReason, Usage, WebSource, WireApi};

pub const MOCK_PROVIDER_ID: &str = "mock";
/// The queue that answers requests carrying hosted tools, whatever model they name. A script
/// with this queue makes the provider one that searches the web itself.
pub const MOCK_SEARCH_QUEUE: &str = "web_search";

/// One scripted response. Give either `events` (verbatim) or `reply` (expanded into events).
/// A step whose first event is `error` fails the call before anything is streamed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockStep {
    /// Wait before the first event.
    #[serde(default)]
    pub delay_ms: u64,
    /// Wait this long before each event after the first, so that a reply streams as it would
    /// from a model.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub event_ms: u64,
    /// The arguments of each tool call of `reply` arrive in pieces of this many characters
    /// rather than at once.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub chunk: usize,
    #[serde(default)]
    pub events: Vec<StreamEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<MockReply>,
}

/// Shorthand for a complete assistant message.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockReply {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub tool_calls: Vec<MockToolCall>,
    #[serde(default)]
    pub usage: Usage,
    /// What a hosted web search found, for a step of the [`MOCK_SEARCH_QUEUE`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<WebSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub input: Value,
}

impl MockStep {
    pub fn text(text: impl Into<String>) -> Self {
        MockStep {
            reply: Some(MockReply {
                text: text.into(),
                ..MockReply::default()
            }),
            ..MockStep::default()
        }
    }

    /// An assistant message with optional text and the given `(id, name, input)` tool calls.
    pub fn tools(text: impl Into<String>, calls: &[(&str, &str, Value)]) -> Self {
        let tool_calls = calls
            .iter()
            .map(|(id, name, input)| MockToolCall {
                id: id.to_string(),
                name: name.to_string(),
                input: input.clone(),
            })
            .collect();
        MockStep {
            reply: Some(MockReply {
                text: text.into(),
                tool_calls,
                ..MockReply::default()
            }),
            ..MockStep::default()
        }
    }

    /// The answer of a hosted web search: `text` with the pages it cites.
    pub fn search(text: impl Into<String>, sources: Vec<WebSource>) -> Self {
        MockStep {
            reply: Some(MockReply {
                text: text.into(),
                sources,
                ..MockReply::default()
            }),
            ..MockStep::default()
        }
    }

    /// The call itself fails with `error`.
    pub fn fail(error: AiError) -> Self {
        MockStep {
            events: vec![StreamEvent::Error { error, partial: None }],
            ..MockStep::default()
        }
    }

    pub fn delayed(mut self, delay: Duration) -> Self {
        self.delay_ms = delay.as_millis() as u64;
        self
    }

    pub fn with_usage(mut self, usage: Usage) -> Self {
        if let Some(reply) = &mut self.reply {
            reply.usage = usage;
        }
        self
    }

    fn into_events(self, model: &str) -> Vec<StreamEvent> {
        let Some(reply) = self.reply else {
            return self.events;
        };
        let mut events = vec![StreamEvent::Start];
        let mut content = Vec::new();
        if !reply.text.is_empty() {
            let index = content.len();
            events.push(StreamEvent::TextDelta {
                index,
                delta: reply.text.clone(),
            });
            let block = Block::text(reply.text);
            events.push(StreamEvent::BlockEnd {
                index,
                block: block.clone(),
            });
            content.push(block);
        }
        for call in reply.tool_calls {
            let index = content.len();
            let input = if call.input.is_null() {
                Value::Object(Default::default())
            } else {
                call.input
            };
            events.push(StreamEvent::ToolCallStart {
                index,
                id: call.id.clone(),
                name: call.name.clone(),
            });
            let args = input.to_string();
            let chars: Vec<char> = args.chars().collect();
            let size = if self.chunk == 0 {
                chars.len().max(1)
            } else {
                self.chunk
            };
            for piece in chars.chunks(size) {
                events.push(StreamEvent::ToolCallDelta {
                    index,
                    delta: piece.iter().collect(),
                });
            }
            let block = Block::ToolCall {
                id: call.id,
                name: call.name,
                input,
                signature: None,
            };
            events.push(StreamEvent::BlockEnd {
                index,
                block: block.clone(),
            });
            content.push(block);
        }
        let stop = if content.iter().any(|b| matches!(b, Block::ToolCall { .. })) {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
        let meta = AssistantMeta {
            provider: MOCK_PROVIDER_ID.to_string(),
            model: model.to_string(),
            api: WireApi::OpenAiChat,
            usage: reply.usage,
            stop,
            response_id: None,
            duration_ms: 0,
            sources: reply.sources,
        };
        events.push(StreamEvent::Done {
            message: Message::assistant(content, meta),
        });
        events
    }
}

fn is_zero<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// Script file: `{ "<model id>": [ <MockStep>, … ] }`.
pub type MockScript = BTreeMap<String, Vec<MockStep>>;

#[derive(Default)]
pub struct MockProvider {
    queues: Mutex<HashMap<String, VecDeque<MockStep>>>,
    requests: Mutex<Vec<(String, Request)>>,
    /// Every request is also appended here as one JSON line `{"model", "request"}`, so a test
    /// driving the binary can see what was sent.
    record: Option<std::path::PathBuf>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl MockProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_script(script: MockScript) -> Self {
        let provider = MockProvider::new();
        for (model, steps) in script {
            locked(&provider.queues).insert(model, steps.into());
        }
        provider
    }

    pub fn from_script_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let script: MockScript =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self::from_script(script))
    }

    /// Also writes every request to `file`, one JSON line each.
    pub fn recording_to(mut self, file: std::path::PathBuf) -> Self {
        self.record = Some(file);
        self
    }

    /// Appends a step to the queue of `mock/<model>`.
    pub fn push(&self, model: &str, step: MockStep) {
        locked(&self.queues)
            .entry(model.to_string())
            .or_default()
            .push_back(step);
    }

    /// Every request received so far as `(model id, request)`, in arrival order.
    pub fn requests(&self) -> Vec<(String, Request)> {
        locked(&self.requests).clone()
    }

    pub fn requests_for(&self, model: &str) -> Vec<Request> {
        locked(&self.requests)
            .iter()
            .filter(|(m, _)| m == model)
            .map(|(_, r)| r.clone())
            .collect()
    }

    /// Steps not yet consumed for `model`.
    pub fn remaining(&self, model: &str) -> usize {
        locked(&self.queues).get(model).map_or(0, VecDeque::len)
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &str {
        MOCK_PROVIDER_ID
    }

    fn hosts_web_search(&self) -> bool {
        locked(&self.queues).contains_key(MOCK_SEARCH_QUEUE)
    }

    async fn stream(
        &self,
        model: &ModelInfo,
        req: &Request,
        cancel: CancellationToken,
    ) -> Result<EventStream, AiError> {
        let mut requests = locked(&self.requests);
        requests.push((model.id.clone(), req.clone()));
        if let Some(file) = &self.record {
            use std::io::Write;
            // One write, under the lock: the lines of agents asking at the same time must not
            // run into each other.
            let line = format!("{}\n", serde_json::json!({ "model": model.id, "request": req }));
            let written = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(file)
                .and_then(|mut out| out.write_all(line.as_bytes()));
            if let Err(e) = written {
                tracing::warn!("could not record the mock request in {}: {e}", file.display());
            }
        }
        drop(requests);
        let queue = match req.hosted.is_empty() {
            true => model.id.as_str(),
            false => MOCK_SEARCH_QUEUE,
        };
        let step = locked(&self.queues).get_mut(queue).and_then(VecDeque::pop_front);
        let Some(step) = step else {
            return Err(AiError::InvalidRequest(format!(
                "mock script exhausted for {queue}"
            )));
        };
        let delay = Duration::from_millis(step.delay_ms);
        let pace = Duration::from_millis(step.event_ms);
        let mut events = step.into_events(&model.id);
        if let Some(StreamEvent::Error { error, .. }) = events.first() {
            return Err(error.clone());
        }
        let ends_properly = events.last().is_some_and(StreamEvent::is_terminal);
        if !ends_properly {
            events.push(StreamEvent::Error {
                error: AiError::Network("mock step has no terminal event".to_string()),
                partial: None,
            });
        }
        let aborted = || StreamEvent::Error {
            error: AiError::Aborted,
            partial: None,
        };
        let stream = futures::stream::once(async move {
            tokio::select! {
                _ = cancel.cancelled() => vec![aborted()],
                _ = tokio::time::sleep(delay) => events,
            }
        })
        .flat_map(futures::stream::iter);
        if pace.is_zero() {
            return Ok(Box::pin(stream));
        }
        let paced = stream.enumerate().then(move |(at, event)| async move {
            if at > 0 {
                tokio::time::sleep(pace).await;
            }
            event
        });
        Ok(Box::pin(paced))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, AiError> {
        let mut ids: Vec<String> = locked(&self.queues).keys().cloned().collect();
        ids.sort();
        Ok(ids
            .iter()
            .map(|id| ModelInfo::unknown(MOCK_PROVIDER_ID, id))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    async fn run(provider: &MockProvider, model: &str) -> Result<Vec<StreamEvent>, AiError> {
        let info = ModelInfo::unknown(MOCK_PROVIDER_ID, model);
        let stream = provider
            .stream(&info, &Request::default(), CancellationToken::new())
            .await?;
        Ok(stream.collect().await)
    }

    #[tokio::test]
    async fn script_file_steps_replay_per_model_then_exhaust() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.json");
        std::fs::write(
            &path,
            json!({
                "a": [
                    {"reply": {"text": "hi", "tool_calls": [{"id": "c1", "name": "read", "input": {"path": "x"}}]}},
                    {"events": [{"type": "error", "error": {"kind": "overloaded"}}]}
                ],
                "b": [{"delay_ms": 1, "events": [{"type": "start"}, {"type": "text_delta", "index": 0, "delta": "x"}]}]
            })
            .to_string(),
        )
        .unwrap();
        let provider = MockProvider::from_script_file(&path).unwrap();

        let events = run(&provider, "a").await.unwrap();
        let Some(StreamEvent::Done { message }) = events.last() else {
            panic!("{events:?}")
        };
        assert_eq!(message.text(), "hi");
        assert_eq!(
            message.tool_calls().collect::<Vec<_>>(),
            vec![("c1", "read", &json!({"path": "x"}))]
        );
        assert_eq!(message.meta.as_ref().map(|m| m.stop), Some(StopReason::ToolUse));

        assert_eq!(run(&provider, "a").await, Err(AiError::Overloaded));
        assert_eq!(
            run(&provider, "a").await,
            Err(AiError::InvalidRequest("mock script exhausted for a".to_string()))
        );

        // A verbatim step without a terminal event is closed with an error instead of hanging.
        let events = run(&provider, "b").await.unwrap();
        assert!(matches!(
            events.last(),
            Some(StreamEvent::Error {
                error: AiError::Network(_),
                ..
            })
        ));
        assert_eq!(provider.requests().len(), 4);
    }
}
