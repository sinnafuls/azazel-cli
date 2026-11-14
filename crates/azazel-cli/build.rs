//! Deflates the changelog that ships inside the binary; `changelog::shipped` inflates it.

use std::io::Write;
use std::path::PathBuf;

use flate2::Compression;
use flate2::write::DeflateEncoder;

fn main() -> std::io::Result<()> {
    const SOURCE: &str = "../../CHANGELOG.md";
    println!("cargo:rerun-if-changed={SOURCE}");
    println!("cargo:rerun-if-changed=build.rs");
    let out_dir = std::env::var_os("OUT_DIR").ok_or(std::io::ErrorKind::NotFound)?;
    let mut packed = DeflateEncoder::new(Vec::new(), Compression::best());
    packed.write_all(&std::fs::read(SOURCE)?)?;
    std::fs::write(
        PathBuf::from(out_dir).join("CHANGELOG.md.deflate"),
        packed.finish()?,
    )
}
