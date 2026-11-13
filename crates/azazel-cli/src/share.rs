//! What leaves for GitHub through the `gh` program: a session shared as a secret gist
//! (`/share`) and the issue a bug report offers (`/bug`, `azazel bug`).
//!
//! Nothing here asks: the caller says what is about to go and waits for a yes.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use azazel_core::SessionStore;
use azazel_core::session::EntryKind;

use crate::export::{Format, default_file_name, has_messages, render};

/// Where azazel's issues are.
pub const REPO: &str = "sinnafuls/azazel-cli";

/// How long `gh` gets for one upload.
const GH_LIMIT: Duration = Duration::from_secs(90);

/// The `gh` program, or what to do about not having it.
pub fn gh() -> Result<PathBuf, String> {
    azazel_tools::schemes::gh_program().ok_or_else(|| {
        "this needs the GitHub program `gh` (https://cli.github.com), signed in with `gh auth login`"
            .to_string()
    })
}

/// Runs `gh` with `args` and returns the address it printed: the last line of its output that
/// is one.
async fn gh_link(gh: &Path, args: &[&str]) -> Result<String, String> {
    let shown = format!("gh {}", args[..2.min(args.len())].join(" "));
    let run = tokio::process::Command::new(gh)
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(GH_LIMIT, run).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(format!("could not run `{shown}`: {e}")),
        Err(_) => return Err(format!("`{shown}` took more than {} s", GH_LIMIT.as_secs())),
    };
    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr);
        let why = why.trim();
        return Err(if why.is_empty() {
            format!("`{shown}` failed ({})", output.status)
        } else {
            format!("`{shown}` failed: {why}")
        });
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .rfind(|line| line.starts_with("https://"))
        .map(str::to_string)
        .ok_or_else(|| format!("`{shown}` printed no address"))
}

/// A session as it is shared: the files of the gist, each with its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shared {
    /// What the gist is called.
    pub title: String,
    pub files: Vec<(String, String)>,
    /// Prompts and replies in it.
    pub messages: usize,
}

impl Shared {
    /// The current branch of `session` with what looks like a secret hidden, whatever
    /// `tui.redact` says: the conversation to read on the gist's page, and the page of
    /// `/export` with the tool output and the images. `Err` when there is nothing to share.
    pub fn of(session: &SessionStore) -> Result<Shared, String> {
        if !has_messages(session) {
            return Err("there is nothing to share yet: this session has no messages".to_string());
        }
        let messages = session
            .branch(None)
            .iter()
            .filter(|entry| matches!(entry.kind, EntryKind::Message { .. }))
            .count();
        let page = format!("azazel-session-{}.view.html", session.id());
        let markdown = render(session, Format::Markdown, true);
        // After the title: where the rest is.
        let (title, rest) = markdown.split_once('\n').unwrap_or((&markdown, ""));
        let markdown = format!(
            "{title}\n\n> Shared from azazel. Tool output and images are in `{page}` of this gist: download it \
             and open it in a browser.\n{rest}"
        );
        Ok(Shared {
            title: session
                .name()
                .unwrap_or_else(|| format!("azazel session {}", session.id())),
            files: vec![
                (default_file_name(session, Format::Markdown), markdown),
                (page, render(session, Format::Html, true)),
            ],
            messages,
        })
    }

    pub fn bytes(&self) -> usize {
        self.files.iter().map(|(_, text)| text.len()).sum()
    }

    /// What the user is told before anything is sent.
    pub fn summary(&self) -> Vec<String> {
        let names: Vec<&str> = self.files.iter().map(|(name, _)| name.as_str()).collect();
        vec![
            format!(
                "This conversation ({} messages, {}) is uploaded to GitHub as a secret gist of the account `gh` is \
                 signed in to.",
                self.messages,
                size(self.bytes())
            ),
            format!(
                "Files: {}. They hold your prompts, the replies, the tool calls and their output, images included.",
                names.join(" and ")
            ),
            "What looks like a secret (keys, tokens, passwords) is hidden. File contents, paths and command output \
             are not."
                .to_string(),
            "Anyone who has the link can read it.".to_string(),
        ]
    }

