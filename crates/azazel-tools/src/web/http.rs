//! `http`: one request to a web API. The credential the user stored for the host is added on
//! the way out and taken out of everything that comes back, so the agent works with a service
//! without ever holding its key.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_ai::ServiceStore;
use azazel_ai::services::{self, NO_SERVICE, REDACTED, Service};
use azazel_core::{ArtifactStore, ExtError, Extension, Registrar};
use reqwest::header::{CONTENT_TYPE, HeaderName, HeaderValue, LOCATION, SET_COOKIE};
use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::{Value, json};

use super::fetch::{
    DEFAULT_LIMIT, GuardedResolver, Kind, MAX_LIMIT, check_target, classify, domain_subject, media_type, save,
};
use super::{MAX_BODY_BYTES, MAX_REDIRECTS, TIMEOUT, USER_AGENT, describe};
use crate::util::parse_input;

/// Hides what looks like a secret in a text, leaving `[redacted:<kind>]` in its place.
pub type Redact = for<'a> fn(&'a str) -> Cow<'a, str>;

const METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];
/// The longest one request may take when the call asks for more than the default.
const MAX_TIMEOUT_SECS: u64 = 300;
/// The mark every redaction leaves, whatever was hidden.
const REDACTED_MARK: &str = "[redacted:";

const DESCRIPTION: &str = "Send one HTTP request to a web API and get the status, the response headers \
and the body back: `method` (GET by default), `url`, `headers`, and a `body` string or a `json` value. \
When the user has stored a credential for the host, it is added to the request for you and taken out of \
what comes back: never ask for a key and never put one into a request yourself. `service` names the \
stored credential to use when several cover the host; `none` sends the request without one. Redirects \
are followed for a GET or HEAD within the same host only. A body longer than `limit` characters (default \
30000) is cut and saved whole to a file; content that is not text is saved to a file. To read a web page \
use `web_fetch`.";

/// The `http` tool.
pub struct HttpTool {
    client: reqwest::Client,
    stored: Arc<ServiceStore>,
    redact: Redact,
    description: String,
}

#[derive(Deserialize)]
struct HttpInput {
    url: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    json: Option<Value>,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// What a request carries.
struct Body {
    bytes: Vec<u8>,
    json: bool,
}

/// One request as the call asked for it; `url` moves on with each redirect followed.
struct Exchange<'a> {
    method: Method,
    url: Url,
    headers: &'a BTreeMap<String, String>,
    body: Option<Body>,
    service: Option<(&'a str, &'a Service)>,
    timeout: Duration,
    limit: usize,
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// A `Set-Cookie` value with the cookie's value hidden: a session opened with a stored
/// credential is as good as the credential.
fn without_cookie_value(cookie: &str) -> String {
    let (pair, attributes) = match cookie.split_once(';') {
        Some((pair, attributes)) => (pair, Some(attributes)),
        None => (cookie, None),
    };
    let name = pair.split_once('=').map_or(pair, |(name, _)| name);
    match attributes {
        Some(attributes) => format!("{name}={REDACTED};{attributes}"),
        None => format!("{name}={REDACTED}"),
    }
}

impl HttpTool {
    /// `redact` is applied, beside the removal of the credential itself, to what a request that
    /// carried a stored credential returns.
    pub fn new(stored: Arc<ServiceStore>, redact: Redact) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(GuardedResolver)
            .build()
            .map_err(|e| format!("could not set up the HTTP client for http: {e}"))?;
        // The services the session starts with are named, so the agent knows what it can reach.
        let named: Vec<String> = stored
            .services()
            .iter()
            .filter(|(_, service)| !matches!(service, Service::Site { .. }))
            .map(|(name, service)| format!("{name} ({})", service.target()))
            .collect();
        let description = match named.as_slice() {
            [] => DESCRIPTION.to_string(),
            named => format!("{DESCRIPTION}\nStored credentials: {}.", named.join(", ")),
        };
        Ok(HttpTool {
            client,
            stored,
            redact,
            description,
        })
    }

