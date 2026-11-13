//! `glob` and `grep`: finding files by name and by content, honouring ignore files.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::PermissionState;
use globset::{GlobBuilder, GlobMatcher};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::util::{display_path, parse_input, resolve_path};

const MAX_GLOB_RESULTS: usize = 1000;
const DEFAULT_GREP_LIMIT: usize = 200;
const MAX_MATCH_CHARS: usize = 500;

/// A pattern without `/` matches file names at any depth; with `/` it matches the path relative to
/// the search root, where `*` stays within one directory and `**` crosses directories.
fn compile_glob(pattern: &str) -> Result<GlobMatcher, String> {
    let pattern = pattern.trim().trim_start_matches("./");
    let anchored = if pattern.contains('/') {
        pattern.to_string()
    } else {
        format!("**/{pattern}")
    };
    GlobBuilder::new(&anchored)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|e| format!("invalid glob: {e}"))
}

/// Files under `base` that are not ignored, as `(path, path relative to base)`.
fn walk(base: &Path) -> impl Iterator<Item = (PathBuf, PathBuf)> + '_ {
    WalkBuilder::new(base)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .map(move |entry| {
            let path = entry.into_path();
            let relative = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
            (path, relative)
        })
}

pub struct GlobTool;

#[derive(Deserialize)]
struct GlobInput {
    pattern: String,
    path: Option<String>,
}

fn glob_files(base: &Path, matcher: &GlobMatcher) -> Vec<PathBuf> {
    let mut found: Vec<(SystemTime, PathBuf)> = walk(base)
        .filter(|(_, relative)| matcher.is_match(relative))
        .map(|(path, _)| {
            let modified = path
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (modified, path)
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().map(|(_, path)| path).collect()
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files by name. `*.rs` matches at any depth; `src/*.rs` only directly in `src`; `src/**/*.rs` \
         anywhere below it. Ignored files are skipped. Results are newest first, at most 1000."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "Directory to search; defaults to the working directory"}
            },
            "required": ["pattern"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        input
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .into_iter()
            .collect()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: GlobInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        let matcher = match compile_glob(&input.pattern) {
            Ok(matcher) => matcher,
            Err(error) => return ToolOutput::error(error),
        };
        let base = resolve_path(&ctx.cwd, input.path.as_deref().unwrap_or("."));
        if !base.is_dir() {
            return ToolOutput::error(format!("{} is not a directory", base.display()));
        }
        let cwd = ctx.cwd.clone();
        let listing = tokio::task::spawn_blocking(move || {
            let files = glob_files(&base, &matcher);
            let total = files.len();
            let mut out: Vec<String> = files
                .iter()
                .take(MAX_GLOB_RESULTS)
                .map(|path| display_path(&cwd, path))
                .collect();
            if total > MAX_GLOB_RESULTS {
                out.push(format!(
                    "[{MAX_GLOB_RESULTS} of {total} files shown; narrow the pattern]"
                ));
            }
            out
        })
        .await;
        match listing {
            Ok(lines) if lines.is_empty() => ToolOutput::text("no files match"),
            Ok(lines) => ToolOutput::text(lines.join("\n")),
            Err(e) => ToolOutput::error(format!("glob failed: {e}")),
        }
    }
}

pub struct GrepTool;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GrepMode {
    #[default]
    Content,
    Files,
    Count,
}

#[derive(Deserialize)]
struct GrepInput {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    context: usize,
    #[serde(default)]
    mode: GrepMode,
    limit: Option<usize>,
    #[serde(default)]
    multiline: bool,
}

/// Collects the matches of one file.
struct Collector<'a> {
    shown: &'a str,
    mode: GrepMode,
    lines: &'a mut Vec<String>,
    /// Matching lines still allowed across the whole search.
    remaining: &'a mut usize,
    matches: usize,
}

