//! `azazel remote`: serves the background sessions of this machine to a browser.
//!
//! The server itself is the crate `azazel-remote`; this is what ties it to the application: the
//! sessions it can reach, the tunnel to another device, and the terminal it is run from.

mod background;
mod qr;
mod start;
pub(crate) mod terminal;
mod tunnel;

use std::io::IsTerminal;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use azazel_core::Paths;
use azazel_core::config::{Config, RemoteAccess};
use azazel_remote::{Access, Link, Options, Server};
use azazel_tui::{GlyphLevel, Glyphs};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::cli::{Cli, RemoteArgs};
use crate::print::{EXIT_FAILED, EXIT_OK};
use background::BackgroundSessions;
use start::{Ask, Rules};
use tunnel::{Chooser, Tunnel};

/// Which way in to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Way {
    Local,
    Cloudflare,
    Tailscale,
}

fn way(args: &RemoteArgs, config: &Config) -> Way {
    match (args.tunnel, args.tailscale, args.share) {
        (true, ..) => Way::Cloudflare,
        (_, true, _) => Way::Tailscale,
        (.., true) => match config.remote.access {
            RemoteAccess::Cloudflare => Way::Cloudflare,
            RemoteAccess::Tailscale => Way::Tailscale,
        },
        _ => Way::Local,
    }
}

/// Asks at the terminal when there is one.
struct Terminal;

#[async_trait]
impl Chooser for Terminal {
    async fn choose(&self, prompt: &str, options: &[&str]) -> Option<usize> {
        if !std::io::stdin().is_terminal() {
            return None;
        }
        eprintln!("{prompt}");
        for (n, option) in options.iter().enumerate() {
            eprintln!("  {}) {option}", n + 1);
        }
        eprint!("> ");
        let mut answer = String::new();
        tokio::task::spawn_blocking(move || {
            std::io::stdin().read_line(&mut answer).ok()?;
            let n: usize = answer.trim().parse().ok()?;
            (1..=answer_count()).contains(&n).then_some(n - 1)
        })
        .await
        .ok()
        .flatten()
    }
}

/// The options of the question `Terminal` asks are three.
const fn answer_count() -> usize {
    3
}

/// Shown whenever a server starts with `remote.full_access` on, by `azazel remote` and by `/remote`.
pub(crate) const FULL_ACCESS_WARNING: &str = "WARNING: remote.full_access is on. A browser with a control link can run shell commands, write files and the configuration, sign in and out, trust projects and work in any folder of this machine. Share control links only with yourself.";

pub async fn run(cli: &Cli, paths: &Paths, config: &Config, args: &RemoteArgs) -> u8 {
    let remote = &config.remote;
    let way = way(args, config);
    let public_url = args
        .public_url
        .clone()
        .or_else(|| Some(remote.public_url.clone()).filter(|url| !url.trim().is_empty()));
    let options = Options {
        bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: args.port.unwrap_or(remote.port),
        public_url,
        link_ttl: Duration::from_secs(remote.link_minutes.max(1) * 60),
        device_ttl: Duration::from_secs(remote.device_hours.max(1) * 3600),
        redact: remote.redact,
        allow_bypass: remote.allow_bypass,
        full_access: remote.full_access,
        view_browser: remote.view_browser,
        gated: azazel_core::config::is_trust_gated,
    };
    let rules = Rules::of(paths.home.clone(), std::env::home_dir(), remote);
    // The terminal answers the question about a folder nobody trusted, while it is there to.
    let (asks, asked) = mpsc::channel::<Ask>(8);
    // `--trust` trusts the project `azazel remote` was started in, not every folder a browser names.
    let carried = cli.carried().into_iter().filter(|arg| arg != "--trust").collect();
    let sessions = BackgroundSessions::new(paths.clone(), remote.redact, carried, rules, Some(asks));
    let server = match Server::start(options, sessions.clone()).await {
        Ok(server) => server,
        Err(e) => {
            eprintln!("azazel: could not start the server: {e}");
            return EXIT_FAILED;
        }
    };
    let tunnel = match open_tunnel(paths, way, server.port()).await {
        Ok(tunnel) => tunnel,
        Err(e) => {
            eprintln!("azazel: {e}");
            server.shutdown().await;
            return EXIT_FAILED;
        }
    };
    for note in tunnel.iter().flat_map(|tunnel| &tunnel.notes) {
        eprintln!("azazel: {note}");
    }
    if let Some(tunnel) = &tunnel
        && let Err(e) = server.set_public_url(&tunnel.url)
    {
        eprintln!("azazel: {e}");
        server.shutdown().await;
        return EXIT_FAILED;
    }
    let first = if args.view { Access::View } else { Access::Control };
    let link = server.link(first);
    let glyphs = Glyphs::new(GlyphLevel::detect(config.tui.glyphs.as_str(), |name| {
        std::env::var(name).ok()
    }));
    if remote.full_access {
        // On standard error, so that `--json` keeps standard output to its one line.
        eprintln!("azazel: {FULL_ACCESS_WARNING}");
    }
    announce(&server, &link, args.json, glyphs);
    let code = wait(&server, glyphs, args.json, asked).await;
    server.shutdown().await;
    if let Some(tunnel) = tunnel {
        tunnel.stop().await;
    }
    let left = tokio::task::spawn_blocking(move || sessions.still_running())
        .await
        .unwrap_or_default();
    if let Some(note) = still_running_note(&left) {
        eprint!("{note}");
    }
    code
}

