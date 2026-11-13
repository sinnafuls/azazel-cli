//! Guards on the quality of a turn, in every permission mode: the lead that calls its work done
//! without having run anything is sent back once (done-check), a command that keeps failing
//! gets a note on its result (stuck), and a reply that loops is stopped as it is written
//! (runaway, [`azazel_core::runaway`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use azazel_ai::Block;
use azazel_core::runaway::Runaway;
use azazel_core::{
    Event, EventHandler, EventKind, ExtCtx, ExtError, Extension, Level, Outcome, Registrar, Ui,
};
use serde_json::Value;

use crate::check::Checks;
use crate::format::words;
use crate::util::display_path;

/// Files named in the done-check message.
const MAX_NAMED: usize = 5;

/// Files that are prose: changing one leaves nothing to build or test.
const PROSE: &[&str] = &["md", "mdx", "markdown", "txt", "rst", "adoc", "org"];

/// Programs that are a check whatever their arguments: test runners, compilers, linters, type
/// checkers and build tools.
const CHECKERS: &[&str] = &[
    "actionlint",
    "ava",
    "biome",
    "cargo-clippy",
    "cargo-nextest",
    "cc",
    "clang",
    "clang++",
    "clang-tidy",
    "credo",
    "ctest",
    "cypress",
    "detekt",
    "dialyzer",
    "eslint",
    "flake8",
    "g++",
    "gcc",
    "gmake",
    "golangci-lint",
    "hadolint",
    "javac",
    "jest",
    "ktlint",
    "luacheck",
    "make",
    "mocha",
    "msbuild",
    "mypy",
    "nextest",
    "ninja",
    "nmake",
    "nox",
    "oxlint",
    "phpstan",
    "phpunit",
    "playwright",
    "psalm",
    "py.test",
    "pylint",
    "pyflakes",
    "pyright",
    "pytest",
    "rspec",
    "rubocop",
    "ruff",
    "rustc",
    "selene",
    "shellcheck",
    "staticcheck",
    "stylelint",
    "swiftlint",
    "tox",
    "tsc",
    "tsgo",
    "ty",
    "unittest",
    "vitest",
    "vue-tsc",
    "xcodebuild",
    "yamllint",
    "zizmor",
];

/// Programs that run what the project names: one of their first arguments has to say that it
/// builds, tests or checks, or runs the program.
const RUNNERS: &[&str] = &[
    "bazel", "bun", "cabal", "cargo", "composer", "dart", "deno", "dotnet", "dune", "flutter", "go",
    "gradle", "gradlew", "hatch", "invoke", "just", "lerna", "meson", "mix", "moon", "mvn", "mvnw", "npm",
    "nx", "pdm", "pnpm", "rake", "rye", "sbt", "stack", "swift", "task", "turbo", "yarn", "zig",
];

/// What an argument of a runner, or the name of a script, says when it is a check. A word is cut
/// at everything that is not a letter or a digit first: `test:unit`, `run-tests.sh`.
const KEYWORDS: &[&str] = &[
    "analyze",
    "assemble",
    "bench",
    "build",
    "check",
    "checks",
    "clippy",
    "compile",
    "e2e",
    "integration",
    "lint",
    "nextest",
    "package",
    "run",
    "runtest",
    "spec",
    "specs",
    "start",
    "test",
    "testing",
    "tests",
    "tsc",
    "typecheck",
    "unit",
    "validate",
    "verify",
    "vet",
];

/// Programs that run a script: with one, the script is being tried out.
const INTERPRETERS: &[&str] = &[
    "bash",
    "bun",
    "deno",
    "lua",
    "luau",
    "node",
    "perl",
    "php",
    "powershell",
    "pwsh",
    "py",
    "python",
    "python3",
    "ruby",
    "sh",
    "ts-node",
    "tsx",
    "zsh",
];

/// The interpreters that are shells: what follows their `-c` is a command line of its own.
const SHELLS: &[&str] = &["bash", "sh", "zsh", "pwsh", "powershell"];

/// Words in front of the program that do not change what is run.
const WRAPPERS: &[&str] = &[
    "sudo", "time", "env", "nice", "npx", "pnpx", "bunx", "uvx", "exec", "call", "command", "nohup",
];

