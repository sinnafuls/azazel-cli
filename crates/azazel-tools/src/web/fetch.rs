//! `web_fetch`, and the `http` and `https` schemes of the `read` tool.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_ai::Block;
use azazel_core::{ArtifactStore, Scheme};
use dom_smoothie::{Article, Config, Readability, TextMode};
use htmd::HtmlToMarkdown;
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{CONTENT_TYPE, LOCATION};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{TIMEOUT, USER_AGENT, describe};
use crate::util::parse_input;

/// The largest response body `web_fetch` accepts.
pub const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
/// The most redirects followed for one fetch.
pub const MAX_REDIRECTS: usize = 5;
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
/// The most result text the cache holds; the oldest entries make room for new ones.
const CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;

/// The IPv6 address of the AWS instance metadata service.
const AWS_METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// How `web_fetch` returns a page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// The main content of an HTML page as Markdown.
    #[default]
    Markdown,
    /// The main content of an HTML page as plain text.
    Text,
    /// The body as the server sent it.
    Raw,
}

/// Whether `ip` is link-local or a cloud metadata address, which a fetch must never reach.
/// Loopback and private addresses are fine: local development servers live there.
fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_link_local(),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(mapped) => mapped.is_link_local(),
            None => ip.is_unicast_link_local() || ip == AWS_METADATA_V6,
        },
    }
}

/// Refuses URLs that are not http or https and hosts written as a blocked address.
fn check_target(url: &Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "only http and https URLs can be fetched; {url} is a {} URL",
            url.scheme()
        ));
    }
    let host = url.host_str().unwrap_or_default();
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    match literal.parse::<IpAddr>() {
        Ok(ip) if is_blocked(ip) => Err(format!(
            "refusing to fetch {url}: {ip} is a link-local or cloud-metadata address"
        )),
        _ => Ok(()),
    }
}

/// Resolves names through the system resolver and fails when any address is blocked, so the
/// address checked is the address connected to.
struct GuardedResolver;

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str();
            let addrs: Vec<SocketAddr> = match tokio::net::lookup_host((host, 0)).await {
                Ok(addrs) => addrs.collect(),
                Err(error) => return Err(BoxError::from(error)),
            };
            if let Some(addr) = addrs.iter().find(|addr| is_blocked(addr.ip())) {
                return Err(BoxError::from(format!(
                    "refusing to connect: {host} resolves to {}, a link-local or cloud-metadata address",
                    addr.ip()
                )));
            }
            let addrs: Addrs = Box::new(addrs.into_iter());
            Ok(addrs)
        })
    }
}

/// The lower-cased media type of a `Content-Type` value, without parameters.
fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Html,
    Text,
    Binary,
}

/// What a body is, from its media type, or from its bytes when the server named none.
fn classify(media_type: Option<&str>, body: &[u8]) -> Kind {
    match media_type.filter(|m| !m.is_empty()) {
        Some("text/html" | "application/xhtml+xml") => Kind::Html,
        Some(m)
            if m.starts_with("text/")
                || m.ends_with("/json")
                || m.ends_with("+json")
                || m.ends_with("/xml")
                || m.ends_with("+xml")
                || matches!(
                    m,
                    "application/javascript"
                        | "application/x-ndjson"
                        | "application/yaml"
                        | "application/x-yaml"
                        | "application/toml"
                ) =>
        {
            Kind::Text
        }
        Some(_) => Kind::Binary,
        None => match std::str::from_utf8(body) {
            Ok(text) => {
                let start: String = text.trim_start().chars().take(15).collect();
                let start = start.to_ascii_lowercase();
                if start.starts_with("<!doctype html") || start.starts_with("<html") {
                    Kind::Html
                } else {
                    Kind::Text
                }
            }
            Err(_) => Kind::Binary,
        },
    }
}

