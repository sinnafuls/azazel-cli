//! An Excel workbook as one CSV file per sheet, and a Markdown file that lists the sheets with
//! the first rows of each.
//!
//! A cell is what Excel last computed for it: a formula is its value, a date is written as a
//! date, and a formula that was never computed is the formula itself.

use quick_xml::events::Event;

use super::ooxml::{Package, is_rooted_in, related};
use super::{Converted, civil, markdown_table, safe_name, unused};
use crate::xml;

/// The rows of a sheet the Markdown file shows,
const PREVIEW_ROWS: usize = 20;
/// the columns of each,
const PREVIEW_COLUMNS: usize = 12;
/// and the characters of each cell.
const PREVIEW_CHARS: usize = 60;
/// Rows a sheet skips are empty lines in its CSV, so that line and row numbers agree, up to
/// this many at a time.
const MAX_GAP: usize = 10_000;

/// The strings the cells of a workbook share, by index.
fn shared_strings(xml_text: &str) -> Vec<String> {
    let mut strings = Vec::new();
    let mut reader = xml::reader(xml_text);
    let mut text = String::new();
    let mut in_text = false;
    // How to say a word, which Excel keeps beside Japanese text, is not the text.
    let mut phonetic = 0;
    while let Some(event) = xml::next(&mut reader) {
        match &event {
            Event::Start(start) => match xml::local(start.name().0) {
                b"si" => text.clear(),
                b"rPh" => phonetic += 1,
                b"t" => in_text = phonetic == 0,
                _ => {}
            },
            Event::End(end) => match xml::local(end.name().0) {
                b"si" => strings.push(std::mem::take(&mut text)),
                b"rPh" => phonetic -= 1,
                b"t" => in_text = false,
                _ => {}
            },
            other if in_text => xml::push_text(other, &mut text),
            _ => {}
        }
    }
    strings
}

/// Whether a number format shows a number as a date or a time.
fn is_date_format(id: u32, code: Option<&str>) -> bool {
    let Some(code) = code else {
        // The formats every Excel has built in.
        return matches!(id, 14..=22 | 27..=36 | 45..=47 | 50..=58);
    };
    let mut chars = code.chars();
    while let Some(c) = chars.next() {
        match c {
            // Text in quotes, a colour or a condition in brackets, and an escaped character
            // are not part of the pattern.
            '"' => {
                chars.by_ref().find(|c| *c == '"');
            }
            '[' => {
                chars.by_ref().find(|c| *c == ']');
            }
            '\\' | '_' | '*' => {
                chars.next();
            }
            'd' | 'D' | 'm' | 'M' | 'y' | 'Y' | 'h' | 'H' | 's' | 'S' => return true,
            _ => {}
        }
    }
    false
}

/// For each cell style, by index, whether it shows a date.
fn date_styles(xml_text: &str) -> Vec<bool> {
    let mut custom: Vec<(u32, String)> = Vec::new();
    let mut styles = Vec::new();
    let mut in_cell_styles = false;
    let mut reader = xml::reader(xml_text);
    while let Some(event) = xml::next(&mut reader) {
        match &event {
            Event::Start(start) => {
                let id = || xml::attr(start, "numFmtId").and_then(|id| id.parse::<u32>().ok());
                match xml::local(start.name().0) {
                    b"numFmt" => {
                        if let (Some(id), Some(code)) = (id(), xml::attr(start, "formatCode")) {
                            custom.push((id, code.into_owned()));
                        }
                    }
                    b"cellXfs" => in_cell_styles = true,
                    b"xf" if in_cell_styles => {
                        let id = id().unwrap_or(0);
                        let code = custom.iter().find(|(custom, _)| *custom == id);
                        styles.push(is_date_format(id, code.map(|(_, code)| code.as_str())));
                    }
                    _ => {}
                }
            }
            Event::End(end) if xml::local(end.name().0) == b"cellXfs" => in_cell_styles = false,
            _ => {}
        }
    }
    styles
}

