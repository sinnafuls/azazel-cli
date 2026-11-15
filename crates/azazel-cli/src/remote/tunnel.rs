//! Reaching the server from another device: a Cloudflare quick tunnel, or Tailscale.
//!
//! Both start a program of the user's (`cloudflared`, `tailscale`) that leads to the local port, and
//! learn the public address from it. azazel installs `cloudflared` when asked to; it never
//! installs Tailscale, which needs its system service and a sign-in.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use azazel_core::Paths;
use azazel_core::ui::UiBridge;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// How long cloudflared may take to print its address.
const TUNNEL_WAIT: Duration = Duration::from_secs(60);
/// How long one `tailscale` command may take.
const TAILSCALE_WAIT: Duration = Duration::from_secs(30);

pub(crate) const CLOUDFLARED_DOWNLOADS: &str =
    "https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/";
const CLOUDFLARED_RELEASES: &str = "https://github.com/cloudflare/cloudflared/releases/latest/download";
/// Where the download comes from instead, for tests of the install against a stand-in server.
const BASE_VARIABLE: &str = "AZAZEL_CLOUDFLARED_BASE_URL";

/// Asks the person at the terminal one question with a few answers.
#[async_trait]
pub(crate) trait Chooser: Send + Sync {
    /// The index of the answer, or `None` when nobody can be asked or they gave none.
    async fn choose(&self, prompt: &str, options: &[&str]) -> Option<usize>;
}

/// The terminal UI asks through its own popups.
#[async_trait]
impl Chooser for std::sync::Arc<dyn UiBridge> {
    async fn choose(&self, prompt: &str, options: &[&str]) -> Option<usize> {
        let options: Vec<String> = options.iter().map(|o| o.to_string()).collect();
        self.select(prompt, &options).await.ok()
    }
}

/// A tunnel that is up. It is taken down by [`Tunnel::stop`], and when it is dropped.
pub(crate) struct Tunnel {
    /// `https://<host>[:port]`
    pub url: String,
    child: Option<Child>,
    undo: Option<Undo>,
}

/// What stops Tailscale serving the port again.
struct Undo {
    program: PathBuf,
    https: u16,
}

