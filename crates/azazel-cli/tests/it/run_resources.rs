//! What the command line adds for one run (`--extension`, `--skill`, `--mcp-config`, `--theme`,
//! `--provider`), packages from npm, and tools that scripts replace or wrap, as the binary does
//! them.

use std::path::{Path, PathBuf};
use std::process::Output;

use crate::common::{fixture_program, stderr, stdout};
use assert_cmd::Command;
use serde_json::{Value, json};

const SKILL: &str = "---\nname: greeter\ndescription: Use when someone should be greeted.\n---\nSay hello.\n";
/// A tool no built-in has.
const SHOUT: &str = r#"
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

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    /// A project nobody has trusted, an empty home, and nothing installed.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        Sandbox { dir }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn write(&self, relative: &str, text: &str) -> String {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.path("home"))
            .env_remove("AZAZEL_MOCK_SCRIPT")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("NPM_CONFIG_REGISTRY")
            .arg("--cwd")
            .arg(self.path("project"))
            .args(["--set", "memory.auto_extract=false"])
            .args(["--set", "memory.auto_index=false"])
            .args(["--set", "session.auto_name=false"]);
        command
    }

    /// `azazel <args> -p go` answering from `script`, with every request recorded.
    fn run(&self, script: &Value, args: &[&str]) -> Output {
        let script_path = self.path("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let _ = std::fs::remove_file(self.path("requests.jsonl"));
        self.command()
            .env("AZAZEL_MOCK_SCRIPT", &script_path)
            .env("AZAZEL_MOCK_REQUESTS", self.path("requests.jsonl"))
            .args(args)
            .args(["--mode", "auto", "-p", "go"])
            .write_stdin("")
            .output()
            .unwrap()
    }

    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
            .collect()
    }

    /// The names of the tools the lead was offered in its first request.
    fn offered(&self) -> Vec<String> {
        let requests = self.requests();
        let tools = requests[0]["tools"].as_array().unwrap();
        tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// The text of every tool result the lead had been given by its last request, in order.
    fn results(&self) -> Vec<String> {
        let requests = self.requests();
        let mut results = Vec::new();
        for message in requests.last().unwrap()["messages"].as_array().unwrap() {
            for block in message["content"].as_array().unwrap() {
                if block["type"] == "tool_result" {
                    let text: Vec<&str> = block["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|inner| inner["text"].as_str())
                        .collect();
                    results.push(text.join("\n"));
                }
            }
        }
        results
    }

    /// Whether the lead's instructions mention `text`.
    fn told(&self, text: &str) -> bool {
        self.requests()[0]["system"].to_string().contains(text)
    }
}

fn succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "exited with {:?}: {}{}",
        output.status.code(),
        stdout(output),
        stderr(output)
    );
}

/// The run ended as a mistake in how azazel was called, and said `why`.
fn refused(output: &Output, why: &str) {
    assert_eq!(output.status.code(), Some(2), "{}", stderr(output));
    assert!(stderr(output).contains(why), "{}", stderr(output));
}

/// A script in which the lead makes `calls` one after another and then says `done`.
fn calls(calls: &[(&str, Value)]) -> Value {
    let mut steps: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, input))| {
            json!({ "reply": { "tool_calls": [{ "id": format!("t{}", i + 1), "name": name, "input": input }] } })
        })
        .collect();
    steps.push(json!({ "reply": { "text": "done" } }));
    json!({ "lead": steps })
}

fn done() -> Value {
    calls(&[])
}

#[cfg(feature = "lua")]
#[test]
fn a_script_named_on_the_command_line_loads_for_that_run_only() {
    let sandbox = Sandbox::new();
    let script = sandbox.write("elsewhere/shout.luau", SHOUT);
    // A script of the project, which is not trusted and so does not load.
    sandbox.write(
        "project/.azazel/extensions/quiet.luau",
        &SHOUT.replace("shout", "whisper"),
    );
    let shout = calls(&[("shout", json!({ "text": "hi" }))]);

    let ran = sandbox.run(&shout, &["-e", &script]);
    succeeded(&ran);
    assert_eq!(sandbox.results(), ["HI"]);
    let offered = sandbox.offered();
    assert!(offered.contains(&"shout".to_string()) && !offered.contains(&"whisper".to_string()));

    // With everything else switched off it is still there.
    succeeded(&sandbox.run(&shout, &["--bare", "--extension", &script]));
    assert_eq!(sandbox.results(), ["HI"]);

    // And the next run without the flag has nothing of it.
    succeeded(&sandbox.run(&done(), &[]));
    assert!(!sandbox.offered().contains(&"shout".to_string()));

    refused(
        &sandbox.run(&done(), &["-e", "absent.luau"]),
        "--extension absent.luau: no such file or directory",
    );
}

