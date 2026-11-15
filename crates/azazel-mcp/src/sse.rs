//! Client transport for the legacy MCP "HTTP+SSE" protocol (specification revision 2024-11-05).
//!
//! The client opens one long-lived event stream with a GET. The server's first event, `endpoint`,
//! names the URL that accepts JSON-RPC messages by POST; every message from the server arrives on the
//! stream as a `message` event.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use azazel_ai::sse::{SseParser, SseRecord};
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, WWW_AUTHENTICATE,
};
use reqwest::{StatusCode, Url};
use rmcp::RoleClient;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Server messages held between the stream reader and [`Transport::receive`] before the reader waits.
const INCOMING_BUFFER: usize = 64;

/// Why a legacy SSE connection could not be opened or used.
#[derive(Debug, thiserror::Error)]
pub enum SseError {
    /// The server answered 401; `www_authenticate` is that header's value when present.
    #[error("the server requires authorization")]
    Unauthorized { www_authenticate: Option<String> },
    /// The server answered with a status that is neither 2xx nor 401.
    #[error("the server answered HTTP {0}")]
    Status(u16),
    /// The request could not be built, sent or read.
    #[error("{0}")]
    Http(String),
    /// The stream did not follow the protocol (no `endpoint` event, bad endpoint URL, ...).
    #[error("{0}")]
    Protocol(String),
    /// The event stream has ended, so the server no longer has a session to post to.
    #[error("the event stream is closed")]
    Closed,
}

/// One legacy SSE session: the open event stream and the URL its messages are posted to.
///
/// Dropping the transport closes the event stream.
pub struct LegacySseTransport {
    client: reqwest::Client,
    endpoint: Url,
    /// The caller's headers plus `Content-Type: application/json`, sent with every POST.
    post_headers: HeaderMap,
    incoming: mpsc::Receiver<RxJsonRpcMessage<RoleClient>>,
    reader: ReaderTask,
}

impl fmt::Debug for LegacySseTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LegacySseTransport")
            .field("endpoint", &self.endpoint.as_str())
            .finish_non_exhaustive()
    }
}

/// Opens the event stream at `url` and waits (at most `timeout`) for the `endpoint` event.
///
/// `headers` are sent with the GET and with every later POST. `client` must not have a total request
/// timeout configured, because that timeout would also end the event stream.
pub async fn connect(
    url: &str,
    client: reqwest::Client,
    headers: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<LegacySseTransport, SseError> {
    let url = Url::parse(url).map_err(|error| SseError::Http(format!("invalid server URL: {error}")))?;
    let headers = header_map(headers)?;
    let started = tokio::time::Instant::now();

    let mut get_headers = headers.clone();
    get_headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    let response = tokio::time::timeout(timeout, client.get(url).headers(get_headers).send())
        .await
        .map_err(|_| SseError::Http(format!("the server did not answer within {timeout:?}")))?
        .map_err(http_error)?;
    check_status(&response)?;
    if let Some(content_type) = response.headers().get(CONTENT_TYPE)
        && !is_event_stream(content_type)
    {
        return Err(SseError::Protocol(format!(
            "the server answered with content type `{}` instead of `text/event-stream`",
            String::from_utf8_lossy(content_type.as_bytes())
        )));
    }

    let (endpoint_tx, endpoint_rx) = oneshot::channel();
    let (messages, incoming) = mpsc::channel(INCOMING_BUFFER);
    // Relative endpoints resolve against the URL that served the stream, which differs from `url`
    // after a redirect.
    let reader = Reader {
        base: response.url().clone(),
        endpoint: Some(endpoint_tx),
        messages,
    };
    // From here on every early return drops `reader`, which stops the task.
    let reader = ReaderTask(tokio::spawn(reader.run(response)));

    let remaining = timeout.saturating_sub(started.elapsed());
    let endpoint = tokio::time::timeout(remaining, endpoint_rx)
        .await
        .map_err(|_| SseError::Protocol(format!("no `endpoint` event arrived within {timeout:?}")))?
        .map_err(|_| SseError::Closed)??;

    let mut post_headers = headers;
    post_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(LegacySseTransport {
        client,
        endpoint,
        post_headers,
        incoming,
        reader,
    })
}

impl Transport<RoleClient> for LegacySseTransport {
    type Error = SseError;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let closed = self.reader.0.is_finished();
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let headers = self.post_headers.clone();
        async move {
            if closed {
                return Err(SseError::Closed);
            }
            let body = serde_json::to_vec(&item)
                .map_err(|error| SseError::Protocol(format!("cannot encode the message: {error}")))?;
            let response = client
                .post(endpoint)
                .headers(headers)
                .body(body)
                .send()
                .await
                .map_err(http_error)?;
            check_status(&response)
        }
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.incoming.recv()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.incoming.close();
        self.reader.stop().await;
        Ok(())
    }
}

