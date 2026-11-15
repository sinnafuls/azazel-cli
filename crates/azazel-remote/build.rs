//! Embeds the web page. `web/build/` (made by `bun --cwd web run build`) is gzipped file by file into
//! `OUT_DIR/assets.rs`; without it, a one-page placeholder stands in, so that Rust builds and tests
//! need no Node.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const PLACEHOLDER: &str = "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>azazel remote</title>\n\
<style>body{font:16px/1.5 system-ui,sans-serif;max-width:36rem;margin:3rem auto;padding:0 1rem}\
code{background:#8882;padding:.1em .3em;border-radius:.2em}</style>\n</head>\n<body>\n\
<h1>azazel remote</h1>\n<p>The web page was not built into this binary.</p>\n\
<p>Build it with <code>bun --cwd web run build</code>, then build azazel again.</p>\n</body>\n</html>\n";

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let build = manifest.join("../../web/build");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=AZAZEL_WEB_BUILD");
    let build = std::env::var_os("AZAZEL_WEB_BUILD")
        .map(PathBuf::from)
        .unwrap_or(build);
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    if build.join("index.html").is_file() {
        // Only a directory that exists is watched: a missing one would make every build run this
        // script again.
        println!("cargo:rerun-if-changed={}", build.display());
        walk(&build, &build, &mut files);
        files.sort();
    } else {
        files.push(("index.html".to_string(), PLACEHOLDER.as_bytes().to_vec()));
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let mut source = String::from("pub static ASSETS: &[Asset] = &[\n");
    for (index, (path, bytes)) in files.iter().enumerate() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(bytes).expect("gzip in memory");
        let gz = gz.finish().expect("gzip in memory");
        let name = format!("asset{index}.gz");
        std::fs::write(out.join(&name), gz).expect("write an asset");
        let digest = Sha256::digest(bytes);
        let mut etag = String::new();
        for byte in &digest[..8] {
            write!(etag, "{byte:02x}").expect("write to a string");
        }
        writeln!(
            source,
            "    Asset {{ path: {path:?}, mime: {:?}, gz: include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{name}\")), etag: \"\\\"{etag}\\\"\" }},",
            mime(path)
        )
        .expect("write to a string");
    }
    source.push_str("];\n");
    std::fs::write(out.join("assets.rs"), source).expect("write assets.rs");
}

fn walk(root: &Path, dir: &Path, files: &mut Vec<(String, Vec<u8>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, files);
        } else if let (Ok(relative), Ok(bytes)) = (path.strip_prefix(root), std::fs::read(&path)) {
            let relative = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            files.push((relative, bytes));
        }
    }
}

fn mime(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, extension)| extension) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
