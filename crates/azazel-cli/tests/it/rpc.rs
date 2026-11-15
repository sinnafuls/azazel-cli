use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use serde_json::{Value, json};

use crate::common::script_fixture as fixture;

/// An `azazel rpc` process and the lines it writes.
struct Client {
    dir: tempfile::TempDir,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Client {
    fn start(script: &str) -> Client {
        Client::start_with(script, "")
    }

    /// Like [`Client::start`], with `config` as the user's settings.
    fn start_with(script: &str, config: &str) -> Client {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::write(dir.path().join("home/config.toml"), config).unwrap();
        let mut child = Command::new(assert_cmd::cargo::cargo_bin("azazel"))
            .env("AZAZEL_HOME", dir.path().join("home"))
            .env("AZAZEL_MOCK_SCRIPT", fixture(script))
            .args(["rpc", "--cwd"])
            .arg(&project)
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
            dir,
            child,
            lines,
        }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn send(&mut self, command: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{command}").unwrap();
        stdin.flush().unwrap();
    }

    /// The next line, which must be JSON: nothing else may appear on standard output.
    fn next(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(60))
            .expect("a protocol line within a minute");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not a protocol line ({e}): {line}"))
    }

    /// Reads up to the first line `wanted` accepts; returns the lines before it and that line.
    fn until(&self, wanted: impl Fn(&Value) -> bool) -> (Vec<Value>, Value) {
        let mut before = Vec::new();
        loop {
            let line = self.next();
            if wanted(&line) {
                return (before, line);
            }
            before.push(line);
        }
    }

    /// Sends a command and returns its response, with the events that came first.
    fn ask(&mut self, id: u64, mut command: Value) -> (Vec<Value>, Value) {
        command["id"] = id.into();
        self.send(command);
        self.until(|line| line["type"] == "response" && line["id"] == id)
    }

    /// Closes standard input and returns the exit code.
    fn finish(mut self) -> Option<i32> {
        drop(self.stdin.take());
        self.child.wait().unwrap().code()
    }
}

fn kinds(lines: &[Value]) -> Vec<&str> {
    lines.iter().map(|line| line["type"].as_str().unwrap()).collect()
}

