//! Packages from the npm registry: `npm:<name>[@<version or tag>]`.
//!
//! The registry is asked what the name offers, the tarball of the chosen version is downloaded
//! and checked against the SHA-512 the registry lists for it, and its files are unpacked. Nothing
//! of the package runs: no install scripts, and no dependencies are fetched.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha512};

/// Where packages come from unless `NPM_CONFIG_REGISTRY` names another registry.
pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

/// A larger tarball is not downloaded.
const MAX_TARBALL: usize = 64 * 1024 * 1024;
/// An archive that unpacks to more than this is refused.
const MAX_UNPACKED: u64 = 512 * 1024 * 1024;
/// A path or other header record longer than this is not a package's.
const MAX_RECORD: u64 = 64 * 1024;
/// An archive with more entries than this is not a package.
const MAX_ENTRIES: u32 = 100_000;
const TIMEOUT: Duration = Duration::from_secs(120);
const BLOCK: usize = 512;

/// The registry this process installs from: what npm itself reads from the environment, or the
/// public one.
pub fn registry(env: &dyn Fn(&str) -> Option<String>) -> String {
    ["NPM_CONFIG_REGISTRY", "npm_config_registry"]
        .into_iter()
        .filter_map(env)
        .map(|url| url.trim().trim_end_matches('/').to_string())
        .find(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string())
}

/// The package name and the wanted version or tag of `left-pad`, `left-pad@1.3.0` or
/// `@scope/name@beta`.
pub fn parse(text: &str) -> Result<(String, Option<String>), String> {
    let text = text.trim();
    // The `@` of a scope is the first character; one after it separates the version.
    let (name, wanted) = match text.char_indices().skip(1).find(|(_, c)| *c == '@') {
        Some((at, _)) => (&text[..at], Some(&text[at + 1..])),
        None => (text, None),
    };
    let part = |part: &str| {
        !part.is_empty()
            && !part.starts_with(['.', '_'])
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '_' | '~'))
    };
    let named = match name.strip_prefix('@') {
        Some(scoped) => scoped
            .split_once('/')
            .is_some_and(|(scope, name)| part(scope) && part(name)),
        None => part(name),
    };
    if !named || name.len() > 214 || wanted.is_some_and(|wanted| wanted.is_empty() || wanted.contains('/')) {
        return Err(format!(
            "`npm:{text}` is not of the form npm:<name>[@<version or tag>], such as npm:left-pad@1.3.0 or \
             npm:@scope/name"
        ));
    }
    Ok((name.to_string(), wanted.map(str::to_string)))
}

/// One published version of a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    /// Where its tarball is.
    pub tarball: String,
    /// The base64 SHA-512 of the tarball, as the registry lists it.
    pub sha512: String,
}

/// The version of `name` that `wanted` (an exact version or a tag; `latest` when `None`) names in
/// the registry's description of the package.
fn release(packument: &Value, name: &str, wanted: Option<&str>) -> Result<Release, String> {
    let tags = packument.get("dist-tags").and_then(Value::as_object);
    let versions = packument.get("versions").and_then(Value::as_object);
    let tagged = |tag: &str| tags.and_then(|tags| tags.get(tag)).and_then(Value::as_str);
    let wanted = wanted.unwrap_or("latest");
    let version = match versions.and_then(|versions| versions.get(wanted)) {
        Some(_) => wanted,
        None => tagged(wanted).ok_or_else(|| {
            let known: Vec<String> = tags
                .into_iter()
                .flatten()
                .map(|(tag, version)| format!("{tag} ({})", version.as_str().unwrap_or("?")))
                .collect();
            match known.as_slice() {
                [] => format!("{name} has no version or tag `{wanted}`"),
                known => format!(
                    "{name} has no version or tag `{wanted}`; give an exact version, or one of its tags: {}",
                    known.join(", ")
                ),
            }
        })?,
    };
    let dist = versions
        .and_then(|versions| versions.get(version))
        .and_then(|listed| listed.get("dist"));
    let field = |key: &str| dist.and_then(|dist| dist.get(key)).and_then(Value::as_str);
    let tarball =
        field("tarball").ok_or_else(|| format!("the registry lists no tarball for {name}@{version}"))?;
    // Subresource-integrity form: one or more `<algorithm>-<base64>`, separated by spaces.
    let sha512 = field("integrity")
        .into_iter()
        .flat_map(str::split_whitespace)
        .find_map(|hash| hash.strip_prefix("sha512-"))
        .ok_or_else(|| {
            format!("the registry lists no SHA-512 for {name}@{version}, so its tarball cannot be checked")
        })?;
    Ok(Release {
        version: version.to_string(),
        tarball: tarball.to_string(),
        sha512: sha512.to_string(),
    })
}

