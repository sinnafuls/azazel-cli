//! `azazel migrate` and `/migrate`: bringing over what pi, omp, jcode, Claude Code and Codex
//! CLI hold: providers, models and roles, API keys, skills, commands, agent definitions, MCP
//! servers, rule files, settings, conversations and memories.
//!
//! Each source has a reader ([`pi`], [`omp`], [`jcode`], [`claude`], [`codex`]) that turns its
//! files into one shape ([`model::Found`]); [`write`] brings that into azazel's own files and
//! reports on every thing. The sources are only ever read.

mod chat;
mod claude;
mod codex;
mod convert;
mod jcode;
mod model;
mod omp;
mod pi;
#[cfg(test)]
mod tests;
mod write;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use azazel_core::Paths;

pub use self::convert::Env;
pub use self::model::{Chat, ChatRef, Found, Kind, MemoryItem, Report, Scope, Source, Status};
use crate::print::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};

/// What to bring over, and how.
#[derive(Clone)]
pub struct Options {
    pub kinds: Vec<Kind>,
    /// Report what would be done; write nothing.
    pub dry_run: bool,
    /// Replace what is already there instead of leaving it.
    pub overwrite: bool,
    /// Told what is being worked on, for a run that takes long.
    pub progress: Arc<dyn Fn(&str) + Send + Sync>,
}

impl Options {
    pub fn new(kinds: Vec<Kind>, dry_run: bool, overwrite: bool) -> Options {
        Options {
            kinds,
            dry_run,
            overwrite,
            progress: Arc::new(|_| {}),
        }
    }
}

/// Where `source` keeps its data for the user of `env`.
pub fn root_of(source: Source, env: &Env) -> PathBuf {
    match source {
        Source::Pi => pi::root(env),
        Source::Omp => omp::root(env),
        Source::Jcode => jcode::root(env),
        Source::Claude => claude::root(env),
        Source::Codex => codex::root(env),
    }
}

/// What each of `sources` holds, for the user and for `project`. A source that is not
/// installed is left out.
pub fn scan(env: &Env, project: &Path, sources: &[Source]) -> Vec<Found> {
    sources
        .iter()
        .filter_map(|source| match source {
            Source::Pi => pi::scan(env, project),
            Source::Omp => omp::scan(env, project),
            Source::Jcode => jcode::scan(env, project),
            Source::Claude => claude::scan(env, project),
            Source::Codex => codex::scan(env, project),
        })
        .collect()
}

/// Reads one conversation of the source `found` came from.
fn read_chat(found: &Found, reference: &ChatRef) -> Result<Chat, String> {
    match found.source {
        Source::Pi => pi::read_chat(reference, None),
        Source::Omp => omp::read_chat(&found.root, reference),
        Source::Jcode => jcode::read_chat(reference),
        Source::Claude => claude::read_chat(reference),
        Source::Codex => codex::read_chat(reference),
    }
}

/// Brings what was found into the azazel home of `paths` (and into the projects it belongs
/// to), or for a dry run says what that would do.
pub async fn migrate(paths: &Paths, found: &[Found], options: &Options) -> Report {
    let mut report = Report {
        dry_run: options.dry_run,
        ..Report::default()
    };
    // A dry run edits a copy of the configuration, so that what it reports is what the files
    // would take.
    let scratch = match options.dry_run {
        true => tempfile::tempdir().ok(),
        false => None,
    };
    let config_home = match &scratch {
        Some(scratch) => {
            for name in ["config.toml", azazel_ai::models_file::FILE_NAME] {
                let _ = std::fs::copy(paths.home.join(name), scratch.path().join(name));
            }
            scratch.path().to_path_buf()
        }
        None => paths.home.clone(),
    };
    let mut planned = write::Planned::default();
    for found in found {
        report.sources.push((found.source, found.root.clone()));
        let mut run = write::Run {
            paths,
            options,
            report: &mut report,
            planned: &mut planned,
            source: found.source,
            config_home: config_home.clone(),
        };
        write::apply(&mut run, found).await;
    }
    report
}

/// The sources `--from` names: one, several separated by commas, or `all`.
pub fn parse_sources(names: &[String]) -> Result<Vec<Source>, String> {
    let mut sources = Vec::new();
    for name in names.iter().flat_map(|names| names.split(',')).map(str::trim) {
        match (name, Source::parse(name)) {
            ("" | "all", _) => return Ok(Source::ALL.to_vec()),
            (_, Some(source)) if !sources.contains(&source) => sources.push(source),
            (_, Some(_)) => {}
            (_, None) => {
                let known: Vec<&str> = Source::ALL.iter().map(|source| source.name()).collect();
                return Err(format!(
                    "unknown source `{name}`: expected {}, or all",
                    known.join(", ")
                ));
            }
        }
    }
    Ok(match sources.is_empty() {
        true => Source::ALL.to_vec(),
        false => sources,
    })
}

