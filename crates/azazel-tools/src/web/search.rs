//! `web_search`: one tool in front of several search services, the first of them the search
//! tool of the lead's own model provider.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_ai::{HostedTool, ModelRef, WebSource};
use azazel_core::side::{LeadModel, Side};
use dom_smoothie::Readability;
use futures::StreamExt;
use reqwest::Url;
use reqwest::header::{ACCEPT, LOCATION};
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
/// The most queries one call may carry.
pub const MAX_QUERIES: usize = 5;
/// Queries of one call sent at the same time.
pub const CONCURRENT_QUERIES: usize = 3;

/// The name the provider's own search has in `[web] search` and in a result's details.
pub const PROVIDER: &str = "provider";
/// How long the provider may take over one search before the configured backend is asked.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(90);
/// The most searches the provider runs for one query, where it takes a limit.
const PROVIDER_SEARCHES: u32 = 2;
/// Where Gemini's grounding puts the pages it found: links that redirect to the page.
pub const GROUNDING_LINKS: &str = "https://vertexaisearch.cloud.google.com/grounding-api-redirect/";
/// How long one such link may take to say where it leads.
const LINK_TIMEOUT: Duration = Duration::from_secs(5);

/// What the provider's model is told when it searches for the tool. The results are read from
/// the citations of its answer, so the answer has to cite every page it reports.
const PROVIDER_INSTRUCTIONS: &str = "You are the search step of a `web_search` tool; a program reads your \
answer. Search the web for the query. Then report what the most relevant pages say about it: one or two \
sentences per page, citing that page, each page on a line of its own, the most relevant first. Cover as \
many different pages as the request asks for. Always search; never answer from memory. Write nothing but \
those lines: no introduction, no conclusion, no advice.";

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

/// The backend `web_search` would use with this configuration and environment when the lead's
/// provider is not asked or does not answer, for `azazel doctor`: `brave`, `tavily`, `exa`,
/// `searxng` or `duckduckgo`; `provider` when only the provider's own search is allowed; `none`
/// when `search` names no backend. `auto` is the first of brave, tavily and exa whose key is set,
/// then searxng when a URL is set, then duckduckgo.
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
        "provider" => PROVIDER,
        _ => "none",
    }
}

/// How far `web_search` goes through the search tool of the lead's model provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSearch {
    /// Never: only the configured backend is asked.
    Off,
    /// First, when the lead's provider has a search tool; the configured backend answers when
    /// it has none or its search fails.
    First,
    /// Nothing else: a search fails when the provider cannot answer it.
    Only,
}

impl ProviderSearch {
    /// What `[web] search` and `[web] provider_search` ask for: `search = "provider"` is the
    /// provider alone, `auto` puts it first unless `provider_search` is off, and a backend
    /// named explicitly is used alone.
    pub fn from_config(search: &str, provider_search: bool) -> Self {
        match search.trim().to_ascii_lowercase().as_str() {
            "provider" => ProviderSearch::Only,
            "" | "auto" if provider_search => ProviderSearch::First,
            _ => ProviderSearch::Off,
        }
    }
}

/// The backend for this configuration and environment pointed at the real service, or why it
/// cannot be used: an explicitly chosen backend without its key or URL, or an unknown name.
/// `provider` has no backend of its own.
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
        PROVIDER => return Err(NO_PROVIDER.to_string()),
        _ => {
            return Err(format!(
                "unknown search backend {search:?}: set [web] search to auto, provider, brave, tavily, exa, \
                 searxng or duckduckgo"
            ));
        }
    };
    Ok(backend)
}

/// What a search says when only the provider may answer it and the lead's provider has no
/// search tool.
const NO_PROVIDER: &str = "[web] search is \"provider\", but the lead's model has no web search of its \
                           own (Anthropic, OpenAI, ChatGPT, Gemini and xAI models do): choose such a \
                           model, or set [web] search to auto or to a backend";

pub struct WebSearchTool {
    backend: Result<Arc<dyn SearchBackend>, String>,
    client: reqwest::Client,
    provider: ProviderSearch,
    /// What the addresses of pages start with when the provider hands out redirect links.
    links: String,
    /// Asks a redirect link where it leads without going there.
    unfollowed: reqwest::Client,
}

