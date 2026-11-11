//! The tools agents use on memory and on the search index.

use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use serde_json::{Value, json};

use crate::index::{CODE, ChunkHit, search_collection};
use crate::store::{KINDS, MemoryRow, MemoryStore, Scope};

const DEFAULT_MEMORY_HITS: usize = 8;
const DEFAULT_CHUNK_HITS: usize = 10;
const MAX_HITS: usize = 50;
/// Lines of a chunk shown in a search result.
const PREVIEW_LINES: usize = 12;

/// A required, non-empty string argument.
fn text_arg<'a>(input: &'a Value, name: &str) -> Result<&'a str, ToolOutput> {
    input
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| ToolOutput::error(format!("`{name}` must be a non-empty string")))
}

fn scope_arg(input: &Value) -> Result<Option<Scope>, ToolOutput> {
    match input.get("scope").and_then(Value::as_str) {
        None => Ok(None),
        Some(name) => Scope::parse(name)
            .map(Some)
            .ok_or_else(|| ToolOutput::error(format!("unknown scope `{name}`: use project or global"))),
    }
}

fn limit_arg(input: &Value, default: usize) -> usize {
    input
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(default, |n| (n as usize).clamp(1, MAX_HITS))
}

/// One memory on one line: id, kind, scope, text, tags.
pub(crate) fn describe(row: &MemoryRow) -> String {
    let mut line = format!("{} [{}, {}] {}", row.id, row.kind, row.scope.as_str(), row.text);
    for tag in &row.tags {
        line.push_str(&format!(" #{tag}"));
    }
    line
}

fn render_chunks(hits: &[ChunkHit]) -> String {
    let rendered: Vec<String> = hits
        .iter()
        .map(|hit| {
            let preview: Vec<&str> = hit.text.lines().take(PREVIEW_LINES).collect();
            format!("{}:{}-{}\n{}", hit.path, hit.start, hit.end, preview.join("\n"))
        })
        .collect();
    rendered.join("\n\n")
}

pub(crate) struct SaveTool(pub(crate) Arc<MemoryStore>);

#[async_trait]
impl Tool for SaveTool {
    fn name(&self) -> &str {
        "memory_save"
    }

    fn description(&self) -> &str {
        "Remember something for later sessions: a fact about the project, a decision and its reason, a \
         preference of the user, a convention of the codebase, or a gotcha. Write one self-contained \
         sentence per memory. Preferences and conventions are shown to every later session; the other \
         kinds come back when a prompt matches them. Never store secrets or credentials."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "What to remember, understandable on its own" },
                "kind": { "type": "string", "enum": KINDS },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Keywords to find it by"
                },
                "scope": {
                    "type": "string",
                    "enum": ["project", "global"],
                    "description": "project (default) or global, which applies to every project of this user"
                }
            },
            "required": ["text", "kind"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Write
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let (text, kind) = match (text_arg(&input, "text"), text_arg(&input, "kind")) {
            (Ok(text), Ok(kind)) => (text, kind),
            (Err(why), _) | (_, Err(why)) => return why,
        };
        let scope = match scope_arg(&input) {
            Ok(scope) => scope.unwrap_or(Scope::Project),
            Err(why) => return why,
        };
        let tags: Vec<String> = input
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        match self.0.save(text, kind, &tags, scope, Some(&ctx.session_id)).await {
            Ok(id) => ToolOutput::text(format!("saved as {id} ({})", scope.as_str())),
            Err(why) => ToolOutput::error(why),
        }
    }
}

pub(crate) struct SearchTool(pub(crate) Arc<MemoryStore>);

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Look up what was remembered in earlier sessions. Returns the best matches, one per line, as \
         `id [kind, scope] text #tags`. Searches this project's memories and the global ones unless `scope` \
         narrows it."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Words or a question about what to recall" },
                "limit": { "type": "integer", "description": "Most results to return (default 8)" },
                "scope": { "type": "string", "enum": ["project", "global"], "description": "Omit for both" }
            },
            "required": ["query"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let query = match text_arg(&input, "query") {
            Ok(query) => query,
            Err(why) => return why,
        };
        let scope = match scope_arg(&input) {
            Ok(scope) => scope,
            Err(why) => return why,
        };
        let limit = limit_arg(&input, DEFAULT_MEMORY_HITS);
        match self.0.search(query, limit, scope).await {
            Ok(rows) if rows.is_empty() => ToolOutput::text(format!("no memory matches `{query}`")),
            Ok(rows) => ToolOutput::text(rows.iter().map(describe).collect::<Vec<_>>().join("\n")),
            Err(why) => ToolOutput::error(why),
        }
    }
}

pub(crate) struct ForgetTool(pub(crate) Arc<MemoryStore>);

#[async_trait]
impl Tool for ForgetTool {
    fn name(&self) -> &str {
        "memory_forget"
    }

    fn description(&self) -> &str {
        "Delete a memory that is wrong or no longer wanted. Takes the id shown by memory_search."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "Id of the memory to delete" } },
            "required": ["id"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Write
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let id = match text_arg(&input, "id") {
            Ok(id) => id,
            Err(why) => return why,
        };
        match self.0.forget(id).await {
            Ok(true) => ToolOutput::text(format!("forgot {id}")),
            Ok(false) => ToolOutput::error(format!(
                "no memory has the id `{id}`; memory_search shows the ids"
            )),
            Err(why) => ToolOutput::error(why),
        }
    }
}

pub(crate) struct SemanticSearchTool(pub(crate) Arc<MemoryStore>);

#[async_trait]
impl Tool for SemanticSearchTool {
    fn name(&self) -> &str {
        "semantic_search"
    }

    fn description(&self) -> &str {
        "Search the indexed project files by what they are about. Use it when you do not know the exact \
         words to grep for. Each result is `path:first-last` (line numbers) followed by the first lines \
         of the matching chunk; read the file for more. `collection` is `code` (the project, default) or \
         the name of a document collection."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What the code or text you are looking for does or says"
                },
                "collection": {
                    "type": "string",
                    "description": "`code` (default) or the name of a document collection"
                },
                "path_glob": {
                    "type": "string",
                    "description": "Only files whose path matches, e.g. `src/**/*.rs`"
                },
                "limit": { "type": "integer", "description": "Most results to return (default 10)" }
            },
            "required": ["query"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let query = match text_arg(&input, "query") {
            Ok(query) => query,
            Err(why) => return why,
        };
        let collection = input
            .get("collection")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map_or_else(|| CODE.to_string(), search_collection);
        let glob = input
            .get("path_glob")
            .and_then(Value::as_str)
            .filter(|glob| !glob.trim().is_empty());
        let limit = limit_arg(&input, DEFAULT_CHUNK_HITS);
        match self.0.semantic_search(query, &collection, glob, limit).await {
            Ok(hits) if hits.is_empty() => {
                ToolOutput::text(format!("nothing in `{collection}` matches `{query}`"))
            }
            Ok(hits) => ToolOutput::text(render_chunks(&hits)),
            Err(why) => ToolOutput::error(why),
        }
    }
}
