//! The loopback redirect of a browser login (RFC 8252): a listener on 127.0.0.1 that takes the
//! request the browser makes after the user signs in.
//!
//! An authorisation-code login gets its code in the query of that request. An implicit grant
//! puts the token in the address fragment, which a browser never sends: the listener answers
//! the fragment-only request with a page that loads the same address again with the fragment as
//! its query, and reads the second request.

use std::collections::BTreeMap;
use std::io;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::OAuthError;

/// Where the browser is sent back to when a provider does not insist on its own path.
pub const DEFAULT_PATH: &str = "/callback";
/// A request head larger than this is not a redirect.
const MAX_HEAD_BYTES: usize = 16 * 1024;

const DONE_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>azazel</title>\
    <p>Signed in. You can close this tab and go back to the terminal.</p>";
const FAILED_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>azazel</title>\
    <p>The sign-in did not complete. Go back to the terminal for the reason.</p>";
/// Sends the fragment of an implicit grant back as a query, which the listener can read.
const BOUNCE_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>azazel</title>\
    <p id=\"m\">Finishing the sign-in…</p><script>var h=location.hash.slice(1);\
    if(h){location.replace(location.pathname+\"?\"+h)}else{document.getElementById(\"m\")\
    .textContent=\"The sign-in did not complete. Go back to the terminal for the reason.\"}</script>";

/// The parameters a redirect carries.
pub type Params = BTreeMap<String, String>;

pub struct Loopback {
    listener: TcpListener,
    host: String,
    port: u16,
    path: String,
}

/// The address the authorisation server is asked to send the browser to. The host is part of it
/// because some servers allowlist `localhost` and refuse `127.0.0.1`, or the other way round.
pub fn redirect_uri(host: &str, port: u16, path: &str) -> String {
    format!("http://{host}:{port}{path}")
}

