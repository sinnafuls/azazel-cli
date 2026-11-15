//! What the repository map adds to a session: the `repo_map` tool, `/repomap`, and, when
//! `[context] repo_map_tokens` is set, a part of the lead's system prompt.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, AgentInfo, Tool, ToolCtx, ToolOutput};
use azazel_core::{
    Command, CommandHandler, CommandOutcome, ExtCtx, ExtError, Extension, Paths, Registrar,
    SystemPartProvider,
};
use serde_json::{Value, json};

use crate::index::{Stats, scan};
use crate::rank::{Focus, outline};

pub const TOOL: &str = "repo_map";
/// The budget when a caller gives none.
const DEFAULT_TOKENS: usize = 1024;
/// The most a caller may ask for.
const MAX_TOKENS: usize = 32_000;

/// The map of one project.
pub struct RepoMap {
    root: PathBuf,
    cache_file: PathBuf,
    /// `[context] repo_map_tokens`: the budget of the system prompt part; 0 leaves it out.
    prompt_tokens: usize,
    /// What the system prompt part shows, computed on first use and kept until `/repomap refresh`
    /// so that the prompt cache stays valid.
    snapshot: tokio::sync::Mutex<Option<String>>,
}

impl RepoMap {
    pub fn new(root: PathBuf, cache_file: PathBuf, prompt_tokens: usize) -> Self {
        RepoMap {
            root,
            cache_file,
            prompt_tokens,
            snapshot: tokio::sync::Mutex::new(None),
        }
    }

    /// The outline of the project within `tokens`, and what reading the files took.
    pub async fn map(&self, focus: Vec<String>, tokens: usize) -> Result<(String, Stats), String> {
        let root = self.root.clone();
        let cache = self.cache_file.clone();
        tokio::task::spawn_blocking(move || {
            let (files, stats) = scan(&root, &cache);
            (outline(&files, &Focus { items: focus }, tokens), stats)
        })
        .await
        .map_err(|why| format!("the repository map could not be built: {why}"))
    }

    /// The budget of the prompt part, or the default when it is off.
    fn command_tokens(&self) -> usize {
        match self.prompt_tokens {
            0 => DEFAULT_TOKENS,
            tokens => tokens,
        }
    }

    async fn refresh(&self) -> Result<String, String> {
        let (text, _) = self.map(Vec::new(), self.command_tokens()).await?;
        if self.prompt_tokens > 0 {
            *self.snapshot.lock().await = Some(text.clone());
        }
        Ok(text)
    }
}

struct RepoMapTool(Arc<RepoMap>);

#[async_trait]
impl Tool for RepoMapTool {
    fn name(&self) -> &str {
        TOOL
    }

    fn description(&self) -> &str {
        "An outline of the project's most important definitions (functions, types, traits, classes, \
         constants) of Rust, Python, Go, JavaScript and TypeScript files, ranked by how much the rest of \
         the code refers to them, within a token budget. Each file is `path:` followed by its indented \
         signature lines, with `⋮` where lines were left out. `focus` lists the files, directories or \
         names you are working on: the map then favours what they are connected to."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "focus": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Paths (relative to the project root) or names of definitions to favour"
                },
                "tokens": {
                    "type": "integer",
                    "description": "Budget of the outline in tokens (default 1024)"
                }
            }
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn replay_safe(&self, _: &Value) -> bool {
        true
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let focus: Vec<String> = input
            .get("focus")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|item| item.trim().replace('\\', "/"))
                    .filter(|item| !item.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let tokens = input
            .get("tokens")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_TOKENS, |n| (n as usize).clamp(1, MAX_TOKENS));
        match self.0.map(focus, tokens).await {
            Ok((text, _)) if text.is_empty() => {
                ToolOutput::text("no Rust, Python, Go, JavaScript or TypeScript definitions were found")
            }
            Ok((text, _)) => ToolOutput::text(text),
            Err(why) => ToolOutput::error(why),
        }
    }
}

struct RepoMapPart(Arc<RepoMap>);

#[async_trait]
impl SystemPartProvider for RepoMapPart {
    async fn render(&self, agent: &AgentInfo) -> Option<String> {
        if self.0.prompt_tokens == 0 || agent.depth != 0 {
            return None;
        }
        let mut snapshot = self.0.snapshot.lock().await;
        if snapshot.is_none() {
            match self.0.map(Vec::new(), self.0.prompt_tokens).await {
                Ok((text, _)) => *snapshot = Some(text),
                Err(why) => {
                    tracing::warn!("{why}");
                    return None;
                }
            }
        }
        let text = snapshot.as_deref().filter(|text| !text.is_empty())?;
        Some(format!(
            "# Repository map\nThe most referenced definitions of the project, as they were when the \
             session started. It is an outline, not the code: read a file before changing it, and call \
             `{TOOL}` for a map around other files.\n\n{text}"
        ))
    }
}

struct RepoMapCommand(Arc<RepoMap>);

