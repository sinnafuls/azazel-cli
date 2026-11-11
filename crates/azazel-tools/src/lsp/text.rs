//! Text positions and file URIs as the protocol counts them: zero-based lines, UTF-16 columns.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use lsp_types::{Position, TextEdit, Uri};

/// UTF-16 code units before the 1-based character `column` of `line`. A column past the end of the
/// line means its end.
pub fn utf16_offset(line: &str, column: u32) -> u32 {
    line.chars()
        .take(column.saturating_sub(1) as usize)
        .map(|c| c.len_utf16() as u32)
        .sum()
}

/// The 1-based character column at UTF-16 offset `character` of `line`.
pub fn char_column(line: &str, character: u32) -> u32 {
    let mut units = 0;
    let mut column = 1;
    for c in line.chars() {
        if units >= character {
            break;
        }
        units += c.len_utf16() as u32;
        column += 1;
    }
    column
}

/// A text split into lines the way the protocol numbers them. Lines end at `\n`; a `\r` before it
/// belongs to the line ending.
pub(crate) struct Lines<'a> {
    text: &'a str,
    starts: Vec<usize>,
}

impl<'a> Lines<'a> {
    pub fn new(text: &'a str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
        Lines { text, starts }
    }

    pub fn count(&self) -> usize {
        self.starts.len()
    }

    /// Line `index` (zero-based) without its line ending.
    pub fn get(&self, index: usize) -> Option<&'a str> {
        let start = *self.starts.get(index)?;
        let end = self
            .starts
            .get(index + 1)
            .map_or(self.text.len(), |next| next - 1);
        let line = &self.text[start..end];
        Some(line.strip_suffix('\r').unwrap_or(line))
    }

    /// The protocol position of a 1-based line and character column, or `None` when the text has
    /// no such line.
    pub fn position(&self, line: u32, column: u32) -> Option<Position> {
        let index = line.checked_sub(1)?;
        let text = self.get(index as usize)?;
        Some(Position::new(index, utf16_offset(text, column)))
    }

    /// The 1-based `(line, character column)` of a protocol position.
    pub fn human(&self, position: Position) -> (u32, u32) {
        let column = match self.get(position.line as usize) {
            Some(text) => char_column(text, position.character),
            None => position.character + 1,
        };
        (position.line + 1, column)
    }

    /// The byte offset of `position`, which is clamped to the end of its line and of the text.
    fn offset(&self, position: Position) -> usize {
        let (Some(&start), Some(line)) = (
            self.starts.get(position.line as usize),
            self.get(position.line as usize),
        ) else {
            return self.text.len();
        };
        let mut units = 0;
        for (index, c) in line.char_indices() {
            if units >= position.character {
                return start + index;
            }
            units += c.len_utf16() as u32;
        }
        start + line.len()
    }
}

/// `text` with `edits` applied. Every edit is positioned against the original text; edits that
/// insert at the same place keep their order, and overlapping edits are refused.
pub fn apply_edits(text: &str, edits: &[TextEdit]) -> Result<String, String> {
    let lines = Lines::new(text);
    let mut spans: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|edit| {
            (
                lines.offset(edit.range.start),
                lines.offset(edit.range.end),
                edit.new_text.as_str(),
            )
        })
        .collect();
    spans.sort_by_key(|(start, _, _)| *start);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end, new_text) in spans {
        if start < at || end < start {
            return Err("the language server returned overlapping edits".to_string());
        }
        out.push_str(&text[at..start]);
        out.push_str(new_text);
        at = end;
    }
    out.push_str(&text[at..]);
    Ok(out)
}

/// The `file://` URI of an absolute path.
pub fn path_to_uri(path: &Path) -> Result<Uri, String> {
    let mut text = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        text = text.replace('\\', "/");
    }
    let mut uri = String::from("file://");
    let rest = match text.strip_prefix("//") {
        // A UNC path: the server name is the URI's authority.
        Some(unc) => unc,
        None => {
            if !text.starts_with('/') {
                uri.push('/');
            }
            text.as_str()
        }
    };
    for (index, byte) in rest.bytes().enumerate() {
        let drive_colon = byte == b':' && index == 1 && rest.as_bytes()[0].is_ascii_alphabetic();
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') || drive_colon {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    Uri::from_str(&uri).map_err(|e| format!("{} cannot be written as a file URI: {e}", path.display()))
}

