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

    /// Reads up to the first line `wanted` accepts; returns the lines before it and that line.
    /// Every line must be JSON: nothing else may appear on standard output.
    fn until(&self, wanted: impl Fn(&Value) -> bool) -> (Vec<Value>, Value) {
        let mut before = Vec::new();
        loop {
            // A line that never comes fails with what did come, so that a rare hang can be read.
            let Ok(line) = self.lines.recv_timeout(Duration::from_secs(60)) else {
                panic!("no awaited protocol line within a minute; the lines before it: {before:#?}");
            };
            let line: Value =
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("not a protocol line ({e}): {line}"));
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
            .contains("the primary agents are: build, lean, plan"),
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
    assert_eq!(primaries, ["build", "lean", "plan"]);
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

#[test]
fn set_profile_switches_the_roles_and_says_so() {
    let config = "[profiles.mine.roles]
default = [\"mock/lead\"]
smol = [\"mock/other\"]

                  [profiles.needy.roles]
default = [\"anthropic/claude-opus-4-5\"]
";
    let mut client = Client::start_with("write_read.json", config);

    let (_, state) = client.ask(1, json!({ "type": "get_state" }));
    assert_eq!(state["data"]["profile"], Value::Null);
    let listed: Vec<(&str, &str, bool)> = state["data"]["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["name"].as_str().unwrap(),
                p["source"].as_str().unwrap(),
                p["usable"].as_bool().unwrap(),
            )
        })
        .collect();
    assert!(listed.contains(&("mine", "user", true)), "{listed:?}");
    assert!(listed.contains(&("needy", "user", false)), "{listed:?}");
    assert!(
        listed.iter().any(|(_, source, _)| *source == "builtin"),
        "{listed:?}"
    );

    let (events, response) = client.ask(2, json!({ "type": "set_profile", "name": "mine" }));
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(response["data"]["profile"], "mine");
    assert_eq!(response["data"]["roles"]["smol"], "mock/other");
    let frame = events
        .iter()
        .find(|line| line["type"] == "profile_changed")
        .unwrap();
    assert_eq!((&frame["to"], &frame["reason"]), (&json!("mine"), &json!("user")));
    let (_, state) = client.ask(3, json!({ "type": "get_state" }));
    assert_eq!(state["data"]["profile"], "mine");

    // A profile that is not there, or whose provider has no login, changes nothing.
    let (events, unknown) = client.ask(4, json!({ "type": "set_profile", "name": "nowhere" }));
    assert!(
        unknown["error"]
            .as_str()
            .unwrap()
            .contains("no profile `nowhere`"),
        "{unknown}"
    );
    let (more, needy) = client.ask(5, json!({ "type": "set_profile", "name": "needy" }));
    assert!(
        needy["error"]
            .as_str()
            .unwrap()
            .contains("azazel login anthropic"),
        "{needy}"
    );
    assert!(
        !kinds(&events).contains(&"profile_changed") && !kinds(&more).contains(&"profile_changed"),
        "{events:?} {more:?}"
    );
    let (_, state) = client.ask(6, json!({ "type": "get_state" }));
    assert_eq!(state["data"]["profile"], "mine");

    // The old command still works and moves the session too.
    let (events, old) = client.ask(7, json!({ "type": "apply_preset", "name": "mine" }));
    assert_eq!(old["success"], true, "{old}");
    assert!(kinds(&events).contains(&"profile_changed"), "{events:?}");

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn settings_are_listed_and_changed_in_the_scope_asked_for() {
    let mut client = Client::start("write_read.json");

    let (_, list) = client.ask(1, json!({ "type": "get_settings" }));
    let rows = list["data"]["settings"].as_array().unwrap();
    let find = |path: &str| {
        rows.iter()
            .find(|row| row["path"] == path)
            .unwrap_or_else(|| panic!("{path}"))
    };
    let theme = find("tui.theme");
    assert_eq!(theme["kind"], "theme");
    assert_eq!(theme["live"], true);
    assert!(
        theme["choices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "nord"),
        "{theme}"
    );
    assert_eq!(theme["default"], theme["value"]);
    assert!(theme.get("source").is_none(), "{theme}");
    assert_eq!(find("permissions.mode")["gated"], true);
    assert_eq!(find("remote.full_access")["value"], false);
    assert_eq!(list["data"]["project"], false);
    assert!(
        list["data"]["pages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|page| page == "Appearance")
    );

    // A live setting changed for this session: the configuration is untouched, a `theme`
    // frame follows and the page's colours are the new theme's.
    let (before, changed) = client.ask(
        2,
        json!({ "type": "set_setting", "path": "tui.theme", "value": "light", "scope": "session" }),
    );
    assert_eq!(changed["success"], true, "{changed}");
    assert_eq!(changed["data"]["setting"]["value"], "light");
    assert_eq!(changed["data"]["setting"]["source"], "session");
    // Kept for the session only: the user's config is as it was.
    assert_eq!(
        std::fs::read_to_string(client.dir.path().join("home/config.toml")).unwrap(),
        ""
    );
    let frame = before
        .iter()
        .find(|line| line["type"] == "theme")
        .unwrap_or_else(|| panic!("no theme frame in {before:?}"));
    assert_eq!(frame["name"], "light");
    assert_eq!(frame["light"], true);
    let (_, theme) = client.ask(3, json!({ "type": "get_theme" }));
    assert_eq!(theme["data"]["name"], "light");
    assert_eq!(theme["data"]["roles"], frame["roles"]);
    assert_eq!(theme["data"]["roles"].as_object().unwrap().len(), 23);
    assert!(
        theme["data"]["roles"]["accent"]
            .as_str()
            .unwrap()
            .starts_with('#')
    );
    let (_, dark) = client.ask(4, json!({ "type": "get_theme", "light": false }));
    assert_eq!(dark["data"]["name"], "light");

    // A setting that applies to the next session cannot be kept for this one.
    let (_, later) = client.ask(
        5,
        json!({ "type": "set_setting", "path": "cache.report_misses", "value": true, "scope": "session" }),
    );
    assert!(
        later["error"].as_str().unwrap().contains("next session"),
        "{later}"
    );
    // Only the theme has an effect for a session without a terminal, and a theme is a name:
    // a path is never read.
    let (_, kept) = client.ask(
        60,
        json!({ "type": "set_setting", "path": "tui.indicator", "value": "kitt", "scope": "session" }),
    );
    assert_eq!(kept["success"], false, "{kept}");
    assert!(
        kept["error"].as_str().unwrap().contains("only the theme"),
        "{kept}"
    );
    for bad in ["/dev/zero", r"\\host\share\t", "../x", r"C:\t.toml", "nope"] {
        let (_, path) = client.ask(
            61,
            json!({ "type": "set_setting", "path": "tui.theme", "value": bad, "scope": "session" }),
        );
        assert_eq!(path["success"], false, "{bad}: {path}");
        assert!(
            path["error"].as_str().unwrap().contains("no theme named"),
            "{path}"
        );
    }
    let (_, auto) = client.ask(
        62,
        json!({ "type": "set_setting", "path": "tui.theme", "value": "auto", "scope": "session" }),
    );
    assert_eq!(auto["success"], true, "{auto}");
    let (_, unknown) = client.ask(
        6,
        json!({ "type": "set_setting", "path": "no.such", "value": 1, "scope": "home" }),
    );
    assert!(
        unknown["error"].as_str().unwrap().contains("not a setting"),
        "{unknown}"
    );
    let (_, invalid) = client.ask(
        7,
        json!({ "type": "set_setting", "path": "editor.mode", "value": "emacs-pro", "scope": "home" }),
    );
    assert_eq!(invalid["success"], false, "{invalid}");

    // The home scope writes the user's config and reads back with its layer.
    let (_, saved) = client.ask(
        8,
        json!({ "type": "set_setting", "path": "tui.todo_max", "value": 7, "scope": "home" }),
    );
    assert_eq!(saved["success"], true, "{saved}");
    assert_eq!(saved["data"]["setting"]["value"], 7);
    assert_eq!(saved["data"]["setting"]["source"], "home");
    let text = std::fs::read_to_string(client.dir.path().join("home/config.toml")).unwrap();
    assert!(text.contains("todo_max = 7"), "{text}");
    let (_, list) = client.ask(9, json!({ "type": "get_settings" }));
    let row = list["data"]["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["path"] == "tui.todo_max")
        .unwrap()
        .clone();
    assert_eq!((&row["value"], &row["source"]), (&json!(7), &json!("home")));
    // `null` takes it out of the file again.
    let (_, reset) = client.ask(
        10,
        json!({ "type": "set_setting", "path": "tui.todo_max", "value": null, "scope": "home" }),
    );
    assert_eq!(reset["success"], true, "{reset}");
    assert!(
        !std::fs::read_to_string(client.dir.path().join("home/config.toml"))
            .unwrap()
            .contains("todo_max")
    );

    // The project's config is written only in a trusted project.
    let (_, project) = client.ask(
        11,
        json!({ "type": "set_setting", "path": "tui.todo_max", "value": 3, "scope": "project" }),
    );
    assert!(
        project["error"].as_str().unwrap().contains("trusted"),
        "{project}"
    );
    assert!(!client.project().join(".azazel/config.toml").exists());
    let (_, refused) = client.ask(
        12,
        json!({ "type": "set_setting", "path": "permissions.mode", "value": "bypass", "scope": "project" }),
    );
    assert!(
        refused["error"].as_str().unwrap().contains("trusted"),
        "{refused}"
    );
    assert!(!client.project().join(".azazel/config.toml").exists());

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn the_reports_are_data() {
    let mut client = Client::start("write_read.json");

    let (_, context) = client.ask(1, json!({ "type": "get_context" }));
    assert_eq!(context["data"], Value::Null, "{context}");
    let (_, cost) = client.ask(2, json!({ "type": "get_cost" }));
    assert_eq!(cost["data"]["requests"], 0);
    assert_eq!(cost["data"]["usage"]["input"], 0);
    assert_eq!(cost["data"]["models"], json!([]));
    assert_eq!(cost["data"]["side"], json!([]));

    client.ask(3, json!({ "type": "set_mode", "mode": "auto" }));
    client.ask(4, json!({ "type": "prompt", "text": "create hello" }));
    client.until(|line| line["type"] == "idle");
    let (_, context) = client.ask(5, json!({ "type": "get_context" }));
    let report = &context["data"];
    assert_eq!(report["agent"], "lead");
    assert!(report["request"].as_u64().unwrap() >= 1);
    assert!(report["tokens"].as_u64().unwrap() > 0, "{report}");
    let categories = report["categories"].as_array().unwrap();
    assert!(
        categories
            .iter()
            .any(|c| c["kind"] == "tools" && c["label"] == "tool schemas"),
        "{report}"
    );
    assert_eq!(
        categories
            .iter()
            .map(|c| c["tokens"].as_u64().unwrap())
            .sum::<u64>(),
        report["tokens"].as_u64().unwrap()
    );
    let (_, cost) = client.ask(6, json!({ "type": "get_cost" }));
    assert!(cost["data"]["requests"].as_u64().unwrap() >= 1, "{cost}");

    assert_eq!(client.finish(), Some(0));
}

#[test]
fn trust_directories_and_approvals_are_changed_like_the_slash_commands() {
    let mut client = Client::start("write_read.json");

    let (_, trust) = client.ask(1, json!({ "type": "trust_project", "trusted": false }));
    assert_eq!(trust["data"]["trusted"], false, "{trust}");
    assert_eq!(trust["data"]["saved"], false);
    let (_, trust) = client.ask(2, json!({ "type": "trust_project" }));
    assert_eq!(trust["data"]["saved"], true);
    // Not in effect before the next start.
    assert_eq!(trust["data"]["trusted"], false);
    assert!(trust["data"]["message"].as_str().unwrap().contains("next start"));
    let trust_file = std::fs::read_to_string(client.dir.path().join("home/trust.json")).unwrap_or_default();
    assert!(!trust_file.is_empty());

    let other = client.dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let (_, added) = client.ask(3, json!({ "type": "add_dir", "path": other.to_str().unwrap() }));
    let dirs: Vec<&str> = added["data"]["dirs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap())
        .collect();
    assert!(dirs.iter().any(|dir| dir.ends_with("other")), "{added}");
    let (_, missing) = client.ask(4, json!({ "type": "add_dir", "path": "no-such-dir" }));
    assert!(
        missing["error"].as_str().unwrap().contains("not a directory"),
        "{missing}"
    );

    let (_, forgot) = client.ask(5, json!({ "type": "forget_approvals" }));
    assert_eq!(forgot["data"]["count"], 0, "{forgot}");

    assert_eq!(client.finish(), Some(0));
}

#[cfg(feature = "browser")]
#[test]
fn the_browser_view_can_be_taken_and_handed_back_and_input_needs_the_hold_and_a_page() {
    let mut client = Client::start("write_read.json");

    let (_, watching) = client.ask(1, json!({ "type": "browser_watch", "on": true }));
    assert_eq!(watching["success"], true, "{watching}");
    assert_eq!(watching["data"]["running"], false, "{watching}");
    assert_eq!(watching["data"]["held_by_you"], false, "{watching}");

    let click = json!({ "type": "browser_input", "input": { "kind": "click", "x": 0.5, "y": 0.5 } });
    let (_, refused) = client.ask(2, click.clone());
    assert_eq!(refused["error"], "take the browser over first", "{refused}");

    // The agent has not opened a page, so there is nothing to take over.
    let (_, nothing) = client.ask(3, json!({ "type": "browser_take", "on": true }));
    assert_eq!(nothing["success"], false, "{nothing}");
    assert_eq!(
        nothing["error"], "no browser is running; the agent has not opened a page yet",
        "{nothing}"
    );
    let (_, no_page) = client.ask(4, click);
    assert_eq!(no_page["error"], "take the browser over first", "{no_page}");
    let (_, off_screen) = client.ask(
        5,
        json!({ "type": "browser_input", "input": { "kind": "click", "x": 2.0, "y": 0.5 } }),
    );
    assert!(
        off_screen["error"].as_str().unwrap().contains("between 0 and 1"),
        "{off_screen}"
    );

    let (_, back) = client.ask(6, json!({ "type": "browser_take", "on": false }));
    assert_eq!(back["data"]["held"], false, "{back}");
    assert_eq!(client.finish(), Some(0));
}

#[cfg(not(feature = "browser"))]
#[test]
fn without_the_browser_feature_the_view_commands_say_so() {
    let mut client = Client::start("write_read.json");
    for (id, command) in [
        json!({ "type": "browser_watch", "on": true }),
        json!({ "type": "browser_take", "on": true }),
        json!({ "type": "browser_sign_in" }),
        json!({ "type": "browser_open" }),
    ]
    .into_iter()
    .enumerate()
    {
        let (_, answer) = client.ask(id as u64 + 1, command);
        assert_eq!(
            answer["error"], "the browser is not part of this build",
            "{answer}"
        );
    }
    assert_eq!(client.finish(), Some(0));
}

#[cfg(feature = "browser")]
#[test]
fn browser_open_refuses_an_address_that_is_not_http() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(assert_cmd::cargo::cargo_bin("azazel"))
        .env("AZAZEL_HOME", dir.path())
        .args(["browser", "open", "file:///etc/passwd"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not an http or https URL"));
}

#[cfg(feature = "lua")]
#[test]
fn a_background_script_asks_with_its_job_and_runs_on_once_approved() {
    // The session is not named: the name would be asked of the lead's model and take the reply
    // of the run the script's wake starts.
    let mut client = Client::start_with("script_approval.json", "[session]\nauto_name = false\n");
    client.ask(
        1,
        json!({ "type": "prompt", "text": "write out.txt in the background" }),
    );

    // The script pauses at the write: the request names the job, and nothing was written.
    let (before, request) = client.until(|line| line["type"] == "approval_request");
    assert_eq!(request["tool"], "write", "{request}");
    assert_eq!(request["job"], "lua-1", "{request}");
    assert!(
        request["note"]
            .as_str()
            .unwrap_or_default()
            .contains("runs the script again"),
        "{request}"
    );
    assert!(!client.project().join("out.txt").exists());

    // The lead's first run ends before the answer, so the script's wake starts a run of its own:
    // always, not only when the answer happens to come late.
    let idle = |line: &Value| line["type"] == "idle" && line["agent"] == "lead";
    if !before.iter().any(idle) {
        client.until(idle);
    }

    let request_id = request["request_id"].clone();
    let (_, answered) = client.ask(
        2,
        json!({ "type": "approval_response", "request_id": request_id, "decision": "allow" }),
    );
    assert_eq!(answered["success"], true, "{answered}");

    // The script ran again, made the write once approved, and woke the lead, who ends the run.
    let (_, idle) = client.until(|line| {
        assert_ne!(line["type"], "failed", "the lead's run failed: {line}");
        line["type"] == "idle" && line["stop"] == "stop"
    });
    assert_eq!(idle["agent"], "lead");
    for _ in 0..100 {
        if client.project().join("out.txt").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        std::fs::read_to_string(client.project().join("out.txt")).unwrap(),
        "hi"
    );
    assert_eq!(client.finish(), Some(0));
}
