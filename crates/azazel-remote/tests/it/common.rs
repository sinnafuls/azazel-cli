use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use azazel_remote::{
    Access, Conn, Folder, FolderKind, Folders, Kind, NewSession, Options, Server, SessionInfo, Sessions,
    StartError,
};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub const WAIT: Duration = Duration::from_secs(10);

/// One session, `s1`: what it is sent comes out of `sent`, and `push` makes it say something.
pub struct Fake {
    sent: mpsc::UnboundedSender<String>,
    open: Mutex<Vec<mpsc::Sender<String>>>,
    /// The sessions it was asked to start.
    pub started: Mutex<Vec<NewSession>>,
}

impl Fake {
    pub fn new() -> (Arc<Fake>, mpsc::UnboundedReceiver<String>) {
        let (sent, received) = mpsc::unbounded_channel();
        let fake = Arc::new(Fake {
            sent,
            open: Mutex::new(Vec::new()),
            started: Mutex::new(Vec::new()),
        });
        (fake, received)
    }

    pub async fn push(&self, line: &str) {
        let open: Vec<_> = self.open.lock().unwrap().clone();
        for frames in open {
            let _ = frames.send(line.to_string()).await;
        }
    }
}

#[async_trait]
impl Sessions for Fake {
    async fn list(&self) -> Vec<SessionInfo> {
        vec![SessionInfo {
            id: "s1".into(),
            name: Some("build".into()),
            kind: Kind::Background,
            state: "idle".into(),
            cwd: "/work".into(),
            cost_usd: Some(0.5),
            mode: Some("manual".into()),
            started: "2026-11-15T10:00:00Z".into(),
        }]
    }

    async fn open(&self, id: &str) -> Result<Conn, String> {
        if id != "s1" {
            return Err(format!("no session {id}"));
        }
        let (frames_tx, frames) = mpsc::channel(64);
        let (commands, mut commands_rx) = mpsc::channel::<String>(64);
        frames_tx
            .send(r#"{"type":"hello","protocol":1,"version":"test"}"#.into())
            .await
            .unwrap();
        self.open.lock().unwrap().push(frames_tx);
        let sent = self.sent.clone();
        tokio::spawn(async move {
            while let Some(line) = commands_rx.recv().await {
                let _ = sent.send(line);
            }
        });
        Ok(Conn { frames, commands })
    }

    fn can_start(&self) -> bool {
        true
    }

    /// `/work` may be started in; `/work` leads to `/work/app`.
    async fn folders(&self, under: Option<&str>) -> Result<Folders, StartError> {
        let folder = |path: &str, kind| Folder {
            path: path.into(),
            name: path.into(),
            kind,
            trusted: true,
            startable: true,
        };
        match under {
            None => Ok(Folders {
                folders: vec![folder("/work", FolderKind::Root)],
                ..Folders::default()
            }),
            Some("/work") => Ok(Folders {
                at: Some("/work".into()),
                parent: None,
                folders: vec![folder("/work/app", FolderKind::Dir)],
                anywhere: false,
            }),
            Some(other) => Err(StartError::Refused(format!("{other} is not open to browsers"))),
        }
    }

    /// Starts `s2` in `/work`; `/missing` does not exist and anything else is refused.
    async fn start(&self, new: NewSession) -> Result<String, StartError> {
        let cwd = new.cwd.clone();
        self.started.lock().unwrap().push(new);
        match cwd.as_str() {
            "/work" => Ok("s2".into()),
            "/missing" => Err(StartError::Invalid("no such folder".into())),
            _ => Err(StartError::Refused("not open to browsers".into())),
        }
    }
}

pub fn options() -> Options {
    Options {
        bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: 0,
        public_url: None,
        link_ttl: Duration::from_secs(900),
        device_ttl: Duration::from_secs(3600),
        redact: true,
        allow_bypass: false,
        full_access: false,
        gated: |_| false,
    }
}

pub async fn server() -> (Server, Arc<Fake>, mpsc::UnboundedReceiver<String>) {
    let (fake, received) = Fake::new();
    let server = Server::start(options(), fake.clone()).await.unwrap();
    (server, fake, received)
}

/// The secret of a link.
pub fn secret(url: &str) -> String {
    url.split_once("#pair=").unwrap().1.to_string()
}

pub struct Answer {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

/// One HTTP/1.1 request on a connection of its own. `Host` is the server's unless given.
pub async fn request(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Answer {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("host")) {
        head.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(WAIT, stream.read_to_end(&mut raw))
        .await
        .unwrap()
        .unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    Answer {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    }
}

/// Pairs with a new link and returns the device's bearer token.
pub async fn pair(server: &Server, access: Access) -> String {
    let link = server.link(access);
    let body = serde_json::json!({ "secret": secret(&link.url), "bearer": true }).to_string();
    let answer = request(server.port(), "POST", "/api/pair", &[("X-Azazel", "1")], &body).await;
    assert_eq!(answer.status, 200, "{}", answer.text());
    answer.json()["token"].as_str().unwrap().to_string()
}

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub async fn connect(port: u16, id: &str, token: Option<&str>) -> Socket {
    let mut request = format!("ws://127.0.0.1:{port}/api/ws/{id}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("origin", format!("http://127.0.0.1:{port}").parse().unwrap());
    if let Some(token) = token {
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

/// The next text message, as JSON.
pub async fn next_json(socket: &mut Socket) -> serde_json::Value {
    loop {
        match tokio::time::timeout(WAIT, socket.next())
            .await
            .expect("a message")
        {
            Some(Ok(Message::Text(text))) => return serde_json::from_str(text.as_str()).unwrap(),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("expected a text message, got {other:?}"),
        }
    }
}

/// The code the server closes with.
pub async fn close_code(socket: &mut Socket) -> u16 {
    loop {
        match tokio::time::timeout(WAIT, socket.next()).await.expect("a close") {
            Some(Ok(Message::Close(Some(frame)))) => return u16::from(frame.code),
            Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("expected a close, got {other:?}"),
        }
    }
}

pub async fn send(socket: &mut Socket, value: serde_json::Value) {
    socket.send(Message::text(value.to_string())).await.unwrap();
}

pub async fn sent(received: &mut mpsc::UnboundedReceiver<String>) -> serde_json::Value {
    let line = tokio::time::timeout(WAIT, received.recv())
        .await
        .expect("a command")
        .unwrap();
    serde_json::from_str(&line).unwrap()
}
