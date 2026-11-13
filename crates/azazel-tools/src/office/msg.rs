//! An Outlook message (`.msg`) as Markdown.
//!
//! Such a file is a compound file: a small file system whose streams are the properties of the
//! message, named after the number and the type of each (`__substg1.0_0037001F` is the subject
//! as UTF-16), with a directory per recipient and per attachment. Numbers and times are rows of
//! one stream, `__properties_version1.0`.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use mail_parser::Charset;

use super::mail::Letter;
use super::{Converted, MAX_PART_BYTES, date_time, html_to_markdown};

const SUBJECT: u16 = 0x0037;
const SUBMIT_TIME: u16 = 0x0039;
const DELIVERY_TIME: u16 = 0x0e06;
const SENDER_NAME: u16 = 0x0c1a;
const SENDER_ADDRESS: u16 = 0x0c1f;
const SENDER_SMTP: u16 = 0x5d01;
const DISPLAY_TO: u16 = 0x0e04;
const DISPLAY_CC: u16 = 0x0e03;
const RECIPIENT_TYPE: u16 = 0x0c15;
const DISPLAY_NAME: u16 = 0x3001;
const ADDRESS: u16 = 0x3003;
const SMTP_ADDRESS: u16 = 0x39fe;
const BODY: u16 = 0x1000;
const BODY_HTML: u16 = 0x1013;
const BODY_RTF: u16 = 0x1009;
const INTERNET_CODE_PAGE: u16 = 0x3fde;
const MESSAGE_CODE_PAGE: u16 = 0x3ffd;
const ATTACHMENT_DATA: u16 = 0x3701;
const ATTACHMENT_LONG_NAME: u16 = 0x3707;
const ATTACHMENT_NAME: u16 = 0x3704;
/// The storage that holds an attached message.
const ATTACHED_MESSAGE: &str = "__substg1.0_3701000D";
/// Messages in messages are followed this deep.
const MAX_DEPTH: usize = 4;

/// The character set Windows gives the number `page`.
fn charset(page: u32) -> Charset {
    let label = match page {
        65001 => "utf-8".to_string(),
        932 => "shift_jis".to_string(),
        936 => "gbk".to_string(),
        949 => "euc-kr".to_string(),
        950 => "big5".to_string(),
        20127 => "us-ascii".to_string(),
        28591..=28605 => format!("iso-8859-{}", page - 28590),
        874 | 1250..=1258 => format!("windows-{page}"),
        _ => "windows-1252".to_string(),
    };
    Charset::from_label(label.as_bytes()).unwrap_or_default()
}

/// What the compressed RTF of a message starts from: text that every such body is likely to
/// hold, which the compression may point back into.
const RTF_SEED: &[u8] = b"{\\rtf1\\ansi\\mac\\deff0\\deftab720{\\fonttbl;}{\\f0\\fnil \\froman \\fswiss \
\\fmodern \\fscript \\fdecor MS Sans SerifSymbolArialTimes New RomanCourier{\\colortbl\\red0\\green0\\blue0\
\r\n\\par \\pard\\plain\\f0\\fs20\\b\\i\\u\\tab\\tx";

