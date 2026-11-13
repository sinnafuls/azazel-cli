//! One background session (`azazel host`): a session that serves the RPC protocol on a local
//! socket instead of standard input and output, and goes on working while nobody is connected.
//!
//! The session itself is the one of `azazel rpc` ([`crate::rpc::serve_lines`]). This module
//! stands between it and the clients:
//!
//! - every event frame is appended to `<home>/run/<id>.log`, for `azazel logs` and for a client
//!   that attaches later;
//! - any number of clients are attached (`attach`): the commands of each reach the session, each
//!   is answered on its own connection, and all of them are sent the session's frames. A client
//!   that attaches is told where the session stands (the reply being written, the tools that
//!   run, what waits) before the frames that follow. Any connection may send `stop`;
//! - approvals and questions wait until some client answers them, and are closed on the other
//!   clients once one has; those raised while nobody is attached are sent to the next client;
//! - `<home>/run/<id>.json` says what the session is doing, for `azazel ps`.
//!
//! The process ends on `stop`, or when the session has had nothing to do and no client for
//! `idle` (ten minutes unless `azazel host` was told otherwise). When it is killed instead, its
//! entry stays, and `azazel attach` starts it again on the same session: a run that was cut
//! short is then gone on with (`[session] resume_runs`).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use azazel_core::{Event, Level};
use azazel_protocol::{
    Attached, Command, Frame, MessageRow, Queued, Replay, Request, Response, RunningTool, Snapshot, State,
    Streaming, UiRequest,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::app::{Active, App};
use crate::attention::{Attention, Kind};
use crate::detach::socket::{self, Accept, Listener};
use crate::print::{EXIT_FAILED, EXIT_OK};
use crate::rpc::{self, Waiting};
use crate::running::{self, Entry, Lock};
use crate::schedule::Scheduler;
use crate::schedule::triggers::{self, Dispatch};

/// The ids of the commands the host itself sends the session. Their responses stay here.
const STATE_ID: &str = "azazel-host:state";
const PROMPT_ID: &str = "azazel-host:prompt";
/// How often the host looks at whether its entry is out of date and whether it has been idle
/// for long enough to end.
const TICK: Duration = Duration::from_millis(500);
const LEAD: &str = "lead";
/// The member of a command's id, as the session sees it, that names the connection the command
/// came on. Two clients may number their commands alike; the session answers each as the host
/// renamed it.
const CONN: &str = "azazel-host:conn";
/// How many messages of the conversation a client that attaches is told.
const TAIL: usize = 20;

pub struct Options {
    /// The id the session is listed under.
    pub id: String,
    /// How long to stay with nothing to do and nobody attached.
    pub idle: Duration,
    /// The first message.
    pub prompt: Option<String>,
    /// The name whoever started the session gave the first message.
    pub request_id: Option<String>,
    /// The options the process was started with that one started in its place is to have.
    pub carried: Vec<String>,
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

/// What is in flight, as the frames so far say: what a client that attaches now has missed
/// and cannot read from the session file.
#[derive(Default)]
struct Live {
    /// The lead is between the start and the end of a run.
    busy: bool,
    streaming: Vec<Streaming>,
    tools: Vec<(RunningTool, Instant)>,
    queue: VecDeque<Queued>,
}

impl Live {
    /// Takes in an event frame.
    fn on_frame(&mut self, kind: &str, frame: &Value) {
        let agent = frame["agent"].as_str().unwrap_or_default();
        let lead = agent == LEAD;
        match kind {
            "agent_start" if lead => {
                self.busy = true;
                // The run is the answer to the oldest input that starts one; input that
                // steers starts one only when nothing else waits.
                let at = self.queue.iter().position(|queued| queued.kind != "steer");
                self.queue.remove(at.unwrap_or(0));
            }
            // Input that steers is taken up between two requests.
            "turn_start" if lead && frame["turn"].as_u64().is_some_and(|turn| turn > 1) => {
                self.queue.retain(|queued| queued.kind != "steer");
            }
            "message_start" => {
                self.streaming.retain(|reply| reply.agent != agent);
                self.streaming.push(Streaming {
                    agent: agent.to_string(),
                    ..Streaming::default()
                });
            }
            "stream" => {
                let Some(reply) = self.streaming.iter_mut().find(|reply| reply.agent == agent) else {
                    return;
                };
                let delta = frame["event"]["delta"].as_str().unwrap_or_default();
                match frame["event"]["type"].as_str() {
                    Some("text_delta") => reply.text.push_str(delta),
                    Some("thinking_delta") => reply.thinking.push_str(delta),
                    _ => {}
                }
            }
            "message_end" | "rule" | "retry" => self.streaming.retain(|reply| reply.agent != agent),
            "tool_start" => self.tools.push((
                RunningTool {
                    agent: agent.to_string(),
                    call_id: frame["call_id"].as_str().unwrap_or_default().to_string(),
                    name: frame["name"].as_str().unwrap_or_default().to_string(),
                    input: frame["input"].clone(),
                    elapsed_ms: 0,
                },
                Instant::now(),
            )),
            "tool_end" => self
                .tools
                .retain(|(tool, _)| !(tool.agent == agent && frame["call_id"] == tool.call_id.as_str())),
            "idle" | "failed" => {
                self.streaming.retain(|reply| reply.agent != agent);
                self.tools.retain(|(tool, _)| tool.agent != agent);
                if lead {
                    self.busy = false;
                }
            }
            _ => {}
        }
    }
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
    /// The connections that are clients of the session, and the process each said it is.
    attached: BTreeMap<u64, Option<u32>>,
    pending: Vec<Pending>,
    live: Live,
    /// Where in the log the lead's run in progress begins: at the input that started it.
    run_start: Option<u64>,
    /// Where in the log the input that has not started its run yet stands.
    input_at: Option<u64>,
    /// The named prompts that went through here: one sent again is said once.
    named: HashSet<String>,
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
        mode: Some(app.permissions.mode().to_string()),
        args: options.carried,
        interrupted_at: None,
    };
    // A run the last process of this session did not finish is gone on with.
    let resume = options.prompt.is_none()
        && app.config.session.resume_runs
        && active.session.interrupted_run(None).is_some();
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
        attached: BTreeMap::new(),
        pending: Vec::new(),
        live: Live::default(),
        run_start: None,
        input_at: None,
        named: HashSet::new(),
        state_asked: false,
        stale: false,
        stopping: false,
        idle: options.idle,
        quiet_since: None,
    };
    if let Some(prompt) = options.prompt {
        host.entry.state = "working".to_string();
        host.user_input(None, "prompt", &prompt);
        let mut command = json!({ "id": PROMPT_ID, "type": "prompt", "text": prompt });
        if let Some(request_id) = options.request_id {
            host.named.insert(request_id.clone());
            command["request_id"] = json!(request_id);
        }
        host.send(command);
    } else if resume {
        host.entry.state = "working".to_string();
        host.send(json!({ "id": PROMPT_ID, "type": "continue_run" }));
    }
    if let Err(e) = host.entry.write(&home) {
        let e = format!("cannot write the entry of the background session: {e}");
        eprintln!("azazel: {e}");
        running::note_failure(&home, &id, &e);
        app.finish(&active).await;
        return EXIT_FAILED;
    }
    host.settle();
    let watched = triggers::watch_app(&app);
    let commands = futures::stream::unfold(commands, |mut commands| async {
        let line = commands.recv().await?;
        Some((line, commands))
    });
    tokio::join!(
        rpc::serve_lines(app, active, commands, lines, waiting),
        host.serve(listener, frames, watched),
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
    async fn serve(
        &mut self,
        listener: Listener,
        mut frames: mpsc::UnboundedReceiver<Option<String>>,
        watched: Option<(Arc<Scheduler>, mpsc::UnboundedReceiver<triggers::Event>)>,
    ) {
        let (reports, mut msgs) = mpsc::unbounded_channel::<Msg>();
        // The triggers of the project (`azazel schedule add --on …`), watched while this
        // session runs.
        let (scheduler, mut happened) = watched.unzip();
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
                Some(event) = async { happened.as_mut()?.recv().await } => {
                    if let Some(scheduler) = &scheduler {
                        self.on_trigger(scheduler, event).await;
                    }
                }
                _ = tick.tick() => self.on_tick(),
            }
        }
    }

    /// A trigger happened: its prompt goes to the lead, after what the lead is doing, or to a
    /// background session of its own.
    async fn on_trigger(&mut self, scheduler: &Scheduler, event: triggers::Event) {
        let (schedule, text) = match event {
            triggers::Event::Fire { schedule, text } => (schedule, text),
            triggers::Event::Note(note) => return self.announce(Level::Warn, note),
        };
        match triggers::dispatch(scheduler, &schedule, &text).await {
            Dispatch::Done(line) => self.announce(Level::Info, line),
            Dispatch::Here if self.input.is_none() => {}
            Dispatch::Here => {
                self.user_input(None, "prompt", &text);
                self.send(json!({ "id": PROMPT_ID, "type": "prompt", "text": text }));
                self.refresh();
            }
        }
    }

    /// Says something to whoever listens to the session: its clients and its hooks.
    fn announce(&self, level: Level, text: String) {
        let bus = self.app.registry.bus.clone();
        tokio::spawn(async move {
            let mut event = Event::Notification { level, text };
            bus.emit(&mut event).await;
        });
    }

    /// Hands the session a command of the host's own.
    fn send(&self, command: Value) {
        self.send_line(command.to_string());
    }

    fn send_line(&self, line: String) {
        if let Some(input) = &self.input {
            let _ = input.send(line);
        }
    }

    /// Tells the session to end; the host follows when it has.
    fn shutdown(&mut self) {
        self.input = None;
    }

    /// Writes a line to one connection.
    fn to(&self, conn: u64, line: String) {
        if let Some(lines) = self.conns.get(&conn) {
            let _ = lines.send(line);
        }
    }

    /// Writes a line to every client but `except`.
    fn to_clients(&self, line: &str, except: Option<u64>) {
        for conn in self.attached.keys().filter(|conn| Some(**conn) != except) {
            self.to(*conn, line.to_string());
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

    /// What the client on `from` sent the lead, for the log, for the other clients and for
    /// whoever attaches later.
    fn user_input(&mut self, from: Option<u64>, kind: &str, text: &str) {
        let frame = Frame::UserInput {
            kind: kind.to_string(),
            text: text.to_string(),
            agent: None,
        };
        if !self.live.busy {
            self.input_at.get_or_insert(self.log_len);
        }
        self.live.queue.push_back(Queued {
            kind: kind.to_string(),
            text: text.to_string(),
        });
        if let Ok(line) = serde_json::to_string(&frame) {
            self.log(&line);
            self.to_clients(&line, from);
        }
    }

    /// The request `request_id` no longer waits: the clients that still show it are told,
    /// `except` the one that answered.
    fn closed(&self, request_id: &str, except: Option<u64>) {
        let frame = Frame::RequestClosed {
            request_id: request_id.to_string(),
        };
        if let Ok(line) = serde_json::to_string(&frame) {
            self.to_clients(&line, except);
        }
    }

    /// Drops the requests whoever asked has stopped waiting for, and says so to the clients.
    fn drop_closed(&mut self) {
        let waiting = self.waiting.clone();
        let (open, closed): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|pending| waiting.is_open(&pending.request_id));
        self.pending = open;
        for pending in closed {
            self.closed(&pending.request_id, None);
        }
    }

    /// Where the session stands, for a client that attaches now.
    fn snapshot(&self) -> Snapshot {
        let tail = self.app.session().map_or_else(Vec::new, |session| {
            let mut rows: Vec<MessageRow> = session
                .branch(None)
                .into_iter()
                .rev()
                .filter_map(|entry| match entry.kind {
                    azazel_core::EntryKind::Message { message } => Some(MessageRow {
                        entry_id: entry.id,
                        timestamp: entry.timestamp,
                        message: serde_json::to_value(message).ok()?,
                    }),
                    _ => None,
                })
                .take(TAIL)
                .collect();
            rows.reverse();
            rows
        });
        Snapshot {
            busy: self.live.busy,
            tail,
            streaming: self.live.streaming.clone(),
            tools: self
                .live
                .tools
                .iter()
                .map(|(tool, since)| RunningTool {
                    elapsed_ms: since.elapsed().as_millis() as u64,
                    ..tool.clone()
                })
                .collect(),
            queue: self.live.queue.iter().cloned().collect(),
            pending: self
                .pending
                .iter()
                .map(|pending| pending.request_id.clone())
                .collect(),
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
                    self.to_asker(frame);
                }
            },
            // An answer in the making belongs to the client that asked.
            "btw_delta" => self.to_asker(frame),
            _ => {
                if of_lead && kind == "agent_start" {
                    self.run_start = Some(self.input_at.take().unwrap_or(self.log_len));
                }
                self.live.on_frame(kind, &frame);
                if of_lead && !self.live.busy {
                    self.run_start = None;
                }
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
                            Ok(UiRequest::Ask(asked)) => asked.question,
                            Ok(UiRequest::Form { questions }) => questions
                                .into_iter()
                                .map(|asked| asked.question)
                                .collect::<Vec<_>>()
                                .join(" / "),
                            Ok(UiRequest::Confirm { prompt })
                            | Ok(UiRequest::Select { prompt, .. })
                            | Ok(UiRequest::Input { prompt, .. }) => prompt,
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
                self.to_clients(&line, None);
            }
        }
    }

    /// Hands a frame that answers a command to the connection the command came on, under the
    /// id the client gave it.
    fn to_asker(&self, mut frame: Value) {
        let Some(conn) = frame["id"][CONN].as_u64() else {
            return;
        };
        frame["id"] = frame["id"]["id"].take();
        self.to(conn, frame.to_string());
    }

    /// A request to the user went out. With nobody attached, the user is called.
    fn asked(&mut self, request_id: String, line: String, text: &str) {
        self.pending.push(Pending { request_id, line });
        self.refresh();
        if !self.attached.is_empty() {
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
        self.drop_closed();
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
        entry.mode = Some(state.mode);
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
        if self.attached.is_empty() && self.entry.state == "idle" {
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
        if self.attached.remove(&conn).is_some() && self.attached.is_empty() {
            self.seen = self.log_len;
            self.settle();
        }
    }

    fn on_line(&mut self, conn: u64, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        let (id, command) = Request::parse(line);
        let command = match command {
            Ok(Command::Attach { pid, replay }) => return self.attach(conn, id, pid, replay),
            Ok(Command::Stop { now }) => {
                self.stopping = true;
                if now {
                    self.send(json!({ "type": "abort" }));
                }
                self.respond(conn, id, Ok(Value::Null));
                return self.refresh();
            }
            _ if !self.attached.contains_key(&conn) => {
                let refused = "send `attach` first: this connection is not a client of the session";
                return self.respond(conn, id, Err(refused.to_string()));
            }
            // The session would say the same; it could not say it to this connection.
            Err(error) => return self.respond(conn, id, Err(error)),
            Ok(command) => command,
        };
        if self.input.is_none() {
            return self.respond(conn, id, Err("the session is ending".to_string()));
        }
        match &command {
            Command::Prompt { text, request_id, .. } => {
                // A named prompt that is sent again starts nothing, so it is not said again.
                let again = request_id.as_ref().is_some_and(|request_id| {
                    !self.named.insert(request_id.clone())
                        || self
                            .app
                            .session()
                            .is_some_and(|session| session.request_run(request_id).is_some())
                });
                if !again {
                    self.user_input(Some(conn), "prompt", text);
                }
            }
            Command::Steer { text, .. } => self.user_input(Some(conn), "steer", text),
            Command::FollowUp { text } => self.user_input(Some(conn), "follow_up", text),
            // An answered request no longer waits, on any client.
            Command::ApprovalResponse { request_id, .. } | Command::UiResponse { request_id, .. } => {
                let before = self.pending.len();
                self.pending.retain(|pending| pending.request_id != *request_id);
                if self.pending.len() != before {
                    self.closed(request_id, Some(conn));
                }
                self.stale = true;
            }
            _ => {}
        }
        let renamed = Request {
            id: json!({ CONN: conn, "id": id }),
            command,
        };
        if let Ok(line) = serde_json::to_string(&renamed) {
            self.send_line(line);
        }
    }

    fn attach(&mut self, conn: u64, id: Value, pid: Option<u32>, replay: Replay) {
        let alone = self.attached.is_empty();
        self.attached.insert(conn, pid);
        self.settle();
        self.drop_closed();
        let attached = Attached {
            host: self.entry.id.clone(),
            pid: self.entry.pid,
            session_id: self.entry.session_id.clone(),
            session_file: self.entry.session_file.display().to_string(),
            clients: self.attached.len() as u32,
            snapshot: self.snapshot(),
        };
        self.respond(
            conn,
            id,
            serde_json::to_value(attached).map_err(|e| e.to_string()),
        );
        let from = match replay {
            Replay::Missed if alone => self.seen,
            // Somebody was here all along: what this client lacks is the run in progress.
            Replay::Missed => self.run_start.unwrap_or(self.log_len),
            Replay::All => 0,
            Replay::None => self.log_len,
        };
        for line in self.logged(from) {
            // Requests are sent below, and only those that still wait.
            let is_request = serde_json::from_str::<Value>(&line).is_ok_and(|frame| {
                frame["type"] == "approval_request" || (frame["type"] == "ui_request" && asks(&frame))
            });
            if !is_request {
                self.to(conn, line);
            }
        }
        for pending in &self.pending {
            self.to(conn, pending.line.clone());
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn queued(live: &Live) -> Vec<(&str, &str)> {
        live.queue
            .iter()
            .map(|queued| (queued.kind.as_str(), queued.text.as_str()))
            .collect()
    }

    #[test]
    fn what_is_in_flight_follows_the_frames() {
        let mut live = Live::default();
        let frame = |live: &mut Live, frame: Value| {
            let kind = frame["type"].as_str().unwrap().to_string();
            live.on_frame(&kind, &frame);
        };
        let input = |live: &mut Live, kind: &str, text: &str| {
            live.queue.push_back(Queued {
                kind: kind.to_string(),
                text: text.to_string(),
            });
        };

        // A prompt starts a run; a reply is written.
        input(&mut live, "prompt", "one");
        frame(&mut live, json!({ "type": "agent_start", "agent": "lead" }));
        assert!(live.busy && live.queue.is_empty());
        frame(
            &mut live,
            json!({ "type": "turn_start", "agent": "lead", "turn": 1 }),
        );
        frame(&mut live, json!({ "type": "message_start", "agent": "lead" }));
        for delta in ["Look", "ing…"] {
            frame(
                &mut live,
                json!({ "type": "stream", "agent": "lead",
                        "event": { "type": "text_delta", "index": 0, "delta": delta } }),
            );
        }
        frame(
            &mut live,
            json!({ "type": "stream", "agent": "lead",
                    "event": { "type": "thinking_delta", "index": 0, "delta": "hm" } }),
        );
        assert_eq!(
            live.streaming,
            [Streaming {
                agent: "lead".into(),
                text: "Looking…".into(),
                thinking: "hm".into()
            }]
        );

        // The reply ends and its calls run; a sub-agent writes meanwhile.
        frame(
            &mut live,
            json!({ "type": "message_end", "agent": "lead", "message": {} }),
        );
        assert!(live.streaming.is_empty());
        for call in ["c1", "c2"] {
            frame(
                &mut live,
                json!({ "type": "tool_start", "agent": "lead", "call_id": call, "name": "read",
                        "input": { "path": "a" } }),
            );
        }
        frame(&mut live, json!({ "type": "message_start", "agent": "scout-1" }));
        frame(
            &mut live,
            json!({ "type": "tool_end", "agent": "lead", "call_id": "c1",
                    "output": { "content": [], "is_error": false }, "duration_ms": 1 }),
        );
        let running: Vec<&str> = live.tools.iter().map(|(tool, _)| tool.call_id.as_str()).collect();
        assert_eq!(running, ["c2"]);
        assert_eq!(live.streaming.len(), 1);

        // What is sent meanwhile waits: a message that steers until the next request, a
        // prompt until the run after this one.
        input(&mut live, "steer", "mind the tests");
        input(&mut live, "prompt", "two");
        frame(
            &mut live,
            json!({ "type": "turn_start", "agent": "lead", "turn": 2 }),
        );
        assert_eq!(queued(&live), [("prompt", "two")]);
        input(&mut live, "steer", "too late for this run");
        frame(
            &mut live,
            json!({ "type": "idle", "agent": "lead", "stop": "aborted" }),
        );
        assert!(!live.busy && live.tools.is_empty());
        // The sub-agent goes on writing; the next run is the prompt's, not the steer's.
        assert_eq!(live.streaming.len(), 1);
        frame(&mut live, json!({ "type": "agent_start", "agent": "lead" }));
        assert_eq!(queued(&live), [("steer", "too late for this run")]);
    }
}
