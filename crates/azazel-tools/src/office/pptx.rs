//! A PowerPoint presentation as Markdown: one heading per slide, in the order of the show, with
//! the text of its shapes, its tables, its pictures and the speaker's notes. Charts and
//! diagrams are not part of it.

use quick_xml::events::{BytesStart, Event};

use super::ooxml::{Package, Pictures, Relations, is_rooted_in, related};
use super::{Converted, markdown_table};
use crate::xml;

/// What a shape is on its slide.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Title,
    Subtitle,
    /// A placeholder of the layout: a list of points.
    Body,
    /// A text box someone drew.
    Free,
    /// The date, the footer and the slide number, which every slide repeats.
    Furniture,
}

#[derive(Default)]
struct Slide {
    title: String,
    /// Paragraphs, lists, tables and pictures, as Markdown.
    blocks: Vec<String>,
}

/// Reads the shapes of a slide, or of the page with its notes.
#[derive(Default)]
struct Walker {
    slide: Slide,
    /// The role of the shape being read, and its paragraphs with their levels.
    shape: Option<(Role, Vec<(usize, String)>)>,
    /// The level and the text of the paragraph being read.
    paragraph: Option<(usize, String)>,
    in_text: bool,
    /// The rows of the table being read, and the paragraphs of its open cell.
    table: Option<Vec<Vec<String>>>,
    cell: Option<Vec<String>>,
    /// Whether a picture is being read, and what it is said to show.
    picture: Option<String>,
    skipping: usize,
}