fn clip(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line);
    let text = text.trim_end_matches(['\n', '\r']);
    match text.char_indices().nth(MAX_MATCH_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

impl Sink for Collector<'_> {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, found: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.matches += 1;
        match self.mode {
            GrepMode::Files => Ok(false),
            GrepMode::Count => Ok(true),
            GrepMode::Content => {
                let first = found.line_number().unwrap_or(0);
                for (i, line) in found.lines().enumerate() {
                    self.lines
                        .push(format!("{}:{}:{}", self.shown, first + i as u64, clip(line)));
                }
                *self.remaining = self.remaining.saturating_sub(1);
                Ok(*self.remaining > 0)
            }
        }
    }

    fn context(&mut self, _: &Searcher, context: &SinkContext<'_>) -> Result<bool, io::Error> {
        if self.mode == GrepMode::Content && !matches!(context.kind(), SinkContextKind::Other) {
            let line = context.line_number().unwrap_or(0);
            self.lines
                .push(format!("{}-{line}-{}", self.shown, clip(context.bytes())));
        }
        Ok(true)
    }

    fn context_break(&mut self, _: &Searcher) -> Result<bool, io::Error> {
        if self.mode == GrepMode::Content {
            self.lines.push("--".to_string());
        }
        Ok(true)
    }
}

