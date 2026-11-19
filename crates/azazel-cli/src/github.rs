//! `/issues`, `/prs`, `/pr` and `/review`: what they ask GitHub and git, without a terminal.
//! Everything goes through the `gh` program. Reading is free; opening a pull request and posting
//! a review change GitHub, so the caller asks first (see [`asks_before_posting`]).

use std::path::{Path, PathBuf};

use azazel_core::PermissionMode;
use azazel_tools::gh::{self, Item, Kind, TIMEOUT};

use crate::git;

/// How much of a diff goes into a request to a model.
const MAX_DIFF_BYTES: usize = 60_000;

/// Whether posting to GitHub waits for the user's yes: in every mode but bypass.
pub fn asks_before_posting(mode: PermissionMode) -> bool {
    mode != PermissionMode::Bypass
}

/// How long ago `created` (as GitHub writes it) is, at `now` (seconds since the epoch): `5m`,
/// `3h`, `12d`, `2mo`, `1y`.
pub fn age(created: &str, now: i64) -> String {
    let Ok(then) = created.parse::<jiff::Timestamp>() else {
        return String::new();
    };
    let seconds = (now - then.as_second()).max(0);
    match seconds {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        86_400..=2_591_999 => format!("{}d", seconds / 86_400),
        2_592_000..=31_535_999 => format!("{}mo", seconds / 2_592_000),
        _ => format!("{}y", seconds / 31_536_000),
    }
}

/// A row of a picker: the number and title, and what goes after them (author, labels, age).
pub fn row(item: &Item, now: i64) -> (String, Vec<String>) {
    let mut parts = vec![item.author.clone()];
    if !item.labels.is_empty() {
        parts.push(item.labels.join(", "));
    }
    let age = age(&item.created, now);
    if !age.is_empty() {
        parts.push(age);
    }
    (format!("#{} {}", item.number, item.title), parts)
}

/// What `/pr` found out about the branch.
#[derive(Debug, Clone)]
pub struct Plan {
    pub top: PathBuf,
    pub branch: String,
    /// The default branch of the repository, which the pull request goes into.
    pub base: String,
    /// `git log --oneline <base>..HEAD`.
    pub log: String,
    /// The diff of the branch against its base, cut when long.
    pub diff: String,
}

async fn git_out(top: &Path, args: &[&str]) -> Result<String, String> {
    git::run(top, args).await.map(|out| out.trim_end().to_string())
}

/// Looks at the current branch of the repository of `cwd`: `Err` with what to do when a pull
/// request cannot be opened from it (the default branch, uncommitted changes, nothing ahead).
pub async fn plan(gh_program: &Path, cwd: &Path) -> Result<Plan, String> {
    let top = git::toplevel(cwd)
        .await
        .ok_or("this directory is not in a git repository")?;
    let branch = git_out(&top, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    if branch == "HEAD" {
        return Err("HEAD is detached; switch to a branch before opening a pull request".to_string());
    }
    let json = gh::run(
        gh_program,
        &[
            "repo".to_string(),
            "view".to_string(),
            "--json".to_string(),
            "defaultBranchRef".to_string(),
        ],
        &top,
        &[],
        TIMEOUT,
    )
    .await?;
    let base = serde_json::from_str::<serde_json::Value>(&json)
        .ok()
        .and_then(|value| value["defaultBranchRef"]["name"].as_str().map(str::to_string))
        .ok_or("gh did not say which branch is the default one")?;
    if branch == base {
        return Err(format!(
            "you are on the default branch ({base}); switch to a feature branch first, for example \
             `!git switch -c my-change`"
        ));
    }
    if !git_out(&top, &["status", "--porcelain"]).await?.is_empty() {
        return Err(
            "there are uncommitted changes; commit them first (/commit), then run /pr again".to_string(),
        );
    }
    // A clone may have the base only as a remote branch.
    let range_base = if git::run(&top, &["rev-parse", "--verify", "--quiet", &base])
        .await
        .is_ok()
    {
        base.clone()
    } else {
        format!("origin/{base}")
    };
    let log = git_out(&top, &["log", "--oneline", &format!("{range_base}..HEAD")]).await?;
    if log.is_empty() {
        return Err(format!(
            "{branch} has no commits that {base} lacks; there is nothing to open"
        ));
    }
    let mut diff = git_out(
        &top,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            &format!("{range_base}...HEAD"),
        ],
    )
    .await?;
    git::cut(&mut diff, MAX_DIFF_BYTES);
    Ok(Plan {
        top,
        branch,
        base,
        log,
        diff,
    })
}

