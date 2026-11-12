//! One extension script: its Luau VM, what it registered while loading, and how calls into it run.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use azazel_agent::{Access, AgentInfo, Services};
use azazel_ai::ProviderConfig;
use azazel_core::{EventKind, Level, PermissionState, Ui, UiExtras};
use mlua::chunk::ChunkMode;
use mlua::{FromLuaMulti, Function, IntoLuaMulti, Lua, Table, VmState};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::confine::Roots;

/// The extension id everything the host registers is attributed to.
pub(crate) const OWNER: &str = "lua";

/// How long the script's own code may run in one event handler, command, shortcut or load. Time
/// spent waiting for a blocking `azazel` function does not count.
pub(crate) const BUDGET: Duration = Duration::from_secs(5);

pub(crate) fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The message of `error` without the Rust and Luau tracebacks around it.
pub(crate) fn describe(error: &mlua::Error) -> String {
    let mut error = error;
    while let mlua::Error::CallbackError { cause, .. } | mlua::Error::WithContext { cause, .. } = error {
        error = &**cause;
    }
    let text = match error {
        mlua::Error::RuntimeError(message) | mlua::Error::SyntaxError { message, .. } => message.clone(),
        other => other.to_string(),
    };
    let message = text.split("\nstack traceback:").next().unwrap_or_default();
    message.trim().to_string()
}

pub(crate) struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub access: Access,
    pub execute: Function,
}

pub(crate) struct CommandDef {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub run: Function,
}

/// What a script registered while its top level ran.
#[derive(Default)]
pub(crate) struct Pending {
    pub tools: Vec<ToolDef>,
    /// `(event name, kind, handler)`.
    pub handlers: Vec<(&'static str, EventKind, Function)>,
    pub commands: Vec<CommandDef>,
    /// `(part name, function that renders it)`.
    pub prompts: Vec<(String, Function)>,
    /// `(key combination, handler)`.
    pub shortcuts: Vec<(String, Function)>,
    pub providers: Vec<ProviderConfig>,
}

/// What bounds the script code running right now.
#[derive(Default)]
struct Limit {
    deadline: Option<Instant>,
    cancel: Option<CancellationToken>,
}

/// Keys of the status segments and widgets a script has on screen.
#[derive(Default)]
struct Shown {
    status: Vec<String>,
    widgets: Vec<String>,
}

/// The part of a script its `azazel` functions capture. It holds no Luau values, so the VM is
/// freed as soon as the host lets go of the [`Script`].
pub(crate) struct Shared {
    /// File stem, or directory name for `<dir>/init.luau`.
    pub id: String,
    /// The script file.
    pub path: PathBuf,
    pub services: Arc<Services>,
    pub roots: Roots,
    pub data_dir: PathBuf,
    state_file: PathBuf,
    /// True while the top level runs: the only time registering functions are valid.
    loading: AtomicBool,
    /// Why the script is disabled.
    error: Mutex<Option<String>>,
    /// Held whenever script code runs, so that one piece of it runs at a time.
    entry: Mutex<()>,
    limit: Mutex<Limit>,
    /// The persistent key-value store, read from `state_file` on first use.
    state: Mutex<Option<Map<String, Value>>>,
    pending: Mutex<Pending>,
    shown: Mutex<Shown>,
}

impl Shared {
    pub fn new(
        id: String,
        path: PathBuf,
        services: Arc<Services>,
        roots: Roots,
        data_dir: PathBuf,
        state_file: PathBuf,
    ) -> Shared {
        Shared {
            id,
            path,
            services,
            roots,
            data_dir,
            state_file,
            loading: AtomicBool::new(false),
            error: Mutex::default(),
            entry: Mutex::default(),
            limit: Mutex::default(),
            state: Mutex::default(),
            pending: Mutex::default(),
            shown: Mutex::default(),
        }
    }

    /// Why the script is disabled, if it is.
    pub fn error(&self) -> Option<String> {
        locked(&self.error).clone()
    }

