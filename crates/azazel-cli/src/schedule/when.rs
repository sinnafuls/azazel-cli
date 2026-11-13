//! When a schedule runs: a cron expression, an interval, one moment, or something that happens
//! (a failed CI run, a changed file).

use std::fmt;
use std::time::Duration;

use jiff::civil::{Date, DateTime};
use jiff::{Timestamp, ToSpan, Zoned};

use crate::loops::{span, span_text};

/// The shortest interval: the schedulers of the operating systems count in minutes.
const MIN_EVERY: Duration = Duration::from_secs(60);
/// The longest interval Task Scheduler repeats at.
const MAX_EVERY: Duration = Duration::from_secs(31 * 24 * 60 * 60);
/// How late a run may start and still count as on time. Past it the run is missed, and missed
/// runs are not caught up.
const CRON_LATE_MINUTES: i64 = 2;
const ONCE_LATE: Duration = Duration::from_secs(10 * 60);
/// How early the scheduler may wake a run that goes by elapsed time.
const EARLY: Duration = Duration::from_secs(20);

/// What a schedule waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    Cron(Cron),
    /// Again and again, this far apart.
    Every(Duration),
    /// Once.
    At(Timestamp),
    /// A CI run of the project failed (`gh run list`).
    CiFailure,
    /// A file matching this glob changed.
    File(String),
}

/// Whether a schedule is to run at a given moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Due {
    Yes,
    No,
    /// Its one moment passed while nothing could run it.
    Missed,
}

impl When {
    /// Reads `text` as typed: `every 10m`, `at +2h`, `at 17:30`, `at 2026-12-01 09:00`,
    /// `on ci-failure`, `on file:src/**/*.rs`, or five cron fields. Times without a zone are in
    /// the zone of `now`.
    pub fn parse(text: &str, now: &Zoned) -> Result<When, String> {
        When::read(text, now, false)
    }

    /// [`When::parse`]; `stored` is whether the text is one that was kept, whose one moment may
    /// have passed since.
    fn read(text: &str, now: &Zoned, stored: bool) -> Result<When, String> {
        let text = text.trim();
        let lower = text.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("every ") {
            return every(rest).map(When::Every);
        }
        if lower.starts_with("at ") {
            return at(text[3..].trim(), now, stored).map(When::At);
        }
        if lower.starts_with("on ") {
            return on(text[3..].trim());
        }
        Cron::parse(text).map(When::Cron)
    }

    /// Whether this is something that happens rather than a time: a session that is open
    /// watches for it, the operating system's scheduler does not.
    pub fn is_trigger(&self) -> bool {
        matches!(self, When::CiFailure | When::File(_))
    }

    /// Whether a run the scheduler wakes at `now` is to go ahead. `last` is when the schedule
    /// last ran, `created` when it was added.
    pub fn due(&self, now: &Zoned, last: Option<Timestamp>, created: Timestamp) -> Due {
        match self {
            When::Cron(cron) => {
                // The newest minute that matches, among the few a late start may be late by.
                let minute = floor_minute(now.datetime());
                let hit = (0..=CRON_LATE_MINUTES)
                    .filter_map(|back| minute.checked_sub(back.minutes()).ok())
                    .find(|minute| cron.matches(*minute));
                let fresh = |hit: DateTime| {
                    last.is_none_or(|last| {
                        floor_minute(last.to_zoned(now.time_zone().clone()).datetime()) < hit
                    })
                };
                match hit {
                    Some(hit) if fresh(hit) => Due::Yes,
                    _ => Due::No,
                }
            }
            When::Every(every) => {
                let since = now.timestamp().duration_since(last.unwrap_or(created));
                let waited = Duration::try_from(since).unwrap_or_default();
                match waited + EARLY >= *every {
                    true => Due::Yes,
                    false => Due::No,
                }
            }
            When::At(at) => {
                let late = Duration::try_from(now.timestamp().duration_since(*at)).ok();
                match late {
                    _ if last.is_some() => Due::No,
                    Some(late) if late > ONCE_LATE => Due::Missed,
                    Some(_) => Due::Yes,
                    // Not there yet; a scheduler that is a few seconds early is let through.
                    None => {
                        let early =
                            Duration::try_from(at.duration_since(now.timestamp())).unwrap_or(EARLY * 2);
                        match early <= EARLY {
                            true => Due::Yes,
                            false => Due::No,
                        }
                    }
                }
            }
            When::CiFailure | When::File(_) => Due::No,
        }
    }

