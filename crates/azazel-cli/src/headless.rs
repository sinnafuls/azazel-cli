//! Sessions run for another program rather than for a person at a terminal: what serving MCP
//! and the Agent Client Protocol share.

use anyhow::Result;
use azazel_agent::AgentEvent;
use azazel_ai::{Block, StopReason};
use azazel_core::Extension;
use azazel_team::{Team, TeamEvent};
use clap::Parser;
use tokio_util::sync::CancellationToken;

use crate::app::{Active, App, discover_paths};
use crate::cli::Cli;

/// Builds the pieces of one more session in this process. The command line the process was
/// started with is its basis: the global flags (`--set`, `--trust`, `--model`, ...) apply, the
/// subcommand does not, and `configure` changes what this session needs different (its
/// directory, its mode). A project is trusted only when it was trusted before or `--trust` was
/// passed: nobody is asked.
pub async fn build_app(
    extensions: &[Box<dyn Extension>],
    configure: impl FnOnce(&mut Cli),
) -> Result<(App, Cli)> {
    let mut cli = Cli::parse();
    cli.command = None;
    cli.prompt.clear();
    // Like print mode: without a model there is nothing a session could do.
    cli.print = true;
    configure(&mut cli);
    let paths = discover_paths(&cli)?;
    let trusted = crate::project_trust(&cli, &paths, false);
    let app = App::build(&cli, paths, trusted, extensions).await?;
    Ok((app, cli))
}

/// How the work a prompt started came to its end.
#[derive(Debug, Clone, PartialEq)]
pub enum Ended {
    /// The lead is idle and no agent it started still works. `answer` is its last message.
    Idle {
        stop: StopReason,
        answer: String,
    },
    Failed(String),
}

/// What a run for another program works on.
pub enum Work {
    /// A prompt; `request_id` is the name its sender gave it.
    Prompt {
        text: String,
        images: Vec<Block>,
        request_id: Option<String>,
    },
    /// The run the session's last process did not finish.
    ContinueRun,
}

/// Sends a prompt to the lead and waits for the end of the work it starts. `on_event` sees
/// every event of the lead and of the agents it starts, with the agent's id. Cancelling `cancel`
/// aborts the work. `Err` is why an extension rejected the prompt.
pub async fn run_prompt(
    app: &App,
    active: &Active,
    text: String,
    images: Vec<Block>,
    cancel: &CancellationToken,
    on_event: &mut (dyn FnMut(&str, &AgentEvent) + Send),
) -> Result<Ended, String> {
    let work = Work::Prompt {
        text,
        images,
        request_id: None,
    };
    run_work(app, active, work, cancel, on_event).await
}

/// [`run_prompt`] for any [`Work`]. `Err` is why the work could not start.
pub async fn run_work(
    app: &App,
    active: &Active,
    work: Work,
    cancel: &CancellationToken,
    on_event: &mut (dyn FnMut(&str, &AgentEvent) + Send),
) -> Result<Ended, String> {
    let mut events = active.lead.subscribe();
    let team = app.registry.services.get::<Team>();
    let mut team_events = team.as_ref().map(|team| team.subscribe());
    match work {
        Work::Prompt {
            text,
            images,
            request_id,
        } => app.submit_as(active, text, images, request_id).await?,
        Work::ContinueRun => {
            app.continue_run(active)?;
        }
    }
    let lead = active.lead.id.clone();
    let mut answer = String::new();
    let mut aborted = false;
    loop {
        let event = tokio::select! {
            event = events.recv() => event,
            Some(event) = async { team_events.as_mut()?.recv().await.ok() } => {
                if let TeamEvent::Agent { id, event } = event {
                    on_event(&id, &event);
                }
                continue;
            }
            () = cancel.cancelled(), if !aborted => {
                aborted = true;
                active.lead.abort();
                continue;
            }
        };
        let Some(event) = event else {
            return Ok(Ended::Failed("the agent stopped unexpectedly".to_string()));
        };
        // What sub-agents did before this event of the lead goes first, so that a call that
        // waited for them ends after them.
        while let Some(Ok(earlier)) = team_events.as_mut().map(|events| events.try_recv()) {
            if let TeamEvent::Agent { id, event } = earlier {
                on_event(&id, &event);
            }
        }
        on_event(&lead, &event);
        match event {
            AgentEvent::MessageEnd { message } => answer = message.text(),
            AgentEvent::Idle { stop } => {
                // Agents still at work will report to the lead, which then goes on.
                let pending = stop != StopReason::Aborted
                    && team.as_ref().is_some_and(|team| {
                        team.list()
                            .iter()
                            .any(|member| member.id != lead && member.state.is_active())
                    });
                if !pending {
                    return Ok(Ended::Idle { stop, answer });
                }
            }
            AgentEvent::Failed { error } => return Ok(Ended::Failed(error)),
            _ => {}
        }
    }
}