    /// The stored service whose credential a call with `input` would carry.
    fn service_of(&self, input: &Value) -> Option<(String, String)> {
        let url = Url::parse(input.get("url")?.as_str()?.trim()).ok()?;
        let named = input.get("service").and_then(Value::as_str);
        let services = self.stored.services();
        let (name, service) = services::pick(&services, &url, named).ok()??;
        Some((name.to_string(), service.target().to_string()))
    }

    fn build(&self, exchange: &Exchange<'_>) -> Result<reqwest::RequestBuilder, String> {
        let service = exchange.service.map(|(_, service)| service);
        let mut target = exchange.url.clone();
        if let Some(Service::Query { name, value, .. }) = service {
            // The stored value takes the place of one the agent wrote.
            if exchange.url.query_pairs().any(|(key, _)| key == name.as_str()) {
                let kept: Vec<(String, String)> = exchange
                    .url
                    .query_pairs()
                    .filter(|(key, _)| key != name.as_str())
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect();
                target.query_pairs_mut().clear().extend_pairs(kept);
            }
            target.query_pairs_mut().append_pair(name, value);
        }
        let mut request = self
            .client
            .request(exchange.method.clone(), target)
            .timeout(exchange.timeout);
        let replaced = match service {
            Some(Service::Header { name, .. }) => Some(name.as_str()),
            Some(Service::Basic { .. }) => Some("authorization"),
            _ => None,
        };
        let mut typed = false;
        for (name, value) in exchange.headers {
            if replaced.is_some_and(|replaced| replaced.eq_ignore_ascii_case(name)) {
                continue;
            }
            let header = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| format!("`{name}` is not a header name"))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| format!("the value of the header `{name}` cannot be sent"))?;
            typed |= header == CONTENT_TYPE;
            request = request.header(header, value);
        }
        if let Some(body) = &exchange.body {
            if body.json && !typed {
                request = request.header(CONTENT_TYPE, "application/json");
            }
            request = request.body(body.bytes.clone());
        }
        Ok(match service {
            Some(Service::Header { name, value, .. }) => request.header(name.as_str(), value.as_str()),
            Some(Service::Basic {
                username, password, ..
            }) => request.basic_auth(username, Some(password)),
            _ => request,
        })
    }

    /// Sends the request, following redirects that stay on the host, and returns the response
    /// with the redirect that was not followed, if it ended on one.
    async fn send(&self, exchange: &mut Exchange<'_>) -> Result<(reqwest::Response, Option<Url>), String> {
        let mut followed = 0;
        loop {
            check_target(&exchange.url)?;
            let (method, url) = (&exchange.method, &exchange.url);
            let response = self
                .build(exchange)?
                .send()
                .await
                .map_err(|e| format!("{method} {url} failed: {}", describe(e)))?;
            let next = match response.status().as_u16() {
                301 | 302 | 303 | 307 | 308 => response
                    .headers()
                    .get(LOCATION)
                    .and_then(|location| url.join(location.to_str().ok()?).ok()),
                _ => None,
            };
            // Another host is another permission and no place for this host's credential; a
            // request that changes something is not repeated elsewhere unasked.
            let follow = next.as_ref().is_some_and(|next| {
                followed < MAX_REDIRECTS
                    && (*method == Method::GET || *method == Method::HEAD)
                    && same_origin(url, next)
            });
            match next {
                Some(mut next) if follow => {
                    followed += 1;
                    next.set_fragment(None);
                    exchange.url = next;
                }
                next => return Ok((response, next)),
            }
        }
    }

    /// The whole call: the text for the model, with the status for the interface.
    async fn exchange(&self, mut exchange: Exchange<'_>, ctx: &ToolCtx) -> Result<(String, u16), String> {
        let (mut response, unfollowed) = self.send(&mut exchange).await?;
        let Exchange {
            method, url, service, ..
        } = &exchange;
        let status = response.status();
        let mut head = format!("HTTP {status} · {method} {url}");
        if let Some((name, _)) = service {
            head.push_str(&format!(" · service {name}"));
        }
        head.push('\n');
        for (name, value) in response.headers() {
            let value = String::from_utf8_lossy(value.as_bytes());
            let value = match service {
                Some(_) if name == SET_COOKIE => Cow::Owned(without_cookie_value(&value)),
                _ => value,
            };
            head.push_str(&format!("{name}: {value}\n"));
        }
        let media_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(media_type);

        let too_large = || {
            format!(
                "the response of {url} is larger than the {} MB limit",
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
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("reading the response of {url} failed: {}", describe(e)))?
        {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(too_large());
            }
            body.extend_from_slice(&chunk);
        }

        let text = match classify(media_type.as_deref(), &body) {
            _ if body.is_empty() => String::new(),
            Kind::Text | Kind::Html => String::from_utf8_lossy(&body).into_owned(),
            Kind::Pdf | Kind::Binary => {
                let what = media_type.as_deref().unwrap_or("binary data");
                save(url, &format!("{what}, not text"), &body, ctx).unwrap_or_else(|why| why)
            }
        };
        let (head, text) = match service {
            Some((_, stored)) => {
                let secrets = stored.secrets();
                let hide = |text: &str| (self.redact)(&services::scrub(text, &secrets)).into_owned();
                (hide(&head), hide(&text))
            }
            None => (head, text),
        };

        let mut out = head;
        if !text.is_empty() {
            out.push('\n');
            match text.char_indices().nth(exchange.limit) {
                None => out.push_str(&text),
                Some((cut, _)) => {
                    out.push_str(&text[..cut]);
                    let saved = ctx.services.get::<ArtifactStore>().and_then(|store| {
                        store
                            .save(&format!("{}-response.txt", ctx.call_id), text.as_bytes())
                            .ok()
                    });
                    let total = text.chars().count();
                    out.push_str(&match saved {
                        Some(path) => format!(
                            "\n[cut at {} of {total} characters; the whole body is in {}]",
                            exchange.limit,
                            path.display()
                        ),
                        None => format!("\n[cut at {} of {total} characters]", exchange.limit),
                    });
                }
            }
        }
        if let Some(next) = unfollowed {
            let next = match service {
                Some((_, stored)) => services::scrub(next.as_str(), &stored.secrets()).into_owned(),
                None => next.to_string(),
            };
            out.push_str(&format!(
                "\n[redirects to {next}; not followed: only a GET or HEAD that stays on the host is]"
            ));
        }
        if let (401, Some((name, _))) = (status.as_u16(), service) {
            out.push_str(&format!(
                "\n[the stored credential of `{name}` was sent and refused]"
            ));
        }
        Ok((out, status.as_u16()))
    }

    async fn request(&self, input: HttpInput, ctx: &ToolCtx) -> ToolOutput {
        let method = input
            .method
            .as_deref()
            .unwrap_or("GET")
            .trim()
            .to_ascii_uppercase();
        let method = match Method::from_bytes(method.as_bytes()) {
            Ok(known) if METHODS.contains(&method.as_str()) => known,
            _ => {
                return ToolOutput::error(format!(
                    "`{method}` is not a method this tool sends; use one of {}",
                    METHODS.join(", ")
                ));
            }
        };
        let body = match (input.body, input.json) {
            (Some(_), Some(_)) => return ToolOutput::error("give `body` or `json`, not both"),
            (Some(text), None) => Some(Body {
                bytes: text.into_bytes(),
                json: false,
            }),
            (None, Some(value)) => Some(Body {
                bytes: value.to_string().into_bytes(),
                json: true,
            }),
            (None, None) => None,
        };
        let mut url = match Url::parse(input.url.trim()) {
            Ok(url) => url,
            Err(e) => {
                return ToolOutput::error(format!(
                    "invalid URL {:?}: {e}; give an absolute http or https URL such as \
                     https://api.example.com/v1/items",
                    input.url
                ));
            }
        };
        url.set_fragment(None);
        let services = self.stored.services();
        let service = match services::pick(&services, &url, input.service.as_deref()) {
            Ok(service) => service,
            Err(why) => return ToolOutput::error(why),
        };
        let exchange = Exchange {
            method,
            url,
            headers: &input.headers,
            body,
            service,
            timeout: Duration::from_secs(
                input
                    .timeout_secs
                    .unwrap_or(TIMEOUT.as_secs())
                    .clamp(1, MAX_TIMEOUT_SECS),
            ),
            limit: input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        };
        let done = tokio::select! {
            _ = ctx.cancel.cancelled() => return ToolOutput::error("aborted by user"),
            done = self.exchange(exchange, ctx) => done,
        };
        let name = service.map(|(name, _)| name);
        match done {
            Ok((text, status)) => {
                let hidden = text.matches(REDACTED_MARK).count();
                let mut details = json!({ "status": status });
                if let Some(name) = name {
                    details["service"] = json!(name);
                }
                if hidden > 0 && name.is_some() {
                    details["redacted"] = json!(hidden);
                }
                ToolOutput::text(text).with_details(details)
            }
            // A failure names the address, which a redirect may have filled with the credential.
            Err(why) => ToolOutput::error(match service {
                Some((_, stored)) => services::scrub(&why, &stored.secrets()).into_owned(),
                None => why,
            }),
        }
    }
}

