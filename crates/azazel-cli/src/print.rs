//! Print mode: one prompt in, the result out, no interactive UI.

use std::io::{IsTerminal, Read, Write};

use azazel_agent::AgentEvent;
use azazel_ai::StopReason;
use azazel_core::StartSource;
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
fn with_piped_stdin(prompt: &str) -> String {
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

pub async fn run(app: &App, active: &Active, prompt: &str, format: OutputFormat) -> u8 {
    let prompt = with_piped_stdin(prompt);
    if prompt.trim().is_empty() {
        eprintln!("azazel: -p needs a prompt: azazel -p \"what to do\", or text piped on standard input");
        app.finish(active).await;
        return EXIT_USAGE;
    }
    app.start(if active.resumed {
        StartSource::Resume
    } else {
        StartSource::Startup
    })
    .await;
    let mut events = active.lead.subscribe();
    if let Err(reason) = app.submit(active, prompt, Vec::new()).await {
        eprintln!("prompt rejected: {reason}");
        app.finish(active).await;
        return EXIT_FAILED;
    }
    let lead = active.lead.id.clone();
    let team = app.registry.services.get::<Team>();
    let mut team_events = team.as_ref().map(|team| team.subscribe());
    let mut stdout = std::io::stdout();
    let mut answer = String::new();
    let mut interrupt = std::pin::pin!(tokio::signal::ctrl_c());
    let mut interrupted = false;
    let code = loop {
        let event = tokio::select! {
            event = events.recv() => event,
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
                if !pending {
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
            _ => {}
        }
    };
    if format == OutputFormat::Text && code == EXIT_OK {
        let _ = writeln!(stdout, "{answer}");
    }
    let _ = stdout.flush();
    app.finish(active).await;
    code
}
