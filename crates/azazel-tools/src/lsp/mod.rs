//! Language servers: started on demand for the files agents touch, asked for diagnostics after
//! every write, and queried through the `lsp` tool.

mod client;
mod servers;
pub mod text;

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::agent::ABORTED_TEXT;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::config::LspServerConfig;
use azazel_core::{
    Command, CommandHandler, CommandOutcome, Event, EventHandler, EventKind, ExtCtx, ExtError, Extension,
    FileTracker, Outcome, Registrar,
};
use lsp_types::{
    Diagnostic, DiagnosticSeverity, DocumentChanges, DocumentSymbol, DocumentSymbolResponse,
    GotoDefinitionResponse, Hover, HoverContents, Location, MarkedString, OneOf, Position, TextEdit, Uri,
    WorkspaceEdit, WorkspaceSymbolResponse,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use self::client::Client;
use self::servers::{Spec, extension_of, find_program, language_id, specs};
use self::text::{Lines, apply_edits, path_key, path_to_uri, uri_to_path};
use crate::util::{parse_input, resolve_path, write_atomic};

/// How long a write waits for the diagnostics of the new content.
const DIAGNOSTICS_WAIT: Duration = Duration::from_millis(1500);
/// Errors appended to the result of a write.
const MAX_WRITE_DIAGNOSTICS: usize = 10;
/// Lines one `lsp` result lists before the rest is counted.
const MAX_LISTED: usize = 100;
/// Times a server is started before it is given up on.
const MAX_STARTS: u32 = 3;
/// Larger files are not sent to a server.
const MAX_SYNC_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SOURCE_CHARS: usize = 160;

const ACTIONS: [&str; 6] = [
    "diagnostics",
    "definition",
    "references",
    "hover",
    "symbols",
    "rename",
];

/// What a server was last told about one file.
struct Opened {
    version: i32,
    hash: u64,
    /// How many times diagnostics had been published for the file when this content was sent.
    seen: u64,
}

#[derive(Default)]
struct State {
    client: Option<Arc<Client>>,
    starts: u32,
    error: Option<String>,
    open: HashMap<String, Opened>,
}

struct Server {
    spec: Spec,
    program: OnceLock<Option<PathBuf>>,
    /// An agent has touched a file of this server, so it is running or about to be.
    wanted: AtomicBool,
    state: tokio::sync::Mutex<State>,
}

impl Server {
    fn program(&self) -> Option<&Path> {
        self.program
            .get_or_init(|| find_program(&self.spec.command))
            .as_deref()
    }
}

/// A file as its server now knows it.
struct Synced {
    client: Arc<Client>,
    seen: u64,
    text: String,
}

/// `path` below `root` with forward slashes, or `None` when it is somewhere else.
fn below(root: &Path, path: &Path) -> Option<String> {
    let same = |a: Component, b: Component| {
        if cfg!(windows) {
            a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
        } else {
            a == b
        }
    };
    let mut rest = path.components();
    for part in root.components() {
        if !same(part, rest.next()?) {
            return None;
        }
    }
    let parts: Vec<_> = rest.map(|part| part.as_os_str().to_string_lossy()).collect();
    Some(parts.join("/"))
}

fn shown(root: &Path, path: &Path) -> String {
    below(root, path).unwrap_or_else(|| path.display().to_string())
}

fn severity_word(severity: Option<DiagnosticSeverity>) -> &'static str {
    match severity {
        Some(DiagnosticSeverity::WARNING) => "warning",
        Some(DiagnosticSeverity::INFORMATION) => "info",
        Some(DiagnosticSeverity::HINT) => "hint",
        // A server that gives no severity leaves it to the client; assume the worst.
        _ => "error",
    }
}

/// `<path>:<line>:<col> <severity> <message>` for each diagnostic `keep` accepts, in the order
/// they appear in the file.
fn diagnostic_lines(
    shown: &str,
    text: &str,
    items: &[Value],
    keep: impl Fn(&Diagnostic) -> bool,
) -> Vec<String> {
    let lines = Lines::new(text);
    let mut found: Vec<Diagnostic> = items
        .iter()
        .filter_map(|item| serde_json::from_value(item.clone()).ok())
        .filter(|diagnostic| keep(diagnostic))
        .collect();
    found.sort_by_key(|diagnostic| (diagnostic.range.start.line, diagnostic.range.start.character));
    found
        .iter()
        .map(|diagnostic| {
            let (line, column) = lines.human(diagnostic.range.start);
            let message = diagnostic
                .message
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                "{shown}:{line}:{column} {} {message}",
                severity_word(diagnostic.severity)
            )
        })
        .collect()
}

