---
name: git-workflow
description: >-
  Use when the task involves git beyond a simple status or diff: shaping commits, working on branches, resolving merge or rebase conflicts, finding the commit that introduced a bug, recovering lost work, or using worktrees. Agents never push; the user does.
---
# Git workflow

## Rules that always hold

- **Agents never push.** `git push` and its equivalents are refused by the harness for every agent. When work is ready to publish, say so and let the user push (they can type `!git push` themselves).
- Do not rewrite history that exists on a remote, and do not force anything. Rebase and amend only commits that are local.
- Do not discard work you did not create. Before `reset --hard`, `checkout -- <path>`, `clean` or `stash drop`, look at what would be lost and ask unless the user told you to.
- Do not change the user's git configuration or bypass hooks (`--no-verify`) without being asked.
- Commit only when the user's workflow calls for it. If unsure whether to commit, leave changes in the working tree and say so.

## Look before acting

```sh
git status --short --branch
git log --oneline --graph -15
git diff                 # unstaged
git diff --staged        # staged
git stash list
```

Know which branch you are on, whether it tracks a remote, and whether an operation (merge, rebase, cherry-pick, bisect) is already in progress.

## Commits

- One logical change per commit: it builds, its tests pass, and it can be reverted alone.
- Stage deliberately: `git add -p` for parts of files, `git add <paths>` for whole files. Never `git add -A` without reading `git status` first; check for secrets, build output and editor files.
- Message format, unless the project defines another (look at `git log` and any contributing guide):

  ```
  <summary in the imperative, 50 to 72 characters>

  <why the change was needed and what it does that the diff does not show>
  ```

  Follow the project's convention for prefixes (`fix:`, `feat:`, ticket numbers) when it has one.
- Fix the last local commit with `git commit --amend`. Fold a fix into an earlier local commit with `git commit --fixup <sha>` then `git rebase -i --autosquash <base>`.

## Branches

- Branch from the up-to-date main line for each piece of work: `git switch -c <topic>`.
- Bring in upstream changes the way the project does. If history is kept linear, `git fetch` then `git rebase origin/main` on a local-only branch. If the branch is shared or the project merges, `git merge origin/main`.
- Keep branches short-lived; integrate small changes often.

## Resolving conflicts

1. See what conflicts: `git status` lists unmerged paths; `git diff` shows the conflict hunks.
2. Understand both sides before editing. `git log --merge -p <path>` shows the commits on each side that touched the file; `git show :1:<path>`, `:2:<path>`, `:3:<path>` give the base, ours and theirs versions.
3. Resolve by intent, not by picking a side: the result must contain what both changes were trying to achieve. If they contradict, ask.
4. Remove every conflict marker; search for `<<<<<<<`, `=======`, `>>>>>>>`.
5. Build and run the tests that cover the conflicted code. A textually clean merge can still be wrong, including in files that had no conflict.
6. `git add <path>` each resolved file, then `git merge --continue` or `git rebase --continue`.

To back out: `git merge --abort` or `git rebase --abort` returns to the state before the operation. During a rebase, "ours" is the branch being rebased onto and "theirs" is the commit being replayed, the reverse of a merge.

For repeated conflicts during long rebases, `git rerere` (when enabled by the user) reuses recorded resolutions.

## Finding the commit that broke something

```sh
git bisect start
git bisect bad                 # the current commit fails
git bisect good <sha-or-tag>   # a commit known to work
git bisect run <command>       # exit 0 = good, 1..127 except 125 = bad, 125 = skip
git bisect reset
```

The command must be a reliable, fast reproduction. Use exit code 125 for commits that cannot be built. When the result is a merge commit, bisect inside the merged branch. Report the first bad commit with its message and the part of its diff that explains the failure.

## Worktrees

A worktree is a second checkout of the same repository in another directory, on its own branch. Use one to work on two things at once or to let a sub-agent make changes without touching the main tree.

```sh
git worktree add ../repo-fix -b fix/timeout
git worktree list
git worktree remove ../repo-fix
```

The harness creates and removes worktrees itself for sub-agents started with worktree isolation; their changes come back as a patch that the lead applies. Do not delete those directories by hand.

## Recovering

- Lost commits after a reset or a bad rebase: `git reflog` lists where `HEAD` has been; `git branch rescue <sha>` or `git reset --hard <sha>` restores it.
- Undo a commit that is already shared: `git revert <sha>` (a new commit), never a reset.
- Recover a deleted file: `git log --diff-filter=D -- <path>` finds the commit; `git restore --source=<sha>^ -- <path>` brings it back.
- Unstage without losing changes: `git restore --staged <path>`.
- Changes made by agents in this harness are also checkpointed per prompt; `/rewind` restores the working tree to an earlier prompt without touching the repository's history.

## Reading history

```sh
git log --follow -p -- <path>        # the life of a file
git log -S'<text>' --oneline          # commits that added or removed the text
git log -G'<regex>' --oneline         # commits whose diff matches
git blame -w -C <path>                # who last changed each line, ignoring whitespace and moves
git diff <base>...HEAD                # what this branch changed since it left base
```

## Handing over

When work is ready, report: the branch, the commits on it (`git log --oneline <base>..HEAD`), what was verified, and the exact command the user can run to publish (`git push -u origin <branch>`). Do not run it.
