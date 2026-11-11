//! One Chrome DevTools Protocol connection: commands with their replies, and the events the
//! browser sends on its own.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

pub const CLOSED: &str = "the browser closed the connection";

/// How long a command may go unanswered.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq)]
pub struct CdpEvent {
    /// The flat session (one per attached tab) the event belongs to; `None` for the browser's own.
    pub session: Option<String>,
    pub method: String,
    pub params: Value,
}

type Reply = oneshot::Sender<Result<Value, String>>;

/// `None` once the connection has ended: later commands fail at once.
type Pending = Arc<Mutex<Option<HashMap<u64, Reply>>>>;

pub struct Cdp {
    next: AtomicU64,
    out: mpsc::UnboundedSender<Message>,
    pending: Pending,
    events: broadcast::Sender<Arc<CdpEvent>>,
}

/// What an incoming frame is: the answer to command `id`, or an event.
#[derive(Debug, PartialEq)]
enum Incoming {
    Reply(u64, Result<Value, String>),
    Event(CdpEvent),
}

fn parse(text: &str) -> Option<Incoming> {
    let mut frame: Value = serde_json::from_str(text).ok()?;
    if let Some(id) = frame.get("id").and_then(Value::as_u64) {
        let result = match frame.get("error") {
            Some(error) => Err(error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the browser refused the command")
                .to_string()),
            None => Ok(frame.get_mut("result").map(Value::take).unwrap_or(Value::Null)),
        };
        return Some(Incoming::Reply(id, result));
    }
    Some(Incoming::Event(CdpEvent {
        session: frame.get("sessionId").and_then(Value::as_str).map(str::to_string),
        method: frame.get("method")?.as_str()?.to_string(),
        params: frame.get_mut("params").map(Value::take).unwrap_or(Value::Null),
    }))
}

impl Cdp {
    /// Connects to the browser's own WebSocket endpoint (`ws://127.0.0.1:<port>/devtools/browser/…`).
    pub async fn connect(url: &str) -> Result<Cdp, String> {
        // Screenshots arrive as one large frame.
        let config = WebSocketConfig::default()
            .max_message_size(None)
            .max_frame_size(None);
        let (stream, _) = tokio_tungstenite::connect_async_with_config(url, Some(config), true)
            .await
            .map_err(|e| format!("could not connect to the browser at {url}: {e}"))?;
        let (mut sink, mut source) = stream.split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<Message>();
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (events, _) = broadcast::channel(4096);

        tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let (waiting, sender) = (pending.clone(), events.clone());
        tokio::spawn(async move {
            while let Some(Ok(message)) = source.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                match parse(text.as_str()) {
                    Some(Incoming::Reply(id, result)) => {
                        let reply = waiting
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .as_mut()
                            .and_then(|map| map.remove(&id));
                        if let Some(reply) = reply {
                            let _ = reply.send(result);
                        }
                    }
                    Some(Incoming::Event(event)) => {
                        let _ = sender.send(Arc::new(event));
                    }
                    None => tracing::debug!("unreadable frame from the browser"),
                }
            }
            // Dropping the senders fails every command still waiting.
            waiting.lock().unwrap_or_else(PoisonError::into_inner).take();
        });

        Ok(Cdp {
            next: AtomicU64::new(1),
            out,
            pending,
            events,
        })
    }

    /// Whether the connection has ended (the browser exited or was closed by the user).
    pub fn is_closed(&self) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none()
    }

    /// Sends one command, to the tab attached as `session` or to the browser itself, and returns
    /// its `result`.
    pub async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        match self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            Some(map) => map.insert(id, reply),
            None => return Err(CLOSED.to_string()),
        };
        let mut frame = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            frame["sessionId"] = Value::String(session.to_string());
        }
        if self.out.send(Message::text(frame.to_string())).is_err() {
            return Err(CLOSED.to_string());
        }
        match tokio::time::timeout(CALL_TIMEOUT, answer).await {
            Ok(Ok(result)) => result.map_err(|e| format!("{method}: {e}")),
            Ok(Err(_)) => Err(CLOSED.to_string()),
            Err(_) => {
                if let Some(map) = self
                    .pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_mut()
                {
                    map.remove(&id);
                }
                Err(format!(
                    "{method}: no answer from the browser within {} s",
                    CALL_TIMEOUT.as_secs()
                ))
            }
        }
    }

    /// Events from now on, of every session.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<CdpEvent>> {
        self.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn frames_are_told_apart_by_their_id() {
        assert_eq!(
            parse(r#"{"id":7,"result":{"frameId":"F"},"sessionId":"S"}"#),
            Some(Incoming::Reply(7, Ok(json!({ "frameId": "F" }))))
        );
        assert_eq!(
            parse(r#"{"id":8,"error":{"code":-32000,"message":"No node with given id found"}}"#),
            Some(Incoming::Reply(8, Err("No node with given id found".into())))
        );
        assert_eq!(
            parse(r#"{"method":"Page.loadEventFired","params":{"timestamp":1.5},"sessionId":"S"}"#),
            Some(Incoming::Event(CdpEvent {
                session: Some("S".into()),
                method: "Page.loadEventFired".into(),
                params: json!({ "timestamp": 1.5 }),
            }))
        );
        assert_eq!(
            parse(r#"{"method":"Target.targetCreated"}"#),
            Some(Incoming::Event(CdpEvent {
                session: None,
                method: "Target.targetCreated".into(),
                params: Value::Null,
            }))
        );
        assert_eq!(parse("not json"), None);
        assert_eq!(parse(r#"{"neither":true}"#), None);
    }
}
