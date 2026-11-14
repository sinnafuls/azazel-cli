//! Format on write: after a tool changes a file, the formatter for its kind runs on it. Which one
//! is `[tools.format]`'s for the extension, else the first of a built-in table that the file's
//! project uses: some always when installed (`rustfmt`, `gofmt`), some only when the project
//! says so (`prettier` when `package.json` depends on it, `clang-format` with a `.clang-format`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use azazel_agent::ToolCtx;
use azazel_core::process::AgentEnv;

/// How long a formatter may take on one file.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Stands for the file's path in a configured command; without it the path is appended.
pub(crate) const FILE: &str = "{file}";

/// `command` as words. Quotes group words with spaces; a backslash is an ordinary character, so
/// Windows paths need no escaping.
pub(crate) fn words(command: &str) -> Vec<String> {
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

/// Script-like files every JavaScript formatter takes.
const SCRIPTS: &[&str] = &["js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts"];
/// What prettier and biome format besides scripts.
const WEB: &[&str] = &[
    "js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts", "html", "htm", "css", "scss", "sass", "less",
    "vue", "svelte", "json", "jsonc", "yaml", "yml", "toml", "xml", "md", "mdx", "graphql", "gql",
];
const RUBY: &[&str] = &["rb", "rake", "gemspec", "ru"];
const NODE_BIN: &str = "node_modules/.bin";

/// When a formatter of the table is used for a file, besides its program being found.
#[derive(Debug, Clone, Copy)]
enum When {
    /// Always.
    Installed,
    /// A file of one of these names is in the file's directory or one above it.
    Config(&'static [&'static str]),
    /// The nearest `package.json` lists this package among its dependencies.
    Npm(&'static str),
    /// The project configures ruff (`ruff.toml`, `.ruff.toml`, `[tool.ruff]` in
    /// `pyproject.toml`) or depends on it (`pyproject.toml`, `requirements.txt`, `Pipfile`).
    Ruff,
    /// The nearest `Gemfile` names this gem.
    Gem(&'static str),
    /// The nearest `composer.json` requires this package.
    Composer(&'static str),
    /// The project has a `uv.lock`, and this version of uv has `uv format`.
    Uv,
    /// The program is the R formatter, not another program of the same name: the first line
    /// of its `--help` says so.
    RFormatter,
}

/// One formatter of the built-in table.
#[derive(Debug, Clone, Copy)]
struct Builtin {
    program: &'static str,
    /// With [`FILE`] for the file's path.
    args: &'static [&'static str],
    extensions: &'static [&'static str],
    when: When,
    /// Where under a directory holding the project's manifest its own copy of the program is,
    /// found before the one on `PATH`. Being the project's code, it runs only in a trusted
    /// project.
    local: Option<&'static str>,
    /// Running it runs code of the project (a JavaScript config file, `.formatter.exs`, a
    /// `require` in `.rubocop.yml`), so it is used only in a trusted project.
    runs_code: bool,
    /// It prints the formatted file instead of changing it, and the file is written from that.
    prints: bool,
}

const fn builtin(
    program: &'static str,
    args: &'static [&'static str],
    extensions: &'static [&'static str],
) -> Builtin {
    Builtin {
        program,
        args,
        extensions,
        when: When::Installed,
        local: None,
        runs_code: false,
        prints: false,
    }
}

impl Builtin {
    const fn when(mut self, when: When) -> Builtin {
        self.when = when;
        self
    }

    const fn local(mut self, dir: &'static str) -> Builtin {
        self.local = Some(dir);
        self
    }

    const fn runs_code(mut self) -> Builtin {
        self.runs_code = true;
        self
    }

    const fn prints(mut self) -> Builtin {
        self.prints = true;
        self
    }
}