#[cfg(feature = "lua")]
#[test]
fn a_directory_named_as_an_extension_is_read_like_a_package_with_its_code_on() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "kit/azazel.toml",
        "skills = [\"skills\"]\ncommands = [\"commands\"]\nextensions = [\"ext/main.luau\"]\n",
    );
    sandbox.write("kit/skills/greeter/SKILL.md", SKILL);
    sandbox.write("kit/commands/hello.md", "Greet $ARGUMENTS warmly.\n");
    sandbox.write("kit/ext/main.luau", SHOUT);
    let kit = sandbox.path("kit").to_string_lossy().into_owned();

    let ran = sandbox.run(&calls(&[("shout", json!({ "text": "kit" }))]), &["-e", &kit]);
    succeeded(&ran);
    assert_eq!(sandbox.results(), ["KIT"]);
    assert!(sandbox.told("greeter"));

    // Nothing was installed.
    let listed = sandbox.command().args(["pkg", "list"]).output().unwrap();
    assert!(
        stdout(&listed).starts_with("no packages are installed"),
        "{}",
        stdout(&listed)
    );
    succeeded(&sandbox.run(&done(), &[]));
    assert!(!sandbox.told("greeter"));
}

#[test]
fn a_skill_named_on_the_command_line_is_offered_even_with_skills_off() {
    let sandbox = Sandbox::new();
    let one = sandbox.write("skills/greeter/SKILL.md", SKILL);
    sandbox.write("many/a/SKILL.md", &SKILL.replace("greeter", "first-of-many"));
    sandbox.write("many/b/SKILL.md", &SKILL.replace("greeter", "second-of-many"));
    let many = sandbox.path("many").to_string_lossy().into_owned();

    succeeded(&sandbox.run(&done(), &["--skill", &one, "--skill", &many]));
    assert!(sandbox.told("greeter") && sandbox.told("first-of-many") && sandbox.told("second-of-many"));

    succeeded(&sandbox.run(&done(), &["--no-skills", "--skill", &one]));
    assert!(sandbox.told("greeter") && !sandbox.told("first-of-many"));

    succeeded(&sandbox.run(&done(), &["--no-skills"]));
    assert!(!sandbox.told("greeter"));

    refused(
        &sandbox.run(&done(), &["--skill", "nowhere"]),
        "--skill nowhere: no such file or directory",
    );
}

#[cfg(feature = "mcp")]
#[test]
fn the_servers_of_an_mcp_file_named_on_the_command_line_are_used_for_that_run() {
    use std::sync::LazyLock;

    static FIXTURE: LazyLock<PathBuf> = LazyLock::new(|| fixture_program("mcp-echo"));

    let sandbox = Sandbox::new();
    let servers = json!({ "mcpServers": { "echo": { "command": FIXTURE.as_path() } } });
    let file = sandbox.write("servers.json", &servers.to_string());
    // A server of the user's own files, which `--no-mcp` leaves out.
    sandbox.write(
        "home/mcp.json",
        &json!({ "mcpServers": { "other": { "command": FIXTURE.as_path() } } }).to_string(),
    );
    let echo = calls(&[("mcp__echo__echo", json!({ "text": "hi" }))]);

    succeeded(&sandbox.run(&echo, &["--no-mcp", "--mcp-config", &file]));
    assert_eq!(sandbox.results(), ["hi"]);
    let offered = sandbox.offered();
    assert!(
        !offered.iter().any(|name| name.starts_with("mcp__other__")),
        "{offered:?}"
    );

    // Not kept: the user's file is as it was.
    let kept: Value =
        serde_json::from_str(&std::fs::read_to_string(sandbox.path("home/mcp.json")).unwrap()).unwrap();
    assert!(kept["mcpServers"].get("echo").is_none());

    let broken = sandbox.write("broken.json", "{\"servers\": {}}");
    refused(
        &sandbox.run(&done(), &["--mcp-config", &broken]),
        "it has no `mcpServers` object",
    );
}

#[test]
fn a_theme_is_named_or_given_as_a_file_and_a_bad_one_stops_the_run() {
    let sandbox = Sandbox::new();
    let file = sandbox.write("dusk.toml", "accent = \"#102030\"\n");
    sandbox.write("home/themes/mine.toml", "accent = \"#405060\"\n");
    let broken = sandbox.write("broken.toml", "accent = \"no colour\"\n");

    for good in ["light", "mine", file.as_str()] {
        succeeded(&sandbox.run(&done(), &["--theme", good]));
    }
    refused(
        &sandbox.run(&done(), &["--theme", "nope"]),
        "--theme: no theme named `nope`",
    );
    refused(&sandbox.run(&done(), &["--theme", &broken]), "--theme: ");
}

