//! Triggers: schedules that run when something happens (`--on ci-failure`, `--on file:<glob>`).
//! A session that is open in the project watches for them: the interactive UI, or a background
//! session. Of several open sessions one owns each trigger (a lock file under `<home>/run/`),
//! so that what happens runs the prompt once.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use tokio::sync::mpsc;

use super::fire::{self, Fired};
use super::when::When;
use super::{Schedule, Scheduler, Target};
use crate::running::{self, Lock};

/// How often the watcher looks at what it owns.
const TICK: Duration = Duration::from_millis(500);
/// How often the files of a `file:` trigger are looked at. A change runs the prompt once no
/// further change was seen for this long.
const FILES_EVERY: Duration = Duration::from_secs(2);
/// How often the watcher looks for triggers nobody owns.
const CLAIM_EVERY: Duration = Duration::from_secs(2);
/// How long `gh` may take to list the runs.
const GH_LIMIT: Duration = Duration::from_secs(30);
/// The most changed files a prompt names.
const FILES_NAMED: usize = 20;
/// The CI runs looked at each time.
const RUNS_LISTED: &str = "20";

/// The file whoever watches trigger `id` holds locked.
pub fn lock_file(home: &Path, id: &str) -> PathBuf {
    running::run_dir(home).join(format!("trigger-{id}.lock"))
}

/// What the watcher tells the session it runs in.
#[derive(Debug)]
pub enum Event {
    /// A trigger happened. `text` is its prompt with what happened.
    Fire { schedule: Box<Schedule>, text: String },
    /// Something the user should know: a trigger that cannot be watched.
    Note(String),
}

/// What the watcher asks the session it runs in.
#[derive(Clone)]
pub struct Watched {
    /// The id of the session that is open now.
    pub session: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    /// Whether the lead is at work: files that change meanwhile are its own doing.
    pub busy: Arc<dyn Fn() -> bool + Send + Sync>,
}

/// A run of the project's CI, as `gh run list --json` gives it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CiRun {
    database_id: u64,
    status: String,
    conclusion: Option<String>,
    name: String,
    display_title: String,
    head_branch: String,
    event: String,
    url: String,
}

impl CiRun {
    fn failed(&self) -> bool {
        self.status == "completed"
            && matches!(
                self.conclusion.as_deref(),
                Some("failure" | "timed_out" | "startup_failure")
            )
    }
}

/// The prompt of a `ci-failure` trigger for one failed run.
fn ci_text(prompt: &str, run: &CiRun) -> String {
    let what = match (run.name.is_empty(), run.display_title.is_empty()) {
        (false, false) => format!("{}: {}", run.name, run.display_title),
        (false, true) => run.name.clone(),
        _ => run.display_title.clone(),
    };
    format!(
        "{prompt}\n\nA CI run failed: {what} (branch {}, {})\n{}\n`gh run view {} --log-failed` prints what failed.",
        run.head_branch, run.event, run.url, run.database_id
    )
}

/// The prompt of a `file:` trigger for the files that changed.
fn files_text(prompt: &str, changed: &BTreeSet<String>) -> String {
    let mut text = format!("{prompt}\n\nChanged files:");
    for file in changed.iter().take(FILES_NAMED) {
        text.push_str(&format!("\n- {file}"));
    }
    if changed.len() > FILES_NAMED {
        text.push_str(&format!("\n… and {} more", changed.len() - FILES_NAMED));
    }
    text
}

type Files = HashMap<String, (Option<SystemTime>, u64)>;

/// The files under `root` that match `glob`, with when each was changed and its size. Files
/// git ignores are left out.
fn scan(root: &Path, glob: &globset::GlobMatcher) -> Files {
    let mut walk = ignore::WalkBuilder::new(root);
    walk.hidden(false)
        .filter_entry(|entry| entry.file_name() != ".git" && entry.file_name() != ".azazel");
    walk.build()
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let relative = entry.path().strip_prefix(root).ok()?;
            let name = relative.to_string_lossy().replace('\\', "/");
            if !glob.is_match(&name) {
                return None;
            }
            let meta = entry.metadata().ok()?;
            Some((name, (meta.modified().ok(), meta.len())))
        })
        .collect()
}

