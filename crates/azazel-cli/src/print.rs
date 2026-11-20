//! Print mode: one prompt in, the result out, no interactive UI.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use azazel_agent::AgentEvent;
use azazel_ai::{Block, StopReason};
use azazel_core::media::{MAX_IMAGE_SIDE, image_block, image_mime};
use azazel_core::{FileTracker, StartSource};
use azazel_team::{Team, TeamEvent};
use serde_json::Value;

use crate::app::{Active, App};
use crate::cli::OutputFormat;

pub const EXIT_OK: u8 = 0;
pub const EXIT_FAILED: u8 = 1;
pub const EXIT_USAGE: u8 = 2;
/// `azazel goal` ended without the goal being verified.
pub const EXIT_GOAL_UNMET: u8 = 2;
pub const EXIT_ABORTED: u8 = 130;

/// An event as one line of the JSON output: the event's own fields plus the agent that emitted it.
pub fn event_json(agent: &str, event: &AgentEvent) -> Value {
    let mut value = serde_json::to_value(event).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        map.insert("agent".to_string(), Value::String(agent.to_string()));
    }
    value
}

/// The prompt with anything piped on stdin appended.
pub(crate) fn with_piped_stdin(prompt: &str) -> String {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return prompt.to_string();
    }
    let mut piped = String::new();
    if stdin.lock().read_to_string(&mut piped).is_err() || piped.trim().is_empty() {
        return prompt.to_string();
    }
    if prompt.trim().is_empty() {
        return piped;
    }
    format!("{prompt}\n\n{piped}")
}

/// Files `-f/--file` takes in one run.
pub const MAX_FILES: usize = 100;
/// Size above which a text file of `-f` is refused, as an `@path` mention leaves it out.
/// The longest a `-p` run waits for background scripts that have not woken the lead.
const PRINT_SCRIPT_WAIT_SECS: u64 = 7200;

const MAX_TEXT_BYTES: u64 = 256 * 1024;
/// Size above which an image of `-f` is refused.
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// The files of `-f/--file`: texts inlined in the prompt as `@path` mentions are, images
/// attached.
#[derive(Debug, Default)]
pub struct Files {
    /// The `<file>` blocks appended to the prompt.
    text: String,
    images: Vec<Block>,
    /// What each text file held, for the lead to count as read.
    read: Vec<(PathBuf, Vec<u8>)>,
}

/// Reads the files of `-f`, given relative to `cwd`. A file that cannot be given to the model
/// is a mistake of the command line, said before anything starts.
pub fn files(given: &[PathBuf], cwd: &Path) -> Result<Files, String> {
    if given.len() > MAX_FILES {
        return Err(format!(
            "-f takes at most {MAX_FILES} files; {} were given",
            given.len()
        ));
    }
    let mut files = Files::default();
    for name in given {
        let path = cwd.join(name);
        let shown = name.display();
        let meta = std::fs::metadata(&path).map_err(|e| format!("-f {shown}: {e}"))?;
        if !meta.is_file() {
            return Err(format!("-f {shown}: not a file"));
        }
        if meta.len() > MAX_IMAGE_BYTES {
            return Err(format!("-f {shown}: larger than 20 MB"));
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("-f {shown}: {e}"))?;
        if image_mime(&bytes).is_some() {
            files
                .images
                .push(image_block(&bytes, MAX_IMAGE_SIDE).map_err(|e| format!("-f {shown}: {e}"))?);
            continue;
        }
        if meta.len() > MAX_TEXT_BYTES {
            return Err(format!(
                "-f {shown}: a text file larger than 256 KB; ask for it to be read instead"
            ));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| format!("-f {shown}: neither text nor a PNG, JPEG, WebP or GIF image"))?;
        files.text.push_str(&format!(
            "\n\n<file path=\"{shown}\">\n{}\n</file>",
            text.trim_end()
        ));
        files.read.push((path, bytes));
    }
    Ok(files)
}

/// Says, at the end of a `--worktree` run, where it happened: a line on standard error, or with
/// `--output json` a last object on standard output.
pub fn worktree_line(created: &crate::worktree::Created, format: OutputFormat) {
    match format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::json!({
                "type": "worktree",
                "path": created.path,
                "branch": created.branch,
            })
        ),
        OutputFormat::Text => eprintln!(
            "azazel: worktree {} (branch {})",
            created.path.display(),
            created.branch
        ),
    }
}