#[test]
fn a_provider_named_on_the_command_line_is_the_only_one_models_are_taken_from() {
    let sandbox = Sandbox::new();

    // A name is looked up among that provider's models only.
    succeeded(&sandbox.run(&done(), &["--provider", "mock", "--model", "mock/lead"]));
    // Without --model the lead keeps the models of its chain that are the provider's.
    succeeded(&sandbox.run(&done(), &["--provider", "mock"]));

    let other = sandbox.run(&done(), &["--provider", "anthropic", "--model", "mock/lead"]);
    refused(&other, "--provider anthropic leaves only that provider's models");
    let none = sandbox.run(&done(), &["--provider", "anthropic"]);
    refused(
        &none,
        "--provider anthropic: none of the configured models (mock/lead) is that provider's",
    );
    let unknown = sandbox.run(&done(), &["--provider", "nope"]);
    refused(
        &unknown,
        "--provider: there is no provider `nope`; the providers are: ",
    );
    assert!(stderr(&unknown).contains("anthropic"), "{}", stderr(&unknown));
}

#[cfg(feature = "lua")]
#[test]
fn a_script_replaces_and_wraps_built_in_tools_and_both_are_reported() {
    let sandbox = Sandbox::new();
    sandbox.write("project/notes.txt", "from the disk\n");
    sandbox.write(
        "home/extensions/vault.luau",
        r#"
        azazel.tool({
            name = "read",
            description = "Read a file from the vault.",
            access = "read",
            replaces = true,
            parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
            execute = function(args)
                return "from the vault: " .. args.path
            end,
        })
        "#,
    );
    sandbox.write(
        "home/extensions/audit.luau",
        r#"
        azazel.wrap("glob", {
            before = function(input)
                if input.pattern == "secret/*" then
                    return { block = true, reason = "that directory is not listed" }
                end
                return { input = { pattern = "*.txt" } }
            end,
            after = function(result, input)
                return "[" .. input.pattern .. "] " .. result.content
            end,
        })
        "#,
    );
    let script = calls(&[
        ("read", json!({ "path": "notes.txt" })),
        ("glob", json!({ "pattern": "*.md" })),
        ("glob", json!({ "pattern": "secret/*" })),
    ]);

    succeeded(&sandbox.run(&script, &[]));
    let results = sandbox.results();
    assert_eq!(results[0], "from the vault: notes.txt");
    // The built-in tool ran with the arguments the wrap gave it, and its result came back changed.
    assert!(
        results[1].starts_with("[*.txt] ") && results[1].contains("notes.txt"),
        "{}",
        results[1]
    );
    assert_eq!(results[2], "that directory is not listed");
    // The model is shown the replacement, not the tool it stands in for.
    let requests = sandbox.requests();
    let read = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "read")
        .unwrap();
    assert_eq!(read["description"], "Read a file from the vault.");

    // Without the scripts the built-in tools are themselves.
    succeeded(&sandbox.run(&script, &["--no-extensions"]));
    assert!(sandbox.results()[0].contains("from the disk"));

    // `/tools` and `azazel doctor` say who stands where.
    let model = sandbox.write("model.json", &done().to_string());
    let asked = sandbox
        .command()
        .env("AZAZEL_MOCK_SCRIPT", &model)
        .arg("rpc")
        .write_stdin("{\"id\": 1, \"type\": \"run_command\", \"name\": \"tools\"}\n")
        .output()
        .unwrap();
    let response = stdout(&asked)
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["type"] == "response" && line["id"] == 1)
        .unwrap_or_else(|| panic!("no response: {}{}", stdout(&asked), stderr(&asked)));
    let tools = response["data"]["text"].as_str().unwrap();
    assert!(
        tools.contains("read (the script vault replaces the core tool)"),
        "{tools}"
    );
    assert!(tools.contains("glob (wrapped by the script audit)"), "{tools}");

    let doctor = sandbox.command().arg("doctor").output().unwrap();
    let report = stdout(&doctor);
    assert!(
        report.contains("tool `read`: replaced by the script vault"),
        "{report}"
    );
    assert!(
        report.contains("tool `glob`: wrapped by the script audit"),
        "{report}"
    );
}