/// At most [`MAX_LISTED`] lines, then how many were left out.
fn listed(mut lines: Vec<String>) -> String {
    let more = lines.len().saturating_sub(MAX_LISTED);
    lines.truncate(MAX_LISTED);
    if more > 0 {
        lines.push(format!("… {more} more"));
    }
    lines.join("\n")
}

fn parse<T: DeserializeOwned>(value: Value) -> Result<Option<T>, String> {
    serde_json::from_value(value).map_err(|e| format!("the language server's answer could not be read: {e}"))
}

/// Turns protocol locations into `path:line:col  source line`, reading each file once.
struct Sources<'a> {
    root: &'a Path,
    texts: HashMap<PathBuf, String>,
}

impl Sources<'_> {
    fn line(&mut self, uri: &Uri, position: Position) -> String {
        let Some(path) = uri_to_path(uri) else {
            return format!(
                "{}:{}:{}",
                uri.as_str(),
                position.line + 1,
                position.character + 1
            );
        };
        let shown = shown(self.root, &path);
        let text = self
            .texts
            .entry(path)
            .or_insert_with_key(|path| std::fs::read_to_string(path).unwrap_or_default());
        let lines = Lines::new(text);
        let (line, column) = lines.human(position);
        match lines.get(position.line as usize).map(str::trim) {
            Some(source) if !source.is_empty() => {
                let source: String = source.chars().take(MAX_SOURCE_CHARS).collect();
                format!("{shown}:{line}:{column}  {source}")
            }
            _ => format!("{shown}:{line}:{column}"),
        }
    }
}

fn hover_text(hover: Hover) -> String {
    let marked = |marked: MarkedString| match marked {
        MarkedString::String(text) => text,
        MarkedString::LanguageString(code) => format!("```{}\n{}\n```", code.language, code.value),
    };
    match hover.contents {
        HoverContents::Scalar(one) => marked(one),
        HoverContents::Array(many) => many.into_iter().map(marked).collect::<Vec<_>>().join("\n\n"),
        HoverContents::Markup(markup) => markup.value,
    }
}

fn kind_word(kind: lsp_types::SymbolKind) -> String {
    format!("{kind:?}").to_lowercase()
}

fn outline(symbols: &[DocumentSymbol], lines: &Lines, depth: usize, out: &mut Vec<String>) {
    for symbol in symbols {
        let (line, _) = lines.human(symbol.selection_range.start);
        out.push(format!(
            "{}{line}: {} {}",
            "  ".repeat(depth),
            kind_word(symbol.kind),
            symbol.name
        ));
        if let Some(children) = &symbol.children {
            outline(children, lines, depth + 1, out);
        }
    }
}

/// The language servers of one project.
pub struct LspManager {
    root: PathBuf,
    servers: Vec<Arc<Server>>,
}

impl LspManager {
    pub fn new(root: PathBuf, configured: &BTreeMap<String, LspServerConfig>) -> Self {
        let servers = specs(configured)
            .into_iter()
            .map(|spec| {
                Arc::new(Server {
                    spec,
                    program: OnceLock::new(),
                    wanted: AtomicBool::new(false),
                    state: tokio::sync::Mutex::default(),
                })
            })
            .collect();
        LspManager { root, servers }
    }

    /// The server for files like `path`. A default counts only when it is installed.
    fn server_for(&self, path: &Path) -> Option<Arc<Server>> {
        let extension = extension_of(path)?;
        let server = self
            .servers
            .iter()
            .find(|server| server.spec.serves(&extension))?;
        (!server.spec.builtin || server.program().is_some()).then(|| server.clone())
    }

