//! What memory adds to a session: the memory tools and `semantic_search`, the `memory` part of the
//! system prompt, the `memory://` addresses, recall on every prompt, automatic extraction, index
//! upkeep and `/memory`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{AgentInfo, ToolCtx};
use azazel_ai::{Block, Credentials, Router};
use azazel_core::{
    Command, CommandHandler, CommandOutcome, Config, Event, EventHandler, EventKind, ExtCtx, ExtError,
    Extension, Memory, MemoryApi, MemoryHit, Outcome, Paths, Registrar, Scheme, SessionStore,
    SystemPartProvider,
};
use serde_json::Value;

use crate::extract::extract;
use crate::store::{MemoryRow, MemoryStore, Scope};
use crate::tools::{ForgetTool, SaveTool, SearchTool, SemanticSearchTool, describe};

/// Memories looked up for each prompt.
const RECALL_LIMIT: usize = 5;
/// Memories `memory://search/<query>` returns.
const SCHEME_HITS: usize = 20;
/// How long after an agent's write the file is re-indexed.
const REINDEX_DELAY: Duration = Duration::from_secs(2);
/// The longest an extraction may take; a session's end waits for it.
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(120);
/// Tool calls remembered while waiting for their results; calls that were denied never get one.
const MAX_TRACKED_CALLS: usize = 256;
const WRITING_TOOLS: [&str; 2] = ["write", "edit"];

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct State {
    store: Arc<MemoryStore>,
    /// `memory.auto_extract` and `memory.auto_index` from the configuration.
    extract_by_default: bool,
    auto_index: bool,
    /// Whether this session extracts; `/memory auto` flips it.
    extracting: AtomicBool,
    /// Ids of the memories this session's prompts have already been given.
    shown: Mutex<HashSet<String>>,
    /// Paths of the `write` and `edit` calls in flight, by call id.
    calls: Mutex<HashMap<String, PathBuf>>,
    /// The latest scheduled re-index of each path; an older one that wakes up does nothing.
    scheduled: Mutex<HashMap<PathBuf, u64>>,
    /// Held while an extraction runs, so two never store the same thing.
    extraction: tokio::sync::Mutex<()>,
}

impl State {
    fn session_started(self: &Arc<Self>) {
        locked(&self.shown).clear();
        self.extracting.store(self.extract_by_default, Ordering::Relaxed);
        let state = self.clone();
        tokio::spawn(async move {
            if let Err(why) = state.store.embed_memories().await {
                tracing::warn!("memories could not be embedded: {why}");
            }
            if state.auto_index
                && let Err(why) = state.store.index_project(false).await
            {
                tracing::warn!("the project index could not be refreshed: {why}");
            }
        });
    }

    /// The `<memory>` block for a prompt: the best-matching memories it has not been given yet in
    /// this session and that the system prompt does not already carry.
    async fn recall(&self, prompt: &str) -> Option<String> {
        if prompt.trim().is_empty() {
            return None;
        }
        let hits = match self.store.search(prompt, RECALL_LIMIT, None).await {
            Ok(hits) => hits,
            Err(why) => {
                tracing::warn!("memory recall failed: {why}");
                return None;
            }
        };
        let pinned: HashSet<String> = self
            .store
            .pinned()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|row| row.id)
            .collect();
        let fresh: Vec<MemoryRow> = {
            let mut shown = locked(&self.shown);
            hits.into_iter()
                .filter(|hit| !pinned.contains(&hit.id) && shown.insert(hit.id.clone()))
                .collect()
        };
        if fresh.is_empty() {
            return None;
        }
        let mut block = String::from("\n\n<memory>\n");
        for memory in &fresh {
            block.push_str(&format!("- [{}] {}\n", memory.kind, memory.text));
        }
        block.push_str("</memory>");
        Some(block)
    }

    fn call_started(&self, call_id: &str, input: &Value, ctx: &ExtCtx) {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return;
        };
        let mut calls = locked(&self.calls);
        if calls.len() >= MAX_TRACKED_CALLS {
            calls.clear();
        }
        calls.insert(call_id.to_string(), ctx.cwd.join(path));
    }

    /// Re-indexes the file a finished `write` or `edit` touched, once it has been quiet for
    /// [`REINDEX_DELAY`].
    fn call_finished(self: &Arc<Self>, call_id: &str, failed: bool) {
        let Some(path) = locked(&self.calls).remove(call_id) else {
            return;
        };
        if failed {
            return;
        }
        let turn = {
            let mut scheduled = locked(&self.scheduled);
            let turn = scheduled.entry(path.clone()).or_insert(0);
            *turn += 1;
            *turn
        };
        let state = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(REINDEX_DELAY).await;
            {
                let mut scheduled = locked(&state.scheduled);
                if scheduled.get(&path) != Some(&turn) {
                    return;
                }
                scheduled.remove(&path);
            }
            if let Err(why) = state.store.refresh_file(&path).await {
                tracing::warn!("{} could not be re-indexed: {why}", path.display());
            }
        });
    }

    /// Runs one extraction over the open session. Failures are logged, never surfaced.
    async fn extract(&self, ctx: &ExtCtx) {
        if !self.extracting.load(Ordering::Relaxed) {
            return;
        }
        if azazel_core::limits::reached(&ctx.services) {
            tracing::info!("memory extraction skipped: the run's cost limit is reached");
            return;
        }
        let (Some(router), Some(session)) =
            (ctx.services.get::<Router>(), ctx.services.get::<SessionStore>())
        else {
            return;
        };
        let _one_at_a_time = self.extraction.lock().await;
        let run = extract(&self.store, &router, &session);
        match tokio::time::timeout(EXTRACT_TIMEOUT, run).await {
            Ok(Ok(stored)) => tracing::debug!("extracted {stored} memories"),
            Ok(Err(why)) => tracing::warn!("memory extraction failed: {why}"),
            Err(_) => tracing::warn!("memory extraction timed out"),
        }
    }
}