/// Whether `tarball` is the file the registry described.
fn verify(tarball: &[u8], sha512: &str) -> bool {
    base64::engine::general_purpose::STANDARD
        .decode(sha512)
        .is_ok_and(|expected| expected.as_slice() == Sha512::digest(tarball).as_slice())
}

fn client() -> Result<reqwest::Client, String> {
    azazel_ai::tls::client_builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("azazel/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())
}

async fn get(client: &reqwest::Client, url: &str, accept: &str) -> Result<reqwest::Response, String> {
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, accept)
        .send()
        .await
        .map_err(|e| format!("{url}: {}", e.without_url()))?;
    match response.status() {
        status if status.is_success() => Ok(response),
        reqwest::StatusCode::NOT_FOUND => Err(format!("{url}: not found")),
        status => Err(format!("{url}: {status}")),
    }
}

async fn find_release(
    client: &reqwest::Client,
    registry: &str,
    name: &str,
    wanted: Option<&str>,
) -> Result<Release, String> {
    // The short form of the description: versions and where their files are, nothing else.
    const ABBREVIATED: &str = "application/vnd.npm.install-v1+json, application/json;q=0.8";
    let url = format!("{registry}/{}", name.replace('/', "%2F"));
    let packument: Value = get(client, &url, ABBREVIATED)
        .await
        .map_err(|why| match why.ends_with(": not found") {
            true => format!("the registry has no package named {name}"),
            false => why,
        })?
        .json()
        .await
        .map_err(|e| format!("{url}: {}", e.without_url()))?;
    release(&packument, name, wanted)
}

async fn download(client: &reqwest::Client, release: &Release) -> Result<Vec<u8>, String> {
    let url = &release.tarball;
    let mut response = get(client, url, "application/octet-stream").await?;
    let mut tarball = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("{url}: {}", e.without_url()))?
    {
        if tarball.len() + chunk.len() > MAX_TARBALL {
            return Err(format!("{url} is larger than {} MB", MAX_TARBALL / (1024 * 1024)));
        }
        tarball.extend_from_slice(&chunk);
    }
    if !verify(&tarball, &release.sha512) {
        return Err(format!(
            "{url} is not the file the registry describes: its SHA-512 differs"
        ));
    }
    Ok(tarball)
}

/// Runs `work` to its end on a thread of its own, so that it can be waited for from inside an
/// async runtime as well as outside one.
fn wait<T: Send>(work: impl Future<Output = Result<T, String>> + Send) -> Result<T, String> {
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?
                .block_on(work)
        });
        worker
            .join()
            .unwrap_or_else(|_| Err("the download stopped unexpectedly".to_string()))
    })
}

/// The version `wanted` names now, without downloading it.
pub fn find(registry: &str, name: &str, wanted: Option<&str>) -> Result<Release, String> {
    wait(async { find_release(&client()?, registry, name, wanted).await })
}

/// Downloads `release`, checks it and unpacks it into the new directory `into`. Nothing is left
/// behind when it fails.
pub fn install(release: &Release, into: &Path) -> Result<(), String> {
    let tarball = wait(async { download(&client()?, release).await })?;
    unpack(&tarball, into).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(into);
    })
}

/// A number as tar headers write it: octal digits, ended by a space or a NUL. A field left empty
/// is zero.
fn octal(field: &[u8]) -> Option<u64> {
    let digits: Vec<u8> = field
        .iter()
        .copied()
        .skip_while(|b| *b == b' ')
        .take_while(|b| b.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return field.iter().all(|b| matches!(b, 0 | b' ')).then_some(0);
    }
    u64::from_str_radix(std::str::from_utf8(&digits).ok()?, 8).ok()
}

