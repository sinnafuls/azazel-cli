//! The text of a PDF for `read`, page by page.

use std::panic::{AssertUnwindSafe, catch_unwind};

use pdf_extract::{Document, PlainTextOutput, output_doc_page};

/// Pages returned by one call.
pub const MAX_PAGES: u32 = 20;
pub const NO_TEXT: &str = "this PDF has no extractable text (it is probably scanned)";

/// The pages a call asks for: `"3"` is one page, `"2-6"` a range; both ends are 1-based.
pub fn parse_pages(text: &str) -> Result<(u32, u32), String> {
    let invalid =
        || format!("pages `{text}`: expected a page number such as \"3\" or a range such as \"2-6\"");
    let number = |part: &str| part.trim().parse::<u32>().ok().filter(|n| *n >= 1);
    let (first, last) = match text.split_once('-') {
        Some((first, last)) => (number(first), number(last)),
        None => (number(text), number(text)),
    };
    match (first, last) {
        (Some(first), Some(last)) if first <= last => Ok((first, last)),
        _ => Err(invalid()),
    }
}

/// Pages `first..=last` of the PDF in `bytes` (at most [`MAX_PAGES`], and fewer when they would
/// pass `max_bytes`), each under a `--- page N ---` line, then a footer that says how to go on
/// when there is more.
pub fn read(bytes: &[u8], pages: Option<(u32, u32)>, max_bytes: usize) -> Result<String, String> {
    // The parser panics on some malformed files; that fails this read and nothing else.
    let parsed = catch_unwind(|| Document::load_mem(bytes))
        .map_err(|_| "the PDF is malformed and could not be read".to_string())?;
    let mut document = parsed.map_err(|e| format!("not a readable PDF: {e}"))?;
    if document.is_encrypted() && document.decrypt("").is_err() {
        return Err("the PDF is protected by a password".to_string());
    }
    let total = document.get_pages().len() as u32;
    if total == 0 {
        return Err("the PDF has no pages".to_string());
    }
    let (first, wanted_last) = pages.unwrap_or((1, total));
    if first > total {
        return Err(format!("page {first} is past the end of the PDF ({total} pages)"));
    }
    let wanted_last = wanted_last.min(total);
    let cap = wanted_last.min(first + MAX_PAGES - 1);

    let mut out = String::new();
    let mut any_text = false;
    let mut last = first - 1;
    for page in first..=cap {
        let text = page_text(&document, page);
        let shown = match &text {
            Some(text) => text.trim(),
            None => "[this page could not be read]",
        };
        if !out.is_empty() && out.len() + shown.len() > max_bytes {
            break;
        }
        any_text |= text.as_ref().is_some_and(|text| !text.trim().is_empty());
        out.push_str(&format!("--- page {page} ---\n{shown}\n\n"));
        last = page;
    }
    if !any_text {
        return Err(NO_TEXT.to_string());
    }
    let mut out = out.trim_end().to_string();
    if first > 1 || last < total {
        out.push_str(&format!("\n[pages {first}-{last} of {total}"));
        if last < wanted_last {
            let next_last = (last + MAX_PAGES).min(total);
            out.push_str(&format!("; continue with pages=\"{}-{next_last}\"", last + 1));
        }
        out.push(']');
    }
    Ok(out)
}

