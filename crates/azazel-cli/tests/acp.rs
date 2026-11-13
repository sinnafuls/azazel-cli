#![cfg(feature = "acp")]
#![allow(clippy::unwrap_used)]

//! `azazel acp`, driven by the client role of the Agent Client Protocol SDK.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema as acp;
use agent_client_protocol::{Agent, ByteStreams, Client};
use serde_json::{Value, json};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// What the agent sent the client, as JSON, in the order it arrived.
#[derive(Clone, Default)]
struct Seen {
    updates: Arc<Mutex<Vec<Value>>>,
    permissions: Arc<Mutex<Vec<Value>>>,
}

impl Seen {
    /// The updates so far, and forgets them.
    fn take(&self) -> Vec<Value> {
        std::mem::take(&mut *self.updates.lock().unwrap())
    }

    /// Waits for an update `wanted` accepts.
    async fn wait_for(&self, wanted: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..300 {
            if let Some(found) = self.updates.lock().unwrap().iter().find(|update| wanted(update)) {
                return found.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the update did not arrive: {:?}", self.updates.lock().unwrap());
    }
}

fn kinds(updates: &[Value]) -> Vec<&str> {
    updates
        .iter()
        .map(|update| update["sessionUpdate"].as_str().unwrap())
        .collect()
}

fn text(prompt: &str) -> acp::ContentBlock {
    acp::ContentBlock::Text(acp::TextContent::new(prompt))
}

/// Starts `azazel acp` answering from `script` and runs `main` as its client.
async fn with_agent<R>(
    dir: &Path,
    script: &Value,
    seen: Seen,
    main: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<R, agent_client_protocol::Error>,
) -> R {
    let script_path = dir.join("script.json");
    std::fs::write(&script_path, script.to_string()).unwrap();
    let mut child = tokio::process::Command::new(assert_cmd::cargo::cargo_bin("azazel"))
        .env("AZAZEL_HOME", dir.join("home"))
        .env("AZAZEL_MOCK_SCRIPT", &script_path)
        .env("AZAZEL_MOCK_REQUESTS", dir.join("requests.jsonl"))
        // The session's name comes from a queue of its own, so it takes no reply of the lead's.
        .args(["--set", "roles.smol=[\"mock/smol\"]", "acp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let transport = ByteStreams::new(
        child.stdin.take().unwrap().compat_write(),
        child.stdout.take().unwrap().compat(),
    );
    let (updates, permissions) = (seen.updates.clone(), seen.permissions.clone());
    let result = Client
        .v1()
        .on_receive_notification(
            async move |notification: acp::SessionNotification, _connection| {
                updates
                    .lock()
                    .unwrap()
                    .push(serde_json::to_value(&notification.update).unwrap());
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: acp::RequestPermissionRequest, responder, _connection| {
                permissions
                    .lock()
                    .unwrap()
                    .push(serde_json::to_value(&request).unwrap());
                // The user allows it, once.
                let allow = request
                    .options
                    .iter()
                    .find(|option| option.kind == acp::PermissionOptionKind::AllowOnce)
                    .unwrap();
                responder.respond(acp::RequestPermissionResponse::new(
                    acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                        allow.option_id.clone(),
                    )),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, main)
        .await
        .unwrap();
    // The connection is closed: the agent ends its sessions and exits.
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .expect("azazel exits when the client goes")
        .unwrap();
    assert!(status.success(), "{status}");
    result
}

fn project(dir: &Path) -> PathBuf {
    let project = dir.join("project");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    project
}

fn script() -> Value {
    json!({
        "lead": [
            { "reply": { "tool_calls": [
                { "id": "c1", "name": "write", "input": { "path": "hello.txt", "content": "hello from azazel\n" } }
            ] } },
            { "reply": { "tool_calls": [{ "id": "c2", "name": "read", "input": { "path": "hello.txt" } }] } },
            { "reply": { "text": "Wrote and verified hello.txt." } },
            { "delay_ms": 20000, "reply": { "text": "never said" } },
            { "delay_ms": 20000, "reply": { "text": "never said either" } },
            { "reply": { "text": "The second prompt was answered." } }
        ],
        "smol": [{ "reply": { "text": "Hello file" } }]
    })
}

#[tokio::test]
async fn an_editor_runs_prompts_answers_permissions_cancels_and_loads_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = project(dir.path());
    let seen = Seen::default();
    let watching = seen.clone();
    let project_dir = cwd.clone();
    let session_id = with_agent(dir.path(), &script(), seen.clone(), async move |agent| {
        let seen = watching;
        let initialized = agent
            .send_request(acp::InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        assert_eq!(initialized.protocol_version, ProtocolVersion::V1);
        let about = serde_json::to_value(&initialized).unwrap();
        assert_eq!(about["agentCapabilities"]["loadSession"], true);
        assert_eq!(
            about["agentCapabilities"]["promptCapabilities"],
            json!({ "image": true, "audio": false, "embeddedContext": true })
        );
        assert_eq!(
            about["agentCapabilities"]["mcpCapabilities"],
            json!({ "http": true, "sse": true })
        );
        assert_eq!(about["authMethods"].as_array().unwrap().len(), 1);
        assert_eq!(about["agentInfo"]["name"], "azazel");
        agent
            .send_request(acp::AuthenticateRequest::new("azazel"))
            .block_task()
            .await?;

        let session = agent
            .send_request(acp::NewSessionRequest::new(project_dir.clone()))
            .block_task()
            .await?;
        let modes = session.modes.clone().unwrap();
        assert_eq!(modes.current_mode_id.to_string(), "ask");
        let offered: Vec<String> = modes
            .available_modes
            .iter()
            .map(|mode| mode.id.to_string())
            .collect();
        assert_eq!(offered, ["plan", "ask", "accept-edits", "auto"]);
        let session_id = session.session_id;

        // A prompt with an embedded file: the write needs the user's word, the read does not.
        let attached = acp::ContentBlock::Resource(acp::EmbeddedResource::new(
            acp::EmbeddedResourceResource::TextResourceContents(acp::TextResourceContents::new(
                "fn main() {}\n",
                "untitled:scratch.rs",
            )),
        ));
        let done = agent
            .send_request(acp::PromptRequest::new(
                session_id.clone(),
                vec![text("create hello"), attached],
            ))
            .block_task()
            .await?;
        assert_eq!(done.stop_reason, acp::StopReason::EndTurn);
        assert_eq!(
            std::fs::read_to_string(project_dir.join("hello.txt")).unwrap(),
            "hello from azazel\n"
        );
        let asked = seen.permissions.lock().unwrap().clone();
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert_eq!(asked[0]["sessionId"], session_id.to_string());
        assert_eq!(
            (&asked[0]["toolCall"]["toolCallId"], &asked[0]["toolCall"]["kind"]),
            (&json!("c1"), &json!("edit"))
        );
        assert_eq!(asked[0]["toolCall"]["title"], "write hello.txt");
        let offered: Vec<&str> = asked[0]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option["kind"].as_str().unwrap())
            .collect();
        assert_eq!(offered, ["allow_once", "allow_always", "reject_once"]);

        // The session is named after its first reply, and the client is told. The name may come
        // before or after this point, so it is waited for before the updates are taken.
        let named = seen
            .wait_for(|update| update["sessionUpdate"] == "session_info_update")
            .await;
        assert_eq!(named["title"], "Hello file");
        let updates: Vec<Value> = seen
            .take()
            .into_iter()
            .filter(|update| update["sessionUpdate"] != "session_info_update")
            .collect();
        assert_eq!(
            kinds(&updates)[..5],
            [
                "tool_call",
                "tool_call_update",
                "tool_call",
                "tool_call_update",
                "agent_message_chunk"
            ],
            "{updates:?}"
        );
        assert_eq!(
            (
                &updates[0]["toolCallId"],
                &updates[0]["kind"],
                &updates[0]["status"]
            ),
            (&json!("c1"), &json!("edit"), &json!("in_progress"))
        );
        assert!(
            updates[0]["locations"][0]["path"]
                .as_str()
                .unwrap()
                .ends_with("hello.txt"),
            "{}",
            updates[0]
        );
        // The write ends with a diff of the file: it did not exist before.
        assert_eq!(updates[1]["status"], "completed");
        let diff = updates[1]["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|content| content["type"] == "diff")
            .unwrap();
        assert_eq!(
            (&diff["oldText"], &diff["newText"]),
            (&Value::Null, &json!("hello from azazel\n"))
        );
        assert_eq!(
            (&updates[2]["toolCallId"], &updates[2]["kind"]),
            (&json!("c2"), &json!("read"))
        );
        assert_eq!(updates[4]["content"]["text"], "Wrote and verified hello.txt.");

        // The mode is the permission mode; the change is announced.
        agent
            .send_request(acp::SetSessionModeRequest::new(session_id.clone(), "auto"))
            .block_task()
            .await?;
        let mode = seen
            .wait_for(|update| update["sessionUpdate"] == "current_mode_update")
            .await;
        assert_eq!(mode["currentModeId"], "auto");
        let unknown = agent
            .send_request(acp::SetSessionModeRequest::new(session_id.clone(), "yolo"))
            .block_task()
            .await;
        assert!(unknown.is_err());

        // A prompt is cancelled while its reply is on the way.
        let slow = agent.send_request(acp::PromptRequest::new(session_id.clone(), vec![text("slow")]));
        tokio::time::sleep(Duration::from_millis(500)).await;
        agent.send_notification(acp::CancelNotification::new(session_id.clone()))?;
        assert_eq!(slow.block_task().await?.stop_reason, acp::StopReason::Cancelled);

        // A prompt that arrives while another runs cancels it first.
        let first = agent.send_request(acp::PromptRequest::new(session_id.clone(), vec![text("one")]));
        tokio::time::sleep(Duration::from_millis(500)).await;
        let second = agent.send_request(acp::PromptRequest::new(session_id.clone(), vec![text("two")]));
        assert_eq!(first.block_task().await?.stop_reason, acp::StopReason::Cancelled);
        assert_eq!(second.block_task().await?.stop_reason, acp::StopReason::EndTurn);
        // The reply's last chunk may arrive just after the prompt's answer.
        seen.wait_for(|update| update["content"]["text"] == "The second prompt was answered.")
            .await;
        seen.take();

        let nowhere = agent
            .send_request(acp::PromptRequest::new("no-such-session", vec![text("x")]))
            .block_task()
            .await;
        assert!(nowhere.is_err());
        Ok(session_id)
    })
    .await;

    // The embedded file went to the model with the prompt.
    let requests = std::fs::read_to_string(dir.path().join("requests.jsonl")).unwrap();
    let first: Value = serde_json::from_str(requests.lines().next().unwrap()).unwrap();
    let sent = first["request"]["messages"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(
        sent,
        "create hello\n\n<file path=\"untitled:scratch.rs\">\nfn main() {}\n</file>"
    );

    // Another editor session loads the stored one: the conversation is replayed first.
    let seen = Seen::default();
    let watching = seen.clone();
    let project_dir = cwd.clone();
    with_agent(dir.path(), &script(), seen.clone(), async move |agent| {
        agent
            .send_request(acp::InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        let loaded = agent
            .send_request(acp::LoadSessionRequest::new(
                session_id.clone(),
                project_dir.clone(),
            ))
            .block_task()
            .await?;
        assert_eq!(loaded.modes.unwrap().available_modes.len(), 4);
        let replayed = watching.take();
        assert_eq!(
            kinds(&replayed)[..6],
            [
                "user_message_chunk",
                "tool_call",
                "tool_call_update",
                "tool_call",
                "tool_call_update",
                "agent_message_chunk"
            ],
            "{replayed:?}"
        );
        assert!(
            replayed[0]["content"]["text"]
                .as_str()
                .unwrap()
                .starts_with("create hello")
        );
        assert_eq!(
            (&replayed[2]["toolCallId"], &replayed[2]["status"]),
            (&json!("c1"), &json!("completed"))
        );
        assert_eq!(replayed[5]["content"]["text"], "Wrote and verified hello.txt.");
        let missing = agent
            .send_request(acp::LoadSessionRequest::new("ffffffff", project_dir))
            .block_task()
            .await;
        assert!(missing.is_err());
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn a_question_is_a_choice_and_the_task_list_is_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = project(dir.path());
    let script = json!({
        "lead": [
            { "reply": { "tool_calls": [{ "id": "c1", "name": "ask_user", "input": {
                "question": "Which colour?", "options": ["red", "blue"]
            } }] } },
            { "reply": { "tool_calls": [{ "id": "c2", "name": "todo", "input": { "items": [
                { "text": "paint it red", "status": "in_progress" },
                { "text": "let it dry", "status": "pending" }
            ] } }] } },
            { "reply": { "text": "Painting." } }
        ],
        "smol": [{ "reply": { "text": "Colours" } }]
    });
    let seen = Seen::default();
    let watching = seen.clone();
    with_agent(dir.path(), &script, seen.clone(), async move |agent| {
        agent
            .send_request(acp::InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        let session = agent
            .send_request(acp::NewSessionRequest::new(cwd))
            .block_task()
            .await?;
        let done = agent
            .send_request(acp::PromptRequest::new(session.session_id, vec![text("paint")]))
            .block_task()
            .await?;
        assert_eq!(done.stop_reason, acp::StopReason::EndTurn);

        // The question came as a choice; the test's user takes the first.
        let asked = watching.permissions.lock().unwrap().clone();
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert_eq!(asked[0]["toolCall"]["title"], "Which colour?");
        let choices: Vec<&str> = asked[0]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option["name"].as_str().unwrap())
            .collect();
        assert_eq!(choices, ["red", "blue"]);

        let updates = watching.take();
        let plan = updates
            .iter()
            .find(|update| update["sessionUpdate"] == "plan")
            .unwrap_or_else(|| panic!("no plan in {updates:?}"));
        assert_eq!(
            plan["entries"],
            json!([
                { "content": "paint it red", "priority": "medium", "status": "in_progress" },
                { "content": "let it dry", "priority": "medium", "status": "pending" }
            ])
        );
        Ok(())
    })
    .await;

    // The answer went back to the model as the tool's result.
    let requests = std::fs::read_to_string(dir.path().join("requests.jsonl")).unwrap();
    let second: Value = serde_json::from_str(requests.lines().nth(1).unwrap()).unwrap();
    let result = second["request"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap()
        .to_string();
    assert!(result.contains("red") && !result.contains("blue"), "{result}");
}
