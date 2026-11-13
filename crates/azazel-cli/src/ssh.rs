//! `azazel --ssh <host>`: the session runs on another machine and is shown here.
//!
//! azazel on the other machine does the work: its agents, tools, credentials and sessions are
//! that machine's. This one starts it through the `ssh` program, with your OpenSSH
//! configuration, and speaks the RPC protocol with it over the connection:
//!
//! - `azazel --ssh host [prompt]` runs `azazel rpc` there and opens the UI on it; with `-p` the
//!   prompt runs and its reply is printed;
//! - `--detach` starts the prompt as a background session there, which outlives the connection,
//!   and opens the UI on it (without a terminal it prints the session's id);
//! - `--attach <id>` opens a background session of that machine (`azazel rpc --attach <id>`
//!   there joins the connection to the session's socket);
//! - `ps`, `logs` and `kill` run there and print here.
//!
//! `ssh` is never allowed to ask: an unknown host key or a key that needs a passphrase fails,
//! as it should under a UI that owns the terminal.

use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};

use azazel_client::{Client, Error};
use azazel_core::Paths;
use azazel_protocol::{Command, Decision, Frame, UiRequest};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;

use crate::cli::{Cli, Sub};
use crate::print::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};
use crate::tui::{Place, Remote};

/// How azazel is installed on a machine that has none, or an old one.
const INSTALL: &str =
    "curl -fsSL https://raw.githubusercontent.com/sinnafuls/azazel-cli/main/install/install.sh | sh";
/// Lines of what `ssh` and the other azazel wrote to standard error that are kept to explain a
/// connection that failed.
const KEPT_ERRORS: usize = 20;

/// One word for the shell of the other machine, which splits the command `ssh` hands it.
fn quoted(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@,+%".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// The command line of azazel on the other machine: how it is started there, then `words`.
fn remote_command(cli: &Cli, words: &[String]) -> String {
    let mut line = cli.remote_azazel.clone();
    for word in words {
        line.push(' ');
        line.push_str(&quoted(word));
    }
    line
}

/// The options of this run that the other azazel is to have too.
fn carried(cli: &Cli) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for assignment in &cli.set {
        words.extend(["--set".to_string(), assignment.clone()]);
    }
    for model in &cli.model {
        words.extend(["--model".to_string(), model.clone()]);
    }
    let mut value = |flag: &str, value: Option<&String>| {
        if let Some(value) = value {
            words.extend([flag.to_string(), value.clone()]);
        }
    };
    value("--thinking", cli.thinking.as_ref());
    value("--mode", cli.mode.as_ref());
    value("--tier", cli.tier.as_ref());
    value("--preset", cli.preset.as_ref());
    value("--session", cli.session.as_ref());
    value("--fork", cli.fork.as_ref());
    value("--name", cli.name.as_ref());
    value("--cwd", cli.remote_dir.as_ref());
    value(
        "--max-turns",
        cli.max_turns.map(|turns| turns.to_string()).as_ref(),
    );
    value("--max-cost", cli.max_cost.map(|cost| cost.to_string()).as_ref());
    let switches = [
        (cli.resume_latest, "--continue"),
        (cli.trust, "--trust"),
        (cli.coordinator, "--coordinator"),
        (cli.no_memory, "--no-memory"),
        (cli.no_mcp, "--no-mcp"),
        (cli.no_hooks, "--no-hooks"),
        (cli.no_skills, "--no-skills"),
        (cli.no_extensions, "--no-extensions"),
        (cli.no_context_files, "--no-context-files"),
        (cli.bare, "--bare"),
    ];
    words.extend(
        switches
            .into_iter()
            .filter(|(on, _)| *on)
            .map(|(_, flag)| flag.to_string()),
    );
    words
}

/// `ssh` to `host` running `command` there. It never asks for anything.
fn ssh(host: &str, command: &str) -> tokio::process::Command {
    let mut ssh = tokio::process::Command::new("ssh");
    ssh.args(["-T", "-o", "BatchMode=yes", host, command]);
    ssh
}

/// Runs a command of the other azazel and lets it print here. Returns its exit code.
async fn pass_through(host: &str, command: &str) -> u8 {
    match ssh(host, command).stdin(Stdio::null()).status().await {
        Ok(status) => status.code().map_or(EXIT_FAILED, |code| code as u8),
        Err(e) => {
            eprintln!("azazel: cannot run ssh: {e}");
            EXIT_FAILED
        }
    }
}

