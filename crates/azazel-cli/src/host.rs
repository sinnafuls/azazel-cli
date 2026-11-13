//! One background session (`azazel host`): a session that serves the RPC protocol on a local
//! socket instead of standard input and output, and goes on working while nobody is connected.
//!
//! The session itself is the one of `azazel rpc` ([`crate::rpc::serve_lines`]). This module
//! stands between it and the clients:
//!
//! - every event frame is appended to `<home>/run/<id>.log`, for `azazel logs` and for a client
//!   that attaches later;
//! - one client at a time is attached (`attach`): its commands reach the session and it is sent
//!   the session's frames. Any connection may send `stop`;
//! - approvals and questions raised while nobody is attached wait, and are sent again to the
//!   next client;
//! - `<home>/run/<id>.json` says what the session is doing, for `azazel ps`.
//!
//! The process ends on `stop`, or when the session has had nothing to do and no client for
//! `idle` (ten minutes unless `azazel host` was told otherwise).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use azazel_core::{Event, Level};
use azazel_protocol::{Attached, Command, Frame, Replay, Request, Response, State, UiRequest};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::app::{Active, App};
use crate::attention::{Attention, Kind};
use crate::detach::socket::{self, Accept, Listener};
use crate::print::{EXIT_FAILED, EXIT_OK};
use crate::rpc::{self, Waiting};
use crate::running::{self, Entry, Lock};

/// The ids of the commands the host itself sends the session. Their responses stay here.
const STATE_ID: &str = "azazel-host:state";
const PROMPT_ID: &str = "azazel-host:prompt";
/// How often the host looks at whether its entry is out of date and whether it has been idle
/// for long enough to end.
const TICK: Duration = Duration::from_millis(500);
const LEAD: &str = "lead";

pub struct Options {
    /// The id the session is listed under.
    pub id: String,
    /// How long to stay with nothing to do and nobody attached.
    pub idle: Duration,
    /// The first message.
    pub prompt: Option<String>,
}

/// What a connection's reader tells the host.
enum Msg {
    Line { conn: u64, line: String },
    Gone { conn: u64 },
}

/// A request to the user that may still wait for its answer.
struct Pending {
    request_id: String,
    line: String,
}

struct Host {
    app: Arc<App>,
    entry: Entry,
    log: File,
    /// The size of the log.
    log_len: u64,
    /// How much of the log the clients so far have been sent.
    seen: u64,
    /// Command lines on their way to the session; `None` once the session is told to end.
    input: Option<mpsc::UnboundedSender<String>>,
    waiting: Arc<Waiting>,
    /// What each open connection is written, by connection.
    conns: HashMap<u64, mpsc::UnboundedSender<String>>,
    /// The connection that is the session's client, and the process it said it is.
    attached: Option<(u64, Option<u32>)>,
    pending: Vec<Pending>,
    /// A `get_state` of the host's own has not been answered yet.
    state_asked: bool,
    /// Something happened that the entry may not say yet.
    stale: bool,
    /// `stop` was asked for: the session ends as soon as the lead has nothing to do.
    stopping: bool,
    idle: Duration,
    /// Since when the session has had nothing to do and nobody attached.
    quiet_since: Option<Instant>,
    attention: Attention,
}

fn now() -> String {
    jiff::Timestamp::now().to_string()
}

