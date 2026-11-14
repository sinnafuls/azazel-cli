//! The stall watchdog against servers that go silent: before the response begins and in the
//! middle of it, through the provider alone and through the router's retries.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use azazel_ai::providers::http::{HttpProvider, http_client};
use azazel_ai::{
    AiError, Credentials, EventStream, Message, ModelInfo, Provider, ProviderConfig, Request, StallLimits,
    StreamEvent, WireApi,
};
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FIRST: &str = "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n";
const REST: &str = concat!(
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n",
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// Long enough for a busy machine to answer on the loopback, short enough for a test.
const LIMIT: Duration = Duration::from_millis(1500);
/// Far longer than any limit here: a server that waits this long has stalled.
const SILENCE: Duration = Duration::from_secs(120);

fn limits() -> StallLimits {
    StallLimits {
        first_event: Some(LIMIT),
        stall: Some(LIMIT),
        total: None,
    }
}

fn provider(address: String, dir: &std::path::Path, limits: StallLimits) -> HttpProvider {
    let config = ProviderConfig {
        requires_key: false,
        ..ProviderConfig::new("acme", WireApi::OpenAiChat, address)
    };
    let credentials = Credentials::with_env(dir.join("auth.json"), Arc::new(|_| None));
    HttpProvider::new(config, http_client(), Arc::new(credentials)).with_stall(limits)
}

fn request() -> Request {
    Request {
        messages: vec![Message::user("hi")],
        ..Request::default()
    }
}

fn model() -> ModelInfo {
    ModelInfo::unknown("acme", "mock-model")
}

/// What a scripted server does with one connection after it has read the request.
#[derive(Clone)]
enum Step {
    Send(&'static str),
    Wait(Duration),
}

/// What a scripted server does when it is asked for its model list.
#[derive(Clone, Copy)]
enum Listing {
    /// Says at once that it has none.
    NotFound,
}

/// A server that answers the n-th request for a reply with the n-th script as an event stream,
/// closing the connection when the script ends. Returns its address and how many such requests
/// it has had.
async fn scripted(scripts: Vec<Vec<Step>>, listing: Listing) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let served = Arc::new(AtomicUsize::new(0));
    let count = served.clone();
    let scripts = Arc::new(scripts);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(serve(socket, scripts.clone(), count.clone(), listing));
        }
    });
    (address, served)
}

async fn serve(
    mut socket: TcpStream,
    scripts: Arc<Vec<Vec<Step>>>,
    count: Arc<AtomicUsize>,
    listing: Listing,
) {
    // The whole request: its head, then as many bytes as the head announces.
    let mut received = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let Ok(n @ 1..) = socket.read(&mut chunk).await else {
            return;
        };
        received.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&received);
        let Some(head_end) = text.find("\r\n\r\n") else {
            continue;
        };
        let length = text[..head_end]
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        if received.len() >= head_end + 4 + length {
            break;
        }
    }
    if received.starts_with(b"GET ") {
        match listing {
            Listing::NotFound => {
                let none = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                let _ = socket.write_all(none.as_bytes()).await;
            }
        }
        return;
    }
    let script = scripts
        .get(count.fetch_add(1, Ordering::SeqCst))
        .cloned()
        .unwrap_or_default();
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    for step in script {
        match step {
            Step::Send(text) => {
                if socket.write_all(text.as_bytes()).await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
            }
            Step::Wait(time) => tokio::time::sleep(time).await,
        }
    }
    let _ = socket.shutdown().await;
}

async fn collect(stream: EventStream) -> Vec<StreamEvent> {
    stream.collect().await
}

fn text_of(events: &[StreamEvent]) -> String {
    match events.last() {
        Some(StreamEvent::Done { message }) => message.text(),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_server_that_never_begins_its_answer_is_given_up_after_the_first_event_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(SILENCE)
                .set_body_raw(REST, "text/event-stream"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let short = StallLimits {
        first_event: Some(Duration::from_millis(300)),
        stall: None,
        total: None,
    };
    let provider = provider(server.uri(), dir.path(), short);

    let began = Instant::now();
    let Err(error) = provider
        .stream(&model(), &request(), CancellationToken::new())
        .await
    else {
        panic!("a response began");
    };
    assert_eq!(error, AiError::Stalled { secs: 1 });
    let waited = began.elapsed();
    assert!(
        waited >= Duration::from_millis(300) && waited < SILENCE / 4,
        "{waited:?}"
    );
}

#[tokio::test]
async fn headers_alone_are_not_an_answer_and_silence_in_mid_reply_ends_the_attempt() {
    // The first connection sends its headers and nothing else; the second stops after the
    // first fragment of the reply.
    let (address, _) = scripted(
        vec![
            vec![Step::Wait(SILENCE)],
            vec![Step::Send(FIRST), Step::Wait(SILENCE)],
        ],
        Listing::NotFound,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let provider = provider(address, dir.path(), limits());

    let began = Instant::now();
    let stream = provider
        .stream(&model(), &request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;
    assert!(began.elapsed() >= LIMIT, "{:?}", began.elapsed());
    assert_eq!(
        events,
        [
            StreamEvent::Start,
            StreamEvent::Error {
                error: AiError::Stalled { secs: 1 },
                partial: None,
            }
        ]
    );

    let stream = provider
        .stream(&model(), &request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;
    let said: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(said, "Hel");
    assert!(
        matches!(
            events.last(),
            Some(StreamEvent::Error {
                error: AiError::Stalled { secs: 1 },
                ..
            })
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn keep_alives_count_as_life_and_limits_that_are_off_wait() {
    // Comment lines every 200 ms for longer than the limit, then the reply.
    let mut alive = vec![Step::Send(FIRST)];
    for _ in 0..12 {
        alive.push(Step::Wait(Duration::from_millis(200)));
        alive.push(Step::Send(": keep-alive\n\n"));
    }
    alive.push(Step::Send(REST));
    // Silent for longer than the limit before and inside the reply.
    let slow = vec![
        Step::Wait(Duration::from_millis(600)),
        Step::Send(FIRST),
        Step::Wait(Duration::from_millis(600)),
        Step::Send(REST),
    ];
    let (address, _) = scripted(vec![alive, slow], Listing::NotFound).await;
    let dir = tempfile::tempdir().unwrap();

    let watched = provider(address.clone(), dir.path(), limits());
    let began = Instant::now();
    let stream = watched
        .stream(&model(), &request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(text_of(&collect(stream).await), "Hello");
    assert!(began.elapsed() > LIMIT, "{:?}", began.elapsed());

    // 0 seconds in the configuration: no limit of either kind.
    let unwatched = provider(address, dir.path(), StallLimits::from_secs(0, 0));
    let stream = unwatched
        .stream(&model(), &request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(text_of(&collect(stream).await), "Hello");
}
