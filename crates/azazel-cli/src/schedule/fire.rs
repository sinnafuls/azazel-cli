//! A run of a schedule: `azazel schedule run`, called by the scheduler of the operating system
//! when the schedule may be due, or by hand.

use std::ffi::OsString;

use azazel_core::SessionStore;
use azazel_protocol::{Attached, Command, Replay};
use jiff::{Timestamp, Zoned};

use super::check;
use super::when::{Due, When};
use super::{Schedule, Scheduler, State, Target};
use crate::detach;
use crate::print::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};
use crate::running::{self, Lock};

/// How long a background session a schedule started stays once it has nothing to do and
/// nobody is attached: what it did is in its log and its session.
const IDLE_SECS: &str = "120";

/// What a run came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fired {
    /// A background session was started; its id.
    Started(String),
    /// The prompt was sent to the background session that has the schedule's session open.
    Sent(String),
    /// Nothing was started, and why.
    Skipped(String),
    /// The scheduler woke the run at a time the schedule does not run at.
    NotDue,
    /// Its one moment passed while nothing could run it.
    Missed,
    /// It stopped for good, and why: its `until` check held, its end came.
    Ended(String),
}

/// What the end and the checks of a schedule say about a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admit {
    /// The prompt runs.
    Go,
    /// Not this time, and why: the `if` check did not hold.
    Skip(String),
    /// Not any more, and why; the schedule has been ended.
    End(String),
}

/// Decides, at `now` and before any model is asked, whether the prompt of `schedule` runs: its
/// end time, then its `until` check (which ends it once it holds), then its `if` check. What the
/// checks said is kept with the schedule. `schedule` is to be as the store has it now.
pub async fn admit(scheduler: &Scheduler, schedule: &Schedule, now: &Zoned) -> Admit {
    let at = now.timestamp();
    if schedule
        .ends
        .as_ref()
        .and_then(|ends| ends.at)
        .is_some_and(|end| at >= end)
    {
        let why = "ended: its end time passed".to_string();
        scheduler.end(schedule, &why, at);
        return Admit::End(why);
    }
    if !schedule.gated() {
        return Admit::Go;
    }
    let mut said = Vec::new();
    let mut until_seen = None;
    let mut if_seen = None;
    let admit = 'decide: {
        if let Some(until) = &schedule.until {
            let judged = check::evaluate(&scheduler.shell, &schedule.cwd, until).await;
            until_seen = Some(judged.seen);
            if judged.held {
                said.push(format!("until held ({})", judged.why));
                break 'decide Admit::End(format!("ended: `until` held ({})", judged.why));
            }
            said.push(format!("until did not hold ({})", judged.why));
        }
        if let Some(gate) = &schedule.if_check {
            let judged = check::evaluate(&scheduler.shell, &schedule.cwd, gate).await;
            if_seen = Some(judged.seen);
            if !judged.held {
                said.push(format!("if did not hold ({})", judged.why));
                break 'decide Admit::Skip(format!("`if` did not hold ({})", judged.why));
            }
            said.push(format!("if held ({})", judged.why));
        }
        Admit::Go
    };
    let noted = scheduler.store().update(|all| {
        let Some(kept) = all.iter_mut().find(|kept| kept.id == schedule.id) else {
            return;
        };
        kept.last_gate = Some(said.join("; "));
        if let (Some(check), Some(seen)) = (kept.until.as_mut(), until_seen) {
            check.seen = seen;
        }
        if let (Some(check), Some(seen)) = (kept.if_check.as_mut(), if_seen) {
            check.seen = seen;
        }
    });
    if let Err(e) = noted {
        tracing::warn!("could not note the checks of schedule {}: {e}", schedule.id);
    }
    if let Admit::End(why) = &admit {
        scheduler.end(schedule, why, at);
    }
    admit
}