/// The files that are new, changed or gone between two looks.
fn changed(before: &Files, after: &Files) -> BTreeSet<String> {
    let moved = after
        .iter()
        .filter(|(name, state)| before.get(*name) != Some(state))
        .map(|(name, _)| name.clone());
    let gone = before.keys().filter(|name| !after.contains_key(*name)).cloned();
    moved.chain(gone).collect()
}

/// The newest CI runs of the project in `root`.
async fn ci_runs(root: &Path) -> Result<Vec<CiRun>, String> {
    let gh = which::which("gh").map_err(|_| {
        "`gh` (the GitHub CLI) is not on PATH, which is how CI runs are looked at".to_string()
    })?;
    let mut command = tokio::process::Command::new(gh);
    command
        .args(["run", "list", "--limit", RUNS_LISTED, "--json"])
        .arg("databaseId,status,conclusion,name,displayTitle,headBranch,event,url")
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(GH_LIMIT, command.output())
        .await
        .map_err(|_| "`gh run list` took too long".to_string())?
        .map_err(|e| format!("cannot run `gh run list`: {e}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "`gh run list` failed: {}",
            said.lines().next().unwrap_or_default()
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("`gh run list` printed something unexpected: {e}"))
}

enum Watch {
    Files {
        glob: globset::GlobMatcher,
        seen: Option<Files>,
        /// What changed and has not run the prompt yet.
        pending: BTreeSet<String>,
        looked: Option<Instant>,
    },
    Ci {
        /// The runs that were there already; `None` until the first look.
        seen: Option<HashSet<u64>>,
        looked: Option<Instant>,
        /// The user has been told that the runs cannot be looked at.
        complained: bool,
    },
}

struct Owned {
    schedule: Schedule,
    watch: Watch,
    _lock: Lock,
}

struct Watcher {
    scheduler: Scheduler,
    about: Watched,
    events: mpsc::UnboundedSender<Event>,
    owned: Vec<Owned>,
    claimed: Option<Instant>,
}

impl Watcher {
    /// Whether this session is one the trigger may run in.
    fn eligible(&self, schedule: &Schedule) -> bool {
        match &schedule.target {
            Target::Session(id) => (self.about.session)().as_deref() == Some(id.as_str()),
            Target::New | Target::Open => true,
        }
    }

    /// Lets go of triggers that are gone, changed or no longer this session's, and takes those
    /// nobody owns.
    fn claim(&mut self) {
        let triggers: Vec<Schedule> = self
            .scheduler
            .store()
            .list()
            .into_iter()
            .filter(|schedule| schedule.when.is_trigger())
            .collect();
        let still = |owned: &Schedule| {
            triggers.iter().any(|schedule| {
                schedule.id == owned.id
                    && schedule.when == owned.when
                    && schedule.prompt == owned.prompt
                    && schedule.target == owned.target
            })
        };
        let kept: Vec<Owned> = std::mem::take(&mut self.owned)
            .into_iter()
            .filter(|owned| still(&owned.schedule) && self.eligible(&owned.schedule))
            .collect();
        self.owned = kept;
        let home = self.scheduler.paths.home.clone();
        for schedule in triggers {
            if self.owned.iter().any(|owned| owned.schedule.id == schedule.id) || !self.eligible(&schedule) {
                continue;
            }
            let watch = match &schedule.when {
                When::File(glob) => match globset::Glob::new(glob) {
                    Ok(glob) => Watch::Files {
                        glob: glob.compile_matcher(),
                        seen: None,
                        pending: BTreeSet::new(),
                        looked: None,
                    },
                    Err(_) => continue,
                },
                _ => Watch::Ci {
                    seen: None,
                    looked: None,
                    complained: false,
                },
            };
            if running::create_run_dir(&home).is_err() {
                return;
            }
            if let Ok(Some(lock)) = Lock::acquire(&lock_file(&home, &schedule.id)) {
                self.owned.push(Owned {
                    schedule,
                    watch,
                    _lock: lock,
                });
            }
        }
    }

    async fn look(&mut self) {
        let root = self.scheduler.paths.project_root.clone();
        let poll = Duration::from_secs(self.scheduler.config.poll_secs.max(1));
        let busy = (self.about.busy)();
        for owned in &mut self.owned {
            let due =
                |looked: &Option<Instant>, every: Duration| looked.is_none_or(|at| at.elapsed() >= every);
            match &mut owned.watch {
                Watch::Files {
                    glob,
                    seen,
                    pending,
                    looked,
                } => {
                    if !due(looked, FILES_EVERY) {
                        continue;
                    }
                    *looked = Some(Instant::now());
                    let (dir, matcher) = (root.clone(), glob.clone());
                    let Ok(now) = tokio::task::spawn_blocking(move || scan(&dir, &matcher)).await else {
                        continue;
                    };
                    let moved = match seen.as_ref() {
                        Some(before) => changed(before, &now),
                        // The first look is where things stand.
                        None => BTreeSet::new(),
                    };
                    *seen = Some(now);
                    // What changes while the lead works is the lead's own doing, or something
                    // it will see: running the prompt for it would go round in circles.
                    if busy {
                        pending.clear();
                        continue;
                    }
                    if !moved.is_empty() {
                        // Whoever is saving may not be done: wait for a look without changes.
                        pending.extend(moved);
                        continue;
                    }
                    if !pending.is_empty() {
                        let text = files_text(&owned.schedule.prompt, pending);
                        pending.clear();
                        let _ = self.events.send(Event::Fire {
                            schedule: Box::new(owned.schedule.clone()),
                            text,
                        });
                    }
                }
                Watch::Ci {
                    seen,
                    looked,
                    complained,
                } => {
                    if !due(looked, poll) {
                        continue;
                    }
                    *looked = Some(Instant::now());
                    let runs = match ci_runs(&root).await {
                        Ok(runs) => runs,
                        Err(why) => {
                            if !std::mem::replace(complained, true) {
                                let _ = self.events.send(Event::Note(format!(
                                    "the trigger {} (on ci-failure) cannot be watched: {why}",
                                    owned.schedule.id
                                )));
                            }
                            continue;
                        }
                    };
                    *complained = false;
                    let Some(known) = seen.as_mut() else {
                        // Runs that failed before anyone watched are not news; one that is
                        // still going is, once it has ended.
                        let ended = runs.iter().filter(|run| run.status == "completed");
                        *seen = Some(ended.map(|run| run.database_id).collect());
                        continue;
                    };
                    // Oldest first, as they happened. A run still going is looked at again.
                    for run in runs.iter().rev().filter(|run| run.status == "completed") {
                        if known.insert(run.database_id) && run.failed() {
                            let _ = self.events.send(Event::Fire {
                                schedule: Box::new(owned.schedule.clone()),
                                text: ci_text(&owned.schedule.prompt, run),
                            });
                        }
                    }
                }
            }
        }
    }

    async fn run(mut self) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while !self.events.is_closed() {
            tick.tick().await;
            if self.claimed.is_none_or(|at| at.elapsed() >= CLAIM_EVERY) {
                self.claimed = Some(Instant::now());
                self.claim();
            }
            self.look().await;
        }
    }
}