/// The formatters used when `[tools.format]` names none for a file's extension. For a file, the
/// first of them that takes its extension, applies to its project and is installed is used.
const BUILTINS: [Builtin; 26] = [
    builtin("rustfmt", &[FILE], &["rs"]),
    builtin("gofmt", &["-w", FILE], &["go"]),
    builtin("biome", &["format", "--write", FILE], WEB)
        .when(When::Config(&["biome.json", "biome.jsonc"]))
        .local(NODE_BIN),
    builtin("oxfmt", &[FILE], SCRIPTS)
        .when(When::Npm("oxfmt"))
        .local(NODE_BIN),
    builtin("prettier", &["--write", FILE], WEB)
        .when(When::Npm("prettier"))
        .local(NODE_BIN)
        .runs_code(),
    builtin("ruff", &["format", FILE], &["py", "pyi"]).when(When::Ruff),
    builtin("uv", &["format", "--", FILE], &["py", "pyi"]).when(When::Uv),
    builtin(
        "clang-format",
        &["-i", FILE],
        &[
            "c", "cc", "cpp", "cxx", "c++", "h", "hh", "hpp", "hxx", "h++", "ino",
        ],
    )
    .when(When::Config(&[".clang-format", "_clang-format"])),
    builtin("zig", &["fmt", FILE], &["zig", "zon"]),
    builtin("terraform", &["fmt", FILE], &["tf", "tfvars"]),
    builtin("shfmt", &["-w", FILE], &["sh", "bash"]),
    builtin("dart", &["format", FILE], &["dart"]),
    builtin(
        "mix",
        &["format", FILE],
        &["ex", "exs", "eex", "heex", "leex", "neex", "sface"],
    )
    .runs_code(),
    builtin("gleam", &["format", FILE], &["gleam"]),
    builtin("nixfmt", &[FILE], &["nix"]),
    builtin("ktlint", &["-F", FILE], &["kt", "kts"]),
    builtin("ocamlformat", &["-i", FILE], &["ml", "mli"]).when(When::Config(&[".ocamlformat"])),
    builtin(
        "cljfmt",
        &["fix", "--quiet", FILE],
        &["clj", "cljs", "cljc", "edn"],
    ),
    builtin("dfmt", &["-i", FILE], &["d"]),
    builtin("latexindent", &["-s", FILE], &["tex"]).prints(),
    builtin("htmlbeautifier", &[FILE], &["erb"]),
    builtin("ormolu", &["-i", FILE], &["hs"]),
    builtin("standardrb", &["--fix", FILE], RUBY)
        .when(When::Gem("standard"))
        .runs_code(),
    builtin("rubocop", &["-a", FILE], RUBY)
        .when(When::Gem("rubocop"))
        .runs_code(),
    builtin("pint", &[FILE], &["php"])
        .when(When::Composer("laravel/pint"))
        .local("vendor/bin"),
    builtin("air", &["format", FILE], &["r"]).when(When::RFormatter),
];

/// Where programs are found, and what they answer when asked for help. The tests stand in for
/// the system.
pub(crate) trait Programs: Send + Sync {
    /// `name` on `PATH`.
    fn on_path(&self, name: &str) -> Option<PathBuf>;
    /// `name` in `dir`, with the extensions the system gives programs.
    fn in_dir(&self, dir: &Path, name: &str) -> Option<PathBuf>;
    /// Whether `program` run with `args` succeeds and prints a first line `wanted` accepts.
    fn answers(&self, program: &Path, args: &[&str], wanted: fn(&str) -> bool) -> bool;
}

/// The programs of this system. What a program answered is remembered for the session.
#[derive(Default)]
struct System {
    answered: Mutex<BTreeMap<(PathBuf, String), bool>>,
}

impl Programs for System {
    fn on_path(&self, name: &str) -> Option<PathBuf> {
        which::which(name).ok()
    }

    fn in_dir(&self, dir: &Path, name: &str) -> Option<PathBuf> {
        which::which_in(name, Some(dir), dir).ok()
    }