    /// Uploads the files as one secret gist and returns its address.
    pub async fn upload(&self, gh: &Path) -> Result<String, String> {
        let dir = tempfile::tempdir().map_err(|e| format!("no temporary directory: {e}"))?;
        let mut paths = Vec::new();
        for (name, text) in &self.files {
            let path = dir.path().join(name);
            std::fs::write(&path, text).map_err(|e| format!("could not write {}: {e}", path.display()))?;
            paths.push(path.to_string_lossy().into_owned());
        }
        // A gist is secret unless `--public` is given.
        let mut args = vec!["gist", "create", "--desc", &self.title];
        args.extend(paths.iter().map(String::as_str));
        gh_link(gh, &args).await
    }
}

/// `12 KB`, `3.4 MB`.
pub fn size(bytes: usize) -> String {
    match bytes {
        0..1024 => format!("{bytes} bytes"),
        1024..1_048_576 => format!("{} KB", bytes.div_ceil(1024)),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

/// Opens an issue on `repo` and returns its address.
pub async fn open_issue(gh: &Path, repo: &str, title: &str, body: &str) -> Result<String, String> {
    let file = tempfile::NamedTempFile::new().map_err(|e| format!("no temporary file: {e}"))?;
    std::fs::write(file.path(), body)
        .map_err(|e| format!("could not write {}: {e}", file.path().display()))?;
    let body_file = file.path().to_string_lossy().into_owned();
    gh_link(
        gh,
        &[
            "issue",
            "create",
            "--repo",
            repo,
            "--title",
            title,
            "--body-file",
            &body_file,
        ],
    )
    .await
}

#[cfg(test)]
mod tests {
    use azazel_ai::{AssistantMeta, Block, Message, StopReason, Usage, WireApi};

    use super::*;

    #[test]
    fn a_shared_session_is_two_files_without_its_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionStore::create(dir.path(), dir.path(), None).unwrap();
        assert!(Shared::of(&session).unwrap_err().contains("nothing to share"));

        session.append(EntryKind::Message {
            message: Message::user("deploy with OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz0123456789"),
        });
        let meta = AssistantMeta {
            provider: "mock".into(),
            model: "lead".into(),
            api: WireApi::AnthropicMessages,
            usage: Usage::default(),
            stop: StopReason::Stop,
            response_id: None,
            duration_ms: 0,
            sources: Vec::new(),
        };
        session.append(EntryKind::Message {
            message: Message::assistant(vec![Block::text("Deployed.")], meta),
        });
        let shared = Shared::of(&session).unwrap();
        assert_eq!(shared.messages, 2);
        assert_eq!(shared.title, format!("azazel session {}", session.id()));
        let names: Vec<&str> = shared.files.iter().map(|(name, _)| name.as_str()).collect();
        // The conversation sorts first, so it is what the gist's page opens on.
        assert_eq!(
            names,
            [
                format!("azazel-session-{}.md", session.id()),
                format!("azazel-session-{}.view.html", session.id())
            ]
        );
        assert!(names[0] < names[1]);
        for (name, text) in &shared.files {
            assert!(text.contains("Deployed."), "{name}");
            assert!(!text.contains("abcdefghijklmnop"), "{name} still holds the key");
            assert!(text.contains("[redacted:"), "{name}");
        }
        assert!(shared.files[0].1.contains(".view.html` of this gist"));
        assert!(shared.files[1].1.starts_with("<!doctype html>"));
        let summary = shared.summary().join("\n");
        assert!(summary.contains("2 messages") && summary.contains("secret gist"));
        assert!(summary.contains(names[0]) && summary.contains(names[1]));
    }

    #[test]
    fn sizes_read_as_a_person_says_them() {
        assert_eq!(size(900), "900 bytes");
        assert_eq!(size(1025), "2 KB");
        assert_eq!(size(3_565_158), "3.4 MB");
    }
}
