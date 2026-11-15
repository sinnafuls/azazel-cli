//! Office and mail files for `read`: `.docx`, `.pptx`, `.xlsx`, `.msg` and `.eml`.
//!
//! Such a file is converted once into a directory under the artifacts directory: a Markdown file
//! with its text, and beside it the pictures of a document, every sheet of a workbook as CSV and
//! the attachments of a message. What `read` returns is where that is and an outline of the
//! Markdown by line, so that a long document is read from the part that matters.

mod docx;
mod mail;
mod msg;
mod ooxml;
mod pptx;
mod xlsx;

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use azazel_agent::{ToolCtx, ToolOutput};
use azazel_core::ArtifactStore;

/// A Markdown file this short is returned whole with its outline.
const INLINE_CHARS: usize = 8_000;
/// The most headings in an outline.
const MAX_OUTLINE: usize = 200;
/// The most files beside the Markdown that are listed.
const MAX_LISTED: usize = 40;
/// The most pictures and attachments taken out of one file,
const MAX_PARTS: usize = 500;
/// and the most bytes of one of them, or of one part of the file itself.
const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;

/// A kind of file that is converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Word,
    Slides,
    Workbook,
    Outlook,
    Mail,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Word => "a Word document",
            Kind::Slides => "a PowerPoint presentation",
            Kind::Workbook => "an Excel workbook",
            Kind::Outlook => "an Outlook message",
            Kind::Mail => "a mail message",
        }
    }
}

/// What `path` is by its extension, when its first bytes agree: an Office file is a zip
/// archive, an Outlook message a compound file, and a `.eml` file starts with a header field.
pub fn kind(path: &Path, bytes: &[u8]) -> Option<Kind> {
    const ZIP: &[u8] = b"PK\x03\x04";
    const COMPOUND: &[u8] = &[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "docx" | "docm" if bytes.starts_with(ZIP) => Some(Kind::Word),
        "pptx" | "pptm" if bytes.starts_with(ZIP) => Some(Kind::Slides),
        "xlsx" | "xlsm" if bytes.starts_with(ZIP) => Some(Kind::Workbook),
        "msg" if bytes.starts_with(COMPOUND) => Some(Kind::Outlook),
        "eml" if mail::looks_like_mail(bytes) => Some(Kind::Mail),
        _ => None,
    }
}

/// A file as Markdown and the files that go beside it.
#[derive(Debug, Default)]
struct Converted {
    markdown: String,
    /// Paths under the directory of the conversion, with `/` between their parts, and what
    /// each holds.
    files: Vec<(String, Vec<u8>)>,
}

/// `name` with everything a file name should not hold replaced, at most `max` characters long.
fn safe_name(name: &str, max: usize) -> String {
    let clean: String = name
        .chars()
        .map(|c| match c {
            c if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') => c,
            _ => '_',
        })
        .take(max)
        .collect();
    let clean = clean.trim_matches('.');
    if clean.is_empty() {
        "file".to_string()
    } else {
        clean.to_string()
    }
}

/// `name`, or `name` with a number before its extension when `taken` already holds it.
fn unused(name: String, taken: &mut Vec<String>) -> String {
    let mut free = name.clone();
    let mut number = 1;
    while taken.iter().any(|other| other.eq_ignore_ascii_case(&free)) {
        number += 1;
        free = match name.rsplit_once('.') {
            Some((stem, extension)) => format!("{stem}-{number}.{extension}"),
            None => format!("{name}-{number}"),
        };
    }
    taken.push(free.clone());
    free
}

/// The date `days` after the first of January 1970, as year, month and day.
fn civil(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `seconds` after the first of January 1970 as `2026-01-31 12:00:03`.
fn date_time(seconds: i64) -> String {
    let (year, month, day) = civil(seconds.div_euclid(86_400));
    let of_day = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60
    )
}

/// The rows of a table as a Markdown table whose first row is the header.
fn markdown_table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return String::new();
    }
    let mut out = String::new();
    for (index, row) in rows.iter().enumerate() {
        out.push('|');
        for column in 0..columns {
            let cell = row.get(column).map_or("", String::as_str);
            let cell = cell.trim().replace('|', "\\|").replace('\n', "<br>");
            out.push_str(&format!(" {cell} |"));
        }
        out.push('\n');
        if index == 0 {
            out.push('|');
            out.push_str(&" --- |".repeat(columns));
            out.push('\n');
        }
    }
    out.pop();
    out
}