/// What the model that drafts a pull request is asked.
pub fn pr_request(plan: &Plan, hint: &str) -> String {
    let mut out = format!(
        "Write the title and description of a GitHub pull request from branch `{}` into `{}`.\n\n\
         - First line: the title, at most 72 characters, in the imperative, without a full stop.\n\
         - Then a blank line and the description in Markdown: what changed and why, and how it was \
         checked when the commits say so. Be brief; no headings unless it is long.\n\
         - Reply with the title and description only: no quotes, no code fence, nothing before or after.\n",
        plan.branch, plan.base
    );
    out.push_str(git::WRITING_RULES);
    if !hint.trim().is_empty() {
        out.push_str(&format!("\nThe author wants the title to be: {}\n", hint.trim()));
    }
    out.push_str(&format!(
        "\nCommits ({}..HEAD):\n{}\n\nThe diff:\n{}\n",
        plan.base, plan.log, plan.diff
    ));
    out
}

/// A drafted or edited text as a title and a body: the first line, then the rest.
pub fn split_draft(text: &str) -> (String, String) {
    let text = git::clean_message(text);
    let (title, body) = text.split_once('\n').unwrap_or((&text, ""));
    (title.trim().to_string(), body.trim().to_string())
}

/// Pushes the branch of `plan` and opens the pull request; the address of the new one.
pub async fn create_pull_request(
    gh_program: &Path,
    plan: &Plan,
    title: &str,
    body: &str,
) -> Result<String, String> {
    git::run(&plan.top, &["push", "--set-upstream", "origin", &plan.branch])
        .await
        .map_err(|why| format!("could not push {}: {why}", plan.branch))?;
    let file = tempfile::NamedTempFile::new().map_err(|e| format!("no temporary file: {e}"))?;
    std::fs::write(file.path(), body)
        .map_err(|e| format!("could not write {}: {e}", file.path().display()))?;
    let args: Vec<String> = [
        "pr",
        "create",
        "--base",
        &plan.base,
        "--head",
        &plan.branch,
        "--title",
        title,
        "--body-file",
        &file.path().to_string_lossy(),
    ]
    .iter()
    .map(|word| word.to_string())
    .collect();
    let out = gh::run(gh_program, &args, &plan.top, &[], TIMEOUT).await?;
    out.lines()
        .map(str::trim)
        .rfind(|line| line.starts_with("https://"))
        .map(str::to_string)
        .ok_or_else(|| "`gh pr create` printed no address".to_string())
}

/// The number of the pull request of the current branch of `cwd`.
pub async fn current_pull_request(gh_program: &Path, cwd: &Path) -> Result<u64, String> {
    let args: Vec<String> = ["pr", "view", "--json", "number"]
        .iter()
        .map(|w| w.to_string())
        .collect();
    let json = gh::run(gh_program, &args, cwd, &[], TIMEOUT)
        .await
        .map_err(|why| format!("this branch has no pull request ({why}); name one: /review <number>"))?;
    serde_json::from_str::<serde_json::Value>(&json)
        .ok()
        .and_then(|value| value["number"].as_u64())
        .ok_or_else(|| "gh did not say which pull request this is".to_string())
}