/// The task reading the event stream; it is aborted when this handle is dropped.
struct ReaderTask(JoinHandle<()>);

impl ReaderTask {
    /// Aborts the task and waits until it is gone, which also closes the HTTP connection.
    async fn stop(&mut self) {
        // A finished handle is never awaited, so its output cannot be taken twice.
        if !self.0.is_finished() {
            self.0.abort();
            let _ = (&mut self.0).await;
        }
    }
}

impl Drop for ReaderTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Turns the event stream into the endpoint URL (once) and a sequence of server messages.
struct Reader {
    base: Url,
    endpoint: Option<oneshot::Sender<Result<Url, SseError>>>,
    messages: mpsc::Sender<RxJsonRpcMessage<RoleClient>>,
}

impl Reader {
    async fn run(mut self, mut response: reqwest::Response) {
        let mut parser = SseParser::new();
        let failure = loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    for record in parser.push(&chunk) {
                        if !self.dispatch(record).await {
                            return;
                        }
                    }
                }
                Ok(None) => {
                    if let Some(record) = parser.finish()
                        && !self.dispatch(record).await
                    {
                        return;
                    }
                    break None;
                }
                Err(error) => break Some(http_error(error)),
            }
        };
        match (self.endpoint.take(), failure) {
            (Some(reply), failure) => {
                let error = failure.unwrap_or_else(|| {
                    SseError::Protocol("the event stream ended before the `endpoint` event".to_string())
                });
                let _ = reply.send(Err(error));
            }
            (None, Some(error)) => tracing::warn!(%error, "the MCP event stream failed"),
            (None, None) => tracing::debug!("the server closed the MCP event stream"),
        }
    }

    /// Handles one event; `false` means nobody is listening any more and reading should stop.
    async fn dispatch(&mut self, record: SseRecord) -> bool {
        match record.event.as_deref() {
            Some("endpoint") => {
                let Some(reply) = self.endpoint.take() else {
                    tracing::debug!("ignoring a repeated `endpoint` event");
                    return true;
                };
                let endpoint = resolve_endpoint(&self.base, &record.data);
                let usable = endpoint.is_ok();
                reply.send(endpoint).is_ok() && usable
            }
            // An event without a name, or with an empty one, is a `message` event.
            None | Some("" | "message") => {
                match serde_json::from_str::<RxJsonRpcMessage<RoleClient>>(&record.data) {
                    Ok(message) => self.messages.send(message).await.is_ok(),
                    Err(error) => {
                        tracing::warn!(%error, "skipping an undecodable message on the MCP event stream");
                        true
                    }
                }
            }
            Some(event) => {
                tracing::trace!(event, "ignoring an unknown event on the MCP event stream");
                true
            }
        }
    }
}

