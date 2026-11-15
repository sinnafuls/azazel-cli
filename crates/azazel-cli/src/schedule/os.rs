//! Schedules in the scheduler of the operating system: a task of Task Scheduler on Windows, a
//! line of the user's crontab on Linux, a launch agent on macOS. Each of them starts
//! `azazel schedule run <id> --scheduled` at the times the schedule may be due; azazel itself
//! decides whether it is (see [`super::when::When::due`]), so an entry may wake it more often
//! than the schedule runs where the scheduler cannot say the schedule exactly.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use jiff::ToSpan;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;

use super::when::{Cron, When};

/// The most triggers one task of Task Scheduler takes.
const MAX_TRIGGERS: usize = 48;
/// The most calendar entries a launch agent is given before it is woken on a plain interval.
const MAX_CALENDAR: usize = 200;

/// What the scheduler is to start, and when.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub when: When,
    /// When the schedule was made, in local time: intervals count from it.
    pub anchor: DateTime,
    /// The zone the scheduler keeps time in.
    pub zone: TimeZone,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// A program that runs `program` without a console window (Windows), when there is one.
    pub wrapper: Option<PathBuf>,
    /// One line that says what the entry is for.
    pub about: String,
}

/// The name of the task, the tag of the crontab line, the label of the launch agent.
pub fn task_name(id: &str) -> String {
    format!(r"azazel\{id}")
}

fn cron_tag(id: &str) -> String {
    format!("# azazel-schedule:{id}")
}

fn agent_label(id: &str) -> String {
    format!("dev.azazel.schedule.{id}")
}

fn gcd(a: u32, b: u32) -> u32 {
    match b {
        0 => a,
        b => gcd(b, a % b),
    }
}

/// The widest step, in minutes, that lands on every time of day in `times`, on the same
/// minutes of every hour: such a step stays in place when the clock moves by an hour.
fn step_of(times: &[(u8, u8)]) -> u32 {
    times
        .iter()
        .map(|(hour, minute)| *hour as u32 * 60 + *minute as u32)
        .fold(60, gcd)
        .max(1)
}

/// When Task Scheduler starts the task.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Wake {
    /// From `start` on, every so many minutes.
    Repeat {
        start: DateTime,
        minutes: u32,
    },
    /// At these times of every day.
    Daily(Vec<(u8, u8)>),
    Once(DateTime),
}

fn wake(job: &Job) -> Result<Wake, String> {
    let midnight = job.anchor.date().at(0, 0, 0, 0);
    match &job.when {
        When::Every(every) => {
            let minutes = (every.as_secs() / 60) as u32;
            let start = job
                .anchor
                .checked_add((minutes as i64).minutes())
                .map_err(|e| e.to_string())?;
            Ok(Wake::Repeat { start, minutes })
        }
        When::Cron(cron) => {
            let times = cron.times_of_day();
            Ok(match times.len() <= MAX_TRIGGERS {
                true => Wake::Daily(times),
                false => Wake::Repeat {
                    start: midnight,
                    minutes: step_of(&times),
                },
            })
        }
        When::At(at) => Ok(Wake::Once(at.to_zoned(job.zone.clone()).datetime())),
        When::CiFailure | When::File(_) | When::PrComment | When::Slack(_) => {
            Err("a trigger is watched by an open session, not by the scheduler".to_string())
        }
    }
}

fn xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn boundary(at: DateTime) -> String {
    at.strftime("%Y-%m-%dT%H:%M:%S").to_string()
}

/// A number of minutes as Task Scheduler writes a length of time.
fn iso_minutes(minutes: u32) -> String {
    match minutes {
        m if m.is_multiple_of(1440) => format!("P{}D", m / 1440),
        m if m.is_multiple_of(60) => format!("PT{}H", m / 60),
        m => format!("PT{m}M"),
    }
}

