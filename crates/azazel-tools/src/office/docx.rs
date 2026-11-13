//! A Word document as Markdown: headings, paragraphs with their bold and italic runs and
//! links, lists, tables and pictures, in the order of the document. Headers, footers, footnotes
//! and comments are not part of it.

use std::collections::HashMap;

use quick_xml::events::{BytesStart, Event};

use super::ooxml::{Package, Pictures, Relations, is_rooted_in, related};
use super::{Converted, markdown_table};
use crate::xml;

/// What a paragraph style makes of a paragraph.
#[derive(Default, Clone)]
struct Style {
    /// The style it is based on, which says what this one does not.
    parent: Option<String>,
    /// The level of a heading style, from 1; the title of the document is level 0.
    heading: Option<u8>,
    /// The list and the level in it that paragraphs of the style belong to.
    list: Option<(String, usize)>,
}

/// The paragraph styles of the document by id. The id is whatever the program that wrote the
/// file chose; the name is what says that a style is a heading.
fn styles(xml_text: &str) -> HashMap<String, Style> {
    let mut styles = HashMap::new();
    let mut reader = xml::reader(xml_text);
    let mut open: Option<(String, Style)> = None;
    let mut level = 0;
    while let Some(event) = xml::next(&mut reader) {
        match &event {
            Event::Start(start) => match start.name().0 {
                b"w:style" => {
                    let id = xml::attr(start, "w:styleId").unwrap_or_default().into_owned();
                    open = Some((id, Style::default()));
                    level = 0;
                }
                name => {
                    let Some((_, style)) = open.as_mut() else { continue };
                    let value = xml::attr(start, "w:val");
                    match (name, value) {
                        (b"w:basedOn", Some(value)) => style.parent = Some(value.into_owned()),
                        (b"w:name", Some(value)) => {
                            let name = value.to_ascii_lowercase();
                            if name == "title" {
                                style.heading = Some(0);
                            } else if let Some(number) = name.strip_prefix("heading ") {
                                style.heading = number.trim().parse().ok();
                            }
                        }
                        (b"w:outlineLvl", Some(value)) if style.heading.is_none() => {
                            style.heading = value
                                .parse::<u8>()
                                .ok()
                                .filter(|level| *level < 9)
                                .map(|level| level + 1);
                        }
                        (b"w:ilvl", Some(value)) => {
                            level = value.parse().unwrap_or(0);
                            if let Some((_, of_list)) = style.list.as_mut() {
                                *of_list = level;
                            }
                        }
                        (b"w:numId", Some(value)) => style.list = Some((value.into_owned(), level)),
                        _ => {}
                    }
                }
            },
            Event::End(end) if end.name().0 == b"w:style" => {
                if let Some((id, style)) = open.take() {
                    styles.insert(id, style);
                }
            }
            _ => {}
        }
    }
    styles
}

/// Which levels of which lists are numbered; the others are bulleted.
#[derive(Default)]
struct Numbering {
    /// The definition each list uses.
    lists: HashMap<String, String>,
    /// The numbered levels of each definition.
    numbered: HashMap<String, Vec<usize>>,
}

impl Numbering {
    fn parse(xml_text: &str) -> Numbering {
        let mut numbering = Numbering::default();
        let mut reader = xml::reader(xml_text);
        let mut definition = None;
        let mut list = None;
        let mut level = 0;
        while let Some(event) = xml::next(&mut reader) {
            match &event {
                Event::Start(start) => {
                    let attr = |name: &str| xml::attr(start, name).map(|value| value.into_owned());
                    match start.name().0 {
                        b"w:abstractNum" => definition = attr("w:abstractNumId"),
                        b"w:num" => list = attr("w:numId"),
                        b"w:lvl" => level = attr("w:ilvl").and_then(|level| level.parse().ok()).unwrap_or(0),
                        b"w:numFmt" => {
                            let numbered =
                                attr("w:val").is_some_and(|format| format != "bullet" && format != "none");
                            if let (true, Some(definition)) = (numbered, &definition) {
                                numbering
                                    .numbered
                                    .entry(definition.clone())
                                    .or_default()
                                    .push(level);
                            }
                        }
                        b"w:abstractNumId" => {
                            if let (Some(list), Some(definition)) = (&list, attr("w:val")) {
                                numbering.lists.insert(list.clone(), definition);
                            }
                        }
                        _ => {}
                    }
                }
                Event::End(end) => match end.name().0 {
                    b"w:abstractNum" => definition = None,
                    b"w:num" => list = None,
                    _ => {}
                },
                _ => {}
            }
        }
        numbering
    }

