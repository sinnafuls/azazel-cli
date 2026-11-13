//! A run of a schedule: `azazel schedule run`, called by the scheduler of the operating system
//! when the schedule may be due, or by hand.

use std::ffi::OsString;

use azazel_core::SessionStore;
use azazel_protocol::{Attached, Command, Replay};
use jiff::{Timestamp, Zoned};

use super::when::{Due, When};
use super::{Schedule, Scheduler, Target};
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
        Ok(Fired::Missed) => ("missed: nothing could run it at its time".to_string(), None),
        Err(why) => (format!("failed: {why}"), None),
    };
    store.record(&schedule.id, at, &result, host, host.is_some());
    result
}

/// Runs the schedule `wanted` names. With `scheduled`, only when it is due now: a run the
/// scheduler starts late, after the machine slept through the time, is not made up for.
pub async fn fire(scheduler: &Scheduler, wanted: &str, scheduled: bool) -> Result<Fired, String> {
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
    let now = Zoned::now();
    if scheduled {
        match schedule.when.due(&now, schedule.last_run, schedule.created) {
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
    let fired = match working {
        Some(entry) => Ok(Fired::Skipped(format!(
            "skipped: the run before it is still at work (azazel attach {})",
            entry.id
        ))),
        None => deliver(scheduler, &schedule, &schedule.prompt).await,
    };
    record(scheduler, &schedule, now.timestamp(), &fired);
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
        Err(why) => {
            eprintln!("azazel: {why}");
            EXIT_FAILED
        }
    }
}