/// The task as the XML `schtasks /Create /XML` reads. A run that was missed is not made up for
/// (`StartWhenAvailable` is off), and a run that would start while the one before it still
/// starts is dropped.
pub fn task_xml(job: &Job) -> Result<String, String> {
    let triggers = match wake(job)? {
        Wake::Repeat { start, minutes } => format!(
            "    <TimeTrigger>\n      <Repetition>\n        <Interval>{}</Interval>\n        \
             <StopAtDurationEnd>false</StopAtDurationEnd>\n      </Repetition>\n      \
             <StartBoundary>{}</StartBoundary>\n      <Enabled>true</Enabled>\n    </TimeTrigger>\n",
            iso_minutes(minutes),
            boundary(start)
        ),
        Wake::Daily(times) => times
            .iter()
            .map(|(hour, minute)| {
                format!(
                    "    <CalendarTrigger>\n      <StartBoundary>{}</StartBoundary>\n      \
                     <Enabled>true</Enabled>\n      <ScheduleByDay>\n        <DaysInterval>1</DaysInterval>\n      \
                     </ScheduleByDay>\n    </CalendarTrigger>\n",
                    boundary(job.anchor.date().at(*hour as i8, *minute as i8, 0, 0))
                )
            })
            .collect(),
        Wake::Once(at) => format!(
            "    <TimeTrigger>\n      <StartBoundary>{}</StartBoundary>\n      <Enabled>true</Enabled>\n    \
             </TimeTrigger>\n",
            boundary(at)
        ),
    };
    let own: Vec<String> = std::iter::once(job.program.to_string_lossy().into_owned())
        .chain(job.args.iter().cloned())
        .collect();
    let (command, arguments) = match &job.wrapper {
        Some(wrapper) => (
            wrapper.to_string_lossy().into_owned(),
            std::iter::once("--headless".to_string())
                .chain(own.iter().map(|arg| crate::detach::quoted(arg)))
                .collect::<Vec<String>>()
                .join(" "),
        ),
        None => (
            own[0].clone(),
            own[1..]
                .iter()
                .map(|arg| crate::detach::quoted(arg))
                .collect::<Vec<String>>()
                .join(" "),
        ),
    };
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
         <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n  \
         <RegistrationInfo>\n    <Description>{about}</Description>\n  </RegistrationInfo>\n  \
         <Triggers>\n{triggers}  </Triggers>\n  \
         <Principals>\n    <Principal id=\"Author\">\n      <LogonType>InteractiveToken</LogonType>\n      \
         <RunLevel>LeastPrivilege</RunLevel>\n    </Principal>\n  </Principals>\n  \
         <Settings>\n    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n    \
         <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n    \
         <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n    \
         <StartWhenAvailable>false</StartWhenAvailable>\n    \
         <ExecutionTimeLimit>PT10M</ExecutionTimeLimit>\n    <Enabled>true</Enabled>\n  </Settings>\n  \
         <Actions Context=\"Author\">\n    <Exec>\n      <Command>{command}</Command>\n      \
         <Arguments>{arguments}</Arguments>\n    </Exec>\n  </Actions>\n</Task>\n",
        about = xml_text(&job.about),
        command = xml_text(&command),
        arguments = xml_text(&arguments),
    ))
}

/// One argument as a POSIX shell reads it back.
fn sh_quoted(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=+,@".contains(c));
    match plain {
        true => arg.to_string(),
        false => format!("'{}'", arg.replace('\'', r"'\''")),
    }
}