/// The session `azazel bg` and `/bg` start: `azazel host` serves it until it is stopped.
pub async fn run(app: Arc<App>, active: Active, options: Options) -> u8 {
    let home = app.paths.home.clone();
    let id = options.id;
    // Everything that can fail does so before the session starts work.
    let prepared = (|| -> Result<(Lock, Listener, String, File), String> {
        running::create_run_dir(&home).map_err(|e| format!("cannot create the run directory: {e}"))?;
        let alive = Lock::acquire(&running::host_lock(&home, &id))
            .map_err(|e| format!("cannot lock the background session: {e}"))?
            .ok_or_else(|| format!("a background session `{id}` is already running"))?;
        let name = socket::name_for(&home, &id).map_err(|e| format!("cannot place the socket: {e}"))?;
        let listener = socket::listen(&name).map_err(|e| format!("cannot listen on {name}: {e}"))?;
        let log = File::options()
            .create(true)
            .append(true)
            .open(running::log_file(&home, &id))
            .map_err(|e| format!("cannot write the log: {e}"))?;
        Ok((alive, listener, name, log))
    })();
    let (alive, listener, socket_name, log) = match prepared {
        Ok(prepared) => prepared,
        Err(e) => {
            eprintln!("azazel: {e}");
            running::note_failure(&home, &id, &e);
            app.finish(&active).await;
            return EXIT_FAILED;
        }
    };
    let entry = Entry {
        id: id.clone(),
        pid: std::process::id(),
        session_id: active.session.id(),
        session_file: active.session.path(),
        cwd: app.paths.cwd.clone(),
        name: active.session.name(),
        started: now(),
        socket: socket_name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        state: "idle".to_string(),
        cost_usd: None,
    };
    let (input, commands) = mpsc::unbounded_channel::<String>();
    let (lines, frames) = mpsc::unbounded_channel::<Option<String>>();
    let waiting = Arc::new(Waiting::default());
    let log_len = log.metadata().map(|meta| meta.len()).unwrap_or(0);
    let mut host = Host {
        attention: Attention::new(
            azazel_tui::term::Caps::detect(&|_| None),
            app.config.notify.clone(),
        ),
        app: app.clone(),
        entry,
        log,
        log_len,
        seen: log_len,
        input: Some(input),
        waiting: waiting.clone(),
        conns: HashMap::new(),
        attached: None,
        pending: Vec::new(),
        state_asked: false,
        stale: false,
        stopping: false,
        idle: options.idle,
        quiet_since: None,
    };
    if let Some(prompt) = options.prompt {
        host.entry.state = "working".to_string();
        host.user_input("prompt", &prompt);
        host.send(json!({ "id": PROMPT_ID, "type": "prompt", "text": prompt }));
    }
    if let Err(e) = host.entry.write(&home) {
        let e = format!("cannot write the entry of the background session: {e}");
        eprintln!("azazel: {e}");
        running::note_failure(&home, &id, &e);
        app.finish(&active).await;
        return EXIT_FAILED;
    }
    host.settle();
    let commands = futures::stream::unfold(commands, |mut commands| async {
        let line = commands.recv().await?;
        Some((line, commands))
    });
    tokio::join!(
        rpc::serve_lines(app, active, commands, lines, waiting),
        host.serve(listener, frames),
    );
    let _ = std::fs::remove_file(running::entry_file(&home, &id));
    // A named pipe goes with the listener; a socket file may be left when the listener was
    // displaced.
    #[cfg(unix)]
    let _ = std::fs::remove_file(&socket_name);
    drop(alive);
    EXIT_OK
}

/// Whether a `ui_request` frame asks for an answer.
fn asks(frame: &Value) -> bool {
    frame["kind"] != "notify"
}

impl Host {
    async fn serve(&mut self, listener: Listener, mut frames: mpsc::UnboundedReceiver<Option<String>>) {
        let (reports, mut msgs) = mpsc::unbounded_channel::<Msg>();
        let mut connections = 0u64;
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                // What the session wrote is logged before anything a client asks next.
                biased;
                frame = frames.recv() => match frame {
                    Some(Some(line)) => self.on_frame(line),
                    // The session has ended.
                    _ => break,
                },
                Some(msg) = msgs.recv() => match msg {
                    Msg::Line { conn, line } => self.on_line(conn, &line),
                    Msg::Gone { conn } => self.gone(conn),
                },
                accepted = listener.accept() => match accepted {
                    Ok(stream) => {
                        connections += 1;
                        self.connect(connections, stream, reports.clone());
                    }
                    Err(e) => {
                        tracing::warn!("a connection to the background session failed: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
                _ = tick.tick() => self.on_tick(),
            }
        }
    }

    /// Hands the session a command of the host's own.
    fn send(&self, command: Value) {
        if let Some(input) = &self.input {
            let _ = input.send(command.to_string());
        }
    }

    /// Tells the session to end; the host follows when it has.
    fn shutdown(&mut self) {
        self.input = None;
    }

    fn to_client(&self, line: String) {
        if let Some((conn, _)) = self.attached
            && let Some(lines) = self.conns.get(&conn)
        {
            let _ = lines.send(line);
        }
    }

    fn respond(&self, conn: u64, id: Value, outcome: Result<Value, String>) {
        let response = match outcome {
            Ok(data) => Response::ok(id, data),
            Err(error) => Response::err(id, error),
        };
        if let (Some(lines), Ok(line)) = (
            self.conns.get(&conn),
            serde_json::to_string(&Frame::Response(response)),
        ) {
            let _ = lines.send(line);
        }
    }

    fn log(&mut self, line: &str) {
        match writeln!(self.log, "{line}") {
            Ok(()) => self.log_len += line.len() as u64 + 1,
            Err(e) => tracing::error!("could not write the log of the background session: {e}"),
        }
    }

    /// What a client sent the lead, for the log and for whoever attaches later.
    fn user_input(&mut self, kind: &str, text: &str) {
        let frame = Frame::UserInput {
            kind: kind.to_string(),
            text: text.to_string(),
        };
        if let Ok(line) = serde_json::to_string(&frame) {
            self.log(&line);
        }
    }

    // ------------------------------------------------------------ the session's frames

    fn on_frame(&mut self, line: String) {
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            return;
        };
        let kind = frame["type"].as_str().unwrap_or_default();
        let of_lead = frame["agent"] == LEAD;
        match kind {
            "response" => match frame["id"].as_str() {
                Some(STATE_ID) => self.on_state(&frame),
                Some(PROMPT_ID) => {
                    if frame["success"] != true {
                        let error =
                            format!("prompt rejected: {}", frame["error"].as_str().unwrap_or_default());
                        self.on_frame(json!({ "type": "failed", "agent": LEAD, "error": error }).to_string());
                    }
                }
                _ => {
                    // The command it answers may have named the session or opened another one.
                    self.stale = true;
                    self.to_client(line);
                }
            },
            // An answer in the making belongs to the client that asked.
            "btw_delta" => self.to_client(line),
            _ => {
                self.log(&line);
                let request_id = frame["request_id"].as_str().map(str::to_string);
                match (kind, request_id) {
                    ("approval_request", Some(request_id)) => {
                        let text = format!(
                            "{} needs permission to use {}",
                            frame["agent"].as_str().unwrap_or(LEAD),
                            frame["tool"].as_str().unwrap_or("a tool")
                        );
                        self.asked(request_id, line.clone(), &text);
                    }
                    ("ui_request", Some(request_id)) if asks(&frame) => {
                        let text = match serde_json::from_value::<UiRequest>(frame.clone()) {
                            Ok(UiRequest::Ask { question, .. }) => question,
                            Ok(UiRequest::Confirm { prompt })
                            | Ok(UiRequest::Select { prompt, .. })
                            | Ok(UiRequest::Input { prompt }) => prompt,
                            _ => "a plan is ready for review".to_string(),
                        };
                        self.asked(request_id, line.clone(), &text);
                    }
                    ("agent_start" | "idle" | "failed", _) if of_lead => {
                        if kind != "agent_start" && self.stopping {
                            self.shutdown();
                        }
                        self.refresh();
                    }
                    ("roster", _) => self.stale = true,
                    _ => {}
                }
                self.to_client(line);
            }
        }
    }

