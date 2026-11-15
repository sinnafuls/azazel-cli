use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use serde_json::{Value, json};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            if entry.file_name() != "target" {
                copy_dir(&entry.path(), &target);
            }
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A private copy of the `broken-crate` fixture: one test, `adds_two_numbers`, fails until `add`
/// adds.
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(
            &fixtures().join("projects/broken-crate"),
            &dir.path().join("broken-crate"),
        );
        Sandbox { dir }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("broken-crate")
    }

    fn azazel(&self, script: &Path) -> Command {
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.dir.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", script)
            .args(["--mode", "auto", "--output", "json", "--trust", "--cwd"])
            .arg(self.project())
            .timeout(std::time::Duration::from_secs(600));
        command
    }

    /// Runs `azazel goal` on the crate with the mock models following `script`.
    fn goal(&self, script: &Path, extra: &[&str]) -> Output {
        self.azazel(script)
            .args(["goal", "make the tests pass"])
            .args(extra)
            .output()
            .unwrap()
    }

    /// Writes a script in which the lead takes `steps` in order.
    fn script(&self, steps: Value) -> PathBuf {
        let file = self.dir.path().join("script.json");
        std::fs::write(&file, json!({ "lead": steps }).to_string()).unwrap();
        file
    }

    /// Every entry of the session transcript, in order.
    fn transcript(&self) -> Vec<Value> {
        let projects = self.dir.path().join("home/projects");
        let mut entries = Vec::new();
        for project in std::fs::read_dir(projects).unwrap().flatten() {
            for file in std::fs::read_dir(project.path().join("sessions"))
                .into_iter()
                .flatten()
                .flatten()
            {
                if file.path().extension().is_some_and(|e| e == "jsonl") {
                    let text = std::fs::read_to_string(file.path()).unwrap();
                    entries.extend(
                        text.lines()
                            .map(|line| serde_json::from_str::<Value>(line).unwrap()),
                    );
                }
            }
        }
        entries
    }

    /// The `goal` entries of the session transcript, in order.
    fn goal_entries(&self) -> Vec<Value> {
        self.transcript()
            .into_iter()
            .filter(|entry| entry["type"] == "goal")
            .collect()
    }

    /// The text of every user message of the session, in order.
    fn user_messages(&self) -> Vec<String> {
        self.transcript()
            .iter()
            .filter(|entry| entry["type"] == "message" && entry["message"]["role"] == "user")
            .filter_map(|entry| {
                entry["message"]["content"][0]["text"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect()
    }
}

fn events(output: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// `(position in the event stream, the event)` of every finished call of `tool` by the lead.
fn lead_results<'a>(events: &'a [Value], tool_calls: &[&str]) -> Vec<(usize, &'a Value)> {
    events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event["agent"] == "lead"
                && event["type"] == "tool_end"
                && tool_calls.contains(&event["call_id"].as_str().unwrap_or_default())
        })
        .collect()
}