/// The five fields that wake a run often enough for `when`. Where cron cannot say an interval,
/// the fields name the widest step that divides it.
fn cron_fields(job: &Job) -> Result<String, String> {
    match &job.when {
        When::Cron(cron) => Ok(cron.text().to_string()),
        When::Every(every) => {
            let minutes = (every.as_secs() / 60) as u32;
            let (hour, minute) = (job.anchor.hour() as u32, job.anchor.minute() as u32);
            // The steps cron can say: parts of an hour, parts of a day, a day.
            let steps = [
                1440, 720, 480, 360, 240, 180, 120, 60, 30, 20, 15, 12, 10, 6, 5, 4, 3, 2, 1,
            ];
            let step = steps
                .into_iter()
                .find(|step| minutes.is_multiple_of(*step))
                .unwrap_or(1);
            Ok(match step {
                1 => "* * * * *".to_string(),
                1440 => format!("{minute} {hour} * * *"),
                step if step >= 60 => {
                    let hours = step / 60;
                    format!("{minute} {}-23/{hours} * * *", hour % hours)
                }
                step => format!("{}-59/{step} * * * *", minute % step),
            })
        }
        When::At(at) => {
            let at = at.to_zoned(job.zone.clone());
            Ok(format!(
                "{} {} {} {} *",
                at.minute(),
                at.hour(),
                at.day(),
                at.month()
            ))
        }
        When::CiFailure | When::File(_) | When::PrComment | When::Slack(_) => {
            Err("a trigger is watched by an open session, not by cron".to_string())
        }
    }
}

/// The line of the user's crontab that runs the job.
pub fn crontab_line(job: &Job) -> Result<String, String> {
    let command: Vec<String> = std::iter::once(job.program.to_string_lossy().into_owned())
        .chain(job.args.iter().cloned())
        .map(|arg| sh_quoted(&arg))
        .collect();
    // A percent sign ends the command in a crontab.
    let command = command.join(" ").replace('%', r"\%");
    Ok(format!(
        "{} {command} >/dev/null 2>&1 {}",
        cron_fields(job)?,
        cron_tag(&job.id)
    ))
}

/// A crontab without the line of schedule `id`, and with `line` in its place when one is given.
pub fn crontab_with(existing: &str, id: &str, line: Option<&str>) -> String {
    let tag = cron_tag(id);
    let mut lines: Vec<&str> = existing
        .lines()
        .filter(|line| !line.trim_end().ends_with(&tag))
        .collect();
    lines.extend(line);
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

/// Every combination of the values of the fields that are not `*`, as launchd's calendar
/// entries: `(key, value)` pairs.
fn calendar(cron: &Cron) -> Vec<Vec<(&'static str, u8)>> {
    let [minutes, hours, days, months, weekdays] = cron.restricted();
    let product = |fields: &[(&'static str, &Option<Vec<u8>>)]| {
        let mut entries: Vec<Vec<(&'static str, u8)>> = vec![Vec::new()];
        for (key, values) in fields {
            let Some(values) = values else { continue };
            entries = entries
                .iter()
                .flat_map(|entry| {
                    values.iter().map(move |value| {
                        let mut entry = entry.clone();
                        entry.push((*key, *value));
                        entry
                    })
                })
                .collect();
        }
        entries
    };
    let time = [("Minute", &minutes), ("Hour", &hours), ("Month", &months)];
    match (&days, &weekdays) {
        // Either kind of day counts: one set of entries for each.
        (Some(_), Some(_)) => {
            let mut by_day = product(&[time[0], time[1], time[2], ("Day", &days)]);
            by_day.extend(product(&[time[0], time[1], time[2], ("Weekday", &weekdays)]));
            by_day
        }
        _ => product(&[time[0], time[1], time[2], ("Day", &days), ("Weekday", &weekdays)]),
    }
}

/// The launch agent that runs the job, as a property list.
pub fn launchd_plist(job: &Job) -> Result<String, String> {
    let entry = |pairs: &[(&str, u8)]| {
        let body: String = pairs
            .iter()
            .map(|(key, value)| format!("      <key>{key}</key>\n      <integer>{value}</integer>\n"))
            .collect();
        format!("    <dict>\n{body}    </dict>\n")
    };
    let interval = |seconds: u64| format!("  <key>StartInterval</key>\n  <integer>{seconds}</integer>\n");
    let start = match &job.when {
        When::Every(every) => interval(every.as_secs()),
        When::Cron(cron) => {
            let entries = calendar(cron);
            match entries.len() <= MAX_CALENDAR {
                true => format!(
                    "  <key>StartCalendarInterval</key>\n  <array>\n{}  </array>\n",
                    entries.iter().map(|pairs| entry(pairs)).collect::<String>()
                ),
                false => interval(step_of(&cron.times_of_day()) as u64 * 60),
            }
        }
        When::At(at) => {
            let at = at.to_zoned(job.zone.clone());
            format!(
                "  <key>StartCalendarInterval</key>\n  <array>\n{}  </array>\n",
                entry(&[
                    ("Minute", at.minute() as u8),
                    ("Hour", at.hour() as u8),
                    ("Day", at.day() as u8),
                    ("Month", at.month() as u8),
                ])
            )
        }
        When::CiFailure | When::File(_) | When::PrComment | When::Slack(_) => {
            return Err("a trigger is watched by an open session, not by launchd".to_string());
        }
    };
    let arguments: String = std::iter::once(job.program.to_string_lossy().into_owned())
        .chain(job.args.iter().cloned())
        .map(|arg| format!("    <string>{}</string>\n", xml_text(&arg)))
        .collect();
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{label}</string>\n  \
         <key>ProgramArguments</key>\n  <array>\n{arguments}  </array>\n{start}  \
         <key>RunAtLoad</key>\n  <false/>\n  <key>ProcessType</key>\n  <string>Background</string>\n\
         </dict>\n</plist>\n",
        label = agent_label(&job.id),
    ))
}

/// Which scheduler this build registers schedules with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    TaskScheduler,
    Cron,
    Launchd,
}

