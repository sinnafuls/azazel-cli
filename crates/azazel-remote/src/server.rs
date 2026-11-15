//! The server: binding, the accept loop, and what every request handler shares.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::auth::Auth;
use crate::limits::{Clock, Connections, SystemClock};
use crate::{Access, Link, Options, Sessions};

/// How many ports from the configured one are tried when it is taken.
const PORT_TRIES: u16 = 10;

/// The address of a tunnel or proxy in front of the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Public {
    /// `https://host[:port]`
    pub origin: String,
    /// `host[:port]`, as a `Host` header has it.
    pub host: String,
    pub https: bool,
}

pub(crate) fn parse_public(url: &str) -> Result<Public, String> {
    let bad = || format!("`{url}` is not an address such as https://remote.example.com");
    let (scheme, rest) = url.trim().split_once("://").ok_or_else(bad)?;
    let scheme = scheme.to_ascii_lowercase();
    let https = match scheme.as_str() {
        "https" => true,
        "http" => false,
        _ => return Err(bad()),
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if host.is_empty() || host.contains('@') || host.contains(char::is_whitespace) {
        return Err(bad());
    }
    Ok(Public {
        origin: format!("{scheme}://{host}"),
        host,
        https,
    })
}

/// What the handlers of requests and sockets share.
pub(crate) struct Shared {
    pub auth: Auth,
    pub sessions: Arc<dyn Sessions>,
    pub allow_bypass: bool,
    pub port: u16,
    pub bind: IpAddr,
    pub public: RwLock<Option<Public>>,
    pub connections: Arc<Mutex<Connections>>,
    pub cancel: CancellationToken,
}

impl Shared {
    /// The hosts this machine is reached by directly.
    fn local_hosts(&self) -> Vec<String> {
        let mut hosts = vec![
            format!("127.0.0.1:{}", self.port),
            format!("localhost:{}", self.port),
            format!("[::1]:{}", self.port),
        ];
        if !self.bind.is_unspecified() {
            hosts.push(SocketAddr::new(self.bind, self.port).to_string());
        }
        hosts
    }

    fn public(&self) -> Option<Public> {
        self.public.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether a `Host` header names this server. Anything else may be a page of another site
    /// that was made to resolve to this machine.
    pub fn host_ok(&self, host: &str) -> bool {
        let host = host.trim().to_ascii_lowercase();
        self.local_hosts().contains(&host) || self.public().is_some_and(|public| public.host == host)
    }

    /// Whether a request to `host` came through an https address (the cookie is then `Secure`).
    pub fn is_https_host(&self, host: &str) -> bool {
        self.public()
            .is_some_and(|public| public.https && public.host == host)
    }

    /// Whether an `Origin` header is one of this server's own pages.
    pub fn origin_ok(&self, origin: &str) -> bool {
        let origin = origin.trim().to_ascii_lowercase();
        self.local_hosts()
            .iter()
            .any(|host| origin == format!("http://{host}"))
            || self.public().is_some_and(|public| public.origin == origin)
    }

    /// Where the page of this server is: the tunnel when there is one.
    pub fn origin(&self) -> String {
        match self.public() {
            Some(public) => public.origin,
            None => format!("http://{}", SocketAddr::new(self.bind, self.port)),
        }
    }

    pub fn link(&self, access: Access, origin: &str) -> Link {
        let (secret, expires) = self.auth.mint_link(access);
        Link {
            url: format!("{origin}/#pair={secret}"),
            access,
            expires: rfc3339(expires),
        }
    }
}

pub(crate) fn rfc3339(time: SystemTime) -> String {
    jiff::Timestamp::try_from(time)
        .map(|timestamp| timestamp.to_string())
        .unwrap_or_default()
}

pub struct Server {
    shared: Arc<Shared>,
    addr: SocketAddr,
    task: JoinHandle<()>,
}

impl Server {
    /// Binds the port of `options` (or the next nine when it is taken) and starts serving.
    pub async fn start(options: Options, sessions: Arc<dyn Sessions>) -> io::Result<Server> {
        Server::start_with_clock(options, sessions, Arc::new(SystemClock)).await
    }

    pub(crate) async fn start_with_clock(
        options: Options,
        sessions: Arc<dyn Sessions>,
        clock: Arc<dyn Clock>,
    ) -> io::Result<Server> {
        let public = match options.public_url.as_deref().filter(|url| !url.trim().is_empty()) {
            Some(url) => Some(parse_public(url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?),
            None => None,
        };
        let tries = if options.port == 0 { 1 } else { PORT_TRIES };
        let mut last = None;
        let mut bound = None;
        for step in 0..tries {
            let Some(port) = options.port.checked_add(step) else {
                break;
            };
            match TcpListener::bind((options.bind, port)).await {
                Ok(listener) => {
                    bound = Some(listener);
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        let Some(listener) = bound else {
            return Err(last.unwrap_or_else(|| io::Error::other("no port to listen on")));
        };
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            auth: Auth::new(clock, options.link_ttl, options.device_ttl),
            sessions,
            allow_bypass: options.allow_bypass,
            port: addr.port(),
            bind: options.bind,
            public: RwLock::new(public),
            connections: Arc::new(Mutex::new(Connections::default())),
            cancel: CancellationToken::new(),
        });
        let task = tokio::spawn(accept(listener, shared.clone()));
        Ok(Server { shared, addr, task })
    }

    /// The port it listens on, which is not the one asked for when that was taken or 0.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The addresses it can be reached at: the local one, and the public one when set.
    pub fn urls(&self) -> Vec<String> {
        let mut urls = vec![format!("http://{}", self.addr)];
        if let Some(public) = self.shared.public() {
            urls.push(public.origin);
        }
        urls
    }

    /// Accepts the address of a tunnel or proxy that now leads here: links point to it, and its
    /// host and origin are accepted from now on.
    pub fn set_public_url(&self, url: &str) -> Result<(), String> {
        let public = parse_public(url)?;
        *self.shared.public.write().unwrap_or_else(|e| e.into_inner()) = Some(public);
        Ok(())
    }

    /// A new single-use link, for the public address when there is one.
    pub fn link(&self, access: Access) -> Link {
        self.shared.link(access, &self.shared.origin())
    }

    /// Signs every device out and voids every link.
    pub fn revoke_all(&self) {
        self.shared.auth.revoke_all();
    }

    /// Stops accepting, closes every socket and waits until that is done.
    pub async fn shutdown(self) {
        self.shared.cancel.cancel();
        let _ = self.task.await;
    }
}

async fn accept(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let accepted = tokio::select! {
            _ = shared.cancel.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, peer)) = accepted else {
            // A connection that was reset before it was accepted, or no file handles left: try again.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let cancel = shared.cancel.clone();
            let peer = peer.ip();
            let service = service_fn(move |req| {
                let shared = shared.clone();
                async move { Ok::<_, std::convert::Infallible>(crate::http::handle(shared, peer, req).await) }
            });
            let connection = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(std::time::Duration::from_secs(10))
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades();
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = connection => {}
            }
        });
    }
}