/// The day and time a date cell holds: Excel counts days, from the end of 1899 or, in
/// workbooks from old Macs, from the start of 1904.
fn date(serial: f64, from_1904: bool) -> Option<String> {
    if !serial.is_finite() || !(0.0..3_000_000.0).contains(&serial) {
        return None;
    }
    let seconds = (serial * 86_400.0).round() as i64;
    let (days, of_day) = (seconds / 86_400, seconds % 86_400);
    let time = format!("{:02}:{:02}:{:02}", of_day / 3600, of_day / 60 % 60, of_day % 60);
    if days == 0 && !from_1904 {
        // A time of day without a day.
        return Some(time);
    }
    // 1970-01-01 is day 25569 of the one calendar and day 24107 of the other. Excel also
    // counts a 29th of February 1900 that never was, so the days before it are one off.
    let unix_days = match from_1904 {
        true => days - 24_107,
        false => days - 25_569 + i64::from(days < 60),
    };
    let (year, month, day) = civil(unix_days);
    Some(match of_day {
        0 => format!("{year:04}-{month:02}-{day:02}"),
        _ => format!("{year:04}-{month:02}-{day:02} {time}"),
    })
}

/// A number as Excel shows it by default: fifteen digits count, so `0.30000000000000004` is
/// `0.3`.
fn number(written: &str) -> String {
    if !written.contains(['.', 'e', 'E']) {
        return written.to_string();
    }
    let Ok(value) = written.parse::<f64>() else {
        return written.to_string();
    };
    if value == 0.0 || !value.is_finite() {
        return "0".to_string();
    }
    let magnitude = value.abs().log10().floor() as i32;
    if !(-10..15).contains(&magnitude) {
        return written.to_string();
    }
    let decimals = (14 - magnitude).clamp(0, 17) as usize;
    let shown = format!("{value:.decimals$}");
    match shown.contains('.') {
        true => shown.trim_end_matches('0').trim_end_matches('.').to_string(),
        false => shown,
    }
}

/// The column of a cell reference, from 0: `C` of `C7` is 2.
fn column(reference: &str) -> Option<usize> {
    let letters = reference.bytes().take_while(u8::is_ascii_alphabetic);
    let column = letters.fold(0usize, |column, letter| {
        column * 26 + usize::from(letter.to_ascii_uppercase() - b'A') + 1
    });
    column.checked_sub(1)
}

/// The rows of a sheet; a row holds its cells up to the last one that is not empty.
fn rows(xml_text: &str, strings: &[String], dates: &[bool], from_1904: bool) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut reader = xml::reader(xml_text);
    // The cell being read: its column, its type, whether its style is a date, its value and
    // its formula.
    let mut cell: Option<(usize, String, bool)> = None;
    let mut value = String::new();
    let mut formula = String::new();
    // Which of the two the text being read belongs to.
    let mut target = None;
    while let Some(event) = xml::next(&mut reader) {
        match &event {
            Event::Start(start) => match xml::local(start.name().0) {
                b"row" => {
                    let at = xml::attr(start, "r").and_then(|row| row.parse::<usize>().ok());
                    let at = at.map_or(rows.len(), |row| row.saturating_sub(1));
                    let gap = at.saturating_sub(rows.len()).min(MAX_GAP);
                    rows.resize(rows.len() + gap, Vec::new());
                    rows.push(Vec::new());
                }
                b"c" => {
                    let next = rows.last().map_or(0, Vec::len);
                    let at = xml::attr(start, "r").and_then(|reference| column(&reference));
                    let kind = xml::attr(start, "t").unwrap_or_default().into_owned();
                    let style = xml::attr(start, "s").and_then(|style| style.parse::<usize>().ok());
                    let is_date = style.and_then(|style| dates.get(style)).copied().unwrap_or(false);
                    cell = Some((at.unwrap_or(next), kind, is_date));
                    value.clear();
                    formula.clear();
                }
                b"v" | b"t" if cell.is_some() => target = Some(false),
                b"f" if cell.is_some() => target = Some(true),
                _ => {}
            },
            Event::End(end) => match xml::local(end.name().0) {
                b"v" | b"t" | b"f" => target = None,
                b"c" => {
                    let Some((at, kind, is_date)) = cell.take() else {
                        continue;
                    };
                    let shown = match kind.as_str() {
                        "s" => value
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .and_then(|index| strings.get(index).cloned()),
                        "b" => Some(if value.trim() == "1" { "TRUE" } else { "FALSE" }.to_string()),
                        "inlineStr" | "str" | "e" => Some(value.clone()),
                        _ if value.is_empty() => None,
                        _ if is_date => value
                            .trim()
                            .parse()
                            .ok()
                            .and_then(|serial| date(serial, from_1904)),
                        _ => Some(number(value.trim())),
                    };
                    let shown = match shown {
                        Some(shown) if !shown.is_empty() => shown,
                        Some(_) | None if !formula.is_empty() && value.is_empty() => format!("={formula}"),
                        Some(shown) => shown,
                        None => value.clone(),
                    };
                    if shown.is_empty() {
                        continue;
                    }
                    let Some(row) = rows.last_mut() else { continue };
                    if at >= row.len() {
                        row.resize(at + 1, String::new());
                    }
                    row[at] = shown;
                }
                _ => {}
            },
            other => match target {
                Some(true) => xml::push_text(other, &mut formula),
                Some(false) => xml::push_text(other, &mut value),
                None => {}
            },
        }
    }
    while rows.last().is_some_and(Vec::is_empty) {
        rows.pop();
    }
    rows
}

