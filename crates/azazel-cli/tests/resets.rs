#![allow(clippy::unwrap_used)]

//! A subscription that runs out in the middle of a run, and the banked reset that gives its
//! window back, as the binary does them: nothing is spent unattended unless
//! `usage.auto_reset` says so, and with a client attached the choice is a question to it.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const REPLY: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"back at work\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);
const RESET_PATH: &str = "/api/organizations/org-1/reset_rate_limits";

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// A stand-in for Claude's subscription endpoints: the 5-hour window of the account is full
/// from the first request on, until it resets `resets_after` seconds later or the account's
/// session reset is spent.
struct Claude {
    server: MockServer,
}

impl Claude {
    async fn start(resets_after: u64) -> Claude {
        let server = MockServer::start().await;
        let cleared = Arc::new(AtomicBool::new(false));
        // Unix seconds at which the window resets; 0 until the first request was refused.
        let resets = Arc::new(AtomicU64::new(0));
        let open = {
            let (cleared, resets) = (cleared.clone(), resets.clone());
            move || {
                let reset_at = resets.load(Ordering::SeqCst);
                cleared.load(Ordering::SeqCst) || (reset_at > 0 && now() >= reset_at)
            }
        };
        let answering = open.clone();
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(move |_: &Request| {
                if answering() {
                    return ResponseTemplate::new(200).set_body_raw(REPLY, "text/event-stream");
                }
                let _ = resets.compare_exchange(0, now() + resets_after, Ordering::SeqCst, Ordering::SeqCst);
                let reset_at = resets.load(Ordering::SeqCst);
                ResponseTemplate::new(429)
                    .insert_header("anthropic-ratelimit-unified-status", "rejected")
                    .insert_header("anthropic-ratelimit-unified-representative-claim", "five_hour")
                    .insert_header("anthropic-ratelimit-unified-reset", reset_at.to_string().as_str())
                    .insert_header("anthropic-ratelimit-unified-5h-utilization", "1.0")
                    .insert_header("anthropic-ratelimit-unified-5h-reset", reset_at.to_string().as_str())
                    .insert_header("anthropic-ratelimit-unified-7d-utilization", "0.31")
                    .set_body_json(json!({
                        "type": "error",
                        "error": { "type": "rate_limit_error", "message": "This request would exceed your account's rate limit." }
                    }))
            })
            .mount(&server)
            .await;
        let reporting = open.clone();
        let spent = cleared.clone();
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(move |_: &Request| {
                let used = if reporting() { 3.0 } else { 100.0 };
                ResponseTemplate::new(200).set_body_json(json!({
                    "five_hour": { "utilization": used, "resets_at": "2099-01-01T00:00:00Z" },
                    "seven_day": { "utilization": 31.0, "resets_at": "2099-01-03T00:00:00Z" },
                    "cedar_ember": null,
                    "juniper_tide": {
                        "eligible": true, "arm": "reset", "available": !spent.load(Ordering::SeqCst),
                        "next_available_at": null, "resets_per_week": 1
                    }
                }))
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(RESET_PATH))
            .respond_with(move |_: &Request| {
                cleared.store(true, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({ "result": "reset" }))
            })
            .mount(&server)
            .await;
        Claude { server }
    }

    /// The bodies of the redemptions the provider got.
    async fn redemptions(&self) -> Vec<Value> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path() == RESET_PATH)
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }

    /// How often the model was asked.
    async fn prompts(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/v1/messages")
            .count()
    }
}

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    /// A home whose `anthropic` is `claude`, signed in to with a subscription.
    fn new(claude: &Claude, more: &str) -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[session]\nauto_name = false\n[usage]\npoll_secs = 0\n[providers.anthropic]\nbase_url = \"{}\"\n{more}",
                claude.server.uri()
            ),
        )
        .unwrap();
        std::fs::write(
            home.join("auth.json"),
            json!({ "anthropic": {
                "type": "oauth",
                "access": "access-token",
                "refresh": "refresh-token",
                // Far enough ahead that nothing tries to refresh it.
                "expires": 4_102_444_800_000u64,
                "email": "jane@example.com",
                "org_id": "org-1"
            } })
            .to_string(),
        )
        .unwrap();
        Sandbox { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("azazel"));
        command
            .env("AZAZEL_HOME", self.home())
            .env_remove("AZAZEL_MOCK_SCRIPT")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .args(args)
            .arg("--cwd")
            .arg(self.dir.path().join("project"));
        command
    }

    async fn azazel(&self, args: &[&str]) -> Output {
        let mut command = self.command(args);
        command.stdin(Stdio::null());
        tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap()
    }

    /// The account's last redemption as the journal has it.
    fn journal(&self) -> Option<Value> {
        let text = std::fs::read_to_string(self.home().join("resets").join("anthropic.default.json")).ok()?;
        Some(serde_json::from_str(&text).unwrap())
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

const PRINT: [&str; 5] = ["-p", "hi", "--model", "anthropic/claude-test", "--no-memory"];

#[tokio::test(flavor = "multi_thread")]
async fn a_run_with_nobody_there_spends_a_reset_only_when_the_settings_say_so() {
    let claude = Claude::start(3 * 3600).await;
    let sandbox = Sandbox::new(&claude, "");

    // As before: the run fails with what ran out, and the reset stays where it is.
    let run = sandbox.azazel(&PRINT).await;
    assert_eq!(run.status.code(), Some(1), "{}", text(&run.stdout));
    let said = text(&run.stderr);
    assert!(said.contains("usage limit reached"), "{said}");
    assert!(claude.redemptions().await.is_empty());
    assert_eq!(sandbox.journal(), None);

    // With `usage.auto_reset` the reset is spent, once, and the run goes on.
    let mut args = PRINT.to_vec();
    args.extend(["--set", "usage.auto_reset=true"]);
    let run = sandbox.azazel(&args).await;
    assert_eq!(run.status.code(), Some(0), "{}", text(&run.stderr));
    assert_eq!(text(&run.stdout), "back at work");
    assert_eq!(claude.redemptions().await, [json!({ "program": "juniper_tide" })]);
    // Written down, and settled by the usage report that showed the room afterwards.
    let entry = sandbox.journal().unwrap();
    assert_eq!(
        (&entry["kind"], &entry["state"], &entry["clears"]),
        (&json!("session"), &json!("confirmed"), &json!(["5h"]))
    );
    assert!(!entry.to_string().contains("access-token"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reset_that_does_not_cover_what_is_full_is_not_spent_unattended() {
    // The weekly window is what ran out; the account's session reset gives back the 5-hour
    // window only.
    let claude = Claude::start(3 * 3600).await;
    claude.server.reset().await;
    let reset_at = now() + 3 * 86_400;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("anthropic-ratelimit-unified-status", "rejected")
                .insert_header("anthropic-ratelimit-unified-representative-claim", "seven_day")
                .insert_header("anthropic-ratelimit-unified-reset", reset_at.to_string().as_str())
                .insert_header("anthropic-ratelimit-unified-7d-utilization", "1.0")
                .insert_header(
                    "anthropic-ratelimit-unified-7d-reset",
                    reset_at.to_string().as_str(),
                )
                .set_body_json(
                    json!({ "type": "error", "error": { "type": "rate_limit_error", "message": "limit" } }),
                ),
        )
        .mount(&claude.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oauth/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "five_hour": { "utilization": 12.0, "resets_at": "2099-01-01T00:00:00Z" },
            "seven_day": { "utilization": 100.0, "resets_at": "2099-01-03T00:00:00Z" },
            "cedar_ember": null,
            "juniper_tide": { "eligible": true, "arm": "reset", "available": true }
        })))
        .mount(&claude.server)
        .await;
    Mock::given(method("POST"))
        .and(path(RESET_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "result": "reset" })))
        .mount(&claude.server)
        .await;
    let sandbox = Sandbox::new(&claude, "");
    let mut args = PRINT.to_vec();
    args.extend(["--set", "usage.auto_reset=true"]);

    let run = sandbox.azazel(&args).await;
    assert_eq!(run.status.code(), Some(1), "{}", text(&run.stdout));
    assert!(claude.redemptions().await.is_empty());
    assert_eq!(sandbox.journal(), None);
}