    /// When it runs next after `now`, for listings. `None` for what no clock decides.
    pub fn next(&self, now: &Zoned, last: Option<Timestamp>, created: Timestamp) -> Option<Zoned> {
        let zone = now.time_zone().clone();
        match self {
            When::Cron(cron) => cron
                .next_after(now.datetime())
                .and_then(|next| next.to_zoned(zone).ok()),
            When::Every(every) => {
                let span = jiff::SignedDuration::try_from(*every).ok()?;
                let mut next = last.unwrap_or(created).checked_add(span).ok()?;
                // Runs that were missed are not made up for: the next one is a whole interval on.
                while next <= now.timestamp() {
                    next = next.checked_add(span).ok()?;
                }
                Some(next.to_zoned(zone))
            }
            When::At(at) => (last.is_none() && *at > now.timestamp()).then(|| at.to_zoned(zone)),
            When::CiFailure | When::File(_) => None,
        }
    }
}

impl fmt::Display for When {
    /// The form [`When::parse`] reads back to the same value, whatever the time is.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            When::Cron(cron) => f.write_str(&cron.text),
            When::Every(every) => write!(f, "every {}", span_text(*every)),
            When::At(at) => write!(f, "at {at}"),
            When::CiFailure => f.write_str("on ci-failure"),
            When::File(glob) => write!(f, "on file:{glob}"),
        }
    }
}

