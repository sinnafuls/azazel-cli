//! `web_fetch`, and the `http` and `https` schemes of the `read` tool.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_ai::Block;
use azazel_core::{ArtifactStore, Lines, Scheme};
use dom_smoothie::{Article, Config, Readability, TextMode};
use htmd::HtmlToMarkdown;
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{CONTENT_TYPE, LOCATION};
use serde::Deserialize;
use serde_json::{Value, json};

use super::github::{self, GitHub};
use super::youtube::{self, YouTube};
use super::{TIMEOUT, USER_AGENT, describe};
use crate::util::parse_input;

/// The largest response body `web_fetch` accepts.
pub const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
/// The most redirects followed for one fetch.
pub const MAX_REDIRECTS: usize = 5;
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
/// The most result text the cache holds; the oldest entries make room for new ones.
const CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;
/// Characters of a page one call returns when it names no `limit`.
pub const DEFAULT_LIMIT: usize = 30_000;
/// The most characters of a page one call can ask for.
pub const MAX_LIMIT: usize = 200_000;

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
pub(super) fn check_target(url: &Url) -> Result<(), String> {
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
pub(super) struct GuardedResolver;

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
pub(super) fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Html,
    Text,
    Pdf,
    Binary,
}

/// What a body is, from its media type, or from its bytes when the server named none.
pub(super) fn classify(media_type: Option<&str>, body: &[u8]) -> Kind {
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
        Some("application/pdf") => Kind::Pdf,
        // Many servers send a PDF as a download of no particular type.
        Some(_) if body.starts_with(b"%PDF-") => Kind::Pdf,
        Some(_) => Kind::Binary,
        None if body.starts_with(b"%PDF-") => Kind::Pdf,
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

/// The lines of a page as `offset` and `web_grep` count them.
pub(super) fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
}

/// The part of a page one call returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// The first line, 1-based.
    pub offset: usize,
    /// The most characters.
    pub chars: usize,
    /// The most lines.
    pub lines: usize,
}

impl Window {
    /// What `web_fetch` returns for its `offset` and `limit`.
    fn of_fetch(offset: Option<usize>, limit: Option<usize>) -> Self {
        Window {
            offset: offset.unwrap_or(1).max(1),
            chars: limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
            lines: usize::MAX,
        }
    }

    /// What `read` returns for its `offset` and `limit`, which counts lines.
    fn of_read(lines: Lines) -> Self {
        Window {
            offset: lines.offset.unwrap_or(1).max(1),
            chars: DEFAULT_LIMIT,
            lines: lines.limit.unwrap_or(usize::MAX).max(1),
        }
    }
}

/// Whole lines of `text` from line `window.offset` on, as many as fit `window`, then a line that
/// says which lines these are and how to go on. A page that fits is returned as it is. A single
/// line longer than the window is cut, and the trailer says so.
pub(super) fn window(text: &str, window: Window) -> Result<String, String> {
    // No more bytes than the characters allowed: it fits without counting.
    if window.offset == 1 && window.lines == usize::MAX && text.len() <= window.chars {
        return Ok(text.to_string());
    }
    let total = lines(text).count();
    let first = window.offset;
    if first > total {
        return Err(format!(
            "offset {first} is past the end of the page ({total} lines)"
        ));
    }
    let mut out = String::new();
    let mut used = 0;
    let mut last = first - 1;
    // The length of a line that did not fit even alone.
    let mut cut = None;
    for (index, line) in lines(text).enumerate().skip(first - 1).take(window.lines) {
        let length = line.chars().count();
        let joint = usize::from(last >= first);
        if used + joint + length > window.chars {
            if last < first {
                let end = line
                    .char_indices()
                    .nth(window.chars)
                    .map_or(line.len(), |(at, _)| at);
                out.push_str(&line[..end]);
                cut = Some(length);
                last = index + 1;
            }
            break;
        }
        if joint == 1 {
            out.push('\n');
        }
        out.push_str(line);
        used += joint + length;
        last = index + 1;
    }
    if first == 1 && last == total && cut.is_none() {
        return Ok(out);
    }
    out.push_str(&format!("\n[lines {first}-{last} of {total}"));
    if let Some(length) = cut {
        out.push_str(&format!(
            "; line {last} is cut at {} of {length} characters",
            window.chars
        ));
    }
    if last < total {
        out.push_str(&format!(
            "; continue with offset={}, or web_grep to search the page",
            last + 1
        ));
    } else if cut.is_some() {
        out.push_str("; web_grep searches all of it");
    }
    out.push(']');
    Ok(out)
}

