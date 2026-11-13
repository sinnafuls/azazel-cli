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
//! tools sorted by name. `build_body` ends by writing the provider's `extra_body` over what it built.
//!
//! The APIs that reuse another format ([`azure`], [`gemini_cli`]) expose only what differs.

pub mod anthropic;
pub mod azure;
pub mod gemini_cli;
pub mod google;
pub mod normalize;
pub mod ollama;
pub mod openai_chat;
pub mod openai_responses;
pub mod thinking;

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use crate::credentials::Auth;
use crate::providers::presets::{AuthStyle, ProviderConfig};
use crate::sse::{JsonLines, SseParser};
use crate::stream::StreamEvent;
use crate::types::{WebSource, WireApi};

/// The header a wire API takes an API key in when the provider does not say otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyHeader {
    Bearer,
    XApiKey,
    XGoogApiKey,
}

/// Adds the credential, then the provider's configured headers, then the headers its login
/// fills; each replaces a header of the same name that is already in `headers`. A login is
/// always a bearer token. A key goes where the provider's `auth` says, or in the API's own
/// header.
pub(crate) fn add_headers(
    headers: &mut HeaderMap,
    provider: &ProviderConfig,
    auth: &Auth,
    native: KeyHeader,
) {
    let credential = match auth {
        Auth::None => None,
        Auth::OAuth { access, .. } => Some((AUTHORIZATION, format!("Bearer {access}"))),
        Auth::ApiKey(key) => Some(match (provider.auth_style, native) {
            (AuthStyle::ApiKey, _) => (HeaderName::from_static("api-key"), key.clone()),
            (AuthStyle::Bearer, _) | (AuthStyle::Auto, KeyHeader::Bearer) => {
                (AUTHORIZATION, format!("Bearer {key}"))
            }
            (AuthStyle::Auto, KeyHeader::XApiKey) => (HeaderName::from_static("x-api-key"), key.clone()),
            (AuthStyle::Auto, KeyHeader::XGoogApiKey) => {
                (HeaderName::from_static("x-goog-api-key"), key.clone())
            }
        }),
    };
    if let Some((name, value)) = credential {
        match HeaderValue::from_str(&value) {
            Ok(mut value) => {
                value.set_sensitive(true);
                headers.insert(name, value);
            }
            Err(_) => tracing::warn!(
                provider = %provider.id,
                "credential has characters that cannot be sent in a header; sending none"
            ),
        }
    }
    let configured = provider
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()));
    for (name, value) in configured.chain(auth.login_headers(provider)) {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            (Ok(name), Ok(value)) => {
                headers.insert(name, value);
            }
            _ => tracing::warn!(provider = %provider.id, header = %name, "ignoring invalid header"),
        }
    }
}

/// The built-in providers that run a web search themselves, and the hosts they answer at, so a
/// provider configured under another name at the same address counts too.
const SEARCH_PROVIDERS: [&str; 7] = [
    "anthropic",
    "google",
    "google-gemini-cli",
    "openai",
    "openai-codex",
    "xai",
    "xai-oauth",
];
const SEARCH_HOSTS: [&str; 6] = [
    "api.anthropic.com",
    "api.openai.com",
    "api.x.ai",
    "chatgpt.com",
    "cloudcode-pa.googleapis.com",
    "generativelanguage.googleapis.com",
];

/// The wire API a request with hosted tools ([`crate::types::HostedTool`]) is written in for
/// `provider`, or `None` when the provider runs no tools itself. The request shapes are in
/// [`anthropic`] (the `web_search` server tool), [`openai_responses`] (the `web_search` tool, at
/// OpenAI, the ChatGPT backend and xAI) and [`google`] (Google Search grounding).
///
/// xAI serves its search tool on the Responses endpoint only, so such a request goes there even
/// for the provider whose conversations use Chat Completions.
pub fn hosted_api(provider: &ProviderConfig) -> Option<WireApi> {
    let host = provider
        .base_url
        .split_once("://")
        .map_or("", |(_, rest)| rest)
        .split(['/', ':'])
        .next()
        .unwrap_or_default();
    if !SEARCH_PROVIDERS.contains(&provider.id.as_str()) && !SEARCH_HOSTS.contains(&host) {
        return None;
    }
    match provider.api {
        WireApi::AnthropicMessages
        | WireApi::OpenAiResponses
        | WireApi::GoogleGenerative
        | WireApi::GoogleGeminiCli => Some(provider.api),
        WireApi::OpenAiChat if provider.id == "xai" || host == "api.x.ai" => Some(WireApi::OpenAiResponses),
        _ => None,
    }
}

