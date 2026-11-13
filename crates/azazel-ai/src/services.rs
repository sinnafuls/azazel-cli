//! Credentials for the services agents call and the sites they sign in to: kept in
//! `secrets.json`, a file of their own beside `auth.json`, added to a request or typed into a
//! page for the agent, and never given to it.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use base64::Engine;
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::credentials::{open_lock, write_private};

/// What stands in for a stored credential in text an agent gets to see.
pub const REDACTED: &str = "[redacted:credential]";

/// The service an agent names to send a request without a stored credential.
pub const NO_SERVICE: &str = "none";

/// A part of a credential shorter than this is not looked for in text: replacing it would
/// mangle the text without hiding anything worth hiding.
const MIN_SECRET: usize = 4;

/// One entry of `secrets.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Service {
    /// A header added to every request to `host`, such as `Authorization: Bearer …`.
    Header {
        host: String,
        name: String,
        value: String,
    },
    /// A query parameter added to every request to `host`.
    Query {
        host: String,
        name: String,
        value: String,
    },
    /// HTTP basic authentication for every request to `host`.
    Basic {
        host: String,
        username: String,
        password: String,
    },
    /// The login typed into the sign-in page of a site: `url` is the pattern its address has to
    /// match.
    Site {
        url: String,
        username: String,
        password: String,
    },
}

/// A host as a service names it: `api.example.com`, `*.example.com` (its subdomains) or either
/// with a port.
struct HostPattern<'a> {
    subdomains: bool,
    host: &'a str,
    port: Option<u16>,
}

impl HostPattern<'_> {
    /// Whether `url` goes to this host. A pattern without a port covers the scheme's own port
    /// only, so a credential never reaches another program on the same machine.
    fn matches(&self, url: &Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        let named = if self.subdomains {
            host.strip_suffix(self.host)
                .is_some_and(|rest| rest.ends_with('.'))
        } else {
            host == self.host
        };
        named
            && match self.port {
                Some(port) => url.port_or_known_default() == Some(port),
                None => url.port().is_none(),
            }
    }
}

fn host_pattern(text: &str) -> Result<HostPattern<'_>, String> {
    let bad = || {
        format!(
            "`{text}` is not a host: write it like `api.example.com`, `*.example.com` or `localhost:8080`"
        )
    };
    let (subdomains, rest) = match text.strip_prefix("*.") {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (host, port) = if rest.starts_with('[') {
        let (host, after) = rest.split_at(rest.find(']').ok_or_else(bad)? + 1);
        match after.strip_prefix(':') {
            Some(port) => (host, Some(port)),
            None if after.is_empty() => (host, None),
            None => return Err(bad()),
        }
    } else {
        match rest.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        }
    };
    let port = port.map(|port| port.parse().map_err(|_| bad())).transpose()?;
    let bracketed = host.starts_with('[');
    let plain = !host.is_empty()
        && host.chars().all(|c| match c {
            '[' | ']' | ':' => bracketed,
            c => c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '_'),
        });
    if !plain || (subdomains && bracketed) {
        return Err(bad());
    }
    Ok(HostPattern {
        subdomains,
        host,
        port,
    })
}

/// Whether `host` (as a URL writes it) is this machine.
fn is_loopback(host: &str) -> bool {
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    match literal.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host == "localhost" || host.ends_with(".localhost"),
    }
}

/// Whether a credential may travel to `url`: over TLS, or to this machine.
fn is_private(url: &Url) -> bool {
    url.scheme() == "https" || url.host_str().is_some_and(is_loopback)
}

/// The address pattern of a sign-in page: scheme, host and a path in which `*` stands for any
/// run of characters. No path, or `/`, covers every page of the site.
struct SitePattern<'a> {
    scheme: &'a str,
    host: HostPattern<'a>,
    path: &'a str,
}

impl SitePattern<'_> {
    fn matches(&self, page: &Url) -> bool {
        page.scheme() == self.scheme
            && self.host.matches(page)
            && (matches!(self.path, "" | "/") || wildcard(self.path, page.path()))
    }
}