impl Kind {
    pub const HERE: Kind = if cfg!(windows) {
        Kind::TaskScheduler
    } else if cfg!(target_os = "macos") {
        Kind::Launchd
    } else {
        Kind::Cron
    };

    pub fn name(self) -> &'static str {
        match self {
            Kind::TaskScheduler => "Task Scheduler",
            Kind::Cron => "cron",
            Kind::Launchd => "launchd",
        }
    }

    fn program(self) -> &'static str {
        match self {
            Kind::TaskScheduler => "schtasks",
            Kind::Cron => "crontab",
            Kind::Launchd => "launchctl",
        }
    }
}

/// The program that talks to the scheduler. `Err` says what is missing.
pub fn scheduler() -> Result<PathBuf, String> {
    let kind = Kind::HERE;
    which::which(kind.program()).map_err(|_| {
        format!(
            "`{}` is not on PATH, so schedules cannot be handed to {}",
            kind.program(),
            kind.name()
        )
    })
}

/// Where the launch agents of this user are kept.
fn agents_dir() -> Result<PathBuf, String> {
    std::env::home_dir()
        .map(|home| home.join("Library").join("LaunchAgents"))
        .ok_or_else(|| "this user has no home directory to keep a launch agent in".to_string())
}

fn agent_file(id: &str) -> Result<PathBuf, String> {
    Ok(agents_dir()?.join(format!("{}.plist", agent_label(id))))
}

fn run(command: &mut Command, input: Option<&str>) -> Result<Output, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        std::io::Write::write_all(&mut stdin, input.as_bytes()).map_err(|e| format!("{program}: {e}"))?;
    }
    child.wait_with_output().map_err(|e| format!("{program}: {e}"))
}

