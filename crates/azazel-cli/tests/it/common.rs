//! Helpers the test modules share: the stand-in programs of `tests/fixtures`, scripted replies,
//! and reading what a run printed or asked for.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

/// The program of the fixture package `package`, built on first use into the directory this
/// test runs from. Built once per test run, however many tests ask for it.
pub fn fixture_program(package: &str) -> PathBuf {
    static BUILT: LazyLock<Mutex<HashMap<String, PathBuf>>> = LazyLock::new(Default::default);
    let mut built = BUILT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(binary) = built.get(package) {
        return binary.clone();
    }
    // <target>/<profile>/deps/<this test>
    let test = std::env::current_exe().unwrap();
    let profile_dir = test.parent().unwrap().parent().unwrap();
    let mut build = std::process::Command::new(env!("CARGO"));
    build.args(["build", "--quiet", "--package", package, "--target-dir"]);
    build.arg(profile_dir.parent().unwrap());
    if profile_dir.file_name().unwrap() == "release" {
        build.arg("--release");
    }
    build.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    assert!(
        build.status().unwrap().success(),
        "could not build the {package} fixture"
    );
    let binary = profile_dir.join(format!("{package}{}", std::env::consts::EXE_SUFFIX));
    assert!(binary.is_file(), "{} is missing", binary.display());
    built.insert(package.to_string(), binary.clone());
    binary
}

/// A script of `tests/fixtures/scripts`.
pub fn script_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/scripts")
        .join(name)
}

/// A scripted reply that says `text`.
pub fn text(text: &str) -> Value {
    json!({ "reply": { "text": text } })
}

/// A scripted reply that calls `tool` once.
pub fn call(id: &str, tool: &str, input: Value) -> Value {
    json!({ "reply": { "tool_calls": [{ "id": id, "name": tool, "input": input }] } })
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// What a run printed, trimmed.
pub fn trimmed(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

/// Every line of `bytes` as JSON.
pub fn json_lines(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The names of the tools a recorded request offered.
pub fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

/// The system prompt of a recorded request, its parts joined.
pub fn system(request: &Value) -> String {
    request["system"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| part["text"].as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Runs `git` in `dir` and returns what it printed; fails the test when git fails.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The lines `reader` gives, read on a thread of their own.
pub fn lines_of(reader: impl std::io::Read + Send + 'static) -> Receiver<String> {
    let (sender, lines) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    lines
}

/// The lines up to and including the first that contains `wanted`; fails after `limit` without one.
pub fn read_until(lines: &Receiver<String>, wanted: &str, limit: Duration) -> Vec<String> {
    let mut read = Vec::new();
    loop {
        let line = lines
            .recv_timeout(limit)
            .unwrap_or_else(|_| panic!("no line with `{wanted}` after {read:?}"));
        let found = line.contains(wanted);
        read.push(line);
        if found {
            return read;
        }
    }
}
