//! What `/diff` and `/commit` need from git: the working tree's changes file by file, and a
//! commit made as the user. Nothing here amends a commit or pushes.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// A file's diff larger than this is cut; the viewer renders every row of what it is given.
const MAX_FILE_DIFF_BYTES: usize = 400_000;
/// An untracked file larger than this is named, not shown.
const MAX_UNTRACKED_BYTES: u64 = 256_000;
const MAX_UNTRACKED_FILES: usize = 300;
/// How much of the diff the model that drafts a commit message is given.
const MAX_DRAFT_DIFF_BYTES: usize = 40_000;
const CUT_NOTE: &str = "[diff cut here]";

/// The changes to one file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    /// The path with what happened to it: `src/lib.rs  +3 −1`, `notes.md (new)  +12`.
    pub file: String,
    /// The file's unified diff, from its `---` and `+++` lines on.
    pub text: String,
}

fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        // Paths as they are, not octal escapes, and never a pager.
        .args(["-c", "core.quotepath=false", "--no-pager"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

/// Runs git in `dir`; the output on success, what git said otherwise.
async fn run(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = git(dir)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("git could not be started: {e}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let said = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if said.is_empty() {
        format!("git {} failed", args.first().copied().unwrap_or_default())
    } else {
        said
    })
}

/// The top directory of the repository `dir` is in; `None` outside a repository or without git.
pub async fn toplevel(dir: &Path) -> Option<PathBuf> {
    let top = run(dir, &["rev-parse", "--show-toplevel"]).await.ok()?;
    let top = top.trim();
    (!top.is_empty()).then(|| PathBuf::from(top))
}

/// What the working tree is compared with: `HEAD`, or the empty tree before the first commit.
async fn base(top: &Path) -> Result<String, String> {
    if run(top, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .await
        .is_ok()
    {
        return Ok("HEAD".to_string());
    }
    // Standard input is empty, so this is the id of a tree with nothing in it.
    let empty = run(top, &["hash-object", "-t", "tree", "--stdin"]).await?;
    Ok(empty.trim().to_string())
}

fn cut(text: &mut String, max: usize) {
    if text.len() > max {
        let mut at = max;
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        text.truncate(at);
        text.push_str(&format!("\n{CUT_NOTE}"));
    }
}

/// A path as git prints it in a diff header, without the quotes it puts around unusual names.
fn unquote(path: &str) -> &str {
    path.trim().trim_matches('"')
}

/// Splits the output of `git diff` into its files. Anything before the first file (a diffstat)
/// is left out.
pub fn split(diff: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut header: Option<&str> = None;
    let mut lines: Vec<&str> = Vec::new();
    for line in diff.lines() {
        if let Some(names) = line.strip_prefix("diff --git ") {
            if let Some(header) = header.replace(names) {
                files.push(file_diff(header, &lines));
            }
            lines.clear();
        } else if header.is_some() {
            lines.push(line);
        }
    }
    if let Some(header) = header {
        files.push(file_diff(header, &lines));
    }
    files
}

/// One file of a diff: `header` is what followed `diff --git`, `lines` the rest up to the next file.
fn file_diff(header: &str, lines: &[&str]) -> FileDiff {
    let (mut old, mut new, mut renamed_from) = (None, None, None);
    let (mut created, mut deleted) = (false, false);
    let (mut added, mut removed) = (0usize, 0usize);
    let mut in_hunks = false;
    for line in lines {
        if line.starts_with("@@") {
            in_hunks = true;
        } else if in_hunks {
            match line.as_bytes().first() {
                Some(b'+') => added += 1,
                Some(b'-') => removed += 1,
                _ => {}
            }
        } else if let Some(path) = line.strip_prefix("--- ") {
            old = unquote(path).strip_prefix("a/").map(str::to_string);
        } else if let Some(path) = line.strip_prefix("+++ ") {
            new = unquote(path).strip_prefix("b/").map(str::to_string);
        } else if let Some(path) = line.strip_prefix("rename from ") {
            renamed_from = Some(unquote(path).to_string());
        } else if let Some(path) = line.strip_prefix("rename to ") {
            new = Some(unquote(path).to_string());
        } else if line.starts_with("new file mode") {
            created = true;
        } else if line.starts_with("deleted file mode") {
            deleted = true;
        }
    }
    // A change of mode alone, or a binary file, names the file in the header only.
    let from_header = || {
        let names = unquote(header);
        let name = names.rsplit_once(" b/").map_or(names, |(_, name)| name);
        unquote(name).to_string()
    };
    let mut file = new.or(old).unwrap_or_else(from_header);
    if let Some(from) = renamed_from.filter(|from| *from != file) {
        file = format!("{from} → {file}");
    }
    if created {
        file.push_str(" (new)");
    } else if deleted {
        file.push_str(" (deleted)");
    }
    if added > 0 {
        file.push_str(&format!("  +{added}"));
    }
    if removed > 0 {
        file.push_str(&format!("{}−{removed}", if added > 0 { " " } else { "  " }));
    }
    let mut text = lines
        .iter()
        .filter(|line| !line.starts_with("index "))
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    cut(&mut text, MAX_FILE_DIFF_BYTES);
    FileDiff { file, text }
}

/// An untracked file as the diff that adding it would be.
fn untracked_diff(top: &Path, relative: &str) -> FileDiff {
    let path = top.join(relative);
    let file = format!("{relative} (untracked)");
    let note = |text: String| FileDiff {
        file: file.clone(),
        text,
    };
    let size = match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => meta.len(),
        // A directory here is a repository of its own inside this one.
        Ok(_) => return note("a directory with its own repository; not shown".to_string()),
        Err(e) => return note(format!("could not be read: {e}")),
    };
    if size > MAX_UNTRACKED_BYTES {
        return note(format!("{size} bytes; too large to show"));
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => return note(format!("could not be read: {e}")),
    };
    let Ok(content) = String::from_utf8(bytes) else {
        return note(format!("binary file, {size} bytes"));
    };
    if content.contains('\0') {
        return note(format!("binary file, {size} bytes"));
    }
    let count = content.lines().count();
    let mut text = format!("new file\n--- /dev/null\n+++ b/{relative}\n");
    if count > 0 {
        // As git writes it: a range of one line has no count.
        let range = if count == 1 {
            "+1".to_string()
        } else {
            format!("+1,{count}")
        };
        text.push_str(&format!("@@ -0,0 {range} @@\n"));
        for line in content.lines() {
            text.push('+');
            text.push_str(line);
            text.push('\n');
        }
        if !content.ends_with('\n') {
            text.push_str("\\ No newline at end of file\n");
        }
    }
    FileDiff {
        file: format!("{file}  +{count}"),
        text: text.trim_end().to_string(),
    }
}

/// Untracked files that are not ignored, as paths from the top of the repository.
async fn untracked(top: &Path) -> Result<Vec<String>, String> {
    let listed = run(top, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    Ok(listed
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| path.trim_end_matches('/').to_string())
        .collect())
}

/// The working tree against `HEAD`, one entry per file: changes to tracked files, staged or not,
/// then untracked files as additions.
pub async fn working_tree(top: &Path) -> Result<Vec<FileDiff>, String> {
    let base = base(top).await?;
    let tracked = run(top, &["diff", "--no-color", "--no-ext-diff", &base]).await?;
    let mut files = split(&tracked);
    let new = untracked(top).await?;
    let hidden = new.len().saturating_sub(MAX_UNTRACKED_FILES);
    for relative in new.iter().take(MAX_UNTRACKED_FILES) {
        files.push(untracked_diff(top, relative));
    }
    if hidden > 0 {
        files.push(FileDiff {
            file: format!("{hidden} more untracked files"),
            text: "not shown".to_string(),
        });
    }
    Ok(files)
}

/// What a commit would be made from.
#[derive(Debug, Clone, PartialEq)]
pub struct Changes {
    /// `git status --short`.
    pub status: String,
    /// The staged diff when something is staged; otherwise every change, untracked files included.
    pub diff: String,
    /// Whether anything is staged. When nothing is, committing means staging everything first.
    pub staged: bool,
    /// The subjects of the last ten commits, newest first, for their style.
    pub subjects: Vec<String>,
}

/// The changes waiting to be committed; `None` when the working tree is clean.
pub async fn changes(top: &Path) -> Result<Option<Changes>, String> {
    let status = run(top, &["status", "--short"]).await?;
    if status.trim().is_empty() {
        return Ok(None);
    }
    let staged_diff = run(top, &["diff", "--cached", "--no-color", "--no-ext-diff"]).await?;
    let staged = !staged_diff.trim().is_empty();
    let diff = if staged {
        staged_diff
    } else {
        working_tree(top)
            .await?
            .iter()
            .map(|file| format!("{}\n{}", file.file, file.text))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    // A repository without a commit yet has no log; that is not an error here.
    let subjects = run(top, &["log", "-10", "--format=%s"])
        .await
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    Ok(Some(Changes {
        status: status.trim_end().to_string(),
        diff,
        staged,
        subjects,
    }))
}

/// What the model that drafts the commit message is asked.
pub fn message_request(changes: &Changes, hint: &str) -> String {
    let mut out = String::from(
        "Write the git commit message for the changes below.\n\n\
         - First line: a summary of at most 72 characters, in the imperative, without a full stop.\n\
         - Only when the change needs explaining, add a blank line and a short body that says what \
         changed and why, wrapped at 72 columns.\n\
         - Follow the style of the recent subjects (prefixes, capitals, tense) when there are any.\n\
         - Reply with the message only: no quotes, no code fence, nothing before or after it.\n",
    );
    if !hint.trim().is_empty() {
        out.push_str(&format!("\nThe author says about this commit: {}\n", hint.trim()));
    }
    if !changes.subjects.is_empty() {
        out.push_str(&format!("\nRecent subjects:\n{}\n", changes.subjects.join("\n")));
    }
    let mut diff = changes.diff.clone();
    cut(&mut diff, MAX_DRAFT_DIFF_BYTES);
    out.push_str(&format!(
        "\ngit status --short:\n{}\n\n{}:\n{diff}\n",
        changes.status,
        if changes.staged {
            "The staged changes, which are what is committed"
        } else {
            "The changes, all of which are committed"
        }
    ));
    out
}

/// A drafted message as it goes into the input: without a code fence or quotes around it.
pub fn clean_message(reply: &str) -> String {
    let mut text = reply.trim();
    if let Some(rest) = text.strip_prefix("```") {
        // The fence's first line may name a language.
        let rest = rest.split_once('\n').map_or("", |(_, rest)| rest);
        text = rest.trim_end().strip_suffix("```").unwrap_or(rest).trim();
    }
    let unquoted = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .filter(|inner| !inner.contains('"'));
    unquoted.unwrap_or(text).trim().to_string()
}

/// What a commit came to.
#[derive(Debug, Clone, PartialEq)]
pub struct Committed {
    /// The command line as the agent is told it.
    pub command: String,
    pub output: String,
    /// `exit code: 0`, as for a shell command.
    pub status: String,
}

/// Commits with `message` as the user would: their identity, their hooks, their signing. With
/// `stage_all` everything is staged first. Never amends and never pushes.
pub async fn commit(top: &Path, message: &str, stage_all: bool) -> Committed {
    let subject = message.lines().next().unwrap_or_default();
    let command = if stage_all {
        format!("git add -A && git commit -m \"{subject}\"")
    } else {
        format!("git commit -m \"{subject}\"")
    };
    let done = |output: String, status: String| Committed {
        command: command.clone(),
        output,
        status,
    };
    if stage_all && let Err(why) = run(top, &["add", "-A"]).await {
        return done(why, "nothing was committed: staging failed".to_string());
    }
    // The message goes in on standard input, so that no shell or argument quoting touches it.
    let mut child = match git(top)
        .args(["commit", "-F", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return done(String::new(), format!("could not start: {e}")),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(message.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }
    match child.wait_with_output().await {
        Ok(output) => {
            let mut said = String::from_utf8_lossy(&output.stdout).into_owned();
            said.push_str(&String::from_utf8_lossy(&output.stderr));
            let status = match output.status.code() {
                Some(code) => format!("exit code: {code}"),
                None => "killed".to_string(),
            };
            done(said.trim_end().to_string(), status)
        }
        Err(e) => done(String::new(), format!("could not wait for git: {e}")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    struct Repo {
        dir: tempfile::TempDir,
    }

    impl Repo {
        async fn new() -> Repo {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repo { dir };
            for args in [
                &["init", "-q"][..],
                &["config", "user.name", "Tester"],
                &["config", "user.email", "tester@example.invalid"],
                &["config", "core.autocrlf", "false"],
                &["config", "commit.gpgsign", "false"],
            ] {
                run(repo.path(), args).await.unwrap();
            }
            repo
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        fn write(&self, file: &str, content: &str) {
            let path = self.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        async fn commit_all(&self, message: &str) {
            let done = commit(self.path(), message, true).await;
            assert_eq!(done.status, "exit code: 0", "{}", done.output);
        }
    }

    #[test]
    fn a_diff_splits_into_files_named_with_what_happened() {
        let diff = "\
 a.txt | 2 +-
 2 files changed

diff --git a/a.txt b/a.txt
index 1111111..2222222 100644
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
 keep
-old
+new
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 3333333..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
--- not a header
diff --git a/old name.txt b/new name.txt
similarity index 100%
rename from old name.txt
rename to new name.txt
diff --git a/run.sh b/run.sh
old mode 100644
new mode 100755
";
        let files = split(diff);
        let names: Vec<&str> = files.iter().map(|file| file.file.as_str()).collect();
        assert_eq!(
            names,
            [
                "a.txt  +1 −1",
                "gone.txt (deleted)  −1",
                "old name.txt → new name.txt",
                "run.sh"
            ]
        );
        assert_eq!(
            files[0].text,
            "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n keep\n-old\n+new"
        );
        assert!(files[3].text.contains("new mode 100755"));
        assert_eq!(split("nothing here"), []);
    }

    #[tokio::test]
    async fn the_working_tree_diff_has_changed_staged_and_untracked_files() {
        let repo = Repo::new().await;
        let top = toplevel(repo.path()).await.unwrap();
        // Before the first commit everything is new.
        repo.write("kept.txt", "one\ntwo\n");
        repo.write(".gitignore", "ignored/\n");
        let fresh = working_tree(&top).await.unwrap();
        assert!(fresh.iter().any(|file| file.file == "kept.txt (untracked)  +2"));
        repo.commit_all("first").await;
        assert_eq!(working_tree(&top).await.unwrap(), []);

        repo.write("kept.txt", "one\n2\n");
        repo.write("staged.txt", "staged\n");
        run(&top, &["add", "staged.txt"]).await.unwrap();
        repo.write("sub/new.txt", "fresh\nlines");
        repo.write("ignored/secret.txt", "x\n");
        repo.write("blob.bin", "a\0b");
        let files = working_tree(&top).await.unwrap();
        let names: Vec<&str> = files.iter().map(|file| file.file.as_str()).collect();
        assert_eq!(
            names,
            [
                "kept.txt  +1 −1",
                "staged.txt (new)  +1",
                "blob.bin (untracked)",
                "sub/new.txt (untracked)  +2"
            ]
        );
        assert_eq!(
            files[3].text,
            "new file\n--- /dev/null\n+++ b/sub/new.txt\n@@ -0,0 +1,2 @@\n+fresh\n+lines\n\\ No newline at end of file"
        );
        assert_eq!(files[2].text, "binary file, 3 bytes");
        repo.write("one.txt", "single\n");
        let files = working_tree(&top).await.unwrap();
        assert_eq!(
            files[3].text,
            "new file\n--- /dev/null\n+++ b/one.txt\n@@ -0,0 +1 @@\n+single"
        );
        // A directory that is not a repository has no top level and nothing to diff.
        let outside = tempfile::tempdir().unwrap();
        if toplevel(outside.path()).await.is_none() {
            assert!(working_tree(outside.path()).await.is_err());
        }
    }

    #[tokio::test]
    async fn a_commit_is_made_from_what_is_staged_or_from_everything() {
        let repo = Repo::new().await;
        let top = toplevel(repo.path()).await.unwrap();
        repo.write("a.txt", "a\n");
        repo.commit_all("Add a").await;
        // Nothing changed: there is nothing to commit.
        assert_eq!(changes(&top).await.unwrap(), None);

        repo.write("a.txt", "a\nmore\n");
        repo.write("b.txt", "b\n");
        let unstaged = changes(&top).await.unwrap().unwrap();
        assert!(!unstaged.staged);
        assert_eq!(unstaged.subjects, ["Add a"]);
        assert!(unstaged.diff.contains("+more") && unstaged.diff.contains("b.txt (untracked)"));
        let request = message_request(&unstaged, "the b file is a fixture");
        assert!(request.contains("Recent subjects:\nAdd a\n"));
        assert!(request.contains("The author says about this commit: the b file is a fixture"));
        assert!(request.contains("?? b.txt"));

        // With something staged, only that is committed.
        run(&top, &["add", "b.txt"]).await.unwrap();
        let staged = changes(&top).await.unwrap().unwrap();
        assert!(staged.staged);
        assert!(staged.diff.contains("b.txt") && !staged.diff.contains("+more"));
        let done = commit(&top, "Add b\n\nA fixture for the tests.", false).await;
        assert_eq!(done.status, "exit code: 0", "{}", done.output);
        assert_eq!(done.command, "git commit -m \"Add b\"");
        let log = run(&top, &["log", "-1", "--format=%B"]).await.unwrap();
        assert_eq!(log.trim(), "Add b\n\nA fixture for the tests.");
        let left = changes(&top).await.unwrap().unwrap();
        assert_eq!(left.status, " M a.txt");

        // Committing everything stages first; an empty commit fails and says so.
        let all = commit(&top, "Extend a", true).await;
        assert_eq!(all.status, "exit code: 0", "{}", all.output);
        assert_eq!(changes(&top).await.unwrap(), None);
        let nothing = commit(&top, "Nothing", false).await;
        assert_eq!(nothing.status, "exit code: 1");
        let count = run(&top, &["rev-list", "--count", "HEAD"]).await.unwrap();
        assert_eq!(count.trim(), "3");
    }

    #[test]
    fn a_drafted_message_loses_its_fence_and_quotes() {
        assert_eq!(clean_message("  Fix the parser\n"), "Fix the parser");
        assert_eq!(
            clean_message("```\nFix the parser\n\nIt dropped the last token.\n```"),
            "Fix the parser\n\nIt dropped the last token."
        );
        assert_eq!(clean_message("```text\nFix it\n```\n"), "Fix it");
        assert_eq!(clean_message("\"Fix the parser\""), "Fix the parser");
        assert_eq!(
            clean_message("Rename \"old\" to \"new\""),
            "Rename \"old\" to \"new\""
        );
        assert_eq!(clean_message(""), "");
    }

    #[test]
    fn a_long_diff_is_cut_for_the_model() {
        let changes = Changes {
            status: " M big.txt".to_string(),
            diff: "+line\n".repeat(20_000),
            staged: true,
            subjects: Vec::new(),
        };
        let request = message_request(&changes, "");
        assert!(request.len() < MAX_DRAFT_DIFF_BYTES + 2_000);
        assert!(request.contains(CUT_NOTE) && !request.contains("Recent subjects"));
        assert!(request.contains("The staged changes"));
    }
}
