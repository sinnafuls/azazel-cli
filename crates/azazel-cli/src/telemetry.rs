//! Trace export (`[telemetry]`): what a session does, sent as spans to an OpenTelemetry
//! collector or to LangSmith.
//!
//! A session's agents are listened to beside the session ([`Telemetry::watch`]); what they emit
//! becomes spans ([`record`]), one trace per prompt, which a task of their own sends in batches
//! in the shape the backend takes ([`wire`]). Nothing here is waited for by an agent: a slow or absent backend
//! costs a turn nothing. The one wait is at the end of the process, for the last batch, and it
//! is short.

pub mod record;
pub mod wire;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use azazel_agent::{AgentEvent, AgentHandle};
use azazel_ai::Role;
use azazel_core::SessionStore;
use azazel_core::config::TelemetryConfig;
use azazel_team::{Team, TeamEvent};
use jiff::Timestamp;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use record::{Recorder, Span, Who};
pub use wire::Wire;

/// Spans that wait this long are sent, however few they are.
const SEND_EVERY: Duration = Duration::from_secs(2);
/// Spans sent in one request.
const BATCH: usize = 64;
/// Spans kept while the backend does not answer; the rest are dropped.
const MAX_WAITING: usize = 4096;
const CONNECT_LIMIT: Duration = Duration::from_secs(3);
const REQUEST_LIMIT: Duration = Duration::from_secs(10);
/// How long the end of the process waits for the last spans to be sent.
const CLOSE_WAIT: Duration = Duration::from_secs(3);

enum Msg {
    Span(Box<Span>),
    /// Send what waits now, and say when that is done.
    Flush(oneshot::Sender<()>),
}

/// The trace export of this process.
pub struct Telemetry {
    queue: mpsc::UnboundedSender<Msg>,
    content: bool,
}