    /// The running client of `server`, started first when it is not running.
    async fn running(&self, server: &Server, state: &mut State) -> Result<Arc<Client>, String> {
        let name = &server.spec.name;
        if let Some(client) = state.client.take() {
            if !client.dead() {
                state.client = Some(client.clone());
                return Ok(client);
            }
            state.open.clear();
            state.error = Some(client.gone());
        }
        if state.starts >= MAX_STARTS {
            let why = state.error.as_deref().unwrap_or("it could not be started");
            return Err(format!("language server `{name}` is not running: {why}"));
        }
        state.starts += 1;
        let started = match server.program() {
            Some(program) => Client::start(program, &server.spec.args, &self.root).await,
            None => Err(format!("`{}` was not found", server.spec.command)),
        };
        match started {
            Ok(client) => {
                state.client = Some(client.clone());
                state.error = None;
                Ok(client)
            }
            Err(why) => {
                state.error = Some(why.clone());
                Err(format!("language server `{name}`: {why}"))
            }
        }
    }

    /// Brings the server's copy of `path` up to date with the file on disk.
    async fn sync(&self, server: &Server, path: &Path) -> Result<Synced, String> {
        let mut state = server.state.lock().await;
        let client = self.running(server, &mut state).await?;
        let unreadable = |e: std::io::Error| format!("{}: {e}", path.display());
        let size = tokio::fs::metadata(path).await.map_err(unreadable)?.len();
        if size > MAX_SYNC_BYTES {
            return Err(format!(
                "{}: too large for the language server ({size} bytes)",
                path.display()
            ));
        }
        let text = tokio::fs::read_to_string(path).await.map_err(unreadable)?;
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let hash = hasher.finish();
        let uri = path_to_uri(path)?;
        let seen = match state.open.get_mut(&path_key(path)) {
            Some(opened) if opened.hash == hash => opened.seen,
            Some(opened) => {
                opened.version += 1;
                opened.hash = hash;
                opened.seen = client.published(path);
                let params = json!({
                    "textDocument": { "uri": uri.as_str(), "version": opened.version },
                    "contentChanges": [{ "text": text }]
                });
                client.notify("textDocument/didChange", params).await?;
                // The content is what is on disk; servers that check on save do so now.
                let saved = json!({ "textDocument": { "uri": uri.as_str() } });
                client.notify("textDocument/didSave", saved).await?;
                opened.seen
            }
            None => {
                let seen = client.published(path);
                let language = extension_of(path).unwrap_or_default();
                let params = json!({
                    "textDocument": {
                        "uri": uri.as_str(),
                        "languageId": language_id(&language),
                        "version": 1,
                        "text": text
                    }
                });
                client.notify("textDocument/didOpen", params).await?;
                let opened = Opened {
                    version: 1,
                    hash,
                    seen,
                };
                state.open.insert(path_key(path), opened);
                seen
            }
        };
        Ok(Synced { client, seen, text })
    }

    /// Tells the server for `path`, if there is one, that an agent is working with the file,
    /// starting the server first when needed. Returns at once.
    pub fn touch(self: &Arc<Self>, path: &Path) {
        let Some(server) = self.server_for(path) else {
            return;
        };
        server.wanted.store(true, Ordering::SeqCst);
        let (manager, path) = (self.clone(), path.to_path_buf());
        tokio::spawn(async move {
            if let Err(why) = manager.sync(&server, &path).await {
                tracing::debug!("{why}");
            }
        });
    }

    /// Sends the new content of `path` to its server and returns the errors the server then
    /// reports, as a `diagnostics:` block. A server that no agent had used yet is started and not
    /// waited for.
    pub async fn after_write(self: &Arc<Self>, path: &Path) -> Option<String> {
        let server = self.server_for(path)?;
        let known = server.wanted.swap(true, Ordering::SeqCst);
        let (manager, owned) = (self.clone(), path.to_path_buf());
        // Its own task, so that giving up on the wait does not stop a server that is starting.
        let job = tokio::spawn(async move { manager.sync(&server, &owned).await });
        if !known {
            return None;
        }
        let deadline = tokio::time::Instant::now() + DIAGNOSTICS_WAIT;
        let synced = tokio::time::timeout_at(deadline, job).await.ok()?.ok()?.ok()?;
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let items = synced.client.diagnostics_after(path, synced.seen, left).await?;
        let mut errors = diagnostic_lines(&shown(&self.root, path), &synced.text, &items, |diagnostic| {
            severity_word(diagnostic.severity) == "error"
        });
        if errors.is_empty() {
            return None;
        }
        let more = errors.len().saturating_sub(MAX_WRITE_DIAGNOSTICS);
        errors.truncate(MAX_WRITE_DIAGNOSTICS);
        if more > 0 {
            errors.push(format!("… {more} more errors"));
        }
        Some(format!("diagnostics:\n{}", errors.join("\n")))
    }