/// The RTF in a compressed RTF stream, as Outlook stores the body of a message.
fn decompress_rtf(data: &[u8]) -> Option<Vec<u8>> {
    let header = data.get(..16)?;
    let body = &data[16..];
    match &header[8..12] {
        // Stored as it is.
        b"MELA" => return Some(body.to_vec()),
        b"LZFu" => {}
        _ => return None,
    }
    let mut window = [0u8; 4096];
    window[..RTF_SEED.len()].copy_from_slice(RTF_SEED);
    let mut at = RTF_SEED.len();
    let mut out = Vec::with_capacity(body.len() * 2);
    let mut bytes = body.iter().copied();
    // A byte of flags, then eight items: a byte as it is, or two bytes that name a place in
    // the last 4096 bytes and a length to repeat from there.
    'stream: while let Some(flags) = bytes.next() {
        for bit in 0..8 {
            if flags & (1 << bit) == 0 {
                let Some(byte) = bytes.next() else { break 'stream };
                out.push(byte);
                window[at] = byte;
                at = (at + 1) % 4096;
                continue;
            }
            let (Some(high), Some(low)) = (bytes.next(), bytes.next()) else {
                break 'stream;
            };
            let reference = u16::from_be_bytes([high, low]);
            let from = usize::from(reference >> 4);
            // A reference to where the next byte goes ends the stream.
            if from == at {
                break 'stream;
            }
            for step in 0..usize::from(reference & 0xf) + 2 {
                let byte = window[(from + step) % 4096];
                out.push(byte);
                window[at] = byte;
                at = (at + 1) % 4096;
            }
        }
    }
    Some(out)
}

/// Parts of an RTF file that are not its text.
const RTF_NOT_TEXT: &[&str] = &[
    "fonttbl",
    "colortbl",
    "stylesheet",
    "info",
    "pict",
    "object",
    "header",
    "headerl",
    "headerr",
    "headerf",
    "footer",
    "footerl",
    "footerr",
    "footerf",
    "footnote",
    "listtable",
    "listoverridetable",
    "rsidtbl",
    "generator",
    "themedata",
    "colorschememapping",
    "latentstyles",
    "datastore",
    "xmlnstbl",
    "fldinst",
];

