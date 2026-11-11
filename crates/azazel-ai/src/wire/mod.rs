//! Wire APIs. Each submodule translates [`crate::types::Request`] into one provider API's HTTP
//! request and decodes that API's SSE records into [`StreamEvent`]s.
//!
//! Every wire module exposes the same free functions:
//!
//! ```ignore
//! pub fn build_body(provider: &ProviderConfig, model: &ModelInfo, req: &Request) -> serde_json::Value;
//! pub fn build_request(client: &reqwest::Client, provider: &ProviderConfig, model: &ModelInfo,
//!                      req: &Request, auth: &Auth) -> reqwest::RequestBuilder;
//! pub fn decoder(provider: &ProviderConfig, model: &ModelInfo) -> Box<dyn Decoder>;
//! pub fn classify_error(status: u16, body: &str) -> Option<AiError>;
//! pub fn models_request(client: &reqwest::Client, provider: &ProviderConfig, auth: &Auth) -> reqwest::RequestBuilder;
//! pub fn parse_models(provider: &ProviderConfig, body: &serde_json::Value) -> Vec<ModelInfo>;
//! ```
//!
//! The request handed to `build_body` has already been through [`normalize::normalize`] and has its
//! tools sorted by name.

pub mod anthropic;
pub mod google;
pub mod normalize;
pub mod openai_chat;
pub mod openai_responses;
pub mod thinking;

use crate::sse::SseParser;
use crate::stream::StreamEvent;

/// Turns the SSE records of one response into stream events.
///
/// Contract:
/// - the first event returned is `Start`;
/// - blocks are indexed in order of first appearance and each ends with a `BlockEnd` carrying the
///   complete block; tool-call arguments that are empty become `{}` and arguments that are not valid
///   JSON become `{"_invalid_json": "<raw text>"}` ([`crate::types::INVALID_JSON_KEY`]);
/// - `Done.message` has role `Assistant`, the blocks in index order, and `meta` with `provider`,
///   `model`, `api`, `usage` (token counts only), `stop` and `response_id`; `duration_ms` is 0 and
///   `usage.cost_usd` is `None` (the HTTP layer fills both);
/// - `stop` is `ToolUse` when the message contains a tool call, `Length` when the provider cut the
///   output at its limit, otherwise `Stop`;
/// - an in-band provider error becomes `Error { error, partial }` where `partial` holds the blocks
///   completed so far (`None` when there are none);
/// - nothing is returned after a terminal event.
pub trait Decoder: Send {
    fn feed(&mut self, event: Option<&str>, data: &str) -> Vec<StreamEvent>;

    /// The byte stream ended. Returns the terminal event if `feed` has not produced one: `Done` when
    /// the API marks completion only by closing the stream and the response was complete, otherwise
    /// `Error { error: Network("stream ended before the response was complete"), partial }`.
    fn finish(&mut self) -> Vec<StreamEvent>;
}

/// Decodes a complete SSE body. Stops at the first terminal event.
pub fn decode_sse(bytes: &[u8], decoder: &mut dyn Decoder) -> Vec<StreamEvent> {
    let mut parser = SseParser::new();
    let mut events = Vec::new();
    let records = parser.push(bytes).into_iter().chain(parser.finish());
    for record in records {
        for event in decoder.feed(record.event.as_deref(), &record.data) {
            let terminal = event.is_terminal();
            events.push(event);
            if terminal {
                return events;
            }
        }
    }
    events.extend(decoder.finish());
    events
}
