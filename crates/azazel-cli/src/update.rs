//! `azazel update`: replaces this binary with the newest GitHub release, and the once-a-day
//! background check whose result the TUI shows when it opens.
//!
//! Releases carry one archive per platform, `azazel-<os>-<arch>.zip` (Windows) or `.tar.gz`,
//! with a `<archive>.sha256` beside it. The archive holds a directory with the binary in it.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use azazel_core::Paths;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::print::{EXIT_FAILED, EXIT_OK};

/// The latest-release document of the project's GitHub repository.
const LATEST_URL: &str = "https://api.github.com/repos/sinnafuls/azazel-cli/releases/latest";
/// Replaces [`LATEST_URL`], for testing against a local server.
const LATEST_URL_ENV: &str = "AZAZEL_UPDATE_URL";
/// Set to anything to skip the background check.
const NO_CHECK_ENV: &str = "AZAZEL_NO_UPDATE_CHECK";
/// How often the background check asks GitHub.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    /// The API address of the file, which a token can download from a private repository.
    #[serde(default)]
    url: String,
}

/// A GitHub token from `GITHUB_TOKEN` or `GH_TOKEN`, needed while the repository is private.
fn token() -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

/// Adds the token, when there is one, to a request for the GitHub API.
fn authorized(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match token() {
        Some(token) => request.bearer_auth(token),
        None => request,
    }
}

/// What the background check found, kept in `~/.azazel/cache/update.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Cached {
    /// Unix seconds of the last answered check.
    checked_at: u64,
    /// The newest released version then, without the leading `v`.
    latest: String,
}

/// `(major, minor, patch)` of `1.2.3`, `v1.2.3` or `1.2.3-beta`; `None` for anything else.
fn version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().trim_start_matches('v');
    let core = text.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

/// Whether `latest` is a newer version than the running one.
fn is_newer(latest: &str) -> bool {
    matches!((version(latest), version(CURRENT)), (Some(l), Some(c)) if l > c)
}

/// The release archive for the machine this runs on.
fn asset_name() -> String {
    let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
    format!(
        "azazel-{}-{}.{extension}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .user_agent(concat!("azazel/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_default()
}

async fn latest_release(client: &reqwest::Client) -> Result<Release, String> {
    let url = std::env::var(LATEST_URL_ENV).unwrap_or_else(|_| LATEST_URL.to_string());
    let response = authorized(client.get(&url))
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("could not reach {url}: {e}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(
            "no release found (none is published yet, or the repository is private: set GITHUB_TOKEN)"
                .to_string(),
        );
    }
    if !status.is_success() {
        return Err(format!("{url} answered HTTP {status}"));
    }
    response
        .json::<Release>()
        .await
        .map_err(|e| format!("unreadable release information from {url}: {e}"))
}

