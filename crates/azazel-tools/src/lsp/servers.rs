//! Which language servers a configuration gives: the configured ones, then the built-in defaults
//! for the extensions nobody configured.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use azazel_core::config::LspServerConfig;

/// One language server and the files it is for.
pub(crate) struct Spec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// Lower-case, without the dot.
    pub extensions: Vec<String>,
    /// A default rather than a configured server: used only when its command is installed.
    pub builtin: bool,
}

impl Spec {
    /// The command and its arguments as one line.
    pub fn command_line(&self) -> String {
        let mut line = self.command.clone();
        for arg in &self.args {
            line.push(' ');
            line.push_str(arg);
        }
        line
    }

    pub fn serves(&self, extension: &str) -> bool {
        self.extensions.iter().any(|known| known == extension)
    }
}

struct Builtin {
    name: &'static str,
    command: &'static str,
    args: &'static [&'static str],
    extensions: &'static [&'static str],
}

const BUILTINS: [Builtin; 5] = [
    Builtin {
        name: "rust",
        command: "rust-analyzer",
        args: &[],
        extensions: &["rs"],
    },
    Builtin {
        name: "typescript",
        command: "typescript-language-server",
        args: &["--stdio"],
        extensions: &["ts", "tsx", "js", "jsx"],
    },
    Builtin {
        name: "python",
        command: "pyright-langserver",
        args: &["--stdio"],
        extensions: &["py"],
    },
    Builtin {
        name: "go",
        command: "gopls",
        args: &[],
        extensions: &["go"],
    },
    Builtin {
        name: "c",
        command: "clangd",
        args: &[],
        extensions: &["c", "h", "cc", "cpp", "hpp"],
    },
];

/// The extension of `path` as specs spell it, or `None` when it has none.
pub(crate) fn extension_of(path: &Path) -> Option<String> {
    Some(path.extension()?.to_string_lossy().to_lowercase())
}

/// Configured servers in name order, then each default that is not replaced by a configured
/// server of the same name, limited to the extensions no configured server claims.
pub(crate) fn specs(configured: &BTreeMap<String, LspServerConfig>) -> Vec<Spec> {
    let mut out: Vec<Spec> = configured
        .iter()
        .map(|(name, server)| Spec {
            name: name.clone(),
            command: server.command.clone(),
            args: server.args.clone(),
            extensions: server
                .extensions
                .iter()
                .map(|extension| extension.trim().trim_start_matches('.').to_lowercase())
                .collect(),
            builtin: false,
        })
        .collect();
    for builtin in &BUILTINS {
        if configured.contains_key(builtin.name) {
            continue;
        }
        let extensions: Vec<String> = builtin
            .extensions
            .iter()
            .filter(|extension| !out.iter().any(|spec| spec.serves(extension)))
            .map(|extension| extension.to_string())
            .collect();
        if !extensions.is_empty() {
            out.push(Spec {
                name: builtin.name.to_string(),
                command: builtin.command.to_string(),
                args: builtin.args.iter().map(|arg| arg.to_string()).collect(),
                extensions,
                builtin: true,
            });
        }
    }
    out
}

/// The executable `command` names: the file itself when `command` is a path, otherwise the first
/// match on `PATH`. On Windows the `PATHEXT` extensions are tried, so `server` finds `server.cmd`.
pub(crate) fn find_program(command: &str) -> Option<PathBuf> {
    let command = Path::new(command);
    if command.components().count() > 1 {
        return runnable(command);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| runnable(&dir.join(command)))
}

#[cfg(windows)]
fn runnable(candidate: &Path) -> Option<PathBuf> {
    if candidate.extension().is_some() && candidate.is_file() {
        return Some(candidate.to_path_buf());
    }
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    extensions
        .split(';')
        .filter(|extension| !extension.is_empty())
        .find_map(|extension| {
            let mut name = candidate.as_os_str().to_owned();
            name.push(extension);
            let path = PathBuf::from(name);
            path.is_file().then_some(path)
        })
}

#[cfg(not(windows))]
fn runnable(candidate: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let meta = std::fs::metadata(candidate).ok()?;
    (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then(|| candidate.to_path_buf())
}

/// The language identifier the protocol uses for files with this extension.
pub(crate) fn language_id(extension: &str) -> &str {
    match extension {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        "h" => "c",
        "cc" | "hpp" => "cpp",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(command: &str, extensions: &[&str]) -> LspServerConfig {
        LspServerConfig {
            command: command.to_string(),
            args: Vec::new(),
            extensions: extensions.iter().map(|extension| extension.to_string()).collect(),
            warm_files: Vec::new(),
        }
    }

    fn summary(configured: &[(&str, LspServerConfig)]) -> Vec<(String, String, bool)> {
        let configured: BTreeMap<String, LspServerConfig> = configured
            .iter()
            .map(|(name, server)| (name.to_string(), server.clone()))
            .collect();
        specs(&configured)
            .into_iter()
            .map(|spec| (spec.name, spec.extensions.join(" "), spec.builtin))
            .collect()
    }

    fn row(name: &str, extensions: &str, builtin: bool) -> (String, String, bool) {
        (name.to_string(), extensions.to_string(), builtin)
    }

    #[test]
    fn without_configuration_the_defaults_cover_their_languages() {
        assert_eq!(
            summary(&[]),
            [
                row("rust", "rs", true),
                row("typescript", "ts tsx js jsx", true),
                row("python", "py", true),
                row("go", "go", true),
                row("c", "c h cc cpp hpp", true),
            ]
        );
    }

    #[test]
    fn a_configured_server_takes_its_extensions_from_the_defaults() {
        let listed = summary(&[
            ("deno", server("deno", &[".TS", "tsx"])),
            ("ra", server("ra-multiplex", &["rs"])),
        ]);
        assert_eq!(
            listed[..3],
            [
                row("deno", "ts tsx", false),
                row("ra", "rs", false),
                row("typescript", "js jsx", true),
            ],
            "a default with no extension left is gone, one with some left keeps those"
        );
        assert!(!listed.iter().any(|(name, _, _)| name == "rust"));
    }

    #[test]
    fn a_configured_server_replaces_the_default_of_the_same_name() {
        let listed = summary(&[("python", server("pylsp", &["pyi"]))]);
        assert_eq!(listed[0], row("python", "pyi", false));
        assert_eq!(listed.iter().filter(|(name, _, _)| name == "python").count(), 1);
    }

    #[test]
    fn a_program_is_found_by_path_or_by_name_on_the_search_path() {
        let this = std::env::current_exe().unwrap();
        assert_eq!(find_program(&this.to_string_lossy()), Some(this.clone()));
        if cfg!(windows) {
            // Without its extension the file is still found, with the extension as `PATHEXT` spells it.
            let found = find_program(&this.with_extension("").to_string_lossy()).unwrap();
            assert!(
                found
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&this.to_string_lossy())
            );
        }
        assert_eq!(find_program("azazel-no-such-language-server"), None);
        let missing = std::env::temp_dir().join("azazel-no-such-dir").join("server");
        assert_eq!(find_program(&missing.to_string_lossy()), None);
    }
}