/// The kinds `--only` names; every kind when it names none.
pub fn parse_kinds(names: &[String]) -> Result<Vec<Kind>, String> {
    let mut kinds = Vec::new();
    for name in names.iter().flat_map(|names| names.split(',')).map(str::trim) {
        match Kind::parse(name) {
            _ if name.is_empty() => {}
            Some(kind) if !kinds.contains(&kind) => kinds.push(kind),
            Some(_) => {}
            None => {
                let known: Vec<&str> = Kind::ALL.iter().map(|kind| kind.name()).collect();
                return Err(format!("unknown kind `{name}`: expected {}", known.join(", ")));
            }
        }
    }
    Ok(match kinds.is_empty() {
        true => Kind::ALL.to_vec(),
        false => kinds,
    })
}

/// What a source holds, in a line: `16 models and roles, 3 keys, 240 chats`.
pub fn counts(found: &Found) -> String {
    let parts: Vec<String> = Kind::ALL
        .into_iter()
        .filter_map(|kind| match found.count(kind) {
            0 => None,
            count => Some(kind.counted(count)),
        })
        .collect();
    match parts.is_empty() {
        true => "nothing to bring over".to_string(),
        false => parts.join(", "),
    }
}

/// `azazel migrate --fix-chats`: renames the tool calls in the sessions imported earlier, in
/// every project, to azazel's tool names, as an import does now.
pub fn fix_chats(paths: &Paths) -> u8 {
    let projects = paths.home.join("projects");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&projects)
        .map(|entries| entries.flatten().map(|entry| entry.path().join("sessions")).collect())
        .unwrap_or_default();
    dirs.sort();
    let (mut files, mut calls, mut failed) = (0, 0, false);
    for dir in dirs.iter().filter(|dir| dir.is_dir()) {
        match chat::fix_chats(dir) {
            Ok(fixed) => {
                for one in fixed {
                    println!(
                        "fixed     {}: {} tool calls renamed (the old file is kept as {})",
                        one.path.display(),
                        one.calls,
                        one.path.with_extension(chat::BACKUP_EXTENSION).display()
                    );
                    files += 1;
                    calls += one.calls;
                }
            }
            Err(e) => {
                println!("failed    {}: {e}", dir.display());
                failed = true;
            }
        }
    }
    match files {
        0 => println!("no imported conversation calls a tool by another harness's name"),
        _ => println!("{calls} tool calls renamed in {files} imported conversations"),
    }
    match failed {
        true => EXIT_FAILED,
        false => EXIT_OK,
    }
}

/// The arguments of `azazel migrate`.
pub struct Args<'a> {
    pub from: &'a [String],
    pub only: &'a [String],
    pub dry_run: bool,
    pub overwrite: bool,
    pub project: Option<&'a Path>,
}

/// `azazel migrate`: reads the sources, brings their data over and prints the report. The exit
/// code is 1 when something failed.
pub async fn run(args: &Args<'_>, paths: &Paths) -> u8 {
    let (sources, kinds) = match (parse_sources(args.from), parse_kinds(args.only)) {
        (Ok(sources), Ok(kinds)) => (sources, kinds),
        (Err(why), _) | (_, Err(why)) => {
            eprintln!("azazel: {why}");
            return EXIT_USAGE;
        }
    };
    let Some(env) = Env::detect() else {
        eprintln!("azazel: cannot find your home directory");
        return EXIT_FAILED;
    };
    let project = match args.project {
        Some(dir) if !dir.is_dir() => {
            eprintln!("azazel: {} is not a directory", dir.display());
            return EXIT_USAGE;
        }
        Some(dir) => azazel_core::paths::canonical(dir),
        None => paths.project_root.clone(),
    };
    let found = scan(&env, &project, &sources);
    if found.is_empty() {
        let looked: Vec<String> = sources
            .iter()
            .map(|source| format!("  {}: {}", source.label(), root_of(*source, &env).display()))
            .collect();
        println!("nothing to migrate. Looked in:\n{}", looked.join("\n"));
        // Asking for one source by name and not finding it is a failure; finding none of all is not.
        return match args.from.is_empty() {
            true => EXIT_OK,
            false => EXIT_FAILED,
        };
    }
    let chatty = std::io::stderr().is_terminal();
    for found in &found {
        eprintln!("{}: {}", found.source.label(), counts(found));
    }
    let mut options = Options::new(kinds, args.dry_run, args.overwrite);
    if chatty {
        options.progress = Arc::new(|what| eprintln!("{what}"));
    }
    let report = migrate(paths, &found, &options).await;
    println!("{}", report.render());
    match report.count(Status::Failed) {
        0 => EXIT_OK,
        _ => EXIT_FAILED,
    }
}
