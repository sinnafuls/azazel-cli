use std::time::Duration;

use azazel_remote::{Access, Server};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::common::*;

#[tokio::test]
async fn without_a_device_or_a_session_the_socket_closes_with_a_code() {
    let (server, ..) = server().await;
    let port = server.port();
    let mut nobody = connect(port, "s1", None).await;
    assert_eq!(close_code(&mut nobody).await, 4401);
    let mut bogus = connect(port, "s1", Some("not-a-token")).await;
    assert_eq!(close_code(&mut bogus).await, 4401);
    let token = pair(&server, Access::Control).await;
    let mut unknown = connect(port, "unknown", Some(&token)).await;
    assert_eq!(close_code(&mut unknown).await, 4404);
}

#[tokio::test]
async fn a_controller_sends_commands_and_gets_the_frames() {
    let (server, fake, mut received) = server().await;
    let token = pair(&server, Access::Control).await;
    let mut socket = connect(server.port(), "s1", Some(&token)).await;
    assert_eq!(next_json(&mut socket).await["type"], "hello");
    send(&mut socket, json!({ "id": 1, "type": "prompt", "text": "go" })).await;
    let command = sent(&mut received).await;
    assert_eq!(
        (command["type"].clone(), command["text"].clone()),
        ("prompt".into(), "go".into())
    );
    fake.push(r#"{"type":"agent_start","agent":"lead","at":10}"#)
        .await;
    let frame = next_json(&mut socket).await;
    assert_eq!(
        (frame["type"].clone(), frame["at"].clone()),
        ("agent_start".into(), 10.into())
    );
}

#[tokio::test]
async fn a_viewer_sees_the_frames_and_its_commands_are_answered_with_a_refusal() {
    let (server, fake, mut received) = server().await;
    let token = pair(&server, Access::View).await;
    let mut socket = connect(server.port(), "s1", Some(&token)).await;
    next_json(&mut socket).await;
    send(&mut socket, json!({ "id": "p", "type": "prompt", "text": "go" })).await;
    let refused = next_json(&mut socket).await;
    assert_eq!(
        (
            refused["type"].clone(),
            refused["id"].clone(),
            refused["success"].clone()
        ),
        ("response".into(), "p".into(), false.into())
    );
    send(&mut socket, json!({ "id": 2, "type": "get_state" })).await;
    // Only what was allowed reached the session.
    assert_eq!(sent(&mut received).await["type"], "get_state");
    fake.push(r#"{"type":"idle"}"#).await;
    assert_eq!(next_json(&mut socket).await["type"], "idle");
}

#[tokio::test]
async fn a_controller_cannot_reach_the_machine_through_the_session() {
    let (server, _fake, mut received) = server().await;
    let token = pair(&server, Access::Control).await;
    let mut socket = connect(server.port(), "s1", Some(&token)).await;
    next_json(&mut socket).await;
    let refused = [
        json!({ "id": "a", "type": "shell", "command": "ls" }),
        json!({ "id": "b", "type": "export", "path": "/tmp/x.html" }),
        json!({ "id": "c", "type": "set_role", "role": "default", "save": "home" }),
        json!({ "id": "d", "type": "set_mode", "mode": "bypass" }),
        json!({ "id": "e", "type": "run_command", "name": "trust" }),
    ];
    for command in &refused {
        send(&mut socket, command.clone()).await;
        let answer = next_json(&mut socket).await;
        assert_eq!(
            (answer["id"].clone(), answer["success"].clone()),
            (command["id"].clone(), false.into())
        );
    }
    // Nothing got through; a command after them does.
    send(&mut socket, json!({ "id": "f", "type": "abort" })).await;
    assert_eq!(sent(&mut received).await["type"], "abort");
}

#[tokio::test]
async fn a_browser_is_offered_only_the_commands_it_may_run() {
    let (server, fake, mut received) = server().await;
    let token = pair(&server, Access::Control).await;
    let mut socket = connect(server.port(), "s1", Some(&token)).await;
    next_json(&mut socket).await;
    send(&mut socket, json!({ "id": "l", "type": "list_commands" })).await;
    assert_eq!(sent(&mut received).await["type"], "list_commands");
    let rows = [
        "cost",
        "trust",
        "login",
        "skill:review",
        "mcp:github:triage",
        "goal",
        "deploy",
    ]
    .map(|name| json!({ "name": name, "description": "" }));
    let answer = json!({ "type": "response", "id": "l", "success": true, "data": rows });
    fake.push(&answer.to_string()).await;
    let listed = next_json(&mut socket).await;
    let names: Vec<&str> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    // `deploy`, a project's own command, needs `allow_bypass`, which this server does not set.
    assert_eq!(names, ["cost", "skill:review", "mcp:github:triage", "goal"]);
    // Another response with the same shape is not touched.
    let other = json!({ "type": "response", "id": "m", "success": true, "data": [{ "name": "trust" }] });
    fake.push(&other.to_string()).await;
    assert_eq!(next_json(&mut socket).await["data"][0]["name"], "trust");
}

#[tokio::test]
async fn a_message_that_is_not_a_command_or_not_text_is_not_forwarded() {
    let (server, ..) = server().await;
    let token = pair(&server, Access::Control).await;
    let mut socket = connect(server.port(), "s1", Some(&token)).await;
    next_json(&mut socket).await;
    socket.send(Message::text("not json")).await.unwrap();
    assert_eq!(next_json(&mut socket).await["success"], false);
    socket.send(Message::binary(vec![1u8, 2, 3])).await.unwrap();
    assert_eq!(close_code(&mut socket).await, 1003);
}

#[tokio::test]
async fn a_burst_of_commands_is_cut_off_at_the_rate_limit() {
    let (server, ..) = server().await;
    let token = pair(&server, Access::Control).await;
    let mut socket = connect(server.port(), "s1", Some(&token)).await;
    next_json(&mut socket).await;
    for n in 0..80 {
        send(&mut socket, json!({ "id": n, "type": "get_state" })).await;
    }
    let first = next_json(&mut socket).await;
    assert_eq!(first["success"], false);
    // The commands within the burst were forwarded, so the first answer is for a surplus one.
    assert!(first["id"].as_u64().unwrap() >= 40);
}

#[tokio::test]
async fn revoking_closes_the_sockets_and_the_device_stays_out() {
    let (server, ..) = server().await;
    let port = server.port();
    let token = pair(&server, Access::Control).await;
    let mut socket = connect(port, "s1", Some(&token)).await;
    next_json(&mut socket).await;
    server.revoke_all();
    assert_eq!(close_code(&mut socket).await, 4401);
    let mut again = connect(port, "s1", Some(&token)).await;
    assert_eq!(close_code(&mut again).await, 4401);
}

#[tokio::test]
async fn a_device_has_four_sockets() {
    let (server, ..) = server().await;
    let port = server.port();
    let token = pair(&server, Access::Control).await;
    let mut held = Vec::new();
    for _ in 0..4 {
        let mut socket = connect(port, "s1", Some(&token)).await;
        next_json(&mut socket).await;
        held.push(socket);
    }
    let mut fifth = connect(port, "s1", Some(&token)).await;
    assert_eq!(close_code(&mut fifth).await, 4409);
    // Another device is not held up by it.
    let other = pair(&server, Access::View).await;
    let mut socket = connect(port, "s1", Some(&other)).await;
    assert_eq!(next_json(&mut socket).await["type"], "hello");
}

#[tokio::test]
async fn stopping_the_server_closes_every_socket() {
    let (server, ..) = server().await;
    let token = pair(&server, Access::Control).await;
    let mut socket = connect(server.port(), "s1", Some(&token)).await;
    next_json(&mut socket).await;
    let waiting = tokio::spawn(async move { close_code(&mut socket).await });
    server.shutdown().await;
    assert_eq!(waiting.await.unwrap(), 1001);
}

#[tokio::test]
async fn a_view_link_is_told_nothing_of_the_browser_unless_view_browser_is_on() {
    let (server, fake, _received) = server().await;
    let viewer = pair(&server, Access::View).await;
    let controller = pair(&server, Access::Control).await;
    let mut view = connect(server.port(), "s1", Some(&viewer)).await;
    let mut control = connect(server.port(), "s1", Some(&controller)).await;
    next_json(&mut view).await;
    next_json(&mut control).await;
    fake.push(
        r#"{"type":"browser_state","running":true,"url":"https://bank.test/account","title":"Account"}"#,
    )
    .await;
    fake.push(r#"{"type":"browser_frame","tab":"t","data":"","width":1,"height":1,"seq":1}"#)
        .await;
    fake.push(r#"{"type":"idle"}"#).await;
    // The controller is told of all three; the viewer reads on to the first line that is not about
    // the browser.
    assert_eq!(next_json(&mut control).await["type"], "browser_state");
    assert_eq!(next_json(&mut control).await["type"], "browser_frame");
    assert_eq!(next_json(&mut view).await["type"], "idle");
}

const PING: Duration = Duration::from_millis(100);
const ANSWER: Duration = Duration::from_millis(300);

#[tokio::test]
async fn a_client_that_stops_answering_pings_is_let_go() {
    let (fake, _received) = Fake::new();
    let server = Server::start_with_pings(options(), fake.clone(), PING, ANSWER)
        .await
        .unwrap();
    let token = pair(&server, Access::Control).await;
    // The socket is not read, so nothing answers the server's pings: a peer that vanished.
    let mut socket = connect(server.port(), "s1", Some(&token)).await;
    tokio::time::timeout(WAIT, async {
        while fake.open_connections() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the server lets the silent client go");
    // Answering the server's pings now is too late: a close frame may still reach the client, or
    // the connection may already be gone.
    while let Some(Ok(message)) = socket.next().await {
        if let Message::Close(Some(frame)) = message {
            assert_eq!(u16::from(frame.code), 4410);
        }
    }
}

#[tokio::test]
async fn a_client_that_answers_pings_stays_connected() {
    let (fake, _received) = Fake::new();
    let server = Server::start_with_pings(options(), fake.clone(), PING, ANSWER)
        .await
        .unwrap();
    let token = pair(&server, Access::Control).await;
    let mut socket = connect(server.port(), "s1", Some(&token)).await;
    assert_eq!(next_json(&mut socket).await["type"], "hello");
    // Reading answers the pings; several rounds go by.
    let mut pings = 0;
    let outcome = tokio::time::timeout(ANSWER * 5, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Ping(_))) => pings += 1,
                other => return other,
            }
        }
    })
    .await;
    assert!(
        outcome.is_err(),
        "the server closed a client that answers: {outcome:?}"
    );
    assert!(pings >= 3, "only {pings} pings arrived");
    assert_eq!(fake.open_connections(), 1);
}