impl WebSearchTool {
    /// `backend` is the service to query, or the reason none is usable, which every call then
    /// reports (see [`backend_from_config`]). The provider's own search is not used; see
    /// [`WebSearchTool::with_provider`].
    pub fn new(backend: Result<Arc<dyn SearchBackend>, String>, client: reqwest::Client) -> Self {
        let unfollowed = reqwest::Client::builder()
            .timeout(LINK_TIMEOUT)
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| client.clone());
        WebSearchTool {
            backend,
            client,
            provider: ProviderSearch::Off,
            links: GROUNDING_LINKS.to_string(),
            unfollowed,
        }
    }

    /// Searches through the lead's provider as `provider` says. The lead's model and the means
    /// to ask it come from the services of the call ([`Side`], [`LeadModel`]).
    pub fn with_provider(mut self, provider: ProviderSearch) -> Self {
        self.provider = provider;
        self
    }

    /// Takes addresses that start with `prefix` for the provider's redirect links
    /// ([`GROUNDING_LINKS`] unless said otherwise).
    pub fn with_links_at(mut self, prefix: impl Into<String>) -> Self {
        self.links = prefix.into();
        self
    }

    /// The lead's model and the way to ask it, when this tool searches through the lead's
    /// provider and that provider has a search tool.
    fn hosted(&self, ctx: &ToolCtx) -> Option<Hosted> {
        if self.provider == ProviderSearch::Off {
            return None;
        }
        let side = ctx.services.get::<Side>()?;
        let lead = ctx.services.get::<LeadModel>()?;
        let model = side.search_model(&(lead.0)())?;
        Some(Hosted { side, model })
    }

    /// The pages the lead's provider finds for `query`, as hits. `Err` says why there are none.
    async fn ask_provider(
        &self,
        hosted: &Hosted,
        query: &SearchQuery,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Vec<SearchHit>, String> {
        let tool = HostedTool::WebSearch {
            domains: query.site.iter().cloned().collect(),
            max_uses: Some(PROVIDER_SEARCHES),
        };
        let request = cancel.child_token();
        let asked = hosted.side.search(
            &hosted.model,
            PROVIDER_INSTRUCTIONS,
            provider_question(query),
            tool,
            request.clone(),
        );
        let answer = match tokio::time::timeout(PROVIDER_TIMEOUT, asked).await {
            Ok(answer) => answer?,
            Err(_) => {
                request.cancel();
                return Err(format!("no answer within {} s", PROVIDER_TIMEOUT.as_secs()));
            }
        };
        let pages =
            futures::future::join_all(answer.sources.into_iter().map(|page| self.page_hit(page))).await;
        let hits: Vec<SearchHit> = pages
            .into_iter()
            .filter(|hit| query.site.as_deref().is_none_or(|site| on_site(&hit.url, site)))
            .take(query.limit)
            .collect();
        if hits.is_empty() {
            return Err("its search named no pages".to_string());
        }
        Ok(hits)
    }

    /// A page of the provider's search as a hit, a redirect link replaced by where it leads.
    async fn page_hit(&self, page: WebSource) -> SearchHit {
        let url = match page.url.starts_with(&self.links) {
            true => self.destination(&page.url).await.unwrap_or(page.url),
            false => page.url,
        };
        SearchHit {
            title: tidy(&page.title),
            url,
            snippet: tidy(&page.snippet),
        }
    }

    /// Where the redirect link `link` leads, asked of the link's own host only.
    async fn destination(&self, link: &str) -> Option<String> {
        let response = self.unfollowed.get(link).send().await.ok()?;
        let target = response.headers().get(LOCATION)?.to_str().ok()?;
        Url::parse(target)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .map(String::from)
    }

    /// One query answered by the provider's search when there is one, else by the configured
    /// backend.
    async fn answer(
        &self,
        query: &SearchQuery,
        hosted: Option<&Hosted>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Answer {
        let mut refused = None;
        if let Some(hosted) = hosted {
            match self.ask_provider(hosted, query, cancel).await {
                Ok(hits) => {
                    return Answer {
                        found: Ok(hits),
                        backend: PROVIDER,
                        refused: None,
                    };
                }
                Err(why) => refused = Some(why),
            }
        }
        let provider_failed = |why: &str| format!("the provider's web search failed: {why}");
        match (&self.backend, self.provider) {
            (_, ProviderSearch::Only) => Answer {
                found: Err(refused.map_or_else(|| NO_PROVIDER.to_string(), |why| provider_failed(&why))),
                backend: PROVIDER,
                refused: None,
            },
            (Ok(backend), _) => {
                let found = backend.search(&self.client, query).await.map(|mut hits| {
                    hits.truncate(query.limit);
                    hits
                });
                Answer {
                    found,
                    backend: backend.name(),
                    refused,
                }
            }
            (Err(reason), _) => Answer {
                found: Err(match &refused {
                    Some(why) => format!("{}; {reason}", provider_failed(why)),
                    None => reason.clone(),
                }),
                backend: "none",
                refused: None,
            },
        }
    }
}

/// The lead's model and the way to send it a request of its own.
struct Hosted {
    side: Arc<Side>,
    model: ModelRef,
}

/// What one query came to.
struct Answer {
    found: Result<Vec<SearchHit>, String>,
    /// Who answered: [`PROVIDER`] or the backend's name.
    backend: &'static str,
    /// Why the provider's search did not answer, when the backend answered in its place.
    refused: Option<String>,
}

impl Answer {
    /// The answer for a result's details: who gave it, its hits or its failure, and why the
    /// provider's search was passed over.
    fn details(&self, entry: &mut serde_json::Map<String, Value>) {
        entry.insert("backend".to_string(), json!(self.backend));
        match &self.found {
            Ok(hits) => entry.insert("results".to_string(), json!(hits)),
            Err(error) => entry.insert("error".to_string(), json!(error)),
        };
        if let Some(why) = &self.refused {
            entry.insert("provider_error".to_string(), json!(why));
        }
    }
}

/// What the provider's model is asked for `query`.
fn provider_question(query: &SearchQuery) -> String {
    let mut question = format!("Query: {}\nPages wanted: {}", query.query, query.limit);
    if let Some(site) = &query.site {
        question.push_str(&format!("\nOnly pages on the site {site}."));
    }
    if let Some(recency) = query.recency {
        question.push_str(&format!(
            "\nOnly pages published or updated in the last {}.",
            recency.word()
        ));
    }
    question
}

/// Whether `url` is a page of `site`: a bare domain, which covers its subdomains, optionally
/// with the start of a path (`docs.rs/tokio`).
fn on_site(url: &str, site: &str) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    let (domain, path) = match site.split_once('/') {
        Some((domain, path)) => (domain, path),
        None => (site, ""),
    };
    let domain = domain.to_ascii_lowercase();
    let on_domain = url.host_str().is_some_and(|host| {
        host == domain
            || host
                .strip_suffix(domain.as_str())
                .is_some_and(|sub| sub.ends_with('.'))
    });
    on_domain && url.path().trim_start_matches('/').starts_with(path)
}

