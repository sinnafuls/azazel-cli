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
use crate::policy::{self, Grants};
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

/// A response the client reads like any other.
fn answer(response: Response) -> Message {
    Message::text(serde_json::to_string(&Frame::Response(response)).unwrap_or_default())
}

/// A command the server does not take from this device; the response says so with a `code` the
/// page can rely on.
fn refusal(id: Value, why: impl Into<String>) -> Message {
    answer(Response::refused(id, why))
}

/// A line that is not a command, or a command sent too fast.
fn failure(id: Value, why: impl Into<String>) -> Message {
    answer(Response::err(id, why))
}

/// A response to one of the `list_commands` in `listings` keeps only the commands a browser may
/// run, so that its menu offers nothing the policy would refuse. Any other line goes out as it is.
fn only_allowed(line: String, listings: &mut Vec<Value>, grants: Grants) -> String {
    if !line.contains("\"response\"") {
        return line;
    }
    let Ok(mut frame) = serde_json::from_str::<Value>(&line) else {
        return line;
    };
    if frame["type"] != "response" {
        return line;
    }
    let Some(at) = listings.iter().position(|id| *id == frame["id"]) else {
        return line;
    };
    listings.remove(at);
    if let Some(rows) = frame.get_mut("data").and_then(Value::as_array_mut) {
        rows.retain(|row| {
            row["name"]
                .as_str()
                .is_some_and(|name| policy::command_listed(name, row["source"].as_str(), grants))
        });
    }
    frame.to_string()
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
    // The ids of `list_commands` sent and not answered yet.
    let mut listings: Vec<Value> = Vec::new();
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
                let line = match listings.is_empty() {
                    true => line,
                    false => only_allowed(line, &mut listings, shared.grants),
                };
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
                    if socket.send(failure(Value::Null, "not JSON: a message is one command as a JSON object")).await.is_err() {
                        return;
                    }
                    continue;
                };
                let id = value.get("id").cloned().unwrap_or(Value::Null);
                // Only the policy's own refusals carry the `refused` code.
                let verdict = if !value.is_object() {
                    Err(failure(id.clone(), "a command is a JSON object"))
                } else if !bucket.take(since_epoch(shared.auth.now())) {
                    Err(failure(id.clone(), "too many commands at once: slow down"))
                } else {
                    policy::check(device.access, &value, shared.grants).map_err(|why| refusal(id.clone(), why))
                };
                let mut value = value;
                match verdict {
                    Ok(()) => {
                        // The session runs a command by name; what it resolves the name to must be
                        // what the policy approved.
                        policy::confine(&mut value, shared.grants);
                        // A listing is matched to its response by id, so one with no id, or with
                        // an id still waiting, is not tracked: it goes out as the session lists it.
                        if value["type"] == "list_commands"
                            && listings.len() < 16
                            && !id.is_null()
                            && !listings.contains(&id)
                        {
                            listings.push(id);
                        }
                        // One line: the session reads its commands line by line.
                        if conn.commands.send(value.to_string()).await.is_err() {
                            return close(&mut socket, 1000, "the session ended").await;
                        }
                    }
                    Err(message) => {
                        if socket.send(message).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}
