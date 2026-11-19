#![cfg(feature = "remote")]

//! `azazel remote`: the web server in front of background sessions, against the real binary, the
//! scripted provider and stand-ins for `cloudflared` and `tailscale`. The client is a WebSocket
//! client with a Bearer token, as a program would be; pairing goes through an HTTP client.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::LazyLock;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::common::{fixture_program, lines_of, script_fixture};

const LIMIT: Duration = Duration::from_secs(60);

static CLOUDFLARED: LazyLock<PathBuf> = LazyLock::new(|| fixture_program("fake-cloudflared"));
static TAILSCALE: LazyLock<PathBuf> = LazyLock::new(|| fixture_program("fake-tailscale"));

/// A home and a project of its own. Background sessions still running when it goes are stopped.
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::copy(script_fixture("web_session.json"), dir.path().join("script.json")).unwrap();
        Sandbox { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn azazel(&self, args: &[&str]) -> Command {
        self.azazel_in(&self.dir.path().join("project"), args)
    }

    /// `azazel` working in `cwd`.
    fn azazel_in(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("azazel"));
        command
            .env("AZAZEL_HOME", self.home())
            .env("AZAZEL_MOCK_SCRIPT", self.dir.path().join("script.json"))
            .args(["--set", "session.auto_name=false", "--no-memory"])
            .args(["--set", "notify.desktop=\"off\""])
            .arg("--cwd")
            .arg(cwd)
            .args(args)
            .stdin(Stdio::null());
        command
    }

    fn ok_at(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.azazel_in(cwd, args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// The rows of `azazel ps --json`.
    fn running(&self) -> Vec<Value> {
        self.ok(&["ps", "--json"])
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.azazel(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// Starts a background session on the prompt of the script and returns its id.
    fn bg(&self, mode: &str) -> String {
        self.ok(&["--mode", mode, "bg", "create hello"])
            .trim()
            .to_string()
    }

    fn wait_for_state(&self, id: &str, state: &str) {
        let started = Instant::now();
        loop {
            let listed = self.ok(&["ps", "--json"]);
            let found = listed
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|row| row["id"] == id && row["state"] == state);
            if found {
                return;
            }
            assert!(started.elapsed() < LIMIT, "{id} never was `{state}`");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `azazel remote --port 0 --json` with `args`, and `dirs` ahead of the rest of `PATH`.
    fn remote(&self, args: &[&str], dirs: &[&Path]) -> Remote {
        let mut all = vec!["remote", "--port", "0", "--json"];
        all.extend_from_slice(args);
        let mut command = self.azazel(&all);
        let mut path: Vec<PathBuf> = dirs.iter().map(|dir| dir.to_path_buf()).collect();
        path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command.env("PATH", std::env::join_paths(path).unwrap());
        Remote::start(command)
    }

    /// A directory with `fixture` in it as `name`.
    fn tool(&self, name: &str, fixture: &Path) -> PathBuf {
        let dir = self.dir.path().join(format!("tool-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(
            fixture,
            dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)),
        )
        .unwrap();
        dir
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let listed = self.azazel(&["ps", "--json"]).output();
        let rows = listed
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        for row in rows
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        {
            if let Some(id) = row["id"].as_str() {
                let _ = self.azazel(&["kill", id]).output();
            }
        }
    }
}

/// A running `azazel remote`.
struct Remote {
    child: Child,
    errors: Receiver<String>,
    /// The first line of its output, which `--json` makes `{"url","pair","port"}`.
    first: Value,
}

impl Remote {
    fn start(mut command: Command) -> Remote {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let lines = lines_of(child.stdout.take().unwrap());
        let errors = lines_of(child.stderr.take().unwrap());
        let line = lines.recv_timeout(LIMIT).unwrap_or_else(|_| {
            let _ = child.kill();
            panic!(
                "azazel remote printed nothing: {:?}",
                errors.try_iter().collect::<Vec<_>>()
            )
        });
        let first = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{line:?}: {e}"));
        // Whatever it prints after that is not read, which a pipe that fills would punish.
        std::thread::spawn(move || for _ in lines {});
        Remote { child, errors, first }
    }

    fn port(&self) -> u16 {
        self.first["port"].as_u64().unwrap() as u16
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port())
    }

    fn secret(&self) -> String {
        self.first["pair"]
            .as_str()
            .unwrap()
            .split_once("#pair=")
            .unwrap()
            .1
            .to_string()
    }

    /// Makes a link with `key` (`l` control, `v` view) and pairs with it; the device's token.
    async fn pair_with(&mut self, secret: &str) -> String {
        let answer = pair(&self.base(), secret, &[]).await;
        assert_eq!(answer.0, 200, "{}", answer.1);
        answer.1["token"].as_str().unwrap().to_string()
    }

    /// The text of what it said on standard error up to a line with `wanted`.
    fn said(&self, wanted: &str) -> Vec<String> {
        crate::common::read_until(&self.errors, wanted, LIMIT)
    }

    /// Types `q` and waits for it to end.
    fn quit(&mut self) -> std::process::ExitStatus {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "q").unwrap();
        stdin.flush().unwrap();
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(started.elapsed() < LIMIT, "azazel remote did not end");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// POSTs a pairing secret; the status and the JSON answer.
async fn pair(base: &str, secret: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    let mut request = azazel_ai::tls::client()
        .post(format!("{base}/api/pair"))
        .header("X-Azazel", "1")
        .json(&json!({ "secret": secret, "bearer": true }));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(port: u16, id: &str, token: Option<&str>, origin: &str) -> Socket {
    let mut request = format!("ws://127.0.0.1:{port}/api/ws/{id}")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert("origin", origin.parse().unwrap());
    if let Some(token) = token {
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

async fn send(socket: &mut Socket, command: Value) {
    socket.send(Message::text(command.to_string())).await.unwrap();
}

async fn next(socket: &mut Socket) -> Value {
    loop {
        match tokio::time::timeout(LIMIT, socket.next()).await.expect("a frame") {
            Some(Ok(Message::Text(text))) => return serde_json::from_str(text.as_str()).unwrap(),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("expected a frame, got {other:?}"),
        }
    }
}

/// The frames up to and including the first that `wanted` accepts.
async fn until(socket: &mut Socket, wanted: impl Fn(&Value) -> bool) -> Vec<Value> {
    let mut seen = Vec::new();
    loop {
        let frame = next(socket).await;
        let done = wanted(&frame);
        seen.push(frame);
        if done {
            return seen;
        }
    }
}

async fn close_code(socket: &mut Socket) -> u16 {
    loop {
        match tokio::time::timeout(LIMIT, socket.next()).await.expect("a close") {
            Some(Ok(Message::Close(Some(frame)))) => return u16::from(frame.code),
            Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("expected a close, got {other:?}"),
        }
    }
}

/// Attaches and waits for the answer; returns it.
async fn attach(socket: &mut Socket, replay: &str) -> Value {
    send(
        socket,
        json!({ "id": "attach", "type": "attach", "replay": replay }),
    )
    .await;
    until(socket, |frame| frame["id"] == "attach")
        .await
        .pop()
        .unwrap()
}

#[tokio::test]
async fn nothing_works_without_a_token_a_right_origin_or_a_fresh_link() {
    let sandbox = Sandbox::new();
    let id = sandbox.bg("auto");
    let mut remote = sandbox.remote(&[], &[]);
    let (base, port) = (remote.base(), remote.port());
    let client = azazel_ai::tls::client();

    assert_eq!(
        client
            .get(format!("{base}/api/sessions"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let mut nobody = connect(port, &id, None, &base).await;
    assert_eq!(close_code(&mut nobody).await, 4401);

    // The page is public and carries its policy.
    let page = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(page.status(), 200);
    let csp = page.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        csp.contains("default-src 'self'") && csp.contains("frame-ancestors 'none'"),
        "{csp}"
    );

    // A link pairs once; from another origin it does not pair at all.
    let secret = remote.secret();
    let foreign = pair(&base, &secret, &[("Origin", "https://evil.example")]).await;
    assert_eq!(foreign.0, 403);
    let token = remote.pair_with(&secret).await;
    assert_eq!(pair(&base, &secret, &[]).await.0, 401);

    let listed: Value = client
        .get(format!("{base}/api/sessions"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["sessions"][0]["id"], json!(id));
    let mut unknown = connect(port, "nosuch", Some(&token), &base).await;
    assert_eq!(close_code(&mut unknown).await, 4404);
}

#[tokio::test]
async fn a_prompt_streams_from_the_start_to_idle() {
    let sandbox = Sandbox::new();
    let id = sandbox.bg("auto");
    let mut remote = sandbox.remote(&[], &[]);
    let token = remote.pair_with(&remote.secret()).await;
    let mut socket = connect(remote.port(), &id, Some(&token), &remote.base()).await;
    assert_eq!(next(&mut socket).await["type"], "hello");
    // `all` replays what the session did since it started, the run of the prompt it began with.
    let attached = attach(&mut socket, "all").await;
    assert_eq!(attached["success"], true, "{attached}");
    let frames = until(&mut socket, |frame| frame["type"] == "idle").await;
    let kinds: Vec<&str> = frames.iter().filter_map(|frame| frame["type"].as_str()).collect();
    assert!(kinds.contains(&"agent_start"), "{kinds:?}");
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(
        position("agent_start") < position("tool_end") && position("tool_end") < position("idle"),
        "{kinds:?}"
    );
    assert!(sandbox.dir.path().join("project/hello.txt").is_file());
}

#[tokio::test]
async fn the_first_of_two_browsers_to_answer_wins_and_the_other_is_told() {
    let sandbox = Sandbox::new();
    let id = sandbox.bg("manual");
    sandbox.wait_for_state(&id, "waiting for you");
    let mut remote = sandbox.remote(&[], &[]);
    let token = remote.pair_with(&remote.secret()).await;
    let mut first = connect(remote.port(), &id, Some(&token), &remote.base()).await;
    let mut second = connect(remote.port(), &id, Some(&token), &remote.base()).await;
    for socket in [&mut first, &mut second] {
        next(socket).await;
        attach(socket, "none").await;
    }
    let asked = until(&mut first, |frame| frame["type"] == "approval_request")
        .await
        .pop()
        .unwrap();
    until(&mut second, |frame| frame["type"] == "approval_request").await;
    let request_id = asked["request_id"].as_str().unwrap().to_string();

    send(
        &mut first,
        json!({ "id": "yes", "type": "approval_response", "request_id": request_id, "decision": "allow" }),
    )
    .await;
    let closed = until(&mut second, |frame| frame["type"] == "request_closed")
        .await
        .pop()
        .unwrap();
    assert_eq!(closed["request_id"], json!(request_id));
    // A second answer is turned away.
    send(
        &mut second,
        json!({ "id": "late", "type": "approval_response", "request_id": request_id, "decision": "deny" }),
    )
    .await;
    let late = until(&mut second, |frame| frame["id"] == "late")
        .await
        .pop()
        .unwrap();
    assert_eq!(late["success"], false, "{late}");
    until(&mut first, |frame| frame["type"] == "idle").await;
}

#[tokio::test]
async fn a_view_link_watches_and_cannot_steer_and_a_control_link_cannot_reach_the_machine() {
    let sandbox = Sandbox::new();
    let id = sandbox.bg("manual");
    sandbox.wait_for_state(&id, "waiting for you");
    let mut remote = sandbox.remote(&["--view"], &[]);
    let view = remote.pair_with(&remote.secret()).await;
    let mut watcher = connect(remote.port(), &id, Some(&view), &remote.base()).await;
    next(&mut watcher).await;
    assert_eq!(attach(&mut watcher, "none").await["success"], true);
    send(&mut watcher, json!({ "id": "p", "type": "prompt", "text": "go" })).await;
    let refused = until(&mut watcher, |frame| frame["id"] == "p")
        .await
        .pop()
        .unwrap();
    assert_eq!(refused["success"], false, "{refused}");
    // The frames still come: the request that waits, and the answer to a read.
    send(&mut watcher, json!({ "id": "s", "type": "get_state" })).await;
    let state = until(&mut watcher, |frame| frame["id"] == "s")
        .await
        .pop()
        .unwrap();
    assert_eq!(state["success"], true, "{state}");

    // The viewer cannot make a control link; stdin of the server can.
    let made: Value = azazel_ai::tls::client()
        .post(format!("{}/api/links", remote.base()))
        .bearer_auth(&view)
        .header("X-Azazel", "1")
        .json(&json!({ "access": "control" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(made["error"].is_string(), "{made}");
    // With `--json` the first link is also on standard error; read past it to the next one.
    remote.said("pair=");
    {
        let stdin = remote.child.stdin.as_mut().unwrap();
        writeln!(stdin, "l").unwrap();
        stdin.flush().unwrap();
    }
    let said = remote.said("pair=");
    let secret = said
        .last()
        .unwrap()
        .split_once("#pair=")
        .unwrap()
        .1
        .trim()
        .to_string();
    let control = remote.pair_with(&secret).await;
    let mut driver = connect(remote.port(), &id, Some(&control), &remote.base()).await;
    next(&mut driver).await;
    attach(&mut driver, "none").await;
    for (n, command) in [
        json!({ "type": "shell", "command": "echo hi" }),
        json!({ "type": "export", "path": "out.html" }),
        json!({ "type": "set_role", "role": "default", "save": "home" }),
        json!({ "type": "set_mode", "mode": "bypass" }),
    ]
    .into_iter()
    .enumerate()
    {
        let mut command = command;
        command["id"] = json!(format!("c{n}"));
        send(&mut driver, command).await;
        let answer = until(&mut driver, |frame| frame["id"] == json!(format!("c{n}")))
            .await
            .pop()
            .unwrap();
        assert_eq!(answer["success"], false, "{answer}");
    }
    assert!(!sandbox.dir.path().join("project/out.html").exists());
}

#[test]
fn a_cloudflare_tunnel_is_the_only_other_address_the_server_answers_to() {
    let sandbox = Sandbox::new();
    let tool = sandbox.tool("cloudflared", &CLOUDFLARED);
    let mut remote = sandbox.remote(&["--tunnel"], &[&tool]);
    assert_eq!(remote.first["url"], "https://fake-tunnel-test.trycloudflare.com");
    let pair_url = remote.first["pair"].as_str().unwrap();
    assert!(
        pair_url.starts_with("https://fake-tunnel-test.trycloudflare.com/#pair="),
        "{pair_url}"
    );
    // cloudflared was told to lead to the port of the server.
    let told = std::fs::read_to_string(tool.join("cloudflared.url")).unwrap();
    assert_eq!(told, format!("http://127.0.0.1:{}", remote.port()));

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let client = azazel_ai::tls::client();
        let base = remote.base();
        let through = |host: &'static str| client.get(format!("{base}/api/me")).header("Host", host).send();
        assert_eq!(
            through("fake-tunnel-test.trycloudflare.com")
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(through("other.trycloudflare.com").await.unwrap().status(), 421);
        let secret = remote.secret();
        let paired = pair(
            &base,
            &secret,
            &[
                ("Host", "fake-tunnel-test.trycloudflare.com"),
                ("Origin", "https://fake-tunnel-test.trycloudflare.com"),
            ],
        )
        .await;
        assert_eq!(paired.0, 200, "{}", paired.1);
    });
    assert!(remote.quit().success());
}

#[test]
fn without_cloudflared_the_run_says_how_to_install_it() {
    let sandbox = Sandbox::new();
    // Nothing on `PATH` but an empty directory, and nothing in the home.
    let empty = sandbox.dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let mut command = sandbox.azazel(&["remote", "--port", "0", "--tunnel"]);
    command.env("PATH", &empty);
    let output = command.output().unwrap();
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("cloudflared is not installed") && said.contains("install"),
        "{said}"
    );
    assert!(said.contains("developers.cloudflare.com"), "{said}");
}

/// Writes what the stand-in `tailscale` knows.
fn tailscale_knows(tool: &Path, status: Option<&str>) {
    let data = tool.join("tailscale-data");
    std::fs::create_dir_all(&data).unwrap();
    if let Some(status) = status {
        std::fs::write(data.join("status.json"), status).unwrap();
    }
}

#[test]
fn tailscale_serves_the_port_and_only_that_entry_is_removed_afterwards() {
    let sandbox = Sandbox::new();
    let tool = sandbox.tool("tailscale", &TAILSCALE);
    tailscale_knows(
        &tool,
        Some(r#"{"BackendState":"Running","Self":{"DNSName":"Laptop.tail1234.ts.net."}}"#),
    );
    // The user serves something on 443 already.
    let serve = tool.join("tailscale-data/serve.json");
    std::fs::write(&serve, r#"{"TCP":{"443":{"HTTPS":true}}}"#).unwrap();
    let mut remote = sandbox.remote(&["--tailscale"], &[&tool]);
    // 443 is theirs, so the next free port is used, and the address says which.
    assert_eq!(remote.first["url"], "https://laptop.tail1234.ts.net:8443");
    let calls = std::fs::read_to_string(tool.join("tailscale-data/calls.log")).unwrap();
    assert!(
        calls.contains(&format!(
            "serve\t--bg\t--https=8443\thttp://127.0.0.1:{}",
            remote.port()
        )),
        "{calls}"
    );
    let serving: Value = serde_json::from_str(&std::fs::read_to_string(&serve).unwrap()).unwrap();
    assert!(serving["TCP"]["8443"].is_object() && serving["TCP"]["443"].is_object());

    assert!(remote.quit().success());
    let left: Value = serde_json::from_str(&std::fs::read_to_string(&serve).unwrap()).unwrap();
    assert!(
        left["TCP"]["8443"].is_null() && left["TCP"]["443"].is_object(),
        "{left}"
    );
}

const TAILNET: &str = r#"{"BackendState":"Running","Self":{"DNSName":"Laptop.tail1234.ts.net."}}"#;

/// The id of a process that has ended.
fn dead_pid() -> u32 {
    let mut child = Command::new(assert_cmd::cargo::cargo_bin("azazel"))
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// What `serve status --json` says with the user's own entry on 443 and a proxy on `https` to `target`.
fn serving(https: u16, target: &str) -> String {
    json!({
        "TCP": { "443": { "HTTPS": true }, https.to_string(): { "HTTPS": true } },
        "Web": {
            "laptop.tail1234.ts.net:443": { "Handlers": { "/": { "Proxy": "http://127.0.0.1:3000" } } },
            format!("laptop.tail1234.ts.net:{https}"): { "Handlers": { "/": { "Proxy": target } } },
        },
    })
    .to_string()
}

/// Leaves the marker an `azazel remote` run of process `pid` makes for its entry on `https`.
fn leave_marker(sandbox: &Sandbox, https: u16, target: &str, pid: u32) -> PathBuf {
    let dir = sandbox.home().join("remote");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("tailscale-serve-{https}.json"));
    let marker = json!({ "https_port": https, "target": target, "pid": pid, "started": 1_700_000_000 });
    std::fs::write(&file, marker.to_string()).unwrap();
    file
}

/// Starts `remote --tailscale`, stops it again, and returns the calls the stand-in `tailscale` saw.
fn tailscale_calls(sandbox: &Sandbox, tool: &Path) -> String {
    let mut remote = sandbox.remote(&["--tailscale"], &[tool]);
    assert!(remote.quit().success());
    std::fs::read_to_string(tool.join("tailscale-data/calls.log")).unwrap()
}

fn serve_json(tool: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(tool.join("tailscale-data/serve.json")).unwrap()).unwrap()
}

#[test]
fn an_entry_a_killed_run_left_is_turned_off_at_the_next_start() {
    let sandbox = Sandbox::new();
    let tool = sandbox.tool("tailscale", &TAILSCALE);
    tailscale_knows(&tool, Some(TAILNET));
    let dead = "http://127.0.0.1:1";
    let gone = dead_pid();
    std::fs::write(tool.join("tailscale-data/serve.json"), serving(8443, dead)).unwrap();
    let marker = leave_marker(&sandbox, 8443, dead, gone);

    let mut remote = sandbox.remote(&["--tailscale"], &[&tool]);
    // The entry of the killed run is gone, so its port is the first free one again.
    assert_eq!(remote.first["url"], "https://laptop.tail1234.ts.net:8443");
    let said = remote.said("earlier").join("\n");
    assert!(
        said.contains("Removed the Tailscale entry an earlier `azazel remote` left on port 8443"),
        "{said}"
    );
    let calls = std::fs::read_to_string(tool.join("tailscale-data/calls.log")).unwrap();
    let off = calls.find("serve\t--https=8443\toff").expect(&calls);
    let bg = calls.find("serve\t--bg\t--https=8443").expect(&calls);
    assert!(off < bg, "{calls}");
    assert!(
        !calls.contains("--https=443\toff"),
        "the user's entry is left alone: {calls}"
    );
    // The marker now belongs to this run.
    let now: Value = serde_json::from_str(&std::fs::read_to_string(&marker).unwrap()).unwrap();
    assert_ne!(now["pid"], json!(gone));
    assert_eq!(now["target"], format!("http://127.0.0.1:{}", remote.port()));

    assert!(remote.quit().success());
    // A clean stop turns the entry off and takes the marker away.
    assert!(!marker.exists());
    let left = serve_json(&tool);
    assert!(
        left["TCP"]["8443"].is_null() && left["TCP"]["443"].is_object(),
        "{left}"
    );
}

#[test]
fn a_marker_whose_entry_the_user_changed_since_only_goes_away() {
    let sandbox = Sandbox::new();
    let tool = sandbox.tool("tailscale", &TAILSCALE);
    tailscale_knows(&tool, Some(TAILNET));
    // The entry on 8443 points somewhere else now: it is the user's.
    let theirs = "http://127.0.0.1:4000";
    std::fs::write(tool.join("tailscale-data/serve.json"), serving(8443, theirs)).unwrap();
    let marker = leave_marker(&sandbox, 8443, "http://127.0.0.1:1", dead_pid());

    let calls = tailscale_calls(&sandbox, &tool);
    assert!(!calls.contains("--https=8443\toff"), "{calls}");
    // 8443 is theirs, so the run used the next port.
    assert!(calls.contains("serve\t--bg\t--https=10000"), "{calls}");
    assert!(!marker.exists());
    let kept = serve_json(&tool);
    assert_eq!(
        kept["Web"]["laptop.tail1234.ts.net:8443"]["Handlers"]["/"]["Proxy"],
        theirs
    );
}

#[test]
fn an_entry_without_a_marker_is_never_turned_off() {
    let sandbox = Sandbox::new();
    let tool = sandbox.tool("tailscale", &TAILSCALE);
    tailscale_knows(&tool, Some(TAILNET));
    // It points at a port nothing listens on, which is what a stale entry looks like too.
    std::fs::write(
        tool.join("tailscale-data/serve.json"),
        serving(8443, "http://127.0.0.1:1"),
    )
    .unwrap();

    let calls = tailscale_calls(&sandbox, &tool);
    assert!(!calls.contains("--https=8443\toff"), "{calls}");
    assert!(!calls.contains("--https=443\toff"), "{calls}");
    // 8443 is skipped, as when the user serves it.
    assert!(calls.contains("serve\t--bg\t--https=10000"), "{calls}");
    assert!(serve_json(&tool)["TCP"]["8443"].is_object());
}

#[test]
fn the_entry_of_a_run_that_still_lives_is_not_turned_off() {
    let sandbox = Sandbox::new();
    let tool = sandbox.tool("tailscale", &TAILSCALE);
    tailscale_knows(&tool, Some(TAILNET));
    let target = "http://127.0.0.1:1";
    std::fs::write(tool.join("tailscale-data/serve.json"), serving(8443, target)).unwrap();
    // The marker is of a process that runs: this test.
    let marker = leave_marker(&sandbox, 8443, target, std::process::id());

    let calls = tailscale_calls(&sandbox, &tool);
    assert!(!calls.contains("--https=8443\toff"), "{calls}");
    assert!(calls.contains("serve\t--bg\t--https=10000"), "{calls}");
    assert!(marker.exists(), "the other run still has its marker");
}

#[test]
fn tailscale_that_is_not_signed_in_is_left_alone_and_the_run_says_what_to_do() {
    let sandbox = Sandbox::new();
    let tool = sandbox.tool("tailscale", &TAILSCALE);
    tailscale_knows(
        &tool,
        Some(r#"{"BackendState":"NeedsLogin","Self":{"DNSName":""}}"#),
    );
    let mut command = sandbox.azazel(&["remote", "--port", "0", "--tailscale"]);
    let mut path = vec![tool.clone()];
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    command.env("PATH", std::env::join_paths(path).unwrap());
    let output = command.output().unwrap();
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("tailscale up"), "{said}");
    let calls = std::fs::read_to_string(tool.join("tailscale-data/calls.log")).unwrap();
    assert!(!calls.contains("--bg"), "nothing was served: {calls}");
}

/// POSTs `body` to `path` with the device's token; the status and the JSON answer.
async fn post(base: &str, token: &str, path: &str, body: Value) -> (u16, Value) {
    let response = azazel_ai::tls::client()
        .post(format!("{base}{path}"))
        .header("X-Azazel", "1")
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

// The terminal is read with blocking calls while the request waits on another thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_browser_starts_a_session_in_a_folder_and_the_terminal_allows_an_untrusted_one() {
    let sandbox = Sandbox::new();
    let work = sandbox.dir.path().join("work");
    let app = work.join("app");
    std::fs::create_dir_all(app.join(".git")).unwrap();
    let app = azazel_core::paths::canonical(&app);
    let folders = format!("remote.folders=['{}']", work.display());
    let mut remote = sandbox.remote(&["--set", &folders], &[]);
    let base = remote.base();
    let token = remote.pair_with(&remote.secret()).await;

    let me = azazel_ai::tls::client()
        .get(format!("{base}/api/me"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(me["can_start"], true, "{me}");
    let (status, listed) = post(&base, &token, "/api/folders", json!({})).await;
    assert_eq!(status, 200, "{listed}");
    assert_eq!(listed["folders"][0]["kind"], "root", "{listed}");
    let (_, inside) = post(&base, &token, "/api/folders", json!({ "under": work })).await;
    assert_eq!(
        inside["folders"][0]["path"],
        app.display().to_string(),
        "{inside}"
    );
    assert_eq!(inside["folders"][0]["trusted"], false, "{inside}");

    // Outside the folders it was given: refused, and nothing is asked.
    let (status, refused) = post(
        &base,
        &token,
        "/api/sessions",
        json!({ "cwd": sandbox.dir.path() }),
    )
    .await;
    assert_eq!(status, 403, "{refused}");

    // The folder is not trusted: the terminal is asked, and no is no.
    let asking = {
        let (base, token, app) = (base.clone(), token.clone(), app.clone());
        tokio::spawn(async move { post(&base, &token, "/api/sessions", json!({ "cwd": app })).await })
    };
    remote.said("which is not trusted");
    writeln!(remote.child.stdin.as_mut().unwrap(), "n").unwrap();
    let (status, said) = asking.await.unwrap();
    assert_eq!(status, 403, "{said}");

    // Yes starts it there, on its first message.
    let asking = {
        let (base, token, app) = (base.clone(), token.clone(), app.clone());
        tokio::spawn(async move {
            post(
                &base,
                &token,
                "/api/sessions",
                json!({ "cwd": app, "prompt": "create hello", "mode": "auto" }),
            )
            .await
        })
    };
    remote.said("which is not trusted");
    writeln!(remote.child.stdin.as_mut().unwrap(), "y").unwrap();
    let (status, started) = asking.await.unwrap();
    assert_eq!(status, 200, "{started}");
    let id = started["id"].as_str().unwrap().to_string();
    let listed = sandbox.ok(&["ps", "--json"]);
    let row = listed
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|row| row["id"] == id.as_str())
        .unwrap_or_else(|| panic!("{id} is not listed: {listed}"));
    assert_eq!(row["cwd"], app.display().to_string());
    assert_eq!(row["mode"], "auto");

    let mut socket = connect(remote.port(), &id, Some(&token), &base).await;
    assert_eq!(next(&mut socket).await["type"], "hello");
    assert_eq!(attach(&mut socket, "all").await["success"], true);
    until(&mut socket, |frame| frame["type"] == "idle").await;
    assert!(app.join("hello.txt").is_file());
}

#[tokio::test]
async fn with_bypass_allowed_a_browser_starts_anywhere_without_a_question() {
    let sandbox = Sandbox::new();
    let anywhere = azazel_core::paths::canonical(&sandbox.dir.path().join("anywhere"));
    std::fs::create_dir_all(&anywhere).unwrap();
    let mut remote = sandbox.remote(&["--set", "remote.allow_bypass=true"], &[]);
    let base = remote.base();
    let token = remote.pair_with(&remote.secret()).await;
    let (status, listed) = post(&base, &token, "/api/folders", json!({})).await;
    assert_eq!((status, &listed["anywhere"]), (200, &json!(true)), "{listed}");
    let (status, started) = post(&base, &token, "/api/sessions", json!({ "cwd": anywhere })).await;
    assert_eq!(status, 200, "{started}");
    let listed = sandbox.ok(&["ps", "--json"]);
    assert!(listed.contains(started["id"].as_str().unwrap()), "{listed}");
}

/// A browser lists the earlier sessions of the machine and goes on with one in the background; asking
/// again for a session that is open already answers the one that has it.
#[tokio::test]
async fn a_browser_goes_on_with_an_earlier_session_in_the_background() {
    let sandbox = Sandbox::new();
    // An earlier session of the project, run in print mode and over.
    sandbox.ok(&["-p", "--mode", "auto", "create hello"]);
    let mut remote = sandbox.remote(&["--set", "remote.allow_bypass=true"], &[]);
    let base = remote.base();
    let token = remote.pair_with(&remote.secret()).await;

    let (status, listed) = post(&base, &token, "/api/history", json!({})).await;
    assert_eq!(status, 200, "{listed}");
    let earlier = listed["sessions"].as_array().unwrap();
    assert_eq!(earlier.len(), 1, "{listed}");
    assert_eq!(earlier[0]["first_prompt"], "create hello");
    assert!(earlier[0]["running"].is_null());
    let session = earlier[0]["id"].as_str().unwrap().to_string();
    let (_, none) = post(&base, &token, "/api/history", json!({ "query": "no such words" })).await;
    assert_eq!(none["sessions"].as_array().unwrap().len(), 0, "{none}");
    let (_, found) = post(&base, &token, "/api/history", json!({ "query": "hello" })).await;
    assert_eq!(found["sessions"].as_array().unwrap().len(), 1, "{found}");

    let (status, resumed) = post(&base, &token, "/api/sessions", json!({ "resume": session })).await;
    assert_eq!(status, 200, "{resumed}");
    let host = resumed["id"].as_str().unwrap().to_string();
    sandbox.wait_for_state(&host, "idle");
    // It is the saved session that runs: no second file, and the list says where it is open.
    assert_eq!(
        std::fs::read_dir(sandbox.home().join("projects"))
            .unwrap()
            .flatten()
            .flat_map(|project| std::fs::read_dir(project.path().join("sessions"))
                .unwrap()
                .flatten())
            .filter(|file| file.path().extension().is_some_and(|ext| ext == "jsonl"))
            .count(),
        1
    );
    let (_, listed) = post(&base, &token, "/api/history", json!({})).await;
    assert_eq!(listed["sessions"][0]["running"], host.as_str(), "{listed}");
    let (status, again) = post(&base, &token, "/api/sessions", json!({ "resume": session })).await;
    assert_eq!(
        (status, again["id"].as_str()),
        (200, Some(host.as_str())),
        "{again}"
    );
    let (status, missing) = post(
        &base,
        &token,
        "/api/sessions",
        json!({ "resume": "nonexistent-id" }),
    )
    .await;
    assert_eq!(status, 400, "{missing}");
}

/// A session a browser started goes on after `q`, as one of `azazel bg` does, and the server says
/// which and how to reach it.
#[tokio::test]
async fn quitting_names_the_sessions_browsers_started_that_still_run() {
    let sandbox = Sandbox::new();
    let anywhere = azazel_core::paths::canonical(&sandbox.dir.path().join("anywhere"));
    std::fs::create_dir_all(&anywhere).unwrap();
    let mut remote = sandbox.remote(&["--set", "remote.allow_bypass=true"], &[]);
    let base = remote.base();
    let token = remote.pair_with(&remote.secret()).await;
    let (status, started) = post(&base, &token, "/api/sessions", json!({ "cwd": anywhere })).await;
    assert_eq!(status, 200, "{started}");
    let id = started["id"].as_str().unwrap().to_string();

    assert!(remote.quit().success());
    let said = remote.said("azazel attach <id>").join("\n");
    assert!(
        said.contains("1 session started from the browser is still running"),
        "{said}"
    );
    assert!(said.contains(&id), "{said}");
    // It is still there to attach to.
    sandbox.wait_for_state(&id, "idle");
}

/// Trusts the project of the sandbox, so that the terminal is not asked about it.
fn trust_project(sandbox: &Sandbox) {
    let project = azazel_core::paths::canonical(&sandbox.dir.path().join("project"));
    azazel_core::TrustStore::new(sandbox.home().join("trust.json"))
        .set(&project, true)
        .unwrap();
}

/// The session id of the background session `host`, from `azazel ps`.
fn session_of(sandbox: &Sandbox, host: &str) -> String {
    sandbox
        .running()
        .into_iter()
        .find(|row| row["id"] == host)
        .and_then(|row| row["session_id"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("{host} is not listed"))
}

/// `[remote] folders` limits the earlier sessions a browser sees and goes on with. A session
/// that a background session has open in a folder outside them is answered exactly as an id that
/// does not exist, so that the answer cannot be used to find ids or running sessions.
#[tokio::test]
async fn earlier_sessions_outside_the_folders_are_neither_listed_nor_resumed_nor_found() {
    let sandbox = Sandbox::new();
    trust_project(&sandbox);
    let project = azazel_core::paths::canonical(&sandbox.dir.path().join("project"));
    let other = sandbox.dir.path().join("other");
    std::fs::create_dir_all(other.join(".git")).unwrap();
    // One earlier session inside the folders, and one in a folder outside them that a
    // background session has open now.
    sandbox.ok(&["-p", "--mode", "auto", "create hello"]);
    let outside = sandbox
        .ok_at(&other, &["--mode", "auto", "bg", "create hello"])
        .trim()
        .to_string();
    sandbox.wait_for_state(&outside, "idle");
    let hidden = session_of(&sandbox, &outside);
    let folders = format!("remote.folders=['{}']", project.display());
    let mut remote = sandbox.remote(&["--set", &folders], &[]);
    let base = remote.base();
    let token = remote.pair_with(&remote.secret()).await;

    let (status, listed) = post(&base, &token, "/api/history", json!({})).await;
    assert_eq!(status, 200, "{listed}");
    let rows = listed["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{listed}");
    assert!(rows.iter().all(|row| row["id"] != hidden.as_str()), "{listed}");
    let (_, found) = post(&base, &token, "/api/history", json!({ "query": "other" })).await;
    assert_eq!(found["sessions"].as_array().unwrap().len(), 0, "{found}");

    let refuse = |id: String| {
        let (base, token) = (base.clone(), token.clone());
        async move { post(&base, &token, "/api/sessions", json!({ "resume": id })).await }
    };
    let unknown = "0123456789abcdef-no-such-session";
    let (status, said) = refuse(unknown.into()).await;
    assert_eq!(status, 400, "{said}");
    assert_eq!(said["error"], format!("there is no earlier session {unknown}"));
    // Ids start alike when sessions are made together: a prefix only the hidden one has.
    let inside = rows[0]["id"].as_str().unwrap().to_string();
    let apart = (8..=hidden.len())
        .find(|n| !inside.starts_with(&hidden[..*n]))
        .unwrap();
    for wanted in [hidden.clone(), hidden[..apart].to_string()] {
        let (status, said) = refuse(wanted.clone()).await;
        assert_eq!(status, 400, "{said}");
        assert_eq!(said["error"], format!("there is no earlier session {wanted}"));
    }
    // Nothing was started: the one host is the one that was there.
    assert_eq!(sandbox.running().len(), 1);

    // The session inside the folders is found by the start of its id, and by no shorter one.
    let (status, said) = refuse(inside[..7].to_string()).await;
    assert_eq!(status, 400, "{said}");
    let (status, started) = refuse(inside[..8].to_string()).await;
    assert_eq!(status, 200, "{started}");
    sandbox.wait_for_state(started["id"].as_str().unwrap(), "idle");
}

/// What is typed in an earlier session may hold a secret: the list hides it as the frames do.
#[tokio::test]
async fn the_list_of_earlier_sessions_hides_secrets_unless_redaction_is_off() {
    let sandbox = Sandbox::new();
    let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH";
    sandbox.ok(&["-p", "--mode", "auto", &format!("create hello with {key}")]);
    let project = azazel_core::paths::canonical(&sandbox.dir.path().join("project"));
    let folders = format!("remote.folders=['{}']", project.display());
    let first_prompt = |on: bool| {
        let sandbox = &sandbox;
        let folders = &folders;
        async move {
            let redact = format!("remote.redact={on}");
            let mut remote = sandbox.remote(&["--set", folders, "--set", &redact], &[]);
            let token = remote.pair_with(&remote.secret()).await;
            let (status, listed) = post(&remote.base(), &token, "/api/history", json!({})).await;
            assert_eq!(status, 200, "{listed}");
            listed["sessions"][0]["first_prompt"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let hidden = first_prompt(true).await;
    assert!(!hidden.contains(key) && hidden.contains("redacted"), "{hidden}");
    assert!(first_prompt(false).await.contains(key));
}

/// Two requests to go on with one session start one background session between them, and a
/// mode the session once recorded does not come back: a browser gets the mode it asks for or the
/// configured one, and `bypass` only where the server allows it.
#[tokio::test]
async fn going_on_with_a_session_twice_at_once_starts_one_host_and_never_restores_bypass() {
    let sandbox = Sandbox::new();
    trust_project(&sandbox);
    sandbox.ok(&["-p", "--mode", "auto", "create hello"]);
    let project = azazel_core::paths::canonical(&sandbox.dir.path().join("project"));
    // The session once was in bypass.
    let file = std::fs::read_dir(sandbox.home().join("projects"))
        .unwrap()
        .flatten()
        .flat_map(|project| {
            std::fs::read_dir(project.path().join("sessions"))
                .unwrap()
                .flatten()
        })
        .map(|file| file.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .unwrap();
    let mut text = std::fs::read_to_string(&file).unwrap();
    let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    let entry = json!({
        "type": "mode_change", "mode": "bypass", "id": "zz-bypass",
        "parentId": last["id"], "timestamp": last["timestamp"],
    });
    text.push_str(&format!("{entry}\n"));
    std::fs::write(&file, text).unwrap();

    let folders = format!("remote.folders=['{}']", project.display());
    let mut remote = sandbox.remote(&["--set", &folders], &[]);
    let base = remote.base();
    let token = remote.pair_with(&remote.secret()).await;
    let (_, listed) = post(&base, &token, "/api/history", json!({})).await;
    let session = listed["sessions"][0]["id"].as_str().unwrap().to_string();

    // Bypass asked for is refused, and nothing is started.
    let (status, said) = post(
        &base,
        &token,
        "/api/sessions",
        json!({ "resume": session, "mode": "bypass" }),
    )
    .await;
    assert_eq!(status, 403, "{said}");
    assert!(sandbox.running().is_empty());

    let go = || post(&base, &token, "/api/sessions", json!({ "resume": session }));
    let ((first, one), (second, two)) = tokio::join!(go(), go());
    assert_eq!((first, second), (200, 200), "{one} {two}");
    assert_eq!(one["id"], two["id"], "{one} {two}");
    let host = one["id"].as_str().unwrap();
    sandbox.wait_for_state(host, "idle");
    let rows = sandbox.running();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_ne!(rows[0]["mode"], "bypass", "{rows:?}");
}

/// A view link may be let watch the agents' browser, never take it over or steer it.
#[cfg(feature = "browser")]
#[tokio::test]
async fn a_view_link_is_refused_the_browser_unless_it_may_watch_and_never_takes_it() {
    let sandbox = Sandbox::new();
    let id = sandbox.bg("manual");
    sandbox.wait_for_state(&id, "waiting for you");
    let mut remote = sandbox.remote(&["--view"], &[]);
    let view = remote.pair_with(&remote.secret()).await;
    let mut watcher = connect(remote.port(), &id, Some(&view), &remote.base()).await;
    next(&mut watcher).await;
    assert_eq!(attach(&mut watcher, "none").await["success"], true);
    for (n, command) in [
        json!({ "type": "browser_watch", "on": true }),
        json!({ "type": "browser_take", "on": true }),
        json!({ "type": "browser_input", "input": { "kind": "back" } }),
        json!({ "type": "browser_sign_in" }),
    ]
    .into_iter()
    .enumerate()
    {
        let mut command = command;
        command["id"] = format!("b{n}").into();
        send(&mut watcher, command).await;
        let answer = until(&mut watcher, |frame| frame["id"] == format!("b{n}"))
            .await
            .pop()
            .unwrap();
        assert_eq!(answer["success"], false, "{answer}");
        assert!(
            answer["error"].as_str().unwrap().contains("control link"),
            "{answer}"
        );
    }
}
