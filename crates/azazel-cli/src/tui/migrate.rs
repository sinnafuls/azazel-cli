//! `/migrate`: what other harnesses hold, ticked by source and by kind, shown as the plan of a
//! dry run, and then brought over.

use std::sync::Arc;

use azazel_core::{AskKind, Question};
use azazel_tui::{Glyph, SelectItem};

use super::form::Form;
use super::overlay::{Action, Overlay};
use super::pager::{self, Source as Shown, Start};
use super::{Internal, Tui};
use crate::migrate::{self, Env, Found, Kind, Options, Report, Status};

/// What was found, and the rows of the two lists it is chosen from.
pub(super) struct Offer {
    found: Vec<Found>,
    /// The rows of the sources, in the order of `found`.
    sources: Vec<String>,
    /// The kinds that have something to bring over, each with its row.
    kinds: Vec<(Kind, String)>,
}

/// What the user ticked.
pub(super) struct Pick {
    found: Vec<Found>,
    kinds: Vec<Kind>,
}

/// What came of work done beside the UI loop.
pub(super) enum Step {
    /// The sources were read.
    Found(Result<Vec<Found>, String>),
    /// What is being worked on, for the line above the input.
    Progress(String),
    /// The dry run of what was ticked.
    Plan(Box<Pick>, Report),
    Done(Report),
}

impl Offer {
    fn of(found: Vec<Found>) -> Offer {
        let sources = found
            .iter()
            .map(|found| format!("{}: {}", found.source.label(), migrate::counts(found)))
            .collect();
        let kinds = Kind::ALL
            .into_iter()
            .filter_map(|kind| {
                let total: usize = found.iter().map(|found| found.count(kind)).sum();
                (total > 0).then(|| (kind, format!("{} ({total}): {}", kind.name(), kind.about())))
            })
            .collect();
        Offer {
            found,
            sources,
            kinds,
        }
    }

    fn form(&self, other: &str) -> Form {
        let list = |header: &str, text: &str, options: Vec<String>| Question {
            header: Some(header.to_string()),
            kind: AskKind::Multiselect,
            options,
            ..Question::text(text)
        };
        let kinds = self.kinds.iter().map(|(_, row)| row.clone()).collect();
        Form::new(
            vec![
                list("From", "Bring over from which of these?", self.sources.clone()),
                list("What", "What of theirs?", kinds),
            ],
            other,
        )
        .all_picked()
    }

    /// The sources and kinds the answers of the form name.
    fn picked(self, answers: &[Vec<String>]) -> Pick {
        let ticked =
            |question: usize, row: &String| answers.get(question).is_some_and(|rows| rows.contains(row));
        let kinds = self
            .kinds
            .iter()
            .filter(|(_, row)| ticked(1, row))
            .map(|(kind, _)| *kind)
            .collect();
        let found = self
            .found
            .into_iter()
            .zip(&self.sources)
            .filter(|(_, row)| ticked(0, row))
            .map(|(found, _)| found)
            .collect();
        Pick { found, kinds }
    }
}

