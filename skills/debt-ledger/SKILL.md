---
name: debt-ledger
description: "Use to list the deliberate shortcuts in a codebase. Collects the `shortcut:` comments (a limit and the trigger to upgrade it) into a table, flags the ones with no trigger, and counts them."
---
# Debt ledger

A `shortcut: <limit>; upgrade when <trigger>` comment marks a decision to do less than the general solution (see the `lean` skill). This skill turns those comments into a ledger the team can read at a glance.

## Method

1. **Collect.** From the repository root run `git grep -n -i "shortcut:"`. Add a path after `--` when the user names a directory. `git grep` skips untracked and ignored files; if the work is not committed yet, also `grep -rn "shortcut:"` over the changed paths.
2. **Keep the markers, drop the noise.** A marker is a comment whose text starts with `shortcut:`. Skip matches in prose, documentation about the marker, tests of this skill, and strings that are not comments. Say how many matches you dropped.
3. **Read each one** with a few lines of context so the limit is stated in the code's terms, and so a trigger written on the next line is not missed.
4. **Split the comment** at `; upgrade when`:
   - the *limit*: what the code does not handle;
   - the *trigger*: the event after which it must be replaced.
5. **Flag** a row when it has no trigger, when the trigger is not observable ("later", "someday", "if needed"), or when the code beside it already meets the trigger (for example the limit is 100 MB and the code reads a test file of 300 MB).

## Output

One row per comment, ordered by path:

```
| # | Location | Limit | Upgrade when | Flag |
|---|---|---|---|---|
| 1 | src/cache.rs:88 | reads the whole file into memory | files pass 100 MB | |
| 2 | src/queue.rs:41 | single consumer | | no trigger |
```

End with one line: `<n> shortcuts, <m> flagged.` With no markers, say `0 shortcuts` and that you searched with `git grep`, and with which paths.

## Rules

- Report; do not edit the code. Adding a missing trigger is a decision for the person who made the shortcut; suggest one in the reply when it is obvious.
- Do not invent a limit the comment does not state.
- If the repository is not a git repository, use `grep -rn` and say so.
