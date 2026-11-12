//! The changelog shipped inside the binary: `azazel changelog`, `/changelog`, and the one-line
//! notice on the first start after an update.

use azazel_core::Paths;

use crate::print::{EXIT_OK, EXIT_USAGE};

pub const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

/// One `## [version]` section: its version (`Unreleased` too) and its text, heading included.
#[derive(Debug, PartialEq)]
pub struct Section<'a> {
    pub version: &'a str,
    pub text: &'a str,
}

pub fn sections(changelog: &str) -> Vec<Section<'_>> {
    let mut starts: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0;
    for line in changelog.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("## [")
            && let Some((version, _)) = rest.split_once(']')
        {
            starts.push((offset, version));
        }
        offset += line.len();
    }
    // Link definitions at the foot (`[0.1.0]: https://...`) are not part of the last section.
    let end_of_sections = starts
        .last()
        .and_then(|(last, _)| changelog[*last..].find("\n[").map(|at| last + at + 1))
        .unwrap_or(changelog.len());
    starts
        .iter()
        .enumerate()
        .map(|(i, (start, version))| {
            let end = starts.get(i + 1).map_or(end_of_sections, |(next, _)| *next);
            Section {
                version,
                text: changelog[*start..end].trim_end(),
            }
        })
        .collect()
}

/// `1.10.2` as numbers for ordering; `None` for `Unreleased` and anything else.
fn numbers(version: &str) -> Option<Vec<u64>> {
    version.split('.').map(|part| part.parse().ok()).collect()
}

/// The released sections newer than `since`, newest first.
pub fn since<'a>(changelog: &'a str, since: &str) -> Result<Vec<Section<'a>>, String> {
    let floor = numbers(since.trim_start_matches('v'))
        .ok_or_else(|| format!("`{since}` is not a version such as 0.2.0"))?;
    Ok(sections(changelog)
        .into_iter()
        .filter(|s| numbers(s.version).is_some_and(|v| v > floor))
        .collect())
}

/// Entries (`- ` lines) in some sections.
fn changes(sections: &[Section<'_>]) -> usize {
    sections
        .iter()
        .map(|s| s.text.lines().filter(|line| line.starts_with("- ")).count())
        .sum()
}

/// The notice for the first start after the version changed, and records the version now
/// running in `cache/last_version`. `None` on a first install or when nothing changed.
pub fn update_notice(paths: &Paths) -> Option<String> {
    let file = paths.cache_dir().join("last_version");
    let current = env!("CARGO_PKG_VERSION");
    let previous = std::fs::read_to_string(&file)
        .ok()
        .map(|text| text.trim().to_string());
    if previous.as_deref() != Some(current) {
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&file, current);
    }
    let previous = previous?;
    if previous == current {
        return None;
    }
    let count = since(CHANGELOG, &previous).map(|s| changes(&s)).unwrap_or(0);
    Some(match count {
        0 => format!("updated {previous} to {current}"),
        n => format!("updated {previous} to {current}: {n} changes, /changelog shows them"),
    })
}

/// The text `/changelog` and `azazel changelog` show: everything, or what is newer than `since`.
pub fn text(since_version: Option<&str>) -> Result<String, String> {
    match since_version {
        None => Ok(CHANGELOG.to_string()),
        Some(version) => {
            let newer = since(CHANGELOG, version)?;
            if newer.is_empty() {
                return Ok(format!("nothing has changed since {version}"));
            }
            Ok(newer.iter().map(|s| s.text).collect::<Vec<_>>().join("\n\n"))
        }
    }
}

/// `azazel changelog [--since VERSION]`.
pub fn run(since_version: Option<&str>) -> u8 {
    match text(since_version) {
        Ok(text) => {
            println!("{}", text.trim_end());
            EXIT_OK
        }
        Err(e) => {
            eprintln!("azazel: --since {e}");
            EXIT_USAGE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "# Changelog\n\nIntro.\n\n## [Unreleased]\n\n- next\n\n## [0.10.0] - 2026-12-01\n\n### Added\n\n\
- a\n- b\n\n## [0.2.0] - 2026-11-12\n\n- c\n\n## [0.1.0] - 2026-11-11\n\n- d\n\n[0.2.0]: https://x\n[0.1.0]: https://y\n";

    #[test]
    fn sections_split_at_version_headings_without_the_link_footer() {
        let all = sections(LOG);
        let versions: Vec<&str> = all.iter().map(|s| s.version).collect();
        assert_eq!(versions, ["Unreleased", "0.10.0", "0.2.0", "0.1.0"]);
        assert_eq!(all[3].text, "## [0.1.0] - 2026-11-11\n\n- d");
    }

    #[test]
    fn since_keeps_newer_releases_compared_as_numbers() {
        let newer = since(LOG, "0.2.0").unwrap();
        assert_eq!(newer.iter().map(|s| s.version).collect::<Vec<_>>(), ["0.10.0"]);
        assert_eq!(changes(&newer), 2);
        assert_eq!(since(LOG, "v0.1.0").unwrap().len(), 2);
        assert!(since(LOG, "latest").is_err());
    }

    #[test]
    fn the_shipped_changelog_parses() {
        assert!(sections(CHANGELOG).iter().any(|s| s.version == "0.1.0"));
    }

    #[test]
    fn the_notice_shows_once_after_a_version_change() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::discover(dir.path(), dir.path().join("home").to_str());
        assert_eq!(update_notice(&paths), None);
        assert_eq!(update_notice(&paths), None);
        std::fs::write(paths.cache_dir().join("last_version"), "0.0.1").unwrap();
        let notice = update_notice(&paths).unwrap();
        assert!(
            notice.starts_with(&format!("updated 0.0.1 to {}", env!("CARGO_PKG_VERSION"))),
            "{notice}"
        );
        assert_eq!(update_notice(&paths), None);
    }
}