    /// One line per server: its name, state, command and file extensions.
    pub fn report(&self) -> String {
        if self.servers.is_empty() {
            return "no language servers are configured".to_string();
        }
        let mut out = String::new();
        for server in &self.servers {
            let state = match server.state.try_lock() {
                Err(_) => "starting".to_string(),
                Ok(state) => match (&state.client, &state.error) {
                    (Some(client), _) if !client.dead() => {
                        format!("running, {} files open", state.open.len())
                    }
                    (Some(_), _) => "exited".to_string(),
                    (None, Some(why)) => format!("failed: {why}"),
                    (None, None) if server.program().is_none() => "not installed".to_string(),
                    (None, None) => "not started".to_string(),
                },
            };
            let extensions: Vec<String> = server.spec.extensions.iter().map(|e| format!(".{e}")).collect();
            out.push_str(&format!(
                "{}  {state}  `{}`  {}\n",
                server.spec.name,
                server.spec.command_line(),
                extensions.join(" ")
            ));
        }
        out.trim_end().to_string()
    }

    /// Each server's name, whether its command was found, and whether it is a built-in default.
    pub fn availability(&self) -> Vec<(&str, bool, bool)> {
        self.servers
            .iter()
            .map(|server| {
                (
                    server.spec.name.as_str(),
                    server.program().is_some(),
                    server.spec.builtin,
                )
            })
            .collect()
    }

    /// Stops every running server. A file touched afterwards starts its server again.
    pub async fn shutdown(&self) {
        for server in &self.servers {
            let mut state = server.state.lock().await;
            state.open.clear();
            server.wanted.store(false, Ordering::SeqCst);
            if let Some(client) = state.client.take() {
                client.shutdown().await;
            }
        }
    }
}

#[derive(Deserialize)]
struct Input {
    action: String,
    path: String,
    line: Option<u32>,
    column: Option<u32>,
    query: Option<String>,
    new_name: Option<String>,
}

/// The `lsp` tool.
pub struct LspTool {
    manager: Arc<LspManager>,
    tracker: Arc<FileTracker>,
}

impl LspTool {
    pub fn new(manager: Arc<LspManager>, tracker: Arc<FileTracker>) -> Self {
        LspTool { manager, tracker }
    }