/// The output of a command that has to succeed.
fn checked(command: &mut Command, input: Option<&str>) -> Result<String, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = run(command, input)?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        let said = match said.trim() {
            "" => String::from_utf8_lossy(&output.stdout).trim().to_string(),
            said => said.to_string(),
        };
        return Err(format!("{program} failed: {said}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The user's crontab; empty when there is none.
fn crontab(program: &Path) -> String {
    run(Command::new(program).arg("-l"), None)
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
}

/// Hands the job to the scheduler of this system, in place of an entry of the same id.
pub fn register(job: &Job) -> Result<(), String> {
    let program = scheduler()?;
    match Kind::HERE {
        Kind::TaskScheduler => {
            let xml = task_xml(job)?;
            // Task Scheduler reads the file in the encoding its first line names.
            let mut bytes = vec![0xFF, 0xFE];
            bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
            // Closed before schtasks opens it: Windows lets no second program read a file that
            // is open for writing.
            let file = tempfile::Builder::new()
                .prefix("azazel-task-")
                .suffix(".xml")
                .tempfile()
                .and_then(|mut file| {
                    std::io::Write::write_all(&mut file, &bytes)?;
                    Ok(file.into_temp_path())
                })
                .map_err(|e| format!("cannot write the task: {e}"))?;
            checked(
                Command::new(program)
                    .args(["/Create", "/TN", &task_name(&job.id), "/F", "/XML"])
                    .arg(&file),
                None,
            )
            .map(drop)
        }
        Kind::Cron => {
            let table = crontab_with(&crontab(&program), &job.id, Some(&crontab_line(job)?));
            checked(Command::new(program).arg("-"), Some(&table)).map(drop)
        }
        Kind::Launchd => {
            let file = agent_file(&job.id)?;
            if file.exists() {
                let _ = run(Command::new(&program).arg("unload").arg(&file), None);
            }
            let plist = launchd_plist(job)?;
            let written = file
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&file, plist.as_bytes()));
            written.map_err(|e| format!("cannot write {}: {e}", file.display()))?;
            checked(Command::new(program).args(["load", "-w"]).arg(&file), None).map(drop)
        }
    }
}

/// Takes the entry of schedule `id` out of the scheduler. An entry that is not there is none
/// to take out.
pub fn unregister(id: &str) -> Result<(), String> {
    let program = scheduler()?;
    match Kind::HERE {
        Kind::TaskScheduler => {
            let name = task_name(id);
            let known = run(Command::new(&program).args(["/Query", "/TN", &name]), None)?;
            if !known.status.success() {
                return Ok(());
            }
            checked(Command::new(program).args(["/Delete", "/TN", &name, "/F"]), None).map(drop)
        }
        Kind::Cron => {
            let existing = crontab(&program);
            let table = crontab_with(&existing, id, None);
            if table == existing {
                return Ok(());
            }
            checked(Command::new(program).arg("-"), Some(&table)).map(drop)
        }
        Kind::Launchd => {
            let file = agent_file(id)?;
            if !file.exists() {
                return Ok(());
            }
            let _ = run(Command::new(program).args(["unload", "-w"]).arg(&file), None);
            std::fs::remove_file(&file).map_err(|e| format!("cannot remove {}: {e}", file.display()))
        }
    }
}

