//! A stand-in for the GitHub program (`gh`) for tests. What it knows is in the directory
//! `gh-data` beside the program, so a test copies the program to a directory of its own:
//!
//! - `fake-gh auth token …` prints a token and exits with 0 when `gh-data/signed-in` exists, and
//!   complains and exits with 1 when it does not;
//! - `fake-gh api [--hostname <host>] [-H <header>]… <path>` prints the file the path names
//!   under `gh-data/api`: the path with everything but letters, digits, `.` and `-` turned into
//!   `_`, then `.json`, `.raw` or `.diff` for what the `Accept` header asks for. When there is
//!   no such file it fails the way `gh` does for an address GitHub does not know, and when the
//!   file holds an escape character it is refused, as `gh` refuses it, unless
//!   `--allow-escape-sequences` is among the arguments;
//! - `fake-gh api … -f <name>=<value> <path>` is a post: it prints `{}` and keeps the path and the
//!   fields as `gh-data/posts/<n>.txt` (the path, then one `name=value` per field), `n` counting
//!   from 1;
//! - `fake-gh repo clone <owner>/<name> <directory> [-- <git options>…]` copies
//!   `gh-data/clone/<owner>-<name>` to the directory, and fails when there is no such directory;
//! - `fake-gh gist create [--desc <text>] <file>…` copies the files to `gh-data/gist`, writes the
//!   description to `gh-data/gist/.description` and prints the address of the new gist, after a
//!   line of progress on standard error as `gh` prints one. Not signed in, it fails;
//! - `fake-gh issue create --repo <owner>/<name> --title <text> --body-file <file>` writes the
//!   title, an empty line and the body to `gh-data/issue.md` and prints the address of the new
//!   issue in that repository. Not signed in, it fails.
//! - `fake-gh issue view <n> --json …` and `fake-gh pr view [<n>] --json …` print
//!   `gh-data/issue-<n>.json` and `gh-data/pr-<n>.json` (`gh-data/pr-current.json` without a number)
//!   and fail the way `gh` does when there is no such file; `fake-gh pr diff <n>` prints
//!   `gh-data/pr-<n>.diff`;
//! - `fake-gh issue list` and `fake-gh pr list` with `--state <state>` print
//!   `gh-data/issue-list-<state>.json` and `gh-data/pr-list-<state>.json`;
//! - `fake-gh repo view --json …` prints `gh-data/repo.json`;
//! - `fake-gh pr create --title <text> --body-file <file> [--base <b>] [--head <h>]` writes the title,
//!   an empty line and the body to `gh-data/pr.md` and prints the address of the new pull request;
//! - `fake-gh pr review <n> --comment --body-file <file>` writes the body to `gh-data/review-<n>.md`.
//!   Both fail when not signed in.
//!
//! Every call adds a line to `gh-data/calls.log`: its arguments, with a tab between them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn fail(message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::from(1)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let program = std::env::current_exe().unwrap_or_default();
    let data: PathBuf = program.parent().unwrap_or(Path::new(".")).join("gh-data");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data.join("calls.log"));
    if let Ok(mut log) = log {
        let _ = writeln!(log, "{}", args.join("\t"));
    }
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["auth", "token", ..] => {
            if data.join("signed-in").exists() {
                println!("gho_fakefakefakefakefakefakefakefakefake");
                ExitCode::SUCCESS
            } else {
                fail("no oauth token found for github.com")
            }
        }
        ["api", rest @ ..] => {
            let mut kind = "json";
            let mut path = None;
            let mut escapes = false;
            let mut fields: Vec<String> = Vec::new();
            let mut rest = rest.iter();
            while let Some(word) = rest.next() {
                match *word {
                    "--hostname" => {
                        rest.next();
                    }
                    "--allow-escape-sequences" => escapes = true,
                    "-f" | "-F" | "--field" | "--raw-field" => {
                        fields.push(rest.next().copied().unwrap_or_default().to_string());
                    }
                    "-X" | "--method" => {
                        rest.next();
                    }
                    "-H" => {
                        let header = rest.next().copied().unwrap_or_default().to_ascii_lowercase();
                        if let Some(accept) = header.strip_prefix("accept:") {
                            kind = match accept.trim() {
                                "application/vnd.github.raw+json" => "raw",
                                "application/vnd.github.diff" => "diff",
                                _ => "json",
                            };
                        }
                    }
                    word => path = Some(word),
                }
            }
            let Some(path) = path else {
                return fail("accepts 1 arg(s), received 0");
            };
            if !fields.is_empty() {
                let posts = data.join("posts");
                let _ = std::fs::create_dir_all(&posts);
                let n = std::fs::read_dir(&posts).map(|dir| dir.count()).unwrap_or(0) + 1;
                let kept = format!(
                    "{path}
{}",
                    fields.join(
                        "
"
                    )
                );
                let _ = std::fs::write(posts.join(format!("{n}.txt")), kept);
                println!("{{}}");
                return ExitCode::SUCCESS;
            }
            let name: String = path
                .chars()
                .map(|c| match c {
                    c if c.is_ascii_alphanumeric() || c == '.' || c == '-' => c,
                    _ => '_',
                })
                .collect();
            match std::fs::read(data.join("api").join(format!("{name}.{kind}"))) {
                Ok(body) if body.contains(&0x1b) && !escapes => fail(
                    "the response contains terminal escape sequences; pass --allow-escape-sequences to output it \
                     anyway",
                ),
                Ok(body) => {
                    let _ = std::io::stdout().write_all(&body);
                    ExitCode::SUCCESS
                }
                Err(_) => {
                    println!("{{\"message\":\"Not Found\",\"status\":\"404\"}}");
                    fail("gh: Not Found (HTTP 404)")
                }
            }
        }
        ["repo", "clone", repo, directory, ..] => {
            let from = data.join("clone").join(repo.replace('/', "-"));
            if !from.is_dir() {
                return fail(&format!(
                    "GraphQL: Could not resolve to a Repository with the name '{repo}'. (repository)"
                ));
            }
            match copy(&from, Path::new(directory)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => fail(&format!("failed to run git: {error}")),
            }
        }
        ["gist", "create", rest @ ..] => {
            if !data.join("signed-in").exists() {
                return fail("To get started with GitHub CLI, please run:  gh auth login");
            }
            let gist = data.join("gist");
            if std::fs::create_dir_all(&gist).is_err() {
                return fail("could not create the gist");
            }
            let mut rest = rest.iter();
            while let Some(word) = rest.next() {
                if *word == "--desc" {
                    let description = rest.next().copied().unwrap_or_default();
                    let _ = std::fs::write(gist.join(".description"), description);
                } else {
                    let file = Path::new(word);
                    let copied = file.file_name().map(|name| std::fs::copy(file, gist.join(name)));
                    if !matches!(copied, Some(Ok(_))) {
                        return fail(&format!("failed to read file {word}"));
                    }
                }
            }
            eprintln!("- Creating gist...");
            println!("https://gist.github.com/tester/0123456789abcdef0123456789abcdef");
            ExitCode::SUCCESS
        }
        ["issue", "create", rest @ ..] => {
            if !data.join("signed-in").exists() {
                return fail("To get started with GitHub CLI, please run:  gh auth login");
            }
            let (mut repo, mut title, mut body) = ("", "", String::new());
            let mut rest = rest.iter();
            while let Some(word) = rest.next() {
                let value = rest.next().copied().unwrap_or_default();
                match *word {
                    "--repo" => repo = value,
                    "--title" => title = value,
                    "--body-file" => match std::fs::read_to_string(value) {
                        Ok(text) => body = text,
                        Err(_) => return fail(&format!("open {value}: no such file")),
                    },
                    other => return fail(&format!("unknown flag: {other}")),
                }
            }
            if repo.is_empty() || title.is_empty() {
                return fail(
                    "must provide `--title` and `--body` (or `--fill`) when not running interactively",
                );
            }
            let _ = std::fs::write(data.join("issue.md"), format!("{title}\n\n{body}"));
            println!("https://github.com/{repo}/issues/7");
            ExitCode::SUCCESS
        }
        [noun @ ("issue" | "pr"), "view", rest @ ..] => {
            let number = rest
                .first()
                .filter(|word| !word.starts_with("--"))
                .copied()
                .unwrap_or("current");
            match std::fs::read(data.join(format!("{noun}-{number}.json"))) {
                Ok(body) => {
                    let _ = std::io::stdout().write_all(&body);
                    ExitCode::SUCCESS
                }
                Err(_) => fail(&format!(
                    "GraphQL: Could not resolve to an issue or pull request with the number of {number}."
                )),
            }
        }
        ["pr", "diff", number, ..] => match std::fs::read(data.join(format!("pr-{number}.diff"))) {
            Ok(body) => {
                let _ = std::io::stdout().write_all(&body);
                ExitCode::SUCCESS
            }
            Err(_) => fail(&format!("could not find pull request diff: {number}")),
        },
        [noun @ ("issue" | "pr"), "list", rest @ ..] => {
            let state = rest
                .iter()
                .position(|word| *word == "--state")
                .and_then(|at| rest.get(at + 1))
                .copied()
                .unwrap_or("open");
            match std::fs::read(data.join(format!("{noun}-list-{state}.json"))) {
                Ok(body) => {
                    let _ = std::io::stdout().write_all(&body);
                    ExitCode::SUCCESS
                }
                Err(_) => {
                    println!("[]");
                    ExitCode::SUCCESS
                }
            }
        }
        ["repo", "view", ..] => match std::fs::read(data.join("repo.json")) {
            Ok(body) => {
                let _ = std::io::stdout().write_all(&body);
                ExitCode::SUCCESS
            }
            Err(_) => {
                fail("none of the git remotes configured for this repository point to a known GitHub host")
            }
        },
        ["pr", "create", rest @ ..] => {
            if !data.join("signed-in").exists() {
                return fail("To get started with GitHub CLI, please run:  gh auth login");
            }
            let (mut title, mut body) = ("", String::new());
            let mut rest = rest.iter();
            while let Some(word) = rest.next() {
                let value = rest.next().copied().unwrap_or_default();
                match *word {
                    "--title" => title = value,
                    "--base" | "--head" => {}
                    "--body-file" => match std::fs::read_to_string(value) {
                        Ok(text) => body = text,
                        Err(_) => return fail(&format!("open {value}: no such file")),
                    },
                    other => return fail(&format!("unknown flag: {other}")),
                }
            }
            if title.is_empty() {
                return fail(
                    "must provide `--title` and `--body` (or `--fill`) when not running interactively",
                );
            }
            let _ = std::fs::write(data.join("pr.md"), format!("{title}\n\n{body}"));
            println!("https://github.com/tester/repo/pull/9");
            ExitCode::SUCCESS
        }
        ["pr", "review", number, rest @ ..] => {
            if !data.join("signed-in").exists() {
                return fail("To get started with GitHub CLI, please run:  gh auth login");
            }
            let mut rest = rest.iter();
            let mut commented = false;
            while let Some(word) = rest.next() {
                match *word {
                    "--comment" => commented = true,
                    "--body-file" => {
                        let value = rest.next().copied().unwrap_or_default();
                        match std::fs::read_to_string(value) {
                            Ok(text) => {
                                let _ = std::fs::write(data.join(format!("review-{number}.md")), text);
                            }
                            Err(_) => return fail(&format!("open {value}: no such file")),
                        }
                    }
                    other => return fail(&format!("unknown flag: {other}")),
                }
            }
            if !commented {
                return fail("specify one of `--approve`, `--comment`, or `--request-changes`");
            }
            ExitCode::SUCCESS
        }
        _ => fail(&format!("unknown command: {}", args.join(" "))),
    }
}
