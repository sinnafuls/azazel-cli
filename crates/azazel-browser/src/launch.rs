//! Finding a Chromium-based browser on this machine and starting it with remote debugging on.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};

pub const NOT_FOUND: &str = "no Chromium-based browser found; set [browser] path";

/// Environment variable naming the browser binary, tried after the configured path.
pub const BROWSER_ENV: &str = "AZAZEL_BROWSER";

/// How long the browser gets to publish its debugging port. A first start on a cold machine (a
/// fresh CI runner, a new profile) can take well over ten seconds.
const STARTUP: Duration = Duration::from_secs(30);

/// What the failure of [`launch`] says when the browser left at once, as it does while another
/// browser holds the profile.
pub const LEFT_AT_ONCE: &str = "before opening its debugging port";

/// The file, in the profile directory, in which the browser writes its debugging port and the path
/// of its WebSocket endpoint.
const PORT_FILE: &str = "DevToolsActivePort";

/// How long the port file is still waited for after the started process exits. On a new profile
/// Edge sets the profile up, starts itself again and exits; the browser that answers is the
/// second process, which writes the port file a moment later.
const AFTER_EXIT: Duration = Duration::from_secs(3);

/// Chrome, Chromium, Edge and Brave, in that order: install locations first, then names on `PATH`.
fn candidates(env: &dyn Fn(&str) -> Option<String>) -> Vec<(Vec<PathBuf>, &'static [&'static str])> {
    let _ = env;
    #[cfg(windows)]
    {
        let roots: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"]
            .iter()
            .filter_map(|name| env(name))
            .map(PathBuf::from)
            .collect();
        let under = |relative: &str| roots.iter().map(|root| root.join(relative)).collect();
        vec![
            (under("Google/Chrome/Application/chrome.exe"), &["chrome"]),
            (under("Chromium/Application/chrome.exe"), &["chromium"]),
            (under("Microsoft/Edge/Application/msedge.exe"), &["msedge"]),
            (
                under("BraveSoftware/Brave-Browser/Application/brave.exe"),
                &["brave"],
            ),
        ]
    }
    #[cfg(target_os = "macos")]
    {
        let mut roots = vec![PathBuf::from("/Applications")];
        roots.extend(env("HOME").map(|home| PathBuf::from(home).join("Applications")));
        let under = |relative: &str| roots.iter().map(|root| root.join(relative)).collect();
        vec![
            (
                under("Google Chrome.app/Contents/MacOS/Google Chrome"),
                &["google-chrome"],
            ),
            (under("Chromium.app/Contents/MacOS/Chromium"), &["chromium"]),
            (
                under("Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
                &["microsoft-edge"],
            ),
            (
                under("Brave Browser.app/Contents/MacOS/Brave Browser"),
                &["brave-browser"],
            ),
        ]
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        vec![
            (Vec::new(), &["google-chrome", "google-chrome-stable"]),
            (Vec::new(), &["chromium", "chromium-browser"]),
            (Vec::new(), &["microsoft-edge", "microsoft-edge-stable"]),
            (Vec::new(), &["brave-browser", "brave"]),
        ]
    }
}

/// A program name on `PATH`, or a path to an existing file.
fn program(name: &str) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    which::which(name).ok().or_else(|| {
        let path = PathBuf::from(name);
        path.is_file().then_some(path)
    })
}

/// The browser to drive: `configured` (`[browser] path`), then `AZAZEL_BROWSER`, then Chrome,
/// Chromium, Edge and Brave in their standard install locations and on `PATH`.
pub fn find_binary(configured: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(found) = program(configured) {
        return Some(found);
    }
    if let Some(found) = env(BROWSER_ENV).and_then(|name| program(&name)) {
        return Some(found);
    }
    candidates(env).into_iter().find_map(|(locations, names)| {
        locations
            .into_iter()
            .find(|path| path.is_file())
            .or_else(|| names.iter().find_map(|name| which::which(name).ok()))
    })
}

pub struct Launched {
    /// The browser process; its whole tree dies when this is dropped.
    pub child: Box<dyn ChildWrapper>,
    /// The browser's own DevTools endpoint.
    pub ws_url: String,
}

/// `ws://127.0.0.1:<port><path>` from the two lines of `DevToolsActivePort`; `None` while the file
/// is incomplete.
fn endpoint(port_file: &str) -> Option<String> {
    let mut lines = port_file.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    path.starts_with('/')
        .then(|| format!("ws://127.0.0.1:{port}{path}"))
}

/// Starts `binary` on the profile in `profile` and waits for its debugging endpoint.
pub async fn launch(binary: &Path, profile: &Path, headless: bool) -> Result<Launched, String> {
    std::fs::create_dir_all(profile).map_err(|e| format!("{}: {e}", profile.display()))?;
    let port_file = profile.join(PORT_FILE);
    // A file left by an earlier run would be read as this one's.
    let _ = std::fs::remove_file(&port_file);

    let mut command = CommandWrap::with_new(binary, |c| {
        c.arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check");
        if headless {
            c.arg("--headless=new");
        }
        c.arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    });
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(process_wrap::tokio::JobObject);
    command.wrap(KillOnDrop);
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", binary.display()))?;

    let started = Instant::now();
    let mut exited: Option<(Instant, String)> = None;
    loop {
        if let Some(ws_url) = std::fs::read_to_string(&port_file)
            .ok()
            .as_deref()
            .and_then(endpoint)
        {
            return Ok(Launched { child, ws_url });
        }
        // A browser already running on this profile takes over the new process, which then
        // exits; so does a browser that starts itself again (its new process is still in the
        // job, and is stopped with it).
        if exited.is_none()
            && let Ok(Some(status)) = child.try_wait()
        {
            exited = Some((Instant::now(), status.to_string()));
        }
        if let Some((at, status)) = &exited
            && at.elapsed() >= AFTER_EXIT
        {
            return Err(format!(
                "{} exited ({status}) {LEFT_AT_ONCE}; another session may be using the profile {}",
                binary.display(),
                profile.display()
            ));
        }
        if started.elapsed() >= STARTUP {
            return Err(format!(
                "{} did not open its debugging port within {} s",
                binary.display(),
                STARTUP.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_endpoint_needs_both_lines_of_the_port_file() {
        assert_eq!(
            endpoint("51234\n/devtools/browser/0f1e-2d\n").as_deref(),
            Some("ws://127.0.0.1:51234/devtools/browser/0f1e-2d")
        );
        assert_eq!(endpoint("51234\n"), None);
        assert_eq!(endpoint(""), None);
        assert_eq!(endpoint("not a port\n/devtools/browser/x"), None);
    }

    #[test]
    fn the_configured_path_wins_over_the_environment_variable() {
        let dir = tempfile::tempdir().unwrap();
        let configured = dir.path().join("configured-browser");
        let from_env = dir.path().join("env-browser");
        std::fs::write(&configured, "").unwrap();
        std::fs::write(&from_env, "").unwrap();
        let env_path = from_env.to_string_lossy().to_string();
        let env = move |name: &str| (name == BROWSER_ENV).then(|| env_path.clone());

        assert_eq!(find_binary(&configured.to_string_lossy(), &env), Some(configured));
        assert_eq!(find_binary("", &env), Some(from_env.clone()));
        // A configured path that does not exist falls through to the next source.
        let missing = dir.path().join("missing").to_string_lossy().to_string();
        assert_eq!(find_binary(&missing, &env), Some(from_env));
    }
}
