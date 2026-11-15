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

/// The sessions a server can reach.
#[async_trait]
pub trait Sessions: Send + Sync + 'static {
    async fn list(&self) -> Vec<SessionInfo>;
    /// Connects to a session. The error says why it cannot be reached (it may have ended).
    async fn open(&self, id: &str) -> Result<Conn, String>;
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