fn site_pattern(text: &str) -> Result<SitePattern<'_>, String> {
    let (scheme, rest) = text
        .split_once("://")
        .filter(|(scheme, _)| matches!(*scheme, "http" | "https"))
        .ok_or_else(|| {
            format!(
                "`{text}` is not the address of a page: write it like `https://example.com/login*`, where \
                 `*` stands for any run of characters in the path"
            )
        })?;
    if rest.contains(['?', '#']) {
        return Err(format!(
            "`{text}`: leave the query and the fragment out; the pattern covers the path only"
        ));
    }
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let host = host_pattern(host)?;
    if scheme == "http" && !is_loopback(host.host) {
        return Err(format!(
            "`{text}`: a login is only typed into an https page, or into a page served by this machine"
        ));
    }
    Ok(SitePattern { scheme, host, path })
}

/// Whether `text` is `pattern`, in which `*` stands for any run of characters.
fn wildcard(pattern: &str, text: &str) -> bool {
    let (pattern, text) = (pattern.as_bytes(), text.as_bytes());
    let (mut p, mut t) = (0, 0);
    // The last `*` and how much of the text it has taken so far.
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if pattern.get(p) == Some(&b'*') {
            star = Some((p, t));
            p += 1;
        } else if pattern.get(p) == Some(&text[t]) {
            p += 1;
            t += 1;
        } else if let Some((at, taken)) = star {
            star = Some((at, taken + 1));
            p = at + 1;
            t = taken + 1;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|byte| *byte == b'*')
}

/// `value` as it stands in a URL's query.
fn in_query(value: &str) -> String {
    Url::parse_with_params("http://x/", [("k", value)])
        .ok()
        .and_then(|url| url.query()?.strip_prefix("k=").map(str::to_string))
        .unwrap_or_else(|| value.to_string())
}

/// Whether `name` can name a service: letters, digits, `-`, `_` and `.`, and not `none`.
pub fn check_name(name: &str) -> Result<(), String> {
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    match (plain, name) {
        (false, _) => Err(format!(
            "`{name}` cannot name a service: use letters, digits, `-`, `_` and `.`"
        )),
        (true, NO_SERVICE) => Err(format!(
            "`{NO_SERVICE}` is what an agent says to send no credential; choose another name"
        )),
        (true, _) => Ok(()),
    }
}

fn normal_host(host: &str) -> Result<String, String> {
    let host = host.trim().to_ascii_lowercase();
    host_pattern(&host)?;
    Ok(host)
}

/// The two halves of `text` around the first `separator`, trimmed and both with something in
/// them.
fn halves<'a>(text: &'a str, separator: char, shape: &str) -> Result<(&'a str, &'a str), String> {
    text.split_once(separator)
        .map(|(left, right)| (left.trim(), right.trim()))
        .filter(|(left, right)| !left.is_empty() && !right.is_empty())
        .ok_or_else(|| format!("write it as `{shape}`"))
}

impl Service {
    /// A header for requests to `host`, from `Name: value`.
    pub fn header(host: &str, header: &str) -> Result<Service, String> {
        let (name, value) = halves(header, ':', "Name: value")?;
        let token = name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
        if !token || value.chars().any(char::is_control) {
            return Err(format!("`{name}` cannot be sent as a header"));
        }
        Ok(Service::Header {
            host: normal_host(host)?,
            name: name.to_string(),
            value: value.to_string(),
        })
    }

    /// A query parameter for requests to `host`, from `name=value`.
    pub fn query(host: &str, parameter: &str) -> Result<Service, String> {
        let (name, value) = halves(parameter, '=', "name=value")?;
        Ok(Service::Query {
            host: normal_host(host)?,
            name: name.to_string(),
            value: value.to_string(),
        })
    }

