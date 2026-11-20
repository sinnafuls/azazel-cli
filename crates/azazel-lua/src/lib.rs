//! The Luau extension host. Each script in an `extensions` directory runs in its own sandboxed
//! VM with the global table `azazel`, through which it adds tools, event handlers, slash commands,
//! key bindings, providers and MCP servers, and reaches the session it runs in.
//!
//! The same VM and sandbox run the scripts agents write with the `eval` tool (`language: "luau"`).

mod api;
mod bridge;
mod code;
mod confine;
mod convert;
mod host;
mod models;
mod script;

use std::path::PathBuf;
use std::sync::Arc;

use azazel_ai::Credentials;
use azazel_core::{ExtError, Extension, Paths, Registrar};

pub use crate::code::{CodeExt, CodeLimits};
use crate::host::Host;

/// The scripts a host loads besides those in the extension directories, each as `(id, file)`.
#[derive(Debug, Clone, Default)]
pub struct Scripts {
    /// The scripts of installed packages. They load first.
    pub packaged: Vec<(String, PathBuf)>,
    /// The scripts named for this run (`--extension`). They load last.
    pub run: Vec<(String, PathBuf)>,
    /// Load only the scripts named for this run (`--no-extensions`).
    pub only_run: bool,
}

/// The extension that loads the Luau scripts. Everything the scripts register belongs to it, so
/// unloading it removes them all; its commands are `/ext` and `/reload`.
pub struct LuaExt {
    host: Arc<Host>,
}

impl LuaExt {
    /// `additional_dirs` are the directories besides the project root that scripts may read and
    /// write.
    ///
    /// The packaged scripts load first, then `<dir>/<id>.luau` and `<dir>/<id>/init.luau` in
    /// `<home>/extensions`, then, when `trusted`, in `<project>/.azazel/extensions`, then the
    /// scripts named for this run; a later script replaces an earlier one with the same id.
    pub fn new(
        paths: Paths,
        trusted: bool,
        credentials: Arc<Credentials>,
        additional_dirs: Vec<PathBuf>,
        scripts: Scripts,
    ) -> LuaExt {
        LuaExt::of(Arc::new(Host::new(
            paths,
            trusted,
            credentials,
            additional_dirs,
            scripts,
        )))
    }

    fn of(host: Arc<Host>) -> LuaExt {
        LuaExt { host }
    }
}

impl Extension for LuaExt {
    fn id(&self) -> &str {
        script::OWNER
    }

    /// Loads every script and runs its top level. A script that fails is disabled and reported by
    /// `/ext`; it does not fail the extension.
    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        self.host.register(r)
    }
}

/// Type declarations for the `azazel` global, for editors: the content of `azazel.d.luau`.
pub fn type_declarations() -> &'static str {
    include_str!("azazel.d.luau")
}
