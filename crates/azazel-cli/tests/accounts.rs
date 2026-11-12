#![allow(clippy::unwrap_used)]

//! Several accounts per provider, moving off an account that ran out of allowance, service tiers
//! and model presets, as the binary does them.

use std::path::PathBuf;
use std::process::Output;

use assert_cmd::Command;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const REPLY: &str = concat!(
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello from work\"}}]}\n\n",
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        Sandbox { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn requests_file(&self) -> PathBuf {
        self.dir.path().join("requests.jsonl")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.home())
            .env_remove("AZAZEL_MOCK_SCRIPT")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .args(args)
            .arg("--cwd")
            .arg(self.dir.path().join("project"))
            .write_stdin("");
        command
    }

    /// Runs `azazel` against real endpoints (the mock servers of a test).
    async fn azazel(&self, args: &[&str]) -> Output {
        let mut command = self.command(args);
        tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap()
    }

    /// Runs `azazel` answering from `script`; the requests it made are recorded.
    fn scripted(&self, script: &Value, args: &[&str]) -> Output {
        let script_path = self.dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let _ = std::fs::remove_file(self.requests_file());
        self.command(args)
            .env("AZAZEL_MOCK_SCRIPT", &script_path)
            .env("AZAZEL_MOCK_REQUESTS", self.requests_file())
            .output()
            .unwrap()
    }

    /// Every recorded request as `(model, request)`.
    fn requests(&self) -> Vec<(String, Value)> {
        std::fs::read_to_string(self.requests_file())
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let recorded: Value = serde_json::from_str(line).unwrap();
                (
                    recorded["model"].as_str().unwrap().to_string(),
                    recorded["request"].clone(),
                )
            })
            .collect()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_account_is_signed_in_to_listed_and_signed_out_of() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "dc-1",
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://id.example/device",
            "expires_in": 600,
            "interval": 0
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-1",
            "refresh_token": "rt-1",
            "expires_in": 3600
        })))
        .mount(&server)
        .await;
    let sandbox = Sandbox::new();
    let uri = server.uri();
    std::fs::write(
        sandbox.home().join("config.toml"),
        format!(
            "[providers.gateway]\napi = \"openai-chat\"\nbase_url = \"{uri}/v1\"\n\n\
             [providers.gateway.oauth]\nflow = \"device\"\nclient_id = \"app-1\"\n\
             device_url = \"{uri}/device\"\ntoken_url = \"{uri}/token\"\n"
        ),
    )
    .unwrap();
    let auth_file = sandbox.home().join("auth.json");

    // A label that cannot name an account is a usage error before anything is asked.
    let bad = sandbox.azazel(&["login", "gateway", "--as", "a b"]).await;
    assert_eq!(bad.status.code(), Some(2), "{}", text(&bad.stderr));
    assert!(
        text(&bad.stderr).contains("cannot name an account"),
        "{}",
        text(&bad.stderr)
    );

    for (args, said) in [
        (
            vec!["login", "gateway"],
            "signed in to gateway; the login is kept in".to_string(),
        ),
        (
            vec!["login", "gateway", "--as", "work"],
            "signed in to gateway as account `work`; the login is kept in".to_string(),
        ),
    ] {
        let login = sandbox.azazel(&args).await;
        assert_eq!(login.status.code(), Some(0), "{}", text(&login.stderr));
        assert!(text(&login.stdout).starts_with(&said), "{}", text(&login.stdout));
    }
    let stored: Value = serde_json::from_slice(&std::fs::read(&auth_file).unwrap()).unwrap();
    assert_eq!(stored["gateway"]["type"], "oauth");
    assert_eq!(stored["gateway#work"]["refresh"], "rt-1");

    let status = sandbox.azazel(&["auth", "status", "--json"]).await;
    let row: Value = serde_json::from_str(text(&status.stdout).lines().next().unwrap()).unwrap();
    assert_eq!(row["provider"], "gateway");
    assert_eq!(row["source"], "auth.json (login)");
    assert_eq!(row["accounts"], json!(["default", "work"]));

    // The configured account is the one in use.
    let mut config = std::fs::read_to_string(sandbox.home().join("config.toml")).unwrap();
    config = config.replace(
        "api = \"openai-chat\"",
        "api = \"openai-chat\"\naccount = \"work\"",
    );
    std::fs::write(sandbox.home().join("config.toml"), config).unwrap();
    let status = text(&sandbox.azazel(&["auth", "status"]).await.stdout);
    assert!(status.contains("auth.json (login, account work)"), "{status}");
    assert!(status.contains("accounts: default, work"), "{status}");

    let logout = sandbox.azazel(&["logout", "gateway", "--account", "work"]).await;
    assert_eq!(
        text(&logout.stdout),
        "forgot the stored gateway login of account `work`"
    );
    let stored: Value = serde_json::from_slice(&std::fs::read(&auth_file).unwrap()).unwrap();
    assert!(stored.get("gateway#work").is_none() && stored.get("gateway").is_some());
    let again = sandbox.azazel(&["logout", "gateway", "--account", "work"]).await;
    assert_eq!(again.status.code(), Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_used_up_account_hands_the_run_over_to_the_next_one() {
    let server = MockServer::start().await;
    // The default account's plan is used up; the work account answers.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer k-default"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached",
                "resets_in_seconds": 7200
            }
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer k-work"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(REPLY, "text/event-stream"))
        .expect(2)
        .mount(&server)
        .await;
    let sandbox = Sandbox::new();
    std::fs::write(
        sandbox.home().join("config.toml"),
        format!(
            "[providers.gateway]\napi = \"openai-chat\"\nbase_url = \"{}/v1\"\n",
            server.uri()
        ),
    )
    .unwrap();
    std::fs::write(
        sandbox.home().join("auth.json"),
        json!({
            "gateway": {"type": "api_key", "key": "k-default"},
            "gateway#work": {"type": "api_key", "key": "k-work"}
        })
        .to_string(),
    )
    .unwrap();

    // Memory would add requests of its own to the count.
    let run = sandbox
        .azazel(&["-p", "hi", "--model", "gateway/small", "--no-memory"])
        .await;
    assert_eq!(run.status.code(), Some(0), "{}", text(&run.stderr));
    assert_eq!(text(&run.stdout), "hello from work");
    let said = text(&run.stderr);
    assert!(
        said.contains("gateway: account default reached its usage limit (resets ")
            && said.contains("); now using work"),
        "{said}"
    );

    // As events, for a program that drives azazel.
    let run = sandbox
        .azazel(&[
            "-p",
            "hi",
            "--model",
            "gateway/small",
            "--no-memory",
            "--output",
            "json",
        ])
        .await;
    assert_eq!(run.status.code(), Some(0), "{}", text(&run.stderr));
    let events: Vec<Value> = text(&run.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let switch = events
        .iter()
        .find(|event| event["type"] == "account")
        .unwrap_or_else(|| panic!("no account event in {events:?}"));
    assert_eq!(
        (
            &switch["provider"],
            &switch["from"],
            &switch["to"],
            &switch["limit"]
        ),
        (
            &json!("gateway"),
            &json!("default"),
            &json!("work"),
            &json!("usage limit")
        )
    );
    assert!(switch["until"].as_u64().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tier_goes_out_with_the_request_and_is_dropped_when_the_model_refuses_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"service_tier": "priority"})))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {
                "message": "Unsupported parameter: 'service_tier' is not supported with this model.",
                "type": "invalid_request_error",
                "param": "service_tier",
                "code": "unsupported_parameter"
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(REPLY, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let sandbox = Sandbox::new();
    std::fs::write(
        sandbox.home().join("config.toml"),
        format!(
            "[providers.gateway]\napi = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key = \"k\"\n",
            server.uri()
        ),
    )
    .unwrap();

    let run = sandbox
        .azazel(&[
            "-p",
            "hi",
            "--model",
            "gateway/small",
            "--no-memory",
            "--tier",
            "priority",
        ])
        .await;
    assert_eq!(run.status.code(), Some(0), "{}", text(&run.stderr));
    assert_eq!(text(&run.stdout), "hello from work");
    assert!(
        text(&run.stderr).contains("gateway/small does not take the priority tier"),
        "{}",
        text(&run.stderr)
    );
}

#[test]
fn tier_and_preset_flags_shape_the_requests_of_a_scripted_run() {
    let sandbox = Sandbox::new();
    std::fs::write(
        sandbox.home().join("config.toml"),
        "[presets.cheap.roles]\ndefault = [\"mock/small\"]\n",
    )
    .unwrap();
    let script = json!({
        "lead": [{ "reply": { "text": "from lead" } }, { "reply": { "text": "from lead" } }],
        "small": [{ "reply": { "text": "from small" } }]
    });

    let fast = sandbox.scripted(&script, &["-p", "hi", "--tier", "priority"]);
    assert_eq!(fast.status.code(), Some(0), "{}", text(&fast.stderr));
    let requests = sandbox.requests();
    assert_eq!(requests[0].1["tier"], "priority");

    // The standard tier is the same as naming none.
    sandbox.scripted(&script, &["-p", "hi", "--tier", "standard"]);
    assert!(sandbox.requests()[0].1.get("tier").is_none());

    let unknown = sandbox.scripted(&script, &["-p", "hi", "--tier", "turbo"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(
        text(&unknown.stderr).contains("--tier: unknown tier `turbo`"),
        "{}",
        text(&unknown.stderr)
    );

    // A preset's roles replace the configured ones for the run.
    let cheap = sandbox.scripted(&script, &["-p", "hi", "--preset", "cheap"]);
    assert_eq!(cheap.status.code(), Some(0), "{}", text(&cheap.stderr));
    assert_eq!(text(&cheap.stdout), "from small");
    assert_eq!(sandbox.requests()[0].0, "small");

    let missing = sandbox.scripted(&script, &["-p", "hi", "--preset", "dear"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(
        text(&missing.stderr).contains("--preset: no preset `dear`; there are: cheap"),
        "{}",
        text(&missing.stderr)
    );
}
