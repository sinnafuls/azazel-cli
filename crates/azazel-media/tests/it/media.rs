use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, ToolOutput, ToolSource, UpdateSink};
use azazel_ai::{Auth, Block};
use azazel_core::config::MediaConfig;
use azazel_core::{CommandOutcome, ExtCtx, Registry};
use azazel_media::tools::GenerateTool;
use azazel_media::{Fal, GoogleImages, Media, MediaBackend, MediaExt, MediaKind, OpenAiImages};
use base64::Engine;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn png() -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(8, 4, image::Rgba([200, 30, 30, 255]));
    let mut bytes = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .unwrap();
    bytes
}

fn ctx(dir: &Path) -> ToolCtx {
    let agent = AgentInfo {
        id: "lead".into(),
        def: "lead".into(),
        parent: None,
        depth: 0,
        session_id: "s".into(),
    };
    ToolCtx {
        agent_id: agent.id.clone(),
        session_id: agent.session_id.clone(),
        agent,
        call_id: "c1".into(),
        cwd: dir.to_path_buf(),
        cancel: CancellationToken::new(),
        update: UpdateSink::default(),
        services: Arc::new(Services::default()),
    }
}

fn config(image: &str) -> MediaConfig {
    let mut config = MediaConfig::default();
    config.models.image = image.to_string();
    config
}

fn image_tool(dir: &Path, image_model: &str, backends: Vec<Arc<dyn MediaBackend>>) -> GenerateTool {
    let media = Media::new(&config(image_model), dir.to_path_buf(), backends);
    GenerateTool::new(Arc::new(media), MediaKind::Image)
}

fn fal(server: &MockServer) -> Arc<dyn MediaBackend> {
    Arc::new(Fal::new(server.uri(), "k", Duration::from_millis(10)))
}

