//! Language servers: started on demand for the files agents touch, asked for diagnostics after
//! every write, and queried through the `lsp` tool.

mod client;
mod servers;
pub mod text;

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::agent::ABORTED_TEXT;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::config::{LspServerConfig, WriteSeverity};
use azazel_core::tracker::Splice;
use azazel_core::{
    Command, CommandHandler, CommandOutcome, Event, EventHandler, EventKind, ExtCtx, ExtError, Extension,
    FileTracker, Outcome, Registrar,
};
use lsp_types::{
    Diagnostic, DiagnosticSeverity, DocumentChanges, DocumentSymbol, DocumentSymbolResponse,
    GotoDefinitionResponse, Hover, HoverContents, Location, MarkedString, OneOf, Position, Range, TextEdit,
    Uri, WorkspaceEdit, WorkspaceSymbolResponse,
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
/// Lines of a symbol `read_symbol` returns before it says where the rest is.
const MAX_SYMBOL_LINES: u32 = 2000;
/// Bytes of a symbol `read_symbol` returns before it says where the rest is.
const MAX_SYMBOL_BYTES: usize = 256 * 1024;

const ACTIONS: [&str; 9] = [
    "diagnostics",
    "definition",
    "references",
    "hover",
    "symbols",
    "outline",
    "read_symbol",
    "rename",
    "rename_file",
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
    /// The server was stopped because none of its files had been touched for a while.
    idle: bool,
}

struct Server {
    spec: Spec,
    program: OnceLock<Option<PathBuf>>,
    /// An agent has touched a file of this server, so it is running or about to be.
    wanted: AtomicBool,
    state: tokio::sync::Mutex<State>,
    /// When a file of this server was last touched.
    touched: Mutex<tokio::time::Instant>,
    /// How many `lsp` calls are asking this server something right now.
    asking: AtomicUsize,
}

impl Server {
    fn program(&self) -> Option<&Path> {
        self.program
            .get_or_init(|| find_program(&self.spec.command))
            .as_deref()
    }

    fn touch(&self) {
        *self.touched.lock().unwrap_or_else(PoisonError::into_inner) = tokio::time::Instant::now();
    }

    fn touched(&self) -> tokio::time::Instant {
        *self.touched.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Keeps a server from being stopped as idle while a call is asking it something.
struct Asking(Arc<Server>);

impl Asking {
    fn new(server: &Arc<Server>) -> Asking {
        server.asking.fetch_add(1, Ordering::SeqCst);
        Asking(server.clone())
    }
}

impl Drop for Asking {
    fn drop(&mut self) {
        self.0.touch();
        self.0.asking.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Stops `server` once none of its files has been touched for `idle`, unless the client it runs
/// has been replaced or has gone by then. A file touched later starts the server again.
fn stop_when_idle(server: &Arc<Server>, client: &Arc<Client>, idle: Duration) {
    let (server, client) = (Arc::downgrade(server), Arc::downgrade(client));
    tokio::spawn(async move {
        loop {
            let Some(due) = Weak::upgrade(&server).map(|server| server.touched() + idle) else {
                return;
            };
            tokio::time::sleep_until(due).await;
            let (Some(server), Some(client)) = (server.upgrade(), client.upgrade()) else {
                return;
            };
            let mut state = server.state.lock().await;
            let current = state
                .client
                .as_ref()
                .is_some_and(|running| Arc::ptr_eq(running, &client));
            if !current || client.dead() {
                return;
            }
            if server.asking.load(Ordering::SeqCst) > 0 {
                server.touch();
                continue;
            }
            if server.touched().elapsed() < idle {
                continue;
            }
            state.client = None;
            state.open.clear();
            state.starts = 0;
            state.idle = true;
            server.wanted.store(false, Ordering::SeqCst);
            client.shutdown().await;
            return;
        }
    });
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

fn symbol_lines(symbols: &[DocumentSymbol], lines: &Lines, depth: usize, out: &mut Vec<String>) {
    for symbol in symbols {
        let (line, _) = lines.human(symbol.selection_range.start);
        out.push(format!(
            "{}{line}: {} {}",
            "  ".repeat(depth),
            kind_word(symbol.kind),
            symbol.name
        ));
        if let Some(children) = &symbol.children {
            symbol_lines(children, lines, depth + 1, out);
        }
    }
}

/// One symbol of a file, as `outline` lists it and `read_symbol` finds it.
struct Outlined {
    kind: String,
    name: String,
    /// The symbols it is inside of, outermost first.
    within: Vec<String>,
    /// Its first and last line, counted from 1.
    first: u32,
    last: u32,
    /// The line its name is on, counted from 0.
    named: u32,
}

impl Outlined {
    /// `<kind> <name>  <first>-<last>`, then the line its name is on.
    fn line(&self, lines: &Lines) -> String {
        let mut line = format!("{} {}  {}-{}", self.kind, self.name, self.first, self.last);
        let signature = lines.get(self.named as usize).map(str::trim).unwrap_or_default();
        if !signature.is_empty() {
            line.push_str("  ");
            line.extend(signature.chars().take(MAX_SOURCE_CHARS));
        }
        line
    }

    /// Whether `wanted`, a name or the last parts of a qualified one, names this symbol.
    fn named(&self, wanted: &[&str], exact: bool) -> bool {
        let Some((name, within)) = wanted.split_last() else {
            return false;
        };
        // Some servers put the parameters in the name: `main()`.
        let matches = |own: &str, wanted: &str| {
            own == wanted
                || (!exact
                    && own
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .next()
                        .is_some_and(|word| word == wanted))
        };
        matches(&self.name, name)
            && within.len() <= self.within.len()
            && self.within[self.within.len() - within.len()..]
                .iter()
                .zip(within)
                .all(|(own, wanted)| matches(own, wanted))
    }
}

/// The lines `range` covers, counted from 1. A range that ends at the start of a line does not
/// reach into that line.
fn line_span(range: Range, count: usize) -> (u32, u32) {
    let first = range.start.line + 1;
    let last = if range.end.character == 0 && range.end.line > range.start.line {
        range.end.line
    } else {
        range.end.line + 1
    };
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    (first.min(count), last.clamp(first, count.max(first)))
}

fn outlined(symbols: &[DocumentSymbol], lines: &Lines, within: &[String], out: &mut Vec<Outlined>) {
    for symbol in symbols {
        let (first, last) = line_span(symbol.range, lines.count());
        out.push(Outlined {
            kind: kind_word(symbol.kind),
            name: symbol.name.clone(),
            within: within.to_vec(),
            first,
            last,
            named: symbol.selection_range.start.line,
        });
        if let Some(children) = &symbol.children {
            let mut inside = within.to_vec();
            inside.push(symbol.name.clone());
            outlined(children, lines, &inside, out);
        }
    }
}

/// The symbols of a file in the order the server gives them, each after what contains it.
fn outline_of(response: Option<DocumentSymbolResponse>, lines: &Lines) -> Vec<Outlined> {
    let mut out = Vec::new();
    match response {
        None => {}
        Some(DocumentSymbolResponse::Nested(symbols)) => outlined(&symbols, lines, &[], &mut out),
        Some(DocumentSymbolResponse::Flat(symbols)) => {
            for symbol in symbols {
                let (first, last) = line_span(symbol.location.range, lines.count());
                out.push(Outlined {
                    kind: kind_word(symbol.kind),
                    name: symbol.name,
                    within: symbol.container_name.into_iter().collect(),
                    first,
                    last,
                    named: symbol.location.range.start.line,
                });
            }
        }
    }
    out
}

fn line_ends(text: &str) -> u32 {
    u32::try_from(text.matches('\n').count()).unwrap_or(u32::MAX)
}

/// The runs of lines `edits` replace, lowest last, so that each is counted in a text the runs
/// below it have already changed.
fn splices_of(edits: &[TextEdit]) -> Vec<Splice> {
    let mut sorted: Vec<&TextEdit> = edits.iter().collect();
    sorted.sort_by_key(|edit| std::cmp::Reverse((edit.range.start.line, edit.range.start.character)));
    sorted
        .into_iter()
        .map(|edit| {
            let (start, end) = (edit.range.start, edit.range.end);
            // An edit that ends at the start of a line leaves that line alone.
            let open = u32::from(!(end.character == 0 && end.line > start.line));
            Splice {
                first: start.line + 1,
                old: end.line.saturating_sub(start.line) + open,
                new: line_ends(&edit.new_text) + open,
            }
        })
        .collect()
}

/// The language servers of one project.
pub struct LspManager {
    root: PathBuf,
    servers: Vec<Arc<Server>>,
    /// A server none of whose files was touched for this long is stopped; zero keeps it running.
    idle: Duration,
    write_severity: WriteSeverity,
}

impl LspManager {
    pub fn new(ext: &LspExt) -> Self {
        let servers = specs(&ext.servers)
            .into_iter()
            .map(|spec| {
                Arc::new(Server {
                    spec,
                    program: OnceLock::new(),
                    wanted: AtomicBool::new(false),
                    state: tokio::sync::Mutex::default(),
                    touched: Mutex::new(tokio::time::Instant::now()),
                    asking: AtomicUsize::new(0),
                })
            })
            .collect();
        LspManager {
            root: ext.root.clone(),
            servers,
            idle: ext.idle,
            write_severity: ext.write_severity,
        }
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

    /// The running client of `server`, started first when it is not running. A server that
    /// starts is given its warm files at once.
    async fn running(&self, server: &Arc<Server>, state: &mut State) -> Result<Arc<Client>, String> {
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
                state.idle = false;
                server.touch();
                if !self.idle.is_zero() {
                    stop_when_idle(server, &client, self.idle);
                }
                for warm in &server.spec.warm_files {
                    let path = resolve_path(&self.root, warm);
                    if let Err(why) = Self::send(&client, state, &path).await {
                        tracing::debug!("warm file: {why}");
                    }
                }
                Ok(client)
            }
            Err(why) => {
                state.error = Some(why.clone());
                Err(format!("language server `{name}`: {why}"))
            }
        }
    }

    /// Brings the server's copy of `path` up to date with the file on disk.
    async fn sync(&self, server: &Arc<Server>, path: &Path) -> Result<Synced, String> {
        let mut state = server.state.lock().await;
        let client = self.running(server, &mut state).await?;
        server.touch();
        let (seen, text) = Self::send(&client, &mut state, path).await?;
        Ok(Synced { client, seen, text })
    }

    /// Sends `client` the content of `path` as it is on disk, unless that is what it has.
    /// Returns how many times diagnostics had been published for the file before, and the
    /// content.
    async fn send(client: &Client, state: &mut State, path: &Path) -> Result<(u64, String), String> {
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
        Ok((seen, text))
    }

    /// Tells the server of `path` that the file is no longer there under that name.
    async fn close(&self, server: &Server, path: &Path) {
        let mut state = server.state.lock().await;
        if state.open.remove(&path_key(path)).is_none() {
            return;
        }
        if let (Some(client), Ok(uri)) = (&state.client, path_to_uri(path)) {
            let closed = json!({ "textDocument": { "uri": uri.as_str() } });
            let _ = client.notify("textDocument/didClose", closed).await;
        }
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
    /// reports (and its warnings, with `lsp.write_severity = "warning"`), as a `diagnostics:`
    /// block. A server that no agent had used yet is started and not waited for.
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
        let of = |severity: &'static str| {
            diagnostic_lines(
                &shown(&self.root, path),
                &synced.text,
                &items,
                move |diagnostic| severity_word(diagnostic.severity) == severity,
            )
        };
        // Errors first: they are what must not be cut off.
        let mut found = of("error");
        let warnings = self.write_severity == WriteSeverity::Warning;
        if warnings {
            found.extend(of("warning"));
        }
        if found.is_empty() {
            return None;
        }
        let more = found.len().saturating_sub(MAX_WRITE_DIAGNOSTICS);
        found.truncate(MAX_WRITE_DIAGNOSTICS);
        if more > 0 {
            let what = if warnings { "errors and warnings" } else { "errors" };
            found.push(format!("… {more} more {what}"));
        }
        Some(format!("diagnostics:\n{}", found.join("\n")))
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
                    (None, None) if state.idle => "stopped, idle".to_string(),
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
            // Stopped on purpose, which is not one of the failures a server is given up over.
            state.starts = 0;
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
    symbol: Option<String>,
    occurrence: Option<usize>,
    new_path: Option<String>,
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
        let _asking = Asking::new(&server);
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
                                symbol_lines(&symbols, &lines, 0, &mut out)
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
            "outline" | "read_symbol" => {
                let params = json!({ "textDocument": document });
                let answer = client.request("textDocument/documentSymbol", params).await?;
                let symbols = outline_of(parse::<DocumentSymbolResponse>(answer)?, &lines);
                if input.action == "read_symbol" {
                    return self.read_symbol(&input, &path, &synced.text, &symbols, ctx);
                }
                let found: Vec<String> = symbols
                    .iter()
                    .map(|symbol| format!("{}{}", "  ".repeat(symbol.within.len()), symbol.line(&lines)))
                    .collect();
                Ok(if found.is_empty() {
                    "no symbols found".to_string()
                } else {
                    listed(found)
                })
            }
            "rename_file" => {
                let new_path = input.new_path.as_deref().map(str::trim).unwrap_or_default();
                if new_path.is_empty() {
                    return Err("`rename_file` needs `new_path`".to_string());
                }
                let to = resolve_path(&ctx.cwd, new_path);
                self.move_file(&server, client, &path, &to, ctx).await
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
                let files = self.apply(edit, "rename", ctx).await?;
                if files.is_empty() {
                    return Err("the language server returned no changes".to_string());
                }
                Ok(format!("renamed to `{new_name}`: {}", changed(root, &files)))
            }
        }
    }

    /// The text of the symbol `input` names, under a line saying what and where it is. The agent
    /// has then read those lines.
    fn read_symbol(
        &self,
        input: &Input,
        path: &Path,
        text: &str,
        symbols: &[Outlined],
        ctx: &ToolCtx,
    ) -> Result<String, String> {
        let lines = Lines::new(text);
        let wanted = input.symbol.as_deref().map(str::trim).unwrap_or_default();
        let parts: Vec<&str> = wanted
            .split("::")
            .flat_map(|part| part.split('.'))
            .filter(|part| !part.is_empty())
            .collect();
        if parts.is_empty() {
            return Err("`read_symbol` needs `symbol`".to_string());
        }
        let named = |exact: bool| -> Vec<&Outlined> {
            symbols
                .iter()
                .filter(|symbol| symbol.named(&parts, exact))
                .collect()
        };
        let mut found = named(true);
        if found.is_empty() {
            found = named(false);
        }
        let symbol = match (found.len(), input.occurrence) {
            (0, _) => {
                return Err(format!(
                    "no symbol `{wanted}` in {}; `outline` lists its symbols",
                    input.path
                ));
            }
            (count, Some(occurrence)) if occurrence == 0 || occurrence > count => {
                return Err(format!(
                    "`occurrence` {occurrence} is not one of the {count} symbols named `{wanted}` in {}",
                    input.path
                ));
            }
            (_, Some(occurrence)) => found[occurrence - 1],
            (1, None) => found[0],
            (count, None) => {
                let listed: Vec<String> = found
                    .iter()
                    .enumerate()
                    .map(|(index, symbol)| format!("{}: {}", index + 1, symbol.line(&lines)))
                    .collect();
                return Err(format!(
                    "`{wanted}` names {count} symbols in {}; say which with `occurrence`:\n{}",
                    input.path,
                    listed.join("\n")
                ));
            }
        };
        let mut out = format!(
            "{} {}  {}:{}-{}\n",
            symbol.kind,
            symbol.name,
            shown(&self.manager.root, path),
            symbol.first,
            symbol.last
        );
        let mut last = symbol.first - 1;
        for number in symbol.first..=symbol.last.min(symbol.first.saturating_add(MAX_SYMBOL_LINES - 1)) {
            let Some(line) = lines.get(number as usize - 1) else {
                break;
            };
            if last >= symbol.first && out.len() + line.len() > MAX_SYMBOL_BYTES {
                break;
            }
            out.push_str(line);
            out.push('\n');
            last = number;
        }
        if last < symbol.last {
            out.push_str(&format!(
                "[lines {}-{last} of {}-{}; continue with read offset={}]",
                symbol.first,
                symbol.first,
                symbol.last,
                last + 1
            ));
        } else {
            out.pop();
        }
        if last >= symbol.first {
            self.tracker
                .record_lines(&ctx.agent_id, path, text.as_bytes(), symbol.first, last);
        }
        Ok(out)
    }

    /// Moves the file `from` to `to`, first making the changes the server asks for in the files
    /// that refer to it, and tells the server once it is done.
    async fn move_file(
        &self,
        server: &Arc<Server>,
        client: &Client,
        from: &Path,
        to: &Path,
        ctx: &ToolCtx,
    ) -> Result<String, String> {
        let root = &self.manager.root;
        if below(root, to).is_none() {
            return Err(format!("{} is outside the project", to.display()));
        }
        if to.exists() {
            return Err(format!("{} already exists", to.display()));
        }
        let moved = json!({
            "files": [{ "oldUri": path_to_uri(from)?.as_str(), "newUri": path_to_uri(to)?.as_str() }]
        });
        let mut files = Vec::new();
        if client.supports("/workspace/fileOperations/willRename") {
            let answer = client.request("workspace/willRenameFiles", moved.clone()).await?;
            if let Some(edit) = parse::<WorkspaceEdit>(answer)? {
                files = self.apply(edit, "move", ctx).await?;
            }
        }
        {
            let _guards = self.tracker.lock_all(&[from, to]).await;
            if to.exists() {
                return Err(format!("{} already exists", to.display()));
            }
            let failed = |e: std::io::Error| format!("{}: {e}", to.display());
            if let Some(dir) = to.parent() {
                std::fs::create_dir_all(dir).map_err(failed)?;
            }
            std::fs::rename(from, to).map_err(failed)?;
            self.tracker.moved(from, to);
        }
        self.manager.close(server, from).await;
        if client.supports("/workspace/fileOperations/didRename") {
            client.notify("workspace/didRenameFiles", moved).await?;
        }
        self.manager.touch(to);
        let mut out = format!("moved {} to {}", shown(root, from), shown(root, to));
        if !files.is_empty() {
            out.push_str(&format!("; {}", changed(root, &files)));
        }
        Ok(out)
    }

    /// Writes a workspace edit to disk: every file or none, unless writing itself fails midway.
    /// `what` names the action whose edit it is. Returns each file changed and the number of
    /// edits made in it.
    async fn apply(
        &self,
        edit: WorkspaceEdit,
        what: &str,
        ctx: &ToolCtx,
    ) -> Result<Vec<(PathBuf, usize)>, String> {
        let root = &self.manager.root;
        let mut files: Vec<(PathBuf, Vec<TextEdit>)> = Vec::new();
        let mut add = |uri: &Uri, edits: Vec<TextEdit>| -> Result<(), String> {
            let path = uri_to_path(uri).ok_or_else(|| format!("{} is not a file", uri.as_str()))?;
            if below(root, &path).is_none() {
                return Err(format!(
                    "the {what} would change {}, which is outside the project; nothing was changed",
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
                return Err(format!(
                    "the {what} would create, move or delete files, which is not supported; nothing was changed"
                ));
            }
            None => {}
        }
        files.retain(|(_, edits)| !edits.is_empty());
        files.sort_by_key(|(path, _)| path_key(path));

        let paths: Vec<&Path> = files.iter().map(|(path, _)| path.as_path()).collect();
        let guards = self.tracker.lock_all(&paths).await;
        let mut updated = Vec::with_capacity(files.len());
        for (path, edits) in &files {
            let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let new = apply_edits(&text, edits)?;
            updated.push((text, new));
        }
        for ((path, edits), (old, new)) in files.iter().zip(&updated) {
            write_atomic(path, new.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
            // The agent has seen the lines the server changed for it, and of the rest of the
            // file what it had seen before.
            let splices = splices_of(edits);
            self.tracker
                .record_edit(&ctx.agent_id, path, old.as_bytes(), new.as_bytes(), &splices);
        }
        drop(guards);
        for (path, _) in &files {
            self.manager.touch(path);
        }
        Ok(files
            .into_iter()
            .map(|(path, edits)| (path, edits.len()))
            .collect())
    }
}

/// `<n> edits in <m> files`, then each file with its number of edits.
fn changed(root: &Path, files: &[(PathBuf, usize)]) -> String {
    let total: usize = files.iter().map(|(_, edits)| edits).sum();
    let mut out = format!("{total} edits in {} files", files.len());
    for (path, edits) in files {
        out.push_str(&format!("\n{} ({edits})", shown(root, path)));
    }
    out
}

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        "Ask the language server of a file what a compiler knows. diagnostics: its errors and warnings. \
         definition, references, hover: about the symbol at `line` and `column` (both 1-based). symbols: \
         the file's symbols by line, or with `query` a search over the project's symbols. outline: every \
         symbol of the file as `<kind> <name>  <first line>-<last line>  <its declaration>`, nested. \
         read_symbol: the source of the symbol named `symbol` (`Type.method` when the name alone is not \
         enough, `occurrence` to pick among several), which then counts as read; prefer outline and \
         read_symbol to reading a whole large file. rename: rename the symbol at the position to \
         `new_name` in every file that uses it. rename_file: move the file to `new_path` and update \
         the files that refer to it. Results are `path:line:column` lines. Prefer this to text search \
         when you need what a name refers to."
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
                "new_name": { "type": "string", "description": "rename: the new name" },
                "symbol": { "type": "string", "description": "read_symbol: the symbol's name, or `Outer.name`" },
                "occurrence": { "type": "integer", "minimum": 1, "description": "read_symbol: which of several symbols of that name, counted from 1" },
                "new_path": { "type": "string", "description": "rename_file: where the file goes" }
            },
            "required": ["action", "path"]
        })
    }

    fn access(&self, input: &Value) -> Access {
        match input.get("action").and_then(Value::as_str) {
            Some("rename" | "rename_file") => Access::Write,
            _ => Access::Read,
        }
    }

    fn replay_safe(&self, input: &Value) -> bool {
        self.access(input) == Access::Read
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        let moved = input.get("action").and_then(Value::as_str) == Some("rename_file");
        let named = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_string);
        named("path")
            .into_iter()
            .chain(named("new_path").filter(|_| moved))
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
    /// `lsp.idle_secs`: a server none of whose files was touched for this long is stopped. Zero
    /// keeps servers running.
    pub idle: Duration,
    /// `lsp.write_severity`: the lowest severity reported after a write.
    pub write_severity: WriteSeverity,
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
        let manager = Arc::new(LspManager::new(self));
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
