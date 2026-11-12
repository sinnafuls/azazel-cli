//! `azazel memory …`, `azazel index` and `azazel rag …`: what is remembered about this project, and
//! what is indexed for semantic search.

use std::path::PathBuf;
use std::sync::Arc;

use azazel_core::{Config, LoadOptions, Paths};
use azazel_memory::{MemoryExt, MemoryRow, MemoryStore, Scope};

use crate::cli::{MemoryAction, RagAction};
use crate::print::{EXIT_FAILED, EXIT_OK};

/// Prefix of the collections `azazel rag` manages; `code` is the project's own index.
const DOCS: &str = "docs:";

/// What one of these commands works on.
pub enum Command<'a> {
    Memory(&'a MemoryAction),
    Index,
    Rag(&'a RagAction),
}

fn open(paths: &Paths, trusted: bool) -> Result<Arc<MemoryStore>, String> {
    let options = LoadOptions {
        trusted,
        overrides: toml::Table::new(),
        env: std::env::vars().collect(),
    };
    let config = Config::load(paths, &options).map_err(|e| e.to_string())?.config;
    let credentials = Arc::new(crate::app::credentials(paths, trusted));
    Ok(MemoryExt::open(paths, &config, credentials)?.store())
}

fn line(row: &MemoryRow) -> String {
    let tags = if row.tags.is_empty() {
        String::new()
    } else {
        format!("  #{}", row.tags.join(" #"))
    };
    format!(
        "{}  {} ({})  {}{tags}",
        row.id,
        row.kind,
        row.scope.as_str(),
        row.text
    )
}

fn lines(rows: &[MemoryRow], none: &str) -> String {
    if rows.is_empty() {
        return none.to_string();
    }
    rows.iter().map(line).collect::<Vec<_>>().join("\n")
}

async fn act(store: &MemoryStore, command: Command<'_>, paths: &Paths) -> Result<String, String> {
    match command {
        Command::Memory(MemoryAction::List) => {
            let mut rows = store.list(Scope::Project).await?;
            rows.extend(store.list(Scope::Global).await?);
            Ok(lines(&rows, "nothing is remembered yet"))
        }
        Command::Memory(MemoryAction::Search { query, limit }) => Ok(lines(
            &store.search(query, *limit, None).await?,
            "no memory matches",
        )),
        Command::Memory(MemoryAction::Forget { id }) => match store.forget(id).await? {
            true => Ok(format!("forgot {id}")),
            false => Err(format!("there is no memory `{id}`")),
        },
        Command::Memory(MemoryAction::Export) => Ok(store.export().await?.trim_end().to_string()),
        Command::Index => {
            let (files, chunks) = store.index_project(true).await?;
            Ok(format!("indexed {files} files in {chunks} chunks"))
        }
        Command::Rag(RagAction::Add { name, paths: given }) => {
            let absolute: Vec<PathBuf> = given.iter().map(|path| paths.cwd.join(path)).collect();
            if let Some(missing) = absolute.iter().find(|path| !path.exists()) {
                return Err(format!("{} does not exist", missing.display()));
            }
            let (files, chunks) = store.add_documents(name, &absolute).await?;
            Ok(format!(
                "{DOCS}{}: {files} files in {chunks} chunks",
                name.trim_start_matches(DOCS)
            ))
        }
        Command::Rag(RagAction::List) => {
            let rows: Vec<String> = store
                .collections()
                .await?
                .into_iter()
                .filter(|(name, _, _)| name.starts_with(DOCS))
                .map(|(name, files, chunks)| format!("{name}: {files} files in {chunks} chunks"))
                .collect();
            Ok(if rows.is_empty() {
                "no document collections".to_string()
            } else {
                rows.join("\n")
            })
        }
        Command::Rag(RagAction::Remove { name }) => match store.remove_collection(name).await? {
            true => Ok(format!("removed {DOCS}{}", name.trim_start_matches(DOCS))),
            false => Err(format!("there is no collection `{name}`")),
        },
    }
}

/// Runs one of the commands and prints its result; the exit code is 1 when it failed.
pub async fn run(command: Command<'_>, paths: &Paths, trusted: bool) -> u8 {
    let done = match open(paths, trusted) {
        Ok(store) => act(&store, command, paths).await,
        Err(why) => Err(why),
    };
    match done {
        Ok(text) => {
            println!("{text}");
            EXIT_OK
        }
        Err(why) => {
            eprintln!("azazel: {why}");
            EXIT_FAILED
        }
    }
}