/// The rows as CSV: a cell with a comma, a quote, a line break or a space at either end is in
/// quotes.
fn csv(rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            let quoted =
                cell.contains([',', '"', '\n', '\r']) || cell.starts_with(' ') || cell.ends_with(' ');
            match quoted {
                true => out.push_str(&format!("\"{}\"", cell.replace('"', "\"\""))),
                false => out.push_str(cell),
            }
        }
        out.push('\n');
    }
    out
}

/// The first rows and columns of a sheet, with long cells cut, for the Markdown file.
fn preview(rows: &[Vec<String>]) -> Vec<Vec<String>> {
    let cut = |cell: &String| match cell.char_indices().nth(PREVIEW_CHARS) {
        Some((end, _)) => format!("{}…", &cell[..end]),
        None => cell.clone(),
    };
    rows.iter()
        .take(PREVIEW_ROWS)
        .map(|row| row.iter().take(PREVIEW_COLUMNS).map(cut).collect())
        .collect()
}

struct Sheet {
    name: String,
    part: String,
    hidden: bool,
}

/// The sheets of the workbook in the order of its tabs, and whether its dates count from 1904.
fn sheets(package: &mut Package<'_>, main: &str) -> (Vec<Sheet>, bool) {
    let relations = package.relations(main);
    let mut sheets = Vec::new();
    let mut from_1904 = false;
    let workbook = package.text(main).unwrap_or_default();
    if !is_rooted_in(&workbook, "workbook") {
        return (sheets, from_1904);
    }
    let mut reader = xml::reader(&workbook);
    while let Some(event) = xml::next(&mut reader) {
        let Event::Start(start) = &event else { continue };
        match xml::local(start.name().0) {
            b"workbookPr" => {
                from_1904 = xml::attr(start, "date1904").is_some_and(|value| matches!(&*value, "1" | "true"));
            }
            b"sheet" => {
                let relation = xml::attr(start, "r:id").and_then(|id| relations.get(&*id));
                // Only sheets of cells: a chart on a tab of its own has none.
                let Some(relation) = relation.filter(|relation| relation.kind.ends_with("/worksheet")) else {
                    continue;
                };
                sheets.push(Sheet {
                    name: xml::attr(start, "name").unwrap_or_default().into_owned(),
                    part: relation.target.clone(),
                    hidden: xml::attr(start, "state").is_some_and(|state| state != "visible"),
                });
            }
            _ => {}
        }
    }
    (sheets, from_1904)
}