/// Starts a background session there and returns its id. What goes wrong is said, as the other
/// azazel said it; `Err` is the exit code.
async fn started(host: &str, command: &str) -> Result<String, u8> {
    let output = ssh(host, command)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .await
        .map_err(|e| {
            eprintln!("azazel: cannot run ssh: {e}");
            EXIT_FAILED
        })?;
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || id.is_empty() {
        return Err(output.status.code().map_or(EXIT_FAILED, |code| code as u8));
    }
    Ok(id)
}

/// A connection to the other azazel, and what was written to standard error on the way.
struct Connection {
    client: Client,
    /// Kept alive with the connection; killed when it is dropped.
    _ssh: Child,
}

/// Starts `command` on `host` and speaks the protocol with it. `Err` says why that failed, in
/// words for the user.
async fn connect(host: &str, command: &str) -> Result<Connection, String> {
    let mut child = ssh(host, command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot run ssh: {e}"))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err("cannot talk to ssh".to_string());
    };
    // Under the UI nothing may be written to the terminal: what goes wrong is kept to be said.
    let errors: Arc<Mutex<Vec<String>>> = Arc::default();
    let kept = errors.clone();
    let collecting = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut kept = kept.lock().unwrap_or_else(PoisonError::into_inner);
            if kept.len() == KEPT_ERRORS {
                kept.remove(0);
            }
            kept.push(line);
        }
    });
    match Client::connect(BufReader::new(stdout), stdin).await {
        Ok(client) => Ok(Connection { client, _ssh: child }),
        Err(e) => {
            // The other end is gone; what it said on the way out is complete now.
            let _ = child.wait().await;
            let _ = collecting.await;
            let said = errors.lock().unwrap_or_else(PoisonError::into_inner).join("\n");
            let why = match (&e, said.is_empty()) {
                (Error::Protocol(why), _) => why.clone(),
                (_, false) => said,
                (_, true) => e.to_string(),
            };
            Err(format!(
                "could not start azazel on {host}: {why}\nIf azazel is missing there, or older than \
                 {}, install it on {host} with:\n  {INSTALL}",
                env!("CARGO_PKG_VERSION")
            ))
        }
    }
}

/// Prints the reply to one prompt run on the other machine. Nobody is there to answer what the
/// agent asks, as in print mode here.
async fn print(client: Client, prompt: String) -> u8 {
    let run = client
        .prompt_and_wait(prompt, |frame| match frame {
            Frame::ApprovalRequest { request_id, .. } => Some(Command::ApprovalResponse {
                request_id: request_id.clone(),
                decision: Decision::Deny,
                feedback: Some("nobody is there to approve this".to_string()),
            }),
            Frame::UiRequest { request_id, request } if !matches!(request, UiRequest::Notify { .. }) => {
                Some(Command::UiResponse {
                    request_id: request_id.clone(),
                    value: Value::Null,
                })
            }
            _ => None,
        })
        .await;
    let code = match run {
        Ok(run) if run.stop == "aborted" => crate::print::EXIT_ABORTED,
        Ok(run) => {
            println!("{}", run.text);
            EXIT_OK
        }
        Err(e) => {
            eprintln!("azazel: {e}");
            EXIT_FAILED
        }
    };
    let _ = client.shutdown().await;
    code
}

/// What `ps`, `logs` and `kill` are called on the other machine.
#[cfg(feature = "detach")]
fn listing_words(command: &Sub) -> Option<Vec<String>> {
    let mut words = Vec::new();
    match command {
        Sub::Ps { json } => {
            words.push("ps".to_string());
            if *json {
                words.push("--json".to_string());
            }
        }
        Sub::Logs { id, follow, json } => {
            words.extend(["logs".to_string(), id.clone()]);
            if *follow {
                words.push("--follow".to_string());
            }
            if *json {
                words.push("--json".to_string());
            }
        }
        Sub::Kill { id } => words.extend(["kill".to_string(), id.clone()]),
        _ => return None,
    }
    Some(words)
}

#[cfg(not(feature = "detach"))]
fn listing_words(_: &Sub) -> Option<Vec<String>> {
    None
}