/// Mounts a fal job at `/<model>` whose status goes through `statuses` and then stays at the last.
async fn fal_job(server: &MockServer, model: &str, statuses: &[Value]) {
    let uri = server.uri();
    Mock::given(method("POST"))
        .and(path(format!("/{model}")))
        .and(header("authorization", "Key k"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "request_id": "r1",
            "status_url": format!("{uri}/requests/r1/status"),
            "response_url": format!("{uri}/requests/r1"),
            "cancel_url": format!("{uri}/requests/r1/cancel"),
        })))
        .mount(server)
        .await;
    for (index, status) in statuses.iter().enumerate() {
        let mock = Mock::given(method("GET"))
            .and(path("/requests/r1/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(status));
        let last = index + 1 == statuses.len();
        if last {
            mock.mount(server).await
        } else {
            mock.up_to_n_times(1).mount(server).await
        }
    }
    Mock::given(method("GET"))
        .and(path("/requests/r1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "images": [{ "url": format!("{uri}/files/out.png"), "content_type": "image/png" }],
            "seed": 7,
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/out.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(png()))
        .mount(server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/requests/r1/cancel"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
}

fn saved_paths(output: &ToolOutput) -> Vec<PathBuf> {
    output
        .text_content()
        .lines()
        .filter_map(|line| line.split_once(" (").map(|(path, _)| PathBuf::from(path)))
        .filter(|path| path.is_absolute())
        .collect()
}

#[tokio::test]
async fn the_fal_queue_is_polled_until_the_file_can_be_fetched_and_saved_with_its_record() {
    let server = MockServer::start().await;
    let waiting = [
        json!({ "status": "IN_QUEUE" }),
        json!({ "status": "IN_PROGRESS" }),
        json!({ "status": "COMPLETED" }),
    ];
    fal_job(&server, "fal-ai/test-model", &waiting).await;
    let dir = tempfile::tempdir().unwrap();
    let tool = image_tool(dir.path(), "fal:fal-ai/test-model", vec![fal(&server)]);

    let output = tool
        .call(
            json!({ "prompt": "A red door, front view!", "size": "64x32" }),
            &ctx(dir.path()),
        )
        .await;
    assert!(!output.is_error, "{}", output.text_content());
    assert!(
        output
            .text_content()
            .starts_with("generated with fal:fal-ai/test-model:\n")
    );
    let paths = saved_paths(&output);
    assert_eq!(paths.len(), 1);
    let file = &paths[0];
    let name = file.file_name().unwrap().to_string_lossy().to_string();
    assert!(
        name.starts_with("a-red-door-front-view-") && name.ends_with("Z.png"),
        "{name}"
    );
    assert_eq!(file.parent().unwrap(), dir.path().join("assets/generated/image"));
    assert_eq!(std::fs::read(file).unwrap(), png());
    assert!(matches!(&output.content[1], Block::Image { mime, .. } if mime == "image/png"));

    let record: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{}.json", file.display())).unwrap()).unwrap();
    assert_eq!(record["prompt"], "A red door, front view!");
    assert_eq!(record["model"], "fal:fal-ai/test-model");
    assert_eq!(record["parameters"], json!({ "size": "64x32" }));
    assert_eq!(record["seed"], 7);

    let requests = server.received_requests().await.unwrap();
    let submitted: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(submitted["image_size"], json!({ "width": 64, "height": 32 }));
    let polls = requests
        .iter()
        .filter(|r| r.url.path() == "/requests/r1/status")
        .count();
    assert_eq!(polls, 3);
}

#[tokio::test]
async fn a_failed_job_reports_what_fal_said_and_aborting_stops_the_wait() {
    let server = MockServer::start().await;
    fal_job(
        &server,
        "bad",
        &[json!({ "status": "COMPLETED", "error": "content policy" })],
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let tool = image_tool(dir.path(), "fal:bad", vec![fal(&server)]);
    let failed = tool.call(json!({ "prompt": "x" }), &ctx(dir.path())).await;
    assert!(failed.is_error);
    assert_eq!(failed.text_content(), "fal job r1 failed: content policy");

    let slow = MockServer::start().await;
    fal_job(&slow, "slow", &[json!({ "status": "IN_QUEUE" })]).await;
    let tool = image_tool(dir.path(), "fal:slow", vec![fal(&slow)]);
    let context = ctx(dir.path());
    let cancel = context.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel.cancel();
    });
    let started = Instant::now();
    let aborted = tool.call(json!({ "prompt": "x" }), &context).await;
    assert!(
        aborted.is_error && aborted.text_content() == "aborted by user",
        "{}",
        aborted.text_content()
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!dir.path().join("assets").exists());
}

#[tokio::test]
async fn the_model_comes_from_the_call_then_the_configuration() {
    let server = MockServer::start().await;
    fal_job(&server, "from-call", &[json!({ "status": "COMPLETED" })]).await;
    let dir = tempfile::tempdir().unwrap();
    let context = ctx(dir.path());

    let configured = image_tool(dir.path(), "fal:from-config", vec![fal(&server)]);
    let output = configured
        .call(json!({ "prompt": "x", "model": "fal:from-call" }), &context)
        .await;
    assert!(!output.is_error, "{}", output.text_content());

    let unset = image_tool(dir.path(), "", vec![fal(&server)]);
    let none = unset.call(json!({ "prompt": "x" }), &context).await;
    assert_eq!(
        none.text_content(),
        "no image model configured: set [media.models] image = \"<backend>:<model id>\""
    );
    let unknown = unset
        .call(json!({ "prompt": "x", "model": "dalle:three" }), &context)
        .await;
    assert_eq!(
        unknown.text_content(),
        "media backend `dalle` is unknown or has no credentials; usable backends: fal"
    );
    let shapeless = unset
        .call(json!({ "prompt": "x", "model": "just-a-name" }), &context)
        .await;
    assert!(
        shapeless
            .text_content()
            .starts_with("`just-a-name` is not `<backend>:<model id>`")
    );
    let no_prompt = configured.call(json!({ "prompt": "  " }), &context).await;
    assert_eq!(no_prompt.text_content(), "`prompt` is required");
}

#[tokio::test]
async fn out_names_the_file_and_must_stay_inside_the_project() {
    let server = MockServer::start().await;
    fal_job(&server, "m", &[json!({ "status": "COMPLETED" })]).await;
    let dir = tempfile::tempdir().unwrap();
    let tool = image_tool(dir.path(), "fal:m", vec![fal(&server)]);
    let context = ctx(dir.path());

    let escaped = tool
        .call(json!({ "prompt": "x", "out": "../stolen.png" }), &context)
        .await;
    assert!(escaped.is_error && escaped.text_content().contains("inside the project"));
    // Nothing was asked of the service for a call that could not be saved.
    assert!(server.received_requests().await.unwrap().is_empty());

    let output = tool
        .call(json!({ "prompt": "x", "out": "art/door" }), &context)
        .await;
    assert!(!output.is_error, "{}", output.text_content());
    assert!(dir.path().join("art/door.png").is_file());
    assert!(dir.path().join("art/door.png.json").is_file());
}

#[tokio::test]
async fn openai_and_google_images_are_decoded_from_their_replies() {
    let encoded = base64::engine::general_purpose::STANDARD.encode(png());
    let dir = tempfile::tempdir().unwrap();
    let context = ctx(dir.path());

    let openai = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/images/generations"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [{ "b64_json": encoded }] })))
        .expect(1)
        .mount(&openai)
        .await;
    let backend: Arc<dyn MediaBackend> =
        Arc::new(OpenAiImages::new(openai.uri(), Auth::ApiKey("sk-test".into())));
    let tool = image_tool(dir.path(), "openai:image-model", vec![backend]);
    let output = tool
        .call(json!({ "prompt": "a blue kite", "out": "kite.png" }), &context)
        .await;
    assert!(!output.is_error, "{}", output.text_content());
    assert_eq!(std::fs::read(dir.path().join("kite.png")).unwrap(), png());
    let sent: Value = serde_json::from_slice(&openai.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(
        (&sent["model"], &sent["prompt"]),
        (&json!("image-model"), &json!("a blue kite"))
    );

    let google = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/models/image-model:generateContent"))
        .and(header("x-goog-api-key", "g-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{ "content": { "parts": [
                { "text": "Here you go." },
                { "inlineData": { "mimeType": "image/png", "data": encoded } }
            ] } }]
        })))
        .expect(1)
        .mount(&google)
        .await;
    let backend: Arc<dyn MediaBackend> =
        Arc::new(GoogleImages::new(google.uri(), Auth::ApiKey("g-test".into())));
    let tool = image_tool(dir.path(), "google:image-model", vec![backend]);
    let output = tool
        .call(json!({ "prompt": "a green kite", "out": "green.png" }), &context)
        .await;
    assert!(!output.is_error, "{}", output.text_content());
    assert_eq!(std::fs::read(dir.path().join("green.png")).unwrap(), png());

    // Image backends are told apart from the ones that make other media.
    let media = Media::new(&config("google:image-model"), dir.path().to_path_buf(), vec![]);
    let audio = GenerateTool::new(Arc::new(media), MediaKind::Audio);
    let refused = audio
        .call(json!({ "prompt": "rain", "kind": "noise" }), &context)
        .await;
    assert_eq!(
        refused.text_content(),
        "`kind` must be `sfx`, `music` or `speech`"
    );
}