/// The path a `file://` URI names, or `None` for any other URI.
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let rest = uri.as_str().strip_prefix("file://")?;
    let (host, path) = match rest.find('/') {
        Some(slash) => rest.split_at(slash),
        None => (rest, ""),
    };
    let path = percent_decode(path)?;
    let text = if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        // `/C:/dir` names a Windows drive.
        let drive = matches!(path.as_bytes(), [b'/', letter, b':', ..] if letter.is_ascii_alphabetic());
        if drive { path[1..].to_string() } else { path }
    } else {
        format!("//{host}{path}")
    };
    Some(PathBuf::from(if cfg!(windows) {
        text.replace('/', "\\")
    } else {
        text
    }))
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// One spelling for every way a client or server may write the same file's path, so paths taken
/// from URIs can be compared.
pub fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.replace('\\', "/").to_lowercase()
    } else {
        text.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use lsp_types::Range;

    use super::*;

    fn edit(start: (u32, u32), end: (u32, u32), new_text: &str) -> TextEdit {
        TextEdit::new(
            Range::new(Position::new(start.0, start.1), Position::new(end.0, end.1)),
            new_text.to_string(),
        )
    }

    #[test]
    fn columns_count_characters_while_the_protocol_counts_utf16_units() {
        // U+1D4B3 is outside the BMP: one character, two UTF-16 units, four UTF-8 bytes.
        let line = "𝒳é = value";
        assert_eq!(utf16_offset(line, 1), 0);
        assert_eq!(utf16_offset(line, 2), 2);
        assert_eq!(utf16_offset(line, 6), 6, "the `v` follows 𝒳, é, space, =, space");
        assert_eq!(utf16_offset(line, 400), 11, "past the end is the end");
        assert_eq!(char_column(line, 0), 1);
        assert_eq!(char_column(line, 2), 2);
        assert_eq!(char_column(line, 6), 6);
        assert_eq!(char_column("plain", 3), 4);
    }

    #[test]
    fn positions_are_zero_based_lines_and_exist_only_for_real_lines() {
        let lines = Lines::new("first\r\n𝒳 second\nthird");
        assert_eq!(lines.count(), 3);
        assert_eq!(
            lines.get(0),
            Some("first"),
            "the carriage return is not part of the line"
        );
        assert_eq!(lines.position(2, 3), Some(Position::new(1, 3)));
        assert_eq!(lines.position(3, 1), Some(Position::new(2, 0)));
        assert_eq!(lines.position(0, 1), None);
        assert_eq!(lines.position(4, 1), None);
        assert_eq!(lines.human(Position::new(1, 3)), (2, 3));
        assert_eq!(lines.human(Position::new(9, 4)), (10, 5));
    }

    #[test]
    fn edits_apply_against_the_original_text_whatever_their_order() {
        let text = "let 𝒳 = old;\r\nuse old;\n";
        let edits = [
            edit((1, 4), (1, 7), "new"),
            edit((0, 9), (0, 12), "new"),
            edit((0, 0), (0, 0), "pub "),
        ];
        assert_eq!(
            apply_edits(text, &edits).unwrap(),
            "pub let 𝒳 = new;\r\nuse new;\n"
        );
    }

    #[test]
    fn inserts_at_one_place_keep_their_order_and_overlaps_are_refused() {
        let inserts = [edit((0, 1), (0, 1), "1"), edit((0, 1), (0, 1), "2")];
        assert_eq!(apply_edits("ab", &inserts).unwrap(), "a12b");
        let overlapping = [edit((0, 0), (0, 3), "x"), edit((0, 2), (0, 4), "y")];
        assert!(apply_edits("abcdef", &overlapping).is_err());
        let multi_line = [edit((0, 1), (2, 1), "-")];
        assert_eq!(apply_edits("ab\ncd\nef", &multi_line).unwrap(), "a-f");
        let past_the_end = [edit((0, 99), (7, 0), "!")];
        assert_eq!(apply_edits("ab\ncd", &past_the_end).unwrap(), "ab!");
    }

    #[test]
    fn paths_and_file_uris_convert_both_ways() {
        let (path, uri) = if cfg!(windows) {
            (
                PathBuf::from(r"C:\work dir\ünï#1.rs"),
                "file:///C:/work%20dir/%C3%BCn%C3%AF%231.rs",
            )
        } else {
            (
                PathBuf::from("/work dir/ünï#1.rs"),
                "file:///work%20dir/%C3%BCn%C3%AF%231.rs",
            )
        };
        let converted = path_to_uri(&path).unwrap();
        assert_eq!(converted.as_str(), uri);
        assert_eq!(uri_to_path(&converted), Some(path));
        assert_eq!(uri_to_path(&Uri::from_str("untitled:Untitled-1").unwrap()), None);
    }

    #[test]
    fn a_server_may_spell_a_drive_its_own_way() {
        if !cfg!(windows) {
            return;
        }
        let theirs = uri_to_path(&Uri::from_str("file:///c%3A/Work/a.rs").unwrap()).unwrap();
        assert_eq!(theirs, PathBuf::from(r"c:\Work\a.rs"));
        assert_eq!(path_key(&theirs), path_key(Path::new(r"C:\work\A.rs")));
        let unc = uri_to_path(&Uri::from_str("file://server/share/a.rs").unwrap()).unwrap();
        assert_eq!(unc, PathBuf::from(r"\\server\share\a.rs"));
        assert_eq!(path_to_uri(&unc).unwrap().as_str(), "file://server/share/a.rs");
    }
}
