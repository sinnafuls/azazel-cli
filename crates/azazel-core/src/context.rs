//! Instruction files the user keeps for agents: `AGENTS.md` (and `CLAUDE.md` for compatibility).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use azazel_agent::AgentInfo;

use crate::paths::Paths;
use crate::prompt::SystemPartProvider;

/// Total size of context files placed in the system prompt.
pub const MAX_CONTEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    pub path: PathBuf,
    pub text: String,
}

/// `~/.azazel/AGENTS.md`, then one file per directory from the project root down to the working
/// directory: `AGENTS.md`, or `CLAUDE.md` when `claude_compat` is on and there is no `AGENTS.md`.
pub fn load(paths: &Paths, claude_compat: bool) -> Vec<ContextFile> {
    let mut candidates = vec![paths.home.join("AGENTS.md")];
    let mut dirs: Vec<&Path> = paths
        .cwd
        .ancestors()
        .take_while(|dir| dir.starts_with(&paths.project_root))
        .collect();
    dirs.reverse();
    if dirs.is_empty() {
        dirs.push(&paths.cwd);
    }
    for dir in dirs {
        let agents = dir.join("AGENTS.md");
        if agents.is_file() {
            candidates.push(agents);
        } else if claude_compat {
            candidates.push(dir.join("CLAUDE.md"));
        }
    }
    candidates
        .into_iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            (!text.trim().is_empty()).then_some(ContextFile { path, text })
        })
        .collect()
}

/// Each file wrapped as `<context file="path">…</context>`. When the total exceeds
/// [`MAX_CONTEXT_BYTES`] the files nearest the root win: later ones are cut or dropped.
pub fn render(files: &[ContextFile]) -> Option<String> {
    let mut out = String::new();
    let mut budget = MAX_CONTEXT_BYTES;
    for file in files {
        if budget == 0 {
            break;
        }
        let mut text = file.text.trim_end();
        let mut cut = false;
        if text.len() > budget {
            let mut end = budget;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text = &text[..end];
            cut = true;
        }
        budget -= text.len();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("<context file=\"{}\">\n{text}", file.path.display()));
        if cut {
            out.push_str("\n[truncated: the context files exceed 64 KB]");
        }
        out.push_str("\n</context>");
    }
    if out.is_empty() {
        return None;
    }
    Some(format!(
        "# Project instructions\nThe user keeps these instructions for you. Follow them.\n\n{out}"
    ))
}

/// The `context_files` part of the system prompt. Files are re-read for every request, so edits
/// take effect on the next turn. Rules that always apply follow the files, inside the same cap.
pub struct ContextFilesPart {
    pub paths: Paths,
    pub claude_compat: bool,
    pub rules: Option<std::sync::Arc<crate::rules::Rules>>,
}

#[async_trait]
impl SystemPartProvider for ContextFilesPart {
    async fn render(&self, agent: &AgentInfo) -> Option<String> {
        let mut files = load(&self.paths, self.claude_compat);
        if let Some(rules) = &self.rules {
            files.extend(rules.always_for(agent));
        }
        render(&files)
    }
}

/// Registers the `context_files` prompt part.
pub struct ContextExt {
    pub paths: Paths,
    pub claude_compat: bool,
    /// The rule files, whose always-on rules belong to this part.
    pub rules: Option<std::sync::Arc<crate::rules::Rules>>,
}

impl crate::ext::Extension for ContextExt {
    fn id(&self) -> &str {
        "context-files"
    }

    fn register(&self, r: &mut crate::ext::Registrar) -> Result<(), crate::ext::ExtError> {
        r.system_part(
            "context_files",
            std::sync::Arc::new(ContextFilesPart {
                paths: self.paths.clone(),
                claude_compat: self.claude_compat,
                rules: self.rules.clone(),
            }),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("proj/crates/core");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(tmp.path().join("proj/.azazel")).unwrap();
        let paths = Paths::discover(&cwd, Some(&home.to_string_lossy()));
        (tmp, paths)
    }

    fn names(files: &[ContextFile], paths: &Paths) -> Vec<String> {
        // Canonical, as the loaded paths are: a temporary directory may sit behind a symlink.
        let base = crate::paths::canonical(paths.home.parent().unwrap());
        files
            .iter()
            .map(|f| {
                crate::paths::canonical(&f.path)
                    .strip_prefix(&base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn files_load_root_first_and_claude_md_only_fills_gaps() {
        let (_tmp, paths) = project();
        let root = paths.project_root.clone();
        std::fs::write(paths.home.join("AGENTS.md"), "global").unwrap();
        std::fs::write(root.join("AGENTS.md"), "root agents").unwrap();
        std::fs::write(root.join("CLAUDE.md"), "root claude, shadowed").unwrap();
        std::fs::write(root.join("crates/CLAUDE.md"), "crates claude").unwrap();
        std::fs::write(root.join("crates/core/AGENTS.md"), "core agents").unwrap();
        std::fs::write(
            root.parent().unwrap().join("AGENTS.md"),
            "above the project: ignored",
        )
        .unwrap();

        let files = load(&paths, true);
        assert_eq!(
            names(&files, &paths),
            [
                "home/AGENTS.md",
                "proj/AGENTS.md",
                "proj/crates/CLAUDE.md",
                "proj/crates/core/AGENTS.md"
            ]
        );
        let without_compat = load(&paths, false);
        assert_eq!(
            names(&without_compat, &paths),
            ["home/AGENTS.md", "proj/AGENTS.md", "proj/crates/core/AGENTS.md"]
        );

        let text = render(&files).unwrap();
        let positions: Vec<usize> = ["global", "root agents", "crates claude", "core agents"]
            .iter()
            .map(|t| text.find(t).unwrap())
            .collect();
        assert!(positions.is_sorted());
        assert!(text.contains(&format!(
            "<context file=\"{}\">\nroot agents\n</context>",
            root.join("AGENTS.md").display()
        )));
    }

    #[test]
    fn empty_or_missing_files_yield_no_part() {
        let (_tmp, paths) = project();
        assert_eq!(render(&load(&paths, true)), None);
        std::fs::write(paths.project_root.join("AGENTS.md"), "  \n").unwrap();
        assert_eq!(render(&load(&paths, true)), None);
    }

    #[test]
    fn the_cap_keeps_root_most_files_and_cuts_the_rest() {
        let file = |name: &str, size: usize| ContextFile {
            path: PathBuf::from(name),
            text: "q".repeat(size),
        };
        let text = render(&[file("a", 40_000), file("b", 40_000), file("c", 10)]).unwrap();
        assert_eq!(text.matches('q').count(), MAX_CONTEXT_BYTES);
        assert!(text.contains("<context file=\"a\">") && text.contains("<context file=\"b\">"));
        assert!(!text.contains("<context file=\"c\">"));
        assert_eq!(text.matches("[truncated").count(), 1);
    }
}