/// The article `dom_smoothie` finds in `html`, if any.
fn extract(html: &str, url: &str, text_mode: TextMode) -> Option<Article> {
    let config = Config {
        text_mode,
        ..Config::default()
    };
    Readability::new(html, Some(url), Some(config)).ok()?.parse().ok()
}

/// The article of an HTML page as Markdown, or the whole page when no article is found.
fn html_to_markdown(html: &str, url: &str) -> Result<String, String> {
    let converter = HtmlToMarkdown::builder()
        .skip_tags(vec![
            "head", "script", "style", "noscript", "template", "svg", "iframe",
        ])
        .build();
    let convert = |html: &str| {
        converter
            .convert(html)
            .map_err(|e| format!("could not convert {url} to Markdown: {e}"))
    };
    if let Some(article) = extract(html, url, TextMode::Raw) {
        let content: &str = &article.content;
        let body = convert(content)?;
        let body = body.trim();
        if !body.is_empty() {
            let title = article.title.trim();
            let has_title = title.is_empty()
                || body
                    .lines()
                    .any(|line| line.starts_with('#') && line.trim_start_matches('#').trim() == title);
            return Ok(if has_title {
                body.to_string()
            } else {
                format!("# {title}\n\n{body}")
            });
        }
    }
    convert(html)
}

/// The article of an HTML page as plain text, or the text of the whole page when no article is
/// found.
fn html_to_text(html: &str, url: &str) -> String {
    if let Some(article) = extract(html, url, TextMode::Formatted) {
        let body = article.text_content.trim();
        if !body.is_empty() {
            let title = article.title.trim();
            return if title.is_empty() || body.contains(title) {
                body.to_string()
            } else {
                format!("{title}\n\n{body}")
            };
        }
    }
    let doc = Readability::from(html).doc;
    doc.select("script, style, noscript, template").remove();
    match doc.body() {
        Some(body) => body.formatted_text().trim().to_string(),
        None => doc.formatted_text().trim().to_string(),
    }
}

struct Cached {
    at: Instant,
    text: String,
}

/// Fetches URLs for `web_fetch` and the URL schemes, remembering results for fifteen minutes.
pub struct Fetcher {
    client: reqwest::Client,
    cache: Mutex<HashMap<(String, Format), Cached>>,
}

