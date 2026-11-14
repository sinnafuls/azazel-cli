//! `cargo xtask docs-check` (this package, run by `xtask`): fails when the docs and the code
//! disagree about a name.
//!
//! The code is asked for what it registers (settings, trust-gated settings, hook events, keymap
//! actions, flags and subcommands, RPC commands and frames, the Luau API, slash commands, tools)
//! and the pages that own each list are read for what they document. A name on one side only is
//! a finding, and so are a default that differs, a page the index does not link, a link that
//! leads nowhere, a stale `rpc.schema.json` and a changelog without the current version.
//! Nothing is asked of the network.

mod checks;
mod code;
mod markdown;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use self::code::Code;
use self::markdown::Page;

/// One disagreement between a page and the code.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Finding {
    /// The page to fix, relative to the workspace (`docs/tools.md`).
    page: String,
    line: Option<usize>,
    text: String,
}

impl Finding {
    fn page(page: &str, text: String) -> Finding {
        Finding {
            page: page.to_string(),
            line: None,
            text,
        }
    }

    fn line(page: &str, line: usize, text: String) -> Finding {
        Finding {
            page: page.to_string(),
            line: Some(line).filter(|line| *line > 0),
            text,
        }
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}:{line}: {}", self.page, self.text),
            None => write!(f, "{}: {}", self.page, self.text),
        }
    }
}

/// The pages of a workspace: `docs/*.md`, the Markdown files at its root and the built-in skills.
struct Docs {
    root: PathBuf,
    pages: BTreeMap<String, Page>,
}

impl Docs {
    fn load(root: &Path) -> Result<Docs> {
        let mut pages = BTreeMap::new();
        let mut read = |relative: String| -> Result<()> {
            let file = root.join(&relative);
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("could not read {}", file.display()))?;
            pages.insert(relative.clone(), Page::new(&relative, &text));
            Ok(())
        };
        let markdown = |dir: &Path| -> Result<Vec<String>> {
            let mut names = Vec::new();
            for entry in
                std::fs::read_dir(dir).with_context(|| format!("could not list {}", dir.display()))?
            {
                let name = entry?.file_name().to_string_lossy().into_owned();
                if name.ends_with(".md") {
                    names.push(name);
                }
            }
            Ok(names)
        };
        for name in markdown(&root.join("docs"))? {
            read(format!("docs/{name}"))?;
        }
        for name in markdown(root)? {
            read(name)?;
        }
        let skills = root.join("skills");
        if skills.is_dir() {
            for entry in std::fs::read_dir(&skills)? {
                let name = entry?.file_name().to_string_lossy().into_owned();
                if skills.join(&name).join("SKILL.md").is_file() {
                    read(format!("skills/{name}/SKILL.md"))?;
                }
            }
        }
        Ok(Docs {
            root: root.to_path_buf(),
            pages,
        })
    }

    fn page(&self, path: &str) -> Option<&Page> {
        self.pages.get(path)
    }

    fn pages(&self) -> impl Iterator<Item = &Page> {
        self.pages.values()
    }

    /// A file of the workspace that is not a page, as text.
    fn text(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(path)).ok()
    }

    fn exists(&self, path: &str) -> bool {
        self.root.join(path).exists()
    }

    /// A page that was loaded, or a Markdown file a link leads to that was not.
    fn page_or_read(&self, path: &str) -> Option<Page> {
        match self.pages.get(path) {
            Some(page) => Some(page.clone()),
            None if path.ends_with(".md") => self.text(path).map(|text| Page::new(path, &text)),
            None => None,
        }
    }
}

/// Every disagreement between the pages and the code, in the order of the checks.
fn check(docs: &Docs, code: &Code) -> Vec<Finding> {
    let mut out = Vec::new();
    checks::config(docs, code, &mut out);
    checks::page_settings(docs, code, &mut out);
    checks::trust_gated(docs, code, &mut out);
    checks::hooks(docs, code, &mut out);
    checks::keymap(docs, code, &mut out);
    checks::cli_tables(docs, code, &mut out);
    checks::cli_usage(docs, code, &mut out);
    checks::rpc(docs, code, &mut out);
    checks::luau(docs, code, &mut out);
    checks::slash_commands(docs, code, &mut out);
    checks::tools(docs, code, &mut out);
    checks::links(docs, &mut out);
    checks::releases(docs, code, &mut out);
    checks::contributor_guide(docs, code, &mut out);
    out
}

/// What the code registers. Building the command line and the application takes more stack in
/// a debug build than a main thread has on Windows, so it happens on a thread of its own.
fn gather(root: &Path) -> Result<Code> {
    let root = root.to_path_buf();
    let gathering = std::thread::Builder::new()
        .name("docs-check".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || -> Result<Code> {
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            let code = runtime.block_on(code::gather(&root));
            runtime.shutdown_background();
            code
        })?;
    match gathering.join() {
        Ok(code) => code,
        Err(_) => bail!("reading the registries of the code panicked"),
    }
}

fn workspace_root() -> PathBuf {
    // This package is `xtask/docs-check` in the workspace.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// Runs the check on the workspace and fails with the list of findings.
fn run(root: &Path) -> Result<()> {
    let code = gather(root)?;
    let docs = Docs::load(root)?;
    let findings = check(&docs, &code);
    if findings.is_empty() {
        println!(
            "docs-check: {} pages agree with the code ({} settings, {} slash commands, {} tools, {} commands of \
             the command line)",
            docs.pages.len(),
            code.config.len(),
            code.slash_commands.len(),
            code.tools.len(),
            code.cli.len(),
        );
        return Ok(());
    }
    for finding in &findings {
        eprintln!("{finding}");
    }
    bail!(
        "the docs and the code disagree in {} place(s). Each line above names the page to fix and the name \
         that is off; the code is right unless the feature itself is wrong. See AGENTS.md, \"Docs\"",
        findings.len()
    );
}

fn main() -> ExitCode {
    match run(&workspace_root()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("docs-check: {e:#}");
            ExitCode::FAILURE
        }
    }
}
