//! The server behind `azazel remote`: serves the web page and carries the RPC protocol of a session
//! over a WebSocket, to browsers that were paired with a link.
//!
//! It knows nothing of the application. Sessions reach it through [`Sessions`]; what it adds is
//! the part that must be right whoever uses it: who may connect ([`auth`]), what they may send
//! ([`policy`]), and the limits.

mod assets;
mod auth;
mod http;
mod limits;
mod policy;
mod qr;
mod server;
#[cfg(test)]
mod testing;
mod ws;

use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub use limits::{Clock, SystemClock};
pub use policy::{check, policy};
pub use qr::{qr_matrix, qr_svg};
pub use server::Server;

/// What a paired device may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Watch only.
    View,
    /// Watch, answer and steer.
    Control,
}

/// Where a session lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The session of a terminal UI that shares itself.
    Terminal,
    /// A background session.
    Background,
}

/// One session, as the list shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: Option<String>,
    pub kind: Kind,
    pub state: String,
    pub cwd: String,
    pub cost_usd: Option<f64>,
    pub mode: Option<String>,
    /// RFC 3339.
    pub started: String,
}

/// A connection to one session: the lines it sends (`hello` first) and a way to send it lines.
pub struct Conn {
    pub frames: mpsc::Receiver<String>,
    pub commands: mpsc::Sender<String>,
}

/// A session a browser asks to start.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NewSession {
    /// The directory it works in; `~` stands for the home directory.
    pub cwd: String,
    /// The first message; without one the session waits for the browser.
    #[serde(default)]
    pub prompt: Option<String>,
    /// The permission mode; the configured one when not given.
    #[serde(default)]
    pub mode: Option<String>,
    /// A name for the request: the same request sent again starts nothing and answers with the
    /// session the first one started.
    #[serde(default)]
    pub request_id: Option<String>,
}

/// Why a session was not started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// The request asks for something that is not there (a folder that does not exist).
    Invalid(String),
    /// It is not allowed, or the person at this machine said no.
    Refused(String),
    /// It was allowed but could not be started.
    Failed(String),
}

/// What a session can be started in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderKind {
    /// A project a session has worked in.
    Project,
    /// A folder of `remote.folders`.
    Root,
    /// A folder inside one of those, or anywhere when every folder is allowed.
    Dir,
}

/// One folder a browser may start a session in, or look into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    /// The full path, as `cwd` takes it.
    pub path: String,
    /// The path as people read it, with `~` for the home directory.
    pub name: String,
    pub kind: FolderKind,
    /// Whether its project is trusted. An untrusted one is started only once the person at
    /// this machine allows it, unless bypass is allowed.
    pub trusted: bool,
    /// Whether a session may be started in it; a folder that only leads to others may not.
    pub startable: bool,
}

/// The answer of `GET /api/folders`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Folders {
    /// The folder looked into, when one was; its parent when that may be looked into too.
    pub at: Option<String>,
    pub parent: Option<String>,
    pub folders: Vec<Folder>,
    /// Whether any folder of this machine may be typed in (bypass is allowed).
    pub anywhere: bool,
}

/// The sessions a server can reach.
#[async_trait]
pub trait Sessions: Send + Sync + 'static {
    async fn list(&self) -> Vec<SessionInfo>;
    /// Connects to a session. The error says why it cannot be reached (it may have ended).
    async fn open(&self, id: &str) -> Result<Conn, String>;
    /// Whether [`Sessions::start`] can work at all; the page offers a new chat only then.
    fn can_start(&self) -> bool {
        false
    }
    /// The folders a session may be started in, or with `under` the folders inside that one.
    async fn folders(&self, _under: Option<&str>) -> Result<Folders, StartError> {
        Err(StartError::Refused(
            "new sessions cannot be started from here".to_string(),
        ))
    }
    /// Starts a session and answers with its id.
    async fn start(&self, _new: NewSession) -> Result<String, StartError> {
        Err(StartError::Refused(
            "new sessions cannot be started from here".to_string(),
        ))
    }
}

pub struct Options {
    pub bind: IpAddr,
    /// Tried first; when it is taken the next nine are. 0 lets the system choose.
    pub port: u16,
    /// The address of a tunnel or proxy in front of the server: the origin of links, and an
    /// origin and host the server accepts.
    pub public_url: Option<String>,
    pub link_ttl: Duration,
    pub device_ttl: Duration,
    pub redact: bool,
    pub allow_bypass: bool,
}

/// A pairing link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Link {
    pub url: String,
    pub access: Access,
    /// RFC 3339.
    pub expires: String,
}
