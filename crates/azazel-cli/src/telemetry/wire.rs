//! Spans as the two backends take them: OTLP over HTTP as JSON, and the runs API of LangSmith.

use azazel_core::config::{TelemetryConfig, TelemetryExporter};
use jiff::Timestamp;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::record::{Content, Kind, Span};

const OTLP_DEFAULT: &str = "http://localhost:4318";
const LANGSMITH_DEFAULT: &str = "https://api.smith.langchain.com";
const SERVICE: &str = "azazel";

/// Where spans go and in what shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wire {
    Otlp {
        url: String,
        headers: Vec<(String, String)>,
        /// `service.name` of everything sent.
        service: String,
    },
    Langsmith {
        url: String,
        key: Option<String>,
        project: String,
    },
}

/// `k1=v1,k2=v2`, as `OTEL_EXPORTER_OTLP_HEADERS` is written.
fn header_list(list: &str) -> Vec<(String, String)> {
    list.split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

/// The address OTLP traces are posted to, given a collector's address.
fn traces_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/v1/traces") {
        base.to_string()
    } else {
        format!("{base}/v1/traces")
    }
}

impl Wire {
    /// What `[telemetry]` asks for; `None` when it is off. `env` reads the environment: the key
    /// `api_key_env` names, and where the config says nothing, the variables each backend's
    /// own tools read.
    pub fn from_config(config: &TelemetryConfig, env: &dyn Fn(&str) -> Option<String>) -> Option<Wire> {
        let set = |name: &str| env(name).filter(|value| !value.trim().is_empty());
        let named_key = Some(config.api_key_env.as_str())
            .filter(|name| !name.is_empty())
            .and_then(set);
        let configured = Some(config.endpoint.trim()).filter(|endpoint| !endpoint.is_empty());
        let project = Some(config.project.trim()).filter(|project| !project.is_empty());
        match config.exporter {
            TelemetryExporter::Off => None,
            TelemetryExporter::Otlp => {
                let url = match configured {
                    Some(endpoint) => traces_url(endpoint),
                    None => match set("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT") {
                        Some(whole) => whole,
                        None => traces_url(
                            &set("OTEL_EXPORTER_OTLP_ENDPOINT").unwrap_or_else(|| OTLP_DEFAULT.to_string()),
                        ),
                    },
                };
                let mut headers = set("OTEL_EXPORTER_OTLP_HEADERS")
                    .map(|list| header_list(&list))
                    .unwrap_or_default();
                if let Some(key) = named_key {
                    // A value that names its own scheme (`Basic …`) is sent as it is.
                    let value = if key.contains(' ') {
                        key
                    } else {
                        format!("Bearer {key}")
                    };
                    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("authorization"));
                    headers.push(("Authorization".to_string(), value));
                }
                Some(Wire::Otlp {
                    url,
                    headers,
                    service: project.unwrap_or(SERVICE).to_string(),
                })
            }
            TelemetryExporter::Langsmith => {
                let base = configured
                    .map(str::to_string)
                    .or_else(|| set("LANGSMITH_ENDPOINT"))
                    .unwrap_or_else(|| LANGSMITH_DEFAULT.to_string());
                let key = if config.api_key_env.is_empty() {
                    set("LANGSMITH_API_KEY")
                } else {
                    named_key
                };
                Some(Wire::Langsmith {
                    url: format!("{}/runs/batch", base.trim_end_matches('/')),
                    key,
                    project: project
                        .map(str::to_string)
                        .or_else(|| set("LANGSMITH_PROJECT"))
                        .unwrap_or_else(|| SERVICE.to_string()),
                })
            }
        }
    }

    pub fn url(&self) -> &str {
        match self {
            Wire::Otlp { url, .. } | Wire::Langsmith { url, .. } => url,
        }
    }

    pub fn headers(&self) -> Vec<(String, String)> {
        match self {
            Wire::Otlp { headers, .. } => headers.clone(),
            Wire::Langsmith { key, .. } => key
                .iter()
                .map(|key| ("x-api-key".to_string(), key.clone()))
                .collect(),
        }
    }

    /// `otlp to http://localhost:4318/v1/traces`.
    pub fn describe(&self) -> String {
        match self {
            Wire::Otlp { url, .. } => format!("otlp to {url}"),
            Wire::Langsmith { url, project, .. } => format!("langsmith to {url}, project {project}"),
        }
    }

    /// The request body that carries `spans`; `None` when none of them is for this backend to
    /// have yet.
    pub fn body(&self, spans: &[Span]) -> Option<Value> {
        match self {
            Wire::Otlp { service, .. } => otlp(spans, service),
            Wire::Langsmith { project, .. } => langsmith(spans, project),
        }
    }
}

