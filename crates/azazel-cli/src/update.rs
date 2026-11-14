//! `azazel update`: replaces this binary with the newest GitHub release, and the once-a-day
//! background check whose result the TUI shows when it opens. With `update.auto` the check also
//! downloads the release into `cache/update/<version>/`, and the UI installs it as it exits.
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
/// The file that says a downloaded release is complete and verified.
const READY: &str = "ready";

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
    azazel_ai::tls::client_builder()
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

/// How many binaries can be retired and still in use at once: one for every azazel that waits
/// for the one it started with `/restart` after an update.
const RETIRED_SLOTS: u32 = 8;

/// Where the old binary is moved while the new one takes its place: `<name>.old`, or
/// `<name>.old1` and so on for `slot` above 0. Windows cannot replace a running program's
/// file but can rename it.
fn retired(exe: &Path, slot: u32) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    if slot > 0 {
        name.push(slot.to_string());
    }
    exe.with_file_name(name)
}

/// A place to retire the binary to that nothing is in: one left by an earlier update is
/// deleted, unless a program still runs from it.
fn free_retired(exe: &Path) -> PathBuf {
    (0..RETIRED_SLOTS)
        .map(|slot| retired(exe, slot))
        .find(|old| !old.exists() || std::fs::remove_file(old).is_ok())
        .unwrap_or_else(|| retired(exe, 0))
}

