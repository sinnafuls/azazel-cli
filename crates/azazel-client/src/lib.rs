//! A client for `azazel rpc`: start azazel (or connect to one that is running), send it
//! [`Command`]s and read its [`Frame`]s.
//!
//! ```no_run
//! use azazel_client::{Client, Options};
//!
//! # async fn demo() -> Result<(), azazel_client::Error> {
//! let client = Client::spawn(Options::new("azazel")).await?;
//! let run = client.prompt_and_wait("what does this project do?", |_| None).await?;
//! println!("{}", run.text);
//! client.shutdown().await?;
//! # Ok(())
//! # }
//! ```

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

pub use azazel_protocol as protocol;
use azazel_protocol::{Command, Frame, PROTOCOL, Request, Response};
use futures::Stream;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Frames kept for the first subscriber while nobody listens.
const BACKLOG: usize = 10_000;
/// The id of the main agent.
const LEAD: &str = "lead";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not start azazel: {0}")]
    Spawn(std::io::Error),
    #[error("the connection to azazel failed: {0}")]
    Io(#[from] std::io::Error),
    /// The other end does not speak the protocol this client was built for.
    #[error("{0}")]
    Protocol(String),
    /// azazel went away before it answered.
    #[error("azazel closed the connection")]
    Closed,
    /// azazel refused the command; the text is its `error`.
    #[error("{0}")]
    Rejected(String),
    /// The run a prompt started failed; the text is the agent's error.
    #[error("the run failed: {0}")]
    Failed(String),
}

/// How to start azazel.
#[derive(Debug, Clone)]
pub struct Options {
    /// The `azazel` program: a path, or a name found on `PATH`.
    pub binary: PathBuf,
    /// The directory to work in; the client's own when left out.
    pub cwd: Option<PathBuf>,
    /// Arguments that go before `rpc`: `--model`, `--mode`, `--session`, `--trust`, ...
    pub args: Vec<String>,
    /// Environment variables added to the client's own, e.g. `AZAZEL_HOME`.
    pub env: Vec<(OsString, OsString)>,
}

impl Options {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Options {
            binary: binary.into(),
            cwd: None,
            args: Vec::new(),
            env: Vec::new(),
        }
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Hands every frame to every subscriber; while there is none, frames wait for the first.
#[derive(Default)]
struct Hub {
    subscribers: Vec<mpsc::UnboundedSender<Frame>>,
    backlog: VecDeque<Frame>,
}

impl Hub {
    fn publish(&mut self, frame: Frame) {
        self.subscribers.retain(|subscriber| !subscriber.is_closed());
        match self.subscribers.as_slice() {
            [] => {
                if self.backlog.len() == BACKLOG {
                    self.backlog.pop_front();
                }
                self.backlog.push_back(frame);
            }
            [one] => {
                let _ = one.send(frame);
            }
            many => {
                for subscriber in many {
                    let _ = subscriber.send(frame.clone());
                }
            }
        }
    }
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Response>>>;

struct Shared {
    hub: Mutex<Hub>,
    pending: Pending,
    /// azazel has gone: no frame and no answer will come.
    closed: AtomicBool,
}

impl Shared {
    /// Tells whoever waits for an answer or a frame that none will come. The flag goes up
    /// first, so a command sent from now on does not wait for an answer.
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        locked(&self.pending).clear();
        locked(&self.hub).subscribers.clear();
    }
}

/// The frames azazel writes, other than the responses to this client's commands.
pub struct Frames(mpsc::UnboundedReceiver<Frame>);

impl Frames {
    /// The next frame; `None` once azazel has gone.
    pub async fn next(&mut self) -> Option<Frame> {
        self.0.recv().await
    }
}

impl Stream for Frames {
    type Item = Frame;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Frame>> {
        self.0.poll_recv(cx)
    }
}

/// How the run a prompt started ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// The text of the lead's last message.
    pub text: String,
    /// Why the lead stopped: `stop`, `length`, `aborted`, ...
    pub stop: String,
}