    /// Basic authentication for requests to `host`, from `user:password`.
    pub fn basic(host: &str, login: &str) -> Result<Service, String> {
        let (username, password) = halves(login, ':', "user:password")?;
        Ok(Service::Basic {
            host: normal_host(host)?,
            username: username.to_string(),
            password: password.to_string(),
        })
    }

    /// The login of the pages whose address matches `pattern`.
    pub fn site(pattern: &str, username: &str, password: &str) -> Result<Service, String> {
        if username.is_empty() || password.is_empty() {
            return Err("a login needs a user name and a password".to_string());
        }
        let pattern = pattern.trim();
        // Scheme and host compare in lower case; the path is kept as written.
        let path = pattern
            .find("://")
            .and_then(|at| Some(at + 3 + pattern[at + 3..].find('/')?))
            .unwrap_or(pattern.len());
        let url = pattern[..path].to_ascii_lowercase() + &pattern[path..];
        site_pattern(&url)?;
        Ok(Service::Site {
            url,
            username: username.to_string(),
            password: password.to_string(),
        })
    }

    /// The host a request credential is for, or the page pattern of a login.
    pub fn target(&self) -> &str {
        match self {
            Service::Header { host, .. } | Service::Query { host, .. } | Service::Basic { host, .. } => host,
            Service::Site { url, .. } => url,
        }
    }

    /// What the entry is, without anything secret: where it applies and how.
    pub fn describe(&self) -> String {
        match self {
            Service::Header { host, name, .. } => format!("{host} · header {name}"),
            Service::Query { host, name, .. } => format!("{host} · query parameter {name}"),
            Service::Basic { host, username, .. } => format!("{host} · basic authentication as {username}"),
            Service::Site { url, username, .. } => format!("{url} · login as {username}"),
        }
    }

    /// Whether this is a request credential for the host of `url`.
    pub fn covers(&self, url: &Url) -> bool {
        match self {
            Service::Header { host, .. } | Service::Query { host, .. } | Service::Basic { host, .. } => {
                host_pattern(host).is_ok_and(|pattern| pattern.matches(url))
            }
            Service::Site { .. } => false,
        }
    }

    /// Whether this is the login of the page at `page`.
    pub fn signs_in(&self, page: &Url) -> bool {
        match self {
            Service::Site { url, .. } => site_pattern(url).is_ok_and(|pattern| pattern.matches(page)),
            _ => false,
        }
    }

    /// The strings that give the credential away, in the forms a server or a page may echo
    /// them, longest first.
    pub fn secrets(&self) -> Vec<String> {
        let mut found = Vec::new();
        match self {
            Service::Header { value, .. } => {
                found.push(value.clone());
                // `Bearer <token>`: the token alone is the credential.
                if let Some((_, token)) = value.split_once(char::is_whitespace) {
                    found.push(token.trim().to_string());
                }
            }
            Service::Query { value, .. } => {
                found.push(value.clone());
                found.push(in_query(value));
            }
            Service::Basic {
                username, password, ..
            } => {
                found.push(password.clone());
                found
                    .push(base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}")));
            }
            Service::Site { password, .. } => found.push(password.clone()),
        }
        found.retain(|secret| secret.len() >= MIN_SECRET);
        found.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        found.dedup();
        found
    }
}

/// The file the credentials for agents are kept in: a JSON object of [`Service`] entries by
/// name, written like `auth.json` (temp file and rename under a lock file, mode 0600 on Unix)
/// so several processes can share it.
pub struct ServiceStore {
    file: PathBuf,
}

