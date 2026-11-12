//! Addresses `read` resolves besides files and web pages: `session://` (saved sessions),
//! `job://` (background jobs), and `pr://` and `issue://` (GitHub, through the `gh` program).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{Access, ToolCtx};
use azazel_ai::{Block, Role};
use azazel_core::process::AgentEnv;
use azazel_core::session::EntryKind;
use azazel_core::transcript::condensed;
use azazel_core::{Scheme, SessionStore, SessionSummary};

use crate::shell::Jobs;

/// Other sessions listed by `session://`.
const LISTED_SESSIONS: usize = 20;
/// Output lines `job://<id>` returns.
const JOB_LINES: usize = 2000;
/// How long a `gh` call may take.
const GH_TIMEOUT: Duration = Duration::from_secs(30);
/// What `pr://` and `issue://` list.
const GH_LIST_LIMIT: &str = "30";

fn open_session(ctx: &ToolCtx) -> Result<Arc<SessionStore>, String> {
    ctx.services
        .get::<SessionStore>()
        .ok_or_else(|| "no session is open".to_string())
}

/// One line for a session in a listing: id, when it was last used, and what it is about.
fn summary_line(summary: &SessionSummary) -> String {
    let about = summary
        .name
        .as_deref()
        .or(summary.first_prompt.as_deref())
        .and_then(|text| text.lines().next())
        .unwrap_or("(no prompt yet)");
    let about: String = about.chars().take(80).collect();
    format!(
        "{}  {}  {} messages  {about}",
        summary.id, summary.updated, summary.messages
    )
}

/// `session://` describes the open session and lists the project's other ones; `session://<id>`
/// is another session's transcript, condensed.
pub struct SessionScheme;

impl SessionScheme {
    fn describe(session: &SessionStore) -> String {
        let header = session.header();
        let (mut prompts, mut replies, mut tool_results) = (0, 0, 0);
        for entry in session.entries() {
            if let EntryKind::Message { message } = entry.kind {
                match message.role {
                    Role::User => prompts += 1,
                    Role::Assistant => replies += 1,
                    Role::Tool => tool_results += 1,
                }
            }
        }
        let mut out = format!("id: {}\nfile: {}\n", header.id, session.path().display());
        if let Some(name) = session.name() {
            out.push_str(&format!("name: {name}\n"));
        }
        out.push_str(&format!(
            "created: {}\ndirectory: {}\n",
            header.timestamp, header.cwd
        ));
        if let Some(parent) = &header.parent_session {
            out.push_str(&format!("continues: {parent}\n"));
        }
        out.push_str(&format!(
            "messages: {prompts} from the user, {replies} from the agent, {tool_results} tool results\n"
        ));
        let others: Vec<String> = session
            .path()
            .parent()
            .map(SessionStore::list)
            .unwrap_or_default()
            .iter()
            .filter(|summary| summary.id != header.id)
            .take(LISTED_SESSIONS)
            .map(summary_line)
            .collect();
        if others.is_empty() {
            out.push_str("\nThis project has no other saved session.");
        } else {
            out.push_str(
                "\nOther sessions of this project, most recently used first (read one with session://<id>):\n",
            );
            out.push_str(&others.join("\n"));
        }
        out
    }

    /// The file of the session whose id starts with `wanted`, among those stored beside `open`.
    fn find(open: &SessionStore, wanted: &str) -> Result<PathBuf, String> {
        let dir = open.path().parent().map(Path::to_path_buf).unwrap_or_default();
        let matches: Vec<SessionSummary> = SessionStore::list(&dir)
            .into_iter()
            .filter(|summary| summary.id.starts_with(wanted))
            .collect();
        match matches.as_slice() {
            [one] => Ok(one.path.clone()),
            [] => Err(format!(
                "no session with id `{wanted}` in this project; session:// lists them"
            )),
            many => Err(format!(
                "`{wanted}` matches {} sessions; give more of the id",
                many.len()
            )),
        }
    }
}

#[async_trait]
impl Scheme for SessionScheme {
    fn scheme(&self) -> &str {
        "session"
    }

    async fn read(&self, rest: &str, ctx: &ToolCtx) -> Result<Vec<Block>, String> {
        let open = open_session(ctx)?;
        let wanted = rest.trim().trim_end_matches('/');
        if wanted.is_empty() {
            return Ok(vec![Block::text(Self::describe(&open))]);
        }
        if !wanted
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!(
                "session://{wanted}: expected session:// or session://<id>"
            ));
        }
        if open.id().starts_with(wanted) {
            return Ok(vec![Block::text(condensed(&open))]);
        }
        let path = Self::find(&open, wanted)?;
        let store = SessionStore::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let text = condensed(&store);
        Ok(vec![Block::text(if text.is_empty() {
            "(the session has no messages)".to_string()
        } else {
            text
        })])
    }
}

/// `job://` lists the background jobs; `job://<id>` is a job's output and state.
pub struct JobScheme(pub Arc<Jobs>);

#[async_trait]
impl Scheme for JobScheme {
    fn scheme(&self) -> &str {
        "job"
    }