pub(super) fn convert(bytes: &[u8]) -> Result<Converted, String> {
    let mut package = Package::open(bytes)?;
    let main = package.main().unwrap_or_else(|| "xl/workbook.xml".to_string());
    let (sheets, from_1904) = sheets(&mut package, &main);
    if sheets.is_empty() {
        return Err("it holds no sheets: it is not an Excel file".to_string());
    }
    let relations = package.relations(&main);
    let part = |package: &mut Package<'_>, kind: &str| {
        let name = related(&relations, kind)?;
        package.text(name)
    };
    let strings = part(&mut package, "/sharedStrings")
        .map(|text| shared_strings(&text))
        .unwrap_or_default();
    let dates = part(&mut package, "/styles")
        .map(|text| date_styles(&text))
        .unwrap_or_default();

    let mut converted = Converted::default();
    let mut names = Vec::new();
    for (index, sheet) in sheets.iter().enumerate() {
        let rows = match package.text(&sheet.part) {
            Some(text) => rows(&text, &strings, &dates, from_1904),
            None => Vec::new(),
        };
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        let file = format!("{}-{}.csv", index + 1, safe_name(&sheet.name, 40));
        let file = format!("sheets/{}", unused(file, &mut names));
        let hidden = if sheet.hidden { ", hidden" } else { "" };
        if !converted.markdown.is_empty() {
            converted.markdown.push('\n');
        }
        let count = |number: usize, noun: &str| match number {
            1 => format!("1 {noun}"),
            _ => format!("{number} {noun}s"),
        };
        converted.markdown.push_str(&format!(
            "# Sheet {}: {}\n\n{}, {}{hidden}. All of it: {file}\n",
            index + 1,
            sheet.name,
            count(rows.len(), "row"),
            count(columns, "column"),
        ));
        let shown = preview(&rows);
        if !shown.is_empty() {
            converted
                .markdown
                .push_str(&format!("\n{}\n", markdown_table(&shown)));
        }
        if rows.len() > PREVIEW_ROWS || columns > PREVIEW_COLUMNS {
            converted.markdown.push_str(&format!(
                "\n[the first {} rows and {} columns]\n",
                rows.len().min(PREVIEW_ROWS),
                columns.min(PREVIEW_COLUMNS)
            ));
        }
        converted.files.push((file, csv(&rows).into_bytes()));
    }
    Ok(converted)
}

#[cfg(test)]
mod tests {
    use super::super::ooxml::tests::archive;
    use super::*;

    #[test]
    fn dates_are_known_by_their_number_format_and_written_as_dates() {
        for built_in in [14, 15, 22, 30, 45, 47, 57] {
            assert!(is_date_format(built_in, None), "{built_in}");
        }
        for plain in [0, 1, 2, 9, 10, 11, 13, 37, 44, 48, 49] {
            assert!(!is_date_format(plain, None), "{plain}");
        }
        for code in [
            "yyyy-mm-dd",
            "d/m/yy h:mm",
            "[$-409]mmmm\\ d\\,\\ yyyy",
            "[h]:mm:ss",
            "DD.MM.YYYY",
        ] {
            assert!(is_date_format(164, Some(code)), "{code}");
        }
        for code in [
            "0.00",
            "#,##0 \"days\"",
            "[Red]0.0;[Blue]-0.0",
            "0.00E+00",
            "0\\d",
            "General",
            "@",
            "0_m",
        ] {
            assert!(!is_date_format(164, Some(code)), "{code}");
        }

        assert_eq!(date(46082.0, false).as_deref(), Some("2026-03-01"));
        assert_eq!(
            date(46096.604166666664, false).as_deref(),
            Some("2026-03-15 14:30:00")
        );
        assert_eq!(date(0.75, false).as_deref(), Some("18:00:00"));
        assert_eq!(date(1.0, false).as_deref(), Some("1900-01-01"));
        assert_eq!(date(59.0, false).as_deref(), Some("1900-02-28"));
        assert_eq!(date(61.0, false).as_deref(), Some("1900-03-01"));
        // A second short of midnight rounds into the next day.
        assert_eq!(date(46082.999999, false).as_deref(), Some("2026-03-02"));
        assert_eq!(date(44620.0, true).as_deref(), Some("2026-03-01"));
        assert_eq!(date(-1.0, false), None);
        assert_eq!(date(f64::NAN, false), None);
    }

    #[test]
    fn numbers_lose_the_noise_of_their_last_digits_and_nothing_else() {
        assert_eq!(number("0.30000000000000004"), "0.3");
        assert_eq!(number("1200.5"), "1200.5");
        assert_eq!(number("42"), "42");
        assert_eq!(number("12345678901234567890"), "12345678901234567890");
        assert_eq!(number("1.0"), "1");
        assert_eq!(number("-2.5000000000000004E-3"), "-0.0025");
        assert_eq!(number("1E+20"), "1E+20");
        assert_eq!(number("0.0"), "0");
        assert_eq!(number("n/a.x"), "n/a.x");
        assert_eq!(column("A1"), Some(0));
        assert_eq!(column("Z9"), Some(25));
        assert_eq!(column("AA10"), Some(26));
        assert_eq!(column("xfd1"), Some(16_383));
        assert_eq!(column("7"), None);
    }

