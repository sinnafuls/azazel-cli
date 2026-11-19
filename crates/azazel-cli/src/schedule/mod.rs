//! Schedules: prompts that run at a time (`azazel schedule add every 10m "…"`) or when something
//! happens (`--on ci-failure`, `--on pr-comment`, `--on slack:<channel>`, `--on file:<glob>`).
//!
//! They are kept per project in `<home>/projects/<project>/schedules.json`. A schedule that goes
//! by the clock is handed to the scheduler of the operating system ([`os`]), which starts
//! `azazel schedule run <id> --scheduled` when it may be due; that run starts a background
//! session with the prompt, or sends the prompt to the session the schedule names ([`fire`]).
//! A trigger is watched by a session that is open in the project ([`triggers`]). At each due
//! time the schedule's checks decide, before any model is asked, whether its prompt runs, and its
//! end whether it stops ([`check`]).

pub mod check;
pub mod fire;
pub mod inbox;
pub mod os;
pub mod tool;
pub mod triggers;
pub mod when;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use azazel_core::config::{Config, ScheduleConfig, TriggersConfig};
use azazel_core::process::Shell;
use azazel_core::{Paths, SessionStore};
use jiff::{Timestamp, Zoned};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::cli::{Cli, ScheduleAction, ScheduleLimits};
use crate::print::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};
use crate::running::Lock;
use check::{Check, Ends};
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

/// Whether a schedule runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    #[default]
    Active,
    /// Paused by hand: it does not run until it is resumed.
    Paused,
    /// Stopped by itself: its `until` check held, its end time passed, or it ran as many times as
    /// it was to. It stays in the list, with why, until it is removed.
    Ended,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            State::Active => "active",
            State::Paused => "paused",
            State::Ended => "ended",
        })
    }
}

/// What a schedule has besides its time and prompt: its checks, its end and its time zone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub if_check: Option<Check>,
    pub until: Option<Check>,
    pub ends: Option<Ends>,
    /// The zone `when` was read in and a cron expression goes by.
    pub tz: Option<String>,
}

