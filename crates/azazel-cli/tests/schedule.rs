#![allow(clippy::unwrap_used)]
#![cfg(feature = "detach")]

//! Schedules and triggers: `azazel schedule add|list|rm|run`, the entry in the system's
//! scheduler, the `schedule` tool, and what a session does when a trigger happens. Against the
//! real binary, the scripted provider, and stand-ins for the scheduler and `gh`.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const LIMIT: Duration = Duration::from_secs(60);

/// The `fake-sched` fixture program, built on first use into the directory this test runs from.
static FIXTURE: LazyLock<PathBuf> = LazyLock::new(|| {
    // <target>/<profile>/deps/<this test>
    let test = std::env::current_exe().unwrap();
    let profile_dir = test.parent().unwrap().parent().unwrap();
    let mut build = Command::new(env!("CARGO"));
    build.args(["build", "--quiet", "--package", "fake-sched", "--target-dir"]);
    build.arg(profile_dir.parent().unwrap());
    if profile_dir.file_name().unwrap() == "release" {
        build.arg("--release");
    }
    assert!(
        build.status().unwrap().success(),
        "could not build the fake-sched fixture"
    );
    let binary = profile_dir.join(format!("fake-sched{}", std::env::consts::EXE_SUFFIX));
    assert!(binary.is_file(), "{} is missing", binary.display());
    binary
});