/// Ends `schedule` when the run it just had is the last its `ends.times` allows. `ran` is
/// whether the prompt went anywhere.
pub fn count_run(scheduler: &Scheduler, schedule: &Schedule, at: Timestamp, ran: bool) {
    let runs = schedule.runs + u32::from(ran);
    if let Some(times) = schedule.ends.as_ref().and_then(|ends| ends.times)
        && ran
        && runs >= times
    {
        scheduler.end(
            schedule,
            &format!("ended: it ran {runs} times, as many as it was to"),
            at,
        );
    }
}

/// The file a run of schedule `id` holds locked while it decides and starts: of two runs woken
/// at once, one goes ahead.
fn lock_file(home: &std::path::Path, id: &str) -> std::path::PathBuf {
    running::run_dir(home).join(format!("schedule-{id}.lock"))
}

/// Starts a background session with `prompt`, on the session in `session_file` when one is
/// given. It works as the schedule was added to work: its model, mode and settings.
async fn start_host(
    scheduler: &Scheduler,
    schedule: &Schedule,
    session_file: Option<std::path::PathBuf>,
    prompt: &str,
) -> Result<Fired, String> {
    let home = &scheduler.paths.home;
    let id = detach::new_id(home);
    let mut args: Vec<OsString> = schedule.args.iter().map(OsString::from).collect();
    args.extend(scheduler.home_args.iter().map(OsString::from));
    args.extend(["--cwd".into(), schedule.cwd.clone().into()]);
    match session_file {
        Some(file) => args.extend(["--session".into(), file.into()]),
        None => args.extend(["--name".into(), format!("schedule {}", schedule.id).into()]),
    }
    args.extend(["host", "--id", &id, "--idle-secs", IDLE_SECS, "--"].map(OsString::from));
    args.push(prompt.into());
    detach::start(home, args, &id).await?;
    Ok(Fired::Started(id))
}

/// Sends `prompt` to a background session: it starts on it at once when it has nothing to do,
/// and after what it is doing otherwise.
async fn send(entry: &running::Entry, prompt: &str) -> Result<Fired, String> {
    let client = detach::connect(entry).await?;
    let attached = client
        .send(Command::Attach {
            pid: None,
            replay: Replay::None,
        })
        .await
        .map_err(|e| e.to_string())?;
    let attached: Attached = serde_json::from_value(attached)
        .map_err(|e| format!("the background session answered something else than expected: {e}"))?;
    // The one sent the last time has not had its turn yet: a second one would only pile up.
    if attached.snapshot.queue.iter().any(|queued| queued.text == prompt) {
        let _ = client.shutdown().await;
        return Ok(Fired::Skipped(format!(
            "skipped: the prompt sent before still waits its turn in {}",
            entry.id
        )));
    }
    let sent = client
        .send(Command::Prompt {
            text: prompt.to_string(),
            images: Vec::new(),
            request_id: None,
        })
        .await;
    let _ = client.shutdown().await;
    sent.map(|_| Fired::Sent(entry.id.clone()))
        .map_err(|e| e.to_string())
}

/// Hands `prompt` to where the schedule's runs go: a new background session, or the session it
/// names.
pub async fn deliver(scheduler: &Scheduler, schedule: &Schedule, prompt: &str) -> Result<Fired, String> {
    let home = &scheduler.paths.home;
    let Target::Session(session_id) = &schedule.target else {
        return start_host(scheduler, schedule, None, prompt).await;
    };
    let host = running::host_of(home, session_id).and_then(|host| running::read_entry(home, &host));
    if let Some(entry) = host {
        return send(&entry, prompt).await;
    }
    let file = SessionStore::list(&scheduler.paths.sessions_dir())
        .into_iter()
        .find(|session| session.id == *session_id)
        .map(|session| session.path)
        .ok_or_else(|| format!("the session {session_id} it goes to is gone"))?;
    start_host(scheduler, schedule, Some(file), prompt).await
}

