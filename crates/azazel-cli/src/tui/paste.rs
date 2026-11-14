//! What the paste keys take from the clipboard: an image, the files it names, or text.
//!
//! The system clipboard is asked first. Where it cannot be reached that way, or holds something
//! in a form that is not understood, the platform's own program is asked: PowerShell on Windows
//! and under WSL (whose Linux side has no way to an image on the Windows clipboard), `osascript`
//! on macOS, `wl-paste` or `xclip` on Linux. Those programs run only because a paste key was
//! pressed, with fixed arguments: nothing from the clipboard ever becomes part of a command.

use std::io::Cursor;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use azazel_ai::Block;
use azazel_core::media::{MAX_IMAGE_SIDE, image_block};
use base64::Engine;

use super::input::{dropped_path, wsl_path};

/// What the clipboard held.
pub(super) enum Clip {
    /// An image, ready to attach.
    Image(Block),
    /// Files: images among them are attached, the others mentioned.
    Files(Vec<PathBuf>),
    Text(String),
    /// Nothing that can go into a prompt.
    Empty,
}

/// How long PowerShell has to start and answer; it is slow to start on a busy machine.
const POWERSHELL_LIMIT: Duration = Duration::from_secs(8);
/// How long the other programs have.
const PROGRAM_LIMIT: Duration = Duration::from_secs(3);

/// Text types asked of a Linux clipboard, best first.
const TEXT_TYPES: [&str; 4] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING"];
/// Image types asked of a Linux clipboard, best first.
const IMAGE_TYPES: [&str; 5] = ["image/png", "image/jpeg", "image/webp", "image/gif", "image/bmp"];

/// Reads the clipboard for `ctrl+v`: an image before files before text.
pub(super) async fn read() -> Clip {
    // A test that wants the platform's program to be the one that reads says so.
    #[cfg(feature = "test-support")]
    if std::env::var_os("AZAZEL_TEST_CLIPBOARD").is_some_and(|how| how == "external") {
        return external().await;
    }
    match tokio::task::spawn_blocking(native).await {
        Ok(Some(clip)) => clip,
        _ => external().await,
    }
}

/// The clipboard's text and nothing else, for the key that pastes text only.
pub(super) async fn read_text() -> Option<String> {
    let read = || arboard::Clipboard::new().ok()?.get_text().ok();
    tokio::task::spawn_blocking(read).await.ok().flatten()
}

/// The system clipboard, read in this process. `None` when there is none to ask or it holds
/// nothing that is understood.
fn native() -> Option<Clip> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    if let Ok(image) = clipboard.get_image() {
        let (width, height) = (image.width as u32, image.height as u32);
        let block = image::RgbaImage::from_raw(width, height, image.bytes.into_owned())
            .and_then(|pixels| png_of(&image::DynamicImage::ImageRgba8(pixels)))
            .and_then(|png| image_block(&png, MAX_IMAGE_SIDE).ok());
        if let Some(block) = block {
            return Some(Clip::Image(block));
        }
    }
    if let Ok(files) = clipboard.get().file_list()
        && !files.is_empty()
    {
        return Some(Clip::Files(files));
    }
    match clipboard.get_text() {
        Ok(text) if !text.trim().is_empty() => Some(Clip::Text(text)),
        _ => None,
    }
}

/// The clipboard as the platform's own program reads it.
async fn external() -> Clip {
    if (cfg!(windows) || under_wsl())
        && let Some(clip) = powershell().await
    {
        return clip;
    }
    if cfg!(target_os = "macos")
        && let Some(clip) = osascript().await
    {
        return clip;
    }
    if cfg!(all(unix, not(target_os = "macos"))) {
        if std::env::var_os("WAYLAND_DISPLAY").is_some()
            && let Some(clip) = linux(&["wl-paste", "--list-types"], &["wl-paste", "--type"]).await
        {
            return clip;
        }
        if std::env::var_os("DISPLAY").is_some() {
            let list = ["xclip", "-selection", "clipboard", "-o", "-t", "TARGETS"];
            let fetch = ["xclip", "-selection", "clipboard", "-o", "-t"];
            if let Some(clip) = linux(&list, &fetch).await {
                return clip;
            }
        }
    }
    Clip::Empty
}

