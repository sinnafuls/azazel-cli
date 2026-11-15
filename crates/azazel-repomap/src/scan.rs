//! The default extractor: a line scanner. It blanks strings and comments (`lex.rs`), then reads
//! definitions off the lines of code: by indentation for Python, by brace depth for the others.
//! It sees less than a grammar does (no macro bodies, no one-line `impl A { fn b() {} }`), but it
//! costs a few kilobytes instead of five megabytes.

use std::collections::HashMap;

use crate::extract::{Def, Lang, MAX_SIGNATURE, Tags};
use crate::lex::{self, blank, identifiers};

/// Most lines a signature may span before the scanner stops looking for its end.
const MAX_HEADER_LINES: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A function or method: what is defined inside its body is not recorded.
    Function,
    /// A type, constant or other definition without definitions of its own.
    Leaf,
    /// Holds definitions: a trait, class, module, interface.
    Container,
    /// Holds definitions but has no name of its own: `impl Foo`.
    Group,
}

/// What a line starts: the definition it is, before its signature has been read.
struct Item {
    shape: Shape,
    name: String,
    /// The signature stops at the `=` (a constant, a type alias).
    until_eq: bool,
    /// The signature is the first line only.
    one_line: bool,
    /// It is a definition only when the signature ends in `=>` (`const f = (x) => …`).
    arrow_only: bool,
    /// It is a definition only when the signature ends in `{` or `;` (a method, not a call).
    needs_end: bool,
}