/// The text of an RTF body: its paragraphs as lines, without the formatting.
fn rtf_text(rtf: &[u8]) -> String {
    // What a group says about the text in it: whether it is text at all, and how many
    // characters stand in for a Unicode character for programs that know none.
    #[derive(Clone, Copy)]
    struct Group {
        skip: bool,
        stand_ins: usize,
    }
    let mut groups = vec![Group {
        skip: false,
        stand_ins: 1,
    }];
    let mut out = String::new();
    // Bytes in the code page of the file, waiting to be decoded together.
    let mut pending: Vec<u8> = Vec::new();
    let mut page = 1252;
    // Stand-ins still to pass over.
    let mut pass = 0;
    let flush = |pending: &mut Vec<u8>, out: &mut String, page: u32| {
        if !pending.is_empty() {
            out.push_str(&charset(page).decode(pending));
            pending.clear();
        }
    };
    let mut at = 0;
    while at < rtf.len() {
        let byte = rtf[at];
        at += 1;
        let group = *groups.last().unwrap_or(&Group {
            skip: false,
            stand_ins: 1,
        });
        match byte {
            b'{' => groups.push(group),
            b'}' => {
                if groups.len() > 1 {
                    groups.pop();
                }
            }
            b'\\' => {
                let Some(&next) = rtf.get(at) else { break };
                if next == b'\'' {
                    // A byte written as two hex digits.
                    let hex = rtf
                        .get(at + 1..at + 3)
                        .and_then(|hex| std::str::from_utf8(hex).ok());
                    let value = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok());
                    at = (at + 3).min(rtf.len());
                    if pass > 0 {
                        pass -= 1;
                    } else if let (Some(value), false) = (value, group.skip) {
                        pending.push(value);
                    }
                    continue;
                }
                if !next.is_ascii_alphabetic() {
                    at += 1;
                    flush(&mut pending, &mut out, page);
                    match next {
                        b'*' => {
                            if let Some(group) = groups.last_mut() {
                                group.skip = true;
                            }
                        }
                        b'\\' | b'{' | b'}' if !group.skip => out.push(char::from(next)),
                        b'~' if !group.skip => out.push(' '),
                        b'_' if !group.skip => out.push('-'),
                        b'\r' | b'\n' if !group.skip => out.push('\n'),
                        _ => {}
                    }
                    continue;
                }
                let start = at;
                while rtf.get(at).is_some_and(u8::is_ascii_alphabetic) {
                    at += 1;
                }
                let word = std::str::from_utf8(&rtf[start..at]).unwrap_or_default();
                let number_start = at;
                if rtf.get(at) == Some(&b'-') {
                    at += 1;
                }
                while rtf.get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
                let number = std::str::from_utf8(&rtf[number_start..at])
                    .ok()
                    .and_then(|number| number.parse::<i32>().ok());
                // One space after a control word belongs to it.
                if rtf.get(at) == Some(&b' ') {
                    at += 1;
                }
                if word != "u" {
                    flush(&mut pending, &mut out, page);
                }
                if RTF_NOT_TEXT.contains(&word) {
                    if let Some(group) = groups.last_mut() {
                        group.skip = true;
                    }
                    continue;
                }
                match (word, number) {
                    ("ansicpg", Some(number)) => page = number.unsigned_abs(),
                    ("uc", Some(number)) => {
                        if let Some(group) = groups.last_mut() {
                            group.stand_ins = number.unsigned_abs() as usize;
                        }
                    }
                    ("bin", Some(number)) => at = (at + number.unsigned_abs() as usize).min(rtf.len()),
                    _ if group.skip => {}
                    ("u", Some(number)) => {
                        flush(&mut pending, &mut out, page);
                        // Written as a signed 16-bit number.
                        let code = u32::from(number as i16 as u16);
                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        pass = group.stand_ins;
                    }
                    ("par" | "line" | "sect" | "page" | "row", _) => out.push('\n'),
                    ("tab" | "cell", _) => out.push('\t'),
                    ("emdash", _) => out.push('—'),
                    ("endash", _) => out.push('–'),
                    ("lquote", _) => out.push('‘'),
                    ("rquote", _) => out.push('’'),
                    ("ldblquote", _) => out.push('“'),
                    ("rdblquote", _) => out.push('”'),
                    ("bullet", _) => out.push('•'),
                    _ => {}
                }
            }
            b'\r' | b'\n' => {}
            _ if pass > 0 => pass -= 1,
            _ if group.skip => {}
            byte => pending.push(byte),
        }
    }
    flush(&mut pending, &mut out, page);
    // Surrogate halves written as two `\u` words come out as replacement characters; lines
    // keep at most one empty line between them.
    let mut text = String::new();
    let mut empty = 0;
    for line in out.lines() {
        let line = line.trim_end();
        empty = if line.is_empty() { empty + 1 } else { 0 };
        if empty < 2 {
            text.push_str(line);
            text.push('\n');
        }
    }
    text.trim().to_string()
}

/// A number or a time of a message: the type of the property and its eight bytes.
type Fixed = HashMap<u16, (u16, [u8; 8])>;

struct Store<'a> {
    file: cfb::CompoundFile<Cursor<&'a [u8]>>,
}