/// `withheld` says which files of a directory are left out: the secret files, which are searched
/// only when the call names one and the user allows it.
fn grep_files(
    input: &GrepInput,
    base: &Path,
    cwd: &Path,
    withheld: &dyn Fn(&Path) -> bool,
) -> Result<String, String> {
    let mut builder = RegexMatcherBuilder::new();
    builder.case_insensitive(input.ignore_case).multi_line(true);
    if input.multiline {
        builder.dot_matches_new_line(true);
    } else {
        builder.line_terminator(Some(b'\n'));
    }
    let matcher = builder.build(&input.pattern).map_err(|e| {
        let hint = if input.multiline {
            ""
        } else {
            " (set multiline to true to match across lines)"
        };
        format!("invalid pattern: {e}{hint}")
    })?;
    let filter = input.glob.as_deref().map(compile_glob).transpose()?;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .multi_line(input.multiline)
        .before_context(input.context)
        .after_context(input.context)
        .binary_detection(BinaryDetection::quit(0))
        .build();

    let limit = input.limit.unwrap_or(DEFAULT_GREP_LIMIT).max(1);
    let mut remaining = limit;
    let mut lines = Vec::new();
    let mut skipped = 0;
    let files: Box<dyn Iterator<Item = (PathBuf, PathBuf)>> = if base.is_file() {
        let name = base.file_name().map(PathBuf::from).unwrap_or_default();
        Box::new(std::iter::once((base.to_path_buf(), name)))
    } else {
        Box::new(walk(base).filter(|(path, relative)| {
            let wanted = filter.as_ref().is_none_or(|f| f.is_match(relative));
            let hidden = wanted && withheld(path);
            skipped += usize::from(hidden);
            !hidden
        }))
    };
    for (path, relative) in files {
        if remaining == 0 {
            break;
        }
        if filter.as_ref().is_some_and(|f| !f.is_match(&relative)) {
            continue;
        }
        let shown = display_path(cwd, &path);
        let mut collector = Collector {
            shown: &shown,
            mode: input.mode,
            lines: &mut lines,
            remaining: &mut remaining,
            matches: 0,
        };
        if searcher.search_path(&matcher, &path, &mut collector).is_err() {
            continue; // unreadable file
        }
        let matches = collector.matches;
        if matches == 0 {
            continue;
        }
        match input.mode {
            GrepMode::Content => {}
            GrepMode::Files => {
                lines.push(shown);
                remaining -= 1;
            }
            GrepMode::Count => {
                lines.push(format!("{shown}:{matches}"));
                remaining -= 1;
            }
        }
    }
    let left_out = match skipped {
        0 => None,
        1 => Some("[1 secret file was not searched; name it as `path` to search it]".to_string()),
        n => Some(format!(
            "[{n} secret files were not searched; name one as `path` to search it]"
        )),
    };
    if lines.is_empty() {
        return Ok(match left_out {
            Some(note) => format!("no matches\n{note}"),
            None => "no matches".to_string(),
        });
    }
    if remaining == 0 {
        lines.push(format!(
            "[limit of {limit} reached; narrow the search or raise limit]"
        ));
    }
    lines.extend(left_out);
    Ok(lines.join("\n"))
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regular expression (Rust regex syntax). `mode` is `content` \
         (matching lines as `path:line:text`, the default), `files` (paths only) or `count` (matches per \
         file). `glob` filters files, `context` adds lines around each match, `multiline` lets a pattern \
         span lines. Ignored and binary files are skipped."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "File or directory; defaults to the working directory"},
                "glob": {"type": "string", "description": "Only search files matching this glob"},
                "ignore_case": {"type": "boolean"},
                "context": {"type": "integer", "minimum": 0, "description": "Lines of context around each match"},
                "mode": {"type": "string", "enum": ["content", "files", "count"]},
                "limit": {"type": "integer", "minimum": 1, "description": "Maximum results (default 200)"},
                "multiline": {"type": "boolean"}
            },
            "required": ["pattern"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        input
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .into_iter()
            .collect()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: GrepInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        let base = resolve_path(&ctx.cwd, input.path.as_deref().unwrap_or("."));
        if !base.exists() {
            return ToolOutput::error(format!("{}: no such file or directory", base.display()));
        }
        let cwd = ctx.cwd.clone();
        // Asking about a secret file is the gate's part, and it asks when a call names one. A
        // search of a directory names none, so it passes them by.
        let permissions = ctx.services.get::<PermissionState>();
        let withheld = move |path: &Path| permissions.as_ref().is_some_and(|state| state.withholds(path));
        match tokio::task::spawn_blocking(move || grep_files(&input, &base, &cwd, &withheld)).await {
            Ok(Ok(text)) => ToolOutput::text(text),
            Ok(Err(error)) => ToolOutput::error(error),
            Err(e) => ToolOutput::error(format!("grep failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, text).unwrap();
        };
        write("Cargo.toml", "[package]\nname = \"demo\"\n");
        write(
            "src/main.rs",
            "fn main() {\n    let answer = 42;\n    println!(\"{answer}\");\n}\n",
        );
        write(
            "src/util/math.rs",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        );
        write("target/debug/build.rs", "fn main() { let answer = 0; }\n");
        write(".gitignore", "target/\n");
        write(".git/config", "fn main\n");
        write("assets/blob.bin", "fn main\0\0\0binary");
        dir
    }

    fn glob(dir: &Path, pattern: &str) -> Vec<String> {
        let mut names: Vec<String> = glob_files(dir, &compile_glob(pattern).unwrap())
            .iter()
            .map(|p| display_path(dir, p))
            .collect();
        names.sort();
        names
    }

    fn grep(dir: &Path, input: Value) -> String {
        let input: GrepInput = serde_json::from_value(input).unwrap();
        grep_files(&input, dir, dir, &|_| false).unwrap()
    }

    #[test]
    fn glob_depth_rules_and_ignore_files() {
        let dir = tree();
        assert_eq!(glob(dir.path(), "*.rs"), ["src/main.rs", "src/util/math.rs"]);
        assert_eq!(glob(dir.path(), "src/*.rs"), ["src/main.rs"]);
        assert_eq!(
            glob(dir.path(), "src/**/*.rs"),
            ["src/main.rs", "src/util/math.rs"]
        );
        assert_eq!(glob(dir.path(), "*.toml"), ["Cargo.toml"]);
        assert!(glob(dir.path(), "*.py").is_empty());
        assert!(compile_glob("src/[").is_err());
    }

    #[test]
    fn glob_lists_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.txt"), "1").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(dir.path().join("new.txt"), "2").unwrap();
        let files = glob_files(dir.path(), &compile_glob("*.txt").unwrap());
        let names: Vec<String> = files.iter().map(|p| display_path(dir.path(), p)).collect();
        assert_eq!(names, ["new.txt", "old.txt"]);
    }

    #[test]
    fn grep_content_skips_ignored_git_and_binary_files() {
        let dir = tree();
        assert_eq!(
            grep(dir.path(), json!({"pattern": "fn main"})),
            "src/main.rs:1:fn main() {"
        );
        assert_eq!(
            grep(
                dir.path(),
                json!({"pattern": "ANSWER =", "ignore_case": true, "context": 1})
            ),
            "src/main.rs-1-fn main() {\nsrc/main.rs:2:    let answer = 42;\nsrc/main.rs-3-    println!(\"{answer}\");"
        );
        assert_eq!(grep(dir.path(), json!({"pattern": "nothing here"})), "no matches");
    }

    #[test]
    fn grep_passes_by_the_files_that_are_withheld_and_says_so() {
        let dir = tree();
        std::fs::write(dir.path().join(".env"), "answer=secret\n").unwrap();
        std::fs::write(dir.path().join("src/.env.local"), "answer=secret\n").unwrap();
        let secret = |path: &Path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".env"))
        };
        let search = |input: Value, base: &Path| {
            let input: GrepInput = serde_json::from_value(input).unwrap();
            grep_files(&input, base, dir.path(), &secret).unwrap()
        };
        assert_eq!(
            search(json!({"pattern": "answer", "mode": "files"}), dir.path()),
            "src/main.rs\n[2 secret files were not searched; name one as `path` to search it]"
        );
        // Only what the search would have read counts, and nothing found is still said.
        assert_eq!(
            search(json!({"pattern": "answer=", "glob": "src/*"}), dir.path()),
            "no matches\n[1 secret file was not searched; name it as `path` to search it]"
        );
        assert_eq!(
            search(
                json!({"pattern": "answer", "glob": "*.rs", "mode": "files"}),
                dir.path()
            ),
            "src/main.rs"
        );
        // A file that is named is the gate's to ask about, not the search's to skip.
        assert_eq!(
            search(json!({"pattern": "answer"}), &dir.path().join(".env")),
            ".env:1:answer=secret"
        );
    }

    #[test]
    fn grep_modes_glob_filter_and_limit() {
        let dir = tree();
        let mut files: Vec<String> = grep(dir.path(), json!({"pattern": "fn ", "mode": "files"}))
            .lines()
            .map(str::to_string)
            .collect();
        files.sort();
        assert_eq!(files, ["src/main.rs", "src/util/math.rs"]);
        assert_eq!(
            grep(dir.path(), json!({"pattern": "answer", "mode": "count"})),
            "src/main.rs:2"
        );
        assert_eq!(
            grep(dir.path(), json!({"pattern": "fn ", "glob": "src/util/*.rs"})),
            "src/util/math.rs:1:pub fn add(a: i32, b: i32) -> i32 {"
        );
        let limited = grep(dir.path(), json!({"pattern": "a", "limit": 1, "glob": "*.rs"}));
        assert_eq!(limited.lines().count(), 2);
        assert!(limited.ends_with("[limit of 1 reached; narrow the search or raise limit]"));
    }

    #[test]
    fn grep_multiline_and_single_file_and_bad_patterns() {
        let dir = tree();
        let single: GrepInput = serde_json::from_value(json!({"pattern": "add"})).unwrap();
        let file = dir.path().join("src/util/math.rs");
        assert_eq!(
            grep_files(&single, &file, dir.path(), &|_| false).unwrap(),
            "src/util/math.rs:1:pub fn add(a: i32, b: i32) -> i32 {"
        );
        let spanning = grep(
            dir.path(),
            json!({"pattern": "i32 \\{\\n\\s+a \\+ b", "multiline": true}),
        );
        assert_eq!(
            spanning,
            "src/util/math.rs:1:pub fn add(a: i32, b: i32) -> i32 {\nsrc/util/math.rs:2:    a + b"
        );

        let across: GrepInput = serde_json::from_value(json!({"pattern": "a\\nb"})).unwrap();
        assert!(
            grep_files(&across, dir.path(), dir.path(), &|_| false)
                .unwrap_err()
                .contains("multiline")
        );
        let broken: GrepInput = serde_json::from_value(json!({"pattern": "("})).unwrap();
        assert!(
            grep_files(&broken, dir.path(), dir.path(), &|_| false)
                .unwrap_err()
                .starts_with("invalid pattern")
        );
    }
}