/// A program that runs another without a console window, on Windows: a task started in the
/// user's session would otherwise flash one every time it runs.
pub fn headless_wrapper() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let root = std::env::var_os("SystemRoot")?;
    let conhost = Path::new(&root).join("System32").join("conhost.exe");
    conhost.is_file().then_some(conhost)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn job(when: &str) -> Job {
        let now: jiff::Zoned = "2026-11-13T10:17:00+00:00[UTC]".parse().unwrap();
        Job {
            id: "ab12cd34".to_string(),
            when: When::parse(when, &now).unwrap(),
            anchor: now.datetime(),
            zone: TimeZone::UTC,
            program: PathBuf::from(r"C:\Program Files\azazel\azazel.exe"),
            args: [
                "--cwd",
                r"C:\my work\app",
                "schedule",
                "run",
                "ab12cd34",
                "--scheduled",
            ]
            .map(String::from)
            .to_vec(),
            wrapper: None,
            about: "azazel schedule ab12cd34: \"check <deps> & report\"".to_string(),
        }
    }

    fn between<'a>(text: &'a str, open: &str, close: &str) -> Vec<&'a str> {
        text.split(open)
            .skip(1)
            .filter_map(|rest| rest.split(close).next())
            .collect()
    }

    #[test]
    fn a_task_repeats_an_interval_from_its_first_run_on() {
        let xml = task_xml(&job("every 90m")).unwrap();
        assert_eq!(between(&xml, "<Interval>", "</Interval>"), ["PT90M"]);
        assert_eq!(
            between(&xml, "<StartBoundary>", "</StartBoundary>"),
            ["2026-11-13T11:47:00"]
        );
        assert_eq!(
            between(&task_xml(&job("every 2h")).unwrap(), "<Interval>", "</Interval>"),
            ["PT2H"]
        );
        assert_eq!(
            between(&task_xml(&job("every 3d")).unwrap(), "<Interval>", "</Interval>"),
            ["P3D"]
        );
        // What was missed is not run later, and one run at a time.
        assert!(xml.contains("<StartWhenAvailable>false</StartWhenAvailable>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
    }

    #[test]
    fn a_task_runs_the_program_with_its_arguments_quoted_and_escaped() {
        let mut job = job("every 10m");
        let xml = task_xml(&job).unwrap();
        assert_eq!(
            between(&xml, "<Command>", "</Command>"),
            [r"C:\Program Files\azazel\azazel.exe"]
        );
        assert_eq!(
            between(&xml, "<Arguments>", "</Arguments>"),
            [r"--cwd &quot;C:\my work\app&quot; schedule run ab12cd34 --scheduled"]
        );
        assert_eq!(
            between(&xml, "<Description>", "</Description>"),
            ["azazel schedule ab12cd34: &quot;check &lt;deps&gt; &amp; report&quot;"]
        );
        // Without a console window where the system has the means.
        job.wrapper = Some(PathBuf::from(r"C:\Windows\System32\conhost.exe"));
        let xml = task_xml(&job).unwrap();
        assert_eq!(
            between(&xml, "<Command>", "</Command>"),
            [r"C:\Windows\System32\conhost.exe"]
        );
        assert_eq!(
            between(&xml, "<Arguments>", "</Arguments>"),
            [
                r"--headless &quot;C:\Program Files\azazel\azazel.exe&quot; --cwd &quot;C:\my work\app&quot; schedule run ab12cd34 --scheduled"
            ]
        );
    }

    #[test]
    fn a_task_wakes_a_cron_expression_at_its_times_of_day_or_on_their_common_step() {
        let xml = task_xml(&job("30 8,20 * * 1-5")).unwrap();
        assert_eq!(
            between(&xml, "<StartBoundary>", "</StartBoundary>"),
            ["2026-11-13T08:30:00", "2026-11-13T20:30:00"]
        );
        assert_eq!(between(&xml, "<DaysInterval>", "</DaysInterval>"), ["1", "1"]);
        // More times than a task has triggers: one trigger that repeats.
        let xml = task_xml(&job("*/15 * * * *")).unwrap();
        assert_eq!(between(&xml, "<Interval>", "</Interval>"), ["PT15M"]);
        assert_eq!(
            between(&xml, "<StartBoundary>", "</StartBoundary>"),
            ["2026-11-13T00:00:00"]
        );
        // A step that would drift off the minutes when the clock moves an hour is narrowed.
        let xml = task_xml(&job("0,8,16,24,32,40,48,56 */2 * * *")).unwrap();
        assert_eq!(between(&xml, "<Interval>", "</Interval>"), ["PT4M"]);
        let xml = task_xml(&job("at 2026-12-01 09:05")).unwrap();
        assert_eq!(
            between(&xml, "<StartBoundary>", "</StartBoundary>"),
            ["2026-12-01T09:05:00"]
        );
        assert!(!xml.contains("<Repetition>"));
        assert!(task_xml(&job("on ci-failure")).is_err());
    }

    #[test]
    fn a_crontab_line_wakes_the_schedule_often_enough() {
        let fields = |when: &str| cron_fields(&job(when)).unwrap();
        assert_eq!(fields("0 9 * * 1-5"), "0 9 * * 1-5");
        assert_eq!(fields("every 10m"), "7-59/10 * * * *");
        assert_eq!(fields("every 1h"), "17 0-23/1 * * *");
        assert_eq!(fields("every 2h"), "17 0-23/2 * * *");
        assert_eq!(fields("every 3h"), "17 1-23/3 * * *");
        assert_eq!(fields("every 1d"), "17 10 * * *");
        assert_eq!(fields("every 3d"), "17 10 * * *");
        // No step of cron's is 45 minutes or 7: the widest one that divides it.
        assert_eq!(fields("every 45m"), "2-59/15 * * * *");
        assert_eq!(fields("every 7m"), "* * * * *");
        assert_eq!(fields("at 2026-12-01 09:05"), "5 9 1 12 *");
    }

    #[test]
    fn a_crontab_keeps_its_other_lines() {
        let mut job = job("every 10m");
        job.program = PathBuf::from("/opt/my tools/azazel");
        job.args = ["--cwd", "/work/50%", "schedule", "run", "ab12cd34", "--scheduled"]
            .map(String::from)
            .to_vec();
        let line = crontab_line(&job).unwrap();
        assert_eq!(
            line,
            r"7-59/10 * * * * '/opt/my tools/azazel' --cwd '/work/50\%' schedule run ab12cd34 --scheduled >/dev/null 2>&1 # azazel-schedule:ab12cd34"
        );
        let existing = "MAILTO=me\n0 4 * * * backup\n";
        let added = crontab_with(existing, "ab12cd34", Some(&line));
        assert_eq!(added, format!("{existing}{line}\n"));
        // Added again it replaces its own line; taken out, the table is as it was.
        assert_eq!(crontab_with(&added, "ab12cd34", Some(&line)), added);
        assert_eq!(crontab_with(&added, "ab12cd34", None), existing);
        assert_eq!(crontab_with("", "ab12cd34", None), "");
    }

    #[test]
    fn a_launch_agent_starts_on_an_interval_or_on_calendar_entries() {
        let plist = launchd_plist(&job("every 10m")).unwrap();
        assert!(plist.contains("<key>StartInterval</key>\n  <integer>600</integer>"));
        assert!(plist.contains("<string>dev.azazel.schedule.ab12cd34</string>"));
        assert_eq!(
            between(&plist, "<string>", "</string>")[1..7],
            [
                r"C:\Program Files\azazel\azazel.exe",
                "--cwd",
                r"C:\my work\app",
                "schedule",
                "run",
                "ab12cd34"
            ]
        );
        let plist = launchd_plist(&job("0 9 * * 1-2")).unwrap();
        let entries = between(&plist, "<dict>\n      ", "    </dict>");
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[1],
            "<key>Minute</key>\n      <integer>0</integer>\n      <key>Hour</key>\n      \
             <integer>9</integer>\n      <key>Weekday</key>\n      <integer>2</integer>\n"
        );
        // Both kinds of day: an entry for each, as cron runs on either.
        let plist = launchd_plist(&job("0 0 1 * sun")).unwrap();
        let entries = between(&plist, "<dict>\n      ", "    </dict>");
        assert_eq!(entries.len(), 2);
        assert!(entries[0].contains("<key>Day</key>") && entries[1].contains("<key>Weekday</key>"));
        // Too many entries to list: woken on the step, and azazel decides.
        let plist = launchd_plist(&job("*/5 * 1-20 * *")).unwrap();
        assert!(plist.contains("<key>StartInterval</key>\n  <integer>300</integer>"));
        let plist = launchd_plist(&job("at 2026-12-01 09:05")).unwrap();
        assert_eq!(between(&plist, "<integer>", "</integer>"), ["5", "9", "1", "12"]);
    }
}