/// Watches the triggers of the project for as long as the events are listened to.
pub fn watch(scheduler: Scheduler, about: Watched) -> mpsc::UnboundedReceiver<Event> {
    let (events, listened) = mpsc::unbounded_channel();
    let watcher = Watcher {
        scheduler,
        about,
        events,
        owned: Vec::new(),
        claimed: None,
    };
    tokio::spawn(watcher.run());
    listened
}

/// Watches the triggers of the project of `app` for the session it has open. `None` when the
/// run has no schedules (they were not registered with it).
pub fn watch_app(app: &Arc<crate::app::App>) -> Option<(Arc<Scheduler>, mpsc::UnboundedReceiver<Event>)> {
    let scheduler = app.registry.services.get::<Scheduler>()?;
    let (open, services) = (app.clone(), app.registry.services.clone());
    let about = Watched {
        session: Arc::new(move || open.session().map(|session| session.id())),
        busy: Arc::new(move || {
            services
                .get::<azazel_team::Team>()
                .and_then(|team| team.lead())
                .is_some_and(|lead| lead.status().busy)
        }),
    };
    Some((scheduler.clone(), watch((*scheduler).clone(), about)))
}

/// Where a trigger that happened runs.
#[derive(Debug, PartialEq, Eq)]
pub enum Dispatch {
    /// In the session that saw it: the caller hands its lead the text.
    Here,
    /// Elsewhere, or not at all; the line says what was done.
    Done(String),
}