#[async_trait]
impl CommandHandler for RepoMapCommand {
    async fn run(&self, args: &str, _: &ExtCtx) -> CommandOutcome {
        let result = match args.trim() {
            "" => self
                .0
                .map(Vec::new(), self.0.command_tokens())
                .await
                .map(|(text, _)| text),
            "refresh" => self.0.refresh().await,
            _ => return CommandOutcome::Error("usage: /repomap [refresh]".to_string()),
        };
        match result {
            Ok(text) if text.is_empty() => CommandOutcome::Show(
                "no Rust, Python, Go, JavaScript or TypeScript definitions were found".to_string(),
            ),
            Ok(text) => CommandOutcome::Show(text),
            Err(why) => CommandOutcome::Error(why),
        }
    }
}

/// Registers the `repo_map` tool, `/repomap` and the system prompt part.
pub struct RepoMapExt {
    map: Arc<RepoMap>,
}

impl RepoMapExt {
    /// `prompt_tokens` is `[context] repo_map_tokens`.
    pub fn new(paths: &Paths, prompt_tokens: usize) -> Self {
        RepoMapExt {
            map: Arc::new(RepoMap::new(
                paths.project_root.clone(),
                paths.project_data.join("repomap.json"),
                prompt_tokens,
            )),
        }
    }
}

impl Extension for RepoMapExt {
    fn id(&self) -> &str {
        "repomap"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.service(self.map.clone());
        r.tool(Arc::new(RepoMapTool(self.map.clone())));
        r.system_part("repomap", Arc::new(RepoMapPart(self.map.clone())));
        r.command(Command {
            name: "repomap".to_string(),
            description: "The repository map: the project's most referenced definitions".to_string(),
            argument_hint: Some("[refresh]".to_string()),
            handler: Arc::new(RepoMapCommand(self.map.clone())),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("core.rs"), "pub fn shared() {}\npub fn lonely() {}\n").unwrap();
        fs::write(src.join("a.rs"), "pub fn a_one() { shared(); }\n").unwrap();
        fs::write(src.join("b.rs"), "pub fn b_one() { shared(); }\n").unwrap();
        fs::write(dir.path().join(".ignore"), "ignored/\n").unwrap();
        fs::create_dir_all(dir.path().join("ignored")).unwrap();
        fs::write(
            dir.path().join("ignored").join("hidden.rs"),
            "pub fn hidden_fn() { shared(); }\n",
        )
        .unwrap();
        fs::write(
            src.join("gen.rs"),
            "// @generated\npub fn generated_fn() { shared(); }\n",
        )
        .unwrap();
        dir
    }

    fn map_of(dir: &tempfile::TempDir, cache: &tempfile::TempDir) -> RepoMap {
        RepoMap::new(dir.path().to_path_buf(), cache.path().join("repomap.json"), 0)
    }

    #[tokio::test]
    async fn ignored_and_generated_files_are_left_out() {
        let (dir, cache) = (project(), tempfile::tempdir().unwrap());
        let (text, _) = map_of(&dir, &cache).map(Vec::new(), 1000).await.unwrap();
        assert!(text.contains("src/core.rs:"), "{text}");
        assert!(
            !text.contains("hidden_fn") && !text.contains("generated_fn"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_second_run_reads_only_the_files_that_changed() {
        let (dir, cache) = (project(), tempfile::tempdir().unwrap());
        let map = map_of(&dir, &cache);
        let (_, cold) = map.map(Vec::new(), 1000).await.unwrap();
        let (_, warm) = map.map(Vec::new(), 1000).await.unwrap();
        assert_eq!((cold.cached, warm.parsed), (0, 0));
        assert_eq!(warm.cached, cold.parsed);

        let file = dir.path().join("src").join("a.rs");
        fs::write(&file, "pub fn a_one() { shared(); }\npub fn a_two() {}\n").unwrap();
        let (text, again) = map.map(Vec::new(), 1000).await.unwrap();
        assert_eq!(again.parsed, 1);
        assert!(text.contains("a_two"), "{text}");
    }

    #[tokio::test]
    async fn the_map_fits_its_budget() {
        let (dir, cache) = (project(), tempfile::tempdir().unwrap());
        let (text, _) = map_of(&dir, &cache).map(Vec::new(), 20).await.unwrap();
        assert!(!text.is_empty() && text.len() <= 80, "{text}");
    }

    #[tokio::test]
    async fn the_prompt_part_is_computed_once_until_refreshed() {
        let (dir, cache) = (project(), tempfile::tempdir().unwrap());
        let map = Arc::new(RepoMap::new(
            dir.path().to_path_buf(),
            cache.path().join("repomap.json"),
            500,
        ));
        let part = RepoMapPart(map.clone());
        let lead = AgentInfo {
            id: Default::default(),
            def: "lead".to_string(),
            parent: None,
            depth: 0,
            session_id: String::new(),
        };
        let first = part.render(&lead).await.unwrap();
        assert!(first.contains("shared"));
        fs::write(
            dir.path().join("src").join("new.rs"),
            "pub fn brand_new() { shared(); }\n",
        )
        .unwrap();
        assert_eq!(part.render(&lead).await.unwrap(), first);
        map.refresh().await.unwrap();
        assert!(part.render(&lead).await.unwrap().contains("brand_new"));
        let sub = AgentInfo { depth: 1, ..lead };
        assert!(part.render(&sub).await.is_none());
    }
}
