//! Streams one reply: `cargo run -p azazel-ai --example chat -- --model provider/id "prompt"`.
//! Credentials come from the provider's environment variable; local servers need none.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use azazel_ai::providers::http::{HttpProvider, http_client};
use azazel_ai::providers::presets::presets;
use azazel_ai::{
    Catalog, Credentials, Message, ModelSpec, Provider, Request, RetryPolicy, Router, StreamEvent,
};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> ExitCode {
    let mut chain: Vec<ModelSpec> = Vec::new();
    let mut prompt = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--model" {
            match args.next().as_deref().map(str::parse) {
                Some(Ok(spec)) => chain.push(spec),
                Some(Err(e)) => {
                    eprintln!("{e}");
                    return ExitCode::from(2);
                }
                None => {}
            }
        } else {
            prompt = arg;
        }
    }
    if chain.is_empty() || prompt.is_empty() {
        eprintln!("usage: chat --model provider/id [--model fallback/id] \"prompt\"");
        return ExitCode::from(2);
    }

    let client = http_client();
    let credentials = Arc::new(Credentials::new(
        std::env::temp_dir().join("azazel-example-auth.json"),
    ));
    let providers: Vec<Arc<dyn Provider>> = presets()
        .into_iter()
        .map(|config| {
            Arc::new(HttpProvider::new(config, client.clone(), credentials.clone())) as Arc<dyn Provider>
        })
        .collect();
    let router = Router::new(
        providers,
        Catalog::embedded(),
        BTreeMap::new(),
        RetryPolicy::default(),
    );

    let request = Request {
        messages: vec![Message::user(prompt)],
        ..Request::default()
    };
    let (model, mut stream) = match router.stream(&chain, &request, CancellationToken::new()).await {
        Ok(started) => started,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("[{model}]");
    let mut stdout = std::io::stdout();
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta { delta, .. } => {
                let _ = write!(stdout, "{delta}");
                let _ = stdout.flush();
            }
            StreamEvent::Done { message } => {
                if let Some(meta) = message.meta {
                    eprintln!(
                        "\n[{} in / {} out tokens, {} ms]",
                        meta.usage.input, meta.usage.output, meta.duration_ms
                    );
                }
            }
            StreamEvent::Error { error, .. } => {
                eprintln!("\n{error}");
                return ExitCode::FAILURE;
            }
            _ => {}
        }
    }
    ExitCode::SUCCESS
}