/// Takes a trigger that happened to where its schedule says, and notes the run with the
/// schedule.
pub async fn dispatch(scheduler: &Scheduler, schedule: &Schedule, text: &str) -> Dispatch {
    let at = jiff::Timestamp::now();
    match schedule.target {
        Target::Open | Target::Session(_) => {
            scheduler
                .store()
                .record(&schedule.id, at, "ran in the open session", None, true);
            Dispatch::Here
        }
        Target::New => {
            let fired = fire::deliver(scheduler, schedule, text).await;
            let result = fire::record(scheduler, schedule, at, &fired);
            Dispatch::Done(match fired {
                Ok(Fired::Started(host)) => format!(
                    "trigger {} ({}): started background session {host}",
                    schedule.id, schedule.when
                ),
                _ => format!("trigger {} ({}): {result}", schedule.id, schedule.when),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn files_that_are_new_changed_or_gone_count_as_changed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("src/a.rs"), "a").unwrap();
        std::fs::write(root.join("src/deep/b.rs"), "b").unwrap();
        std::fs::write(root.join("src/notes.md"), "n").unwrap();
        std::fs::write(root.join(".git/c.rs"), "c").unwrap();
        let glob = globset::Glob::new("src/**/*.rs").unwrap().compile_matcher();
        let before = scan(root, &glob);
        let mut names: Vec<&String> = before.keys().collect();
        names.sort();
        assert_eq!(names, ["src/a.rs", "src/deep/b.rs"]);
        assert!(changed(&before, &scan(root, &glob)).is_empty());

        std::fs::write(root.join("src/a.rs"), "a longer").unwrap();
        std::fs::write(root.join("src/new.rs"), "new").unwrap();
        std::fs::remove_file(root.join("src/deep/b.rs")).unwrap();
        std::fs::write(root.join("src/notes.md"), "not watched").unwrap();
        let moved: Vec<String> = changed(&before, &scan(root, &glob)).into_iter().collect();
        assert_eq!(moved, ["src/a.rs", "src/deep/b.rs", "src/new.rs"]);
    }

    #[test]
    fn the_prompt_says_what_happened() {
        let run: CiRun = serde_json::from_value(serde_json::json!({
            "databaseId": 77, "status": "completed", "conclusion": "failure", "name": "CI",
            "displayTitle": "Fix the parser", "headBranch": "main", "event": "push",
            "url": "https://github.com/o/r/actions/runs/77"
        }))
        .unwrap();
        assert!(run.failed());
        assert_eq!(
            ci_text("fix the build", &run),
            "fix the build\n\nA CI run failed: CI: Fix the parser (branch main, push)\n\
             https://github.com/o/r/actions/runs/77\n`gh run view 77 --log-failed` prints what failed."
        );
        let passed = CiRun {
            conclusion: Some("success".into()),
            ..run.clone()
        };
        let going = CiRun {
            status: "in_progress".into(),
            conclusion: None,
            ..run
        };
        assert!(!passed.failed() && !going.failed());

        let files: BTreeSet<String> = (0..23).map(|n| format!("src/f{n:02}.rs")).collect();
        let text = files_text("run the tests", &files);
        assert!(text.starts_with("run the tests\n\nChanged files:\n- src/f00.rs\n- src/f01.rs\n"));
        assert!(text.ends_with("- src/f19.rs\n… and 3 more"));
    }
}
