#![allow(clippy::unwrap_used)]

//! `azazel secrets`, and an agent using what it stored: the `http` tool sends the credential of
//! the host and the agent never gets it.

use std::path::PathBuf;
use std::process::Output;

use assert_cmd::Command;
use serde_json::{Value, json};
use wiremock::matchers::{bearer_token, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "tok_live_12345678";

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        Sandbox { dir }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn auth(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.home().join("auth.json")).unwrap()).unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.home())
            .env_remove("AZAZEL_MOCK_SCRIPT")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .arg("--cwd")
            .arg(self.project());
        command
    }

    /// `azazel secrets <args>`, with `typed` on standard input.
    fn secrets(&self, args: &[&str], typed: &str) -> Output {
        self.command()
            .arg("secrets")
            .args(args)
            .write_stdin(typed)
            .output()
            .unwrap()
    }

    /// `azazel -p go` answering from `script`; returns the output and the tool results the lead
    /// had been given by its last request.
    fn run(&self, script: &Value, args: &[&str]) -> (Output, Vec<String>) {
        let script_path = self.dir.path().join("script.json");
        let requests = self.dir.path().join("requests.jsonl");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let _ = std::fs::remove_file(&requests);
        let output = self
            .command()
            .env("AZAZEL_MOCK_SCRIPT", &script_path)
            .env("AZAZEL_MOCK_REQUESTS", &requests)
            .args(["--set", "memory.auto_extract=false"])
            .args(["--set", "memory.auto_index=false"])
            .args(["--set", "session.auto_name=false"])
            .args(args)
            .args(["-p", "go"])
            .write_stdin("")
            .output()
            .unwrap();
        let recorded = std::fs::read_to_string(&requests).unwrap_or_default();
        let last: Value = recorded
            .lines()
            .last()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
            .unwrap_or_default();
        let mut results = Vec::new();
        for message in last["messages"].as_array().into_iter().flatten() {
            for block in message["content"].as_array().into_iter().flatten() {
                if block["type"] == "tool_result" {
                    let text: Vec<&str> = block["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|inner| inner["text"].as_str())
                        .collect();
                    results.push(text.join("\n"));
                }
            }
        }
        (output, results)
    }
}

fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

#[test]
fn credentials_are_stored_listed_without_their_secret_and_forgotten() {
    let sandbox = Sandbox::new();
    // A login that is there already stays as it is.
    let account = json!({"acme": {"type": "api_key", "key": "provider-key"}});
    std::fs::write(sandbox.home().join("auth.json"), account.to_string()).unwrap();

    let empty = sandbox.secrets(&["list"], "");
    assert!(
        out(&empty).starts_with("nothing is stored for agents"),
        "{}",
        out(&empty)
    );

    let header = format!("Authorization: Bearer {TOKEN}");
    let added = sandbox.secrets(
        &["add", "github", "--host", "API.github.com", "--header", &header],
        "",
    );
    assert!(added.status.success(), "{}", err(&added));
    assert_eq!(
        out(&added),
        "stored `github`: api.github.com · header Authorization"
    );
    // `-` takes the secret from the terminal, or from a pipe, instead of the command line.
    let piped = sandbox.secrets(
        &["add", "shop", "--host", "localhost:8080", "--basic", "-"],
        "me:hunter2-pass\n",
    );
    assert!(piped.status.success(), "{}", err(&piped));
    assert_eq!(
        out(&piped),
        "stored `shop`: localhost:8080 · basic authentication as me"
    );
    let query = sandbox.secrets(
        &["add", "maps", "--host", "*.maps.test", "--query", "key=k-123456"],
        "",
    );
    assert_eq!(out(&query), "stored `maps`: *.maps.test · query parameter key");
    let site = sandbox.secrets(
        &[
            "add-site",
            "https://Example.com/login*",
            "--username",
            "me@example.com",
        ],
        "site-password-1\n",
    );
    assert!(site.status.success(), "{}", err(&site));
    assert_eq!(
        out(&site),
        "stored `example.com`: https://example.com/login* · login as me@example.com"
    );

    assert_eq!(
        sandbox.auth(),
        json!({
            "acme": {"type": "api_key", "key": "provider-key"},
            "services": {
                "example.com": {
                    "type": "site", "url": "https://example.com/login*",
                    "username": "me@example.com", "password": "site-password-1"
                },
                "github": {
                    "type": "header", "host": "api.github.com",
                    "name": "Authorization", "value": format!("Bearer {TOKEN}")
                },
                "maps": {"type": "query", "host": "*.maps.test", "name": "key", "value": "k-123456"},
                "shop": {"type": "basic", "host": "localhost:8080", "username": "me", "password": "hunter2-pass"},
            }
        })
    );

    let listed = out(&sandbox.secrets(&["list"], ""));
    assert_eq!(
        listed.lines().collect::<Vec<_>>(),
        [
            "example.com  https://example.com/login* · login as me@example.com",
            "github       api.github.com · header Authorization",
            "maps         *.maps.test · query parameter key",
            "shop         localhost:8080 · basic authentication as me",
        ]
    );
    let json_lines = out(&sandbox.secrets(&["list", "--json"], ""));
    let entries: Vec<Value> = json_lines
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        entries[1],
        json!({"name": "github", "type": "header", "host": "api.github.com", "header": "Authorization"})
    );
    for secret in [TOKEN, "hunter2-pass", "k-123456", "site-password-1"] {
        assert!(
            !listed.contains(secret) && !json_lines.contains(secret),
            "{secret}"
        );
    }
    // The providers' side of the file does not take a service for an account.
    let status = sandbox.command().args(["auth", "status"]).output().unwrap();
    assert!(status.status.success(), "{}", err(&status));
    assert!(!out(&status).contains("services"), "{}", out(&status));

    // A name in use is replaced, and says so.
    let rotated = sandbox.secrets(
        &["add", "maps", "--host", "maps.test", "--query", "key=k-654321"],
        "",
    );
    assert_eq!(
        out(&rotated),
        "stored `maps`: maps.test · query parameter key (in place of the one stored before)"
    );

    let forgotten = sandbox.secrets(&["rm", "github"], "");
    assert_eq!(
        out(&forgotten),
        "forgot `github`: api.github.com · header Authorization"
    );
    let again = sandbox.secrets(&["rm", "github"], "");
    assert_eq!(again.status.code(), Some(1));
    assert_eq!(err(&again), "azazel: nothing is stored as `github`");
    assert_eq!(sandbox.auth()["services"].as_object().unwrap().len(), 3);
    assert_eq!(sandbox.auth()["acme"]["key"], "provider-key");
}