fn text(event: &Value) -> String {
    event["output"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn a_goal_is_worked_on_by_a_team_until_the_checker_verifies_it() {
    let sandbox = Sandbox::new();
    let output = sandbox.goal(&fixtures().join("scripts/team_goal.json"), &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let events = events(&output);

    // Three scouts worked, each under its own id.
    let mut scouts: Vec<&str> = events
        .iter()
        .filter(|event| event["type"] == "message_end")
        .filter_map(|event| event["agent"].as_str())
        .filter(|agent| agent.starts_with("scout-"))
        .collect();
    scouts.sort();
    scouts.dedup();
    assert_eq!(scouts, ["scout-1", "scout-2", "scout-3"]);

    // The first claim is rejected with the failing test; the second is verified.
    let claims = lead_results(&events, &["t3", "t6"]);
    let (first_at, first) = claims[0];
    let (second_at, second) = claims[1];
    assert_eq!(first["output"]["is_error"], true);
    assert!(text(first).contains("`cargo test` failed"), "{}", text(first));
    assert!(text(first).contains("adds_two_numbers"), "{}", text(first));
    assert!(
        text(first).contains("call goal_done again (attempt 2 of 8)"),
        "{}",
        text(first)
    );
    assert_eq!(second["output"]["is_error"], false);
    assert!(text(second).ends_with("verified"), "{}", text(second));

    // The verifier agent ran for the second claim only.
    let verifier: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event["agent"]
                .as_str()
                .is_some_and(|agent| agent.starts_with("verifier-"))
        })
        .map(|(at, _)| at)
        .collect();
    assert!(!verifier.is_empty());
    assert!(
        verifier.iter().all(|at| *at > first_at && *at < second_at),
        "{verifier:?} vs {first_at}..{second_at}"
    );

    let goals = sandbox.goal_entries();
    let last = goals.last().unwrap();
    assert_eq!(
        (last["status"].as_str(), last["iteration"].as_u64()),
        (Some("done"), Some(1))
    );
    assert_eq!(last["objective"], "make the tests pass");
    let statuses: Vec<&str> = goals
        .iter()
        .filter_map(|entry| entry["status"].as_str())
        .collect();
    assert_eq!(statuses, ["active", "verifying", "active", "verifying", "done"]);
    assert!(
        std::fs::read_to_string(sandbox.project().join("src/lib.rs"))
            .unwrap()
            .contains("a + b")
    );
}

#[test]
fn a_goal_the_verifier_keeps_rejecting_fails_when_its_attempts_are_used_up() {
    let sandbox = Sandbox::new();
    let rejection = "The crate has no documentation.\n\n```json\n{\"pass\": false, \"failures\": [{\"what\": \"README \
                     is missing\", \"evidence\": \"ls shows no README.md\"}]}\n```";
    let call = |id: &str, name: &str, input: Value| json!({ "reply": { "tool_calls": [{ "id": id, "name": name, "input": input }] } });
    let claim = json!({ "summary": "fixed add", "evidence": "cargo test" });
    let script = json!({
        "lead": [
            call("c1", "read", json!({ "path": "src/lib.rs" })),
            call("c2", "edit", json!({ "path": "src/lib.rs", "edits": [{ "old": "a - b", "new": "a + b" }] })),
            call("c3", "goal_done", claim.clone()),
            call("c4", "goal_done", claim),
            { "reply": { "text": "The verifier wants a README, which was not asked for." } }
        ],
        "verifier": [{ "reply": { "text": rejection } }, { "reply": { "text": rejection } }]
    });
    let file = sandbox.dir.path().join("script.json");
    std::fs::write(&file, script.to_string()).unwrap();

    let output = sandbox.goal(&file, &["--max-iterations", "2"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("status: failed"), "{stderr}");
    assert!(stderr.contains("not met after 2 attempts"), "{stderr}");

    let events = events(&output);
    let claims = lead_results(&events, &["c3", "c4"]);
    assert!(
        text(claims[0].1).contains("- README is missing\n    ls shows no README.md"),
        "{}",
        text(claims[0].1)
    );
    assert!(
        text(claims[0].1).contains("call goal_done again (attempt 2 of 2)"),
        "{}",
        text(claims[0].1)
    );
    assert!(
        text(claims[1].1).contains("the goal has failed"),
        "{}",
        text(claims[1].1)
    );

    let goals = sandbox.goal_entries();
    let last = goals.last().unwrap();
    assert_eq!(
        (last["status"].as_str(), last["iteration"].as_u64()),
        (Some("failed"), Some(2))
    );
}

const NUDGE: &str =
    "The goal is not marked complete. Continue working, call goal_done with evidence, or call goal_blocked.";

#[test]
fn a_lead_that_keeps_stopping_short_is_nudged_and_then_the_goal_is_paused() {
    let sandbox = Sandbox::new();
    let idle = json!({ "reply": { "text": "I think that is everything." } });
    let script = sandbox.script(json!([idle, idle, idle, idle]));
    let output = sandbox.goal(&script, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("status: paused"), "{stderr}");
    assert!(
        stderr.contains("stopped 4 times in a row without finishing"),
        "{stderr}"
    );

    let said = sandbox.user_messages();
    assert_eq!(
        said[0],
        "Work towards this goal until it is met:\nmake the tests pass"
    );
    assert_eq!(said[1..], [NUDGE, NUDGE, NUDGE]);
    let last = sandbox.goal_entries().pop().unwrap();
    assert_eq!(
        (last["status"].as_str(), last["iteration"].as_u64()),
        (Some("paused"), Some(0))
    );
}

#[test]
fn a_lead_that_says_it_is_blocked_pauses_the_goal_without_being_nudged() {
    let sandbox = Sandbox::new();
    let script = sandbox.script(json!([
        { "reply": { "tool_calls": [{ "id": "b1", "name": "goal_blocked", "input": { "reason": "the spec is missing" } }] } },
        { "reply": { "text": "I need the spec to go on." } }
    ]));
    let output = sandbox.goal(&script, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("blocked: the spec is missing"), "{stderr}");
    assert_eq!(sandbox.user_messages().len(), 1);

    // Outside a goal the goal tools are not offered, so a call to one is refused.
    let stray = Sandbox::new();
    let script = stray.script(json!([
        { "reply": { "tool_calls": [{ "id": "g1", "name": "goal_done", "input": { "summary": "x", "evidence": "y" } }] } },
        { "reply": { "text": "ok" } }
    ]));
    let output = stray.azazel(&script).args(["-p", "hello"]).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    let events = events(&output);
    let refused = lead_results(&events, &["g1"]);
    assert_eq!(refused[0].1["output"]["is_error"], true);
}

#[test]
fn with_verify_on_stop_failing_checks_come_back_to_the_lead_until_they_pass() {
    let sandbox = Sandbox::new();
    let config = sandbox.project().join(".azazel/config.toml");
    let mut settings = std::fs::read_to_string(&config).unwrap();
    settings.push_str("\n[verify]\non_stop = true\ncommands = [\"cargo test\"]\n");
    std::fs::write(&config, settings).unwrap();
    let edit = |id: &str, old: &str, new: &str| {
        json!({ "reply": { "tool_calls": [{ "id": id, "name": "edit", "input": {
            "path": "src/lib.rs", "edits": [{ "old": old, "new": new }] } }] } })
    };
    let script = sandbox.script(json!([
        { "reply": { "tool_calls": [{ "id": "r1", "name": "read", "input": { "path": "src/lib.rs" } }] } },
        edit("e1", "a - b", "a * b"),
        { "reply": { "text": "Changed it." } },
        edit("e2", "a * b", "a + b"),
        { "reply": { "text": "Now it adds." } }
    ]));
    let output = sandbox.azazel(&script).args(["-p", "fix add"]).output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let said = sandbox.user_messages();
    assert_eq!(said.len(), 2, "{said:?}");
    assert!(
        said[1].starts_with("The project's checks fail after your changes.\n$ cargo test\n"),
        "{}",
        said[1]
    );
    assert!(said[1].contains("adds_two_numbers"), "{}", said[1]);
    assert!(
        std::fs::read_to_string(sandbox.project().join("src/lib.rs"))
            .unwrap()
            .contains("a + b")
    );
}