    #[test]
    fn a_workbook_as_excel_writes_it_is_read_with_its_shared_strings_dates_and_formulas() {
        let bytes = archive(&[
            (
                "_rels/.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://x/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<workbook><workbookPr date1904="false"/><sheets>
                   <sheet name="Chart" sheetId="3" r:id="rId9"/>
                   <sheet name="Data/2026" sheetId="1" r:id="rId1"/>
                   <sheet name="Old" sheetId="2" state="veryHidden" r:id="rId2"/>
                   </sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships>
                   <Relationship Id="rId1" Type="http://x/worksheet" Target="worksheets/sheet1.xml"/>
                   <Relationship Id="rId2" Type="http://x/worksheet" Target="/xl/worksheets/sheet2.xml"/>
                   <Relationship Id="rId9" Type="http://x/chartsheet" Target="chartsheets/sheet1.xml"/>
                   <Relationship Id="rId3" Type="http://x/sharedStrings" Target="sharedStrings.xml"/>
                   <Relationship Id="rId4" Type="http://x/styles" Target="styles.xml"/>
                   </Relationships>"#,
            ),
            (
                "xl/sharedStrings.xml",
                r#"<sst><si><t>Item</t></si><si><r><t>Gears</t></r><r><t xml:space="preserve"> &amp; oil</t></r></si>
                   <si><t>東京</t><rPh sb="0" eb="2"><t>トウキョウ</t></rPh><phoneticPr fontId="1"/></si></sst>"#,
            ),
            (
                "xl/styles.xml",
                r#"<styleSheet><numFmts><numFmt numFmtId="164" formatCode="0.0 &quot;kg&quot;"/></numFmts>
                   <cellStyleXfs><xf numFmtId="14"/></cellStyleXfs>
                   <cellXfs><xf numFmtId="0"/><xf numFmtId="14"/><xf numFmtId="164"/></cellXfs></styleSheet>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData>
                   <row r="1"><c r="A1" t="s"><v>0</v></c><c r="C1" t="s"><v>1</v></c><c r="D1" t="s"><v>2</v></c></row>
                   <row r="3"><c r="A3" s="1"><v>46082</v></c><c r="B3" s="2"><v>12.5</v></c>
                     <c r="C3"><f>SUM(B1:B2)</f><v>0.30000000000000004</v></c>
                     <c r="D3" t="str"><f>A1&amp;"!"</f><v>Item!</v></c>
                     <c r="E3" t="e"><f>1/0</f><v>#DIV/0!</v></c>
                     <c r="F3"><f>NOW()</f></c><c r="G3" s="1"/></row>
                   </sheetData></worksheet>"#,
            ),
            ("xl/worksheets/sheet2.xml", "<worksheet><sheetData/></worksheet>"),
        ]);
        let converted = convert(&bytes).unwrap();
        let files: Vec<(&str, &str)> = converted
            .files
            .iter()
            .map(|(name, bytes)| (name.as_str(), std::str::from_utf8(bytes).unwrap()))
            .collect();
        assert_eq!(
            files,
            [
                (
                    "sheets/1-Data_2026.csv",
                    "Item,,Gears & oil,東京\n\n2026-03-01,12.5,0.3,Item!,#DIV/0!,=NOW()\n"
                ),
                ("sheets/2-Old.csv", ""),
            ]
        );
        assert_eq!(
            converted.markdown,
            "# Sheet 1: Data/2026\n\n3 rows, 6 columns. All of it: sheets/1-Data_2026.csv\n\n\
             | Item |  | Gears & oil | 東京 |  |  |\n| --- | --- | --- | --- | --- | --- |\n\
             |  |  |  |  |  |  |\n| 2026-03-01 | 12.5 | 0.3 | Item! | #DIV/0! | =NOW() |\n\n\
             # Sheet 2: Old\n\n0 rows, 0 columns, hidden. All of it: sheets/2-Old.csv\n"
        );
        assert!(convert(&archive(&[("word/document.xml", "<w:document/>")])).is_err());
    }
}