    fn is_numbered(&self, list: &str, level: usize) -> bool {
        let levels = self
            .lists
            .get(list)
            .and_then(|definition| self.numbered.get(definition));
        levels.is_some_and(|levels| levels.contains(&level))
    }
}

/// A stretch of a paragraph in one style.
struct Span {
    text: String,
    bold: bool,
    italic: bool,
    /// Markdown already: a link or a picture, which is not to be wrapped in emphasis.
    made: bool,
}

/// The spans as Markdown; `plain` leaves bold and italic out.
fn inline(spans: &[Span], plain: bool) -> String {
    let mut out = String::new();
    let mut at = 0;
    while at < spans.len() {
        let first = &spans[at];
        // Runs in one style are one stretch, whatever Word split them for.
        let mut text = first.text.clone();
        let mut next = at + 1;
        while let Some(span) = spans.get(next)
            && (span.bold, span.italic, span.made) == (first.bold, first.italic, first.made)
        {
            text.push_str(&span.text);
            next += 1;
        }
        at = next;
        let marks = match (first.bold, first.italic) {
            _ if plain || first.made => "",
            (true, true) => "***",
            (true, false) => "**",
            (false, true) => "*",
            (false, false) => "",
        };
        let inner = text.trim();
        if marks.is_empty() || inner.is_empty() {
            out.push_str(&text);
            continue;
        }
        // The marks hug the words: the spaces around them stay outside.
        let lead = text.len() - text.trim_start().len();
        let trail = text.trim_end().len();
        out.push_str(&text[..lead]);
        out.push_str(marks);
        out.push_str(inner);
        out.push_str(marks);
        out.push_str(&text[trail..]);
    }
    out
}

#[derive(Default)]
struct Paragraph {
    style: Option<String>,
    outline: Option<u8>,
    list: Option<String>,
    level: Option<usize>,
    spans: Vec<Span>,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    /// The paragraphs of the cell being read.
    cell: Option<Vec<String>>,
}

/// A piece of the document as Markdown.
enum Block {
    /// The title of the document.
    Title(String),
    /// A heading and its level, from 1.
    Heading(u8, String),
    Item(String),
    Text(String),
}

/// Reads the body of a document into blocks.
struct Walker<'a> {
    relations: &'a Relations,
    styles: &'a HashMap<String, Style>,
    numbering: &'a Numbering,
    pictures: Pictures,
    blocks: Vec<Block>,
    /// The paragraphs and tables that are open, the innermost last: a text box is paragraphs
    /// in a paragraph, and a cell may hold a table.
    paragraphs: Vec<Paragraph>,
    tables: Vec<Table>,
    /// Whether a run is open, and whether it is bold and italic.
    run: Option<(bool, bool)>,
    in_paragraph_properties: bool,
    in_run_properties: bool,
    in_text: bool,
    /// Where the link being read starts among the spans of its paragraph, and where it leads.
    links: Vec<(usize, Option<String>)>,
    /// What the picture being read is said to show.
    describes: Option<String>,
    /// How far into content that is given a second time, for programs that cannot show the
    /// first, the reader is.
    skipping: usize,
    /// How many items each numbered list has had at each level.
    counters: HashMap<String, Vec<usize>>,
}

