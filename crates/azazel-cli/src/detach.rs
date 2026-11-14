//! Background sessions from the outside: `azazel bg`, `ps`, `logs`, `attach` and `kill`, and the
//! `/bg` command. The process each of them talks to is [`crate::host`].

pub(crate) mod printer;
pub(crate) mod socket;

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_client::{Client, Error};
use azazel_core::{
    Command as SlashCommand, CommandHandler, CommandOutcome, ExtCtx, ExtError, Extension, Paths,
    PermissionState, Registrar, SessionStore,
};
use azazel_protocol::{AskKind, Attached, Command, Decision, Frame, Replay, UiRequest};
use azazel_team::Team;
use azazel_tui::components::format_elapsed;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::cli::Cli;
use crate::print::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};
use crate::running::{self, Entry, Lock};
use printer::Printer;

/// How long a background session may take to say it has started.
const START_LIMIT: Duration = Duration::from_secs(60);
/// How long `kill` waits for the session to end.
const KILL_LIMIT: Duration = Duration::from_secs(30);
/// How often a followed log is looked at again.
const FOLLOW_EVERY: Duration = Duration::from_millis(200);

/// An id for a new background session: eight hex digits nobody else on this machine has.
pub(crate) fn new_id(home: &Path) -> String {
    let mut salt = 0u32;
    loop {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default();
        let digest = Sha256::digest(format!("{}:{nanos}:{salt}", std::process::id()));
        let id: String = digest.iter().take(4).map(|byte| format!("{byte:02x}")).collect();
        let taken = running::entry_file(home, &id).exists() || running::log_file(home, &id).exists();
        if !taken {
            return id;
        }
        salt += 1;
    }
}

/// Where the subcommand stands among the arguments azazel was started with (the program name
/// not counted): the first word that is neither an option nor the value of one.
fn subcommand_at(args: &[OsString]) -> Option<usize> {
    let command = <Cli as clap::CommandFactory>::command();
    let takes_value = |name: &str| {
        command.get_arguments().any(|arg| {
            let named = match name.strip_prefix("--") {
                Some(long) => {
                    arg.get_long() == Some(long)
                        || arg.get_all_aliases().is_some_and(|all| all.contains(&long))
                }
                None => name
                    .chars()
                    .nth(1)
                    .is_some_and(|short| arg.get_short() == Some(short)),
            };
            named && arg.get_action().takes_values()
        })
    };
    let mut at = 0;
    while at < args.len() {
        let word = args[at].to_string_lossy();
        if word == "--" {
            return None;
        }
        if !word.starts_with('-') {
            return Some(at);
        }
        // `--name value` and `-r value` take the next word; `--name=value` and `-rvalue` do not.
        let alone = if word.starts_with("--") {
            !word.contains('=')
        } else {
            word.len() == 2
        };
        at += if alone && takes_value(&word) { 2 } else { 1 };
    }
    None
}

/// The arguments of the background process for a run started as `azazel [flags] bg [flags]
/// <prompt>`: the same, with `host --id <id>` where `bg` stood.
fn host_args(args: &[OsString], id: &str) -> Option<Vec<OsString>> {
    let at = subcommand_at(args)?;
    let mut host: Vec<OsString> = args[..at].to_vec();
    host.extend(["host", "--id", id].map(OsString::from));
    host.extend_from_slice(&args[at + 1..]);
    Some(host)
}

