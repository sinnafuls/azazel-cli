//! Ghost text in the input: the rest of the word being typed, from what the user wrote before,
//! and, in an empty input after a finished turn, a guess at the next message.
//!
//! The words come from the prompts of every project (their `history.jsonl`), the file names of
//! this project, the prompts sent in this run and what the agent wrote in it. They are read once
//! in the background and merged into the model when ready.

use std::path::Path;

use azazel_core::Paths;
use azazel_tui::components::vim::Mode;
use azazel_tui::components::words::WordModel;
use tokio_util::sync::CancellationToken;

use super::{Internal, Tui, project_files};

/// Most prompts read from the histories of all projects together.
const MAX_PROMPTS: usize = 20_000;
/// Most files of the project whose names are read.
const MAX_FILES: usize = 5_000;

/// The words of earlier prompts: those of this project first, then every other project's.
pub(super) fn load(paths: &Paths, root: Option<&Path>, ignored: bool) -> WordModel {
    let mut model = WordModel::new();
    let mut files = vec![crate::history::file(paths)];
    if let Ok(projects) = std::fs::read_dir(paths.home.join("projects")) {
        for project in projects.flatten() {
            let file = project.path().join("history.jsonl");
            if !files.contains(&file) {
                files.push(file);
            }
        }
    }
    let mut left = MAX_PROMPTS;
    for file in files {
        for record in crate::history::load(&file).iter().rev().take(left) {
            model.learn(&record.text, 1);
            left = left.saturating_sub(1);
        }
        if left == 0 {
            break;
        }
    }
    if let Some(root) = root {
        for file in project_files(root, ignored).iter().take(MAX_FILES) {
            model.learn_path(file);
        }
    }
    model
}

/// The request for the user's next message that is in flight, and the answer once it is in.
#[derive(Default)]
pub(super) struct Predictor {
    last: u64,
    current: Option<(u64, CancellationToken)>,
    shown: Option<String>,
}

impl Predictor {
    /// Starts a request: the id its answer carries back and the token that cancels it. A request
    /// still in flight is cancelled, and what was shown is gone.
    pub(super) fn begin(&mut self) -> (u64, CancellationToken) {
        self.cancel();
        self.last += 1;
        let cancel = CancellationToken::new();
        self.current = Some((self.last, cancel.clone()));
        (self.last, cancel)
    }

    /// Drops the request in flight and the prediction shown.
    pub(super) fn cancel(&mut self) {
        if let Some((_, cancel)) = self.current.take() {
            cancel.cancel();
        }
        self.shown = None;
    }

    /// What request `id` came back with. An answer to a request that was cancelled or replaced,
    /// or that arrives while the input has something in it, is discarded.
    pub(super) fn finish(&mut self, id: u64, answer: Result<String, String>, input_empty: bool) {
        if self.current.as_ref().is_none_or(|(current, _)| *current != id) {
            return;
        }
        self.current = None;
        self.shown = answer
            .ok()
            .and_then(|text| azazel_core::side::clean_prediction(&text))
            .filter(|_| input_empty);
    }

    pub(super) fn text(&self) -> Option<&str> {
        self.shown.as_deref()
    }
}

impl Tui {
    /// Reads the words of earlier prompts in the background; they join the model when ready.
    pub(super) fn load_words(&mut self) {
        if self.words_loaded || self.backend.local().is_none() {
            return;
        }
        self.words_loaded = true;
        let paths = self.env.paths.clone();
        let root = self.local_cwd();
        let ignored = self.env.config.tui.complete_ignored;
        let done = self.internal_tx.clone();
        tokio::task::spawn_blocking(move || {
            let _ = done.send(Internal::Words(load(&paths, root.as_deref(), ignored)));
        });
    }

    /// Sets the ghost text of the input for what is typed now: the rest of a word, or in an
    /// empty input the predicted next message. Vim's normal mode and an open completion menu
    /// get none.
    pub(super) fn sync_ghost(&mut self) {
        let words = self.env.config.tui.autocomplete == azazel_core::config::Autocomplete::Words;
        let inserting = self.vim.as_ref().is_none_or(|vim| vim.mode() == Mode::Insert);
        let ghost = if !inserting || self.completion.is_some() || self.overlay.is_some() {
            None
        } else if self.editor.is_empty() {
            self.predictor.text().map(str::to_string)
        } else if words {
            self.words.suggest(self.editor.text(), self.editor.cursor())
        } else {
            None
        };
        self.editor.set_ghost(ghost);
    }

    /// After a finished turn: asks the session's model what the user will say next.
    pub(super) fn predict_next(&mut self) {
        if !self.env.config.tui.predict_next || !self.editor.is_empty() {
            return;
        }
        let (id, cancel) = self.predictor.begin();
        if let Err(why) = self.backend.predict(id, self.internal_tx.clone(), cancel) {
            // Not an error worth the user's notice: the feature is a convenience.
            tracing::debug!("next-message prediction not asked: {why}");
            self.predictor.cancel();
        }
    }

    pub(super) fn on_prediction(&mut self, id: u64, answer: Result<String, String>) {
        let empty = self.editor.is_empty() && !self.busy;
        self.predictor.finish(id, answer, empty);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_shown_once_and_only_while_it_is_current() {
        let mut predictor = Predictor::default();
        let (first, first_cancel) = predictor.begin();
        // A newer request replaces it: the older one is cancelled and its answer ignored.
        let (second, _) = predictor.begin();
        assert!(first_cancel.is_cancelled());
        predictor.finish(first, Ok("stale".into()), true);
        assert_eq!(predictor.text(), None);
        predictor.finish(second, Ok("\"run the tests\"".into()), true);
        assert_eq!(predictor.text(), Some("run the tests"));
        // Typing cancels, and takes the shown text away.
        let (third, cancel) = predictor.begin();
        assert_eq!(predictor.text(), None);
        predictor.cancel();
        assert!(cancel.is_cancelled());
        predictor.finish(third, Ok("late".into()), true);
        assert_eq!(predictor.text(), None);
    }

    #[test]
    fn nothing_is_shown_for_no_prediction_a_failure_or_a_busy_input() {
        let mut predictor = Predictor::default();
        for (answer, empty) in [
            (Ok("NO_PREDICTION".to_string()), true),
            (Err("overloaded".to_string()), true),
            (Ok("commit it".to_string()), false),
        ] {
            let (id, _) = predictor.begin();
            predictor.finish(id, answer, empty);
            assert_eq!(predictor.text(), None);
        }
    }

    #[test]
    fn words_come_from_every_project_and_the_files_of_this_one() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("src")).unwrap();
        std::fs::write(project.path().join("src/Scheduler.rs"), "x").unwrap();
        for slug in ["here", "elsewhere"] {
            let dir = home.path().join("projects").join(slug);
            std::fs::create_dir_all(&dir).unwrap();
            let text = if slug == "here" { "migrate the database" } else { "benchmark the parser" };
            crate::history::append(&dir.join("history.jsonl"), "s1", text).unwrap();
        }
        let paths = Paths::discover(project.path(), home.path().to_str());
        let model = load(&paths, Some(project.path()), false);
        for (typed, rest) in [("migr", "ate"), ("bench", "mark"), ("sched", "uler")] {
            assert_eq!(model.suggest(typed, typed.len()).as_deref(), Some(rest), "{typed}");
        }
    }
}
