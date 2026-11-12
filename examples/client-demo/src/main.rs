//! Drives one prompt through `azazel rpc` with the client library.
//!
//! ```sh
//! cargo run -p client-demo -- [--binary <azazel>] [--cwd <dir>] [--allow] <prompt…>
//! ```
//!
//! The reply is printed as it is written and tool calls are named on standard error. A tool call
//! that needs permission is refused unless `--allow` is given.

use std::io::Write;
use std::process::ExitCode;

use azazel_client::protocol::{Command, Decision, Frame, StreamEvent, UiRequest};
use azazel_client::{Client, Error, Options};

struct Args {
    options: Options,
    allow: bool,
    prompt: String,
}

fn parse_args() -> Result<Args, String> {
    let mut options = Options::new("azazel");
    let mut allow = false;
    let mut prompt = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--binary" => options.binary = args.next().ok_or("--binary needs a path")?.into(),
            "--cwd" => options.cwd = Some(args.next().ok_or("--cwd needs a directory")?.into()),
            "--allow" => allow = true,
            _ => prompt.push(arg),
        }
    }
    if prompt.is_empty() {
        return Err("usage: client-demo [--binary <azazel>] [--cwd <dir>] [--allow] <prompt…>".to_string());
    }
    Ok(Args {
        options,
        allow,
        prompt: prompt.join(" "),
    })
}

/// Shows a frame of the run and says what to answer, if it asks for an answer.
fn on_frame(frame: &Frame, allow: bool) -> Option<Command> {
    match frame {
        Frame::Stream {
            agent,
            event: StreamEvent::TextDelta { delta, .. },
        } if agent == "lead" => {
            print!("{delta}");
            let _ = std::io::stdout().flush();
            None
        }
        Frame::ToolStart { agent, name, .. } => {
            eprintln!("[{agent}] {name}");
            None
        }
        Frame::ApprovalRequest {
            request_id,
            tool,
            subjects,
            ..
        } => {
            let verdict = if allow { "allowed" } else { "refused" };
            eprintln!("{tool} {} needs permission: {verdict}", subjects.join(" "));
            Some(Command::ApprovalResponse {
                request_id: request_id.clone(),
                decision: if allow { Decision::Allow } else { Decision::Deny },
                feedback: (!allow).then(|| "the demo client was started without --allow".to_string()),
            })
        }
        Frame::UiRequest {
            request: UiRequest::Notify { level, text },
            ..
        } => {
            eprintln!("{level}: {text}");
            None
        }
        // Nobody is here to answer a question; dismissing it lets the agent go on.
        Frame::UiRequest { request_id, .. } => Some(Command::UiResponse {
            request_id: request_id.clone(),
            value: serde_json::Value::Null,
        }),
        _ => None,
    }
}

async fn run(args: Args) -> Result<(), Error> {
    let client = Client::spawn(args.options).await?;
    eprintln!("azazel {}", client.version());
    let run = client
        .prompt_and_wait(args.prompt, |frame| on_frame(frame, args.allow))
        .await?;
    println!();
    eprintln!("stopped: {}", run.stop);
    client.shutdown().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(usage) => {
            eprintln!("{usage}");
            return ExitCode::from(2);
        }
    };
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("client-demo: {e}");
            ExitCode::FAILURE
        }
    }
}