/// What a run in print mode works on.
pub enum What<'a> {
    /// A prompt, with anything piped on standard input appended, and the files of `-f`.
    Prompt(&'a str, Files),
    /// The run the session's last process did not finish (`--continue-run`).
    ContinueRun,
}

pub async fn run(app: &App, active: &Active, what: What<'_>, format: OutputFormat) -> u8 {
    let prompt = match what {
        What::Prompt(prompt, files) => {
            let prompt = with_piped_stdin(prompt);
            if prompt.trim().is_empty() {
                eprintln!(
                    "azazel: -p needs a prompt: azazel -p \"what to do\", or text piped on standard input"
                );
                app.finish(active).await;
                return EXIT_USAGE;
            }
            if let Some(tracker) = app.registry.services.get::<FileTracker>() {
                for (path, bytes) in &files.read {
                    tracker.record(&active.lead.id, path, bytes);
                }
            }
            Some((prompt + &files.text, files.images))
        }
        What::ContinueRun => None,
    };
    app.start(if active.resumed {
        StartSource::Resume
    } else {
        StartSource::Startup
    })
    .await;
    let mut events = active.lead.subscribe();
    let started = match prompt {
        Some((prompt, images)) => app
            .submit(active, prompt, images)
            .await
            .map_err(|reason| format!("prompt rejected: {reason}")),
        None => app.continue_run(active).map(|_| ()),
    };
    if let Err(why) = started {
        eprintln!("{why}");
        app.finish(active).await;
        return EXIT_FAILED;
    }
    // Tests ask a side question while the lead works on the prompt.
    #[cfg(feature = "test-support")]
    let side_question = crate::side::ask_from_env(
        app.registry.services.clone(),
        active.session.clone(),
        active.lead.clone(),
    );
    let lead = active.lead.id.clone();
    let team = app.registry.services.get::<Team>();
    let mut team_events = team.as_ref().map(|team| team.subscribe());
    let mut stdout = std::io::stdout();
    let mut answer = String::new();
    // Tests send follow-up messages through the queue the terminal UI holds them in.
    #[cfg(feature = "test-support")]
    let mut queued = crate::tui::queue_from_env();
    // Tests run a `/loop` through the rounds the terminal UI would send.
    #[cfg(feature = "test-support")]
    let mut looped = crate::loops::from_env(app.config.schedule.loop_max);
    let mut interrupt = std::pin::pin!(tokio::signal::ctrl_c());
    let mut interrupted = false;
    // When the run stops waiting for background scripts that have not woken the lead.
    let mut scripts_deadline: Option<tokio::time::Instant> = None;
    let mut code = loop {
        let event = tokio::select! {
            event = events.recv() => event,
            _ = async {
                match scripts_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                eprintln!("azazel: stopped waiting for background scripts after the print-mode limit of 2 hours");
                break EXIT_OK;
            }
            // Sub-agents' events go out tagged with their own ids.
            Some(event) = async { team_events.as_mut()?.recv().await.ok() } => {
                if let (OutputFormat::Json, TeamEvent::Agent { id, event }) = (format, event) {
                    let _ = writeln!(stdout, "{}", event_json(&id, &event));
                }
                continue;
            }
            _ = &mut interrupt, if !interrupted => {
                interrupted = true;
                active.lead.abort();
                continue;
            }
        };
        let Some(event) = event else {
            eprintln!("the agent stopped unexpectedly");
            break EXIT_FAILED;
        };
        if format == OutputFormat::Json {
            // What sub-agents did before this event of the lead goes out first, so that a call
            // that waited for them ends after them in the output too.
            while let Some(Ok(earlier)) = team_events.as_mut().map(|events| events.try_recv()) {
                if let TeamEvent::Agent { id, event } = earlier {
                    let _ = writeln!(stdout, "{}", event_json(&id, &event));
                }
            }
            let _ = writeln!(stdout, "{}", event_json(&lead, &event));
        }
        match event {
            AgentEvent::MessageEnd { message } => answer = message.text(),
            AgentEvent::Idle {
                stop: StopReason::Aborted,
            } => break EXIT_ABORTED,
            AgentEvent::Idle { .. } => {
                // Agents still at work will report to the lead, which then goes on.
                let pending = team.as_ref().is_some_and(|team| {
                    team.list()
                        .iter()
                        .any(|member| member.id != lead && member.state.is_active())
                });
                // A background script wakes the lead when it ends, and that is a turn of its own;
                // the script's own wall-clock cap bounds the wait.
                let scripts = app
                    .registry
                    .services
                    .get::<azazel_tools::Jobs>()
                    .is_some_and(|jobs| jobs.running_tasks() > 0);
                scripts_deadline = match (scripts, scripts_deadline) {
                    (false, _) => None,
                    (true, Some(deadline)) => Some(deadline),
                    (true, None) => {
                        // The script's own cap ends it first (and wakes the lead); with no cap
                        // the run still ends after two hours.
                        let limit = Duration::from_secs(PRINT_SCRIPT_WAIT_SECS);
                        let cap = Duration::from_secs(app.config.tools.eval.background_max_secs);
                        let wait = if cap.is_zero() { limit } else { cap.min(limit) };
                        Some(tokio::time::Instant::now() + wait + Duration::from_secs(10))
                    }
                };
                if !pending && !scripts {
                    #[cfg(feature = "test-support")]
                    if let Some((text, images)) = queued.next(app.config.tui.queue_mode) {
                        match app.submit(active, text, images).await {
                            Ok(()) => continue,
                            Err(reason) => {
                                eprintln!("prompt rejected: {reason}");
                                break EXIT_FAILED;
                            }
                        }
                    }
                    #[cfg(feature = "test-support")]
                    if let Some(looped) = &mut looped {
                        let side = app.registry.services.get::<azazel_core::side::Side>();
                        match crate::loops::drive(looped, side, &active.lead, &active.session).await {
                            Ok(prompt) => match app.submit(active, prompt, Vec::new()).await {
                                Ok(()) => continue,
                                Err(reason) => {
                                    eprintln!("prompt rejected: {reason}");
                                    break EXIT_FAILED;
                                }
                            },
                            Err(why) => eprintln!("{why}"),
                        }
                    }
                    break EXIT_OK;
                }
            }
            AgentEvent::Failed { error } => {
                eprintln!("{error}");
                break EXIT_FAILED;
            }
            // Both change what the rest of the run costs or runs on, so they are said even here.
            AgentEvent::Account {
                provider,
                from,
                to,
                until,
                limit,
            } if format == OutputFormat::Text => {
                eprintln!(
                    "{}",
                    crate::accounts::switch_line(&provider, &from, &to, until, &limit)
                );
            }
            AgentEvent::TierDropped { model, tier } if format == OutputFormat::Text => {
                eprintln!("{}", crate::routing::tier_dropped_line(&model, tier));
            }
            // A stall is a long silence: say that the run has not hung.
            AgentEvent::Retry {
                attempt,
                model,
                stalled_secs: stalled_secs @ Some(_),
                ..
            } if format == OutputFormat::Text => {
                eprintln!(
                    "{}",
                    crate::routing::retry_line(&model.to_string(), attempt, stalled_secs)
                );
            }
            _ => {}
        }
    };
    // A typed run prints the value it reported and nothing else; without one it has failed.
    let reported = app.report.as_ref().map(|report| report.value());
    match reported {
        Some(Some(value)) if code == EXIT_OK && format == OutputFormat::Text => {
            let _ = writeln!(stdout, "{value}");
        }
        Some(None) if code == EXIT_OK => {
            eprintln!("azazel: the run ended without a result that matches --json-schema");
            code = EXIT_FAILED;
        }
        None if format == OutputFormat::Text && code == EXIT_OK => {
            let _ = writeln!(stdout, "{answer}");
        }
        _ => {}
    }
    let _ = stdout.flush();
    #[cfg(feature = "test-support")]
    if let Some(asked) = side_question {
        let _ = asked.await;
    }
    app.finish(active).await;
    code
}