impl Tui {
    /// `/migrate`: looks for the other harnesses, then asks what to bring over.
    pub(super) fn migrate_command(&mut self) {
        let Some(env) = Env::detect() else {
            return self.fail("cannot find your home directory");
        };
        let project = self.env.paths.project_root.clone();
        let done = self.internal_tx.clone();
        self.notice = Some("looking for other harnesses…".to_string());
        tokio::task::spawn_blocking(move || {
            let found = migrate::scan(&env, &project, &migrate::Source::ALL);
            let found = match found.is_empty() {
                false => Ok(found),
                true => Err(migrate::Source::ALL
                    .iter()
                    .map(|source| {
                        format!(
                            "  {}: {}",
                            source.label(),
                            migrate::root_of(*source, &env).display()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")),
            };
            let _ = done.send(Internal::Migrate(Step::Found(found)));
        });
    }

    /// Runs a migration beside the UI loop: reading conversations takes as long as they are.
    fn migrate_run(&mut self, pick: Pick, dry_run: bool) {
        let paths = self.env.paths.clone();
        let done = self.internal_tx.clone();
        let progress = done.clone();
        let mut options = Options::new(pick.kinds.clone(), dry_run, false);
        options.progress = Arc::new(move |what| {
            let _ = progress.send(Internal::Migrate(Step::Progress(what.to_string())));
        });
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let report = runtime.block_on(migrate::migrate(&paths, &pick.found, &options));
            let step = match dry_run {
                true => Step::Plan(Box::new(pick), report),
                false => Step::Done(report),
            };
            let _ = done.send(Internal::Migrate(step));
        });
    }

    pub(super) async fn on_migrate(&mut self, step: Step) {
        match step {
            Step::Progress(what) => self.notice = Some(what),
            Step::Found(Err(looked)) => {
                self.notice = None;
                self.info(&format!("nothing to migrate. Looked in:\n{looked}"));
            }
            Step::Found(Ok(found)) => {
                self.notice = None;
                let offer = Offer::of(found);
                if offer.kinds.is_empty() {
                    return self
                        .info("the harnesses found hold nothing azazel does not already read where it is");
                }
                let form = offer.form(&format!("Other{}", self.theme.glyph(Glyph::Ellipsis)));
                self.overlay = Some(Overlay::form(form, Action::Migrate(Box::new(offer))));
            }
            Step::Plan(pick, report) => {
                self.notice = None;
                let mut body = vec![report.summary()];
                body.push(String::new());
                body.extend(report.render().lines().map(str::to_string));
                let items = vec![
                    SelectItem::new("Bring it over", "nothing that is already there is replaced"),
                    SelectItem::new("Cancel", ""),
                ];
                self.overlay = Some(
                    Overlay::select("Migrate: the plan", body, items, false, Action::MigrateGo(pick))
                        .starting_at(1),
                );
            }
            Step::Done(report) => {
                self.notice = None;
                let sources = vec![Shown::Text(report.render())];
                pager::show(self, "Migration report", sources, Start::Top).await;
                let failed = report.count(Status::Failed);
                let summary = format!(
                    "migrated: {}. Settings, skills and commands take effect after /reload or a restart.",
                    report.summary()
                );
                match failed {
                    0 => self.info(&summary),
                    _ => self.warn(&summary),
                }
            }
        }
    }

    /// The answers of the form: a dry run of what was ticked.
    pub(super) fn migrate_picked(&mut self, offer: Offer, answers: &[Vec<String>]) {
        let pick = offer.picked(answers);
        if pick.found.is_empty() || pick.kinds.is_empty() {
            return self.info("nothing was ticked; nothing is brought over");
        }
        self.notice = Some("working out what would be brought over…".to_string());
        self.migrate_run(pick, true);
    }

    /// The answer to the plan.
    pub(super) fn migrate_go(&mut self, pick: Pick, choice: usize) {
        if choice != 0 {
            return self.info("nothing was brought over");
        }
        self.notice = Some("bringing it over…".to_string());
        self.migrate_run(pick, false);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::migrate::Source;

    #[test]
    fn the_answers_of_the_form_name_sources_and_kinds() {
        let mut omp = Found::new(Source::Omp, Path::new("/omp"));
        omp.chats.push(crate::migrate::ChatRef {
            file: "/omp/a.jsonl".into(),
            id: "a".to_string(),
            cwd: None,
            title: None,
        });
        let claude = Found::new(Source::Claude, Path::new("/claude"));
        let offer = Offer::of(vec![omp, claude]);
        assert_eq!(
            offer.sources,
            ["omp: 1 chats", "Claude Code: nothing to bring over"]
        );
        // Only what some source has is offered.
        let kinds: Vec<Kind> = offer.kinds.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(kinds, [Kind::Chats]);

        let answers = vec![vec![offer.sources[0].clone()], vec![offer.kinds[0].1.clone()]];
        let pick = offer.picked(&answers);
        assert_eq!(pick.found.len(), 1);
        assert_eq!(pick.found[0].source, Source::Omp);
        assert_eq!(pick.kinds, [Kind::Chats]);
    }

    #[test]
    fn the_form_opens_with_everything_ticked_and_enter_takes_it() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        use super::super::form::Step;
        let mut omp = Found::new(Source::Omp, Path::new("/omp"));
        omp.memories.push(crate::migrate::MemoryItem {
            scope: crate::migrate::Scope::Home,
            kind: "fact",
            text: "something".to_string(),
        });
        let offer = Offer::of(vec![omp]);
        let mut form = offer.form("Other");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        // The sources, the kinds, then the tab that sends.
        assert!(matches!(form.on_key(&enter), Step::Pending));
        assert!(matches!(form.on_key(&enter), Step::Pending));
        let Step::Done(answers) = form.on_key(&enter) else {
            panic!("three presses of enter send the form");
        };
        assert_eq!(answers, [offer.sources.clone(), vec![offer.kinds[0].1.clone()]]);
        // Unticking a row leaves it out.
        let mut form = offer.form("Other");
        form.on_key(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        form.on_key(&KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        form.on_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        form.on_key(&enter);
        form.on_key(&enter);
        let Step::Done(answers) = form.on_key(&enter) else {
            panic!("the form is sent");
        };
        assert!(answers[0].is_empty(), "{answers:?}");
        let pick = offer.picked(&answers);
        assert!(pick.found.is_empty());
    }
}
