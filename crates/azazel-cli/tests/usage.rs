#![allow(clippy::unwrap_used)]

//! `azazel usage` against the providers' usage endpoints, and what a refusal for lack of
//! allowance says, as the binary does them.

use std::path::PathBuf;
use std::process::Output;

use assert_cmd::Command;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

    async fn azazel(&self, args: &[&str]) -> Output {
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
        tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

fn login(email: &str, account_id: &str) -> Value {
    json!({
        "type": "oauth",
        "access": "access-token",
        "refresh": "refresh-token",
        // Far enough ahead that nothing tries to refresh it.
        "expires": 4_102_444_800_000u64,
        "email": email,
        "account_id": account_id
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_asks_both_subscriptions_and_remembers_the_answers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oauth/usage"))
        .and(header("authorization", "Bearer access-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "five_hour": { "utilization": 42.0, "resets_at": "2099-01-01T00:00:00Z" },
            "seven_day": { "utilization": 9.0, "resets_at": "2099-01-03T00:00:00Z" },
            "seven_day_opus": null,
            "limits": [
                { "kind": "weekly_all", "percent": 9, "resets_at": "2099-01-03T00:00:00Z", "is_active": false },
                { "kind": "weekly_scoped", "percent": 100, "resets_at": "2099-01-03T00:00:00Z",
                  "scope": { "model": { "display_name": "Fable" } }, "is_active": true }
            ],
            "extra_usage": { "is_enabled": false }
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .and(header("chatgpt-account-id", "acct-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": { "used_percent": 5, "limit_window_seconds": 18000, "reset_after_seconds": 16000 },
                "secondary_window": { "used_percent": 14, "limit_window_seconds": 604800, "reset_after_seconds": 500000 }
            },
            "credits": { "has_credits": false, "unlimited": false }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let sandbox = Sandbox::new();
    // No login yet: said plainly, and not a failure.
    let none = sandbox.azazel(&["usage"]).await;
    assert_eq!(none.status.code(), Some(0), "{}", text(&none.stderr));
    assert!(text(&none.stdout).starts_with("no subscription is signed in"));
    let none = sandbox.azazel(&["usage", "--json"]).await;
    assert_eq!(text(&none.stdout), "[]");

    std::fs::write(
        sandbox.home().join("config.toml"),
        format!(
            "[providers.anthropic]\nbase_url = \"{uri}\"\n[providers.openai-codex]\nbase_url = \"{uri}/backend-api/codex\"\n",
            uri = server.uri()
        ),
    )
    .unwrap();
    std::fs::write(
        sandbox.home().join("auth.json"),
        json!({
            "anthropic": login("a@example.com", "acct-a"),
            "openai-codex": login("c@example.com", "acct-1"),
            // A stored key has no usage windows and is not asked about.
            "groq": { "type": "api_key", "key": "k" }
        })
        .to_string(),
    )
    .unwrap();

    let shown = sandbox.azazel(&["usage"]).await;
    assert_eq!(shown.status.code(), Some(0), "{}", text(&shown.stderr));
    // Into a pipe the rows of the box come as plain text.
    let out = text(&shown.stdout);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "anthropic");
    // The address is hidden but for its first letter and its domain.
    assert_eq!(lines[1], " ● default  a***@example.com · in use");
    assert!(
        lines[2].starts_with("     5h        ━━━━━━━━────────────  42%  resets in "),
        "{out}"
    );
    assert!(
        lines[3].starts_with("     7d        ━━──────────────────   9%  resets in "),
        "{out}"
    );
    // The weekly window of one model family, with what it counts.
    assert!(
        lines[4].starts_with("     7d Fable  ━━━━━━━━━━━━━━━━━━━━ 100%  no requests for ")
            && lines[4].ends_with("  Fable only"),
        "{out}"
    );
    assert_eq!(lines[5], "     extra usage off");
    assert_eq!(lines[6], "");
    assert_eq!(lines[7], "openai-codex");
    assert_eq!(lines[8], " ● default  c***@example.com · plus plan · in use");
    assert!(
        lines[9].starts_with("     5h        ━───────────────────   5%  resets in 4 h 2"),
        "{out}"
    );
    assert!(lines[10].contains(" 14%  resets in 5 d 1"), "{out}");
    assert!(!out.contains("groq"), "{out}");

    // The answers are kept per provider and account, without anything secret in them.
    let cache = std::fs::read_to_string(sandbox.home().join("cache").join("limits.json")).unwrap();
    let cached: Value = serde_json::from_str(&cache).unwrap();
    assert_eq!(cached["anthropic"]["windows"][0]["name"], "5h");
    assert_eq!(cached["anthropic"]["windows"][2]["scope"], "Fable");
    assert_eq!(cached["openai-codex"]["plan"], "plus");
    assert!(!cache.contains("access-token") && !cache.contains("refresh-token"));

    // A second look, as JSON, is answered from the cache: each endpoint was asked once.
    let listed = sandbox.azazel(&["usage", "--json"]).await;
    assert_eq!(listed.status.code(), Some(0), "{}", text(&listed.stderr));
    let accounts: Value = serde_json::from_str(&text(&listed.stdout)).unwrap();
    assert_eq!(accounts.as_array().unwrap().len(), 2);
    assert_eq!(accounts[0]["provider"], "anthropic");
    assert_eq!(accounts[0]["email"], "a***@example.com");
    assert_eq!(accounts[0]["in_use"], true);
    assert_eq!(accounts[0]["extra_usage"], false);
    assert_eq!(accounts[0]["windows"][2]["label"], "7-day Fable limit");
    assert_eq!(accounts[1]["windows"][1]["name"], "7d");
    assert_eq!(accounts[1]["error"], Value::Null);

    // With the setting off the address is shown whole, in both forms.
    let whole = sandbox
        .azazel(&["usage", "--set", "usage.redact_email=false"])
        .await;
    assert!(
        text(&whole.stdout).contains(" ● default  a@example.com · in use"),
        "{}",
        text(&whole.stdout)
    );
    let whole = sandbox
        .azazel(&["usage", "--json", "--set", "usage.redact_email=false"])
        .await;
    let accounts: Value = serde_json::from_str(&text(&whole.stdout)).unwrap();
    assert_eq!(accounts[1]["email"], "c@example.com");
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_that_cannot_be_asked_for_says_why_and_fails() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oauth/usage"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let sandbox = Sandbox::new();
    std::fs::write(
        sandbox.home().join("config.toml"),
        format!("[providers.anthropic]\nbase_url = \"{}\"\n", server.uri()),
    )
    .unwrap();
    std::fs::write(
        sandbox.home().join("auth.json"),
        json!({ "anthropic": login("a@example.com", "acct-a") }).to_string(),
    )
    .unwrap();
    let shown = sandbox.azazel(&["usage"]).await;
    assert_eq!(shown.status.code(), Some(1));
    let out = text(&shown.stdout);
    assert!(
        out.starts_with("anthropic\n ● default  a***@example.com · in use"),
        "{out}"
    );
    assert!(
        out.contains("anthropic answered HTTP 503 Service Unavailable when asked for its usage"),
        "{out}"
    );
    // Nothing was learnt, so nothing is kept.
    assert!(!sandbox.home().join("cache").join("limits.json").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_for_extra_usage_names_the_full_window_and_its_reset() {
    let server = MockServer::start().await;
    let reset = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 2 * 3600
        + 10 * 60
        + 30;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header("anthropic-ratelimit-unified-status", "allowed")
                .insert_header("anthropic-ratelimit-unified-5h-utilization", "1.0")
                .insert_header("anthropic-ratelimit-unified-5h-reset", reset.to_string().as_str())
                .insert_header("anthropic-ratelimit-unified-7d-utilization", "0.31")
                .set_body_json(json!({
                    "type": "error",
                    "error": {
                        "type": "invalid_request_error",
                        "message": "You're out of extra usage. Add more at claude.ai/settings/usage and keep going."
                    }
                })),
        )
        .mount(&server)
        .await;
    let sandbox = Sandbox::new();
    std::fs::write(
        sandbox.home().join("config.toml"),
        format!(
            "[session]\nauto_name = false\n[providers.anthropic]\nbase_url = \"{}\"\n",
            server.uri()
        ),
    )
    .unwrap();
    std::fs::write(
        sandbox.home().join("auth.json"),
        json!({ "anthropic": { "type": "api_key", "key": "k-stored" } }).to_string(),
    )
    .unwrap();

    let run = sandbox
        .azazel(&["-p", "hi", "--model", "anthropic/claude-test", "--no-memory"])
        .await;
    assert_eq!(run.status.code(), Some(1));
    let said = text(&run.stderr);
    assert!(said.contains("You're out of extra usage."), "{said}");
    assert!(
        said.contains("the 5-hour limit of this account is full; it resets in 2 h 10 min"),
        "{said}"
    );
}
