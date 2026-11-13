//! What `.docx`, `.pptx` and `.xlsx` files share: a zip archive of XML parts, and the
//! relationship files that say which part a part refers to under which id.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use quick_xml::events::Event;

use super::{MAX_PART_BYTES, MAX_PARTS, safe_name, unused};
use crate::xml;

/// What a part refers to under an id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    /// What the other end is, as a URI that ends in `/image`, `/hyperlink`, `/slide` and so on.
    pub kind: String,
    /// The name of the part in the archive, or the address when the other end is outside it.
    pub target: String,
    pub external: bool,
}

pub type Relations = HashMap<String, Relation>;

/// The part `kind` names among `relations`.
pub fn related<'a>(relations: &'a Relations, kind: &str) -> Option<&'a str> {
    relations
        .values()
        .find(|relation| !relation.external && relation.kind.ends_with(kind))
        .map(|relation| relation.target.as_str())
}

/// `target` as the name of a part, when a part in the directory `base` refers to it.
fn resolve(base: &str, target: &str) -> String {
    let mut parts: Vec<&str> = match target.strip_prefix('/') {
        Some(_) => Vec::new(),
        None => base.split('/').filter(|part| !part.is_empty()).collect(),
    };
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

/// The directory of a part and the name of the file with its relationships.
fn relations_of(part: &str) -> (&str, String) {
    let (dir, file) = part.rsplit_once('/').unwrap_or(("", part));
    let relations = match dir {
        "" => format!("_rels/{file}.rels"),
        dir => format!("{dir}/_rels/{file}.rels"),
    };
    (dir, relations)
}

/// An Office file opened as the archive it is.
pub struct Package<'a> {
    archive: zip::ZipArchive<Cursor<&'a [u8]>>,
}

impl<'a> Package<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self, String> {
        let archive = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| format!("it is not a readable Office file: {e}"))?;
        Ok(Package { archive })
    }

    /// The bytes of the part `name`; `None` when there is no such part or it cannot be
    /// unpacked.
    pub fn bytes(&mut self, name: &str) -> Option<Vec<u8>> {
        let part = self.archive.by_name(name).ok()?;
        let mut bytes = Vec::new();
        part.take(MAX_PART_BYTES).read_to_end(&mut bytes).ok()?;
        Some(bytes)
    }

    /// The part `name` as text.
    pub fn text(&mut self, name: &str) -> Option<String> {
        let bytes = self.bytes(name)?;
        Some(match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(other) => String::from_utf8_lossy(other.as_bytes()).into_owned(),
        })
    }

    /// What the part `name` refers to, by id. Targets inside the archive are part names.
    pub fn relations(&mut self, name: &str) -> Relations {
        let (dir, file) = relations_of(name);
        let mut relations = Relations::new();
        let Some(text) = self.text(&file) else {
            return relations;
        };
        let mut reader = xml::reader(&text);
        while let Some(event) = xml::next(&mut reader) {
            let Event::Start(start) = &event else { continue };
            if xml::local(start.name().0) != b"Relationship" {
                continue;
            }
            let (Some(id), Some(target)) = (xml::attr(start, "Id"), xml::attr(start, "Target")) else {
                continue;
            };
            let external = xml::attr(start, "TargetMode").is_some_and(|mode| mode == "External");
            let target = match external {
                true => target.into_owned(),
                false => resolve(dir, &target),
            };
            let kind = xml::attr(start, "Type").unwrap_or_default().into_owned();
            relations.insert(
                id.into_owned(),
                Relation {
                    kind,
                    target,
                    external,
                },
            );
        }
        relations
    }

    /// The part the file is about: the document, the presentation or the workbook.
    pub fn main(&mut self) -> Option<String> {
        let relations = self.relations("");
        related(&relations, "/officeDocument").map(str::to_string)
    }

    /// The names of the parts, in the order of the archive.
    pub fn names(&self) -> Vec<String> {
        self.archive
            .file_names()
            .filter_map(|name| name.ok().map(|name| name.into_owned()))
            .collect()
    }
}

/// Whether the first element of `xml_text` is named `name`, whatever its prefix.
pub fn is_rooted_in(xml_text: &str, name: &str) -> bool {
    let mut reader = xml::reader(xml_text);
    while let Some(event) = xml::next(&mut reader) {
        if let Event::Start(start) = &event {
            return xml::local(start.name().0) == name.as_bytes();
        }
    }
    false
}