/// One argument as the Windows C runtime reads it back from a command line.
pub(crate) fn quoted(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '"']) {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            // Backslashes before a quote count double, and the quote is escaped.
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    // Those before the closing quote count double too.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// A background process that was just started.
struct Started {
    #[cfg(unix)]
    child: Box<dyn process_wrap::tokio::ChildWrapper>,
    #[cfg(windows)]
    process: winsafe::guard::CloseHandlePiGuard,
}

impl Started {
    /// Whether the process has ended already.
    fn ended(&mut self) -> bool {
        #[cfg(unix)]
        {
            !matches!(self.child.try_wait(), Ok(None))
        }
        #[cfg(windows)]
        {
            !matches!(
                self.process.hProcess.WaitForSingleObject(Some(0)),
                Ok(winsafe::co::WAIT::TIMEOUT)
            )
        }
    }
}

/// Starts `azazel host` with `args`, detached from this terminal: closing the terminal, or
/// ending this process, leaves it running. It gets none of this process's inputs and outputs,
/// so whoever reads them does not wait for it.
fn spawn_host(args: &[OsString]) -> Result<Started, String> {
    let program = std::env::current_exe().map_err(|e| e.to_string())?;
    // A session of its own: no controlling terminal, so no hang-up when this one closes.
    #[cfg(unix)]
    {
        let mut command = process_wrap::tokio::CommandWrap::with_new(program, |command| {
            command
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        });
        command.wrap(process_wrap::tokio::ProcessSession);
        let child = command.spawn().map_err(|e| e.to_string())?;
        Ok(Started { child })
    }
    // A console of its own that has no window; the commands the session runs inherit it (a
    // process with no console at all would have each of them open a window). No handle of
    // this process is passed on: the standard library would pass on every inheritable one,
    // the pipe a caller reads this process's output through among them.
    #[cfg(windows)]
    {
        use winsafe::co::CREATE;
        let mut line = quoted(&program.to_string_lossy());
        for arg in args {
            let arg = arg
                .to_str()
                .ok_or("an argument is not Unicode text; it cannot be passed to the background session")?;
            line.push(' ');
            line.push_str(&quoted(arg));
        }
        let mut startup = winsafe::STARTUPINFO::default();
        let process = winsafe::CreateProcess(
            None,
            Some(&line),
            None,
            None,
            false,
            CREATE::NO_WINDOW | CREATE::NEW_PROCESS_GROUP,
            &[],
            None,
            &mut startup,
        )
        .map_err(|e| e.to_string())?;
        Ok(Started { process })
    }
}

/// Starts a background session and waits until it is listed. `Err` says why it did not start.
fn start_blocking(home: &Path, args: &[OsString], id: &str) -> Result<(), String> {
    running::create_run_dir(home).map_err(|e| format!("cannot create the run directory: {e}"))?;
    let mut host = spawn_host(args).map_err(|e| format!("could not start the session: {e}"))?;
    let started = Instant::now();
    loop {
        if running::entry_file(home, id).exists() {
            return Ok(());
        }
        let ended = host.ended();
        if ended || started.elapsed() > START_LIMIT {
            // A session that cannot start says why in a file: nobody reads what it prints.
            let said = std::fs::read_to_string(running::err_file(home, id)).unwrap_or_default();
            let _ = std::fs::remove_file(running::err_file(home, id));
            return Err(match (ended, said.trim().is_empty()) {
                (_, false) => said.trim().to_string(),
                (true, true) => "the background session ended before it started work".to_string(),
                (false, true) => "the background session did not start in time".to_string(),
            });
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// [`start_blocking`] beside the async work. The process handle never crosses an await.
pub(crate) async fn start(home: &Path, args: Vec<OsString>, id: &str) -> Result<(), String> {
    let (home, id) = (home.to_path_buf(), id.to_string());
    tokio::task::spawn_blocking(move || start_blocking(&home, &args, &id))
        .await
        .map_err(|e| format!("could not start the session: {e}"))?
}

/// `azazel bg <prompt>`: the prompt runs in a session of its own that outlives this terminal.
/// With a `request_id`, the command sent again starts nothing and prints the id of the session
/// the first one started.
pub async fn bg(paths: &Paths, request_id: Option<&str>) -> u8 {
    let id = new_id(&paths.home);
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some(args) = host_args(&args, &id) else {
        eprintln!("azazel: bg needs a prompt: azazel bg \"what to do\"");
        return EXIT_USAGE;
    };
    if let Some(request_id) = request_id {
        match running::claim_request(&paths.home, request_id, &id) {
            Ok(Ok(())) => {}
            Ok(Err(started)) => {
                println!("{started}");
                eprintln!("this request started {started} before; nothing was started");
                return EXIT_OK;
            }
            Err(e) => {
                eprintln!("azazel: cannot note the request: {e}");
                return EXIT_FAILED;
            }
        }
    }
    match start(&paths.home, args, &id).await {
        Ok(()) => {
            println!("{id}");
            eprintln!("azazel attach {id} connects to it, azazel logs -f {id} follows it");
            EXIT_OK
        }
        Err(e) => {
            if let Some(request_id) = request_id {
                running::release_request(&paths.home, request_id);
            }
            eprintln!("azazel: {e}");
            EXIT_FAILED
        }
    }
}

/// Starts the background session of `entry` again, whose process died in the middle of a run,
/// on the same session and under the same id. It works as the one before did, in the mode
/// asked for now or else the one it was in; with `[session] resume_runs` it goes on with the
/// run by itself. Returns the entry of the new process.
async fn revive(cli: &Cli, paths: &Paths, entry: &Entry) -> Result<Entry, String> {
    let home = &paths.home;
    let mut args: Vec<OsString> = entry.args.iter().map(OsString::from).collect();
    args.extend(["--cwd".into(), entry.cwd.clone().into()]);
    args.extend(["--session".into(), entry.session_file.clone().into()]);
    if let Some(mode) = cli.mode.as_ref().or(entry.mode.as_ref()) {
        args.extend(["--mode".into(), mode.into()]);
    }
    for spec in &cli.model {
        args.extend(["--model".into(), spec.into()]);
    }
    if let Some(key) = &cli.api_key {
        args.extend(["--api-key".into(), key.into()]);
    }
    args.extend(["host", "--id", &entry.id].map(OsString::from));
    // The entry of the process that died stands where the new one is looked for.
    let _ = std::fs::remove_file(running::entry_file(home, &entry.id));
    eprintln!(
        "background session {} had stopped in the middle of a run; starting it again…",
        entry.id
    );
    match start(home, args, &entry.id).await {
        Ok(()) => running::read_entry(home, &entry.id)
            .ok_or_else(|| format!("the background session {} ended as soon as it started", entry.id)),
        Err(e) => {
            // Still there to try again.
            let _ = entry.write(home);
            Err(e)
        }
    }
}

/// The background session `wanted` names, running: one whose process died in the middle of a
/// run is started again first.
async fn find_running(cli: &Cli, paths: &Paths, wanted: &str) -> Result<Entry, String> {
    let entry = running::find(&paths.home, wanted)?;
    if entry.interrupted() {
        return revive(cli, paths, &entry).await;
    }
    Ok(entry)
}

fn elapsed_since(started: &str) -> Duration {
    started
        .parse::<jiff::Timestamp>()
        .ok()
        .and_then(|started| Duration::try_from(jiff::Timestamp::now().duration_since(started)).ok())
        .unwrap_or_default()
}

/// `azazel ps`: the background sessions that are running, and those whose process died in the
/// middle of a run.
pub fn ps(paths: &Paths, json: bool) -> u8 {
    let entries = running::list(&paths.home);
    if json {
        for entry in entries {
            let mut row = serde_json::to_value(&entry).unwrap_or(Value::Null);
            row["elapsed_secs"] = json!(elapsed_since(&entry.started).as_secs());
            println!("{row}");
        }
        return EXIT_OK;
    }
    if entries.is_empty() {
        println!("no background session is running");
        return EXIT_OK;
    }
    let rows: Vec<[String; 6]> = entries
        .iter()
        .map(|entry| {
            [
                entry.id.clone(),
                entry.name.clone().unwrap_or_else(|| "-".to_string()),
                entry.state.clone(),
                format_elapsed(elapsed_since(&entry.started)),
                entry
                    .cost_usd
                    .map_or_else(|| "-".to_string(), |cost| format!("${cost:.2}")),
                entry.cwd.display().to_string(),
            ]
        })
        .collect();
    let heads = ["ID", "NAME", "STATE", "ELAPSED", "COST", "DIRECTORY"];
    let width = |column: usize| {
        rows.iter()
            .map(|row| row[column].chars().count())
            .chain([heads[column].len()])
            .max()
            .unwrap_or(0)
    };
    let widths: Vec<usize> = (0..heads.len()).map(width).collect();
    let print = |cells: &[String]| {
        let line: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        println!("{}", line.join("  ").trim_end());
    };
    print(&heads.map(str::to_string));
    for row in &rows {
        print(row);
    }
    if entries.iter().any(Entry::interrupted) {
        println!(
            "interrupted: the process stopped in the middle of a run; `azazel attach <id>` starts it again \
             and goes on, `azazel kill <id>` forgets it"
        );
    }
    EXIT_OK
}

/// The id whose log `wanted` names: a running session, else a log that is still kept.
fn log_id(home: &Path, wanted: &str) -> Result<String, String> {
    if let Ok(entry) = running::find(home, wanted) {
        return Ok(entry.id);
    }
    let kept: Vec<String> = std::fs::read_dir(running::run_dir(home))
        .into_iter()
        .flatten()
        .flatten()
        .map(|file| file.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
        .filter_map(|path| Some(path.file_stem()?.to_string_lossy().into_owned()))
        .filter(|id| id.starts_with(wanted))
        .collect();
    match kept.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("there is no log of a background session `{wanted}`")),
        many => Err(format!(
            "`{wanted}` matches {} logs: {}",
            many.len(),
            many.join(", ")
        )),
    }
}

/// `azazel logs <id>`: what the session has done, as text or as the frames it wrote.
pub async fn logs(paths: &Paths, wanted: &str, follow: bool, json: bool) -> u8 {
    let home = &paths.home;
    let id = match log_id(home, wanted) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("azazel: {e}");
            return EXIT_FAILED;
        }
    };
    let mut file = match std::fs::File::open(running::log_file(home, &id)) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("azazel: cannot read the log of {id}: {e}");
            return EXIT_FAILED;
        }
    };
    let mut printer = Printer::new(std::io::stdout(), false);
    // Bytes read that do not end a line yet.
    let mut partial = String::new();
    let mut at = 0u64;
    loop {
        let alive = Lock::held(&running::host_lock(home, &id));
        let mut text = String::new();
        let read = file
            .seek(SeekFrom::Start(at))
            .and_then(|_| file.read_to_string(&mut text));
        if let Ok(read) = read {
            at += read as u64;
        }
        partial.push_str(&text);
        while let Some(end) = partial.find('\n') {
            let line: String = partial.drain(..=end).collect();
            let line = line.trim_end();
            if json {
                println!("{line}");
            } else if let Ok(frame) = serde_json::from_str::<Frame>(line) {
                printer.frame(&frame);
            }
        }
        // The last look was taken after the session had ended: nothing more will come.
        if !follow || !alive {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(FOLLOW_EVERY) => {}
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    EXIT_OK
}

/// Connects to a background session; nothing has been said to it yet.
pub(crate) async fn connect(entry: &Entry) -> Result<Client, String> {
    let (reader, writer) = socket::connect(&entry.socket)
        .await
        .map_err(|e| format!("cannot reach the background session {}: {e}", entry.id))?;
    Client::connect(reader, writer).await.map_err(|e| e.to_string())
}

/// Tells a background session to end and waits until its process has gone. With `now` the
/// lead is stopped; otherwise the session ends when the lead has finished what it is doing.
async fn stop(home: &Path, entry: &Entry, now: bool, limit: Option<Duration>) -> Result<(), String> {
    let client = connect(entry).await?;
    let mut frames = client.frames();
    match client.send(Command::Stop { now }).await {
        // A session that was already ending closes the connection instead of answering.
        Ok(_) | Err(Error::Closed) => {}
        Err(e) => return Err(e.to_string()),
    }
    let gone = async {
        while frames.next().await.is_some() {}
        // The connection closes a moment before the process has let go of its files.
        while Lock::held(&running::host_lock(home, &entry.id)) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    match limit {
        Some(limit) => tokio::time::timeout(limit, gone)
            .await
            .map_err(|_| format!("the background session {} did not end in time", entry.id)),
        None => {
            gone.await;
            Ok(())
        }
    }
}

/// `azazel kill <id>`: stops the lead, ends the session, and the process exits. A session
/// whose process is gone already is taken off the list.
pub async fn kill(paths: &Paths, wanted: &str) -> u8 {
    let outcome = match running::find(&paths.home, wanted) {
        Ok(entry) if entry.interrupted() => {
            running::forget(&paths.home, &entry);
            Ok(entry.id)
        }
        Ok(entry) => stop(&paths.home, &entry, true, Some(KILL_LIMIT))
            .await
            .map(|()| entry.id),
        Err(e) => Err(e),
    };
    match outcome {
        Ok(id) => {
            println!("stopped {id}");
            EXIT_OK
        }
        Err(e) => {
            eprintln!("azazel: {e}");
            EXIT_FAILED
        }
    }
}

/// `azazel attach --take <id>`: ends the background session and returns what it was, so that
/// its session can be opened here.
pub async fn take(paths: &Paths, wanted: &str, now: bool) -> Result<Entry, String> {
    let entry = running::find(&paths.home, wanted)?;
    // Its process is gone: the session is free to be opened here.
    if entry.interrupted() {
        running::forget(&paths.home, &entry);
        return Ok(entry);
    }
    if !now && entry.state != "idle" {
        eprintln!(
            "waiting for {} to finish what it is doing (--now stops it instead)…",
            entry.id
        );
    }
    stop(&paths.home, &entry, now, None).await?;
    // What the session said last about itself: it may have moved to another session file.
    Ok(running::read_entry(&paths.home, &entry.id).unwrap_or(entry))
}

/// A request the session has made that the line client has not answered yet.
enum Asked {
    Approval(String),
    Ui(String, UiRequest),
}

/// The option `word` names: its number, counted from one, or its text.
fn option_index(options: &[String], word: &str) -> Option<usize> {
    let word = word.trim();
    match word.parse::<usize>() {
        Ok(number) => (1..=options.len()).contains(&number).then(|| number - 1),
        Err(_) => options
            .iter()
            .position(|option| option.eq_ignore_ascii_case(word)),
    }
}

const YES: [&str; 5] = ["yes", "y", "true", "ok", "approve"];
const NO: [&str; 3] = ["no", "n", "false"];

/// What `text` answers to one question of an agent. `Err` says what to type.
fn answer_question(question: &azazel_protocol::Question, text: &str) -> Result<Value, String> {
    let lower = text.to_ascii_lowercase();
    match question.kind() {
        AskKind::Text => Ok(json!(text)),
        AskKind::Confirm => {
            if YES.contains(&lower.as_str()) {
                Ok(json!(true))
            } else if NO.contains(&lower.as_str()) {
                Ok(json!(false))
            } else {
                Err("answer yes or no".to_string())
            }
        }
        AskKind::Select | AskKind::Multiselect => {
            let options = &question.options;
            let words: Vec<&str> = if question.multi {
                text.split(',').collect()
            } else {
                vec![text]
            };
            let chosen = words
                .into_iter()
                .map(|word| {
                    option_index(options, word)
                        .map(|index| json!(options[index]))
                        .ok_or_else(|| format!("`{}` is not one of the options", word.trim()))
                })
                .collect::<Result<Vec<Value>, String>>();
            match chosen {
                // What is none of the options is the answer itself where one may be typed.
                Err(_) if question.allow_freeform => Ok(json!([text])),
                chosen => chosen.map(Value::Array),
            }
        }
    }
}

/// The `value` of the `ui_response` that `text` gives to `request`. `Err` says what to type.
fn answer(request: &UiRequest, text: &str) -> Result<Value, String> {
    let text = text.trim();
    if text.is_empty() {
        // Dismissed, as closing the question in the full UI would.
        return Ok(Value::Null);
    }
    let lower = text.to_ascii_lowercase();
    match request {
        UiRequest::Ask(question) => answer_question(question, text),
        UiRequest::Form { questions } => {
            // One answer per question, in their order; an empty one leaves its question open.
            let parts: Vec<&str> = text.split(';').collect();
            if parts.len() != questions.len() {
                return Err(format!(
                    "give {} answers, separated by `;` (an empty one leaves a question open)",
                    questions.len()
                ));
            }
            questions
                .iter()
                .zip(parts)
                .enumerate()
                .map(|(at, (question, part))| match part.trim() {
                    "" => Ok(Value::Null),
                    part => answer_question(question, part).map_err(|wrong| format!("{}: {wrong}", at + 1)),
                })
                .collect::<Result<Vec<Value>, String>>()
                .map(Value::Array)
        }
        UiRequest::Confirm { .. } => {
            if YES.contains(&lower.as_str()) {
                Ok(json!(true))
            } else if NO.contains(&lower.as_str()) {
                Ok(json!(false))
            } else {
                Err("answer yes or no".to_string())
            }
        }
        UiRequest::Select { options, .. } => option_index(options, text)
            .map(|index| json!(index))
            .ok_or_else(|| format!("`{text}` is not one of the options")),
        UiRequest::Input { .. } => Ok(json!(text)),
        UiRequest::Plan { .. } if YES.contains(&lower.as_str()) => Ok(json!(true)),
        UiRequest::Plan { .. } => Ok(json!(text)),
        UiRequest::Notify { .. } => Ok(Value::Null),
    }
}

const ATTACH_HELP: &str = "text is sent to the session as a message; /approve, /session, /always and /deny \
                           [note] answer a permission request; /answer <text> answers a question; /abort stops the current \
                           work; /continue goes on with a run that was interrupted; /detach leaves the session running; \
                           /kill ends it";

/// What one typed line asks for.
enum Typed {
    Send(Command),
    Say(String),
    Detach,
}

/// Reads one line typed into the line client. Requests it answers leave `asked`.
fn typed(line: &str, asked: &mut VecDeque<Asked>) -> Typed {
    let line = line.trim();
    let Some(command) = line.strip_prefix('/') else {
        return match line.is_empty() {
            true => Typed::Say(String::new()),
            false => Typed::Send(Command::Prompt {
                text: line.to_string(),
                images: Vec::new(),
                request_id: None,
            }),
        };
    };
    let (name, rest) = command.split_once(char::is_whitespace).unwrap_or((command, ""));
    let rest = rest.trim();
    let mut approval = |decision: Decision, feedback: Option<String>| {
        let at = asked.iter().position(|asked| matches!(asked, Asked::Approval(_)));
        match at.and_then(|at| asked.remove(at)) {
            Some(Asked::Approval(request_id)) => Typed::Send(Command::ApprovalResponse {
                request_id,
                decision,
                feedback,
            }),
            _ => Typed::Say("no permission request is waiting".to_string()),
        }
    };
    match name {
        "approve" => approval(Decision::Allow, None),
        "session" => approval(Decision::Session, None),
        "always" => approval(Decision::Always, None),
        "deny" => approval(
            Decision::Deny,
            Some(rest.to_string()).filter(|note| !note.is_empty()),
        ),
        "answer" => {
            let at = asked.iter().position(|asked| matches!(asked, Asked::Ui(..)));
            let Some(Asked::Ui(request_id, request)) = at.and_then(|at| asked.get(at)) else {
                return Typed::Say("no question is waiting".to_string());
            };
            match answer(request, rest) {
                Ok(value) => {
                    let request_id = request_id.clone();
                    asked.remove(at.unwrap_or_default());
                    Typed::Send(Command::UiResponse { request_id, value })
                }
                Err(e) => Typed::Say(e),
            }
        }
        "abort" => Typed::Send(Command::Abort),
        "continue" => Typed::Send(Command::ContinueRun),
        "kill" => Typed::Send(Command::Stop { now: true }),
        "detach" => Typed::Detach,
        "help" => Typed::Say(ATTACH_HELP.to_string()),
        other => Typed::Say(format!("no command /{other}; /help lists them")),
    }
}

/// `azazel attach <id>`: the background session in the full UI, or with `--plain` (and where
/// there is no terminal) as lines.
pub async fn attach(cli: &Cli, paths: Paths, wanted: &str) -> u8 {
    let entry = match find_running(cli, &paths, wanted).await {
        Ok(entry) => entry,
        Err(e) => {
            eprintln!("azazel: {e}");
            return EXIT_FAILED;
        }
    };
    let client = match connect(&entry).await {
        Ok(client) => client,
        Err(e) => {
            eprintln!("azazel: {e}");
            return EXIT_FAILED;
        }
    };
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if cli.plain || !terminal {
        return line_client(client).await;
    }
    let opened = match crate::tui::env_here(cli, paths) {
        Ok(env) => crate::tui::Remote::open(client, crate::tui::Place::Background { id: entry.id })
            .await
            .map(|remote| (remote, env)),
        Err(e) => Err(format!("{e:#}")),
    };
    match opened {
        Ok((remote, env)) => crate::tui::run_remote(remote, env, None).await,
        Err(e) => {
            eprintln!("azazel: {e}");
            EXIT_FAILED
        }
    }
}

/// `azazel rpc --attach <id>`: standard input and output become the connection to a background
/// session, for a program that speaks the protocol (and for azazel on another machine, over SSH).
pub async fn bridge(cli: &Cli, paths: &Paths, wanted: &str) -> u8 {
    let connected = match find_running(cli, paths, wanted).await {
        Ok(entry) => socket::connect(&entry.socket)
            .await
            .map_err(|e| format!("cannot reach the background session {}: {e}", entry.id)),
        Err(e) => Err(e),
    };
    let (mut reader, mut writer) = match connected {
        Ok(connected) => connected,
        Err(e) => {
            eprintln!("azazel: {e}");
            return EXIT_FAILED;
        }
    };
    let (mut input, mut output) = (tokio::io::stdin(), tokio::io::stdout());
    // Whichever side ends first ends the bridge: the session went away, or the program did.
    tokio::select! {
        _ = tokio::io::copy(&mut input, &mut writer) => {}
        _ = async {
            let mut buffer = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    // Written at once: a line that waits in a buffer is a frame that is late.
                    Ok(read) => {
                        if output.write_all(&buffer[..read]).await.is_err() || output.flush().await.is_err() {
                            break;
                        }
                    }
                }
            }
        } => {}
    }
    EXIT_OK
}