impl Item {
    fn new(shape: Shape, name: &str) -> Item {
        Item {
            shape,
            name: name.to_string(),
            until_eq: false,
            one_line: false,
            arrow_only: false,
            needs_end: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Brace,
    Semi,
    Arrow,
    Eq,
    /// No end was found: the signature is the first line.
    Open,
}

/// Where a signature ends: its last line and the byte in that line where it is cut.
struct Header {
    last: usize,
    cut: usize,
    end: End,
}

fn header(code: &[&str], first: usize, arrow: bool, until_eq: bool, max_lines: usize) -> Header {
    let mut open = 0i32;
    for (line, text) in code.iter().enumerate().skip(first).take(max_lines) {
        let b = text.as_bytes();
        for (k, &c) in b.iter().enumerate() {
            let end = match c {
                b'(' | b'[' => {
                    open += 1;
                    None
                }
                b')' | b']' => {
                    open -= 1;
                    None
                }
                b'{' if open <= 0 => Some((k, End::Brace)),
                b';' if open <= 0 => Some((k, End::Semi)),
                b'=' if open <= 0 => {
                    let after = b.get(k + 1);
                    let before = if k > 0 { b[k - 1] } else { b' ' };
                    if after == Some(&b'>') {
                        arrow.then_some((k + 2, End::Arrow))
                    } else if until_eq && after != Some(&b'=') && !b"=!<>".contains(&before) {
                        Some((k, End::Eq))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some((cut, end)) = end {
                return Header { last: line, cut, end };
            }
        }
    }
    Header {
        last: first,
        cut: code.get(first).map_or(0, |l| l.len()),
        end: End::Open,
    }
}

/// The identifier a string starts with, and what follows it.
fn word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    let end = s.bytes().position(|c| !lex::is_ident_byte(c)).unwrap_or(s.len());
    s.split_at(end)
}

/// What follows the `)` that closes the `(` `s` starts with.
fn after_parens(s: &str) -> &str {
    let mut open = 0;
    for (k, c) in s.char_indices() {
        match c {
            '(' => open += 1,
            ')' => {
                open -= 1;
                if open == 0 {
                    return s.get(k + 1..).unwrap_or("");
                }
            }
            _ => {}
        }
    }
    ""
}

fn rust_item(t: &str) -> Option<Item> {
    let mut rest = t;
    loop {
        let (w, after) = word(rest);
        match w {
            "pub" => {
                rest = after.trim_start();
                if rest.starts_with('(') {
                    rest = after_parens(rest);
                }
            }
            "async" | "unsafe" | "extern" | "default" => rest = after,
            "const" if matches!(word(after).0, "fn" | "unsafe" | "async" | "extern") => rest = after,
            _ => break,
        }
    }
    let (kw, after) = word(rest);
    let (name, tail) = word(after);
    let named = |shape| (!name.is_empty()).then(|| Item::new(shape, name));
    match kw {
        "fn" => named(Shape::Function),
        "struct" | "enum" | "union" => named(Shape::Leaf),
        "type" => named(Shape::Leaf).map(|i| Item { until_eq: true, ..i }),
        "trait" | "mod" => named(Shape::Container),
        "const" | "static" => {
            let (name, _) = if name == "mut" { word(tail) } else { (name, tail) };
            (!name.is_empty() && name != "_").then(|| Item {
                until_eq: true,
                ..Item::new(Shape::Leaf, name)
            })
        }
        "impl" => Some(Item::new(Shape::Group, "")),
        "macro_rules" => {
            let (name, _) = word(after.trim_start().strip_prefix('!')?);
            (!name.is_empty()).then(|| Item::new(Shape::Leaf, name))
        }
        _ => None,
    }
}

fn go_item(t: &str) -> Option<Item> {
    let (kw, after) = word(t);
    match kw {
        "func" => {
            let mut rest = after.trim_start();
            if rest.starts_with('(') {
                rest = after_parens(rest);
            }
            let (name, tail) = word(rest);
            (!name.is_empty() && tail.starts_with(['(', '['])).then(|| Item::new(Shape::Function, name))
        }
        "type" => {
            let (name, _) = word(after);
            (!name.is_empty()).then(|| Item {
                one_line: true,
                ..Item::new(Shape::Leaf, name)
            })
        }
        "const" | "var" => {
            let (name, _) = word(after);
            name.starts_with(|c: char| c.is_uppercase()).then(|| Item {
                one_line: true,
                ..Item::new(Shape::Leaf, name)
            })
        }
        _ => None,
    }
}

/// `const (`, `var (` or `type (`: a block of declarations.
fn go_block_start(t: &str) -> Option<&'static str> {
    let (kw, after) = word(t);
    let kind = match kw {
        "const" => "const",
        "var" => "var",
        "type" => "type",
        _ => return None,
    };
    (after.trim() == "(").then_some(kind)
}

/// After `=`: is the value a function? `Some(true)` when it is one only if it ends in `=>`.
fn js_function_value(value: &str) -> Option<bool> {
    let value = value.trim_start();
    let (w, after) = word(value);
    if w == "function" {
        return Some(false);
    }
    if w == "async" && word(after).0 == "function" {
        return Some(false);
    }
    let arrow = value.starts_with(['(', '<'])
        || w == "async"
        || (!w.is_empty() && after.trim_start().starts_with("=>"));
    arrow.then_some(true)
}

/// `name = value` or `name: Type = value`: the value, when `s` starts after the name.
fn js_value(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    let k =
        (0..b.len()).find(|&k| b[k] == b'=' && b.get(k + 1) != Some(&b'>') && b.get(k + 1) != Some(&b'='))?;
    s.get(k + 1..)
}

fn js_item(t: &str, member: bool) -> Option<Item> {
    let mut rest = t;
    loop {
        let (w, after) = word(rest);
        match w {
            "export" | "default" | "declare" | "abstract" | "async" => rest = after,
            "public" | "private" | "protected" | "static" | "readonly" | "override" if member => rest = after,
            "get" | "set" if member && lex_ident_follows(after) => rest = after,
            _ => break,
        }
    }
    let (kw, after) = word(rest);
    if member {
        return js_member(rest);
    }
    let (name, tail) = word(after);
    match kw {
        "function" => {
            let (name, _) = word(after.trim_start().trim_start_matches('*'));
            (!name.is_empty()).then(|| Item::new(Shape::Function, name))
        }
        "class" | "interface" => (!name.is_empty()).then(|| Item::new(Shape::Container, name)),
        "enum" => (!name.is_empty()).then(|| Item::new(Shape::Leaf, name)),
        "type" => (!name.is_empty() && tail.trim_start().starts_with(['=', '<'])).then(|| Item {
            until_eq: true,
            ..Item::new(Shape::Leaf, name)
        }),
        "const" | "let" | "var" => {
            if kw == "const" && name == "enum" {
                let (name, _) = word(tail);
                return (!name.is_empty()).then(|| Item::new(Shape::Leaf, name));
            }
            if name.is_empty() {
                return None;
            }
            let arrow_only = js_function_value(js_value(tail)?)?;
            Some(Item {
                arrow_only,
                ..Item::new(Shape::Function, name)
            })
        }
        _ => None,
    }
}

fn lex_ident_follows(s: &str) -> bool {
    s.trim_start().bytes().next().is_some_and(lex::is_ident_byte)
}

/// A method or an arrow-function property of a class or an interface.
fn js_member(rest: &str) -> Option<Item> {
    let rest = rest.trim_start().trim_start_matches('*');
    let rest = rest.strip_prefix('#').unwrap_or(rest);
    let (name, after) = word(rest);
    if name.is_empty()
        || matches!(
            name,
            "if" | "for" | "while" | "switch" | "catch" | "return" | "function" | "new" | "super"
        )
    {
        return None;
    }
    let mut after = after.trim_start();
    after = after.strip_prefix(['?', '!']).unwrap_or(after).trim_start();
    if after.starts_with('<') {
        let mut open = 0;
        let end = after.char_indices().find_map(|(k, c)| {
            match c {
                '<' => open += 1,
                '>' => open -= 1,
                _ => {}
            }
            (open == 0).then_some(k + 1)
        })?;
        after = after.get(end..)?.trim_start();
    }
    if after.starts_with('(') {
        return Some(Item {
            needs_end: true,
            ..Item::new(Shape::Function, name)
        });
    }
    let arrow_only = js_function_value(js_value(after)?)?;
    arrow_only.then(|| Item {
        arrow_only,
        ..Item::new(Shape::Function, name)
    })
}

struct Scope {
    /// Brace depth outside the block.
    base: i32,
    /// Line on which the block opens.
    header_end: usize,
    index: u32,
}

struct Scan<'a> {
    lang: Lang,
    code: Vec<&'a str>,
    read: Vec<&'a str>,
    defs: Vec<Def>,
    refs: HashMap<&'a str, u32>,
    scope: Vec<Scope>,
    /// Inside a block that is left out (`mod tests`): its brace depth and the line it opens on.
    skip: Option<(i32, usize)>,
    /// Lines before this one continue a signature already read.
    resume: usize,
    pending_test: bool,
    /// Inside a Go `const (`, `var (` or `type (` block.
    block: Option<&'static str>,
}

impl<'a> Scan<'a> {
    fn count(&mut self, line: &'a str) {
        identifiers(line, |name| *self.refs.entry(name).or_default() += 1);
    }

    fn javascript(&self) -> bool {
        matches!(self.lang, Lang::JavaScript | Lang::TypeScript | Lang::Tsx)
    }

    fn braces(&mut self) {
        let mut depth = 0i32;
        for i in 0..self.code.len() {
            let line = self.code[i];
            if let Some((base, from)) = self.skip
                && i > from
                && depth <= base
            {
                self.skip = None;
            }
            if self.skip.is_none() {
                while self
                    .scope
                    .last()
                    .is_some_and(|s| i > s.header_end && depth <= s.base)
                {
                    self.scope.pop();
                }
                if i >= self.resume {
                    self.detect(i, depth);
                }
                // Counted after the detection: the line that opens a left-out block is not a use.
                if self.skip.is_none() {
                    self.count(line);
                }
            }
            for c in line.bytes() {
                match c {
                    b'{' => depth += 1,
                    b'}' => depth = (depth - 1).max(0),
                    _ => {}
                }
            }
        }
    }

    fn detect(&mut self, i: usize, depth: i32) {
        let t = self.code[i].trim();
        if t.is_empty() {
            return;
        }
        if self.lang == Lang::Rust && t.starts_with("#[") {
            self.pending_test |= t.contains("cfg(test)");
            return;
        }
        let test = std::mem::take(&mut self.pending_test);
        // Definitions are only looked for directly inside the block they belong to: lines of a
        // function body, a struct or an object literal are one level deeper.
        if depth != self.scope.last().map_or(0, |s| s.base + 1) {
            return;
        }
        if let Some(kind) = self.block {
            if t.starts_with(')') {
                self.block = None;
            } else {
                self.go_entry(kind, i, t);
            }
            return;
        }
        if self.lang == Lang::Go
            && let Some(kind) = go_block_start(t)
        {
            self.block = Some(kind);
            return;
        }
        let item = match self.lang {
            Lang::Rust => rust_item(t),
            Lang::Go => go_item(t),
            Lang::Python => None,
            _ => js_item(t, !self.scope.is_empty()),
        };
        let Some(item) = item else {
            return;
        };
        let max = if item.one_line { 1 } else { MAX_HEADER_LINES };
        let h = header(&self.code, i, self.javascript(), item.until_eq, max);
        if (item.arrow_only && h.end != End::Arrow)
            || (item.needs_end && !matches!(h.end, End::Brace | End::Semi))
        {
            return;
        }
        let in_tests = self.lang == Lang::Rust && item.shape == Shape::Container && item.name == "tests";
        if test || in_tests {
            if h.end == End::Brace {
                self.skip = Some((depth, h.last));
            }
            self.resume = h.last + 1;
            return;
        }
        let index = self.defs.len() as u32;
        let def = self.define(&item, i, &h);
        self.defs.push(def);
        self.resume = h.last + 1;
        if matches!(item.shape, Shape::Container | Shape::Group) && h.end == End::Brace {
            self.scope.push(Scope {
                base: depth,
                header_end: h.last,
                index,
            });
        }
    }

    fn go_entry(&mut self, kind: &str, i: usize, t: &str) {
        let (name, _) = word(t);
        if name.is_empty() || name == "_" || (kind != "type" && !name.starts_with(|c: char| c.is_uppercase()))
        {
            return;
        }
        let line = self.read[i].trim().trim_end_matches('{').trim_end();
        let mut sig = format!("{kind} {line}");
        truncate(&mut sig);
        self.defs.push(Def {
            name: name.to_string(),
            line: i as u32 + 1,
            rows: 1,
            parent: None,
            depth: 0,
            sig,
        });
    }

    fn define(&self, item: &Item, first: usize, h: &Header) -> Def {
        Def {
            name: item.name.clone(),
            line: first as u32 + 1,
            rows: (h.last - first + 1) as u32,
            parent: self.scope.last().map(|s| s.index),
            depth: self.scope.len() as u32,
            sig: signature(&self.read, first, h.last, h.cut),
        }
    }

    fn python(&mut self) {
        // Open classes with the indentation of their `class` line.
        let mut classes: Vec<(usize, u32)> = Vec::new();
        let mut function: Option<usize> = None;
        let mut open = 0i32;
        for i in 0..self.code.len() {
            let line = self.code[i];
            self.count(line);
            let t = line.trim_start();
            let continued = open > 0;
            for c in line.bytes() {
                match c {
                    b'(' | b'[' | b'{' => open += 1,
                    b')' | b']' | b'}' => open -= 1,
                    _ => {}
                }
            }
            open = open.max(0);
            if t.is_empty() || continued {
                continue;
            }
            let indent = line.len() - t.len();
            if function.is_some_and(|f| indent <= f) {
                function = None;
            }
            while classes.last().is_some_and(|c| c.0 >= indent) {
                classes.pop();
            }
            if function.is_some() {
                continue;
            }
            let (w, after) = word(t);
            let (kw, after) = if w == "async" { word(after) } else { (w, after) };
            let shape = match kw {
                "def" => Shape::Function,
                "class" => Shape::Container,
                _ => continue,
            };
            let (name, _) = word(after);
            if name.is_empty() {
                continue;
            }
            let h = python_header(&self.code, i);
            let index = self.defs.len() as u32;
            self.defs.push(Def {
                name: name.to_string(),
                line: i as u32 + 1,
                rows: (h.last - i + 1) as u32,
                parent: classes.last().map(|c| c.1),
                depth: classes.len() as u32,
                sig: signature(&self.read, i, h.last, h.cut),
            });
            if shape == Shape::Function {
                function = Some(indent);
            } else {
                classes.push((indent, index));
            }
        }
    }
}

/// The signature of a Python definition ends at the first `:` outside brackets.
fn python_header(code: &[&str], first: usize) -> Header {
    let mut open = 0i32;
    for (line, text) in code.iter().enumerate().skip(first).take(MAX_HEADER_LINES) {
        for (k, c) in text.bytes().enumerate() {
            match c {
                b'(' | b'[' | b'{' => open += 1,
                b')' | b']' | b'}' => open -= 1,
                b':' if open <= 0 => {
                    return Header {
                        last: line,
                        cut: k + 1,
                        end: End::Brace,
                    };
                }
                _ => {}
            }
        }
    }
    Header {
        last: first,
        cut: code.get(first).map_or(0, |l| l.len()),
        end: End::Open,
    }
}

fn truncate(sig: &mut String) {
    if sig.chars().count() > MAX_SIGNATURE {
        *sig = sig.chars().take(MAX_SIGNATURE - 1).collect::<String>() + "…";
    }
}

/// The lines `first..=last` of the source on one line, cut at byte `cut` of the last.
fn signature(read: &[&str], first: usize, last: usize, cut: usize) -> String {
    let mut text = String::new();
    for (line, r) in read.iter().enumerate().take(last + 1).skip(first) {
        let r = if line == last {
            r.get(..cut).unwrap_or(r)
        } else {
            r
        };
        text.push_str(r);
        text.push(' ');
    }
    let mut sig = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&mut sig);
    sig
}

/// The definitions of `source` and the identifiers it uses.
pub fn extract(lang: Lang, source: &str) -> Tags {
    let (code, read) = blank(lang, source);
    let mut scan = Scan {
        lang,
        code: code.lines().collect(),
        read: read.lines().collect(),
        defs: Vec::new(),
        refs: HashMap::new(),
        scope: Vec::new(),
        skip: None,
        resume: 0,
        pending_test: false,
        block: None,
    };
    if lang == Lang::Python {
        scan.python();
    } else {
        scan.braces();
    }
    let mut refs: Vec<(String, u32)> = scan.refs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    refs.sort();
    Tags {
        defs: scan.defs,
        refs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sigs(tags: &Tags) -> Vec<(&str, u32, &str)> {
        tags.defs
            .iter()
            .map(|d| (d.name.as_str(), d.depth, d.sig.as_str()))
            .collect()
    }

    fn uses(tags: &Tags, name: &str) -> u32 {
        tags.refs.iter().find(|(n, _)| n == name).map_or(0, |(_, c)| *c)
    }

    #[test]
    fn rust_braces_in_strings_and_comments_do_not_move_the_nesting() {
        let tags = extract(
            Lang::Rust,
            r##"
/// A doc comment with a { brace.
pub struct Config {
    brace: char,
}

impl Config {
    pub fn open() -> Config {
        let s = "}}} not a close";
        let c = '}';
        let r = r#"{ raw "# ;
        /* { nested /* { */ } */
        Config { brace: c }
    }

    pub(crate) async fn load(&self) {}
}

fn after_all() {}
"##,
        );
        assert_eq!(
            sigs(&tags),
            [
                ("Config", 0, "pub struct Config"),
                ("", 0, "impl Config"),
                ("open", 1, "pub fn open() -> Config"),
                ("load", 1, "pub(crate) async fn load(&self)"),
                ("after_all", 0, "fn after_all()"),
            ]
        );
        assert_eq!(uses(&tags, "not"), 0);
        assert_eq!(uses(&tags, "nested"), 0);
    }

    #[test]
    fn rust_signatures_span_lines_and_cut_at_the_body() {
        let tags = extract(
            Lang::Rust,
            "pub fn build<T>(\n    a: T,\n    b: [u8; 4],\n) -> Result<(), Error>\nwhere\n    T: Clone,\n{\n    todo()\n}\n",
        );
        assert_eq!(tags.defs.len(), 1);
        assert_eq!(
            tags.defs[0].sig,
            "pub fn build<T>( a: T, b: [u8; 4], ) -> Result<(), Error> where T: Clone,"
        );
        assert_eq!(tags.defs[0].rows, 7);
    }

    #[test]
    fn rust_items_of_every_kind_and_traits_with_methods() {
        let tags = extract(
            Lang::Rust,
            "pub trait Store {\n    type Item;\n    fn get(&self) -> u8;\n}\nimpl<T> Store for Vec<T> {\n    type Item = T;\n    fn get(&self) -> u8 { 0 }\n}\npub const LIMIT: usize = 4;\nstatic mut COUNT: u32 = 0;\nunion U { a: u8 }\nmacro_rules! twice { ($e:expr) => { $e * 2 } }\nextern \"C\" fn callback() {}\nunsafe impl Send for Foo {}\npub type Alias = Vec<u8>;\n",
        );
        let names: Vec<_> = tags.defs.iter().map(|d| (d.name.as_str(), d.depth)).collect();
        assert_eq!(
            names,
            [
                ("Store", 0),
                ("Item", 1),
                ("get", 1),
                ("", 0),
                ("Item", 1),
                ("get", 1),
                ("LIMIT", 0),
                ("COUNT", 0),
                ("U", 0),
                ("twice", 0),
                ("callback", 0),
                ("", 0),
                ("Alias", 0),
            ]
        );
        assert_eq!(tags.defs[3].sig, "impl<T> Store for Vec<T>");
        assert_eq!(tags.defs[6].sig, "pub const LIMIT: usize");
        assert_eq!(tags.defs[10].sig, "extern \"C\" fn callback()");
    }

    #[test]
    fn rust_test_blocks_are_left_out_wherever_they_are() {
        let tags = extract(
            Lang::Rust,
            "fn real() {}\n#[cfg(test)]\nfn helper_for_tests() { only_here(); }\n#[cfg(test)]\nmod checks {\n    fn inside() {}\n}\nmod tests {\n    fn t() { in_tests(); }\n}\nfn last() {}\n",
        );
        let names: Vec<_> = tags.defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["real", "last"]);
        assert_eq!(uses(&tags, "only_here") + uses(&tags, "in_tests"), 0);
    }

