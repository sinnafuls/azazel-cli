//! `[profiles] on_limit` over the real HTTP path: the lead's provider is a local server that
//! speaks Anthropic Messages and answers with what Anthropic sends when a subscription is out of
//! allowance, the profile it falls back to is a second server that speaks OpenAI Responses.
//! Nothing here is scripted: the classification of the answer is the program's own.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::common::{stderr, stdout};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const MESSAGES: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"claude is back\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

const RESPONSES: &str = concat!(
    "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
    "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"content\":[],\"role\":\"assistant\"}}\n\n",
    "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"gpt took over\"}\n\n",
    "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"annotations\":[],\"text\":\"gpt took over\"}],\"role\":\"assistant\"}}\n\n",
    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"annotations\":[],\"text\":\"gpt took over\"}],\"role\":\"assistant\"}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
);

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// What Anthropic answers a subscription whose 5-hour window is full: 429, the unified rate-limit
/// headers with the reset time, and the `rate_limit_error` envelope.
fn usage_limit() -> ResponseTemplate {
    let reset = (now() + 2 * 3600).to_string();
    ResponseTemplate::new(429)
        .insert_header("anthropic-ratelimit-unified-status", "rejected")
        .insert_header("anthropic-ratelimit-unified-representative-claim", "five_hour")
        .insert_header("anthropic-ratelimit-unified-reset", reset.as_str())
        .insert_header("anthropic-ratelimit-unified-5h-utilization", "1.0")
        .insert_header("anthropic-ratelimit-unified-5h-reset", reset.as_str())
        .set_body_json(json!({
            "type": "error",
            "error": { "type": "rate_limit_error", "message": "This request would exceed your account's rate limit." }
        }))
}

/// A plain 429 that asks to wait two hours, as an API key's allowance does.
fn long_retry_after() -> ResponseTemplate {
    ResponseTemplate::new(429)
        .insert_header("retry-after", "7200")
        .set_body_json(json!({
            "type": "error",
            "error": { "type": "rate_limit_error", "message": "Your monthly spend limit was reached." }
        }))
}

/// Anthropic's transient overload.
fn overloaded() -> ResponseTemplate {
    ResponseTemplate::new(529).set_body_json(json!({
        "type": "error",
        "error": { "type": "overloaded_error", "message": "Overloaded" }
    }))
}

fn messages_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(MESSAGES, "text/event-stream")
}

/// A server that answers its first request with `first` and every later one with the reply.
async fn anthropic(first: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(first)
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(messages_ok())
        .mount(&server)
        .await;
    server
}

async fn openai() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(RESPONSES, "text/event-stream"))
        .mount(&server)
        .await;
    server
}

/// The JSON bodies of the requests a server got to `route`.
async fn bodies(server: &MockServer, route: &str) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request: &&Request| request.url.path() == route)
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