#[async_trait]
impl Tool for HttpTool {
    fn name(&self) -> &str {
        "http"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "method": {"type": "string", "enum": METHODS, "description": "GET when left out"},
                "url": {"type": "string"},
                "headers": {
                    "type": "object",
                    "additionalProperties": {"type": "string"},
                    "description": "Request headers; a stored credential replaces one of the same name"
                },
                "body": {"type": "string", "description": "The request body as text"},
                "json": {"description": "The request body as a JSON value; sets Content-Type: application/json"},
                "service": {
                    "type": "string",
                    "description": format!(
                        "The stored credential to send when several cover the host; `{NO_SERVICE}` sends none"
                    )
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_LIMIT,
                    "description": "Most characters of the body to return (default 30000)"
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TIMEOUT_SECS,
                    "description": "Seconds the request may take (default 30)"
                }
            },
            "required": ["url"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Net
    }

    /// `domain:<host>`, and `auth:<service>` when the request carries a stored credential.
    fn subjects(&self, input: &Value) -> Vec<String> {
        let url = input.get("url").and_then(Value::as_str);
        let auth = self.service_of(input).map(|(name, _)| format!("auth:{name}"));
        url.and_then(domain_subject).into_iter().chain(auth).collect()
    }

    fn concurrency_safe(&self, input: &Value) -> bool {
        let method = input.get("method").and_then(Value::as_str).unwrap_or("GET");
        ["GET", "HEAD", "OPTIONS"]
            .iter()
            .any(|safe| method.trim().eq_ignore_ascii_case(safe))
    }