    #[test]
    fn python_methods_nest_by_indentation_and_decorators_are_not_part_of_the_signature() {
        let tags = extract(
            Lang::Python,
            "import os\n\n\nclass Store(Base):\n    \"\"\"def fake(): pass\"\"\"\n\n    @property\n    @cached(ttl=3)\n    def size(self) -> int:\n        def inner():\n            return 1\n        return inner()\n\n    async def fetch(\n        self,\n        key: str = \"a: b\",\n    ) -> dict[str, int]:\n        x = {1: 2}\n\n    class Meta:\n        pass\n\n\ndef top(a, b=lambda: 1): return a\n",
        );
        assert_eq!(
            sigs(&tags),
            [
                ("Store", 0, "class Store(Base):"),
                ("size", 1, "def size(self) -> int:"),
                (
                    "fetch",
                    1,
                    "async def fetch( self, key: str = \"a: b\", ) -> dict[str, int]:"
                ),
                ("Meta", 1, "class Meta:"),
                ("top", 0, "def top(a, b=lambda: 1):"),
            ]
        );
        assert_eq!(tags.defs[2].rows, 4);
        assert_eq!(tags.defs[1].parent, Some(0));
        assert_eq!(uses(&tags, "fake"), 0);
    }

    #[test]
    fn go_receivers_types_and_declaration_blocks() {
        let tags = extract(
            Lang::Go,
            "package a\n\nconst Version = \"1\"\nconst hidden = 2\n\nconst (\n\tModeA Mode = iota\n\tmodeB\n)\n\nvar (\n\tErrNone = errors.New(\"}\")\n)\n\ntype Server struct {\n\tport int\n}\n\ntype Handler func(int) error\n\ntype Doer interface {\n\tDo()\n}\n\nfunc (s *Server) Start(addr string) error {\n\tx := map[string]int{\"{\": 1}\n\treturn nil\n}\n\nfunc New[T any](v T) *Server { return nil }\n",
        );
        assert_eq!(
            sigs(&tags),
            [
                ("Version", 0, "const Version = \"1\""),
                ("ModeA", 0, "const ModeA Mode = iota"),
                ("ErrNone", 0, "var ErrNone = errors.New(\"}\")"),
                ("Server", 0, "type Server struct"),
                ("Handler", 0, "type Handler func(int) error"),
                ("Doer", 0, "type Doer interface"),
                ("Start", 0, "func (s *Server) Start(addr string) error"),
                ("New", 0, "func New[T any](v T) *Server"),
            ]
        );
    }