impl serde::Serialize for When {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for When {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<When, D::Error> {
        let text = String::deserialize(deserializer)?;
        // What is stored names its moment in full, so the time of reading does not matter.
        When::read(&text, &Zoned::now(), true).map_err(serde::de::Error::custom)
    }
}

fn floor_minute(at: DateTime) -> DateTime {
    at.with().second(0).subsec_nanosecond(0).build().unwrap_or(at)
}

fn every(text: &str) -> Result<Duration, String> {
    let every = span(text)?;
    if every < MIN_EVERY || every > MAX_EVERY {
        return Err("`every` takes between one minute and 31 days".to_string());
    }
    if !every.as_secs().is_multiple_of(60) {
        return Err("`every` takes whole minutes: the system's scheduler counts in minutes".to_string());
    }
    Ok(every)
}

fn at(text: &str, now: &Zoned, may_have_passed: bool) -> Result<Timestamp, String> {
    let zone = now.time_zone().clone();
    let local = |at: DateTime| {
        at.to_zoned(zone.clone())
            .map(|at| at.timestamp())
            .map_err(|e| format!("`{text}` is no time here: {e}"))
    };
    let at = if let Some(ahead) = text.strip_prefix('+') {
        let ahead = jiff::SignedDuration::try_from(span(ahead)?).map_err(|e| e.to_string())?;
        now.timestamp().checked_add(ahead).map_err(|e| e.to_string())?
    } else if let Ok(at) = text.parse::<Timestamp>() {
        at
    } else if let Ok(at) = text.parse::<DateTime>() {
        local(at)?
    } else if let Ok(time) = text.parse::<jiff::civil::Time>() {
        // The next time the clock shows it: today, or tomorrow when it has passed.
        let today = now.date().to_datetime(time);
        match today > now.datetime() {
            true => local(today)?,
            false => local(today.checked_add(1.day()).map_err(|e| e.to_string())?)?,
        }
    } else {
        return Err(format!(
            "`{text}` is not a time: +2h, 17:30, 2026-12-01 09:00, or a full timestamp"
        ));
    };
    if at <= now.timestamp() && !may_have_passed {
        return Err(format!("`{text}` is in the past"));
    }
    Ok(at)
}

fn on(text: &str) -> Result<When, String> {
    if text.eq_ignore_ascii_case("ci-failure") {
        return Ok(When::CiFailure);
    }
    match text.strip_prefix("file:").map(str::trim) {
        Some("") => Err("`file:` needs a glob, such as file:src/**/*.rs".to_string()),
        Some(glob) => globset::Glob::new(glob)
            .map(|_| When::File(glob.to_string()))
            .map_err(|e| format!("`{glob}` is not a glob: {e}")),
        None => Err(format!(
            "`{text}` is nothing to wait for: ci-failure, or file:<glob>"
        )),
    }
}

/// A cron expression of five fields: minute, hour, day of the month, month, day of the week.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    /// The fields as written, one space apart.
    text: String,
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    /// Bit 0 is Sunday.
    weekdays: u64,
    /// The day of the month, or the day of the week, is `*`: when neither is, a day that matches
    /// either one counts, as cron has it.
    any_day: bool,
    any_weekday: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// One field as the set of its values. `names` are words for the values from `low` on.
fn field(text: &str, low: u32, high: u32, names: &[&str], what: &str) -> Result<u64, String> {
    let bad = |part: &str| format!("`{part}` is not a {what} of a cron expression ({low}-{high})");
    let value = |word: &str| -> Option<u32> {
        if let Some(at) = names.iter().position(|name| name.eq_ignore_ascii_case(word)) {
            return Some(low + at as u32);
        }
        word.parse().ok().filter(|value| (low..=high).contains(value))
    };
    let mut set = 0u64;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (
                range,
                step.parse::<u32>()
                    .ok()
                    .filter(|step| *step > 0)
                    .ok_or_else(|| bad(part))?,
            ),
            None => (part, 1),
        };
        let (from, to) = match range.split_once('-') {
            _ if range == "*" => (low, high),
            Some((from, to)) => (
                value(from).ok_or_else(|| bad(part))?,
                value(to).ok_or_else(|| bad(part))?,
            ),
            // `5/15` is from 5 on, every 15.
            None if part.contains('/') => (value(range).ok_or_else(|| bad(part))?, high),
            None => {
                let one = value(range).ok_or_else(|| bad(part))?;
                (one, one)
            }
        };
        if from > to {
            return Err(bad(part));
        }
        for value in (from..=to).step_by(step as usize) {
            set |= 1 << value;
        }
    }
    Ok(set)
}