impl Walker<'_> {
    fn on(value: Option<std::borrow::Cow<'_, str>>) -> bool {
        !value.is_some_and(|value| matches!(&*value, "0" | "false" | "off"))
    }

    fn push(&mut self, text: &str, made: bool) {
        let (bold, italic) = self.run.unwrap_or_default();
        if let Some(paragraph) = self.paragraphs.last_mut() {
            paragraph.spans.push(Span {
                text: text.to_string(),
                bold,
                italic,
                made,
            });
        }
    }

    fn start(&mut self, start: &BytesStart<'_>) {
        let value = || xml::attr(start, "w:val");
        match start.name().0 {
            b"mc:Fallback" => self.skipping = 1,
            b"w:p" => self.paragraphs.push(Paragraph::default()),
            b"w:pPr" => self.in_paragraph_properties = true,
            b"w:r" => self.run = Some((false, false)),
            b"w:rPr" if self.run.is_some() => self.in_run_properties = true,
            b"w:t" if self.run.is_some() => self.in_text = true,
            b"w:tbl" => self.tables.push(Table::default()),
            b"w:tr" => {
                if let Some(table) = self.tables.last_mut() {
                    table.rows.push(Vec::new());
                }
            }
            b"w:tc" => {
                if let Some(table) = self.tables.last_mut() {
                    table.cell = Some(Vec::new());
                }
            }
            b"w:hyperlink" => {
                let target = xml::attr(start, "r:id")
                    .and_then(|id| self.relations.get(&*id))
                    .filter(|relation| relation.external)
                    .map(|relation| relation.target.clone());
                let from = self
                    .paragraphs
                    .last()
                    .map_or(0, |paragraph| paragraph.spans.len());
                self.links.push((from, target));
            }
            b"wp:docPr" => {
                self.describes = xml::attr(start, "descr")
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty());
            }
            name @ (b"a:blip" | b"v:imagedata") => {
                let id = xml::attr(start, if name == b"a:blip" { "r:embed" } else { "r:id" });
                let part = id
                    .and_then(|id| self.relations.get(&*id))
                    .filter(|relation| !relation.external);
                if let Some(file) = part.and_then(|part| self.pictures.file(&part.target)) {
                    let shows = self.describes.take().unwrap_or_default();
                    self.push(&format!("![{shows}]({file})"), true);
                }
            }
            name if self.in_paragraph_properties => {
                let Some(paragraph) = self.paragraphs.last_mut() else {
                    return;
                };
                match name {
                    b"w:pStyle" => paragraph.style = value().map(|style| style.into_owned()),
                    b"w:outlineLvl" => paragraph.outline = value().and_then(|level| level.parse().ok()),
                    b"w:numId" => paragraph.list = value().map(|list| list.into_owned()),
                    b"w:ilvl" => paragraph.level = value().and_then(|level| level.parse().ok()),
                    _ => {}
                }
            }
            name if self.in_run_properties => {
                let Some((bold, italic)) = self.run.as_mut() else {
                    return;
                };
                match name {
                    b"w:b" => *bold = Walker::on(value()),
                    b"w:i" => *italic = Walker::on(value()),
                    _ => {}
                }
            }
            b"w:tab" if self.run.is_some() => self.push(" ", false),
            b"w:br" | b"w:cr" if self.run.is_some() => self.push("\n", false),
            b"w:noBreakHyphen" if self.run.is_some() => self.push("-", false),
            _ => {}
        }
    }

    fn end(&mut self, name: &[u8]) {
        match name {
            b"w:pPr" => self.in_paragraph_properties = false,
            b"w:rPr" => self.in_run_properties = false,
            b"w:t" => self.in_text = false,
            b"w:r" => self.run = None,
            b"w:hyperlink" => self.end_link(),
            b"w:p" => {
                if let Some(paragraph) = self.paragraphs.pop()
                    && let Some(block) = self.block(paragraph)
                {
                    self.emit(block);
                }
            }
            b"w:tc" => {
                if let Some(table) = self.tables.last_mut()
                    && let Some(cell) = table.cell.take()
                    && let Some(row) = table.rows.last_mut()
                {
                    row.push(cell.join("\n"));
                }
            }
            b"w:tbl" => {
                let Some(table) = self.tables.pop() else { return };
                let block = match self.tables.is_empty() {
                    true => markdown_table(&table.rows),
                    // A table in a cell: its rows, one to a line.
                    false => table
                        .rows
                        .iter()
                        .map(|row| row.join(" · "))
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                if !block.is_empty() {
                    self.emit(Block::Text(block));
                }
            }
            _ => {}
        }
    }

    /// Makes the spans since the link started one span of Markdown.
    fn end_link(&mut self) {
        let Some((from, target)) = self.links.pop() else {
            return;
        };
        let (Some(target), Some(paragraph)) = (target, self.paragraphs.last_mut()) else {
            return;
        };
        if from >= paragraph.spans.len() {
            return;
        }
        let label = inline(&paragraph.spans[from..], false);
        paragraph.spans.truncate(from);
        paragraph.spans.push(Span {
            text: format!("[{}]({target})", label.trim()),
            bold: false,
            italic: false,
            made: true,
        });
    }

    /// What `style` and the styles it is based on say with `get`.
    fn inherited<T>(&self, style: Option<&str>, get: impl Fn(&Style) -> Option<T>) -> Option<T> {
        let mut style = self.styles.get(style?);
        for _ in 0..16 {
            let found = style?;
            if let Some(value) = get(found) {
                return Some(value);
            }
            style = self.styles.get(found.parent.as_deref()?);
        }
        None
    }

    fn block(&mut self, paragraph: Paragraph) -> Option<Block> {
        let style = paragraph.style.as_deref();
        let heading = match paragraph.outline {
            Some(level) if level < 9 => Some(level + 1),
            Some(_) => None,
            None => self
                .inherited(style, |style| style.heading)
                // A file without its styles: the ids Word gives the built-in ones.
                .or_else(|| match style? {
                    "Title" => Some(0),
                    id => id.strip_prefix("Heading")?.parse().ok(),
                }),
        };
        if let Some(level) = heading {
            let text = inline(&paragraph.spans, true);
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            return match (text.is_empty(), level) {
                (true, _) => None,
                (false, 0) => Some(Block::Title(text)),
                (false, level) => Some(Block::Heading(level, text)),
            };
        }
        let text = inline(&paragraph.spans, false);
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let of_style = self.inherited(style, |style| style.list.clone());
        let list = match (paragraph.list, paragraph.level, of_style) {
            (Some(list), level, of_style) => {
                Some((list, level.or(of_style.map(|(_, level)| level)).unwrap_or(0)))
            }
            (None, level, Some((list, of_style))) => Some((list, level.unwrap_or(of_style))),
            (None, _, None) => None,
        };
        // List 0 is how a paragraph says it is in no list after all.
        let Some((list, level)) = list.filter(|(list, _)| list != "0") else {
            return Some(Block::Text(text.to_string()));
        };
        let mark = match self.numbering.is_numbered(&list, level) {
            true => {
                let counters = self.counters.entry(list).or_default();
                counters.resize(level + 1, 0);
                counters[level] += 1;
                format!("{}.", counters[level])
            }
            false => "-".to_string(),
        };
        let pad = "    ".repeat(level);
        let text = text.replace('\n', &format!("\n{pad}  "));
        Some(Block::Item(format!("{pad}{mark} {text}")))
    }

    /// Adds a block to the cell being read, or to the document.
    fn emit(&mut self, block: Block) {
        match self.tables.last_mut().and_then(|table| table.cell.as_mut()) {
            Some(cell) => cell.push(match block {
                Block::Title(text) | Block::Heading(_, text) | Block::Text(text) => text,
                Block::Item(text) => text.trim_start().to_string(),
            }),
            None => self.blocks.push(block),
        }
    }
}

