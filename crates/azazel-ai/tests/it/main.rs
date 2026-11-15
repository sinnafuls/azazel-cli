#![allow(clippy::unwrap_used)]

//! The integration tests of the crate, one program for all of them.

mod hosted_search;
mod http_provider;
mod oauth;
mod provider_wires;
mod sign_ins;
mod stall;
mod wire_anthropic;
mod wire_google;
mod wire_ollama;
mod wire_openai_chat;
mod wire_openai_responses;