fn under_wsl() -> bool {
    cfg!(target_os = "linux")
        && ["WSL_DISTRO_NAME", "WSL_INTEROP"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// An image in any format that can be decoded, as a block a model takes: what is not PNG,
/// JPEG, WebP or GIF (a BMP, which is what Windows hands to WSL) is converted to PNG, and
/// everything is scaled to the usual limit.
pub(super) fn image_from(bytes: &[u8]) -> Option<Block> {
    if let Ok(block) = image_block(bytes, MAX_IMAGE_SIDE) {
        return Some(block);
    }
    let png = png_of(&image::load_from_memory(bytes).ok()?)?;
    image_block(&png, MAX_IMAGE_SIDE).ok()
}

fn png_of(image: &image::DynamicImage) -> Option<Vec<u8>> {
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

/// What `program` writes when it is run with `args` and ends well within `limit`.
async fn output(program: &str, args: &[&str], limit: Duration) -> Option<Vec<u8>> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Without a console of its own the program cannot touch the one the UI draws in.
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let output = tokio::time::timeout(limit, command.output()).await.ok()?.ok()?;
    output.status.success().then_some(output.stdout)
}

// ------------------------------------------------------------ Windows and WSL

/// Writes `image:<base64 PNG>` when the clipboard holds an image, else one `file:<path>` line
/// per file it holds, else `text:<base64 UTF-8>` for its text. Nothing of the script comes
/// from outside this source file.
const POWERSHELL_SCRIPT: &str = "\
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$image = [System.Windows.Forms.Clipboard]::GetImage()
if ($image -ne $null) {
    $stream = New-Object System.IO.MemoryStream
    $image.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
    [Console]::Out.Write('image:' + [Convert]::ToBase64String($stream.ToArray()))
} else {
    $files = [System.Windows.Forms.Clipboard]::GetFileDropList()
    if ($files.Count -gt 0) {
        foreach ($file in $files) {
            [Console]::Out.WriteLine('file:' + $file)
        }
    } else {
        $text = [System.Windows.Forms.Clipboard]::GetText()
        if ($text) {
            [Console]::Out.Write('text:' + [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($text)))
        }
    }
}
";

/// Where Windows PowerShell is when WSL does not have the Windows programs on its `PATH`.
const POWERSHELL_FROM_WSL: &str = "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe";

async fn powershell() -> Option<Clip> {
    // The script goes over as UTF-16 in base64, the one form no shell or launcher re-quotes.
    let utf16: Vec<u8> = POWERSHELL_SCRIPT
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    let args = [
        "-NoProfile",
        "-NonInteractive",
        "-Sta",
        "-EncodedCommand",
        encoded.as_str(),
    ];
    let mut written = output("powershell.exe", &args, POWERSHELL_LIMIT).await;
    if written.is_none() && cfg!(unix) {
        written = output(POWERSHELL_FROM_WSL, &args, POWERSHELL_LIMIT).await;
    }
    powershell_clip(&String::from_utf8_lossy(&written?))
}

/// What the PowerShell script reported.
fn powershell_clip(written: &str) -> Option<Clip> {
    let written = written.trim();
    let decode = |data: &str| base64::engine::general_purpose::STANDARD.decode(data.trim()).ok();
    if let Some(data) = written.strip_prefix("image:") {
        return image_from(&decode(data)?).map(Clip::Image);
    }
    if let Some(data) = written.strip_prefix("text:") {
        let text = String::from_utf8(decode(data)?).ok()?;
        return (!text.trim().is_empty()).then_some(Clip::Text(text));
    }
    let files: Vec<PathBuf> = written
        .lines()
        .filter_map(|line| line.trim().strip_prefix("file:"))
        .map(|path| match wsl_path(path) {
            // Under WSL the Windows drives are below `/mnt`.
            Some(mounted) if cfg!(unix) => mounted,
            _ => PathBuf::from(path),
        })
        .collect();
    (!files.is_empty()).then_some(Clip::Files(files))
}

// ------------------------------------------------------------ macOS

/// The files on the pasteboard (copied in Finder), one path per line.
const FILES_APPLESCRIPT: [&str; 9] = [
    "use framework \"AppKit\"",
    "set board to current application's NSPasteboard's generalPasteboard()",
    "set found to board's readObjectsForClasses:{current application's NSURL} options:(missing value)",
    "set out to \"\"",
    "if found is not missing value then",
    "repeat with fileURL in found",
    "if (fileURL's isFileURL()) as boolean then set out to out & ((fileURL's |path|()) as text) & linefeed",
    "end repeat",
    "end if",
];

async fn osascript() -> Option<Clip> {
    let image = output(
        "osascript",
        &["-e", "the clipboard as «class PNGf»"],
        PROGRAM_LIMIT,
    )
    .await
    .and_then(|written| applescript_data(&String::from_utf8_lossy(&written)))
    .and_then(|png| image_from(&png));
    if let Some(block) = image {
        return Some(Clip::Image(block));
    }
    let mut args = Vec::new();
    for line in FILES_APPLESCRIPT {
        args.extend(["-e", line]);
    }
    args.extend(["-e", "return out"]);
    let files: Vec<PathBuf> = output("osascript", &args, PROGRAM_LIMIT)
        .await
        .map(|written| {
            String::from_utf8_lossy(&written)
                .lines()
                .filter_map(|line| dropped_path(line.trim(), None))
                .collect()
        })
        .unwrap_or_default();
    if !files.is_empty() {
        return Some(Clip::Files(files));
    }
    let text = String::from_utf8(output("pbpaste", &[], PROGRAM_LIMIT).await?).ok()?;
    (!text.trim().is_empty()).then_some(Clip::Text(text))
}

/// The bytes AppleScript prints for clipboard data: `«data PNGf89504E47…»`.
fn applescript_data(printed: &str) -> Option<Vec<u8>> {
    let hex = printed.trim().strip_prefix("«data ")?.strip_suffix('»')?;
    // Four characters name the type, the rest is the data.
    let hex = hex.get(4..)?;
    if hex.is_empty() || hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok())
        .collect()
}

// ------------------------------------------------------------ Linux

/// Asks a Linux clipboard program what it has (`list`) and fetches the best of it (`fetch`
/// followed by a type, which is one of ours and never one the clipboard named).
async fn linux(list: &[&str], fetch: &[&str]) -> Option<Clip> {
    let listed = output(list[0], &list[1..], PROGRAM_LIMIT).await?;
    let listed = String::from_utf8_lossy(&listed);
    let get = async |kind: &'static str| {
        let mut args: Vec<&str> = fetch[1..].to_vec();
        args.push(kind);
        output(fetch[0], &args, PROGRAM_LIMIT).await
    };
    if let Some(kind) = offered(&listed, &IMAGE_TYPES) {
        return image_from(&get(kind).await?).map(Clip::Image);
    }
    if listed.lines().any(|line| line.trim() == "text/uri-list") {
        let home = std::env::home_dir();
        let files: Vec<PathBuf> = String::from_utf8_lossy(&get("text/uri-list").await?)
            .lines()
            .filter(|line| !line.starts_with('#'))
            .filter_map(|line| dropped_path(line.trim(), home.as_deref()))
            .collect();
        return (!files.is_empty()).then_some(Clip::Files(files));
    }
    let text = String::from_utf8(get(offered(&listed, &TEXT_TYPES)?).await?).ok()?;
    (!text.trim().is_empty()).then_some(Clip::Text(text))
}

