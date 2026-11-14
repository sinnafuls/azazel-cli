//! Reading the arguments of a tool call while the model is still writing them: the string
//! values of a JSON object of which only the start has arrived.

/// A string value under a key of an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Field {
    /// How deep the object stands: 1 for the top level, 3 for an object in an array under a
    /// top-level key.
    pub depth: usize,
    pub key: String,
    /// The text, with its escapes decoded, as far as it has arrived.
    pub value: String,
    /// The closing quote has arrived.
    pub complete: bool,
}

enum Frame {
    Object { key: Option<String>, wants_key: bool },
    Array,
}

/// Every string value of `prefix` that stands under a key, in the order they come. The last may
/// be cut short. Whatever cannot be read (the prefix ends, or it is not JSON) ends the reading:
/// what was read before it is returned.
pub(super) fn string_fields(prefix: &str) -> Vec<Field> {
    let mut fields = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut rest = prefix;
    loop {
        rest = rest.trim_start();
        let Some(c) = rest.chars().next() else {
            return fields;
        };
        rest = &rest[c.len_utf8()..];
        match c {
            '{' => stack.push(Frame::Object {
                key: None,
                wants_key: true,
            }),
            '[' => stack.push(Frame::Array),
            '}' | ']' => {
                stack.pop();
            }
            ',' => {
                if let Some(Frame::Object { key, wants_key }) = stack.last_mut() {
                    *key = None;
                    *wants_key = true;
                }
            }
            ':' => {
                if let Some(Frame::Object { wants_key, .. }) = stack.last_mut() {
                    *wants_key = false;
                }
            }
            '"' => {
                let (text, complete, after) = string(rest);
                rest = after;
                let depth = stack.len();
                match stack.last_mut() {
                    Some(Frame::Object { key, wants_key: true }) => {
                        *key = Some(text);
                    }
                    Some(Frame::Object {
                        key: Some(key),
                        wants_key: false,
                    }) => fields.push(Field {
                        depth,
                        key: key.clone(),
                        value: text,
                        complete,
                    }),
                    _ => {}
                }
                if !complete {
                    return fields;
                }
            }
            // A number, `true`, `false` or `null`: nothing to keep.
            _ => {
                let end = rest
                    .find(|c: char| matches!(c, ',' | '}' | ']') || c.is_whitespace())
                    .unwrap_or(rest.len());
                rest = &rest[end..];
            }
        }
    }
}

/// The string that starts at `s` (after its opening quote): its text decoded, whether its
/// closing quote came, and what follows it. An escape cut short is left out.
fn string(s: &str) -> (String, bool, &str) {
    let mut out = String::new();
    let mut chars = s.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => return (out, true, &s[at + 1..]),
            '\\' => {
                let Some((_, escape)) = chars.next() else {
                    return (out, false, "");
                };
                match escape {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'u' => {
                        let unit = match hex4(&mut chars) {
                            Hex::Unit(unit) => unit,
                            Hex::Short => return (out, false, ""),
                            Hex::Bad => {
                                out.push(char::REPLACEMENT_CHARACTER);
                                continue;
                            }
                        };
                        let decoded = match unit {
                            0xD800..=0xDBFF => {
                                // A high surrogate needs the low one that follows it.
                                let mut ahead = chars.clone();
                                let low = match (ahead.next(), ahead.next()) {
                                    (Some((_, '\\')), Some((_, 'u'))) => hex4(&mut ahead),
                                    (None, _) | (Some((_, '\\')), None) => Hex::Short,
                                    _ => Hex::Bad,
                                };
                                match low {
                                    Hex::Short => return (out, false, ""),
                                    Hex::Unit(low @ 0xDC00..=0xDFFF) => {
                                        chars = ahead;
                                        char::from_u32(0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00))
                                    }
                                    _ => None,
                                }
                            }
                            unit => char::from_u32(unit),
                        };
                        out.push(decoded.unwrap_or(char::REPLACEMENT_CHARACTER));
                    }
                    // `\"`, `\\`, `\/`, and anything else stands for itself.
                    other => out.push(other),
                }
            }
            c => out.push(c),
        }
    }
    (out, false, "")
}