/// What a review is made from: the pull request as text and its diff.
pub async fn review_material(gh_program: &Path, cwd: &Path, number: u64) -> Result<(String, String), String> {
    let view = gh::run(
        gh_program,
        &gh::view_arguments(Kind::Pr, &number.to_string()),
        cwd,
        &[],
        TIMEOUT,
    )
    .await?;
    let view = gh::render_view(Kind::Pr, &view)?;
    let diff = gh::run(
        gh_program,
        &["pr".to_string(), "diff".to_string(), number.to_string()],
        cwd,
        &[],
        TIMEOUT,
    )
    .await?;
    let mut diff = diff;
    git::cut(&mut diff, MAX_DIFF_BYTES);
    Ok((view, diff))
}

/// What the reviewer is asked: the skill's method, the pull request, and the findings in the
/// P0 to P3 scale.
pub fn review_request(skill: &str, view: &str, diff: &str) -> String {
    format!(
        "Review the pull request below. Method:\n\n{skill}\n\n\
         Write the findings as a list, most severe first, each as `[P0|P1|P2|P3] path:line - what is wrong, \
         why it matters, a suggested fix`. P0 is a blocker (wrong results, data loss, security hole, broken \
         build), P1 a major defect or a missing test for important behaviour, P2 minor, P3 a nit. Start with \
         one line saying what the change does and a verdict. You cannot run anything: say what you could not \
         check. Reply with the review only. It is posted as a plain comment; never approve or request changes.\n\n\
         The pull request:\n{view}\n\nIts diff:\n{diff}\n"
    )
}

/// Posts `body` as a comment review (never an approval or a request for changes).
pub async fn post_review(gh_program: &Path, cwd: &Path, number: u64, body: &str) -> Result<(), String> {
    let file = tempfile::NamedTempFile::new().map_err(|e| format!("no temporary file: {e}"))?;
    std::fs::write(file.path(), body)
        .map_err(|e| format!("could not write {}: {e}", file.path().display()))?;
    let body_file = file.path().to_string_lossy().into_owned();
    let args: Vec<String> = [
        "pr",
        "review",
        &number.to_string(),
        "--comment",
        "--body-file",
        &body_file,
    ]
    .iter()
    .map(|word| word.to_string())
    .collect();
    gh::run(gh_program, &args, cwd, &[], TIMEOUT).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_picker_row_has_number_title_author_labels_and_age() {
        let item = Item {
            number: 12,
            title: "Crash on start".into(),
            author: "ann".into(),
            labels: vec!["bug".into(), "p1".into()],
            state: "OPEN".into(),
            created: "2026-11-02T10:00:00Z".into(),
            url: String::new(),
        };
        let now = "2026-11-05T10:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_second();
        assert_eq!(
            row(&item, now),
            (
                "#12 Crash on start".to_string(),
                vec!["ann".to_string(), "bug, p1".to_string(), "3d".to_string()]
            )
        );
        assert_eq!(age("2026-11-05T09:30:00Z", now), "30m");
    }

    #[test]
    fn the_pull_request_request_carries_the_style_rules_and_the_commits() {
        let plan = Plan {
            top: PathBuf::new(),
            branch: "topic".into(),
            base: "main".into(),
            log: "abc Add x".into(),
            diff: "+x".into(),
        };
        let request = pr_request(&plan, "Add x");
        assert!(request.contains(git::WRITING_RULES));
        assert!(request.contains("The author wants the title to be: Add x"));
        assert!(request.contains("abc Add x") && request.contains("+x"));
    }

    #[test]
    fn a_title_is_the_first_line_of_a_draft() {
        assert_eq!(
            split_draft("```\nAdd x\n\nDoes x.\nMore.\n```"),
            ("Add x".to_string(), "Does x.\nMore.".to_string())
        );
        assert_eq!(
            split_draft("Only a title"),
            ("Only a title".to_string(), String::new())
        );
    }

    #[test]
    fn only_bypass_posts_without_asking() {
        for mode in [
            PermissionMode::Plan,
            PermissionMode::Manual,
            PermissionMode::AcceptEdits,
        ] {
            assert!(asks_before_posting(mode), "{mode:?}");
        }
        assert!(!asks_before_posting(PermissionMode::Bypass));
    }
}
