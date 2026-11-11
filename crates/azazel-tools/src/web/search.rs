//! `web_search`: one tool in front of several search services.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use dom_smoothie::Readability;
use reqwest::Url;
use reqwest::header::ACCEPT;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{TIMEOUT, USER_AGENT, describe};
use crate::util::parse_input;

pub const BRAVE_URL: &str = "https://api.search.brave.com";
pub const TAVILY_URL: &str = "https://api.tavily.com";
pub const EXA_URL: &str = "https://api.exa.ai";
pub const DUCKDUCKGO_URL: &str = "https://html.duckduckgo.com";

const DEFAULT_LIMIT: usize = 8;
const MAX_LIMIT: usize = 20;
const SNIPPET_CHARS: usize = 500;

/// The backends that need an API key, in the order `auto` tries them, with the variable holding it.
static KEYED: [(&str, &str); 3] = [
    ("brave", "BRAVE_API_KEY"),
    ("tavily", "TAVILY_API_KEY"),
    ("exa", "EXA_API_KEY"),
];

/// How far back results may date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recency {
    Day,
    Week,
    Month,
    Year,
}

impl Recency {
    fn word(self) -> &'static str {
        match self {
            Recency::Day => "day",
            Recency::Week => "week",
            Recency::Month => "month",
            Recency::Year => "year",
        }
    }

    fn letter(self) -> &'static str {
        match self {
            Recency::Day => "d",
            Recency::Week => "w",
            Recency::Month => "m",
            Recency::Year => "y",
        }
    }

    fn days(self) -> u64 {
        match self {
            Recency::Day => 1,
            Recency::Week => 7,
            Recency::Month => 31,
            Recency::Year => 365,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub query: String,
    /// The most results wanted.
    pub limit: usize,
    /// A bare domain (`docs.rs`) to restrict results to.
    pub site: Option<String>,
    pub recency: Option<Recency>,
}

impl SearchQuery {
    /// The query with `site` as a `site:` operator, for services that filter through query syntax.
    fn with_site_operator(&self) -> String {
        match &self.site {
            Some(site) => format!("{} site:{site}", self.query),
            None => self.query.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[async_trait]
pub trait SearchBackend: Send + Sync {
    /// The name this backend has in `[web] search`.
    fn name(&self) -> &'static str;
    /// Where requests go; its host is what permission rules see.
    fn base_url(&self) -> &str;
    /// The results for `query`, best first. May return more than `query.limit`.
    async fn search(&self, client: &reqwest::Client, query: &SearchQuery) -> Result<Vec<SearchHit>, String>;
}

/// The client `web_search` sends its requests with.
pub(crate) fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("could not set up the HTTP client for web_search: {e}"))
}

/// `base` + `path` with `params` as the query string.
fn endpoint(base: &str, path: &str, params: &[(&str, &str)]) -> Result<Url, String> {
    let mut url = Url::parse(&format!("{}{path}", base.trim_end_matches('/')))
        .map_err(|e| format!("invalid search URL {base:?}: {e}"))?;
    url.query_pairs_mut().extend_pairs(params);
    Ok(url)
}

/// Sends `request` and returns the body of a successful response.
async fn send(request: reqwest::RequestBuilder, backend: &str) -> Result<String, String> {
    let failed = |e: reqwest::Error| format!("{backend} search failed: {}", describe(e));
    let response = request.send().await.map_err(failed)?;
    let status = response.status();
    let body = response.text().await.map_err(failed)?;
    if !status.is_success() {
        return Err(format!(
            "{backend} search failed with HTTP {status}: {}",
            tidy(&body)
        ));
    }
    Ok(body)
}

fn decode<'a, T: Deserialize<'a>>(body: &'a str, backend: &str) -> Result<T, String> {
    serde_json::from_str(body).map_err(|e| format!("{backend} sent a response that could not be read: {e}"))
}

/// One line of text: whitespace runs collapsed, cut to a snippet's length.
fn tidy(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match joined.char_indices().nth(SNIPPET_CHARS) {
        Some((cut, _)) => format!("{}…", &joined[..cut]),
        None => joined,
    }
}

/// The text of an HTML fragment: tags dropped, entities decoded.
fn plain(html: &str) -> String {
    if html.contains(['<', '&']) {
        tidy(&Readability::from(html).doc.text())
    } else {
        tidy(html)
    }
}

/// `secs` after the Unix epoch as `YYYY-MM-DDTHH:MM:SS.000Z`.
fn iso_utc(secs: u64) -> String {
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days to a civil date, counting from 0000-03-01 so leap days fall at the end of a year.
    let shifted = days + 719_468;
    let (era, day_of_era) = (shifted / 146_097, shifted % 146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

/// Brave Search: `GET /res/v1/web/search` with the key in `X-Subscription-Token`.
pub struct Brave {
    base_url: String,
    key: String,
}

impl Brave {
    pub fn new(base_url: impl Into<String>, key: impl Into<String>) -> Self {
        Brave {
            base_url: base_url.into(),
            key: key.into(),
        }
    }
}

#[derive(Deserialize)]
struct BraveResponse {
    #[serde(default)]
    web: Option<BraveWeb>,
}

#[derive(Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Deserialize)]
struct BraveResult {
    #[serde(default)]
    title: Option<String>,
    url: String,
    #[serde(default)]
    description: Option<String>,
}

#[async_trait]
impl SearchBackend for Brave {
    fn name(&self) -> &'static str {
        "brave"
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn search(&self, client: &reqwest::Client, query: &SearchQuery) -> Result<Vec<SearchHit>, String> {
        let q = query.with_site_operator();
        let count = query.limit.min(MAX_LIMIT).to_string();
        let freshness = query.recency.map(|r| format!("p{}", r.letter()));
        let mut params = vec![
            ("q", q.as_str()),
            ("count", count.as_str()),
            ("text_decorations", "false"),
        ];
        if let Some(freshness) = &freshness {
            params.push(("freshness", freshness.as_str()));
        }
        let url = endpoint(&self.base_url, "/res/v1/web/search", &params)?;
        let request = client
            .get(url)
            .header("X-Subscription-Token", &self.key)
            .header(ACCEPT, "application/json");
        let body = send(request, self.name()).await?;
        let response: BraveResponse = decode(&body, self.name())?;
        let results = response.web.map(|web| web.results).unwrap_or_default();
        Ok(results
            .into_iter()
            .map(|r| SearchHit {
                title: plain(r.title.as_deref().unwrap_or_default()),
                url: r.url,
                snippet: plain(r.description.as_deref().unwrap_or_default()),
            })
            .collect())
    }
}

/// Tavily: `POST /search` with a bearer key.
pub struct Tavily {
    base_url: String,
    key: String,
}

impl Tavily {
    pub fn new(base_url: impl Into<String>, key: impl Into<String>) -> Self {
        Tavily {
            base_url: base_url.into(),
            key: key.into(),
        }
    }
}

#[derive(Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    results: Vec<TavilyResult>,
}

#[derive(Deserialize)]
struct TavilyResult {
    #[serde(default)]
    title: Option<String>,
    url: String,
    #[serde(default)]
    content: Option<String>,
}

#[async_trait]
impl SearchBackend for Tavily {
    fn name(&self) -> &'static str {
        "tavily"
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn search(&self, client: &reqwest::Client, query: &SearchQuery) -> Result<Vec<SearchHit>, String> {
        let mut body = json!({"query": query.query, "max_results": query.limit.min(MAX_LIMIT)});
        if let Some(site) = &query.site {
            body["include_domains"] = json!([site]);
        }
        if let Some(recency) = query.recency {
            body["time_range"] = json!(recency.word());
        }
        let request = client
            .post(format!("{}/search", self.base_url.trim_end_matches('/')))
            .bearer_auth(&self.key)
            .json(&body);
        let body = send(request, self.name()).await?;
        let response: TavilyResponse = decode(&body, self.name())?;
        Ok(response
            .results
            .into_iter()
            .map(|r| SearchHit {
                title: tidy(r.title.as_deref().unwrap_or_default()),
                url: r.url,
                snippet: tidy(r.content.as_deref().unwrap_or_default()),
            })
            .collect())
    }
}

/// Exa: `POST /search` with the key in `x-api-key`; snippets are the result's highlights.
pub struct Exa {
    base_url: String,
    key: String,
}

impl Exa {
    pub fn new(base_url: impl Into<String>, key: impl Into<String>) -> Self {
        Exa {
            base_url: base_url.into(),
            key: key.into(),
        }
    }
}

#[derive(Deserialize)]
struct ExaResponse {
    #[serde(default)]
    results: Vec<ExaResult>,
}

#[derive(Deserialize)]
struct ExaResult {
    #[serde(default)]
    title: Option<String>,
    url: String,
    #[serde(default)]
    highlights: Option<Vec<String>>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[async_trait]
impl SearchBackend for Exa {
    fn name(&self) -> &'static str {
        "exa"
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn search(&self, client: &reqwest::Client, query: &SearchQuery) -> Result<Vec<SearchHit>, String> {
        let mut body = json!({
            "query": query.query,
            "numResults": query.limit.min(MAX_LIMIT),
            "contents": {"highlights": true},
        });
        if let Some(site) = &query.site {
            body["includeDomains"] = json!([site]);
        }
        if let Some(recency) = query.recency {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs());
            body["startPublishedDate"] = json!(iso_utc(now.saturating_sub(recency.days() * 86_400)));
        }
        let request = client
            .post(format!("{}/search", self.base_url.trim_end_matches('/')))
            .header("x-api-key", &self.key)
            .json(&body);
        let body = send(request, self.name()).await?;
        let response: ExaResponse = decode(&body, self.name())?;
        Ok(response
            .results
            .into_iter()
            .map(|r| {
                let highlights = r.highlights.filter(|h| !h.is_empty()).map(|h| h.join(" "));
                let snippet = highlights.or(r.summary).or(r.text).unwrap_or_default();
                SearchHit {
                    title: tidy(r.title.as_deref().unwrap_or_default()),
                    url: r.url,
                    snippet: tidy(&snippet),
                }
            })
            .collect())
    }
}

/// A SearXNG instance: `GET <instance>/search?format=json`. The instance must have the `json`
/// format enabled in its settings.
pub struct Searxng {
    base_url: String,
}

impl Searxng {
    pub fn new(base_url: impl Into<String>) -> Self {
        Searxng {
            base_url: base_url.into(),
        }
    }
}

#[derive(Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngResult>,
}

#[derive(Deserialize)]
struct SearxngResult {
    #[serde(default)]
    title: Option<String>,
    url: String,
    #[serde(default)]
    content: Option<String>,
}

#[async_trait]
impl SearchBackend for Searxng {
    fn name(&self) -> &'static str {
        "searxng"
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn search(&self, client: &reqwest::Client, query: &SearchQuery) -> Result<Vec<SearchHit>, String> {
        let q = query.with_site_operator();
        let mut params = vec![("q", q.as_str()), ("format", "json")];
        if let Some(recency) = query.recency {
            params.push(("time_range", recency.word()));
        }
        let url = endpoint(&self.base_url, "/search", &params)?;
        let body = send(client.get(url).header(ACCEPT, "application/json"), self.name()).await?;
        let response: SearxngResponse = decode(&body, self.name())?;
        Ok(response
            .results
            .into_iter()
            .map(|r| SearchHit {
                title: tidy(r.title.as_deref().unwrap_or_default()),
                url: r.url,
                snippet: tidy(r.content.as_deref().unwrap_or_default()),
            })
            .collect())
    }
}

/// DuckDuckGo's keyless HTML results page: `GET /html/`.
pub struct DuckDuckGo {
    base_url: String,
}

impl DuckDuckGo {
    pub fn new(base_url: impl Into<String>) -> Self {
        DuckDuckGo {
            base_url: base_url.into(),
        }
    }
}

#[async_trait]
impl SearchBackend for DuckDuckGo {
    fn name(&self) -> &'static str {
        "duckduckgo"
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn search(&self, client: &reqwest::Client, query: &SearchQuery) -> Result<Vec<SearchHit>, String> {
        let q = query.with_site_operator();
        let mut params = vec![("q", q.as_str())];
        if let Some(recency) = query.recency {
            params.push(("df", recency.letter()));
        }
        let url = endpoint(&self.base_url, "/html/", &params)?;
        let body = send(client.get(url), self.name()).await?;
        parse_duckduckgo(&body)
    }
}

/// The organic results of a DuckDuckGo HTML results page.
fn parse_duckduckgo(html: &str) -> Result<Vec<SearchHit>, String> {
    let doc = Readability::from(html).doc;
    if doc.select(".anomaly-modal__modal").exists() {
        return Err(
            "duckduckgo answered with a bot challenge instead of results; wait before searching again, or \
             set BRAVE_API_KEY, TAVILY_API_KEY, EXA_API_KEY or [web] searxng_url to use another backend"
                .to_string(),
        );
    }
    let mut hits = Vec::new();
    for result in doc.select("div.result").iter() {
        if result.has_class("result--ad") {
            continue;
        }
        let link = result.select("a.result__a");
        let Some(url) = link.attr("href").and_then(|href| duckduckgo_target(&href)) else {
            continue;
        };
        hits.push(SearchHit {
            title: tidy(&link.text()),
            url,
            snippet: tidy(&result.select(".result__snippet").text()),
        });
    }
    Ok(hits)
}

/// The page a DuckDuckGo result link leads to. Result links point at a redirector carrying the
/// target in `uddg`; advertisement links have no target.
fn duckduckgo_target(href: &str) -> Option<String> {
    let absolute = match href.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None if href.starts_with('/') => format!("https://duckduckgo.com{href}"),
        None => href.to_string(),
    };
    let url = Url::parse(&absolute).ok()?;
    if !url
        .host_str()
        .is_some_and(|host| host == "duckduckgo.com" || host.ends_with(".duckduckgo.com"))
    {
        return Some(absolute);
    }
    url.query_pairs()
        .find(|(name, _)| name == "uddg")
        .map(|(_, target)| target.into_owned())
}

fn key(env: &dyn Fn(&str) -> Option<String>, var: &str) -> Option<String> {
    env(var).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// The backend `web_search` would use with this configuration and environment, for `azazel doctor`:
/// `brave`, `tavily`, `exa`, `searxng` or `duckduckgo`, or `none` when `search` names no backend.
/// `auto` is the first of brave, tavily and exa whose key is set, then searxng when a URL is set,
/// then duckduckgo.
pub fn active_backend(search: &str, searxng_url: &str, env: &dyn Fn(&str) -> Option<String>) -> &'static str {
    match search.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => match KEYED.iter().find(|(_, var)| key(env, var).is_some()) {
            Some((name, _)) => name,
            None if searxng_url.trim().is_empty() => "duckduckgo",
            None => "searxng",
        },
        "brave" => "brave",
        "tavily" => "tavily",
        "exa" => "exa",
        "searxng" => "searxng",
        "duckduckgo" => "duckduckgo",
        _ => "none",
    }
}

