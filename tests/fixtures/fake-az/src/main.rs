//! A stand-in for the Azure CLI (`az`) for tests. What it knows is in the directory `az-data`
//! beside the program, so a test copies the program to a directory of its own:
//!
//! - `fake-az account get-access-token --resource <resource> …` prints `az-data/token.json` with
//!   every `{resource}` in it replaced by the resource asked for, and exits with 0; without that
//!   file it says the account is not signed in, as `az` does, and exits with 1.
//!
//! Every call adds a line to `az-data/calls.log`: its arguments, with a tab between them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let program = std::env::current_exe().unwrap_or_default();
    let data: PathBuf = program.parent().unwrap_or(Path::new(".")).join("az-data");
    if let Ok(mut log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data.join("calls.log"))
    {
        let _ = writeln!(log, "{}", args.join("\t"));
    }
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let ["account", "get-access-token", rest @ ..] = words.as_slice() else {
        eprintln!("fake-az: unknown command {}", args.join(" "));
        return ExitCode::from(2);
    };
    let resource = rest
        .windows(2)
        .find(|pair| pair[0] == "--resource")
        .map_or("", |pair| pair[1]);
    match std::fs::read_to_string(data.join("token.json")) {
        Ok(token) => {
            println!("{}", token.replace("{resource}", resource));
            ExitCode::SUCCESS
        }
        Err(_) => {
            eprintln!("ERROR: Please run 'az login' to setup account.");
            ExitCode::from(1)
        }
    }
}