    fn answers(&self, program: &Path, args: &[&str], wanted: fn(&str) -> bool) -> bool {
        let key = (program.to_path_buf(), args.join(" "));
        let mut answered = self.answered.lock().unwrap_or_else(PoisonError::into_inner);
        *answered.entry(key).or_insert_with(|| {
            std::process::Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .is_ok_and(|output| {
                    output.status.success()
                        && wanted(
                            String::from_utf8_lossy(&output.stdout)
                                .lines()
                                .next()
                                .unwrap_or(""),
                        )
                })
        })
    }
}

/// The formatter settings of a session with `tools.format_on_write` on. Registered as a service;
/// `write` and `edit` format only when it is there.
pub struct Formatters {
    /// `[tools.format]`: file extension (without the dot) to command.
    configured: BTreeMap<String, String>,
    /// The project root: what a file's project uses is looked for up to here.
    root: PathBuf,
    /// Whether the project is trusted: only then are formatters used that run its code.
    trusted: bool,
    programs: Box<dyn Programs>,
}

/// A command that formats a file.
#[derive(Debug, PartialEq)]
struct Formatter {
    program: PathBuf,
    arguments: Vec<String>,
    /// It prints the formatted file rather than changing it.
    prints: bool,
}

/// How a built-in formatter stands for the project, as `azazel doctor` reports it.
#[derive(Debug, PartialEq)]
pub enum Planned {
    /// It formats the files of these extensions with this program.
    Used { program: PathBuf },
    /// The project uses it, but the project is not trusted.
    Untrusted,
}

/// The directories from `dir` up to `root`; only `dir` when it is not under `root`.
fn upward<'a>(dir: &'a Path, root: &'a Path) -> impl Iterator<Item = &'a Path> {
    let inside = dir.starts_with(root);
    dir.ancestors()
        .enumerate()
        .take_while(move |(at, ancestor)| *at == 0 || (inside && ancestor.starts_with(root)))
        .map(|(_, ancestor)| ancestor)
}

/// The nearest file called `name` from `dir` up to `root`.
fn nearest(dir: &Path, root: &Path, name: &str) -> Option<PathBuf> {
    upward(dir, root)
        .map(|at| at.join(name))
        .find(|path| path.is_file())
}

/// Whether the `package.json` at `path` lists `package` in any of its dependency tables.
fn depends_on(path: &Path, package: &str) -> bool {
    let Some(manifest) = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    else {
        return false;
    };
    [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ]
    .iter()
    .any(|table| manifest.get(table).and_then(|deps| deps.get(package)).is_some())
}

/// Whether the `composer.json` at `path` requires `package`.
fn composer_requires(path: &Path, package: &str) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|manifest| {
            ["require", "require-dev"]
                .iter()
                .any(|table| manifest.get(table).and_then(|deps| deps.get(package)).is_some())
        })
}

/// Whether the nearest `Gemfile` names `gem` in a `gem` line.
fn gemfile_names(dir: &Path, root: &Path, gem: &str) -> bool {
    let Some(text) = nearest(dir, root, "Gemfile").and_then(|path| std::fs::read_to_string(path).ok()) else {
        return false;
    };
    text.lines().any(|line| {
        let line = line.trim_start();
        line.strip_prefix("gem ").is_some_and(|rest| {
            let name = rest.trim_start().trim_start_matches(['"', '\'']);
            name.strip_prefix(gem)
                .is_some_and(|after| after.starts_with(['"', '\'']))
        })
    })
}

fn uses_ruff(dir: &Path, root: &Path) -> bool {
    upward(dir, root).any(|at| {
        if at.join("ruff.toml").is_file() || at.join(".ruff.toml").is_file() {
            return true;
        }
        let names = |file: &str, what: fn(&str) -> bool| {
            std::fs::read_to_string(at.join(file)).is_ok_and(|text| what(&text))
        };
        names("pyproject.toml", |text| {
            text.contains("[tool.ruff") || text.contains("ruff")
        }) || names("requirements.txt", |text| {
            text.lines().any(|line| line.trim_start().starts_with("ruff"))
        }) || names("Pipfile", |text| {
            text.lines().any(|line| line.trim_start().starts_with("ruff"))
        })
    })
}