fn params_in(query: &str) -> Result<Params, OAuthError> {
    let url = reqwest::Url::parse(&format!("http://127.0.0.1/?{query}"))
        .map_err(|e| OAuthError::Protocol(format!("unreadable redirect: {e}")))?;
    Ok(url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
}

/// The parameters of a redirect, after checking that it answers the login that sent `state` and
/// carries no refusal.
fn checked(params: Params, state: Option<&str>) -> Result<Params, OAuthError> {
    if let Some(code) = params.get("error") {
        return Err(OAuthError::Refused {
            code: code.clone(),
            description: params.get("error_description").cloned().unwrap_or_default(),
        });
    }
    // A server that never echoes `state` is taken at its word: the PKCE verifier, or the
    // one-request listener itself, is what ties the redirect to this login.
    if let Some(expected) = state
        && params.get("state").map(String::as_str) != Some(expected)
    {
        return Err(OAuthError::State);
    }
    Ok(params)
}

/// The authorisation code of a checked redirect.
pub fn code_of(params: &Params) -> Result<String, OAuthError> {
    params
        .get("code")
        .filter(|code| !code.is_empty())
        .cloned()
        .ok_or_else(|| OAuthError::Protocol("the redirect carries no authorisation code".to_string()))
}

/// The parameters of a redirect address the user copied from the browser: the whole address,
/// or its query alone. With `fragment` (an implicit grant) they are read from after the `#`
/// when there is one.
pub fn parse_pasted(pasted: &str, state: Option<&str>, fragment: bool) -> Result<Params, OAuthError> {
    let pasted = pasted.trim();
    let encoded = match (fragment, pasted.split_once('#')) {
        (true, Some((_, after))) => after,
        // A fragment is never part of what the server sent to a code login.
        _ => {
            let before = pasted.split('#').next().unwrap_or_default();
            before.split_once('?').map_or(before, |(_, query)| query)
        }
    };
    checked(params_in(encoded)?, state)
}

/// The code in a redirect address the user copied from the browser.
pub fn parse_redirect(pasted: &str, state: Option<&str>) -> Result<String, OAuthError> {
    code_of(&parse_pasted(pasted, state, false)?)
}

async fn respond(stream: &mut TcpStream, status: &str, page: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// The target of the request line (`/callback?code=…`), or `None` when this is not a `GET`.
async fn request_target(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut chunk = [0; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") && head.len() < MAX_HEAD_BYTES {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..read]);
    }
    let head = String::from_utf8_lossy(&head);
    let mut words = head.lines().next()?.split(' ');
    (words.next()? == "GET").then(|| words.next().map(str::to_string))?
}

impl Loopback {
    /// Listens on the loopback address; port 0 takes a free one. `host` is only what the
    /// redirect address says: the socket is always bound to 127.0.0.1.
    pub async fn bind(host: &str, port: u16, path: &str) -> io::Result<Loopback> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let port = listener.local_addr()?.port();
        Ok(Loopback {
            listener,
            host: host.to_string(),
            port,
            path: path.to_string(),
        })
    }

    pub fn redirect_uri(&self) -> String {
        redirect_uri(&self.host, self.port, &self.path)
    }

    /// Waits for the redirect and returns its authorisation code. Requests for anything else
    /// (a browser asking for an icon) are turned away without ending the wait.
    pub async fn wait(self, state: Option<&str>) -> Result<String, OAuthError> {
        code_of(&self.wait_params(state, false).await?)
    }

    /// Waits for the redirect and returns its parameters. With `implicit`, a request that
    /// carries neither a token nor a refusal is the browser arriving with them in the fragment:
    /// it is answered with the page that sends them again as a query.
    pub async fn wait_params(self, state: Option<&str>, implicit: bool) -> Result<Params, OAuthError> {
        loop {
            let (mut stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|e| OAuthError::Network(e.to_string()))?;
            let Some(target) = request_target(&mut stream).await else {
                respond(&mut stream, "400 Bad Request", FAILED_PAGE).await;
                continue;
            };
            let (path, query) = target.split_once('?').unwrap_or((&target, ""));
            if path != self.path {
                respond(&mut stream, "404 Not Found", FAILED_PAGE).await;
                continue;
            }
            let params = match params_in(query) {
                Ok(params) => params,
                Err(why) => {
                    respond(&mut stream, "400 Bad Request", FAILED_PAGE).await;
                    return Err(why);
                }
            };
            if implicit && !params.contains_key("access_token") && !params.contains_key("error") {
                respond(&mut stream, "200 OK", BOUNCE_PAGE).await;
                continue;
            }
            let outcome = checked(params, state).and_then(|params| match implicit {
                true => Ok(params),
                false => code_of(&params).map(|_| params),
            });
            match &outcome {
                Ok(_) => respond(&mut stream, "200 OK", DONE_PAGE).await,
                Err(_) => respond(&mut stream, "400 Bad Request", FAILED_PAGE).await,
            }
            return outcome;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_pasted_redirect_gives_its_code_only_for_the_right_state() {
        let state = Some("s1");
        let pasted = " http://127.0.0.1:9/callback?code=a%2Fb&state=s1#frag \n";
        assert_eq!(parse_redirect(pasted, state).unwrap(), "a/b");
        assert_eq!(parse_redirect("state=s1&code=xyz", state).unwrap(), "xyz");
        assert_eq!(parse_redirect(pasted, Some("other")), Err(OAuthError::State));
        assert_eq!(parse_redirect("code=xyz", state), Err(OAuthError::State));
        // A login whose server does not echo `state` takes the code as it comes.
        assert_eq!(parse_redirect("code=xyz", None).unwrap(), "xyz");
        assert!(matches!(
            parse_redirect("state=s1", state),
            Err(OAuthError::Protocol(_))
        ));
        assert_eq!(
            parse_redirect("error=access_denied&error_description=No+thanks&state=s1", state),
            Err(OAuthError::Refused {
                code: "access_denied".into(),
                description: "No thanks".into()
            })
        );
    }

    #[test]
    fn a_pasted_implicit_grant_is_read_from_its_fragment() {
        let pasted = "http://localhost:9/auth/callback#access_token=t%2B1&expires_in=60&state=s1";
        let params = parse_pasted(pasted, Some("s1"), true).unwrap();
        assert_eq!(params["access_token"], "t+1");
        assert_eq!(params["expires_in"], "60");
        assert_eq!(
            parse_pasted(pasted, Some("s2"), true),
            Err(OAuthError::State)
        );
        // Without a fragment the query is read, as for a code.
        assert_eq!(
            parse_pasted("access_token=t&state=s1", Some("s1"), true).unwrap()["access_token"],
            "t"
        );
    }
}