/// A background session as lines: what it does is printed as it happens, and each line typed
/// is a message or an answer for it.
pub async fn line_client(client: Client) -> u8 {
    let mut frames = client.frames();
    let attached = client
        .send(Command::Attach {
            pid: Some(std::process::id()),
            replay: Replay::Missed,
        })
        .await;
    let attached: Attached = match attached.map(serde_json::from_value) {
        Ok(Ok(attached)) => attached,
        Ok(Err(e)) => {
            eprintln!("azazel: the background session answered something else than expected: {e}");
            return EXIT_FAILED;
        }
        Err(e) => {
            eprintln!("azazel: {e}");
            return EXIT_FAILED;
        }
    };
    eprintln!(
        "attached to {} (session {}); /help says what can be typed, /detach leaves it running",
        attached.host, attached.session_id
    );
    // A thread of its own: a read of the terminal cannot be given up, and must not keep the
    // process from ending.
    let (lines, mut typed_lines) = mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let mut printer = Printer::new(std::io::stdout(), true);
    let mut asked: VecDeque<Asked> = VecDeque::new();
    let mut interrupt = std::pin::pin!(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            frame = frames.next() => match frame {
                Some(frame) => {
                    match &frame {
                        Frame::ApprovalRequest { request_id, .. } => {
                            asked.push_back(Asked::Approval(request_id.clone()));
                        }
                        Frame::UiRequest { request_id, request }
                            if !matches!(request, UiRequest::Notify { .. }) =>
                        {
                            asked.push_back(Asked::Ui(request_id.clone(), request.clone()));
                        }
                        // Another client answered it, or whoever asked gave up.
                        Frame::RequestClosed { request_id } => asked.retain(|asked| match asked {
                            Asked::Approval(id) | Asked::Ui(id, _) => id != request_id,
                        }),
                        _ => {}
                    }
                    printer.frame(&frame);
                }
                None => {
                    printer.line("the background session has ended");
                    return EXIT_OK;
                }
            },
            line = typed_lines.recv() => match line.as_deref().map(|line| typed(line, &mut asked)) {
                Some(Typed::Send(command)) => match client.send(command).await {
                    Ok(_) | Err(Error::Closed) => {}
                    Err(e) => printer.line(&e.to_string()),
                },
                Some(Typed::Say(said)) if said.is_empty() => {}
                Some(Typed::Say(said)) => printer.line(&said),
                // The end of the input leaves the session as `/detach` does.
                Some(Typed::Detach) | None => break,
            },
            _ = &mut interrupt => break,
        }
    }
    printer.line(&format!("detached; {} goes on in the background", attached.host));
    let _ = client.shutdown().await;
    EXIT_OK
}

