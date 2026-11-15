//! Which debug adapter runs a program, found on `PATH` when a session starts, and the arguments
//! that make it launch the program.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use azazel_core::config::DebugAdapterConfig;
use serde_json::{Value, json};

use super::dap::Adapter;

/// What a language's adapters have in common: the shape of their `launch` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    /// Rust, C and C++: `lldb-dap`, or CodeLLDB.
    Native,
    Python,
    Go,
}

impl Family {
    pub fn adapter_id(self) -> &'static str {
        match self {
            Family::Native => "lldb",
            Family::Python => "python",
            Family::Go => "go",
        }
    }

    /// The command that installs the adapter, for the message when it is missing.
    fn install(self) -> &'static str {
        match self {
            Family::Native => {
                "install LLVM's lldb-dap (`winget install LLVM.LLVM`, `brew install llvm` or \
                 `apt install lldb`) or CodeLLDB's `codelldb`"
            }
            Family::Python => "install debugpy with `python -m pip install debugpy`",
            Family::Go => "install Delve with `go install github.com/go-delve/delve/cmd/dlv@latest`",
        }
    }
}

/// `language` as a family and the setting's name for it.
fn family_of(language: &str) -> Option<(Family, &'static str)> {
    match language.trim().to_ascii_lowercase().as_str() {
        "rust" => Some((Family::Native, "rust")),
        "c" => Some((Family::Native, "c")),
        "cpp" | "c++" | "cxx" => Some((Family::Native, "cpp")),
        "python" | "py" => Some((Family::Python, "python")),
        "go" | "golang" => Some((Family::Go, "go")),
        _ => None,
    }
}

/// The language a program is written in, as far as its name says: scripts have an extension,
/// anything else is a native executable.
fn guess(program: &Path) -> &'static str {
    match program.extension().and_then(|e| e.to_str()) {
        Some("py") => "python",
        Some("go") => "go",
        _ => "rust",
    }
}

/// The adapter for `language` (guessed from `program` when absent): the command set in the
/// settings, else the first adapter `find` has.
pub(crate) fn resolve_with(
    language: Option<&str>,
    program: &Path,
    configured: &BTreeMap<String, DebugAdapterConfig>,
    find: &dyn Fn(&str) -> Option<PathBuf>,
) -> Result<(Family, Adapter), String> {
    let language = language
        .filter(|l| !l.trim().is_empty())
        .unwrap_or_else(|| guess(program));
    let Some((family, name)) = family_of(language) else {
        return Err(format!(
            "no debug adapter is known for `{language}`; use rust, c, cpp, python or go"
        ));
    };
    if let Some(custom) = configured.get(name) {
        let Some(path) = find(&custom.command) else {
            return Err(format!(
                "the debug adapter `{}` set in debug.adapters.{name} was not found on PATH",
                custom.command
            ));
        };
        let listens = custom.listens.unwrap_or(family == Family::Go);
        return Ok((
            family,
            Adapter {
                program: path,
                args: custom.args.clone(),
                listens,
            },
        ));
    }
    let found = match family {
        Family::Native => ["lldb-dap", "lldb-vscode"]
            .iter()
            .find_map(|c| {
                find(c).map(|program| Adapter {
                    program,
                    args: Vec::new(),
                    listens: false,
                })
            })
            .or_else(|| {
                find("codelldb").map(|program| Adapter {
                    program,
                    args: vec!["--port".to_string(), "0".to_string()],
                    listens: true,
                })
            }),
        Family::Python => ["python3", "python"]
            .iter()
            .find_map(|c| find(c))
            .map(|program| Adapter {
                program,
                args: vec!["-m".to_string(), "debugpy.adapter".to_string()],
                listens: false,
            }),
        Family::Go => find("dlv").map(|program| Adapter {
            program,
            args: vec!["dap".to_string(), "--listen=127.0.0.1:0".to_string()],
            listens: true,
        }),
    };
    found.map(|adapter| (family, adapter)).ok_or_else(|| {
        format!(
            "no debug adapter for {name} is installed: {}. Or name one with debug.adapters.{name}",
            family.install()
        )
    })
}

