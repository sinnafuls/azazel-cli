#![cfg(feature = "lua")]
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use serde_json::Value;

const MANIFEST: &str = "skills = [\"skills\"]\ncommands = [\"commands\"]\nextensions = [\"ext/main.luau\"]\n";
const SKILL: &str = "---\nname: greeter\ndescription: Use when someone should be greeted.\n---\nSay hello.\n";
const SCRIPT: &str = r#"
azazel.tool({
	name = "shout",
	description = "Upper-case a text.",
	parameters = { type = "object", properties = { text = { type = "string" } }, required = { "text" } },
	access = "read",
	execute = function(args)
		return string.upper(args.text)
	end,
})
"#;
/// The model calls the tool the package's script adds.
const MODEL: &str = r#"{ "lead": [
  { "reply": { "tool_calls": [ { "id": "c1", "name": "shout", "input": { "text": "hi" } } ] } },
  { "reply": { "text": "done" } }
] }"#;

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let write = |file: &str, text: &str| {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("pack/azazel.toml", MANIFEST);
        write("pack/skills/greeter/SKILL.md", SKILL);
        write("pack/commands/hello.md", "Greet $ARGUMENTS warmly.\n");
        write("pack/ext/main.luau", SCRIPT);
        write("model.json", MODEL);
        write("project/README.md", "x\n");
        Sandbox { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn azazel(&self, args: &[&str], stdin: &str) -> Output {
        Command::cargo_bin("azazel")
            .unwrap()
            .env("AZAZEL_HOME", self.path("home"))
            .env("AZAZEL_MOCK_SCRIPT", self.path("model.json"))
            .args(args)
            .arg("--cwd")
            .arg(self.path("project"))
            .write_stdin(stdin)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str], stdin: &str) -> String {
        let output = self.azazel(args, stdin);
        assert!(
            output.status.success(),
            "{args:?} exited with {:?}: {}{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim_end().to_string()
    }

    /// Whether the scripted model's call of the package's tool worked, and what came back.
    fn shout(&self) -> (bool, String) {
        let events = self.ok(&["-p", "go", "--mode", "auto", "--output", "json"], "");
        let result = events
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|event| event["type"] == "tool_end")
            .unwrap();
        let output = &result["output"];
        (
            output["is_error"].as_bool().unwrap(),
            output["content"][0]["text"].as_str().unwrap().to_string(),
        )
    }
}

fn pack(sandbox: &Sandbox) -> String {
    sandbox.path("pack").to_string_lossy().into_owned()
}

fn canonical(path: &Path) -> String {
    dunce::canonicalize(path).unwrap().display().to_string()
}

#[test]
fn a_packages_code_runs_only_when_it_was_enabled_at_install() {
    let sandbox = Sandbox::new();
    let source = canonical(&sandbox.path("pack"));

    // Declined: the skill and the command are there, the script's tool is not.
    let declined = sandbox.azazel(&["pkg", "add", &pack(&sandbox)], "n\n");
    assert!(
        declined.status.success(),
        "{}",
        String::from_utf8_lossy(&declined.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&declined.stdout).trim_end(),
        format!(
            "installed pack from {source}\n  skills: greeter\n  command directories: 1\n  \
             code: not enabled (its extensions, MCP servers and skill hooks stay off)"
        )
    );
    let asked = String::from_utf8_lossy(&declined.stderr).to_string();
    assert!(asked.contains("It contains 1 extension script(s)."), "{asked}");
    assert!(
        asked.contains("This package can run code on your machine. Enable its code? [y/N]"),
        "{asked}"
    );
    let skills = sandbox.ok(&["skills", "list"], "");
    assert!(
        skills.contains("greeter [package pack]\n  Use when someone should be greeted."),
        "{skills}"
    );
    let (failed, why) = sandbox.shout();
    assert!(failed && why.contains("shout"), "{why}");
    assert_eq!(
        sandbox.ok(&["pkg", "update"], ""),
        "pack: a local directory, used as it is"
    );

    let twice = sandbox.azazel(&["pkg", "add", &pack(&sandbox)], "y\n");
    assert_eq!(twice.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&twice.stderr).contains("a package named `pack` is installed"));

    // Removed and installed again with its code enabled: the tool answers.
    assert_eq!(sandbox.ok(&["skills", "remove", "pack"], ""), "removed pack");
    assert!(
        sandbox
            .ok(&["pkg", "list"], "")
            .starts_with("no packages are installed; ")
    );
    let enabled = sandbox.ok(&["pkg", "add", &pack(&sandbox), "--name", "tools"], "yes\n");
    assert!(enabled.ends_with("\n  code: enabled"), "{enabled}");
    assert_eq!(sandbox.shout(), (false, "HI".to_string()));
    let listed = sandbox.ok(&["pkg", "list"], "");
    assert!(
        listed.starts_with(&format!("tools  {source} (local, code enabled)  installed ")),
        "{listed}"
    );
    // Its exit status is about the model setup, which this sandbox does not have.
    let doctor = String::from_utf8_lossy(&sandbox.azazel(&["doctor"], "").stdout).to_string();
    assert!(doctor.contains("packages: tools (code enabled)"), "{doctor}");

    // The package's directory was never touched.
    assert!(sandbox.path("pack").join("ext").join("main.luau").is_file());
}

#[test]
fn what_cannot_be_installed_is_explained() {
    let sandbox = Sandbox::new();
    let unknown = sandbox.azazel(&["pkg", "add", "no-such-thing"], "");
    assert_eq!(unknown.status.code(), Some(1));
    let why = String::from_utf8_lossy(&unknown.stderr).to_string();
    assert!(
        why.contains(
            "`no-such-thing` is not a git URL, `github:<owner>/<repo>`, `npm:<name>`, a directory or a registry \
             name"
        ),
        "{why}"
    );
    let missing = sandbox.azazel(&["pkg", "remove", "ghost"], "");
    assert_eq!(
        String::from_utf8_lossy(&missing.stderr).trim(),
        "azazel: no package named `ghost` is installed; none are"
    );
    // One skill of a package can only be asked for when the package is known.
    let stray = sandbox.azazel(&["skills", "add", "ghost/skill"], "");
    assert_eq!(stray.status.code(), Some(1));
}
