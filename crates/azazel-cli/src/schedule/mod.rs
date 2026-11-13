//! Schedules: prompts that run at a time (`azazel schedule add every 10m "…"`) or when something
//! happens (`--on ci-failure`, `--on file:<glob>`).
//!
//! They are kept per project in `<home>/projects/<project>/schedules.json`. A schedule that goes
//! by the clock is handed to the scheduler of the operating system ([`os`]), which starts
//! `azazel schedule run <id> --scheduled` when it may be due; that run starts a background
//! session with the prompt, or sends the prompt to the session the schedule names ([`fire`]).
//! A trigger is watched by a session that is open in the project ([`triggers`]).

pub mod fire;
pub mod os;
pub mod tool;
pub mod triggers;
pub mod when;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use azazel_core::config::ScheduleConfig;
use azazel_core::{Paths, SessionStore};
use jiff::{Timestamp, Zoned};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::cli::{Cli, ScheduleAction};
use crate::print::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};
use crate::running::Lock;
use when::When;

/// How long a change to the schedules waits for another process to finish its own.
const STORE_LIMIT: Duration = Duration::from_secs(10);
/// How much of a prompt a listing shows.
const PROMPT_SHOWN: usize = 48;

/// Where the prompt of a schedule goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A background session of its own, every time.
    New,
    /// This session: a background session that has it open is sent the prompt, otherwise one
    /// is started on it.
    Session(String),
    /// The session that sees the trigger happen.
    Open,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::New => f.write_str("new"),
            Target::Session(id) => write!(f, "session:{id}"),
            Target::Open => f.write_str("open"),
        }
    }
}

impl FromStr for Target {
    type Err = String;

    fn from_str(text: &str) -> Result<Target, String> {
        match text {
            "new" => Ok(Target::New),
            "open" => Ok(Target::Open),
            _ => text
                .strip_prefix("session:")
                .filter(|id| !id.is_empty())
                .map(|id| Target::Session(id.to_string()))
                .ok_or_else(|| format!("`{text}` is no target of a schedule")),
        }
    }
}

impl Serialize for Target {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Target, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Moments as RFC 3339 text in the file.
mod stamp {
    use jiff::Timestamp;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(at: &Timestamp, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(at)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Timestamp, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }

    pub mod optional {
        use super::*;

        pub fn serialize<S: Serializer>(at: &Option<Timestamp>, serializer: S) -> Result<S::Ok, S::Error> {
            match at {
                Some(at) => serializer.collect_str(at),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Timestamp>, D::Error> {
            Option::<String>::deserialize(deserializer)?
                .map(|text| text.parse().map_err(serde::de::Error::custom))
                .transpose()
        }
    }
}

/// One schedule of a project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub when: When,
    pub prompt: String,
    pub target: Target,
    /// The directory its runs work in.
    pub cwd: PathBuf,
    /// The options its runs are started with: model, mode, settings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(with = "stamp")]
    pub created: Timestamp,
    #[serde(default, with = "stamp::optional", skip_serializing_if = "Option::is_none")]
    pub last_run: Option<Timestamp>,
    /// What the last run came to: `started <id>`, `sent to <id>`, `skipped: <why>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_result: Option<String>,
    /// The background session the last run started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_host: Option<String>,
    #[serde(default)]
    pub runs: u32,
    /// The scheduler of the operating system has it.
    #[serde(default)]
    pub registered: bool,
}

impl Schedule {
    /// The prompt on one line, cut to fit a listing.
    fn prompt_shown(&self) -> String {
        let line = self.prompt.lines().next().unwrap_or_default();
        match line.char_indices().nth(PROMPT_SHOWN) {
            Some((cut, _)) => format!("{}…", &line[..cut]),
            None if self.prompt.lines().count() > 1 => format!("{line}…"),
            None => line.to_string(),
        }
    }

    fn next(&self, now: &Zoned) -> Option<Zoned> {
        self.when.next(now, self.last_run, self.created)
    }

