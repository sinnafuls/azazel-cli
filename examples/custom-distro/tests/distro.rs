#![allow(clippy::unwrap_used)]

use assert_cmd::Command;
use serde_json::Value;

/// The model's side of the run: it uses the tool the extension adds, then tries what the
/// extension forbids.
const SCRIPT: &str = r#"{
  "lead": [
    { "reply": { "tool_calls": [{ "id": "c1", "name": "count_lines", "input": { "path": "notes.txt" } }] } },
    { "reply": { "tool_calls": [{ "id": "c2", "name": "shell", "input": { "command": "sudo true" } }] } },
    { "reply": { "text": "notes.txt has 3 lines." } }
  ]
}"#;

#[test]
fn the_compiled_in_extension_adds_its_tool_and_its_veto() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("notes.txt"), "one\ntwo\nthree\n").unwrap();
    let script = dir.path().join("script.json");
    std::fs::write(&script, SCRIPT).unwrap();

    let output = Command::cargo_bin("custom-distro")
        .unwrap()
        .env("AZAZEL_HOME", dir.path().join("home"))
        .env("AZAZEL_MOCK_SCRIPT", &script)
        .args([
            "-p",
            "how long is notes.txt?",
            "--output",
            "json",
            "--mode",
            "auto",
            "--cwd",
        ])
        .arg(&project)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let result = |call: &str| {
        let end = events
            .iter()
            .find(|e| e["type"] == "tool_end" && e["call_id"] == call)
            .unwrap();
        (
            end["output"]["is_error"].as_bool().unwrap(),
            end["output"]["content"][0]["text"].as_str().unwrap().to_string(),
        )
    };
    assert_eq!(result("c1"), (false, "3 lines".to_string()));
    let (refused, why) = result("c2");
    assert!(refused && why.contains("does not run commands as root"), "{why}");
}