impl ServiceStore {
    pub fn new(file: PathBuf) -> Self {
        ServiceStore { file }
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    /// What the file holds; `Ok(None)` when there is no file.
    fn read(&self) -> io::Result<Option<BTreeMap<String, Service>>> {
        let text = match std::fs::read_to_string(&self.file) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// The stored credentials by name; empty when there are none or the file cannot be read.
    pub fn services(&self) -> BTreeMap<String, Service> {
        match self.read() {
            Ok(stored) => stored.unwrap_or_default(),
            Err(e) => {
                tracing::warn!("ignoring {}: {e}", self.file.display());
                BTreeMap::new()
            }
        }
    }

    /// Keeps `service` under `name`. Returns whether it took the place of another one.
    pub fn store_service(&self, name: &str, service: Service) -> io::Result<bool> {
        let mut replaced = false;
        self.modify(|services| replaced = services.insert(name.to_string(), service).is_some())?;
        Ok(replaced)
    }

    /// Forgets the service `name` and returns it when there was one.
    pub fn remove_service(&self, name: &str) -> io::Result<Option<Service>> {
        let mut removed = None;
        self.modify(|services| removed = services.remove(name))?;
        Ok(removed)
    }

    /// Read-modify-write under the advisory lock, so concurrent processes never lose an update.
    /// A file that cannot be read is left as it is: it may be a newer version's.
    fn modify(&self, f: impl FnOnce(&mut BTreeMap<String, Service>)) -> io::Result<()> {
        let lock = open_lock(&self.file)?;
        let result = self.read().and_then(|stored| {
            let mut services = stored.unwrap_or_default();
            f(&mut services);
            write_private(&self.file, &serde_json::to_vec_pretty(&services)?)
        });
        lock.unlock()?;
        result
    }
}

/// `text` with every one of `secrets` replaced by [`REDACTED`]; the text itself when it holds
/// none.
pub fn scrub<'a>(text: &'a str, secrets: &[String]) -> Cow<'a, str> {
    let mut out = Cow::Borrowed(text);
    for secret in secrets {
        if out.contains(secret.as_str()) {
            out = Cow::Owned(out.replace(secret.as_str(), REDACTED));
        }
    }
    out
}

/// The request credential for `url`: the service `named`, else the one stored for the host most
/// exactly (a host over a `*.` pattern, then the longer pattern, then the first name). `Ok(None)`
/// when no service covers the host or the agent named [`NO_SERVICE`]. An error when the named
/// service is not for this host, or when the credential would travel in the clear.
pub fn pick<'a>(
    services: &'a BTreeMap<String, Service>,
    url: &Url,
    named: Option<&str>,
) -> Result<Option<(&'a str, &'a Service)>, String> {
    let host = url.host_str().unwrap_or_default();
    let chosen = match named {
        Some(NO_SERVICE) => return Ok(None),
        Some(name) => {
            let Some((name, service)) = services
                .get_key_value(name)
                .filter(|(_, service)| !matches!(service, Service::Site { .. }))
            else {
                let known: Vec<&str> = services
                    .iter()
                    .filter(|(_, service)| !matches!(service, Service::Site { .. }))
                    .map(|(name, _)| name.as_str())
                    .collect();
                return Err(match known.as_slice() {
                    [] => format!("there is no service `{name}`: no service is stored"),
                    known => format!("there is no service `{name}`; stored: {}", known.join(", ")),
                });
            };
            if !service.covers(url) {
                return Err(format!(
                    "the credential of `{name}` is stored for {}, and is not sent to {host}",
                    service.target()
                ));
            }
            Some((name.as_str(), service))
        }
        None => services
            .iter()
            .filter(|(_, service)| service.covers(url))
            // Later names lose a tie, so the first name wins.
            .rev()
            .max_by_key(|(_, service)| (!service.target().starts_with("*."), service.target().len()))
            .map(|(name, service)| (name.as_str(), service)),
    };
    match chosen {
        Some((name, _)) if !is_private(url) => Err(format!(
            "`{name}` is stored for {host}, and a credential is only sent over https; use https, or \
             `service: \"{NO_SERVICE}\"` to send the request without it"
        )),
        chosen => Ok(chosen),
    }
}