/// What `azazel remote` says on quitting about the sessions browsers started that go on: like
/// `azazel bg`, they run until they end or are stopped, and this says how to reach them.
fn still_running_note(left: &[crate::running::Entry]) -> Option<String> {
    if left.is_empty() {
        return None;
    }
    let mut note = format!(
        "{} started from the browser {} still running in the background:\n",
        if left.len() == 1 {
            "1 session".to_string()
        } else {
            format!("{} sessions", left.len())
        },
        if left.len() == 1 { "is" } else { "are" },
    );
    for entry in left {
        let name = entry
            .name
            .as_deref()
            .map(|name| format!(" {name}"))
            .unwrap_or_default();
        note.push_str(&format!(
            "  {}{name}  {}  ({})\n",
            entry.id,
            entry.cwd.display(),
            entry.state
        ));
    }
    note.push_str(
        "azazel attach <id> opens one, azazel kill <id> stops it, and azazel in its folder continues it.\n",
    );
    Some(note)
}

async fn open_tunnel(paths: &Paths, way: Way, port: u16) -> Result<Option<Tunnel>, String> {
    match way {
        Way::Local => Ok(None),
        Way::Cloudflare => {
            let program = tunnel::ensure_cloudflared(paths, Some(&Terminal)).await?;
            eprintln!("azazel: starting the Cloudflare tunnel…");
            tunnel::cloudflare(&program, port).await.map(Some)
        }
        Way::Tailscale => {
            eprintln!("azazel: serving the port through Tailscale…");
            tunnel::tailscale(paths, port).await.map(Some)
        }
    }
}

/// What a person needs to open the page: the address, the link, and the link as a QR code. With
/// `--json` the first line of standard output is the same as data, and the rest goes to standard
/// error.
fn announce(server: &Server, link: &Link, json: bool, glyphs: Glyphs) {
    let urls = server.urls();
    let origin = urls.last().cloned().unwrap_or_default();
    if json {
        println!(
            "{}",
            json!({ "url": origin, "pair": link.url, "port": server.port() })
        );
    }
    let mut out = String::new();
    out.push_str(&format!(
        "azazel remote is serving the background sessions at {}\n",
        urls.join(" and ")
    ));
    out.push_str(&describe(link, glyphs));
    out.push_str("\nl: a new control link   v: a new view link   r: sign every device out   q: quit\n");
    if json {
        eprint!("{out}");
    } else {
        print!("{out}");
    }
}

/// A link as text and as a QR code.
fn describe(link: &Link, glyphs: Glyphs) -> String {
    let kind = match link.access {
        Access::Control => "control (can answer and steer)",
        Access::View => "view (can watch only)",
    };
    let mut text = format!(
        "\n{kind} link, works once, until {}:\n  {}\n\n",
        link.expires, link.url
    );
    for line in qr::qr_lines(&azazel_remote::qr_matrix(&link.url), glyphs) {
        text.push_str(&format!("  {line}\n"));
    }
    text
}

/// Serves until `q`, the end of the terminal's input after `q`, or Ctrl+C; the keys make links.
/// A question from a browser that wants a session in a folder nobody trusted is asked here, one
/// at a time, and the next line answers it; once the input ends, every one is answered no.
async fn wait(server: &Server, glyphs: Glyphs, json: bool, asked: mpsc::Receiver<Ask>) -> u8 {
    let say = |text: String| {
        if json {
            eprint!("{text}");
        } else {
            print!("{text}");
        }
    };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut input_open = true;
    let mut asked = Some(asked);
    let mut pending: Option<Ask> = None;
    loop {
        let waiting = pending.is_some();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return EXIT_OK,
            ask = async {
                match asked.as_mut() {
                    Some(asked) => asked.recv().await,
                    None => std::future::pending().await,
                }
            }, if !waiting => match ask {
                Some(ask) => {
                    say(ask.question.clone());
                    pending = Some(ask);
                }
                None => asked = None,
            },
            () = async {
                match pending.as_mut() {
                    Some(ask) => ask.answer.closed().await,
                    None => std::future::pending().await,
                }
            }, if waiting => {
                pending = None;
                say("\nthe browser stopped waiting for an answer\n".to_string());
            },
            line = lines.next_line(), if input_open => {
                let Ok(Some(line)) = line else {
                    // No input (a background run, a closed pipe): serve until interrupted, and
                    // nobody is here to allow a session.
                    input_open = false;
                    asked = None;
                    if let Some(ask) = pending.take() {
                        let _ = ask.answer.send(false);
                    }
                    continue;
                };
                if let Some(ask) = pending.take() {
                    let yes = matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
                    let _ = ask.answer.send(yes);
                    say(if yes { "allowed\n" } else { "refused\n" }.to_string());
                    continue;
                }
                match line.trim() {
                    "l" => say(describe(&server.link(Access::Control), glyphs)),
                    "v" => say(describe(&server.link(Access::View), glyphs)),
                    "r" => {
                        server.revoke_all();
                        say("every device is signed out and every link is void\n".to_string());
                    }
                    "q" => return EXIT_OK,
                    "" => {}
                    other => say(format!("`{other}`: l, v, r or q\n")),
                }
            }
        }
    }
}

#[allow(dead_code)]
fn _assert_chooser_is_object_safe(_: Option<Arc<dyn Chooser>>) {}