impl Walker {
    fn start(&mut self, start: &BytesStart<'_>, relations: &Relations, pictures: &mut Pictures) {
        match start.name().0 {
            b"mc:Fallback" => self.skipping = 1,
            b"p:sp" => self.shape = Some((Role::Free, Vec::new())),
            b"p:ph" => {
                if let Some((role, _)) = self.shape.as_mut() {
                    *role = match xml::attr(start, "type").as_deref() {
                        Some("title" | "ctrTitle") => Role::Title,
                        Some("subTitle") => Role::Subtitle,
                        Some("dt" | "ftr" | "hdr" | "sldNum" | "sldImg") => Role::Furniture,
                        _ => Role::Body,
                    };
                }
            }
            b"a:p" => self.paragraph = Some((0, String::new())),
            b"a:pPr" => {
                if let Some((level, _)) = self.paragraph.as_mut() {
                    *level = xml::attr(start, "lvl")
                        .and_then(|level| level.parse().ok())
                        .unwrap_or(0);
                }
            }
            b"a:t" => self.in_text = self.paragraph.is_some(),
            b"a:br" => {
                if let Some((_, text)) = self.paragraph.as_mut() {
                    text.push('\n');
                }
            }
            b"a:tbl" => self.table = Some(Vec::new()),
            b"a:tr" => {
                if let Some(rows) = self.table.as_mut() {
                    rows.push(Vec::new());
                }
            }
            b"a:tc" => self.cell = Some(Vec::new()),
            b"p:pic" => self.picture = Some(String::new()),
            b"p:cNvPr" => {
                if let Some(shows) = self.picture.as_mut() {
                    *shows = xml::attr(start, "descr").unwrap_or_default().trim().to_string();
                }
            }
            b"a:blip" => {
                let part = xml::attr(start, "r:embed")
                    .and_then(|id| relations.get(&*id))
                    .filter(|relation| !relation.external);
                if let (Some(shows), Some(part)) = (self.picture.take(), part)
                    && let Some(file) = pictures.file(&part.target)
                {
                    self.slide.blocks.push(format!("![{shows}]({file})"));
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, name: &[u8]) {
        match name {
            b"a:t" => self.in_text = false,
            b"a:p" => {
                let Some((level, text)) = self.paragraph.take() else {
                    return;
                };
                let text = text.trim();
                if text.is_empty() {
                    return;
                }
                if let Some(cell) = self.cell.as_mut() {
                    cell.push(text.to_string());
                } else if let Some((_, paragraphs)) = self.shape.as_mut() {
                    paragraphs.push((level, text.to_string()));
                }
            }
            b"a:tc" => {
                if let (Some(cell), Some(rows)) = (self.cell.take(), self.table.as_mut())
                    && let Some(row) = rows.last_mut()
                {
                    row.push(cell.join("\n"));
                }
            }
            b"a:tbl" => {
                if let Some(rows) = self.table.take() {
                    let table = markdown_table(&rows);
                    if !table.is_empty() {
                        self.slide.blocks.push(table);
                    }
                }
            }
            b"p:pic" => self.picture = None,
            b"p:sp" => {
                let Some((role, paragraphs)) = self.shape.take() else {
                    return;
                };
                if paragraphs.is_empty() {
                    return;
                }
                let lines = |each: &dyn Fn(usize, &str) -> String| {
                    let lines: Vec<String> = paragraphs
                        .iter()
                        .map(|(level, text)| each(*level, text))
                        .collect();
                    lines.join("\n")
                };
                match role {
                    Role::Furniture => {}
                    Role::Title => {
                        let title = lines(&|_, text| text.split_whitespace().collect::<Vec<_>>().join(" "));
                        self.slide.title = title.replace('\n', " ");
                    }
                    Role::Body => self.slide.blocks.push(lines(&|level, text| {
                        let pad = "  ".repeat(level);
                        format!("{pad}- {}", text.replace('\n', &format!("\n{pad}  ")))
                    })),
                    Role::Subtitle | Role::Free => self.slide.blocks.push(lines(&|_, text| text.to_string())),
                }
            }
            _ => {}
        }
    }
}

/// The shapes in `xml_text` as a slide.
fn slide(xml_text: &str, relations: &Relations, pictures: &mut Pictures) -> Slide {
    let mut walker = Walker::default();
    let mut reader = xml::reader(xml_text);
    while let Some(event) = xml::next(&mut reader) {
        match &event {
            Event::Start(_) if walker.skipping > 0 => walker.skipping += 1,
            Event::End(_) if walker.skipping > 0 => walker.skipping -= 1,
            Event::Start(start) => walker.start(start, relations, pictures),
            Event::End(end) => walker.end(end.name().0),
            other if walker.in_text && walker.skipping == 0 => {
                if let Some((_, text)) = walker.paragraph.as_mut() {
                    xml::push_text(other, text);
                }
            }
            _ => {}
        }
    }
    walker.slide
}

/// The parts of the slides in the order of the show.
fn slides(package: &mut Package<'_>, main: &str) -> Vec<String> {
    let relations = package.relations(main);
    let mut slides = Vec::new();
    if let Some(presentation) = package.text(main) {
        let mut reader = xml::reader(&presentation);
        while let Some(event) = xml::next(&mut reader) {
            if let Event::Start(start) = &event
                && start.name().0 == b"p:sldId"
                && let Some(id) = xml::attr(start, "r:id")
                && let Some(relation) = relations.get(&*id)
            {
                slides.push(relation.target.clone());
            }
        }
    }
    if slides.is_empty() {
        // A presentation that does not list its slides: by the numbers in their names.
        let number = |name: &str| {
            let number = name.strip_prefix("ppt/slides/slide")?.strip_suffix(".xml")?;
            number.parse::<u32>().ok()
        };
        let mut numbered: Vec<(u32, String)> = package
            .names()
            .into_iter()
            .filter_map(|name| Some((number(&name)?, name)))
            .collect();
        numbered.sort();
        slides = numbered.into_iter().map(|(_, name)| name).collect();
    }
    slides
}

pub(super) fn convert(bytes: &[u8]) -> Result<Converted, String> {
    let mut package = Package::open(bytes)?;
    let main = package
        .main()
        .unwrap_or_else(|| "ppt/presentation.xml".to_string());
    let parts = slides(&mut package, &main);
    let is_show = package
        .text(&main)
        .is_none_or(|text| is_rooted_in(&text, "presentation"));
    if parts.is_empty() || !is_show {
        return Err("it holds no slides: it is not a PowerPoint file".to_string());
    }
    let mut pictures = Pictures::default();
    let mut markdown = String::new();
    for (index, part) in parts.iter().enumerate() {
        let relations = package.relations(part);
        let shown = package.text(part).unwrap_or_default();
        let shown = slide(&shown, &relations, &mut pictures);
        let notes = related(&relations, "/notesSlide").map(str::to_string);
        let notes = notes.and_then(|part| {
            let relations = package.relations(&part);
            let page = package.text(&part)?;
            let blocks = slide(&page, &relations, &mut Pictures::default()).blocks;
            (!blocks.is_empty()).then(|| blocks.join("\n"))
        });
        if !markdown.is_empty() {
            markdown.push('\n');
        }
        markdown.push_str(&format!("# Slide {}", index + 1));
        if !shown.title.is_empty() {
            markdown.push_str(&format!(": {}", shown.title));
        }
        markdown.push('\n');
        for block in &shown.blocks {
            markdown.push_str(&format!("\n{block}\n"));
        }
        if let Some(notes) = notes {
            // The notes page keeps its points as plain lines.
            let notes: Vec<&str> = notes.lines().map(|line| line.trim_start_matches("- ")).collect();
            markdown.push_str(&format!("\nNotes: {}\n", notes.join("\n")));
        }
    }
    Ok(Converted {
        markdown,
        files: pictures.into_files(&mut package),
    })
}

#[cfg(test)]
mod tests {
    use super::super::ooxml::tests::archive;
    use super::*;

    fn shape(placeholder: &str, paragraphs: &str) -> String {
        format!(
            "<p:sp><p:nvSpPr><p:nvPr>{placeholder}</p:nvPr></p:nvSpPr><p:txBody>{paragraphs}</p:txBody></p:sp>"
        )
    }

    #[test]
    fn slides_come_in_the_order_of_the_show_with_points_boxes_and_notes() {
        let first = [
            shape(
                r#"<p:ph type="ctrTitle"/>"#,
                "<a:p><a:r><a:t>Gears</a:t></a:r><a:br/><a:r><a:t>for all</a:t></a:r></a:p>",
            ),
            shape(
                r#"<p:ph type="subTitle" idx="1"/>"#,
                "<a:p><a:r><a:t>Spring review</a:t></a:r></a:p>",
            ),
            shape(
                r#"<p:ph type="sldNum"/>"#,
                "<a:p><a:fld><a:t>1</a:t></a:fld></a:p>",
            ),
        ]
        .concat();
        let second = [
            shape(
                r#"<p:ph idx="1"/>"#,
                r#"<a:p><a:r><a:t>Up </a:t></a:r><a:r><a:t>12 %</a:t></a:r></a:p><a:p><a:pPr lvl="2"/><a:r><a:t>North</a:t></a:r></a:p><a:p></a:p>"#,
            ),
            shape("", "<a:p><a:r><a:t>A box &amp; its text</a:t></a:r></a:p>"),
            r#"<mc:AlternateContent><mc:Choice/><mc:Fallback><p:sp><p:txBody><a:p><a:r><a:t>twice</a:t></a:r></a:p></p:txBody></p:sp></mc:Fallback></mc:AlternateContent>"#.to_string(),
        ]
        .concat();
        let notes = [
            shape(r#"<p:ph type="sldImg"/>"#, ""),
            shape(
                r#"<p:ph type="body" idx="1"/>"#,
                "<a:p><a:r><a:t>Say hello.</a:t></a:r></a:p><a:p><a:r><a:t>Then stop.</a:t></a:r></a:p>",
            ),
            shape(
                r#"<p:ph type="sldNum"/>"#,
                "<a:p><a:fld><a:t>2</a:t></a:fld></a:p>",
            ),
        ]
        .concat();
        let bytes = archive(&[
            (
                "_rels/.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://x/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#,
            ),
            // The show starts with the file named slide2.
            (
                "ppt/presentation.xml",
                r#"<p:presentation><p:sldIdLst><p:sldId id="256" r:id="rId3"/><p:sldId id="257" r:id="rId2"/></p:sldIdLst></p:presentation>"#,
            ),
            (
                "ppt/_rels/presentation.xml.rels",
                r#"<Relationships><Relationship Id="rId2" Type="http://x/slide" Target="slides/slide1.xml"/><Relationship Id="rId3" Type="http://x/slide" Target="slides/slide2.xml"/></Relationships>"#,
            ),
            ("ppt/slides/slide2.xml", &format!("<p:sld>{first}</p:sld>")),
            ("ppt/slides/slide1.xml", &format!("<p:sld>{second}</p:sld>")),
            (
                "ppt/slides/_rels/slide1.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://x/notesSlide" Target="../notesSlides/notesSlide1.xml"/></Relationships>"#,
            ),
            (
                "ppt/notesSlides/notesSlide1.xml",
                &format!("<p:notes>{notes}</p:notes>"),
            ),
        ]);
        assert_eq!(
            convert(&bytes).unwrap().markdown,
            "# Slide 1: Gears for all\n\nSpring review\n\n\
             # Slide 2\n\n- Up 12 %\n    - North\n\nA box & its text\n\nNotes: Say hello.\nThen stop.\n"
        );

        // Without a list of slides, the files are taken by their numbers.
        let unlisted = archive(&[
            ("ppt/slides/slide10.xml", "<p:sld/>"),
            (
                "ppt/slides/slide2.xml",
                &format!(
                    "<p:sld>{}</p:sld>",
                    shape(r#"<p:ph type="title"/>"#, "<a:p><a:r><a:t>Two</a:t></a:r></a:p>")
                ),
            ),
        ]);
        assert_eq!(
            convert(&unlisted).unwrap().markdown,
            "# Slide 1: Two\n\n# Slide 2\n"
        );
        assert!(convert(&archive(&[("word/document.xml", "<w:document/>")])).is_err());
    }
}