/// The blocks as one Markdown text. A document with a title has its headings one level below
/// it; the items of a list follow each other without a blank line.
fn markdown(blocks: &[Block]) -> String {
    let below_title = u8::from(blocks.iter().any(|block| matches!(block, Block::Title(_))));
    let mut out = String::new();
    let mut after_item = false;
    for block in blocks {
        let is_item = matches!(block, Block::Item(_));
        if !out.is_empty() {
            out.push_str(if is_item && after_item { "\n" } else { "\n\n" });
        }
        after_item = is_item;
        match block {
            Block::Title(text) => out.push_str(&format!("# {text}")),
            Block::Heading(level, text) => {
                let level = usize::from((level + below_title).min(6));
                out.push_str(&format!("{} {text}", "#".repeat(level)));
            }
            Block::Item(text) | Block::Text(text) => out.push_str(text),
        }
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

pub(super) fn convert(bytes: &[u8]) -> Result<Converted, String> {
    let mut package = Package::open(bytes)?;
    let main = package.main().unwrap_or_else(|| "word/document.xml".to_string());
    let document = package
        .text(&main)
        .filter(|text| is_rooted_in(text, "document"))
        .ok_or("it holds no document: it is not a Word file")?;
    let relations = package.relations(&main);
    let part = |package: &mut Package<'_>, kind: &str| {
        let name = related(&relations, kind)?;
        package.text(name)
    };
    let styles = part(&mut package, "/styles")
        .map(|text| styles(&text))
        .unwrap_or_default();
    let numbering = part(&mut package, "/numbering")
        .map(|text| Numbering::parse(&text))
        .unwrap_or_default();
    let mut walker = Walker {
        relations: &relations,
        styles: &styles,
        numbering: &numbering,
        pictures: Pictures::default(),
        blocks: Vec::new(),
        paragraphs: Vec::new(),
        tables: Vec::new(),
        run: None,
        in_paragraph_properties: false,
        in_run_properties: false,
        in_text: false,
        links: Vec::new(),
        describes: None,
        skipping: 0,
        counters: HashMap::new(),
    };
    let mut reader = xml::reader(&document);
    let mut text = String::new();
    while let Some(event) = xml::next(&mut reader) {
        match &event {
            Event::Start(_) if walker.skipping > 0 => walker.skipping += 1,
            Event::End(_) if walker.skipping > 0 => walker.skipping -= 1,
            Event::Start(start) => walker.start(start),
            Event::End(end) => {
                if walker.in_text && end.name().0 == b"w:t" {
                    walker.push(&text, false);
                    text.clear();
                }
                walker.end(end.name().0);
            }
            other if walker.in_text && walker.skipping == 0 => xml::push_text(other, &mut text),
            _ => {}
        }
    }
    Ok(Converted {
        markdown: markdown(&walker.blocks),
        files: walker.pictures.into_files(&mut package),
    })
}

#[cfg(test)]
mod tests {
    use super::super::ooxml::tests::archive;
    use super::*;

    fn span(text: &str, bold: bool, italic: bool) -> Span {
        Span {
            text: text.into(),
            bold,
            italic,
            made: false,
        }
    }

    #[test]
    fn runs_in_one_style_are_one_stretch_and_the_marks_hug_the_words() {
        let spans = [
            span("Revenue ", false, false),
            span("grew ", true, false),
            span("a lot", true, false),
            span(" and ", false, true),
            span("fast", true, true),
            span("  ", true, false),
            span(".", false, false),
        ];
        assert_eq!(
            inline(&spans, false),
            "Revenue **grew a lot** *and* ***fast***  ."
        );
        assert_eq!(inline(&spans, true), "Revenue grew a lot and fast  .");
    }

    const RELS: &str = r#"<Relationships><Relationship Id="rId1" Type="http://x/officeDocument" Target="word/document.xml"/></Relationships>"#;

    fn document(body: &str, more: &[(&str, &str)]) -> String {
        let document = format!("<w:document><w:body>{body}</w:body></w:document>");
        let mut parts = vec![("_rels/.rels", RELS), ("word/document.xml", document.as_str())];
        parts.extend_from_slice(more);
        convert(&archive(&parts)).unwrap().markdown
    }

    #[test]
    fn headings_come_from_the_names_of_styles_not_from_their_ids() {
        // A German Word: the ids are its own, the names are the ones every Word uses.
        let styles = r#"<w:styles>
            <w:style w:type="paragraph" w:styleId="berschrift1"><w:name w:val="heading 1"/></w:style>
            <w:style w:type="paragraph" w:styleId="Kapitel"><w:name w:val="Kapitel"/><w:basedOn w:val="berschrift1"/></w:style>
            <w:style w:type="paragraph" w:styleId="Gliederung"><w:name w:val="Gliederung"/><w:pPr><w:outlineLvl w:val="2"/></w:pPr></w:style>
            </w:styles>"#;
        let rels = r#"<Relationships><Relationship Id="rId1" Type="http://x/styles" Target="styles.xml"/></Relationships>"#;
        let body = r#"
            <w:p><w:pPr><w:pStyle w:val="berschrift1"/></w:pPr><w:r><w:rPr><w:b/></w:rPr><w:t>Eins</w:t></w:r></w:p>
            <w:p><w:pPr><w:pStyle w:val="Kapitel"/></w:pPr><w:r><w:t xml:space="preserve">Zwei </w:t></w:r><w:r><w:t>Teile</w:t></w:r></w:p>
            <w:p><w:pPr><w:pStyle w:val="Gliederung"/></w:pPr><w:r><w:t>Drei</w:t></w:r></w:p>
            <w:p><w:pPr><w:outlineLvl w:val="1"/></w:pPr><w:r><w:t>Vier</w:t></w:r></w:p>
            <w:p><w:pPr><w:pStyle w:val="Unbekannt"/></w:pPr><w:r><w:t>Text</w:t></w:r></w:p>
            <w:p><w:pPr><w:pStyle w:val="berschrift1"/></w:pPr></w:p>"#;
        assert_eq!(
            document(
                body,
                &[
                    ("word/styles.xml", styles),
                    ("word/_rels/document.xml.rels", rels)
                ]
            ),
            "# Eins\n\n# Zwei Teile\n\n### Drei\n\n## Vier\n\nText\n"
        );
        // Without the styles, the ids Word gives its own headings still count.
        let bare = r#"<w:p><w:pPr><w:pStyle w:val="Title"/></w:pPr><w:r><w:t>T</w:t></w:r></w:p>
                      <w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>H</w:t></w:r></w:p>"#;
        assert_eq!(document(bare, &[]), "# T\n\n### H\n");
    }

    #[test]
    fn lists_are_numbered_or_bulleted_as_their_definition_says_and_nest_by_level() {
        let numbering = r#"<w:numbering>
            <w:abstractNum w:abstractNumId="7">
              <w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl>
              <w:lvl w:ilvl="1"><w:numFmt w:val="bullet"/></w:lvl>
            </w:abstractNum>
            <w:num w:numId="3"><w:abstractNumId w:val="7"/></w:num>
            </w:numbering>"#;
        let rels = r#"<Relationships><Relationship Id="rId1" Type="http://x/numbering" Target="numbering.xml"/></Relationships>"#;
        let item = |list: &str, level: usize, text: &str| {
            format!(
                r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{level}"/><w:numId w:val="{list}"/></w:numPr></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
            )
        };
        let body = [
            item("3", 0, "First"),
            item("3", 1, "Under first"),
            item("3", 0, "Second"),
            "<w:p><w:r><w:t>Between</w:t></w:r></w:p>".to_string(),
            item("3", 0, "Third"),
            item("9", 0, "Of a list nobody defined"),
            item("0", 0, "In no list"),
        ]
        .concat();
        assert_eq!(
            document(
                &body,
                &[
                    ("word/numbering.xml", numbering),
                    ("word/_rels/document.xml.rels", rels)
                ]
            ),
            "1. First\n    - Under first\n2. Second\n\nBetween\n\n3. Third\n- Of a list nobody defined\n\nIn no list\n"
        );
    }

    #[test]
    fn tables_text_boxes_breaks_and_what_is_given_twice() {
        let body = r#"
            <w:tbl>
              <w:tr><w:tc><w:p><w:r><w:t>Name</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Notes</w:t></w:r></w:p></w:tc></w:tr>
              <w:tr><w:tc><w:p><w:r><w:t>A|B</w:t></w:r></w:p></w:tc>
                    <w:tc><w:p><w:r><w:t>one</w:t></w:r></w:p><w:p><w:r><w:t>two</w:t><w:br/><w:t>three</w:t></w:r></w:p>
                          <w:tbl><w:tr><w:tc><w:p><w:r><w:t>in</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>side</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
                    </w:tc></w:tr>
            </w:tbl>
            <w:p><w:r><w:t>Before</w:t></w:r><w:r><mc:AlternateContent>
              <mc:Choice><w:drawing><wps:txbx><w:txbxContent><w:p><w:r><w:t>In a box</w:t></w:r></w:p></w:txbxContent></wps:txbx></w:drawing></mc:Choice>
              <mc:Fallback><w:pict><v:textbox><w:txbxContent><w:p><w:r><w:t>In a box</w:t></w:r></w:p></w:txbxContent></v:textbox></w:pict></mc:Fallback>
            </mc:AlternateContent></w:r><w:r><w:tab/><w:t>after</w:t></w:r></w:p>
            <w:p><w:r><w:rPr><w:b w:val="0"/><w:i w:val="true"/></w:rPr><w:t>slanted</w:t></w:r><w:del><w:r><w:delText>gone</w:delText></w:r></w:del></w:p>"#;
        assert_eq!(
            document(body, &[]),
            "| Name | Notes |\n| --- | --- |\n| A\\|B | one<br>two<br>three<br>in · side |\n\n\
             In a box\n\nBefore after\n\n*slanted*\n"
        );
    }
}
