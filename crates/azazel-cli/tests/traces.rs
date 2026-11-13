#![allow(clippy::unwrap_used)]

//! Trace export as a run of the binary does it: `[telemetry]` in the user's config sends the
//! span tree of a prompt to a collector, what was said only when asked for, and a collector
//! that fails or hangs costs the run nothing.

use std::path::PathBuf;
use std::process::Output;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROMPT: &str = "summarise note.txt please";
const NOTE: &str = "the launch code is 4471";
const REPLY: &str = "It holds a number.";

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    /// A project with a note in it, and a home whose config has this `[telemetry]`.
    fn new(telemetry: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::write(dir.path().join("project/note.txt"), NOTE).unwrap();
        std::fs::write(
            dir.path().join("home/config.toml"),
            format!("[telemetry]\n{telemetry}\n"),
        )
        .unwrap();
        Sandbox { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// `azazel -p <PROMPT>`: the lead reads the note and answers. `think_ms` is how long the
    /// model takes over its answer.
    fn run(&self, think_ms: u64) -> Output {
        let usage = json!({"input": 100, "output": 20, "cache_read": 900, "cost_usd": 0.01});
        self.follow(&json!({ "lead": [
            { "reply": {
                "text": "I will read it.",
                "tool_calls": [{ "id": "c1", "name": "read", "input": { "path": "note.txt" } }],
                "usage": usage,
            } },
            { "delay_ms": think_ms, "reply": { "text": REPLY, "usage": usage } },
        ] }))
    }

    /// `azazel -p <PROMPT>` with the agents answering from `script`.
    fn follow(&self, script: &Value) -> Output {
        std::fs::write(self.path("script.json"), script.to_string()).unwrap();
        Command::cargo_bin("azazel")
            .unwrap()
            .env("AZAZEL_HOME", self.path("home"))
            .env("AZAZEL_MOCK_SCRIPT", self.path("script.json"))
            .env("TRACE_KEY", "k-123")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
            .env_remove("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
            .env_remove("OTEL_EXPORTER_OTLP_HEADERS")
            .arg("--cwd")
            .arg(self.path("project"))
            .args(["--model", "mock/lead", "--mode", "auto"])
            .args(["--set", "memory.auto_extract=false"])
            .args(["--set", "memory.auto_index=false"])
            .args(["--set", "session.auto_name=false"])
            .args(["-p", PROMPT])
            .write_stdin("")
            .output()
            .unwrap()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.path("home/logs/azazel.log")).unwrap_or_default()
    }
}

/// A collector that answers every post to `route` with `response`.
async fn collector(route: &str, response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

async fn run(sandbox: Sandbox, think_ms: u64) -> (Sandbox, Output) {
    tokio::task::spawn_blocking(move || {
        let output = sandbox.run(think_ms);
        (sandbox, output)
    })
    .await
    .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn attribute<'a>(span: &'a Value, key: &str) -> &'a Value {
    span["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attribute| attribute["key"] == key)
        .map_or(&Value::Null, |attribute| &attribute["value"])
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_reaches_an_otlp_collector_as_one_tree_without_what_was_said() {
    let server = collector("/v1/traces", ResponseTemplate::new(200).set_body_json(json!({}))).await;
    let sandbox = Sandbox::new(&format!(
        "exporter = \"otlp\"\nendpoint = \"{}\"\napi_key_env = \"TRACE_KEY\"",
        server.uri()
    ));
    let (sandbox, output) = run(sandbox, 0).await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(stdout(&output).trim(), REPLY);

    let received = server.received_requests().await.unwrap();
    assert!(!received.is_empty(), "nothing was sent");
    let mut spans = Vec::new();
    for request in &received {
        assert_eq!(request.headers["authorization"], "Bearer k-123");
        assert_eq!(request.headers["content-type"], "application/json");
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let resource = &body["resourceSpans"][0];
        assert_eq!(
            resource["resource"]["attributes"][0]["value"]["stringValue"],
            "azazel"
        );
        spans.extend(resource["scopeSpans"][0]["spans"].as_array().unwrap().clone());
        // Nothing of the prompt, the file or the reply left the machine.
        let text = body.to_string();
        for said in [PROMPT, NOTE, REPLY, "note.txt"] {
            assert!(!text.contains(said), "`{said}` was sent");
        }
    }
    let named = |name: &str| -> Vec<&Value> { spans.iter().filter(|span| span["name"] == name).collect() };
    let (session, prompt) = (named("session"), named("prompt"));
    let (requests, tools) = (named("chat lead"), named("execute_tool read"));
    assert_eq!(spans.len(), 5, "{spans:#?}");
    assert_eq!(
        (session.len(), prompt.len(), requests.len(), tools.len()),
        (1, 1, 2, 1)
    );
    // session -> prompt -> model request -> tool call, in one trace.
    assert!(session[0].get("parentSpanId").is_none());
    assert_eq!(prompt[0]["parentSpanId"], session[0]["spanId"]);
    for request in &requests {
        assert_eq!(request["parentSpanId"], prompt[0]["spanId"]);
    }
    let asked = requests
        .iter()
        .find(|request| attribute(request, "azazel.tool_calls")["intValue"] == "1")
        .unwrap();
    assert_eq!(tools[0]["parentSpanId"], asked["spanId"]);
    assert!(spans.iter().all(|span| span["traceId"] == session[0]["traceId"]));

    // Model, tokens and cost of each request; the prompt adds them up.
    assert_eq!(attribute(asked, "gen_ai.request.model")["stringValue"], "lead");
    assert_eq!(attribute(asked, "gen_ai.provider.name")["stringValue"], "mock");
    assert_eq!(attribute(asked, "gen_ai.usage.input_tokens")["intValue"], "1000");
    assert_eq!(attribute(asked, "gen_ai.usage.output_tokens")["intValue"], "20");
    assert_eq!(attribute(asked, "azazel.cost_usd")["doubleValue"], 0.01);
    assert_eq!(
        attribute(prompt[0], "gen_ai.usage.output_tokens")["intValue"],
        "40"
    );
    assert_eq!(attribute(prompt[0], "azazel.cost_usd")["doubleValue"], 0.02);
    assert_eq!(attribute(prompt[0], "azazel.stop")["stringValue"], "stop");
    assert_eq!(attribute(tools[0], "azazel.tool.is_error")["boolValue"], false);
    // Every span has a duration, and a child begins no earlier than its parent.
    let time = |span: &Value, key: &str| span[key].as_str().unwrap().parse::<u128>().unwrap();
    for span in &spans {
        assert!(time(span, "endTimeUnixNano") >= time(span, "startTimeUnixNano"));
    }
    assert!(time(prompt[0], "startTimeUnixNano") >= time(session[0], "startTimeUnixNano"));
    assert!(time(session[0], "endTimeUnixNano") >= time(prompt[0], "endTimeUnixNano"));
    assert!(!sandbox.log().contains("trace export"), "{}", sandbox.log());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_is_a_tree_under_the_call_that_started_it() {
    let server = collector("/v1/traces", ResponseTemplate::new(200).set_body_json(json!({}))).await;
    let sandbox = Sandbox::new(&format!("exporter = \"otlp\"\nendpoint = \"{}\"", server.uri()));
    let script = json!({
        "lead": [
            { "reply": { "tool_calls": [{ "id": "t1", "name": "task", "input": {
                "agents": [{ "agent": "scout", "prompt": "Read note.txt and say what it holds." }]
            } }] } },
            { "reply": { "text": REPLY } },
        ],
        "scout": [{ "reply": { "text": "note.txt holds a launch code." } }],
    });
    let output = tokio::task::spawn_blocking(move || sandbox.follow(&script))
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    let mut spans = Vec::new();
    for request in &server.received_requests().await.unwrap() {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        spans.extend(
            body["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .unwrap()
                .clone(),
        );
    }
    let agent = |span: &Value| attribute(span, "gen_ai.agent.id")["stringValue"].clone();
    let one = |name: &str, of: &str| -> &Value {
        let found: Vec<&Value> = spans
            .iter()
            .filter(|span| span["name"] == name && agent(span) == of)
            .collect();
        assert_eq!(found.len(), 1, "{name} of {of}: {spans:#?}");
        found[0]
    };
    let call = one("execute_tool task", "lead");
    let delegated = one("prompt", "scout-1");
    let asked = one("chat scout", "scout-1");
    assert_eq!(delegated["parentSpanId"], call["spanId"]);
    assert_eq!(asked["parentSpanId"], delegated["spanId"]);
    assert_eq!(attribute(delegated, "gen_ai.agent.name")["stringValue"], "scout");
    assert_eq!(delegated["traceId"], one("prompt", "lead")["traceId"]);
    // The session counts the work of both agents.
    let session = spans.iter().find(|span| span["name"] == "session").unwrap();
    assert_eq!(attribute(session, "azazel.prompts")["intValue"], "2");
    assert_eq!(attribute(session, "azazel.requests")["intValue"], "3");
}

#[tokio::test(flavor = "multi_thread")]
async fn langsmith_gets_runs_with_what_was_said_when_content_is_on() {
    let server = collector("/runs/batch", ResponseTemplate::new(202).set_body_json(json!({}))).await;
    let sandbox = Sandbox::new(&format!(
        "exporter = \"langsmith\"\nendpoint = \"{}\"\napi_key_env = \"TRACE_KEY\"\nproject = \"ci\"\ncontent = true",
        server.uri()
    ));
    let (_sandbox, output) = run(sandbox, 0).await;
    assert!(output.status.success(), "{output:?}");

    let received = server.received_requests().await.unwrap();
    let mut runs = Vec::new();
    for request in &received {
        assert_eq!(request.headers["x-api-key"], "k-123");
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        runs.extend(body["post"].as_array().unwrap().clone());
        // The end of a session that was posted in an earlier batch.
        for patch in body["patch"].as_array().unwrap() {
            let run = runs.iter_mut().find(|run| run["id"] == patch["id"]).unwrap();
            *run = patch.clone();
        }
    }
    let of_type = |kind: &str| -> Vec<&Value> { runs.iter().filter(|run| run["run_type"] == kind).collect() };
    let (chains, requests, tools) = (of_type("chain"), of_type("llm"), of_type("tool"));
    assert_eq!(
        (chains.len(), requests.len(), tools.len()),
        (2, 2, 1),
        "{runs:#?}"
    );
    let session = chains.iter().find(|run| run["name"] == "session").unwrap();
    let prompt = chains.iter().find(|run| run["name"] == "prompt").unwrap();
    assert!(session.get("parent_run_id").is_none());
    assert!(session["end_time"].is_string());
    assert_eq!(prompt["parent_run_id"], session["id"]);
    for run in &runs {
        assert_eq!(run["trace_id"], session["id"]);
        assert_eq!(run["session_name"], "ci");
        assert!(
            run["dotted_order"]
                .as_str()
                .unwrap()
                .starts_with(session["dotted_order"].as_str().unwrap())
        );
    }
    assert_eq!(prompt["inputs"]["prompt"], PROMPT);
    assert_eq!(prompt["outputs"]["reply"], REPLY);
    let asked = requests
        .iter()
        .find(|run| run["outputs"]["tool_calls"] != json!([]))
        .unwrap();
    assert_eq!(asked["outputs"]["content"], "I will read it.");
    assert_eq!(
        asked["outputs"]["tool_calls"],
        json!([{"id": "c1", "name": "read", "args": {"path": "note.txt"}}])
    );
    assert_eq!(asked["outputs"]["usage_metadata"]["input_tokens"], 1000);
    assert_eq!(asked["outputs"]["usage_metadata"]["total_cost"], 0.01);
    assert_eq!(asked["extra"]["metadata"]["ls_model_name"], "lead");
    assert_eq!(tools[0]["name"], "read");
    assert_eq!(tools[0]["parent_run_id"], asked["id"]);
    assert_eq!(tools[0]["inputs"], json!({"path": "note.txt"}));
    assert!(
        tools[0]["outputs"]["output"].as_str().unwrap().contains(NOTE),
        "{}",
        tools[0]["outputs"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_collector_that_refuses_is_logged_once_and_the_run_goes_on() {
    let server = collector(
        "/v1/traces",
        ResponseTemplate::new(500).set_body_string("collector is down"),
    )
    .await;
    let sandbox = Sandbox::new(&format!("exporter = \"otlp\"\nendpoint = \"{}\"", server.uri()));
    // Long enough for a batch to go out while the model thinks, before the one at the end.
    let (sandbox, output) = run(sandbox, 3000).await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(stdout(&output).trim(), REPLY);
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sent = server.received_requests().await.unwrap().len();
    assert!(sent >= 2, "{sent} batches were sent");
    let log = sandbox.log();
    let failures: Vec<&str> = log.lines().filter(|line| line.contains("trace export")).collect();
    assert_eq!(failures.len(), 1, "{log}");
    assert!(
        failures[0].contains("500") && failures[0].contains("collector is down"),
        "{log}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_collector_that_hangs_holds_nothing_up() {
    let slow = ResponseTemplate::new(200).set_delay(Duration::from_secs(60));
    let server = collector("/v1/traces", slow).await;
    let sandbox = Sandbox::new(&format!("exporter = \"otlp\"\nendpoint = \"{}\"", server.uri()));
    let started = Instant::now();
    let (_sandbox, output) = run(sandbox, 0).await;
    let took = started.elapsed();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(stdout(&output).trim(), REPLY);
    // The turn waited for nothing; the end of the run waits a few seconds for the last batch
    // and then leaves it.
    assert!(took < Duration::from_secs(30), "the run took {took:?}");
}