/// The names of the tools a request offers, on either wire.
fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new(claude: &MockServer, gpt: &MockServer) -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(dir.path().join("project/.azazel")).unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[profiles]\non_limit = \"fallback\"\n\
                 [profiles.fallback.roles]\ndefault = [\"openai/gpt-5\"]\n\
                 [providers.anthropic]\nbase_url = \"{}\"\napi_key = \"claude-key\"\n\
                 [providers.openai]\nbase_url = \"{}/v1\"\napi_key = \"gpt-key\"\n",
                claude.uri(),
                gpt.uri()
            ),
        )
        .unwrap();
        Sandbox { dir }
    }

    async fn run(&self) -> Output {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("azazel"));
        command
            .env("AZAZEL_HOME", self.dir.path().join("home"))
            .env_remove("AZAZEL_MOCK_SCRIPT")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .args([
                "-p",
                "go",
                "--model",
                "anthropic/claude-test",
                "--mode",
                "auto",
                "--no-memory",
                "--set",
                "session.auto_name=false",
                "--cwd",
            ])
            .arg(self.dir.path().join("project"))
            .stdin(Stdio::null());
        tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap()
    }

    /// The entries of the session file the run wrote.
    fn entries(&self) -> Vec<Value> {
        let projects = self.dir.path().join("home/projects");
        let file: PathBuf = std::fs::read_dir(projects)
            .unwrap()
            .flatten()
            .flat_map(|project| {
                std::fs::read_dir(project.path().join("sessions"))
                    .unwrap()
                    .flatten()
            })
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .expect("a session file");
        std::fs::read_to_string(file)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

async fn assert_switched(claude: &MockServer, gpt: &MockServer, sandbox: &Sandbox, output: &Output) {
    assert!(output.status.success(), "{}", stderr(output));
    assert!(stdout(output).contains("gpt took over"), "{}", stdout(output));
    let notice = stderr(output);
    assert!(
        notice.contains("usage limit reached") && notice.contains("Switched to profile fallback"),
        "{notice}"
    );
    assert_eq!(notice.matches("Switched to").count(), 1, "{notice}");

    // The request that was refused, then the same turn on the other provider.
    let sent = bodies(claude, "/v1/messages").await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    let retried = bodies(gpt, "/v1/responses").await;
    assert_eq!(retried.len(), 1, "{retried:?}");
    assert_eq!(retried[0]["model"], "gpt-5");
    // Each provider got the tools of its own family: GPT edits with `patch`, Claude with `edit`.
    let (claude_tools, gpt_tools) = (tool_names(&sent[0]), tool_names(&retried[0]));
    assert!(claude_tools.contains(&"edit".to_string()) && !claude_tools.contains(&"patch".to_string()));
    assert!(gpt_tools.contains(&"patch".to_string()) && !gpt_tools.contains(&"edit".to_string()));
    assert!(gpt_tools.contains(&"read".to_string()));

    // The session says which profile it is on.
    let changes: Vec<Value> = sandbox
        .entries()
        .into_iter()
        .filter(|entry| entry["type"] == "profile_change")
        .collect();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0]["profile"], "fallback");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscription_that_is_out_of_allowance_moves_the_run_to_the_profile_on_another_provider() {
    let (claude, gpt) = (anthropic(usage_limit()).await, openai().await);
    let sandbox = Sandbox::new(&claude, &gpt);
    let output = sandbox.run().await;
    assert_switched(&claude, &gpt, &sandbox, &output).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_429_that_asks_for_a_long_wait_counts_as_a_usage_limit_too() {
    let (claude, gpt) = (anthropic(long_retry_after()).await, openai().await);
    let sandbox = Sandbox::new(&claude, &gpt);
    let output = sandbox.run().await;
    assert_switched(&claude, &gpt, &sandbox, &output).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transient_overload_is_retried_on_the_same_provider_and_switches_nothing() {
    let (claude, gpt) = (anthropic(overloaded()).await, openai().await);
    let sandbox = Sandbox::new(&claude, &gpt);
    let output = sandbox.run().await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("claude is back"), "{}", stdout(&output));
    assert!(!stderr(&output).contains("usage limit"), "{}", stderr(&output));
    assert_eq!(bodies(&claude, "/v1/messages").await.len(), 2);
    assert!(gpt.received_requests().await.unwrap().is_empty());
    assert!(
        sandbox
            .entries()
            .iter()
            .all(|entry| entry["type"] != "profile_change")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_api_account_without_credit_hands_the_same_model_to_the_next_account() {
    let server = MockServer::start().await;
    // The API-key account is out of credit; the subscription login answers.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(wiremock::matchers::header("x-api-key", "k-api"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(wiremock::matchers::header("authorization", "Bearer access-token"))
        .respond_with(messages_ok())
        .expect(2)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(dir.path().join("project/.azazel")).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[session]\nauto_name = false\n[accounts]\norder = {{ anthropic = [\"default\", \"work\"] }}\n\
             [providers.anthropic]\nbase_url = \"{}\"\n",
            server.uri()
        ),
    )
    .unwrap();
    std::fs::write(
        home.join("auth.json"),
        json!({
            "anthropic": { "type": "api_key", "key": "k-api" },
            "anthropic#work": {
                "type": "oauth",
                "access": "access-token",
                "refresh": "refresh-token",
                "expires": 4_102_444_800_000u64,
                "email": "jane@example.com"
            }
        })
        .to_string(),
    )
    .unwrap();
    let run = || {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("azazel"));
        command
            .env("AZAZEL_HOME", &home)
            .env_remove("AZAZEL_MOCK_SCRIPT")
            .env_remove("ANTHROPIC_API_KEY")
            .args([
                "-p",
                "go",
                "--model",
                "anthropic/claude-test",
                "--no-memory",
                "--cwd",
            ])
            .arg(dir.path().join("project"))
            .stdin(Stdio::null());
        tokio::task::spawn_blocking(move || command.output().unwrap())
    };

    let first = run().await.unwrap();
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(stdout(&first).contains("claude is back"), "{}", stdout(&first));
    let said = stderr(&first);
    assert!(
        said.contains("anthropic: account default") && said.contains("now using work"),
        "{said}"
    );
    // The same model went to both accounts.
    let sent = bodies(&server, "/v1/messages").await;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent.iter().all(|body| body["model"] == "claude-test"));

    // The account stays marked: the next run starts on the one with credit and never asks the
    // empty one (`expect(1)` above).
    let second = run().await.unwrap();
    assert!(second.status.success(), "{}", stderr(&second));
}
