//! The models chosen lately, kept between runs for the model picker.

use std::path::{Path, PathBuf};

use azazel_core::Paths;

/// How many are kept.
const KEPT: usize = 8;

/// Where the list is kept: one model or `@role` on a line, the latest first.
pub(super) fn file(paths: &Paths) -> PathBuf {
    paths.cache_dir().join("recent-models")
}

/// The models chosen lately, the latest first; none when there is no list to read.
pub(super) fn load(file: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    let mut recent: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if recent.len() < KEPT && !recent.iter().any(|known| known == line) {
            recent.push(line.to_string());
        }
    }
    recent
}

/// Puts `spec` at the head of the list. The list is a convenience: when it cannot be written
/// the choice stands all the same, and nothing is said.
pub(super) fn remember(file: &Path, spec: &str) {
    let spec = spec.trim();
    if spec.is_empty() || spec.contains('\n') {
        return;
    }
    let mut recent = load(file);
    if recent.first().is_some_and(|first| first == spec) {
        return;
    }
    recent.retain(|known| known != spec);
    recent.insert(0, spec.to_string());
    recent.truncate(KEPT);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, recent.join("\n") + "\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_choice_comes_first_and_old_ones_fall_off_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cache").join("recent-models");
        assert_eq!(load(&file), Vec::<String>::new());
        remember(&file, "mock/a");
        remember(&file, "@smol");
        remember(&file, "mock/b");
        assert_eq!(load(&file), ["mock/b", "@smol", "mock/a"]);
        // Chosen again, a model moves to the head instead of being listed twice.
        remember(&file, "mock/a");
        assert_eq!(load(&file), ["mock/a", "mock/b", "@smol"]);
        remember(&file, "  ");
        assert_eq!(load(&file), ["mock/a", "mock/b", "@smol"]);
        for n in 0..KEPT {
            remember(&file, &format!("mock/m{n}"));
        }
        let recent = load(&file);
        assert_eq!(recent.len(), KEPT);
        assert_eq!(recent[0], format!("mock/m{}", KEPT - 1));
        assert!(!recent.contains(&"mock/a".to_string()));
        // A file written by hand may hold blank lines and repeats.
        std::fs::write(&file, "\nmock/a\n\nmock/b\nmock/a\n").unwrap();
        assert_eq!(load(&file), ["mock/a", "mock/b"]);
    }
}