impl Formatters {
    pub fn new(configured: BTreeMap<String, String>, root: PathBuf, trusted: bool) -> Formatters {
        Formatters {
            configured,
            root,
            trusted,
            programs: Box::new(System::default()),
        }
    }

    #[cfg(test)]
    fn with_programs(mut self, programs: impl Programs + 'static) -> Formatters {
        self.programs = Box::new(programs);
        self
    }

    /// The program of `entry` for a file in `dir`, `None` when the entry does not apply there.
    /// `Err` when it applies but needs a trusted project.
    fn program(&self, entry: &Builtin, dir: &Path) -> Option<Result<PathBuf, ()>> {
        let applies = match entry.when {
            When::Installed | When::RFormatter => true,
            When::Config(names) => names.iter().any(|name| nearest(dir, &self.root, name).is_some()),
            When::Npm(package) => {
                nearest(dir, &self.root, "package.json").is_some_and(|path| depends_on(&path, package))
            }
            When::Ruff => uses_ruff(dir, &self.root),
            When::Gem(gem) => gemfile_names(dir, &self.root, gem),
            When::Composer(package) => nearest(dir, &self.root, "composer.json")
                .is_some_and(|path| composer_requires(&path, package)),
            When::Uv => nearest(dir, &self.root, "uv.lock").is_some(),
        };
        if !applies {
            return None;
        }
        let local = entry.local.and_then(|bin| {
            upward(dir, &self.root).find_map(|at| self.programs.in_dir(&at.join(bin), entry.program))
        });
        // The project's own copy runs only in a trusted project; otherwise the one on `PATH`
        // does, except for a program that comes only with the project's packages.
        let program = match local {
            Some(local) if self.trusted => local,
            local => match self.programs.on_path(entry.program) {
                Some(found) if !matches!(entry.when, When::Composer(_)) => found,
                _ if local.is_some() => return Some(Err(())),
                _ => return None,
            },
        };
        if entry.runs_code && !self.trusted {
            return Some(Err(()));
        }
        let works = match entry.when {
            When::Uv => self.programs.answers(&program, &["format", "--help"], |_| true),
            When::RFormatter => self.programs.answers(&program, &["--help"], |first| {
                first.contains("R language") && first.contains("formatter")
            }),
            _ => true,
        };
        works.then_some(Ok(program))
    }

