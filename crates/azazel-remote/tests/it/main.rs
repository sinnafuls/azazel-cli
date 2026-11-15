#![allow(clippy::unwrap_used)]

//! The server over real sockets: pairing, the checks on `Host` and `Origin`, the policy on a
//! WebSocket, and the limits. The sessions behind it are a stand-in.

mod common;
mod http;
mod socket;
