//! The background sessions of this machine, as the sessions a remote server can reach.

use std::ffi::OsString;
use std::sync::Arc;

use async_trait::async_trait;
use azazel_core::Paths;
use azazel_remote::{Conn, Folders, Kind, NewSession, SavedSession, SessionInfo, Sessions, StartError};
use clap::Parser;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::start::{Ask, Rules, host_args};
use crate::cli::Cli;
use crate::detach::{revive, socket};
use crate::running;

/// Frames that may wait for a browser; the socket closes the browser that lets 4096 pile up.
const QUEUE: usize = 8192;

pub struct BackgroundSessions {
    paths: Paths,
    redact: bool,
    /// The options `azazel remote` was started with, which a session it starts is started with.
    carried: Vec<OsString>,
    /// Where a browser may start a session.
    rules: Rules,
    /// The terminal that allows a session in a folder nobody trusted; none without one.
    asks: Option<mpsc::Sender<Ask>>,
    /// The background sessions browsers started through this server, for the note on quitting.
    started: std::sync::Mutex<Vec<String>>,
    /// The ids of the saved sessions browsers started or went on with through this server: they
    /// may be gone on with again wherever they work.
    owned: std::sync::Mutex<Vec<String>>,
    /// Held while a session is gone on with, so that two requests for one session start one host.
    resuming: tokio::sync::Mutex<()>,
}

impl BackgroundSessions {
    pub fn new(
        paths: Paths,
        redact: bool,
        carried: Vec<OsString>,
        rules: Rules,
        asks: Option<mpsc::Sender<Ask>>,
    ) -> Arc<BackgroundSessions> {
        Arc::new(BackgroundSessions {
            paths,
            redact,
            carried,
            rules,
            asks,
            started: std::sync::Mutex::default(),
            owned: std::sync::Mutex::default(),
            resuming: tokio::sync::Mutex::new(()),
        })
    }

    /// The background session that has the saved session `session_id` open, if one does.
    async fn host_of(&self, session_id: &str) -> Result<Option<String>, StartError> {
        let (home, session) = (self.paths.home.clone(), session_id.to_string());
        tokio::task::spawn_blocking(move || running::host_of(&home, &session))
            .await
            .map_err(|e| StartError::Failed(e.to_string()))
    }

    /// Notes a background session a browser has through this server, for the note on quitting.
    fn note_started(&self, id: &str) {
        let mut started = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !started.iter().any(|known| known == id) {
            started.push(id.to_string());
        }
    }

    fn owned(&self) -> Vec<String> {
        self.owned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn own(&self, session_id: String) {
        let mut owned = self
            .owned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !owned.contains(&session_id) {
            owned.push(session_id);
        }
    }

    /// The sessions browsers started through this server that are still running, oldest first.
    pub fn still_running(&self) -> Vec<running::Entry> {
        let started = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        running::list(&self.paths.home)
            .into_iter()
            .filter(|entry| started.contains(&entry.id) && entry.alive(&self.paths.home))
            .collect()
    }
}

#[async_trait]
impl Sessions for BackgroundSessions {
    async fn list(&self) -> Vec<SessionInfo> {
        let home = self.paths.home.clone();
        let entries = tokio::task::spawn_blocking(move || running::list(&home))
            .await
            .unwrap_or_default();
        entries
            .into_iter()
            .map(|entry| SessionInfo {
                id: entry.id,
                name: entry.name,
                kind: Kind::Background,
                state: entry.state,
                cwd: entry.cwd.display().to_string(),
                cost_usd: entry.cost_usd,
                mode: entry.mode,
                started: entry.started,
            })
            .collect()
    }

    async fn open(&self, id: &str) -> Result<Conn, String> {
        let home = self.paths.home.clone();
        let wanted = id.to_string();
        let found =
            tokio::task::spawn_blocking(move || running::list(&home).into_iter().find(|e| e.id == wanted))
                .await
                .map_err(|e| e.to_string())?;
        let mut entry = found.ok_or_else(|| format!("no background session {id}"))?;
        if entry.interrupted() {
            // Its process died in the middle of a run: it starts again where it stopped.
            let cli = Cli::parse_from(["azazel"]);
            entry = revive(&cli, &self.paths, &entry).await?;
        }
        let (mut reader, mut writer) = socket::connect(&entry.socket)
            .await
            .map_err(|e| format!("cannot reach the background session {}: {e}", entry.id))?;
        let (frames_tx, frames) = mpsc::channel::<String>(QUEUE);
        let (commands, mut commands_rx) = mpsc::channel::<String>(64);
        let redact = self.redact;
        tokio::spawn(async move {
            let mut line = String::new();
            loop {
                line.clear();
                let read = tokio::select! {
                    _ = frames_tx.closed() => return,
                    read = reader.read_line(&mut line) => read,
                };
                if !matches!(read, Ok(n) if n > 0) {
                    return;
                }
                let text = line.trim_end();
                if text.is_empty() {
                    continue;
                }
                let text = if redact { redacted(text) } else { text.to_string() };
                if frames_tx.send(text).await.is_err() {
                    return;
                }
            }
        });
        tokio::spawn(async move {
            while let Some(mut line) = commands_rx.recv().await {
                line.push('\n');
                if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                    return;
                }
            }
        });
        Ok(Conn { frames, commands })
    }

    fn can_start(&self) -> bool {
        true
    }

    async fn folders(&self, under: Option<&str>) -> Result<Folders, StartError> {
        let (rules, under) = (self.rules.clone(), under.map(str::to_string));
        tokio::task::spawn_blocking(move || rules.folders(under.as_deref()))
            .await
            .map_err(|e| StartError::Failed(e.to_string()))?
    }