impl Cron {
    pub fn parse(text: &str) -> Result<Cron, String> {
        let text = match text.trim().to_ascii_lowercase().as_str() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            _ => text,
        };
        let fields: Vec<&str> = text.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields.as_slice() else {
            return Err(format!(
                "`{text}` is not a time to run at: five cron fields (minute hour day month weekday, such as \
                 \"0 9 * * 1-5\"), `every 10m`, `at +2h`, or with --on a trigger"
            ));
        };
        let mut weekdays = field(weekday, 0, 7, &WEEKDAYS, "day of the week")?;
        // 7 is Sunday too.
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays & !(1 << 7)) | 1;
        }
        Ok(Cron {
            text: fields.join(" "),
            minutes: field(minute, 0, 59, &[], "minute")?,
            hours: field(hour, 0, 23, &[], "hour")?,
            days: field(day, 1, 31, &[], "day of the month")?,
            months: field(month, 1, 12, &MONTHS, "month")?,
            weekdays,
            any_day: day.starts_with('*'),
            any_weekday: weekday.starts_with('*'),
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn day_matches(&self, date: Date) -> bool {
        if self.months & (1 << date.month()) == 0 {
            return false;
        }
        let day = self.days & (1 << date.day()) != 0;
        let weekday = self.weekdays & (1 << date.weekday().to_sunday_zero_offset()) != 0;
        match (self.any_day, self.any_weekday) {
            (false, false) => day || weekday,
            _ => day && weekday,
        }
    }

    /// Whether the expression matches the minute `at` lies in.
    pub fn matches(&self, at: DateTime) -> bool {
        self.minutes & (1 << at.minute()) != 0
            && self.hours & (1 << at.hour()) != 0
            && self.day_matches(at.date())
    }

    /// The times of a day the expression matches on a day it matches, in order.
    pub fn times_of_day(&self) -> Vec<(u8, u8)> {
        (0..24u8)
            .filter(|hour| self.hours & (1 << hour) != 0)
            .flat_map(|hour| {
                (0..60u8)
                    .filter(|minute| self.minutes & (1 << minute) != 0)
                    .map(move |minute| (hour, minute))
            })
            .collect()
    }

    /// The values of each field that is not `*`: minute, hour, day of the month, month, day of
    /// the week (0 is Sunday).
    pub fn restricted(&self) -> [Option<Vec<u8>>; 5] {
        let values = |set: u64, low: u8, high: u8, any: bool| {
            let all: Vec<u8> = (low..=high).filter(|value| set & (1 << value) != 0).collect();
            (!any && all.len() < (high - low + 1) as usize).then_some(all)
        };
        [
            values(self.minutes, 0, 59, false),
            values(self.hours, 0, 23, false),
            values(self.days, 1, 31, self.any_day),
            values(self.months, 1, 12, false),
            values(self.weekdays, 0, 6, self.any_weekday),
        ]
    }

    /// The first minute after `after` the expression matches, within the next five years.
    pub fn next_after(&self, after: DateTime) -> Option<DateTime> {
        let times = self.times_of_day();
        let start = floor_minute(after);
        let mut date = start.date();
        for _ in 0..(366 * 5) {
            if self.day_matches(date) {
                let found = times
                    .iter()
                    .map(|(hour, minute)| date.at(*hour as i8, *minute as i8, 0, 0))
                    .find(|at| *at > start);
                if found.is_some() {
                    return found;
                }
            }
            date = date.tomorrow().ok()?;
        }
        None
    }
}

/// A moment as the listings show it: the local date and time to the minute.
pub fn shown(at: &Zoned) -> String {
    at.strftime("%Y-%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn zoned(text: &str) -> Zoned {
        format!("{text}[UTC]").parse().unwrap()
    }

    fn civil(text: &str) -> DateTime {
        text.parse().unwrap()
    }

    #[test]
    fn schedules_are_read_as_typed_and_written_back_the_same() {
        let now = zoned("2026-11-13T10:00:00+00:00");
        let parse = |text: &str| When::parse(text, &now);
        assert_eq!(parse("every 10m"), Ok(When::Every(Duration::from_secs(600))));
        assert_eq!(parse("every 1h30m"), Ok(When::Every(Duration::from_secs(5400))));
        assert_eq!(parse("Every 2 hours"), Ok(When::Every(Duration::from_secs(7200))));
        assert_eq!(parse("every day"), Ok(When::Every(Duration::from_secs(86_400))));
        assert_eq!(
            parse("at +2h"),
            Ok(When::At("2026-11-13T12:00:00Z".parse().unwrap()))
        );
        // A time of day is the next time the clock shows it.
        assert_eq!(
            parse("at 17:30"),
            Ok(When::At("2026-11-13T17:30:00Z".parse().unwrap()))
        );
        assert_eq!(
            parse("at 09:00"),
            Ok(When::At("2026-11-14T09:00:00Z".parse().unwrap()))
        );
        assert_eq!(
            parse("at 2026-12-01 09:00"),
            Ok(When::At("2026-12-01T09:00:00Z".parse().unwrap()))
        );
        assert_eq!(parse("on ci-failure"), Ok(When::CiFailure));
        assert_eq!(
            parse("on file:src/**/*.rs"),
            Ok(When::File("src/**/*.rs".to_string()))
        );
        for text in [
            "every 90m",
            "every 2h",
            "every 3d",
            "at 2026-11-13T12:00:00Z",
            "on ci-failure",
            "on file:docs/*.md",
            "*/15 9-17 * * 1-5",
        ] {
            assert_eq!(parse(text).unwrap().to_string(), text);
        }
        assert_eq!(
            parse("  0   9 * *   mon-fri ").unwrap().to_string(),
            "0 9 * * mon-fri"
        );
    }

    #[test]
    fn a_stored_moment_is_read_back_after_it_has_passed() {
        let passed = When::At("2020-01-01T10:00:00Z".parse().unwrap());
        let stored = serde_json::to_string(&passed).unwrap();
        assert_eq!(stored, "\"at 2020-01-01T10:00:00Z\"");
        assert_eq!(serde_json::from_str::<When>(&stored).unwrap(), passed);
        let cron: When = serde_json::from_str("\"0 9 * * 1-5\"").unwrap();
        assert_eq!(serde_json::to_string(&cron).unwrap(), "\"0 9 * * 1-5\"");
        assert!(serde_json::from_str::<When>("\"whenever\"").is_err());
    }

    #[test]
    fn what_cannot_be_scheduled_says_why() {
        let now = zoned("2026-11-13T10:00:00+00:00");
        let error = |text: &str| When::parse(text, &now).unwrap_err();
        assert!(error("every 30s").contains("between one minute and 31 days"));
        assert!(error("every 90s").contains("whole minutes"));
        assert!(error("every 40d").contains("31 days"));
        assert!(error("every soon").contains("not a length of time"));
        assert!(error("at 2020-01-01 10:00").contains("in the past"));
        assert!(error("at noonish").contains("not a time"));
        assert!(error("on tuesday").contains("ci-failure"));
        assert!(error("on file:").contains("needs a glob"));
        assert!(error("0 9 * *").contains("five cron fields"));
        assert!(error("61 9 * * *").contains("minute"));
        assert!(error("0 9 * * 8").contains("day of the week"));
        assert!(error("0 9 */0 * *").contains("day of the month"));
        assert!(error("0 9 * 5-2 *").contains("month"));
    }

    #[test]
    fn a_cron_expression_matches_its_minutes_only() {
        let weekdays = Cron::parse("*/15 9-17 * * 1-5").unwrap();
        // 2026-11-13 is a Friday.
        assert!(weekdays.matches(civil("2026-11-13T09:00:00")));
        assert!(weekdays.matches(civil("2026-11-13T17:45:59")));
        assert!(!weekdays.matches(civil("2026-11-13T18:00:00")));
        assert!(!weekdays.matches(civil("2026-11-13T09:07:00")));
        assert!(!weekdays.matches(civil("2026-11-14T09:00:00")));
        // Both days given: either one counts.
        let either = Cron::parse("0 0 1 * sun").unwrap();
        assert!(either.matches(civil("2026-11-01T00:00:00")));
        assert!(either.matches(civil("2026-11-15T00:00:00")));
        assert!(!either.matches(civil("2026-11-16T00:00:00")));
        // 7 and names.
        let sunday = Cron::parse("30 6 * jan,jul 7").unwrap();
        assert!(sunday.matches(civil("2027-01-03T06:30:00")));
        assert!(!sunday.matches(civil("2026-11-15T06:30:00")));
        assert_eq!(Cron::parse("@daily").unwrap().text(), "0 0 * * *");
        assert_eq!(
            Cron::parse("5/20 8,20 * * *").unwrap().times_of_day(),
            [(8, 5), (8, 25), (8, 45), (20, 5), (20, 25), (20, 45)]
        );
    }

    #[test]
    fn the_next_run_of_a_cron_expression_is_found_across_days_and_years() {
        let next = |cron: &str, after: &str| Cron::parse(cron).unwrap().next_after(civil(after));
        assert_eq!(
            next("0 9 * * 1-5", "2026-11-13T09:00:00"),
            Some(civil("2026-11-16T09:00:00"))
        );
        assert_eq!(
            next("*/15 * * * *", "2026-11-13T09:07:30"),
            Some(civil("2026-11-13T09:15:00"))
        );
        assert_eq!(
            next("0 0 29 2 *", "2026-11-13T00:00:00"),
            Some(civil("2028-02-29T00:00:00"))
        );
        assert_eq!(next("0 0 31 2 *", "2026-11-13T00:00:00"), None);
    }

    #[test]
    fn a_run_is_due_in_its_minute_and_a_missed_one_is_not_caught_up() {
        let created: Timestamp = "2026-11-13T08:00:00Z".parse().unwrap();
        let stamp = |text: &str| -> Option<Timestamp> { Some(text.parse().unwrap()) };

        let cron = When::parse("0 9 * * *", &zoned("2026-11-13T08:00:00+00:00")).unwrap();
        let due = |now: &str, last: Option<Timestamp>| cron.due(&zoned(now), last, created);
        assert_eq!(due("2026-11-13T09:00:04+00:00", None), Due::Yes);
        // The scheduler started it late, but within reach.
        assert_eq!(due("2026-11-13T09:02:30+00:00", None), Due::Yes);
        // The machine woke up half an hour on: that run is gone.
        assert_eq!(due("2026-11-13T09:30:00+00:00", None), Due::No);
        assert_eq!(due("2026-11-13T08:59:00+00:00", None), Due::No);
        // Woken twice in the same minute, it runs once.
        assert_eq!(
            due("2026-11-13T09:00:50+00:00", stamp("2026-11-13T09:00:05Z")),
            Due::No
        );
        assert_eq!(
            due("2026-11-14T09:00:01+00:00", stamp("2026-11-13T09:00:05Z")),
            Due::Yes
        );

        let every = When::Every(Duration::from_secs(600));
        let due = |now: &str, last: Option<Timestamp>| every.due(&zoned(now), last, created);
        assert_eq!(due("2026-11-13T08:05:00+00:00", None), Due::No);
        assert_eq!(due("2026-11-13T08:09:50+00:00", None), Due::Yes);
        assert_eq!(
            due("2026-11-13T08:15:00+00:00", stamp("2026-11-13T08:10:00Z")),
            Due::No
        );
        assert_eq!(
            due("2026-11-13T08:20:00+00:00", stamp("2026-11-13T08:10:00Z")),
            Due::Yes
        );

        let once = When::At("2026-11-13T12:00:00Z".parse().unwrap());
        let due = |now: &str, last: Option<Timestamp>| once.due(&zoned(now), last, created);
        assert_eq!(due("2026-11-13T11:00:00+00:00", None), Due::No);
        assert_eq!(due("2026-11-13T11:59:50+00:00", None), Due::Yes);
        assert_eq!(due("2026-11-13T12:03:00+00:00", None), Due::Yes);
        assert_eq!(due("2026-11-13T13:00:00+00:00", None), Due::Missed);
        assert_eq!(
            due("2026-11-13T12:03:00+00:00", stamp("2026-11-13T12:00:01Z")),
            Due::No
        );
    }

    #[test]
    fn the_next_run_skips_what_was_missed() {
        let created: Timestamp = "2026-11-13T08:00:00Z".parse().unwrap();
        let now = zoned("2026-11-13T10:25:00+00:00");
        let every = When::Every(Duration::from_secs(3600));
        assert_eq!(
            every.next(&now, None, created).map(|next| next.timestamp()),
            Some("2026-11-13T11:00:00Z".parse().unwrap())
        );
        let cron = When::Cron(Cron::parse("0 9 * * *").unwrap());
        assert_eq!(
            cron.next(&now, None, created).map(|next| shown(&next)),
            Some("2026-11-14 09:00".to_string())
        );
        assert_eq!(When::CiFailure.next(&now, None, created), None);
        let once = When::At("2026-11-13T09:00:00Z".parse().unwrap());
        assert_eq!(once.next(&now, None, created), None);
    }
}
