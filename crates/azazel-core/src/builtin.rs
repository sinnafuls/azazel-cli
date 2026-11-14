//! The skills that ship inside the binary.

use std::io::{Error, ErrorKind, Read};
use std::path::{Path, PathBuf};

use flate2::read::DeflateDecoder;

/// The files of `skills/` as the build script packed them (`build.rs` describes the layout).
/// They are only read again when a new build first runs, so they stay deflated in the binary.
static SKILLS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/skills.pack"));

/// Marks a directory whose extraction finished.
const COMPLETE: &str = ".complete";
const PREFIX: &str = "skills-";

/// Splits a length-prefixed field off the front of `pack`.
fn field(pack: &[u8]) -> Option<(&[u8], &[u8])> {
    let (length, rest) = pack.split_first_chunk::<4>()?;
    rest.split_at_checked(u32::from_le_bytes(*length) as usize)
}

/// Writes the packed skills below `root`.
fn unpack(root: &Path) -> std::io::Result<()> {
    let mut pack = Vec::new();
    DeflateDecoder::new(SKILLS).read_to_end(&mut pack)?;
    let mut rest = pack.as_slice();
    while !rest.is_empty() {
        let damaged = || Error::new(ErrorKind::InvalidData, "the built-in skills are damaged");
        let (name, after) = field(rest).ok_or_else(damaged)?;
        let (contents, after) = field(after).ok_or_else(damaged)?;
        let file = root.join(std::str::from_utf8(name).map_err(|_| damaged())?);
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(file, contents)?;
        rest = after;
    }
    Ok(())
}

/// The directory holding the built-in skills as files, so that the references beside each
/// `SKILL.md` resolve like those of any other skill. They are written below `cache_dir` the first
/// time a build runs; the copies of earlier builds are removed then.
pub fn skill_root(cache_dir: &Path) -> std::io::Result<PathBuf> {
    let name = format!("{PREFIX}{}", &blake3::hash(SKILLS).to_hex()[..16]);
    let root = cache_dir.join(&name);
    if root.join(COMPLETE).is_file() {
        return Ok(root);
    }
    std::fs::create_dir_all(&root)?;
    unpack(&root)?;
    std::fs::write(root.join(COMPLETE), "")?;
    for entry in std::fs::read_dir(cache_dir)?.flatten() {
        let other = entry.file_name();
        let other = other.to_string_lossy();
        if other.starts_with(PREFIX) && *other != name {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{SkillRoot, SkillSource, discover};

    /// The skills every installation has.
    const REQUIRED: [&str; 20] = [
        "api-design",
        "azazel-extensions",
        "browser-testing",
        "code-review",
        "debugging",
        "design-tokens",
        "docs-writing",
        "frontend-design",
        "game-dev",
        "git-workflow",
        "mcp-builder",
        "media-generation",
        "performance",
        "playtest",
        "refactoring",
        "release-engineering",
        "reverse-engineering",
        "security-review",
        "testing",
        "ui-review",
    ];

    /// The targets of the relative Markdown links in `text`.
    fn relative_links(text: &str) -> Vec<&str> {
        text.split("](")
            .skip(1)
            .filter_map(|rest| rest.split(')').next())
            .filter(|target| !target.contains("://") && !target.starts_with('#'))
            .collect()
    }

    #[test]
    fn every_built_in_skill_is_valid_bounded_and_has_its_references() {
        let cache = tempfile::tempdir().unwrap();
        let root = skill_root(cache.path()).unwrap();
        let set = discover(&[SkillRoot::new(root.clone(), SkillSource::Builtin)]);
        assert_eq!(set.invalid, []);
        assert_eq!(
            set.skills.keys().map(String::as_str).collect::<Vec<_>>(),
            REQUIRED
        );

        for skill in set.skills.values() {
            assert!(skill.body.lines().count() <= 500, "{} is too long", skill.name);
            // The folder is named after the skill, so `skill://<name>/…` and the folder agree.
            assert_eq!(skill.dir, root.join(&skill.name));
            let mut documents = vec![skill.dir.join("SKILL.md")];
            if let Ok(references) = std::fs::read_dir(skill.dir.join("references")) {
                documents.extend(references.flatten().map(|entry| entry.path()));
            }
            for document in documents {
                let text = std::fs::read_to_string(&document).unwrap();
                for link in relative_links(&text) {
                    let target = document.parent().unwrap().join(link);
                    assert!(target.is_file(), "{} links to missing {link}", document.display());
                }
            }
        }
    }

    /// The files below `dir`, as paths relative to it.
    fn files(dir: &Path, below: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir.join(below)).unwrap().flatten() {
            let path = below.join(entry.file_name());
            if entry.path().is_dir() {
                files(dir, &path, found);
            } else {
                found.push(path);
            }
        }
    }

    #[test]
    fn the_unpacked_skills_are_the_files_of_the_skills_directory() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills");
        let cache = tempfile::tempdir().unwrap();
        let root = skill_root(cache.path()).unwrap();

        let (mut shipped, mut unpacked) = (Vec::new(), Vec::new());
        files(&source, Path::new(""), &mut shipped);
        files(&root, Path::new(""), &mut unpacked);
        shipped.sort();
        unpacked.retain(|path| path != Path::new(COMPLETE));
        unpacked.sort();
        assert_eq!(unpacked, shipped);
        for file in &shipped {
            let same = std::fs::read(source.join(file)).unwrap() == std::fs::read(root.join(file)).unwrap();
            assert!(same, "{} differs from the file in skills/", file.display());
        }
    }

    #[test]
    fn extraction_happens_once_and_replaces_the_copy_of_another_build() {
        let cache = tempfile::tempdir().unwrap();
        let stale = cache.path().join("skills-0000000000000000");
        std::fs::create_dir_all(&stale).unwrap();
        let unrelated = cache.path().join("models.json");
        std::fs::write(&unrelated, "{}").unwrap();

        let root = skill_root(cache.path()).unwrap();
        assert!(root.join("debugging").join("SKILL.md").is_file());
        assert!(!stale.exists() && unrelated.is_file());

        // A second call finds the finished copy and leaves local edits alone.
        let edited = root.join("debugging").join("SKILL.md");
        std::fs::write(&edited, "changed").unwrap();
        assert_eq!(skill_root(cache.path()).unwrap(), root);
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "changed");
    }
}
