//! User-defined slash commands (Markdown prompt templates) and `@path` mentions in prompts.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use azazel_ai::ModelSpec;
use regex::Regex;
use serde_json::Value;

use crate::ext::{Command, CommandHandler, CommandOutcome, ExtCtx, ExtError, Extension, Registrar};
use crate::frontmatter;
use crate::paths::Paths;
use crate::process::{RunOptions, Shell, run};

#[derive(Debug, Clone, PartialEq)]
pub struct CustomCommand {
    /// Path below the commands directory without `.md`, with `/` written as `:`.
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub model: Option<Vec<ModelSpec>>,
    pub body: String,
    pub path: PathBuf,
}

/// Command roots, lowest precedence first: installed packages, the user's, the project's.
pub fn command_roots(paths: &Paths, claude_compat: bool, packages: &crate::packages::Loaded) -> Vec<PathBuf> {
    let mut roots = packages.command_roots.clone();
    roots.push(paths.home.join("commands"));
    if claude_compat {
        roots.push(paths.project_root.join(".claude").join("commands"));
    }
    roots.push(paths.project_dir().join("commands"));
    roots
}

fn collect(dir: &Path, prefix: &str, out: &mut Vec<CustomCommand>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        if path.is_dir() {
            collect(&path, &format!("{prefix}{stem}:"), out);
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (meta, body) = match frontmatter::split(&text) {
            Some((yaml, body)) => (frontmatter::parse(yaml).unwrap_or_default(), body),
            None => (Default::default(), text.as_str()),
        };
        let field = |key: &str| {
            meta.get(key)
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let body = body.trim().to_string();
        let description = field("description")
            .unwrap_or_else(|| body.lines().next().unwrap_or_default().chars().take(80).collect());
        let model = frontmatter::string_list(meta.get("model"))
            .map(|specs| {
                specs
                    .iter()
                    .filter_map(|s| s.parse().ok())
                    .collect::<Vec<ModelSpec>>()
            })
            .filter(|chain| !chain.is_empty());
        out.push(CustomCommand {
            name: format!("{prefix}{stem}"),
            description,
            argument_hint: field("argument-hint"),
            model,
            body,
            path,
        });
    }
}

/// Every command in `roots`; a later root replaces an earlier command of the same name.
pub fn discover(roots: &[PathBuf]) -> Vec<CustomCommand> {
    let mut commands: Vec<CustomCommand> = Vec::new();
    for root in roots {
        let mut found = Vec::new();
        collect(root, "", &mut found);
        for command in found {
            commands.retain(|c| c.name != command.name);
            commands.push(command);
        }
    }
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands
}

/// Splits arguments like a shell would: whitespace separates, quotes group.
pub fn split_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in args.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => current.push(c),
        }
    }
    if started || !current.is_empty() {
        out.push(current);
    }
    out
}

static PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{([1-9]):-([^}]*)\}|\$ARGUMENTS|\$([1-9])").expect("static regex"));

/// Fills `$ARGUMENTS`, `$1`..`$9` and `${N:-default}` in a command body.
pub fn expand(body: &str, args: &str) -> String {
    let words = split_args(args);
    let word = |digit: &str| {
        digit
            .parse::<usize>()
            .ok()
            .and_then(|n| words.get(n - 1))
            .cloned()
    };
    PLACEHOLDER
        .replace_all(body, |caps: &regex::Captures<'_>| {
            match (caps.get(1), caps.get(3)) {
                (Some(n), _) => word(n.as_str()).unwrap_or_else(|| caps[2].to_string()),
                (None, Some(n)) => word(n.as_str()).unwrap_or_default(),
                (None, None) => args.trim().to_string(),
            }
        })
        .into_owned()
}

/// Replaces each line of the form `` !`command` `` with that command's output, run in `shell`.
pub async fn run_inline_commands(text: &str, shell: &Shell, cwd: &Path) -> String {
    let mut out = Vec::new();
    for line in text.lines() {
        let inline = line
            .trim()
            .strip_prefix("!`")
            .and_then(|rest| rest.strip_suffix('`'));
        match inline {
            Some(command) if !command.trim().is_empty() => {
                let mut options = RunOptions::new(cwd.to_path_buf());
                options.timeout = Some(std::time::Duration::from_secs(30));
                match run(shell, command, options).await {
                    Ok(result) => out.push(result.output.render()),
                    Err(e) => out.push(format!("[could not run `{command}`: {e}]")),
                }
            }
            _ => out.push(line.to_string()),
        }
    }
    out.join("\n")
}