#[test]
fn a_prompt_runs_through_an_approval_to_idle() {
    let mut client = Client::start("write_read.json");

    let (_, state) = client.ask(1, json!({ "type": "get_state" }));
    assert_eq!(state["success"], true, "{state}");
    let data = &state["data"];
    assert_eq!(
        (&data["mode"], &data["busy"], &data["turn"]),
        (&json!("manual"), &json!(false), &json!(0))
    );
    assert_eq!(data["chain"], json!(["@lead"]));

    // The prompt is accepted at once; the work shows up as events.
    let (_, accepted) = client.ask(2, json!({ "type": "prompt", "text": "create hello" }));
    assert_eq!(
        accepted,
        json!({ "id": 2, "type": "response", "success": true, "data": null })
    );

    // Writing a file needs the user's word in `manual` mode: the agent waits for the answer.
    let (before, request) = client.until(|line| line["type"] == "approval_request");
    assert!(kinds(&before).contains(&"tool_start"), "{before:?}");
    assert!(!kinds(&before).contains(&"tool_end"));
    assert_eq!(request["agent"], "lead");
    assert_eq!(request["tool"], "write");
    assert_eq!(request["access"], "write");
    assert_eq!(request["subjects"], json!(["hello.txt"]));
    assert_eq!(request["input"]["content"], "hello from azazel\n");
    assert!(!client.project().join("hello.txt").exists());
    let (_, busy) = client.ask(3, json!({ "type": "get_state" }));
    assert_eq!(busy["data"]["busy"], true);

    // An answer for a request that does not exist changes nothing.
    let (_, stray) = client.ask(
        4,
        json!({ "type": "approval_response", "request_id": "r999", "decision": "allow" }),
    );
    assert_eq!(stray["error"], "no approval is waiting as `r999`");
    let request_id = request["request_id"].clone();
    let (_, unknown) = client.ask(
        5,
        json!({ "type": "approval_response", "request_id": request_id, "decision": "maybe" }),
    );
    assert_eq!(unknown["success"], false);

    let (_, answered) = client.ask(
        6,
        json!({ "type": "approval_response", "request_id": request_id, "decision": "allow" }),
    );
    assert_eq!(answered["success"], true, "{answered}");
    let (after, idle) = client.until(|line| line["type"] == "idle");
    assert_eq!((&idle["agent"], &idle["stop"]), (&json!("lead"), &json!("stop")));
    let finished: Vec<&Value> = after.iter().filter(|line| line["type"] == "tool_end").collect();
    assert_eq!(finished.len(), 2, "{after:?}");
    assert!(finished.iter().all(|line| line["output"]["is_error"] == false));
    assert_eq!(
        std::fs::read_to_string(client.project().join("hello.txt")).unwrap(),
        "hello from azazel\n"
    );
    // The same answer cannot be given twice.
    let (_, again) = client.ask(
        7,
        json!({ "type": "approval_response", "request_id": request_id, "decision": "allow" }),
    );
    assert_eq!(again["success"], false);

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn questions_say_what_they_expect_and_an_approval_can_hold_for_the_session() {
    let mut client = Client::start("questions.json");
    let (_, accepted) = client.ask(1, json!({ "type": "prompt", "text": "paint" }));
    assert_eq!(accepted["success"], true);
    let result_of = |events: &[Value], call: &str| -> Value {
        events
            .iter()
            .find(|line| line["type"] == "tool_end" && line["call_id"] == call)
            .unwrap_or_else(|| panic!("no result of {call} in {events:?}"))["output"]
            .clone()
    };
    let asked = |line: &Value| line["type"] == "ui_request" && line["kind"] == "ask";

    // A choice that also takes a typed answer, with what the user needs to know.
    let (_, choice) = client.until(asked);
    assert_eq!(
        choice,
        json!({ "type": "ui_request", "request_id": choice["request_id"], "kind": "ask",
                "question": "Which colour?", "context": "Two paints are left.", "expects": "select",
                "options": ["red", "blue"], "multi": false, "allow_freeform": true })
    );
    client.ask(
        2,
        json!({ "type": "ui_response", "request_id": choice["request_id"], "value": "teal" }),
    );

    // Yes or no.
    let (before, confirm) = client.until(asked);
    let typed = result_of(&before, "c1");
    assert_eq!(typed["content"], json!([{ "type": "text", "text": "teal" }]));
    assert_eq!(typed["details"]["freeform"], true);
    assert_eq!(
        (&confirm["expects"], &confirm["options"], &confirm["question"]),
        (&json!("confirm"), &json!([]), &json!("Paint it?"))
    );
    assert!(confirm.get("allow_freeform").is_none() && confirm.get("context").is_none());
    client.ask(
        3,
        json!({ "type": "ui_response", "request_id": confirm["request_id"], "value": true }),
    );

    // Allowed for the session: the next write the rule covers is not asked about, and nothing
    // is saved for the project.
    let (before, approval) = client.until(|line| line["type"] == "approval_request");
    assert_eq!(
        result_of(&before, "c2")["content"],
        json!([{ "type": "text", "text": "yes" }])
    );
    assert_eq!(approval["rule"], "write(notes/**)");
    let (_, answered) = client.ask(
        4,
        json!({ "type": "approval_response", "request_id": approval["request_id"], "decision": "session" }),
    );
    assert_eq!(answered["success"], true, "{answered}");

    let (before, last) = client.until(asked);
    assert!(
        !kinds(&before).contains(&"approval_request"),
        "the second write was asked about: {before:?}"
    );
    assert_eq!(result_of(&before, "c4")["is_error"], false);
    assert_eq!(
        std::fs::read_to_string(client.project().join("notes/b.txt")).unwrap(),
        "dry\n"
    );
    assert!(!client.project().join(".azazel/config.local.toml").exists());
    assert_eq!(last["expects"], "text");

    // Dismissed: the agent is told so, and that is not a failed call.
    client.ask(
        5,
        json!({ "type": "ui_response", "request_id": last["request_id"], "value": null }),
    );
    let (after, _) = client.until(|line| line["type"] == "idle");
    let declined = result_of(&after, "c5");
    assert_eq!(
        (&declined["is_error"], &declined["content"][0]["text"]),
        (&json!(false), &json!("the user declined to answer"))
    );
    assert_eq!(declined["details"]["cancelled"], true);
    assert_eq!(client.finish(), Some(0));
}

#[test]
fn several_questions_are_one_form_and_an_async_answer_arrives_as_a_message() {
    // The session is not named: the name would be asked of the lead's model, whose last reply
    // is for the run the answer starts.
    let mut client = Client::start_with("forms.json", "[session]\nauto_name = false\n");
    let (_, accepted) = client.ask(1, json!({ "type": "prompt", "text": "set the store up" }));
    assert_eq!(accepted["success"], true);

    let (_, form) = client.until(|line| line["type"] == "ui_request");
    assert_eq!(form["kind"], "form");
    let questions = form["questions"].as_array().unwrap();
    assert_eq!(
        questions[0],
        json!({ "question": "Which store should the service use?", "header": "Store",
                "context": "The service keeps sessions between restarts.",
                "code": "CREATE TABLE sessions (id TEXT PRIMARY KEY, data BLOB);",
                "expects": "select", "options": ["sqlite", "postgres"], "multi": false,
                "recommended": "postgres" })
    );
    assert_eq!(
        questions[1],
        json!({ "question": "Anything else I should know?", "header": "Notes", "expects": "text",
                "options": [], "multi": false })
    );
    assert_eq!(
        (
            &questions[2]["expects"],
            questions[2]["diff"]
                .as_str()
                .map(|diff| diff.contains("+    disk();"))
        ),
        (&json!("confirm"), Some(true))
    );
    // One answer per question; the second is left open.
    client.ask(
        2,
        json!({ "type": "ui_response", "request_id": form["request_id"],
                "value": [["postgres"], null, true] }),
    );

    // The async question returns at once: its call ends before anyone has answered.
    let (before, later) = client.until(|line| line["type"] == "ui_request");
    let result_of = |events: &[Value], call: &str| -> Value {
        events
            .iter()
            .find(|line| line["type"] == "tool_end" && line["call_id"] == call)
            .unwrap_or_else(|| panic!("no result of {call} in {events:?}"))["output"]
            .clone()
    };
    assert_eq!(
        result_of(&before, "c1")["content"][0]["text"],
        "1. Which store should the service use?\npostgres\n\n2. Anything else I should know?\n(no \
         answer)\n\n3. Apply this change?\nyes"
    );
    assert_eq!(
        (&later["kind"], &later["expects"], &later["recommended"]),
        (&json!("ask"), &json!("confirm"), &json!("yes"))
    );
    let (meanwhile, _) = client.until(|line| line["type"] == "idle");
    let returned = result_of(&[before, meanwhile].concat(), "c2");
    assert_eq!(returned["is_error"], false);
    assert!(
        returned["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("The question is with the user."),
        "{returned}"
    );
    assert_eq!(returned["details"]["async"], true);

    // Answered while the lead is idle: the answer starts it again, as a message.
    client.ask(
        3,
        json!({ "type": "ui_response", "request_id": later["request_id"], "value": true }),
    );
    let (after, _) = client.until(|line| line["type"] == "idle");
    assert!(kinds(&after).contains(&"agent_start"), "{after:?}");
    let (_, messages) = client.ask(4, json!({ "type": "get_messages" }));
    let said: Vec<&str> = messages["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["message"]["content"][0]["text"].as_str())
        .collect();
    assert!(
        said.contains(&"<user-answer>\nShip it when the tests pass?\nyes\n</user-answer>"),
        "{said:?}"
    );
    assert_eq!(said.last(), Some(&"Shipping."));
    assert_eq!(client.finish(), Some(0));
}

#[test]
fn a_mention_starts_an_agent_or_messages_it_and_what_it_is_handed_is_told() {
    // Scouts answer from a queue of their own, which naming the session must not take from.
    let config = "[roles]\nsmol = [\"mock/scout\"]\n\n[session]\nauto_name = false\n";
    let mut client = Client::start_with("mention.json", config);
    let (_, neither) = client.ask(
        1,
        json!({ "type": "agent_mention", "name": "ghost", "text": "do it" }),
    );
    assert_eq!(
        (&neither["success"], &neither["data"]),
        (&json!(true), &Value::Null)
    );

    let (_, started) = client.ask(
        2,
        json!({ "type": "agent_mention", "name": "scout", "text": "where is the retry policy?" }),
    );
    assert_eq!(started["data"], json!({ "agent": "scout-1", "started": true }));
    // Its report goes to the lead, which answers it.
    let lead_idle = |line: &Value| line["type"] == "idle" && line["agent"] == "lead";
    let (first, _) = client.until(lead_idle);
    let handed = |lines: &[Value]| -> Vec<Value> {
        lines
            .iter()
            .filter(|line| line["type"] == "user_input")
            .cloned()
            .collect()
    };
    assert_eq!(
        handed(&first),
        [
            json!({ "type": "user_input", "kind": "prompt", "text": "where is the retry policy?",
                 "agent": "scout-1" })
        ]
    );

    // The id now names the agent: the text is a message to it.
    let (_, sent) = client.ask(
        3,
        json!({ "type": "agent_mention", "name": "scout-1", "text": "anything else?" }),
    );
    assert_eq!(sent["data"], json!({ "agent": "scout-1", "started": false }));
    let (second, _) = client.until(lead_idle);
    assert_eq!(handed(&second)[0]["text"], "anything else?");
    let said: Vec<&str> = second
        .iter()
        .filter(|line| line["type"] == "message_end" && line["agent"] == "scout-1")
        .filter_map(|line| line["message"]["content"][0]["text"].as_str())
        .collect();
    assert_eq!(said, ["It is also read in src/net.rs."]);
    assert_eq!(client.finish(), Some(0));
}

#[test]
fn commands_change_the_session_and_mistakes_are_answered() {
    let mut client = Client::start("write_read.json");

    let (_, mode) = client.ask(1, json!({ "type": "set_mode", "mode": "auto" }));
    assert_eq!(mode["data"], json!({ "mode": "auto" }));
    let (_, bad_mode) = client.ask(2, json!({ "type": "set_mode", "mode": "yolo" }));
    assert!(bad_mode["error"].as_str().unwrap().contains("yolo"), "{bad_mode}");
    let (_, model) = client.ask(
        3,
        json!({ "type": "set_model", "chain": ["mock/other", "@default"] }),
    );
    assert_eq!(model["data"]["models"], json!(["mock/other", "mock/lead"]));
    let (_, no_model) = client.ask(4, json!({ "type": "set_model", "chain": ["nowhere/m"] }));
    // A name of no known provider is looked up loosely and matches nothing.
    assert!(
        no_model["error"]
            .as_str()
            .unwrap()
            .starts_with("nowhere/m: no model"),
        "{no_model}"
    );
    let (_, state) = client.ask(5, json!({ "type": "get_state" }));
    assert_eq!(state["data"]["mode"], "auto");
    assert_eq!(state["data"]["chain"], json!(["mock/other", "@default"]));
    // Back to the model the script answers as.
    let (_, model) = client.ask(50, json!({ "type": "set_model", "chain": ["@default"] }));
    assert_eq!(model["data"]["models"], json!(["mock/lead"]));
    // The lead becomes another primary agent; one that is not is refused with the ones there are.
    let (_, agent) = client.ask(51, json!({ "type": "set_agent", "agent": "plan" }));
    assert_eq!(agent["data"], json!({ "agent": "plan" }));
    let (_, bad_agent) = client.ask(52, json!({ "type": "set_agent", "agent": "scout" }));
    assert!(
        bad_agent["error"]
            .as_str()
            .unwrap()
            .contains("the primary agents are: build, plan"),
        "{bad_agent}"
    );
    let (_, state) = client.ask(53, json!({ "type": "get_state" }));
    assert_eq!(state["data"]["agent"], "plan");
    let primaries: Vec<&str> = state["data"]["primary_agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent["name"].as_str().unwrap())
        .collect();
    assert_eq!(primaries, ["build", "plan"]);
    client.ask(54, json!({ "type": "set_agent", "agent": "build" }));

    for (id, command, error) in [
        (6, json!({ "type": "dance" }), "unknown command `dance`"),
        (7, json!({ "text": "hi" }), "the command has no `type`"),
        (8, json!({ "type": "prompt" }), "`text` must be a string"),
        (
            9,
            json!({ "type": "agent_stop", "agent": "ghost-1" }),
            "no agent named `ghost-1`",
        ),
        (
            10,
            json!({ "type": "ui_response", "request_id": "r1", "value": true }),
            "no request is waiting as `r1`",
        ),
    ] {
        let (_, response) = client.ask(id, command);
        assert_eq!(
            (&response["success"], &response["error"]),
            (&json!(false), &json!(error))
        );
    }
    client.send(json!([1, 2]));
    let (_, not_object) = client.until(|line| line["type"] == "response");
    assert_eq!(
        not_object,
        json!({ "id": null, "type": "response", "success": false, "error": "a command is a JSON object" })
    );
    client.stdin.as_mut().unwrap().write_all(b"\n{oops\n").unwrap();
    let (_, not_json) = client.until(|line| line["type"] == "response");
    assert!(
        not_json["error"].as_str().unwrap().starts_with("not JSON: "),
        "{not_json}"
    );

    // In `auto` nothing is asked: the prompt runs to the end on its own.
    let (_, accepted) = client.ask(11, json!({ "type": "prompt", "text": "create hello" }));
    assert_eq!(accepted["success"], true);
    let (events, _) = client.until(|line| line["type"] == "idle");
    assert!(!kinds(&events).contains(&"approval_request"), "{events:?}");
    assert!(client.project().join("hello.txt").is_file());

    assert_eq!(client.finish(), Some(0));
}