    /// A request to the user went out. With nobody attached, the user is called.
    fn asked(&mut self, request_id: String, line: String, text: &str) {
        self.pending.push(Pending { request_id, line });
        self.refresh();
        if self.attached.is_some() {
            return;
        }
        let label = self.entry.name.clone().unwrap_or_else(|| self.entry.id.clone());
        let text = format!("background session {label}: {text}");
        let bus = self.app.registry.bus.clone();
        let announced = text.clone();
        tokio::spawn(async move {
            let mut event = Event::Notification {
                level: Level::Info,
                text: announced,
            };
            bus.emit(&mut event).await;
        });
        // There is no terminal to write to; a notification of the operating system is sent
        // on its own.
        let _ = self.attention.signal(Kind::NeedsYou, &text, None);
    }

    /// Asks the session where it stands, unless the answer to that is already on its way.
    fn refresh(&mut self) {
        if self.state_asked {
            self.stale = true;
            return;
        }
        self.stale = false;
        self.state_asked = true;
        self.send(json!({ "id": STATE_ID, "type": "get_state" }));
    }

    fn on_state(&mut self, response: &Value) {
        self.state_asked = false;
        let Ok(state) = serde_json::from_value::<State>(response["data"].clone()) else {
            return;
        };
        let waiting = self.waiting.clone();
        self.pending
            .retain(|pending| waiting.is_open(&pending.request_id));
        let agents_at_work = state.agents.iter().any(|member| {
            member.id != LEAD
                && matches!(
                    member.state.as_str(),
                    "queued" | "running" | "waiting for approval"
                )
        });
        let mut entry = self.entry.clone();
        entry.state = if !self.pending.is_empty() {
            "waiting for you"
        } else if state.busy || agents_at_work {
            "working"
        } else {
            "idle"
        }
        .to_string();
        // The roster counts the lead; without one the lead's own total is all there is.
        entry.cost_usd = match state.agents.as_slice() {
            [] => state.usage.cost_usd,
            agents => agents
                .iter()
                .filter_map(|member| member.cost_usd)
                .reduce(|a, b| a + b),
        };
        entry.name = state.name;
        entry.session_id = state.session_id;
        if !state.session_file.is_empty() {
            entry.session_file = state.session_file.into();
        }
        if entry != self.entry {
            self.entry = entry;
            if let Err(e) = self.entry.write(&self.app.paths.home) {
                tracing::warn!("could not update the entry of the background session: {e}");
            }
        }
        if self.stopping && !state.busy {
            self.shutdown();
        }
        self.settle();
        if self.stale {
            self.refresh();
        }
    }

    /// Notes whether the session has anything to do or anyone to do it for.
    fn settle(&mut self) {
        if self.attached.is_none() && self.entry.state == "idle" {
            self.quiet_since.get_or_insert_with(Instant::now);
        } else {
            self.quiet_since = None;
        }
    }