/// The four hex digits of a `\u` escape.
enum Hex {
    Unit(u32),
    /// They have not all arrived.
    Short,
    /// One of them is not a hex digit.
    Bad,
}

fn hex4(chars: &mut std::str::CharIndices<'_>) -> Hex {
    let mut unit = 0;
    for _ in 0..4 {
        let Some((_, digit)) = chars.next() else {
            return Hex::Short;
        };
        match digit.to_digit(16) {
            Some(value) => unit = unit * 16 + value,
            None => return Hex::Bad,
        }
    }
    Hex::Unit(unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top(prefix: &str) -> Vec<(String, String, bool)> {
        string_fields(prefix)
            .into_iter()
            .filter(|field| field.depth == 1)
            .map(|field| (field.key, field.value, field.complete))
            .collect()
    }

    #[test]
    fn the_string_fields_of_a_prefix_are_read_as_far_as_they_go() {
        let whole = r#"{"path": "src/a.rs", "lines": 12, "ok": true, "content": "fn main() {\n\t\"hi\" \u00e9\ud83d\ude00\\ end"}"#;
        assert_eq!(
            top(whole),
            [
                ("path".into(), "src/a.rs".into(), true),
                ("content".into(), "fn main() {\n\t\"hi\" é😀\\ end".into(), true),
            ]
        );
        // Every prefix reads without failing, and what it reads is the start of the whole.
        for end in (0..=whole.len()).filter(|end| whole.is_char_boundary(*end)) {
            let fields = top(&whole[..end]);
            if let Some((_, value, _)) = fields.iter().find(|(key, ..)| key == "content") {
                assert!(
                    "fn main() {\n\t\"hi\" é😀\\ end".starts_with(value.as_str()),
                    "{end}: {value:?}"
                );
            }
        }
        // An escape or a surrogate pair cut short is left out until the rest of it arrives.
        assert_eq!(top(r#"{"a": "x\"#), [("a".into(), "x".into(), false)]);
        assert_eq!(top(r#"{"a": "x\u00"#), [("a".into(), "x".into(), false)]);
        assert_eq!(top(r#"{"a": "x\ud83d"#), [("a".into(), "x".into(), false)]);
        assert_eq!(top(r#"{"a": "x\ud83d\ude"#), [("a".into(), "x".into(), false)]);
        // A key that has not been finished has no value yet.
        assert!(top(r#"{"path": "a", "conte"#).len() == 1);
    }

    #[test]
    fn nested_values_are_known_by_their_depth() {
        let edit = r#"{"path": "a.rs", "edits": [{"old": "x", "new": "y"}, {"old": "p", "new": "q\nr"#;
        let fields = string_fields(edit);
        let new: Vec<(&str, usize)> = fields
            .iter()
            .filter(|field| field.key == "new")
            .map(|field| (field.value.as_str(), field.depth))
            .collect();
        assert_eq!(new, [("y", 3), ("q\nr", 3)]);
        // Strings in arrays and the keys themselves are not values.
        assert_eq!(
            top(r#"{"tags": ["a", "b"], "name": "n"}"#),
            [("name".into(), "n".into(), true)]
        );
    }

    #[test]
    fn what_is_not_json_ends_the_reading_without_a_panic() {
        for junk in [
            "",
            "}",
            "]]]",
            "\"",
            "{\"a\" \"b\"",
            "{:::,,,\"a\":",
            "nul",
            "{\"a\": 1e",
            "\\u",
            "{\"a\":\"\\uZZZZ\"}",
        ] {
            let _ = string_fields(junk);
        }
        let bad = top(r#"{"a": "\uZZZZ", "b": "c"}"#);
        assert!(bad[0].1.starts_with('\u{FFFD}') && bad[0].2, "{bad:?}");
        assert_eq!(bad[1], ("b".into(), "c".into(), true));
    }
}