pub async fn run(cli: &Cli, paths: Paths, host: &str) -> u8 {
    // A name that starts with a dash would be read by ssh as one of its options.
    if host.is_empty() || host.starts_with('-') {
        eprintln!("azazel: --ssh takes the name of a machine, as `ssh` does");
        return EXIT_USAGE;
    }
    if let Some(command) = &cli.command {
        return match listing_words(command) {
            Some(words) => pass_through(host, &remote_command(cli, &words)).await,
            None => {
                eprintln!(
                    "azazel: with --ssh there is a prompt, --detach, --attach <id>, or one of ps, logs and kill"
                );
                EXIT_USAGE
            }
        };
    }
    let prompt = cli.prompt_text();
    let terminal = std::io::IsTerminal::is_terminal(&std::io::stdin())
        && std::io::IsTerminal::is_terminal(&std::io::stdout());
    // A background session there: started now, or one that is running.
    let attach = if cli.detach {
        if prompt.trim().is_empty() {
            eprintln!("azazel: --detach needs a prompt to run in the background");
            return EXIT_USAGE;
        }
        let mut words = carried(cli);
        words.extend(["bg".to_string(), "--".to_string(), prompt.clone()]);
        let command = remote_command(cli, &words);
        // Without a terminal, or for a script, its id is all there is to say.
        if !terminal || cli.print || cli.plain {
            return pass_through(host, &command).await;
        }
        match started(host, &command).await {
            Ok(id) => {
                println!("background session {id} on {host}");
                Some(id)
            }
            Err(code) => return code,
        }
    } else {
        cli.attach.clone()
    };
    let (words, place) = match &attach {
        Some(id) => (
            vec!["rpc".to_string(), "--attach".to_string(), id.clone()],
            Place::SshBackground {
                host: host.to_string(),
                id: id.clone(),
            },
        ),
        None => {
            let mut words = carried(cli);
            words.push("rpc".to_string());
            (
                words,
                Place::Ssh {
                    host: host.to_string(),
                },
            )
        }
    };
    let lines = attach.is_some() && (cli.plain || !terminal);
    if !lines && !cli.print && !terminal {
        eprintln!("azazel: the interactive UI needs a terminal; pass -p \"prompt\" to run without one");
        return EXIT_USAGE;
    }
    let connection = match connect(host, &remote_command(cli, &words)).await {
        Ok(connection) => connection,
        Err(e) => {
            eprintln!("azazel: {e}");
            return EXIT_FAILED;
        }
    };
    if lines {
        #[cfg(feature = "detach")]
        return crate::detach::line_client(connection.client).await;
        #[cfg(not(feature = "detach"))]
        {
            eprintln!("azazel: this build has no background sessions");
            return EXIT_FAILED;
        }
    }
    if cli.print {
        let prompt = crate::print::with_piped_stdin(&prompt);
        if prompt.trim().is_empty() {
            eprintln!("azazel: -p needs a prompt: azazel -p \"what to do\", or text piped on standard input");
            return EXIT_USAGE;
        }
        return print(connection.client, prompt).await;
    }
    let Connection { client, _ssh } = connection;
    let opened = match crate::tui::env_here(cli, paths) {
        Ok(env) => Remote::open(client, place).await.map(|remote| (remote, env)),
        Err(e) => Err(format!("{e:#}")),
    };
    match opened {
        Ok((remote, env)) => {
            let first = (!prompt.trim().is_empty() && attach.is_none()).then_some(prompt);
            crate::tui::run_remote(remote, env, first).await
        }
        Err(e) => {
            eprintln!("azazel: {e}");
            EXIT_FAILED
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn words_reach_the_other_shell_whole() {
        assert_eq!(quoted("rpc"), "rpc");
        assert_eq!(quoted("--cwd=/srv/app"), "--cwd=/srv/app");
        assert_eq!(quoted("fix the tests"), "'fix the tests'");
        assert_eq!(quoted("it's"), r"'it'\''s'");
        assert_eq!(quoted(""), "''");
        assert_eq!(quoted("$(rm -rf ~)"), "'$(rm -rf ~)'");
    }

    #[test]
    fn the_other_azazel_gets_this_runs_options() {
        let cli = Cli::parse_from([
            "azazel",
            "--ssh",
            "build-box",
            "--remote-dir",
            "/srv/my app",
            "--model",
            "@smol",
            "--mode",
            "auto",
            "--trust",
            "-c",
            "fix",
            "it",
        ]);
        let mut words = carried(&cli);
        words.push("rpc".to_string());
        assert_eq!(
            remote_command(&cli, &words),
            "azazel --model @smol --mode auto --cwd '/srv/my app' --continue --trust rpc"
        );
        // How azazel is started there is given as a command, not as one word.
        let cli = Cli::parse_from([
            "azazel",
            "--ssh",
            "h",
            "--remote-azazel",
            "env AZAZEL_HOME=/tmp/h /opt/azazel",
            "--attach",
            "ab12",
        ]);
        let words = ["rpc".to_string(), "--attach".to_string(), "ab12".to_string()];
        assert_eq!(
            remote_command(&cli, &words),
            "env AZAZEL_HOME=/tmp/h /opt/azazel rpc --attach ab12"
        );
    }
}
