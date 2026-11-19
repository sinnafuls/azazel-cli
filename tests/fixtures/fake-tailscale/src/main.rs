//! A stand-in for `tailscale` for tests. What it knows is in the directory `tailscale-data` beside
//! the program, so a test copies the program to a directory of its own:
//!
//! - `status --json` prints `status.json`, or says the daemon is not running (exit 1);
//! - `serve status --json` prints `serve.json`, or `{}`;
//! - `serve --bg --https=<port> <target>` adds the port to `serve.json`, with the target as the proxy
//!   of its `Web` entry, as the real `serve status --json` shows it;
//! - `serve --https=<port> off` removes both.
//!
//! Every call adds a line to `calls.log`: its arguments, with a tab between them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value, json};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let program = std::env::current_exe().unwrap_or_default();
    let data: PathBuf = program.parent().unwrap_or(Path::new(".")).join("tailscale-data");
    if let Ok(mut log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data.join("calls.log"))
    {
        let _ = writeln!(log, "{}", args.join("\t"));
    }
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let serve_file = data.join("serve.json");
    let mut serve: Value = std::fs::read_to_string(&serve_file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}));
    let https = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--https="))
        .unwrap_or_default()
        .to_string();
    match words.as_slice() {
        ["status", "--json"] => match std::fs::read_to_string(data.join("status.json")) {
            Ok(status) => println!("{status}"),
            Err(_) => {
                eprintln!("failed to connect to local tailscaled; it doesn't appear to be running");
                return ExitCode::from(1);
            }
        },
        ["serve", "status", "--json"] => println!("{serve}"),
        ["serve", "--bg", _, target] if target.starts_with("http://127.0.0.1:") => {
            serve["TCP"][&https] = json!({ "HTTPS": true });
            let dns = std::fs::read_to_string(data.join("status.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|status| status["Self"]["DNSName"].as_str().map(str::to_string))
                .unwrap_or_default();
            let host = format!("{}:{https}", dns.trim_end_matches('.'));
            serve["Web"][host] = json!({ "Handlers": { "/": { "Proxy": target } } });
            let _ = std::fs::write(&serve_file, serve.to_string());
        }
        ["serve", _, "off"] => {
            if let Some(tcp) = serve["TCP"].as_object_mut() {
                tcp.remove(&https);
            }
            if let Some(web) = serve["Web"].as_object_mut() {
                web.retain(|host, _| !host.ends_with(&format!(":{https}")));
            }
            let _ = std::fs::write(&serve_file, serve.to_string());
        }
        _ => {
            eprintln!("fake-tailscale: unknown command {}", args.join(" "));
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}