    async fn run(&self, input: Input, ctx: &ToolCtx) -> Result<String, String> {
        let root = &self.manager.root;
        if !ACTIONS.contains(&input.action.as_str()) {
            return Err(format!(
                "unknown action `{}`; actions: {}",
                input.action,
                ACTIONS.join(", ")
            ));
        }
        let path = resolve_path(&ctx.cwd, &input.path);
        let Some(server) = self.manager.server_for(&path) else {
            let kind = match extension_of(&path) {
                Some(extension) => format!("`.{extension}` files"),
                None => "files without an extension".to_string(),
            };
            return Err(format!(
                "no language server is available for {kind}; /lsp lists the servers"
            ));
        };
        server.wanted.store(true, Ordering::SeqCst);
        let synced = self.manager.sync(&server, &path).await?;
        let client = &synced.client;
        let lines = Lines::new(&synced.text);
        let uri = path_to_uri(&path)?;
        let document = json!({ "uri": uri.as_str() });
        let position = || -> Result<Position, String> {
            let line = input
                .line
                .ok_or_else(|| format!("`{}` needs `line` (and `column`, 1 when left out)", input.action))?;
            lines
                .position(line, input.column.unwrap_or(1))
                .ok_or_else(|| format!("{} has no line {line}; it has {}", input.path, lines.count()))
        };
        let mut sources = Sources {
            root,
            texts: HashMap::new(),
        };

        match input.action.as_str() {
            "diagnostics" => {
                let items = match client
                    .diagnostics_after(&path, synced.seen, DIAGNOSTICS_WAIT)
                    .await
                {
                    Some(items) => items,
                    None => client.diagnostics(&path),
                };
                let found = diagnostic_lines(&shown(root, &path), &synced.text, &items, |_| true);
                Ok(if found.is_empty() {
                    "no diagnostics".to_string()
                } else {
                    listed(found)
                })
            }
            "definition" => {
                let params = json!({ "textDocument": document, "position": position()? });
                let answer = client.request("textDocument/definition", params).await?;
                let found: Vec<String> = match parse::<GotoDefinitionResponse>(answer)? {
                    None => Vec::new(),
                    Some(GotoDefinitionResponse::Scalar(one)) => {
                        vec![sources.line(&one.uri, one.range.start)]
                    }
                    Some(GotoDefinitionResponse::Array(many)) => many
                        .iter()
                        .map(|location| sources.line(&location.uri, location.range.start))
                        .collect(),
                    Some(GotoDefinitionResponse::Link(links)) => links
                        .iter()
                        .map(|link| sources.line(&link.target_uri, link.target_selection_range.start))
                        .collect(),
                };
                Ok(if found.is_empty() {
                    "no definition found".to_string()
                } else {
                    listed(found)
                })
            }
            "references" => {
                let params = json!({
                    "textDocument": document,
                    "position": position()?,
                    "context": { "includeDeclaration": true }
                });
                let answer = client.request("textDocument/references", params).await?;
                let found: Vec<String> = parse::<Vec<Location>>(answer)?
                    .unwrap_or_default()
                    .iter()
                    .map(|location| sources.line(&location.uri, location.range.start))
                    .collect();
                Ok(if found.is_empty() {
                    "no references found".to_string()
                } else {
                    format!("{} references\n{}", found.len(), listed(found))
                })
            }
            "hover" => {
                let params = json!({ "textDocument": document, "position": position()? });
                let answer = client.request("textDocument/hover", params).await?;
                let text = parse::<Hover>(answer)?.map(hover_text).unwrap_or_default();
                Ok(if text.trim().is_empty() {
                    "nothing to show here".to_string()
                } else {
                    text.trim().to_string()
                })
            }
            "symbols" => {
                let query = input
                    .query
                    .as_deref()
                    .map(str::trim)
                    .filter(|query| !query.is_empty());
                let found = match query {
                    Some(query) => {
                        let answer = client
                            .request("workspace/symbol", json!({ "query": query }))
                            .await?;
                        match parse::<WorkspaceSymbolResponse>(answer)? {
                            None => Vec::new(),
                            Some(WorkspaceSymbolResponse::Flat(symbols)) => symbols
                                .iter()
                                .map(|symbol| {
                                    let at = sources.line(&symbol.location.uri, symbol.location.range.start);
                                    format!("{} {}  {at}", kind_word(symbol.kind), symbol.name)
                                })
                                .collect(),
                            Some(WorkspaceSymbolResponse::Nested(symbols)) => symbols
                                .iter()
                                .map(|symbol| {
                                    let at = match &symbol.location {
                                        OneOf::Left(location) => {
                                            sources.line(&location.uri, location.range.start)
                                        }
                                        OneOf::Right(file) => uri_to_path(&file.uri)
                                            .map(|path| shown(root, &path))
                                            .unwrap_or_else(|| file.uri.as_str().to_string()),
                                    };
                                    format!("{} {}  {at}", kind_word(symbol.kind), symbol.name)
                                })
                                .collect(),
                        }
                    }
                    None => {
                        let params = json!({ "textDocument": document });
                        let answer = client.request("textDocument/documentSymbol", params).await?;
                        let mut out = Vec::new();
                        match parse::<DocumentSymbolResponse>(answer)? {
                            None => {}
                            Some(DocumentSymbolResponse::Nested(symbols)) => {
                                outline(&symbols, &lines, 0, &mut out)
                            }
                            Some(DocumentSymbolResponse::Flat(symbols)) => {
                                for symbol in symbols {
                                    let (line, _) = lines.human(symbol.location.range.start);
                                    out.push(format!("{line}: {} {}", kind_word(symbol.kind), symbol.name));
                                }
                            }
                        }
                        out
                    }
                };
                Ok(if found.is_empty() {
                    "no symbols found".to_string()
                } else {
                    listed(found)
                })
            }
            _ => {
                let new_name = input.new_name.as_deref().map(str::trim).unwrap_or_default();
                if new_name.is_empty() {
                    return Err("`rename` needs `new_name`".to_string());
                }
                let params =
                    json!({ "textDocument": document, "position": position()?, "newName": new_name });
                let answer = client.request("textDocument/rename", params).await?;
                let Some(edit) = parse::<WorkspaceEdit>(answer)? else {
                    return Err("the language server cannot rename what is at this position".to_string());
                };
                self.apply(edit, new_name, ctx).await
            }
        }
    }

