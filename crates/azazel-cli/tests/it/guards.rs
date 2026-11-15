//! The guards on a turn as a session meets them, in every mode: the done-check when the lead
//! stops on unchecked changes, the note on a command that keeps failing, a looping reply
//! withdrawn as it is written, and a stalled reply asked for again.

use std::path::PathBuf;
use std::process::Output;

use crate::common::{call, stderr, stdout, text};
use assert_cmd::Command;
use serde_json::{Value, json};

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["project/.git", "home"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        Sandbox { dir }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    /// `azazel -p go` in mode `auto` with the lead following `steps`, every request recorded.
    /// `queue` holds the prompts sent after the first, one per line.
    fn run_queued(&self, steps: &Value, args: &[&str], queue: &str) -> Output {
        std::fs::write(self.path("script.json"), json!({ "lead": steps }).to_string()).unwrap();
        let _ = std::fs::remove_file(self.path("requests.jsonl"));
        Command::cargo_bin("azazel")
            .unwrap()
            .env("AZAZEL_HOME", self.path("home"))
            .env("AZAZEL_MOCK_SCRIPT", self.path("script.json"))
            .env("AZAZEL_MOCK_REQUESTS", self.path("requests.jsonl"))
            .env("AZAZEL_TEST_QUEUE", queue)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .arg("--cwd")
            .arg(self.path("project"))
            .args(["--set", "memory.auto_extract=false"])
            .args(["--set", "memory.auto_index=false"])
            .args(["--set", "session.auto_name=false"])
            .args(["--set", "tools.check_on_write=false"])
            .args(["--mode", "auto"])
            .args(args)
            .args(["-p", "go"])
            .write_stdin("")
            .output()
            .unwrap()
    }

    fn run(&self, steps: &Value, args: &[&str]) -> Output {
        self.run_queued(steps, args, "")
    }

    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
            .collect()
    }

    /// The text of every session file of the sandbox.
    fn transcripts(&self) -> String {
        let mut text = String::new();
        for project in std::fs::read_dir(self.path("home/projects")).unwrap().flatten() {
            for file in std::fs::read_dir(project.path().join("sessions"))
                .unwrap()
                .flatten()
            {
                if file.path().extension().is_some_and(|e| e == "jsonl") {
                    text.push_str(&std::fs::read_to_string(file.path()).unwrap());
                }
            }
        }
        text
    }
}

fn write(id: &str, path: &str) -> Value {
    call(id, "write", json!({ "path": path, "content": "one\n" }))
}

fn shell(id: &str, command: &str) -> Value {
    call(id, "shell", json!({ "command": command }))
}

/// The text of every block of `kind` the lead had been sent by `request`, in order: what its
/// tool calls returned (`tool_result`) or what it was told as the user (`text`).
fn sent(request: &Value, kind: &str) -> Vec<String> {
    let mut texts = Vec::new();
    for message in request["messages"].as_array().unwrap() {
        if message["role"] != "user" && kind == "text" {
            continue;
        }
        for block in message["content"].as_array().unwrap() {
            if block["type"] != kind {
                continue;
            }
            match kind {
                "tool_result" => texts.extend(
                    block["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|inner| inner["text"].as_str().map(str::to_string)),
                ),
                _ => texts.extend(block["text"].as_str().map(str::to_string)),
            }
        }
    }
    texts
}

/// What the done-check told the lead by `request`: the files it named, once per time it spoke.
fn done_checks(request: &Value) -> Vec<String> {
    sent(request, "text")
        .iter()
        .filter_map(|told| {
            let (_, rest) = told.split_once("<done-check>\n")?;
            let (line, _) = rest.split_once("\n</done-check>")?;
            Some(line.to_string())
        })
        .collect()
}

#[test]
fn the_lead_that_stops_on_unchecked_changes_is_sent_back_once_per_prompt() {
    let sandbox = Sandbox::new();
    let steps = json!([
        write("c1", "src/a.py"),
        text("done"),
        // Sent back: it changes more and stops again, and is not asked a second time.
        write("c2", "src/b.py"),
        text("unverified: nothing here runs Python"),
        // The second prompt is a new turn.
        write("c3", "src/c.py"),
        text("done again"),
        text("unverified again"),
    ]);
    let output = sandbox.run_queued(&steps, &[], "and once more");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "unverified again");

    let requests = sandbox.requests();
    assert_eq!(requests.len(), 7);
    let named = "1 file changed in this turn and nothing has run since that shows the change works: ";
    assert_eq!(done_checks(&requests[1]), Vec::<String>::new());
    assert_eq!(done_checks(&requests[2]), [format!("{named}src/a.py")]);
    // The fourth request ended the first prompt; the note was not repeated.
    assert_eq!(done_checks(&requests[3]), [format!("{named}src/a.py")]);
    assert_eq!(
        done_checks(&requests[6]),
        [format!("{named}src/a.py"), format!("{named}src/c.py")]
    );
    let told = sent(&requests[2], "text").pop().unwrap();
    assert!(
        told.ends_with("say plainly in your answer that the change is unverified."),
        "{told}"
    );
}

