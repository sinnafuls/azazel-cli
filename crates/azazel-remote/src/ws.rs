//! One WebSocket: the session's lines go out as text messages, the client's messages go in as
//! lines, after the policy has seen them.

use std::sync::Arc;
use std::time::Duration;

use azazel_protocol::{Frame, Response};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::auth::Device;
use crate::limits::{Bucket, Connections, since_epoch};
use crate::policy;
use crate::server::Shared;

/// Frames that may wait for a client that reads slowly; a client that lets this many pile up is
/// cut off and resumes with `since`.
pub(crate) const QUEUE: usize = 4096;
/// How long one message may take to reach the client.
const SEND_LIMIT: Duration = Duration::from_secs(30);

async fn close<S: AsyncRead + AsyncWrite + Unpin>(socket: &mut WebSocketStream<S>, code: u16, reason: &str) {
    let reason: String = reason.chars().take(100).collect();
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: reason.into(),
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), socket.close(Some(frame))).await;
}

/// A refusal the client reads like any other response.
fn refusal(id: Value, why: impl Into<String>) -> Message {
    let line = serde_json::to_string(&Frame::Response(Response::err(id, why))).unwrap_or_default();
    Message::text(line)
}

pub(crate) async fn run<S>(
    shared: Arc<Shared>,
    mut socket: WebSocketStream<S>,
    device: Option<Device>,
    id: String,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Some(device) = device else {
        return close(&mut socket, 4401, "not signed in").await;
    };
    let Some(_place) = Connections::acquire(&shared.connections, &device.id) else {
        return close(&mut socket, 4409, "too many connections").await;
    };
    let mut conn = match shared.sessions.open(&id).await {
        Ok(conn) => conn,
        Err(why) => return close(&mut socket, 4404, &why).await,
    };
    let mut bucket = Bucket::new(20.0, 40.0, since_epoch(shared.auth.now()));
    let left = device
        .expires
        .duration_since(shared.auth.now())
        .unwrap_or_default();
    let expiry = tokio::time::sleep(left);
    tokio::pin!(expiry);
    loop {
        tokio::select! {
            _ = device.cancel.cancelled() => return close(&mut socket, 4401, "signed out").await,
            _ = &mut expiry => return close(&mut socket, 4401, "the device has expired").await,
            _ = shared.cancel.cancelled() => return close(&mut socket, 1001, "the server stopped").await,
            line = conn.frames.recv() => {
                let Some(line) = line else {
                    return close(&mut socket, 1000, "the session ended").await;
                };
                if conn.frames.len() >= QUEUE {
                    return close(&mut socket, 4408, "too slow: reconnect and attach with `since`").await;
                }
                match tokio::time::timeout(SEND_LIMIT, socket.send(Message::text(line))).await {
                    Ok(Ok(())) => {}
                    _ => return,
                }
            }
            message = socket.next() => {
                let message = match message {
                    None | Some(Ok(Message::Close(_))) => return,
                    Some(Err(tokio_tungstenite::tungstenite::Error::Capacity(_))) => {
                        return close(&mut socket, 1009, "the message is too large").await;
                    }
                    Some(Err(_)) => return,
                    Some(Ok(message)) => message,
                };
                let text = match message {
                    Message::Text(text) => text,
                    Message::Binary(_) => return close(&mut socket, 1003, "send text").await,
                    // Pings are answered by the socket itself.
                    _ => continue,
                };
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                    if socket.send(refusal(Value::Null, "not JSON: a message is one command as a JSON object")).await.is_err() {
                        return;
                    }
                    continue;
                };
                let id = value.get("id").cloned().unwrap_or(Value::Null);
                let verdict = if !value.is_object() {
                    Err("a command is a JSON object".to_string())
                } else if !bucket.take(since_epoch(shared.auth.now())) {
                    Err("too many commands at once: slow down".to_string())
                } else {
                    policy::check(device.access, &value, shared.allow_bypass)
                };
                match verdict {
                    Ok(()) => {
                        // One line: the session reads its commands line by line.
                        if conn.commands.send(value.to_string()).await.is_err() {
                            return close(&mut socket, 1000, "the session ended").await;
                        }
                    }
                    Err(why) => {
                        if socket.send(refusal(id, why)).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}