    /// Writes a workspace edit to disk: every file or none, unless writing itself fails midway.
    async fn apply(&self, edit: WorkspaceEdit, new_name: &str, ctx: &ToolCtx) -> Result<String, String> {
        let root = &self.manager.root;
        let mut files: Vec<(PathBuf, Vec<TextEdit>)> = Vec::new();
        let mut add = |uri: &Uri, edits: Vec<TextEdit>| -> Result<(), String> {
            let path = uri_to_path(uri).ok_or_else(|| format!("{} is not a file", uri.as_str()))?;
            if below(root, &path).is_none() {
                return Err(format!(
                    "the rename would change {}, which is outside the project; nothing was changed",
                    path.display()
                ));
            }
            match files
                .iter_mut()
                .find(|(known, _)| path_key(known) == path_key(&path))
            {
                Some((_, all)) => all.extend(edits),
                None => files.push((path, edits)),
            }
            Ok(())
        };
        for (uri, edits) in edit.changes.unwrap_or_default() {
            add(&uri, edits)?;
        }
        match edit.document_changes {
            Some(DocumentChanges::Edits(changes)) => {
                for change in changes {
                    let edits = change
                        .edits
                        .into_iter()
                        .map(|edit| match edit {
                            OneOf::Left(plain) => plain,
                            OneOf::Right(annotated) => annotated.text_edit,
                        })
                        .collect();
                    add(&change.text_document.uri, edits)?;
                }
            }
            Some(DocumentChanges::Operations(_)) => {
                return Err(
                    "the rename would create, move or delete files, which is not supported; nothing was changed"
                        .to_string(),
                );
            }
            None => {}
        }
        files.retain(|(_, edits)| !edits.is_empty());
        if files.is_empty() {
            return Err("the language server returned no changes".to_string());
        }
        // One order for every caller, so two renames cannot wait on each other's files.
        files.sort_by_key(|(path, _)| path_key(path));

        let mut guards = Vec::with_capacity(files.len());
        let mut updated = Vec::with_capacity(files.len());
        for (path, edits) in &files {
            guards.push(self.tracker.lock(path).await);
            let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
            updated.push(apply_edits(&text, edits)?);
        }
        let mut report = String::new();
        let mut total = 0;
        for ((path, edits), text) in files.iter().zip(&updated) {
            write_atomic(path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
            self.tracker.record_write(&ctx.agent_id, path, text.as_bytes());
            total += edits.len();
            report.push_str(&format!("\n{} ({})", shown(root, path), edits.len()));
        }
        drop(guards);
        for (path, _) in &files {
            self.manager.touch(path);
        }
        Ok(format!(
            "renamed to `{new_name}`: {total} edits in {} files{report}",
            files.len()
        ))
    }
}

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        "Ask the language server of a file what a compiler knows. diagnostics: its errors and warnings. \
         definition, references, hover: about the symbol at `line` and `column` (both 1-based). symbols: \
         the file's outline, or with `query` a search over the project's symbols. rename: rename the \
         symbol at the position to `new_name` in every file that uses it. Results are \
         `path:line:column` lines. Prefer this to text search when you need what a name refers to."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ACTIONS },
                "path": { "type": "string", "description": "The file to ask about" },
                "line": { "type": "integer", "minimum": 1 },
                "column": { "type": "integer", "minimum": 1, "description": "Character column; 1 when left out" },
                "query": { "type": "string", "description": "symbols: search the project for this name" },
                "new_name": { "type": "string", "description": "rename: the new name" }
            },
            "required": ["action", "path"]
        })
    }

    fn access(&self, input: &Value) -> Access {
        match input.get("action").and_then(Value::as_str) {
            Some("rename") => Access::Write,
            _ => Access::Read,
        }
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        input
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .into_iter()
            .collect()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        tokio::select! {
            done = self.run(input, ctx) => match done {
                Ok(text) => ToolOutput::text(text),
                Err(why) => ToolOutput::error(why),
            },
            _ = ctx.cancel.cancelled() => ToolOutput::error(ABORTED_TEXT),
        }
    }
}