impl Fetcher {
    pub fn new() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(GuardedResolver)
            .build()
            .map_err(|e| format!("could not set up the HTTP client for web_fetch: {e}"))?;
        Ok(Fetcher {
            client,
            cache: Mutex::default(),
        })
    }

    /// The content at `url` in `format`: page text, or for content that is not text a note
    /// naming the artifact file it was saved to.
    pub async fn fetch(&self, url: &str, format: Format, ctx: &ToolCtx) -> Result<String, String> {
        let mut url = Url::parse(url.trim()).map_err(|e| {
            format!(
                "invalid URL {url:?}: {e}; give an absolute http or https URL such as \
                 https://example.com/page"
            )
        })?;
        url.set_fragment(None);
        let key = (url.to_string(), format);
        if let Some(text) = self.cached(&key) {
            return Ok(text);
        }
        let text = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err("aborted by user".to_string()),
            fetched = self.fetch_fresh(url, format, ctx) => fetched?,
        };
        self.remember(key, text.clone());
        Ok(text)
    }

    fn cached(&self, key: &(String, Format)) -> Option<String> {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache
            .get(key)
            .filter(|entry| entry.at.elapsed() < CACHE_TTL)
            .map(|entry| entry.text.clone())
    }

    fn remember(&self, key: (String, Format), text: String) {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache.retain(|_, entry| entry.at.elapsed() < CACHE_TTL);
        let mut held: usize = cache.values().map(|entry| entry.text.len()).sum();
        while held + text.len() > CACHE_MAX_BYTES {
            let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(entry) = cache.remove(&oldest) {
                held -= entry.text.len();
            }
        }
        cache.insert(
            key,
            Cached {
                at: Instant::now(),
                text,
            },
        );
    }

    async fn fetch_fresh(&self, url: Url, format: Format, ctx: &ToolCtx) -> Result<String, String> {
        let (url, mut response) = self.follow(url).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("{url} returned HTTP {status}"));
        }
        let media_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(media_type);

        let too_large = || {
            format!(
                "{url} is larger than the {} MB limit",
                MAX_BODY_BYTES / (1024 * 1024)
            )
        };
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            return Err(too_large());
        }
        let mut body = Vec::new();
        loop {
            let chunk = response
                .chunk()
                .await
                .map_err(|e| format!("reading {url} failed: {}", describe(e)))?;
            let Some(chunk) = chunk else { break };
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(too_large());
            }
            body.extend_from_slice(&chunk);
        }

        let text = match classify(media_type.as_deref(), &body) {
            Kind::Binary => {
                let what = media_type.as_deref().unwrap_or("binary data");
                return save(&url, what, &body, ctx);
            }
            Kind::Text => String::from_utf8_lossy(&body).into_owned(),
            Kind::Html => {
                let html = String::from_utf8_lossy(&body).into_owned();
                match format {
                    Format::Raw => html,
                    Format::Markdown | Format::Text => {
                        let page = url.to_string();
                        // Parsing is CPU work, and the parsers' trees cannot cross an await.
                        tokio::task::spawn_blocking(move || match format {
                            Format::Text => Ok(html_to_text(&html, &page)),
                            _ => html_to_markdown(&html, &page),
                        })
                        .await
                        .map_err(|_| format!("could not convert {url}: its HTML could not be processed"))??
                    }
                }
            }
        };
        if text.trim().is_empty() {
            return Ok(format!("({url} has no text content)"));
        }
        Ok(text)
    }

    /// Requests `url`, following up to [`MAX_REDIRECTS`] redirects, each checked like the first
    /// URL. Returns the response with the URL it came from.
    async fn follow(&self, mut url: Url) -> Result<(Url, reqwest::Response), String> {
        let mut redirects = 0;
        loop {
            check_target(&url)?;
            let response = self
                .client
                .get(url.clone())
                .send()
                .await
                .map_err(|e| format!("could not fetch {url}: {}", describe(e)))?;
            let next = match response.status().as_u16() {
                301 | 302 | 303 | 307 | 308 => response.headers().get(LOCATION).map(|location| {
                    let location = location.to_str().ok()?;
                    url.join(location).ok()
                }),
                _ => None,
            };
            let next = match next {
                None => return Ok((url, response)),
                Some(next) => {
                    next.ok_or_else(|| format!("{url} redirects to a location that is not a URL"))?
                }
            };
            redirects += 1;
            if redirects > MAX_REDIRECTS {
                return Err(format!(
                    "gave up on {url}: it redirects again after {MAX_REDIRECTS} redirects"
                ));
            }
            url = next;
            url.set_fragment(None);
        }
    }
}

/// Saves a body that is not text as an artifact and says where it is.
fn save(url: &Url, what: &str, body: &[u8], ctx: &ToolCtx) -> Result<String, String> {
    let store = ctx
        .services
        .get::<ArtifactStore>()
        .ok_or_else(|| format!("{url} is {what}, not text, and there is no artifact store to save it to"))?;
    let file = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
        .unwrap_or("download");
    let path = store
        .save(&format!("{}-{file}", ctx.call_id), body)
        .map_err(|e| format!("{url} is {what}, not text, and saving it failed: {e}"))?;
    Ok(format!(
        "{url} is {what} ({} bytes), not text. Saved to {}",
        body.len(),
        path.display()
    ))
}

/// What permission rules are matched against when `url` is fetched: `domain:<host>`. `None` when
/// it is not an http or https URL.
pub fn domain_subject(url: &str) -> Option<String> {
    let url = Url::parse(url.trim()).ok()?;
    let host = url
        .host_str()
        .filter(|_| matches!(url.scheme(), "http" | "https"))?;
    Some(format!("domain:{host}"))
}