    #[test]
    fn typescript_exports_classes_members_and_types() {
        let tags = extract(
            Lang::TypeScript,
            "export const load = async (id: string): Promise<Item> => {\n  return fetchItem(`${id}}`);\n};\nexport const answer = 42;\nconst total = (a + b) * 2;\nexport default function main() {}\nexport abstract class Repo<T> extends Base {\n  private cache = new Map();\n  static async create(opts: Opts): Promise<Repo<T>> { return x; }\n  get size(): number { return 1; }\n  handler = (e: Event) => { run(e); };\n  find(\n    id: string,\n  ): T | undefined { if (id) { return a; } }\n  abstract remove(id: string): void;\n}\ninterface Item { id: string; render(): string }\nexport type Pair<A> = [A, A];\nexport const enum Mode { A, B }\nconst re = /[{}]\\/+/g;
const pick = x => x.id;\n",
        );
        assert_eq!(
            sigs(&tags),
            [
                (
                    "load",
                    0,
                    "export const load = async (id: string): Promise<Item> =>"
                ),
                ("main", 0, "export default function main()"),
                ("Repo", 0, "export abstract class Repo<T> extends Base"),
                ("create", 1, "static async create(opts: Opts): Promise<Repo<T>>"),
                ("size", 1, "get size(): number"),
                ("handler", 1, "handler = (e: Event) =>"),
                ("find", 1, "find( id: string, ): T | undefined"),
                ("remove", 1, "abstract remove(id: string): void"),
                ("Item", 0, "interface Item"),
                ("Pair", 0, "export type Pair<A>"),
                ("Mode", 0, "export const enum Mode"),
                ("pick", 0, "const pick = x =>"),
            ]
        );
    }

