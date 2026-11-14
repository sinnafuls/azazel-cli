//! `/loop`: the rounds a loop sends and what ends it, against the real binary and the scripted
//! provider. The loop is started the way the terminal UI starts it, through `AZAZEL_TEST_LOOP`.

use std::path::Path;
use std::process::Output;

use assert_cmd::Command;
use serde_json::{Value, json};

/// Runs `azazel -p <prompt>` as the first round of the loop `/loop <args>` would start.
fn looped(dir: &Path, script: &Value, args: &str, prompt: &str) -> Output {
    let project = dir.join("project");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    // The judge is the small model; a queue of its own keeps its answers from the lead's.
    std::fs::write(dir.join("home/config.toml"), "[roles]\nsmol = [\"mock/judge\"]\n").unwrap();
    std::fs::write(dir.join("script.json"), script.to_string()).unwrap();
    Command::cargo_bin("azazel")
        .unwrap()
        .env("AZAZEL_HOME", dir.join("home"))
        .env("AZAZEL_MOCK_SCRIPT", dir.join("script.json"))
        .env("AZAZEL_MOCK_REQUESTS", dir.join("requests.jsonl"))
        .env("AZAZEL_TEST_LOOP", args)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .arg("--cwd")
        .arg(&project)
        .args(["--no-memory", "--set", "session.auto_name=false"])
        .args(["-p", prompt])
        .write_stdin("")
        .output()
        .unwrap()
}

/// The requests the scripted provider received: the model asked, and the text of the last
/// user message.
fn requests(dir: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(dir.join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .map(|line| {
            let messages = line["request"]["messages"].as_array().unwrap();
            let last = messages
                .iter()
                .rev()
                .find(|message| message["role"] == "user")
                .unwrap();
            let text: Vec<&str> = last["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect();
            (line["model"].as_str().unwrap().to_string(), text.join("\n"))
        })
        .collect()
}

fn said(text: &str) -> Value {
    json!({ "reply": { "text": text } })
}

fn text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn a_loop_with_a_condition_goes_on_until_the_small_model_says_it_holds() {
    let dir = tempfile::tempdir().unwrap();
    let script = json!({
        "lead": [
            said("I fixed parse_empty; 2 tests still fail."),
            said("I fixed the other two; cargo test: 12 passed, 0 failed."),
            said("never asked for"),
        ],
        "judge": [
            said("NO. Two tests still fail."),
            said("YES. The last run shows 12 passed and 0 failed."),
        ],
    });
    let output = looped(
        dir.path(),
        &script,
        "until \"cargo test passes\" fix the next failing test",
        "fix the next failing test",
    );
    let (out, err) = text(&output);
    assert!(output.status.success(), "{err}");
    assert_eq!(
        out.trim(),
        "I fixed the other two; cargo test: 12 passed, 0 failed."
    );
    assert!(
        err.contains("loop finished after 2 rounds: The last run shows 12 passed and 0 failed."),
        "{err}"
    );
    let sent = requests(dir.path());
    let models: Vec<&str> = sent.iter().map(|(model, _)| model.as_str()).collect();
    assert_eq!(models, ["lead", "judge", "lead", "judge"]);
    assert_eq!(sent[2].1, "fix the next failing test");
    // The judge is told the condition and shown what the conversation came to.
    let asked = &sent[3].1;
    assert!(asked.contains("The condition: cargo test passes"), "{asked}");
    assert!(
        asked.contains("user: fix the next failing test\nagent: I fixed parse_empty; 2 tests still fail.\n"),
        "{asked}"
    );
    assert!(
        asked.contains("agent: I fixed the other two; cargo test: 12 passed, 0 failed.\n</conversation>"),
        "{asked}"
    );
}

#[test]
fn a_condition_that_cannot_be_checked_ends_the_loop() {
    let dir = tempfile::tempdir().unwrap();
    let script = json!({
        "lead": [said("I looked at it."), said("never asked for")],
        "judge": [said("Hard to say without running the tests.")],
    });
    let output = looped(dir.path(), &script, "until \"it works\" look at it", "look at it");
    let (_, err) = text(&output);
    assert!(output.status.success(), "{err}");
    assert!(
        err.contains(
            "loop stopped: the condition could not be checked (the judge answered neither yes nor no: Hard to say"
        ),
        "{err}"
    );
    assert_eq!(requests(dir.path()).len(), 2);
}
