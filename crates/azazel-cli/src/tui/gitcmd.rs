//! `/diff` and `/commit`: what changed, in the viewer, and a commit made as the user with a
//! message a small model drafts and the user edits.

use std::path::PathBuf;
use std::time::Duration;

use azazel_agent::{AgentCmd, UserInput};
use azazel_ai::{ModelSpec, Role as MessageRole};
use azazel_core::EntryKind;
use azazel_core::side::{Side, SideQuestion};
use azazel_tui::SelectItem;
use tokio_util::sync::CancellationToken;

use super::overlay::{Action, Kind, Overlay};
use super::pager::{self, Source, Start};
use super::{Internal, SHELL_NOTE, Tui};
use crate::git::{self, Changes, FileDiff};

/// The role that drafts commit messages.
const DRAFT_ROLE: &str = "smol";
const DRAFT_LIMIT: Duration = Duration::from_secs(45);
/// How much of a diff between two checkpoints is read.
const MAX_CHECKPOINT_DIFF_BYTES: usize = 2_000_000;

/// A commit that waits for a decision: stage everything, or not at all.
pub(super) struct Pending {
    top: PathBuf,
    changes: Changes,
    hint: String,
}

/// A drafted commit message on its way to the input it is edited in.
pub(super) struct Draft {
    top: PathBuf,
    stage_all: bool,
    /// The message, or why there is none.
    message: Result<String, String>,
}

fn diff_sources(summary: String, files: Vec<FileDiff>) -> Vec<Source> {
    let mut sources = vec![Source::Note(summary)];
    sources.extend(files.into_iter().map(|file| Source::Diff {
        file: file.file,
        text: file.text,
    }));
    sources
}

fn files_changed(count: usize) -> String {
    let files = if count == 1 {
        "1 file".to_string()
    } else {
        format!("{count} files")
    };
    format!("{files} changed · [ and ] jump between files · y copies the file's diff")
}

impl Tui {
    /// `/diff`: the working tree against `HEAD`; `/diff turn` and `/diff session`: what changed
    /// since the last prompt, or since the session's first one, from its checkpoints.
    pub(super) async fn diff_command(&mut self, args: &str) {
        match args {
            "" => self.working_tree_diff().await,
            "turn" => self.checkpoint_diff(false).await,
            "session" => self.checkpoint_diff(true).await,
            _ => self.fail("usage: /diff, /diff turn or /diff session"),
        }
    }

    async fn working_tree_diff(&mut self) {
        let Some(cwd) = self.local_cwd() else {
            self.here_only("/diff");
            return;
        };
        let Some(top) = git::toplevel(&cwd).await else {
            return self.fail(
                "this directory is not in a git repository; /diff turn and /diff session show what \
                 changed since a checkpoint",
            );
        };
        let files = match git::working_tree(&top).await {
            Ok(files) => files,
            Err(why) => return self.fail(&format!("git: {why}")),
        };
        if files.is_empty() {
            return self.info("no changes: the working tree matches HEAD");
        }
        let sources = diff_sources(files_changed(files.len()), files);
        pager::show(self, "Diff · working tree against HEAD", sources, Start::Top).await;
    }

    /// The checkpoint to compare with: the session's first, or the one taken before the last
    /// prompt. A checkpoint a rewind left behind precedes no prompt and is not one of these.
    fn checkpoint(&self, first: bool) -> Option<String> {
        let mut newest = None;
        let mut before_prompt = None;
        for entry in self.session.branch(None) {
            match entry.kind {
                EntryKind::Checkpoint { commit, .. } => newest = Some(commit),
                EntryKind::Message { message }
                    if message.role == MessageRole::User && !super::slash::is_note(&message.text()) =>
                {
                    if first && before_prompt.is_some() {
                        continue;
                    }
                    if let Some(commit) = &newest {
                        before_prompt = Some(commit.clone());
                    }
                }
                _ => {}
            }
        }
        before_prompt
    }

    async fn checkpoint_diff(&mut self, whole_session: bool) {
        let Some(local) = self.backend.local() else {
            self.here_only("/diff");
            return;
        };
        let checkpoints = local.active.checkpoints.clone();
        let Some(from) = self.checkpoint(whole_session) else {
            return self.info(&match checkpoints.disabled().await {
                Some(reason) => format!("no checkpoint to compare with: {reason}"),
                None => "no checkpoint to compare with: no prompt has been sent in this session".to_string(),
            });
        };
        let now = match checkpoints.snapshot("diff").await {
            Ok(Some(now)) => now,
            Ok(None) => {
                let reason = checkpoints.disabled().await.unwrap_or_default();
                return self.fail(&format!("checkpoints are off: {reason}"));
            }
            Err(reason) => return self.fail(&format!("checkpoints are off: {reason}")),
        };
        let since = if whole_session {
            "the session's first prompt"
        } else {
            "your last prompt"
        };
        let diff = checkpoints.diff(&from, &now, MAX_CHECKPOINT_DIFF_BYTES).await;
        let files = git::split(&diff);
        if files.is_empty() {
            return self.info(&format!("no file has changed since {since}"));
        }
        let sources = diff_sources(files_changed(files.len()), files);
        pager::show(self, &format!("Diff · since {since}"), sources, Start::Top).await;
    }