/// Programs that run something else when their next word is the one given: `uv run pytest`.
const RUN_WRAPPERS: &[(&str, &str)] = &[
    ("uv", "run"),
    ("poetry", "run"),
    ("pipenv", "run"),
    ("pdm", "run"),
    ("hatch", "run"),
    ("rye", "run"),
    ("bundle", "exec"),
    ("python", "-m"),
    ("python3", "-m"),
    ("py", "-m"),
    ("cmd", "/c"),
];

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `command` with its white space collapsed and without what only shapes its output (a
/// trailing `2>&1`, a last `| tail` or `| head`), so that the same command asked for twice is
/// the same command.
pub fn normalise(command: &str) -> String {
    let mut normal = command.split_whitespace().collect::<Vec<_>>().join(" ");
    loop {
        let before = normal.len();
        let filter = normal.rfind('|').filter(|&at| {
            let shown = normal[at + 1..].trim_start();
            let program = shown.split(' ').next().unwrap_or_default();
            // Not `||`, and nothing but the filter after it.
            at > 0 && !normal[..at].ends_with('|') && matches!(program, "tail" | "head")
        });
        if let Some(at) = filter {
            normal.truncate(at);
        }
        for redirect in ["2>&1", "2>/dev/null", "2>$null"] {
            if let Some(kept) = normal.trim_end().strip_suffix(redirect).map(str::len) {
                normal.truncate(kept);
            }
        }
        normal.truncate(normal.trim_end().len());
        if normal.len() == before {
            return normal;
        }
    }
}

/// The name of a program as the tables have it: without its directory and its extension.
fn program_name(word: &str) -> String {
    let name = word
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_ascii_lowercase();
    for extension in [".exe", ".cmd", ".bat", ".ps1", ".sh"] {
        if let Some(stem) = name.strip_suffix(extension) {
            return stem.to_string();
        }
    }
    name
}

fn has_keyword(word: &str) -> bool {
    word.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|piece| KEYWORDS.contains(&piece.to_ascii_lowercase().as_str()))
}

/// Whether `word` names a program of the project itself: a path from here, `./run.sh`.
fn is_local(word: &str) -> bool {
    ["./", ".\\", "../", "..\\"]
        .iter()
        .any(|prefix| word.starts_with(prefix))
}

/// Whether one simple command (no `&&`, no pipe) builds, tests or lints, or runs the program.
fn simple_is_check(words: &[String], verify: &[String]) -> bool {
    let mut words = words
        .iter()
        .map(String::as_str)
        // `FOO=bar cmd`, and PowerShell's call operator.
        .skip_while(|word| *word == "&" || (word.contains('=') && !word.starts_with('-')))
        .peekable();
    // The program, behind whatever only passes it on: `named` when a runner was told to run
    // it, `pathed` when it was given with its directory, `local` when that is the project's.
    let (mut program, mut named, mut pathed, mut local) = (String::new(), false, false, false);
    while let Some(word) = words.next() {
        program = program_name(word);
        pathed = word.contains(['/', '\\']);
        local = is_local(word);
        let passes_on = RUN_WRAPPERS.iter().any(|(wrapper, then)| {
            program == *wrapper && words.next_if(|next| next.eq_ignore_ascii_case(then)).is_some()
        });
        if !passes_on && !WRAPPERS.contains(&program.as_str()) {
            break;
        }
        named |= passes_on;
        program.clear();
    }
    if program.is_empty() {
        return false;
    }
    if CHECKERS.contains(&program.as_str()) || local {
        return true;
    }
    let rest: Vec<&str> = words.collect();
    let arguments = || rest.iter().filter(|word| !word.starts_with('-')).take(3);
    if RUNNERS.contains(&program.as_str()) && arguments().any(|word| has_keyword(word)) {
        return true;
    }
    if INTERPRETERS.contains(&program.as_str()) {
        // The first word that is not a flag is the script. `-c` and its kind take a program
        // text instead: a shell's is a command line, anything else's is not a check.
        for (at, word) in rest.iter().enumerate() {
            let inline = ["-c", "-lc", "-e", "-p", "--eval", "--print", "-command"]
                .iter()
                .any(|flag| word.eq_ignore_ascii_case(flag));
            if inline {
                return SHELLS.contains(&program.as_str())
                    && rest.get(at + 1).is_some_and(|line| is_check(line, verify));
            }
            if !word.starts_with('-') {
                return word.contains(['.', '/', '\\']) || has_keyword(word);
            }
        }
        return false;
    }
    // A script named for what it does, given by its path or handed to a runner.
    (named || pathed) && has_keyword(&program)
}

