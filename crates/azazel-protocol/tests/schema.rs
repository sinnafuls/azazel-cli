#![allow(clippy::unwrap_used)]

use std::path::Path;

use azazel_protocol::{Frame, Response, schema};
use serde_json::{Value, json};

#[test]
fn the_checked_in_schema_is_current() {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/rpc.schema.json");
    let checked_in: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert!(
        checked_in == schema(),
        "docs/rpc.schema.json is stale: run `cargo xtask rpc-schema` and commit the result"
    );
}

#[test]
fn the_schema_names_every_line_and_result() {
    let schema = schema();
    let defs = schema["$defs"].as_object().unwrap();
    for name in [
        "Request",
        "Frame",
        "State",
        "SessionRow",
        "TreeRow",
        "BtwAnswer",
        "CommandResult",
    ] {
        assert!(defs.contains_key(name), "{name} is not defined");
    }
    assert_eq!(
        schema["oneOf"],
        json!([{ "$ref": "#/$defs/Request" }, { "$ref": "#/$defs/Frame" }])
    );
}

#[test]
fn responses_keep_their_wire_form() {
    let ok = Frame::Response(Response::ok(json!(2), Value::Null));
    assert_eq!(
        serde_json::to_value(&ok).unwrap(),
        json!({ "id": 2, "type": "response", "success": true, "data": null })
    );
    let failed = Frame::Response(Response::err(Value::Null, "unknown command `dance`"));
    let wire = serde_json::to_value(&failed).unwrap();
    assert_eq!(
        wire,
        json!({ "id": null, "type": "response", "success": false, "error": "unknown command `dance`" })
    );
    assert_eq!(serde_json::from_value::<Frame>(wire).unwrap(), failed);
    assert_eq!(
        serde_json::from_value::<Frame>(json!({ "id": 2, "type": "response", "success": true, "data": null }))
            .unwrap(),
        ok
    );
}

#[test]
fn frames_read_what_azazel_writes_and_tolerate_what_they_do_not_know() {
    let request: Frame = serde_json::from_value(json!({
        "type": "ui_request", "request_id": "r3", "kind": "select", "prompt": "Pick", "options": ["a", "b"]
    }))
    .unwrap();
    assert_eq!(
        request,
        Frame::UiRequest {
            request_id: "r3".into(),
            request: azazel_protocol::UiRequest::Select {
                prompt: "Pick".into(),
                options: vec!["a".into(), "b".into()]
            }
        }
    );
    let delta: Frame = serde_json::from_value(json!({
        "type": "stream", "agent": "lead", "event": { "type": "text_delta", "index": 0, "delta": "hi" }
    }))
    .unwrap();
    assert_eq!(delta.agent(), Some("lead"));
    let later: Frame = serde_json::from_value(json!({ "type": "from_the_future", "x": 1 })).unwrap();
    assert_eq!(later, Frame::Unknown);
    let event: Frame = serde_json::from_value(json!({
        "type": "stream", "agent": "lead", "event": { "type": "from_the_future" }
    }))
    .unwrap();
    assert!(matches!(
        event,
        Frame::Stream {
            event: azazel_protocol::StreamEvent::Unknown,
            ..
        }
    ));
}
