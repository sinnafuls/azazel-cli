//! What the UI needs from a session, wherever the session runs.
//!
//! [`Local`] is a session in this process: the application and its lead agent, as every other
//! mode uses them. [`super::remote::Remote`] is a session in another process (a background
//! session on this machine, or azazel on another machine reached over SSH), spoken to in the
//! RPC protocol.
//!
//! The UI reads what a session holds from a [`SessionStore`]: the live one of a local session,
//! a copy of it for a remote one ([`Backend::session`]). What changes a session goes through
//! the methods here. What has no counterpart in the protocol is reached through
//! [`Backend::local`], and is not there for a remote session.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_agent::{AgentCmd, AgentEvent, AgentStatus, UserInput};
use azazel_ai::providers::presets;
use azazel_ai::{Block, ModelSpec, ProviderConfig, ThinkingLevel, Tier};
use azazel_core::process::{RunOptions, run as run_command};
use azazel_core::session::SessionSummary;
use azazel_core::skills::invocation_text;
use azazel_core::{CommandOutcome, Config, EntryKind, Paths, PermissionMode, SessionStore, StartSource, Ui};
use azazel_protocol::{ModelRow, PrimaryAgent, ProviderRow, Roles, SaveTo};
use azazel_team::{Goals, MemberInfo, Mentioned, Team, TeamEvent};
use azazel_tools::{TodoItem, TodoStore};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use super::bridge::{TuiBridge, UiRequest};
use super::overlay::RewindPoint;
use super::{Internal, SHELL_NOTE, btw, slash, team};
use crate::app::{Active, App, Open};
use crate::retry::Again;
use crate::side::{Btw, Exchange};

/// The machine the UI itself runs on: where its settings, key bindings and themes are read
/// from, whatever session it shows.
pub(crate) struct Env {
    pub paths: Paths,
    pub config: Config,
    /// This machine's credentials, for what the UI itself asks of a provider (voice).
    #[cfg(feature = "voice")]
    pub credentials: Arc<azazel_ai::Credentials>,
}

/// What reaches the UI from the session.
pub(crate) enum Incoming {
    /// An event of the lead.
    Lead(AgentEvent),
    Team(TeamEvent),
    /// An agent or a command asks the user something.
    Request(UiRequest),
    /// Another client of the session sent the lead this. `kind` is `prompt`, `steer` or
    /// `follow_up`.
    UserInput {
        kind: String,
        text: String,
    },
    /// The session is gone: the text says why.
    Closed(String),
}

/// Where the conversation goes on.
pub(crate) enum Go {
    /// A new session.
    New,
    /// The saved session in this file.
    File(PathBuf),
    /// A copy of the open session.
    Fork,
    /// Another entry of the open session; with `before`, the entry before it.
    Leaf { entry: String, before: bool },
}

/// How the session the conversation moved to was opened.
pub(crate) struct Moved {
    /// Whether it existed before.
    pub resumed: bool,
    /// Something about how it was opened that the user should be told.
    pub notice: Option<String>,
    /// Where its last run stopped, when the end of a process cut it short.
    pub interrupted: Option<String>,
}

/// What to say of a session whose last run was cut short `stopped` (where it stopped).
pub(crate) fn interrupted_notice(stopped: &str) -> String {
    format!("the last run of this session was interrupted ({stopped}); /continue goes on with it")
}

/// What the status line and the live region show beside the lead's own status.
#[derive(Default)]
pub(crate) struct Facts {
    pub coordinator: bool,
    pub tier: Option<Tier>,
    /// The open goal, in a few words.
    pub goal: Option<String>,
    pub todo: Vec<TodoItem>,
    /// How the lead's model is paid for.
    pub billing: Billing,
    /// What a run is waiting for while its account is out of allowance, and the Unix seconds
    /// at which the wait is over.
    pub waiting: Option<(String, u64)>,
    /// The stored account of the lead's provider that requests go out with: a sign-in or a
    /// stored key. None for a key from elsewhere.
    pub account: Option<String>,
    /// The e-mail address that account signed in with, as `usage.redact_email` shows it.
    pub account_email: Option<String>,
}

/// How the lead's model is paid for, which decides what the status line says about money.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) enum Billing {
    /// By the token: an API key, or a server that takes none. The cost is money spent.
    #[default]
    Metered,
    /// A subscription the user signed in to: its tokens are not paid for one by one. `window`
    /// is the fullest usage window of the account, as its name and the share used, once the
    /// provider has reported one.
    Subscription { window: Option<(String, f32)> },
}

