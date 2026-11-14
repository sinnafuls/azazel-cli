//! `web_search` through the lead's own provider, as the binary wires it: the search request goes
//! out on the lead's model, `[web]` decides whether it does, and `doctor` names who answers.

use std::path::PathBuf;
use std::process::Output;

use assert_cmd::Command;
use serde_json::{Value, json};

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        Sandbox { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// `azazel` in this sandbox, with no search key in its environment.
    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.path("home"))
            .arg("--cwd")
            .arg(self.path("project"));
        for key in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "BRAVE_API_KEY",
            "TAVILY_API_KEY",
            "EXA_API_KEY",
        ] {
            command.env_remove(key);
        }
        command
    }

    /// `azazel -p go` answering from `script`, every request recorded.
    fn run(&self, script: &Value, args: &[&str]) -> Output {
        std::fs::write(self.path("script.json"), script.to_string()).unwrap();
        let _ = std::fs::remove_file(self.path("requests.jsonl"));
        self.command()
            .env("AZAZEL_MOCK_SCRIPT", self.path("script.json"))
            .env("AZAZEL_MOCK_REQUESTS", self.path("requests.jsonl"))
            .args(["--mode", "auto"])
            .args(["--set", "memory.auto_extract=false"])
            .args(["--set", "memory.auto_index=false"])
            .args(["--set", "session.auto_name=false"])
            .args(args)
            .args(["-p", "go"])
            .write_stdin("")
            .output()
            .unwrap()
    }

    /// Every request the mock provider received, as `{"model", "request"}`.
    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// The tool results the lead had been given by its last request.
    fn results(&self) -> Vec<String> {
        let requests = self.requests();
        let last = requests
            .iter()
            .rfind(|sent| sent["request"].get("hosted").is_none())
            .unwrap();
        let mut results = Vec::new();
        for message in last["request"]["messages"].as_array().unwrap() {
            for block in message["content"].as_array().unwrap() {
                if block["type"] == "tool_result" {
                    results.push(block["content"][0]["text"].as_str().unwrap().to_string());
                }
            }
        }
        results
    }
}

/// The lead searches once, then stops.
fn lead() -> Value {
    json!([
        {"reply": {"tool_calls": [
            {"id": "c1", "name": "web_search", "input": {"query": "tokio latest release", "limit": 3}}
        ]}},
        {"reply": {"text": "done"}},
    ])
}

#[test]
fn the_lead_searches_through_its_provider_on_its_own_model() {
    let sandbox = Sandbox::new();
    let script = json!({
        "lead": lead(),
        "web_search": [{"reply": {
            "text": "Tokio 1.48 is out.",
            "usage": {"input": 900, "output": 40},
            "sources": [
                {"url": "https://tokio.rs/blog/announcing-1-48", "title": "Announcing Tokio 1.48",
                 "snippet": "Tokio 1.48 is out."},
                {"url": "https://github.com/tokio-rs/tokio/releases", "title": "Releases"},
            ],
        }}],
    });
    let output = sandbox.run(&script, &[]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        sandbox.results(),
        [
            "1. Announcing Tokio 1.48\n   https://tokio.rs/blog/announcing-1-48\n   Tokio 1.48 is out.\n\n\
          2. Releases\n   https://github.com/tokio-rs/tokio/releases"
        ]
    );

    // The search was one request of its own on the lead's model, between the lead's two.
    let requests = sandbox.requests();
    let models: Vec<&str> = requests
        .iter()
        .map(|sent| sent["model"].as_str().unwrap())
        .collect();
    assert_eq!(models, ["lead", "lead", "lead"]);
    let search = &requests[1]["request"];
    assert_eq!(search["hosted"], json!([{"type": "web_search", "max_uses": 2}]));
    assert_eq!(
        search["messages"][0]["content"][0]["text"],
        "Query: tokio latest release\nPages wanted: 3"
    );
    assert!(search["tools"].as_array().unwrap().is_empty());
}

#[test]
fn the_web_table_decides_whether_the_provider_is_asked() {
    let sandbox = Sandbox::new();
    let found = json!([{"reply": {"sources": [{"url": "https://tokio.rs/", "title": "Tokio"}]}}]);
    // A backend that refuses at once stands in for the configured one.
    let unreachable = [
        "--set",
        "web.search=searxng",
        "--set",
        "web.searxng_url=http://127.0.0.1:1",
    ];

    // A backend named explicitly is used alone, though the provider could search.
    let output = sandbox.run(&json!({"lead": lead(), "web_search": found}), &unreachable);
    assert!(output.status.success(), "{output:?}");
    let results = sandbox.results();
    assert!(results[0].starts_with("searxng search failed"), "{results:?}");
    assert!(
        sandbox
            .requests()
            .iter()
            .all(|sent| sent["request"].get("hosted").is_none())
    );

    // `provider` asks nobody else: with a lead whose provider has no search tool, the search
    // fails and says what to change.
    let output = sandbox.run(&json!({"lead": lead()}), &["--set", "web.search=provider"]);
    assert!(output.status.success(), "{output:?}");
    let results = sandbox.results();
    assert!(
        results[0].contains("[web] search is \"provider\", but the lead's model has no web search"),
        "{results:?}"
    );

    // With a provider that searches, `provider` is answered by it.
    let output = sandbox.run(
        &json!({"lead": lead(), "web_search": found}),
        &["--set", "web.search=provider"],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(sandbox.results(), ["1. Tokio\n   https://tokio.rs/"]);
}