impl Store<'_> {
    fn stream(&mut self, dir: &str, name: &str) -> Option<Vec<u8>> {
        let stream = self.file.open_stream(format!("{dir}/{name}")).ok()?;
        let mut bytes = Vec::new();
        stream.take(MAX_PART_BYTES).read_to_end(&mut bytes).ok()?;
        Some(bytes)
    }

    /// The property `id` of the object in `dir` as bytes.
    fn binary(&mut self, dir: &str, id: u16) -> Option<Vec<u8>> {
        self.stream(dir, &format!("__substg1.0_{id:04X}0102"))
    }

    /// The property `id` of the object in `dir` as text: stored as UTF-16, or in the code
    /// page of the message.
    fn text(&mut self, dir: &str, id: u16, page: u32) -> Option<String> {
        let text = match self.stream(dir, &format!("__substg1.0_{id:04X}001F")) {
            Some(wide) => {
                let units: Vec<u16> = wide
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_le_bytes(*pair))
                    .collect();
                String::from_utf16_lossy(&units)
            }
            None => {
                let narrow = self.stream(dir, &format!("__substg1.0_{id:04X}001E"))?;
                charset(page).decode(&narrow).into_owned()
            }
        };
        let text = text.trim_end_matches('\0').to_string();
        (!text.trim().is_empty()).then_some(text)
    }

    /// The numbers and times of the object in `dir`; `header` is how many bytes come before
    /// the rows, which depends on what the object is.
    fn fixed(&mut self, dir: &str, header: usize) -> Fixed {
        let rows = self.stream(dir, "__properties_version1.0").unwrap_or_default();
        let rows = rows.get(header..).unwrap_or_default();
        rows.as_chunks::<16>()
            .0
            .iter()
            .map(|row| {
                let kind = u16::from_le_bytes([row[0], row[1]]);
                let id = u16::from_le_bytes([row[2], row[3]]);
                let mut value = [0u8; 8];
                value.copy_from_slice(&row[8..16]);
                (id, (kind, value))
            })
            .collect()
    }

    /// The directories in `dir` whose names start with `prefix`, in order.
    fn storages(&self, dir: &str, prefix: &str) -> Vec<String> {
        let root = if dir.is_empty() { "/" } else { dir };
        let Ok(entries) = self.file.read_storage(root) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter(|entry| entry.is_storage() && entry.name().starts_with(prefix))
            .map(|entry| format!("{dir}/{}", entry.name()))
            .collect();
        names.sort();
        names
    }

    /// The message in `dir`. `header` is 32 for the message the file is, 24 for one attached
    /// to another.
    fn letter(&mut self, dir: &str, header: usize, depth: usize) -> Letter {
        let fixed = self.fixed(dir, header);
        let number = |id: u16| {
            fixed
                .get(&id)
                .map(|(_, value)| u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
        };
        let page = number(MESSAGE_CODE_PAGE).unwrap_or(1252);
        let time = [SUBMIT_TIME, DELIVERY_TIME].iter().find_map(|id| {
            let (_, value) = fixed.get(id)?;
            // Tenths of a microsecond since 1601.
            let ticks = i64::try_from(u64::from_le_bytes(*value)).ok()?;
            (ticks > 0).then(|| format!("{} UTC", date_time(ticks / 10_000_000 - 11_644_473_600)))
        });

        let person = |name: Option<String>, address: Option<String>| match (name, address) {
            (Some(name), Some(address)) if name != address => format!("{name} <{address}>"),
            (_, Some(only)) | (Some(only), None) => only,
            (None, None) => String::new(),
        };
        // An address inside an Exchange organisation is not one mail can be sent to.
        let smtp = |address: Option<String>| address.filter(|address| address.contains('@'));
        let sender =
            smtp(self.text(dir, SENDER_SMTP, page)).or_else(|| smtp(self.text(dir, SENDER_ADDRESS, page)));
        let from = person(self.text(dir, SENDER_NAME, page), sender);

        let (mut to, mut cc) = (Vec::new(), Vec::new());
        for recipient in self.storages(dir, "__recip_version1.0_") {
            let kind = self
                .fixed(&recipient, 8)
                .get(&RECIPIENT_TYPE)
                .map(|(_, value)| value[0]);
            let address = smtp(self.text(&recipient, SMTP_ADDRESS, page))
                .or_else(|| smtp(self.text(&recipient, ADDRESS, page)));
            let who = person(self.text(&recipient, DISPLAY_NAME, page), address);
            match kind {
                _ if who.is_empty() => {}
                Some(2) => cc.push(who),
                // Who got a hidden copy is not written down for the others.
                Some(3) => {}
                _ => to.push(who),
            }
        }
        let listed = |people: Vec<String>, shown: Option<String>| match people.is_empty() {
            true => shown.unwrap_or_default().trim().to_string(),
            false => people.join(", "),
        };
        let to = listed(to, self.text(dir, DISPLAY_TO, page));
        let cc = listed(cc, self.text(dir, DISPLAY_CC, page));

        let body = match self.text(dir, BODY, page) {
            Some(plain) => plain.replace("\r\n", "\n"),
            None => match self.binary(dir, BODY_HTML) {
                Some(html) => {
                    let page = number(INTERNET_CODE_PAGE).unwrap_or(page);
                    html_to_markdown(&charset(page).decode(&html))
                }
                None => {
                    let rtf = self
                        .binary(dir, BODY_RTF)
                        .and_then(|packed| decompress_rtf(&packed));
                    rtf.map(|rtf| rtf_text(&rtf)).unwrap_or_default()
                }
            },
        };

        let mut attachments = Vec::new();
        let mut enclosed = Vec::new();
        for (index, attachment) in self.storages(dir, "__attach_version1.0_").into_iter().enumerate() {
            let inner = format!("{attachment}/{ATTACHED_MESSAGE}");
            if self.file.is_storage(&inner) {
                if depth < MAX_DEPTH {
                    enclosed.push(self.letter(&inner, 24, depth + 1));
                }
                continue;
            }
            let Some(data) = self.binary(&attachment, ATTACHMENT_DATA) else {
                continue;
            };
            let name = [ATTACHMENT_LONG_NAME, ATTACHMENT_NAME, DISPLAY_NAME]
                .iter()
                .find_map(|id| self.text(&attachment, *id, page))
                .unwrap_or_else(|| format!("attachment-{}", index + 1));
            attachments.push((name, data));
        }
        Letter {
            subject: self.text(dir, SUBJECT, page).unwrap_or_default(),
            from,
            to,
            cc,
            date: time.unwrap_or_default(),
            body,
            attachments,
            enclosed,
        }
    }
}