    /// `/commit [hint]`: drafts a message for what is staged (or, after asking, for everything),
    /// lets the user edit it and commits. Never amends, never pushes.
    pub(super) async fn commit_command(&mut self, hint: &str) {
        let Some(cwd) = self.local_cwd() else {
            self.here_only("/commit");
            return;
        };
        let Some(top) = git::toplevel(&cwd).await else {
            return self.fail("this directory is not in a git repository");
        };
        let changes = match git::changes(&top).await {
            Ok(Some(changes)) => changes,
            Ok(None) => return self.info("nothing to commit: the working tree is clean"),
            Err(why) => return self.fail(&format!("git: {why}")),
        };
        let pending = Pending {
            top,
            changes,
            hint: hint.to_string(),
        };
        if pending.changes.staged {
            return self.draft_commit(pending, false);
        }
        let items = vec![
            SelectItem::new("Stage everything and commit", "git add -A"),
            SelectItem::new("Cancel", "stage what you want with !git add, then /commit"),
        ];
        let mut body = vec!["Nothing is staged. These files have changes:".to_string()];
        body.extend(pending.changes.status.lines().map(str::to_string));
        self.overlay = Some(Overlay::select(
            "Commit",
            body,
            items,
            false,
            Action::CommitStage(Box::new(pending)),
        ));
    }

    /// Asks the small model for a message in the background; the input opens when it is there.
    fn draft_commit(&mut self, pending: Pending, stage_all: bool) {
        let Pending { top, changes, hint } = pending;
        let done = self.internal_tx.clone();
        let local = self.backend.local();
        let side = local.and_then(|local| local.app.registry.services.get::<Side>());
        let lead = local.map(|local| local.active.lead.clone());
        let (Some(side), Some(lead)) = (side, lead) else {
            let message = Err("no model is available in this session".to_string());
            let _ = done.send(Internal::CommitDraft(Draft {
                top,
                stage_all,
                message,
            }));
            return;
        };
        self.info("drafting a commit message…");
        // The small model first, then whatever the lead runs on.
        let mut chain = vec![ModelSpec::Role(DRAFT_ROLE.to_string())];
        chain.extend(lead.status().chain);
        let agent = lead.info().clone();
        tokio::spawn(async move {
            let mut ignore = |_: &str| {};
            let asked = side.ask(
                SideQuestion {
                    agent: &agent,
                    context: Vec::new(),
                    question: git::message_request(&changes, &hint),
                    chain,
                    purpose: "commit",
                    framed: false,
                },
                &mut ignore,
                CancellationToken::new(),
            );
            let message = match tokio::time::timeout(DRAFT_LIMIT, asked).await {
                Ok(Ok(answer)) => Ok(git::clean_message(&answer.text)),
                Ok(Err(why)) => Err(why),
                Err(_) => Err("the model took too long".to_string()),
            };
            let _ = done.send(Internal::CommitDraft(Draft {
                top,
                stage_all,
                message,
            }));
        });
    }

    /// Opens the drafted message for editing, once nothing else is asking for the user.
    pub(super) fn on_commit_draft(&mut self, draft: Draft) {
        let mut body = vec![if draft.stage_all {
            "Everything is staged first (git add -A), then committed.".to_string()
        } else {
            "What is staged is committed.".to_string()
        }];
        let message = match draft.message {
            Ok(message) => message,
            Err(why) => {
                body.push(format!("No message was drafted: {why}. Write it yourself."));
                String::new()
            }
        };
        let editor_key = self
            .keymap
            .keys_for("external_editor")
            .first()
            .map(|key| format!(" · {key} opens your editor"))
            .unwrap_or_default();
        body.push(format!(
            "Edit the message. shift+enter adds a line{editor_key}. An empty message cancels."
        ));
        let mut overlay = Overlay::input(
            "Commit message",
            body,
            Action::Commit {
                top: draft.top,
                stage_all: draft.stage_all,
            },
        );
        if let Kind::Input { editor, .. } = &mut overlay.kind {
            editor.set_text(&message);
        }
        self.deferred.push_back(overlay);
    }

    /// What the overlays of this module do when they are answered.
    pub(super) fn finish_commit(&mut self, action: Action, choice: usize, typed: String) {
        match action {
            Action::CommitStage(pending) if choice == 0 => self.draft_commit(*pending, true),
            Action::CommitStage(_) => self.info("nothing was committed"),
            Action::Commit { top, stage_all } => {
                let message = typed.trim().to_string();
                if message.is_empty() {
                    return self.info("nothing was committed: the message is empty");
                }
                let Some(lead) = self.backend.local().map(|local| local.active.lead.clone()) else {
                    return;
                };
                self.notice = Some("committing…".to_string());
                let done = self.internal_tx.clone();
                tokio::spawn(async move {
                    let made = git::commit(&top, &message, stage_all).await;
                    // Shown, and told to the agent, like a command the user ran with `!`.
                    let note = format!(
                        "{SHELL_NOTE}{}` myself:\n{}\n{}",
                        made.command, made.output, made.status
                    );
                    lead.send(AgentCmd::Append(UserInput::text(note)));
                    let _ = done.send(Internal::Shell {
                        output: made.output,
                        status: made.status,
                    });
                });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_viewer_gets_a_summary_and_one_item_per_file() {
        let files = vec![
            FileDiff {
                file: "a.rs  +1".into(),
                text: "--- a/a.rs\n+++ b/a.rs".into(),
            },
            FileDiff {
                file: "b.rs (new)".into(),
                text: "new file".into(),
            },
        ];
        let sources = diff_sources(files_changed(files.len()), files);
        assert!(matches!(&sources[0], Source::Note(note) if note.starts_with("2 files changed")));
        assert!(matches!(&sources[1], Source::Diff { file, .. } if file == "a.rs  +1"));
        assert!(
            matches!(&sources[2], Source::Diff { file, text } if file == "b.rs (new)" && text == "new file")
        );
        assert!(files_changed(1).starts_with("1 file changed"));
    }
}