/// Notes what a delivery came to with the schedule, and returns it as a line.
pub fn record(
    scheduler: &Scheduler,
    schedule: &Schedule,
    at: Timestamp,
    fired: &Result<Fired, String>,
) -> String {
    let store = scheduler.store();
    let (result, host) = match fired {
        Ok(Fired::Started(host)) => (format!("started {host}"), Some(host.as_str())),
        Ok(Fired::Sent(host)) => (format!("sent to {host}"), Some(host.as_str())),
        Ok(Fired::Skipped(why)) => (why.clone(), None),
        Ok(Fired::NotDue) => return "not due".to_string(),
        // Noted when it was ended.
        Ok(Fired::Ended(why)) => return why.clone(),
        Ok(Fired::Missed) => ("missed: nothing could run it at its time".to_string(), None),
        Err(why) => (format!("failed: {why}"), None),
    };
    store.record(&schedule.id, at, &result, host, host.is_some());
    result
}

/// Runs the schedule `wanted` names. With `scheduled`, only when it is due now: a run the
/// scheduler starts late, after the machine slept through the time, is not made up for.
pub async fn fire(scheduler: &Scheduler, wanted: &str, scheduled: bool) -> Result<Fired, String> {
    fire_at(scheduler, wanted, scheduled, &Zoned::now()).await
}

/// [`fire`] at `now`.
async fn fire_at(scheduler: &Scheduler, wanted: &str, scheduled: bool, now: &Zoned) -> Result<Fired, String> {
    let home = &scheduler.paths.home;
    let store = scheduler.store();
    let id = store.find(wanted)?.id;
    running::create_run_dir(home).map_err(|e| format!("cannot create the run directory: {e}"))?;
    let Some(_lock) =
        Lock::acquire(&lock_file(home, &id)).map_err(|e| format!("cannot lock the schedule: {e}"))?
    else {
        return Ok(Fired::Skipped(
            "skipped: another run of it is being started".to_string(),
        ));
    };
    // As it is now that no other run is deciding about it.
    let schedule = store.find(&id)?;
    match (schedule.state, scheduled) {
        (State::Active, _) => {}
        (_, true) => return Ok(Fired::NotDue),
        (State::Paused, false) => {
            return Ok(Fired::Skipped(format!(
                "skipped: it is paused (azazel schedule resume {} lets it run again)",
                schedule.id
            )));
        }
        (State::Ended, false) => {
            return Ok(Fired::Skipped(format!(
                "skipped: it has ended ({})",
                schedule.last_result.as_deref().unwrap_or("ended")
            )));
        }
    }
    if scheduled {
        match schedule.due(now) {
            Due::Yes => {}
            Due::No => return Ok(Fired::NotDue),
            Due::Missed => {
                // Its entry in the scheduler has nothing left to start.
                if schedule.registered {
                    let _ = super::os::unregister(&schedule.id);
                }
                let _ = store.update(|all| {
                    if let Some(kept) = all.iter_mut().find(|kept| kept.id == schedule.id) {
                        kept.registered = false;
                    }
                });
                let missed = Ok(Fired::Missed);
                record(scheduler, &schedule, now.timestamp(), &missed);
                return missed;
            }
        }
    }
    // One run at a time: a session the last run started that still works is left to it.
    let working = schedule
        .last_host
        .as_ref()
        .filter(|_| !matches!(schedule.target, Target::Session(_)))
        .and_then(|host| running::read_entry(home, host))
        .filter(|entry| entry.alive(home) && entry.state != "idle");
    if let Some(entry) = working {
        let skipped = Ok(Fired::Skipped(format!(
            "skipped: the run before it is still at work (azazel attach {})",
            entry.id
        )));
        record(scheduler, &schedule, now.timestamp(), &skipped);
        return skipped;
    }
    match admit(scheduler, &schedule, now).await {
        Admit::Go => {}
        Admit::End(why) => return Ok(Fired::Ended(why)),
        // A single moment that the check let pass has had its time.
        Admit::Skip(why) if matches!(schedule.when, When::At(_)) => {
            let why = format!("ended: {why} at its time");
            scheduler.end(&schedule, &why, now.timestamp());
            return Ok(Fired::Ended(why));
        }
        Admit::Skip(why) => {
            let skipped = Ok(Fired::Skipped(format!("skipped: {why}")));
            record(scheduler, &schedule, now.timestamp(), &skipped);
            return skipped;
        }
    }
    let fired = deliver(scheduler, &schedule, &schedule.prompt).await;
    record(scheduler, &schedule, now.timestamp(), &fired);
    let ran = matches!(fired, Ok(Fired::Started(_) | Fired::Sent(_)));
    count_run(scheduler, &schedule, now.timestamp(), ran);
    // A single moment has had its run.
    if matches!(schedule.when, When::At(_))
        && matches!(fired, Ok(Fired::Started(_) | Fired::Sent(_)))
        && let Err(e) = scheduler.remove(&schedule.id)
    {
        tracing::warn!("could not remove the schedule {} after its run: {e}", schedule.id);
    }
    fired
}

