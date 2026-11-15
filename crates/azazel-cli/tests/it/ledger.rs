//! The usage ledger, from outside: every answered request of a run is one line under
//! `<home>/usage`, whoever asked.

use std::collections::BTreeMap;

use crate::common::json_lines;
use assert_cmd::Command;
use serde_json::{Value, json};

#[test]
fn lead_sub_agent_and_side_requests_are_each_one_line_with_who_asked() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let usage = |input: u64| json!({ "input": input, "output": 1 });
    let script = json!({
        "lead": [
            { "delay_ms": 1500, "reply": { "tool_calls": [{ "id": "t1", "name": "task", "input": {
                "agents": [{ "agent": "scout", "prompt": "look", "model": "mock/s1" }] } }],
                "usage": usage(10) } },
            // The side question is answered from the queue while the lead's first turn waits.
            { "reply": { "text": "Two.", "usage": usage(20) } },
            { "reply": { "text": "done", "usage": usage(30) } },
        ],
        "s1": [ { "reply": { "text": "found", "usage": usage(40) } } ],
    });
    let script_path = dir.path().join("script.json");
    std::fs::write(&script_path, script.to_string()).unwrap();
    let output = Command::cargo_bin("azazel")
        .unwrap()
        .env("AZAZEL_HOME", dir.path().join("home"))
        .env("AZAZEL_MOCK_SCRIPT", &script_path)
        .env("AZAZEL_TEST_BTW", "how many?")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .arg("--cwd")
        .arg(dir.path().join("project"))
        .args([
            "-p",
            "go",
            "--mode",
            "auto",
            "--no-memory",
            "--set",
            "session.auto_name=false",
        ])
        .write_stdin("")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut lines = Vec::new();
    for file in std::fs::read_dir(dir.path().join("home/usage"))
        .unwrap()
        .flatten()
    {
        if file.path().extension().is_some_and(|e| e == "jsonl") {
            lines.extend(json_lines(&std::fs::read(file.path()).unwrap()));
        }
    }
    let by_agent: BTreeMap<String, Vec<&Value>> = lines.iter().fold(BTreeMap::new(), |mut map, line| {
        map.entry(line["agent"].as_str().unwrap().to_string())
            .or_default()
            .push(line);
        map
    });
    let inputs = |agent: &str| -> Vec<u64> {
        let mut inputs: Vec<u64> = by_agent[agent]
            .iter()
            .map(|l| l["input"].as_u64().unwrap())
            .collect();
        inputs.sort();
        inputs
    };
    assert_eq!(inputs("lead"), [10, 30], "{lines:?}");
    assert_eq!(inputs("btw"), [20], "{lines:?}");
    let sub: Vec<&String> = by_agent
        .keys()
        .filter(|k| !["lead", "btw"].contains(&k.as_str()))
        .collect();
    assert_eq!(sub.len(), 1, "{by_agent:?}");
    assert_eq!(inputs(sub[0]), [40]);
    assert_eq!(by_agent[sub[0].as_str()][0]["def"], "scout");
    assert_eq!(lines.len(), 4, "{lines:?}");
    for line in &lines {
        assert_eq!(line["requests"], 1);
        assert_eq!(line["provider"], "mock");
        assert!(line["session"].as_str().is_some_and(|s| !s.is_empty()) || line["agent"] == "btw");
    }
}
