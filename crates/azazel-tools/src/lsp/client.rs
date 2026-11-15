//! One language server process: JSON-RPC with `Content-Length` framing over its stdio.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use process_wrap::tokio::ChildWrapper;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::{Notify, oneshot};

use super::text::{path_key, path_to_uri, uri_to_path};

/// How long a request may go unanswered.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the server gets to answer `initialize`; some index before they do.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
const METHOD_NOT_FOUND: i64 = -32601;
/// Characters kept of the server's last line on standard error.
const MAX_LAST_WORDS: usize = 300;

type Reply = oneshot::Sender<Result<Value, String>>;

/// The diagnostics a server last published for one file.
#[derive(Default, Clone)]
struct Published {
    /// How many times the server has published for this file.
    count: u64,
    items: Vec<Value>,
}

#[derive(Default)]
struct Shared {
    pending: Mutex<HashMap<i64, Reply>>,
    diagnostics: Mutex<HashMap<String, Published>>,
    published: Notify,
    dead: AtomicBool,
    /// The last line the server wrote to its standard error.
    last_words: Mutex<String>,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) struct Client {
    writer: Arc<tokio::sync::Mutex<ChildStdin>>,
    shared: Arc<Shared>,
    next: AtomicI64,
    child: tokio::sync::Mutex<Box<dyn ChildWrapper>>,
    /// What the server said it can do when it answered `initialize`.
    capabilities: OnceLock<Value>,
}

async fn write_frame(writer: &tokio::sync::Mutex<ChildStdin>, message: &Value) -> std::io::Result<()> {
    let body = message.to_string();
    let mut writer = writer.lock().await;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
        .await?;
    writer.flush().await
}

/// The next framed message, or `None` when the stream has ended or is not the protocol.
async fn read_frame(reader: &mut BufReader<ChildStdout>) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; length?];
    reader.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}

/// What this client answers when the server asks it something.
fn answer(method: &str, params: &Value) -> Result<Value, Value> {
    match method {
        // One `null` per requested section: no settings beyond the server's defaults.
        "workspace/configuration" => {
            let sections = params.get("items").and_then(Value::as_array).map_or(0, Vec::len);
            Ok(Value::Array(vec![Value::Null; sections]))
        }
        "client/registerCapability" | "client/unregisterCapability" | "window/workDoneProgress/create" => {
            Ok(Value::Null)
        }
        _ => Err(json!({ "code": METHOD_NOT_FOUND, "message": format!("{method} is not supported") })),
    }
}

/// Keeps the last line of the server's standard error, which says why it exited when it does.
async fn listen(stderr: ChildStderr, shared: Arc<Shared>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if !line.is_empty() {
            *locked(&shared.last_words) = line.chars().take(MAX_LAST_WORDS).collect();
        }
    }
}

async fn read_loop(
    mut reader: BufReader<ChildStdout>,
    writer: Arc<tokio::sync::Mutex<ChildStdin>>,
    shared: Arc<Shared>,
    complaints: tokio::task::JoinHandle<()>,
) {
    while let Some(message) = read_frame(&mut reader).await {
        let id = message.get("id").cloned().filter(|id| !id.is_null());
        match (message.get("method").and_then(Value::as_str), id) {
            (Some(method), Some(id)) => {
                let params = message.get("params").unwrap_or(&Value::Null);
                let reply = match answer(method, params) {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
                };
                if write_frame(&writer, &reply).await.is_err() {
                    break;
                }
            }
            (Some("textDocument/publishDiagnostics"), None) => {
                let params = message.get("params").unwrap_or(&Value::Null);
                let path = params
                    .get("uri")
                    .and_then(Value::as_str)
                    .and_then(|uri| uri.parse::<lsp_types::Uri>().ok())
                    .and_then(|uri| uri_to_path(&uri));
                if let Some(path) = path {
                    let items = params
                        .get("diagnostics")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let mut all = locked(&shared.diagnostics);
                    let entry = all.entry(path_key(&path)).or_default();
                    entry.count += 1;
                    entry.items = items;
                }
                shared.published.notify_waiters();
            }
            (Some(_), None) => {}
            (None, Some(id)) => {
                let Some(reply) = id.as_i64().and_then(|id| locked(&shared.pending).remove(&id)) else {
                    continue;
                };
                let outcome = match message.get("error") {
                    Some(error) => Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("the language server reported an error")
                        .to_string()),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = reply.send(outcome);
            }
            (None, None) => {}
        }
    }
    // The reason usually arrives on standard error just before the process goes.
    let _ = tokio::time::timeout(Duration::from_millis(500), complaints).await;
    shared.dead.store(true, Ordering::SeqCst);
    // Dropping the senders fails every request still waiting.
    locked(&shared.pending).clear();
    shared.published.notify_waiters();
}