    async fn history(&self, query: Option<&str>) -> Result<Vec<SavedSession>, StartError> {
        let (rules, query) = (self.rules.clone(), query.map(str::to_string));
        let home = self.paths.home.clone();
        let (owned, redact) = (self.owned(), self.redact);
        // What was typed in a session may hold a secret, as what it says now may.
        let hide = move |text: String| match redact {
            true => azazel_tui::text::redact(&text).into_owned(),
            false => text,
        };
        tokio::task::spawn_blocking(move || {
            rules
                .saved(query.as_deref(), &owned)
                .into_iter()
                .map(|session| SavedSession {
                    running: running::host_of(&home, &session.id),
                    name: session.name.map(hide),
                    cwd: session.cwd,
                    created: session.timestamp,
                    updated: session.updated,
                    first_prompt: session.first_prompt.map(hide),
                    messages: session.messages as u64,
                    id: session.id,
                })
                .collect()
        })
        .await
        .map_err(|e| StartError::Failed(e.to_string()))
    }

    async fn start(&self, mut new: NewSession) -> Result<String, StartError> {
        let home = &self.paths.home;
        let mut resume = None;
        let mut gate = None;
        let mut resumed = None;
        if let Some(wanted) = new.resume.clone() {
            // Only a session in a folder a browser may look into, or one it started itself, is
            // found at all: an id elsewhere is answered as an id that does not exist, whether or
            // not a background session has it open.
            let (rules, owned) = (self.rules.clone(), self.owned());
            let found = tokio::task::spawn_blocking(move || rules.find_saved(&wanted, &owned))
                .await
                .map_err(|e| StartError::Failed(e.to_string()))??;
            // Open in a background session already: that one, not a second.
            if let Some(host) = self.host_of(&found.id).await? {
                self.note_started(&host);
                return Ok(host);
            }
            new.cwd = found.cwd;
            resume = Some(found.path);
            resumed = Some(found.id);
        }
        let (dir, trust) = self.rules.admit(&new, self.asks.as_ref()).await?;
        if let Some(session) = &resumed {
            // One request at a time decides whether a host has the session, so that two for the
            // same one do not start two writers of its file. Looked at again: the other may have
            // started it while this one waited for the terminal.
            gate = Some(self.resuming.lock().await);
            if let Some(host) = self.host_of(session).await? {
                self.note_started(&host);
                return Ok(host);
            }
        }
        let id = crate::detach::new_id(home);
        if let Some(request) = &new.request_id {
            match running::claim_request(home, request, &id) {
                Ok(Ok(())) => {}
                // Sent again: the session the first one started.
                Ok(Err(started)) => return Ok(started),
                Err(e) => return Err(StartError::Failed(format!("cannot note the request: {e}"))),
            }
        }
        let args = host_args(&self.carried, &dir, &new, trust, &id, resume.as_deref());
        match crate::detach::start(home, args, &id).await {
            Ok(()) => {
                self.note_started(&id);
                let session =
                    resumed.or_else(|| running::read_entry(home, &id).map(|entry| entry.session_id));
                if let Some(session) = session {
                    self.own(session);
                }
                drop(gate);
                Ok(id)
            }
            Err(e) => {
                if let Some(request) = &new.request_id {
                    running::release_request(home, request);
                }
                Err(StartError::Failed(e))
            }
        }
    }
}

/// Frames that are complete and may carry what a tool read, a model wrote or a page was called.
const COMPLETE: &[&str] = &[
    "message_end",
    "tool_start",
    "tool_end",
    "approval_request",
    "response",
    // The address and the title of the page may carry a token. The pictures are not redacted.
    "browser_state",
];

/// `line` with the secrets in its strings hidden, when it is a complete frame. Deltas are left
/// alone: `message_end` replaces them.
pub(crate) fn redacted(line: &str) -> String {
    let Ok(mut frame) = serde_json::from_str::<Value>(line) else {
        return line.to_string();
    };
    if !frame["type"]
        .as_str()
        .is_some_and(|kind| COMPLETE.contains(&kind))
    {
        return line.to_string();
    }
    if !hide(&mut frame) {
        return line.to_string();
    }
    frame.to_string()
}

/// Whether anything was hidden.
fn hide(value: &mut Value) -> bool {
    match value {
        Value::String(text) => match azazel_tui::text::redact(text) {
            std::borrow::Cow::Owned(hidden) => {
                *text = hidden;
                true
            }
            std::borrow::Cow::Borrowed(_) => false,
        },
        Value::Array(items) => items.iter_mut().fold(false, |any, item| hide(item) | any),
        Value::Object(members) => members.values_mut().fold(false, |any, item| hide(item) | any),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_frames_lose_their_secrets_and_deltas_and_other_frames_are_left_alone() {
        let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH";
        let end = format!(r#"{{"type":"tool_end","agent":"lead","output":{{"text":"KEY={key}"}}}}"#);
        let hidden = redacted(&end);
        assert!(!hidden.contains(key) && hidden.contains("redacted"), "{hidden}");
        let delta = format!(r#"{{"type":"stream","text":"{key}"}}"#);
        assert_eq!(redacted(&delta), delta);
        let plain = r#"{"type":"idle"}"#;
        assert_eq!(redacted(plain), plain);
        assert_eq!(redacted("not json"), "not json");
    }

    #[test]
    fn the_address_and_title_of_the_agents_page_lose_a_token() {
        let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH";
        let state = format!(
            r#"{{"type":"browser_state","running":true,"url":"https://x.test/cb?k={key}","title":"Key {key}","held":false,"held_by_you":false,"watchers":1}}"#
        );
        let hidden = redacted(&state);
        assert!(!hidden.contains(key) && hidden.contains("redacted"), "{hidden}");
        let frame: Value = serde_json::from_str(&hidden).unwrap();
        assert_eq!(frame["running"], true);
    }
}
