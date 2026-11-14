//! Deflates the model catalogue that ships inside the binary: three quarters of a megabyte of
//! JSON, a fifth of that packed. `Catalog::embedded` inflates it.

use std::io::Write;
use std::path::PathBuf;

use flate2::Compression;
use flate2::write::DeflateEncoder;

fn main() -> std::io::Result<()> {
    const SOURCE: &str = "catalog/models.json";
    println!("cargo:rerun-if-changed={SOURCE}");
    println!("cargo:rerun-if-changed=build.rs");
    let out_dir = std::env::var_os("OUT_DIR").ok_or(std::io::ErrorKind::NotFound)?;
    let mut packed = DeflateEncoder::new(Vec::new(), Compression::best());
    packed.write_all(&std::fs::read(SOURCE)?)?;
    std::fs::write(
        PathBuf::from(out_dir).join("models.json.deflate"),
        packed.finish()?,
    )
}