    /// The schedule as one line of JSON, with its next run.
    fn row(&self, now: &Zoned) -> serde_json::Value {
        let mut row = serde_json::to_value(self).unwrap_or_default();
        row["next"] = json!(self.next(now).map(|next| next.timestamp().to_string()));
        row
    }
}

/// The schedules of one project, on disk.
pub struct Store {
    file: PathBuf,
}

impl Store {
    pub fn of(paths: &Paths) -> Store {
        Store {
            file: paths.project_data.join("schedules.json"),
        }
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Every schedule, oldest first; none when there is no file yet.
    fn read(&self) -> io::Result<Vec<Schedule>> {
        match std::fs::read(&self.file) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::from),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Every schedule, oldest first. A file that cannot be read holds none.
    pub fn list(&self) -> Vec<Schedule> {
        self.read().unwrap_or_else(|e| {
            tracing::warn!("cannot read {}: {e}", self.file.display());
            Vec::new()
        })
    }

    /// Changes the schedules with no other process changing them meanwhile.
    pub fn update<T>(&self, change: impl FnOnce(&mut Vec<Schedule>) -> T) -> io::Result<T> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let lock_file = self.file.with_extension("lock");
        let started = Instant::now();
        let _lock = loop {
            match Lock::acquire(&lock_file)? {
                Some(lock) => break lock,
                None if started.elapsed() > STORE_LIMIT => {
                    return Err(io::Error::other("another process is changing the schedules"));
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        // A file that cannot be read is left as it is rather than written over.
        let mut all = self.read()?;
        let out = change(&mut all);
        let staged = self.file.with_extension("json.tmp");
        std::fs::write(&staged, serde_json::to_vec_pretty(&all)?)?;
        std::fs::rename(&staged, &self.file)?;
        Ok(out)
    }

    /// The schedule `wanted` names: its id, or the start of it.
    pub fn find(&self, wanted: &str) -> Result<Schedule, String> {
        let mut matches: Vec<Schedule> = self
            .list()
            .into_iter()
            .filter(|schedule| schedule.id.starts_with(wanted))
            .collect();
        match matches.len() {
            0 => Err(format!("there is no schedule `{wanted}` in this project")),
            1 => Ok(matches.remove(0)),
            many => Err(format!(
                "`{wanted}` matches {many} schedules; give more of the id"
            )),
        }
    }

    /// Notes what a run of schedule `id` came to: `host` is the background session it went to,
    /// `ran` whether the prompt went anywhere.
    pub fn record(&self, id: &str, at: Timestamp, result: &str, host: Option<&str>, ran: bool) {
        let noted = self.update(|all| {
            if let Some(schedule) = all.iter_mut().find(|schedule| schedule.id == id) {
                schedule.last_run = Some(at);
                schedule.last_result = Some(result.to_string());
                if let Some(host) = host {
                    schedule.last_host = Some(host.to_string());
                }
                schedule.runs += u32::from(ran);
            }
        });
        if let Err(e) = noted {
            tracing::warn!("could not note the run of schedule {id}: {e}");
        }
    }
}

fn new_id(taken: &[Schedule]) -> String {
    let mut salt = 0u32;
    loop {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default();
        let digest = Sha256::digest(format!("schedule:{}:{nanos}:{salt}", std::process::id()));
        let id: String = digest.iter().take(4).map(|byte| format!("{byte:02x}")).collect();
        if !taken.iter().any(|schedule| schedule.id == id) {
            return id;
        }
        salt += 1;
    }
}

/// `--home <dir>` when azazel's own files are not where they are by default: what another
/// process is started with (by the scheduler, or as a background session) to find the same.
pub fn home_args(cli: &Cli, paths: &Paths) -> Vec<String> {
    let elsewhere =
        cli.home.is_some() || std::env::var_os("AZAZEL_HOME").is_some_and(|home| !home.is_empty());
    match elsewhere {
        true => vec!["--home".to_string(), paths.home.to_string_lossy().into_owned()],
        false => Vec::new(),
    }
}

/// What the runs of a schedule added from this command line are started with.
fn run_args(cli: &Cli) -> Vec<String> {
    let mut args = crate::host_carried(cli);
    for spec in &cli.model {
        args.extend(["--model".to_string(), spec.clone()]);
    }
    if let Some(level) = &cli.thinking {
        args.extend(["--thinking".to_string(), level.clone()]);
    }
    if let Some(mode) = &cli.mode {
        args.extend(["--mode".to_string(), mode.clone()]);
    }
    args
}

/// What is needed to add, remove and run schedules of the project.
#[derive(Debug, Clone)]
pub struct Scheduler {
    pub paths: Paths,
    pub config: ScheduleConfig,
    /// See [`home_args`].
    pub home_args: Vec<String>,
}

impl Scheduler {
    pub fn new(cli: &Cli, paths: &Paths, config: ScheduleConfig) -> Scheduler {
        Scheduler {
            paths: paths.clone(),
            config,
            home_args: home_args(cli, paths),
        }
    }

    pub fn store(&self) -> Store {
        Store::of(&self.paths)
    }

    fn job(&self, schedule: &Schedule) -> Result<os::Job, String> {
        let zone = jiff::tz::TimeZone::system();
        let mut args = self.home_args.clone();
        args.extend(["--cwd".to_string(), schedule.cwd.to_string_lossy().into_owned()]);
        args.extend(["schedule", "run", &schedule.id, "--scheduled"].map(String::from));
        Ok(os::Job {
            id: schedule.id.clone(),
            when: schedule.when.clone(),
            anchor: schedule.created.to_zoned(zone.clone()).datetime(),
            zone,
            program: std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?,
            args,
            wrapper: os::headless_wrapper(),
            about: format!(
                "azazel schedule {} ({}) in {}: {}",
                schedule.id,
                schedule.when,
                schedule.cwd.display(),
                schedule.prompt_shown()
            ),
        })
    }

    /// Adds a schedule: one that goes by the clock is handed to the system's scheduler first,
    /// and is not kept when that fails.
    pub fn add(
        &self,
        when: When,
        prompt: &str,
        target: Target,
        args: Vec<String>,
    ) -> Result<Schedule, String> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err("a schedule needs a prompt: what to do when it runs".to_string());
        }
        if target == Target::Open && !when.is_trigger() {
            return Err(
                "a schedule that goes by the clock runs in a new session, or in one named with --session"
                    .to_string(),
            );
        }
        let store = self.store();
        let mut schedule = Schedule {
            id: new_id(&store.list()),
            when,
            prompt: prompt.to_string(),
            target,
            cwd: self.paths.cwd.clone(),
            args,
            created: Timestamp::now(),
            last_run: None,
            last_result: None,
            last_host: None,
            runs: 0,
            registered: false,
        };
        if !schedule.when.is_trigger() && self.config.register {
            os::register(&self.job(&schedule)?).map_err(|e| {
                format!(
                    "{e}\nNothing was added. With `--set schedule.register=false` the schedule is kept \
                     without the system's scheduler, and runs when `azazel schedule run --due` is called."
                )
            })?;
            schedule.registered = true;
        }
        let kept = schedule.clone();
        match store.update(|all| all.push(kept)) {
            Ok(()) => Ok(schedule),
            Err(e) => {
                if schedule.registered {
                    let _ = os::unregister(&schedule.id);
                }
                Err(format!("cannot write {}: {e}", store.file().display()))
            }
        }
    }

    /// Removes the schedule `wanted` names, from the system's scheduler too.
    pub fn remove(&self, wanted: &str) -> Result<Schedule, String> {
        let store = self.store();
        let schedule = store.find(wanted)?;
        if schedule.registered {
            os::unregister(&schedule.id)?;
        }
        store
            .update(|all| all.retain(|kept| kept.id != schedule.id))
            .map_err(|e| format!("cannot write {}: {e}", store.file().display()))?;
        // Nothing owns its trigger any more.
        let _ = std::fs::remove_file(triggers::lock_file(&self.paths.home, &schedule.id));
        Ok(schedule)
    }

    /// One line about a schedule that was just added.
    pub fn added_line(&self, schedule: &Schedule) -> String {
        let now = Zoned::now();
        let goes = match (&schedule.target, schedule.when.is_trigger()) {
            (Target::New, _) => "starts a background session".to_string(),
            (Target::Session(id), _) => format!("goes to session {id}"),
            (Target::Open, _) => {
                "goes to the session that is open in this project when it happens".to_string()
            }
        };
        let how = match (schedule.when.is_trigger(), schedule.registered) {
            (true, _) => "watched while a session is open here".to_string(),
            (false, true) => format!(
                "next run {}, started by {}",
                schedule
                    .next(&now)
                    .map_or_else(|| "never".to_string(), |next| when::shown(&next)),
                os::Kind::HERE.name()
            ),
            (false, false) => {
                "not handed to the system's scheduler: `azazel schedule run --due` runs it".to_string()
            }
        };
        format!("schedule {} ({}): {goes}; {how}", schedule.id, schedule.when)
    }
}

/// The schedules as a table, for `azazel schedule list` and the `schedule` tool.
pub fn table(schedules: &[Schedule], now: &Zoned) -> String {
    if schedules.is_empty() {
        return "this project has no schedule".to_string();
    }
    let shown = |at: Option<Zoned>| at.map_or_else(|| "-".to_string(), |at| when::shown(&at));
    let rows: Vec<[String; 7]> = schedules
        .iter()
        .map(|schedule| {
            [
                schedule.id.clone(),
                match &schedule.when {
                    When::At(at) => format!("at {}", when::shown(&at.to_zoned(now.time_zone().clone()))),
                    when => when.to_string(),
                },
                match (&schedule.when, schedule.next(now)) {
                    (when, _) if when.is_trigger() => "when it happens".to_string(),
                    (_, next) => shown(next),
                },
                match (&schedule.last_run, &schedule.last_result) {
                    (Some(at), Some(result)) => {
                        format!("{} {result}", when::shown(&at.to_zoned(now.time_zone().clone())))
                    }
                    (None, Some(result)) => result.clone(),
                    _ => "-".to_string(),
                },
                schedule.runs.to_string(),
                schedule.target.to_string(),
                schedule.prompt_shown(),
            ]
        })
        .collect();
    let heads = ["ID", "WHEN", "NEXT", "LAST", "RUNS", "TARGET", "PROMPT"];
    let widths: Vec<usize> = (0..heads.len())
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .chain([heads[column].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: &[String]| {
        let cells: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        cells.join("  ").trim_end().to_string()
    };
    let mut out = vec![line(&heads.map(str::to_string))];
    out.extend(rows.iter().map(|row| line(row)));
    let unregistered: Vec<&str> = schedules
        .iter()
        .filter(|schedule| !schedule.when.is_trigger() && !schedule.registered)
        .map(|schedule| schedule.id.as_str())
        .collect();
    if !unregistered.is_empty() {
        out.push(format!(
            "not handed to the system's scheduler: {}; `azazel schedule run --due` runs what is due",
            unregistered.join(", ")
        ));
    }
    out.join("\n")
}

/// The session `wanted` names in this project: its id in full.
pub fn session_id(paths: &Paths, wanted: &str) -> Result<String, String> {
    let matches: Vec<String> = SessionStore::list(&paths.sessions_dir())
        .into_iter()
        .filter(|session| session.id.starts_with(wanted))
        .map(|session| session.id)
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("no session with id `{wanted}` in this project")),
        many => Err(format!(
            "`{wanted}` matches {} sessions; give more of the id",
            many.len()
        )),
    }
}

/// `azazel schedule …`.
pub async fn run(cli: &Cli, paths: &Paths, config: ScheduleConfig, action: &ScheduleAction) -> u8 {
    let scheduler = Scheduler::new(cli, paths, config);
    let outcome = match action {
        ScheduleAction::Add {
            on,
            session,
            new,
            spec,
        } => add(cli, &scheduler, on.as_deref(), session.as_deref(), *new, spec),
        ScheduleAction::List { json } => {
            let now = Zoned::now();
            let schedules = scheduler.store().list();
            match json {
                true => schedules
                    .iter()
                    .for_each(|schedule| println!("{}", schedule.row(&now))),
                false => println!("{}", table(&schedules, &now)),
            }
            Ok(())
        }
        ScheduleAction::Rm { id } => scheduler
            .remove(id)
            .map(|schedule| println!("removed {}", schedule.id))
            .map_err(Problem::Failed),
        ScheduleAction::Run { id, due, scheduled } => {
            return fire::command(&scheduler, id.as_deref(), *due, *scheduled).await;
        }
    };
    match outcome {
        Ok(()) => EXIT_OK,
        Err(Problem::Usage(why)) => {
            eprintln!("azazel: {why}");
            EXIT_USAGE
        }
        Err(Problem::Failed(why)) => {
            eprintln!("azazel: {why}");
            EXIT_FAILED
        }
    }
}

enum Problem {
    Usage(String),
    Failed(String),
}

impl From<String> for Problem {
    fn from(why: String) -> Problem {
        Problem::Failed(why)
    }
}

fn add(
    cli: &Cli,
    scheduler: &Scheduler,
    on: Option<&str>,
    session: Option<&str>,
    new: bool,
    spec: &[String],
) -> Result<(), Problem> {
    let usage = "azazel schedule add \"0 9 * * 1-5\" \"<prompt>\", add every 10m \"<prompt>\", add at +2h \
                 \"<prompt>\", or add --on ci-failure|file:<glob> \"<prompt>\"";
    let Some((prompt, time)) = spec.split_last() else {
        return Err(Problem::Usage(usage.to_string()));
    };
    let now = Zoned::now();
    let when = match (on, time.is_empty()) {
        (Some(event), true) => When::parse(&format!("on {event}"), &now),
        (Some(_), false) => Err(format!("a schedule runs at a time or on --on, not both; {usage}")),
        (None, true) => Err(format!("when is it to run? {usage}")),
        (None, false) => When::parse(&time.join(" "), &now),
    }
    .map_err(Problem::Usage)?;
    let target = match (session, new, when.is_trigger()) {
        (Some(wanted), _, _) => {
            Target::Session(session_id(&scheduler.paths, wanted).map_err(Problem::Usage)?)
        }
        (None, false, true) => Target::Open,
        (None, _, _) => Target::New,
    };
    let schedule = scheduler.add(when, prompt, target, run_args(cli))?;
    println!("{}", schedule.id);
    eprintln!("{}", scheduler.added_line(&schedule));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn scheduler(dir: &Path) -> Scheduler {
        let home = dir.join("home");
        let project = dir.join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let paths = Paths::discover(&project, Some(&home.to_string_lossy()));
        Scheduler {
            paths,
            config: ScheduleConfig {
                register: false,
                ..ScheduleConfig::default()
            },
            home_args: Vec::new(),
        }
    }

    #[test]
    fn schedules_are_kept_per_project_and_found_by_the_start_of_their_id() {
        let dir = tempfile::tempdir().unwrap();
        let scheduler = scheduler(dir.path());
        let now = Zoned::now();
        let every = scheduler
            .add(
                When::parse("every 10m", &now).unwrap(),
                "  check the deps\n",
                Target::New,
                vec!["--mode".into(), "auto".into()],
            )
            .unwrap();
        let trigger = scheduler
            .add(When::CiFailure, "fix the build", Target::Open, Vec::new())
            .unwrap();
        assert_eq!(every.prompt, "check the deps");
        assert!(!every.registered && every.id.len() == 8 && every.id != trigger.id);

        let store = scheduler.store();
        assert_eq!(store.list(), [every.clone(), trigger.clone()]);
        assert_eq!(store.find(&every.id[..4]), Ok(every.clone()));
        assert!(store.find("zz").unwrap_err().contains("no schedule `zz`"));
        // What a run came to is kept with it.
        let at: Timestamp = "2026-11-13T10:00:00Z".parse().unwrap();
        store.record(&every.id, at, "started 1a2b3c4d", Some("1a2b3c4d"), true);
        store.record(
            &every.id,
            at,
            "skipped: the run before it is still working",
            None,
            false,
        );
        let noted = store.find(&every.id).unwrap();
        assert_eq!((noted.runs, noted.last_run), (1, Some(at)));
        assert_eq!(noted.last_host.as_deref(), Some("1a2b3c4d"));
        assert_eq!(
            noted.last_result.as_deref(),
            Some("skipped: the run before it is still working")
        );

        assert_eq!(scheduler.remove(&every.id).unwrap().id, every.id);
        assert_eq!(store.list(), [trigger]);
        // Another project has its own.
        let other = Paths::discover(dir.path(), Some(&dir.path().join("home").to_string_lossy()));
        assert_eq!(Store::of(&other).list(), []);
    }

    #[test]
    fn a_file_that_cannot_be_read_is_not_written_over() {
        let dir = tempfile::tempdir().unwrap();
        let scheduler = scheduler(dir.path());
        let store = scheduler.store();
        std::fs::create_dir_all(store.file().parent().unwrap()).unwrap();
        std::fs::write(store.file(), "[{\"id\": \"from a later version\"}]").unwrap();
        assert_eq!(store.list(), []);
        let refused = scheduler.add(
            When::Every(Duration::from_secs(600)),
            "go",
            Target::New,
            Vec::new(),
        );
        assert!(refused.unwrap_err().starts_with("cannot write "));
        assert_eq!(
            std::fs::read_to_string(store.file()).unwrap(),
            "[{\"id\": \"from a later version\"}]"
        );
    }

    #[test]
    fn what_makes_no_schedule_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let scheduler = scheduler(dir.path());
        let every = When::Every(Duration::from_secs(600));
        assert!(
            scheduler
                .add(every.clone(), "  ", Target::New, Vec::new())
                .unwrap_err()
                .contains("needs a prompt")
        );
        assert!(
            scheduler
                .add(every, "go", Target::Open, Vec::new())
                .unwrap_err()
                .contains("goes by the clock")
        );
        assert_eq!(scheduler.store().list(), []);
    }