fn page_text(document: &Document, page: u32) -> Option<String> {
    catch_unwind(AssertUnwindSafe(|| {
        let mut text = String::new();
        let mut output = PlainTextOutput::new(&mut text);
        output_doc_page(document, &mut output, page).ok()?;
        Some(text)
    }))
    .ok()
    .flatten()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A PDF with one page per entry of `pages`, each showing that text in Helvetica. An empty
    /// entry is a page without text.
    pub(crate) fn fixture(pages: &[&str]) -> Vec<u8> {
        let mut objects: Vec<String> = Vec::new();
        let kids: Vec<String> = (0..pages.len()).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
        objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_string());
        objects.push(format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.join(" "),
            pages.len()
        ));
        objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string());
        for (i, text) in pages.iter().enumerate() {
            let content = if text.is_empty() {
                String::new()
            } else {
                format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET")
            };
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {} 0 R \
                 /Resources << /Font << /F1 3 0 R >> >> >>",
                5 + 2 * i
            ));
            objects.push(format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ));
        }
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", i + 1));
        }
        let xref = pdf.len();
        pdf.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1));
        for offset in offsets {
            pdf.push_str(&format!("{offset:010} 00000 n \n"));
        }
        pdf.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        ));
        pdf.into_bytes()
    }

    #[test]
    fn pages_are_one_number_or_a_range() {
        assert_eq!(parse_pages("3"), Ok((3, 3)));
        assert_eq!(parse_pages(" 2 - 6 "), Ok((2, 6)));
        for bad in ["", "0", "6-2", "a", "1-", "-3", "1,2"] {
            assert!(
                parse_pages(bad).unwrap_err().contains("expected a page number"),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_whole_pdf_reads_with_page_separators_and_one_page_reads_alone() {
        let pdf = fixture(&["Alpha one", "Beta two", "Gamma three"]);
        assert_eq!(
            read(&pdf, None, usize::MAX).unwrap(),
            "--- page 1 ---\nAlpha one\n\n--- page 2 ---\nBeta two\n\n--- page 3 ---\nGamma three"
        );
        assert_eq!(
            read(&pdf, Some((2, 2)), usize::MAX).unwrap(),
            "--- page 2 ---\nBeta two\n[pages 2-2 of 3]"
        );
        assert_eq!(
            read(&pdf, Some((2, 9)), usize::MAX).unwrap(),
            "--- page 2 ---\nBeta two\n\n--- page 3 ---\nGamma three\n[pages 2-3 of 3]"
        );
        assert_eq!(
            read(&pdf, Some((4, 4)), usize::MAX).unwrap_err(),
            "page 4 is past the end of the PDF (3 pages)"
        );
    }

    #[test]
    fn long_pdfs_come_twenty_pages_at_a_time_with_the_next_range_named() {
        let texts: Vec<String> = (1..=45).map(|n| format!("Text of page {n}")).collect();
        let pages: Vec<&str> = texts.iter().map(String::as_str).collect();
        let pdf = fixture(&pages);
        let first = read(&pdf, None, usize::MAX).unwrap();
        assert!(first.starts_with("--- page 1 ---\nText of page 1\n"), "{first}");
        assert!(first.contains("--- page 20 ---") && !first.contains("--- page 21 ---"));
        assert!(
            first.ends_with("\n[pages 1-20 of 45; continue with pages=\"21-40\"]"),
            "{first}"
        );
        let asked = read(&pdf, Some((30, 60)), usize::MAX).unwrap();
        assert!(asked.ends_with("\n[pages 30-45 of 45]"), "{asked}");
        let wide = read(&pdf, Some((1, 30)), usize::MAX).unwrap();
        assert!(
            wide.ends_with("\n[pages 1-20 of 45; continue with pages=\"21-40\"]"),
            "{wide}"
        );
        // The size bound ends a call early, and the footer still says where to go on.
        let small = read(&pdf, None, 60).unwrap();
        assert!(
            small.ends_with("\n[pages 1-2 of 45; continue with pages=\"3-22\"]"),
            "{small}"
        );
    }

    #[test]
    fn a_pdf_without_a_text_layer_and_a_broken_file_say_what_is_wrong() {
        assert_eq!(read(&fixture(&["", ""]), None, usize::MAX).unwrap_err(), NO_TEXT);
        let broken = read(b"%PDF-1.4\nnot really a pdf", None, usize::MAX).unwrap_err();
        assert!(
            broken.starts_with("not a readable PDF") || broken.contains("malformed"),
            "{broken}"
        );
    }
}
