//! Packs the built-in skills into one deflated blob for the binary; `builtin::skill_root` unpacks
//! it to disk the first time a build runs.
//!
//! The blob is a run of files in path order, each as its path relative to `skills/` (with `/`
//! between the parts) and its bytes, both preceded by their length as a little-endian `u32`.

use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::DeflateEncoder;

/// The files below `dir` as `(path relative to root, file)`.
fn files(root: &Path, dir: &Path, found: &mut Vec<(String, PathBuf)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            files(root, &path, found)?;
        } else if let Ok(relative) = path.strip_prefix(root) {
            let parts: Vec<_> = relative.iter().map(|part| part.to_string_lossy()).collect();
            found.push((parts.join("/"), path));
        }
    }
    Ok(())
}

fn sized(pack: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    let length = u32::try_from(bytes.len()).map_err(|_| std::io::ErrorKind::FileTooLarge)?;
    pack.write_all(&length.to_le_bytes())?;
    pack.write_all(bytes)
}

fn main() -> std::io::Result<()> {
    let root = Path::new("../../skills");
    println!("cargo:rerun-if-changed={}", root.display());
    println!("cargo:rerun-if-changed=build.rs");
    let out_dir = std::env::var_os("OUT_DIR").ok_or(std::io::ErrorKind::NotFound)?;
    let mut found = Vec::new();
    files(root, root, &mut found)?;
    found.sort();
    let mut pack = DeflateEncoder::new(Vec::new(), Compression::best());
    for (name, path) in &found {
        sized(&mut pack, name.as_bytes())?;
        sized(&mut pack, &std::fs::read(path)?)?;
    }
    std::fs::write(PathBuf::from(out_dir).join("skills.pack"), pack.finish()?)
}