// ------------------------------------------------------------ OTLP

fn any_value(value: &Value) -> Value {
    match value {
        Value::String(text) => json!({ "stringValue": text }),
        Value::Bool(flag) => json!({ "boolValue": flag }),
        // A 64-bit integer is a string in the JSON form of protobuf.
        Value::Number(number) if number.is_i64() || number.is_u64() => {
            json!({ "intValue": number.to_string() })
        }
        Value::Number(number) => json!({ "doubleValue": number }),
        Value::Array(items) => {
            json!({ "arrayValue": { "values": items.iter().map(any_value).collect::<Vec<_>>() } })
        }
        other => json!({ "stringValue": other.to_string() }),
    }
}

fn key_value(key: &str, value: &Value) -> Value {
    json!({ "key": key, "value": any_value(value) })
}

fn nanos(at: Timestamp) -> String {
    at.as_nanosecond().to_string()
}

/// The eight bytes of a span's id that are not its time.
fn span_id(id: Uuid) -> String {
    id.as_bytes()[8..]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn text_part(text: &str) -> Value {
    json!({ "type": "text", "content": text })
}

/// What was said, as the attributes the GenAI conventions of OpenTelemetry name.
fn otlp_content(content: &Content, attributes: &mut Vec<Value>) {
    let mut add = |key: &str, value: Value| {
        attributes.push(key_value(key, &Value::String(value.to_string())));
    };
    match content {
        Content::Prompt { prompt, reply } => {
            if let Some(prompt) = prompt {
                add(
                    "gen_ai.input.messages",
                    json!([{ "role": "user", "parts": [text_part(prompt)] }]),
                );
            }
            if let Some(reply) = reply {
                add(
                    "gen_ai.output.messages",
                    json!([{ "role": "assistant", "parts": [text_part(reply)] }]),
                );
            }
        }
        Content::Reply { text, calls } => {
            let mut parts = Vec::new();
            if !text.is_empty() {
                parts.push(text_part(text));
            }
            parts.extend(calls.iter().map(|call| {
                json!({ "type": "tool_call", "id": call.id, "name": call.name, "arguments": call.input })
            }));
            add(
                "gen_ai.output.messages",
                json!([{ "role": "assistant", "parts": parts }]),
            );
        }
        Content::Tool { input, output } => {
            add("gen_ai.tool.call.arguments", input.clone());
            attributes.push(key_value("gen_ai.tool.call.result", &json!(output)));
        }
    }
}

fn otlp(spans: &[Span], service: &str) -> Option<Value> {
    let spans: Vec<Value> = spans
        .iter()
        .filter_map(|span| {
            // A span has no place in OTLP before it has ended.
            let end = span.end?;
            let mut attributes: Vec<Value> = span
                .attrs
                .iter()
                .map(|(key, value)| key_value(key, value))
                .collect();
            if let Some(content) = &span.content {
                otlp_content(content, &mut attributes);
            }
            let mut out = json!({
                "traceId": span.trace.simple().to_string(),
                "spanId": span_id(span.id),
                "name": span.name,
                // A request leaves the process; everything else happens in it.
                "kind": if span.kind == Kind::Request { 3 } else { 1 },
                "startTimeUnixNano": nanos(span.start),
                "endTimeUnixNano": nanos(end),
                "attributes": attributes,
                "status": match &span.error {
                    Some(error) => json!({ "code": 2, "message": error }),
                    None => json!({ "code": 1 }),
                },
            });
            if let Some(parent) = span.parent {
                out["parentSpanId"] = json!(span_id(parent));
            }
            Some(out)
        })
        .collect();
    if spans.is_empty() {
        return None;
    }
    Some(json!({
        "resourceSpans": [{
            "resource": { "attributes": [
                key_value("service.name", &json!(service)),
                key_value("service.version", &json!(env!("CARGO_PKG_VERSION"))),
                key_value("os.type", &json!(std::env::consts::OS)),
            ] },
            "scopeSpans": [{
                "scope": { "name": SERVICE, "version": env!("CARGO_PKG_VERSION") },
                "spans": spans,
            }],
        }],
    }))
}

// ------------------------------------------------------------ LangSmith

/// A span as a run. Tokens and cost of a request go where LangSmith reads them.
fn run(span: &Span, project: &str) -> Value {
    let attr = |key: &str| span.attrs.get(key);
    let mut metadata = span.attrs.clone();
    // What LangSmith groups the runs of one conversation by.
    if let Some(session) = attr("session.id") {
        metadata.insert("thread_id".into(), session.clone());
    }
    let mut inputs = Map::new();
    let mut outputs = Map::new();
    let (run_type, name) = match span.kind {
        Kind::Session | Kind::Prompt => ("chain", span.name.clone()),
        Kind::Request => {
            if let Some(model) = attr("gen_ai.request.model") {
                metadata.insert("ls_model_name".into(), model.clone());
                metadata.insert("ls_model_type".into(), json!("chat"));
            }
            if let Some(provider) = attr("gen_ai.provider.name") {
                metadata.insert("ls_provider".into(), provider.clone());
            }
            let tokens = |key: &str| attr(key).and_then(Value::as_u64).unwrap_or(0);
            if attr("gen_ai.usage.input_tokens").is_some() {
                let (input, output) = (
                    tokens("gen_ai.usage.input_tokens"),
                    tokens("gen_ai.usage.output_tokens"),
                );
                let mut usage = json!({
                    "input_tokens": input,
                    "output_tokens": output,
                    "total_tokens": input + output,
                    "input_token_details": {
                        "cache_read": tokens("gen_ai.usage.cache_read.input_tokens"),
                        "cache_creation": tokens("gen_ai.usage.cache_creation.input_tokens"),
                    },
                });
                if let Some(cost) = attr("azazel.cost_usd") {
                    usage["total_cost"] = cost.clone();
                }
                outputs.insert("usage_metadata".into(), usage);
            }
            ("llm", span.name.clone())
        }
        Kind::Tool => {
            let name = attr("gen_ai.tool.name").and_then(Value::as_str);
            ("tool", name.unwrap_or(&span.name).to_string())
        }
    };
    match &span.content {
        Some(Content::Prompt { prompt, reply }) => {
            if let Some(prompt) = prompt {
                inputs.insert("prompt".into(), json!(prompt));
            }
            if let Some(reply) = reply {
                outputs.insert("reply".into(), json!(reply));
            }
        }
        Some(Content::Reply { text, calls }) => {
            outputs.insert("role".into(), json!("assistant"));
            outputs.insert("content".into(), json!(text));
            let calls: Vec<Value> = calls
                .iter()
                .map(|call| json!({ "id": call.id, "name": call.name, "args": call.input }))
                .collect();
            outputs.insert("tool_calls".into(), json!(calls));
        }
        Some(Content::Tool { input, output }) => {
            match input {
                Value::Object(members) => inputs = members.clone(),
                other => drop(inputs.insert("input".into(), other.clone())),
            }
            outputs.insert("output".into(), json!(output));
        }
        None => {}
    }
    let mut run = json!({
        "id": span.id.to_string(),
        "trace_id": span.trace.to_string(),
        "dotted_order": span.order,
        "name": name,
        "run_type": run_type,
        "start_time": span.start.to_string(),
        "inputs": inputs,
        "session_name": project,
        "extra": { "metadata": metadata },
    });
    if let Some(parent) = span.parent {
        run["parent_run_id"] = json!(parent.to_string());
    }
    if let Some(end) = span.end {
        run["end_time"] = json!(end.to_string());
        run["outputs"] = Value::Object(outputs);
    }
    if let Some(error) = &span.error {
        run["error"] = json!(error);
    }
    run
}

fn langsmith(spans: &[Span], project: &str) -> Option<Value> {
    let (mut post, mut patch) = (Vec::new(), Vec::new());
    for span in spans {
        let opened_here = |id: Uuid| spans.iter().any(|other| other.id == id && other.end.is_none());
        let closed_here = |id: Uuid| spans.iter().any(|other| other.id == id && other.end.is_some());
        match span.end {
            // A run that opens and closes in one batch is sent once, whole.
            None if closed_here(span.id) => {}
            // It was posted when it opened: its end is a change to it.
            Some(_) if span.kind == Kind::Session && !opened_here(span.id) => patch.push(run(span, project)),
            _ => post.push(run(span, project)),
        }
    }
    if post.is_empty() && patch.is_empty() {
        return None;
    }
    Some(json!({ "post": post, "patch": patch }))
}

#[cfg(test)]
mod tests {
    use azazel_agent::{AgentEvent, ToolOutput};
    use azazel_ai::{AssistantMeta, Block, Message, StopReason, Usage, WireApi};

    use super::super::record::{Recorder, SessionFacts, Who};
    use super::*;

    fn at(ms: i64) -> Timestamp {
        Timestamp::from_millisecond(1_800_000_000_000 + ms).unwrap()
    }

    /// A session with one prompt: a reply that asks for `read`, the call, the closing.
    /// The first span is the session as it opened.
    fn spans(content: bool) -> Vec<Span> {
        let facts = SessionFacts {
            id: "s1".into(),
            name: None,
            resumed: false,
        };
        let (mut recorder, opened) = Recorder::new(facts, content, at(0));
        let lead = Who {
            id: "lead",
            def: "lead",
            parent: None,
        };
        let meta = AssistantMeta {
            provider: "anthropic".into(),
            model: "claude-opus".into(),
            api: WireApi::AnthropicMessages,
            usage: Usage {
                input: 10,
                output: 5,
                cache_read: 90,
                cache_write: 0,
                reasoning: 0,
                cost_usd: Some(0.25),
            },
            stop: StopReason::ToolUse,
            response_id: None,
            duration_ms: 0,
            sources: Vec::new(),
        };
        let message = Message::assistant(
            vec![
                Block::text("reading"),
                Block::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    input: json!({"path": "a.rs"}),
                    signature: None,
                },
            ],
            meta,
        );
        let events = vec![
            (1, AgentEvent::AgentStart),
            (2, AgentEvent::TurnStart { turn: 1 }),
            (3, AgentEvent::MessageStart),
            (10, AgentEvent::MessageEnd { message }),
            (
                11,
                AgentEvent::ToolStart {
                    call_id: "c1".into(),
                    name: "read".into(),
                    input: json!({"path": "a.rs"}),
                },
            ),
            (
                20,
                AgentEvent::ToolEnd {
                    call_id: "c1".into(),
                    output: ToolOutput::error("no such file"),
                    duration_ms: 9,
                },
            ),
            (
                30,
                AgentEvent::Idle {
                    stop: StopReason::Stop,
                },
            ),
        ];
        let mut out = vec![opened];
        for (ms, event) in events {
            if matches!(event, AgentEvent::MessageStart) && recorder.needs_prompt("lead") {
                recorder.prompt_text("lead", "what is in a.rs?");
            }
            out.extend(recorder.event(&lead, &event, at(ms)));
        }
        out.extend(recorder.close(at(40)));
        out
    }

    fn config(toml: &str) -> TelemetryConfig {
        toml::from_str(toml).unwrap()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn attribute<'a>(span: &'a Value, key: &str) -> &'a Value {
        span["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|attribute| attribute["key"] == key)
            .map_or(&Value::Null, |attribute| &attribute["value"])
    }

    #[test]
    fn the_config_and_the_environment_decide_where_spans_go() {
        assert_eq!(Wire::from_config(&config(""), &no_env), None);
        // The collector on this machine, unless something names another.
        let wire = Wire::from_config(&config("exporter = 'otlp'"), &no_env).unwrap();
        assert_eq!(wire.url(), "http://localhost:4318/v1/traces");
        assert!(wire.headers().is_empty());
        assert_eq!(wire.describe(), "otlp to http://localhost:4318/v1/traces");

        let env = |name: &str| match name {
            "OTEL_EXPORTER_OTLP_ENDPOINT" => Some("http://collector:4318/".to_string()),
            "OTEL_EXPORTER_OTLP_HEADERS" => Some("x-team=abc, authorization=Basic old".to_string()),
            "TRACE_KEY" => Some("k123".to_string()),
            "LANGSMITH_API_KEY" => Some("ls-key".to_string()),
            "LANGSMITH_PROJECT" => Some("from-env".to_string()),
            _ => None,
        };
        let wire = Wire::from_config(&config("exporter = 'otlp'"), &env).unwrap();
        assert_eq!(wire.url(), "http://collector:4318/v1/traces");
        assert_eq!(
            wire.headers(),
            [
                ("x-team".to_string(), "abc".to_string()),
                ("authorization".to_string(), "Basic old".to_string())
            ]
        );
        // The config wins over the environment, and the named key over a header of the list.
        let wire = Wire::from_config(
            &config(
                "exporter = 'otlp'\nendpoint = 'https://otel.example/v1/traces'\napi_key_env = 'TRACE_KEY'\n\
                 project = 'ci'",
            ),
            &env,
        )
        .unwrap();
        assert_eq!(
            wire,
            Wire::Otlp {
                url: "https://otel.example/v1/traces".into(),
                headers: vec![
                    ("x-team".into(), "abc".into()),
                    ("Authorization".into(), "Bearer k123".into())
                ],
                service: "ci".into(),
            }
        );

        let wire = Wire::from_config(&config("exporter = 'langsmith'"), &env).unwrap();
        assert_eq!(wire.url(), "https://api.smith.langchain.com/runs/batch");
        assert_eq!(wire.headers(), [("x-api-key".to_string(), "ls-key".to_string())]);
        assert_eq!(
            wire.describe(),
            "langsmith to https://api.smith.langchain.com/runs/batch, project from-env"
        );
        // A key the config names and the environment lacks is no key: not another one.
        let wire = Wire::from_config(
            &config("exporter = 'langsmith'\napi_key_env = 'MISSING'\nproject = 'mine'"),
            &env,
        )
        .unwrap();
        assert!(wire.headers().is_empty());
        assert!(wire.describe().ends_with("project mine"));
    }

    #[test]
    fn otlp_carries_the_tree_with_ids_times_and_typed_attributes() {
        let wire = Wire::from_config(&config("exporter = 'otlp'"), &no_env).unwrap();
        let all = spans(false);
        // The session as it opened is not for OTLP: alone it makes no request.
        assert_eq!(wire.body(&all[..1]), None);
        let body = wire.body(&all).unwrap();
        let resource = &body["resourceSpans"][0];
        assert_eq!(
            resource["resource"]["attributes"][0],
            json!({"key": "service.name", "value": {"stringValue": "azazel"}})
        );
        let sent = resource["scopeSpans"][0]["spans"].as_array().unwrap();
        let names: Vec<&str> = sent.iter().map(|span| span["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            ["chat claude-opus", "execute_tool read", "prompt", "session"]
        );
        let [request, tool, prompt, session] = &sent[..] else {
            unreachable!()
        };
        for span in sent {
            assert_eq!(span["traceId"], session["traceId"]);
            assert_eq!(span["traceId"].as_str().unwrap().len(), 32);
            assert_eq!(span["spanId"].as_str().unwrap().len(), 16);
        }
        assert!(session.get("parentSpanId").is_none());
        assert_eq!(prompt["parentSpanId"], session["spanId"]);
        assert_eq!(request["parentSpanId"], prompt["spanId"]);
        assert_eq!(tool["parentSpanId"], request["spanId"]);
        assert_eq!(request["startTimeUnixNano"], "1800000000002000000");
        assert_eq!(request["endTimeUnixNano"], "1800000000010000000");
        assert_eq!(
            (request["kind"].as_i64(), tool["kind"].as_i64()),
            (Some(3), Some(1))
        );
        assert_eq!(
            attribute(request, "gen_ai.usage.input_tokens"),
            &json!({"intValue": "100"})
        );
        assert_eq!(
            attribute(request, "azazel.cost_usd"),
            &json!({"doubleValue": 0.25})
        );
        assert_eq!(
            attribute(request, "gen_ai.response.finish_reasons"),
            &json!({"arrayValue": {"values": [{"stringValue": "tool_use"}]}})
        );
        assert_eq!(
            attribute(tool, "azazel.tool.is_error"),
            &json!({"boolValue": true})
        );
        assert_eq!(
            tool["status"],
            json!({"code": 2, "message": "the tool reported an error"})
        );
        assert_eq!(request["status"], json!({"code": 1}));
        // Nothing that was said.
        let text = body.to_string();
        assert!(!text.contains("a.rs") && !text.contains("reading") && !text.contains("no such file"));
    }

    #[test]
    fn otlp_carries_what_was_said_when_content_is_on() {
        let wire = Wire::from_config(&config("exporter = 'otlp'"), &no_env).unwrap();
        let body = wire.body(&spans(true)).unwrap();
        let sent = body["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        let text = |span: &Value, key: &str| -> Value {
            serde_json::from_str(attribute(span, key)["stringValue"].as_str().unwrap()).unwrap()
        };
        assert_eq!(
            text(&sent[0], "gen_ai.output.messages"),
            json!([{"role": "assistant", "parts": [
                {"type": "text", "content": "reading"},
                {"type": "tool_call", "id": "c1", "name": "read", "arguments": {"path": "a.rs"}},
            ]}])
        );
        assert_eq!(
            text(&sent[1], "gen_ai.tool.call.arguments"),
            json!({"path": "a.rs"})
        );
        assert_eq!(
            attribute(&sent[1], "gen_ai.tool.call.result"),
            &json!({"stringValue": "no such file"})
        );
        assert_eq!(
            text(&sent[2], "gen_ai.input.messages"),
            json!([{"role": "user", "parts": [{"type": "text", "content": "what is in a.rs?"}]}])
        );
    }

    #[test]
    fn langsmith_posts_runs_and_patches_a_session_it_posted_earlier() {
        let wire = Wire::from_config(&config("exporter = 'langsmith'\nproject = 'p'"), &no_env).unwrap();
        let all = spans(true);
        // The session opens: one run without an end.
        let opening = wire.body(&all[..1]).unwrap();
        assert_eq!(opening["patch"], json!([]));
        let opened = &opening["post"][0];
        assert_eq!(opened["run_type"], "chain");
        assert_eq!(opened["session_name"], "p");
        assert_eq!(opened["start_time"], "2027-01-15T08:00:00Z");
        assert!(opened.get("end_time").is_none() && opened.get("outputs").is_none());
        assert_eq!(opened["trace_id"], opened["id"]);
        assert_eq!(opened["extra"]["metadata"]["thread_id"], "s1");

        // The rest arrives later: runs are posted, the session's end is a patch.
        let later = wire.body(&all[1..]).unwrap();
        let posted = later["post"].as_array().unwrap();
        let types: Vec<&str> = posted
            .iter()
            .map(|run| run["run_type"].as_str().unwrap())
            .collect();
        assert_eq!(types, ["llm", "tool", "chain"]);
        let [request, tool, prompt] = &posted[..] else {
            unreachable!()
        };
        assert_eq!(later["patch"].as_array().unwrap().len(), 1);
        let closed = &later["patch"][0];
        assert_eq!(closed["id"], opened["id"]);
        assert_eq!(closed["end_time"], "2027-01-15T08:00:00.04Z");

        assert_eq!(prompt["parent_run_id"], opened["id"]);
        assert_eq!(request["parent_run_id"], prompt["id"]);
        assert_eq!(tool["parent_run_id"], request["id"]);
        let order = |run: &Value| run["dotted_order"].as_str().unwrap().to_string();
        assert_eq!(
            order(request),
            format!(
                "{}.20270115T080000001000Z{}.20270115T080000002000Z{}",
                order(opened),
                prompt["id"].as_str().unwrap(),
                request["id"].as_str().unwrap()
            )
        );
        assert_eq!(request["name"], "chat claude-opus");
        assert_eq!(
            request["outputs"]["usage_metadata"],
            json!({
                "input_tokens": 100, "output_tokens": 5, "total_tokens": 105,
                "input_token_details": {"cache_read": 90, "cache_creation": 0},
                "total_cost": 0.25,
            })
        );
        assert_eq!(request["extra"]["metadata"]["ls_model_name"], "claude-opus");
        assert_eq!(request["extra"]["metadata"]["ls_provider"], "anthropic");
        assert_eq!(request["outputs"]["content"], "reading");
        assert_eq!(
            request["outputs"]["tool_calls"][0]["args"],
            json!({"path": "a.rs"})
        );
        assert_eq!(tool["name"], "read");
        assert_eq!(tool["inputs"], json!({"path": "a.rs"}));
        assert_eq!(tool["outputs"], json!({"output": "no such file"}));
        assert_eq!(tool["error"], "no such file");
        assert_eq!(prompt["inputs"], json!({"prompt": "what is in a.rs?"}));

        // A session that opens and closes within one batch is one whole run, not two halves.
        let whole = wire.body(&all).unwrap();
        assert_eq!(whole["patch"], json!([]));
        let sessions: Vec<&Value> = whole["post"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|run| run["name"] == "session")
            .collect();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["end_time"], "2027-01-15T08:00:00.04Z");

        // Without content a run says what happened and nothing of what was said.
        let quiet = wire.body(&spans(false)).unwrap().to_string();
        assert!(!quiet.contains("a.rs") && !quiet.contains("reading") && !quiet.contains("no such file"));
    }
}
