//! Reading XML as a run of events: what the timed text of a video and the parts of an Office file
//! are read with.

use std::borrow::Cow;

use quick_xml::escape::resolve_xml_entity;
use quick_xml::events::{BytesStart, Event};

/// Reads `xml` event by event. An element without content is its start and its end.
pub fn reader(xml: &str) -> quick_xml::Reader<&[u8]> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let config = reader.config_mut();
    config.expand_empty_elements = true;
    // What is wrong with a file should not hide the text in it.
    config.check_end_names = false;
    config.allow_unmatched_ends = true;
    config.allow_dangling_amp = true;
    reader
}

/// The next event of `reader`, or `None` at the end and where the XML cannot be read further.
pub fn next<'a>(reader: &mut quick_xml::Reader<&'a [u8]>) -> Option<Event<'a>> {
    match reader.read_event() {
        Ok(Event::Eof) | Err(_) => None,
        Ok(event) => Some(event),
    }
}

/// The name of an element without its namespace prefix: `p` of `w:p`.
#[cfg(any(feature = "office", test))]
pub fn local(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|byte| *byte == b':') {
        Some(colon) => &name[colon + 1..],
        None => name,
    }
}

/// The value of the attribute `name` (with its prefix, as written) of `start`.
pub fn attr<'a>(start: &'a BytesStart<'_>, name: &str) -> Option<Cow<'a, str>> {
    let attribute = start.try_get_attribute(name).ok()??;
    attribute
        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
        .ok()
}

/// Adds the text `event` holds, when it is text, a character data section or a reference such
/// as `&amp;` or `&#39;`.
pub fn push_text(event: &Event<'_>, out: &mut String) {
    match event {
        Event::Text(text) => {
            if let Ok(text) = text.decode() {
                out.push_str(&text);
            }
        }
        Event::CData(data) => {
            if let Ok(text) = data.decode() {
                out.push_str(&text);
            }
        }
        Event::GeneralRef(reference) => {
            if let Ok(Some(character)) = reference.resolve_char_ref() {
                out.push(character);
            } else if let Ok(name) = reference.decode()
                && let Some(text) = resolve_xml_entity(&name)
            {
                out.push_str(text);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_comes_back_with_its_references_resolved_and_empty_elements_open_and_close() {
        let xml = r#"<w:p w:id="a &amp; b"><w:r/>Tom &amp; Jerry&#39;s <![CDATA[<b>]]> &#x263A;</w:p>"#;
        let mut reader = reader(xml);
        let mut text = String::new();
        let mut names = Vec::new();
        while let Some(event) = next(&mut reader) {
            match &event {
                Event::Start(start) => {
                    names.push(format!("+{}", String::from_utf8_lossy(local(start.name().0))));
                    if let Some(id) = attr(start, "w:id") {
                        names.push(format!("id={id}"));
                    }
                }
                Event::End(end) => names.push(format!("-{}", String::from_utf8_lossy(local(end.name().0)))),
                other => push_text(other, &mut text),
            }
        }
        assert_eq!(names, ["+p", "id=a & b", "+r", "-r", "-p"]);
        assert_eq!(text, "Tom & Jerry's <b> ☺");
    }
}