/// The text of a header field up to its NUL.
fn text(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Whether `header` carries the checksum of its own bytes, which a block that is not a header
/// does not.
fn is_header(header: &[u8; BLOCK]) -> bool {
    let sum: u64 = header
        .iter()
        .enumerate()
        .map(|(at, byte)| match at {
            // The checksum counts its own field as spaces.
            148..156 => u64::from(b' '),
            _ => u64::from(*byte),
        })
        .sum();
    octal(&header[148..156]) == Some(sum)
}

/// The `path` of a pax extended header, whose records are `<length> <key>=<value>\n`.
fn pax_path(mut records: &[u8]) -> Option<String> {
    while !records.is_empty() {
        let space = records.iter().position(|b| *b == b' ')?;
        let length: usize = std::str::from_utf8(&records[..space]).ok()?.parse().ok()?;
        if length <= space || length > records.len() {
            return None;
        }
        let record = &records[space + 1..length];
        if let Some(path) = record.strip_prefix(b"path=") {
            let path = String::from_utf8_lossy(path);
            return Some(path.strip_suffix('\n').unwrap_or(&path).to_string());
        }
        records = &records[length..];
    }
    None
}

/// Where the archive entry `name` goes below `into`: its path without the directory every entry
/// of a package tarball is in. `None` for that directory itself; an error for a path that would
/// leave `into`.
fn target(into: &Path, name: &str) -> Result<Option<PathBuf>, String> {
    let outside = || format!("the archive holds `{name}`, which is not a path inside the package");
    let inner = match name.trim_start_matches("./").split_once('/') {
        Some((_, inner)) if !inner.is_empty() => inner,
        _ => return Ok(None),
    };
    // A colon names a drive or a hidden stream of a file on Windows.
    if inner.contains(['\\', '\0']) || (cfg!(windows) && inner.contains(':')) {
        return Err(outside());
    }
    let mut path = into.to_path_buf();
    for part in Path::new(inner).components() {
        match part {
            Component::CurDir => {}
            Component::Normal(part) => path.push(part),
            _ => return Err(outside()),
        }
    }
    Ok(Some(path))
}

/// The archive being read: the unpacked bytes of the tarball, up to the most a package may
/// unpack to.
type Archive<'a> = std::io::Take<flate2::read::GzDecoder<&'a [u8]>>;

/// Why the archive ended before it should have.
fn ended(archive: &Archive) -> String {
    match archive.limit() {
        0 => format!(
            "the archive unpacks to more than {} MB",
            MAX_UNPACKED / (1024 * 1024)
        ),
        _ => "the archive is cut short".to_string(),
    }
}

/// Reads exactly one block; false at the end of the archive.
fn read_block(archive: &mut Archive, block: &mut [u8; BLOCK]) -> Result<bool, String> {
    let mut filled = 0;
    while filled < BLOCK {
        match archive.read(&mut block[filled..]) {
            Ok(0) if filled == 0 && archive.limit() > 0 => return Ok(false),
            Ok(0) => return Err(ended(archive)),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("the archive cannot be read: {e}")),
        }
    }
    Ok(true)
}

/// Moves the next `count` bytes of the archive into `to`.
fn pour(archive: &mut Archive, count: u64, to: &mut impl Write) -> Result<(), String> {
    match std::io::copy(&mut archive.by_ref().take(count), to) {
        Ok(moved) if moved == count => Ok(()),
        Ok(_) => Err(ended(archive)),
        Err(e) => Err(format!("the archive cannot be unpacked: {e}")),
    }
}