/// Downloads a release file: through the API with the token when there is one (private
/// repositories), else from its public address. The token is not sent on to the storage host the
/// API redirects to.
async fn download(client: &reqwest::Client, asset: &Asset) -> Result<Vec<u8>, String> {
    let request = match token() {
        Some(token) if !asset.url.is_empty() => client
            .get(&asset.url)
            .bearer_auth(token)
            .header("accept", "application/octet-stream"),
        _ => client.get(&asset.browser_download_url),
    };
    let url = &asset.name;
    let response = request
        .send()
        .await
        .map_err(|e| format!("could not download {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("could not download {url}: HTTP {}", response.status()));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("could not download {url}: {e}"))?;
    Ok(bytes.to_vec())
}

/// The hex digest in a `sha256sum`-style file: the first word of its first line.
fn expected_digest(text: &str) -> Option<String> {
    let word = text.split_whitespace().next()?.to_ascii_lowercase();
    (word.len() == 64 && word.bytes().all(|b| b.is_ascii_hexdigit())).then_some(word)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Unpacks `archive` into `into` with the system `tar`, which reads zip on Windows (bsdtar, shipped
/// since Windows 10) and gzip archives everywhere else.
fn unpack(archive: &Path, into: &Path) -> Result<(), String> {
    let program = if cfg!(windows) {
        let system = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_default();
        system.join("System32").join("tar.exe")
    } else {
        PathBuf::from("tar")
    };
    let flags = if cfg!(windows) { "-xf" } else { "-xzf" };
    let output = std::process::Command::new(&program)
        .arg(flags)
        .arg(archive)
        .arg("-C")
        .arg(into)
        .output()
        .map_err(|e| format!("could not run {}: {e}", program.display()))?;
    if !output.status.success() {
        return Err(format!(
            "could not unpack the release: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// The binary inside an unpacked release: at the top or one directory down.
fn find_binary(dir: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) { "azazel.exe" } else { "azazel" };
    let top = dir.join(name);
    if top.is_file() {
        return Some(top);
    }
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(name))
        .find(|candidate| candidate.is_file())
}

/// Where the old binary is moved while the new one takes its place. Windows cannot replace a
/// running program's file but can rename it.
fn retired(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

/// Removes the binary a previous update retired, which the running program could not delete.
pub fn remove_retired() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(retired(&exe));
    }
}

/// Puts `new` where `exe` is, keeping the old binary until the new one is in place.
fn replace(exe: &Path, new: &Path) -> Result<(), String> {
    let dir = exe.parent().ok_or("the running binary has no directory")?;
    // Copy next to the target first, so the final step is a rename on one filesystem.
    let staged = dir.join(format!(".azazel-update-{}", std::process::id()));
    std::fs::copy(new, &staged).map_err(|e| format!("cannot write to {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }
    let old = retired(exe);
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(exe, &old) {
        let _ = std::fs::remove_file(&staged);
        return Err(format!("cannot move {} aside: {e}", exe.display()));
    }
    if let Err(e) = std::fs::rename(&staged, exe) {
        // Put the old binary back so the install keeps working.
        let _ = std::fs::rename(&old, exe);
        let _ = std::fs::remove_file(&staged);
        return Err(format!("cannot put the new binary in place: {e}"));
    }
    // Fails on Windows while the old binary still runs; `remove_retired` finishes the job later.
    let _ = std::fs::remove_file(&old);
    Ok(())
}

/// Downloads, verifies and installs `release` over the running binary.
async fn install(client: &reqwest::Client, release: &Release) -> Result<(), String> {
    let name = asset_name();
    let archive = release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .ok_or_else(|| {
            format!(
                "release {} has no build for this platform ({name})",
                release.tag_name
            )
        })?;
    let checksum_name = format!("{name}.sha256");
    let checksum = release
        .assets
        .iter()
        .find(|asset| asset.name == checksum_name)
        .ok_or_else(|| {
            format!(
                "release {} has no {checksum_name}; not installing it",
                release.tag_name
            )
        })?;

    let bytes = download(client, archive).await?;
    let sums = download(client, checksum).await?;
    let expected = expected_digest(&String::from_utf8_lossy(&sums))
        .ok_or_else(|| format!("{checksum_name} holds no SHA-256 digest"))?;
    let actual = hex(&Sha256::digest(&bytes));
    if actual != expected {
        return Err(format!(
            "the download does not match its checksum (expected {expected}, got {actual}); nothing was changed"
        ));
    }

    let work = tempfile::tempdir().map_err(|e| format!("cannot create a temporary directory: {e}"))?;
    let saved = work.path().join(&name);
    std::fs::write(&saved, &bytes).map_err(|e| format!("cannot save the download: {e}"))?;
    let unpacked = work.path().join("unpacked");
    std::fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
    unpack(&saved, &unpacked)?;
    let binary = find_binary(&unpacked).ok_or("the release archive holds no azazel binary")?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate the running binary: {e}"))?;
    replace(&exe, &binary)
}

fn read_cache(paths: &Paths) -> Cached {
    std::fs::read_to_string(paths.cache_dir().join("update.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_cache(paths: &Paths, cached: &Cached) {
    let dir = paths.cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(text) = serde_json::to_string(cached) {
        let _ = std::fs::write(dir.join("update.json"), text);
    }
}

/// Whether the background check may run: on in config and not turned off by the environment.
pub fn checks_enabled(config_check: bool) -> bool {
    config_check && std::env::var_os(NO_CHECK_ENV).is_none()
}

/// The notice for a newer release a previous check found, if any.
pub fn notice(paths: &Paths) -> Option<String> {
    let cached = read_cache(paths);
    is_newer(&cached.latest).then(|| {
        format!(
            "azazel {} is available (you have {CURRENT}): run `azazel update`",
            cached.latest
        )
    })
}

/// Asks GitHub for the newest release when the last check is more than a day old, without
/// blocking; the answer is shown the next time the UI opens.
pub fn check_in_background(paths: &Paths) {
    let cached = read_cache(paths);
    if now_secs().saturating_sub(cached.checked_at) < CHECK_EVERY.as_secs() {
        return;
    }
    let paths = paths.clone();
    tokio::spawn(async move {
        match latest_release(&client(CHECK_TIMEOUT)).await {
            Ok(release) => write_cache(
                &paths,
                &Cached {
                    checked_at: now_secs(),
                    latest: release.tag_name.trim_start_matches('v').to_string(),
                },
            ),
            Err(why) => tracing::debug!("update check failed: {why}"),
        }
    });
}

/// `azazel update [--check]`.
pub async fn run(paths: &Paths, check_only: bool) -> u8 {
    let client = client(DOWNLOAD_TIMEOUT);
    let release = match latest_release(&client).await {
        Ok(release) => release,
        Err(why) => {
            eprintln!("azazel: update: {why}");
            return EXIT_FAILED;
        }
    };
    let latest = release.tag_name.trim_start_matches('v').to_string();
    write_cache(
        paths,
        &Cached {
            checked_at: now_secs(),
            latest: latest.clone(),
        },
    );
    if !is_newer(&latest) {
        println!("azazel {CURRENT} is up to date");
        return EXIT_OK;
    }
    if check_only {
        println!("azazel {latest} is available (you have {CURRENT}): run `azazel update`");
        return EXIT_OK;
    }
    println!("updating azazel {CURRENT} -> {latest}...");
    match install(&client, &release).await {
        Ok(()) => {
            println!("azazel {latest} is installed");
            EXIT_OK
        }
        Err(why) => {
            eprintln!("azazel: update: {why}");
            EXIT_FAILED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_compared_by_number_not_by_text() {
        assert_eq!(version("v1.10.2"), Some((1, 10, 2)));
        assert_eq!(version("0.2.0-beta.1"), Some((0, 2, 0)));
        assert_eq!(version("1.2"), None);
        assert_eq!(version("1.2.3.4"), None);
        assert_eq!(version("latest"), None);
        assert!(version("v0.10.0") > version("v0.9.9"));
        assert!(!is_newer(CURRENT));
        assert!(!is_newer("garbage"));
        assert!(is_newer("999.0.0"));
    }

    #[test]
    fn only_a_well_formed_digest_is_accepted() {
        let digest = "a".repeat(64);
        assert_eq!(
            expected_digest(&format!("{digest}  azazel-linux-x86_64.tar.gz\n")),
            Some(digest)
        );
        assert_eq!(expected_digest(&"A".repeat(64)), Some("a".repeat(64)));
        assert_eq!(expected_digest("abc  file"), None);
        assert_eq!(expected_digest(""), None);
        assert_eq!(hex(&[0, 15, 255]), "000fff");
    }

    #[test]
    fn the_binary_is_found_at_the_top_or_one_level_down() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_binary(dir.path()).is_none());
        let name = if cfg!(windows) { "azazel.exe" } else { "azazel" };
        let nested = dir.path().join("azazel-x");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(name), b"bin").unwrap();
        assert_eq!(find_binary(dir.path()), Some(nested.join(name)));
    }

    #[test]
    fn replacing_keeps_the_target_path_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("tool");
        let new = dir.path().join("new");
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        replace(&exe, &new).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert!(!retired(&exe).exists());
    }
}
