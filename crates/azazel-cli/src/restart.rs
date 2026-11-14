//! `/restart`: azazel starts again in the same terminal, on the session that was open. It is
//! what picks up a new binary after an update without losing the conversation. `/open`, `/cd`
//! and `/worktree` start again the same way, in another directory: what is read once at the
//! start (the project, its trust, configuration and context files) is read there afresh. A
//! session that changes project goes into the other project's sessions on the way ([`Move`]).
//!
//! The UI asks for it as it ends ([`request`]); once the run is over and everything it held is
//! closed, [`Builder::run`](crate::Builder::run) starts the binary again ([`exec`]). On Unix
//! the new program takes the place of this process. Windows has no such thing: the new process
//! is started in the same console and this one stays only to hand on its exit code, since the
//! shell that started azazel takes the terminal back the moment this process ends.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock, PoisonError};

use azazel_core::SessionStore;
use clap::CommandFactory;

use crate::cli::Cli;

/// The options that choose a session or say what to do in it. The process that starts again
/// gets none of them: it opens the session that was open, and nothing is sent.
const SESSION_OPTIONS: &[&str] = &[
    "resume_latest",
    "session",
    "continue_run",
    "fork",
    "name",
    "detach",
    "attach",
];
/// The options a session changes while it runs. The process that starts again on a session of
/// its own is told what they are now.
const LIVE_OPTIONS: &[&str] = &["model", "thinking", "mode"];

/// The binary this process was started from. Asked for at the start: after an update the
/// running program's own file has been moved aside, and the new one is where this was.
static BINARY: OnceLock<Option<PathBuf>> = OnceLock::new();
/// What to start again with, once the UI asked for it. The whole process ends and starts over,
/// so this is the one thing that is the process's and not a session's.
static REQUESTED: Mutex<Option<Request>> = Mutex::new(None);

/// A session that goes into the sessions of another project as azazel starts again. It moves
/// once this run has closed its file, so that nothing written at the end is lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Move {
    /// Its file now.
    pub file: PathBuf,
    /// The sessions directory of the project it goes to.
    pub dir: PathBuf,
    /// Where it works from now on.
    pub cwd: PathBuf,
}

impl Move {
    /// Where its file will be.
    pub fn target(&self) -> PathBuf {
        match self.file.file_name() {
            Some(name) => self.dir.join(name),
            None => self.file.clone(),
        }
    }
}

/// What azazel starts again with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    args: Vec<OsString>,
    moving: Option<Move>,
}

/// Notes where the binary is, before anything can move it.
pub(crate) fn remember_binary() {
    BINARY.get_or_init(|| std::env::current_exe().ok());
}

/// Asks for azazel to start again with `args` once this run has ended; `moving`, when the
/// session goes into another project's sessions on the way. `args` then open it at
/// [`Move::target`].
pub(crate) fn request(args: Vec<OsString>, moving: Option<Move>) {
    *REQUESTED.lock().unwrap_or_else(PoisonError::into_inner) = Some(Request { args, moving });
}