struct Template {
    command: CustomCommand,
    shell: Shell,
    cwd: PathBuf,
    /// Whether `` !`command` `` lines may run.
    trusted: bool,
}

#[async_trait]
impl CommandHandler for Template {
    async fn run(&self, args: &str, _: &ExtCtx) -> CommandOutcome {
        let mut text = expand(&self.command.body, args);
        if self.trusted {
            text = run_inline_commands(&text, &self.shell, &self.cwd).await;
        }
        CommandOutcome::Prompt {
            text,
            model: self.command.model.clone(),
        }
    }
}

/// Registers the custom commands found on disk.
pub struct CustomCommandsExt {
    pub roots: Vec<PathBuf>,
    pub shell: Shell,
    pub cwd: PathBuf,
    pub trusted: bool,
}

impl Extension for CustomCommandsExt {
    fn id(&self) -> &str {
        "custom-commands"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        // Highest precedence first, because the first registration of a name wins.
        let mut commands = discover(&self.roots);
        commands.reverse();
        for command in commands {
            r.command(Command {
                name: command.name.clone(),
                description: command.description.clone(),
                argument_hint: command.argument_hint.clone(),
                handler: Arc::new(Template {
                    command,
                    shell: self.shell.clone(),
                    cwd: self.cwd.clone(),
                    trusted: self.trusted,
                }),
            });
        }
        Ok(())
    }
}

/// Size above which a mentioned text file is not inlined.
const MAX_MENTION_BYTES: u64 = 256 * 1024;
/// Size above which a mentioned image is not attached.
const MAX_MENTION_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
/// Images attached from mentions in one prompt.
pub const MAX_MENTION_IMAGES: usize = 8;

/// A prompt after its `@path` mentions were expanded.
#[derive(Debug, Default)]
pub struct Mentions {
    /// The prompt with each mentioned text file appended in a `<file>` block.
    pub text: String,
    /// Path and bytes of every inlined text file, so the caller can record them as read.
    pub files: Vec<(PathBuf, Vec<u8>)>,
    /// Each mentioned image, as given in the prompt, with its bytes. Decoding is left to the
    /// caller because it is CPU-bound.
    pub images: Vec<(String, Vec<u8>)>,
}

// A path with spaces is written in quotes: `@"my notes/a file.md"`.
static MENTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:^|\s)@(?:"([^"\n]+)"|([^\s@"]+))"#).expect("static regex"));

