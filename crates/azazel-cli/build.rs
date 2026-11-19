//! Deflates the changelog that ships inside the binary (`changelog::shipped` inflates it) and, for a
//! Windows target, embeds the icon and the file details that Explorer shows.

use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::DeflateEncoder;

fn main() -> std::io::Result<()> {
    const SOURCE: &str = "../../CHANGELOG.md";
    println!("cargo:rerun-if-changed={SOURCE}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=AZAZEL_REQUIRE_RESOURCE");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").ok_or(std::io::ErrorKind::NotFound)?);
    let mut packed = DeflateEncoder::new(Vec::new(), Compression::best());
    packed.write_all(&std::fs::read(SOURCE)?)?;
    std::fs::write(out_dir.join("CHANGELOG.md.deflate"), packed.finish()?)?;

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_resource(&out_dir)?;
    }
    Ok(())
}

/// A resource is cosmetic, so a machine without a resource compiler (a cross build without
/// `llvm-rc` or `windres`) gets a warning and a binary without it, never a failed build. A release
/// build (`cargo xtask dist`) sets `AZAZEL_REQUIRE_RESOURCE=1`, and then a failure is an error:
/// an archive must not ship without its icon and file details unnoticed.
fn windows_resource(out_dir: &Path) -> std::io::Result<()> {
    const ICON: &str = "assets/azazel.ico";
    println!("cargo:rerun-if-changed={ICON}");
    let env = |name: &str| std::env::var(name).unwrap_or_default();
    let manifest_dir = PathBuf::from(env("CARGO_MANIFEST_DIR"));
    // A resource script is C-like: a backslash in a string starts an escape, and `"` is written `""`.
    let quote = |text: &str| text.replace('\\', "\\\\").replace('"', "\"\"");
    let icon = manifest_dir.join(ICON).to_string_lossy().replace('\\', "/");
    let (major, minor, patch) = (
        env("CARGO_PKG_VERSION_MAJOR"),
        env("CARGO_PKG_VERSION_MINOR"),
        env("CARGO_PKG_VERSION_PATCH"),
    );
    let version = env("CARGO_PKG_VERSION");
    let author = env("CARGO_PKG_AUTHORS");
    let strings = [
        ("CompanyName", author.clone()),
        (
            "FileDescription",
            "azazel: a coding agent for the terminal".into(),
        ),
        ("FileVersion", version.clone()),
        ("InternalName", "azazel".into()),
        ("LegalCopyright", format!("Copyright (c) 2026 {author}")),
        ("OriginalFilename", "azazel.exe".into()),
        ("ProductName", "azazel".into()),
        ("ProductVersion", version),
        ("Comments", env("CARGO_PKG_HOMEPAGE")),
    ]
    .map(|(key, value)| format!("      VALUE \"{key}\", \"{}\"\n", quote(&value)))
    .concat();
    let script = format!(
        "#pragma code_page(65001)\n\
         1 ICON \"{icon}\"\n\
         1 VERSIONINFO\n\
         FILEVERSION {major},{minor},{patch},0\n\
         PRODUCTVERSION {major},{minor},{patch},0\n\
         FILEFLAGSMASK 0x3f\n\
         FILEFLAGS 0\n\
         FILEOS 0x40004\n\
         FILETYPE 1\n\
         FILESUBTYPE 0\n\
         BEGIN\n\
         \x20 BLOCK \"StringFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   BLOCK \"040904B0\"\n\
         \x20   BEGIN\n\
         {strings}\
         \x20   END\n\
         \x20 END\n\
         \x20 BLOCK \"VarFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   VALUE \"Translation\", 0x0409, 0x04B0\n\
         \x20 END\n\
         END\n"
    );
    let path = out_dir.join("azazel.rc");
    std::fs::write(&path, script)?;
    match embed_resource::compile(&path, embed_resource::NONE) {
        embed_resource::CompilationResult::Ok | embed_resource::CompilationResult::NotWindows => {}
        embed_resource::CompilationResult::NotAttempted(why)
        | embed_resource::CompilationResult::Failed(why) => {
            let message = format!("azazel.exe is built without its icon and file details: {why}");
            if std::env::var("AZAZEL_REQUIRE_RESOURCE").as_deref() == Ok("1") {
                return Err(std::io::Error::other(message));
            }
            println!("cargo:warning={message}");
        }
    }
    Ok(())
}