    #[test]
    fn javascript_functions_and_classes() {
        let tags = extract(
            Lang::JavaScript,
            "function* gen() { yield '{'; }\nclass A {\n  constructor(x) { this.x = x; }\n  run() { if (this.x) { go(); } }\n}\nmodule.exports = { A };\n",
        );
        assert_eq!(
            sigs(&tags),
            [
                ("gen", 0, "function* gen()"),
                ("A", 0, "class A"),
                ("constructor", 1, "constructor(x)"),
                ("run", 1, "run()"),
            ]
        );
    }

    #[test]
    fn identifiers_in_strings_comments_and_docstrings_are_not_uses() {
        let tags = extract(
            Lang::Python,
            "# commented_out()\ndef f():\n    \"\"\"docstring_word\"\"\"\n    s = 'in_string'\n    return real_call(s)\n",
        );
        assert_eq!(uses(&tags, "real_call"), 1);
        assert_eq!(
            uses(&tags, "commented_out") + uses(&tags, "docstring_word") + uses(&tags, "in_string"),
            0
        );
    }

    #[test]
    fn an_unclosed_quote_does_not_swallow_the_rest_of_the_file() {
        let tags = extract(Lang::JavaScript, "const s = 'oops;\nfunction after() {}\n");
        assert_eq!(tags.defs.len(), 1);
        assert_eq!(tags.defs[0].name, "after");
    }
}