/// Whether `command` shows that a change works: it is, or contains, one of the project's check
/// commands (`verify`), or one of its parts builds, tests, lints or runs something.
pub fn is_check(command: &str, verify: &[String]) -> bool {
    let normal = normalise(command);
    if verify
        .iter()
        .map(|check| normalise(check))
        .any(|check| !check.is_empty() && normal.contains(&check))
    {
        return true;
    }
    command
        .split(['\n', ';', '|', '&'])
        .map(words)
        .filter(|words| !words.is_empty())
        .any(|words| {
            // `cmake --build` and `node --test` say it in a flag.
            let flagged = words.iter().any(|word| word == "--build" || word == "--test");
            flagged || simple_is_check(&words, verify)
        })
}

/// Whether a `shell` result is a failure: the command could not be run, was killed or timed
/// out, or ended with a code other than 0.
fn failed(is_error: bool, details: Option<&Value>) -> bool {
    let code = details
        .and_then(|details| details.get("exit_code"))
        .and_then(Value::as_i64);
    is_error || code.is_some_and(|code| code != 0)
}

fn is_prose(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| PROSE.contains(&extension.to_ascii_lowercase().as_str()))
}

/// What the guards know of the prompt being worked on.
#[derive(Default)]
struct Prompt {
    /// The `shell` commands being run, normalised: `(agent id, call id)`.
    calls: HashMap<(String, String), String>,
    /// The files changed since a check last ran, in the order they were first changed.
    unverified: Vec<PathBuf>,
    /// How often in a row each command of each agent has failed: `(agent id, command)`.
    failures: HashMap<(String, String), u32>,
    /// The lead has been sent back by the done-check.
    asked: bool,
}

struct Guards {
    done_check: bool,
    stuck_after: u32,
    verify: Vec<String>,
    prompt: Mutex<Prompt>,
}

impl Guards {
    /// A `shell` call ended. Returns the note for its result when the command has now failed
    /// `stuck_after` times in a row, or a multiple of that.
    fn shell_ended(
        &self,
        agent: &str,
        call_id: &str,
        is_error: bool,
        details: Option<&Value>,
    ) -> Option<String> {
        let mut prompt = locked(&self.prompt);
        let command = prompt.calls.remove(&(agent.to_string(), call_id.to_string()))?;
        if is_check(&command, &self.verify) {
            prompt.unverified.clear();
        }
        let key = (agent.to_string(), command);
        if !failed(is_error, details) {
            prompt.failures.remove(&key);
            return None;
        }
        let count = prompt.failures.entry(key).or_insert(0);
        *count += 1;
        (self.stuck_after > 0 && count.is_multiple_of(self.stuck_after)).then(|| {
            format!(
                "[stuck] this command has failed {count} times in a row in this turn. Running it again \
                 will fail the same way: form a new hypothesis about the cause and test that before \
                 trying this again."
            )
        })
    }