pub struct WebFetchTool {
    fetcher: Arc<Fetcher>,
}

impl WebFetchTool {
    pub fn new(fetcher: Arc<Fetcher>) -> Self {
        WebFetchTool { fetcher }
    }
}

#[derive(Deserialize)]
struct FetchInput {
    url: String,
    #[serde(default)]
    format: Format,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch an http or https URL. `format` is `markdown` (default: the main content of an HTML page \
         as Markdown), `text` (the same as plain text) or `raw` (the body as sent). JSON and plain text \
         are returned unchanged; anything else is saved to a file whose path is returned. Bodies over \
         5 MB are refused. Results are cached for 15 minutes. Pages that build their content with \
         scripts come back empty: use `browser` for those."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "format": {"type": "string", "enum": ["markdown", "text", "raw"]}
            },
            "required": ["url"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Net
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        let url = input.get("url").and_then(Value::as_str);
        url.and_then(domain_subject).into_iter().collect()
    }

    fn concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: FetchInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        match self.fetcher.fetch(&input.url, input.format, ctx).await {
            Ok(text) => ToolOutput::text(text),
            Err(error) => ToolOutput::error(error),
        }
    }
}

/// `http://…` and `https://…` for the `read` tool: the page as Markdown.
pub struct UrlScheme {
    scheme: &'static str,
    fetcher: Arc<Fetcher>,
}

impl UrlScheme {
    pub fn http(fetcher: Arc<Fetcher>) -> Self {
        UrlScheme {
            scheme: "http",
            fetcher,
        }
    }

    pub fn https(fetcher: Arc<Fetcher>) -> Self {
        UrlScheme {
            scheme: "https",
            fetcher,
        }
    }
}

#[async_trait]
impl Scheme for UrlScheme {
    fn scheme(&self) -> &str {
        self.scheme
    }

    async fn read(&self, rest: &str, ctx: &ToolCtx) -> Result<Vec<Block>, String> {
        let url = format!("{}://{rest}", self.scheme);
        let text = self.fetcher.fetch(&url, Format::Markdown, ctx).await?;
        Ok(vec![Block::text(text)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_local_and_metadata_addresses_are_blocked_and_local_ones_are_not() {
        for blocked in [
            "169.254.169.254",
            "169.254.0.1",
            "169.254.255.255",
            "fe80::1",
            "febf::1",
            "::ffff:169.254.169.254",
            "fd00:ec2::254",
        ] {
            assert!(is_blocked(blocked.parse().unwrap()), "{blocked}");
        }
        for allowed in [
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.10",
            "172.16.0.1",
            "169.253.255.255",
            "169.255.0.0",
            "93.184.216.34",
            "::1",
            "fec0::1",
            "fd00::1",
            "::ffff:10.0.0.1",
            "2606:4700::1111",
        ] {
            assert!(!is_blocked(allowed.parse().unwrap()), "{allowed}");
        }
    }

    #[test]
    fn bodies_are_classified_by_media_type_then_by_content() {
        assert_eq!(classify(Some("text/html"), b""), Kind::Html);
        assert_eq!(classify(Some("application/xhtml+xml"), b""), Kind::Html);
        for text in [
            "text/plain",
            "text/csv",
            "application/json",
            "application/problem+json",
            "application/xml",
            "image/svg+xml",
        ] {
            assert_eq!(classify(Some(text), b""), Kind::Text, "{text}");
        }
        for binary in ["application/pdf", "image/png", "application/octet-stream"] {
            assert_eq!(classify(Some(binary), b"plain"), Kind::Binary, "{binary}");
        }
        assert_eq!(classify(None, b"  <!DOCTYPE html><html></html>"), Kind::Html);
        assert_eq!(classify(None, b"just words"), Kind::Text);
        assert_eq!(classify(None, &[0xff, 0xfe, 0x00]), Kind::Binary);
        assert_eq!(media_type("Text/HTML; charset=UTF-8"), "text/html");
    }
}
