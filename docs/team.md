# The team

An azazel session is a team, not one chatbot. The agent you talk to is the **lead**. It can hand work to **sub-agents**: specialists that run in parallel, each with a fresh context, its own model chain, its own tool set and its own transcript. You can watch, message or stop any of them while they work. Everything runs inside the one `azazel` process.

## How it works

- The lead (id `lead`) starts sub-agents with the `task` tool. Each one is created from an **agent definition** and sees only the prompt written for it.
- A sub-agent's last message is its **report**. That report is all its parent sees of the work.
- Ids are `<definition>-<n>` (`scout-1`, `scout-2`, …) unless the caller gives a `name`.
- At most `team.max_parallel` sub-agents run at once; the rest are `queued`.
- States: `queued`, `running`, `waiting for approval`, `done`, `failed`, `stopped`.
- Transcripts are kept beside the session: `~/.azazel/projects/<slug>/sessions/<session id>/agents/<agent id>.jsonl`.
- Sub-agents follow the session's [permission mode](permissions.md). Their approval prompts appear in the same place as the lead's, titled with the agent id.
- Two agents may work in the same tree. `write` and `edit` refuse a file the agent has not read since it last changed, and an agent whose file was changed by another is told `<path> was modified by <agent id>; read it again before editing` before its next step.

## Built-in agents

| Name | Model role | Tools | Turn limit | Purpose |
|---|---|---|---|---|
| `scout` | `@smol` | `read`, `glob`, `grep`, `semantic_search`, `web_search`, `web_fetch` | 40 | Fast read-only exploration: where something lives, how it works |
| `planner` | `@plan` | scout's tools, plus `write` and `edit` limited to `.azazel/plans/**` | 80 | Writes an implementation plan file before code is touched |
| `coder` | `@default` | all; may spawn `scout` | 150 | Implements a scoped change and proves it by building and testing |
| `tester` | `@default` | all | 120 | Writes and runs tests, reports precise failures |
| `reviewer` | `@slow` | `read`, `glob`, `grep`, `shell` limited to `git diff`, `git log`, `git show` | 80 | Independent review of a diff |
| `verifier` | `@verify` | `read`, `glob`, `grep`, `shell`; `write` and `edit` disallowed | 80 | Checks a claim of completion by running commands (see [goals](goals.md)) |
| `researcher` | `@default` | `web_search`, `web_fetch`, `browser`, `read` | 80 | Research outside the repository, with sources |
| `designer` | `@default` | all; skill `frontend-design`; may spawn `scout` | 150 | UI work checked in a real browser |
| `artist` | `@default` | `generate_image`, `generate_video`, `generate_3d`, `generate_audio`, `sprite_sheet`, `read`, `write`, `shell` | 100 | Produces image, video, 3D and audio assets |
| `playtester` | `@vision` | `shell`, `job`, `screen`, `browser`, `read` | 80 | Runs and plays a build, reports defects with frames |