    #[test]
    fn a_listing_says_when_each_schedule_runs_next_and_what_it_last_did() {
        let now: Zoned = "2026-11-13T10:00:00+00:00[UTC]".parse().unwrap();
        let schedule = |id: &str, when: &str, target: Target, prompt: &str| Schedule {
            id: id.to_string(),
            when: When::parse(when, &now).unwrap(),
            prompt: prompt.to_string(),
            target,
            cwd: PathBuf::from("/work"),
            args: Vec::new(),
            created: "2026-11-13T09:30:00Z".parse().unwrap(),
            last_run: None,
            last_result: None,
            last_host: None,
            runs: 0,
            registered: true,
        };
        let mut daily = schedule("aa11aa11", "0 9 * * *", Target::New, "check the deps\nand report");
        daily.last_run = Some("2026-11-13T09:00:02Z".parse().unwrap());
        daily.last_result = Some("started 1a2b3c4d".to_string());
        daily.runs = 3;
        let mut every = schedule(
            "bb22bb22",
            "every 45m",
            Target::Session("0190aaaa".into()),
            "look at the queue",
        );
        every.registered = false;
        let trigger = schedule("cc33cc33", "on file:src/**/*.rs", Target::Open, "run the tests");
        assert_eq!(
            table(&[daily, every, trigger], &now),
            "ID        WHEN                 NEXT              LAST                               RUNS  TARGET            PROMPT\n\
             aa11aa11  0 9 * * *            2026-11-14 09:00  2026-11-13 09:00 started 1a2b3c4d  3     new               check the deps…\n\
             bb22bb22  every 45m            2026-11-13 10:15  -                                  0     session:0190aaaa  look at the queue\n\
             cc33cc33  on file:src/**/*.rs  when it happens   -                                  0     open              run the tests\n\
             not handed to the system's scheduler: bb22bb22; `azazel schedule run --due` runs what is due"
        );
        assert_eq!(table(&[], &now), "this project has no schedule");
    }
}