/// An `azazel rpc` process and the lines it writes.
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Client {
    fn start(sandbox: &Sandbox, args: &[&str]) -> Client {
        let mut command = sandbox.command(&["--no-memory", "rpc"]);
        let mut child = command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Client {
            stdin: child.stdin.take(),
            child,
            lines,
        }
    }

    fn send(&mut self, command: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{command}").unwrap();
        stdin.flush().unwrap();
    }

    /// Reads up to the first line `wanted` accepts; returns the lines before it and that line.
    fn until(&self, wanted: impl Fn(&Value) -> bool) -> (Vec<Value>, Value) {
        let mut before = Vec::new();
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(60))
                .unwrap_or_else(|_| panic!("no protocol line within a minute, after {before:?}"));
            let line: Value = serde_json::from_str(&line).unwrap();
            if wanted(&line) {
                return (before, line);
            }
            before.push(line);
        }
    }

    fn answer(&mut self, request: &Value, value: Value) {
        self.send(
            json!({ "id": 99, "type": "ui_response", "request_id": request["request_id"], "value": value }),
        );
    }

    fn finish(mut self) -> Option<i32> {
        drop(self.stdin.take());
        self.child.wait().unwrap().code()
    }
}

fn asked(line: &Value) -> bool {
    line["type"] == "ui_request" && line["kind"] == "select"
}

