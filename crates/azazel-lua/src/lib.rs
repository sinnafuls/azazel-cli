//! The Luau extension host. Each script in an `extensions` directory runs in its own sandboxed
//! VM with the global table `azazel`, through which it adds tools, event handlers, slash commands,
//! key bindings, providers and MCP servers, and reaches the session it runs in.

mod api;
mod bridge;
mod confine;
mod convert;
mod host;
mod script;

use std::path::PathBuf;
use std::sync::Arc;

use azazel_ai::Credentials;
use azazel_core::{ExtError, Extension, Paths, Registrar};

use crate::host::Host;

/// The extension that loads the Luau scripts. Everything the scripts register belongs to it, so
/// unloading it removes them all; its commands are `/ext` and `/reload`.
pub struct LuaExt {
    host: Arc<Host>,
}

impl LuaExt {
    /// `packaged` are the scripts of installed packages, as `(id, file)`; `additional_dirs` are
    /// the directories besides the project root that scripts may read and write.
    ///
    /// The packaged scripts load first, then `<dir>/<id>.luau` and `<dir>/<id>/init.luau` in
    /// `<home>/extensions`, then, when `trusted`, in `<project>/.azazel/extensions`; a later script
    /// replaces an earlier one with the same id.
    pub fn new(
        paths: Paths,
        trusted: bool,
        credentials: Arc<Credentials>,
        additional_dirs: Vec<PathBuf>,
        packaged: Vec<(String, PathBuf)>,
    ) -> LuaExt {
        LuaExt::of(Arc::new(Host::new(
            paths,
            trusted,
            credentials,
            additional_dirs,
            packaged,
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
