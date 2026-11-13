#![cfg(feature = "mcp")]
#![allow(clippy::unwrap_used)]

//! `azazel mcp serve`, driven by an MCP client.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, ElicitRequestParams,
    ElicitResult, ElicitationAction, ElicitationCapability, FormElicitationCapability, Implementation,
};
use rmcp::service::{RequestContext, RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientHandler, ErrorData, ServiceExt};
use serde_json::{Value, json};

/// An MCP client that may be able to ask its user, and then always answers the same.
#[derive(Clone)]
struct Client {
    /// What the user answers; `None` when the client cannot ask.
    answer: Option<bool>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl ClientHandler for Client {
    fn get_info(&self) -> ClientConfig {
        let mut capabilities = ClientCapabilities::default();
        if self.answer.is_some() {
            capabilities.elicitation =
                Some(ElicitationCapability::new().with_form(FormElicitationCapability::new()));
        }
        ClientConfig::new(capabilities, Implementation::new("test-client", "1.0.0"))
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        let ElicitRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } = request
        else {
            return Ok(ElicitResult::new(ElicitationAction::Decline));
        };
        let schema = serde_json::to_value(&requested_schema).unwrap();
        assert_eq!(schema["properties"]["allow"]["type"], "boolean", "{schema}");
        self.asked.lock().unwrap().push(message);
        Ok(ElicitResult::new(ElicitationAction::Accept)
            .with_content(json!({ "allow": self.answer.unwrap_or(false) })))
    }
}