    /// What the lead is told when it stops with changes nothing has checked, once per prompt.
    fn done_check(&self, ctx: &ExtCtx) -> Option<(String, String)> {
        if !self.done_check {
            return None;
        }
        let checks = ctx.services.get::<Checks>();
        let mut prompt = locked(&self.prompt);
        if prompt.asked {
            return None;
        }
        let open: Vec<String> = prompt
            .unverified
            .iter()
            .filter(|file| !checks.as_ref().is_some_and(|checks| checks.ran_clean(file)))
            .map(|file| display_path(&ctx.cwd, file))
            .collect();
        if open.is_empty() {
            return None;
        }
        prompt.asked = true;
        let mut named = open[..open.len().min(MAX_NAMED)].join(", ");
        if open.len() > MAX_NAMED {
            named.push_str(&format!(" and {} more", open.len() - MAX_NAMED));
        }
        let files = match open.len() {
            1 => "1 file".to_string(),
            n => format!("{n} files"),
        };
        Some((
            format!(
                "<done-check>\n{files} changed in this turn and nothing has run since that shows the \
                 change works: {named}\n</done-check>\nThis is an automatic note, not a message from the \
                 user. Prove the change: run the project's tests, its build or its linter now and say what \
                 they showed. If nothing can check it, say plainly in your answer that the change is \
                 unverified."
            ),
            format!("done-check: {files} changed and nothing run since; the agent was asked to verify"),
        ))
    }
}

#[async_trait]
impl EventHandler for Guards {
    async fn handle(&self, event: &mut Event, ctx: &ExtCtx) -> Outcome {
        match event {
            Event::UserPrompt { .. } => *locked(&self.prompt) = Prompt::default(),
            Event::ToolCall {
                agent,
                call_id,
                tool,
                input,
                ..
            } if tool == "shell" => {
                if let Some(command) = input.get("command").and_then(Value::as_str) {
                    locked(&self.prompt)
                        .calls
                        .insert((agent.id.clone(), call_id.clone()), normalise(command));
                }
            }
            Event::ToolResult {
                agent,
                call_id,
                tool,
                output,
            } if tool == "shell" => {
                let note = self.shell_ended(&agent.id, call_id, output.is_error, output.details.as_ref());
                if let Some(note) = note {
                    output.content.push(Block::text(note));
                }
            }
            // A goal that was claimed done has had the project's checks and the verifier run on
            // it, whatever they said.
            Event::ToolResult { tool, .. } if tool == "goal_done" => {
                locked(&self.prompt).unverified.clear();
            }
            Event::FileChanged { path, .. } if !is_prose(path) => {
                let mut prompt = locked(&self.prompt);
                if !prompt.unverified.contains(path) {
                    prompt.unverified.push(path.clone());
                }
            }
            Event::Stop { agent, .. } if agent.depth == 0 => {
                if let Some((reason, summary)) = self.done_check(ctx) {
                    if let Some(ui) = ctx.services.get::<Ui>() {
                        ui.0.notify(Level::Info, &summary);
                    }
                    return Outcome::Block { reason };
                }
            }
            _ => {}
        }
        Outcome::Continue
    }
}

/// Registers the guards of `[guards]`. Load it after the checks on write, whose report comes
/// first when the lead stops, and before the goal.
pub struct GuardsExt {
    /// `guards.done_check`. Leave it off when the application runs the project's checks itself
    /// at every stop (`verify.on_stop`).
    pub done_check: bool,
    /// `guards.stuck_after`; 0 is off.
    pub stuck_after: u32,
    /// `guards.runaway`.
    pub runaway: bool,
    /// `verify.commands`: running one of them is a check, whatever it is called.
    pub verify: Vec<String>,
}