    async fn read(&self, rest: &str, _: &ToolCtx) -> Result<Vec<Block>, String> {
        let id = rest.trim().trim_end_matches('/');
        if id.is_empty() {
            let jobs = self.0.list();
            return Ok(vec![Block::text(if jobs.is_empty() {
                "no background jobs".to_string()
            } else {
                jobs.join("\n")
            })]);
        }
        match self.0.output(id, JOB_LINES) {
            Some(output) => Ok(vec![Block::text(output)]),
            None => {
                let known: Vec<String> = self
                    .0
                    .list()
                    .iter()
                    .filter_map(|line| line.split_whitespace().next().map(str::to_string))
                    .collect();
                Err(if known.is_empty() {
                    format!("no job named `{id}`; there are no background jobs")
                } else {
                    format!("no job named `{id}`; the jobs are: {}", known.join(", "))
                })
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GhKind {
    Pr,
    Issue,
}

/// `pr://` and `issue://` list what is open in the repository of the working directory;
/// `pr://<n>` and `issue://<n>` show one with its comments; `pr://<n>/diff` is a pull request's
/// diff. All of it comes from the `gh` program, with the account `gh` is signed in to.
pub struct GhScheme {
    kind: GhKind,
    program: PathBuf,
}

impl GhScheme {
    pub fn pull_requests(program: PathBuf) -> GhScheme {
        GhScheme {
            kind: GhKind::Pr,
            program,
        }
    }

    pub fn issues(program: PathBuf) -> GhScheme {
        GhScheme {
            kind: GhKind::Issue,
            program,
        }
    }

    /// The `gh` arguments for the address `rest`.
    fn arguments(&self, rest: &str) -> Result<Vec<String>, String> {
        let noun = self.scheme();
        let rest = rest.trim().trim_end_matches('/');
        let (number, part) = rest.split_once('/').unwrap_or((rest, ""));
        let usage = || match self.kind {
            GhKind::Pr => format!("pr://{rest}: expected pr://, pr://<number> or pr://<number>/diff"),
            GhKind::Issue => format!("issue://{rest}: expected issue:// or issue://<number>"),
        };
        let owned = |words: &[&str]| words.iter().map(|word| word.to_string()).collect();
        if number.is_empty() && part.is_empty() {
            return Ok(owned(&[noun, "list", "--limit", GH_LIST_LIMIT]));
        }
        // Only digits reach the command line, so an address can never smuggle in an option.
        if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
            return Err(usage());
        }
        match (self.kind, part) {
            (_, "") => Ok(owned(&[noun, "view", number, "--comments"])),
            (GhKind::Pr, "diff") => Ok(owned(&["pr", "diff", number])),
            _ => Err(usage()),
        }
    }
}

#[async_trait]
impl Scheme for GhScheme {
    fn scheme(&self) -> &str {
        match self.kind {
            GhKind::Pr => "pr",
            GhKind::Issue => "issue",
        }
    }

    async fn read(&self, rest: &str, ctx: &ToolCtx) -> Result<Vec<Block>, String> {
        let arguments = self.arguments(rest)?;
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&arguments)
            .current_dir(&ctx.cwd)
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1")
            .env("GH_PAGER", "cat")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(env) = ctx.services.get::<AgentEnv>() {
            command.envs(env.vars());
        }
        let shown = format!("gh {}", arguments.join(" "));
        let run = async {
            tokio::time::timeout(GH_TIMEOUT, command.output())
                .await
                .map_err(|_| format!("`{shown}` did not finish within {} s", GH_TIMEOUT.as_secs()))?
                .map_err(|e| format!("cannot run `{shown}`: {e}"))
        };
        let output = tokio::select! {
            output = run => output?,
            _ = ctx.cancel.cancelled() => return Err("cancelled".to_string()),
        };
        if !output.status.success() {
            let why = String::from_utf8_lossy(&output.stderr);
            let why = why.trim();
            return Err(if why.is_empty() {
                format!("`{shown}` failed ({})", output.status)
            } else {
                format!("`{shown}` failed: {why}")
            });
        }
        let text = String::from_utf8_lossy(&output.stdout).trim_end().to_string();
        Ok(vec![Block::text(if text.is_empty() {
            match self.kind {
                GhKind::Pr => "no open pull requests".to_string(),
                GhKind::Issue => "no open issues".to_string(),
            }
        } else {
            text
        })])
    }

    /// `gh` talks to GitHub, so these addresses are asked about like a fetch from it.
    fn access(&self, _: &str) -> (Access, Option<String>) {
        (Access::Net, Some("domain:github.com".to_string()))
    }
}

/// Where the `gh` program is, when it is installed.
pub fn gh_program() -> Option<PathBuf> {
    which::which("gh").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_addresses_become_gh_commands_and_nothing_else_does() {
        let prs = GhScheme::pull_requests(PathBuf::from("gh"));
        let issues = GhScheme::issues(PathBuf::from("gh"));
        assert_eq!(prs.arguments("").unwrap(), ["pr", "list", "--limit", "30"]);
        assert_eq!(prs.arguments("12").unwrap(), ["pr", "view", "12", "--comments"]);
        assert_eq!(prs.arguments("12/diff").unwrap(), ["pr", "diff", "12"]);
        assert_eq!(issues.arguments("/").unwrap(), ["issue", "list", "--limit", "30"]);
        assert_eq!(issues.arguments("7").unwrap(), ["issue", "view", "7", "--comments"]);
        assert_eq!(
            prs.arguments("12/files").unwrap_err(),
            "pr://12/files: expected pr://, pr://<number> or pr://<number>/diff"
        );
        assert_eq!(
            issues.arguments("7/diff").unwrap_err(),
            "issue://7/diff: expected issue:// or issue://<number>"
        );
        for smuggled in ["--repo=x/y", "1 --web", "-R", "/diff"] {
            assert!(prs.arguments(smuggled).is_err(), "{smuggled}");
        }
        assert_eq!(
            prs.access("12"),
            (Access::Net, Some("domain:github.com".to_string()))
        );
    }
}