impl Client {
    /// Starts `program` and completes the `initialize` handshake for the workspace at `root`.
    pub async fn start(program: &Path, args: &[String], root: &Path) -> Result<Arc<Client>, String> {
        let mut child = azazel_core::process::helper_command(program, |command| {
            command
                .args(args)
                .current_dir(root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        })
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", program.display()))?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin().take(), child.stdout().take(), child.stderr().take())
        else {
            return Err(format!(
                "could not reach the standard streams of {}",
                program.display()
            ));
        };
        let writer = Arc::new(tokio::sync::Mutex::new(stdin));
        let shared = Arc::new(Shared::default());
        let complaints = tokio::spawn(listen(stderr, shared.clone()));
        tokio::spawn(read_loop(
            BufReader::new(stdout),
            writer.clone(),
            shared.clone(),
            complaints,
        ));
        let client = Arc::new(Client {
            writer,
            shared,
            next: AtomicI64::new(1),
            child: tokio::sync::Mutex::new(child),
            capabilities: OnceLock::new(),
        });

        let uri = path_to_uri(root)?;
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "azazel", "version": env!("CARGO_PKG_VERSION") },
            "rootUri": uri.as_str(),
            "workspaceFolders": [{ "uri": uri.as_str(), "name": name }],
            "capabilities": {
                "general": { "positionEncodings": ["utf-16"] },
                "workspace": {
                    "configuration": true,
                    "workspaceFolders": true,
                    "fileOperations": { "willRename": true, "didRename": true }
                },
                "textDocument": {
                    "synchronization": { "didSave": true },
                    "publishDiagnostics": { "relatedInformation": false },
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "definition": { "linkSupport": false },
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "rename": { "prepareSupport": false }
                }
            }
        });
        let mut answer = client
            .request_within("initialize", params, INITIALIZE_TIMEOUT)
            .await?;
        let _ = client.capabilities.set(answer["capabilities"].take());
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    /// Whether the server announced the capability at `pointer`, a JSON pointer into its
    /// capabilities such as `/workspace/fileOperations/willRename`.
    pub fn supports(&self, pointer: &str) -> bool {
        self.capabilities
            .get()
            .and_then(|capabilities| capabilities.pointer(pointer))
            .is_some_and(|value| !value.is_null() && value != &Value::Bool(false))
    }

    /// Whether the server process has gone.
    pub fn dead(&self) -> bool {
        self.shared.dead.load(Ordering::SeqCst)
    }

    /// Why a request could not be made: the server exited, with what it last said if anything.
    pub fn gone(&self) -> String {
        let last_words = locked(&self.shared.last_words);
        if last_words.is_empty() {
            "it exited".to_string()
        } else {
            format!("it exited: {last_words}")
        }
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.request_within(method, params, REQUEST_TIMEOUT).await
    }

    async fn request_within(&self, method: &str, params: Value, limit: Duration) -> Result<Value, String> {
        if self.dead() {
            return Err(self.gone());
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        locked(&self.shared.pending).insert(id, reply);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if write_frame(&self.writer, &message).await.is_err() {
            locked(&self.shared.pending).remove(&id);
            return Err(self.gone());
        }
        match tokio::time::timeout(limit, answer).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(self.gone()),
            Err(_) => {
                locked(&self.shared.pending).remove(&id);
                Err(format!(
                    "the language server did not answer {method} within {} s",
                    limit.as_secs()
                ))
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        write_frame(&self.writer, &message).await.map_err(|_| self.gone())
    }

    /// How many times diagnostics have been published for `path`.
    pub fn published(&self, path: &Path) -> u64 {
        locked(&self.shared.diagnostics)
            .get(&path_key(path))
            .map_or(0, |published| published.count)
    }

    /// The diagnostics last published for `path`.
    pub fn diagnostics(&self, path: &Path) -> Vec<Value> {
        locked(&self.shared.diagnostics)
            .get(&path_key(path))
            .map(|p| p.items.clone())
            .unwrap_or_default()
    }

    /// The diagnostics published for `path` after the `seen`-th publication, waiting up to `limit`
    /// for them. `None` when the server published nothing new in that time.
    pub async fn diagnostics_after(&self, path: &Path, seen: u64, limit: Duration) -> Option<Vec<Value>> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let waiter = self.shared.published.notified();
            if self.published(path) > seen {
                return Some(self.diagnostics(path));
            }
            if self.dead() || tokio::time::timeout_at(deadline, waiter).await.is_err() {
                return None;
            }
        }
    }

    /// Asks the server to shut down, then makes sure the process is gone.
    pub async fn shutdown(&self) {
        if !self.dead() {
            let _ = self
                .request_within("shutdown", Value::Null, Duration::from_secs(2))
                .await;
            let _ = self.notify("exit", Value::Null).await;
        }
        let mut child = self.child.lock().await;
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
            let _ = child.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_requests_get_an_answer_that_keeps_the_server_going() {
        let configuration = answer("workspace/configuration", &json!({ "items": [{}, {}] }));
        assert_eq!(configuration, Ok(json!([null, null])));
        assert_eq!(answer("client/registerCapability", &Value::Null), Ok(Value::Null));
        let unknown = answer("workspace/applyEdit", &Value::Null).unwrap_err();
        assert_eq!(unknown["code"], METHOD_NOT_FOUND);
    }
}