/// What [`request`] asked for, if anything.
pub(crate) fn take() -> Option<Request> {
    REQUESTED.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// Moves the session where the request says, then starts again ([`exec`]). A session that
/// cannot be moved is opened where it is, and why is said.
pub(crate) fn start(request: Request) -> ExitCode {
    let Request { mut args, moving } = request;
    if let Some(moving) = moving
        && let Err(e) = SessionStore::relocate(&moving.file, &moving.dir, &moving.cwd)
    {
        eprintln!(
            "azazel: the session stays in {}, as it could not be moved: {e}",
            moving.file.display()
        );
        let target = OsString::from(moving.target());
        for arg in &mut args {
            if *arg == target {
                *arg = moving.file.clone().into_os_string();
            }
        }
    }
    exec(args)
}

/// The arguments for the azazel that starts again: those this one was started with
/// (`original`, without the program's name), minus the prompt, the options that chose a
/// session and a subcommand with what belongs to it, plus `reopen`, which opens the session
/// that is open now. With `live` the model, the thinking level and the permission mode are
/// left out too, for `reopen` to say what they are now. `without` names more options to leave
/// out, by their ids (`cwd`, `trust`).
pub(crate) fn arguments(
    original: &[OsString],
    reopen: &[OsString],
    live: bool,
    without: &[&str],
) -> Vec<OsString> {
    let mut command = Cli::command();
    command.build();
    // What the command line parser took for the subcommand; a prompt can hold the same word.
    let started = Cli::command()
        .try_get_matches_from(std::iter::once(OsString::from("azazel")).chain(original.iter().cloned()))
        .ok()
        .and_then(|matches| matches.subcommand_name().map(str::to_string));
    let mut options = &command;
    let mut in_subcommand = false;
    let mut kept = Vec::new();
    let mut words = original.iter();
    while let Some(word) = words.next() {
        let text = word.to_string_lossy();
        if text == "--" {
            // What follows is the prompt.
            break;
        }
        if !text.starts_with('-') || text == "-" {
            // The subcommand, or a word of the prompt or of the subcommand: none is kept.
            if !in_subcommand
                && started.as_deref() == Some(&*text)
                && let Some(sub) = command.find_subcommand(&*text)
            {
                options = sub;
                in_subcommand = true;
            }
            continue;
        }
        // `--name value` and `-r value` take the next word; `--name=value` and `-rvalue` do not.
        let (option, alone) = match text.strip_prefix("--") {
            Some(long) => {
                let name = long.split_once('=').map_or(long, |(name, _)| name);
                let option = options.get_arguments().find(|arg| {
                    arg.get_long() == Some(name)
                        || arg.get_all_aliases().is_some_and(|all| all.contains(&name))
                });
                (option, !long.contains('='))
            }
            None => {
                let short = text.chars().nth(1);
                let option = options.get_arguments().find(|arg| arg.get_short() == short);
                (option, text.chars().count() == 2)
            }
        };
        let value = match option {
            Some(option) if alone && option.get_action().takes_values() => words.next(),
            _ => None,
        };
        let keep = option.is_some_and(|option| {
            let id = option.get_id().as_str();
            !SESSION_OPTIONS.contains(&id)
                && !without.contains(&id)
                && !(live && LIVE_OPTIONS.contains(&id))
                && (!in_subcommand || option.is_global_set())
        });
        if keep {
            kept.push(word.clone());
            kept.extend(value.cloned());
        }
    }
    kept.extend(reopen.iter().cloned());
    kept
}

/// Starts azazel again with `args` in this terminal. On Unix this does not return when it
/// works; on Windows it returns what the new process ended with.
pub(crate) fn exec(args: Vec<OsString>) -> ExitCode {
    let binary = BINARY
        .get()
        .cloned()
        .flatten()
        .or_else(|| std::env::current_exe().ok());
    let Some(binary) = binary else {
        eprintln!("azazel: cannot start again: the path of the binary is not known");
        return ExitCode::FAILURE;
    };
    let failed = |e: std::io::Error| {
        eprintln!("azazel: cannot start {} again: {e}", binary.display());
        ExitCode::FAILURE
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        failed(std::process::Command::new(&binary).args(args).exec())
    }
    #[cfg(not(unix))]
    {
        // While the new process has the terminal, ctrl+c is its to handle: this one only
        // waits, and must not end before it.
        let waited = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .and_then(|runtime| {
                runtime.block_on(async {
                    let mut child = tokio::process::Command::new(&binary).args(args).spawn()?;
                    loop {
                        tokio::select! {
                            status = child.wait() => break status,
                            _ = tokio::signal::ctrl_c() => {}
                        }
                    }
                })
            });
        match waited {
            Ok(status) => match status.code().and_then(|code| u8::try_from(code).ok()) {
                Some(code) => ExitCode::from(code),
                None => ExitCode::FAILURE,
            },
            Err(e) => failed(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    fn restarted(original: &[&str], reopen: &[&str], live: bool) -> Vec<String> {
        arguments(&words(original), &words(reopen), live, &[])
            .iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_prompt_and_what_chose_the_session_go_and_the_open_session_is_named() {
        let reopen = ["--model", "mock/lead", "--mode", "auto", "-r", "/s/one.jsonl"];
        // Nothing but a prompt.
        assert_eq!(restarted(&["fix", "the", "tests"], &reopen, true), reopen);
        assert_eq!(restarted(&[], &reopen, true), reopen);
        // How the run is shaped stays; the session, the name and the prompt do not.
        assert_eq!(
            restarted(
                &[
                    "--set",
                    "tui.popups=false",
                    "-c",
                    "--name",
                    "work",
                    "--cwd=/p",
                    "--no-memory",
                    "--theme",
                    "light",
                    "go on"
                ],
                &reopen,
                true
            ),
            [
                &[
                    "--set",
                    "tui.popups=false",
                    "--cwd=/p",
                    "--no-memory",
                    "--theme",
                    "light"
                ][..],
                &reopen[..]
            ]
            .concat()
        );
        // Every way a session is named, and a value that reads like an option or a subcommand.
        for chosen in [
            &["-r", "abc"][..],
            &["-rabc"],
            &["--session", "abc"],
            &["--session=abc"],
            &["--resume", "doctor"],
            &["--fork", "abc"],
            &["-c", "--continue-run"],
            &["--continue"],
        ] {
            let original = [&["--trust"][..], chosen, &["--debug"]].concat();
            assert_eq!(
                restarted(&original, &reopen, true),
                [&["--trust", "--debug"][..], &reopen[..]].concat(),
                "{chosen:?}"
            );
        }
        // The model the session is on now is the one `reopen` names, not the one it began on.
        assert_eq!(
            restarted(
                &[
                    "--model",
                    "a/b",
                    "--model=c/d",
                    "--thinking",
                    "high",
                    "--mode",
                    "plan"
                ],
                &reopen,
                true
            ),
            reopen
        );
        // What follows `--` is the prompt, whatever it looks like.
        assert_eq!(
            restarted(&["--trust", "--", "--not-a-flag", "doctor"], &reopen, true),
            [&["--trust"][..], &reopen[..]].concat()
        );
    }

    #[cfg(feature = "detach")]
    #[test]
    fn a_session_elsewhere_is_opened_again_as_it_was() {
        // A background session: the subcommand and what belongs to it go, what applies to
        // every command stays, wherever it stood.
        assert_eq!(
            restarted(
                &[
                    "--theme",
                    "light",
                    "attach",
                    "ab12",
                    "--take",
                    "--set",
                    "tui.popups=false"
                ],
                &["attach", "ab12"],
                false
            ),
            ["--theme", "light", "--set", "tui.popups=false", "attach", "ab12"]
        );
        // Over SSH the host stays, and with it the model the session there was started on.
        assert_eq!(
            restarted(
                &[
                    "--ssh",
                    "box",
                    "--remote-dir",
                    "/w",
                    "--model",
                    "a/b",
                    "--detach",
                    "do it"
                ],
                &["--attach", "ab12"],
                false
            ),
            [
                "--ssh",
                "box",
                "--remote-dir",
                "/w",
                "--model",
                "a/b",
                "--attach",
                "ab12"
            ]
        );
        // A word of the prompt that is also a subcommand's name is a word of the prompt.
        assert_eq!(
            restarted(&["--trust", "run", "doctor", "now"], &["-r", "x"], true),
            ["--trust", "-r", "x"]
        );
    }

    #[test]
    fn what_was_asked_for_is_taken_once() {
        request(words(&["-r", "x"]), None);
        assert_eq!(take().map(|request| request.args), Some(words(&["-r", "x"])));
        assert_eq!(take(), None);
    }

    #[test]
    fn starting_in_another_directory_leaves_out_the_directory_and_the_trust_given() {
        let original = words(&["--cwd", "/old", "--trust", "--set", "tui.popups=false", "-c"]);
        let reopen = words(&["--cwd", "/new", "-r", "/s/one.jsonl"]);
        let moved: Vec<String> = arguments(&original, &reopen, true, &["cwd", "trust"])
            .iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            moved,
            ["--set", "tui.popups=false", "--cwd", "/new", "-r", "/s/one.jsonl"]
        );
        let moving = Move {
            file: PathBuf::from("/a/sessions/x.jsonl"),
            dir: PathBuf::from("/b/sessions"),
            cwd: PathBuf::from("/b"),
        };
        assert_eq!(moving.target(), PathBuf::from("/b/sessions/x.jsonl"));
    }
}