/// Unpacks the files of a gzipped tar archive into `into`, leaving out the top directory npm
/// wraps a package in. Only plain files are written: links and devices are skipped, and a path
/// that would leave `into` fails the whole archive.
fn unpack(tarball: &[u8], into: &Path) -> Result<(), String> {
    let fail = |path: &Path, e: std::io::Error| format!("{}: {e}", path.display());
    // Everything read counts, so that an archive of entries that are skipped ends as well.
    let mut archive: Archive = flate2::read::GzDecoder::new(tarball).take(MAX_UNPACKED);
    let mut header = [0u8; BLOCK];
    // A path too long for a header comes in an entry of its own before the entry it names.
    let mut long_path: Option<String> = None;
    let mut entries = 0;
    std::fs::create_dir_all(into).map_err(|e| fail(into, e))?;
    while read_block(&mut archive, &mut header)? {
        if header.iter().all(|b| *b == 0) {
            break;
        }
        if !is_header(&header) {
            return Err("the archive is not a tar archive".to_string());
        }
        entries += 1;
        if entries > MAX_ENTRIES {
            return Err(format!("the archive has more than {MAX_ENTRIES} entries"));
        }
        let size = octal(&header[124..136]).ok_or("the archive has an entry of unreadable size")?;
        let padding = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
        match header[156] {
            kind @ (b'x' | b'L') => {
                if size > MAX_RECORD {
                    return Err("the archive has a path that is too long".to_string());
                }
                let mut record = Vec::new();
                pour(&mut archive, size, &mut record)?;
                long_path = match kind {
                    b'x' => pax_path(&record).or(long_path),
                    _ => Some(text(&record)),
                };
            }
            // Settings for the entries that follow; none of them matters here.
            b'g' => pour(&mut archive, size, &mut std::io::sink())?,
            b'0' | 0 | b'7' => {
                let name = long_path.take().unwrap_or_else(|| {
                    let name = text(&header[..100]);
                    // The first half of a long path, in archives of the POSIX format.
                    match &header[257..263] == b"ustar\0" {
                        true if header[345] != 0 => format!("{}/{name}", text(&header[345..500])),
                        _ => name,
                    }
                });
                // Old archives write a directory as a file whose name ends in a slash.
                match target(into, &name)?.filter(|_| !name.ends_with('/')) {
                    Some(path) => {
                        if let Some(dir) = path.parent() {
                            std::fs::create_dir_all(dir).map_err(|e| fail(dir, e))?;
                        }
                        let mut file = std::fs::File::create(&path).map_err(|e| fail(&path, e))?;
                        pour(&mut archive, size, &mut file)?;
                        #[cfg(unix)]
                        if octal(&header[100..108]).is_some_and(|mode| mode & 0o111 != 0) {
                            use std::os::unix::fs::PermissionsExt;
                            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
                        }
                    }
                    None => pour(&mut archive, size, &mut std::io::sink())?,
                }
            }
            _ => {
                long_path = None;
                pour(&mut archive, size, &mut std::io::sink())?;
            }
        }
        pour(&mut archive, padding, &mut std::io::sink())?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod testing {
    use std::io::Write;

    use super::*;

    /// One entry of a tar archive: a header block, then the body padded to whole blocks.
    pub fn entry(name: &str, kind: u8, body: &[u8]) -> Vec<u8> {
        entry_in("", name, kind, body)
    }

    /// An entry whose path is `prefix`, when there is one, then `name`: how the POSIX format
    /// writes a path too long for the name alone.
    pub fn entry_in(prefix: &str, name: &str, kind: u8, body: &[u8]) -> Vec<u8> {
        let mut header = [0u8; BLOCK];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        header[100..107].copy_from_slice(b"0000755");
        header[124..135].copy_from_slice(format!("{:011o}", body.len()).as_bytes());
        header[156] = kind;
        header[257..263].copy_from_slice(b"ustar\0");
        header[148..156].fill(b' ');
        let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        header[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        let mut out = header.to_vec();
        out.extend_from_slice(body);
        out.resize(out.len() + (BLOCK - body.len() % BLOCK) % BLOCK, 0);
        out
    }

    /// A gzipped archive of `entries`, ended the way tar ends one.
    pub fn gzipped(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        for entry in entries {
            encoder.write_all(entry).unwrap();
        }
        encoder.write_all(&[0u8; 2 * BLOCK]).unwrap();
        encoder.finish().unwrap()
    }

    /// A package tarball as `npm pack` lays it out: every file below `package/`.
    pub fn tarball(files: &[(&str, &str)]) -> Vec<u8> {
        let entries: Vec<Vec<u8>> = files
            .iter()
            .map(|(name, body)| entry(&format!("package/{name}"), b'0', body.as_bytes()))
            .collect();
        gzipped(&entries)
    }

    /// The registry's description of `name` with one version, tagged `latest`.
    pub fn packument(name: &str, version: &str, tarball_url: &str, tarball: &[u8]) -> Value {
        let sha512 = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(tarball));
        serde_json::json!({
            "name": name,
            "dist-tags": { "latest": version },
            "versions": {
                version: { "dist": { "tarball": tarball_url, "integrity": format!("sha512-{sha512}") } },
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{entry, entry_in, gzipped, packument, tarball};
    use super::*;

    #[test]
    fn names_versions_and_the_registry_are_read() {
        assert_eq!(parse("left-pad").unwrap(), ("left-pad".to_string(), None));
        assert_eq!(
            parse("left-pad@1.3.0").unwrap(),
            ("left-pad".to_string(), Some("1.3.0".to_string()))
        );
        assert_eq!(parse("@scope/pack").unwrap(), ("@scope/pack".to_string(), None));
        assert_eq!(
            parse("@scope/pack@beta").unwrap(),
            ("@scope/pack".to_string(), Some("beta".to_string()))
        );
        for bad in [
            "", "@scope", "@/name", "Upper", "a/b", "name@", "../etc", "name@1/2", ".hidden", "a b",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }

        let none = |_: &str| None;
        assert_eq!(registry(&none), DEFAULT_REGISTRY);
        let set =
            |name: &str| (name == "npm_config_registry").then(|| " http://localhost:4873/ ".to_string());
        assert_eq!(registry(&set), "http://localhost:4873");
    }

    #[test]
    fn a_version_is_chosen_by_number_or_tag_and_must_have_a_sha512() {
        let description = serde_json::json!({
            "dist-tags": { "latest": "1.1.0", "next": "2.0.0-rc.1" },
            "versions": {
                "1.0.0": { "dist": { "tarball": "https://r/p-1.0.0.tgz", "integrity": "sha1-AAAA sha512-ONE" } },
                "1.1.0": { "dist": { "tarball": "https://r/p-1.1.0.tgz", "integrity": "sha512-TWO" } },
                "2.0.0-rc.1": { "dist": { "tarball": "https://r/p-2.tgz", "shasum": "abc" } },
            },
        });
        let latest = release(&description, "p", None).unwrap();
        assert_eq!(
            (latest.version.as_str(), latest.sha512.as_str()),
            ("1.1.0", "TWO")
        );
        let exact = release(&description, "p", Some("1.0.0")).unwrap();
        assert_eq!(
            (exact.tarball.as_str(), exact.sha512.as_str()),
            ("https://r/p-1.0.0.tgz", "ONE")
        );
        let unchecked = release(&description, "p", Some("next")).unwrap_err();
        assert!(unchecked.contains("no SHA-512 for p@2.0.0-rc.1"), "{unchecked}");
        let unknown = release(&description, "p", Some("^1.0")).unwrap_err();
        assert!(
            unknown.contains("no version or tag `^1.0`")
                && unknown.contains("latest (1.1.0), next (2.0.0-rc.1)"),
            "{unknown}"
        );
    }

    #[test]
    fn files_are_unpacked_without_the_top_directory_and_links_are_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let into = dir.path().join("out");
        let long = format!("package/{}/deep.txt", "d".repeat(120));
        let pax = format!("path={long}\n");
        let pax = format!("{} {pax}", pax.len() + 4);
        assert_eq!(
            pax.len(),
            pax.split(' ').next().unwrap().parse::<usize>().unwrap()
        );
        let prefixed = entry_in("package", "skills/a/SKILL.md", b'0', b"skill");
        let archive = gzipped(&[
            entry("package/", b'5', b""),
            entry("package/old-dir/", b'0', b""),
            entry("package/old-dir/./inside.txt", b'0', b"in"),
            entry("package/azazel.toml", b'0', b"skills = [\"skills\"]\n"),
            prefixed,
            entry("package/link", b'2', b""),
            entry("PaxHeader/deep.txt", b'x', pax.as_bytes()),
            entry("package/short-name", b'0', b"deep"),
            entry("././@LongLink", b'L', b"package/gnu-long.txt\0"),
            entry("package/cut", 0, &[b'z'; 700]),
        ]);
        unpack(&archive, &into).unwrap();
        let read = |file: &str| std::fs::read_to_string(into.join(file)).unwrap();
        assert_eq!(read("azazel.toml"), "skills = [\"skills\"]\n");
        assert_eq!(read("skills/a/SKILL.md"), "skill");
        assert_eq!(read("old-dir/inside.txt"), "in");
        assert_eq!(read(&format!("{}/deep.txt", "d".repeat(120))), "deep");
        assert_eq!(read("gnu-long.txt").len(), 700);
        assert!(!into.join("link").exists() && !into.join("short-name").exists());
    }

    #[test]
    fn an_archive_that_reaches_outside_or_is_cut_short_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "package/../../escaped.txt",
            "package//etc/escaped.txt",
            "package/a\\..\\..\\escaped.txt",
            #[cfg(windows)]
            "package/C:escaped.txt",
            #[cfg(windows)]
            "package/notes.txt:hidden",
        ] {
            let into = dir.path().join("out");
            let why = unpack(&gzipped(&[entry(name, b'0', b"x")]), &into).unwrap_err();
            assert!(why.contains("not a path inside the package"), "{name}: {why}");
        }
        assert!(!dir.path().join("escaped.txt").exists());
        // A long path may carry what a header cannot: a NUL.
        let record = "26 path=package/a\0b/c.txt\n";
        let named = [
            entry("PaxHeader/c.txt", b'x', record.as_bytes()),
            entry("package/c.txt", b'0', b"x"),
        ];
        let why = unpack(&gzipped(&named), &dir.path().join("nul")).unwrap_err();
        assert!(why.contains("not a path inside the package"), "{why}");

        let whole = tarball(&[("a.txt", &"a".repeat(2000))]);
        let mut plain = Vec::new();
        flate2::read::GzDecoder::new(whole.as_slice())
            .read_to_end(&mut plain)
            .unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&plain[..BLOCK + 1000]).unwrap();
        let cut = encoder.finish().unwrap();
        let why = unpack(&cut, &dir.path().join("cut")).unwrap_err();
        assert!(why.contains("cut short"), "{why}");
        assert!(unpack(b"not gzip", &dir.path().join("bad")).is_err());

        // A block that is not a header where one belongs.
        let mut stray = entry("package/a.txt", b'0', b"x");
        stray[0] ^= 0xff;
        let why = unpack(&gzipped(&[stray]), &dir.path().join("stray")).unwrap_err();
        assert_eq!(why, "the archive is not a tar archive");
        // Far more than a package holds, in entries that write nothing: it ends at the limit.
        let link = entry("package/link", b'2', b"");
        let mut size = link.clone();
        size[124..135].copy_from_slice(b"77777777777");
        size[148..156].fill(b' ');
        let sum: u32 = size.iter().map(|byte| u32::from(*byte)).sum();
        size[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&size).unwrap();
        let zeros = vec![0u8; 1024 * 1024];
        for _ in 0..=MAX_UNPACKED / zeros.len() as u64 {
            encoder.write_all(&zeros).unwrap();
        }
        let bomb = encoder.finish().unwrap();
        let why = unpack(&bomb, &dir.path().join("bomb")).unwrap_err();
        assert!(why.contains("unpacks to more than 512 MB"), "{why}");
    }

    #[tokio::test]
    async fn a_tarball_is_installed_only_when_it_is_the_one_the_registry_describes() {
        use wiremock::matchers::{header_regex, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let good = tarball(&[("SKILL.md", "body")]);
        let url = |file: &str| format!("{}/files/{file}", server.uri());
        Mock::given(method("GET"))
            .and(path("/@scope%2Fpack"))
            .and(header_regex("accept", r"application/vnd\.npm\.install-v1\+json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(packument(
                "@scope/pack",
                "1.2.3",
                &url("good.tgz"),
                &good,
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/files/good.tgz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(good.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/files/other.tgz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball(&[("SKILL.md", "tampered")])))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let registry = server.uri();
        // Called the way the command line calls it: from inside a runtime.
        let found = find(&registry, "@scope/pack", None).unwrap();
        assert_eq!(found.version, "1.2.3");
        install(&found, &dir.path().join("good")).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("good/SKILL.md")).unwrap(),
            "body"
        );

        let swapped = Release {
            tarball: url("other.tgz"),
            ..found
        };
        let why = install(&swapped, &dir.path().join("bad")).unwrap_err();
        assert!(why.contains("its SHA-512 differs"), "{why}");
        assert!(!dir.path().join("bad").exists());

        let missing = find(&registry, "absent", None).unwrap_err();
        assert_eq!(missing, "the registry has no package named absent");
    }
}
