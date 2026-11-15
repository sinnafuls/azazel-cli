//! One debug adapter process: Debug Adapter Protocol messages with `Content-Length` framing over its
//! standard streams or, for adapters that only listen, a TCP socket.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;

/// How long a request may go unanswered.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// How long an adapter that listens on a socket gets to say where.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Characters kept of the adapter's last line on standard error.
const MAX_LAST_WORDS: usize = 300;
/// Bytes of program output kept unread; the oldest goes first.
const MAX_OUTPUT: usize = 64 * 1024;

type Reply = oneshot::Sender<Result<Value, String>>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// How a debug adapter is started.
#[derive(Debug, Clone)]
pub(crate) struct Adapter {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The adapter listens on a socket and says where on standard output
    /// (`DAP server listening at: 127.0.0.1:4711`) instead of talking on its standard streams.
    pub listens: bool,
}

/// Why the program stopped.
#[derive(Debug, Clone, Default)]
pub(crate) struct Stop {
    pub thread: Option<i64>,
    pub reason: String,
    pub description: String,
}

#[derive(Default)]
struct State {
    initialized: bool,
    /// How many `stopped` events have arrived.
    stops: u64,
    stopped: Option<Stop>,
    terminated: bool,
    exit_code: Option<i64>,
    /// What the program printed, the end of it when it printed more than [`MAX_OUTPUT`].
    output: String,
    /// How much of `output` a result has already shown.
    seen: usize,
    output_cut: bool,
}

#[derive(Default)]
struct Shared {
    pending: Mutex<HashMap<i64, Reply>>,
    state: Mutex<State>,
    changed: Notify,
    dead: AtomicBool,
    /// The last line the adapter wrote to its standard error.
    last_words: Mutex<String>,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a wait for the program ended with.
pub(crate) enum Wait {
    Stopped(Stop),
    /// The program ended (or the adapter went); its exit code when it said.
    Terminated(Option<i64>),
    TimedOut,
    Cancelled,
}

pub(crate) struct Dap {
    writer: Arc<tokio::sync::Mutex<Writer>>,
    shared: Arc<Shared>,
    next: AtomicI64,
    child: tokio::sync::Mutex<Child>,
}

async fn write_frame(writer: &tokio::sync::Mutex<Writer>, message: &Value) -> std::io::Result<()> {
    let body = message.to_string();
    let mut writer = writer.lock().await;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
        .await?;
    writer.flush().await
}

/// The next framed message, or `None` when the stream has ended or is not the protocol.
async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; length?];
    reader.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}

/// Keeps the last line the adapter wrote, which says why it exited when it does.
async fn listen(stream: impl AsyncRead + Unpin, shared: Arc<Shared>) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if !line.is_empty() {
            *locked(&shared.last_words) = line.chars().take(MAX_LAST_WORDS).collect();
        }
    }
}

fn error_text(message: &Value) -> String {
    let said = message
        .get("message")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty());
    let detail = message
        .pointer("/body/error/format")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty());
    detail
        .or(said)
        .unwrap_or("the debug adapter reported an error")
        .to_string()
}

