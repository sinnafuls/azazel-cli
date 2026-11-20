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
  history.jsonl                                     what you typed, across sessions
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
| `model_change` | A model chosen with `/model`, the model picker or `--model` |
| `thinking_change` | A thinking level chosen with `/thinking`, the thinking key or `--thinking` |
| `mode_change` | A change of permission mode |
| `agent_change` | The [primary agent](team.md#primary-agents) the lead switched to, with `tab`, `/agent` or `--agent`; resuming the session comes back on it |
| `context_edit` | An old tool result replaced by a short note (see compaction) |
| `compaction` | A summary standing in for older history |
| `checkpoint` | The snapshot taken before a prompt |
| `agent` | A sub-agent that finished, with the path of its transcript |
| `goal` | The state of a [goal](goals.md) |
| `label` | A label on an entry, given in `/tree`; an empty label takes it away |
| `session_info` | The session's name; the newest one counts |
| `pin` | Whether the session is pinned |
| `archive` | Whether the session is [archived](#archived-sessions): kept out of the lists of sessions |
| `custom` | Extension data, and under `customType` `run_code_store` (the name from before `run_code` became part of `eval`) a value a Luau script kept with `store` (`data.key`, `data.value`) |
| `run_started` | A run began (the work one prompt starts): `runId`, and the `requestId` its sender gave the prompt, if any. Its `parentId` is the prompt |
| `request_started` | A request of the run went out to `model`; `via` names the [virtual model](models-and-logins.md#virtual-models) that picked it, when one did |
| `tool_started` | A tool call of the run was let through and began: `callId`, `name` (the tool's own name), `input` (as it ran), and `repairs` when the call was [mended](tools.md#calls-that-are-mended) first |
| `run_ended` | The run ended: `runId` and `stop` (`stop`, `length`, `tool_use`, `error`, `aborted` or `failed`) |

The last four are the journal of a run. Tool results and replies are `message` entries as always; a
`run_started` with no `run_ended` is a run its process did not finish (see [interrupted runs](#interrupted-runs)).
Journal entries are notes beside the conversation: each hangs from the entry it was written after, and nothing
continues from one, so the conversation reads the same with or without them (an azazel from before 0.5 skips
them).

The entries form a tree. Going back and continuing from an earlier point adds a new branch; nothing is rewritten
or deleted, so every branch stays in the file. A line that cannot be parsed is skipped with a warning in the log
and left in place.

## Resuming

```sh
azazel -c                      # continue the most recent session of this project
azazel -r 0193a5                # by session id; a unique prefix is enough
azazel --session ./saved.jsonl  # by file, from anywhere
azazel --fork 0193a5 try another way   # continue in a copy, the original stays as it is
azazel -c -p "now add tests"    # all of them work with print mode too
```

| Flag | Behaviour |
|---|---|
| `-c`, `--continue` | Opens the newest session started in this folder (see [Sessions belong to a folder](#sessions-belong-to-a-folder)). Fails with `this project has no session to continue` when the project has none, and with `no session was started in this folder to continue` when it has some, but none from here |
| `-r`, `--session <PATH\|ID>` | Opens a session file, or the session whose id starts with `<ID>`. Fails when none or more than one matches. `--resume` is the old name |
| `--fork <PATH\|ID>` | Copies the session (its current branch) into a new one, whose header names the original, and opens the copy |
| `--name <NAME>` | Names the session, as `/name` would |
| `--session-dir <DIR>` | Keeps this run's sessions, and its sub-agents' transcripts, in `DIR` instead of the project's directory |
| `--no-session` | Writes the transcript to a temporary directory that is deleted when azazel exits, and turns checkpoints off |
| `--new` | Starts a new session, also when `[session] resume = "last"` would continue the newest one. Cannot go with `-c`, `--session` or `--fork` |

To always come back to where you left off, set `[session] resume = "last"` (or `/settings` → Sessions → Open at
start). Starting the UI in a project without `-c`, `--session`, `--fork`, `--new` or `--no-session` then opens the
newest session of this folder as `-c` does, on the same model and thinking level rules; in a project with no session
yet it starts a new one. Print mode (`-p`) is not affected: it starts a new session unless `-c` or `--session` is
given.

### Sessions belong to a folder

A project is the folder with `.git` or `.azazel/` and everything under it, and azazel can be started in any folder of
it. Sessions are keyed by the folder azazel was started in (the working directory, `--cwd`), as in Claude Code:
`foo/web` and `foo` each have sessions of their own, though all of the project's are kept in one place so that memory,
the code index, checkpoints and the prompt history stay shared.

- `-c`, `[session] resume = "last"`, `/resume` and `azazel sessions list` and `search` show the sessions started in the
  folder you are in. A session started before this existed shows in the folder it was started in, which its header
  records.
- To see the others, `/resume` has `ctrl+f` (all folders of the project, and back), and `azazel sessions list` and
  `search` take `--all-folders`. `--all-projects` includes every folder of every project.
- `--session <id>`, `--fork` and `azazel export <id>` open any session of the project by id, wherever it began.
- [Plain `azazel` attaches](background-and-remote.md#one-writer-per-session) only to a background session that was
  started in the same folder.
- What stays per project: the configuration in `.azazel/`, rules, commands, agents, skills, MCP servers, trust (trusting
  the project covers its folders), `AGENTS.md` files (read from the project root down to the folder), git, worktrees,
  checkpoints and memory. What is per folder: the sessions, the shell and relative paths, and where agents may write
  without asking ([Permissions](permissions.md#modes)).

The session id is the part of the file name after the underscore; `/export` without a path also puts it in the
name of the file it writes. A resumed session opens at its last entry and the UI shows the last two messages.

A session that a [background session](background-and-remote.md) has open cannot be opened a second time: it
fails with `this session is running in the background: azazel attach <id>`. `azazel attach` opens it where it
runs, and `azazel attach --take` brings it back here. Plain `azazel` that would continue such a session attaches to it
instead of failing.

### Resuming from the browser

In the page of `azazel remote`, `/resume` (also **Resume…** in the sidebar) lists the earlier sessions of the folders a
browser may start in, with a search over their names, folders and conversations. Picking one continues it in a
background session, as `azazel bg` would with `--session`; if a background session has it open already, that one is
opened, as sessions have one writer. It needs a control device. See
[starting a session from the browser](remote.md#starting-a-session-from-the-browser).

### Model and thinking level

A session that is opened again comes back on the model chain and the thinking level it last switched to. This
holds for `-c`, `--session`, `--fork`, `/resume` and `[session] resume = "last"`, in the UI, in print mode and over
RPC.

- What counts is the newest `model_change` and `thinking_change` entry on the branch the session ends on. `/model`,
  the model keys, `/thinking` and the thinking key write them, and so do `--model` and `--thinking` at launch.
- A session that never switched starts on what the run is on: the `lead` role at launch, the model you are
  using when you `/resume` it.
- `--model` and `--thinking` on the command line win over what the session last used.
- A thinking level written by `/thinking` or `--thinking` wins over the `:level` of the model's role; a session
  that never chose one asks for the role's level (see [thinking levels](models-and-logins.md#thinking-levels)).
- A chain that can no longer be used (the model or its login is gone) gives way to the run's own model, with the
  notice `this session last used <chain>, which cannot be used now; it goes on with <chain>`.
- `[session] restore_model = false` turns all of this off.
- Moving within the open session (`/tree`, `/rewind`, `/retry`, `/fork`) keeps the model you are on.

The permission mode comes from the current flags and [configuration](configuration.md), not from the session.

## Interrupted runs

A run that ends is recorded as ended, whether the agent finished, failed, or you stopped it with `Esc` or by
quitting. When the process is killed instead (a crash, a closed terminal, a machine that went down), the session
file ends in the middle of the run, and the next process that opens the session can go on with it:

- Tool calls of the last reply that have no result get one. A call that had not started yet runs as it would
  have. A call that had started is run again when it only reads (`read`, `glob`, `grep`, the reading actions of
  `lsp`, `web_fetch`, `web_search`, `web_grep`, `memory_search`, `semantic_search`, `session_recall`). Any other call that had
  started (`shell`, `write`, `edit`, an MCP tool, a sub-agent, ...) is not repeated: its result is `interrupted:
  the process stopped before this call finished; check its effects before retrying`, and the model decides what
  to do.
- A request the model never answered is sent again as it was.

| Where | How the run goes on |
|---|---|
| The UI | Opening the session says `the last run of this session was interrupted (stopped while shell ran); /continue goes on with it`. `/continue` does so. `azazel -c --continue-run` opens the UI and continues at once |
| Print mode | `azazel -c -p --continue-run` (or with `--session <id>`) continues instead of sending a prompt, and prints the reply. It fails with `no run of this session was interrupted` when there is nothing to continue |
| RPC | `get_state` has `interrupted`; the command `continue_run` continues. See [RPC mode](rpc.md) |
| A background session | `azazel ps` lists it as `interrupted`, and `azazel attach <id>` starts it again and continues. See [background sessions](background-and-remote.md#when-a-background-session-dies) |

Sending a new prompt instead of continuing is fine too: the calls that were left without a result are then
answered with a note that they may not have run, and the conversation goes on from your prompt. Nothing is
continued twice, and a run that was gone on with is the same run in the file: it gets its one `run_ended`.

A session copied with `/fork`, `--fork` or `/bg` holds the conversation and nothing of the journal, so a copy
never has a run to continue; the original keeps its own.

## Repairing a conversation

A provider refuses a conversation that is not well formed, with an error such as `tool_use ids were found
without tool_result blocks` or `text content blocks must be non-empty`. Conversations get that way rarely: a
file edited by hand, a chat imported from another harness, a process that died at the wrong moment. `/fix`
puts the conversation the lead would send right and says what it did:

| What is wrong | What `/fix` does |
|---|---|
| A tool call without a result, anywhere in the conversation | The call gets the result `interrupted: this call has no result, so it may not have run or may have run in part; check its effects before retrying`, marked as an error, in the tool message after the reply (a new one when there is none) |
| A tool result that answers no call of the reply before it, or a second result for the same call | The result is dropped |
| A text block with nothing but white space, an empty thinking block that carries no signature | The block is dropped |
| A tool call without an id or a name, a picture without data, a tool result outside a tool message | The block is dropped |
| A message left with no block at all | The message is dropped |

```text
fixed the conversation:
  answered 2 tool calls that had no result (read, shell): the agent is told they were interrupted
  dropped 1 empty block
the conversation as it was stays in the session as a branch; /tree shows it
```

A session file is only ever added to. When only the end is wrong (calls of the last reply without results), the
results are added after it. Otherwise the conversation is written again, repaired, from the first entry that
had to change, as a new branch that the session goes on from; labels, checkpoints and compactions follow to the
new entries, and the branch as it was stays where it is. What a compaction summarised is no longer sent and is
not looked at. A conversation with nothing wrong is left alone (`nothing to fix: 42 messages checked, ...`).

`/fix` needs the agent to be idle and works in a session that runs in this process. Calls it answered are not
run again by `/continue`; the request after them is.

## Starting azazel again

`/restart` ends the UI and starts the azazel binary again in the same terminal, on the session that was open:
the conversation, the model, the thinking level and the permission mode are as they were. It is how a running
session gets onto a new version after `azazel update` (or onto a staged release, which is put in place as the
UI ends when `[update] auto = true`), and how changes to settings that are read at the start take effect.

- The new process gets the options this one was started with (`--set`, `--cwd`, `--theme`, `--add-dir`, the
  `--no-*` switches and so on), without the prompt and without what chose the session (`-c`, `-r`, `--fork`,
  `--name`): it opens the session by its file, `-r <file>`. The model, thinking level and mode are passed as
  they are now, not as they were given at the start.
- A session elsewhere is opened again where it runs: `azazel attach <id>` for a background session,
  `--ssh <host> --attach <id>` for one on another machine, `--ssh <host> -c` for a session over SSH.
- A `--no-session` run cannot be restarted: it keeps no file to open.
- The agent must be idle. Queued messages, stashed drafts and what is typed in the input are not carried over.
- On Linux and macOS the new program takes the place of the old process. On Windows, where a process cannot be
  replaced, the new one is started in the same console and the old one stays, idle, until the new one exits,
  and ends with its exit code; the terminal and the shell see one program throughout.

## Names

A session gets a name in one of these ways:

- `--name <NAME>` on the command line.
- `/name <text>` in the UI. `/name` alone asks the `smol` role for a title and replaces the name with it.
- `Ctrl+R` on a session in the `/resume` picker.
- Automatically: after the lead's first reply in a session that has no name, the `smol` role is asked for a title
  of at most six words, from the first prompt and the first reply. It has 10 seconds and no tools. Turn this off
  with `[session] auto_name = false`.
- A name given by hand is never replaced by the automatic one.

Names are stored as `session_info` entries and show in the welcome line, `/session`, session lists and exports.

## Session commands

`/new`, `/resume`, `/tree`, `/fork`, `/retry`, `/continue`, `/fix`, `/handoff` and `/restart` need the agent to be idle
(press `Esc` first).

| Command | What it does |
|---|---|
| `/new` | Starts a new session and keeps the current model |
| `/resume` | Lists the project's sessions, pinned ones first, then by last use; type to filter, `Enter` opens one. See [the session picker](#the-session-picker) |
| `/name [text]` | Names the session; without text, has a name suggested |
| `/session` | Shows the session's file, id, name, when it was created and last used, messages by who wrote them, branch points, tokens and cost of the lead, of sub-agents and of requests on the side, the model chain, checkpoints and the agents that ran |
| `/search <text>` | Lists the lines of this project's sessions that contain the text (case-insensitive), each with its session and date; `Enter` resumes that session |
| `/tree` | Lists every prompt and reply of this session, newest first; `•` marks the current branch. Choosing one of your prompts goes back to before it and puts its text in the input; choosing a reply continues from it. See [labels and filters](#labels-and-filters-in-tree) |
| `/fork` | Copies the current branch into a new session file and switches to it. The original is unchanged |
| `/retry` (`F5`) | Goes back to before your last prompt and sends it again, with its images; `@` mentions are read again. It is checkpointed and hooked like any prompt. The first attempt stays in the session as another branch, which `/tree` shows |
| `/continue` | Goes on with the run that was cut short when the session's process was killed in the middle of it. See [interrupted runs](#interrupted-runs) |
| `/fix` | Repairs a conversation the provider refuses and lists what it changed. See [repairing a conversation](#repairing-a-conversation) |
| `/restart` | Starts azazel again in this terminal on this session. See [starting azazel again](#starting-azazel-again) |
| `/debug` | Shows, full screen, where azazel keeps its files (and which of them are not there), the state of the UI and the session, the variables that describe the terminal, the names of the `AZAZEL_` variables that are set, and the last 200 lines of the log with secrets hidden |
| `/handoff [what matters]` | Has the current model write a structured brief of this session, then opens a new session whose first message is that brief and whose header names this session. Nothing runs until you send a message. Both ids are shown. See [handoff](#handoff) |
| `/consult [question]` | Pick a saved session and ask it a question. The answer comes from that session's conversation in a single request, shows in a panel (`Enter` on a row closes it, copies it or opens it full screen) and is stored in neither session |
| `/export [path]` | Writes the conversation to `path` (relative to the working directory), or to `azazel-session-<id>.md`. The extension picks the format: `.md`, `.html` or `.jsonl` |
| `/share` | Uploads the conversation, secrets hidden, as a secret gist and prints its link. It asks first. See [sharing a session](#sharing-a-session) |
| `/bug [what went wrong]` | Writes a bug report to a zip under `~/.azazel/reports/` and offers to open an issue. See [bug reports](#bug-reports) |
| `/compact [what to keep]` | Summarises the conversation now; the optional text is passed to the summariser as extra instructions |

`/tree` moves within the conversation only; files on disk are not changed. Use `/rewind` to move files as well.

### Handoff

`/handoff` (the `handoff` command of [RPC](rpc.md), and `/handoff` in the web page, which switches to the new
session) moves the work to a fresh session when the context has grown large.

1. A draft brief is written first, without a model, to `handoff-<session id>.md` in the session's folder:
   the requests you made and whether they were answered, the task list and the last reply, and the files. A
   request that fails still leaves it, and the error names it. Secrets in it are hidden.
2. One request goes to the current model, which gets the conversation and writes the brief in sections:
   Requests (with their status), Where things stand, Rules you set, Files, Errors already solved, Next steps. It
   is held to about 1500 words and told not to repeat `AGENTS.md`.
3. The files are ranked by what the session did to them, not left to the model: an edit, write or patch counts
   3, a read 2, a path named in a shell command 1. The 15 highest that still exist are listed.
4. The new session starts with the brief (secrets hidden) and the old session's name plus `(continued)`.

With `[session] handoff_at` set, azazel offers `/handoff` once per session when the context passes that many
tokens, and with `[cache] suggest_handoff_at` it says so when the [prompt cache](models-and-logins.md#prompt-cache)
is about to run out. Neither makes a request.

### Branch summaries

Going back with `/tree` leaves a branch: everything that was said after the point the conversation goes on
from. The agent would know nothing of it. So the `smol` role is asked to write up what happened there in at
most six lines (what was asked for, what was done, what was found out, what was left unfinished), and that is
added to the conversation where it goes on, as a note of yours that starts with `I went back to an earlier
point of this conversation. What happened on the branch I left, in short:`. The note asks for no reply: it is
there when you send your next prompt, and the transcript shows `the agent was told what happened on the branch
you left:` with the lines.

- The request has no tools and 60 seconds; it gets the branch as text, tool results cut to 400 characters and
  the middle of a long branch left out. What it costs is counted in the run's spend as `branch summary`.
- Nothing is written when the agent did nothing on the branch (a prompt that was never answered), when the
  summary fails, or when the conversation has moved elsewhere again before it arrives.
- Moving forward along the same branch leaves nothing, and nothing is summarised.
- `/retry`, `/rewind` and `/fork` do not summarise.
- Turn it off with `[session] branch_summary = false`.

`/handoff` and `/consult` are requests outside the conversation: the model gets the lead's system prompt and the
conversation, may not use tools, and its reply is not added to any session. What they cost is counted in the
run's spend and listed by `/session`. `Esc` stops one while its reply arrives.

### The session picker

`/resume` and `/consult` list sessions with their name (or first prompt), whether they are pinned or open, when
they were last used and how many messages they hold. Keys act on the selected row:

| Key | Does |
|---|---|
| `Enter` | Opens the session (`/resume`) or asks it (`/consult`) |
| `Ctrl+R` | Renames it |
| `Ctrl+D` | Deletes its file and its sub-agents' transcripts, after asking. The open session cannot be deleted |
| `Ctrl+P` | Pins or unpins it. Pinned sessions are listed first and are never pruned |
| `Ctrl+N` | Shows named sessions only, or all again |
| `Tab` | Shows its last six messages above the list |

Naming, pinning or labelling a session is not use: it does not move the session up the list and does not save it
from `sessions prune`.

### Labels and filters in `/tree`

| Key | Does |
|---|---|
| `Alt+L` | Labels the selected entry; the label shows beside it. An empty label takes it away |
| `Ctrl+L` | Lists labelled entries only, or all again |
| `Ctrl+U` | Lists your prompts only, or all again |

The tree filters as you type, so its keys are chords.

### Prompt history

What you send in the UI (prompts and `!` shell lines, with the `!`) is added to `history.jsonl` in the project's
data directory, one `{"ts", "session", "text"}` object per line. It is cut back to its newest 5,000 lines when it
passes 6,000.

- `Up` and `Down` in the input step through the newest 1,000 entries, across sessions.
- `Ctrl+R` lists the project's history newest first, each prompt once, with the name of the session it was sent
  in; type to filter, `Enter` puts the prompt in the input.
- A `--no-session` run adds nothing to it.

## From the shell

| Command | What it does |
|---|---|
| `azazel sessions list [--all-projects] [--all-folders] [--all] [--json]` | One line per session started in this folder (`--all-folders`: every folder of the project): id, last used, name or first prompt, messages, cost, pinned. [Archived](#archived-sessions) sessions only with `--all` |
| `azazel sessions search <text> [--all-projects] [--all-folders] [--json]` | Sessions of this folder (`--all-folders`: of the project) whose messages contain the text (case-insensitive), as `date · name · matching line` |
| `azazel sessions delete <PATH\|ID> [--yes]` | Deletes the session file and its sub-agents' transcripts. Asks first; without a terminal it needs `--yes` |
| `azazel sessions prune [--older-than 30d] [--all-projects] [--dry-run]` | Deletes unpinned sessions not used for that long (`h`, `d` or `w`) |
| `azazel sessions export [PATH\|ID] [--format F] [-o FILE] [--sanitize]` | Same as `azazel export` |
| `azazel sessions import <FILE\|URL> [--project DIR]` | Makes a new session of this project, or of the project of `DIR`, from a session file or a JSON lines export (`--format jsonl`), read from a file or fetched from an `https://` address. Its entries are taken as they are; it gets an id of its own, and the time of its first entry |
| `azazel open` | Starts the UI with the [`/open`](#projects-and-directories) picker up |
| `azazel stats [--days 30] [--period day\|week\|month\|year\|all] [--since DATE] [--by provider\|model\|project\|agent] [--json] [--rebuild]` | Requests, tokens and cost from the [usage log](#usage-statistics), with a one-line chart and a table per provider, model, project or agent. `/stats` opens the same in a panel |
| `azazel context [PATH\|ID] [--json]` | What the last request of a session was made of: tokens per category (each part of the system prompt, tool schemas, messages, tool results, images). The latest session of this project by default; see [the context inspector](tui.md#the-context-inspector) |
| `azazel export [PATH\|ID] [--format markdown\|html\|jsonl] [-o FILE] [--sanitize]` | Writes a session, the latest of this project by default. The format comes from `--format`, else the file's extension, else Markdown. `-o -` writes to standard output. `--sanitize` writes it [for sharing](#sanitized-exports) |
| `azazel bug [what went wrong] [--transcript] [--session PATH\|ID] [--issue]` | Writes a [bug report](#bug-reports) to a zip under `~/.azazel/reports/` |

Export formats:

| Format | Holds |
|---|---|
| Markdown | Your prompts, the assistant's text and one line per tool call |
| HTML | One file with its own styles and no scripts: messages, tool calls with their output (diffs coloured) and thinking folded under `<details>`, images inline |
| JSON lines | The branch's entries exactly as stored in the session file |

With `tui.redact` on (the default), what looks like a secret is replaced by `[redacted:<kind>]` in a Markdown
or HTML export, as it is [on screen](tui.md#hiding-secrets). A JSON lines export holds the entries as stored,
secrets included, so that it opens as a session again: mind where you send it.

### Sanitized exports

`azazel export --sanitize` and `/export [path] --sanitize` write a session to share with someone who should
not see your code or your keys, in any format:

- what looks like a secret is replaced by `[redacted:<kind>]` everywhere, whatever `tui.redact` says, the JSON
  lines included;
- the results of the tools that read, search or change files (`read`, `grep`, `edit`, `write`, `patch`, `lsp`,
  `ast_grep`) become `[file content removed]`, and so does the text `write`, `edit` and `patch` were given;
- the conversation, the other tools' output and pictures stay.

A sanitized JSON lines export still opens as a session (`azazel sessions import`), without what was removed.

With `[session] retention_days = N` (0, the default, keeps everything), sessions of the project not used for `N`
days are deleted in the background when azazel starts, at most once a day. Pinned sessions and the open one are
kept.

A Markdown export holds your prompts, the assistant's text and one line per tool call; tool output and thinking are
left out. Compacted history appears under the heading `Earlier (summarised)` in Markdown and HTML.

Conversations held in pi, omp, jcode, Claude Code or Codex CLI become sessions with `azazel migrate --only chats`
(or `/migrate`): each in the project it was held in, named `imported from <source>: <title>`, with the times
of the original. See [migrating](migrating.md#conversations).

### Archived sessions

`azazel -p --archived "..."` keeps the session of the run out of `/resume`, `/open`, `-c` and `azazel sessions
list`: a script or a CI job can run azazel without filling your lists. The session is kept as any other;
`azazel sessions list --all` lists it, marked `archived`, and `-r <id>` opens it.

## Projects and directories

A session belongs to the project it works in, and azazel reads that project once, when it starts: its root, its
trust, its configuration, context files, rules and MCP servers. The commands below go to another directory by
starting azazel again there, in the same terminal, the way [`/restart`](#starting-azazel-again) does: the
session goes on, and everything about the project is read afresh, the trust question included when the
project has configuration that needs it.

| Command | What it does |
|---|---|
| `/open` | A picker of the recent sessions of every project, newest first, each with the directory it works in; type to filter. Its keys rename, delete, pin and preview as in [the session picker](#the-session-picker). A session of this project opens in place, as with `/resume`; one of another project opens there, on the model and thinking level it last used. `azazel open` starts the UI with this picker up |
| `/cd <dir>` | The session goes on in another directory, relative to the one it is in, or under `~`; `Tab` completes directory names. A directory of the same project keeps the session where it is; one of another project moves the session into that project's sessions |
| `/worktree [name]` | Makes a [worktree](#worktrees) and the session goes on in it, with the uncommitted changes |
| `/worktree list` | The worktrees of the project, with their branches, the one the session is in marked `(here)` |
| `/worktree remove <name> [--force]` | Removes a worktree. One with uncommitted changes is kept unless `--force`; its branch is deleted only when another branch holds all its commits |

A session that moves to another project takes its sub-agents' transcripts with it, and from then on is listed
there. Its [checkpoints](#checkpoints) and saved tool outputs stay with the project they were taken in, so
`/rewind` goes back no further than the move. A `--trust` given on the command line is not carried into
another project (it is into a worktree, which holds the same configuration). A session that keeps no file
(`--no-session`) cannot move, and a session that runs elsewhere (`azazel attach`, `--ssh`) does none of this.

### Worktrees

`/worktree [name]` makes `.azazel/worktrees/<name>` of the project (of the main project, when the session is
already in a worktree) on a new branch `azazel/<name>` from the HEAD of the tree the session is in; without a
name it is named after the time. The session goes on in the same place inside it (`crates/x` stays `crates/x`).

The uncommitted changes move with the session: they are stashed in the tree it leaves (`git stash push
--include-untracked`, the project's `.azazel/` left out), and the stash is popped in the new worktree, which
starts at the same commit, so they apply as they were, staged changes staged. The tree it left is clean
afterwards. Ignored files stay where they are. When the stash cannot be applied there, it is put back where it
was and nothing is made; should it apply only in part, the worktree stays with what did, the stash (`git stash
list`) keeps all of it, and you finish by hand.

A worktree of a project you trust is trusted. `azazel -p --worktree[=NAME]` runs a prompt in a new worktree in
the same way, from HEAD: it takes no uncommitted changes, and leaves the tree it was started in as it is. Its
path and branch are printed at the end, on standard error, or as the last line `{"type":"worktree",...}` with
`--output json`.

## Sharing a session

`/share` uploads the current branch of the conversation to GitHub as a **secret gist** and prints its link. It
goes through the GitHub program [`gh`](https://cli.github.com), which must be installed and signed in
(`gh auth login`); the gist belongs to the account `gh` is signed in to.

Before anything is sent, a box says what is: how many messages and how large, the two files, and that anyone who
has the link can read them. `Share` uploads; `Cancel` or `Esc` sends nothing.

| File | Holds |
|---|---|
| `azazel-session-<id>.md` | Your prompts, the replies and one line per tool call. GitHub shows it as the gist's page |
| `azazel-session-<id>.view.html` | The HTML export: the same with tool output, diffs, thinking and images. Download it and open it in a browser |

Both are written with secrets hidden, as an [export](#from-the-shell) with `tui.redact` on is, whatever
`tui.redact` is set to. That hides what looks like a key, a token or a password. It does not hide file contents,
paths or command output, so read the box before you say yes. A secret gist is not listed anywhere, but it is not
private: `gh gist delete <link>` takes it down.

## Bug reports

`/bug [what went wrong]` and `azazel bug [what went wrong]` write one zip, `~/.azazel/reports/azazel-report-<time>.zip`,
and say what is in it:

| File | Holds |
|---|---|
| `report.md` | What you said went wrong, azazel's version, the operating system, the terminal, the default model, and what `azazel doctor` found wrong |
| `doctor.txt` | What `azazel doctor` prints |
| `config.toml` | The settings in effect, defaults included. Keys, tokens, passwords, headers and environment tables are replaced by `[removed]`; names of variables and of programs stay |
| `log.txt` | The last 256 KB of `~/.azazel/logs/azazel.log`, secrets hidden |
| `transcript.html` | Only when asked for: the HTML export of a session, secrets hidden |

The zip stays on your machine. Nothing is uploaded unless you say so, and then only the summary:

- In the UI, `/bug` asks whether to write the report with or without the open conversation. Once it is written,
  and when `gh` is installed, it shows the text of an issue on `sinnafuls/azazel-cli` (`report.md` without its
  title) and asks whether to open it; the first row is `No`.
- `azazel bug` at a terminal asks the same two questions. Elsewhere it asks nothing: `--transcript` includes the
  latest session of the project, `--session <PATH|ID>` another one, and `--issue` opens the issue. Without
  `--issue` nothing leaves the machine.

The issue is opened with `gh issue create`, as the account `gh` is signed in to, and its link is printed. The
zip is never attached: add it to the issue yourself after reading it, if it is asked for.

## What the last request was made of

Each time the lead sends a request, the session's own folder (`<sessions>/<session id>/`, beside its
sub-agents' transcripts) gets a `context.json`: the request's tokens by category, written again when the provider
has counted the request. `/context` and `azazel context` read it; see
[the context inspector](tui.md#the-context-inspector). It holds numbers only, never the text of the request, and
is deleted with the session.

## Usage statistics

Every answered model request is one line in `~/.azazel/usage/YYYY-MM.jsonl` (the local month of the request): the
time, provider, model, project, session, who asked (`lead`, a sub-agent's id, or the purpose of a side request:
`btw`, `prediction`, `memory`, `name`, `advisor`, `judge`, `compaction`, `web_search`, ...), the tokens, the cost
estimated from the model catalogue, and whether the request went through a subscription login. Sub-agents and side
requests are counted, and a request that was retried is counted once, for the attempt that answered. The log is
kept apart from the sessions: `session.retention_days`, `team.agent_retention_days` and `azazel sessions delete`
never remove it. `[usage] ledger = false` stops writing it.

The first time the log is read, azazel imports the history of every project's sessions and sub-agent transcripts,
each reply at the time it was made, and writes `~/.azazel/usage/backfilled`. `azazel stats --rebuild` imports again;
a reply that is already in the log is not added twice.

`azazel stats` prints a header with the totals, a one-line chart, and a table. `--period` takes `day`, `week`,
`month`, `year` or `all` (`--days N` and `--since 2026-03-01` name other windows; the default is 30 days). `--by`
groups the table by `provider` (with its models under it), `model`, `project` or `agent`. `--json` prints one object.
Tokens are shown as *fresh* (input, output and cache writes) and *cache read* apart: a cache read repeats the same
context on every request and would make the totals look far larger than what was asked. A cost marked `≈` includes
requests made through a subscription login; it is what those tokens would cost at API prices, not what you paid.

## Compaction

Long sessions outgrow the model's context window. Azazel keeps an estimate of the context size and reduces it in
two steps. Both are recorded as new entries; the original messages stay in the session file.

1. **Eliding old tool results.** When the estimate passes `micro_threshold` of the window, the body of every tool
   result older than the newest `keep_tool_results` becomes
   `[result elided: <n> bytes; session_recall expand=<entry id>]`. Eliding changes messages the provider has
   cached, so the next request writes the whole conversation to the cache again; it is therefore done in
   batches, once the results to elide add up to `micro_min_savings` of the window (or when a summary is due).
2. **Summarising.** When the estimate comes within `reserve_tokens` of the window, everything except roughly the
   last `keep_recent_tokens` is replaced by a summary with the headings Goal, Constraints, Progress, Decisions,
   Files read, Files modified and Next steps. The cut never separates a tool call from its result, and an earlier
   summary is merged into the new one. The summary is written by the `smol` role, or by the agent's own model
   when that role resolves to nothing.

Neither step loses anything: the agent reads the originals back with
[`session_recall`](tools.md#recalling-the-conversation). An elision note names the entry that holds the result.
The summariser sees every message labelled with its entry id and puts the id after a fact whose detail it leaves
out (`src/parser.rs was read [1a2b3c4d]`), and the summary ends with a note naming the newest entry it covers:

```text
[The messages summarised above are kept in the session, up to entry 5e6f7a8b. session_recall finds them by
`query` and returns them in full by `range` or `expand`; an id in square brackets above is the entry that
holds the detail.]
```

An agent that is not offered `session_recall` (a sub-agent whose definition leaves it out) gets the older note
instead, `[result elided: <n> bytes; re-run the tool if needed]`, and a summary without ids.

The UI reports `context compacted: <before> → <after> tokens`; `/context` shows what the context is made of. If
a provider rejects a request as too large, azazel summarises and retries once.

```toml
[compaction]
enabled = true              # false turns both automatic steps off; /compact still works
reserve_tokens = 16384
keep_recent_tokens = 20000
keep_tool_results = 8
micro_threshold = 0.6
micro_min_savings = 0.1
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
- So are the patterns of `checkpoints.exclude`, written like `.gitignore` lines. The default leaves out `.azazel/worktrees/` (sub-agent worktrees); a list of your own replaces it. The list is applied each time a session starts; a file a snapshot already holds stays in the snapshots after it is excluded.
- Only files under the project root are covered; effects outside it cannot be undone.
- A prompt that changed no file since the last snapshot takes none of its own: its checkpoint is the snapshot that is already there.
- A fork continues the history of the session it was copied from, so the prompts it inherited can be rewound to in the fork as well.
- When a session ends, the repository is packed once it has gathered a few hundred loose objects (`git gc --auto` with `gc.auto` at 256).

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
before a rewind)`. Choosing it undoes the rewind. A rewind that fails part of the way (a file another program
holds open, a full disk) is taken back: the files are put as they were before it, and the error ends with
`the files are as they were before the rewind`.

### When checkpoints are off

| Reason | Message |
|---|---|
| `[checkpoints] enabled = false` | `checkpoints are turned off in the configuration` |
| `git` is not on `PATH` | `git is not installed` |
| The project root is your home directory | `the project root is your home directory` |
| The project root is a filesystem root | `the project root is a filesystem root` |
| The first snapshot of a project would add more than `checkpoints.max_files` files (default 100000) | `the project has <n> files, more than checkpoints.max_files (<limit>)` |
| A `git` command of a snapshot ran longer than `checkpoints.timeout_ms` (default 10000, kept between 1000 and 300000) and was stopped | `git add took longer than 10000 ms (`checkpoints.timeout_ms`)` |

The session carries on without checkpoints. When the cause is found at a snapshot (no `git`, too many
files, a `git` that does not finish in time), that prompt shows `checkpoints are off: <reason>` once. `/rewind` then answers
`nothing to rewind to: <reason>`, and `azazel doctor` reports whether checkpoints are on. Going back in the
conversation with `/tree` works either way.

```toml
[checkpoints]
enabled = true
max_files = 100000
timeout_ms = 10000                  # longest one git command of a snapshot may take
exclude = [".azazel/worktrees/"]    # patterns left out, as in .gitignore
```
