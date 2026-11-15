//! Walks the project and keeps what was read from each file, so that a second run parses only the
//! files that changed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};

use crate::extract::{Lang, Tags, extract};

/// Files larger than this are data or generated code, not something to outline.
const MAX_FILE_BYTES: u64 = 512 * 1024;
/// Bump when what `extract` produces changes, so that an old cache is not trusted.
const CACHE_VERSION: u32 = 1;
/// A generated file says so near its top.
const GENERATED_MARKERS: [&str; 4] = ["@generated", "DO NOT EDIT", "Code generated", "auto-generated"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    mtime: u64,
    size: u64,
    /// `None` for a file that was looked at and has nothing to outline (generated, unreadable).
    tags: Option<Tags>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    version: u32,
    files: BTreeMap<String, Entry>,
}

/// What one scan did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Files read and parsed because they were new or had changed.
    pub parsed: usize,
    /// Files whose earlier result was used.
    pub cached: usize,
}

/// A source file and what it holds.
#[derive(Debug, Clone)]
pub struct FileTags {
    /// Relative to the project root, with `/`.
    pub path: String,
    pub tags: Tags,
}

fn load(path: &Path) -> Cache {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Cache>(&bytes).ok())
        .filter(|cache| cache.version == CACHE_VERSION)
        .unwrap_or_default()
}

fn save(path: &Path, cache: &Cache) {
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let temp = path.with_extension("tmp");
        fs::write(&temp, serde_json::to_vec(cache).map_err(std::io::Error::other)?)?;
        fs::rename(&temp, path)
    };
    if let Err(why) = write() {
        tracing::warn!("the repository map cache could not be written: {why}");
    }
}

fn modified(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos() as u64)
}

fn read(path: &Path, lang: Lang) -> Option<Tags> {
    let source = String::from_utf8(fs::read(path).ok()?).ok()?;
    let head: String = source.chars().take(600).collect();
    if GENERATED_MARKERS.iter().any(|marker| head.contains(marker)) {
        return None;
    }
    extract(lang, &source)
}

/// The source files under `root` that git and `.ignore` do not leave out, read through `cache_file`.
pub fn scan(root: &Path, cache_file: &Path) -> (Vec<FileTags>, Stats) {
    let old = load(cache_file);
    let mut fresh = Cache {
        version: CACHE_VERSION,
        files: BTreeMap::new(),
    };
    let mut stats = Stats::default();
    let mut walk = WalkBuilder::new(root);
    walk.require_git(false);
    let mut found: Vec<(String, PathBuf, Lang, u64, u64)> = Vec::new();
    for entry in walk.build().flatten() {
        let path = entry.path();
        let Some(lang) = Lang::of(path) else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let name = relative.to_string_lossy().replace('\\', "/");
        found.push((name, path.to_path_buf(), lang, modified(&meta), meta.len()));
    }
    found.sort();
    let mut stale = Vec::new();
    for (name, path, lang, mtime, size) in found {
        match old.files.get(&name) {
            Some(entry) if entry.mtime == mtime && entry.size == size => {
                stats.cached += 1;
                fresh.files.insert(name, entry.clone());
            }
            _ => stale.push((name, path, lang, mtime, size)),
        }
    }
    stats.parsed = stale.len();
    // Parsing is the cost of a cold run, and every file is independent.
    let workers = std::thread::available_parallelism().map_or(2, usize::from).min(8);
    let per = stale.len().div_ceil(workers.max(1)).max(1);
    let parsed: Vec<(String, Entry)> = std::thread::scope(|scope| {
        let handles: Vec<_> = stale
            .chunks(per)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|(name, path, lang, mtime, size)| {
                            let entry = Entry {
                                mtime: *mtime,
                                size: *size,
                                tags: read(path, *lang),
                            };
                            (name.clone(), entry)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    fresh.files.extend(parsed);
    if stats.parsed > 0 || fresh.files.len() != old.files.len() {
        save(cache_file, &fresh);
    }
    let files = fresh
        .files
        .into_iter()
        .filter_map(|(path, entry)| entry.tags.map(|tags| FileTags { path, tags }))
        .collect();
    (files, stats)
}
