# The team

An azazel session is a team, not one chatbot. The agent you talk to is the **lead**. It can hand work to **sub-agents**: specialists that run in parallel, each with a fresh context, its own model chain, its own tool set and its own transcript. You can watch, message or stop any of them while they work. Everything runs inside the one `azazel` process.

## How it works

- The lead (id `lead`) starts sub-agents with the `task` tool. Each one is created from an **agent definition** and sees only the prompt written for it.
- A sub-agent's last message is its **report**. That report is all its parent sees of the work. An agent asked for a [typed result](#typed-results) reports a JSON value instead.
- Ids are `<definition>-<n>` (`scout-1`, `scout-2`, …) unless the caller gives a `name`.
- At most `team.max_parallel` sub-agents run at once; the rest are `queued`.
- States: `queued`, `running`, `waiting for approval`, `done`, `failed`, `stopped`.
- Transcripts are kept beside the session: `~/.azazel/projects/<slug>/sessions/<session id>/agents/<agent id>.jsonl`.
- Sub-agents follow the session's [permission mode](permissions.md), also as it changes: switch the mode and an agent in the middle of its work is judged by the new one from its next tool call. Their approval prompts appear in the same place as the lead's, titled with the agent id.
- Two agents may work in the same tree. `write` and `edit` refuse a file the agent has not read since it last changed, and an agent whose file was changed by another is told `<path> was modified by <agent id>; read it again before editing` before its next step.

## Built-in agents

| Name | Model role | Tools | Turn limit | Purpose |
|---|---|---|---|---|
| `scout` | `@scout` → `@smol` | `read`, `glob`, `grep`, `semantic_search`, `web_search`, `web_fetch`, `web_grep` | 40 | Fast read-only exploration: where something lives, how it works |
| `planner` | `@planner` → `@plan` | scout's tools, plus `write` and `edit` limited to `.azazel/plans/**` | 80 | Writes an implementation plan file before code is touched |
| `coder` | `@coder` → `@default` | all; skill `lean`; may spawn `scout` | 150 | Implements a scoped change and proves it by building and testing |
| `tester` | `@tester` → `@default` | all | 120 | Writes and runs tests, reports precise failures |
| `reviewer` | `@reviewer` → `@slow` | `read`, `glob`, `grep`, `shell` limited to `git diff`, `git log`, `git show` | 80 | Independent review of a diff |
| `verifier` | `@verifier` → `@verify` | `read`, `glob`, `grep`, `shell`; `write` and `edit` disallowed | 80 | Checks a claim of completion by running commands (see [goals](goals.md)) |
| `researcher` | `@researcher` → `@default` | `web_search`, `web_fetch`, `web_grep`, `browser`, `read` | 80 | Research outside the repository, with sources |
| `designer` | `@designer` → `@default` | all; skill `frontend-design`; may spawn `scout` | 150 | UI work checked in a real browser |
| `artist` | `@artist` → `@default` | `generate_image`, `generate_video`, `generate_3d`, `generate_audio`, `sprite_sheet`, `read`, `write`, `shell` | 100 | Produces image, video, 3D and audio assets |
| `playtester` | `@playtester` → `@vision` | `shell`, `job`, `screen`, `browser`, `read` | 80 | Runs and plays a build, reports defects with frames |

Every built-in agent has a role of its own, named after it, which falls back to the general role shown after the arrow until you set it. The lead uses the role `lead`, which falls back to `default`. Give any of them a model in `/settings` → Models, or in `config.toml`:

```toml
[roles]
lead = ["anthropic/<model-id>"]
coder = ["openai/<model-id>", "@default"]
scout = ["ollama/<local-model>"]
```

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
| `mode` | `subagent` (the team starts it with `task`), `primary` (a persona of the lead, see [primary agents](#primary-agents)) or `all` (both) | `subagent` |
| `permission-mode` | `inherit` or `plan` (read-only whatever the session mode is). A definition can only narrow the session's mode, never widen it | `inherit` |
| `color` | `#rrggbb`, or `red`, `orange`, `yellow`, `green`, `cyan`, `blue`, `purple`, `magenta`, `pink`, `gray`: the colour the UI shows a primary agent in | none |
| `max-turns` | Model requests per prompt before the agent is told to give its final report | `[team] max_turns` (200) |
| `skills` | [Skills](skills-and-commands.md) whose text is appended to the system prompt | none |
| `spawns` | Definitions this agent may itself start | none |
| `isolation` | `none` or `worktree` | `none` |
| `output-schema` | A JSON Schema (a YAML mapping, or JSON in a string) the agent's result must match; see [typed results](#typed-results) | none |

`disallowedTools`, `maxTurns`, `permissionMode` and `outputSchema` are accepted as aliases. Other keys are ignored. A definition written before primary agents with `mode: plan` is refused with a message: that is `permission-mode: plan` now.

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

## Primary agents

The lead is an agent too, and its persona is a definition: a **primary agent**. Three are built in, and `tab` in an empty input switches between them (`shift+tab` stays the [permission mode](permissions.md)):

| Name | What the lead is |
|---|---|
| `build` | The default: the lead as it always worked, with every tool |
| `plan` | Investigates read-only and writes a plan under `.azazel/plans/`; it changes nothing else, and the agents it starts are held to the same limit. It is plan mode as a persona (`permission-mode: plan`): `plan_submit` asks you to approve the plan, and approving it switches the lead to `build`, which carries it out |
| `lean` | The lead as a lean engineer, with the `lean` skill: it takes the smallest complete change, pushes back on scope the need does not justify and asks before building optional extras. It never cuts validation at trust boundaries, data-loss handling, security or accessibility |

Any definition with `mode: primary` (or `all`) is one more. While the lead is a primary agent:

- the definition's body replaces the lead's identity at the top of the system prompt; the rules, the environment, the context files and the rest stay;
- its `tools` and `disallowed-tools` decide what the lead is offered, and `tool(pattern)` grants restrict the lead's calls as they restrict a sub-agent's;
- `permission-mode: plan` keeps the lead (and every agent it starts) in plan mode;
- its `model` and `thinking` replace the lead's while it is selected, and the lead goes back to its own when you switch to an agent without them (`--model` and `--thinking` on the command line win);
- its `skills` are appended to its prompt, and its `color` paints the status line's `agent` segment and the rules around the input.

```markdown
---
name: docs
description: Writes and edits documentation only.
mode: primary
tools: read, glob, grep, web_search, web_fetch, write(docs/**), edit(docs/**)
color: green
---
You are azazel, and you write documentation. Change files under docs/ only.
```

Switch with `tab`, with `/agent` (a picker) or `/agent <name>`; start a run as one with `--agent <name>`; set the agent sessions start as with `[agents] default` (`build` unless set). A client of [RPC](rpc.md) switches with `set_agent`, an [ACP](acp.md) client with the session option `agent`. The switch is written to the session, and resuming it comes back on the same agent. A primary agent is never started by `task`; one with `mode: all` is both.

```toml
[agents]
default = "plan"
```

## Orchestration tools

| Tool | Input | Behaviour |
|---|---|---|
| `task` | `agents: [{agent, prompt, name?, model?, isolation?, schema?}]`, `wait?` (default `true`) | Starts every listed agent; they run in parallel. With `wait` it returns all reports (each cut at 8000 characters, with a pointer to `agent://<id>`). With `wait: false` it returns the ids at once and each result arrives later as an `<agent-result>` message. If one agent cannot be started, none are. Cancelling a waiting call stops the agents it started. When you type while it waits, the wait ends at once: the result names the agents still running, which go on in the background and report later as messages. |
| `agent_send` | `to` (an id, `parent`, or `all`), `message`, `ask?` (default `false`) | Returns at once with how the message arrived, one line per agent: `delivered at its next step` (it is working), `woke it` (it was finished, and is revived with the message, keeping its context) or `queued` (it is still waiting to start; the message goes with its prompt). With `ask: true` the message is a question answered on the side: see [Asking an agent](#asking-an-agent). |
| `agent_stop` | `id`, `reason?` | Stops the agent and every agent it started. |
| `agent_wait` | `ids?`, `timeout_secs?` (default 600) | Returns when one of the caller's agents has a new result, or a message for the caller arrives, or the user types something for the caller. The agents keep running. |
| `agent_list` | none | Id, definition, state, activity, time, tokens and cost of every agent. |
| `agent_status` | `id` (an id, `lead`, or `all`) | What the agent is doing, at once and without asking a model: state and time, the tool it runs now and for how long, the last lines it wrote, the files its `write`, `edit` and `patch` calls changed, for an isolated agent its worktree's `git status` and the commits since it started, tokens, cost, and what its side questions cost. `all` gives one line per agent. |
| `worktree_apply` | `agent` | Applies an isolated agent's changes to the main tree. |
| `worktree_discard` | `agent` | Removes an isolated agent's worktree and branch. |

The lead has all of them. A sub-agent always has `agent_send`, restricted to `to: "parent"`. It gets `task`, `agent_stop`, `agent_wait`, `agent_list` and `agent_status` only when its definition lists `spawns` and it is above the depth limit, and may then start only the listed definitions. `worktree_apply` and `worktree_discard` are the lead's alone.

### Reading an agent's work

The `read` tool accepts these URLs:

| URL | Content |
|---|---|
| `agent://<id>` | The agent's latest report, in full; for a [typed result](#typed-results), the JSON value alone |
| `agent://<id>/transcript` | A condensed transcript: prompts, everything the agent said, one line per tool call |
| `agent://<id>/status` | What `agent_status` returns; `agent://all/status` is the table of every agent |

### Asking an agent

`agent_send` with `ask: true` puts a question to one agent without interrupting it, like `/btw` for the lead.
The question goes, with the agent's conversation as it is at that moment, as one separate request to the
agent's own model, framed with its own prompt and tools so its prompt cache is reused. The agent keeps working,
nothing is written to its transcript, and the answer is the result of the `agent_send` call, usually within
seconds. A finished agent answers from its final transcript; one that has not started yet has nothing to answer
from, and the call says so. The request may take 120 seconds; when it takes longer, or the model fails, the call
fails with the reason. What it costs is counted to the agent that asked (`agent_status` shows it) and under
`btw` in `/cost`.

A Luau `eval` script starts agents too: `agents.run(agent, prompt, { model?, isolation?, schema? })` is a
`task` call for one agent that returns its report (or typed value) to the script, and `parallel` runs several
at once. The script keeps what the agents return in variables, so only what it prints or returns reaches the
conversation. See [code execution](tools.md#code-execution).

## Typed results

An agent normally reports in prose. When the caller needs data, it gives the agent a JSON Schema and gets back
a value that matches it.

```json
{ "agents": [{
    "agent": "scout",
    "prompt": "List every file under src/ that calls parse_config.",
    "schema": {
      "type": "object",
      "properties": { "files": { "type": "array", "items": { "type": "string" } } },
      "required": ["files"]
    }
}] }
```

- The schema comes from the `schema` of the `task` entry, or from `output-schema` in the agent's definition.
  The caller's schema wins.
- Such an agent gets a `report` tool that no other agent sees, whatever its definition grants, and is told to
  finish by calling it. `report` takes `result` and checks it against the schema. A value that does not match
  is refused with the list of violations, so the agent can correct it and call again. The last value that
  matched is the result.
- The agent's report is that value as JSON. The `task` result shows it in a fenced `json` block, and
  `agent://<id>` returns the JSON alone, so it parses.
- An agent that stops without a report is reminded, up to three times. If it still has not reported, it
  fails with `ended without a report`.
- A schema that is not valid JSON Schema fails the `task` call before any agent starts. References inside
  the schema (`#/$defs/...`) work. A schema may not refer to a file or a URL: nothing is fetched for a schema.

Print mode uses the same mechanism for the lead:

```sh
azazel -p --json-schema schema.json "which crates depend on serde?"
azazel -p --json-schema '{"type":"object","properties":{"crates":{"type":"array"}},"required":["crates"]}' "..."
```

`--json-schema` takes a file or the schema itself. The lead gets the `report` tool; standard output is exactly
the reported JSON on one line, and the exit status is `0`. A run that ends without a matching report prints
nothing on standard output and exits `1`. A schema that cannot be read or is not a JSON Schema exits `2`. With
`--output json` the events are printed as usual and only the exit status changes.

## Coordinator mode

In coordinator mode the lead plans and delegates and does none of the work itself. It is offered the
orchestration tools, `read`, `glob`, `grep`, `todo`, `ask_user`, `session_recall`, `skill`, the memory tools,
`semantic_search`,
`web_search`, `web_fetch` and `web_grep`, and nothing that edits files or runs commands: no `write`, `edit`, `shell`,
`job`, `pty`, `eval`, no MCP tools. Scripts run for the lead are held to the same list. Its prompt tells it to
plan first, delegate independent pieces in one `task` call, read every report critically, and verify through
agents. Sub-agents are not affected.

| Way | Effect |
|---|---|
| `/coordinator on`, `/coordinator off` | Switches the mode for the open session; `/coordinator` shows it |
| `--coordinator` | Starts the run in coordinator mode |
| `[team] coordinator = true` | Makes it the default for new sessions |

The status line shows `coordinator` while the mode is on. The mode is recorded in the session, so resuming a
session brings it back, whatever the default is. It is off by default: it changes how every prompt is worked.

## Advisor

The advisor is a second model that looks over the lead's work. It is off by default, because every review is
one more request.

```toml
[advisor]
model = ["openai/<model-id>"]   # a chain: the first that answers
when = "stop"                   # "off" (default) or "stop"
```

With `when = "stop"`, each time the lead is about to finish a prompt the advisor is sent what you asked in
that run, the lead's last message and the diff since the prompt's [checkpoint](sessions-and-checkpoints.md)
(cut at 30 KB). It answers `OK`, or it gives advice.

- `OK`: the lead stops as it would have.
- Advice: the lead gets it as `<advisor>...</advisor>`, with a line saying a second model reviewed the work,
  and takes one more round. That round is not reviewed again.
- No answer within 120 seconds, or an error: the lead stops without advice and a warning is logged.

The advisor has no tools and no part in the conversation: it sees only that brief, and its answer is not
stored in the session except as the advice the lead received. A [goal](goals.md)'s own check still runs after
the extra round and has the last word. Only the lead is reviewed, not sub-agents.

`/advise [question]` asks the advisor on demand, about the work so far or about your question. Only you see
the answer; the lead does not. It works with `when = "off"` too, as long as a model is set.

What the advisor costs is counted in the run's spend (so `--max-cost` covers it) and listed as `advisor` by
`/cost`. `[advisor]` is a [gated setting](configuration.md#trust): a project you have not trusted cannot set it.

## Model routing

With `[team] model_routing = "auto"`, the `smol` role reads the prompt of each sub-agent before it starts and rates it as JSON: `tier` (`fast`, `balanced` or `deep`), `confidence` and `risky`, each 0 to 1. The tier describes the shape of the work, not its size: `fast` is mechanical, local and well specified; `balanced` is ordinary engineering; `deep` means an unknown cause, an open design or an action that cannot be undone. The task is cut to about 12,000 characters and sent as data, in one request with no cache marks, and the agent's first request waits for it for at most 8 seconds.

Each tier has a model: `routing_fast` (`@smol`), `routing_balanced` (`@default`) and `routing_deep` (`@slow`). The rating moves the agent only under these rules:

- Down to a cheaper model needs `min_downgrade_confidence` (0.6); up to a stronger one needs `min_upgrade_confidence` (0.85).
- `deep` is used only when `allow_deep = true`; otherwise the most routing picks is `routing_balanced`.
- `risky` above 0.7 keeps the agent at `balanced` or above.
- A missing or invalid answer, or one that takes longer than 8 seconds, changes nothing.

Routing never changes the lead, an agent whose `task` call names a `model`, or an agent whose definition's model was set by you (a `model` in its file or `[agents.<name>]`, other than its own role or `@default`).

`"suggest"` rates every task but changes nothing: the line it would have written appears under the agent in `/agents` and in its status. Both modes append a `routing` entry to the session with the tier, confidence, the models and the reason. When the agent ends, its real tokens are priced on both models from the model catalogue, and `/cost` shows the saving of moved agents (and, for `suggest`, what it would have been). The rating itself is counted under `routing` in the side-request costs. With no price known for a model there is no saving figure.

## Worktree isolation

With `isolation: worktree` (in the definition, or per agent in a `task` call) the agent works in its own git worktree instead of the shared tree.

1. Azazel runs `git worktree add <project>/.azazel/worktrees/<id> -b azazel/<session>/<id> HEAD`. The project must be a git repository with at least one commit; otherwise the spawn fails with `isolation: worktree requires a git repository`.
2. The agent's working directory is the worktree. If the main tree had uncommitted changes, its report starts with `worktree started from HEAD; uncommitted changes were not visible`.
3. When the agent finishes, its changes are saved as `<id>.patch` in the project's artifacts directory and the report ends with the diffstat and the patch path. A worktree without changes is removed at once.
4. `worktree_apply` applies the patch to the main tree with `git apply --3way` and removes the worktree and branch. The changes arrive as ordinary uncommitted modifications; nothing is committed. On a conflict the conflict markers are left in the files, the conflicting paths are returned, and the worktree is kept.
5. `worktree_discard` removes the worktree and branch without applying anything.

### Build output of worktrees

A worktree has no `target/` yet, so every agent's first build would start from scratch, and several agents would compile the same dependencies at once. With `[team] worktree_target = "shared"` (the default) the processes of an agent in a worktree of a Cargo project get `CARGO_TARGET_DIR` set to the main checkout's `target/`, unless your environment already sets it. Cargo takes a lock on the target directory, so builds that share one wait for each other instead of corrupting it; fewer compiles run at the same time, which is also the point. The cost is that two agents building different code take turns, and each may rebuild what the other's changes touched.

- `own`: each worktree builds into its own `target/`.
- `sccache`: each worktree builds into its own `target/` with `RUSTC_WRAPPER=sccache`, when `sccache` is installed (azazel does not install it) and `RUSTC_WRAPPER` is not set.

`cargo clean` and `cargo xtask prune` in the main checkout act on the shared directory.

Worktrees still present when the session ends are removed. Add `.azazel/worktrees/` to your `.gitignore`.

## Controlling agents from the TUI

While sub-agents are queued, working or waiting for you, the **roster** stands above the editor: a header that
counts them, with the key of the agent hub at its right end, and one line for each of the first three (id,
definition, what it is doing, time and tokens); `… N more` counts the others. The status line counts them too
(`⇶ 2 agents`), in every preset.

```text
Agents · 2 running · 1 queued                                      alt+a hub
● scout-1  scout  grep parse_config  12s  3.4k
● scout-2  scout  waiting for your approval  1m05s  1.1k
● coder-1  coder  queued  0s  0
```

| Control | Effect |
|---|---|
| `Down` in an empty input | Puts the keyboard in the roster: `Up`/`Down` move, `Enter` opens the selected agent's work in a popup you can type to it in, `x` stops it, `Esc` returns. See [the fleet view](tui.md#the-fleet-view) |
| `@<id> <text>` | Sends the text to that agent, as `/msg` does; `@<id>` alone opens its popup |
| `@<definition> <text>` | Starts an agent from that definition with the text as its task, as `/task` does |
| `Alt+A`, or `Left` twice in an empty input | Opens the agent hub: every sub-agent of the session, those at work first, then finished ones newest first, with its state, definition, parent, time, tokens, cost and what it is doing or was asked. Finished agents older than `team.agent_retention_days` (14) leave the list. See [the agent hub](tui.md#the-agent-hub) |
| `Enter` in the hub | Opens the agent's chat: its whole transcript, followed live, with a line to type to it. `Esc` or `Left` twice returns to the hub |
| `x` / `S` in the hub | Stop the selected agent / stop all sub-agents |
| `m` in the hub | Message the selected agent |
| `f` in the hub | Follows that agent: what it says and does appears in the transcript marked `[<id>]`; `f` on it again stops following |
| `/agents` | Lists the session's agents, every definition with its source, and invalid definition files |
| `/msg <id\|all> <text>` | Sends a message as you |
| `/stop <id\|all>` | Stops an agent and the agents it started |
| `/task <agent> [name=<id>] <prompt>` | Starts an agent from any definition yourself; `/task` alone lists the definitions |
| `/review [ref]`, `/security-review [ref]` | Starts a `reviewer` on the uncommitted changes, or on the commits since `ref` |
| `Esc` | Aborts the lead only. Agents a waiting `task` call started stop with it; agents started with `wait: false` keep running. |

### Starting agents yourself

The lead decides when to delegate, but you can start an agent without asking it:

```text
❯ @scout where is the retry policy configured?
❯ /task scout where is the retry policy configured?
❯ /task coder name=fixer make the flaky upload test deterministic
❯ /review
❯ /review main
❯ /security-review
```

- `/task <agent> [name=<id>] <prompt>` starts the named definition with your prompt. The agent appears in the
  roster like any other, and its report is sent to the lead when it finishes, so the lead can act on it.
- `@<agent> <prompt>` at the start of a prompt does the same without the command; `@` there completes the
  definitions. When a running agent has that id, the text is a message to it instead.
- `/review` starts the `reviewer` on the uncommitted changes: it is told to run `git diff HEAD` and is given the
  names of new, untracked files to read. `/review <ref>` reviews the commits since a branch, tag or commit with
  `git diff <ref>...HEAD`. The reviewer reports findings with `file:line`, a severity and a fix, and may not edit
  anything. With nothing to review, no agent is started.
- `/security-review [ref]` starts the same reviewer with the built-in `security-review` skill added to its task,
  and limits the review to security.

`/coordinator` and `/advise` are described under [coordinator mode](#coordinator-mode) and
[advisor](#advisor). `/playtest` starts a `playtester` directly; see [screen and playtesting](screen-and-playtesting.md). In print mode's JSON output and in [RPC mode](rpc.md) every event carries the id of the agent it came from.

## Limits

```toml
[team]
max_parallel = 4   # sub-agents working at the same time
max_depth = 2      # levels of agents below the lead
max_turns = 200    # requests per task for an agent whose definition has no max-turns; 0 is no limit
coordinator = false  # true: the lead only plans and delegates (coordinator mode)
worktree_target = "shared"  # shared | own | sccache: where agents in worktrees build
model_routing = "off"  # off | suggest | auto: rate each task and pick the sub-agent's model; see Model routing
```

An agent with a turn limit (`max-turns`, `[team] max_turns`, or `--max-turns` for the lead) is told in its last two turns how many are left: `2 turns left: finish the task and report what is done and what is not`, then `1 turn left`. After the last turn it is asked for its final report without tools.

With `max_depth = 2`, an agent started by the lead may start agents of its own (if its definition lists `spawns`), and those may not start any. An agent revived by a message runs even when every slot is taken.

A `subagent_start` [hook](hooks.md) can refuse a spawn, and [Luau extensions](extensions.md) can start, message and stop agents through `azazel.agent`.