struct Cached {
    at: Instant,
    text: Arc<str>,
}

/// The sites whose addresses are read in their own way instead of as a page.
#[derive(Default)]
pub struct Sites {
    pub github: GitHub,
    pub youtube: YouTube,
}

/// Fetches URLs for `web_fetch` and the URL schemes, remembering results for fifteen minutes.
pub struct Fetcher {
    client: reqwest::Client,
    cache: Mutex<HashMap<(String, Format), Cached>>,
    sites: Sites,
}

impl Fetcher {
    pub fn new() -> Result<Self, String> {
        Fetcher::with_sites(Sites::default())
    }

    /// A fetcher that reaches GitHub and YouTube where `sites` says they are.
    pub fn with_sites(sites: Sites) -> Result<Self, String> {
        let client = azazel_ai::tls::client_builder()
            .timeout(TIMEOUT)
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(GuardedResolver)
            .build()
            .map_err(|e| format!("could not set up the HTTP client for web_fetch: {e}"))?;
        Ok(Fetcher {
            client,
            cache: Mutex::default(),
            sites,
        })
    }

    /// The content at `url` in `format`: the whole page text, or for content that is not text a
    /// note naming the artifact file it was saved to.
    pub async fn fetch(&self, url: &str, format: Format, ctx: &ToolCtx) -> Result<Arc<str>, String> {
        let mut url = Url::parse(url.trim()).map_err(|e| {
            format!(
                "invalid URL {url:?}: {e}; give an absolute http or https URL such as \
                 https://example.com/page"
            )
        })?;
        // The body as the server sent it is the page itself, whatever the site.
        let site = match format {
            Format::Raw => None,
            Format::Markdown | Format::Text => Site::of(&url),
        };
        // The lines a GitHub file address names are part of what is asked for.
        if !matches!(
            site,
            Some(Site::GitHub(github::Target::Blob { lines: Some(_), .. }))
        ) {
            url.set_fragment(None);
        }
        let key = (url.to_string(), format);
        if let Some(text) = self.cached(&key) {
            return Ok(text);
        }
        let fresh = async {
            let read = match &site {
                Some(Site::GitHub(target)) => self.sites.github.read(target, &url, self, ctx).await?,
                Some(Site::YouTube(video)) => Some(self.sites.youtube.transcript(video, self).await?),
                None => None,
            };
            match read {
                Some(text) => Ok(text),
                None => self.fetch_fresh(url.clone(), format, ctx).await,
            }
        };
        let text: Arc<str> = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err("aborted by user".to_string()),
            fetched = fresh => fetched?.into(),
        };
        self.remember(key, text.clone());
        Ok(text)
    }

    fn cached(&self, key: &(String, Format)) -> Option<Arc<str>> {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache
            .get(key)
            .filter(|entry| entry.at.elapsed() < CACHE_TTL)
            .map(|entry| entry.text.clone())
    }

    fn remember(&self, key: (String, Format), text: Arc<str>) {
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
        let (url, response) = self.get(url, &[]).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("{url} returned HTTP {status}"));
        }
        let media_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(media_type);
        let body = body(response, &url, MAX_BODY_BYTES).await?;

        let text = match classify(media_type.as_deref(), &body) {
            Kind::Pdf => return pdf(&url, body, ctx).await,
            Kind::Binary => {
                let what = media_type.as_deref().unwrap_or("binary data");
                return save(&url, &format!("{what}, not text"), &body, ctx);
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

    /// Requests `url` with `headers`, following up to [`MAX_REDIRECTS`] redirects, each checked
    /// like the first URL. Returns the response with the URL it came from.
    pub(super) async fn get(
        &self,
        url: Url,
        headers: &[(&str, &str)],
    ) -> Result<(Url, reqwest::Response), String> {
        self.request(url, headers, None).await
    }

    /// Sends `json` to `url` and returns the answer.
    pub(super) async fn post(&self, url: Url, json: &Value) -> Result<(Url, reqwest::Response), String> {
        self.request(url, &[], Some(json)).await
    }

    async fn request(
        &self,
        mut url: Url,
        headers: &[(&str, &str)],
        json: Option<&Value>,
    ) -> Result<(Url, reqwest::Response), String> {
        let mut redirects = 0;
        loop {
            check_target(&url)?;
            let mut request = match json {
                Some(json) => self.client.post(url.clone()).json(json),
                None => self.client.get(url.clone()),
            };
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            let response = request
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

/// A site whose addresses are read in their own way.
enum Site {
    GitHub(github::Target),
    YouTube(String),
}

impl Site {
    fn of(url: &Url) -> Option<Site> {
        github::parse(url)
            .map(Site::GitHub)
            .or_else(|| youtube::video(url).map(Site::YouTube))
    }
}

/// The body of `response`, which came from `url`; a body of more than `limit` bytes is refused.
pub(super) async fn body(
    mut response: reqwest::Response,
    url: &Url,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let too_large = || format!("{url} is larger than the {} MB limit", limit / (1024 * 1024));
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
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
        if body.len() + chunk.len() > limit {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A file that came from `url` as text: its text, the text of a PDF, or for other content that
/// is not text a note naming the artifact it was saved to.
pub(super) async fn file_text(url: &Url, body: Vec<u8>, ctx: &ToolCtx) -> Result<String, String> {
    if body.starts_with(b"%PDF-") {
        return pdf(url, body, ctx).await;
    }
    match String::from_utf8(body) {
        Ok(text) => Ok(text),
        Err(binary) => save(url, "binary data, not text", binary.as_bytes(), ctx),
    }
}

#[cfg(feature = "pdf")]
async fn pdf(url: &Url, body: Vec<u8>, ctx: &ToolCtx) -> Result<String, String> {
    pdf_text(url, body, ctx).await
}

#[cfg(not(feature = "pdf"))]
async fn pdf(url: &Url, body: Vec<u8>, ctx: &ToolCtx) -> Result<String, String> {
    save(url, "a PDF, which this build of azazel cannot read", &body, ctx)
}

/// Saves a body as an artifact named after the last segment of `url`.
fn save_artifact(url: &Url, body: &[u8], ctx: &ToolCtx) -> Result<PathBuf, String> {
    let store = ctx
        .services
        .get::<ArtifactStore>()
        .ok_or("there is no artifact store to save it to")?;
    let file = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
        .unwrap_or("download");
    store
        .save(&format!("{}-{file}", ctx.call_id), body)
        .map_err(|e| format!("saving it failed: {e}"))
}

/// Saves a body that cannot be returned as text and says where it is. `what` is what the body is
/// in place of text, such as `image/png, not text`.
pub(super) fn save(url: &Url, what: &str, body: &[u8], ctx: &ToolCtx) -> Result<String, String> {
    match save_artifact(url, body, ctx) {
        Ok(path) => Ok(format!(
            "{url} is {what} ({} bytes). Saved to {}",
            body.len(),
            path.display()
        )),
        Err(why) => Err(format!("{url} is {what}, and {why}")),
    }
}

/// The text of a PDF, every page under a `--- page N ---` line, below a first line that names
/// the artifact the file was saved to. A PDF without readable text is saved and reported like
/// other content that is not text.
#[cfg(feature = "pdf")]
async fn pdf_text(url: &Url, body: Vec<u8>, ctx: &ToolCtx) -> Result<String, String> {
    let size = body.len();
    let saved = save_artifact(url, &body, ctx);
    // Parsing a large PDF takes a while: off the async threads.
    let read = tokio::task::spawn_blocking(move || crate::pdf::read_all(&body)).await;
    let read = read.unwrap_or_else(|_| Err("the PDF could not be read".to_string()));
    match (read, saved) {
        (Ok(text), Ok(path)) => Ok(format!(
            "{url} is a PDF ({size} bytes), saved to {}\n\n{text}",
            path.display()
        )),
        (Ok(text), Err(why)) => Ok(format!(
            "{url} is a PDF ({size} bytes); it was not saved: {why}\n\n{text}"
        )),
        (Err(unreadable), Ok(path)) => Ok(format!(
            "{url} is a PDF ({size} bytes) without text to return: {unreadable}. Saved to {}",
            path.display()
        )),
        (Err(unreadable), Err(why)) => Err(format!(
            "{url} is a PDF without text to return: {unreadable}; {why}"
        )),
    }
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
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch an http or https URL. `format` is `markdown` (default: the main content of an HTML page \
         as Markdown), `text` (the same as plain text) or `raw` (the body as sent). JSON and plain text \
         are returned unchanged, a PDF comes back as its text, and anything else is saved to a file \
         whose path is returned. A long page comes in parts of `limit` characters (default 30000): the \
         last line names the lines shown and the `offset` (1-based line) to continue from; `web_grep` \
         finds the lines that matter without reading the page. Bodies over 5 MB are refused. Results \
         are cached for 15 minutes, so paging and `web_grep` fetch a page once. Pages that build their \
         content with scripts come back empty: use `browser` for those. A GitHub address is read for \
         what it names: a repository is cloned one commit deep and comes back as its file tree, its \
         README and the path of the clone; a file (with `#L10-L20`, those lines), a directory, a pull \
         request (`/files` or `.diff` for the changes), an issue and an Actions run (with the logs of \
         its failed steps) come from GitHub's API, with the login of `gh` when it is signed in. A \
         YouTube video comes back as its captions with the time of each part."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "format": {"type": "string", "enum": ["markdown", "text", "raw"]},
                "offset": {"type": "integer", "minimum": 1, "description": "First line to return (1-based)"},
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_LIMIT,
                    "description": "Most characters to return (default 30000)"
                }
            },
            "required": ["url"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Net
    }

    fn replay_safe(&self, _: &Value) -> bool {
        true
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
        let page = match self.fetcher.fetch(&input.url, input.format, ctx).await {
            Ok(page) => page,
            Err(error) => return ToolOutput::error(error),
        };
        match window(&page, Window::of_fetch(input.offset, input.limit)) {
            Ok(text) => ToolOutput::text(text),
            Err(error) => ToolOutput::error(error),
        }
    }
}

/// `http://…` and `https://…` for the `read` tool: the page as Markdown, paged by `read`'s
/// `offset` and `limit`.
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
        self.read_lines(rest, Lines::default(), ctx).await
    }

    async fn read_lines(&self, rest: &str, lines: Lines, ctx: &ToolCtx) -> Result<Vec<Block>, String> {
        let url = format!("{}://{rest}", self.scheme);
        let page = self.fetcher.fetch(&url, Format::Markdown, ctx).await?;
        Ok(vec![Block::text(window(&page, Window::of_read(lines))?)])
    }

    /// Reading a web address is a fetch of its host.
    fn access(&self, rest: &str) -> (Access, Option<String>) {
        match domain_subject(&format!("{}://{rest}", self.scheme)) {
            Some(domain) => (Access::Net, Some(domain)),
            None => (Access::Read, None),
        }
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
        for binary in ["image/png", "application/octet-stream"] {
            assert_eq!(classify(Some(binary), b"plain"), Kind::Binary, "{binary}");
        }
        // A PDF is known by its type, or by its first bytes when the type says nothing useful.
        assert_eq!(classify(Some("application/pdf"), b"plain"), Kind::Pdf);
        assert_eq!(
            classify(Some("application/octet-stream"), b"%PDF-1.7\n"),
            Kind::Pdf
        );
        assert_eq!(classify(None, b"%PDF-1.4\n"), Kind::Pdf);
    }

    fn part(text: &str, offset: usize, chars: usize) -> Result<String, String> {
        window(
            text,
            Window {
                offset,
                chars,
                lines: usize::MAX,
            },
        )
    }

    #[test]
    fn a_page_that_fits_is_returned_as_it_is() {
        let page = "line one\r\n  line two\n";
        assert_eq!(part(page, 1, page.len()).unwrap(), page);
        assert_eq!(part(page, 1, DEFAULT_LIMIT).unwrap(), page);
        // Characters are counted, not bytes: four two-byte letters fit a window of four.
        assert_eq!(part("éé\néé", 1, 5).unwrap(), "éé\néé");
    }

    #[test]
    fn a_long_page_comes_in_whole_lines_and_each_part_names_the_next() {
        let page: String = (1..=10).map(|n| format!("l{n}\n")).collect();
        assert_eq!(
            part(&page, 1, 8).unwrap(),
            "l1\nl2\nl3\n[lines 1-3 of 10; continue with offset=4, or web_grep to search the page]"
        );
        // Following the offsets walks the page once, in order, with nothing lost or repeated.
        let mut seen = Vec::new();
        let mut offset = 1;
        loop {
            let text = part(&page, offset, 8).unwrap();
            let (body, trailer) = text.rsplit_once("\n[").unwrap();
            seen.extend(body.lines().map(str::to_string));
            match trailer.split_once("offset=") {
                Some((_, rest)) => offset = rest.split(',').next().unwrap().parse().unwrap(),
                None => {
                    assert_eq!(trailer, "lines 10-10 of 10]");
                    break;
                }
            }
        }
        let all: Vec<String> = (1..=10).map(|n| format!("l{n}")).collect();
        assert_eq!(seen, all);

        assert_eq!(
            part(&page, 11, 8).unwrap_err(),
            "offset 11 is past the end of the page (10 lines)"
        );
    }

    #[test]
    fn read_counts_its_limit_in_lines() {
        let page: String = (1..=10).map(|n| format!("l{n}\n")).collect();
        let lines = |offset, limit| window(&page, Window::of_read(Lines { offset, limit }));
        assert_eq!(lines(None, None).unwrap(), page);
        assert_eq!(
            lines(Some(4), Some(2)).unwrap(),
            "l4\nl5\n[lines 4-5 of 10; continue with offset=6, or web_grep to search the page]"
        );
        assert_eq!(lines(Some(9), None).unwrap(), "l9\nl10\n[lines 9-10 of 10]");
        assert_eq!(lines(None, Some(10)).unwrap(), page.trim_end());
    }

    #[test]
    fn a_line_longer_than_the_window_is_cut_and_the_trailer_says_so() {
        let long = "é".repeat(50);
        assert_eq!(
            part(&long, 1, 20).unwrap(),
            format!(
                "{}\n[lines 1-1 of 1; line 1 is cut at 20 of 50 characters; web_grep searches all of it]",
                "é".repeat(20)
            )
        );
        let page = format!("short\n{long}\nend");
        // The long line does not fit after `short`, so it starts the next part.
        assert_eq!(
            part(&page, 1, 20).unwrap(),
            "short\n[lines 1-1 of 3; continue with offset=2, or web_grep to search the page]"
        );
        assert_eq!(
            part(&page, 2, 20).unwrap(),
            format!(
                "{}\n[lines 2-2 of 3; line 2 is cut at 20 of 50 characters; continue with offset=3, \
                 or web_grep to search the page]",
                "é".repeat(20)
            )
        );
    }

    #[test]
    fn web_fetch_limits_stay_within_their_bounds() {
        assert_eq!(
            Window::of_fetch(None, None),
            Window {
                offset: 1,
                chars: DEFAULT_LIMIT,
                lines: usize::MAX
            }
        );
        assert_eq!(Window::of_fetch(Some(0), Some(0)).chars, 1);
        assert_eq!(Window::of_fetch(Some(0), Some(0)).offset, 1);
        assert_eq!(Window::of_fetch(Some(7), Some(usize::MAX)).chars, MAX_LIMIT);
        assert_eq!(classify(None, b"  <!DOCTYPE html><html></html>"), Kind::Html);
        assert_eq!(classify(None, b"just words"), Kind::Text);
        assert_eq!(classify(None, &[0xff, 0xfe, 0x00]), Kind::Binary);
        assert_eq!(media_type("Text/HTML; charset=UTF-8"), "text/html");
    }
}