    fn on_tick(&mut self) {
        if self.stale {
            self.refresh();
        }
        if self.quiet_since.is_some_and(|since| since.elapsed() >= self.idle) {
            self.shutdown();
        }
    }

    // ------------------------------------------------------------ connections

    fn connect(
        &mut self,
        conn: u64,
        stream: interprocess::local_socket::tokio::Stream,
        reports: mpsc::UnboundedSender<Msg>,
    ) {
        let (reader, mut writer) = socket::halves(stream);
        let (lines, mut queued) = mpsc::unbounded_channel::<String>();
        if let Ok(hello) = serde_json::to_string(&rpc::hello()) {
            let _ = lines.send(hello);
        }
        self.conns.insert(conn, lines);
        tokio::spawn(async move {
            while let Some(mut line) = queued.recv().await {
                line.push('\n');
                if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                    break;
                }
            }
            let _ = writer.shutdown().await;
        });
        tokio::spawn(async move {
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if reports.send(Msg::Line { conn, line }).is_err() {
                    return;
                }
            }
            let _ = reports.send(Msg::Gone { conn });
        });
    }

    fn gone(&mut self, conn: u64) {
        self.conns.remove(&conn);
        if self.attached.is_some_and(|(attached, _)| attached == conn) {
            self.attached = None;
            self.seen = self.log_len;
            self.settle();
        }
    }

    fn on_line(&mut self, conn: u64, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        let (id, command) = Request::parse(line);
        let attached = self.attached.is_some_and(|(attached, _)| attached == conn);
        match command {
            Ok(Command::Attach { pid, replay }) => self.attach(conn, id, pid, replay),
            Ok(Command::Stop { now }) => {
                self.stopping = true;
                if now {
                    self.send(json!({ "type": "abort" }));
                }
                self.respond(conn, id, Ok(Value::Null));
                self.refresh();
            }
            _ if !attached => self.respond(
                conn,
                id,
                Err("send `attach` first: this connection is not the session's client".to_string()),
            ),
            command => {
                match &command {
                    Ok(Command::Prompt { text, .. }) => self.user_input("prompt", text),
                    Ok(Command::Steer { text }) => self.user_input("steer", text),
                    Ok(Command::FollowUp { text }) => self.user_input("follow_up", text),
                    // An answered request no longer waits.
                    Ok(Command::ApprovalResponse { .. } | Command::UiResponse { .. }) => self.stale = true,
                    _ => {}
                }
                match &self.input {
                    // A line the session cannot read is the session's to refuse.
                    Some(input) => drop(input.send(line.to_string())),
                    None => self.respond(conn, id, Err("the session is ending".to_string())),
                }
            }
        }
    }

    fn attach(&mut self, conn: u64, id: Value, pid: Option<u32>, replay: Replay) {
        if let Some((other, pid)) = self.attached
            && other != conn
        {
            let who = match pid {
                Some(pid) => format!(" (process {pid})"),
                None => String::new(),
            };
            return self.respond(
                conn,
                id,
                Err(format!("another client is attached to this session{who}")),
            );
        }
        self.attached = Some((conn, pid));
        self.settle();
        let attached = Attached {
            host: self.entry.id.clone(),
            pid: self.entry.pid,
            session_id: self.entry.session_id.clone(),
            session_file: self.entry.session_file.display().to_string(),
        };
        self.respond(
            conn,
            id,
            serde_json::to_value(attached).map_err(|e| e.to_string()),
        );
        let from = match replay {
            Replay::Missed => self.seen,
            Replay::All => 0,
            Replay::None => self.log_len,
        };
        for line in self.logged(from) {
            // Requests are sent below, and only those that still wait.
            let is_request = serde_json::from_str::<Value>(&line).is_ok_and(|frame| {
                frame["type"] == "approval_request" || (frame["type"] == "ui_request" && asks(&frame))
            });
            if !is_request {
                self.to_client(line);
            }
        }
        let waiting = self.waiting.clone();
        self.pending
            .retain(|pending| waiting.is_open(&pending.request_id));
        for pending in &self.pending {
            self.to_client(pending.line.clone());
        }
    }

    /// The lines of the log from byte `from` on.
    fn logged(&self, from: u64) -> Vec<String> {
        let read = || -> std::io::Result<String> {
            let mut file = File::open(running::log_file(&self.app.paths.home, &self.entry.id))?;
            file.seek(SeekFrom::Start(from))?;
            let mut text = String::new();
            file.take(self.log_len.saturating_sub(from))
                .read_to_string(&mut text)?;
            Ok(text)
        };
        match read() {
            Ok(text) => text.lines().map(str::to_string).collect(),
            Err(e) => {
                tracing::warn!("could not read the log of the background session: {e}");
                Vec::new()
            }
        }
    }
}