/// The stored login of the page at the address `page`: the one whose pattern is the longest.
pub fn login_for<'a>(services: &'a BTreeMap<String, Service>, page: &str) -> Option<(&'a str, &'a Service)> {
    let page = Url::parse(page).ok()?;
    services
        .iter()
        .filter(|(_, service)| service.signs_in(&page))
        .rev()
        .max_by_key(|(_, service)| service.target().len())
        .map(|(name, service)| (name.as_str(), service))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn stored(entries: &[(&str, Service)]) -> BTreeMap<String, Service> {
        entries
            .iter()
            .map(|(name, service)| (name.to_string(), service.clone()))
            .collect()
    }

    #[test]
    fn a_host_pattern_covers_its_host_its_subdomains_and_only_the_named_port() {
        let api = Service::header("API.Example.com", "Authorization: Bearer abc123").unwrap();
        assert_eq!(api.target(), "api.example.com");
        assert!(api.covers(&url("https://api.example.com/v1/users?page=2")));
        assert!(api.covers(&url("https://API.EXAMPLE.COM:443/")));
        for other in [
            "https://example.com/",
            "https://v2.api.example.com/",
            "https://evilapi.example.com/",
            "https://api.example.com.evil.test/",
            "https://api.example.com:8443/",
        ] {
            assert!(!api.covers(&url(other)), "{other}");
        }

        let any = Service::query("*.example.com", "key=abc123").unwrap();
        assert!(any.covers(&url("https://api.example.com/")));
        assert!(any.covers(&url("https://a.b.example.com/")));
        assert!(!any.covers(&url("https://example.com/")));
        assert!(!any.covers(&url("https://badexample.com/")));

        let local = Service::basic("localhost:8080", "me:hunter22").unwrap();
        assert!(local.covers(&url("http://localhost:8080/x")));
        assert!(!local.covers(&url("http://localhost/x")));
        assert!(!local.covers(&url("http://localhost:8081/x")));
        let six = Service::basic("[::1]:9000", "me:hunter22").unwrap();
        assert!(six.covers(&url("http://[::1]:9000/")));

        for bad in [
            "",
            "https://api.example.com",
            "api.example.com/v1",
            "api.*.com",
            "a b",
            "host:port",
            "a:b:1",
            "*.[::1]",
        ] {
            assert!(Service::header(bad, "X-Key: abc123").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn what_the_user_writes_is_split_into_its_parts_or_refused() {
        assert_eq!(
            Service::header("h.test", " Authorization :  Bearer a:b c ").unwrap(),
            Service::Header {
                host: "h.test".into(),
                name: "Authorization".into(),
                value: "Bearer a:b c".into(),
            }
        );
        assert_eq!(
            Service::query("h.test", "api_key=a=b").unwrap(),
            Service::Query {
                host: "h.test".into(),
                name: "api_key".into(),
                value: "a=b".into(),
            }
        );
        assert_eq!(
            Service::basic("h.test", "me:pa:ss").unwrap(),
            Service::Basic {
                host: "h.test".into(),
                username: "me".into(),
                password: "pa:ss".into(),
            }
        );
        for bad in ["no colon", ": value", "Name:", "Bad Name: x", "X: a\nb"] {
            assert!(Service::header("h.test", bad).is_err(), "{bad:?}");
        }
        assert!(Service::query("h.test", "novalue").is_err());
        assert!(Service::basic("h.test", ":password").is_err());

        assert!(check_name("github-api.v2").is_ok());
        assert!(check_name("none").is_err());
        assert!(check_name("two words").is_err());
        assert!(check_name("").is_err());
    }

    #[test]
    fn the_most_exact_service_is_picked_and_a_named_one_only_for_its_own_host() {
        let services = stored(&[
            (
                "everything",
                Service::header("*.example.com", "X-Key: wide-key").unwrap(),
            ),
            (
                "work",
                Service::header("api.example.com", "X-Key: work-key").unwrap(),
            ),
            (
                "personal",
                Service::header("api.example.com", "X-Key: personal-key").unwrap(),
            ),
            (
                "site",
                Service::site("https://api.example.com/login", "me", "hunter22").unwrap(),
            ),
        ]);
        let name = |address: &str, named: Option<&str>| {
            pick(&services, &url(address), named).map(|found| found.map(|(name, _)| name))
        };
        // Of two services for the same host the first name is the default.
        assert_eq!(name("https://api.example.com/x", None), Ok(Some("personal")));
        assert_eq!(name("https://api.example.com/x", Some("work")), Ok(Some("work")));
        assert_eq!(name("https://cdn.example.com/x", None), Ok(Some("everything")));
        assert_eq!(name("https://other.test/x", None), Ok(None));
        assert_eq!(name("https://api.example.com/x", Some("none")), Ok(None));

        // A credential is never sent to a host it was not stored for, whatever the agent names.
        let elsewhere = name("https://other.test/x", Some("work")).unwrap_err();
        assert!(elsewhere.contains("not sent to other.test"), "{elsewhere}");
        let unknown = name("https://api.example.com/x", Some("nope")).unwrap_err();
        assert!(
            unknown.contains("everything, personal, work") && !unknown.contains("site"),
            "{unknown}"
        );
        // A site login is not a request credential.
        assert!(name("https://api.example.com/login", Some("site")).is_err());

        // Nor in the clear, except to this machine.
        let clear = name("http://api.example.com/x", None).unwrap_err();
        assert!(clear.contains("only sent over https"), "{clear}");
        assert_eq!(name("http://api.example.com/x", Some("none")), Ok(None));
        let local = stored(&[("dev", Service::basic("127.0.0.1:3000", "me:hunter22").unwrap())]);
        assert!(
            pick(&local, &url("http://127.0.0.1:3000/api"), None)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_login_is_for_the_pages_its_pattern_names_on_its_own_site() {
        let login = Service::site("HTTPS://Example.com/Login*", "me", "hunter22").unwrap();
        assert_eq!(login.target(), "https://example.com/Login*");
        assert!(login.signs_in(&url("https://example.com/Login")));
        assert!(login.signs_in(&url("https://example.com/Login/sso?next=%2F#form")));
        for other in [
            "https://example.com/login",
            "https://example.com/",
            "http://example.com/Login",
            "https://example.com.evil.test/Login",
            "https://evil.test/https://example.com/Login",
            "https://example.com:8443/Login",
        ] {
            assert!(!login.signs_in(&url(other)), "{other}");
        }
        // Without a path the whole site is covered.
        let site = Service::site("https://*.example.com", "me", "hunter22").unwrap();
        assert!(site.signs_in(&url("https://accounts.example.com/any/page")));
        assert!(!site.signs_in(&url("https://example.com/")));
        let middle = Service::site("https://example.com/*/signin", "me", "hunter22").unwrap();
        assert!(middle.signs_in(&url("https://example.com/en/us/signin")));
        assert!(!middle.signs_in(&url("https://example.com/en/signin/help")));

        for bad in [
            "example.com/login",
            "ftp://example.com/",
            "http://example.com/login",
            "https://example.com/login?next=*",
            "https://*/login",
        ] {
            assert!(Service::site(bad, "me", "hunter22").is_err(), "{bad}");
        }
        assert!(Service::site("http://localhost:3000/login", "me", "hunter22").is_ok());
        assert!(Service::site("https://example.com/", "", "hunter22").is_err());

        let services = stored(&[
            (
                "a-site",
                Service::site("https://example.com", "any", "any-password").unwrap(),
            ),
            (
                "b-admin",
                Service::site("https://example.com/admin/*", "root", "root-password").unwrap(),
            ),
        ]);
        let found = |address: &str| login_for(&services, address).map(|(name, _)| name);
        assert_eq!(found("https://example.com/admin/login"), Some("b-admin"));
        assert_eq!(found("https://example.com/shop"), Some("a-site"));
        assert_eq!(found("https://elsewhere.test/admin/login"), None);
        assert_eq!(found("about:blank"), None);
    }

    #[test]
    fn every_form_of_a_credential_is_taken_out_of_text() {
        let header = Service::header("h.test", "Authorization: Bearer tok_12345").unwrap();
        assert_eq!(header.secrets(), ["Bearer tok_12345", "tok_12345"]);
        assert_eq!(
            scrub("sent `Bearer tok_12345`; token=tok_12345.", &header.secrets()),
            format!("sent `{REDACTED}`; token={REDACTED}.")
        );

        let query = Service::query("h.test", "key=a b&c").unwrap();
        assert_eq!(
            scrub("https://h.test/?key=a+b%26c and a b&c", &query.secrets()),
            format!("https://h.test/?key={REDACTED} and {REDACTED}")
        );

        let basic = Service::basic("h.test", "me:hunter22").unwrap();
        assert_eq!(
            scrub("Basic bWU6aHVudGVyMjI= is me:hunter22", &basic.secrets()),
            format!("Basic {REDACTED} is me:{REDACTED}")
        );

        // Text without the credential is handed back as it is.
        let clean = "nothing to hide";
        assert!(matches!(scrub(clean, &basic.secrets()), Cow::Borrowed(text) if text == clean));
        // A part too short to be a secret is left alone rather than cut out of every word.
        let short = Service::header("h.test", "X-Mode: on").unwrap();
        assert!(short.secrets().is_empty());
    }

    #[test]
    fn entries_are_written_with_their_kind() {
        let service = Service::header("h.test", "X-Key: abc123").unwrap();
        let json = serde_json::to_value(&service).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type": "header", "host": "h.test", "name": "X-Key", "value": "abc123"})
        );
        assert_eq!(serde_json::from_value::<Service>(json).unwrap(), service);
        let site: Service = serde_json::from_value(serde_json::json!({
            "type": "site", "url": "https://example.com/login", "username": "me", "password": "pw"
        }))
        .unwrap();
        assert!(site.signs_in(&url("https://example.com/login")));
    }

    #[test]
    fn the_store_keeps_entries_by_name_in_a_file_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let store = ServiceStore::new(dir.path().join("home").join("secrets.json"));
        // No file yet: nothing is stored, and nothing is created by looking.
        assert!(store.services().is_empty());
        assert!(!store.file().exists());

        let api = Service::header("api.acme.test", "Authorization: Bearer tok_12345").unwrap();
        let login = Service::site("https://acme.test/login", "me", "hunter22").unwrap();
        assert!(!store.store_service("acme-api", api.clone()).unwrap());
        assert!(!store.store_service("acme.test", login.clone()).unwrap());
        assert_eq!(
            store.services(),
            BTreeMap::from([
                ("acme-api".to_string(), api),
                ("acme.test".to_string(), login.clone())
            ])
        );

        // A service under a name in use takes its place.
        let rotated = Service::header("api.acme.test", "Authorization: Bearer tok_67890").unwrap();
        assert!(store.store_service("acme-api", rotated.clone()).unwrap());
        assert_eq!(store.remove_service("acme-api").unwrap(), Some(rotated));
        assert_eq!(store.remove_service("acme-api").unwrap(), None);
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(store.file()).unwrap()).unwrap();
        assert_eq!(
            file,
            serde_json::json!({"acme.test": {
                "type": "site", "url": "https://acme.test/login", "username": "me", "password": "hunter22"
            }})
        );
        // Nothing but the file itself and its lock is left beside it.
        let mut beside: Vec<String> = std::fs::read_dir(store.file().parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        beside.sort();
        assert_eq!(beside, ["secrets.json", "secrets.json.lock"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.file()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_file_that_cannot_be_read_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = ServiceStore::new(dir.path().join("secrets.json"));
        let written = r#"{"x": {"type": "from-the-future"}}"#;
        std::fs::write(store.file(), written).unwrap();
        assert!(store.services().is_empty());
        let service = Service::query("api.acme.test", "key=abc123").unwrap();
        assert!(store.store_service("acme", service).is_err());
        assert!(store.remove_service("x").is_err());
        assert_eq!(std::fs::read_to_string(store.file()).unwrap(), written);
    }
}