pub(super) fn convert(bytes: &[u8]) -> Result<Converted, String> {
    let file = cfb::CompoundFile::open(Cursor::new(bytes))
        .map_err(|e| format!("it is not a readable Outlook message: {e}"))?;
    let mut store = Store { file };
    if !store.file.is_stream("/__properties_version1.0") {
        return Err("it holds no message: it is not an Outlook message file".to_string());
    }
    Ok(store.letter("", 32, 0).into_converted())
}

#[cfg(test)]
pub(super) mod tests {
    use std::io::Write;

    use super::*;

    /// The rows of a `__properties_version1.0` stream after a header of `header` bytes.
    pub fn fixed_rows(header: usize, rows: &[(u16, u16, u64)]) -> Vec<u8> {
        let mut bytes = vec![0u8; header];
        for (id, kind, value) in rows {
            bytes.extend_from_slice(&kind.to_le_bytes());
            bytes.extend_from_slice(&id.to_le_bytes());
            bytes.extend_from_slice(&6u32.to_le_bytes());
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    pub fn wide(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    /// A compound file with these streams; the directories on the way to each are made.
    pub fn compound(streams: &[(String, Vec<u8>)]) -> Vec<u8> {
        let mut file = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        for (path, bytes) in streams {
            if let Some((dir, _)) = path.rsplit_once('/')
                && !dir.is_empty()
            {
                file.create_storage_all(dir).unwrap();
            }
            file.create_stream(path).unwrap().write_all(bytes).unwrap();
        }
        file.flush().unwrap();
        file.into_inner().into_inner()
    }

    /// 2026-03-01 14:30:03 UTC as Windows counts time.
    pub const MARCH_FIRST: u64 = (1_772_375_403 + 11_644_473_600) * 10_000_000;

    fn string(dir: &str, id: u16, text: &str) -> (String, Vec<u8>) {
        (format!("{dir}/__substg1.0_{id:04X}001F"), wide(text))
    }

    #[test]
    fn a_message_is_read_with_its_people_its_time_its_attachments_and_the_message_inside() {
        let recipient = |index: usize, kind: u64, name: &str, address: &str| {
            let dir = format!("/__recip_version1.0_#{index:08X}");
            vec![
                (
                    format!("{dir}/__properties_version1.0"),
                    fixed_rows(8, &[(RECIPIENT_TYPE, 3, kind)]),
                ),
                string(&dir, DISPLAY_NAME, name),
                string(&dir, SMTP_ADDRESS, address),
            ]
        };
        let attached = "/__attach_version1.0_#00000001/__substg1.0_3701000D";
        let mut streams = vec![
            (
                "/__properties_version1.0".to_string(),
                fixed_rows(
                    32,
                    &[(SUBMIT_TIME, 0x40, MARCH_FIRST), (MESSAGE_CODE_PAGE, 3, 1252)],
                ),
            ),
            string("", SUBJECT, "Gears for Lyon\0"),
            string("", SENDER_NAME, "Grace Hopper"),
            // The sender is inside the organisation; the address for mail is stored apart.
            string("", SENDER_ADDRESS, "/O=ACME/OU=EXCHANGE/CN=RECIPIENTS/CN=GRACE"),
            string("", SENDER_SMTP, "grace@example.com"),
            string("", DISPLAY_TO, "Ada Lovelace; Linus"),
            string("", BODY, "Dear all,\r\n\r\nthe gears ship on Monday.\r\n"),
            (
                "/__substg1.0_10130102".to_string(),
                b"<p>not used: there is plain text</p>".to_vec(),
            ),
            (
                "/__attach_version1.0_#00000000/__properties_version1.0".to_string(),
                fixed_rows(8, &[]),
            ),
            string("/__attach_version1.0_#00000000", ATTACHMENT_NAME, "PACKIN~1.CSV"),
            string(
                "/__attach_version1.0_#00000000",
                ATTACHMENT_LONG_NAME,
                "packing list.csv",
            ),
            (
                "/__attach_version1.0_#00000000/__substg1.0_37010102".to_string(),
                b"gear,12\n".to_vec(),
            ),
            (
                format!("{attached}/__properties_version1.0"),
                fixed_rows(24, &[(DELIVERY_TIME, 0x40, MARCH_FIRST - 600_000_000)]),
            ),
            // An older message, stored in the code page of its time.
            (
                format!("{attached}/__substg1.0_0037001E"),
                b"Fr\xfchere Nachricht".to_vec(),
            ),
            (format!("{attached}/__substg1.0_0C1A001E"), b"Linus".to_vec()),
            (
                format!("{attached}/__substg1.0_10130102"),
                b"<html><body><p>Wann <b>kommen</b> die R\xe4der?</p></body></html>".to_vec(),
            ),
        ];
        streams.extend(recipient(0, 1, "Ada Lovelace", "ada@example.com"));
        streams.extend(recipient(1, 2, "linus@example.com", "linus@example.com"));
        streams.extend(recipient(2, 3, "Hidden", "hidden@example.com"));
        let converted = convert(&compound(&streams)).unwrap();
        assert_eq!(
            converted.markdown,
            "# Gears for Lyon\n\n\
             - From: Grace Hopper <grace@example.com>\n\
             - To: Ada Lovelace <ada@example.com>\n\
             - Cc: linus@example.com\n\
             - Date: 2026-03-01 14:30:03 UTC\n\
             \n\
             ## Message\n\n\
             Dear all,\n\nthe gears ship on Monday.\n\
             \n\
             ## Attachments\n\n\
             - attachments/packing_list.csv (8 bytes)\n\
             \n\
             ## Frühere Nachricht\n\n\
             - From: Linus\n\
             - Date: 2026-03-01 14:29:03 UTC\n\
             \n\
             ### Message\n\n\
             Wann **kommen** die Räder?\n"
        );
        assert_eq!(
            converted.files,
            [("attachments/packing_list.csv".to_string(), b"gear,12\n".to_vec())]
        );
    }

    #[test]
    fn a_message_without_recipient_records_shows_the_names_outlook_displays() {
        let streams = vec![
            ("/__properties_version1.0".to_string(), fixed_rows(32, &[])),
            string("", DISPLAY_TO, "Ada Lovelace; Linus "),
            string("", DISPLAY_CC, "Grace"),
        ];
        assert_eq!(
            convert(&compound(&streams)).unwrap().markdown,
            "# (no subject)\n\n- To: Ada Lovelace; Linus\n- Cc: Grace\n\n## Message\n\n(no text)\n"
        );
        // A compound file that is something else, and a file that is not one.
        let other = compound(&[("/WordDocument".to_string(), b"x".to_vec())]);
        assert!(
            convert(&other)
                .unwrap_err()
                .contains("not an Outlook message file")
        );
        assert!(convert(b"plain").is_err());
    }

    #[test]
    fn a_body_stored_only_as_compressed_rtf_is_unpacked_and_read_as_text() {
        // The example of the format's specification.
        let packed: Vec<u8> = [
            0x2d, 0x00, 0x00, 0x00, 0x2b, 0x00, 0x00, 0x00, 0x4c, 0x5a, 0x46, 0x75, 0xf1, 0xc5, 0xc7, 0xa7,
            0x03, 0x00, 0x0a, 0x00, 0x72, 0x63, 0x70, 0x67, 0x31, 0x32, 0x35, 0x42, 0x32, 0x0a, 0xf3, 0x20,
            0x68, 0x65, 0x6c, 0x09, 0x00, 0x20, 0x62, 0x77, 0x05, 0xb0, 0x6c, 0x64, 0x7d, 0x0a, 0x80, 0x0f,
            0xa0,
        ]
        .to_vec();
        assert_eq!(RTF_SEED.len(), 207);
        let rtf = decompress_rtf(&packed).unwrap();
        assert_eq!(rtf, b"{\\rtf1\\ansi\\ansicpg1252\\pard hello world}\r\n");
        assert_eq!(rtf_text(&rtf), "hello world");

        let mut stored = vec![0u8; 8];
        stored.extend_from_slice(b"MELA\0\0\0\0{\\rtf1 as it is}");
        assert_eq!(decompress_rtf(&stored).unwrap(), b"{\\rtf1 as it is}");
        assert_eq!(decompress_rtf(b"too short"), None);
        assert_eq!(decompress_rtf(&[0u8; 20]), None);

        let streams = vec![
            ("/__properties_version1.0".to_string(), fixed_rows(32, &[])),
            string("", SUBJECT, "Only RTF"),
            ("/__substg1.0_10090102".to_string(), packed),
        ];
        assert_eq!(
            convert(&compound(&streams)).unwrap().markdown,
            "# Only RTF\n\n## Message\n\nhello world\n"
        );
    }

    #[test]
    fn rtf_becomes_its_text_without_fonts_pictures_and_the_html_it_wraps() {
        let rtf = br"{\rtf1\ansi\ansicpg1252\fromhtml1 \deff0{\fonttbl{\f0\fswiss Arial;}}{\colortbl;\red0\green0\blue0;}
{\*\generator Riched20;}{\*\htmltag64 <p>}\htmlrtf {\htmlrtf0 Caf\'e9 \u8364?5 \{net\}\par
{\*\htmltag72 </p>}\htmlrtf}\htmlrtf0
{\pict\wmetafile8 010009000003}\b bold\b0\tab next\line
\uc2\u-10179 ??\u-8704 ??done\par\par\par last\~word {\field{\*\fldinst HYPERLINK x}{\fldrslt link}}}";
        assert_eq!(
            rtf_text(rtf),
            "Café €5 {net}\nbold\tnext\n\u{fffd}\u{fffd}done\n\nlast word link"
        );
        assert_eq!(rtf_text(b""), "");
        assert_eq!(rtf_text(b"{\\rtf1 cut \\"), "cut");
    }
}