    /// Stops the script from acting and takes what it shows off the screen. The first reason is
    /// kept and the user is told once.
    pub fn disable(&self, why: String) {
        {
            let mut error = locked(&self.error);
            if error.is_some() {
                return;
            }
            *error = Some(why.clone());
        }
        if let Some(extras) = self.services.get::<UiExtras>() {
            let shown = std::mem::take(&mut *locked(&self.shown));
            for key in shown.status {
                extras.set_status(OWNER, &key, None);
            }
            for key in shown.widgets {
                extras.set_widget(OWNER, &key, None);
            }
        }
        let text = format!("extension `{}` is disabled: {why}", self.id);
        match self.services.get::<Ui>() {
            Some(ui) => ui.0.notify(Level::Error, &text),
            None => tracing::error!("{text}"),
        }
    }

    /// Fails unless the script's top level is running.
    pub fn registering(&self, function: &str) -> mlua::Result<()> {
        if self.loading.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(mlua::Error::runtime(format!(
                "{function} can only be called while the script loads, not from a tool, handler or command"
            )))
        }
    }

    /// Fails while the script's top level is running, where nothing can be waited for.
    pub fn running(&self, function: &str) -> mlua::Result<()> {
        if self.loading.load(Ordering::SeqCst) {
            Err(mlua::Error::runtime(format!(
                "{function} cannot be called while the script loads; call it from a tool, handler or command"
            )))
        } else {
            Ok(())
        }
    }

    pub fn pending(&self) -> MutexGuard<'_, Pending> {
        locked(&self.pending)
    }

    /// The token that cancels the call running right now; one that never fires when there is none.
    pub fn current_cancel(&self) -> CancellationToken {
        locked(&self.limit).cancel.clone().unwrap_or_default()
    }

    /// Sets the status-line segment `key` of this script; `None` removes it.
    pub fn set_status(&self, key: &str, text: Option<String>) {
        let Some(extras) = self.services.get::<UiExtras>() else {
            return;
        };
        let key = format!("{}/{key}", self.id);
        let mut shown = locked(&self.shown);
        shown.status.retain(|k| *k != key);
        if text.is_some() {
            shown.status.push(key.clone());
        }
        extras.set_status(OWNER, &key, text);
    }

    /// Sets the widget `key` of this script; `None` removes it.
    pub fn set_widget(&self, key: &str, lines: Option<Vec<String>>) {
        let Some(extras) = self.services.get::<UiExtras>() else {
            return;
        };
        let key = format!("{}/{key}", self.id);
        let mut shown = locked(&self.shown);
        shown.widgets.retain(|k| *k != key);
        if lines.is_some() {
            shown.widgets.push(key.clone());
        }
        extras.set_widget(OWNER, &key, lines);
    }

    /// Runs `f` on the store, reading it from disk first when this is its first use.
    fn with_state<R>(
        &self,
        f: impl FnOnce(&mut Map<String, Value>) -> Result<R, String>,
    ) -> Result<R, String> {
        let mut state = locked(&self.state);
        let map = match state.take() {
            Some(map) => map,
            None => {
                let file = self.state_file.display();
                match std::fs::read_to_string(&self.state_file) {
                    Ok(text) => serde_json::from_str(&text)
                        .map_err(|e| format!("the state file {file} is not a JSON object: {e}"))?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
                    Err(e) => return Err(format!("cannot read the state file {file}: {e}")),
                }
            }
        };
        f(state.insert(map))
    }

    /// The stored value of `key`.
    pub fn state_get(&self, key: &str) -> Result<Option<Value>, String> {
        self.with_state(|map| Ok(map.get(key).cloned()))
    }

    /// Stores `value` under `key` (`None` removes the key) and writes the store to disk.
    pub fn state_set(&self, key: &str, value: Option<Value>) -> Result<(), String> {
        self.with_state(|map| {
            match value {
                Some(value) => {
                    map.insert(key.to_string(), value);
                }
                None => {
                    map.remove(key);
                }
            }
            let file = &self.state_file;
            let failed = |e: std::io::Error| format!("cannot write the state file {}: {e}", file.display());
            let text = serde_json::to_string_pretty(map).map_err(|e| format!("cannot store `{key}`: {e}"))?;
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir).map_err(failed)?;
            }
            let partial = file.with_extension("json.tmp");
            std::fs::write(&partial, text).map_err(failed)?;
            std::fs::rename(&partial, file).map_err(failed)
        })
    }

    /// Runs `f`, which enters the VM, alone and under `budget` and `cancel`.
    fn enter<R>(
        &self,
        budget: Option<Duration>,
        cancel: Option<CancellationToken>,
        f: impl FnOnce() -> R,
    ) -> (R, Duration) {
        let _entry = locked(&self.entry);
        let started = Instant::now();
        *locked(&self.limit) = Limit {
            deadline: budget.map(|budget| started + budget),
            cancel,
        };
        let result = f();
        *locked(&self.limit) = Limit::default();
        (result, started.elapsed())
    }

    /// Called by the VM at every loop iteration and function call of the script.
    fn interrupt(&self) -> mlua::Result<VmState> {
        let limit = locked(&self.limit);
        if limit.cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
            return Err(mlua::Error::runtime("cancelled"));
        }
        if limit.deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(mlua::Error::runtime(format!(
                "stopped after running for {} s without finishing",
                BUDGET.as_secs()
            )));
        }
        Ok(VmState::Continue)
    }
}