#[test]
fn a_check_run_after_the_change_prose_and_the_setting_keep_the_done_check_away() {
    // The project's own check command, run after the last change.
    let sandbox = Sandbox::new();
    let steps = json!([
        write("c1", "src/a.py"),
        shell("c2", "echo all good"),
        text("done"),
    ]);
    let output = sandbox.run(&steps, &["--set", "verify.commands=[\"echo all good\"]"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "done");

    // The same command run before the change proves nothing about it.
    let sandbox = Sandbox::new();
    let steps = json!([
        shell("c1", "echo all good"),
        write("c2", "src/a.py"),
        text("done"),
        text("unverified"),
    ]);
    let output = sandbox.run(&steps, &["--set", "verify.commands=[\"echo all good\"]"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(done_checks(&sandbox.requests()[3]).len(), 1);

    // Prose only, and the guard turned off.
    let sandbox = Sandbox::new();
    let steps = json!([write("c1", "docs/notes.md"), text("done")]);
    let output = sandbox.run(&steps, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "done");

    let sandbox = Sandbox::new();
    let steps = json!([write("c1", "src/a.py"), text("done")]);
    let output = sandbox.run(&steps, &["--set", "guards.done_check=false"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "done");
    assert_eq!(sandbox.requests().len(), 2);
}

#[test]
fn the_third_failure_of_one_command_tells_the_agent_to_think_again() {
    let sandbox = Sandbox::new();
    let steps = json!([
        shell("c1", "exit 3"),
        shell("c2", "exit  3"),
        shell("c3", "exit 4"),
        shell("c4", "exit 3"),
        text("it keeps failing"),
    ]);
    let output = sandbox.run(&steps, &[]);
    assert!(output.status.success(), "{}", stderr(&output));

    let results = sent(&sandbox.requests()[4], "tool_result");
    assert_eq!(results.len(), 5, "{results:?}");
    let noted: Vec<bool> = [0, 1, 2, 3]
        .iter()
        .map(|&at| results[at].contains("[stuck]"))
        .collect();
    // The command of `c3` failed once; the other three calls are one command.
    assert_eq!(noted, [false, false, false, false], "{results:?}");
    assert!(results[3].ends_with("exit code: 3"), "{results:?}");
    assert_eq!(
        results[4],
        "[stuck] this command has failed 3 times in a row in this turn. Running it again will fail the \
         same way: form a new hypothesis about the cause and test that before trying this again."
    );

    // Off with `stuck_after = 0`.
    let output = sandbox.run(&steps, &["--set", "guards.stuck_after=0"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let results = sent(&sandbox.requests()[4], "tool_result");
    assert!(
        results.iter().all(|result| !result.contains("[stuck]")),
        "{results:?}"
    );
}

/// A reply that goes round in a loop: the same paragraph, three times over.
fn looping() -> String {
    let paragraph = "First I will read the configuration, then I will look at the module that loads it, and \
                     then I will compare the two to see where the default value is lost on the way. After \
                     that I will write the fix and run the tests to see that nothing else depended on it.\n";
    assert!(paragraph.chars().count() >= 200);
    format!("Here is my plan.\n{}", paragraph.repeat(3))
}

#[test]
fn a_reply_that_loops_is_withdrawn_and_asked_for_again_with_a_note() {
    let sandbox = Sandbox::new();
    let steps = json!([text(&looping()), text("The default is lost in load().")]);
    let output = sandbox.run(&steps, &["--output", "json"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let events = stdout(&output);
    assert_eq!(events.matches("\"type\":\"rule\"").count(), 1, "{events}");
    assert!(events.contains("\"name\":\"runaway\""), "{events}");
    let requests = sandbox.requests();
    assert_eq!(requests.len(), 2);
    let told = sent(&requests[1], "text").pop().unwrap();
    assert!(
        told.starts_with(
            "<rule name=\"runaway\" reason=\"repetition\">\nYour reply repeated the same text 3 times in a \
             row:\nFirst I will read the configuration"
        ),
        "{told}"
    );
    // The loop itself is neither sent back to the model nor stored.
    let second = requests[1]["messages"].to_string();
    assert!(!second.contains("Here is my plan."), "{second}");
    let stored = sandbox.transcripts();
    assert!(!stored.contains("Here is my plan."));
    assert!(stored.contains("The default is lost in load()."));

    // With the guard off the reply stands as it was written.
    let sandbox = Sandbox::new();
    let output = sandbox.run(&json!([text(&looping())]), &["--set", "guards.runaway=false"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), looping().trim());
}

#[test]
fn a_stalled_reply_is_dropped_and_asked_for_again_and_the_user_is_told() {
    let stalled = json!({ "kind": "stalled", "detail": { "secs": 20 } });
    let sandbox = Sandbox::new();
    let steps = json!([
        // Silent from the start, then silent in the middle of the reply, then an answer.
        { "events": [{ "type": "error", "error": stalled }] },
        { "events": [
            { "type": "start" },
            { "type": "text_delta", "index": 0, "delta": "The answer is forty-" },
            { "type": "error", "error": stalled },
        ] },
        text("The answer is forty-two."),
    ]);
    let output = sandbox.run(&steps, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "The answer is forty-two.");
    let said = stderr(&output);
    assert!(
        said.contains("no answer from mock/lead for 20 s; retrying (attempt 1)")
            && said.contains("no answer from mock/lead for 20 s; retrying (attempt 2)"),
        "{said}"
    );
    // Three requests, each the same: the half of a reply never reached the model.
    let requests = sandbox.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["messages"], requests[2]["messages"]);
    assert!(!sandbox.transcripts().contains("forty-\""));

    // A third stall is the limit: with no other model in the chain the run fails, saying why.
    let steps = json!([
        { "events": [{ "type": "error", "error": stalled }] },
        { "events": [{ "type": "error", "error": stalled }] },
        { "events": [{ "type": "error", "error": stalled }] },
        text("never asked for"),
    ]);
    let output = sandbox.run(&steps, &[]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("no answer for 20 s"),
        "{}",
        stderr(&output)
    );
    assert_eq!(sandbox.requests().len(), 3);

    // `max_stalls` moves that limit.
    let output = sandbox.run(&steps, &["--set", "retry.max_stalls=4", "--output", "json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let events = stdout(&output);
    assert_eq!(events.matches("\"stalled_secs\":20").count(), 3, "{events}");
    assert_eq!(sandbox.requests().len(), 4);
}
