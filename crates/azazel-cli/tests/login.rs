#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::Output;

use assert_cmd::Command;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const REPLY: &str = concat!(
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello from the gateway\"}}]}\n\n",
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    /// Runs `azazel` with this sandbox as its home and an empty project as its directory.
    async fn azazel(&self, args: &[&str]) -> Output {
        let project = self.dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.home())
            .env_remove("AZAZEL_MOCK_SCRIPT")
            .args(args)
            .arg("--cwd")
            .arg(project);
        tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_with_an_authorisation_server_is_signed_in_to_and_gets_the_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "dc-1",
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://id.example/device",
            "expires_in": 600,
            "interval": 0
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-1",
            "refresh_token": "rt-1",
            "expires_in": 3600
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer at-1"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(REPLY, "text/event-stream"))
        .mount(&server)
        .await;

    let sandbox = Sandbox {
        dir: tempfile::tempdir().unwrap(),
    };
    std::fs::create_dir_all(sandbox.home()).unwrap();
    let uri = server.uri();
    std::fs::write(
        sandbox.home().join("config.toml"),
        format!(
            "[providers.gateway]\napi = \"openai-chat\"\nbase_url = \"{uri}/v1\"\n\n\
             [providers.gateway.oauth]\nflow = \"device\"\nclient_id = \"app-1\"\n\
             device_url = \"{uri}/device\"\ntoken_url = \"{uri}/token\"\n"
        ),
    )
    .unwrap();
    let prompt = ["-p", "hi", "--model", "gateway/small"];

    // Not signed in yet: the request is not sent without a token.
    let before = sandbox.azazel(&prompt).await;
    assert_eq!(before.status.code(), Some(1));
    assert!(
        text(&before.stderr).contains("no credentials for gateway: run `azazel login gateway`"),
        "{}",
        text(&before.stderr)
    );

    let login = sandbox.azazel(&["login", "gateway"]).await;
    assert_eq!(login.status.code(), Some(0), "{}", text(&login.stderr));
    assert_eq!(
        text(&login.stderr),
        "Open https://id.example/device and enter the code WDJB-MJHT"
    );
    let auth_file = sandbox.home().join("auth.json");
    assert_eq!(
        text(&login.stdout),
        format!(
            "signed in to gateway; the login is kept in {}",
            auth_file.display()
        )
    );
    let stored: Value = serde_json::from_slice(&std::fs::read(&auth_file).unwrap()).unwrap();
    assert_eq!(stored["gateway"]["type"], "oauth");
    assert_eq!(stored["gateway"]["refresh"], "rt-1");

    let answered = sandbox.azazel(&prompt).await;
    assert_eq!(answered.status.code(), Some(0), "{}", text(&answered.stderr));
    assert_eq!(text(&answered.stdout), "hello from the gateway");

    let doctor = text(&sandbox.azazel(&["doctor"]).await.stdout);
    assert!(
        doctor.contains("provider gateway: credential from auth.json (login)"),
        "{doctor}"
    );

    let logout = sandbox.azazel(&["logout", "gateway"]).await;
    assert_eq!(text(&logout.stdout), "forgot the stored gateway login");
    assert_eq!(sandbox.azazel(&prompt).await.status.code(), Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sign_in_asks_its_inputs_and_its_login_keeps_the_address_requests_go_to() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/acme/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "dc-1",
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://id.example/device",
            "interval": 0
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/acme/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-acme", "expires_in": 3600
        })))
        .mount(&server)
        .await;
    // The provider's own address is unset; the login's is the tenant's.
    Mock::given(method("POST"))
        .and(path("/acme/v1/chat/completions"))
        .and(header("authorization", "Bearer at-acme"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(REPLY, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let sandbox = Sandbox {
        dir: tempfile::tempdir().unwrap(),
    };
    std::fs::create_dir_all(sandbox.home()).unwrap();
    let uri = server.uri();
    std::fs::write(
        sandbox.home().join("config.toml"),
        format!(
            "[providers.tenant]\napi = \"openai-chat\"\nbase_url = \"https://${{TENANT_HOST}}/v1\"\n\n\
             [providers.tenant.oauth]\nflow = \"device\"\nclient_id = \"app-1\"\n\
             device_url = \"{uri}/{{tenant}}/device\"\ntoken_url = \"{uri}/{{tenant}}/token\"\n\
             api_base_url = \"{uri}/{{tenant}}/v1\"\n\
             inputs = [{{ name = \"tenant\", prompt = \"Tenant:\", env = \"P2_TEST_TENANT\" }}]\n"
        ),
    )
    .unwrap();

    let mut login = Command::cargo_bin("azazel").unwrap();
    login
        .env("AZAZEL_HOME", sandbox.home())
        .env("P2_TEST_TENANT", "acme")
        .args(["login", "tenant"]);
    let login = tokio::task::spawn_blocking(move || login.output().unwrap())
        .await
        .unwrap();
    assert_eq!(login.status.code(), Some(0), "{}", text(&login.stderr));
    let stored: Value =
        serde_json::from_slice(&std::fs::read(sandbox.home().join("auth.json")).unwrap()).unwrap();
    assert_eq!(stored["tenant"]["tenant"], "acme");
    assert_eq!(stored["tenant"]["base_url"], format!("{uri}/acme/v1"));

    let answered = sandbox
        .azazel(&[
            "-p",
            "hi",
            "--model",
            "tenant/small",
            "--no-memory",
            "--set",
            "session.auto_name=false",
        ])
        .await;
    assert_eq!(answered.status.code(), Some(0), "{}", text(&answered.stderr));
    assert_eq!(text(&answered.stdout), "hello from the gateway");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_device_and_enterprise_flags_name_the_providers_they_work_with() {
    let sandbox = Sandbox {
        dir: tempfile::tempdir().unwrap(),
    };
    std::fs::create_dir_all(sandbox.home()).unwrap();
    let both = sandbox
        .azazel(&["login", "openai-codex", "--key", "--device"])
        .await;
    assert_eq!(both.status.code(), Some(2), "{}", text(&both.stderr));
    let wrong = sandbox.azazel(&["login", "groq", "--device"]).await;
    assert_eq!(wrong.status.code(), Some(1));
    assert!(
        text(&wrong.stderr).contains("groq has no device sign-in; --device works with openai-codex"),
        "{}",
        text(&wrong.stderr)
    );
    let enterprise = sandbox
        .azazel(&["login", "anthropic", "--enterprise", "ghe.example.com"])
        .await;
    assert!(
        text(&enterprise.stderr).contains("--enterprise works with github-copilot"),
        "{}",
        text(&enterprise.stderr)
    );
}