#[tokio::test]
async fn generation_tools_are_offered_only_when_a_backend_can_be_used() {
    let dir = tempfile::tempdir().unwrap();
    let names = |backends: Vec<Arc<dyn MediaBackend>>| {
        let services = Arc::new(Services::default());
        let registry = Registry::new(ExtCtx {
            services,
            cwd: dir.path().to_path_buf(),
            session_id: "s".into(),
        });
        let media = Arc::new(Media::new(&config(""), dir.path().to_path_buf(), backends));
        registry.load(&MediaExt { media }).unwrap();
        let tools: Vec<String> = registry
            .tools
            .tools(&ctx(dir.path()).agent)
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        (tools, registry)
    };

    let (without, registry) = names(vec![]);
    assert_eq!(without, ["sprite_sheet"]);
    let command = registry.command("media").unwrap();
    let shown = command.handler.run("", &registry.bus.ctx()).await;
    let CommandOutcome::Show(report) = shown else {
        panic!("{shown:?}")
    };
    assert!(report.starts_with("backends: none has credentials"), "{report}");
    assert!(
        report.contains("image: not configured\n") && report.contains("output directory: assets/generated\n")
    );

    let server = MockServer::start().await;
    let (with, _) = names(vec![fal(&server)]);
    assert_eq!(
        with,
        [
            "generate_image",
            "generate_video",
            "generate_3d",
            "generate_audio",
            "sprite_sheet"
        ]
    );
}