impl Undo {
    fn run_blocking(&self) {
        let _ = std::process::Command::new(&self.program)
            .args(["serve", &format!("--https={}", self.https), "off"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Tunnel {
    pub async fn stop(mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
        }
        if let Some(undo) = self.undo.take() {
            let _ = tokio::task::spawn_blocking(move || undo.run_blocking()).await;
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // The child is killed by `kill_on_drop`; Tailscale's entry is removed by hand.
        if let Some(undo) = self.undo.take() {
            undo.run_blocking();
        }
    }
}

// ---- Cloudflare -------------------------------------------------------------------------------

/// The address a line of cloudflared's output names, `https://<name>.trycloudflare.com`. Its error
/// messages name `api.trycloudflare.com`, which is not a tunnel.
pub(crate) fn parse_trycloudflare(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let host: String = line[start + "https://".len()..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
        .collect::<String>()
        .to_ascii_lowercase();
    let name = host.strip_suffix(".trycloudflare.com")?;
    (!name.is_empty() && name != "api" && !name.contains('.')).then(|| format!("https://{host}"))
}

/// The `cloudflared` on `PATH` or in `~/.azazel/remote/`.
pub(crate) fn find_cloudflared(paths: &Paths) -> Option<PathBuf> {
    let managed = paths
        .remote_dir()
        .join(format!("cloudflared{}", std::env::consts::EXE_SUFFIX));
    which::which("cloudflared")
        .ok()
        .or_else(|| managed.is_file().then_some(managed))
}

/// What to do about a missing cloudflared, for the person who has to do it.
pub(crate) fn install_help() -> String {
    let command = match std::env::consts::OS {
        "windows" => "winget install --id Cloudflare.cloudflared",
        "macos" => "brew install cloudflared",
        _ => {
            "install the cloudflared package of your distribution, or download the binary from the page below"
        }
    };
    format!("install cloudflared: {command}\n  more ways: {CLOUDFLARED_DOWNLOADS}")
}

/// The name of the release file for this platform, and whether it is a gzipped tar archive.
pub(crate) fn release_asset() -> Option<(String, bool)> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => return None,
    };
    match (std::env::consts::OS, arch) {
        ("linux", arch) => Some((format!("cloudflared-linux-{arch}"), false)),
        ("macos", arch) => Some((format!("cloudflared-darwin-{arch}.tgz"), true)),
        ("windows", "amd64") => Some(("cloudflared-windows-amd64.exe".to_string(), false)),
        _ => None,
    }
}

/// The first regular file called `name` in a gzipped tar archive.
pub(crate) fn file_in_tgz(bytes: &[u8], name: &str) -> Result<Vec<u8>, String> {
    let mut tar = Vec::new();
    // Nothing the release holds comes near this; a larger one is not what was asked for.
    flate2::read::GzDecoder::new(bytes)
        .take(512 * 1024 * 1024)
        .read_to_end(&mut tar)
        .map_err(|e| format!("the download is not a gzip file: {e}"))?;
    let mut at = 0;
    while at + 512 <= tar.len() {
        let header = &tar[at..at + 512];
        if header.iter().all(|b| *b == 0) {
            break;
        }
        let field = |range: std::ops::Range<usize>| {
            let bytes = &header[range];
            let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
            String::from_utf8_lossy(&bytes[..end]).trim().to_string()
        };
        let size =
            usize::from_str_radix(&field(124..136), 8).map_err(|_| "the archive is damaged".to_string())?;
        let body = at + 512;
        if body + size > tar.len() {
            return Err("the archive is cut short".to_string());
        }
        let entry = field(0..100);
        let base = entry.rsplit('/').next().unwrap_or_default();
        if matches!(header[156], b'0' | 0) && base == name {
            return Ok(tar[body..body + size].to_vec());
        }
        at = body + size.div_ceil(512) * 512;
    }
    Err(format!("the archive has no {name}"))
}

/// Downloads cloudflared from `base` into `dir` and returns its path.
pub(crate) async fn install_cloudflared(dir: &Path, base: &str) -> Result<PathBuf, String> {
    let (asset, archive) = release_asset().ok_or("this platform has no cloudflared release")?;
    let bytes = azazel_tools::util::download(&format!("{}/{asset}", base.trim_end_matches('/'))).await?;
    let program = if archive {
        file_in_tgz(&bytes, "cloudflared")?
    } else {
        bytes
    };
    azazel_tools::util::install_program(&program, dir, "cloudflared")
}

/// The cloudflared to run: the one found, or the one installed after asking. `Err` says what to
/// do when there is none.
pub(crate) async fn ensure_cloudflared(paths: &Paths, ask: Option<&dyn Chooser>) -> Result<PathBuf, String> {
    if let Some(found) = find_cloudflared(paths) {
        return Ok(found);
    }
    let missing = format!("cloudflared is not installed.\n  {}", install_help());
    let Some(ask) = ask else { return Err(missing) };
    let answer = ask
        .choose(
            "cloudflared is needed for the tunnel and is not installed. Install it into ~/.azazel/remote?",
            &["Install it for me", "Show me how", "Cancel"],
        )
        .await;
    if answer != Some(0) {
        return Err(missing);
    }
    let base = std::env::var(BASE_VARIABLE).unwrap_or_else(|_| CLOUDFLARED_RELEASES.to_string());
    eprintln!("azazel: downloading cloudflared…");
    install_cloudflared(&paths.remote_dir(), &base)
        .await
        .map_err(|e| format!("{e}\n  {}", install_help()))
}

/// Starts `cloudflared tunnel --url http://127.0.0.1:<port>` and waits for it to say its address.
pub(crate) async fn cloudflare(program: &Path, port: u16) -> Result<Tunnel, String> {
    let mut child = Command::new(program)
        .args([
            "tunnel",
            "--no-autoupdate",
            "--url",
            &format!("http://127.0.0.1:{port}"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", program.display()))?;
    let (lines_tx, mut lines) = mpsc::unbounded_channel::<String>();
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = lines_tx.clone();
        // Read to the end, also after the address: a pipe nobody reads fills up and stops the tunnel.
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if tx.send(line).is_err() {
                    // Nobody listens any more; keep draining.
                }
            }
        });
    }
    drop(lines_tx);
    let mut seen = Vec::new();
    let found = tokio::time::timeout(TUNNEL_WAIT, async {
        while let Some(line) = lines.recv().await {
            if let Some(url) = parse_trycloudflare(&line) {
                return Some(url);
            }
            seen.push(line);
        }
        None
    })
    .await;
    match found {
        Ok(Some(url)) => Ok(Tunnel {
            url,
            child: Some(child),
            undo: None,
        }),
        Ok(None) => Err(format!(
            "cloudflared ended without giving an address:\n{}",
            tail(&seen)
        )),
        Err(_) => Err(format!(
            "cloudflared did not give an address within {} seconds:\n{}",
            TUNNEL_WAIT.as_secs(),
            tail(&seen)
        )),
    }
}

fn tail(lines: &[String]) -> String {
    let start = lines.len().saturating_sub(8);
    lines[start..]
        .iter()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- Tailscale --------------------------------------------------------------------------------

const TAILSCALE_HELP: &str = "Tailscale is not set up on this machine.\n  \
install it from https://tailscale.com/download, sign in with `tailscale up`, and install the \
Tailscale app on the phone (signed in to the same account)";

fn find_tailscale() -> Option<PathBuf> {
    which::which("tailscale").ok().or_else(|| {
        let app = PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
        app.is_file().then_some(app)
    })
}

/// Runs a `tailscale` command; its output, or why it failed.
async fn tailscale_output(program: &Path, args: &[&str]) -> Result<String, String> {
    let run = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(TAILSCALE_WAIT, run)
        .await
        .map_err(|_| {
            format!(
                "`tailscale {}` did not answer within {} seconds",
                args.join(" "),
                TAILSCALE_WAIT.as_secs()
            )
        })?
        .map_err(|e| format!("could not run tailscale: {e}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    let said = if said.trim().is_empty() {
        String::from_utf8_lossy(&output.stdout)
    } else {
        said
    };
    Err(format!("`tailscale {}` failed: {}", args.join(" "), said.trim()))
}

/// The name of this machine on the tailnet from `tailscale status --json`, when it is signed in.
pub(crate) fn tailnet_name(status: &str) -> Result<String, String> {
    let status: Value =
        serde_json::from_str(status).map_err(|e| format!("tailscale's status is not JSON: {e}"))?;
    let state = status["BackendState"].as_str().unwrap_or_default();
    if state != "Running" {
        return Err(format!(
            "Tailscale is not connected (it is `{}`): sign in with `tailscale up`",
            if state.is_empty() { "unknown" } else { state }
        ));
    }
    let name = status["Self"]["DNSName"]
        .as_str()
        .unwrap_or_default()
        .trim_end_matches('.');
    if name.is_empty() {
        return Err(
            "this machine has no name on the tailnet: turn on MagicDNS and HTTPS certificates in the Tailscale admin console"
                .to_string(),
        );
    }
    Ok(name.to_ascii_lowercase())
}

/// The first HTTPS port of Tailscale Serve that nothing uses: this must not replace what the user
/// serves already.
pub(crate) fn free_https_port(serve: &str) -> Option<u16> {
    let used: Vec<u16> = serde_json::from_str::<Value>(serve)
        .ok()
        .and_then(|serve| serve["TCP"].as_object().cloned())
        .map(|tcp| tcp.keys().filter_map(|port| port.parse().ok()).collect())
        .unwrap_or_default();
    [443, 8443, 10000].into_iter().find(|port| !used.contains(port))
}

/// Serves the port over HTTPS to the tailnet.
pub(crate) async fn tailscale(port: u16) -> Result<Tunnel, String> {
    let program = find_tailscale().ok_or(TAILSCALE_HELP)?;
    let status = tailscale_output(&program, &["status", "--json"])
        .await
        .map_err(|e| format!("{e}\n  {TAILSCALE_HELP}"))?;
    let name = tailnet_name(&status)?;
    // Without a configuration, `serve status` has nothing to say; that is no error.
    let serve = tailscale_output(&program, &["serve", "status", "--json"])
        .await
        .unwrap_or_default();
    let https = free_https_port(&serve)
        .ok_or("Tailscale Serve uses ports 443, 8443 and 10000 already: free one with `tailscale serve --https=<port> off`")?;
    let target = format!("http://127.0.0.1:{port}");
    tailscale_output(&program, &["serve", "--bg", &format!("--https={https}"), &target]).await?;
    let url = match https {
        443 => format!("https://{name}"),
        other => format!("https://{name}:{other}"),
    };
    Ok(Tunnel {
        url,
        child: None,
        undo: Some(Undo { program, https }),
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn the_tunnel_address_is_the_trycloudflare_host_and_not_the_api() {
        let said = "2026-11-15T10:00:00Z INF |  https://quiet-river-1234.trycloudflare.com  |";
        assert_eq!(
            parse_trycloudflare(said).unwrap(),
            "https://quiet-river-1234.trycloudflare.com"
        );
        let failed = "ERR failed to request quick Tunnel: Post \"https://api.trycloudflare.com/tunnel\"";
        assert_eq!(parse_trycloudflare(failed), None);
        assert_eq!(
            parse_trycloudflare("https://evil.example.com/.trycloudflare.com"),
            None
        );
        assert_eq!(parse_trycloudflare("https://a.b.trycloudflare.com"), None);
        assert_eq!(parse_trycloudflare("no address here"), None);
    }

    #[test]
    fn the_tailnet_name_needs_a_signed_in_machine() {
        let up = r#"{"BackendState":"Running","Self":{"DNSName":"Laptop.tail1234.ts.net."}}"#;
        assert_eq!(tailnet_name(up).unwrap(), "laptop.tail1234.ts.net");
        let out = r#"{"BackendState":"NeedsLogin","Self":{"DNSName":""}}"#;
        assert!(tailnet_name(out).unwrap_err().contains("tailscale up"));
        assert!(tailnet_name("nope").is_err());
    }

    #[test]
    fn an_https_port_that_the_user_serves_already_is_left_alone() {
        assert_eq!(free_https_port(""), Some(443));
        assert_eq!(free_https_port("{}"), Some(443));
        let busy = r#"{"TCP":{"443":{"HTTPS":true},"8443":{"HTTPS":true}}}"#;
        assert_eq!(free_https_port(busy), Some(10000));
        let full = r#"{"TCP":{"443":{},"8443":{},"10000":{}}}"#;
        assert_eq!(free_https_port(full), None);
    }

    fn tgz(name: &str, body: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[124..135].copy_from_slice(format!("{:011o}", body.len()).as_bytes());
        header[156] = b'0';
        let mut tar = header.to_vec();
        tar.extend_from_slice(body);
        tar.resize(tar.len() + (512 - body.len() % 512) % 512, 0);
        tar.extend_from_slice(&[0u8; 1024]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn the_program_is_found_in_the_archive_of_the_release() {
        let archive = tgz("cloudflared", b"#!program");
        assert_eq!(file_in_tgz(&archive, "cloudflared").unwrap(), b"#!program");
        assert!(file_in_tgz(&archive, "other").is_err());
        assert!(file_in_tgz(b"not gzip", "cloudflared").is_err());
    }

    #[tokio::test]
    async fn the_release_for_this_platform_is_downloaded_and_installed() {
        let Some((asset, archive)) = release_asset() else {
            return;
        };
        let body = if archive {
            tgz("cloudflared", b"the program")
        } else {
            b"the program".to_vec()
        };
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/{asset}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let installed = install_cloudflared(&dir.path().join("remote"), &server.uri())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&installed).unwrap(), b"the program");
        assert!(installed.starts_with(dir.path()));
        // Nothing half-written is left beside it.
        assert_eq!(std::fs::read_dir(installed.parent().unwrap()).unwrap().count(), 1);

        let missing = MockServer::start().await;
        let failed = install_cloudflared(&dir.path().join("other"), &missing.uri()).await;
        assert!(failed.unwrap_err().contains("could not download"));
    }
}