/// HTML as Markdown, or as it is when it cannot be converted.
fn html_to_markdown(html: &str) -> String {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["head", "script", "style", "noscript", "template"])
        .build();
    match converter.convert(html) {
        Ok(markdown) => markdown.trim().to_string(),
        Err(_) => html.to_string(),
    }
}

/// The headings of `markdown`, each with the number of its line. Lines of a code block are not
/// headings.
fn outline(markdown: &str) -> Vec<(usize, &str)> {
    let mut headings = Vec::new();
    let mut fenced = false;
    for (index, line) in markdown.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if !fenced && line.starts_with('#') && line.trim_start_matches('#').starts_with(' ') {
            headings.push((index + 1, line));
        }
    }
    headings
}

/// The directory a conversion of this file goes to: named after the file and what is in it, so
/// the same file is converted once and a changed one again.
fn bundle_name(path: &Path, bytes: &[u8]) -> String {
    let mut hasher = std::hash::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let stem = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    format!("{}-{:08x}", safe_name(&stem, 48), hasher.finish() as u32)
}

/// The name of the Markdown file for `path`: `report.md` for `report.docx`.
fn markdown_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy())
        .unwrap_or_default();
    format!("{}.md", safe_name(&stem, 48))
}

/// Converts the file into `dir` unless that was done before, and returns the Markdown.
fn convert_into(kind: Kind, path: &Path, bytes: &[u8], dir: &Path) -> Result<String, String> {
    let markdown_path = dir.join(markdown_name(path));
    if let Ok(markdown) = std::fs::read_to_string(&markdown_path) {
        return Ok(markdown);
    }
    let converted = match kind {
        Kind::Word => docx::convert(bytes),
        Kind::Slides => pptx::convert(bytes),
        Kind::Workbook => xlsx::convert(bytes),
        Kind::Outlook => msg::convert(bytes),
        Kind::Mail => mail::convert(bytes),
    }?;
    let write = |path: &Path, bytes: &[u8]| {
        let made = match path.parent() {
            Some(parent) => std::fs::create_dir_all(parent),
            None => Ok(()),
        };
        made.and_then(|()| std::fs::write(path, bytes))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    };
    for (name, bytes) in &converted.files {
        write(&dir.join(name), bytes)?;
    }
    // The Markdown is written last: a directory that holds it is a finished conversion.
    write(&markdown_path, converted.markdown.as_bytes())?;
    Ok(converted.markdown)
}

/// The files under `dir` other than `skip`, as paths with `/` between their parts, with their
/// sizes, in order.
fn files_beside(dir: &Path, skip: &str) -> Vec<(String, u64)> {
    let mut files = Vec::new();
    let mut pending = vec![(dir.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = pending.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
            match entry.metadata() {
                Ok(meta) if meta.is_dir() => pending.push((entry.path(), format!("{name}/"))),
                Ok(meta) if name != skip => files.push((name, meta.len())),
                _ => {}
            }
        }
    }
    files.sort();
    files
}

/// What `read` says about a converted file: where the conversion is, what is in it, and the
/// outline of the Markdown; a short Markdown file follows whole.
fn describe(kind: Kind, path: &Path, size: usize, dir: &Path, markdown: &str) -> String {
    let name = markdown_name(path);
    let lines = markdown.lines().count();
    let mut out = format!(
        "{} is {} ({size} bytes). It was converted to Markdown in {}:\n{name}\t{lines} lines",
        path.display(),
        kind.name(),
        dir.display()
    );
    let beside = files_beside(dir, &name);
    for (file, bytes) in beside.iter().take(MAX_LISTED) {
        out.push_str(&format!("\n{file}\t{bytes} bytes"));
    }
    if beside.len() > MAX_LISTED {
        out.push_str(&format!("\n[{} more files]", beside.len() - MAX_LISTED));
    }
    let headings = outline(markdown);
    if !headings.is_empty() {
        out.push_str(&format!(
            "\n\nOutline of {name}; `L` is the line, which `offset` takes when reading this file or {name}:"
        ));
        for (line, heading) in headings.iter().take(MAX_OUTLINE) {
            out.push_str(&format!("\nL{line}\t{heading}"));
        }
        if headings.len() > MAX_OUTLINE {
            out.push_str(&format!("\n[{} more headings]", headings.len() - MAX_OUTLINE));
        }
    }
    if markdown.trim().is_empty() {
        out.push_str("\n\nThere is no text in it.");
    } else if markdown.chars().count() <= INLINE_CHARS {
        out.push_str(&format!("\n\nAll of {name}:\n\n{}", markdown.trim_end()));
    } else {
        out.push_str(&format!(
            "\n\nRead {name} for the text: give this file or it an `offset`."
        ));
    }
    out
}

