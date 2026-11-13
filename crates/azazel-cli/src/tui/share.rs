//! `/share` and `/bug`: a session uploaded as a secret gist, and a bug report written to a zip
//! with the offer of an issue. Both say what is about to leave the machine and wait for a yes.

use std::path::PathBuf;

use azazel_tui::SelectItem;

use super::overlay::{Action, Overlay};
use super::{Internal, Tui};
use crate::bug::Report;
use crate::export::has_messages;
use crate::share::{self, Shared};

/// What came of something written or uploaded beside the UI loop.
pub(super) enum Sent {
    /// The address of the gist of `/share`.
    Gist(Result<String, String>),
    /// The bug report and the file it was written to.
    Report(Result<(Box<Report>, PathBuf), String>),
    /// The address of the issue of `/bug`.
    Issue(Result<String, String>),
}

impl Tui {
    /// `/share`: says what would be uploaded and asks.
    pub(super) fn share_command(&mut self) {
        let shared = match Shared::of(&self.session) {
            Ok(shared) => shared,
            Err(why) => return self.info(&why),
        };
        if let Err(why) = share::gh() {
            return self.fail(&why);
        }
        let items = vec![
            SelectItem::new("Share", "upload it as a secret gist"),
            SelectItem::new("Cancel", "nothing is sent"),
        ];
        self.overlay = Some(Overlay::select(
            "Share this session?",
            shared.summary(),
            items,
            false,
            Action::Share(Box::new(shared)),
        ));
    }

    /// `/bug [what went wrong]`: says what a report holds and asks whether the conversation
    /// goes into it.
    pub(super) fn bug_command(&mut self, description: &str) {
        let body = vec![
            format!(
                "A zip is written to {}: what you said, azazel's version and the system, what `azazel doctor` \
                 prints, the settings in effect with secrets removed, and the end of the log.",
                self.env.paths.reports_dir().display()
            ),
            "It stays on this machine. Afterwards you are asked whether to open an issue with a summary."
                .to_string(),
        ];
        let mut items = vec![SelectItem::new("Write the report", "without the conversation")];
        let mut rows = vec![Some(false)];
        if has_messages(&self.session) {
            items.push(SelectItem::new(
                "Write it with this conversation",
                "as a page in the zip, secrets hidden",
            ));
            rows.push(Some(true));
        }
        items.push(SelectItem::new("Cancel", ""));
        rows.push(None);
        let action = Action::Bug {
            description: description.to_string(),
            rows,
        };
        self.overlay = Some(Overlay::select("Bug report", body, items, false, action));
    }

    /// What the overlays of this module do when they are answered.
    pub(super) fn finish_share(&mut self, action: Action, choice: usize) {
        let done = self.internal_tx.clone();
        match action {
            Action::Share(shared) if choice == 0 => {
                let gh = match share::gh() {
                    Ok(gh) => gh,
                    Err(why) => return self.fail(&why),
                };
                self.notice = Some("sharing…".to_string());
                tokio::spawn(async move {
                    let _ = done.send(Internal::Sent(Sent::Gist(shared.upload(&gh).await)));
                });
            }
            Action::Share(_) => self.info("nothing was shared"),
            Action::Bug { description, rows } => {
                let Some(transcript) = rows.get(choice).copied().flatten() else {
                    return self.info("no report was written");
                };
                let paths = self.env.paths.clone();
                let config = self.env.config.clone();
                let trusted = self.backend.local().is_some_and(|local| local.app.trusted);
                let session = transcript.then(|| self.session.clone());
                self.notice = Some("writing the bug report…".to_string());
                tokio::spawn(async move {
                    let report =
                        Report::gather(&paths, trusted, &config, &description, session.as_deref()).await;
                    let written = report
                        .write(&paths.reports_dir())
                        .map(|file| (Box::new(report), file));
                    let _ = done.send(Internal::Sent(Sent::Report(written)));
                });
            }
            Action::BugIssue(report) if choice == 1 => {
                let gh = match share::gh() {
                    Ok(gh) => gh,
                    Err(why) => return self.fail(&why),
                };
                self.notice = Some("opening the issue…".to_string());
                tokio::spawn(async move {
                    let opened =
                        share::open_issue(&gh, share::REPO, &report.issue_title(), &report.issue_body())
                            .await;
                    let _ = done.send(Internal::Sent(Sent::Issue(opened)));
                });
            }
            _ => {}
        }
    }

    /// Says what came of an upload or a report; a report that was written offers its issue.
    pub(super) fn on_sent(&mut self, sent: Sent) {
        self.notice = None;
        match sent {
            Sent::Gist(Ok(link)) => self.info(&format!(
                "shared: {link}\nIt is a secret gist: anyone who has the link can read it. `gh gist delete {link}` \
                 takes it down."
            )),
            Sent::Gist(Err(why)) => self.fail(&format!("not shared: {why}")),
            Sent::Report(Err(why)) => self.fail(&why),
            Sent::Report(Ok((report, file))) => {
                self.info(&report.written(&file));
                if share::gh().is_err() {
                    return self.info(&format!(
                        "To report it, open an issue at https://github.com/{}/issues",
                        share::REPO
                    ));
                }
                let mut body = vec![
                    format!("An issue on {} would say this, in public:", share::REPO),
                    String::new(),
                    format!("Title: {}", report.issue_title()),
                    String::new(),
                ];
                body.extend(report.issue_body().lines().map(str::to_string));
                let items = vec![
                    SelectItem::new("No", "the report stays where it is"),
                    SelectItem::new("Open the issue", "through gh, as the account it is signed in to"),
                ];
                self.deferred.push_back(Overlay::select(
                    "Open an issue?",
                    body,
                    items,
                    false,
                    Action::BugIssue(report),
                ));
            }
            Sent::Issue(Ok(link)) => self.info(&format!("issue opened: {link}")),
            Sent::Issue(Err(why)) => self.fail(&format!("no issue was opened: {why}")),
        }
    }
}