struct Handler(Arc<State>);

#[async_trait]
impl EventHandler for Handler {
    async fn handle(&self, event: &mut Event, ctx: &ExtCtx) -> Outcome {
        let state = &self.0;
        match event {
            Event::SessionStart { .. } => state.session_started(),
            Event::UserPrompt { text, .. } => {
                if let Some(block) = state.recall(text).await {
                    text.push_str(&block);
                }
            }
            Event::ToolCall {
                call_id, tool, input, ..
            } if WRITING_TOOLS.contains(&tool.as_str()) => state.call_started(call_id, input, ctx),
            Event::ToolResult {
                call_id,
                tool,
                output,
                ..
            } if WRITING_TOOLS.contains(&tool.as_str()) => state.call_finished(call_id, output.is_error),
            // The process may end right after the session does, so this one is waited for.
            Event::SessionEnd => state.extract(ctx).await,
            Event::PostCompact { agent } if agent.depth == 0 => {
                let (state, ctx) = (state.clone(), ctx.clone());
                tokio::spawn(async move { state.extract(&ctx).await });
            }
            _ => {}
        }
        Outcome::Continue
    }
}

struct MemoryPart(Arc<MemoryStore>);

#[async_trait]
impl SystemPartProvider for MemoryPart {
    async fn render(&self, _: &AgentInfo) -> Option<String> {
        let pinned = match self.0.pinned().await {
            Ok(pinned) => pinned,
            Err(why) => {
                tracing::warn!("the memory part could not be rendered: {why}");
                return None;
            }
        };
        if pinned.is_empty() {
            return None;
        }
        let mut text = String::from(
            "# Memory\nPreferences and conventions remembered from earlier sessions. Follow them unless the \
             user says otherwise. Each is given here in full: answer from this list directly, and search \
             memory only for what it does not cover.",
        );
        for memory in &pinned {
            text.push_str(&format!("\n- [{}] {}", memory.kind, memory.text));
        }
        Some(text)
    }
}

/// `memory://` lists what is remembered, `memory://<id>` shows one memory and
/// `memory://search/<query>` the memories that match a query.
struct MemoryScheme(Arc<MemoryStore>);

impl MemoryScheme {
    async fn all(&self) -> Result<Vec<MemoryRow>, String> {
        let mut rows = self.0.list(Scope::Project).await?;
        rows.append(&mut self.0.list(Scope::Global).await?);
        Ok(rows)
    }
}