#[derive(Deserialize)]
struct SearchInput {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    queries: Vec<String>,
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

/// What a call searches for: `query`, then `queries`, without blanks and repeats.
fn wanted(query: Option<String>, queries: Vec<String>) -> Result<Vec<String>, String> {
    let mut wanted: Vec<String> = Vec::new();
    for query in query.into_iter().chain(queries) {
        let query = query.trim();
        if !query.is_empty() && !wanted.iter().any(|seen| seen == query) {
            wanted.push(query.to_string());
        }
    }
    match wanted.len() {
        0 => Err("query is empty".to_string()),
        count if count > MAX_QUERIES => Err(format!(
            "{count} queries in one call; at most {MAX_QUERIES} are searched at once"
        )),
        _ => Ok(wanted),
    }
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
        "Search the web. Returns numbered results, each with title, URL and snippet. Give one `query`, \
         or up to 5 `queries` to search at once: each gets its own `## <query>` section, and one that \
         fails says so in its section. `site` keeps results to one domain (`docs.rs`); `recency` keeps \
         results from the last `day`, `week`, `month` or `year`; `limit` is the number of results per \
         query (default 8, at most 20). Snippets are excerpts: read a result with `web_fetch` before \
         relying on it."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "queries": {
                    "type": "array",
                    "items": {"type": "string"},
                    "maxItems": MAX_QUERIES,
                    "description": "Several searches in one call, in place of or beside `query`"
                },
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT},
                "site": {"type": "string"},
                "recency": {"type": "string", "enum": ["day", "week", "month", "year"]}
            }
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Net
    }

    /// The host of the configured backend. A search through the lead's provider goes to the
    /// host the session's model requests already go to, which is no subject of its own.
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
        let wanted = match wanted(input.query, input.queries) {
            Ok(wanted) => wanted,
            Err(error) => return ToolOutput::error(error),
        };
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let site = input.site.as_deref().and_then(bare_domain);
        let hosted = self.hosted(ctx);
        let searches = futures::stream::iter(wanted.into_iter().map(|query| {
            let query = SearchQuery {
                query,
                limit,
                site: site.clone(),
                recency: input.recency,
            };
            let hosted = hosted.as_ref();
            async move {
                let answer = self.answer(&query, hosted, &ctx.cancel).await;
                (query.query, answer)
            }
        }))
        .buffered(CONCURRENT_QUERIES)
        .collect::<Vec<_>>();
        let mut answers = tokio::select! {
            _ = ctx.cancel.cancelled() => return ToolOutput::error("aborted by user"),
            answers = searches => answers,
        };

        // One query answers as it always did.
        if answers.len() == 1 {
            let (query, answer) = answers.remove(0);
            let mut details = serde_json::Map::new();
            answer.details(&mut details);
            return match answer.found {
                Ok(hits) => ToolOutput::text(render(&query, &hits)).with_details(Value::Object(details)),
                Err(error) => ToolOutput::error(error),
            };
        }
        let mut text = String::new();
        let mut details = Vec::new();
        let all_failed = answers.iter().all(|(_, answer)| answer.found.is_err());
        for (query, answer) in &answers {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!("## {query}\n"));
            match &answer.found {
                Ok(hits) => text.push_str(&render(query, hits)),
                Err(error) => text.push_str(error),
            }
            let mut entry = serde_json::Map::new();
            entry.insert("query".to_string(), json!(query));
            answer.details(&mut entry);
            details.push(Value::Object(entry));
        }
        let output = if all_failed {
            ToolOutput::error(text)
        } else {
            ToolOutput::text(text)
        };
        // Who answered the call: the queries of one call may be answered by different services
        // when the provider's search fails for some of them.
        let backend = answers
            .iter()
            .find(|(_, answer)| answer.found.is_ok())
            .or(answers.first())
            .map_or("none", |(_, answer)| answer.backend);
        output.with_details(json!({"backend": backend, "queries": details}))
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
    fn a_call_searches_its_query_and_queries_once_each_up_to_five() {
        let list = |queries: &[&str]| queries.iter().map(|q| q.to_string()).collect::<Vec<_>>();
        assert_eq!(wanted(Some(" one ".into()), Vec::new()), Ok(list(&["one"])));
        assert_eq!(
            wanted(Some("one".into()), list(&["two", " ", "one", "three"])),
            Ok(list(&["one", "two", "three"]))
        );
        assert_eq!(wanted(None, list(&["a", "b"])), Ok(list(&["a", "b"])));
        assert_eq!(wanted(None, Vec::new()).unwrap_err(), "query is empty");
        assert_eq!(
            wanted(Some("  ".into()), list(&[""])).unwrap_err(),
            "query is empty"
        );
        assert_eq!(
            wanted(Some("a".into()), list(&["b", "c", "d", "e", "f"])).unwrap_err(),
            "6 queries in one call; at most 5 are searched at once"
        );
        assert!(wanted(None, list(&["a", "b", "c", "d", "e"])).is_ok());
    }

    #[test]
    fn sites_are_reduced_to_a_domain() {
        assert_eq!(bare_domain(" https://docs.rs/ ").as_deref(), Some("docs.rs"));
        assert_eq!(bare_domain("docs.rs/tokio").as_deref(), Some("docs.rs/tokio"));
        assert_eq!(bare_domain("  "), None);
    }
}
