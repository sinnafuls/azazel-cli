//! The client library against the real binary.

use std::path::Path;

use azazel_client::protocol::{Command, Decision, Frame, State};
use azazel_client::{Client, Options, Run};

#[tokio::test]
async fn the_client_library_drives_a_prompt_through_an_approval() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/scripts/write_read.json");
    let mut options = Options::new(assert_cmd::cargo::cargo_bin("azazel"));
    options.cwd = Some(project.clone());
    options.args = vec!["--set".into(), "session.auto_name=false".into()];
    options.env = vec![
        ("AZAZEL_HOME".into(), dir.path().join("home").into()),
        ("AZAZEL_MOCK_SCRIPT".into(), script.into()),
    ];
    let client = Client::spawn(options).await.unwrap();
    assert_eq!(client.version(), env!("CARGO_PKG_VERSION"));

    let mut asked = Vec::new();
    let run = client
        .prompt_and_wait("create hello", |frame| match frame {
            Frame::ApprovalRequest {
                request_id,
                tool,
                subjects,
                ..
            } => {
                asked.push((tool.clone(), subjects.clone()));
                Some(Command::ApprovalResponse {
                    request_id: request_id.clone(),
                    decision: Decision::Allow,
                    feedback: None,
                })
            }
            _ => None,
        })
        .await
        .unwrap();
    assert_eq!(
        run,
        Run {
            text: "Wrote and verified hello.txt.".into(),
            stop: "stop".into()
        }
    );
    // Writing needs the user's word in `manual` mode; reading does not.
    assert_eq!(asked, [("write".to_string(), vec!["hello.txt".to_string()])]);
    assert_eq!(
        std::fs::read_to_string(project.join("hello.txt")).unwrap(),
        "hello from azazel\n"
    );

    let state: State = serde_json::from_value(client.send(Command::GetState).await.unwrap()).unwrap();
    assert_eq!((state.mode.as_str(), state.busy), ("manual", false));
    let refused = client
        .send(Command::SetMode { mode: "yolo".into() })
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("yolo"), "{refused}");
    // The mode of a session someone is attached to is changed there, by its new name only.
    let renamed = client
        .send(Command::SetMode { mode: "ask".into() })
        .await
        .unwrap_err();
    assert!(renamed.to_string().contains("`manual`"), "{renamed}");
    client
        .send(Command::SetMode {
            mode: "bypass".into(),
        })
        .await
        .unwrap();
    let state: State = serde_json::from_value(client.send(Command::GetState).await.unwrap()).unwrap();
    assert_eq!(state.mode, "bypass");
    assert_eq!(client.shutdown().await.unwrap(), Some(0));
}