/// The backend for this configuration and environment pointed at the real service, or why it
/// cannot be used: an explicitly chosen backend without its key or URL, or an unknown name.
pub fn backend_from_config(
    search: &str,
    searxng_url: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Arc<dyn SearchBackend>, String> {
    let name = active_backend(search, searxng_url, env);
    let keyed = KEYED.iter().find(|(backend, _)| *backend == name);
    let api_key = match keyed {
        Some((_, var)) => {
            key(env, var).ok_or_else(|| format!("the {name} search backend needs an API key: set {var}"))?
        }
        None => String::new(),
    };
    let backend: Arc<dyn SearchBackend> = match name {
        "brave" => Arc::new(Brave::new(BRAVE_URL, api_key)),
        "tavily" => Arc::new(Tavily::new(TAVILY_URL, api_key)),
        "exa" => Arc::new(Exa::new(EXA_URL, api_key)),
        "searxng" => {
            let url = searxng_url.trim();
            if url.is_empty() {
                return Err(
                    "the searxng search backend needs the instance's address: set [web] searxng_url"
                        .to_string(),
                );
            }
            Arc::new(Searxng::new(url))
        }
        "duckduckgo" => Arc::new(DuckDuckGo::new(DUCKDUCKGO_URL)),
        _ => {
            return Err(format!(
                "unknown search backend {search:?}: set [web] search to auto, brave, tavily, exa, searxng \
                 or duckduckgo"
            ));
        }
    };
    Ok(backend)
}

pub struct WebSearchTool {
    backend: Result<Arc<dyn SearchBackend>, String>,
    client: reqwest::Client,
}

impl WebSearchTool {
    /// `backend` is the service to query, or the reason none is usable, which every call then
    /// reports (see [`backend_from_config`]).
    pub fn new(backend: Result<Arc<dyn SearchBackend>, String>, client: reqwest::Client) -> Self {
        WebSearchTool { backend, client }
    }
}

#[derive(Deserialize)]
struct SearchInput {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    site: Option<String>,
    #[serde(default)]
    recency: Option<Recency>,
}

/// `site` as a bare domain: no scheme, no trailing slash.
fn bare_domain(site: &str) -> Option<String> {
    let site = site.trim();
    let site = site
        .strip_prefix("https://")
        .or_else(|| site.strip_prefix("http://"))
        .unwrap_or(site)
        .trim_end_matches('/');
    (!site.is_empty()).then(|| site.to_string())
}

fn render(query: &str, hits: &[SearchHit]) -> String {
    if hits.is_empty() {
        return format!("no results for {query:?}");
    }
    let mut out = String::new();
    for (index, hit) in hits.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let title = if hit.title.is_empty() {
            &hit.url
        } else {
            &hit.title
        };
        out.push_str(&format!("{}. {title}\n   {}\n", index + 1, hit.url));
        if !hit.snippet.is_empty() {
            out.push_str(&format!("   {}\n", hit.snippet));
        }
    }
    out.truncate(out.trim_end().len());
    out
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web. Returns numbered results, each with title, URL and snippet. `site` keeps \
         results to one domain (`docs.rs`); `recency` keeps results from the last `day`, `week`, `month` \
         or `year`; `limit` is the number of results (default 8, at most 20). Snippets are excerpts: \
         read a result with `web_fetch` before relying on it."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT},
                "site": {"type": "string"},
                "recency": {"type": "string", "enum": ["day", "week", "month", "year"]}
            },
            "required": ["query"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Net
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        let Ok(backend) = &self.backend else {
            return Vec::new();
        };
        let host = Url::parse(backend.base_url())
            .ok()
            .and_then(|url| url.host_str().map(str::to_string));
        host.map(|host| vec![format!("domain:{host}")])
            .unwrap_or_default()
    }

    fn concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: SearchInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        let query = SearchQuery {
            query: input.query.trim().to_string(),
            limit: input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
            site: input.site.as_deref().and_then(bare_domain),
            recency: input.recency,
        };
        if query.query.is_empty() {
            return ToolOutput::error("query is empty");
        }
        let backend = match &self.backend {
            Ok(backend) => backend,
            Err(reason) => return ToolOutput::error(reason.clone()),
        };
        let found = tokio::select! {
            _ = ctx.cancel.cancelled() => return ToolOutput::error("aborted by user"),
            found = backend.search(&self.client, &query) => found,
        };
        match found {
            Ok(mut hits) => {
                hits.truncate(query.limit);
                let details = json!({"backend": backend.name(), "results": hits});
                ToolOutput::text(render(&query.query, &hits)).with_details(details)
            }
            Err(error) => ToolOutput::error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_utc_calendar_dates() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_utc(951_782_400), "2000-02-29T00:00:00.000Z");
        assert_eq!(iso_utc(951_868_799), "2000-02-29T23:59:59.000Z");
        assert_eq!(iso_utc(951_868_800), "2000-03-01T00:00:00.000Z");
        assert_eq!(iso_utc(1_700_000_000), "2023-11-14T22:13:20.000Z");
        assert_eq!(iso_utc(1_735_689_599), "2024-12-31T23:59:59.000Z");
    }

    #[test]
    fn duckduckgo_links_resolve_to_the_result_page() {
        assert_eq!(
            duckduckgo_target("//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust%2Dlang.org%2Fstd%2F&rut=abc")
                .as_deref(),
            Some("https://doc.rust-lang.org/std/")
        );
        assert_eq!(
            duckduckgo_target("https://example.com/a?b=c").as_deref(),
            Some("https://example.com/a?b=c")
        );
        assert_eq!(duckduckgo_target("//duckduckgo.com/y.js?ad_provider=x"), None);
        assert_eq!(duckduckgo_target("not a url"), None);
    }

    #[test]
    fn snippets_are_one_line_and_bounded() {
        assert_eq!(tidy("  a\n\tb   c "), "a b c");
        let long = "é".repeat(SNIPPET_CHARS + 10);
        let cut = tidy(&long);
        assert_eq!(cut.chars().count(), SNIPPET_CHARS + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(
            plain("Vec&lt;T&gt; &amp; <strong>Option</strong>&#x27;s"),
            "Vec<T> & Option's"
        );
    }

    #[test]
    fn sites_are_reduced_to_a_domain() {
        assert_eq!(bare_domain(" https://docs.rs/ ").as_deref(), Some("docs.rs"));
        assert_eq!(bare_domain("docs.rs/tokio").as_deref(), Some("docs.rs/tokio"));
        assert_eq!(bare_domain("  "), None);
    }
}