/// What `read` returns for an Office or mail file: the outline of its conversion, or with an
/// `offset` or a `limit` those lines of the Markdown, numbered like the lines of a text file.
pub async fn read(
    kind: Kind,
    path: &Path,
    bytes: Vec<u8>,
    offset: Option<usize>,
    limit: Option<usize>,
    ctx: &ToolCtx,
) -> ToolOutput {
    let Some(store) = ctx.services.get::<ArtifactStore>() else {
        return ToolOutput::error(format!(
            "{}: there is no artifacts directory to convert {} into",
            path.display(),
            kind.name()
        ));
    };
    let dir: PathBuf = store.dir().join(bundle_name(path, &bytes));
    let size = bytes.len();
    let file = path.to_path_buf();
    // Unpacking and parsing a large file takes a while: off the async threads.
    let converted = tokio::task::spawn_blocking(move || {
        let markdown = convert_into(kind, &file, &bytes, &dir);
        (markdown, dir)
    })
    .await;
    match converted {
        Ok((Ok(markdown), _)) if offset.is_some() || limit.is_some() => {
            crate::read::render_text(&markdown, offset, limit).0
        }
        Ok((Ok(markdown), dir)) => ToolOutput::text(describe(kind, path, size, &dir, &markdown)),
        Ok((Err(error), _)) => ToolOutput::error(format!("{}: {error}", path.display())),
        Err(_) => ToolOutput::error(format!("{}: the file could not be converted", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_known_by_its_extension_when_its_first_bytes_agree() {
        let zip = b"PK\x03\x04rest";
        let compound = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1, 0x00];
        assert_eq!(kind(Path::new("a/Report.DOCX"), zip), Some(Kind::Word));
        assert_eq!(kind(Path::new("deck.pptx"), zip), Some(Kind::Slides));
        assert_eq!(kind(Path::new("macros.xlsm"), zip), Some(Kind::Workbook));
        assert_eq!(kind(Path::new("mail.msg"), &compound), Some(Kind::Outlook));
        assert_eq!(
            kind(Path::new("mail.eml"), b"From: a@example.com\r\n"),
            Some(Kind::Mail)
        );
        // A text file with an Office name is a text file, and old Office files are not read.
        assert_eq!(kind(Path::new("notes.docx"), b"plain words"), None);
        assert_eq!(kind(Path::new("notes.eml"), b"plain words"), None);
        assert_eq!(kind(Path::new("mail.msg"), b"plain words"), None);
        assert_eq!(kind(Path::new("old.doc"), &compound), None);
        assert_eq!(kind(Path::new("archive.zip"), zip), None);
        assert_eq!(kind(Path::new("docx"), zip), None);
    }

    #[test]
    fn file_names_are_made_safe_and_never_repeat() {
        assert_eq!(safe_name("Q3 report (final).pdf", 48), "Q3_report__final_.pdf");
        assert_eq!(safe_name("../../etc/passwd", 48), "_.._etc_passwd");
        assert_eq!(safe_name("...", 48), "file");
        assert_eq!(safe_name("Übersicht 2026.xlsx", 9), "Übersicht");
        let mut taken = Vec::new();
        assert_eq!(unused("a.png".into(), &mut taken), "a.png");
        assert_eq!(unused("A.PNG".into(), &mut taken), "A-2.PNG");
        assert_eq!(unused("a.png".into(), &mut taken), "a-3.png");
        assert_eq!(unused("notes".into(), &mut taken), "notes");
        assert_eq!(unused("notes".into(), &mut taken), "notes-2");
    }

    #[test]
    fn a_table_becomes_a_markdown_table_and_an_outline_is_the_headings_by_line() {
        let rows = vec![
            vec!["Region".to_string(), "Q3".to_string()],
            vec![
                "North | East".to_string(),
                "1\n2".to_string(),
                "extra".to_string(),
            ],
            vec![],
        ];
        assert_eq!(
            markdown_table(&rows),
            "| Region | Q3 |  |\n| --- | --- | --- |\n| North \\| East | 1<br>2 | extra |\n|  |  |  |"
        );
        assert_eq!(markdown_table(&[]), "");

        let markdown = "# Title\n\ntext #not a heading\n#hashtag\n```sh\n# a comment\n```\n## Part two\n";
        assert_eq!(outline(markdown), [(1, "# Title"), (8, "## Part two")]);
    }
}
