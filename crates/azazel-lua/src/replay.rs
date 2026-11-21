//! The call log of a background script that can pause. Every call whose result steers the script is
//! recorded with the place it was made from; when the script runs again after an approval, each
//! earlier call gets its recorded result instead of being made again.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Mutex, PoisonError};

use serde_json::Value;

/// The most one recorded call (its arguments and its outcome as JSON) may take.
pub(crate) const MAX_RECORDED_BYTES: usize = 256 * 1024;
/// The most the recorded calls of one run may take together.
pub(crate) const MAX_LOG_BYTES: usize = 8 * 1024 * 1024;

/// Where a call is made: its ordinal in its branch, after the ordinals of the `parallel` calls and
/// branches that lead there (`3` at the top level, `3.2.1` for call 1 of branch 2 of call 3).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct Site(pub Vec<u32>);

impl fmt::Display for Site {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, ordinal) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            write!(f, "{ordinal}")?;
        }
        Ok(())
    }
}

/// What a recorded call is, for matching it on replay.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Tool(String),
    Agent,
    Classify,
    Step(String),
    Store,
    Load,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Kind::Tool(name) => f.write_str(name),
            Kind::Agent => f.write_str("agents.run"),
            Kind::Classify => f.write_str("classify"),
            Kind::Step(name) => write!(f, "step {name}"),
            Kind::Store => f.write_str("store"),
            Kind::Load => f.write_str("load"),
        }
    }
}

/// What the script saw of a call.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Outcome {
    Ok(Value),
    Err(String),
}

impl Outcome {
    fn bytes(&self) -> usize {
        match self {
            Outcome::Ok(value) => value.to_string().len(),
            Outcome::Err(text) => text.len(),
        }
    }
}

enum State {
    Executing,
    Applied(Outcome),
    /// The call needs the user's answer; the script ended its pass here.
    Pending,
}

struct Entry {
    kind: Kind,
    args: Value,
    state: State,
    bytes: usize,
}

/// What the log says about a call that is about to be made.
#[derive(Debug, PartialEq)]
pub(crate) enum Lookup {
    /// Nothing is recorded here: make the call.
    Live,
    /// The same call was made before: return its outcome.
    Replay(Outcome),
    /// Something else was recorded here: the script is not repeating itself.
    Diverged(String),
    /// The call that paused the run, now answered: make it.
    Approved,
}

#[derive(Default)]
struct Log {
    entries: BTreeMap<Site, Entry>,
    bytes: usize,
}

/// The log of one background run, kept across its passes.
#[derive(Default)]
pub(crate) struct Journal {
    log: Mutex<Log>,
}