/// A home and a project of its own, and a directory of programs put first on `PATH`: the
/// fixture under the names of the system's scheduler and of `gh`. Background sessions still
/// running when it goes are stopped.
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new(script: &Value) -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["project/.git", "home", "bin", "sched"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        for name in ["schtasks", "crontab", "launchctl", "gh"] {
            let program = dir
                .path()
                .join("bin")
                .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
            std::fs::copy(&*FIXTURE, program).unwrap();
        }
        std::fs::write(dir.path().join("script.json"), script.to_string()).unwrap();
        Sandbox { dir }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    /// `azazel` with this sandbox's home, script, programs and project, and `args` after them.
    fn azazel(&self, args: &[&str]) -> Command {
        let mut path = vec![self.path("bin")];
        path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("azazel"));
        command
            .env("AZAZEL_HOME", self.path("home"))
            // Where a launch agent would be written.
            .env("HOME", self.path("home"))
            .env("PATH", std::env::join_paths(path).unwrap())
            .env("FAKE_SCHED_DIR", self.path("sched"))
            .env("AZAZEL_MOCK_SCRIPT", self.path("script.json"))
            .env("AZAZEL_MOCK_REQUESTS", self.path("requests.jsonl"))
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .args(["--set", "session.auto_name=false", "--no-memory"])
            .args(["--set", "notify.desktop=\"off\""])
            .arg("--cwd")
            .arg(self.path("project"))
            .args(args)
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.azazel(args).output().unwrap()
    }

    /// The standard output of a command that must succeed.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// Adds a schedule that is kept without the system's scheduler, and returns its id.
    fn add(&self, args: &[&str]) -> String {
        let mut all = vec!["--set", "schedule.register=false", "schedule", "add"];
        all.extend(args);
        self.ok(&all).trim().to_string()
    }

    fn schedules(&self) -> Vec<Value> {
        self.ok(&["schedule", "list", "--json"])
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn ps(&self) -> Vec<Value> {
        self.ok(&["ps", "--json"])
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Waits until `azazel ps` says the session `id` is in `state`.
    fn wait_for_state(&self, id: &str, state: &str) -> Value {
        self.wait(&format!("{id} to be `{state}`"), || {
            self.ps()
                .into_iter()
                .find(|row| row["id"] == id && row["state"] == state)
        })
    }

    /// Looks again and again until `look` finds something.
    fn wait<T>(&self, what: &str, look: impl Fn() -> Option<T>) -> T {
        let started = Instant::now();
        loop {
            if let Some(found) = look() {
                return found;
            }
            assert!(
                started.elapsed() < LIMIT,
                "waited in vain for {what}; sessions: {:?}",
                self.ps()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The last user message of every request the scripted provider received, over every
    /// process of the sandbox.
    fn prompts(&self) -> Vec<String> {
        std::fs::read_to_string(self.path("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
            .filter_map(|request| {
                let messages = request["messages"].as_array()?;
                let last = messages.iter().rev().find(|message| message["role"] == "user")?;
                let text: Vec<&str> = last["content"]
                    .as_array()?
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .collect();
                Some(text.join("\n"))
            })
            .collect()
    }

    /// What the stand-in for the scheduler was called with.
    fn scheduler_calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("sched/calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let listed = self.azazel(&["ps", "--json"]).output();
        let rows = listed
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        for row in rows
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        {
            if let Some(id) = row["id"].as_str() {
                let _ = self.azazel(&["kill", id]).output();
            }
        }
    }
}

fn text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn said(text: &str) -> Value {
    json!({ "reply": { "text": text } })
}

#[test]
fn a_schedule_is_added_listed_run_in_the_background_and_removed() {
    let sandbox = Sandbox::new(&json!({ "lead": [
        { "delay_ms": 1500, "reply": { "text": "Nothing is out of date." } },
    ] }));
    let id = sandbox.add(&["--mode", "auto", "every", "10m", "check the dependencies"]);
    assert!(
        id.len() == 8 && id.chars().all(|c| c.is_ascii_hexdigit()),
        "an id: {id:?}"
    );
    let rows = sandbox.schedules();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(
        (
            &row["id"],
            &row["when"],
            &row["prompt"],
            &row["target"],
            &row["runs"]
        ),
        (
            &json!(id),
            &json!("every 10m"),
            &json!("check the dependencies"),
            &json!("new"),
            &json!(0)
        ),
        "{row}"
    );
    assert!(row["next"].is_string() && row["last_run"].is_null(), "{row}");

    // The scheduler would wake it now: it is not due, and nothing starts.
    let early = sandbox.run(&["schedule", "run", &id, "--scheduled"]);
    assert!(early.status.success(), "{}", text(&early).1);
    assert!(sandbox.ps().is_empty());
    let none_due = sandbox.ok(&["schedule", "run", "--due"]);
    assert_eq!(none_due, "");

    // By hand it runs whatever the time is: a background session with the prompt, working
    // the way the schedule was added to work.
    let host = sandbox.ok(&["schedule", "run", &id[..4]]).trim().to_string();
    let listed = sandbox.wait_for_state(&host, "working");
    assert_eq!(listed["name"], json!(format!("schedule {id}")), "{listed}");
    assert_eq!(listed["mode"], json!("auto"), "{listed}");
    // The run before it is still at work: a second one is not started on top of it.
    let again = sandbox.run(&["schedule", "run", &id]);
    let (out, err) = text(&again);
    assert!(again.status.success() && out.is_empty(), "{out}\n{err}");
    assert!(
        err.contains(&format!(
            "skipped: the run before it is still at work (azazel attach {host})"
        )),
        "{err}"
    );
    assert_eq!(sandbox.ps().len(), 1);

    sandbox.wait_for_state(&host, "idle");
    assert_eq!(sandbox.prompts(), ["check the dependencies"]);
    assert!(
        sandbox.ok(&["logs", &host]).contains("Nothing is out of date."),
        "{}",
        sandbox.ok(&["logs", &host])
    );
    let row = &sandbox.schedules()[0];
    assert_eq!(
        (&row["runs"], &row["last_host"]),
        (&json!(1), &json!(host)),
        "{row}"
    );
    let table = sandbox.ok(&["schedule", "list"]);
    assert!(table.starts_with("ID "), "{table}");
    assert!(
        table.contains(&id) && table.contains("every 10m") && table.contains("check the dependencies"),
        "{table}"
    );
    assert!(
        table.contains(&format!("not handed to the system's scheduler: {id}")),
        "{table}"
    );

    assert_eq!(
        sandbox.ok(&["schedule", "rm", &id]).trim(),
        format!("removed {id}")
    );
    assert!(sandbox.schedules().is_empty());
    let gone = sandbox.run(&["schedule", "run", &id]);
    assert_eq!(gone.status.code(), Some(1));
    assert!(
        text(&gone).1.contains("there is no schedule"),
        "{}",
        text(&gone).1
    );
}

#[test]
fn what_cannot_be_scheduled_is_refused_with_the_reason() {
    let sandbox = Sandbox::new(&json!({ "lead": [] }));
    let refused = |args: &[&str]| {
        let mut all = vec!["--set", "schedule.register=false", "schedule", "add"];
        all.extend(args);
        let output = sandbox.run(&all);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {:?}", text(&output));
        text(&output).1
    };
    assert!(refused(&["every", "30s", "go"]).contains("between one minute and 31 days"));
    assert!(refused(&["at", "2020-01-01 10:00", "go"]).contains("in the past"));
    assert!(refused(&["go"]).contains("when is it to run?"));
    assert!(refused(&["--on", "weather", "go"]).contains("ci-failure, or file:<glob>"));
    assert!(refused(&["--on", "ci-failure", "every", "10m", "go"]).contains("not both"));
    assert!(refused(&["--session", "nosuch", "every", "10m", "go"]).contains("no session with id `nosuch`"));
    assert!(sandbox.schedules().is_empty());
}

#[test]
fn a_schedule_with_a_session_sends_its_prompt_to_the_background_session_that_has_it_open() {
    let sandbox = Sandbox::new(&json!({ "lead": [
        { "delay_ms": 2500, "reply": { "text": "The first is done." } },
        said("The second is done."),
    ] }));
    let host = sandbox.ok(&["bg", "do the first thing"]).trim().to_string();
    let listed = sandbox.wait_for_state(&host, "working");
    let session = listed["session_id"].as_str().unwrap().to_string();

    // One moment, an hour from now, in that session.
    let id = sandbox.add(&["--session", &session[..12], "at", "+1h", "do the second thing"]);
    assert_eq!(
        sandbox.schedules()[0]["target"],
        json!(format!("session:{session}"))
    );
    let sent = sandbox.run(&["schedule", "run", &id]);
    let (out, err) = text(&sent);
    assert!(sent.status.success(), "{err}");
    assert_eq!(out.trim(), host, "{err}");

    // The session takes it up after what it was doing, and no second process was started.
    sandbox.wait("the second prompt to be answered", || {
        (sandbox.prompts().len() == 2).then_some(())
    });
    sandbox.wait_for_state(&host, "idle");
    assert_eq!(sandbox.prompts(), ["do the first thing", "do the second thing"]);
    assert_eq!(sandbox.ps().len(), 1);
    let log = sandbox.ok(&["logs", &host]);
    assert!(
        log.contains("do the second thing") && log.contains("The second is done."),
        "{log}"
    );
    // A single moment has had its run.
    assert!(sandbox.schedules().is_empty());
}

#[test]
fn a_single_moment_runs_when_the_scheduler_is_on_time_and_is_missed_when_it_is_late() {
    let sandbox = Sandbox::new(&json!({ "lead": [said("Done at the appointed time.")] }));
    let late = sandbox.add(&["at", "+1h", "do it late"]);
    let timely = sandbox.add(&["at", "+2h", "do it on time"]);
    // Time passes: the one moment is two hours gone, the other half a minute.
    let stores: Vec<PathBuf> = std::fs::read_dir(sandbox.path("home/projects"))
        .unwrap()
        .flatten()
        .map(|project| project.path().join("schedules.json"))
        .filter(|file| file.is_file())
        .collect();
    let [store] = stores.as_slice() else {
        panic!("one project has schedules: {stores:?}");
    };
    let now = jiff::Timestamp::now();
    let ago = |seconds: i64| format!("at {}", now - jiff::SignedDuration::from_secs(seconds));
    let mut kept: Value = serde_json::from_slice(&std::fs::read(store).unwrap()).unwrap();
    for schedule in kept.as_array_mut().unwrap() {
        let when = if schedule["id"] == late.as_str() {
            ago(7200)
        } else {
            ago(30)
        };
        schedule["when"] = json!(when);
    }
    std::fs::write(store, kept.to_string()).unwrap();

    // The scheduler wakes both. What it is far too late for is not made up for.
    let woken = sandbox.run(&["schedule", "run", &late, "--scheduled"]);
    assert!(woken.status.success(), "{:?}", text(&woken));
    assert!(sandbox.ps().is_empty());
    let rows = sandbox.schedules();
    let row = rows.iter().find(|row| row["id"] == late.as_str()).unwrap();
    assert_eq!(
        (&row["runs"], &row["last_result"]),
        (&json!(0), &json!("missed: nothing could run it at its time")),
        "{row}"
    );
    assert!(sandbox.ok(&["schedule", "list"]).contains("missed"));

    // What it is a little late for runs, once, and is then gone from the list.
    sandbox.ok(&["schedule", "run", &timely, "--scheduled"]);
    let host = sandbox.wait("a background session", || {
        sandbox
            .ps()
            .first()
            .map(|row| row["id"].as_str().unwrap().to_string())
    });
    sandbox.wait_for_state(&host, "idle");
    assert_eq!(sandbox.prompts(), ["do it on time"]);
    let left: Vec<Value> = sandbox.schedules();
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0]["id"], json!(late));
    // Nothing more comes of either, however often the scheduler wakes them.
    assert_eq!(sandbox.ok(&["schedule", "run", "--due"]), "");
    assert_eq!(sandbox.ps().len(), 1);
}

/// The words of the command the scheduler was given for the schedule.
fn registered_command(sandbox: &Sandbox, id: &str) -> Vec<String> {
    if cfg!(windows) {
        let calls = sandbox.scheduler_calls();
        let create = calls
            .iter()
            .find(|call| call["args"][0] == "/Create")
            .unwrap_or_else(|| panic!("no task was created: {calls:?}"));
        assert_eq!(create["args"][2], json!(format!(r"azazel\{id}")), "{create}");
        let xml = create["xml"].as_str().unwrap();
        assert!(
            xml.contains("<StartWhenAvailable>false</StartWhenAvailable>"),
            "{xml}"
        );
        let part = |open: &str, close: &str| {
            let rest = xml.split(open).nth(1).unwrap();
            rest.split(close)
                .next()
                .unwrap()
                .replace("&quot;", "\"")
                .replace("&amp;", "&")
        };
        // The program, and its arguments as a command line: the test's paths have no spaces
        // in the parts that are split here.
        let mut words = vec![part("<Command>", "</Command>")];
        words.extend(
            part("<Arguments>", "</Arguments>")
                .split_whitespace()
                .map(|word| word.trim_matches('"').to_string()),
        );
        // Without a console window where the system can: the program follows the wrapper.
        if words[1] == "--headless" {
            words.drain(..2);
        }
        words
    } else if cfg!(target_os = "macos") {
        let agent = sandbox
            .path("home/Library/LaunchAgents")
            .join(format!("dev.azazel.schedule.{id}.plist"));
        let plist = std::fs::read_to_string(&agent).unwrap();
        let loaded = sandbox.scheduler_calls();
        assert!(
            loaded
                .iter()
                .any(|call| call["args"][0] == "load" && call["args"][2] == json!(agent.to_string_lossy())),
            "{loaded:?}"
        );
        plist
            .split("<string>")
            .skip(2)
            .filter_map(|rest| rest.split("</string>").next())
            .take_while(|word| *word != "Background")
            .map(str::to_string)
            .collect()
    } else {
        let table = std::fs::read_to_string(sandbox.path("sched/crontab")).unwrap();
        let line = table
            .lines()
            .find(|line| line.ends_with(&format!("# azazel-schedule:{id}")))
            .unwrap_or_else(|| panic!("no line for {id}: {table}"));
        assert!(line.starts_with("* * * * * "), "{line}");
        line.split_whitespace()
            .skip(5)
            .take_while(|word| *word != ">/dev/null")
            .map(|word| word.trim_matches('\'').to_string())
            .collect()
    }
}

#[test]
fn a_schedule_is_handed_to_the_systems_scheduler_whose_command_runs_it_when_due() {
    let sandbox = Sandbox::new(&json!({ "lead": [said("The minute was noted.")] }));
    // Every minute: whenever the scheduler wakes it, it is due.
    let added = sandbox.run(&["schedule", "add", "* * * * *", "note the minute"]);
    let (id, about) = text(&added);
    let id = id.trim();
    assert!(added.status.success(), "{about}");
    assert!(
        about.contains("next run ") && about.contains("started by "),
        "{about}"
    );
    assert_eq!(sandbox.schedules()[0]["registered"], json!(true));

    // What the scheduler was told to start is this program, in this project and home.
    let command = registered_command(&sandbox, id);
    let program = dunce::canonicalize(&command[0]).unwrap();
    assert_eq!(
        program,
        dunce::canonicalize(assert_cmd::cargo::cargo_bin("azazel")).unwrap()
    );
    let tail: Vec<&str> = command[command.len() - 4..].iter().map(String::as_str).collect();
    assert_eq!(tail, ["schedule", "run", id, "--scheduled"]);
    let home = command.iter().position(|word| word == "--home").unwrap();
    assert_eq!(
        dunce::canonicalize(&command[home + 1]).unwrap(),
        dunce::canonicalize(sandbox.path("home")).unwrap()
    );

    // The scheduler starts it, with none of this test's settings but the script: the run
    // finds the home and the project by what the command says.
    let started = Command::new(&command[0])
        .args(&command[1..])
        .env_remove("AZAZEL_HOME")
        .env("AZAZEL_MOCK_SCRIPT", sandbox.path("script.json"))
        .env("AZAZEL_MOCK_REQUESTS", sandbox.path("requests.jsonl"))
        .current_dir(sandbox.path("sched"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(started.status.success(), "{:?}", text(&started));
    let host = sandbox.wait("a background session", || {
        sandbox
            .ps()
            .first()
            .map(|row| row["id"].as_str().unwrap().to_string())
    });
    sandbox.wait("the prompt to be answered", || {
        (sandbox.prompts() == ["note the minute"]).then_some(())
    });
    let row = &sandbox.schedules()[0];
    assert_eq!(
        (&row["runs"], &row["last_host"]),
        (&json!(1), &json!(host)),
        "{row}"
    );

    // Removed, the scheduler no longer has it.
    sandbox.ok(&["schedule", "rm", id]);
    if cfg!(windows) {
        let calls = sandbox.scheduler_calls();
        let last = calls.last().unwrap();
        assert_eq!(
            last["args"],
            json!(["/Delete", "/TN", format!(r"azazel\{id}"), "/F"])
        );
    } else if cfg!(target_os = "macos") {
        assert!(
            std::fs::read_dir(sandbox.path("home/Library/LaunchAgents"))
                .unwrap()
                .next()
                .is_none()
        );
    } else {
        assert_eq!(
            std::fs::read_to_string(sandbox.path("sched/crontab")).unwrap(),
            ""
        );
    }
}

#[test]
fn a_changed_file_runs_its_trigger_once_in_one_of_the_sessions_that_are_open() {
    let sandbox = Sandbox::new(&json!({ "lead": [said("Ready."), said("The notes were read.")] }));
    std::fs::create_dir_all(sandbox.path("project/notes")).unwrap();
    std::fs::write(sandbox.path("project/notes/old.md"), "old\n").unwrap();
    let id = sandbox.add(&["--on", "file:notes/*.md", "read the notes that changed"]);
    let row = &sandbox.schedules()[0];
    assert_eq!(
        (&row["when"], &row["target"]),
        (&json!("on file:notes/*.md"), &json!("open")),
        "{row}"
    );

    // Two sessions are open in the project; each has looked at the files once it is idle
    // for a moment.
    let first = sandbox.ok(&["bg", "get ready"]).trim().to_string();
    let second = sandbox.ok(&["bg", "get ready"]).trim().to_string();
    sandbox.wait_for_state(&first, "idle");
    sandbox.wait_for_state(&second, "idle");
    std::thread::sleep(Duration::from_secs(5));
    std::fs::write(sandbox.path("project/notes/new.md"), "new\n").unwrap();
    std::fs::write(sandbox.path("project/notes/skipped.txt"), "not a note\n").unwrap();

    let wanted = "read the notes that changed\n\nChanged files:\n- notes/new.md";
    sandbox.wait("the trigger to run", || {
        sandbox
            .prompts()
            .iter()
            .any(|prompt| prompt == wanted)
            .then_some(())
    });
    // One of them owns the trigger: the other stays as it was.
    std::thread::sleep(Duration::from_secs(5));
    let prompts = sandbox.prompts();
    assert_eq!(
        prompts.iter().filter(|prompt| *prompt == wanted).count(),
        1,
        "{prompts:?}"
    );
    let row = &sandbox.schedules()[0];
    assert_eq!(
        (&row["id"], &row["runs"], &row["last_result"]),
        (&json!(id), &json!(1), &json!("ran in the open session")),
        "{row}"
    );
    // What the session itself did in answer is not a change to run for again.
    assert_eq!(prompts.len(), 3, "{prompts:?}");
}

#[test]
fn a_ci_run_that_fails_while_a_session_is_open_runs_its_trigger() {
    let sandbox = Sandbox::new(&json!({ "lead": [said("Ready."), said("The build is fixed.")] }));
    let run = |id: u64, status: &str, conclusion: &str| {
        json!({
            "databaseId": id, "status": status, "conclusion": conclusion, "name": "CI",
            "displayTitle": format!("Change {id}"), "headBranch": "main", "event": "push",
            "url": format!("https://github.com/o/r/actions/runs/{id}"),
        })
    };
    let runs = |all: &[Value]| {
        std::fs::write(sandbox.path("sched/runs.json"), json!(all).to_string()).unwrap();
    };
    // One run failed before anybody watched; one is still going.
    runs(&[run(2, "in_progress", ""), run(1, "completed", "failure")]);
    sandbox.add(&["--on", "ci-failure", "fix the build"]);
    let host = sandbox
        .ok(&["--set", "schedule.poll_secs=1", "bg", "get ready"])
        .trim()
        .to_string();
    sandbox.wait_for_state(&host, "idle");
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(sandbox.prompts(), ["get ready"]);

    // The run that was going fails, and another one passes.
    runs(&[
        run(3, "completed", "success"),
        run(2, "completed", "failure"),
        run(1, "completed", "failure"),
    ]);
    sandbox.wait("the trigger to run", || {
        (sandbox.prompts().len() == 2).then_some(())
    });
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        sandbox.prompts(),
        [
            "get ready",
            "fix the build\n\nA CI run failed: CI: Change 2 (branch main, push)\n\
             https://github.com/o/r/actions/runs/2\n`gh run view 2 --log-failed` prints what failed."
        ]
    );
}

#[test]
fn the_agent_schedules_with_permission_only() {
    let add = json!({ "action": "add", "when": "every 2h", "prompt": "look for new issues" });
    let sandbox = Sandbox::new(&json!({ "lead": [
        { "reply": { "tool_calls": [{ "id": "c1", "name": "schedule", "input": add }] } },
        { "reply": { "tool_calls": [{ "id": "c2", "name": "schedule", "input": { "action": "list" } }] } },
        said("Done."),
    ] }));
    let run = |mode: &str| {
        let _ = std::fs::remove_file(sandbox.path("requests.jsonl"));
        let output = sandbox.run(&[
            "--set",
            "schedule.register=false",
            "--mode",
            mode,
            "-p",
            "keep an eye on the issues",
        ]);
        assert!(output.status.success(), "{:?}", text(&output));
        // What the tool answered each call with.
        let requests: Vec<Value> = std::fs::read_to_string(sandbox.path("requests.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
            .collect();
        let result = |request: &Value| {
            let last = request["messages"].as_array().unwrap().last().unwrap().clone();
            last["content"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };
        (result(&requests[1]), result(&requests[2]))
    };

    // Nobody is there to ask: adding is refused, listing is not.
    let (added, listed) = run("manual");
    assert!(added.contains("permission required for schedule"), "{added}");
    assert_eq!(listed, "this project has no schedule");
    assert!(sandbox.schedules().is_empty());

    // In a mode that asks nothing, it is added, to run the way this session runs.
    let (added, listed) = run("auto");
    let rows = sandbox.schedules();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    let id = row["id"].as_str().unwrap();
    assert!(
        added.starts_with(&format!(
            "schedule {id} (every 2h): starts a background session; "
        )),
        "{added}"
    );
    assert!(
        listed.contains(id) && listed.contains("every 2h") && listed.contains("look for new issues"),
        "{listed}"
    );
    assert_eq!(
        (&row["prompt"], &row["target"]),
        (&json!("look for new issues"), &json!("new")),
        "{row}"
    );
    let args: Vec<&str> = row["args"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let mode = args.iter().position(|arg| *arg == "--mode").unwrap();
    assert_eq!(args[mode + 1], "auto", "{args:?}");
    assert!(args.contains(&"--model"), "{args:?}");
}

#[test]
fn doctor_says_who_starts_the_schedules() {
    let sandbox = Sandbox::new(&json!({ "lead": [] }));
    sandbox.add(&["every", "10m", "go"]);
    sandbox.add(&["--on", "file:*.md", "go"]);
    // The report comes whatever else doctor finds amiss in an empty home.
    let report_of = |args: &[&str]| text(&sandbox.run(args)).0;
    let report = report_of(&["doctor"]);
    assert!(report.contains("schedules: 1 here, started by "), "{report}");
    assert!(
        report.contains("triggers: 1 here, watched while a session is open"),
        "{report}"
    );
    std::fs::create_dir_all(sandbox.path("project/.azazel")).unwrap();
    std::fs::write(
        sandbox.path("project/.azazel/config.toml"),
        "[schedule]\nregister = false\n",
    )
    .unwrap();
    let off = report_of(&["doctor"]);
    assert!(
        off.contains("schedules: 1 here, not handed to ") && off.contains("(`schedule.register` is off)"),
        "{off}"
    );
}

#[test]
fn a_schedule_runs_its_prompt_only_when_its_check_holds_and_ends_once_until_holds() {
    let sandbox = Sandbox::new(&json!({ "lead": [said("Deployed.")] }));
    let id = sandbox.add(&[
        "--mode",
        "auto",
        "--if",
        "cat flag.txt",
        "--if-matches",
        "^go$",
        "--until",
        "cat stop.txt",
        "every",
        "10m",
        "deploy",
    ]);
    let row = &sandbox.schedules()[0];
    assert_eq!(
        (&row["if"], &row["until"], &row["state"]),
        (
            &json!({ "run": "cat flag.txt", "matches": "^go$" }),
            &json!({ "run": "cat stop.txt" }),
            &json!("active")
        ),
        "{row}"
    );

    // The check does not hold: nothing is started, and the list says why.
    let skipped = sandbox.run(&["schedule", "run", &id]);
    let (out, err) = text(&skipped);
    assert!(skipped.status.success() && out.is_empty(), "{out}\n{err}");
    assert!(
        err.contains("skipped: `if` did not hold (exit 1, not 0)"),
        "{err}"
    );
    assert!(sandbox.ps().is_empty());
    let row = &sandbox.schedules()[0];
    assert_eq!(
        row["last_gate"],
        json!("until did not hold (exit 1, not 0); if did not hold (exit 1, not 0)"),
        "{row}"
    );
    assert_eq!(row["runs"], json!(0), "{row}");
    std::fs::write(sandbox.path("project/flag.txt"), "wait\n").unwrap();
    let (_, err) = text(&sandbox.run(&["schedule", "run", &id]));
    assert!(err.contains("printed nothing that matches `^go$`"), "{err}");

    // It holds: the prompt runs.
    std::fs::write(sandbox.path("project/flag.txt"), "go\n").unwrap();
    let host = sandbox.ok(&["schedule", "run", &id]).trim().to_string();
    sandbox.wait_for_state(&host, "idle");
    assert_eq!(sandbox.prompts(), ["deploy"]);
    let table = sandbox.ok(&["schedule", "list"]);
    assert!(
        table.contains("CHECKS")
            && table.contains("until did not hold (exit 1, not 0); if held (exit 0, matched)"),
        "{table}"
    );

    // `until` holds: the schedule ends without running its prompt, and stays in the list.
    std::fs::write(sandbox.path("project/stop.txt"), "merged\n").unwrap();
    let ended = sandbox.run(&["schedule", "run", &id]);
    let (out, err) = text(&ended);
    assert!(ended.status.success() && out.is_empty(), "{out}\n{err}");
    assert!(err.contains("ended: `until` held (exit 0)"), "{err}");
    let row = &sandbox.schedules()[0];
    assert_eq!(
        (&row["state"], &row["last_result"], &row["runs"], &row["next"]),
        (
            &json!("ended"),
            &json!("ended: `until` held (exit 0)"),
            &json!(1),
            &Value::Null
        ),
        "{row}"
    );
    assert_eq!(sandbox.ok(&["schedule", "run", "--due"]), "");
    let resumed = sandbox.run(&["schedule", "resume", &id]);
    assert_eq!(resumed.status.code(), Some(1));
    assert!(text(&resumed).1.contains("has ended"), "{}", text(&resumed).1);
    assert_eq!(sandbox.prompts(), ["deploy"]);
    assert_eq!(sandbox.ps().len(), 1);
}

#[test]
fn a_schedule_is_paused_out_of_the_systems_scheduler_and_resumed_into_it() {
    let sandbox = Sandbox::new(&json!({ "lead": [] }));
    let added = sandbox.run(&[
        "schedule",
        "add",
        "--tz",
        "UTC",
        "30 6 * * *",
        "--times",
        "3",
        "report",
    ]);
    assert!(added.status.success(), "{:?}", text(&added));
    let id = text(&added).0.trim().to_string();
    let row = &sandbox.schedules()[0];
    assert_eq!(
        (&row["when"], &row["tz"], &row["ends"], &row["registered"]),
        (
            &json!("30 6 * * *"),
            &json!("UTC"),
            &json!({ "times": 3 }),
            &json!(true)
        ),
        "{row}"
    );
    let created = sandbox.scheduler_calls().len();

    assert_eq!(
        sandbox.ok(&["schedule", "pause", &id[..4]]).trim(),
        format!("paused {id}")
    );
    let row = &sandbox.schedules()[0];
    assert_eq!(
        (&row["state"], &row["registered"], &row["next"]),
        (&json!("paused"), &json!(false), &Value::Null),
        "{row}"
    );
    let entry_gone = if cfg!(windows) {
        sandbox.scheduler_calls().last().unwrap()["args"]
            == json!(["/Delete", "/TN", format!(r"azazel\{id}"), "/F"])
    } else if cfg!(target_os = "macos") {
        !sandbox
            .path("home/Library/LaunchAgents")
            .join(format!("dev.azazel.schedule.{id}.plist"))
            .exists()
    } else {
        std::fs::read_to_string(sandbox.path("sched/crontab"))
            .unwrap()
            .is_empty()
    };
    assert!(entry_gone, "{:?}", sandbox.scheduler_calls());
    assert!(sandbox.ok(&["schedule", "list"]).contains(" paused "));
    let woken = sandbox.run(&["schedule", "run", &id, "--scheduled"]);
    assert!(woken.status.success());
    let (_, err) = text(&sandbox.run(&["schedule", "run", &id]));
    assert!(err.contains("skipped: it is paused"), "{err}");
    assert!(sandbox.ps().is_empty());

    let resumed = sandbox.run(&["schedule", "resume", &id]);
    let (out, err) = text(&resumed);
    assert!(resumed.status.success(), "{err}");
    assert_eq!(out.trim(), format!("resumed {id}"));
    assert!(err.contains("next run "), "{err}");
    let row = &sandbox.schedules()[0];
    assert_eq!(
        (&row["state"], &row["registered"]),
        (&json!("active"), &json!(true)),
        "{row}"
    );
    assert!(row["next"].is_string(), "{row}");
    assert!(sandbox.scheduler_calls().len() > created + 1);
    if cfg!(windows) {
        assert_eq!(sandbox.scheduler_calls().last().unwrap()["args"][0], "/Create");
    }
}

#[test]
fn checks_ends_and_zones_that_make_no_sense_are_refused() {
    let sandbox = Sandbox::new(&json!({ "lead": [] }));
    let refused = |args: &[&str]| {
        let mut all = vec!["--set", "schedule.register=false", "schedule", "add"];
        all.extend(args);
        let output = sandbox.run(&all);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {:?}", text(&output));
        text(&output).1
    };
    assert!(refused(&["--tz", "Mars/Base", "0 9 * * *", "go"]).contains("no time zone"));
    assert!(refused(&["--tz", "UTC", "every", "10m", "go"]).contains("cron expression or `at`"));
    assert!(
        refused(&["--if", "true", "--if-matches", "(", "every", "10m", "go"])
            .contains("not a regular expression")
    );
    assert!(refused(&["--if-is", "1", "every", "10m", "go"]).contains("--if"));
    assert!(refused(&["--times", "0", "every", "10m", "go"]).contains("1 at least"));
    assert!(refused(&["--ends-at", "2020-01-01 00:00", "every", "10m", "go"]).contains("in the past"));
    assert!(sandbox.schedules().is_empty());
}

#[test]
fn the_agent_schedules_checks_as_it_may_run_their_commands_and_pauses_and_resumes() {
    let add = json!({
        "action": "add", "when": "every 2h", "prompt": "look at the release",
        "if": { "run": "curl -s https://example.com/version", "changes": true },
        "ends": { "after": "7d", "times": 4 },
    });
    let script = |calls: &[Value]| {
        let mut lead: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(n, input)| {
                json!({ "reply": { "tool_calls": [{ "id": format!("c{n}"), "name": "schedule", "input": input }] } })
            })
            .collect();
        lead.push(said("Done."));
        json!({ "lead": lead })
    };
    let sandbox = Sandbox::new(&script(&[add]));
    let run = |extra: &[&str]| -> Vec<String> {
        let _ = std::fs::remove_file(sandbox.path("requests.jsonl"));
        let mut args = vec!["--set", "schedule.register=false", "--mode", "auto"];
        args.extend(extra);
        args.extend(["-p", "watch the release"]);
        let output = sandbox.run(&args);
        assert!(output.status.success(), "{:?}", text(&output));
        std::fs::read_to_string(sandbox.path("requests.jsonl"))
            .unwrap()
            .lines()
            .skip(1)
            .map(|line| {
                let request = serde_json::from_str::<Value>(line).unwrap()["request"].clone();
                let last = request["messages"].as_array().unwrap().last().unwrap().clone();
                last["content"][0]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    };

    // A deny rule for `shell` refuses a check that runs what it denies, in any mode.
    let results = run(&["--set", "permissions.deny=[\"shell(curl:*)\"]"]);
    assert!(
        results[0].contains("denied by rule `shell(curl:*)`"),
        "{results:?}"
    );
    assert!(sandbox.schedules().is_empty());

    // Without it the mode lets it through.
    let results = run(&[]);
    assert!(results[0].starts_with("schedule "), "{results:?}");
    let row = &sandbox.schedules()[0];
    let id = row["id"].as_str().unwrap().to_string();
    assert_eq!(
        row["if"],
        json!({ "run": "curl -s https://example.com/version", "changes": true }),
        "{row}"
    );
    assert_eq!(row["ends"]["times"], json!(4), "{row}");
    assert!(row["ends"]["at"].is_string(), "{row}");

    // Paused and resumed by the agent.
    std::fs::write(
        sandbox.path("script.json"),
        script(&[
            json!({ "action": "pause", "id": id }),
            json!({ "action": "list" }),
            json!({ "action": "resume", "id": id }),
        ])
        .to_string(),
    )
    .unwrap();
    let results = run(&[]);
    assert_eq!(
        results[0],
        format!("paused schedule {id} (every 2h)"),
        "{results:?}"
    );
    assert!(
        results[1].contains(" paused ") && results[1].contains("not run yet"),
        "{results:?}"
    );
    assert!(
        results[2].starts_with(&format!("resumed {id} (every 2h)")),
        "{results:?}"
    );
    assert_eq!(sandbox.schedules()[0]["state"], json!("active"));
}
