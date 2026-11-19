---
name: audit
description: "Use to audit a whole repository, not a diff: find what is most likely to be wrong or fragile today. Runs in a reviewer sub-agent, maps entry points and expected load first, then reports at most 20 ranked findings with path:line and evidence."
context: fork
agent: reviewer
---
# Repository audit

You audit the repository you are in, not a change. `$ARGUMENTS` may narrow the scope to a directory, a subsystem or a concern (for example "the sync code" or "load"); with no arguments, audit the whole repository. You only read: you change nothing.

## 1. Map first

Before judging anything, build a short map from what you read:

- **Entry points:** binaries, servers, handlers, scheduled jobs, command-line commands, message consumers, public library functions. Find them from manifests, `main` functions, routing tables and configuration.
- **Expected load:** who calls each entry point, how often, with how much data, and what it waits on (disk, network, locks, other services). Take it from the code, the documentation and the configuration, and say which is which. When it is a guess, label it a guess.
- **Trust boundaries:** where input from users, files, the network or other processes enters.
- **Where the risk sits:** state that persists, money or data that cannot be recovered, concurrency, anything that runs unattended.

Write the map first, in at most 15 lines.

## 2. Look where the map says it matters

Spend your reading on entry points and the code they reach, in this order: data loss and corruption, security at the boundaries, behaviour under the expected load (unbounded memory, work in loops, missing timeouts, lock contention), error handling that hides failures, then tests missing for the risky paths. Use `grep` for the shape of a defect across the whole tree, then read the hits. Do not report style.

Use the `security-review` skill's method for the boundary checks, and `git log` on a file when its history explains a hazard.

## 3. Report at most 20 findings

Rank them by how likely the failure is times how bad it is. Number them across the whole list, so the user can say "fix 3 and 7". For each:

```
1. [blocker|major|minor] path:line — what is wrong, in one sentence
   Evidence: the code, the call path or the command output that shows it
   If skipped: what happens, and when
```

Rules:

- No concrete failing case, no finding. If you cannot say what input or situation breaks it, it is a question, listed apart.
- Evidence is something you read or ran. Do not report what a tool would probably say.
- A `shortcut: ... ; upgrade when ...` comment is a decision, not a finding, unless its trigger has already happened.
- Fewer than 20 is fine. Do not fill the list.

## 4. End with coverage

Finish with:

- a verdict in one line;
- **Not read:** directories, generated code, vendored code, large files and anything you skipped, and why;
- **Not run:** builds, tests or tools you did not run.