/// The roles the cycling keys step through when there are no favourites, as `@role`: those of
/// `roles` that answer with a model (each comes with the models it resolves to), `default`,
/// `smol` and `slow` first and the others by name. A role that answers with the same models as
/// one before it is left out: a step onto it would change nothing.
pub(crate) fn cycle_of(mut roles: Vec<(String, Vec<String>)>) -> Vec<String> {
    const FIRST: [&str; 3] = ["default", "smol", "slow"];
    let rank = |role: &str| {
        FIRST
            .iter()
            .position(|first| *first == role)
            .unwrap_or(FIRST.len())
    };
    roles.sort_by(|(a, _), (b, _)| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
    let mut seen: Vec<Vec<String>> = Vec::new();
    let mut cycle = Vec::new();
    for (role, models) in roles {
        if models.is_empty() || seen.contains(&models) {
            continue;
        }
        seen.push(models);
        cycle.push(format!("@{role}"));
    }
    cycle
}

/// The stored account requests go out with.
#[derive(Clone)]
struct InUse {
    label: String,
    /// A sign-in: a subscription.
    login: bool,
    /// Its e-mail address, as it is shown.
    email: Option<String>,
}

/// Who pays for a provider, as it was last looked up.
struct Payer {
    provider: String,
    asked: Instant,
    /// The stored account in use.
    account: Option<InUse>,
}

/// The answer is read from files: often enough to follow a sign-in, not on every frame.
const PAYER_FRESH: Duration = Duration::from_secs(2);

#[async_trait]
pub(crate) trait Backend: Send {
    /// The session, when it runs in this process.
    fn local(&self) -> Option<&Local> {
        None
    }

    /// Where the session runs, when that is not this process.
    fn place(&self) -> Option<String> {
        None
    }

    /// What to say when the UI leaves a session that goes on without it.
    fn farewell(&self) -> Option<String> {
        None
    }

    /// The next thing the session says. Giving up the wait loses nothing.
    async fn next(&mut self) -> Incoming;

    /// Tells extensions that the session the UI opens on has started.
    async fn started(&mut self) {}

    /// How the session the UI opens on came to be open.
    fn opened(&mut self) -> Moved;

    // ------------------------------------------------------------ what the session is

    fn status(&self) -> AgentStatus;

    /// The model the lead answers with; empty when there is none.
    fn model_label(&self) -> String;

    /// Whether the lead has a model it can use.
    fn has_model(&self) -> bool;

    fn mode(&self) -> PermissionMode;

    /// The directory the session works in, as the machine it runs on names it.
    fn cwd(&self) -> PathBuf;

    /// The checked-out git branch of the project.
    fn branch(&self) -> Option<String>;

    fn facts(&self) -> Facts;

    /// Every agent of the session, the lead first.
    fn roster(&self) -> Vec<MemberInfo>;

    /// The commands beside the UI's own, as `(name, description)`: those of extensions, then
    /// the skills as `skill:<name>`.
    fn commands(&self) -> Vec<(String, String)>;

    /// The thinking levels the model in use takes; none when that is not known.
    fn thinking_levels(&self) -> Vec<ThinkingLevel>;

    /// Whether `spec` names a model the session can use.
    fn resolves(&self, spec: &str) -> bool;

    /// The roles that were given models, as `@role`: what the cycling keys step through when
    /// there are no favourites.
    fn cycle_roles(&self) -> Vec<String>;

    /// Where the lead is in `cycle`: the entry it was put on.
    fn place_in(&self, cycle: &[String]) -> Option<usize> {
        let chain = self.status().chain;
        let [only] = chain.as_slice() else {
            return None;
        };
        let current = only.to_string();
        cycle.iter().position(|spec| *spec == current)
    }

    /// What the open session holds, to read. Changes go through the methods below.
    async fn session(&mut self) -> Arc<SessionStore>;

    /// The saved sessions of the project, most recently used first; with `query`, those whose
    /// messages contain it, each with the lines that matched.
    async fn sessions(&mut self, query: Option<&str>) -> Result<Vec<(SessionSummary, Vec<String>)>, String>;

    /// What the model picker offers, as `(spec, detail)`: the roles, then the models.
    async fn models(&mut self) -> Vec<(String, String)>;

    /// The model the lead's next request goes to, as `provider/id`.
    fn current_model(&self) -> Option<String>;

    /// The models of every provider, each saying whether its provider can be used.
    async fn model_rows(&mut self) -> Result<Vec<ModelRow>, String>;

    /// Every provider: whether it can be used, how, and how many models it has.
    async fn providers(&mut self) -> Result<Vec<ProviderRow>, String>;

    /// Asks `provider` for the models it serves now, beside the UI loop; the answer comes back
    /// as [`Internal::Models`].
    fn refresh_models(&mut self, provider: String, done: mpsc::UnboundedSender<Internal>);

    // ------------------------------------------------------------ talking to the lead

    /// Starts work on a prompt. `Err` is why it was rejected.
    async fn prompt(&mut self, text: String, images: Vec<Block>) -> Result<(), String>;

    /// Goes on with the run the end of a process cut short: tool calls without a result get
    /// one and the request nobody answered is sent again. `Err` when no run was interrupted.
    async fn continue_run(&mut self) -> Result<(), String>;

    /// Hands the working lead input before its next request.
    fn steer(&mut self, text: String, images: Vec<Block>);

    fn abort(&mut self);

    fn compact(&mut self, instructions: Option<String>);

    /// Puts the lead on `chain` without recording it: for one prompt, or back from one.
    fn set_chain(&mut self, chain: Vec<ModelSpec>);

    /// Puts the lead on the models named as typed, and records it. Returns the chain.
    async fn set_model(&mut self, specs: &[String]) -> Result<Vec<ModelSpec>, String>;

    async fn set_thinking(&mut self, level: ThinkingLevel) -> Result<(), String>;

    /// The model roles, where each one's value comes from, and the saved presets.
    async fn roles(&mut self) -> Result<Roles, String>;

    /// Points `role` at `chain`, or takes away the value `save` names; see `crate::roles::set`.
    async fn set_role(
        &mut self,
        role: &str,
        chain: Option<Vec<String>>,
        save: SaveTo,
    ) -> Result<Roles, String>;

    async fn save_preset(&mut self, name: &str) -> Result<Roles, String>;

    async fn apply_preset(&mut self, name: &str) -> Result<Roles, String>;

    async fn set_mode(&mut self, mode: PermissionMode) -> Result<(), String>;

    /// The primary agents the lead can be, in the order `tab` steps through them, and the name
    /// of the one it is.
    fn agents(&self) -> (Vec<PrimaryAgent>, String);

    /// Makes the lead the primary agent `name`.
    async fn set_agent(&mut self, name: &str) -> Result<(), String>;

    // ------------------------------------------------------------ the session and its tree

    async fn set_name(&mut self, name: String) -> Result<(), String>;

    /// Labels an entry of the tree; an empty label takes the label away.
    async fn set_label(&mut self, entry: &str, label: &str) -> Result<(), String>;

    /// Leaves the open session, or the place in it, for another.
    async fn go(&mut self, to: Go) -> Result<Moved, String>;

    /// The checkpoints to go back to, newest first, each with what changed in the files since.
    async fn rewind_points(&mut self) -> Result<Vec<(RewindPoint, String)>, String>;

    /// Goes back to a checkpoint: the files, the conversation, or both. `Some` when the
    /// conversation moved.
    async fn rewind(
        &mut self,
        point: &RewindPoint,
        code: bool,
        conversation: bool,
    ) -> Result<Option<Moved>, String>;

    /// Goes back to before the last prompt and sends it again.
    async fn retry(&mut self, again: Again) -> Result<Moved, String>;

    /// How an azazel that starts in this one's place opens this session again: the arguments
    /// that name it, and for a session of this process the model, the thinking level and the
    /// permission mode it is on now. `Err` says why it could not be opened again.
    fn reopen(&self) -> Result<Vec<std::ffi::OsString>, String>;

    /// Repairs the conversation so that a provider accepts it again (see [`crate::fix`]).
    /// Returns what was found, and how the session was opened again when something changed.
    async fn fix(&mut self) -> Result<(String, Option<Moved>), String> {
        Err("/fix works only in a session that runs in this process; this one runs elsewhere".to_string())
    }

    /// Writes the conversation to `path`; `redact` hides what looks like a secret in it.
    async fn export(&mut self, path: &str, redact: bool) -> Result<String, String>;

    // ------------------------------------------------------------ beside the conversation

    /// Runs a command in the user's shell where the session works; with `share` the lead is
    /// told. What it printed comes back as [`Internal::Shell`].
    fn shell(&mut self, command: String, share: bool, done: mpsc::UnboundedSender<Internal>);

    /// Runs a command of an extension, or a skill. Its outcome comes back as
    /// [`Internal::Command`].
    fn command(&mut self, name: &str, args: &str, done: mpsc::UnboundedSender<Internal>);

    /// What the session says about itself under `name` (`cost`, `tools`, ...); `None` when
    /// that is not a report.
    async fn report(&mut self, name: &str, args: &str) -> Option<Result<String, String>>;

    /// Asks a question on the side; the answer comes back as [`Internal::Btw`] updates
    /// carrying `id`.
    fn btw(
        &mut self,
        id: u64,
        question: String,
        earlier: Vec<Exchange>,
        updates: mpsc::UnboundedSender<Internal>,
        cancel: CancellationToken,
    ) -> Result<(), String>;

    /// Asks for the files `@` completes, when listing them takes a round trip; they come
    /// back as [`Internal::Files`]. A local session's files are read where they are needed.
    fn list_files(&mut self, _done: mpsc::UnboundedSender<Internal>) {}

    /// Stops a sub-agent and the agents it started, or `all`. Returns who was stopped.
    async fn stop_agent(&mut self, id: &str) -> Result<Vec<String>, String>;

    /// Messages a sub-agent. Returns who was reached.
    async fn message_agent(&mut self, id: &str, message: &str) -> Result<Vec<String>, String>;

    /// `@name text` typed at the prompt: `text` goes to the sub-agent with the id `name`, or
    /// starts an agent from the definition of that name. `None` when `name` is neither.
    async fn mention(&mut self, name: &str, text: &str) -> Option<Result<Mentioned, String>>;

    /// The definitions an agent can be started from, as `(name, description)`, for completing
    /// `@`. None for a session elsewhere, whose definitions are not this machine's.
    fn agent_defs(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Leaves the session as the UI ends: a local one is closed, a remote one goes on or
    /// ends as its own process decides.
    async fn finish(&mut self);
}

/// A session in this process.
pub(crate) struct Local {
    pub app: Arc<App>,
    pub active: Active,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    requests: mpsc::UnboundedReceiver<UiRequest>,
    team: Option<Arc<Team>>,
    team_events: Option<broadcast::Receiver<TeamEvent>>,
    /// Whether the session existed before the UI opened it.
    resumed: bool,
    /// The providers as configured, to tell where each one's credential comes from.
    providers: OnceLock<Vec<ProviderConfig>>,
    payer: Mutex<Option<Payer>>,
    /// The model lists providers gave this session.
    listed: Arc<crate::models::Listed>,
}

impl Local {
    /// Where role changes from the UI land: this session, whose lead follows `default`.
    fn roles_place(&self) -> crate::roles::Place<'_> {
        crate::roles::Place::of(&self.app, Some((&self.active.lead, Some(&*self.active.session))))
    }

    /// Takes the session over for the UI: agents reach the user through it from now on.
    pub(crate) fn new(app: Arc<App>, active: Active) -> Local {
        let (request_tx, requests) = mpsc::unbounded_channel();
        app.registry
            .services
            .insert(Arc::new(Ui(Arc::new(TuiBridge::new(request_tx)))));
        let team = app.registry.services.get::<Team>();
        Local {
            events: active.lead.subscribe(),
            requests,
            team_events: team.as_ref().map(|team| team.subscribe()),
            team,
            resumed: active.resumed,
            providers: OnceLock::new(),
            payer: Mutex::new(None),
            listed: Arc::default(),
            app,
            active,
        }
    }

    /// The providers as configured.
    fn configured(&self) -> &[ProviderConfig] {
        self.providers
            .get_or_init(|| presets::resolve(&self.app.config.providers).unwrap_or_default())
    }

    /// Requests moved to another account: who pays is asked anew for the next status line.
    pub(super) fn account_changed(&self) {
        *self.payer.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// How the lead's model is paid for (a subscription with its fullest usage window, or by
    /// the token), and the stored account that pays: its label and its e-mail address.
    fn billing(&self) -> (Billing, Option<String>, Option<String>) {
        let status = self.active.lead.status();
        let Some(model) = status
            .model
            .or_else(|| self.app.router.resolve(&status.chain).into_iter().next())
        else {
            return (Billing::Metered, None, None);
        };
        let (provider, model) = (model.provider, model.model);
        let mut payer = self.payer.lock().unwrap_or_else(PoisonError::into_inner);
        let known = payer
            .as_ref()
            .filter(|payer| payer.provider == provider && payer.asked.elapsed() < PAYER_FRESH);
        let account = match known {
            Some(payer) => payer.account.clone(),
            None => {
                let account = self
                    .configured()
                    .iter()
                    .find(|config| config.id == provider)
                    .and_then(|config| crate::models::account_in_use(&self.app.credentials, config))
                    .map(|account| InUse {
                        email: crate::usage::shown_email(&account, self.app.config.usage.redact_email),
                        label: account.label,
                        login: account.login,
                    });
                *payer = Some(Payer {
                    provider: provider.clone(),
                    asked: Instant::now(),
                    account: account.clone(),
                });
                account
            }
        };
        let billing = match &account {
            Some(InUse {
                label, login: true, ..
            }) => Billing::Subscription {
                window: crate::usage::fullest(&self.app.router.limits().windows(&provider, label), &model),
            },
            _ => Billing::Metered,
        };
        match account {
            Some(account) => (billing, Some(account.label), account.email),
            None => (billing, None, None),
        }
    }

    /// Where the open session's last run stopped, when the end of a process cut it short.
    fn interrupted(&self) -> Option<String> {
        self.active
            .session
            .interrupted_run(None)
            .map(|run| run.describe())
    }

    /// The environment of a UI on a local session: the application's own.
    pub(crate) fn env(&self) -> Env {
        Env {
            paths: self.app.paths.clone(),
            config: self.app.config.clone(),
            #[cfg(feature = "voice")]
            credentials: self.app.credentials.clone(),
        }
    }

    /// Replaces the open session with another one, keeping the model selection. When the other
    /// one cannot be opened, the session that was open is opened again.
    async fn open(&mut self, how: Open, source: StartSource) -> Result<Moved, String> {
        let status = self.active.lead.status();
        self.app.close(&self.active).await;
        let opened = self
            .app
            .open(how, status.chain.clone(), status.thinking_set)
            .await;
        let problem = match opened {
            Ok(active) => {
                self.events = active.lead.subscribe();
                self.active = active;
                self.app.start(source).await;
                return Ok(Moved {
                    resumed: self.active.resumed,
                    notice: self.active.notice.take(),
                    interrupted: self.interrupted(),
                });
            }
            Err(e) => format!("could not open the session: {e:#}"),
        };
        // Keep working in the session we had.
        let previous = Open::Store(self.active.session.clone());
        match self.app.open(previous, status.chain, status.thinking_set).await {
            Ok(active) => {
                self.events = active.lead.subscribe();
                self.active = active;
                Err(problem)
            }
            Err(e) => Err(format!(
                "{problem}; the previous session could not be reopened either: {e:#}"
            )),
        }
    }
}

#[async_trait]
impl Backend for Local {
    fn local(&self) -> Option<&Local> {
        Some(self)
    }

    async fn next(&mut self) -> Incoming {
        tokio::select! {
            event = self.events.recv() => match event {
                Some(event) => Incoming::Lead(event),
                None => Incoming::Closed("the agent stopped unexpectedly".to_string()),
            },
            Some(request) = self.requests.recv() => Incoming::Request(request),
            event = team::next_event(&mut self.team_events) => Incoming::Team(event),
        }
    }

    fn opened(&mut self) -> Moved {
        Moved {
            resumed: self.resumed,
            notice: self.active.notice.take(),
            interrupted: self.interrupted(),
        }
    }

    async fn started(&mut self) {
        let source = if self.resumed {
            StartSource::Resume
        } else {
            StartSource::Startup
        };
        self.app.start(source).await;
    }

    fn status(&self) -> AgentStatus {
        self.active.lead.status()
    }

    fn model_label(&self) -> String {
        crate::reports::model_label(&self.app, &self.active)
    }

    fn has_model(&self) -> bool {
        self.app.has_model(&self.active.lead.status().chain)
    }

    fn mode(&self) -> PermissionMode {
        self.app.permissions.mode()
    }

    fn cwd(&self) -> PathBuf {
        self.app.paths.cwd.clone()
    }

    fn branch(&self) -> Option<String> {
        azazel_core::prompt::git_branch(&self.app.paths.project_root)
    }

    fn facts(&self) -> Facts {
        let services = &self.app.registry.services;
        let (billing, account, account_email) = self.billing();
        Facts {
            coordinator: self.app.coordinator.is_on(),
            tier: self.app.router.tier(),
            goal: services
                .get::<Goals>()
                .and_then(|goals| goals.current())
                .filter(|goal| goal.status.is_open())
                .map(|goal| format!("goal {}, attempt {}", goal.status.label(), goal.iteration + 1)),
            todo: services
                .get::<TodoStore>()
                .map(|store| store.get(&self.active.lead.id))
                .unwrap_or_default(),
            billing,
            waiting: services
                .get::<crate::resets::Waits>()
                .and_then(|waits| waits.current()),
            account,
            account_email,
        }
    }

    fn roster(&self) -> Vec<MemberInfo> {
        self.team.as_ref().map(|team| team.list()).unwrap_or_default()
    }

    fn commands(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for command in self.app.registry.commands() {
            let about = match &command.argument_hint {
                Some(hint) => format!("{} {hint}", command.description),
                None => command.description,
            };
            out.push((command.name, about));
        }
        for skill in self.app.registry.skills.list() {
            if skill.user_invocable {
                out.push((format!("skill:{}", skill.name), skill.description));
            }
        }
        out
    }

    fn thinking_levels(&self) -> Vec<ThinkingLevel> {
        let status = self.active.lead.status();
        let model = status
            .model
            .or_else(|| self.app.router.resolve(&status.chain).into_iter().next());
        model
            .map(|model| {
                self.app
                    .router
                    .catalog()
                    .get(&model.provider, &model.model)
                    .thinking_levels
            })
            .unwrap_or_default()
    }

    fn resolves(&self, spec: &str) -> bool {
        spec.parse::<ModelSpec>()
            .is_ok_and(|spec| !self.app.router.resolve(&[spec]).is_empty())
    }

    fn cycle_roles(&self) -> Vec<String> {
        let router = &self.app.router;
        let roles = router.roles().into_keys().map(|role| {
            let models = router.resolve(&[ModelSpec::Role(role.clone())]);
            (role, models.iter().map(ToString::to_string).collect())
        });
        cycle_of(roles.collect())
    }

    /// The entry the lead was put on, or else the first one that answers with the model the
    /// lead answers with: a lead on `@default` is on the favourite that names its model.
    fn place_in(&self, cycle: &[String]) -> Option<usize> {
        let chain = self.active.lead.status().chain;
        if let [only] = chain.as_slice() {
            let current = only.to_string();
            if let Some(at) = cycle.iter().position(|spec| *spec == current) {
                return Some(at);
            }
        }
        let router = &self.app.router;
        let answers = router.resolve(&chain).into_iter().next()?;
        cycle.iter().position(|spec| {
            spec.parse::<ModelSpec>()
                .is_ok_and(|spec| router.resolve(&[spec]).first() == Some(&answers))
        })
    }

    async fn session(&mut self) -> Arc<SessionStore> {
        self.active.session.clone()
    }

    async fn sessions(&mut self, query: Option<&str>) -> Result<Vec<(SessionSummary, Vec<String>)>, String> {
        let dir = self.app.paths.sessions_dir();
        Ok(match query {
            Some(query) => SessionStore::search(&dir, query, slash::SEARCH_LINES),
            None => SessionStore::list(&dir)
                .into_iter()
                .map(|session| (session, Vec::new()))
                .collect(),
        })
    }

    async fn models(&mut self) -> Vec<(String, String)> {
        let router = &self.app.router;
        let mut rows = Vec::new();
        for role in router.roles().keys() {
            let resolved = router.resolve(&[ModelSpec::Role(role.clone())]);
            if let Some(first) = resolved.first() {
                rows.push((format!("@{role}"), first.to_string()));
            }
        }
        for name in self.app.virtual_models() {
            rows.push((name, crate::models::VIRTUAL_DETAIL.to_string()));
        }
        // Providers that can be reached: a credential is present, or none is needed.
        let configured = self.configured();
        for provider in router.provider_ids() {
            let config = configured.iter().find(|p| p.id == provider);
            let usable = config.is_none_or(|config| crate::models::usable(&self.app.credentials, config));
            // `--provider` leaves one provider to choose from.
            if !usable || self.app.provider.as_deref().is_some_and(|only| only != provider) {
                continue;
            }
            // A provider used through a subscription login is not paid for by the token.
            let login = config.is_some_and(|config| crate::models::login(&self.app.credentials, config));
            for model in router.catalog().for_provider(provider) {
                let detail = crate::settings::model_detail(model.context_window, model.cost.as_ref(), login);
                rows.push((model.key(), detail));
            }
        }
        rows
    }

    fn current_model(&self) -> Option<String> {
        let status = self.active.lead.status();
        let model = self
            .app
            .router
            .resolve(&status.chain)
            .into_iter()
            .next()
            .or(status.model)?;
        Some(format!("{}/{}", model.provider, model.model))
    }

    async fn model_rows(&mut self) -> Result<Vec<ModelRow>, String> {
        Ok(crate::models::rows(
            &self.app.router,
            &self.listed,
            self.configured(),
            &self.app.credentials,
            self.app.provider.as_deref(),
            true,
        ))
    }

    async fn providers(&mut self) -> Result<Vec<ProviderRow>, String> {
        Ok(crate::models::provider_rows(
            &self.app.router,
            &self.listed,
            self.configured(),
            &self.app.credentials,
            self.app.provider.as_deref(),
        ))
    }

    fn refresh_models(&mut self, provider: String, done: mpsc::UnboundedSender<Internal>) {
        let (app, listed) = (self.app.clone(), self.listed.clone());
        tokio::spawn(async move {
            let cache = app.paths.models_cache();
            let result = crate::models::refresh(&app.router, &listed, &cache, &provider)
                .await
                .map(|models| {
                    let config = presets::resolve(&app.config.providers).unwrap_or_default();
                    let (usable, login) = crate::models::standing(&provider, &config, &app.credentials);
                    models
                        .iter()
                        .map(|model| crate::models::model_row(model, usable, login))
                        .collect()
                });
            let _ = done.send(Internal::Models(super::models::Update::Refreshed {
                provider,
                result,
            }));
        });
    }

    async fn prompt(&mut self, text: String, images: Vec<Block>) -> Result<(), String> {
        self.app.submit(&self.active, text, images).await
    }

    async fn continue_run(&mut self) -> Result<(), String> {
        self.app.continue_run(&self.active).map(|_| ())
    }

    fn steer(&mut self, text: String, mut images: Vec<Block>) {
        let mut content = vec![Block::text(text)];
        content.append(&mut images);
        self.active.lead.steer(UserInput::blocks(content));
    }

    fn abort(&mut self) {
        self.active.lead.abort();
    }

    fn compact(&mut self, instructions: Option<String>) {
        self.active.lead.send(AgentCmd::Compact { instructions });
    }

    fn set_chain(&mut self, chain: Vec<ModelSpec>) {
        self.active.lead.send(AgentCmd::SetModel(chain));
    }

    async fn set_model(&mut self, specs: &[String]) -> Result<Vec<ModelSpec>, String> {
        let chain = self
            .app
            .model_specs(specs)
            .map_err(|e| e.trim_start_matches("--model ").to_string())?;
        if self.app.router.resolve(&chain).is_empty() {
            return Err("that selection resolves to no model".to_string());
        }
        self.active.lead.send(AgentCmd::SetModel(chain.clone()));
        // Recorded, so that a resumed session comes back on it.
        self.active
            .session
            .append(EntryKind::ModelChange { chain: chain.clone() });
        Ok(chain)
    }

    async fn roles(&mut self) -> Result<Roles, String> {
        Ok(crate::roles::list(&self.roles_place()))
    }

    async fn set_role(
        &mut self,
        role: &str,
        chain: Option<Vec<String>>,
        save: SaveTo,
    ) -> Result<Roles, String> {
        let chain = chain.as_deref().map(crate::roles::parse_chain).transpose()?;
        crate::roles::set(&self.roles_place(), role, chain, save)
    }

    async fn save_preset(&mut self, name: &str) -> Result<Roles, String> {
        crate::roles::save_preset(&self.roles_place(), name)
    }

    async fn apply_preset(&mut self, name: &str) -> Result<Roles, String> {
        crate::roles::apply_preset(&self.roles_place(), name)
    }

    async fn set_thinking(&mut self, level: ThinkingLevel) -> Result<(), String> {
        self.active.lead.send(AgentCmd::SetThinking(Some(level)));
        self.active.session.append(EntryKind::ThinkingChange {
            level: level.to_string(),
        });
        Ok(())
    }

    async fn set_mode(&mut self, mode: PermissionMode) -> Result<(), String> {
        self.app.permissions.set_mode(mode);
        Ok(())
    }

    fn agents(&self) -> (Vec<PrimaryAgent>, String) {
        let lead = &self.app.lead_agent;
        let agents = lead
            .primaries()
            .iter()
            .map(|def| PrimaryAgent {
                name: def.name.clone(),
                description: def.description.clone(),
                color: def.color.clone(),
            })
            .collect();
        (agents, lead.name())
    }

    async fn set_agent(&mut self, name: &str) -> Result<(), String> {
        self.app.set_agent(name).map(|_| ())
    }

    async fn set_name(&mut self, name: String) -> Result<(), String> {
        self.active.session.append(EntryKind::SessionInfo { name });
        Ok(())
    }

    async fn set_label(&mut self, entry: &str, label: &str) -> Result<(), String> {
        self.active.session.append(EntryKind::Label {
            target_id: entry.to_string(),
            label: label.to_string(),
        });
        Ok(())
    }

    async fn go(&mut self, to: Go) -> Result<Moved, String> {
        match to {
            Go::New => self.open(Open::New, StartSource::Clear).await,
            Go::File(path) => self.open(Open::File(path), StartSource::Resume).await,
            Go::Fork => {
                let fork = self
                    .active
                    .session
                    .fork(&self.app.paths.sessions_dir(), None)
                    .map_err(|e| format!("could not fork the session: {e}"))?;
                self.open(Open::Store(Arc::new(fork)), StartSource::Resume).await
            }
            Go::Leaf { entry, before } => {
                let session = self.active.session.clone();
                let left = session.leaf();
                let target = if before {
                    session.entry(&entry).and_then(|entry| entry.parent_id)
                } else {
                    Some(entry)
                };
                if !session.set_leaf(target.as_deref()) {
                    return Err("that entry is no longer part of the session".to_string());
                }
                let moved = self.open(Open::Store(session), StartSource::Resume).await?;
                // What happened on the branch that was left is written up for the agent.
                crate::branch::summarise_left(&self.app, &self.active, left.as_deref());
                Ok(moved)
            }
        }
    }

    async fn rewind_points(&mut self) -> Result<Vec<(RewindPoint, String)>, String> {
        let points = slash::rewind_points(&self.active.session);
        if points.is_empty() {
            return Err(match self.active.checkpoints.disabled().await {
                Some(reason) => format!("nothing to rewind to: {reason}"),
                None => "nothing to rewind to: no prompt has been sent in this session".to_string(),
            });
        }
        let checkpoints = self.active.checkpoints.clone();
        let now = checkpoints.snapshot("rewind list").await.ok().flatten();
        let mut listed = Vec::new();
        for point in points {
            let changed = match &now {
                Some(now) => checkpoints.shortstat(&point.commit, now).await,
                None => String::new(),
            };
            listed.push((point, changed.trim().to_string()));
        }
        Ok(listed)
    }

    async fn rewind(
        &mut self,
        point: &RewindPoint,
        code: bool,
        conversation: bool,
    ) -> Result<Option<Moved>, String> {
        let session = self.active.session.clone();
        let undo = if code {
            let before = self.active.checkpoints.restore(&point.commit).await;
            Some(before.map_err(|e| format!("could not restore the files: {e}"))?)
        } else {
            None
        };
        if conversation {
            let resume_at = session.entry(&point.entry_id).and_then(|entry| entry.parent_id);
            session.set_leaf(resume_at.as_deref());
        }
        // The files as they were just now stay reachable, so the rewind can itself be rewound.
        if let Some(commit) = undo {
            let turn_entry_id = session.leaf().unwrap_or_default();
            session.append(EntryKind::Checkpoint {
                turn_entry_id,
                commit,
            });
        }
        if conversation {
            return self
                .open(Open::Store(session), StartSource::Resume)
                .await
                .map(Some);
        }
        let target = if point.prompt.is_empty() {
            "before my previous rewind".to_string()
        } else {
            format!("before this prompt: {}", super::first_line(&point.prompt))
        };
        let note = format!(
            "{} to how they were {target}\nFiles you read or changed since then may differ \
             now; read them again before editing.",
            slash::REWIND_NOTE
        );
        self.active.lead.send(AgentCmd::Append(UserInput::text(note)));
        Ok(None)
    }

    async fn retry(&mut self, again: Again) -> Result<Moved, String> {
        let session = self.active.session.clone();
        if !session.set_leaf(again.parent.as_deref()) {
            return Err("that prompt is no longer part of the session".to_string());
        }
        let moved = self.open(Open::Store(session), StartSource::Resume).await?;
        self.app
            .submit(&self.active, again.text, again.images)
            .await
            .map_err(|reason| format!("prompt rejected: {reason}"))?;
        Ok(moved)
    }

    fn reopen(&self) -> Result<Vec<std::ffi::OsString>, String> {
        use std::ffi::OsString;
        let file = self.active.session.path();
        if !file.is_file() {
            return Err(
                "this session keeps no file (--no-session), so an azazel that starts again could not open it"
                    .to_string(),
            );
        }
        let status = self.active.lead.status();
        let mut args: Vec<OsString> = Vec::new();
        for spec in &status.chain {
            args.extend(["--model".into(), spec.to_string().into()]);
        }
        if let Some(level) = status.thinking_set {
            args.extend(["--thinking".into(), level.to_string().into()]);
        }
        args.extend(["--mode".into(), self.app.permissions.mode().to_string().into()]);
        args.extend(["-r".into(), file.into()]);
        Ok(args)
    }

    async fn fix(&mut self) -> Result<(String, Option<Moved>), String> {
        let session = self.active.session.clone();
        let fixed = crate::fix::fix(&session);
        if !fixed.changed() {
            return Ok((fixed.report(), None));
        }
        // The lead is started again on the conversation as it is now.
        let moved = self.open(Open::Store(session), StartSource::Resume).await?;
        Ok((fixed.report(), Some(moved)))
    }

    async fn export(&mut self, path: &str, redact: bool) -> Result<String, String> {
        use crate::export::{Format, default_file_name, render};
        let session = &self.active.session;
        let cwd = &self.app.paths.cwd;
        let file = if path.is_empty() {
            cwd.join(default_file_name(session, Format::Markdown))
        } else {
            cwd.join(path)
        };
        let format = Format::from_path(&file).unwrap_or(Format::Markdown);
        std::fs::write(&file, render(session, format, redact))
            .map_err(|e| format!("could not write {}: {e}", file.display()))?;
        Ok(file.display().to_string())
    }

    /// Outside the permission engine and the push guard: it is the user's own command.
    fn shell(&mut self, command: String, share: bool, done: mpsc::UnboundedSender<Internal>) {
        let shell = self.app.shell.clone();
        let cwd = self.app.paths.cwd.clone();
        let lead = self.active.lead.clone();
        tokio::spawn(async move {
            let mut options = RunOptions::new(cwd);
            options.keep = (200, 800);
            let (output, status) = match run_command(&shell, &command, options).await {
                Ok(result) => {
                    let status = match result.code {
                        Some(code) => format!("exit code: {code}"),
                        None => "killed".to_string(),
                    };
                    (result.output.render(), status)
                }
                Err(e) => (String::new(), format!("could not start: {e}")),
            };
            if share {
                // The agent sees what the user ran, without being asked to respond.
                let note = format!("{SHELL_NOTE}{command}` myself:\n{output}\n{status}");
                lead.send(AgentCmd::Append(UserInput::text(note)));
            }
            let _ = done.send(Internal::Shell { output, status });
        });
    }

    /// Registered commands run beside the UI loop: some wait on the network or on the user.
    fn command(&mut self, name: &str, args: &str, done: mpsc::UnboundedSender<Internal>) {
        let outcome = |outcome| drop(done.send(Internal::Command(outcome)));
        if let Some(skill_name) = name.strip_prefix("skill:") {
            let skills = self.app.registry.skills.clone();
            return match skills.get(skill_name).filter(|skill| skill.user_invocable) {
                Some(skill) => {
                    skills.mark_used(&skill);
                    azazel_core::skills::allow_for_prompt(&self.app.registry.services, &skill);
                    outcome(CommandOutcome::Prompt {
                        text: invocation_text(&skill, args),
                        model: skill.model.clone(),
                    });
                }
                None => outcome(CommandOutcome::Error(format!(
                    "no skill named `{skill_name}`; /skills lists them"
                ))),
            };
        }
        let Some(command) = self.app.registry.command(name) else {
            return outcome(CommandOutcome::Error(format!(
                "unknown command /{name}; /help lists commands"
            )));
        };
        let context = self.app.registry.bus.ctx();
        let args = args.to_string();
        tokio::spawn(async move {
            let _ = done.send(Internal::Command(command.handler.run(&args, &context).await));
        });
    }

    async fn report(&mut self, name: &str, args: &str) -> Option<Result<String, String>> {
        crate::reports::run(&self.app, &self.active, name, args).await
    }

    fn btw(
        &mut self,
        id: u64,
        question: String,
        earlier: Vec<Exchange>,
        updates: mpsc::UnboundedSender<Internal>,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let asked = Btw::prepare(
            &self.app.registry.services,
            &self.active.session,
            &self.active.lead,
            &earlier,
            &question,
        )?;
        tokio::spawn(async move {
            let text = updates.clone();
            let mut on_text = move |delta: &str| {
                let delta = delta.to_string();
                let _ = text.send(Internal::Btw(btw::Update::Text { id, delta }));
            };
            let result = asked.ask(&mut on_text, cancel).await;
            let _ = updates.send(Internal::Btw(btw::Update::Done { id, result }));
        });
        Ok(())
    }

    async fn stop_agent(&mut self, id: &str) -> Result<Vec<String>, String> {
        let team = self.team.as_ref().ok_or("this session has no team")?;
        match id {
            "all" => Ok(team.stop_all()),
            id => team.stop(id),
        }
    }

    async fn message_agent(&mut self, id: &str, message: &str) -> Result<Vec<String>, String> {
        let team = self.team.as_ref().ok_or("this session has no team")?;
        team.send_from_user(id, message)
    }

    async fn mention(&mut self, name: &str, text: &str) -> Option<Result<Mentioned, String>> {
        self.team.as_ref()?.mention(name, text).await
    }

    fn agent_defs(&self) -> Vec<(String, String)> {
        let Some(team) = &self.team else {
            return Vec::new();
        };
        team.defs()
            .subagents()
            .iter()
            .map(|def| (def.name.clone(), def.description.clone()))
            .collect()
    }

    async fn finish(&mut self) {
        self.app.finish(&self.active).await;
    }
}

#[cfg(test)]
mod tests {
    use azazel_ai::{Credential, Credentials};

    use super::*;
    use crate::models::account_in_use;

    fn login() -> Credential {
        Credential::OAuth {
            access: "access".to_string(),
            refresh: "refresh".to_string(),
            expires: u64::MAX,
            extra: Default::default(),
        }
    }

    fn key() -> Credential {
        Credential::ApiKey {
            key: "sk-test".to_string(),
        }
    }

    #[test]
    fn the_roles_to_step_through_are_those_that_answer_with_models_of_their_own() {
        let role = |name: &str, models: &[&str]| {
            (
                name.to_string(),
                models
                    .iter()
                    .map(|model| (*model).to_string())
                    .collect::<Vec<_>>(),
            )
        };
        let cycle = cycle_of(vec![
            role("commit", &["mock/small"]),
            role("default", &["mock/lead", "mock/spare"]),
            role("lead", &["mock/lead", "mock/spare"]),
            role("plan", &["mock/lead"]),
            role("slow", &["mock/deep"]),
            role("smol", &["mock/small"]),
            role("vision", &[]),
        ]);
        // `default`, `smol` and `slow` come first, the others by name. `commit` and `lead`
        // answer with what `smol` and `default` answer with, and `vision` with nothing.
        assert_eq!(cycle, ["@default", "@smol", "@slow", "@plan"]);
        assert_eq!(cycle_of(Vec::new()), Vec::<String>::new());
    }

    /// The account of `provider` whose sign-in the next request would use; `None` when the
    /// request would carry an API key (from a flag, the environment, a command, the
    /// configuration or the credential store) or no credential at all.
    fn login_in_use(credentials: &Credentials, provider: &ProviderConfig) -> Option<String> {
        account_in_use(credentials, provider)
            .filter(|account| account.login)
            .map(|account| account.label)
    }

    fn credentials(dir: &std::path::Path, env: &'static [(&'static str, &'static str)]) -> Credentials {
        Credentials::with_env(
            dir.join("auth.json"),
            Arc::new(move |name: &str| {
                env.iter()
                    .find(|(known, _)| *known == name)
                    .map(|(_, value)| (*value).to_string())
            }),
        )
    }

    fn provider(id: &str) -> ProviderConfig {
        presets::presets()
            .into_iter()
            .find(|provider| provider.id == id)
            .unwrap()
    }

    #[test]
    fn a_sign_in_is_a_subscription_and_any_key_is_paid_by_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let anthropic = provider("anthropic");
        let stored = credentials(dir.path(), &[]);
        assert_eq!(login_in_use(&stored, &anthropic), None, "nothing is stored yet");

        stored.store("anthropic", login()).unwrap();
        assert_eq!(login_in_use(&stored, &anthropic).as_deref(), Some("default"));
        // The sign-in of one provider says nothing about another.
        assert_eq!(login_in_use(&stored, &provider("openai")), None);

        // A key in the environment is what requests carry, whatever is stored.
        let with_key = credentials(dir.path(), &[("ANTHROPIC_API_KEY", "sk-ant-test")]);
        assert_eq!(login_in_use(&with_key, &anthropic), None);
        // So is a key given on the command line.
        stored.set_flag_key("anthropic", "sk-ant-flag".to_string());
        assert_eq!(login_in_use(&stored, &anthropic), None);

        // A stored key is a key.
        let other = tempfile::tempdir().unwrap();
        let keyed = credentials(other.path(), &[]);
        keyed.store("anthropic", key()).unwrap();
        assert_eq!(login_in_use(&keyed, &anthropic), None);
        // It is an account all the same: the `account` segment names it.
        let account = account_in_use(&keyed, &anthropic).unwrap();
        assert_eq!((account.label.as_str(), account.login), ("default", false));
        // A key from elsewhere belongs to no stored account.
        assert!(account_in_use(&with_key, &anthropic).is_none());
    }

    #[test]
    fn the_selected_account_decides_between_a_stored_key_and_a_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let anthropic = provider("anthropic");
        let stored = credentials(dir.path(), &[]);
        stored.store("anthropic", key()).unwrap();
        stored
            .store(&azazel_ai::credentials::account_key("anthropic", "work"), login())
            .unwrap();
        assert_eq!(
            login_in_use(&stored, &anthropic),
            None,
            "the default account is a key"
        );
        stored.select("anthropic", "work");
        assert_eq!(login_in_use(&stored, &anthropic).as_deref(), Some("work"));
    }
}