/// Removes the binaries previous updates retired, which the programs running then could not
/// delete.
pub fn remove_retired() {
    if let Ok(exe) = std::env::current_exe() {
        for slot in 0..RETIRED_SLOTS {
            let _ = std::fs::remove_file(retired(&exe, slot));
        }
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
    let old = free_retired(exe);
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

/// Downloads the archive of `release` for this platform, checks it against its published
/// SHA-256 and unpacks it under `into`. Returns the binary inside.
async fn fetch_binary(client: &reqwest::Client, release: &Release, into: &Path) -> Result<PathBuf, String> {
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

    let saved = into.join(&name);
    std::fs::write(&saved, &bytes).map_err(|e| format!("cannot save the download: {e}"))?;
    let unpacked = into.join("unpacked");
    std::fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
    unpack(&saved, &unpacked)?;
    // Only the unpacked binary is needed from here on.
    let _ = std::fs::remove_file(&saved);
    find_binary(&unpacked).ok_or_else(|| "the release archive holds no azazel binary".to_string())
}

/// Downloads, verifies and installs `release` over the running binary.
async fn install(client: &reqwest::Client, release: &Release) -> Result<(), String> {
    let work = tempfile::tempdir().map_err(|e| format!("cannot create a temporary directory: {e}"))?;
    let binary = fetch_binary(client, release, work.path()).await?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate the running binary: {e}"))?;
    replace(&exe, &binary)
}

/// Where downloaded releases wait to be installed.
fn staging_root(paths: &Paths) -> PathBuf {
    paths.cache_dir().join("update")
}

/// Whether release `latest` is downloaded, verified and waiting to be installed.
fn is_staged(paths: &Paths, latest: &str) -> bool {
    version(latest).is_some() && staging_root(paths).join(latest).join(READY).is_file()
}

/// Downloads and verifies `release` into `cache/update/<version>/`, where [`apply_staged`]
/// finds it. Whatever was there before (an older release, an interrupted download) is removed.
async fn stage(client: &reqwest::Client, release: &Release, paths: &Paths) -> Result<(), String> {
    let latest = release.tag_name.trim_start_matches('v');
    // The version names a directory: only a plain one will do.
    if version(latest).is_none() {
        return Err(format!("`{latest}` is not a version"));
    }
    if is_staged(paths, latest) {
        return Ok(());
    }
    let root = staging_root(paths);
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join(latest);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let staged = match fetch_binary(client, release, &dir).await {
        Ok(_) => std::fs::write(dir.join(READY), latest).map_err(|e| e.to_string()),
        Err(why) => Err(why),
    };
    if staged.is_err() {
        let _ = std::fs::remove_dir_all(&root);
    }
    staged
}

/// Installs the release a background check downloaded, when it is newer than the running
/// binary, and returns the line to print. Called as the UI exits.
pub fn apply_staged(paths: &Paths) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    apply_staged_to(paths, &exe)
}

fn apply_staged_to(paths: &Paths, exe: &Path) -> Option<String> {
    let root = staging_root(paths);
    let (latest, dir) = std::fs::read_dir(&root)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (is_newer(&name) && entry.path().join(READY).is_file()).then(|| (name, entry.path()))
        })
        .max_by_key(|(name, _)| version(name))?;
    let binary = find_binary(&dir.join("unpacked"))?;
    let line = match replace(exe, &binary) {
        Ok(()) => format!("azazel updated {CURRENT} -> {latest}; the next start runs it"),
        Err(why) => {
            format!("azazel {latest} is downloaded but was not installed ({why}); run `azazel update`")
        }
    };
    // Installed or not, the download is not tried again from here: `azazel update` is.
    let _ = std::fs::remove_dir_all(&root);
    Some(line)
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
/// blocking; the answer is shown the next time the UI opens. With `auto` (`update.auto`) a newer
/// release is also downloaded and verified, for [`apply_staged`] to install when the UI exits.
pub fn check_in_background(paths: &Paths, auto: bool) {
    let cached = read_cache(paths);
    let due = now_secs().saturating_sub(cached.checked_at) >= CHECK_EVERY.as_secs();
    // A newer release that is known but not here yet (the last download did not finish).
    let missing = auto && is_newer(&cached.latest) && !is_staged(paths, &cached.latest);
    if !due && !missing {
        return;
    }
    let paths = paths.clone();
    tokio::spawn(async move {
        let release = match latest_release(&client(CHECK_TIMEOUT)).await {
            Ok(release) => release,
            Err(why) => return tracing::debug!("update check failed: {why}"),
        };
        write_cache(
            &paths,
            &Cached {
                checked_at: now_secs(),
                latest: release.tag_name.trim_start_matches('v').to_string(),
            },
        );
        if auto
            && is_newer(&release.tag_name)
            && let Err(why) = stage(&client(DOWNLOAD_TIMEOUT), &release, &paths).await
        {
            tracing::debug!("could not download the update: {why}");
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
        assert!(!retired(&exe, 0).exists());
    }

    /// An azazel that waits for the one it started with `/restart` still runs from the binary
    /// the update before retired; the next update must not stop at it.
    #[cfg(windows)]
    #[test]
    fn a_retired_binary_that_is_still_in_use_is_passed_over() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("tool.exe");
        let new = dir.path().join("new");
        std::fs::write(&exe, b"second").unwrap();
        std::fs::write(&new, b"third").unwrap();
        std::fs::write(retired(&exe, 0), b"first").unwrap();
        // Open the way a running program holds its file: it cannot be deleted or replaced.
        let in_use = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(retired(&exe, 0))
            .unwrap();
        replace(&exe, &new).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"third");
        assert!(retired(&exe, 0).exists());
        assert!(!retired(&exe, 1).exists());
        drop(in_use);
        assert_eq!(std::fs::read(retired(&exe, 0)).unwrap(), b"first");
    }

    /// A release archive for this platform holding a binary with `content`, made with the
    /// system `tar` the way `cargo xtask dist` lays it out.
    fn archive(dir: &Path, content: &[u8]) -> Vec<u8> {
        let inner = dir.join("azazel-pack");
        std::fs::create_dir_all(&inner).unwrap();
        let name = if cfg!(windows) { "azazel.exe" } else { "azazel" };
        std::fs::write(inner.join(name), content).unwrap();
        let out = dir.join(asset_name());
        let program = if cfg!(windows) {
            PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32")
                .join("tar.exe")
        } else {
            PathBuf::from("tar")
        };
        let flags = if cfg!(windows) { "-acf" } else { "-czf" };
        let made = std::process::Command::new(program)
            .arg(flags)
            .arg(&out)
            .arg("-C")
            .arg(dir)
            .arg("azazel-pack")
            .status()
            .unwrap();
        assert!(made.success());
        std::fs::read(out).unwrap()
    }

    async fn release_server(tag: &str, archive: Vec<u8>, digest: String) -> (wiremock::MockServer, Release) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let name = asset_name();
        Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(archive))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/{name}.sha256")))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("{digest}  {name}\n")))
            .mount(&server)
            .await;
        let asset = |name: String| Asset {
            browser_download_url: format!("{}/{name}", server.uri()),
            name,
            url: String::new(),
        };
        let release = Release {
            tag_name: tag.to_string(),
            assets: vec![asset(name.clone()), asset(format!("{name}.sha256"))],
        };
        (server, release)
    }

    #[tokio::test]
    async fn a_download_with_a_wrong_checksum_is_not_staged_and_not_applied() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::discover(home.path(), Some(&home.path().to_string_lossy()));
        let exe = home.path().join("installed");
        std::fs::write(&exe, b"old").unwrap();
        let bytes = archive(home.path(), b"new");
        let (_server, release) = release_server("v999.0.0", bytes, "0".repeat(64)).await;

        let refused = stage(&client(CHECK_TIMEOUT), &release, &paths).await.unwrap_err();
        assert!(refused.contains("does not match its checksum"), "{refused}");
        assert!(!is_staged(&paths, "999.0.0"));
        assert!(!staging_root(&paths).exists());
        assert_eq!(apply_staged_to(&paths, &exe), None);
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }

    #[tokio::test]
    async fn a_verified_download_waits_in_the_cache_and_is_installed_once() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::discover(home.path(), Some(&home.path().to_string_lossy()));
        let exe = home.path().join("installed");
        std::fs::write(&exe, b"old").unwrap();
        let bytes = archive(home.path(), b"new");
        let digest = hex(&Sha256::digest(&bytes));
        let (server, release) = release_server("v999.0.0", bytes, digest).await;

        stage(&client(CHECK_TIMEOUT), &release, &paths).await.unwrap();
        assert!(is_staged(&paths, "999.0.0"));
        // Nothing is touched until the UI exits.
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        // Staging the same release again downloads nothing: the server could be gone.
        drop(server);
        stage(&client(CHECK_TIMEOUT), &release, &paths).await.unwrap();

        let line = apply_staged_to(&paths, &exe).unwrap();
        assert_eq!(
            line,
            format!("azazel updated {CURRENT} -> 999.0.0; the next start runs it")
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert!(!staging_root(&paths).exists());
        assert_eq!(apply_staged_to(&paths, &exe), None);
    }

    #[test]
    fn a_staged_release_that_is_not_newer_or_not_complete_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::discover(home.path(), Some(&home.path().to_string_lossy()));
        let exe = home.path().join("installed");
        std::fs::write(&exe, b"old").unwrap();
        let name = if cfg!(windows) { "azazel.exe" } else { "azazel" };
        // The running version, complete; and a newer one whose download never finished.
        for (staged, ready) in [(CURRENT, true), ("999.0.0", false)] {
            let dir = staging_root(&paths).join(staged);
            std::fs::create_dir_all(dir.join("unpacked")).unwrap();
            std::fs::write(dir.join("unpacked").join(name), b"new").unwrap();
            if ready {
                std::fs::write(dir.join(READY), staged).unwrap();
            }
        }
        assert!(!is_staged(&paths, "999.0.0"));
        assert_eq!(apply_staged_to(&paths, &exe), None);
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }
}