/// A loaded script. Dropping it frees its VM.
pub(crate) struct Script {
    pub shared: Arc<Shared>,
    /// `ctx.ui`, the same table for every call.
    ui: Table,
    lua: Lua,
}

impl Script {
    /// Creates the sandboxed VM of the script and runs its top level. What the script registered
    /// is left in [`Shared::pending`].
    pub fn load(shared: Arc<Shared>) -> Result<Script, String> {
        let source = std::fs::read(&shared.path).map_err(|e| format!("cannot read the script: {e}"))?;
        let lua = Lua::new();
        let ui = crate::api::install(&lua, &shared).map_err(|e| describe(&e))?;
        lua.sandbox(true).map_err(|e| describe(&e))?;
        let watched = shared.clone();
        lua.set_interrupt(move |_| watched.interrupt());

        let chunk = lua
            .load(source)
            .set_name(format!("@{}", shared.path.display()))
            .set_mode(ChunkMode::Text);
        shared.loading.store(true, Ordering::SeqCst);
        let (ran, _) = shared.enter(Some(BUDGET), None, || chunk.exec());
        shared.loading.store(false, Ordering::SeqCst);
        ran.map_err(|e| describe(&e))?;
        Ok(Script { shared, ui, lua })
    }

    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Starts a call of `function`, a function of this script. With a `budget` the call fails once
    /// the script's own code has run that long; with `cancel` it fails at the script's next step
    /// after the token fires.
    pub fn call<R: FromLuaMulti>(
        &self,
        function: &Function,
        args: impl IntoLuaMulti,
        budget: Option<Duration>,
        cancel: Option<CancellationToken>,
    ) -> Run<'_, R> {
        Run {
            call: function.call_async(args),
            shared: &self.shared,
            budget,
            cancel,
        }
    }

    /// The `ctx` argument of a tool, handler, command or shortcut.
    pub fn ctx(&self, cwd: &Path, session_id: &str, agent: Option<&AgentInfo>) -> mlua::Result<Table> {
        let mode = self.shared.services.get::<PermissionState>().map(|state| {
            let mode = match agent {
                Some(agent) => state.mode_for(agent),
                None => state.mode(),
            };
            mode.as_str()
        });
        let ctx = self.lua.create_table()?;
        ctx.set("cwd", &*cwd.to_string_lossy())?;
        ctx.set("session_id", session_id)?;
        ctx.set(
            "agent_id",
            agent.map_or(azazel_team::LEAD, |agent| agent.id.as_str()),
        )?;
        ctx.set("mode", mode)?;
        ctx.set("ui", &self.ui)?;
        ctx.set_readonly(true);
        Ok(ctx)
    }
}

/// A call into a script, driven to completion by awaiting it. The script's code runs only inside
/// `poll`, one call at a time; while the call waits for a blocking `azazel` function other calls
/// into the same script proceed, so a script can handle the events its own waiting causes.
pub(crate) struct Run<'a, R: FromLuaMulti> {
    call: mlua::function::AsyncCallFuture<R>,
    shared: &'a Shared,
    /// Running time the script's code has left.
    budget: Option<Duration>,
    cancel: Option<CancellationToken>,
}

impl<R: FromLuaMulti> Future for Run<'_, R> {
    type Output = mlua::Result<R>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let call = Pin::new(&mut this.call);
        let (polled, spent) = this
            .shared
            .enter(this.budget, this.cancel.clone(), || call.poll(cx));
        if let Some(budget) = &mut this.budget {
            *budget = budget.saturating_sub(spent);
        }
        polled
    }
}
