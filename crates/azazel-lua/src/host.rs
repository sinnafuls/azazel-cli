//! The host: finds the scripts, loads each into its own VM, hands what they registered to the
//! registrar, and provides `/ext` and `/reload`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use azazel_agent::{AgentInfo, Tool, ToolSource};
use azazel_ai::providers::http::{HttpProvider, http_client};
use azazel_ai::{Credentials, StallLimits};
use azazel_core::paths::canonical;
use azazel_core::{
    Command, CommandHandler, CommandOutcome, CommandSource, ExtCtx, ExtError, Paths, Registrar, Registry,
    RegistryHandle, UiExtras,
};

use crate::LuaExt;
use crate::bridge::{LuaCommand, LuaHandler, LuaPrompt, LuaRouter, LuaShortcut, LuaTool};
use crate::confine::Roots;
use crate::script::{OWNER, Script, Shared, locked};

/// What the scripts of the latest load contribute.
#[derive(Default)]
struct Loaded {
    /// Every script found, loaded or not, in load order.
    scripts: Vec<Arc<Shared>>,
    tools: Vec<Arc<LuaTool>>,
    commands: Vec<(Arc<Shared>, Command)>,
}

pub(crate) struct Host {
    paths: Paths,
    trusted: bool,
    credentials: Arc<Credentials>,
    additional_dirs: Vec<PathBuf>,
    /// `(id, script file)` of the scripts of installed packages.
    packaged: Vec<(String, PathBuf)>,
    loaded: Mutex<Loaded>,
    /// Ids of the providers scripts have registered. A provider stays registered across reloads.
    providers: Mutex<Vec<String>>,
}

impl Host {
    pub fn new(
        paths: Paths,
        trusted: bool,
        credentials: Arc<Credentials>,
        additional_dirs: Vec<PathBuf>,
        packaged: Vec<(String, PathBuf)>,
    ) -> Host {
        Host {
            paths,
            trusted,
            credentials,
            additional_dirs: additional_dirs.into_iter().map(|dir| canonical(&dir)).collect(),
            packaged,
            loaded: Mutex::default(),
            providers: Mutex::default(),
        }
    }

    /// `~/.azazel/extensions`, which also holds the scripts' data and state.
    fn home_dir(&self) -> PathBuf {
        self.paths.home.join("extensions")
    }