struct Sandbox {
    dir: tempfile::TempDir,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        Sandbox {
            dir,
            asked: Arc::default(),
        }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    /// Starts `azazel mcp serve` answering from `script` and connects a client to it.
    async fn serve(&self, script: &Value, answer: Option<bool>) -> RunningService<RoleClient, Client> {
        let script_path = self.dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let mut command = tokio::process::Command::new(assert_cmd::cargo::cargo_bin("azazel"));
        command
            .env("AZAZEL_HOME", self.dir.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", &script_path)
            .env("AZAZEL_MOCK_REQUESTS", self.dir.path().join("requests.jsonl"))
            .args(["--set", "session.auto_name=false", "mcp", "serve", "--cwd"])
            .arg(self.project());
        let client = Client {
            answer,
            asked: self.asked.clone(),
        };
        client
            .serve(TokioChildProcess::new(command).unwrap())
            .await
            .unwrap()
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

async fn call(service: &RunningService<RoleClient, Client>, tool: &str, arguments: Value) -> CallToolResult {
    let params =
        CallToolRequestParams::new(tool.to_string()).with_arguments(arguments.as_object().unwrap().clone());
    service.call_tool(params).await.unwrap()
}

fn texts(result: &CallToolResult) -> Vec<String> {
    let content = serde_json::to_value(&result.content).unwrap();
    content
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["text"].as_str().map(str::to_string))
        .collect()
}

fn session_files(home: &Path) -> usize {
    let mut count = 0;
    for project in std::fs::read_dir(home.join("projects"))
        .into_iter()
        .flatten()
        .flatten()
    {
        for file in std::fs::read_dir(project.path().join("sessions"))
            .into_iter()
            .flatten()
            .flatten()
        {
            count += usize::from(file.path().extension().is_some_and(|e| e == "jsonl"));
        }
    }
    count
}

#[tokio::test]
async fn a_client_lists_the_tools_reads_a_file_and_gets_a_reply_to_a_prompt() {
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.project().join("notes.txt"), "alpha\nbeta\n").unwrap();
    let script = json!({ "lead": [{ "reply": { "text": "It has two lines." } }] });
    let service = sandbox.serve(&script, None).await;

    let tools = service.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert_eq!(
        names,
        ["azazel_prompt", "read", "glob", "grep", "web_search", "web_fetch"]
    );
    let prompt_tool = serde_json::to_value(&tools[0]).unwrap();
    assert_eq!(prompt_tool["inputSchema"]["required"], json!(["prompt"]));
    assert!(tools[1..].iter().all(|tool| {
        tool.annotations
            .as_ref()
            .is_some_and(|notes| notes.read_only_hint == Some(true))
    }));

    // A read-only tool, under its own name, in the directory the server works in.
    let read = call(&service, "read", json!({ "path": "notes.txt" })).await;
    assert_eq!(read.is_error, Some(false));
    assert!(texts(&read)[0].contains("alpha") && texts(&read)[0].contains("beta"));
    let missing = call(&service, "read", json!({ "path": "nope.txt" })).await;
    assert_eq!(missing.is_error, Some(true));
    let unknown = service
        .call_tool(CallToolRequestParams::new("write".to_string()))
        .await;
    assert!(unknown.is_err(), "write is not served");

    // A prompt runs a session to its end; the reply comes with the id to continue it by.
    let reply = call(
        &service,
        "azazel_prompt",
        json!({ "prompt": "how long is notes.txt?" }),
    )
    .await;
    assert_eq!(reply.is_error, Some(false), "{reply:?}");
    let said = texts(&reply);
    assert_eq!(said[0], "It has two lines.");
    let session_id = said[1].strip_prefix("session: ").unwrap().to_string();
    let structured = reply.structured_content.clone().unwrap();
    assert_eq!(
        structured,
        json!({ "reply": "It has two lines.", "session_id": session_id })
    );
    assert_eq!(session_files(&sandbox.dir.path().join("home")), 1);

    // Continuing it adds to the same session: the request carries the earlier exchange. (Each
    // session reads the script from its start, so the reply is the same.)
    let again = call(
        &service,
        "azazel_prompt",
        json!({ "prompt": "and in bytes?", "session": session_id }),
    )
    .await;
    assert_eq!(again.structured_content.unwrap()["session_id"], session_id);
    assert_eq!(session_files(&sandbox.dir.path().join("home")), 1);
    let requests = std::fs::read_to_string(sandbox.dir.path().join("requests.jsonl")).unwrap();
    // (Memory makes requests of its own when a session ends; the lead's are told by the tools
    // they offer.)
    let of_the_lead: Vec<Value> = requests
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
        .filter(|request| request["tools"].as_array().is_some_and(|tools| !tools.is_empty()))
        .collect();
    assert_eq!(of_the_lead.len(), 2);
    let sent: Vec<&str> = of_the_lead[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"][0]["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        sent,
        ["how long is notes.txt?", "It has two lines.", "and in bytes?"]
    );

    let failed = call(
        &service,
        "azazel_prompt",
        json!({ "prompt": "x", "session": "ffffffff" }),
    )
    .await;
    assert_eq!(failed.is_error, Some(true));
    assert!(
        texts(&failed)[0].contains("no session with id `ffffffff`"),
        "{failed:?}"
    );

    service.cancel().await.unwrap();
}

fn write_script() -> Value {
    json!({ "lead": [
        { "reply": { "tool_calls": [
            { "id": "c1", "name": "write", "input": { "path": "made.txt", "content": "made\n" } }
        ] } },
        { "reply": { "text": "Done." } }
    ] })
}

#[tokio::test]
async fn a_permission_is_asked_of_the_client_that_can_ask_and_refused_otherwise() {
    // The client's user allows it.
    let sandbox = Sandbox::new();
    let service = sandbox.serve(&write_script(), Some(true)).await;
    let reply = call(&service, "azazel_prompt", json!({ "prompt": "make a file" })).await;
    assert_eq!(texts(&reply)[0], "Done.");
    assert_eq!(
        sandbox.asked(),
        ["azazel wants to use `write` on made.txt. Allow it?"]
    );
    assert!(sandbox.project().join("made.txt").is_file());
    service.cancel().await.unwrap();

    // The client's user refuses it.
    let sandbox = Sandbox::new();
    let service = sandbox.serve(&write_script(), Some(false)).await;
    call(&service, "azazel_prompt", json!({ "prompt": "make a file" })).await;
    assert_eq!(sandbox.asked().len(), 1);
    assert!(!sandbox.project().join("made.txt").exists());
    service.cancel().await.unwrap();

    // A client that cannot ask: the call is refused without a question, unless the mode
    // allows it anyway.
    let sandbox = Sandbox::new();
    let service = sandbox.serve(&write_script(), None).await;
    call(&service, "azazel_prompt", json!({ "prompt": "make a file" })).await;
    assert!(!sandbox.project().join("made.txt").exists());
    // A client cannot lift the guards for itself: bypass is for whoever starts the server.
    let refused = call(
        &service,
        "azazel_prompt",
        json!({ "prompt": "make a file", "mode": "bypass" }),
    )
    .await;
    assert_eq!(refused.is_error, Some(true));
    assert!(
        texts(&refused)[0].contains("bypass mode is not available to an MCP client"),
        "{refused:?}"
    );
    assert!(!sandbox.project().join("made.txt").exists());
    let allowed = call(
        &service,
        "azazel_prompt",
        json!({ "prompt": "make a file", "mode": "auto" }),
    )
    .await;
    assert_eq!(texts(&allowed)[0], "Done.");
    assert!(sandbox.project().join("made.txt").is_file());
    assert_eq!(sandbox.asked(), Vec::<String>::new());
    service.cancel().await.unwrap();
}