impl Telemetry {
    /// Starts the export `config` asks for; `None` when it asks for none. Needs a runtime.
    pub fn start(config: &TelemetryConfig) -> Option<Arc<Telemetry>> {
        let wire = Wire::from_config(config, &|name| std::env::var(name).ok())?;
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_LIMIT)
            .timeout(REQUEST_LIMIT)
            .user_agent(concat!("azazel/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        let (queue, waiting) = mpsc::unbounded_channel();
        tokio::spawn(send_all(waiting, wire, client));
        Some(Arc::new(Telemetry {
            queue,
            content: config.content,
        }))
    }

    fn send(&self, spans: Vec<Span>) {
        for span in spans {
            let _ = self.queue.send(Msg::Span(Box::new(span)));
        }
    }

    /// Waits, but not long, until what has been recorded so far is sent. For the end of the
    /// process: until then batches leave on their own.
    pub async fn flush(&self) {
        let (done, sent) = oneshot::channel();
        if self.queue.send(Msg::Flush(done)).is_ok() {
            let _ = tokio::time::timeout(CLOSE_WAIT, sent).await;
        }
    }

    /// Records `session` from now until the [`Watch`] is closed: what `lead` does, and what
    /// the sub-agents of `team` do.
    pub fn watch(
        self: &Arc<Self>,
        session: &SessionStore,
        lead: AgentHandle,
        team: Option<Arc<Team>>,
    ) -> Watch {
        let stop = CancellationToken::new();
        let follower = Follower {
            telemetry: self.clone(),
            recorder: Recorder::new(session.id(), self.content),
            // Subscribed here, not in the task: nothing the lead does after this is missed.
            lead_events: lead.subscribe(),
            team_events: team.as_ref().map(|team| team.subscribe()),
            lead,
            team,
            members: HashMap::new(),
        };
        let task = tokio::spawn(follower.run(stop.clone()));
        Watch {
            stop,
            task: Mutex::new(Some(task)),
        }
    }
}

/// The recording of one open session.
pub struct Watch {
    stop: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Watch {
    /// The session closed: what its agents did until now is recorded, and a prompt that is
    /// still open ends. Call after the lead has shut down. Nothing here waits for the backend.
    pub async fn close(&self) {
        self.stop.cancel();
        let task = self.task.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct Follower {
    telemetry: Arc<Telemetry>,
    recorder: Recorder,
    lead: AgentHandle,
    lead_events: mpsc::UnboundedReceiver<AgentEvent>,
    team: Option<Arc<Team>>,
    team_events: Option<broadcast::Receiver<TeamEvent>>,
    /// The definition and the parent of each sub-agent seen so far.
    members: HashMap<String, (String, Option<String>)>,
}

/// The next thing a sub-agent did. Never, when there is no team.
async fn next_of_team(events: &mut Option<broadcast::Receiver<TeamEvent>>) -> (String, AgentEvent) {
    loop {
        let Some(receiver) = events else {
            return std::future::pending().await;
        };
        match receiver.recv().await {
            Ok(TeamEvent::Agent { id, event }) => return (id, *event),
            // Events that were dropped because this side fell behind leave spans that end when
            // their prompt does.
            Ok(TeamEvent::Roster) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => *events = None,
        }
    }
}

impl Follower {
    async fn run(mut self, stop: CancellationToken) {
        loop {
            tokio::select! {
                biased;
                event = self.lead_events.recv() => match event {
                    Some(event) => self.on_lead(event),
                    None => break,
                },
                (id, event) = next_of_team(&mut self.team_events) => self.on_member(&id, event),
                _ = stop.cancelled() => break,
            }
        }
        // What the agents emitted before the session closed.
        while let Ok(event) = self.lead_events.try_recv() {
            self.on_lead(event);
        }
        while let Some(receiver) = &mut self.team_events {
            match receiver.try_recv() {
                Ok(TeamEvent::Agent { id, event }) => self.on_member(&id, *event),
                Ok(TeamEvent::Roster) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
        let spans = self.recorder.close(Timestamp::now());
        self.telemetry.send(spans);
    }

    fn on_lead(&mut self, event: AgentEvent) {
        let id = self.lead.id.clone();
        // The prompt is not in an event: it is the last thing the user said in the request
        // the lead has just sent.
        if matches!(event, AgentEvent::MessageStart)
            && self.recorder.needs_prompt(&id)
            && let Some(sent) = self.lead.last_request()
            && let Some(prompt) = sent
                .request
                .messages
                .iter()
                .rfind(|message| message.role == Role::User)
        {
            self.recorder.prompt_text(&id, &prompt.text());
        }
        let who = Who {
            id: &id,
            def: &self.lead.info().def,
            parent: None,
        };
        let spans = self.recorder.event(&who, &event, Timestamp::now());
        self.telemetry.send(spans);
    }

    fn on_member(&mut self, id: &str, event: AgentEvent) {
        if !self.members.contains_key(id) {
            let known = self.team.as_ref().and_then(|team| team.member(id));
            let (def, parent) = match known {
                Some((info, _)) => (info.def, info.parent),
                None => (id.to_string(), Some(self.lead.id.clone())),
            };
            self.members.insert(id.to_string(), (def, parent));
        }
        let (def, parent) = &self.members[id];
        let who = Who {
            id,
            def,
            parent: parent.as_deref(),
        };
        let spans = self.recorder.event(&who, &event, Timestamp::now());
        self.telemetry.send(spans);
    }
}

/// Sends what is recorded, in batches, until nobody records any more. A batch the backend
/// does not take is dropped; the first such failure is logged and no later one.
async fn send_all(mut waiting: mpsc::UnboundedReceiver<Msg>, wire: Wire, client: reqwest::Client) {
    let mut batch: Vec<Span> = Vec::new();
    let mut failed = false;
    let mut dropped = false;
    let mut tick = tokio::time::interval(SEND_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let mut flushed = None;
        tokio::select! {
            msg = waiting.recv() => match msg {
                Some(Msg::Span(span)) => {
                    if batch.len() < MAX_WAITING {
                        batch.push(*span);
                    } else if !dropped {
                        dropped = true;
                        tracing::warn!("trace export: more than {MAX_WAITING} spans wait to be sent; the rest are dropped");
                    }
                    if batch.len() < BATCH {
                        continue;
                    }
                }
                Some(Msg::Flush(done)) => flushed = Some(done),
                None => {
                    post(&client, &wire, &mut batch, &mut failed).await;
                    return;
                }
            },
            _ = tick.tick() => {}
        }
        post(&client, &wire, &mut batch, &mut failed).await;
        if let Some(done) = flushed {
            let _ = done.send(());
        }
    }
}

async fn post(client: &reqwest::Client, wire: &Wire, batch: &mut Vec<Span>, failed: &mut bool) {
    if batch.is_empty() {
        return;
    }
    let body = wire.body(batch);
    batch.clear();
    let mut request = client.post(wire.url()).json(&body);
    for (name, value) in wire.headers() {
        request = request.header(name, value);
    }
    let problem = match request.send().await {
        Ok(response) if response.status().is_success() => None,
        Ok(response) => {
            let status = response.status();
            let said = response.text().await.unwrap_or_default();
            let said: String = said.chars().take(300).collect();
            Some(format!("{status} {}", said.trim()))
        }
        Err(e) => Some(e.to_string()),
    };
    if let Some(problem) = problem
        && !*failed
    {
        *failed = true;
        tracing::warn!(
            "trace export ({}) failed: {}; later failures are not logged",
            wire.describe(),
            problem.trim()
        );
    }
}