/// `/bg <prompt>`: the prompt runs in the background, in a copy of the open session.
pub struct BgExt {
    pub paths: Paths,
    pub permissions: Arc<PermissionState>,
    /// The options this run was started with that the background one is to have too.
    pub carried: Vec<OsString>,
    /// The run keeps no session (`--no-session`): there is nothing to copy.
    pub ephemeral: bool,
}

struct BgCommand {
    paths: Paths,
    permissions: Arc<PermissionState>,
    carried: Vec<OsString>,
    ephemeral: bool,
}

impl BgCommand {
    async fn start(&self, prompt: &str, ctx: &ExtCtx) -> Result<String, String> {
        if prompt.trim().is_empty() {
            return Err("usage: /bg <what to do in the background>".to_string());
        }
        if self.ephemeral {
            return Err(
                "this run keeps no session (--no-session), so there is none to continue in the background"
                    .to_string(),
            );
        }
        let session = ctx
            .services
            .get::<SessionStore>()
            .ok_or("there is no open session")?;
        // A copy, so that this process and the background one never write the same file.
        let fork = session
            .fork(&self.paths.sessions_dir(), None)
            .map_err(|e| format!("could not copy the session: {e}"))?;
        let file = fork.path();
        drop(fork);
        let mut args = self.carried.clone();
        args.extend(["--cwd".into(), self.paths.cwd.clone().into()]);
        args.extend(["--session".into(), file.into()]);
        args.extend(["--mode".into(), self.permissions.mode().to_string().into()]);
        // The background session goes on with the model and thinking level of this one.
        if let Some(lead) = ctx.services.get::<Team>().and_then(|team| team.lead()) {
            let status = lead.status();
            for spec in &status.chain {
                args.extend(["--model".into(), spec.to_string().into()]);
            }
            if let Some(level) = status.thinking_set {
                args.extend(["--thinking".into(), level.to_string().into()]);
            }
        }
        let id = new_id(&self.paths.home);
        args.extend(["host", "--id", &id, "--"].map(OsString::from));
        args.push(prompt.trim().into());
        start(&self.paths.home, args, &id).await?;
        Ok(id)
    }
}