/// The first of `kinds` that a clipboard lists.
fn offered(listed: &str, kinds: &[&'static str]) -> Option<&'static str> {
    kinds
        .iter()
        .copied()
        .find(|kind| listed.lines().any(|line| line.trim() == *kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(format: image::ImageFormat, width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([20, 120, 220]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut Cursor::new(&mut bytes), format)
            .unwrap();
        bytes
    }

    fn decoded(block: &Block) -> (String, image::DynamicImage) {
        let Block::Image { mime, data } = block else {
            panic!("{block:?}")
        };
        let bytes = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
        (mime.clone(), image::load_from_memory(&bytes).unwrap())
    }

    #[test]
    fn a_bitmap_becomes_a_png_and_a_large_image_is_scaled() {
        let (mime, image) = decoded(&image_from(&picture(image::ImageFormat::Bmp, 30, 20)).unwrap());
        assert_eq!(mime, "image/png");
        assert_eq!((image.width(), image.height()), (30, 20));
        assert_eq!(image.to_rgb8().get_pixel(3, 3).0, [20, 120, 220]);

        // A PNG goes through as it is; one larger than the limit comes out at the limit.
        let png = picture(image::ImageFormat::Png, 40, 10);
        let Block::Image { data, .. } = image_from(&png).unwrap() else {
            panic!()
        };
        assert_eq!(
            base64::engine::general_purpose::STANDARD.decode(data).unwrap(),
            png
        );
        let wide = picture(image::ImageFormat::Bmp, MAX_IMAGE_SIDE * 2, 8);
        let (_, image) = decoded(&image_from(&wide).unwrap());
        assert_eq!((image.width(), image.height()), (MAX_IMAGE_SIDE, 4));

        assert!(image_from(b"not an image").is_none());
        assert!(image_from(b"").is_none());
    }

    #[test]
    fn the_powershell_report_is_an_image_or_files_or_nothing() {
        let png = picture(image::ImageFormat::Png, 6, 4);
        let report = format!(
            "image:{}\r\n",
            base64::engine::general_purpose::STANDARD.encode(&png)
        );
        let Some(Clip::Image(block)) = powershell_clip(&report) else {
            panic!("an image was reported")
        };
        let (mime, image) = decoded(&block);
        assert_eq!(
            (mime.as_str(), image.width(), image.height()),
            ("image/png", 6, 4)
        );

        let Some(Clip::Files(files)) =
            powershell_clip("file:C:\\Users\\me\\shot one.png\r\nfile:D:\\b.txt\r\n")
        else {
            panic!("files were reported")
        };
        if cfg!(windows) {
            assert_eq!(
                files,
                [
                    PathBuf::from("C:\\Users\\me\\shot one.png"),
                    PathBuf::from("D:\\b.txt")
                ]
            );
        } else {
            assert_eq!(
                files,
                [
                    PathBuf::from("/mnt/c/Users/me/shot one.png"),
                    PathBuf::from("/mnt/d/b.txt")
                ]
            );
        }

        let Some(Clip::Text(text)) = powershell_clip("text:Z3LDvMOfZQpsaW5lIDI=") else {
            panic!("text was reported")
        };
        assert_eq!(text, "grüße\nline 2");
        assert!(powershell_clip("text:").is_none());
        assert!(powershell_clip("text:ICAK").is_none(), "blank text is nothing");
        assert!(powershell_clip("").is_none());
        assert!(powershell_clip("image:not base64!").is_none());
        assert!(
            powershell_clip("image:aGVsbG8=").is_none(),
            "base64 of something that is no image"
        );
        assert!(powershell_clip("something else\n").is_none());
    }

    #[test]
    fn applescript_data_is_read_from_its_hexadecimal_form() {
        assert_eq!(
            applescript_data("«data PNGf89504E470D0A»\n"),
            Some(vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a])
        );
        assert_eq!(applescript_data("«data PNGf»"), None);
        assert_eq!(applescript_data("«data PNGf895»"), None);
        assert_eq!(applescript_data("«data PNGfzz»"), None);
        assert_eq!(applescript_data("missing value"), None);
        assert_eq!(applescript_data(""), None);
    }

    #[test]
    fn the_best_type_a_clipboard_lists_is_asked_for() {
        let image = |listed: &str| offered(listed, &IMAGE_TYPES);
        assert_eq!(
            image("TARGETS\nimage/bmp\nimage/png\ntext/plain\n"),
            Some("image/png")
        );
        assert_eq!(image("image/bmp\r\n"), Some("image/bmp"));
        assert_eq!(image("text/plain\ntext/uri-list\n"), None);
        // A type is matched whole, and only ours are ever passed on.
        assert_eq!(image("image/png;evil\nximage/png\n"), None);
        assert_eq!(image(""), None);
        assert_eq!(
            offered("TARGETS\nSTRING\nUTF8_STRING\n", &TEXT_TYPES),
            Some("UTF8_STRING")
        );
        assert_eq!(offered("text/html\n", &TEXT_TYPES), None);
    }
}