struct LspCommand(Arc<LspManager>);

#[async_trait]
impl CommandHandler for LspCommand {
    async fn run(&self, _: &str, _: &ExtCtx) -> CommandOutcome {
        CommandOutcome::Show(self.0.report())
    }
}

struct StopServers(Arc<LspManager>);

#[async_trait]
impl EventHandler for StopServers {
    async fn handle(&self, _: &mut Event, _: &ExtCtx) -> Outcome {
        self.0.shutdown().await;
        Outcome::Continue
    }
}

/// Registers the [`LspManager`] service, the `lsp` tool and `/lsp`. Load it after the core tools.
pub struct LspExt {
    pub root: PathBuf,
    pub servers: BTreeMap<String, LspServerConfig>,
}

impl Extension for LspExt {
    fn id(&self) -> &str {
        "lsp"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        let tracker = r
            .services()
            .get::<FileTracker>()
            .ok_or_else(|| ExtError("the file tracker service is missing".to_string()))?;
        let manager = Arc::new(LspManager::new(self.root.clone(), &self.servers));
        r.service(manager.clone());
        r.tool(Arc::new(LspTool::new(manager.clone(), tracker)));
        r.command(Command {
            name: "lsp".to_string(),
            description: "Language servers and their state".to_string(),
            argument_hint: None,
            handler: Arc::new(LspCommand(manager.clone())),
        });
        r.on(EventKind::SessionEnd, Arc::new(StopServers(manager)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostic(line: u32, character: u32, severity: Option<u8>, message: &str) -> Value {
        let at = json!({ "line": line, "character": character });
        let mut value = json!({ "range": { "start": at, "end": at }, "message": message });
        if let Some(severity) = severity {
            value["severity"] = severity.into();
        }
        value
    }

    #[test]
    fn diagnostics_are_ordered_and_positioned_in_characters() {
        let text = "let a = 1;\n𝄞é = oops;\n";
        let items = [
            diagnostic(1, 3, Some(1), "cannot find value\n  `oops`"),
            diagnostic(0, 4, Some(2), "unused variable"),
            diagnostic(0, 0, None, "no severity given"),
            json!({ "not": "a diagnostic" }),
        ];
        assert_eq!(
            diagnostic_lines("src/a.rs", text, &items, |_| true),
            [
                "src/a.rs:1:1 error no severity given",
                "src/a.rs:1:5 warning unused variable",
                // `𝄞` is two UTF-16 units and one character.
                "src/a.rs:2:3 error cannot find value `oops`",
            ]
        );
        let errors = diagnostic_lines("a", text, &items, |d| severity_word(d.severity) == "error");
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn paths_below_the_root_are_shown_relative() {
        let root = Path::new(if cfg!(windows) {
            r"C:\work\proj"
        } else {
            "/work/proj"
        });
        assert_eq!(
            below(root, &root.join("src").join("a.rs")).as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(below(root, root.parent().unwrap()), None);
        let sibling = root.parent().unwrap().join("project2").join("a.rs");
        assert_eq!(below(root, &sibling), None);
        assert_eq!(shown(root, &sibling), sibling.display().to_string());
        if cfg!(windows) {
            // Servers write drive letters either way.
            assert_eq!(
                below(root, Path::new(r"c:\Work\proj\a.rs")).as_deref(),
                Some("a.rs")
            );
        }
    }

    #[test]
    fn long_lists_are_cut_with_a_count() {
        let lines: Vec<String> = (0..MAX_LISTED + 7).map(|i| i.to_string()).collect();
        let out = listed(lines);
        assert_eq!(out.lines().count(), MAX_LISTED + 1);
        assert!(out.ends_with("… 7 more"));
        assert_eq!(listed(vec!["one".into()]), "one");
    }
}