/// Resolves the `endpoint` event's data against the stream URL.
///
/// The endpoint must share the stream's origin: it receives the caller's headers, credentials included.
fn resolve_endpoint(base: &Url, data: &str) -> Result<Url, SseError> {
    let data = data.trim();
    if data.is_empty() {
        return Err(SseError::Protocol(
            "the `endpoint` event carries no URL".to_string(),
        ));
    }
    let endpoint = base
        .join(data)
        .map_err(|error| SseError::Protocol(format!("the `endpoint` event holds an invalid URL: {error}")))?;
    if endpoint.origin() != base.origin() {
        return Err(SseError::Protocol(format!(
            "the `endpoint` event points at another origin ({})",
            endpoint.origin().ascii_serialization()
        )));
    }
    Ok(endpoint)
}

/// Converts the configured headers, naming the offending header (never its value) on failure.
fn header_map(headers: &BTreeMap<String, String>) -> Result<HeaderMap, SseError> {
    let mut map = HeaderMap::with_capacity(headers.len() + 1);
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| SseError::Http(format!("invalid header name `{name}`")))?;
        let mut value = HeaderValue::from_str(value)
            .map_err(|_| SseError::Http(format!("invalid value for header `{name}`")))?;
        if name == AUTHORIZATION {
            value.set_sensitive(true);
        }
        map.insert(name, value);
    }
    Ok(map)
}

fn is_event_stream(content_type: &HeaderValue) -> bool {
    content_type
        .to_str()
        .ok()
        .and_then(|value| value.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
}

/// Maps a response status to the transport's error; any 2xx is success.
fn check_status(response: &reqwest::Response) -> Result<(), SseError> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    if status != StatusCode::UNAUTHORIZED {
        return Err(SseError::Status(status.as_u16()));
    }
    let challenges: Vec<&str> = response
        .headers()
        .get_all(WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    let www_authenticate = if challenges.is_empty() {
        None
    } else {
        Some(challenges.join(", "))
    };
    Err(SseError::Unauthorized { www_authenticate })
}

/// Describes a request failure with its whole cause chain and without the URL, whose query string
/// may hold a credential.
fn http_error(error: reqwest::Error) -> SseError {
    let error = error.without_url();
    let mut text = error.to_string();
    let mut cause = std::error::Error::source(&error);
    while let Some(inner) = cause {
        text.push_str(": ");
        text.push_str(&inner.to_string());
        cause = inner.source();
    }
    SseError::Http(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://mcp.example.com/v1/sse?token=abc").unwrap()
    }

    #[test]
    fn relative_endpoints_resolve_against_the_stream_url() {
        let absolute_path = resolve_endpoint(&base(), "/messages?sessionId=t1").unwrap();
        assert_eq!(
            absolute_path.as_str(),
            "https://mcp.example.com/messages?sessionId=t1"
        );
        let sibling = resolve_endpoint(&base(), " messages?sessionId=t1\n").unwrap();
        assert_eq!(
            sibling.as_str(),
            "https://mcp.example.com/v1/messages?sessionId=t1"
        );
    }

    #[test]
    fn absolute_endpoints_must_stay_on_the_stream_origin() {
        let same = resolve_endpoint(&base(), "https://mcp.example.com/post").unwrap();
        assert_eq!(same.as_str(), "https://mcp.example.com/post");
        let foreign = [
            "https://evil.example.net/post",
            "http://mcp.example.com/post",
            "https://mcp.example.com:8443/post",
        ];
        for endpoint in foreign {
            assert!(
                matches!(resolve_endpoint(&base(), endpoint), Err(SseError::Protocol(_))),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn invalid_headers_are_reported_by_name_only() {
        let headers = BTreeMap::from([("X-Token".to_string(), "line\nbreak".to_string())]);
        let Err(SseError::Http(message)) = header_map(&headers) else {
            panic!("a header value with a line break must be rejected");
        };
        assert!(message.contains("x-token"), "{message}");
        assert!(!message.contains("break"), "{message}");

        let headers = BTreeMap::from([("bad name".to_string(), "v".to_string())]);
        assert!(matches!(header_map(&headers), Err(SseError::Http(_))));
    }
}
