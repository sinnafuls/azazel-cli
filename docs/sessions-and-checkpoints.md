# Sessions and checkpoints

A session is one conversation with the lead agent, stored as a file so it can be continued, branched and exported.
A checkpoint is a snapshot of the project's files taken before each prompt, so that `/rewind` can put the files,
the conversation or both back to an earlier point. This page covers where sessions live, how to resume and branch
them, how context compaction works, and how checkpoints behave.

## Where sessions live

Everything about a project is kept outside the project, under the azazel home (`~/.azazel`, or `AZAZEL_HOME`):

```text
~/.azazel/projects/<slug>/
  sessions/<utc-timestamp>_<session-id>.jsonl       one file per session
  sessions/<session-id>/agents/<agent-id>.jsonl     transcripts of that session's sub-agents
  artifacts/                                        tool output too large for the context
  shadow.git/                                       the checkpoint store
```

`<slug>` is the project root's path with every run of non-alphanumeric characters turned into `-`, followed by
`-` and 8 hex digits of a hash of the path. Sessions belong to the project root, so starting azazel anywhere
inside a project finds the same sessions.

### File format

A session file is append-only JSON Lines. The first line is a header; every other line is an entry with an `id`
(8 hex digits), the `parentId` it follows, a `timestamp` and a `type`:

```json
{"type":"session","version":1,"id":"<uuid>","timestamp":"…","cwd":"…","parentSession":null}
{"id":"a0b1c2d3","parentId":null,"timestamp":"…","type":"checkpoint","turnEntryId":"","commit":"<sha>"}
{"id":"e4f5a6b7","parentId":"a0b1c2d3","timestamp":"…","type":"message","message":{"role":"user","content":[{"type":"text","text":"…"}]}}
```

| Entry type | Records |
|---|---|
| `message` | A user, assistant or tool message |
| `model_change` | A model chosen with `/model` or the model picker |
| `mode_change` | A change of permission mode |
| `context_edit` | An old tool result replaced by a short note (see compaction) |
| `compaction` | A summary standing in for older history |
| `checkpoint` | The snapshot taken before a prompt |
| `agent` | A sub-agent that finished, with the path of its transcript |
| `goal` | The state of a [goal](goals.md) |
| `label`, `session_info`, `custom` | A label on an entry, a session name, extension data |

The entries form a tree. Going back and continuing from an earlier point adds a new branch; nothing is rewritten
or deleted, so every branch stays in the file. A line that cannot be parsed is skipped with a warning in the log
and left in place.

## Resuming

```sh
azazel -c                      # continue the most recent session of this project
azazel -r 0193a5                # resume by session id; a unique prefix is enough
azazel -c -p "now add tests"    # both work with print mode too
```

| Flag | Behaviour |
|---|---|
| `-c`, `--continue` | Opens the newest session. Fails with `this project has no session to continue` when there is none |
| `-r`, `--resume <ID>` | Opens the session whose id starts with `<ID>`. Fails when none or more than one matches |

The session id is the part of the file name after the underscore; `/export` without a path also puts it in the
name of the file it writes. A resumed session opens at its last entry and the UI shows the last two messages. The
model and permission mode come from the current flags and [configuration](configuration.md), not from the
session.

## Session commands

All of these need the agent to be idle (press `Esc` first), except `/compact` and `/export`.

| Command | What it does |
|---|---|
| `/new` | Starts a new session and keeps the current model |
| `/resume` | Lists the project's other sessions by first prompt, time and message count; type to filter, `Enter` opens one |
| `/tree` | Lists every prompt and reply of this session, newest first; `•` marks the current branch. Choosing one of your prompts goes back to before it and puts its text in the input; choosing a reply continues from it |
| `/fork` | Copies the current branch into a new session file and switches to it. The original is unchanged |
| `/export [path]` | Writes the conversation as Markdown to `path` (relative to the working directory), or to `azazel-session-<id>.md` |
| `/compact [what to keep]` | Summarises the conversation now; the optional text is passed to the summariser as extra instructions |