fn on_event(shared: &Shared, event: &str, body: &Value) {
    let mut state = locked(&shared.state);
    match event {
        "initialized" => state.initialized = true,
        "stopped" => {
            let text = |key: &str| {
                body.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let description = if text("description").is_empty() {
                text("text")
            } else {
                text("description")
            };
            state.stopped = Some(Stop {
                thread: body.get("threadId").and_then(Value::as_i64),
                reason: text("reason"),
                description,
            });
            state.stops += 1;
        }
        // `continued` is ignored: it can arrive after the next stop, and the client forgets a
        // stop itself when it resumes the program.
        "output" => {
            let category = body.get("category").and_then(Value::as_str).unwrap_or("console");
            if matches!(category, "stdout" | "stderr")
                && let Some(text) = body.get("output").and_then(Value::as_str)
            {
                state.output.push_str(text);
                if state.output.len() > MAX_OUTPUT {
                    let mut at = state.output.len() - MAX_OUTPUT;
                    while !state.output.is_char_boundary(at) {
                        at += 1;
                    }
                    state.output.drain(..at);
                    state.seen = state.seen.saturating_sub(at);
                    state.output_cut = true;
                }
            }
        }
        "exited" => state.exit_code = body.get("exitCode").and_then(Value::as_i64),
        "terminated" => state.terminated = true,
        _ => return,
    }
    drop(state);
    shared.changed.notify_waiters();
}

async fn read_loop<R: AsyncBufRead + Unpin>(
    mut reader: R,
    writer: Arc<tokio::sync::Mutex<Writer>>,
    shared: Arc<Shared>,
    complaints: tokio::task::JoinHandle<()>,
) {
    let mut seq = 1_000_000_i64;
    while let Some(message) = read_frame(&mut reader).await {
        match message.get("type").and_then(Value::as_str) {
            Some("response") => {
                let Some(reply) = message
                    .get("request_seq")
                    .and_then(Value::as_i64)
                    .and_then(|id| locked(&shared.pending).remove(&id))
                else {
                    continue;
                };
                let outcome = if message.get("success").and_then(Value::as_bool) == Some(true) {
                    Ok(message.get("body").cloned().unwrap_or(Value::Null))
                } else {
                    Err(error_text(&message))
                };
                let _ = reply.send(outcome);
            }
            Some("event") => {
                let event = message.get("event").and_then(Value::as_str).unwrap_or_default();
                on_event(&shared, event, message.get("body").unwrap_or(&Value::Null));
            }
            // `runInTerminal` and `startDebugging`: this client offers neither.
            Some("request") => {
                seq += 1;
                let command = message.get("command").and_then(Value::as_str).unwrap_or_default();
                let reply = json!({
                    "seq": seq,
                    "type": "response",
                    "request_seq": message.get("seq").cloned().unwrap_or(Value::Null),
                    "success": false,
                    "command": command,
                    "message": format!("{command} is not supported"),
                });
                if write_frame(&writer, &reply).await.is_err() {
                    break;
                }
            }
            _ => {}
        }
    }
    // The reason usually arrives on standard error just before the process goes.
    let _ = tokio::time::timeout(Duration::from_millis(500), complaints).await;
    shared.dead.store(true, Ordering::SeqCst);
    // Dropping the senders fails every request still waiting.
    locked(&shared.pending).clear();
    shared.changed.notify_waiters();
}

/// The address a listening adapter announced, from a line such as
/// `DAP server listening at: 127.0.0.1:4711`.
fn announced_address(line: &str) -> Option<String> {
    if let Some((_, rest)) = line.split_once("listening at:") {
        let address = rest.trim();
        return (!address.is_empty()).then(|| address.to_string());
    }
    // CodeLLDB: `Listening on port 4711`.
    let (_, rest) = line.split_once("Listening on port ")?;
    let port: u16 = rest.trim().parse().ok()?;
    Some(format!("127.0.0.1:{port}"))
}

impl Dap {
    /// Starts the adapter in `cwd`; the `initialize` handshake is the caller's.
    pub async fn start(adapter: &Adapter, cwd: &std::path::Path) -> Result<Dap, String> {
        let mut command = Command::new(&adapter.program);
        command
            .args(&adapter.args)
            .current_dir(cwd)
            .stdin(if adapter.listens {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", adapter.program.display()))?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(format!(
                "could not reach the standard streams of {}",
                adapter.program.display()
            ));
        };
        let shared = Arc::new(Shared::default());
        let complaints = tokio::spawn(listen(stderr, shared.clone()));
        let (reader, writer): (Box<dyn AsyncBufRead + Send + Unpin>, Writer) = if adapter.listens {
            let mut lines = BufReader::new(stdout).lines();
            let address = tokio::time::timeout(LISTEN_TIMEOUT, async {
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(address) = announced_address(&line) {
                        return Some(address);
                    }
                }
                None
            })
            .await
            .ok()
            .flatten();
            let Some(address) = address else {
                let why = locked(&shared.last_words).clone();
                return Err(format!(
                    "{} did not say where it listens{}",
                    adapter.program.display(),
                    if why.is_empty() {
                        String::new()
                    } else {
                        format!(": {why}")
                    }
                ));
            };
            // Whatever it prints from now on must not fill the pipe.
            tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
            let socket = TcpStream::connect(&address)
                .await
                .map_err(|e| format!("could not connect to the debug adapter at {address}: {e}"))?;
            let (read, write) = socket.into_split();
            (Box::new(BufReader::new(read)), Box::new(write))
        } else {
            let Some(stdin) = child.stdin.take() else {
                return Err(format!(
                    "could not reach the standard streams of {}",
                    adapter.program.display()
                ));
            };
            (Box::new(BufReader::new(stdout)), Box::new(stdin))
        };
        let writer = Arc::new(tokio::sync::Mutex::new(writer));
        tokio::spawn(read_loop(reader, writer.clone(), shared.clone(), complaints));
        Ok(Dap {
            writer,
            shared,
            next: AtomicI64::new(1),
            child: tokio::sync::Mutex::new(child),
        })
    }

    /// Whether the adapter process has gone.
    pub fn dead(&self) -> bool {
        self.shared.dead.load(Ordering::SeqCst)
    }

    /// Why a request could not be made: the adapter exited, with what it last said if anything.
    pub fn gone(&self) -> String {
        let last_words = locked(&self.shared.last_words);
        if last_words.is_empty() {
            "the debug adapter exited".to_string()
        } else {
            format!("the debug adapter exited: {last_words}")
        }
    }

    /// Sends a request and returns where its answer will arrive.
    pub async fn send(
        &self,
        command: &str,
        arguments: Value,
    ) -> Result<oneshot::Receiver<Result<Value, String>>, String> {
        if self.dead() {
            return Err(self.gone());
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        locked(&self.shared.pending).insert(id, reply);
        let message = json!({ "seq": id, "type": "request", "command": command, "arguments": arguments });
        if write_frame(&self.writer, &message).await.is_err() {
            locked(&self.shared.pending).remove(&id);
            return Err(self.gone());
        }
        Ok(answer)
    }

    pub async fn request(&self, command: &str, arguments: Value) -> Result<Value, String> {
        self.request_within(command, arguments, REQUEST_TIMEOUT).await
    }

    pub async fn request_within(
        &self,
        command: &str,
        arguments: Value,
        limit: Duration,
    ) -> Result<Value, String> {
        let answer = self.send(command, arguments).await?;
        match tokio::time::timeout(limit, answer).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(self.gone()),
            Err(_) => Err(format!(
                "the debug adapter did not answer {command} within {} s",
                limit.as_secs()
            )),
        }
    }

    /// Waits for the `initialized` event; false when it does not come in time or the adapter goes.
    pub async fn wait_initialized(&self, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let waiter = self.shared.changed.notified();
            if locked(&self.shared.state).initialized {
                return true;
            }
            if self.dead() || tokio::time::timeout_at(deadline, waiter).await.is_err() {
                return false;
            }
        }
    }

    /// How many `stopped` events have arrived; pass it to [`Dap::wait_stop`] before resuming.
    pub fn stops(&self) -> u64 {
        locked(&self.shared.state).stops
    }

    /// Forgets the stop: the program is about to run.
    pub fn resumed(&self) {
        locked(&self.shared.state).stopped = None;
    }

    /// The stop the program is at, if it is stopped.
    pub fn stopped(&self) -> Option<Stop> {
        locked(&self.shared.state).stopped.clone()
    }

    /// The exit code and whether the program has ended (the adapter going counts).
    pub fn ended(&self) -> Option<Option<i64>> {
        let state = locked(&self.shared.state);
        (state.terminated || self.dead()).then_some(state.exit_code)
    }

    /// Waits until the program stops after the `before`-th stop, ends, `limit` passes or `cancel`
    /// fires.
    pub async fn wait_stop(&self, before: u64, limit: Duration, cancel: &CancellationToken) -> Wait {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let waiter = self.shared.changed.notified();
            {
                let state = locked(&self.shared.state);
                if state.stops > before
                    && let Some(stop) = &state.stopped
                {
                    return Wait::Stopped(stop.clone());
                }
                if state.terminated || self.dead() {
                    return Wait::Terminated(state.exit_code);
                }
            }
            tokio::select! {
                _ = waiter => {}
                _ = tokio::time::sleep_until(deadline) => return Wait::TimedOut,
                _ = cancel.cancelled() => return Wait::Cancelled,
            }
        }
    }

    /// What the program printed since a result last showed output.
    pub fn new_output(&self) -> String {
        let mut state = locked(&self.shared.state);
        let new = state.output[state.seen..].to_string();
        state.seen = state.output.len();
        new
    }

    /// Everything the program printed that is kept, and whether older output was dropped.
    pub fn all_output(&self) -> (String, bool) {
        let mut state = locked(&self.shared.state);
        state.seen = state.output.len();
        (state.output.clone(), state.output_cut)
    }

    /// Asks the adapter to end the program and itself, then makes sure the process is gone.
    pub async fn shutdown(&self) {
        if !self.dead() {
            let _ = self
                .request_within(
                    "disconnect",
                    json!({ "terminateDebuggee": true }),
                    Duration::from_secs(3),
                )
                .await;
        }
        let mut child = self.child.lock().await;
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
            let _ = child.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listening_adapter_is_found_by_the_line_it_prints() {
        assert_eq!(
            announced_address("DAP server listening at: 127.0.0.1:4711").as_deref(),
            Some("127.0.0.1:4711")
        );
        assert_eq!(
            announced_address("DAP server listening at: [::1]:4711").as_deref(),
            Some("[::1]:4711")
        );
        assert_eq!(
            announced_address("Listening on port 4711").as_deref(),
            Some("127.0.0.1:4711")
        );
        assert_eq!(announced_address("starting"), None);
    }

    #[test]
    fn output_keeps_its_end_when_it_grows_too_large() {
        let shared = Shared::default();
        let chunk = "é".repeat(MAX_OUTPUT / 2);
        for _ in 0..3 {
            on_event(
                &shared,
                "output",
                &json!({ "category": "stdout", "output": chunk }),
            );
        }
        on_event(
            &shared,
            "output",
            &json!({ "category": "console", "output": "ignored" }),
        );
        let state = locked(&shared.state);
        assert!(state.output.len() <= MAX_OUTPUT && state.output.ends_with('é'));
        assert!(state.output_cut && !state.output.contains("ignored"));
    }
}