    /// `(id, script file)` of every script, in load order: packages, then the user's own, then the
    /// project's. A script found later replaces an earlier one with the same id.
    fn discover(&self) -> Vec<(String, PathBuf)> {
        let mut dirs = vec![self.home_dir()];
        if self.trusted {
            dirs.push(self.paths.project_dir().join("extensions"));
        }

        let mut found: Vec<(String, PathBuf)> = self
            .packaged
            .iter()
            .filter(|(_, script)| script.is_file())
            .cloned()
            .collect();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut entries: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
            entries.sort();
            for entry in entries {
                let (name, script) = if entry.is_dir() {
                    (entry.file_name(), entry.join("init.luau"))
                } else if entry.extension().is_some_and(|extension| extension == "luau") {
                    (entry.file_stem(), entry.clone())
                } else {
                    continue;
                };
                let Some(id) = name.map(|name| name.to_string_lossy().into_owned()) else {
                    continue;
                };
                if id.is_empty() || id.starts_with('.') || !script.is_file() {
                    continue;
                }
                match found.iter_mut().find(|(known, _)| *known == id) {
                    Some(earlier) => earlier.1 = script,
                    None => found.push((id, script)),
                }
            }
        }
        found
    }

    /// Loads every script and registers what they define. A script that fails to load is kept, with
    /// its error, for `/ext`; it never fails the others.
    pub fn register(self: &Arc<Self>, r: &mut Registrar) -> Result<(), ExtError> {
        let services = r.services().clone();
        let extras = services.get::<UiExtras>();
        let mut loaded = Loaded::default();
        for (id, path) in self.discover() {
            let data_dir = self.home_dir().join(".data").join(&id);
            let state_file = self.home_dir().join(".state").join(format!("{id}.json"));
            let mut others = self.additional_dirs.clone();
            others.push(data_dir.clone());
            let roots = Roots {
                project: self.paths.project_root.clone(),
                others,
            };
            let shared = Arc::new(Shared::new(
                id,
                path,
                services.clone(),
                roots,
                data_dir,
                state_file,
            ));
            loaded.scripts.push(shared.clone());
            let script = match Script::load(shared.clone()) {
                Ok(script) => Arc::new(script),
                Err(why) => {
                    shared.disable(why);
                    continue;
                }
            };
            let pending = std::mem::take(&mut *shared.pending());
            for def in pending.tools {
                loaded.tools.push(Arc::new(LuaTool {
                    script: script.clone(),
                    def,
                }));
            }
            for (event, kind, function) in pending.handlers {
                r.on(
                    kind,
                    Arc::new(LuaHandler {
                        script: script.clone(),
                        event,
                        function,
                    }),
                );
            }
            for def in pending.commands {
                let command = Command {
                    name: def.name.clone(),
                    description: def.description,
                    argument_hint: def.argument_hint,
                    handler: Arc::new(LuaCommand {
                        script: script.clone(),
                        name: def.name,
                        function: def.run,
                    }),
                };
                loaded.commands.push((shared.clone(), command));
            }
            for (name, function) in pending.prompts {
                let part = LuaPrompt {
                    script: script.clone(),
                    name: name.clone(),
                    function,
                    cwd: self.paths.cwd.clone(),
                };
                r.system_part(&name, Arc::new(part));
            }
            for (key, function) in pending.shortcuts {
                if let Some(extras) = &extras {
                    let handler = LuaShortcut {
                        script: script.clone(),
                        key: key.clone(),
                        function,
                    };
                    extras.bind(OWNER, &key, Arc::new(handler));
                }
            }
            for (name, function) in pending.routers {
                let router = LuaRouter {
                    script: script.clone(),
                    function,
                };
                r.virtual_model(&name, Arc::new(router));
            }
            // The session's stall limits, when the application set any.
            let stall = r.services().get::<StallLimits>().map(|limits| *limits);
            for config in pending.providers {
                let mut registered = locked(&self.providers);
                if !registered.contains(&config.id) {
                    registered.push(config.id.clone());
                    r.provider(Arc::new(
                        HttpProvider::new(config, http_client(), self.credentials.clone())
                            .with_stall(stall.unwrap_or_default()),
                    ));
                }
            }
        }
        *locked(&self.loaded) = loaded;

        r.tool_source(Arc::new(ScriptTools(self.clone())));
        r.command_source(Arc::new(ScriptCommands(self.clone())));
        r.command(Command {
            name: "ext".to_string(),
            description: "List the loaded extensions and Luau scripts, with the errors of disabled scripts"
                .to_string(),
            argument_hint: None,
            handler: Arc::new(ExtCommand(self.clone())),
        });
        r.command(Command {
            name: "reload".to_string(),
            description: "Drop all Luau extension state and load the scripts again".to_string(),
            argument_hint: None,
            handler: Arc::new(ReloadCommand(self.clone())),
        });
        Ok(())
    }

    /// The scripts, one per line with their file, and under each disabled one its error.
    fn report(&self) -> String {
        let scripts = locked(&self.loaded).scripts.clone();
        if scripts.is_empty() {
            return format!(
                "no Luau scripts; put `<name>.luau` or `<name>/init.luau` in {}\n",
                self.home_dir().display()
            );
        }
        let mut out = String::from("Luau scripts:\n");
        for script in scripts {
            out.push_str(&format!("  {}  {}\n", script.id, script.path.display()));
            if let Some(error) = script.error() {
                out.push_str(&format!("    disabled: {error}\n"));
            }
        }
        out
    }
}

fn registry(ctx: &ExtCtx) -> Option<Arc<Registry>> {
    ctx.services.get::<RegistryHandle>()?.0.upgrade()
}

/// The tools of the scripts that are not disabled.
struct ScriptTools(Arc<Host>);

impl ToolSource for ScriptTools {
    fn tools(&self, _: &AgentInfo) -> Vec<Arc<dyn Tool>> {
        let loaded = locked(&self.0.loaded);
        loaded
            .tools
            .iter()
            .filter(|tool| tool.script.shared.error().is_none())
            .map(|tool| tool.clone() as Arc<dyn Tool>)
            .collect()
    }
}

/// The commands of the scripts that are not disabled.
struct ScriptCommands(Arc<Host>);

impl CommandSource for ScriptCommands {
    fn commands(&self) -> Vec<Command> {
        let loaded = locked(&self.0.loaded);
        loaded
            .commands
            .iter()
            .filter(|(script, _)| script.error().is_none())
            .map(|(_, command)| command.clone())
            .collect()
    }
}

/// `/ext`.
struct ExtCommand(Arc<Host>);

#[async_trait]
impl CommandHandler for ExtCommand {
    async fn run(&self, _: &str, ctx: &ExtCtx) -> CommandOutcome {
        let mut out = String::new();
        if let Some(registry) = registry(ctx) {
            out.push_str(&format!("extensions: {}\n", registry.extensions().join(", ")));
        }
        out.push_str(&self.0.report());
        CommandOutcome::Show(out)
    }
}

/// `/reload`.
struct ReloadCommand(Arc<Host>);

#[async_trait]
impl CommandHandler for ReloadCommand {
    async fn run(&self, _: &str, ctx: &ExtCtx) -> CommandOutcome {
        let Some(registry) = registry(ctx) else {
            return CommandOutcome::Error(
                "the Luau scripts cannot be reloaded here: the extension registry is not available"
                    .to_string(),
            );
        };
        registry.unload(OWNER);
        match registry.load(&LuaExt::of(self.0.clone())) {
            Ok(()) => CommandOutcome::Show(format!("reloaded\n{}", self.0.report())),
            Err(e) => CommandOutcome::Error(format!("the Luau scripts did not load again: {e}")),
        }
    }
}
