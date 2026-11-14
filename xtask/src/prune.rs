//! `cargo xtask prune`: deletes what earlier builds left in `target/` and the newest build no
//! longer uses. Cargo keeps every version of a crate it ever built (each under its own hash), so
//! the build directory grows with every change of code, features or flags. For every crate this
//! keeps the version built last, and any version built in the last hour (one a build of another
//! feature set or profile still uses), and deletes the rest.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;

/// Versions built this recently are kept even when a newer one exists: another build may use them.
const RECENT: Duration = Duration::from_secs(60 * 60);

/// The directories of a profile whose entries are named `<crate>-<hash>`.
const HASHED: [&str; 4] = ["deps", "build", ".fingerprint", "incremental"];

/// The name of a build output without its hash, and the hash: `libfoo-0123456789abcdef.rlib` is
/// `("libfoo", "0123456789abcdef")`. `None` for a name that carries no hash.
pub fn split(name: &str) -> Option<(&str, &str)> {
    let dash = name.rfind('-')?;
    let hash = name[dash + 1..].split('.').next()?;
    (hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then(|| (&name[..dash], hash))
}

/// The entries of `dir` to delete: for each name, every hash but the newest one and those built
/// within [`RECENT`] of `now`.
pub fn stale(dir: &Path, now: SystemTime) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    // name -> hash -> (newest modification, entries)
    let mut groups: HashMap<String, HashMap<String, (SystemTime, Vec<PathBuf>)>> = HashMap::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some((name, hash)) = file_name.to_str().and_then(split) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let version = groups
            .entry(name.to_string())
            .or_default()
            .entry(hash.to_string())
            .or_insert((SystemTime::UNIX_EPOCH, Vec::new()));
        version.0 = version.0.max(modified);
        version.1.push(entry.path());
    }
    let mut out = Vec::new();
    for versions in groups.into_values() {
        let newest = versions.values().map(|(modified, _)| *modified).max();
        for (modified, paths) in versions.into_values() {
            let recent = now.duration_since(modified).is_ok_and(|age| age < RECENT);
            if Some(modified) != newest && !recent {
                out.extend(paths);
            }
        }
    }
    out
}

/// The profile directories under `target`: `debug`, `release`, and those of other targets and
/// custom target directories (`target/<triple>/release`, `target/size/release`).
fn profiles(target: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(target) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join("deps").is_dir() || path.join(".fingerprint").is_dir() {
            out.push(path);
        } else {
            out.extend(profiles(&path));
        }
    }
    out
}

fn size(path: &Path) -> u64 {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::read_dir(path)
            .map(|entries| entries.flatten().map(|entry| size(&entry.path())).sum())
            .unwrap_or(0),
        Ok(meta) => meta.len(),
        Err(_) => 0,
    }
}

/// Deletes the stale build output under `target` and returns the bytes freed and what could not
/// be deleted (a file a running program holds).
pub fn prune(target: &Path, dry_run: bool) -> (u64, Vec<String>) {
    let now = SystemTime::now();
    let mut freed = 0;
    let mut failed = Vec::new();
    for profile in profiles(target) {
        for dir in HASHED {
            for path in stale(&profile.join(dir), now) {
                let bytes = size(&path);
                let removed = match dry_run {
                    true => Ok(()),
                    false if path.is_dir() => std::fs::remove_dir_all(&path),
                    false => std::fs::remove_file(&path),
                };
                match removed {
                    Ok(()) => freed += bytes,
                    Err(e) => failed.push(format!("{}: {e}", path.display())),
                }
            }
        }
    }
    (freed, failed)
}

pub fn run(root: &Path, args: &[String]) -> Result<()> {
    let dry_run = args.iter().any(|arg| arg == "--dry-run");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let (freed, failed) = prune(&target, dry_run);
    let verb = if dry_run { "would free" } else { "freed" };
    println!(
        "prune: {verb} {:.1} GB in {}",
        freed as f64 / 1e9,
        target.display()
    );
    for line in failed.iter().take(10) {
        eprintln!("prune: could not delete {line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_split_at_their_hash() {
        assert_eq!(
            split("libfoo-0123456789abcdef.rlib"),
            Some(("libfoo", "0123456789abcdef"))
        );
        assert_eq!(
            split("azazel-0123456789abcdef.exe"),
            Some(("azazel", "0123456789abcdef"))
        );
        assert_eq!(
            split("azazel_cli-0123456789abcdef"),
            Some(("azazel_cli", "0123456789abcdef"))
        );
        assert_eq!(split("azazel.exe"), None);
        assert_eq!(split("foo-bar.d"), None);
    }

    #[test]
    fn only_old_versions_replaced_by_a_newer_one_go() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, age_hours: u64| {
            let path = dir.path().join(name);
            std::fs::write(&path, b"x").unwrap();
            let when = SystemTime::now() - Duration::from_secs(age_hours * 3600);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(when)
                .unwrap();
        };
        // Three builds of foo: the newest stays, one from 30 minutes ago would stay too, the old
        // one goes with all its files. A crate with one version and other files are untouched.
        file("libfoo-aaaaaaaaaaaaaaaa.rlib", 0);
        file("libfoo-bbbbbbbbbbbbbbbb.rlib", 5);
        file("libfoo-bbbbbbbbbbbbbbbb.rmeta", 5);
        file("libbar-cccccccccccccccc.rlib", 48);
        file("azazel.exe", 48);
        let mut gone: Vec<String> = stale(dir.path(), SystemTime::now())
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        gone.sort();
        assert_eq!(
            gone,
            ["libfoo-bbbbbbbbbbbbbbbb.rlib", "libfoo-bbbbbbbbbbbbbbbb.rmeta"]
        );

        file("libfoo-dddddddddddddddd.rlib", 0);
        let recent: Vec<_> = stale(dir.path(), SystemTime::now() + Duration::from_secs(30 * 60))
            .into_iter()
            .filter(|path| path.to_string_lossy().contains("dddd") || path.to_string_lossy().contains("aaaa"))
            .collect();
        assert!(recent.is_empty(), "{recent:?}");
    }
}