/// A connection to one `azazel rpc` session.
pub struct Client {
    shared: Arc<Shared>,
    /// Lines on their way to azazel. A task writes them, so a command is written whole even
    /// when the caller stops waiting for it half-way.
    lines: Mutex<Option<mpsc::UnboundedSender<String>>>,
    writing: Mutex<Option<JoinHandle<()>>>,
    next_id: AtomicU64,
    version: String,
    child: Mutex<Option<Child>>,
}

/// The text blocks of a message as `message_end` carries it.
pub fn message_text(message: &Value) -> String {
    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect()
}

impl Client {
    /// Starts `azazel rpc` as a child process and connects to it. The child is killed when the
    /// client is dropped; [`Client::shutdown`] ends it properly.
    pub async fn spawn(options: Options) -> Result<Client, Error> {
        let mut command = tokio::process::Command::new(&options.binary);
        command.args(&options.args).arg("rpc");
        if let Some(cwd) = &options.cwd {
            command.arg("--cwd").arg(cwd);
        }
        command
            .envs(options.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(Error::Spawn)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(Error::Closed);
        };
        let client = Client::connect(BufReader::new(stdout), stdin).await?;
        *locked(&client.child) = Some(child);
        Ok(client)
    }

    /// Connects to an azazel that is already serving the protocol on `reader` and `writer` (a
    /// socket, a pipe, an SSH channel). The first frame must be its `hello`.
    pub async fn connect(
        reader: impl AsyncBufRead + Send + Unpin + 'static,
        mut writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Result<Client, Error> {
        let mut lines = reader.lines();
        let first = lines.next_line().await?.ok_or(Error::Closed)?;
        let version = match serde_json::from_str::<Frame>(&first) {
            Ok(Frame::Hello { protocol, version }) if protocol == PROTOCOL => version,
            Ok(Frame::Hello { protocol, version }) => {
                return Err(Error::Protocol(format!(
                    "azazel {version} speaks protocol {protocol}; this client speaks protocol {PROTOCOL}"
                )));
            }
            _ => {
                return Err(Error::Protocol(
                    "the other end did not say hello: it is not azazel, or an azazel too old for this client"
                        .to_string(),
                ));
            }
        };
        let shared = Arc::new(Shared {
            hub: Mutex::new(Hub::default()),
            pending: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        });
        let reading = shared.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                // A line that is not a frame is skipped: later versions may write what this one
                // cannot read.
                match serde_json::from_str::<Frame>(&line) {
                    Ok(Frame::Response(response)) => {
                        let waiting = response
                            .id
                            .as_u64()
                            .and_then(|id| locked(&reading.pending).remove(&id));
                        if let Some(waiting) = waiting {
                            let _ = waiting.send(response);
                        }
                    }
                    Ok(frame) => locked(&reading.hub).publish(frame),
                    Err(_) => {}
                }
            }
            reading.close();
        });
        let (lines, mut queued) = mpsc::unbounded_channel::<String>();
        let written = shared.clone();
        let writing = tokio::spawn(async move {
            while let Some(line) = queued.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                    // What cannot be sent will not be answered.
                    written.close();
                    return;
                }
            }
            let _ = writer.shutdown().await;
        });
        Ok(Client {
            shared,
            lines: Mutex::new(Some(lines)),
            writing: Mutex::new(Some(writing)),
            next_id: AtomicU64::new(1),
            version,
            child: Mutex::new(None),
        })
    }

    /// The version of the azazel at the other end.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Sends a command and waits for its response: the `data` of a successful one,
    /// [`Error::Rejected`] for a failed one.
    pub async fn send(&self, command: Command) -> Result<Value, Error> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, response) = oneshot::channel();
        locked(&self.shared.pending).insert(id, reply);
        if self.shared.closed.load(Ordering::SeqCst) {
            locked(&self.shared.pending).remove(&id);
            return Err(Error::Closed);
        }
        let request = Request {
            id: id.into(),
            command,
        };
        let mut line = serde_json::to_string(&request).map_err(|e| Error::Protocol(e.to_string()))?;
        line.push('\n');
        let queued = locked(&self.lines)
            .as_ref()
            .is_some_and(|lines| lines.send(line).is_ok());
        if !queued {
            locked(&self.shared.pending).remove(&id);
            return Err(Error::Closed);
        }
        let response = response.await.map_err(|_| Error::Closed)?;
        response.into_result().map_err(Error::Rejected)
    }

    /// The frames from now on. The first subscriber also gets the frames that arrived while
    /// nobody listened. Subscribe before sending the command whose frames you want.
    pub fn frames(&self) -> Frames {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut hub = locked(&self.shared.hub);
        for frame in hub.backlog.drain(..) {
            let _ = sender.send(frame);
        }
        if !self.shared.closed.load(Ordering::SeqCst) {
            hub.subscribers.push(sender);
        }
        Frames(receiver)
    }

    /// Sends a prompt and waits until the lead, and every sub-agent it started, is done.
    ///
    /// `on_frame` sees every frame of the run and may answer with a command, which is how an
    /// `approval_request` or a `ui_request` gets its response.
    pub async fn prompt_and_wait(
        &self,
        text: impl Into<String>,
        mut on_frame: impl FnMut(&Frame) -> Option<Command>,
    ) -> Result<Run, Error> {
        let mut frames = self.frames();
        self.send(Command::Prompt {
            text: text.into(),
            images: Vec::new(),
            request_id: None,
        })
        .await?;
        let mut answer = String::new();
        let mut agents_at_work = false;
        while let Some(frame) = frames.next().await {
            if let Some(command) = on_frame(&frame) {
                self.send(command).await?;
            }
            match frame {
                Frame::MessageEnd { agent, message } if agent == LEAD => answer = message_text(&message),
                Frame::Roster { members } => {
                    agents_at_work = members.iter().any(|member| {
                        member.id != LEAD
                            && matches!(
                                member.state.as_str(),
                                "queued" | "running" | "waiting for approval"
                            )
                    });
                }
                // Agents still at work report to the lead, which then goes on.
                Frame::Idle { agent, stop } if agent == LEAD && !agents_at_work => {
                    return Ok(Run { text: answer, stop });
                }
                Frame::Failed { agent, error } if agent == LEAD => return Err(Error::Failed(error)),
                _ => {}
            }
        }
        Err(Error::Closed)
    }

    /// Ends the session: closes the connection and, for a child process, waits for it to exit.
    /// Returns the child's exit code.
    pub async fn shutdown(self) -> Result<Option<i32>, Error> {
        self.close().await
    }

    /// [`Client::shutdown`] for a client that is shared: commands sent after it fail.
    pub async fn close(&self) -> Result<Option<i32>, Error> {
        // Closing the queue lets the writer send what is still in it, then close the connection.
        drop(locked(&self.lines).take());
        let writing = locked(&self.writing).take();
        if let Some(writing) = writing {
            let _ = writing.await;
        }
        let child = locked(&self.child).take();
        match child {
            Some(mut child) => Ok(child.wait().await?.code()),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::json;
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

    use super::*;

    /// The server's end of an in-memory connection.
    struct Server {
        input: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
        output: WriteHalf<DuplexStream>,
    }

    impl Server {
        async fn say(&mut self, line: Value) {
            self.output
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
        }

        async fn hear(&mut self) -> Value {
            serde_json::from_str(&self.input.next_line().await.unwrap().unwrap()).unwrap()
        }
    }

    fn pair() -> (
        Server,
        impl AsyncBufRead + Send + Unpin,
        impl AsyncWrite + Send + Unpin,
    ) {
        let (near, far) = tokio::io::duplex(64 * 1024);
        let (server_in, server_out) = tokio::io::split(far);
        let (client_in, client_out) = tokio::io::split(near);
        let server = Server {
            input: BufReader::new(server_in).lines(),
            output: server_out,
        };
        (server, BufReader::new(client_in), client_out)
    }

    #[tokio::test]
    async fn a_prompt_runs_to_idle_and_requests_are_answered_on_the_way() {
        let (mut server, reader, writer) = pair();
        let serving = tokio::spawn(async move {
            server
                .say(json!({ "type": "hello", "protocol": PROTOCOL, "version": "9.9.9" }))
                .await;
            let prompt = server.hear().await;
            assert_eq!(
                (&prompt["type"], &prompt["text"]),
                (&json!("prompt"), &json!("go"))
            );
            server
                .say(json!({ "id": prompt["id"], "type": "response", "success": true, "data": null }))
                .await;
            server
                .say(json!({
                    "type": "approval_request", "request_id": "r1", "agent": "lead", "tool": "write",
                    "access": "write", "subjects": ["a.txt"], "input": {}
                }))
                .await;
            let answer = server.hear().await;
            assert_eq!(
                (&answer["type"], &answer["request_id"], &answer["decision"]),
                (&json!("approval_response"), &json!("r1"), &json!("allow"))
            );
            server
                .say(json!({ "id": answer["id"], "type": "response", "success": true, "data": null }))
                .await;
            server.say(json!({ "type": "from_a_later_version" })).await;
            server
                .say(json!({
                    "type": "message_end", "agent": "lead",
                    "message": { "role": "assistant", "content": [{ "type": "text", "text": "Done." }] }
                }))
                .await;
            server
                .say(json!({ "type": "idle", "agent": "lead", "stop": "stop" }))
                .await;
            // A command azazel refuses.
            let bad = server.hear().await;
            server
                .say(json!({ "id": bad["id"], "type": "response", "success": false, "error": "no agent named `x`" }))
                .await;
        });

        let client = Client::connect(reader, writer).await.unwrap();
        assert_eq!(client.version(), "9.9.9");
        let mut seen = Vec::new();
        let run = client
            .prompt_and_wait("go", |frame| {
                seen.push(frame.clone());
                match frame {
                    Frame::ApprovalRequest { request_id, .. } => Some(Command::ApprovalResponse {
                        request_id: request_id.clone(),
                        decision: azazel_protocol::Decision::Allow,
                        feedback: None,
                    }),
                    _ => None,
                }
            })
            .await
            .unwrap();
        assert_eq!(
            run,
            Run {
                text: "Done.".into(),
                stop: "stop".into()
            }
        );
        assert!(seen.contains(&Frame::Unknown), "{seen:?}");
        let refused = client.send(Command::AgentStop { agent: "x".into() }).await;
        assert!(matches!(refused, Err(Error::Rejected(why)) if why == "no agent named `x`"));
        serving.await.unwrap();

        // The server has gone: frames end and commands fail.
        let mut frames = client.frames();
        assert_eq!(frames.next().await, None);
        assert!(client.send(Command::Abort).await.is_err());
    }

    #[tokio::test]
    async fn the_other_end_must_say_hello_in_this_protocol() {
        let (mut server, reader, writer) = pair();
        server
            .say(json!({ "type": "hello", "protocol": PROTOCOL + 1, "version": "9.9.9" }))
            .await;
        let newer = Client::connect(reader, writer).await.err().unwrap();
        assert_eq!(
            newer.to_string(),
            format!(
                "azazel 9.9.9 speaks protocol {}; this client speaks protocol {PROTOCOL}",
                PROTOCOL + 1
            )
        );

        let (mut server, reader, writer) = pair();
        server
            .say(json!({ "type": "agent_start", "agent": "lead" }))
            .await;
        let silent = Client::connect(reader, writer).await.err().unwrap();
        assert!(matches!(silent, Error::Protocol(why) if why.contains("did not say hello")));
    }
}