/// The pages of a hosted web search, gathered while a response is decoded, in the order
/// [`crate::types::AssistantMeta::sources`] promises: the pages the answer cites, in the order
/// it first cites them, then the pages the search found and the answer did not use.
#[derive(Default)]
pub(crate) struct Sources {
    cited: Vec<WebSource>,
    found: Vec<WebSource>,
}

impl Sources {
    /// A page the search returned.
    pub(crate) fn found(&mut self, url: &str, title: &str) {
        let url = untracked(url);
        if url.is_empty() || self.found.iter().any(|page| page.url == url) {
            return;
        }
        self.found.push(WebSource {
            url: url.to_string(),
            title: title.trim().to_string(),
            snippet: String::new(),
        });
    }

    /// A citation of the page at `url`. `passage` is what the citation is about: the text quoted
    /// from the page, or the part of the answer the page supports. Passages of one page are
    /// joined in citation order.
    pub(crate) fn cite(&mut self, url: &str, title: &str, passage: &str) {
        let (url, title, passage) = (untracked(url), title.trim(), passage.trim());
        let url = url.as_str();
        if url.is_empty() {
            return;
        }
        let known = match self.cited.iter().position(|page| page.url == url) {
            Some(known) => known,
            None => {
                self.cited.push(WebSource {
                    url: url.to_string(),
                    ..WebSource::default()
                });
                self.cited.len() - 1
            }
        };
        let page = &mut self.cited[known];
        if page.title.is_empty() {
            page.title = title.to_string();
        }
        if !passage.is_empty() && !page.snippet.contains(passage) {
            if !page.snippet.is_empty() {
                page.snippet.push_str(" … ");
            }
            page.snippet.push_str(passage);
        }
    }

    /// The pages gathered so far; leaves the collection empty.
    pub(crate) fn finish(&mut self) -> Vec<WebSource> {
        let mut pages = std::mem::take(&mut self.cited);
        for page in std::mem::take(&mut self.found) {
            match pages.iter_mut().find(|cited| cited.url == page.url) {
                Some(cited) if cited.title.is_empty() => cited.title = page.title,
                Some(_) => {}
                None => pages.push(page),
            }
        }
        pages
    }
}

/// `url` without the `utm_*` tracking parameters a search adds to every page it returns
/// (OpenAI adds `utm_source=openai`), so a page is the same link however it was found.
fn untracked(url: &str) -> String {
    let url = url.trim();
    let (base, fragment) = match url.split_once('#') {
        Some((base, fragment)) => (base, Some(fragment)),
        None => (url, None),
    };
    let Some((path, query)) = base.split_once('?') else {
        return url.to_string();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty() && !pair.to_ascii_lowercase().starts_with("utm_"))
        .collect();
    let mut out = path.to_string();
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// Writes the provider's `extra_body` into a request body, member by member, replacing what the
/// wire module wrote under the same name.
pub(crate) fn apply_extra_body(body: &mut Value, provider: &ProviderConfig) {
    if let Value::Object(members) = body {
        for (name, value) in &provider.extra_body {
            members.insert(name.clone(), value.clone());
        }
    }
}

/// Turns the records of one response (SSE records, or the lines of a JSON-lines body) into stream
/// events.
///
/// Contract:
/// - the first event returned is `Start`;
/// - blocks are indexed in order of first appearance and each ends with a `BlockEnd` carrying the
///   complete block; tool-call arguments that are empty become `{}` and arguments that are not valid
///   JSON become `{"_invalid_json": "<raw text>"}` ([`crate::types::INVALID_JSON_KEY`]);
/// - `Done.message` has role `Assistant`, the blocks in index order, and `meta` with `provider`,
///   `model`, `api`, `usage`, `stop` and `response_id`; `duration_ms` is 0 (the HTTP layer fills
///   it) and `usage.cost_usd` is the price the provider itself reported, else `None` (the HTTP
///   layer then prices the tokens from the catalogue);
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

/// Decodes a complete JSON-lines body. Stops at the first terminal event.
pub fn decode_lines(bytes: &[u8], decoder: &mut dyn Decoder) -> Vec<StreamEvent> {
    let mut lines = JsonLines::new();
    let mut events = Vec::new();
    let records = lines.push(bytes).into_iter().chain(lines.finish());
    for record in records {
        for event in decoder.feed(None, &record.data) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking_parameters_are_dropped_and_a_page_is_one_source() {
        let mut sources = Sources::default();
        sources.cite("https://docs.rs/x?utm_source=openai", "X", "one");
        sources.cite("https://docs.rs/x", "", "two");
        sources.found("https://a.example/p?q=1&UTM_medium=m#part", "A");
        let pages = sources.finish();
        let urls: Vec<&str> = pages.iter().map(|page| page.url.as_str()).collect();
        assert_eq!(urls, ["https://docs.rs/x", "https://a.example/p?q=1#part"]);
        assert_eq!(pages[0].snippet, "one … two");
    }
}