/// The time now in the zone `tz` names, or in the local one.
pub fn now_in(tz: Option<&str>) -> Result<Zoned, String> {
    let now = Zoned::now();
    match tz.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => Ok(now.with_time_zone(when::zone(name)?)),
        None => Ok(now),
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
    /// Its prompt runs only when this check holds.
    #[serde(default, rename = "if", skip_serializing_if = "Option::is_none")]
    pub if_check: Option<Check>,
    /// It stops once this check holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Check>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends: Option<Ends>,
    /// The zone its cron expression goes by; the local one when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
    #[serde(default)]
    pub state: State,
    /// What its checks said the last time they ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_gate: Option<String>,
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

    /// `now` in the zone the schedule goes by.
    fn zoned(&self, now: &Zoned) -> Zoned {
        match self.tz.as_deref().map(when::zone) {
            Some(Ok(zone)) => now.with_time_zone(zone),
            _ => now.clone(),
        }
    }

    /// Whether a run the scheduler wakes at `now` is to go ahead, as far as the time goes.
    pub fn due(&self, now: &Zoned) -> when::Due {
        self.when.due(&self.zoned(now), self.last_run, self.created)
    }

    /// When it runs next, in the zone of `now`: never while it is paused or has ended, nor past
    /// its end.
    fn next(&self, now: &Zoned) -> Option<Zoned> {
        if self.state != State::Active {
            return None;
        }
        let next = self.when.next(&self.zoned(now), self.last_run, self.created)?;
        let end = self.ends.as_ref().and_then(|ends| ends.at);
        if end.is_some_and(|end| next.timestamp() > end) {
            return None;
        }
        Some(next.with_time_zone(now.time_zone().clone()))
    }

    /// Whether it has checks that run before its prompt.
    fn gated(&self) -> bool {
        self.if_check.is_some() || self.until.is_some()
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
    /// Who may start the runs of `on pr-comment` and `on slack:<channel>`, and how they work.
    pub triggers: TriggersConfig,
    /// See [`home_args`].
    pub home_args: Vec<String>,
    /// What runs the checks: the shell the agents' commands run in.
    pub shell: Shell,
}

impl Scheduler {
    pub fn new(cli: &Cli, paths: &Paths, config: &Config) -> Scheduler {
        Scheduler {
            paths: paths.clone(),
            config: config.schedule.clone(),
            triggers: config.triggers.clone(),
            home_args: home_args(cli, paths),
            shell: Shell::resolve(&config.tools.shell),
        }
    }

    pub fn store(&self) -> Store {
        Store::of(&self.paths)
    }

    fn job(&self, schedule: &Schedule) -> Result<os::Job, String> {
        let zone = jiff::tz::TimeZone::system();
        // The scheduler keeps local time: a cron expression of another zone wakes the run at its
        // times as they are here, and the run decides in the expression's own zone.
        let wake = match (&schedule.when, schedule.tz.as_deref()) {
            (When::Cron(cron), Some(name)) => {
                let theirs = when::zone(name)?;
                let now = Timestamp::now();
                let apart = zone.to_offset(now).seconds() - theirs.to_offset(now).seconds();
                match theirs.iana_name().is_some() && theirs.iana_name() == zone.iana_name() {
                    true => schedule.when.clone(),
                    false => When::Cron(cron.shifted(apart / 60)),
                }
            }
            (when, _) => when.clone(),
        };
        let mut args = self.home_args.clone();
        args.extend(["--cwd".to_string(), schedule.cwd.to_string_lossy().into_owned()]);
        args.extend(["schedule", "run", &schedule.id, "--scheduled"].map(String::from));
        Ok(os::Job {
            id: schedule.id.clone(),
            when: wake,
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
        options: Options,
    ) -> Result<Schedule, String> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err("a schedule needs a prompt: what to do when it runs".to_string());
        }
        if options.tz.is_some() && !matches!(when, When::Cron(_) | When::At(_)) {
            return Err("a time zone is for a cron expression or `at`".to_string());
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
            if_check: options.if_check,
            until: options.until,
            ends: options.ends,
            tz: options.tz,
            state: State::Active,
            last_gate: None,
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

    /// Pauses the schedule `wanted` names: it does not run, and is out of the system's scheduler,
    /// until it is resumed.
    pub fn pause(&self, wanted: &str) -> Result<Schedule, String> {
        let store = self.store();
        let schedule = store.find(wanted)?;
        match schedule.state {
            State::Active => {}
            State::Paused => return Err(format!("schedule {} is paused already", schedule.id)),
            State::Ended => return Err(ended_already(&schedule)),
        }
        if schedule.registered {
            os::unregister(&schedule.id)?;
        }
        self.set(&schedule.id, |kept| {
            kept.state = State::Paused;
            kept.registered = false;
        })
    }

    /// Lets a paused schedule run again, handing it back to the system's scheduler.
    pub fn resume(&self, wanted: &str) -> Result<Schedule, String> {
        let store = self.store();
        let schedule = store.find(wanted)?;
        match schedule.state {
            State::Paused => {}
            State::Active => return Err(format!("schedule {} is not paused", schedule.id)),
            State::Ended => return Err(ended_already(&schedule)),
        }
        let register = !schedule.when.is_trigger() && self.config.register;
        if register {
            os::register(&self.job(&schedule)?)?;
        }
        self.set(&schedule.id, |kept| {
            kept.state = State::Active;
            kept.registered = register;
        })
    }

    /// Stops the schedule for good, saying why; it stays in the list until it is removed.
    pub fn end(&self, schedule: &Schedule, why: &str, at: Timestamp) {
        if schedule.registered
            && let Err(e) = os::unregister(&schedule.id)
        {
            tracing::warn!(
                "could not take schedule {} out of the system's scheduler: {e}",
                schedule.id
            );
        }
        let ended = self.set(&schedule.id, |kept| {
            kept.state = State::Ended;
            kept.registered = false;
            kept.last_run = Some(at);
            kept.last_result = Some(why.to_string());
        });
        if let Err(e) = ended {
            tracing::warn!("could not note the end of schedule {}: {e}", schedule.id);
        }
    }

    /// Changes the schedule `id` and returns it as it is then.
    fn set(&self, id: &str, change: impl FnOnce(&mut Schedule)) -> Result<Schedule, String> {
        let store = self.store();
        store
            .update(|all| {
                let kept = all.iter_mut().find(|kept| kept.id == id)?;
                change(kept);
                Some(kept.clone())
            })
            .map_err(|e| format!("cannot write {}: {e}", store.file().display()))?
            .ok_or_else(|| format!("there is no schedule `{id}` in this project"))
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

fn ended_already(schedule: &Schedule) -> String {
    format!(
        "schedule {} has ended ({}); add it again to run it again",
        schedule.id,
        schedule.last_result.as_deref().unwrap_or("ended")
    )
}

/// The schedules as a table, for `azazel schedule list` and the `schedule` tool.
pub fn table(schedules: &[Schedule], now: &Zoned) -> String {
    if schedules.is_empty() {
        return "this project has no schedule".to_string();
    }
    let shown = |at: Option<Zoned>| at.map_or_else(|| "-".to_string(), |at| when::shown(&at));
    let rows: Vec<[String; 9]> = schedules
        .iter()
        .map(|schedule| {
            [
                schedule.id.clone(),
                match (&schedule.when, &schedule.tz) {
                    (When::At(at), _) => format!("at {}", when::shown(&at.to_zoned(now.time_zone().clone()))),
                    (when, Some(tz)) => format!("{when} ({tz})"),
                    (when, None) => when.to_string(),
                },
                schedule.state.to_string(),
                match (&schedule.when, schedule.next(now)) {
                    (when, _) if when.is_trigger() && schedule.state == State::Active => {
                        "when it happens".to_string()
                    }
                    (_, next) => shown(next),
                },
                match (&schedule.last_run, &schedule.last_result) {
                    (Some(at), Some(result)) => {
                        format!("{} {result}", when::shown(&at.to_zoned(now.time_zone().clone())))
                    }
                    (None, Some(result)) => result.clone(),
                    _ => "-".to_string(),
                },
                schedule
                    .last_gate
                    .clone()
                    .unwrap_or_else(|| match schedule.gated() {
                        true => "not run yet".to_string(),
                        false => "-".to_string(),
                    }),
                schedule.runs.to_string(),
                schedule.target.to_string(),
                schedule.prompt_shown(),
            ]
        })
        .collect();
    let heads = [
        "ID", "WHEN", "STATE", "NEXT", "LAST", "CHECKS", "RUNS", "TARGET", "PROMPT",
    ];
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
        .filter(|schedule| {
            !schedule.when.is_trigger() && !schedule.registered && schedule.state == State::Active
        })
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
    let matches: Vec<String> = SessionStore::list_all(&paths.sessions_dir())
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
pub async fn run(cli: &Cli, paths: &Paths, config: &Config, action: &ScheduleAction) -> u8 {
    let scheduler = Scheduler::new(cli, paths, config);
    let outcome = match action {
        ScheduleAction::Add {
            on,
            session,
            new,
            spec,
            limits,
        } => add(
            cli,
            &scheduler,
            on.as_deref(),
            session.as_deref(),
            *new,
            spec,
            limits,
        ),
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
        ScheduleAction::Pause { id } => scheduler
            .pause(id)
            .map(|schedule| println!("paused {}", schedule.id))
            .map_err(Problem::Failed),
        ScheduleAction::Resume { id } => scheduler
            .resume(id)
            .map(|schedule| {
                println!("resumed {}", schedule.id);
                eprintln!("{}", scheduler.added_line(&schedule));
            })
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

/// The checks, end and zone `azazel schedule add` was given.
fn options(limits: &ScheduleLimits) -> Result<Options, String> {
    let check = |run: &Option<String>, is, matches: &Option<String>, changes| {
        run.as_deref()
            .map(|run| Check::new(run, is, matches.as_deref(), changes))
            .transpose()
    };
    let now = now_in(limits.tz.as_deref())?;
    Ok(Options {
        if_check: check(
            &limits.if_run,
            limits.if_is,
            &limits.if_matches,
            limits.if_changes,
        )?,
        until: check(
            &limits.until_run,
            limits.until_is,
            &limits.until_matches,
            limits.until_changes,
        )?,
        ends: Ends::parse(
            limits.ends_after.as_deref(),
            limits.ends_at.as_deref(),
            limits.times,
            &now,
        )?,
        tz: limits.tz.as_deref().map(str::trim).map(str::to_string),
    })
}

fn add(
    cli: &Cli,
    scheduler: &Scheduler,
    on: Option<&str>,
    session: Option<&str>,
    new: bool,
    spec: &[String],
    limits: &ScheduleLimits,
) -> Result<(), Problem> {
    let usage = "azazel schedule add \"0 9 * * 1-5\" \"<prompt>\", add every 10m \"<prompt>\", add at +2h \
                 \"<prompt>\", or add --on ci-failure|pr-comment|slack:<channel>|file:<glob> \"<prompt>\"";
    let Some((prompt, time)) = spec.split_last() else {
        return Err(Problem::Usage(usage.to_string()));
    };
    let options = options(limits).map_err(Problem::Usage)?;
    let now = now_in(options.tz.as_deref()).map_err(Problem::Usage)?;
    let when = match (on, time.is_empty()) {
        (Some(event), true) => When::parse(&format!("on {event}"), &now),
        (Some(_), false) => Err(format!("a schedule runs at a time or on --on, not both; {usage}")),
        (None, true) => Err(format!("when is it to run? {usage}")),
        (None, false) => When::parse(&time.join(" "), &now),
    }
    .map_err(Problem::Usage)?;
    if options.tz.is_some() && !matches!(when, When::Cron(_) | When::At(_)) {
        return Err(Problem::Usage(
            "--tz is for a cron expression or `at`".to_string(),
        ));
    }
    let target = match (session, new, when.is_trigger()) {
        (Some(wanted), _, _) => {
            Target::Session(session_id(&scheduler.paths, wanted).map_err(Problem::Usage)?)
        }
        (None, false, true) => Target::Open,
        (None, _, _) => Target::New,
    };
    let schedule = scheduler.add(when, prompt, target, run_args(cli), options)?;
    println!("{}", schedule.id);
    eprintln!("{}", scheduler.added_line(&schedule));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    pub(super) fn scheduler(dir: &Path) -> Scheduler {
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
            triggers: TriggersConfig::default(),
            home_args: Vec::new(),
            shell: Shell::resolve(""),
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
                Options::default(),
            )
            .unwrap();
        let trigger = scheduler
            .add(
                When::CiFailure,
                "fix the build",
                Target::Open,
                Vec::new(),
                Options::default(),
            )
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
            Options::default(),
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
                .add(every.clone(), "  ", Target::New, Vec::new(), Options::default())
                .unwrap_err()
                .contains("needs a prompt")
        );
        assert!(
            scheduler
                .add(every.clone(), "go", Target::Open, Vec::new(), Options::default())
                .unwrap_err()
                .contains("goes by the clock")
        );
        let zoned = Options {
            tz: Some("Europe/Berlin".into()),
            ..Options::default()
        };
        assert!(
            scheduler
                .add(every, "go", Target::New, Vec::new(), zoned)
                .unwrap_err()
                .contains("a time zone is for a cron expression")
        );
        assert_eq!(scheduler.store().list(), []);
    }
}
