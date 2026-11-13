//! `web_search`, `web_fetch` and `web_grep`, and the `http`/`https` schemes that let `read` take URLs.

pub mod fetch;
pub mod github;
pub mod grep;
pub mod search;
pub mod youtube;

use std::sync::Arc;
use std::time::Duration;

use azazel_core::{ExtError, Extension, Registrar};

pub use fetch::{Fetcher, Format, MAX_BODY_BYTES, MAX_REDIRECTS, Sites, UrlScheme, WebFetchTool};
pub use github::GitHub;
pub use grep::WebGrepTool;
pub use search::{
    Brave, DuckDuckGo, Exa, Recency, SearchBackend, SearchHit, SearchQuery, Searxng, Tavily, WebSearchTool,
    active_backend, backend_from_config,
};
pub use youtube::YouTube;

const USER_AGENT: &str = concat!("azazel/", env!("CARGO_PKG_VERSION"));
/// Covers one request from connecting to the last byte of the body.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Registers `web_search`, `web_fetch`, `web_grep` and the `http` and `https` schemes.
pub struct WebExt {
    /// Config `web.search`: `auto`, `brave`, `tavily`, `exa`, `searxng` or `duckduckgo`.
    pub search: String,
    /// Config `web.searxng_url`.
    pub searxng_url: String,
}

impl Extension for WebExt {
    fn id(&self) -> &str {
        "web"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        let env = |name: &str| std::env::var(name).ok();
        let backend = backend_from_config(&self.search, &self.searxng_url, &env);
        let client = search::http_client().map_err(ExtError)?;
        let fetcher = Arc::new(Fetcher::new().map_err(ExtError)?);
        r.tool(Arc::new(WebSearchTool::new(backend, client)));
        r.tool(Arc::new(WebFetchTool::new(fetcher.clone())));
        r.tool(Arc::new(WebGrepTool::new(fetcher.clone())));
        r.scheme(Arc::new(UrlScheme::http(fetcher.clone())));
        r.scheme(Arc::new(UrlScheme::https(fetcher)));
        Ok(())
    }
}

/// `error` with its causes, which `reqwest` hides behind "error sending request", and without the
/// URL, which callers name themselves.
fn describe(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut text = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}
