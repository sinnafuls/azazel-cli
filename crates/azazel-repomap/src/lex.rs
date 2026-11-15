//! A small lexer for the line scanner: it blanks the strings and comments of a source file, so
//! that the scanner sees only code, and splits code into the identifiers it uses.

use crate::extract::Lang;

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80
}

pub(crate) fn is_ident_byte(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

/// Overwrites `from..to` with spaces, keeping the line breaks so that offsets and lines stay put.
fn wipe(buf: &mut [u8], from: usize, to: usize) {
    let to = to.min(buf.len());
    for byte in &mut buf[from..to] {
        if *byte != b'\n' {
            *byte = b' ';
        }
    }
}

/// The end (exclusive) of a quoted literal that starts at `i`. A literal that may not span lines
/// ends at the line break when it is not closed, so one stray quote cannot swallow the file.
fn quoted(b: &[u8], i: usize, quote: u8, multiline: bool) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j = after_escape(b, j),
            c if c == quote => return j + 1,
            b'\n' if !multiline => return j,
            _ => j += 1,
        }
    }
    b.len()
}

/// Past a backslash and the character it escapes, which may be several bytes long.
fn after_escape(b: &[u8], i: usize) -> usize {
    let mut j = i + 2;
    while j < b.len() && (0x80..0xC0).contains(&b[j]) {
        j += 1;
    }
    j
}

fn triple(b: &[u8], i: usize) -> bool {
    b.len() >= i + 3 && b[i] == b[i + 1] && b[i] == b[i + 2]
}

fn eol(b: &[u8], i: usize) -> usize {
    b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p)
}

/// `/* … */`, which nests in Rust.
fn block_comment(b: &[u8], i: usize, nests: bool) -> usize {
    let mut depth = 1;
    let mut j = i + 2;
    while j < b.len() {
        if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
            depth -= 1;
            j += 2;
            if depth == 0 {
                return j;
            }
        } else if nests && b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
            depth += 1;
            j += 2;
        } else {
            j += 1;
        }
    }
    b.len()
}

/// `'x'` and `'\n'` are literals in Rust, `'a` is a lifetime. Returns the end of a literal, or
/// `i + 1` for a lifetime.
fn rust_quote(b: &[u8], i: usize) -> usize {
    match b.get(i + 1) {
        Some(b'\\') => (i + 3..(i + 14).min(b.len()))
            .find(|&j| b[j] == b'\'')
            .map_or(i + 1, |j| j + 1),
        Some(&c) => {
            let width = match c {
                0xF0.. => 4,
                0xE0.. => 3,
                0xC0.. => 2,
                _ => 1,
            };
            if b.get(i + 1 + width) == Some(&b'\'') {
                i + 2 + width
            } else {
                i + 1
            }
        }
        None => i + 1,
    }
}

/// `r#"…"#`: returns the end when a raw string starts at `i` (after its `r`).
fn rust_raw(b: &[u8], i: usize) -> Option<usize> {
    let hashes = b[i..].iter().take_while(|&&c| c == b'#').count();
    if b.get(i + hashes) != Some(&b'"') {
        return None;
    }
    let mut j = i + hashes + 1;
    while j < b.len() {
        if b[j] == b'"' && b[j + 1..].iter().take_while(|&&c| c == b'#').count() >= hashes {
            return Some(j + 1 + hashes);
        }
        j += 1;
    }
    Some(b.len())
}

/// A regular expression literal that starts at `i`, on one line.
fn js_regex(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    let mut class = false;
    while j < b.len() {
        match b[j] {
            b'\\' => j = after_escape(b, j) - 1,
            b'[' => class = true,
            b']' => class = false,
            b'/' if !class => {
                j += 1;
                while b.get(j).is_some_and(|c| c.is_ascii_alphabetic()) {
                    j += 1;
                }
                return Some(j);
            }
            b'\n' => return None,
            _ => {}
        }
        j += 1;
    }
    None
}