/// `azazel schedule run [<id>] [--due]`.
pub async fn command(scheduler: &Scheduler, id: Option<&str>, due: bool, scheduled: bool) -> u8 {
    let Some(id) = id else {
        if !due {
            eprintln!(
                "azazel: which schedule? azazel schedule run <id>, or --due for every one that is due now"
            );
            return EXIT_USAGE;
        }
        let mut code = EXIT_OK;
        let timed = scheduler
            .store()
            .list()
            .into_iter()
            .filter(|schedule| !schedule.when.is_trigger());
        for schedule in timed {
            match fire(scheduler, &schedule.id, true).await {
                Ok(Fired::NotDue) => {}
                Ok(Fired::Started(host)) => println!("{}: started {host}", schedule.id),
                Ok(Fired::Sent(host)) => println!("{}: sent to {host}", schedule.id),
                Ok(Fired::Skipped(why)) => println!("{}: {why}", schedule.id),
                Ok(Fired::Missed) => println!("{}: missed", schedule.id),
                Ok(Fired::Ended(why)) => println!("{}: {why}", schedule.id),
                Err(why) => {
                    eprintln!("azazel: {}: {why}", schedule.id);
                    code = EXIT_FAILED;
                }
            }
        }
        return code;
    };
    let fired = fire(scheduler, id, scheduled || due).await;
    // Nobody reads what a run the scheduler started prints.
    if scheduled {
        match &fired {
            Ok(fired) => tracing::info!("schedule {id}: {fired:?}"),
            Err(why) => tracing::warn!("schedule {id} did not run: {why}"),
        }
    }
    match fired {
        Ok(Fired::Started(host)) => {
            println!("{host}");
            eprintln!("azazel attach {host} connects to it, azazel logs -f {host} follows it");
            EXIT_OK
        }
        Ok(Fired::Sent(host)) => {
            println!("{host}");
            eprintln!("sent to the background session {host}, which has the schedule's session open");
            EXIT_OK
        }
        Ok(Fired::Skipped(why)) => {
            eprintln!("{why}");
            EXIT_OK
        }
        Ok(Fired::NotDue) => {
            eprintln!("not due now");
            EXIT_OK
        }
        Ok(Fired::Missed) => {
            eprintln!("missed: its time passed while nothing could run it");
            EXIT_OK
        }
        Ok(Fired::Ended(why)) => {
            eprintln!("{why}");
            EXIT_OK
        }
        Err(why) => {
            eprintln!("azazel: {why}");
            EXIT_FAILED
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::super::check::{Check, Ends};
    use super::super::{Options, tests::scheduler};
    use super::*;

    fn at(text: &str) -> Zoned {
        format!("{text}[UTC]").parse().unwrap()
    }

    fn add(scheduler: &Scheduler, when: &str, options: Options) -> Schedule {
        let now = Zoned::now();
        scheduler
            .add(
                When::parse(when, &now).unwrap(),
                "go",
                Target::New,
                Vec::new(),
                options,
            )
            .unwrap()
    }

    fn kept(scheduler: &Scheduler, id: &str) -> Schedule {
        scheduler.store().find(id).unwrap()
    }

    #[tokio::test]
    async fn checks_decide_whether_the_prompt_runs_and_when_the_schedule_ends() {
        let dir = tempfile::tempdir().unwrap();
        let scheduler = scheduler(dir.path());
        let cwd = scheduler.paths.cwd.clone();
        let write = |file: &str, text: &str| std::fs::write(cwd.join(file), text).unwrap();
        write("until.txt", "working\n");
        write("if.txt", "v1\n");
        let schedule = add(
            &scheduler,
            "every 10m",
            Options {
                if_check: Some(Check::new("cat if.txt", None, None, true).unwrap()),
                until: Some(Check::new("cat until.txt", None, Some("^done$"), false).unwrap()),
                ..Options::default()
            },
        );
        let now = at("2026-11-13T10:00:00+00:00");

        // The first run of a `changes` check has nothing to compare with.
        let first = admit(&scheduler, &kept(&scheduler, &schedule.id), &now).await;
        assert_eq!(
            first,
            Admit::Skip("`if` did not hold (first run: nothing to compare with yet)".into())
        );
        assert_eq!(
            kept(&scheduler, &schedule.id).last_gate.as_deref(),
            Some(
                "until did not hold (printed nothing that matches `^done$`); \
                 if did not hold (first run: nothing to compare with yet)"
            )
        );
        let same = admit(&scheduler, &kept(&scheduler, &schedule.id), &now).await;
        assert_eq!(
            same,
            Admit::Skip("`if` did not hold (printed the same as the time before)".into())
        );
        write("if.txt", "v2\n");
        let changed = admit(&scheduler, &kept(&scheduler, &schedule.id), &now).await;
        assert_eq!(changed, Admit::Go);
        assert!(
            kept(&scheduler, &schedule.id)
                .last_gate
                .unwrap()
                .ends_with("if held (exit 0, changed)")
        );

        // Once `until` holds, the schedule ends and the `if` check is not run.
        let seen = kept(&scheduler, &schedule.id).if_check.unwrap().seen;
        write("until.txt", "done\n");
        write("if.txt", "v3\n");
        let ended = admit(&scheduler, &kept(&scheduler, &schedule.id), &now).await;
        assert_eq!(ended, Admit::End("ended: `until` held (exit 0, matched)".into()));
        let after = kept(&scheduler, &schedule.id);
        assert_eq!(after.state, State::Ended);
        assert_eq!(
            after.last_result.as_deref(),
            Some("ended: `until` held (exit 0, matched)")
        );
        assert_eq!(after.last_gate.as_deref(), Some("until held (exit 0, matched)"));
        assert_eq!(after.next(&now), None);
        // A check that was not run keeps what it saw.
        assert_eq!(after.if_check.unwrap().seen, seen);
    }

    #[tokio::test]
    async fn a_run_goes_by_its_zone_its_checks_and_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let scheduler = scheduler(dir.path());
        let cwd = scheduler.paths.cwd.clone();
        let schedule = add(
            &scheduler,
            "0 9 * * *",
            Options {
                if_check: Some(Check::new("echo checked >> ran.txt; exit 1", None, None, false).unwrap()),
                ends: Some(Ends {
                    at: Some("2026-11-14T00:00:00Z".parse().unwrap()),
                    times: None,
                }),
                tz: Some("America/New_York".into()),
                ..Options::default()
            },
        );
        // Nine in the morning here is four in New York: not due, and no check is run.
        let early = fire_at(&scheduler, &schedule.id, true, &at("2026-11-13T09:00:10+00:00")).await;
        assert_eq!(early, Ok(Fired::NotDue));
        assert!(!cwd.join("ran.txt").exists());

        // Nine in New York: the check runs, does not hold, and nothing is started.
        let due = at("2026-11-13T14:00:10+00:00");
        let skipped = fire_at(&scheduler, &schedule.id, true, &due).await;
        assert_eq!(
            skipped,
            Ok(Fired::Skipped(
                "skipped: `if` did not hold (exit 1, not 0)".into()
            ))
        );
        assert_eq!(
            std::fs::read_to_string(cwd.join("ran.txt")).unwrap().trim(),
            "checked"
        );
        let noted = kept(&scheduler, &schedule.id);
        assert_eq!((noted.runs, noted.last_run), (0, Some(due.timestamp())));
        // Woken again in the same minute, it does not run twice.
        assert_eq!(
            fire_at(&scheduler, &schedule.id, true, &at("2026-11-13T14:00:40+00:00")).await,
            Ok(Fired::NotDue)
        );

        // Its end has passed by the next morning: it ends without running its check.
        std::fs::remove_file(cwd.join("ran.txt")).unwrap();
        let ended = fire_at(&scheduler, &schedule.id, true, &at("2026-11-14T14:00:05+00:00")).await;
        assert_eq!(ended, Ok(Fired::Ended("ended: its end time passed".into())));
        assert!(!cwd.join("ran.txt").exists());
        assert_eq!(kept(&scheduler, &schedule.id).state, State::Ended);
        let by_hand = fire_at(&scheduler, &schedule.id, false, &at("2026-11-14T15:00:00+00:00")).await;
        assert_eq!(
            by_hand,
            Ok(Fired::Skipped(
                "skipped: it has ended (ended: its end time passed)".into()
            ))
        );
        assert!(scheduler.resume(&schedule.id).unwrap_err().contains("has ended"));
    }

    #[tokio::test]
    async fn a_paused_schedule_does_not_run_until_it_is_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let scheduler = scheduler(dir.path());
        let schedule = add(&scheduler, "every 10m", Options::default());
        let paused = scheduler.pause(&schedule.id[..4]).unwrap();
        assert_eq!(paused.state, State::Paused);
        assert!(
            scheduler
                .pause(&schedule.id)
                .unwrap_err()
                .contains("paused already")
        );
        let later = Zoned::now()
            .checked_add(jiff::SignedDuration::from_hours(1))
            .unwrap();
        assert_eq!(
            fire_at(&scheduler, &schedule.id, true, &later).await,
            Ok(Fired::NotDue)
        );
        let by_hand = fire_at(&scheduler, &schedule.id, false, &later).await.unwrap();
        assert!(
            matches!(&by_hand, Fired::Skipped(why) if why.starts_with("skipped: it is paused")),
            "{by_hand:?}"
        );
        assert_eq!(kept(&scheduler, &schedule.id).next(&later), None);

        let resumed = scheduler.resume(&schedule.id).unwrap();
        assert_eq!(resumed.state, State::Active);
        assert!(resumed.next(&later).is_some());
        assert!(scheduler.resume(&schedule.id).unwrap_err().contains("not paused"));
    }

    #[test]
    fn a_schedule_ends_once_it_ran_its_number_of_times() {
        let dir = tempfile::tempdir().unwrap();
        let scheduler = scheduler(dir.path());
        let schedule = add(
            &scheduler,
            "every 10m",
            Options {
                ends: Some(Ends {
                    at: None,
                    times: Some(2),
                }),
                ..Options::default()
            },
        );
        let now = Timestamp::now();
        // A run that went nowhere does not count.
        count_run(&scheduler, &schedule, now, false);
        assert_eq!(kept(&scheduler, &schedule.id).state, State::Active);
        scheduler
            .store()
            .record(&schedule.id, now, "started 1a2b3c4d", Some("1a2b3c4d"), true);
        let once = kept(&scheduler, &schedule.id);
        count_run(&scheduler, &schedule, now, true);
        assert_eq!(kept(&scheduler, &schedule.id).state, State::Active);
        scheduler
            .store()
            .record(&schedule.id, now, "started 5e6f7a8b", Some("5e6f7a8b"), true);
        count_run(&scheduler, &once, now, true);
        let ended = kept(&scheduler, &schedule.id);
        assert_eq!(ended.state, State::Ended);
        assert_eq!(
            ended.last_result.as_deref(),
            Some("ended: it ran 2 times, as many as it was to")
        );
    }
}