fn idle(line: &Value) -> bool {
    line["type"] == "idle" || line["type"] == "failed"
}

/// What the notices among `lines` say.
fn notices(lines: &[Value]) -> String {
    lines
        .iter()
        .filter(|line| line["type"] == "ui_request" && line["kind"] == "notify")
        .filter_map(|line| line["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text the model's reply added, from the stream events among `lines`.
fn replied(lines: &[Value]) -> bool {
    lines.iter().any(|line| line.to_string().contains("back at work"))
}

#[tokio::test(flavor = "multi_thread")]
async fn with_a_client_the_choice_is_a_question_and_the_reset_lets_the_run_go_on() {
    let claude = Claude::start(3 * 3600).await;
    let sandbox = Sandbox::new(&claude, "");
    let mut client = Client::start(&sandbox, &["--model", "anthropic/claude-test"]);
    client.send(json!({ "id": 1, "type": "prompt", "text": "hi" }));

    let (_, question) = client.until(asked);
    let prompt = question["prompt"].as_str().unwrap();
    let mut lines = prompt.lines();
    assert_eq!(lines.next(), Some("Usage limit reached"));
    let said = lines.next().unwrap();
    assert!(
        said.starts_with("anthropic: 5-hour limit reached until ")
            && said.ends_with(" You have 1 banked reset."),
        "{said}"
    );
    assert_eq!(
        question["options"],
        json!(["Use the reset", "Wait until reset", "Pick another model"])
    );
    let context = prompt;
    // The account is named without its whole address, and the user is told what it costs.
    assert!(
        context.contains("Account default (j***@example.com)."),
        "{context}"
    );
    assert!(
        context.contains("Session reset") && context.contains("cannot be undone"),
        "{context}"
    );
    // Nothing was spent by asking.
    assert!(claude.redemptions().await.is_empty());

    client.answer(&question, json!("Use the reset"));
    let (after, end) = client.until(idle);
    assert_eq!(end["type"], "idle", "{end}");
    assert!(replied(&after), "{after:?}");
    assert!(
        notices(&after).contains("one reset spent (Session reset)"),
        "{after:?}"
    );
    assert_eq!(claude.redemptions().await, [json!({ "program": "juniper_tide" })]);
    // The request that was refused went out once more, and no third time.
    assert_eq!(claude.prompts().await, 2);
    assert_eq!(sandbox.journal().unwrap()["state"], "confirmed");
    // What the usage views knew about the account's resets went with the one that was spent.
    let kept = std::fs::read_to_string(sandbox.home().join("cache").join("limits.json")).unwrap();
    let kept: Value = serde_json::from_str(&kept).unwrap();
    assert_eq!(kept["anthropic"]["windows"][0]["name"], "5h");
    assert_eq!(kept["anthropic"].get("resets"), None, "{kept}");
    assert_eq!(client.finish(), Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_that_is_dismissed_leaves_the_failure_and_the_reset_as_they_were() {
    let claude = Claude::start(3 * 3600).await;
    let sandbox = Sandbox::new(&claude, "");
    let mut client = Client::start(&sandbox, &["--model", "anthropic/claude-test"]);
    client.send(json!({ "id": 1, "type": "prompt", "text": "hi" }));

    let (_, question) = client.until(asked);
    client.answer(&question, Value::Null);
    let (_, end) = client.until(idle);
    assert_eq!(end["type"], "failed", "{end}");
    assert!(
        end["error"].as_str().unwrap().contains("usage limit reached"),
        "{end}"
    );
    assert!(claude.redemptions().await.is_empty());
    assert_eq!(client.finish(), Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn waiting_goes_on_at_the_reset_and_picking_goes_on_with_the_other_model() {
    // The window resets in a moment: the run that waits goes on then, by itself.
    let claude = Claude::start(2).await;
    let sandbox = Sandbox::new(&claude, "");
    let mut client = Client::start(&sandbox, &["--model", "anthropic/claude-test"]);
    client.send(json!({ "id": 1, "type": "prompt", "text": "hi" }));
    let (_, question) = client.until(asked);
    client.answer(&question, json!("Wait until reset"));
    let (after, end) = client.until(idle);
    assert_eq!(end["type"], "idle", "{end}");
    assert!(replied(&after), "{after:?}");
    assert!(notices(&after).contains("waiting until "), "{after:?}");
    assert!(claude.redemptions().await.is_empty());
    assert_eq!(client.finish(), Some(0));

    // Another model: the scripted provider stands in for it.
    let claude = Claude::start(3 * 3600).await;
    let sandbox = Sandbox::new(&claude, "[roles]\nspare = [\"mock/lead\"]\n");
    let script = sandbox.dir.path().join("script.json");
    std::fs::write(
        &script,
        json!({ "lead": [{ "reply": { "text": "the spare one answers" } }] }).to_string(),
    )
    .unwrap();
    let mut command = sandbox.command(&["--no-memory", "rpc", "--model", "anthropic/claude-test"]);
    let mut child = command
        .env("AZAZEL_MOCK_SCRIPT", &script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, lines) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let mut client = Client {
        stdin: child.stdin.take(),
        child,
        lines,
    };
    client.send(json!({ "id": 1, "type": "prompt", "text": "hi" }));
    let (_, question) = client.until(asked);
    client.answer(&question, json!("Pick another model"));
    let (_, models) = client.until(asked);
    assert!(
        models["prompt"]
            .as_str()
            .unwrap()
            .starts_with("Continue on which model?"),
        "{models}"
    );
    let options: Vec<&str> = models["options"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    // The model that ran out is not among them; the role that leads elsewhere is.
    assert!(options.contains(&"@spare\tmock/lead"), "{options:?}");
    assert!(
        !options
            .iter()
            .any(|option| option.contains("anthropic/claude-test")),
        "{options:?}"
    );
    client.answer(&models, json!("@spare\tmock/lead"));
    let (after, end) = client.until(idle);
    assert_eq!(end["type"], "idle", "{end}");
    assert!(
        after
            .iter()
            .any(|line| line.to_string().contains("the spare one answers")),
        "{after:?}"
    );
    assert!(claude.redemptions().await.is_empty());
    // The session is on the model that was picked from here on.
    client.send(json!({ "id": 2, "type": "get_state" }));
    let (_, state) = client.until(|line| line["type"] == "response" && line["id"] == 2);
    assert_eq!(state["data"]["chain"], json!(["@spare"]));
    assert_eq!(client.finish(), Some(0));
}
