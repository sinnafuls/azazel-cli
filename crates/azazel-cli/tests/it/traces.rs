//! Trace export as a run of the binary does it: `[telemetry]` in the user's config sends each
//! prompt to a collector as a trace of its own that names its session, what was said only when
//! asked for, and a collector that fails or hangs costs the run nothing.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use crate::common::stdout;
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

    /// The id of the session the run left behind.
    fn session_id(&self) -> String {
        let paths =
            azazel_core::Paths::discover(&self.path("project"), Some(&self.path("home").to_string_lossy()));
        azazel_core::SessionStore::list(&paths.sessions_dir())
            .remove(0)
            .id
    }
}

/// Every span the collector has been sent so far.
async fn spans_of(server: &MockServer) -> Vec<Value> {
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
    spans
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

fn attribute<'a>(span: &'a Value, key: &str) -> &'a Value {
    span["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attribute| attribute["key"] == key)
        .map_or(&Value::Null, |attribute| &attribute["value"])
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_reaches_an_otlp_collector_as_one_trace_without_what_was_said() {
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
    let prompt = named("prompt");
    let (requests, tools) = (named("chat lead"), named("execute_tool read"));
    assert_eq!(spans.len(), 4, "{spans:#?}");
    assert_eq!((prompt.len(), requests.len(), tools.len()), (1, 2, 1));
    // prompt -> model request -> tool call, the prompt being the root of the trace.
    assert!(prompt[0].get("parentSpanId").is_none());
    for request in &requests {
        assert_eq!(request["parentSpanId"], prompt[0]["spanId"]);
    }
    let asked = requests
        .iter()
        .find(|request| attribute(request, "azazel.tool_calls")["intValue"] == "1")
        .unwrap();
    assert_eq!(tools[0]["parentSpanId"], asked["spanId"]);
    // The session is no span: every span says which one it belongs to.
    let session = sandbox.session_id();
    for span in &spans {
        assert_eq!(span["traceId"], prompt[0]["traceId"]);
        assert_eq!(attribute(span, "session.id")["stringValue"], session.as_str());
        assert_eq!(
            attribute(span, "gen_ai.conversation.id")["stringValue"],
            session.as_str()
        );
    }

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
    assert!(time(asked, "startTimeUnixNano") >= time(prompt[0], "startTimeUnixNano"));
    assert!(time(prompt[0], "endTimeUnixNano") >= time(tools[0], "endTimeUnixNano"));
    assert!(!sandbox.log().contains("trace export"), "{}", sandbox.log());
}

/// An `azazel rpc` process in a sandbox, answering from a script.
struct Rpc {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Rpc {
    fn start(sandbox: &Sandbox, script: &Value) -> Rpc {
        std::fs::write(sandbox.path("script.json"), script.to_string()).unwrap();
        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("azazel"))
            .env("AZAZEL_HOME", sandbox.path("home"))
            .env("AZAZEL_MOCK_SCRIPT", sandbox.path("script.json"))
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
            .env_remove("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
            .env_remove("OTEL_EXPORTER_OTLP_HEADERS")
            .args(["rpc", "--cwd"])
            .arg(sandbox.path("project"))
            .args(["--set", "memory.auto_extract=false"])
            .args(["--set", "memory.auto_index=false"])
            .args(["--set", "session.auto_name=false"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Rpc {
            stdin: child.stdin.take(),
            child,
            lines,
        }
    }

    /// Sends a prompt and reads on until the lead is idle again.
    fn prompt(&mut self, text: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", json!({ "id": 1, "type": "prompt", "text": text })).unwrap();
        stdin.flush().unwrap();
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(60))
                .expect("a protocol line within a minute");
            let line: Value = serde_json::from_str(&line).unwrap();
            assert_ne!(line["type"], "failed", "{line}");
            if line["type"] == "idle" {
                return;
            }
        }
    }

    /// Closes standard input and waits for the process to end.
    fn finish(mut self) {
        drop(self.stdin.take());
        assert!(self.child.wait().unwrap().success());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn each_prompt_of_a_session_is_a_trace_that_arrives_when_it_ends() {
    let server = collector("/v1/traces", ResponseTemplate::new(200).set_body_json(json!({}))).await;
    let sandbox = Sandbox::new(&format!("exporter = \"otlp\"\nendpoint = \"{}\"", server.uri()));
    let script = json!({ "lead": [{ "reply": { "text": "One." } }, { "reply": { "text": "Two." } }] });
    let mut rpc = Rpc::start(&sandbox, &script);
    tokio::task::block_in_place(|| rpc.prompt("first"));

    // The session is still open, and its first prompt is already at the collector, whole.
    let started = Instant::now();
    let first = loop {
        let spans = spans_of(&server).await;
        if spans.iter().any(|span| span["name"] == "prompt") {
            break spans;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the prompt did not arrive while the session was open"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(rpc.child.try_wait().unwrap().is_none(), "the session ended");
    let names: Vec<&str> = first.iter().map(|span| span["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["chat lead", "prompt"]);
    assert!(first[1].get("parentSpanId").is_none());
    assert_eq!(first[0]["parentSpanId"], first[1]["spanId"]);

    tokio::task::block_in_place(|| rpc.prompt("second"));
    tokio::task::block_in_place(|| rpc.finish());
    let spans = spans_of(&server).await;
    let prompts: Vec<&Value> = spans.iter().filter(|span| span["name"] == "prompt").collect();
    assert_eq!((spans.len(), prompts.len()), (4, 2), "{spans:#?}");
    // Two traces, each rooted at its prompt, both of the one session.
    assert_ne!(prompts[0]["traceId"], prompts[1]["traceId"]);
    assert!(prompts[1].get("parentSpanId").is_none());
    let session = sandbox.session_id();
    for span in &spans {
        assert_eq!(attribute(span, "session.id")["stringValue"], session.as_str());
    }
    for prompt in &prompts {
        let under = spans
            .iter()
            .filter(|span| span["parentSpanId"] == prompt["spanId"])
            .count();
        assert_eq!(under, 1);
    }
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
