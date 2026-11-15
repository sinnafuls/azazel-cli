//! A stand-in for `cloudflared` for tests. `fake-cloudflared tunnel --url <url>` writes the lines a
//! quick tunnel writes, with the address `https://fake-tunnel-test.trycloudflare.com`, to standard
//! error, records the url it was given in `cloudflared.url` beside the program, and then waits
//! until it is killed. Started without `tunnel`, it fails.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("tunnel") {
        eprintln!("fake-cloudflared: unknown command {}", args.join(" "));
        return ExitCode::from(2);
    }
    let url = args
        .iter()
        .position(|arg| arg == "--url")
        .and_then(|at| args.get(at + 1))
        .cloned()
        .unwrap_or_default();
    let program = std::env::current_exe().unwrap_or_default();
    let beside: PathBuf = program.parent().unwrap_or(Path::new(".")).join("cloudflared.url");
    let _ = std::fs::write(beside, &url);
    eprintln!("INF Requesting new quick Tunnel on trycloudflare.com...");
    eprintln!(
        "INF +--------------------------------------------------------------------------------------------+"
    );
    eprintln!(
        "INF |  Your quick Tunnel has been created! Visit it at (it may take some time to be reachable):  |"
    );
    eprintln!(
        "INF |  https://fake-tunnel-test.trycloudflare.com                                                |"
    );
    eprintln!(
        "INF +--------------------------------------------------------------------------------------------+"
    );
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
