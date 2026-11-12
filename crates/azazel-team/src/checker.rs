//! The checker: decides whether an objective is met by running the project's build and tests and,
//! when those pass, asking an independent verifier agent.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use azazel_agent::{AgentInfo, Services};
use azazel_core::process::{AgentEnv, RunOptions, Shell, run};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::detect::verify_commands;
use crate::team::{MemberState, SpawnRequest, Team};

/// Lines of a command's output that are kept, counted from the end.
pub const TAIL_LINES: usize = 200;
/// The failure recorded when the verifier's report carries no readable verdict.
pub const NO_VERDICT: &str = "verifier produced no verdict";
const VERIFIER: &str = "verifier";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub command: String,
    /// `None` when the command was killed or could not be started.
    pub code: Option<i32>,
    pub timed_out: bool,
    /// The end of the command's output.
    pub tail: String,
}

impl CommandResult {
    pub fn passed(&self) -> bool {
        self.code == Some(0)
    }

    fn outcome(&self) -> String {
        match (self.code, self.timed_out) {
            (_, true) => "timed out".to_string(),
            (Some(code), _) => format!("exit code {code}"),
            (None, _) => "did not run to the end".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Failure {
    pub what: String,
    #[serde(default)]
    pub evidence: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The commands that ran, in order; the last one is the failing one when a command failed.
    pub commands: Vec<CommandResult>,
    /// Empty when the objective is met.
    pub failures: Vec<Failure>,
    /// Id of the verifier agent, when one ran.
    pub verifier: Option<String>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    /// The outcome as text: each command with its result, then the failures with their evidence.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for result in &self.commands {
            out.push_str(&format!("$ {} ({})\n", result.command, result.outcome()));
        }
        if self.passed() {
            out.push_str("verified");
            return out;
        }
        out.push_str("not verified:\n");
        for failure in &self.failures {
            out.push_str(&format!("- {}\n", failure.what));
            if !failure.evidence.trim().is_empty() {
                for line in failure.evidence.trim_end().lines() {
                    out.push_str(&format!("    {line}\n"));
                }
            }
        }
        out.trim_end().to_string()
    }
}

/// What the checker is asked to judge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Claim {
    pub objective: String,
    /// What the working agent says it did.
    pub summary: String,
    /// What the working agent offers as proof.
    pub evidence: String,
    /// The diffstat and diff of the project since the work began; empty when unknown.
    pub changes: String,
}

#[derive(Deserialize)]
struct Verdict {
    pass: bool,
    #[serde(default)]
    failures: Vec<Failure>,
}

/// Reads the verdict from the last fenced block of a verifier's report: the failures, none meaning
/// a pass. `None` when the report has no block that parses.
pub fn parse_verdict(report: &str) -> Option<Vec<Failure>> {
    let close = report.rfind("```")?;
    let open = report[..close].rfind("```")?;
    let block = &report[open + 3..close];
    // The opening fence may carry a language tag.
    let json = block.find('{').map(|start| &block[start..])?;
    let verdict: Verdict = serde_json::from_str(json.trim()).ok()?;
    if verdict.pass {
        return Some(Vec::new());
    }
    if verdict.failures.is_empty() {
        return Some(vec![Failure {
            what: "the verifier judged the objective not met without saying why".to_string(),
            evidence: String::new(),
        }]);
    }
    Some(verdict.failures)
}

pub struct Checker {
    /// `verify.commands`; empty means the commands are detected from the project's files.
    pub commands: Vec<String>,
    /// Whether a verifier agent judges the work once the commands pass.
    pub use_agent: bool,
    /// How long each command may run.
    pub timeout: Duration,
    pub shell: Shell,
    pub project_root: PathBuf,
    pub services: Arc<Services>,
}

impl Checker {
    /// The commands a check runs: the configured ones, else those the project's files suggest.
    pub fn commands(&self) -> Vec<String> {
        if self.commands.is_empty() {
            verify_commands(&self.project_root)
        } else {
            self.commands.clone()
        }
    }

    /// Runs the commands in order, with the environment every agent process gets, and stops at
    /// the first one that fails.
    pub async fn run_commands(&self, cancel: &CancellationToken) -> Vec<CommandResult> {
        let env = self
            .services
            .get::<AgentEnv>()
            .map(|env| env.vars())
            .unwrap_or_default();
        let mut results = Vec::new();
        for command in self.commands() {
            let mut options = RunOptions::new(self.project_root.clone());
            options.env = env.clone();
            options.timeout = Some(self.timeout);
            options.cancel = cancel.clone();
            options.keep = (0, TAIL_LINES);
            let result = match run(&self.shell, &command, options).await {
                Ok(output) => CommandResult {
                    command,
                    code: output.code,
                    timed_out: output.timed_out,
                    tail: output.output.render(),
                },
                Err(e) => CommandResult {
                    command,
                    code: None,
                    timed_out: false,
                    tail: format!("could not start: {e}"),
                },
            };
            let passed = result.passed();
            results.push(result);
            if !passed {
                break;
            }
        }
        results
    }