`/tree` moves within the conversation only; files on disk are not changed. Use `/rewind` to move files as well.

The export holds your prompts, the assistant's text and one line per tool call. Tool output and thinking are left
out, and compacted history appears under the heading `Earlier (summarised)`.

## Compaction

Long sessions outgrow the model's context window. Azazel keeps an estimate of the context size and reduces it in
two steps. Both are recorded as new entries; the original messages stay in the session file.

1. **Eliding old tool results.** When the estimate passes `micro_threshold` of the window, the body of every tool
   result older than the newest `keep_tool_results` becomes
   `[result elided: <n> bytes; re-run the tool if needed]`.
2. **Summarising.** When the estimate comes within `reserve_tokens` of the window, everything except roughly the
   last `keep_recent_tokens` is replaced by a summary with the headings Goal, Constraints, Progress, Decisions,
   Files read, Files modified and Next steps. The cut never separates a tool call from its result, and an earlier
   summary is merged into the new one. The summary is written by the `smol` role, or by the agent's own model
   when that role resolves to nothing.

The UI reports `context compacted: <before> → <after> tokens`; `/context` shows how full the window is. If a
provider rejects a request as too large, azazel summarises and retries once.

```toml
[compaction]
enabled = true              # false turns both automatic steps off; /compact still works
reserve_tokens = 16384
keep_recent_tokens = 20000
keep_tool_results = 8
micro_threshold = 0.6
```

`pre_compact` and `post_compact` [hooks](hooks.md) run around a summary; a `pre_compact` hook can cancel `/compact`.

## Checkpoints

Before each prompt you send (in the UI, in print mode and over [RPC](rpc.md)), azazel snapshots the project tree
and adds a `checkpoint` entry to the session. Messages sent while the agent is already working do not take one.

Snapshots are stored with `git` in a separate bare repository, `shadow.git` in the project's data directory. Your
own repository, its index and its history are never touched, and the project does not need to be a git repository.
Because the whole tree is captured, changes made by shell commands are covered as well as those made by `write`
and `edit`.

- Files matched by the project's `.gitignore` are neither saved nor restored.
- `.azazel/worktrees/` (sub-agent worktrees) is excluded.
- Only files under the project root are covered; effects outside it cannot be undone.

### `/rewind`

`/rewind`, or `Esc` twice on an empty input while the agent is idle, lists the earlier prompts of the current
branch, newest first, each with a summary of how the files have changed since. Pick a prompt, then what to
rewind:

| Choice | Files | Conversation |
|---|---|---|
| `Code and conversation` | Restored to how they were before that prompt | Goes back to before that prompt; its text returns to the input |
| `Code only` | Restored | Unchanged; the agent is told the files were rewound and to read them again |
| `Conversation only` | Unchanged | Goes back to before that prompt; its text returns to the input |

Restoring files first snapshots the current state, which then shows up in the list as `(files as they were
before a rewind)`. Choosing it undoes the rewind.

### When checkpoints are off

| Reason | Message |
|---|---|
| `[checkpoints] enabled = false` | `checkpoints are turned off in the configuration` |
| `git` is not on `PATH` | `git is not installed` |
| The project root is your home directory | `the project root is your home directory` |
| The project root is a filesystem root | `the project root is a filesystem root` |
| The first snapshot of a project would add more than `checkpoints.max_files` files (default 100000) | `the project has <n> files, more than checkpoints.max_files (<limit>)` |

The session carries on without checkpoints. When the cause is found at the first snapshot (no `git`, too many
files), that prompt shows `checkpoints are off: <reason>` once. `/rewind` then answers
`nothing to rewind to: <reason>`, and `azazel doctor` reports whether checkpoints are on. Going back in the
conversation with `/tree` works either way.

```toml
[checkpoints]
enabled = true
max_files = 100000
```