/// Expands the `@path` mentions of a prompt: text files are appended in `<file>` blocks, images
/// (at most [`MAX_MENTION_IMAGES`]) are returned to be attached. Mentions that name neither are left
/// as they are.
pub fn expand_mentions(text: &str, cwd: &Path) -> Mentions {
    let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut images: Vec<(String, Vec<u8>)> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out = text.to_string();
    for caps in MENTION.captures_iter(text) {
        let mention = match (caps.get(1), caps.get(2)) {
            (Some(quoted), _) => quoted.as_str(),
            (None, Some(bare)) => bare
                .as_str()
                .trim_end_matches([',', '.', ';', ':', ')', '!', '?']),
            (None, None) => continue,
        };
        let path = if Path::new(mention).is_absolute() {
            PathBuf::from(mention)
        } else {
            cwd.join(mention)
        };
        if seen.contains(&path) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_MENTION_IMAGE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        seen.push(path.clone());
        if crate::media::image_mime(&bytes).is_some() {
            if images.len() < MAX_MENTION_IMAGES {
                images.push((mention.to_string(), bytes));
            }
            continue;
        }
        if meta.len() > MAX_MENTION_BYTES {
            continue;
        }
        let Ok(content) = std::str::from_utf8(&bytes) else {
            continue;
        };
        out.push_str(&format!(
            "\n\n<file path=\"{mention}\">\n{}\n</file>",
            content.trim_end()
        ));
        files.push((path, bytes));
    }
    Mentions {
        text: out,
        files,
        images,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_expand_with_defaults_and_quoted_arguments() {
        let body =
            "Review $1 against ${2:-main}. Focus: ${3:-correctness, security}. All: $ARGUMENTS. Cost: $5.";
        assert_eq!(
            expand(body, "feature-x \"release 2\""),
            "Review feature-x against release 2. Focus: correctness, security. All: feature-x \"release 2\". Cost: ."
        );
        assert_eq!(expand("Fix $ARGUMENTS", "  the bug  "), "Fix the bug");
        assert_eq!(
            expand("No placeholders, price $10 and $HOME", "x"),
            "No placeholders, price x0 and $HOME"
        );
        assert_eq!(split_args("a 'b c' \"\" d"), ["a", "b c", "", "d"]);
    }

    #[test]
    fn discovery_names_nested_files_with_colons_and_later_roots_win() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, project) = (tmp.path().join("home"), tmp.path().join("project"));
        let write = |root: &Path, rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            &home,
            "review.md",
            "---\ndescription: home review\n---\nHome body",
        );
        write(&home, "git/sync.md", "Fetch and rebase onto $1.");
        write(
            &project,
            "review.md",
            "---\ndescription: project review\nargument-hint: \"[focus]\"\nmodel: \"@slow\"\n---\nProject body $ARGUMENTS",
        );
        write(&project, "notes.txt", "not a command");

        let commands = discover(&[home, project]);
        assert_eq!(
            commands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["git:sync", "review"]
        );
        let review = &commands[1];
        assert_eq!(review.description, "project review");
        assert_eq!(review.argument_hint.as_deref(), Some("[focus]"));
        assert_eq!(review.model, Some(vec![ModelSpec::Role("slow".into())]));
        assert_eq!(review.body, "Project body $ARGUMENTS");
        // Without frontmatter the first line stands in as the description.
        assert_eq!(commands[0].description, "Fetch and rebase onto $1.");
    }

    #[tokio::test]
    async fn inline_commands_run_only_on_their_own_line() {
        let shell = Shell::resolve("");
        let echo = if shell.is_posix() {
            "echo from-shell"
        } else {
            "Write-Output from-shell"
        };
        let text = format!("Before\n  !`{echo}`\nInline !`{echo}` stays\nAfter");
        let out = run_inline_commands(&text, &shell, &std::env::temp_dir()).await;
        assert_eq!(out, format!("Before\nfrom-shell\nInline !`{echo}` stays\nAfter"));
    }

    #[test]
    fn mentions_inline_existing_text_files_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("blob.bin"), [0xff, 0xfe, 0x00]).unwrap();
        let mut png = Vec::new();
        image::RgbaImage::new(2, 2)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        for n in 0..=MAX_MENTION_IMAGES {
            std::fs::write(dir.path().join(format!("shot{n}.png")), &png).unwrap();
        }

        let prompt = "Compare @src/a.rs, with @src/a.rs and @missing.rs; mail me@example.com about @blob.bin, @shot0.png";
        let found = expand_mentions(prompt, dir.path());
        assert_eq!(
            found.text,
            format!("{prompt}\n\n<file path=\"src/a.rs\">\nfn a() {{}}\n</file>")
        );
        assert_eq!(
            found.files,
            vec![(dir.path().join("src/a.rs"), b"fn a() {}\n".to_vec())]
        );
        assert_eq!(found.images, vec![("shot0.png".to_string(), png.clone())]);

        let many: Vec<String> = (0..=MAX_MENTION_IMAGES)
            .map(|n| format!("@shot{n}.png"))
            .collect();
        let capped = expand_mentions(&many.join(" "), dir.path());
        assert_eq!(capped.images.len(), MAX_MENTION_IMAGES);

        let none = expand_mentions("no mentions here", dir.path());
        assert_eq!(
            (none.text.as_str(), none.files.len(), none.images.len()),
            ("no mentions here", 0, 0)
        );

        // A path with spaces is mentioned in quotes.
        std::fs::create_dir_all(dir.path().join("my notes")).unwrap();
        std::fs::write(dir.path().join("my notes/a file.md"), "spaced\n").unwrap();
        let quoted = expand_mentions("read @\"my notes/a file.md\" please", dir.path());
        assert!(
            quoted
                .text
                .ends_with("<file path=\"my notes/a file.md\">\nspaced\n</file>"),
            "{}",
            quoted.text
        );
    }
}
