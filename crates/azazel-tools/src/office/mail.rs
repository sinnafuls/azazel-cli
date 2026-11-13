//! A mail message (`.eml`) as Markdown: who it is from and to, when it was sent, what it says,
//! and its attachments as files beside it.

use mail_parser::{AddressList, MessageParser, PartKind};

use super::{Converted, MAX_PARTS, html_to_markdown, safe_name, unused};

/// A message, whichever kind of file it came from.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Letter {
    pub subject: String,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub date: String,
    /// What it says, as Markdown.
    pub body: String,
    /// The name and the bytes of each attachment.
    pub attachments: Vec<(String, Vec<u8>)>,
    /// The messages attached to it that cannot be files of their own.
    pub enclosed: Vec<Letter>,
}

impl Letter {
    /// The letter under a heading of `level` number signs, its attachments as files under
    /// `attachments/`.
    fn write(self, level: usize, names: &mut Vec<String>, out: &mut Converted) {
        let marks = "#".repeat(level);
        let subject = self.subject.split_whitespace().collect::<Vec<_>>().join(" ");
        let subject = if subject.is_empty() {
            "(no subject)"
        } else {
            &subject
        };
        let markdown = &mut out.markdown;
        if !markdown.is_empty() {
            markdown.push('\n');
        }
        markdown.push_str(&format!("{marks} {subject}\n\n"));
        let fields = [
            ("From", &self.from),
            ("To", &self.to),
            ("Cc", &self.cc),
            ("Date", &self.date),
        ];
        for (label, value) in fields.iter().filter(|(_, value)| !value.is_empty()) {
            markdown.push_str(&format!("- {label}: {value}\n"));
        }
        if fields.iter().any(|(_, value)| !value.is_empty()) {
            markdown.push('\n');
        }
        // Mail programs pad their text with empty lines; one is enough.
        let mut body = String::new();
        let mut empty = 0;
        for line in self.body.trim().lines() {
            let line = line.trim_end();
            empty = if line.is_empty() { empty + 1 } else { 0 };
            if empty < 2 {
                body.push_str(line);
                body.push('\n');
            }
        }
        let body = if body.is_empty() {
            "(no text)"
        } else {
            body.trim_end()
        };
        markdown.push_str(&format!("{marks}# Message\n\n{body}\n"));
        if !self.attachments.is_empty() {
            markdown.push_str(&format!("\n{marks}# Attachments\n\n"));
        }
        for (name, bytes) in self.attachments.into_iter().take(MAX_PARTS) {
            let file = format!("attachments/{}", unused(safe_name(&name, 80), names));
            out.markdown
                .push_str(&format!("- {file} ({} bytes)\n", bytes.len()));
            out.files.push((file, bytes));
        }
        for enclosed in self.enclosed {
            enclosed.write(level + 1, names, out);
        }
    }

    pub(super) fn into_converted(self) -> Converted {
        let mut out = Converted::default();
        self.write(1, &mut Vec::new(), &mut out);
        out
    }
}

/// The people of an address field, as `Name <address>`, with commas between them.
fn people(list: Option<AddressList<'_>>) -> String {
    let Some(list) = list else { return String::new() };
    let people: Vec<String> = list
        .mailboxes()
        .filter_map(|mailbox| match (mailbox.name(), mailbox.address()) {
            (Some(name), Some(address)) => Some(format!("{name} <{address}>")),
            (None, Some(only)) | (Some(only), None) => Some(only.to_string()),
            (None, None) => None,
        })
        .collect();
    people.join(", ")
}

/// Whether `bytes` start the way a message does: with a header field, after the line some
/// mail programs put before the first one.
pub(super) fn looks_like_mail(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]);
    let mut lines = text
        .lines()
        .skip_while(|line| line.trim().is_empty() || line.starts_with("From "));
    lines.next().is_some_and(|line| {
        let name = line.split(':').next().unwrap_or_default();
        line.contains(':') && !name.is_empty() && name.chars().all(|c| c.is_ascii_graphic())
    })
}