/// `text` with `%20`-style escapes and `+` turned back into the characters they stand for.
fn unescaped(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%')
            .then(|| text.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[async_trait]
impl Scheme for MemoryScheme {
    fn scheme(&self) -> &str {
        "memory"
    }

    async fn read(&self, rest: &str, _: &ToolCtx) -> Result<Vec<Block>, String> {
        let rest = rest.trim().trim_end_matches('/');
        let text = if rest.is_empty() {
            let rows = self.all().await?;
            if rows.is_empty() {
                "nothing is remembered yet".to_string()
            } else {
                rows.iter().map(describe).collect::<Vec<_>>().join("\n")
            }
        } else if let Some(query) = rest
            .strip_prefix("search/")
            .or_else(|| (rest == "search").then_some(""))
        {
            let query = unescaped(query);
            if query.trim().is_empty() {
                return Err("memory://search/: give the words to search for after the slash".to_string());
            }
            let hits = self.0.search(&query, SCHEME_HITS, None).await?;
            if hits.is_empty() {
                format!("no memory matches `{}`", query.trim())
            } else {
                hits.iter().map(describe).collect::<Vec<_>>().join("\n")
            }
        } else {
            let rows = self.all().await?;
            let row = rows.iter().find(|row| row.id == rest).ok_or_else(|| {
                format!("no memory has the id `{rest}`; memory:// lists them, memory://search/<query> searches")
            })?;
            let mut text = format!(
                "id: {}\nkind: {}\nscope: {}\nsaved: {}\n",
                row.id,
                row.kind,
                row.scope.as_str(),
                row.created_at
            );
            if !row.tags.is_empty() {
                text.push_str(&format!("tags: {}\n", row.tags.join(", ")));
            }
            text.push_str(&format!("\n{}", row.text));
            text
        };
        Ok(vec![Block::text(text)])
    }
}

struct MemoryCommand(Arc<State>);

impl MemoryCommand {
    async fn list(&self) -> Result<String, String> {
        let store = &self.0.store;
        let mut rows = store.list(Scope::Project).await?;
        rows.append(&mut store.list(Scope::Global).await?);
        if rows.is_empty() {
            return Ok("nothing is remembered yet".to_string());
        }
        Ok(rows.iter().map(describe).collect::<Vec<_>>().join("\n"))
    }

    fn auto(&self) -> String {
        let state = if self.0.extracting.load(Ordering::Relaxed) {
            "on"
        } else {
            "off"
        };
        format!("automatic extraction is {state} for this session")
    }
}

#[async_trait]
impl CommandHandler for MemoryCommand {
    async fn run(&self, args: &str, _: &ExtCtx) -> CommandOutcome {
        let words: Vec<&str> = args.split_whitespace().collect();
        let result = match words.as_slice() {
            [] | ["list"] => self.list().await,
            ["forget", id] => match self.0.store.forget(id).await {
                Ok(true) => Ok(format!("forgot {id}")),
                Ok(false) => Err(format!("no memory has the id `{id}`; /memory lists them")),
                Err(why) => Err(why),
            },
            ["auto"] => Ok(self.auto()),
            ["auto", switch @ ("on" | "off")] => {
                self.0.extracting.store(*switch == "on", Ordering::Relaxed);
                Ok(self.auto())
            }
            _ => Err("usage: /memory [list | forget <id> | auto on|off]".to_string()),
        };
        match result {
            Ok(shown) => CommandOutcome::Show(shown),
            Err(why) => CommandOutcome::Error(why),
        }
    }
}

/// The store behind the seam other crates reach memory through.
struct Seam(Arc<MemoryStore>);

#[async_trait]
impl MemoryApi for Seam {
    async fn save(&self, text: &str, kind: &str, tags: &[String], global: bool) -> Result<String, String> {
        let scope = if global { Scope::Global } else { Scope::Project };
        self.0.save(text, kind, tags, scope, None).await
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryHit>, String> {
        let rows = self.0.search(query, limit, None).await?;
        Ok(rows
            .into_iter()
            .map(|row| MemoryHit {
                id: row.id,
                kind: row.kind,
                text: row.text,
                tags: row.tags,
                scope: row.scope.as_str().to_string(),
            })
            .collect())
    }
}

/// Project memory and retrieval. Registers the memory tools and `semantic_search`, the `memory`
/// system part, recall, automatic extraction, index upkeep, `/memory`, and the [`MemoryStore`] and
/// [`Memory`] services.
pub struct MemoryExt {
    state: Arc<State>,
}

impl MemoryExt {
    /// Opens (creating when missing) the stores of this project. `config.memory.enabled == false`
    /// is the caller's business.
    pub fn open(paths: &Paths, config: &Config, credentials: Arc<Credentials>) -> Result<MemoryExt, String> {
        let store = Arc::new(MemoryStore::open(paths, config, credentials)?);
        Ok(MemoryExt {
            state: Arc::new(State {
                store,
                extract_by_default: config.memory.auto_extract,
                auto_index: config.memory.auto_index,
                extracting: AtomicBool::new(config.memory.auto_extract),
                shown: Mutex::default(),
                calls: Mutex::default(),
                scheduled: Mutex::default(),
                extraction: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// The stores, for the command line.
    pub fn store(&self) -> Arc<MemoryStore> {
        self.state.store.clone()
    }
}

impl Extension for MemoryExt {
    fn id(&self) -> &str {
        "memory"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        let store = self.state.store.clone();
        r.service(store.clone());
        r.service(Arc::new(Memory(Arc::new(Seam(store.clone())))));
        r.tool(Arc::new(SaveTool(store.clone())));
        r.tool(Arc::new(SearchTool(store.clone())));
        r.tool(Arc::new(ForgetTool(store.clone())));
        r.tool(Arc::new(SemanticSearchTool(store.clone())));
        r.scheme(Arc::new(MemoryScheme(store.clone())));
        r.system_part("memory", Arc::new(MemoryPart(store)));
        r.command(Command {
            name: "memory".to_string(),
            description: "What is remembered: list it, forget one, switch automatic extraction".to_string(),
            argument_hint: Some("[list | forget <id> | auto on|off]".to_string()),
            handler: Arc::new(MemoryCommand(self.state.clone())),
        });
        let handler = Arc::new(Handler(self.state.clone()));
        for kind in [
            EventKind::SessionStart,
            EventKind::SessionEnd,
            EventKind::UserPrompt,
            EventKind::PostCompact,
            EventKind::ToolCall,
            EventKind::ToolResult,
        ] {
            r.on(kind, handler.clone());
        }
        Ok(())
    }
}