pub(crate) fn resolve(
    language: Option<&str>,
    program: &Path,
    configured: &BTreeMap<String, DebugAdapterConfig>,
) -> Result<(Family, Adapter), String> {
    resolve_with(language, program, configured, &|command| {
        if Path::new(command).components().count() > 1 {
            return Path::new(command).is_file().then(|| PathBuf::from(command));
        }
        which::which(command).ok()
    })
}

/// An adapter that exited while starting is most often one that is not set up: say how to set it up.
pub(crate) fn with_hint(family: Family, error: String) -> String {
    if error.starts_with("the debug adapter exited") {
        format!("{error} (to set it up, {})", family.install())
    } else {
        error
    }
}

/// The arguments of `launch` for `program`.
pub(crate) fn launch_arguments(
    family: Family,
    program: &Path,
    args: &[String],
    cwd: &Path,
    stop_on_entry: bool,
) -> Value {
    let program = program.to_string_lossy();
    let cwd = cwd.to_string_lossy();
    match family {
        Family::Native => json!({
            "type": "lldb", "request": "launch", "name": "azazel",
            "program": program, "args": args, "cwd": cwd, "stopOnEntry": stop_on_entry,
        }),
        Family::Python => json!({
            "type": "python", "request": "launch", "name": "azazel",
            "program": program, "args": args, "cwd": cwd, "stopOnEntry": stop_on_entry,
            // The program's output comes back as events instead of going to a terminal.
            "console": "internalConsole", "justMyCode": false,
        }),
        Family::Go => json!({
            "type": "go", "request": "launch", "name": "azazel",
            "mode": if program.ends_with(".go") { "debug" } else { "exec" },
            "program": program, "args": args, "cwd": cwd, "stopOnEntry": stop_on_entry,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(names: &'static [&'static str]) -> impl Fn(&str) -> Option<PathBuf> {
        move |command| names.contains(&command).then(|| PathBuf::from(command))
    }

    #[test]
    fn a_missing_adapter_says_how_to_install_it() {
        let none = BTreeMap::new();
        let error = resolve_with(Some("go"), Path::new("app"), &none, &only(&[])).unwrap_err();
        assert!(
            error.contains("go install github.com/go-delve/delve/cmd/dlv@latest"),
            "{error}"
        );
        let error = resolve_with(None, Path::new("tool.py"), &none, &only(&[])).unwrap_err();
        assert!(error.contains("pip install debugpy"), "{error}");
        let error = resolve_with(Some("rust"), Path::new("app"), &none, &only(&[])).unwrap_err();
        assert!(error.contains("lldb-dap"), "{error}");
    }

    #[test]
    fn adapters_are_found_by_language_and_the_settings_win() {
        let none = BTreeMap::new();
        let (family, adapter) = resolve_with(
            None,
            Path::new("target/debug/app"),
            &none,
            &only(&["lldb-vscode"]),
        )
        .unwrap();
        assert_eq!((family, adapter.listens), (Family::Native, false));
        let (_, adapter) = resolve_with(Some("cpp"), Path::new("a"), &none, &only(&["codelldb"])).unwrap();
        assert!(adapter.listens);
        let (family, adapter) = resolve_with(None, Path::new("x.py"), &none, &only(&["python"])).unwrap();
        assert_eq!(family, Family::Python);
        assert_eq!(adapter.args, ["-m", "debugpy.adapter"]);

        let custom = DebugAdapterConfig {
            command: "my-lldb".to_string(),
            args: vec!["--stdio".to_string()],
            listens: None,
        };
        let configured = BTreeMap::from([("rust".to_string(), custom)]);
        let (_, adapter) = resolve_with(
            Some("rust"),
            Path::new("a"),
            &configured,
            &only(&["my-lldb", "lldb-dap"]),
        )
        .unwrap();
        assert_eq!(
            (adapter.program, adapter.args),
            (PathBuf::from("my-lldb"), vec!["--stdio".to_string()])
        );
        let error =
            resolve_with(Some("rust"), Path::new("a"), &configured, &only(&["lldb-dap"])).unwrap_err();
        assert!(error.contains("debug.adapters.rust"), "{error}");
        assert!(resolve_with(Some("cobol"), Path::new("a"), &none, &only(&[])).is_err());
    }
}