/// `text` cut to about 200 characters.
fn short(text: &str) -> String {
    match text.char_indices().nth(200) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

fn kilobytes(bytes: usize) -> usize {
    bytes.div_ceil(1024)
}

impl Journal {
    fn log(&self) -> std::sync::MutexGuard<'_, Log> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn lookup(&self, site: &Site, kind: &Kind, args: &Value) -> Lookup {
        let log = self.log();
        let Some(entry) = log.entries.get(site) else {
            return Lookup::Live;
        };
        if &entry.kind != kind || &entry.args != args {
            return Lookup::Diverged(format!(
                "replay diverged at call {site}: the first run called {} {}, this run called {kind} {args}; \
                 a script that pauses must make the same calls in the same order: wrap os.time, \
                 math.random and the like in step(name, fn)",
                entry.kind,
                short(&entry.args.to_string()),
            ));
        }
        match &entry.state {
            State::Applied(outcome) => Lookup::Replay(outcome.clone()),
            State::Pending => Lookup::Approved,
            State::Executing => Lookup::Live,
        }
    }

    /// Notes that the call at `site` has started.
    pub fn begin(&self, site: Site, kind: Kind, args: Value) {
        let mut log = self.log();
        if let Some(old) = log.entries.remove(&site) {
            log.bytes -= old.bytes;
        }
        log.entries.insert(
            site,
            Entry {
                kind,
                args,
                state: State::Executing,
                bytes: 0,
            },
        );
    }

    /// Records what the call at `site` gave. `Err(why)` when it is over [`MAX_RECORDED_BYTES`] or
    /// the log is over [`MAX_LOG_BYTES`] with it: the outcome is then not kept, and the run cannot
    /// replay.
    pub fn applied(&self, site: &Site, outcome: Outcome) -> Result<(), String> {
        let mut log = self.log();
        let Some(entry) = log.entries.get(site) else {
            return Ok(());
        };
        let size = entry.args.to_string().len() + outcome.bytes();
        let too_big = if size > MAX_RECORDED_BYTES {
            Some(format!(
                "the result of call {site} ({}) is {} KB, over the {} KB a paused script can keep: keep \
                 large data in a file and pass its path",
                entry.kind,
                kilobytes(size),
                MAX_RECORDED_BYTES / 1024
            ))
        } else if log.bytes + size > MAX_LOG_BYTES {
            Some(format!(
                "the calls of this run add up to over {} MB, more than a paused script can keep",
                MAX_LOG_BYTES / (1024 * 1024)
            ))
        } else {
            None
        };
        if let Some(why) = too_big {
            log.entries.remove(site);
            return Err(why);
        }
        log.bytes += size;
        if let Some(entry) = log.entries.get_mut(site) {
            entry.bytes = size;
            entry.state = State::Applied(outcome);
        }
        Ok(())
    }

    /// Notes that the call at `site` waits for the user.
    pub fn pend(&self, site: &Site) {
        if let Some(entry) = self.log().entries.get_mut(site) {
            entry.state = State::Pending;
        }
    }

    /// When a pass that did not pause ended without making a call the log has (the script did not
    /// repeat itself), what to say about it.
    pub fn unreached(&self, seen: &BTreeSet<Site>) -> Option<String> {
        let log = self.log();
        let (site, entry) = log.entries.iter().find(|(site, _)| !seen.contains(*site))?;
        Some(format!(
            "replay diverged: the first run called {} {} as call {site}, this run never got there; a \
             script that pauses must make the same calls in the same order: wrap os.time, math.random \
             and the like in step(name, fn)",
            entry.kind,
            short(&entry.args.to_string()),
        ))
    }

    /// How many calls the log can replay.
    pub fn replayed(&self) -> usize {
        self.log()
            .entries
            .values()
            .filter(|entry| matches!(entry.state, State::Applied(_)))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn site(path: &[u32]) -> Site {
        Site(path.to_vec())
    }

    fn read(path: &str) -> (Kind, Value) {
        (Kind::Tool("read".into()), json!({"path": path}))
    }

    fn record(journal: &Journal, at: &Site, kind: Kind, args: Value, outcome: Outcome) {
        journal.begin(at.clone(), kind, args);
        journal.applied(at, outcome).unwrap();
    }

    #[test]
    fn the_same_call_at_the_same_place_replays_its_outcome_and_object_key_order_does_not_matter() {
        let journal = Journal::default();
        let at = site(&[1]);
        assert_eq!(journal.lookup(&at, &Kind::Agent, &json!(null)), Lookup::Live);
        record(
            &journal,
            &at,
            Kind::Tool("read".into()),
            json!({"path": "a", "limit": 3}),
            Outcome::Ok(json!("text")),
        );
        assert_eq!(
            journal.lookup(&at, &Kind::Tool("read".into()), &json!({"limit": 3, "path": "a"})),
            Lookup::Replay(Outcome::Ok(json!("text")))
        );
        assert_eq!(journal.replayed(), 1);
    }

    #[test]
    fn a_different_tool_arguments_or_step_name_diverge_and_the_text_names_the_place() {
        let journal = Journal::default();
        let (kind, args) = read("a");
        record(
            &journal,
            &site(&[3]),
            kind.clone(),
            args.clone(),
            Outcome::Ok(json!("x")),
        );
        record(
            &journal,
            &site(&[4]),
            Kind::Step("t".into()),
            Value::Null,
            Outcome::Ok(json!(1)),
        );

        let (_, other_args) = read("b");
        for lookup in [
            journal.lookup(&site(&[3]), &Kind::Tool("write".into()), &args),
            journal.lookup(&site(&[3]), &kind, &other_args),
            journal.lookup(&site(&[4]), &Kind::Step("u".into()), &Value::Null),
        ] {
            let Lookup::Diverged(text) = lookup else {
                panic!("expected a divergence, got {lookup:?}");
            };
            assert!(text.starts_with("replay diverged at call "), "{text}");
            assert!(text.contains("step(name, fn)"), "{text}");
        }
        let Lookup::Diverged(text) = journal.lookup(&site(&[3]), &Kind::Tool("write".into()), &args) else {
            panic!("diverged");
        };
        assert!(text.contains("call 3"), "{text}");
        assert!(text.contains("first run called read"), "{text}");
        assert!(text.contains("this run called write"), "{text}");
    }

    #[test]
    fn a_pending_call_is_approved_when_it_is_made_again_and_its_args_still_match() {
        let journal = Journal::default();
        let (kind, args) = read("a");
        journal.begin(site(&[1]), kind.clone(), args.clone());
        journal.pend(&site(&[1]));
        assert_eq!(journal.lookup(&site(&[1]), &kind, &args), Lookup::Approved);
        assert_eq!(journal.replayed(), 0);
    }

    #[test]
    fn an_entry_over_the_entry_limit_or_a_log_over_the_log_limit_is_refused_and_not_kept() {
        let journal = Journal::default();
        let (kind, args) = read("a");
        journal.begin(site(&[1]), kind.clone(), args.clone());
        let why = journal
            .applied(&site(&[1]), Outcome::Ok(json!("x".repeat(300 * 1024))))
            .unwrap_err();
        assert!(why.contains("call 1 (read)"), "{why}");
        assert!(why.contains("256 KB"), "{why}");
        assert_eq!(journal.lookup(&site(&[1]), &kind, &args), Lookup::Live);

        let chunk = json!("x".repeat(MAX_RECORDED_BYTES - 100));
        let mut refused = None;
        for ordinal in 1..=40 {
            let at = site(&[ordinal]);
            journal.begin(at.clone(), kind.clone(), args.clone());
            if let Err(why) = journal.applied(&at, Outcome::Ok(chunk.clone())) {
                refused = Some(why);
                break;
            }
        }
        assert!(refused.expect("the log fills up").contains("8 MB"));
    }

    #[test]
    fn a_call_the_log_has_and_the_pass_never_made_is_a_divergence() {
        let journal = Journal::default();
        let (kind, args) = read("a");
        record(
            &journal,
            &site(&[1]),
            kind.clone(),
            args.clone(),
            Outcome::Ok(json!("x")),
        );
        record(&journal, &site(&[2]), kind, args, Outcome::Err("no".into()));
        let seen: BTreeSet<Site> = [site(&[1])].into();
        let text = journal.unreached(&seen).expect("call 2 was not reached");
        assert!(text.contains("call 2"), "{text}");
        let seen: BTreeSet<Site> = [site(&[1]), site(&[2])].into();
        assert_eq!(journal.unreached(&seen), None);
    }

    #[test]
    fn a_site_displays_as_its_path_and_sorts_by_it() {
        assert_eq!(site(&[3, 2, 1]).to_string(), "3.2.1");
        assert_eq!(site(&[3]).to_string(), "3");
        assert!(site(&[2, 5]) < site(&[3]));
        assert!(site(&[3]) < site(&[3, 1]));
        assert!(site(&[3, 1, 2]) < site(&[3, 2, 1]));
    }
}