/// A package tarball as `npm pack` makes one, from the files of `dir/package`, packed by the
/// system's own `tar`.
fn tarball(dir: &Path) -> Vec<u8> {
    let program = if cfg!(windows) {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("tar.exe")
    } else {
        PathBuf::from("tar")
    };
    let out = dir.join("package.tgz");
    let made = std::process::Command::new(program)
        .arg("-czf")
        .arg(&out)
        .arg("-C")
        .arg(dir)
        .arg("package")
        .status()
        .unwrap();
    assert!(made.success());
    std::fs::read(out).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_package_is_installed_from_npm_checked_used_and_updated() {
    use base64::Engine;
    use sha2::{Digest, Sha512};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let sandbox = Sandbox::new();
    let server = MockServer::start().await;
    // Version `version` of the package: a skill, and a long path of the kind tar writes apart.
    let publish = |version: &str| {
        let root = format!("npm/{version}");
        sandbox.write(&format!("{root}/package/package.json"), "{}");
        sandbox.write(&format!("{root}/package/LICENSE"), "MIT");
        sandbox.write(&format!("{root}/package/skills/greeter/SKILL.md"), SKILL);
        sandbox.write(
            &format!("{root}/package/{}/{}.md", "deep/".repeat(14), "n".repeat(60)),
            version,
        );
        let bytes = tarball(&sandbox.path(&root));
        let sha512 = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&bytes));
        let described = json!({
            "name": "@acme/review-kit",
            "dist-tags": { "latest": version },
            "versions": { version: { "dist": {
                "tarball": format!("{}/tarballs/{version}.tgz", server.uri()),
                "integrity": format!("sha512-{sha512}"),
            } } },
        });
        (bytes, described)
    };
    let (first, described) = publish("1.0.0");
    Mock::given(method("GET"))
        .and(path("/tarballs/1.0.0.tgz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(first))
        .mount(&server)
        .await;
    let listed = Mock::given(method("GET"))
        .and(path("/@acme%2Freview-kit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(described))
        .mount_as_scoped(&server)
        .await;
    let azazel = |args: &[&str]| {
        let output = sandbox
            .command()
            .env("NPM_CONFIG_REGISTRY", server.uri())
            .args(args)
            .write_stdin("")
            .output()
            .unwrap();
        (output.status.success(), stdout(&output), stderr(&output))
    };

    let (ok, out, err) = azazel(&["pkg", "add", "npm:@acme/review-kit"]);
    assert!(ok, "{err}");
    assert_eq!(
        out.trim_end(),
        "installed review-kit from npm:@acme/review-kit (1.0.0)\n  skills: greeter"
    );
    let root = sandbox.path("home/packages/review-kit");
    let deep = root
        .join("deep/".repeat(14))
        .join(format!("{}.md", "n".repeat(60)));
    assert_eq!(std::fs::read_to_string(&deep).unwrap(), "1.0.0");
    let (_, listing, _) = azazel(&["pkg", "list"]);
    assert!(
        listing.starts_with("review-kit  npm:@acme/review-kit (1.0.0)  installed "),
        "{listing}"
    );

    // A session has its skill.
    succeeded(&sandbox.run(&done(), &[]));
    assert!(sandbox.told("greeter"));

    let (_, same, _) = azazel(&["pkg", "update"]);
    assert_eq!(same.trim_end(), "review-kit: up to date (1.0.0)");

    drop(listed);
    let (second, described) = publish("1.1.0");
    Mock::given(method("GET"))
        .and(path("/tarballs/1.1.0.tgz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(second))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/@acme%2Freview-kit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(described))
        .mount(&server)
        .await;
    let (ok, updated, err) = azazel(&["pkg", "update", "review-kit"]);
    assert!(ok, "{err}");
    assert_eq!(updated.trim_end(), "review-kit: 1.0.0 -> 1.1.0");
    assert_eq!(std::fs::read_to_string(&deep).unwrap(), "1.1.0");

    // A version the registry does not have, and a package it does not have.
    let (ok, _, err) = azazel(&["pkg", "add", "npm:@acme/review-kit@9.9.9", "--name", "other"]);
    assert!(!ok && err.contains("has no version or tag `9.9.9`"), "{err}");
    let (ok, _, err) = azazel(&["pkg", "add", "npm:absent"]);
    assert!(
        !ok && err.contains("the registry has no package named absent"),
        "{err}"
    );
    assert!(!sandbox.path("home/packages/other").exists());

    let (ok, removed, _) = azazel(&["pkg", "remove", "review-kit"]);
    assert!(ok && removed.trim_end() == "removed review-kit");
    assert!(!root.exists());
}