    /// The command that formats `file`: the configured one for its extension, else the first
    /// built-in one that applies to it.
    fn command(&self, file: &Path) -> Option<Result<Formatter, String>> {
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
            return Some(Ok(Formatter {
                program,
                arguments,
                prints: false,
            }));
        }
        let dir = file.parent().unwrap_or(file);
        let (entry, program) = BUILTINS
            .iter()
            .filter(|entry| entry.extensions.contains(&extension.as_str()))
            .find_map(|entry| match self.program(entry, dir)? {
                Ok(program) => Some((entry, program)),
                Err(()) => None,
            })?;
        let mut arguments: Vec<String> = entry.args.iter().map(|arg| arg.replace(FILE, &path)).collect();
        if entry.program == "rustfmt"
            && let Some(edition) = rust_edition(file)
        {
            arguments.splice(0..0, ["--edition".to_string(), edition]);
        }
        Some(Ok(Formatter {
            program,
            arguments,
            prints: entry.prints,
        }))
    }

    /// The built-in formatters that apply to the project, for a file at its root, with the
    /// extensions each takes: the program each runs, or that it needs the project trusted.
    /// An extension `[tools.format]` names is left out of the built-in ones.
    pub fn planned(&self) -> Vec<(String, Vec<&'static str>, Planned)> {
        let mut taken: Vec<&str> = self.configured.keys().map(String::as_str).collect();
        let mut out = Vec::new();
        for entry in &BUILTINS {
            let extensions: Vec<&'static str> = entry
                .extensions
                .iter()
                .copied()
                .filter(|extension| !taken.contains(extension))
                .collect();
            if extensions.is_empty() {
                continue;
            }
            match self.program(entry, &self.root) {
                Some(Ok(program)) => {
                    taken.extend(&extensions);
                    out.push((entry.program.to_string(), extensions, Planned::Used { program }));
                }
                Some(Err(())) => out.push((entry.program.to_string(), extensions, Planned::Untrusted)),
                None => {}
            }
        }
        out
    }

    /// The `[tools.format]` entries whose program is not installed, as (extension, program).
    pub fn missing(&self) -> Vec<(String, String)> {
        self.configured
            .iter()
            .filter_map(|(extension, command)| {
                let name = words(command).into_iter().next()?;
                which::which(&name).is_err().then(|| (extension.clone(), name))
            })
            .collect()
    }

    /// Formats `file` in place. `None` when no formatter applies to it; otherwise the formatter's
    /// name, or one line saying why it did not format.
    pub async fn run(&self, file: &Path, ctx: &ToolCtx) -> Option<Result<String, String>> {
        let formatter = match self.command(file)? {
            Ok(formatter) => formatter,
            Err(why) => return Some(Err(why)),
        };
        let program = &formatter.program;
        let name = program
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.display().to_string());
        let mut command = tokio::process::Command::new(program);
        command
            .args(&formatter.arguments)
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
            Ok(Ok(output)) if output.status.success() && formatter.prints => {
                if output.stdout.is_empty() {
                    Err(format!("{name} printed nothing"))
                } else {
                    crate::util::write_atomic(file, &output.stdout)
                        .map(|()| name.clone())
                        .map_err(|e| format!("{name}: {e}"))
                }
            }
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
        let formatters = Formatters::new(
            BTreeMap::from([("zz".to_string(), "azazel-no-such-formatter --fix".to_string())]),
            PathBuf::from("."),
            true,
        );
        assert_eq!(
            formatters.command(Path::new("a.zz")).unwrap().unwrap_err(),
            "formatter `azazel-no-such-formatter` was not found"
        );
        assert_eq!(
            formatters.missing(),
            [("zz".to_string(), "azazel-no-such-formatter".to_string())]
        );
        assert!(formatters.command(Path::new("a.unknown-kind")).is_none());
        assert!(formatters.command(Path::new("Makefile")).is_none());
    }

    /// Programs as a test sets them up: those named are on `PATH` under `/bin`; a file in a
    /// directory is a program there; asked for help, every program answers `help`.
    struct Fake {
        installed: Vec<&'static str>,
        help: &'static str,
    }

    impl Programs for Fake {
        fn on_path(&self, name: &str) -> Option<PathBuf> {
            self.installed
                .contains(&name)
                .then(|| Path::new("/bin").join(name))
        }
        fn in_dir(&self, dir: &Path, name: &str) -> Option<PathBuf> {
            Some(dir.join(name)).filter(|path| path.is_file())
        }
        fn answers(&self, _: &Path, _: &[&str], wanted: fn(&str) -> bool) -> bool {
            wanted(self.help)
        }
    }

    /// A project in a temporary directory with `files` (path, content).
    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        dir
    }

    fn formatters(root: &Path, trusted: bool, installed: &[&'static str]) -> Formatters {
        Formatters::new(BTreeMap::new(), root.to_path_buf(), trusted).with_programs(Fake {
            installed: installed.to_vec(),
            help: "",
        })
    }

    /// The program and arguments that format `file` of `root`, the path shown as `{file}`.
    fn used(formatters: &Formatters, root: &Path, file: &str) -> Option<String> {
        let path = root.join(file);
        let formatter = formatters.command(&path)?.unwrap();
        let program = match formatter.program.strip_prefix(root) {
            Ok(local) => local.to_string_lossy().replace('\\', "/"),
            Err(_) => formatter.program.file_name()?.to_string_lossy().into_owned(),
        };
        let shown = path.to_string_lossy().into_owned();
        let arguments: Vec<String> = formatter
            .arguments
            .iter()
            .map(|argument| argument.replace(&shown, FILE))
            .collect();
        Some(format!("{program} {}", arguments.join(" ")))
    }

    #[test]
    fn formatters_of_every_language_are_used_when_installed() {
        let root = project(&[]);
        let root = root.path();
        let all = BUILTINS.iter().map(|entry| entry.program).collect::<Vec<_>>();
        let formatters = formatters(root, true, &all);
        for (file, expected) in [
            ("main.go", "gofmt -w {file}"),
            ("a.zig", "zig fmt {file}"),
            ("main.tf", "terraform fmt {file}"),
            ("run.sh", "shfmt -w {file}"),
            ("app.dart", "dart format {file}"),
            ("lib.ex", "mix format {file}"),
            ("a.gleam", "gleam format {file}"),
            ("flake.nix", "nixfmt {file}"),
            ("A.kt", "ktlint -F {file}"),
            ("core.clj", "cljfmt fix --quiet {file}"),
            ("app.d", "dfmt -i {file}"),
            ("paper.tex", "latexindent -s {file}"),
            ("page.erb", "htmlbeautifier {file}"),
            ("Main.hs", "ormolu -i {file}"),
            ("lib.rs", "rustfmt {file}"),
        ] {
            assert_eq!(used(&formatters, root, file).as_deref(), Some(expected), "{file}");
        }
        // Without its program, a language has no formatter.
        assert_eq!(used(&self::formatters(root, true, &[]), root, "main.go"), None);
        // The ones a project has to ask for are not used by a project that does not.
        for file in ["a.ts", "a.py", "a.c", "a.ml", "a.rb", "a.php", "a.r"] {
            assert_eq!(used(&formatters, root, file), None, "{file}");
        }
    }

    #[test]
    fn javascript_formatters_follow_the_project_and_prefer_its_own_copy() {
        let both = project(&[
            ("package.json", r#"{"devDependencies": {"prettier": "^3"}}"#),
            ("node_modules/.bin/prettier", ""),
            ("web/src/app.ts", ""),
        ]);
        let root = both.path();
        assert_eq!(
            used(&formatters(root, true, &["prettier"]), root, "web/src/app.ts").as_deref(),
            Some("node_modules/.bin/prettier --write {file}")
        );
        // Prettier runs the project's config, so an untrusted project gets none.
        assert_eq!(
            used(&formatters(root, false, &["prettier"]), root, "web/src/app.ts"),
            None
        );

        let oxfmt = project(&[
            ("package.json", r#"{"dependencies": {"oxfmt": "1"}}"#),
            ("node_modules/.bin/oxfmt", ""),
        ]);
        let root = oxfmt.path();
        assert_eq!(
            used(&formatters(root, true, &[]), root, "a.ts").as_deref(),
            Some("node_modules/.bin/oxfmt {file}")
        );
        // Not trusted: the project's copy is not run, the one on PATH is.
        assert_eq!(used(&formatters(root, false, &[]), root, "a.ts"), None);
        assert_eq!(
            used(&formatters(root, false, &["oxfmt"]), root, "a.ts").as_deref(),
            Some("oxfmt {file}")
        );
        // Only scripts are oxfmt's.
        assert_eq!(used(&formatters(root, true, &[]), root, "a.css"), None);

        let biome = project(&[
            ("biome.jsonc", "{}"),
            ("package.json", r#"{"devDependencies": {"prettier": "3"}}"#),
        ]);
        let root = biome.path();
        assert_eq!(
            used(&formatters(root, true, &["biome", "prettier"]), root, "a.css").as_deref(),
            Some("biome format --write {file}")
        );
    }

    #[test]
    fn other_formatters_need_the_project_to_use_them() {
        let installed = [
            "clang-format",
            "ruff",
            "uv",
            "ocamlformat",
            "rubocop",
            "standardrb",
        ];
        let c = project(&[(".clang-format", "BasedOnStyle: LLVM\n")]);
        let ocaml = project(&[(".ocamlformat", "")]);
        let ruff = project(&[("pyproject.toml", "[tool.ruff]\nline-length = 100\n")]);
        let ruff_dependency = project(&[("requirements.txt", "requests\nruff==0.5\n")]);
        let uv = project(&[("uv.lock", ""), ("pyproject.toml", "[project]\nname = \"a\"\n")]);
        let standard = project(&[(
            "Gemfile",
            "source 'https://rubygems.org'\ngem \"standard\", group: :dev\n",
        )]);
        let rubocop = project(&[("Gemfile", "gem 'rubocop-rails'\ngem 'rubocop'\n")]);
        let pint = project(&[
            ("composer.json", r#"{"require-dev": {"laravel/pint": "^1"}}"#),
            ("vendor/bin/pint", ""),
        ]);
        for (dir, file, expected) in [
            (&c, "src/a.cpp", "clang-format -i {file}"),
            (&ocaml, "a.ml", "ocamlformat -i {file}"),
            (&ruff, "pkg/a.py", "ruff format {file}"),
            (&ruff_dependency, "a.py", "ruff format {file}"),
            (&uv, "a.py", "uv format -- {file}"),
            (&standard, "a.rb", "standardrb --fix {file}"),
            (&rubocop, "a.rb", "rubocop -a {file}"),
            (&pint, "app/A.php", "vendor/bin/pint {file}"),
        ] {
            let root = dir.path();
            assert_eq!(
                used(&formatters(root, true, &installed), root, file).as_deref(),
                Some(expected),
                "{file}"
            );
        }
        // Rubocop and pint run the project's code.
        for (dir, file) in [(&rubocop, "a.rb"), (&pint, "a.php")] {
            assert_eq!(
                used(&formatters(dir.path(), false, &installed), dir.path(), file),
                None
            );
        }
    }

    #[test]
    fn air_and_uv_are_used_only_when_they_answer_as_the_formatter() {
        let project = project(&[("uv.lock", "")]);
        let root = project.path();
        let asked = |help: &'static str| {
            Formatters::new(BTreeMap::new(), root.to_path_buf(), true).with_programs(Fake {
                installed: vec!["air", "uv"],
                help,
            })
        };
        let r_air = asked("Air: An R language server and formatter");
        assert_eq!(used(&r_air, root, "a.R").as_deref(), Some("air format {file}"));
        // The Go program called air is something else.
        assert_eq!(used(&asked("Usage of air:"), root, "a.R"), None);
        assert_eq!(used(&r_air, root, "a.py").as_deref(), Some("uv format -- {file}"));
    }

    #[test]
    fn the_plan_names_what_the_project_would_use_and_what_needs_trust() {
        let project = project(&[
            ("package.json", r#"{"devDependencies": {"prettier": "3"}}"#),
            (".clang-format", ""),
        ]);
        let root = project.path();
        let installed = ["rustfmt", "prettier", "clang-format"];
        let mut configured = Formatters::new(
            BTreeMap::from([("rs".to_string(), "rustfmt --edition 2024".to_string())]),
            root.to_path_buf(),
            false,
        )
        .with_programs(Fake {
            installed: installed.to_vec(),
            help: "",
        });
        let shown = |formatters: &Formatters| -> Vec<String> {
            formatters
                .planned()
                .into_iter()
                .map(|(name, extensions, planned)| {
                    let state = match planned {
                        Planned::Used { .. } => "used",
                        Planned::Untrusted => "untrusted",
                    };
                    format!("{name} {} {state}", extensions.len())
                })
                .collect()
        };
        // `rs` is configured, so rustfmt's only extension is taken.
        assert_eq!(
            shown(&configured),
            ["prettier 26 untrusted", "clang-format 11 used"]
        );
        configured.trusted = true;
        assert_eq!(shown(&configured), ["prettier 26 used", "clang-format 11 used"]);
    }
}