impl Extension for GuardsExt {
    fn id(&self) -> &str {
        "guards"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        if self.runaway {
            r.service(Arc::new(Runaway));
        }
        if !self.done_check && self.stuck_after == 0 {
            return Ok(());
        }
        let guards = Arc::new(Guards {
            done_check: self.done_check,
            stuck_after: self.stuck_after,
            verify: self.verify.clone(),
            prompt: Mutex::default(),
        });
        for kind in [
            EventKind::UserPrompt,
            EventKind::ToolCall,
            EventKind::ToolResult,
            EventKind::FileChanged,
            EventKind::Stop,
        ] {
            r.on(kind, guards.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_is_the_same_command_whatever_shapes_its_output() {
        assert_eq!(normalise("  cargo   test\t-p core "), "cargo test -p core");
        assert_eq!(normalise("cargo test 2>&1 | tail -n 20"), "cargo test");
        assert_eq!(normalise("cargo test 2>&1 | tail -20 | head -5"), "cargo test");
        assert_eq!(normalise("npm test 2>/dev/null"), "npm test");
        // What a filter in the middle does is part of the command.
        assert_eq!(
            normalise("cat log.txt | grep error | wc -l"),
            "cat log.txt | grep error | wc -l"
        );
        assert_eq!(normalise("ls || tail x"), "ls || tail x");
        assert_eq!(normalise(""), "");
    }

    #[test]
    fn builds_tests_linters_and_runs_of_the_program_are_checks() {
        let checks = [
            "cargo test",
            "cargo test --workspace 2>&1 | tail -n 30",
            "cd crates/core && cargo clippy --all-targets -- -D warnings",
            "cargo +nightly build --release",
            "cargo run -- --help",
            "RUSTFLAGS=-Dwarnings cargo check",
            "npm test",
            "npm run test:unit",
            "pnpm lint",
            "yarn typecheck",
            "npx tsc --noEmit",
            "npx vitest run",
            "pytest -q tests/test_api.py",
            "python -m pytest",
            "python3 -m unittest discover",
            "uv run pytest -x",
            "poetry run mypy src",
            "bundle exec rspec",
            "go test ./...",
            "go vet ./...",
            "make",
            "make -j8 all",
            "just check",
            "./gradlew build",
            "mvn -q verify",
            "dotnet test",
            "cmake --build build",
            "node --test",
            "ruff check .",
            "eslint src",
            "C:\\tools\\shellcheck.exe run.sh",
            "./scripts/run-tests.sh",
            "pwsh ./build.ps1",
            "python script.py",
            "node dist/index.js",
            "git status; cargo nextest run",
            "& .\\gradlew.bat assemble",
            "bash -c \"cargo test --quiet\"",
            "cmd /c npm test",
            "./a.out",
            "..\\target\\debug\\app.exe --version",
            "hatch run test",
        ];
        for command in checks {
            assert!(is_check(command, &[]), "{command}");
        }
        let others = [
            "ls -la",
            "git status",
            "git checkout -b test-branch",
            "cat tests/test_api.py",
            "rg build src",
            "cargo fmt",
            "cargo add serde",
            "npm install",
            "pip install pytest",
            "echo done",
            "cd build",
            "mkdir tests",
            "python -c \"print(1)\"",
            "python --version",
            "python -c \"import os.path\"",
            "bash -c \"ls -la\"",
            "test -f Cargo.toml",
            "npm ci",
            "cmake -S . -B build",
            "rm -rf target",
            "sed -i s/test/spec/ a.txt",
            "",
        ];
        for command in others {
            assert!(!is_check(command, &[]), "{command}");
        }
    }

    #[test]
    fn the_projects_own_check_commands_count_whatever_they_are_called() {
        let verify = ["tools/ok --all".to_string(), "  ".to_string()];
        assert!(is_check("tools/ok  --all", &verify));
        assert!(is_check("cd sub && tools/ok --all 2>&1 | tail -5", &verify));
        assert!(!is_check("tools/ok", &verify));
        assert!(!is_check("tools/ok --all", &[]));
        assert!(!is_check("ls", &verify));
    }

    #[test]
    fn a_shell_result_fails_by_its_exit_code_or_when_the_command_did_not_end() {
        let details = |code: Value| serde_json::json!({ "exit_code": code, "timed_out": false });
        assert!(!failed(false, Some(&details(0.into()))));
        assert!(failed(false, Some(&details(1.into()))));
        assert!(failed(false, Some(&details((-1).into()))));
        // Killed or timed out: no code, and the tool says so.
        assert!(failed(true, Some(&details(Value::Null))));
        // Moved to the background: still running.
        assert!(!failed(false, Some(&serde_json::json!({ "job_id": "job-1" }))));
        assert!(!failed(false, None));
    }

    #[test]
    fn prose_is_told_from_code_by_the_extension() {
        for path in ["README.md", "docs/guide.MDX", "notes.txt", "CHANGES.rst"] {
            assert!(is_prose(Path::new(path)), "{path}");
        }
        for path in ["src/main.rs", "Makefile", "config.toml", "a.md.rs", ".env"] {
            assert!(!is_prose(Path::new(path)), "{path}");
        }
    }
}