    fn verifier_prompt(claim: &Claim, commands: &[CommandResult]) -> String {
        let mut prompt = format!("Objective:\n{}\n\n", claim.objective.trim());
        prompt.push_str("The agent that did the work says the objective is met.\n");
        if !claim.summary.trim().is_empty() {
            prompt.push_str(&format!("Its summary:\n{}\n\n", claim.summary.trim()));
        }
        if !claim.evidence.trim().is_empty() {
            prompt.push_str(&format!("Its evidence:\n{}\n\n", claim.evidence.trim()));
        }
        if commands.is_empty() {
            prompt.push_str(
                "No build or test command is known for this project: find the project's build and tests and \
                 run them yourself.\n\n",
            );
        } else {
            prompt.push_str("These commands were just run and passed:\n");
            for result in commands {
                prompt.push_str(&format!("- `{}` ({})\n", result.command, result.outcome()));
            }
            prompt.push('\n');
        }
        if claim.changes.trim().is_empty() {
            prompt.push_str(
                "What changed since the work began is not recorded; inspect the project yourself.\n\n",
            );
        } else {
            prompt.push_str(&format!(
                "Changes since the work began:\n{}\n\n",
                claim.changes.trim_end()
            ));
        }
        prompt.push_str(
            "Decide for yourself whether the objective is met. End your report with a fenced JSON block and \
             nothing after it:\n```json\n{\"pass\": true, \"failures\": []}\n```\nor, when it is not met, \
             `\"pass\": false` and one `{\"what\": \"…\", \"evidence\": \"…\"}` entry per failure.",
        );
        prompt
    }

    /// Judges `claim` for `parent`: the commands first; when they pass and a verifier is wanted,
    /// a verifier agent started under `parent`.
    pub async fn check(&self, parent: &AgentInfo, claim: &Claim, cancel: &CancellationToken) -> Report {
        let commands = self.run_commands(cancel).await;
        let mut report = Report {
            commands,
            ..Report::default()
        };
        if let Some(failed) = report.commands.last().filter(|result| !result.passed()) {
            report.failures.push(Failure {
                what: format!("`{}` failed ({})", failed.command, failed.outcome()),
                evidence: failed.tail.clone(),
            });
            return report;
        }
        if !self.use_agent {
            return report;
        }
        let failed = |what: String| {
            vec![Failure {
                what,
                evidence: String::new(),
            }]
        };
        let Some(team) = self.services.get::<Team>() else {
            report.failures = failed("no verifier agent is available in this session".to_string());
            return report;
        };
        let request = SpawnRequest {
            def: VERIFIER.to_string(),
            prompt: Self::verifier_prompt(claim, &report.commands),
            name: None,
            model: None,
            isolation: None,
            tools: None,
        };
        let id = match team.spawn(parent, request, false).await {
            Ok(id) => id,
            Err(why) => {
                report.failures = failed(format!("the verifier agent could not be started: {why}"));
                return report;
            }
        };
        report.verifier = Some(id.clone());
        let result = team
            .wait_all(std::slice::from_ref(&id), cancel)
            .await
            .and_then(|mut results| results.pop());
        report.failures = match result {
            None => failed("the check was cancelled".to_string()),
            Some(result) if result.state != MemberState::Done => {
                vec![Failure {
                    what: format!("the verifier agent {}", result.state),
                    evidence: result.report,
                }]
            }
            Some(result) => parse_verdict(&result.report).unwrap_or_else(|| {
                vec![Failure {
                    what: NO_VERDICT.to_string(),
                    evidence: format!("full report: agent://{id}"),
                }]
            }),
        };
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_verdict_is_the_last_fenced_block_of_the_report() {
        let passed = "All good.\n```json\n{\"pass\": true, \"failures\": []}\n```";
        assert_eq!(parse_verdict(passed), Some(Vec::new()));

        let failed = "Looked at it.\n```\nsome output\n```\nVerdict:\n```json\n{\"pass\": false, \"failures\": \
                      [{\"what\": \"test x fails\", \"evidence\": \"src/lib.rs:3\"}, {\"what\": \"no docs\"}]}\n```\n";
        assert_eq!(
            parse_verdict(failed),
            Some(vec![
                Failure {
                    what: "test x fails".into(),
                    evidence: "src/lib.rs:3".into()
                },
                Failure {
                    what: "no docs".into(),
                    evidence: String::new()
                },
            ])
        );
        // A failure without reasons still fails.
        let bare = parse_verdict("```json\n{\"pass\": false}\n```").unwrap();
        assert_eq!(bare.len(), 1);

        for unreadable in [
            "",
            "VERDICT: PASS",
            "```json\n{\"pass\": tru\n```",
            "```\nnot json\n```",
            "```json\n{\"pass\": true}",
        ] {
            assert_eq!(parse_verdict(unreadable), None, "{unreadable}");
        }
    }

    #[test]
    fn a_report_reads_as_commands_then_failures() {
        let command = |command: &str, code| CommandResult {
            command: command.into(),
            code,
            timed_out: false,
            tail: String::new(),
        };
        let passed = Report {
            commands: vec![command("cargo build", Some(0)), command("cargo test", Some(0))],
            ..Report::default()
        };
        assert_eq!(
            passed.render(),
            "$ cargo build (exit code 0)\n$ cargo test (exit code 0)\nverified"
        );

        let failed = Report {
            commands: vec![command("cargo test", Some(101))],
            failures: vec![Failure {
                what: "`cargo test` failed (exit code 101)".into(),
                evidence: "test adds ... FAILED\n".into(),
            }],
            verifier: None,
        };
        assert_eq!(
            failed.render(),
            "$ cargo test (exit code 101)\nnot verified:\n- `cargo test` failed (exit code 101)\n    test adds ... FAILED"
        );
        let killed = CommandResult {
            command: "make".into(),
            code: None,
            timed_out: true,
            tail: String::new(),
        };
        assert_eq!(killed.outcome(), "timed out");
    }
}