/// The pictures a document shows, taken out of it as files under `images/`.
#[derive(Default)]
pub struct Pictures {
    /// The part of each picture and the file it becomes.
    taken: Vec<(String, String)>,
    names: Vec<String>,
}

impl Pictures {
    /// The file, under the directory of the conversion, the picture in `part` becomes.
    pub fn file(&mut self, part: &str) -> Option<String> {
        if let Some((_, file)) = self.taken.iter().find(|(taken, _)| taken == part) {
            return Some(file.clone());
        }
        if self.taken.len() == MAX_PARTS {
            return None;
        }
        let name = part.rsplit('/').next().unwrap_or(part);
        let file = format!("images/{}", unused(safe_name(name, 64), &mut self.names));
        self.taken.push((part.to_string(), file.clone()));
        Some(file)
    }

    /// The pictures as files with their bytes.
    pub fn into_files(self, package: &mut Package<'_>) -> Vec<(String, Vec<u8>)> {
        self.taken
            .into_iter()
            .filter_map(|(part, file)| Some((file, package.bytes(&part)?)))
            .collect()
    }
}

#[cfg(test)]
pub mod tests {
    use std::io::Write;

    use super::*;

    /// An archive with these parts.
    pub fn archive(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in parts {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(text.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn relations_name_parts_from_the_directory_of_the_part_that_refers_to_them() {
        assert_eq!(resolve("word", "media/image1.png"), "word/media/image1.png");
        assert_eq!(
            resolve("ppt/slides", "../media/image1.png"),
            "ppt/media/image1.png"
        );
        assert_eq!(
            resolve("ppt/slides", "/ppt/notesSlides/notesSlide1.xml"),
            "ppt/notesSlides/notesSlide1.xml"
        );
        assert_eq!(resolve("", "word/document.xml"), "word/document.xml");
        assert_eq!(
            relations_of("word/document.xml"),
            ("word", "word/_rels/document.xml.rels".to_string())
        );
        assert_eq!(relations_of(""), ("", "_rels/.rels".to_string()));

        let bytes = archive(&[
            (
                "_rels/.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://x/officeDocument" Target="word/document.xml"/></Relationships>"#,
            ),
            (
                "word/_rels/document.xml.rels",
                r#"<Relationships>
                   <Relationship Id="rId2" Type="http://x/image" Target="media/a.png"/>
                   <Relationship Id="rId3" Type="http://x/hyperlink" Target="https://example.com/a?b=1&amp;c=2" TargetMode="External"/>
                   </Relationships>"#,
            ),
            ("word/document.xml", "<w:document/>"),
            ("word/media/a.png", "png"),
        ]);
        let mut package = Package::open(&bytes).unwrap();
        assert_eq!(package.main().as_deref(), Some("word/document.xml"));
        let relations = package.relations("word/document.xml");
        assert_eq!(related(&relations, "/image"), Some("word/media/a.png"));
        assert_eq!(related(&relations, "/hyperlink"), None);
        assert_eq!(
            relations["rId3"],
            Relation {
                kind: "http://x/hyperlink".into(),
                target: "https://example.com/a?b=1&c=2".into(),
                external: true
            }
        );
        assert_eq!(
            package.text("word/document.xml").as_deref(),
            Some("<w:document/>")
        );
        assert_eq!(package.text("word/missing.xml"), None);
        assert!(Package::open(b"not a zip").is_err());
        assert!(is_rooted_in(
            "<?xml version=\"1.0\"?>\n<!-- c --><w:document><w:body/></w:document>",
            "document"
        ));
        assert!(!is_rooted_in("<workbook><document/></workbook>", "document"));
        assert!(!is_rooted_in("", "document"));

        let mut pictures = Pictures::default();
        assert_eq!(pictures.file("word/media/a.png").as_deref(), Some("images/a.png"));
        assert_eq!(
            pictures.file("word/embeddings/a.png").as_deref(),
            Some("images/a-2.png")
        );
        assert_eq!(pictures.file("word/media/a.png").as_deref(), Some("images/a.png"));
        let files = pictures.into_files(&mut package);
        assert_eq!(files, [("images/a.png".to_string(), b"png".to_vec())]);
    }
}