    fn approval_note(&self, input: &Value) -> Option<String> {
        self.service_of(input).map(|(name, target)| {
            format!("the request carries the credential stored as `{name}` for {target}")
        })
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        match parse_input(input) {
            Ok(input) => self.request(input, ctx).await,
            Err(error) => error,
        }
    }
}

/// Registers the `http` tool.
pub struct HttpExt {
    /// Where the credentials for agents are stored.
    pub stored: Arc<ServiceStore>,
    /// What hides secrets in the result of a request that carried a stored credential.
    pub redact: Redact,
}

impl Extension for HttpExt {
    fn id(&self) -> &str {
        "http"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        let tool = HttpTool::new(self.stored.clone(), self.redact).map_err(ExtError)?;
        r.tool(Arc::new(tool));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cookie_keeps_its_name_and_attributes_and_loses_its_value() {
        assert_eq!(
            without_cookie_value("sid=abc123; Path=/; HttpOnly"),
            format!("sid={REDACTED}; Path=/; HttpOnly")
        );
        assert_eq!(without_cookie_value("sid=a=b"), format!("sid={REDACTED}"));
    }

    #[test]
    fn a_redirect_is_on_the_host_only_with_the_same_scheme_and_port() {
        let at = |text: &str| Url::parse(text).unwrap();
        assert!(same_origin(
            &at("https://api.test/a"),
            &at("https://api.test:443/b?c")
        ));
        assert!(!same_origin(&at("https://api.test/a"), &at("http://api.test/a")));
        assert!(!same_origin(
            &at("https://api.test/a"),
            &at("https://api.test:8443/a")
        ));
        assert!(!same_origin(&at("https://api.test/a"), &at("https://cdn.test/a")));
    }
}
