//! The queue of follow-up messages: each one is sent like a typed prompt.

use std::path::PathBuf;

use assert_cmd::Command;
use serde_json::{Value, json};

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(status.status.success(), "git {args:?}: {status:?}");
}

#[test]
fn a_queued_message_is_checkpointed_and_expanded_like_a_typed_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    git(&project, &["init", "-q"]);
    std::fs::write(project.join("note.txt"), "the note says hello\n").unwrap();
    let script = dir.path().join("script.json");
    let replies = json!({ "lead": [
        { "reply": { "text": "first answer" } },
        { "reply": { "text": "second answer" } },
    ] });
    std::fs::write(&script, replies.to_string()).unwrap();
    let requests = dir.path().join("requests.jsonl");

    let output = Command::cargo_bin("azazel")
        .unwrap()
        .env("AZAZEL_HOME", dir.path().join("home"))
        .env("AZAZEL_MOCK_SCRIPT", &script)
        .env("AZAZEL_MOCK_REQUESTS", &requests)
        .env("AZAZEL_TEST_QUEUE", "and then read @note.txt")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .arg("--cwd")
        .arg(&project)
        // No session naming: the script holds the lead's replies only.
        .args([
            "--no-memory",
            "--set",
            "session.auto_name=false",
            "-p",
            "do the first thing",
        ])
        .write_stdin("")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "second answer");

    // The queued message went out as a request of its own, with the mentioned file attached.
    let sent: Vec<Value> = std::fs::read_to_string(&requests)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
        .collect();
    assert_eq!(sent.len(), 2);
    let last_user = sent[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .unwrap()
        .to_string();
    assert!(last_user.contains("and then read @note.txt"), "{last_user}");
    assert!(last_user.contains("the note says hello"), "{last_user}");

    // In the session file a checkpoint comes right before each of the two prompts.
    let mut sessions: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir.path().join("home/projects"))
        .unwrap()
        .flatten()
    {
        for file in std::fs::read_dir(entry.path().join("sessions"))
            .unwrap()
            .flatten()
        {
            if file.path().extension().is_some_and(|e| e == "jsonl") {
                sessions.push(file.path());
            }
        }
    }
    assert_eq!(sessions.len(), 1);
    let entries: Vec<Value> = std::fs::read_to_string(&sessions[0])
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let kinds: Vec<String> = entries
        .iter()
        .filter_map(|entry| {
            let kind = entry["type"].as_str()?;
            Some(match kind {
                "message" => format!("message:{}", entry["message"]["role"].as_str().unwrap_or("?")),
                other => other.to_string(),
            })
        })
        .collect();
    let prompts: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter(|(_, kind)| *kind == "message:user")
        .map(|(at, _)| at)
        .collect();
    assert_eq!(prompts.len(), 2, "{kinds:?}");
    for at in prompts {
        assert_eq!(kinds[at - 1], "checkpoint", "{kinds:?}");
    }
}

/// The texts of the user messages of each request the scripted provider was sent.
fn prompts_sent(requests: &std::path::Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(requests)
        .unwrap()
        .lines()
        .map(|line| {
            let request = serde_json::from_str::<Value>(line).unwrap()["request"].clone();
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "user")
                .map(|message| {
                    message["content"][0]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                })
                .collect()
        })
        .collect()
}

/// Runs a prompt with two messages queued behind it and returns what each request held.
fn run_with_queue(mode: &str, replies: usize) -> Vec<Vec<String>> {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join(".azazel")).unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let script = dir.path().join("script.json");
    let lead: Vec<Value> = (1..=replies)
        .map(|n| json!({ "reply": { "text": format!("answer {n}") } }))
        .collect();
    std::fs::write(&script, json!({ "lead": lead }).to_string()).unwrap();
    let requests = dir.path().join("requests.jsonl");
    let output = Command::cargo_bin("azazel")
        .unwrap()
        .env("AZAZEL_HOME", dir.path().join("home"))
        .env("AZAZEL_MOCK_SCRIPT", &script)
        .env("AZAZEL_MOCK_REQUESTS", &requests)
        .env("AZAZEL_TEST_QUEUE", "then the second thing\nand the third thing")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .arg("--cwd")
        .arg(&project)
        .args(["--no-memory", "--set", "session.auto_name=false", "--set"])
        .arg(format!("tui.queue_mode={mode}"))
        .args(["-p", "do the first thing"])
        .write_stdin("")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("answer {replies}")
    );
    prompts_sent(&requests)
}

#[test]
fn queued_messages_go_one_per_run_or_all_in_one_prompt() {
    // One at a time: three runs, each with one prompt more than the one before.
    let one = run_with_queue("one", 3);
    assert_eq!(one.len(), 3);
    assert_eq!(
        one[2],
        [
            "do the first thing",
            "then the second thing",
            "and the third thing"
        ]
    );

    // All together: the two queued messages are the second run's one prompt.
    let all = run_with_queue("all", 2);
    assert_eq!(all.len(), 2);
    assert_eq!(
        all[1],
        [
            "do the first thing",
            "then the second thing\n\nand the third thing"
        ]
    );
}