Roles are explained in [models and logins](models-and-logins.md#roles). The lead is told each definition's name and description and decides when to delegate; you can also just ask ("use three scouts to map `src/`, `tests/` and `docs/`").

## Writing your own agent

A definition is a Markdown file: YAML frontmatter, then the system prompt.

```markdown
---
name: migrator
description: Use to write and check database migrations. Give it the schema change wanted.
tools: read, glob, grep, edit(migrations/**), write(migrations/**), shell(sqlx migrate:*)
model: ["@slow", "@default"]
thinking: medium
max-turns: 60
skills: [testing]
---
You write SQL migrations for this project. Read the existing files under
`migrations/` first and follow their naming. Never edit a migration that has
already been applied. Report the files you created and the commands you ran.
```

| Key | Value | Default |
|---|---|---|
| `name` | Required. Lowercase words joined by hyphens, at most 64 characters | |
| `description` | Required. Shown to the lead: say when to use the agent | |
| `tools` | Comma-separated string or list. `tool(pattern)` grants the tool and restricts this agent to calls matching the pattern ([rule grammar](permissions.md#rules)) | every tool |
| `disallowed-tools` | Tool names removed from the set | none |
| `model` | A spec or a list of specs (fallback chain) | `@default` |
| `thinking` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` | `off` |
| `mode` | `inherit` or `plan` (read-only whatever the session mode is) | `inherit` |
| `max-turns` | Model requests per prompt before the agent is told to give its final report | no limit |
| `skills` | [Skills](skills-and-commands.md) whose text is appended to the system prompt | none |
| `spawns` | Definitions this agent may itself start | none |
| `isolation` | `none` or `worktree` | `none` |

`disallowedTools`, `maxTurns` and `permissionMode` are accepted as aliases. Other keys are ignored.

### Where definitions live

Highest precedence first; a definition replaces a lower one with the same `name`:

1. `<project>/.azazel/agents/`
2. `<project>/.claude/agents/` (only while `compat.claude` is true)
3. `~/.azazel/agents/`
4. Agents of installed [packages](packages.md)
5. Built-in

Every `*.md` below these directories is read. A file that fails to parse is listed with its error by `/agents`; it is never dropped silently.

Files under `.claude/agents/` are read as written for Claude Code: its tool names (`Read`, `Write`, `Edit`, `Bash`, `Glob`, `Grep`, `WebFetch`, `WebSearch`, `Task`, `TodoWrite`) map to azazel's, and the model aliases `haiku`, `sonnet`, `opus` map to `@smol`, `@default`, `@slow`.

### Overriding a definition in config

```toml
[agents.scout]
model = ["ollama/<local-model>", "@smol"]
thinking = "low"

[agents.reviewer]
thinking = "high"
```

`[agents.<name>]` takes `model` (a list) and `thinking`. It applies to any existing definition, built-in or your own. `model = []` removes the definition's chain so the agent uses `@default`.

## Orchestration tools

| Tool | Input | Behaviour |
|---|---|---|
| `task` | `agents: [{agent, prompt, name?, model?, isolation?}]`, `wait?` (default `true`) | Starts every listed agent; they run in parallel. With `wait` it returns all reports (each cut at 8000 characters, with a pointer to `agent://<id>`). With `wait: false` it returns the ids at once and each result arrives later as an `<agent-result>` message. If one agent cannot be started, none are. Cancelling a waiting call stops the agents it started. |
| `agent_send` | `to` (an id, `parent`, or `all`), `message` | A working agent sees the message before its next step. A finished one is revived with it, keeping its context. |
| `agent_stop` | `id`, `reason?` | Stops the agent and every agent it started. |
| `agent_wait` | `ids?`, `timeout_secs?` (default 600) | Returns when one of the caller's agents has a new result, or a message for the caller arrives. |
| `agent_list` | none | Id, definition, state, activity, time, tokens and cost of every agent. |
| `worktree_apply` | `agent` | Applies an isolated agent's changes to the main tree. |
| `worktree_discard` | `agent` | Removes an isolated agent's worktree and branch. |

The lead has all of them. A sub-agent always has `agent_send`, restricted to `to: "parent"`. It gets `task`, `agent_stop`, `agent_wait` and `agent_list` only when its definition lists `spawns` and it is above the depth limit, and may then start only the listed definitions. `worktree_apply` and `worktree_discard` are the lead's alone.

### Reading an agent's work

The `read` tool accepts two URLs:

| URL | Content |
|---|---|
| `agent://<id>` | The agent's latest report, in full |
| `agent://<id>/transcript` | A condensed transcript: prompts, everything the agent said, one line per tool call |

## Worktree isolation

With `isolation: worktree` (in the definition, or per agent in a `task` call) the agent works in its own git worktree instead of the shared tree.

1. Azazel runs `git worktree add <project>/.azazel/worktrees/<id> -b azazel/<session>/<id> HEAD`. The project must be a git repository with at least one commit; otherwise the spawn fails with `isolation: worktree requires a git repository`.
2. The agent's working directory is the worktree. If the main tree had uncommitted changes, its report starts with `worktree started from HEAD; uncommitted changes were not visible`.
3. When the agent finishes, its changes are saved as `<id>.patch` in the project's artifacts directory and the report ends with the diffstat and the patch path. A worktree without changes is removed at once.
4. `worktree_apply` applies the patch to the main tree with `git apply --3way` and removes the worktree and branch. The changes arrive as ordinary uncommitted modifications; nothing is committed. On a conflict the conflict markers are left in the files, the conflicting paths are returned, and the worktree is kept.
5. `worktree_discard` removes the worktree and branch without applying anything.

Worktrees still present when the session ends are removed. Add `.azazel/worktrees/` to your `.gitignore`.

## Controlling agents from the TUI

While sub-agents are queued, working or waiting for you, the **roster** shows one line each above the editor:

```text
● scout-1  grep parse_config  12s  3.4k
● scout-2  waiting for your approval  1m05s  1.1k
```

| Control | Effect |
|---|---|
| `Alt+A` | Opens the agent hub: every agent with its definition, state, time and tokens |
| `Enter` in the hub | Follows that agent: what it says and does appears in the transcript marked `[<id>]`. Choose `lead` to stop following. |
| `m` in the hub | Message the selected agent |
| `s` / `S` in the hub | Stop the selected agent / stop all sub-agents |
| `/agents` | Lists the session's agents, every definition with its source, and invalid definition files |
| `/msg <id\|all> <text>` | Sends a message as you |
| `/stop <id\|all>` | Stops an agent and the agents it started |
| `Esc` | Aborts the lead only. Agents a waiting `task` call started stop with it; agents started with `wait: false` keep running. |

`/playtest` starts a `playtester` directly; see [screen and playtesting](screen-and-playtesting.md). In print mode's JSON output and in [RPC mode](rpc.md) every event carries the id of the agent it came from.

## Limits

```toml
[team]
max_parallel = 4   # sub-agents working at the same time
max_depth = 2      # levels of agents below the lead
```

With `max_depth = 2`, an agent started by the lead may start agents of its own (if its definition lists `spawns`), and those may not start any. An agent revived by a message runs even when every slot is taken.

A `subagent_start` [hook](hooks.md) can refuse a spawn, and [Luau extensions](extensions.md) can start, message and stop agents through `azazel.agent`.