/// The source with its strings and comments blanked, and the source with only its comments
/// blanked (for signatures, which show their strings). Both keep the length and the line breaks.
pub(crate) fn blank(lang: Lang, src: &str) -> (String, String) {
    let b = src.as_bytes();
    let mut code = b.to_vec();
    let mut readable = b.to_vec();
    let python = lang == Lang::Python;
    let rust = lang == Lang::Rust;
    let go = lang == Lang::Go;
    let script = !python && !rust && !go;
    let mut last = b'\n';
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if (!python && c == b'/' && next == Some(b'/')) || (python && c == b'#') {
            let end = eol(b, i);
            wipe(&mut code, i, end);
            wipe(&mut readable, i, end);
            i = end;
        } else if !python && c == b'/' && next == Some(b'*') {
            let end = block_comment(b, i, rust);
            wipe(&mut code, i, end);
            wipe(&mut readable, i, end);
            i = end;
        } else if c == b'"' || (c == b'\'' && !rust) || (c == b'`' && (go || script)) {
            let end = if python && triple(b, i) {
                let mut j = i + 3;
                while j < b.len() && !(b[j] == c && triple(b, j)) {
                    j = if b[j] == b'\\' { after_escape(b, j) } else { j + 1 };
                }
                (j + 3).min(b.len())
            } else {
                quoted(b, i, c, rust || go || c == b'`')
            };
            wipe(&mut code, i, end);
            i = end.max(i + 1);
            last = b'"';
        } else if rust && c == b'\'' {
            let end = rust_quote(b, i);
            wipe(&mut code, i, end);
            i = end;
        } else if script && c == b'/' && b"(,=:[!&|?{};+-*%~^\n".contains(&last) {
            match js_regex(b, i) {
                Some(end) => {
                    wipe(&mut code, i, end);
                    i = end;
                    last = b'"';
                }
                None => {
                    last = c;
                    i += 1;
                }
            }
        } else if is_ident_start(c) {
            let mut j = i + 1;
            while j < b.len() && is_ident_byte(b[j]) {
                j += 1;
            }
            let word = &b[i..j];
            if rust
                && matches!(word, b"r" | b"br" | b"cr")
                && matches!(b.get(j), Some(b'"' | b'#'))
                && let Some(end) = rust_raw(b, j)
            {
                wipe(&mut code, i, end);
                i = end;
                last = b'"';
                continue;
            }
            // After `return` a `/` starts a regular expression, not a division.
            last = if matches!(word, b"return" | b"typeof") {
                b'('
            } else {
                b[j - 1]
            };
            i = j;
        } else {
            if !c.is_ascii_whitespace() {
                last = c;
            }
            i += 1;
        }
    }
    (
        String::from_utf8_lossy(&code).into_owned(),
        String::from_utf8_lossy(&readable).into_owned(),
    )
}

fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "fn" | "let"
            | "mut"
            | "pub"
            | "use"
            | "impl"
            | "for"
            | "in"
            | "if"
            | "else"
            | "match"
            | "return"
            | "self"
            | "mod"
            | "struct"
            | "enum"
            | "trait"
            | "type"
            | "const"
            | "static"
            | "async"
            | "await"
            | "where"
            | "while"
            | "loop"
            | "break"
            | "continue"
            | "unsafe"
            | "crate"
            | "super"
            | "def"
            | "class"
            | "import"
            | "from"
            | "as"
            | "not"
            | "and"
            | "or"
            | "is"
            | "None"
            | "True"
            | "False"
            | "pass"
            | "lambda"
            | "with"
            | "try"
            | "except"
            | "finally"
            | "raise"
            | "yield"
            | "func"
            | "var"
            | "range"
            | "go"
            | "defer"
            | "package"
            | "function"
            | "export"
            | "default"
            | "new"
            | "this"
            | "null"
            | "true"
            | "false"
            | "interface"
            | "extends"
            | "typeof"
            | "switch"
            | "case"
            | "catch"
            | "throw"
            | "void"
            | "of"
            | "do"
    )
}

/// Calls `f` with every identifier of a line of blanked code that is not a keyword.
pub(crate) fn identifiers<'a>(line: &'a str, mut f: impl FnMut(&'a str)) {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() {
            // A number, `0xFF` and `1e5` included: not an identifier.
            while i < b.len() && is_ident_byte(b[i]) {
                i += 1;
            }
        } else if is_ident_start(c) {
            let start = i;
            while i < b.len() && is_ident_byte(b[i]) {
                i += 1;
            }
            if let Some(word) = line.get(start..i)
                && word.len() >= 2
                && !is_keyword(word)
            {
                f(word);
            }
        } else {
            i += 1;
        }
    }
}