#[test]
fn what_cannot_be_stored_is_refused_with_the_reason() {
    let sandbox = Sandbox::new();
    for (args, why) in [
        (
            vec!["add", "x", "--host", "api.test"],
            "give one of --header, --query and --basic",
        ),
        (
            vec![
                "add", "x", "--host", "api.test", "--header", "A: bcdef", "--query", "a=bcdef",
            ],
            "give one of --header, --query and --basic",
        ),
        (
            vec![
                "add",
                "x",
                "--host",
                "https://api.test/v1",
                "--header",
                "A: bcdef",
            ],
            "`https://api.test/v1` is not a host",
        ),
        (
            vec!["add", "x", "--host", "api.test", "--header", "no separator"],
            "write it as `Name: value`",
        ),
        (
            vec!["add", "none", "--host", "api.test", "--header", "A: bcdef"],
            "choose another name",
        ),
        (
            vec![
                "add-site",
                "http://example.com/login",
                "--username",
                "me",
                "--password",
                "pw-123456",
            ],
            "only typed into an https page",
        ),
        (
            vec![
                "add-site",
                "example.com",
                "--username",
                "me",
                "--password",
                "pw-123456",
            ],
            "is not the address of a page",
        ),
    ] {
        let refused = sandbox.secrets(&args, "");
        assert_eq!(refused.status.code(), Some(2), "{args:?}: {}", err(&refused));
        assert!(err(&refused).contains(why), "{args:?}: {}", err(&refused));
    }
    assert!(!sandbox.home().join("auth.json").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_calls_a_service_with_the_stored_credential_and_never_holds_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/me"))
        .and(bearer_token(TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(r#"{{"login":"me","auth":"Bearer {TOKEN}"}}"#),
            "application/json",
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_raw("no credential", "text/plain"))
        .mount(&server)
        .await;
    let sandbox = Sandbox::new();
    let host = server.uri().strip_prefix("http://").unwrap().to_string();
    let header = format!("Authorization: Bearer {TOKEN}");
    let added = sandbox.secrets(&["add", "acme", "--host", &host, "--header", &header], "");
    assert!(added.status.success(), "{}", err(&added));

    let call = json!({"url": format!("{}/v1/me", server.uri())});
    let script = json!({ "lead": [
        { "reply": { "tool_calls": [{ "id": "t1", "name": "http", "input": call }] } },
        { "reply": { "text": "done" } },
    ] });
    let (auto, sandbox, script) = tokio::task::spawn_blocking(move || {
        let auto = sandbox.run(&script, &["--mode", "auto"]);
        (auto, sandbox, script)
    })
    .await
    .unwrap();
    let (output, results) = auto;
    assert!(output.status.success(), "{}", err(&output));
    let [result] = results.as_slice() else {
        panic!("{results:?}");
    };
    assert!(
        result.starts_with(&format!(
            "HTTP 200 OK · GET {}/v1/me · service acme\n",
            server.uri()
        )),
        "{result}"
    );
    assert!(
        result.ends_with(r#"{"login":"me","auth":"[redacted:credential]"}"#),
        "{result}"
    );
    // Neither the model nor the session file got the token.
    assert!(!result.contains(TOKEN), "{result}");
    let (mut stack, mut read) = (vec![sandbox.home()], 0);
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten() {
            let entry = entry.unwrap().path();
            if entry.is_dir() {
                stack.push(entry);
            } else if entry.file_name().unwrap() != "auth.json"
                && let Ok(text) = std::fs::read_to_string(&entry)
            {
                read += 1;
                assert!(!text.contains(TOKEN), "{}", entry.display());
            }
        }
    }
    assert!(
        read > 0,
        "the session wrote nothing under {}",
        sandbox.home().display()
    );

    // Allowing the host is not allowing the credential: with nobody to ask, the call with it is
    // refused, and the same request goes through once the service is allowed too.
    let domain = "permissions.allow=[\"http(domain:127.0.0.1)\"]";
    let both = "permissions.allow=[\"http(domain:127.0.0.1)\",\"http(auth:acme)\"]";
    let (host_only, with_service) = tokio::task::spawn_blocking(move || {
        let host_only = sandbox.run(&script, &["--set", domain]);
        let with_service = sandbox.run(&script, &["--set", both]);
        (host_only, with_service)
    })
    .await
    .unwrap();
    assert!(
        host_only.1[0].starts_with("permission required for http"),
        "{:?}",
        host_only.1
    );
    assert!(
        with_service.1[0].starts_with("HTTP 200 OK"),
        "{:?}",
        with_service.1
    );
    let carried = server.received_requests().await.unwrap();
    assert_eq!(carried.len(), 2);
}
