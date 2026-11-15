//! The web page, embedded by `build.rs`: each file gzipped, served as it is to a browser that
//! accepts gzip and inflated for one that does not.

use std::io::Read;

pub(crate) struct Asset {
    pub path: &'static str,
    pub mime: &'static str,
    pub gz: &'static [u8],
    pub etag: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

/// The asset a request path names. A path that names no file gets `index.html`, so that the page
/// can route by itself; a missing file of the build output (`/_app/…`) or with an extension is
/// not a page and gets nothing.
pub(crate) fn find(path: &str) -> Option<&'static Asset> {
    let wanted = path.trim_start_matches('/');
    let wanted = if wanted.is_empty() { "index.html" } else { wanted };
    if let Some(asset) = ASSETS.iter().find(|asset| asset.path == wanted) {
        return Some(asset);
    }
    let last = wanted.rsplit('/').next().unwrap_or_default();
    if wanted.starts_with("_app/") || last.contains('.') {
        return None;
    }
    ASSETS.iter().find(|asset| asset.path == "index.html")
}

/// How long a browser may keep a file: the build names the files under `_app/immutable/` by what
/// they hold, so they never change; everything else is checked each time.
pub(crate) fn cache_control(asset: &Asset) -> &'static str {
    if asset.path.starts_with("_app/immutable/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

pub(crate) fn inflate(asset: &Asset) -> Vec<u8> {
    let mut out = Vec::new();
    // The bytes were made by the build script from a file it read; they are a valid gzip stream.
    if flate2::read::GzDecoder::new(asset.gz)
        .read_to_end(&mut out)
        .is_err()
    {
        out.clear();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_and_unknown_pages_are_the_index_and_unknown_files_are_not() {
        assert_eq!(find("/").map(|a| a.path), Some("index.html"));
        assert_eq!(find("/s/abc123").map(|a| a.path), Some("index.html"));
        assert!(find("/_app/immutable/missing.js").is_none());
        assert!(find("/missing.png").is_none());
    }

    #[test]
    fn an_asset_inflates_to_what_was_embedded() {
        let index = find("/").unwrap();
        let text = String::from_utf8(inflate(index)).unwrap();
        assert!(text.contains("<html"));
        assert_eq!(cache_control(index), "no-cache");
    }
}
