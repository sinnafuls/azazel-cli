//! Format on write: after `write` or `edit` changes a file, the formatter for its kind runs on it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use azazel_agent::ToolCtx;
use azazel_core::process::AgentEnv;

/// How long a formatter may take on one file.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Stands for the file's path in a configured command; without it the path is appended.
const FILE: &str = "{file}";

/// The formatters used when the configuration names none for a file's extension, each for the
/// extensions after it. They apply only when the program is installed.
const DEFAULTS: [(&str, &[&str]); 5] = [
    ("rustfmt", &["rs"]),
    (
        "prettier --write",
        &[
            "js", "jsx", "mjs", "cjs", "ts", "tsx", "json", "css", "scss", "less", "html", "vue",
        ],
    ),
    ("gofmt -w", &["go"]),
    ("ruff format", &["py", "pyi"]),
    ("clang-format -i", &["c", "h", "cc", "cpp", "cxx", "hpp", "hh"]),
];

/// The formatter settings of a session with `tools.format_on_write` on. Registered as a service;
/// `write` and `edit` format only when it is there.
pub struct Formatters {
    /// `[tools.format]`: file extension (without the dot) to command.
    configured: BTreeMap<String, String>,
}

/// `command` as words. Quotes group words with spaces; a backslash is an ordinary character, so
/// Windows paths need no escaping.
fn words(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for c in command.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            None => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(word);
    }
    out
}

/// The Rust edition of the package `file` belongs to, read from the nearest `Cargo.toml` that
/// states one. `rustfmt` run on a single file does not look it up itself and assumes 2015.
fn rust_edition(file: &Path) -> Option<String> {
    file.ancestors().skip(1).find_map(|dir| {
        let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
        manifest.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            let edition = value.trim().trim_matches('"');
            let stated = key.trim() == "edition" && edition.len() == 4 && edition.starts_with("20");
            stated.then(|| edition.to_string())
        })
    })
}

impl Formatters {
    pub fn new(configured: BTreeMap<String, String>) -> Formatters {
        Formatters { configured }
    }

    /// The command line that formats `file`: the configured one for its extension, else the
    /// default for it when that program is installed.
    fn command(&self, file: &Path) -> Option<Result<(PathBuf, Vec<String>), String>> {
        let extension = file.extension()?.to_str()?.to_ascii_lowercase();
        let path = file.to_string_lossy().into_owned();
        if let Some(configured) = self.configured.get(&extension) {
            let mut words = words(configured);
            if words.is_empty() {
                return None;
            }
            let name = words.remove(0);
            let Ok(program) = which::which(&name) else {
                return Some(Err(format!("formatter `{name}` was not found")));
            };
            let named = words.iter().any(|word| word.contains(FILE));
            let mut arguments: Vec<String> = words.iter().map(|word| word.replace(FILE, &path)).collect();
            if !named {
                arguments.push(path);
            }
            return Some(Ok((program, arguments)));
        }
        let (command, _) = DEFAULTS
            .iter()
            .find(|(_, extensions)| extensions.contains(&extension.as_str()))?;
        let mut words = words(command);
        let program = which::which(words.remove(0)).ok()?;
        if extension == "rs"
            && let Some(edition) = rust_edition(file)
        {
            words.extend(["--edition".to_string(), edition]);
        }
        words.push(path);
        Some(Ok((program, words)))
    }

    /// Formats `file` in place. `None` when no formatter applies to it; otherwise the formatter's
    /// name, or one line saying why it did not format.
    pub async fn run(&self, file: &Path, ctx: &ToolCtx) -> Option<Result<String, String>> {
        let (program, arguments) = match self.command(file)? {
            Ok(command) => command,
            Err(why) => return Some(Err(why)),
        };
        let name = program
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.display().to_string());
        let mut command = tokio::process::Command::new(&program);
        command
            .args(&arguments)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(env) = ctx.services.get::<AgentEnv>() {
            command.envs(env.vars());
        }
        let finished = tokio::select! {
            finished = tokio::time::timeout(TIMEOUT, command.output()) => finished,
            _ = ctx.cancel.cancelled() => return Some(Err(format!("{name} was cancelled"))),
        };
        Some(match finished {
            Err(_) => Err(format!("{name} did not finish within {} s", TIMEOUT.as_secs())),
            Ok(Err(e)) => Err(format!("{name} could not be started: {e}")),
            Ok(Ok(output)) if output.status.success() => Ok(name),
            Ok(Ok(output)) => {
                let said = String::from_utf8_lossy(&output.stderr);
                let why = said.lines().map(str::trim).find(|line| !line.is_empty());
                Err(match why {
                    Some(why) => {
                        let why: String = why.chars().take(200).collect();
                        format!("{name} failed: {why}")
                    }
                    None => format!("{name} failed ({})", output.status),
                })
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_split_into_words_with_quotes_and_plain_backslashes() {
        assert_eq!(words("rustfmt --edition 2024"), ["rustfmt", "--edition", "2024"]);
        assert_eq!(
            words(r#"  "C:\Program Files\fmt\fmt.exe"  -w 'a b' "" "#),
            [r"C:\Program Files\fmt\fmt.exe", "-w", "a b", ""]
        );
        assert!(words("   ").is_empty());
    }

    #[test]
    fn the_rust_edition_comes_from_the_nearest_manifest_that_states_one() {
        let dir = tempfile::tempdir().unwrap();
        let member = dir.path().join("crates/a/src");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace.package]\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("crates/a/Cargo.toml"),
            "[package]\nname = \"a\"\nedition.workspace = true\n",
        )
        .unwrap();
        assert_eq!(rust_edition(&member.join("lib.rs")).as_deref(), Some("2024"));
        std::fs::write(
            dir.path().join("crates/a/Cargo.toml"),
            "[package]\nedition = \"2021\"\n",
        )
        .unwrap();
        assert_eq!(rust_edition(&member.join("lib.rs")).as_deref(), Some("2021"));
    }

    #[test]
    fn a_configured_formatter_that_is_missing_is_reported_and_unknown_files_have_none() {
        let formatters = Formatters::new(BTreeMap::from([(
            "zz".to_string(),
            "azazel-no-such-formatter --fix".to_string(),
        )]));
        assert_eq!(
            formatters.command(Path::new("a.zz")).unwrap().unwrap_err(),
            "formatter `azazel-no-such-formatter` was not found"
        );
        assert!(formatters.command(Path::new("a.unknown-kind")).is_none());
        assert!(formatters.command(Path::new("Makefile")).is_none());
    }
}