pub(super) fn convert(bytes: &[u8]) -> Result<Converted, String> {
    let message = MessageParser::default()
        .parse(bytes)
        .ok_or("it is not a mail message")?;
    let mut body = String::new();
    for part in message.text_body() {
        let text = match part.kind() {
            PartKind::Html => part.html_utf8().map(|html| html_to_markdown(&html)),
            _ => part.text().map(|text| text.trim().to_string()),
        };
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&text);
        }
    }
    let attachments = message
        .attachments()
        .enumerate()
        .map(|(index, part)| {
            let name = match (part.attachment_name(), part.kind()) {
                (Some(name), _) if !name.trim().is_empty() => name.to_string(),
                // A forwarded message is a message again: `read` takes the file.
                (_, PartKind::Message(_)) => format!("message-{}.eml", index + 1),
                _ => match part.content_type().and_then(|kind| kind.subtype()) {
                    Some(subtype) => format!("attachment-{}.{}", index + 1, safe_name(subtype, 12)),
                    None => format!("attachment-{}", index + 1),
                },
            };
            (name, part.decoded().into_owned())
        })
        .collect();
    let letter = Letter {
        subject: message.subject().unwrap_or_default().to_string(),
        from: people(message.from()),
        to: people(message.to()),
        cc: people(message.cc()),
        date: message.date().map(|date| date.to_rfc3339()).unwrap_or_default(),
        body,
        attachments,
        enclosed: Vec::new(),
    };
    Ok(letter.into_converted())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_starts_with_a_header_field() {
        assert!(looks_like_mail(
            b"Received: from a by b\r\nSubject: x\r\n\r\nbody"
        ));
        assert!(looks_like_mail(
            b"\r\nFrom sender@example.com Mon Jan  5 2026\nX-Mailer: m\n"
        ));
        assert!(looks_like_mail(b"From: Ada <ada@example.com>\n"));
        assert!(!looks_like_mail(b"Dear all,\nthe meeting: it moved.\n"));
        assert!(!looks_like_mail(b"no colon here\n"));
        assert!(!looks_like_mail(b": empty name\n"));
        assert!(!looks_like_mail(b""));
    }

    #[test]
    fn a_message_with_only_html_is_converted_and_unnamed_attachments_get_names() {
        let message = "From: =?UTF-8?Q?Gr=C3=A4ce?= <grace@example.com>\r\n\
                       To: ada@example.com, Linus <linus@example.com>\r\n\
                       Subject: Gears\r\n\
                       MIME-Version: 1.0\r\n\
                       Content-Type: multipart/mixed; boundary=\"b\"\r\n\
                       \r\n\
                       --b\r\n\
                       Content-Type: text/html; charset=utf-8\r\n\
                       \r\n\
                       <html><head><style>p { color: red }</style></head><body><p>See <a href=\"https://example.com/g\">the <b>gear</b></a>.</p></body></html>\r\n\
                       --b\r\n\
                       Content-Type: application/pdf\r\n\
                       Content-Transfer-Encoding: base64\r\n\
                       \r\n\
                       JVBERi0=\r\n\
                       --b\r\n\
                       Content-Type: message/rfc822\r\n\
                       \r\n\
                       From: ada@example.com\r\n\
                       Subject: Earlier\r\n\
                       \r\n\
                       The earlier one.\r\n\
                       --b--\r\n";
        let converted = convert(message.as_bytes()).unwrap();
        assert_eq!(
            converted.markdown,
            "# Gears\n\n\
             - From: Gräce <grace@example.com>\n\
             - To: ada@example.com, Linus <linus@example.com>\n\
             \n\
             ## Message\n\n\
             See [the **gear**](https://example.com/g).\n\
             \n\
             ## Attachments\n\n\
             - attachments/attachment-1.pdf (5 bytes)\n\
             - attachments/message-2.eml (59 bytes)\n"
        );
        assert_eq!(
            converted.files[0],
            ("attachments/attachment-1.pdf".to_string(), b"%PDF-".to_vec())
        );
        let forwarded = std::str::from_utf8(&converted.files[1].1).unwrap();
        assert!(
            forwarded.starts_with("From: ada@example.com\r\nSubject: Earlier\r\n"),
            "{forwarded}"
        );
    }

    #[test]
    fn letters_inside_a_letter_follow_it_one_level_down() {
        let inner = Letter {
            subject: "Inner".into(),
            from: "b@example.com".into(),
            body: "inner text".into(),
            attachments: vec![("a.txt".into(), b"2".to_vec())],
            ..Letter::default()
        };
        let outer = Letter {
            subject: " Outer\r\n subject ".into(),
            date: "2026-03-01 14:30:03 UTC".into(),
            attachments: vec![
                ("a.txt".into(), b"1".to_vec()),
                ("../b c.txt".into(), b"22".to_vec()),
            ],
            enclosed: vec![inner],
            ..Letter::default()
        };
        let converted = outer.into_converted();
        assert_eq!(
            converted.markdown,
            "# Outer subject\n\n- Date: 2026-03-01 14:30:03 UTC\n\n## Message\n\n(no text)\n\n## Attachments\n\n\
             - attachments/a.txt (1 bytes)\n- attachments/_b_c.txt (2 bytes)\n\n\
             ## Inner\n\n- From: b@example.com\n\n### Message\n\ninner text\n\n### Attachments\n\n\
             - attachments/a-2.txt (1 bytes)\n"
        );
        let files: Vec<&str> = converted.files.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            files,
            ["attachments/a.txt", "attachments/_b_c.txt", "attachments/a-2.txt"]
        );
    }
}