#[async_trait]
impl CommandHandler for BgCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        match self.start(args, ctx).await {
            Ok(id) => CommandOutcome::Show(format!(
                "started background session {id} on a copy of this session; `azazel attach {id}` \
                 connects to it, `azazel ps` lists what is running"
            )),
            Err(e) => CommandOutcome::Error(e),
        }
    }
}

impl Extension for BgExt {
    fn id(&self) -> &str {
        "background"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.command(SlashCommand {
            name: "bg".to_string(),
            description: "Run a prompt in the background, in a copy of this session".to_string(),
            argument_hint: Some("<prompt>".to_string()),
            handler: Arc::new(BgCommand {
                paths: self.paths.clone(),
                permissions: self.permissions.clone(),
                carried: self.carried.clone(),
                ephemeral: self.ephemeral,
            }),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[cfg(windows)]
    #[test]
    fn arguments_are_quoted_for_a_windows_command_line() {
        assert_eq!(quoted("plain"), "plain");
        assert_eq!(quoted(""), "\"\"");
        assert_eq!(quoted("two words"), "\"two words\"");
        assert_eq!(quoted(r"C:\dir\"), r"C:\dir\");
        assert_eq!(quoted(r"C:\my dir\"), r#""C:\my dir\\""#);
        assert_eq!(quoted(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quoted(r#"a\"b c"#), r#""a\\\"b c""#);
    }

    #[test]
    fn typed_lines_are_messages_and_answers() {
        let mut asked = VecDeque::from([
            Asked::Approval("r1".into()),
            Asked::Ui(
                "r2".into(),
                UiRequest::Ask(azazel_protocol::Question {
                    question: "Which?".into(),
                    expects: Some(AskKind::Select),
                    options: vec!["red".into(), "blue".into()],
                    ..Default::default()
                }),
            ),
        ]);
        let sent = |typed: Typed| match typed {
            Typed::Send(command) => serde_json::to_value(command).unwrap(),
            Typed::Say(said) => json!({ "said": said }),
            Typed::Detach => json!("detach"),
        };
        assert_eq!(
            sent(typed("fix it", &mut asked)),
            json!({ "type": "prompt", "text": "fix it" })
        );
        assert_eq!(
            sent(typed("/answer green", &mut asked)),
            json!({ "said": "`green` is not one of the options" })
        );
        assert_eq!(
            sent(typed("/answer 2", &mut asked)),
            json!({ "type": "ui_response", "request_id": "r2", "value": ["blue"] })
        );
        assert_eq!(
            sent(typed("/answer 1", &mut asked)),
            json!({ "said": "no question is waiting" })
        );
        assert_eq!(
            sent(typed("/deny use cargo instead", &mut asked)),
            json!({ "type": "approval_response", "request_id": "r1", "decision": "deny",
                    "feedback": "use cargo instead" })
        );
        assert_eq!(
            sent(typed("/approve", &mut asked)),
            json!({ "said": "no permission request is waiting" })
        );
        assert_eq!(sent(typed("/detach", &mut asked)), json!("detach"));
        assert_eq!(
            sent(typed("/kill", &mut asked)),
            json!({ "type": "stop", "now": true })
        );
    }

    #[test]
    fn answers_take_the_shape_each_question_expects() {
        let options = vec!["red".to_string(), "blue".to_string()];
        let select = UiRequest::Select {
            prompt: "?".into(),
            options: options.clone(),
        };
        assert_eq!(answer(&select, "blue"), Ok(json!(1)));
        assert_eq!(answer(&select, "1"), Ok(json!(0)));
        assert!(answer(&select, "3").is_err());
        let question = |expects: AskKind, allow_freeform: bool| azazel_protocol::Question {
            question: "?".into(),
            expects: Some(expects),
            options: match expects {
                AskKind::Select | AskKind::Multiselect => options.clone(),
                AskKind::Text | AskKind::Confirm => Vec::new(),
            },
            multi: expects == AskKind::Multiselect,
            allow_freeform,
            ..Default::default()
        };
        let ask = |expects: AskKind, allow_freeform: bool| UiRequest::Ask(question(expects, allow_freeform));
        // A form takes one answer per question, each as the question alone would take it.
        let form = UiRequest::Form {
            questions: vec![
                question(AskKind::Select, false),
                question(AskKind::Confirm, false),
                question(AskKind::Text, false),
            ],
        };
        assert_eq!(
            answer(&form, "2; yes; because"),
            Ok(json!([["blue"], true, "because"]))
        );
        assert_eq!(answer(&form, "red;;"), Ok(json!([["red"], null, null])));
        assert_eq!(
            answer(&form, "red; perhaps; x"),
            Err("2: answer yes or no".to_string())
        );
        assert!(answer(&form, "red").is_err());
        assert_eq!(
            answer(&ask(AskKind::Multiselect, false), "2, red"),
            Ok(json!(["blue", "red"]))
        );
        assert_eq!(answer(&ask(AskKind::Select, false), "1"), Ok(json!(["red"])));
        assert!(answer(&ask(AskKind::Select, false), "green").is_err());
        // Where an answer may be typed, what is no option is the answer.
        assert_eq!(answer(&ask(AskKind::Select, true), "green"), Ok(json!(["green"])));
        assert_eq!(answer(&ask(AskKind::Select, true), "blue"), Ok(json!(["blue"])));
        assert_eq!(answer(&ask(AskKind::Text, false), "1"), Ok(json!("1")));
        assert_eq!(answer(&ask(AskKind::Confirm, false), "y"), Ok(json!(true)));
        assert_eq!(answer(&ask(AskKind::Confirm, false), "No"), Ok(json!(false)));
        assert!(answer(&ask(AskKind::Confirm, false), "perhaps").is_err());
        let confirm = UiRequest::Confirm { prompt: "?".into() };
        assert_eq!(answer(&confirm, "Y"), Ok(json!(true)));
        assert_eq!(answer(&confirm, "no"), Ok(json!(false)));
        assert_eq!(answer(&confirm, "maybe"), Err("answer yes or no".to_string()));
        let plan = UiRequest::Plan {
            path: "p".into(),
            plan: "x".into(),
        };
        assert_eq!(answer(&plan, "yes"), Ok(json!(true)));
        assert_eq!(answer(&plan, "split step 2"), Ok(json!("split step 2")));
        assert_eq!(answer(&plan, ""), Ok(Value::Null));
    }
}
