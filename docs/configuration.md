# Configuration

Azazel is configured with TOML files, environment variables and command-line flags. This page lists where the
files live, how they are merged, every table and key with its default, which project settings are held back until
you trust the project, the command-line flags and the subcommands.

Every key below can also be read and changed from inside the UI: [`/settings`](tui.md#settings) shows each one
with its value, its default and the layer that sets it.

## Files and load order

Later layers win:

| Order | Source | Notes |
|---|---|---|
| 1 | Built-in defaults | The values in the tables below |
| 2 | `~/.azazel/models.yml` | Providers and their models only; see [models.yml](models-and-logins.md#modelsyml) |
| 3 | `~/.azazel/config.toml` | Your settings for every project |
| 4 | `<project>/.azazel/models.yml` | Providers and models for this project; read only when the project is trusted |
| 5 | `<project>/.azazel/config.toml` | Shared project settings, meant to be committed |
| 6 | `<project>/.azazel/config.local.toml` | Your settings for this project; `Always allow` rules are saved here |
| 7 | `AZAZEL_<TABLE>__<KEY>` environment variables | See below |
| 8 | Command-line flags | `--mode` sets `permissions.mode` |

The home directory is `~/.azazel`, or the value of `AZAZEL_HOME`. The project root is the nearest ancestor of the
working directory that contains `.azazel/` or `.git`; without one, it is the working directory.

Merge rules:

- Tables merge key by key.
- `permissions.allow`, `permissions.ask`, `permissions.deny` and every `hooks.<event>` array concatenate across layers.
- Every other value, including other arrays, replaces the one below it.
- An unknown table or key is a load error that names the file. Nothing is ignored silently.
- A `models.yml` contributes `[providers]` and `[models]` and merges like any other layer; a mistake in it is
  a load error that names the file and the key (`providers.<id>.models[0].cost`).

`azazel doctor` shows which files were loaded.

## References to the environment and to files

Any string value of `config.toml`, `models.yml` and `mcp.json` may hold `{env:VAR}` and `{file:path}`. They are
filled once, when the file is read:

```toml
[providers.gateway]
api_key = "{file:~/.secrets/gateway.key}"
base_url = "https://{env:GATEWAY_HOST}/v1"
```

- `{env:VAR}` is the value of the environment variable; an empty value counts as set.
- `{file:path}` is the content of the file, without the line breaks at its end. A relative path is relative to the
  directory of the file that names it, and `~/` is your home directory.
- A variable that is not set, or a file that cannot be read, stops azazel with the file and the key named
  (`providers.gateway.api_key: {env:GATEWAY_KEY} is not set`); it never becomes an empty value. What is filled in is
  not logged or shown in errors.
- What is filled in is not read again: a file that holds `{env:X}` gives that text.
- The file keeps the reference: `/settings` changes other keys around it.
- A project's files read no variable or file until the project is trusted: such a value is left out and listed
  like the other [held-back settings](#trust).
- `${VAR}` and `${VAR:-default}` keep working where they did: in a provider's `base_url` and `headers` and in
  `mcp.json` entries.

## Environment overrides

`AZAZEL_<TABLE>__<KEY>` sets one key; a double underscore separates nesting levels and names are lower-cased.
The value is read as a TOML literal when it is one (`true`, `8`, `["a", "b"]`) and as a string otherwise.

```sh
AZAZEL_PERMISSIONS__MODE=auto azazel -p "run the tests"
AZAZEL_TEAM__MAX_PARALLEL=8 azazel
AZAZEL_TOOLS__DISABLED='["web_search", "web_fetch"]' azazel
AZAZEL_MEDIA__MODELS__IMAGE="<backend>:<model-id>" azazel
```

Other `AZAZEL_` variables are not config overrides: `AZAZEL_HOME` (home directory), `AZAZEL_BROWSER` (browser
binary), `AZAZEL_HOOKS_DISABLED` (load no hooks), `AZAZEL_NO_UPDATE_CHECK` (skip the update check),
`AZAZEL_GLYPHS` (`nerd`, `unicode` or `ascii`, over `tui.glyphs`).

## Models

```toml
[roles]
default = ["<provider>/<model-id>", "ollama/<model-id>"]   # an ordered fallback chain
smol = ["ollama/<model-id>", "@default"]
slow = ["<provider>/<model-id>:high", "@default"]         # the role thinks at high

[providers.my-server]
api = "openai-chat"
base_url = "http://localhost:8000/v1"
api_key_env = "MY_SERVER_KEY"

[models."my-server/<model-id>"]
context_window = 200000
max_output = 32000

[agents.scout]
model = ["@smol"]
thinking = "low"
```

| Table | Key | Default | Meaning |
|---|---|---|---|
| `[roles]` | `default` | unset | The lead agent's chain. Entries are `provider/model[:thinking]` or `@role`. A `:thinking` level is the role's own: its requests ask for it unless a level was chosen for the session (`--thinking`, `/thinking`, an agent's `thinking`); see [thinking levels](models-and-logins.md#thinking-levels). `azazel roles` lists and changes roles from the shell |
| | `smol`, `slow`, `verify`, `vision` | `["@default"]` | Chains for cheap work, hard work, the checker and image input |
| | `plan` | `["@slow"]` | Chain for planning |
| | `memory` | `["@smol"]` | Chain for memory extraction |
| | `judge` | `["@smol"]` | Chain for `classify` in scripts |
| | `lead`, `coder`, `tester`, `researcher`, `designer`, `artist` | `["@default"]` | The lead's chain, and the chain of the built-in agent of that name, so each can have its own model; see [roles](models-and-logins.md#roles) |
| | `scout` | `["@smol"]` | The `scout` agent's chain |
| | `planner` | `["@plan"]` | The `planner` agent's chain |
| | `reviewer` | `["@slow"]` | The `reviewer` agent's chain |
| | `verifier` | `["@verify"]` | The `verifier` agent's chain |
| | `playtester` | `["@vision"]` | The `playtester` agent's chain |
| `[providers.<id>]` | `name` | preset, else the id | What the provider is called in menus |
| | `api` | preset | A [wire API](models-and-logins.md#wire-apis): `anthropic-messages`, `openai-responses`, `openai-chat`, `openrouter`, `google-generative`, `google-gemini-cli`, `ollama-chat`, `azure-openai-responses`, `azure-openai-chat` |
| | `base_url` | preset | Endpoint; may contain `${VAR}` and `${VAR:-default}`. A new provider id needs both `api` and `base_url` |
| | `api_key_env` | preset | Environment variable holding the key |
| | `api_key_command` | unset | Shell command whose output is the key |
| | `api_key` | unset | The key itself |
| | `headers` | preset | Extra headers on every request; values may contain `${VAR}`, and `{session_id}` becomes the id of the conversation |
| | `auth` | `auto` | `auto` (what the wire API prescribes), `bearer`, or `api-key` for an `api-key:` header (Azure resource keys) |
| | `discovery` | preset, else `auto` | How models are listed: `auto`, `ollama`, `lm-studio` or `off` |
| | `extra_body` | `{}` | Members written into every request body (routing preferences, vendor switches) |
| | `tls` | unset | `{ cert, key, ca }`: a client certificate and the server's authority, as PEM paths or PEM; see [client certificates](models-and-logins.md#client-certificates) |
| | `timeout_secs` | unset | Longest a whole request to this provider may take; `0` or unset has no limit. See [stalls](models-and-logins.md#stalls) |
| | `first_event_secs`, `stall_secs` | `[retry]` | This provider's own `retry.first_event_secs` and `retry.stall_secs` |
| | `region`, `profile` | unset | AWS settings, for `bedrock` |
| | `project`, `location` | unset | Google Cloud settings, for `vertex` |
| | `oauth`, `oauth_headers` | preset | A sign-in instead of a key; see [models and logins](models-and-logins.md) |
| | `account` | the default account | Which stored account of the provider to use; see [several accounts](models-and-logins.md#several-accounts) |
| `[profiles]` | `on_limit` | `""` | The profile to switch to when a usage limit is reached; empty never switches. Must be a built-in profile or have a `[profiles.<name>.roles]` table; see [profiles](models-and-logins.md#profiles) |
| | `on_reset` | `"ask"` | When the limit has reset: `ask`, `auto` or `off` |
| | `disabled` | `[]` | Built-in profiles to hide from `/profile`, `--profile`, the web page and `on_limit`; the Profiles view of `/models` turns them on and off |
| `[profiles.<name>.roles]` | `<role>` | none | A set of role chains that `/profile <name>` and `--profile <name>` apply; a name of a built-in profile replaces it |
| `[presets.<name>.roles]` | `<role>` | none | The old name of `[profiles.<name>.roles]`; `[profiles]` wins when both have the name |
| `[models."<provider>/<id>"]` | `name` | catalogue | Display name |
| | `context_window`, `max_output` | catalogue | Token limits; unknown models get 128000 and 8192 |
| | `vision`, `reasoning` | catalogue | Capabilities; unknown models get `false` |
| | `thinking_levels` | every level | The thinking levels the model accepts, for example `["low", "medium", "high"]` |
| | `cost` | catalogue | `{ input, output, cache_read, cache_write }` in USD per million tokens |
| `[agents]` | `default` | `"build"` | The [primary agent](team.md#primary-agents) the lead is when a session starts; one that is no primary agent stops azazel with a message |
| `[agents.<name>]` | `model` | definition | Chain (a list) replacing the agent definition's; see [team](team.md) |
| | `thinking` | definition | Thinking level replacing the definition's |
| `[retry]` | `max_attempts` | `3` | Tries per model before moving down the chain |
| | `cooldown_secs` | `300` | How long a failed model is skipped |
| | `first_event_secs` | `20` | Longest wait, in seconds, for the first sign of a response after the request is sent; `0` waits forever. See [stalls](models-and-logins.md#stalls) |
| | `stall_secs` | `120` | Longest a response that has begun may send nothing; `0` waits forever |
| | `max_stalls` | `3` | Stalls of one model within a request before moving down the chain; `0` asks the stalled model again for as long as it stalls |

## Behaviour

| Table | Key | Default | Meaning |
|---|---|---|---|
| `[permissions]` | `mode` | `"manual"` | `plan`, `manual`, `accept-edits`, `auto` or `bypass`; see [permissions](permissions.md#modes). `ask`, the name `manual` had before 0.5, is refused with a message that says so. A project may set `bypass` only once it is trusted |
| | `allow`, `ask`, `deny` | `[]` | Rules such as `"shell(cargo build:*)"` |
| | `additional_dirs` | `[]` | Directories outside the folder azazel was started in that agents may work in (relative to the project root) |
| | `destructive` | `"ask"` | What a command that destroys work (`git reset --hard`, `rm -r ~`, `curl … \| sh`) needs in every mode but `bypass`: `ask`, `allow` or `deny`; see [permissions](permissions.md#destructive-commands) |
| | `secret_paths` | `[]` | Patterns, written like the path of a rule, added to the built-in list of [secret files](permissions.md#secret-files): reading or writing one is asked about in every mode but `bypass` |
| `[checkpoints]` | `enabled` | `true` | Snapshot the project before each prompt; see [sessions and checkpoints](sessions-and-checkpoints.md) |
| | `max_files` | `100000` | Checkpoints turn off for projects with more files than this |
| | `timeout_ms` | `10000` | Longest one snapshot may take (1000 to 300000); past it checkpoints turn off for the session |
| | `exclude` | `[".azazel/worktrees/"]` | Patterns, as in `.gitignore`, that checkpoints leave out |
| `[session]` | `auto_name` | `true` | Have the `smol` role name a session after its first reply; see [sessions](sessions-and-checkpoints.md#names) |
| | `resume` | `"new"` | What the interactive UI opens when no session is named: `new`, or `last` for the most recent session started in this folder, as `-c` does (`--new` starts a new one anyway). Print mode starts a new session unless `-c` or `--session` is given; see [sessions](sessions-and-checkpoints.md#resuming) |
| | `restore_model` | `true` | A reopened session comes back on the model and thinking level it last used; see [sessions](sessions-and-checkpoints.md#model-and-thinking-level) |
| | `retention_days` | `0` | Delete unpinned sessions not used for this many days when azazel starts; `0` keeps everything |
| | `resume_runs` | `true` | A background session that is started again after its process died in the middle of a run goes on with the run by itself; with `false` it waits for `/continue`. See [interrupted runs](sessions-and-checkpoints.md#interrupted-runs) |
| | `branch_summary` | `true` | Going back with `/tree` has the `smol` role tell the agent in a few lines what happened on the branch you left; see [sessions](sessions-and-checkpoints.md#branch-summaries) |
| | `handoff_at` | `0` | Past this many context tokens, offer `/handoff` once per session (a notice in the terminal UI, a toast in the web page); `0` never offers. It makes no request; see [handoff](sessions-and-checkpoints.md#handoff) |
| `[compaction]` | `enabled` | `true` | Automatic context compaction |
| | `reserve_tokens` | `16384` | Summarise when the context is within this many tokens of the window |
| | `auto_percent` | `0` | Summarise once the context fills this percentage of the window (`70` summarises at 70%); `0` leaves it to `reserve_tokens`. Whichever comes first applies, and on a subscription login the percentage is of `login_max_tokens` |
| | `keep_recent_tokens` | `20000` | Roughly how much recent context is kept verbatim |
| | `keep_tool_results` | `8` | Newest tool results that are never elided |
| | `micro_threshold` | `0.6` | Fraction of the window at which old tool results are elided |
| | `micro_min_savings` | `0.1` | Old tool results are elided only once they add up to this fraction of the window, since each elision makes the provider cache the whole conversation again |
| | `login_max_tokens` | `300000` | On a subscription login, old tool results are elided and the context summarised as if the model's window held at most this many tokens, since every request counts against the usage window by its size; `0` uses the model's whole window |
| `[team]` | `max_parallel` | `4` | Sub-agents running at once; see [team](team.md) |
| | `max_depth` | `2` | How deep agents may spawn agents |
| | `max_turns` | `200` | Requests per task for a sub-agent whose definition sets no `max-turns`; `0` is no limit |
| | `agent_retention_days` | `14` | Days a finished sub-agent stays in the [agent hub](tui.md#the-agent-hub); after that its transcript file is deleted at the next start. `0` keeps them as long as the session |
| | `coordinator` | `false` | The lead only plans and delegates; see [coordinator mode](team.md#coordinator-mode) |
| | `worktree_target` | `"shared"` | Where an agent in a worktree of a Cargo project builds: `shared` (the main checkout's `target/`), `own`, or `sccache`; see [worktrees](team.md#build-output-of-worktrees) |
| | `model_routing` | `"off"` | `suggest` has the `smol` role rate each sub-agent's task and records the model it would pick; `auto` moves the agent to that model; see [model routing](team.md#model-routing) |
| | `routing_fast` | `"@smol"` | Model or `@role` for tasks rated `fast` (mechanical, local, well specified) |
| | `routing_balanced` | `"@default"` | Model or `@role` for tasks rated `balanced` (ordinary engineering) |
| | `routing_deep` | `"@slow"` | Model or `@role` for tasks rated `deep` (unknown cause, open design, irreversible action) |
| | `allow_deep` | `false` | Routing may move an agent to `routing_deep`; off, it never goes above `routing_balanced` |
| | `min_upgrade_confidence` | `0.85` | The confidence a rating needs to move an agent to a stronger model |
| | `min_downgrade_confidence` | `0.6` | The confidence a rating needs to move an agent to a cheaper model |
| `[advisor]` | `model` | `[]` | Chain of the second model that reviews the lead's work; empty means no advisor; see [advisor](team.md#advisor) |
| | `when` | `"off"` | `stop` reviews each time the lead is about to finish; `off` leaves the advisor to `/advise` |
| `[verify]` | `commands` | `[]` | Check commands; empty means auto-detect; see [goals](goals.md) |
| | `agent` | `true` | Run the verifier agent after the commands pass |
| | `timeout_secs` | `900` | Time limit per command |
| | `on_stop` | `false` | Also run the commands when an ordinary prompt changed something |
| `[goal]` | `max_iterations` | `8` | Failed verifications after which a goal fails |
| | `max_nudges` | `3` | Idle reminders before a goal is paused |
| `[limits]` | `max_cost_usd` | `0` | Spend, in US dollars, at which every agent of the run stops making requests; `0` is no cap. `--max-cost` sets it for one run |
| | `warn_cost_usd` | `0` | Spend at which a warning is shown once, and from which the cost in the [status line](tui.md#status-line) is in the warning colour; `0` is no warning |
| | `max_turns` | `0` | Requests the lead may make per prompt; `0` is no limit. `--max-turns` sets it for one run |
| `[tools]` | `disabled` | `[]` | Tool names withheld from every agent; see [tools](tools.md) |
| | `defer` | `["mcp__*", "browser", "screen", "debug", "sprite_sheet", "generate_*", "schedule", "pty", "http", "list_mcp_resources", "worktree_apply", "worktree_discard", "memory_forget"]` | Tools kept out of the prompt until the model searches for them, on Claude 4.5 and later through Anthropic's API; a trailing `*` covers every name it starts; see [deferred tools](tools.md#deferred-tools) |
| | `code_first` | `false` | Give the model full schemas only for `read`, `edit`, `write`, `shell`, `eval`, `ask_user`, `todo` and `task` (and `patch`, `plan_submit` and the goal and report tools where they apply); it calls every other tool from a Luau `eval` script. Needs `tools.eval.luau`; see [code-first mode](tools.md#code-first-mode) |
| | `code_first_listing` | `"names"` | How `eval`'s description lists the tools `code_first` hides: `names` (by name, MCP tools counted per server; the model uses `tools.search` and `tools.describe` for arguments) or `signatures` (one-line signatures); see [code-first mode](tools.md#code-first-mode) |
| | `max_result_bytes` | `50000` | Tool output beyond this is cut and saved as an artifact |
| | `shell_timeout_secs` | `120` | Default time limit of a `shell` call |
| | `cache_reads` | `true` | A repeated `read`, `grep` or `glob` that gives the same result while the earlier one is still in the context is answered with a short note; see [tools](tools.md#repeated-reads-retries-and-time-limits) |
| | `retries` | `2` | How often a read-only web, LSP or MCP call is run again after a transient failure |
| | `call_timeout_secs` | `120` | Time limit of a web, LSP or MCP call; `0` is no limit |
| | `priority` | `"low"` | Operating-system priority of the processes started for agents (`shell`, jobs, terminals, `eval`, checks, hooks): `low`, `idle` or `normal`; see [tools](tools.md#keeping-the-machine-responsive) |
| | `build_jobs` | `"half"` | Parallel build jobs cargo, make and cmake may run for agents: `half` of the cores (at least 2), a number, or `all` to set nothing; see [tools](tools.md#keeping-the-machine-responsive) |
| | `shell` | `""` | Shell program; empty means auto-detect (`bash`, else `sh`; on Windows Git's bash, `pwsh`, `powershell`) |
| | `format_on_write` | `true` | Run the file's formatter after `write`, `edit` and `patch`: the configured one, else one the file's project uses; see [tools](tools.md#format-on-write) |
| | `format` | `{}` | `[tools.format]`: file extension to formatter command, such as `rs = "rustfmt --edition 2024"`; an empty command turns formatting off for the extension |
| | `patch` | `"auto"` | Which models get the [`patch`](tools.md#patch) tool: `auto` (OpenAI GPT and Codex models, in place of `edit`), `on` (every model, beside `edit`) or `off` |
| | `check_on_write` | `true` | Run the file's checkers (linters) after `write` and `edit`, when they are installed; see [tools](tools.md#checks-on-write) |
| | `check` | `{}` | `[tools.check]`: file extension to checker command, replacing the built-in one; `{file}` is the file's path, and an empty command turns the check off |
| | `check_report` | `"delta"` | What the lead is told about findings when it stops: `delta` (those on lines changed in the turn), `all` or `off`; see [tools](tools.md#the-report-when-the-lead-stops) |
| | `read_guard` | `"range"` | What a read must cover before an edit: `range` (the lines the edit changes), `file` (any read of the file) or `off`; see [the read guard](tools.md#the-read-guard) |
| | `redact_output` | `false` | Replace secrets in tool results with `[redacted:<kind>]` before the model and the session file get them; see [permissions](permissions.md#secrets-in-tool-results) |
| | `slim_results` | `true` | Drop `node_id`s, HTML copies and empty fields (and colours, avatars and API links of GitHub and Linear results) from large JSON results of MCP tools and web fetches, with a note and the full text saved; see [tools](tools.md#slimmer-and-digested-results) |
| | `digest` | `"off"` | `smol`: a large MCP, web or PDF result is replaced by notes the `smol` model writes, with the full text saved to a file; `off` keeps results as they are; see [tools](tools.md#slimmer-and-digested-results) |
| | `digest_min_chars` | `16000` | Characters of an MCP or web result above which it is digested |
| | `digest_min_pages` | `10` | Pages of a PDF read above which it is digested |
| `[tools.scan]` | `secrets` | `true` | Look for committed secrets with `gitleaks` once per session, when it is installed and the project is trusted; see [tools](tools.md#secret-scan) |
| `[tools.eval]` | `python` | `true` | Let the [`eval`](tools.md#eval) tool run Python |
| | `javascript` | `true` | Let the [`eval`](tools.md#eval) tool run JavaScript |
| | `luau` | `true` | Let the [`eval`](tools.md#eval) tool run Luau scripts that call other tools; with all three off, `eval` is not offered |
| | `background_max_secs` | `7200` | Longest a background Luau script (`eval` with `background: true`) may run, in seconds of wall-clock time; `0` is no limit |
| | `background_approval` | `pause` | What a background Luau script does at a tool call that needs your approval: `pause` asks you and runs the script again once you answer; `deny` refuses the call |
| | `background_pause_secs` | `0` | Longest a paused background script waits for your answer, in seconds; `0` waits until the session ends |
| `[[hooks.<event>]]` | `matcher`, `if`, `type`, `command`, `url`, `headers`, `prompt`, `model` | unset | See [hooks](hooks.md). `type` defaults to `command` |
| | `timeout` | `60` | Seconds |
| | `async` | `false` | Do not wait for the hook |
| | `once` | `false` | Run at most one time per session |
| | `wake` | `false` | With `async`: give the hook's output to the lead agent when it finishes |
| `[guards]` | `done_check` | `true` | Send the lead back once when it stops on changed code and has run no build, test or linter since. See [guards](tools.md#guards) |
| | `stuck_after` | `3` | Add a note to the result of a `shell` command that has failed this many times in a row in a turn; `0` is off |
| | `runaway` | `true` | Stop a reply that repeats a block of 200 characters or more three times in a row, and ask again |
| `[schedule]` | `register` | `true` | Hand [schedules](background-and-remote.md#schedules) to the system's scheduler (Task Scheduler, cron, launchd). Off, they run only when `azazel schedule run --due` is called |
| | `poll_secs` | `60` | Seconds between two looks at the CI runs for a [`ci-failure` trigger](background-and-remote.md#triggers) |
| | `loop_max` | `50` | Rounds after which [`/loop`](tui.md#loops) with an interval or a condition stops by itself; `0` is no limit |
| `[triggers]` | `handle` | `"@azazel"` | The handle a pull-request comment has to mention to start a run of an [`on pr-comment` trigger](background-and-remote.md#comments-and-messages-as-triggers) |
| | `mode` | `"accept-edits"` | The permission mode of the runs `on pr-comment` and `on slack:<channel>` start; `bypass` counts as `auto`, and a schedule added with a stricter `--mode` keeps it. The runs are headless: what would ask is refused |
| | `github_authors` | `[]` | GitHub users whose comments start runs. Empty: the user `gh` is signed in as, and users with write access to the repository |
| | `slack_users` | `[]` | Slack user ids whose messages start runs. Empty: nobody |
| | `slack_url` | `"https://slack.com/api"` | The address of the Slack Web API; the bot token is sent only there |
| `[cache]` | `warming` | `"off"` | `idle` keeps Anthropic's prompt cache warm while the session waits for you; see [prompt cache](models-and-logins.md#prompt-cache) |
| | `warm_minutes` | `30` | How long after the lead's last request the cache is kept warm |
| | `report_misses` | `false` | Say in the transcript when a request misses the cached prefix, and why; `/cost` lists the misses either way |
| | `suggest_handoff_at` | `0` | Past this many context tokens (try `200000`), say once, a minute before the cache runs out, that `/handoff` or `/compact` is cheapest now; `0` never says it. It makes no request; see [prompt cache](models-and-logins.md#prompt-cache) |
| `[context]` | `nested` | `true` | Give an agent the `AGENTS.md` of every directory it works in, with its first tool call there; see [context files](rules.md#context-files) |
| | `include_depth` | `4` | How deep `@path` includes inside context files and rule files are followed; `0` follows none |
| | `repo_map_tokens` | `0` | Tokens of the [repository map](tools.md#repository-map) put in the lead's system prompt; `0` leaves it out. The map is made once per session and changes only with `/repomap refresh`, so the prompt cache stays valid |
| `[rules]` | `compat` | `true` | Also read the rule files of Cursor, Windsurf, Copilot, Gemini and Cline; see [rules](rules.md) |
| `[[stream_rules]]` | `pattern` | required | Regular expression matched against a reply as it is written |
| | `message` | required | What the agent is told when the pattern matches |
| | `scope` | `"text"` | `text`, `thinking` or `tool:<name>` |

Hook events: `pre_tool_use`, `post_tool_use`, `post_tool_use_failure`, `user_prompt_submit`, `session_start`,
`session_end`, `stop`, `subagent_start`, `subagent_stop`, `pre_compact`, `post_compact`, `permission_request`,
`notification`, `file_changed`, `config_change`, `worktree_create`, `worktree_remove`, `setup`.

`[[stream_rules]]` arrays concatenate across layers, like the hook arrays.

## Features

| Table | Key | Default | Meaning |
|---|---|---|---|
| `[memory]` | `enabled` | `true` | Project memory and its tools; see [memory](memory.md) |
| | `auto_extract` | `true` | Extract memories after compaction, and from a session that ended in the next session of the project |
| | `auto_index` | `true` | Refresh the code index in the background |
| `[embeddings]` | `provider`, `model` | absent | Embedding endpoint; both keys are required. Absent means word search only |
| `[web]` | `search` | `"auto"` | `auto`, `provider`, `brave`, `tavily`, `exa`, `searxng` or `duckduckgo`; see [web and browser](web-and-browser.md) |
| | `searxng_url` | `""` | Address of a SearXNG instance |
| | `provider_search` | `true` | Under `search = "auto"`, search through the lead model's provider first when it has a search tool; see [the provider's own search](web-and-browser.md#the-providers-own-search) |
| `[remote]` | `port` | `7420` | The port `azazel remote` listens on, on this machine only; when it is taken the next nine are tried. 0 lets the system choose; see [remote](remote.md) |
| | `access` | `"cloudflare"` | How `azazel remote --share` and `/remote` reach the server from another device: `cloudflare` (a quick tunnel) or `tailscale` |
| | `public_url` | `""` | The address of your own tunnel or reverse proxy in front of the server; it becomes the origin of the links and an accepted origin |
| | `link_minutes` | `15` | Minutes a pairing link works (once) |
| | `device_hours` | `24` | Hours a paired device stays signed in |
| | `redact` | `true` | Replace API keys and tokens in the messages and tool results sent to a browser |
| | `allow_bypass` | `false` | Let a browser with control access switch to the `bypass` mode, and [start sessions](remote.md#starting-a-session-from-the-browser) in any folder without asking at the computer |
| | `view_browser` | `false` | Let a view link watch the agent's browser live too. The picture is not redacted, so by default only a control link may; taking over always needs a control link. See [watching the agent's browser](remote.md#watching-the-agents-browser) |
| | `full_access` | `false` | Let a browser with control access do everything the terminal can: shell commands, files, settings, sign-ins, trust, `bypass` and any folder. A view link stays view-only. See [full access](remote.md#full-access) |
| | `folders` | `[]` | Folders a browser may [start sessions](remote.md#starting-a-session-from-the-browser) in, and the folders inside them, beside the projects sessions have worked in; `~` is the home directory |
| `[telemetry]` | `exporter` | `"off"` | `off`, `otlp` or `langsmith`; see [trace export](#trace-export) |
| | `endpoint` | `""` | Collector address; empty uses the exporter's default |
| | `project` | `""` | Project name shown by the backend: LangSmith's project, `service.name` over OTLP |
| | `api_key_env` | `""` | Environment variable holding the backend's key |
| | `content` | `false` | Send prompt and reply text, tool input and tool output, not only the shape of each step |
| `[usage]` | `auto_reset` | `false` | Spend a banked usage reset without asking when nobody is there to ask (print mode, background sessions, schedules) and a subscription runs out; see [when every account is out](models-and-logins.md#when-every-account-is-out). With somebody there the choice is always theirs |
| | `poll_secs` | `300` | How often, in seconds, the usage of every stored subscription login is fetched in the background while a session is open, so that a request does not start on an account with nothing left. Never more often than every 60; `0` never. A login is not renewed for this; see [several accounts](models-and-logins.md#several-accounts) |
| | `redact_email` | `true` | Show e-mail addresses as `j***@example.com` in `/usage`, `azazel usage` (`--json` included) and `/account` |
| | `ledger` | `true` | Append every answered model request (time, provider, model, project, session, agent or purpose, tokens, estimated cost) to `~/.azazel/usage/YYYY-MM.jsonl`, the history `azazel stats` and `/stats` read. Session and agent retention never delete it |
| `[accounts]` | `order` | `{}` | Provider id to account labels in the order work moves through them when the account in use runs out, such as `{ anthropic = ["work", "default"] }`. Accounts that are not listed follow: the default one, then the rest by label; see [several accounts](models-and-logins.md#several-accounts) |
| `[browser]` | `path` | `""` | Browser binary; empty means look for Chrome, Chromium, Edge or Brave |
| | `headless` | `true` | Run the browser without a window |
| | `screenshot_scale` | `1.0` | Scale screenshots by this (0.1 to 1) before the model gets them, never below 512 pixels on the long side; `0.5` costs about a quarter of the tokens; see [the browser](web-and-browser.md#screenshots-and-snapshots-that-cost-less) |
| | `snapshot_diff` | `true` | A snapshot of the page the agent's last snapshot of the tab showed sends only what changed, and refs keep their numbers; see [the browser](web-and-browser.md#screenshots-and-snapshots-that-cost-less) |
| | `live_fps` | `5` | Pictures per second of the live view of the agent's browser in the web page of `azazel remote` (1 to 15); see [watching and taking over](web-and-browser.md#watching-and-taking-over-from-the-web-page) |
| | `live_quality` | `60` | JPEG quality of the live view (20 to 90); lower sends less data |
| | `live_max_width` | `1280` | Width in pixels the live view is scaled down to (320 to 1920) |
| `[mcp]` | `startup_wait_ms` | `250` | How long the first request waits for MCP servers; see [MCP](mcp.md) |
| | `defer_threshold` | `40` | Above this many MCP tools, they are found through `tool_search` instead |
| | `idle_timeout_secs` | `600` | A server started on demand (`lifecycle: lazy` in its entry) stops after this many seconds without a call; `0` never stops it. See [mcp](mcp.md#when-servers-start) |
| `[media]` | `output_dir` | `"assets/generated"` | Where generated files go; see [media and sprites](media-and-sprites.md) |
| `[media.models]` | `image`, `video`, `model3d`, `audio` | `""` | Model per kind, as `<backend>:<model id>` |
| `[lsp.servers.<name>]` | `command`, `extensions` | required | Language server program and the file extensions it handles; see [LSP](lsp.md) |
| | `args` | `[]` | Arguments |
| | `warm_files` | `[]` | Files opened as soon as the server starts, relative to the project root |
| `[lsp]` | `idle_secs` | `240` | Stop a server after this many seconds in which none of its files was touched; `0` keeps it running |
| | `install` | `"ask"` | What happens when a file has a default language server that is not installed: `ask` asks once per server, `auto` installs without asking, `off` only says the command. The whole `[lsp]` table is trust-gated. Installs go into `~/.azazel/lsp/`; see [installing servers](lsp.md#installing-servers) |
| | `write_severity` | `"error"` | The lowest severity reported after a write: `error` or `warning` |
| `[debug.adapters.<language>]` | `command` | required | Debug adapter program for `rust`, `c`, `cpp`, `python` or `go`, instead of the one found on `PATH`; see [tools](tools.md#debugger) |
| | `args` | `[]` | Arguments |
| | `listens` | `false` (`true` for `go`) | The adapter listens on a socket and prints where, instead of talking on its standard streams |
| `[debug]` | `timeout_secs` | `30` | Seconds a `debug` call waits for the program to stop or end |
| `[voice]` | `stt`, `tts` | `""` | Speech-to-text and text-to-speech model, as `<provider>/<model>`; see [voice](voice.md) |
| | `voice`, `language` | `""` | Voice name for speech; language code of what is dictated |
| | `record_command`, `stt_command`, `play_command` | `""` | Programs that record, transcribe and play instead of the built-in ways |
| | `auto_send` | `false` | Send a transcript as a prompt at once |
| | `max_secs` | `120` | Longest recording, in seconds |
| `[tui]` | `theme` | `"dark"` | A built-in theme (`auto`, `dark`, `light`, `catppuccin`, `dracula`, `nord`, `solarized`, `github`, …) or a file in `~/.azazel/themes/`. A built-in theme with a dark and a light palette follows the terminal's background (`auto` is `light` or `dark`); `<name>-dark` and `<name>-light` pin one; see [the terminal UI](tui.md#themes) |
| | `glyphs` | `"auto"` | Symbols the UI draws with: `unicode`, `nerd` (needs a Nerd Font) or `ascii`; `auto` is `unicode` unless the locale is not UTF-8. `AZAZEL_GLYPHS` overrides it |
| | `popups` | `true` | Menus, pickers and questions open as a box in the middle of the window |
| | `mouse` | `true` | Click, hover and scroll in a popup; see [the mouse in popups](tui.md#the-mouse-in-popups). `false` leaves the mouse to the terminal at all times |
| | `complete_ignored` | `false` | `@` also offers files that `.gitignore` leaves out; `.git/`, `target/` and `node_modules/` are still skipped |
| | `indicator` | `"shimmer"` | How the [working indicator](tui.md#working-indicator) moves: `shimmer`, `kitt` or `static` |
| | `indicator_text` | `[]` | Phrases the working indicator shows in turn; empty shows what the agent is doing. A running tool is always named |
| | `session_accent` | `false` | Tint the working indicator with a colour derived from the session's name |
| | `todo_max` | `8` | Rows the todo list above the input takes at most. A longer list folds the finished items into one row (`✓ 11 done`), shows the item in progress and the next ones, and counts the rest (`… 3 more`); `0` hides the list |
| | `tool_style` | `"card"` | `card` frames each tool call; `compact` shows a finished call on one row and calls that follow one another as a tree; see [compact style](tui.md#compact-style) |
| | `diff_view` | `"auto"` | How `/diff` shows a file: `split` (old beside new), `unified` (one column), or `auto`, split in a window of 140 columns or more; see [diffs and commits](tui.md#diffs-and-commits) |
| | `redact` | `true` | Hide what looks like a secret in tool cards, the transcript viewer and Markdown and HTML exports; the model and the session file keep it. See [hiding secrets](tui.md#hiding-secrets) |
| | `favourites` | `[]` | Models and roles `Ctrl+P` steps through, such as `["@default", "anthropic/claude-x"]`; `Ctrl+F` in the model picker edits the list. Empty steps through the roles that were given models; see [models and roles at a key](tui.md#models-and-roles-at-a-key) |
| | `images` | `"auto"` | Inline images: `auto` (what the terminal supports), `off`, or a protocol to force: `kitty`, `iterm`, `sixel`; see [images](tui.md#images) |
| | `show_thinking` | `false` | Start with the model's thinking shown (`Ctrl+T` toggles it) |
| | `expand_tools` | `false` | Start with tool output shown in full (`Ctrl+O` toggles it) |
| | `autocomplete` | `"words"` | Ghost text in the input: `words` completes the word being typed from your earlier prompts, this run and the project's file names (`Tab` accepts with a space, `Right` without); `off` shows none; see [ghost text](tui.md#ghost-text) |
| | `predict_next` | `false` | After a finished turn, ask the session's model for your likely next message and show it as ghost text in the empty input. Each finished turn costs one more request, counted in `/cost` as `prediction` |
| | `queue_mode` | `"one"` | Messages queued while the agent works: `one` sends them one turn at a time, `all` sends them together as one prompt; see [queued messages](tui.md#queued-messages-and-stashed-drafts) |
| `[statusline]` | `preset` | `"default"` | Segments shown when `left` and `right` are empty: `default`, `minimal`, `powerline`, `lean` or `full`; see [the status line](tui.md#status-line). `/statusline` sets these keys up with the line in view |
| | `left`, `right` | `[]` | Segment names on each side, in order: `mode`, `agent`, `model`, `profile`, `thinking`, `tier`, `path`, `branch`, `context`, `bar`, `rate`, `cost`, `usage`, `agents`, `goal`, `todo`, `checked`, `queue`, `session`, `time`, `git_dirty`, `account`, `cache`, `extensions`. Empty takes the preset's; `["none"]` is an empty side |
| | `separator` | `"dot"` | Between segments: `dot`, `bar`, `space`, or `powerline` (each segment a block of colour, ended by an arrow with `tui.glyphs = "nerd"`). The `powerline` preset is drawn in blocks while this is `dot`; every other preset, `lean` included, follows this setting |
| | `segments.<name>` | `{}` | Options of one segment: `priority` (`0` to `100`; the lowest leaves a narrow line first) and `icon` (`false` leaves the symbol out) for all; `max` columns for `path`, `model`, `branch`, `goal`, `session`, `account`, `extensions`; `style` for `path` (`short`, `full`, `basename`), `model` (`full`, `short`), `context` (`percent`, `tokens`) and `account` (`label`, `email`); `format` (`24h`, `12h`) and `seconds` for `time`; `cells` and `min_width` for `bar`. For example `[statusline.segments.path] style = "basename"` |
| | `command` | `""` | A program whose first line of output replaces the left side of the status line; it is given the session as JSON on its standard input, runs at most once a second and is ended after two seconds. Empty runs nothing. See [your own command](tui.md#your-own-command) |
| `[editor]` | `mode` | `"emacs"` | `emacs` or `vim` keys in the input; `/vim` switches; see [Vim keys](tui.md#vim-keys) |
| | `command` | `""` | Program `Ctrl+G` opens the draft in; empty uses `$VISUAL`, then `$EDITOR`, then `notepad` or `vi` |
| | `fold_paste_lines` | `10` | Pastes longer than this many lines fold into one token; `0` never folds |
| | `paste` | `"attachment"` | How a folded paste reaches the agent: `attachment` wraps it in `<attachment>` tags, `file` saves it as `local://paste-N.md` for the agent to read when it needs to, `inline` sends it as typed; see [pasting](tui.md#pasting) |
| `[compat]` | `claude` | `true` | Also read `CLAUDE.md` and `.claude/` skills, agents and commands |
| `[notify]` | `bell` | `true` | Ring the terminal bell when an agent needs you or a long run ends, while the terminal is not focused; see [attention](tui.md#attention-title-and-progress) |
| | `desktop` | `"auto"` | Desktop notification at the same moments: `auto`, `terminal`, `system` or `off` |
| | `title` | `true` | Show the session and its state in the terminal's title |
| | `progress` | `true` | Show working, waiting and failed in the terminal's tab (OSC 9;4) |
| | `min_secs` | `20` | A finished or failed run notifies only when it took at least this many seconds |
| | `keep_awake` | `true` | Keep the machine from sleeping while agents work |
| `[update]` | `check` | `true` | Look for a newer release at most once a day |
| | `auto` | `false` | Also download a newer release in the background and install it when the UI exits; see [updating](getting-started.md#updating) |

## Trace export

With `[telemetry] exporter` set, azazel sends what a session does to a tracing backend as spans. It is off by
default, and a project's config can turn it on only once the project is [trusted](#trust).

```toml
[telemetry]
exporter = "otlp"                      # or "langsmith"
endpoint = "http://localhost:4318"     # the collector; /v1/traces is added
api_key_env = "OTEL_KEY"               # sent as `Authorization: Bearer …`
```

Each prompt is one trace, with the prompt as its root:

| Span | Name | Carries |
|---|---|---|
| Prompt | `prompt` | The agent (`gen_ai.agent.name`, `gen_ai.agent.id`), how it ended (`azazel.stop`), its requests, tool calls, tokens and cost; the error of a prompt that failed |
| Model request | `chat <model>` | Provider and model, input, output, cached and reasoning tokens, cost (`azazel.cost_usd`), the stop reason, the time to the first token, retries, stalls and the model it fell back from; the error of a request that failed |
| Tool call | `execute_tool <name>` | The tool, its call id, how long it ran, the size of its output and whether it reported an error |

A tool call hangs under the request whose reply asked for it. A sub-agent's prompt hangs under the tool call that
was running in its parent when it started, in the same trace, so a delegated task reads as one tree. A
sub-agent that is picked up again while its parent is at rest starts a trace of its own.

The session is not a span. Every span carries its id as `session.id` and `gen_ai.conversation.id`, and a
backend groups the prompts of a session by that. A span is sent when it ends, so a prompt is whole in the
backend as soon as it finishes, however long the session goes on. Attribute names follow the
OpenTelemetry conventions for generative AI (`gen_ai.*`) where one exists. Requests outside a conversation
(session names, summaries, `/btw`, the advisor) are not traced.

**What was said is not sent** unless `content = true`: without it a span holds names, counts, sizes, durations
and the errors of providers, and a failed tool call says only that it failed. With it, a prompt's span carries the
prompt and the final reply, a request's span the reply and the tool calls it asked for, and a tool call's span its
input and output. What looks like a secret is replaced by `[redacted:<kind>]` first, and a text longer than
20,000 characters is cut. A sub-agent's prompt is the input of the tool call that started it.

| Exporter | Where to | Key |
|---|---|---|
| `otlp` | OTLP over HTTP as JSON, posted to `<endpoint>/v1/traces`. Without `endpoint`: `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, else `OTEL_EXPORTER_OTLP_ENDPOINT`, else `http://localhost:4318` | The variable `api_key_env` names, as `Authorization: Bearer <key>` (a value with a space in it, such as `Basic …`, is sent as it is). `OTEL_EXPORTER_OTLP_HEADERS` (`name=value,…`) adds headers |
| `langsmith` | The runs API, `<endpoint>/runs/batch`. Without `endpoint`: `LANGSMITH_ENDPOINT`, else `https://api.smith.langchain.com`. Runs go to the project `project`, else `LANGSMITH_PROJECT`, else `azazel`; the session id is each run's `session_id` and `thread_id` metadata, which the Threads view groups by | The variable `api_key_env` names, else `LANGSMITH_API_KEY`, as `x-api-key` |

Spans are sent beside the session, in batches: every two seconds, or sooner when 64 wait. No turn waits for the
backend. When azazel exits, it waits up to three seconds for the last batch and then leaves it. A batch the
backend refuses is dropped; the first failure is written to `~/.azazel/logs/azazel.log` and later ones are not.
`azazel doctor` says where traces go, whether text goes with them and whether the key is missing.

## Trust

A project's files can make azazel run programs. Until you trust a project, the parts that could do so are held
back. A project needs a trust decision when it has any of:

- files in `.azazel/extensions/`
- `.azazel/mcp.json` or `.mcp.json`
- one of the gated settings in `.azazel/config.toml` or `.azazel/config.local.toml`
- `.azazel/models.yml`
- a project skill with `hooks` in its frontmatter
- a `Cargo.toml`, an eslint config file or a `go.mod` at its root, when checks on write are on and clippy, eslint
  or go is installed: checking a file with them runs the project's build scripts, macros or lint configuration;
  see [tools](tools.md#checkers-that-run-the-projects-code)

Gated settings: `hooks`, `roles`, `profiles`, `providers`, `permissions.allow`, `permissions.destructive`, `permissions.mode` when it
is `bypass`, `lsp`, `debug.adapters`, `verify.commands`,
`tools.shell`, `tools.format`, `tools.format_on_write`, `tools.check`, `tools.scan`, `tools.eval`, `browser.path`, `editor.command`,
`voice.record_command`, `voice.stt_command`, `voice.play_command`, `advisor`, `telemetry`, `statusline.command`,
`triggers.mode`, `triggers.github_authors`, `triggers.slack_users`, `triggers.slack_url`, `remote`.

The interactive UI asks once per project, before it starts: `Trust this project? Its config can run commands.
[y/N]`. The answer is stored in `~/.azazel/trust.json`. In the UI, `/trust` shows the decision and what it
covers, and `/trust yes` or `/trust no` changes it; the change applies from the next start.
Print mode, RPC mode and the subcommands never ask: they treat an undecided project as untrusted unless you pass
`--trust`, which trusts it for that run only.

In an untrusted project:

- gated settings in the two project config files are dropped; everything else in them still applies
- the project's `models.yml` is not read
- project extensions, project MCP servers and skill hooks are not loaded
- the checkers that run the project's code (clippy, eslint, go vet) are off; the others still check what is written
- API keys in the project's `.env` are not read
- `` !`command` `` lines in custom commands are left as text
- context files, rule files, skills, agent definitions and custom commands still load

In a trusted project each MCP server the project defines still waits for your approval; see
[MCP](mcp.md#approving-a-projects-servers).

`~/.azazel/config.toml` is never filtered. The UI lists what was skipped when it starts, and so does
`azazel doctor`.

## Command-line flags

| Flag | Meaning |
|---|---|
| `PROMPT...` | Words after the flags: the first message of the session, or with `-p` the whole run. Put `--` before a prompt that starts with a subcommand's name |
| `-p`, `--print` | Run the prompt without the UI and print the result |
| `--model <SPEC>` | `provider/model[:thinking]`, `@role`, or part of a model's name (`fable`); repeat to add fallbacks. Concrete models become this run's `default` role |
| `--models <SPECS>` | What `Ctrl+P` steps through in this run, in place of `tui.favourites`: models and `@roles` separated by commas, named as for `--model`; see [models and roles at a key](tui.md#models-and-roles-at-a-key) |
| `--thinking <LEVEL>` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`; wins over the level of the lead's role. Without it the lead asks for its model's `:level`, else `off` |
| `--mode <MODE>` | Permission mode: `plan`, `manual`, `accept-edits`, `auto`, `bypass` |
| `--agent <NAME>` | The [primary agent](team.md#primary-agents) the lead is for this run: `build`, `plan`, `lean` or one of your own; a name that is no primary agent exits with status 2 |
| `--output <text\|json>` | Output of print mode; default `text` |
| `-c`, `--continue` | Continue the most recent session of this project |
| `--new` | Start a new session, also when `[session] resume = "last"` would continue the latest; cannot go with `-c`, `--session` or `--fork` |
| `--continue-run` | With `-c` or `--session`: go on with the run the session's last process did not finish, instead of sending a prompt; see [interrupted runs](sessions-and-checkpoints.md#interrupted-runs) |
| `-r`, `--session <PATH\|ID>` | Open a session by its file or its id (a unique prefix is enough); `--resume` is the old name |
| `--fork <PATH\|ID>` | Continue in a copy of a session, leaving the original as it is |
| `--name <NAME>` | Name the session |
| `--session-dir <DIR>` | Keep this run's sessions in `DIR` |
| `--no-session` | Keep nothing of the run: no session file, no checkpoints |
| `--add-dir <DIR>` | Let agents work in another directory too; repeatable |
| `--set <TABLE.KEY=VALUE>` | Change one setting for this run, as written in `config.toml` (`--set limits.max_turns=20`); repeatable |
| `--tools <NAMES>` | Offer only these tools, comma-separated; globs such as `mcp__github__*` work |
| `--no-tools` | Offer no tools |
| `--disallowed-tools <NAMES>` | Never offer these tools, on top of `[tools] disabled` |
| `--system-prompt <TEXT\|@FILE>` | Replace the lead's built-in instructions (its identity and rules); sub-agents keep theirs |
| `--append-system-prompt <TEXT\|@FILE>` | Add text to the lead's instructions, after the context files |
| `-e`, `--extension <PATH>` | Load a Luau script, or a directory laid out like a [package](packages.md), for this run; repeatable. See [resources for one run](#resources-for-one-run) |
| `--skill <PATH>` | Add a skill (its directory or its `SKILL.md`) or a directory of skills for this run; repeatable |
| `--mcp-config <FILE>` | Connect to the MCP servers of this `mcp.json` for this run; repeatable |
| `--theme <NAME\|FILE>` | Show this run in a theme: a built-in one, `auto`, one from `~/.azazel/themes`, or a theme file |
| `--provider <ID>` | Take this run's models from one provider only; see [choosing the model](models-and-logins.md#choosing-the-model) |
| `--no-extensions`, `--no-skills`, `--no-hooks`, `--no-context-files`, `--no-mcp`, `--no-memory` | Leave that kind of addition out of this run. `--no-context-files` also leaves out [rule files](rules.md). What `--extension`, `--skill` and `--mcp-config` name still loads |
| `--bare` | All six `--no-*` switches |
| `--max-turns <N>` | Requests the lead may make per prompt (`[limits] max_turns`) |
| `--max-cost <USD>` | Stop every agent once the run has spent this much (`[limits] max_cost_usd`) |
| `--coordinator` | Start in [coordinator mode](team.md#coordinator-mode): the lead plans and delegates |
| `--json-schema <FILE\|JSON>` | With `-p`: the run must end in JSON that matches this schema, and only that JSON is printed; see [typed results](team.md#typed-results) |
| `-f`, `--file <PATH>` | With `-p`: a file for the prompt, repeatable up to 100. A text file is added to the prompt as an `@path` mention is, an image is attached. A file that is missing, larger than 256 KB of text or neither text nor an image ends the run with status 2 before anything is sent |
| `--worktree[=NAME]` | With `-p`: run in a new git worktree, `.azazel/worktrees/<NAME>` on a new branch `azazel/<NAME>` from HEAD (named after the time when no name is given); its path is printed at the end. See [worktrees](sessions-and-checkpoints.md#worktrees) |
| `--archived` | With `-p`: keep the session out of `/resume`, `/open`, `-c` and `azazel sessions list` (`--all` lists it); for runs from scripts and CI |
| `--tier <TIER>` | [Service tier](models-and-logins.md#service-tiers) of the requests: `priority`, `standard`, `economy` |
| `--profile <NAME>` | Use the models of a [profile](models-and-logins.md#profiles), built-in or saved, for this run; also before a subcommand. `--preset` is the old name |
| `--cwd <DIR>` | Work in this directory instead of the current one |
| `--trust` | Trust this project's configuration for this run |
| `--ssh <HOST>` | Work on another machine: azazel runs there and is shown here; see [remote work](background-and-remote.md#remote-work-over-ssh) |
| `--remote-dir <DIR>`, `--remote-azazel <COMMAND>` | With `--ssh`: the directory to work in there, and how azazel is started there |
| `--detach`, `--attach <ID>` | With `--ssh`: run the prompt in a background session there, or open one of its background sessions |
| `--plain` | Show a background session as lines instead of in the full UI (`azazel attach`, `--ssh --attach`) |
| `--api-key <KEY>` | Key for the provider of the first `--model`, for this run only |
| `--debug` | Write debug-level logs to `~/.azazel/logs/azazel.log` |
| `--version`, `--help` | Print the version or the usage |

`--model`, `--thinking`, `--mode`, `--agent`, `--output`, `--cwd`, `--trust`, `--session-dir`, `--add-dir`, `--set`,
`--tier`, `--profile`, `--theme`, `--coordinator`, `--max-turns`, `--ssh`, `--remote-dir`, `--remote-azazel` and
`--plain` may also follow a subcommand, as in `azazel goal "make the tests pass" --mode auto`.

### Resources for one run

`--extension`, `--skill`, `--mcp-config` and `--theme` load something for one run without installing it: to
try an extension while writing it, to give a CI job the one skill it needs, to see a theme before keeping it.
Nothing is copied and nothing is remembered; the next run without the flag does not have it.

| Flag | Takes | Loads |
|---|---|---|
| `-e`, `--extension` | A `.luau` file | That script. Its id is the file's name, or the directory's for `<name>/init.luau` |
| | A directory | Everything a [package](packages.md#what-a-package-contains) in that directory would offer (skills, agent definitions, commands, Luau scripts, MCP servers) with its code enabled, plus an `init.luau` at its top |
| `--skill` | A skill's directory, its `SKILL.md`, or a directory of skill directories | Those skills, shown as `command line` |
| `--mcp-config` | A file with an `mcpServers` object, as [`mcp.json`](mcp.md#configuration-files) | Its servers |
| `--theme` | A theme's name, or a [theme file](tui.md#themes) | The colours of the UI; the same as `--set tui.theme=<name>` for a name |

- A path is relative to the working directory (`--cwd`). One that does not exist, or holds nothing of the
  kind, ends the run with status 2 and the reason.
- They are **your** word, so they are trusted like the files under `~/.azazel`: they load in a project that is
  not trusted, a script may run commands, and the hooks of a skill count.
- They come last, so they win: a skill, command, agent definition, script id or MCP server of the same name
  found elsewhere is replaced for this run.
- They load with the matching `--no-*` switch and with `--bare`, which then leave only them:
  `azazel --bare -e ./audit.luau -p "..."` runs with that one script and nothing else that was discovered.
- A background session started from the run (`/bg`) has them too. `--extension`, `--skill` and `--mcp-config`
  name files of this machine and cannot be combined with `--ssh`.

A wrong flag value (an unknown tool, a model name that matches nothing or several models, a bad `--set`, an
unknown tier or profile) exits with status 2.

## Subcommands

`azazel <command>` does one thing and exits, or starts another way of running. Each has `--help`.

| Command | Actions | Does |
|---|---|---|
| `azazel init` | | Choose the default model and create the project's config file; see [getting started](getting-started.md) |
| `azazel doctor` | | Check the installation and the project's setup |
| `azazel remote` | | Follow and steer the background sessions from a browser or a phone: `azazel remote [--port N] [--view] [--json] [--share] [--tunnel] [--tailscale] [--public-url URL]`; see [remote](remote.md) |
| `azazel update` | | Install the newest release; see [updating](getting-started.md#updating) |
| `azazel completions` | | Print a completion script for a shell |
| `azazel changelog` | | What changed in each release |
| `azazel bug` | | Write a bug report as a zip; see [bug reports](sessions-and-checkpoints.md#bug-reports) |
| `azazel login` | | Sign in to a provider or store its API key; see [models and logins](models-and-logins.md) |
| `azazel logout` | | Forget the stored key or login of a provider |
| `azazel auth` | `status` | Where each provider's credential comes from |
| `azazel models` | | List the models of the providers you have a credential for, with the roles that use them; see [choosing the model](models-and-logins.md#choosing-the-model) |
| `azazel roles` | `set`, `clear`, `preset` | List the model roles with their models, thinking levels and sources, and change them in the home or project config; see [`azazel roles`](models-and-logins.md#azazel-roles) |
| `azazel roles preset` | `save`, `load` | Save every role as a profile, or write a profile's roles into a config file |
| `azazel usage` | | What your subscriptions have left; see [usage](models-and-logins.md#usage) |
| `azazel config` | `get`, `set`, `unset` | Print a setting and the layer that sets it (`azazel config get permissions.mode`), or write one into the home config (`azazel config set retry.max_attempts 5`; a list as comma-separated text or a TOML array: `azazel config set triggers.slack_users '["U0123ABCD"]'`). `azazel config set [--project]` writes the project's config instead, in a trusted project only. `azazel config unset <key> [--project]` takes a setting (or one entry of a table, `lsp.servers.<name>`) out of that file again so it falls back to the next layer or its default; `set` with an empty value writes an empty string and does not do that. The value is checked as the settings screen checks it, and a bad key or value exits with status 2; see [configuration files](#files-and-load-order) |
| `azazel secrets` | `add`, `add-site`, `list`, `rm` | Credentials agents use without seeing them; see [stored credentials](web-and-browser.md#credentials-for-services) |
| `azazel sessions` | `list`, `search`, `delete`, `prune`, `export`, `import` | Manage saved sessions, and bring one in from a file; see [sessions](sessions-and-checkpoints.md#from-the-shell) |
| `azazel export` | | Write a session to a file; `--sanitize` for sharing |
| `azazel open` | | Pick a recent session of any project and open it in the UI; see [projects and directories](sessions-and-checkpoints.md#projects-and-directories) |
| `azazel context` | | What the last request of a session was made of |
| `azazel stats` | | Requests, tokens and cost over time and per provider, model, project and agent, from the usage log; see [usage statistics](sessions-and-checkpoints.md#usage-statistics) |
| `azazel migrate` | | Bring models, keys, skills, commands, agents, MCP servers, rules, settings, chats and memory over from pi, omp, jcode, Claude Code or Codex CLI; see [migrating](migrating.md) |
| `azazel goal` | | Work on an objective until it is verified; see [goals](goals.md) |
| `azazel bg` | | Run a prompt in a background session; see [background sessions](background-and-remote.md) |
| `azazel ps` | | List the background sessions |
| `azazel logs` | | Show what a background session has done |
| `azazel attach` | | Open a background session in the UI |
| `azazel kill` | | Stop a background session |
| `azazel schedule` | `add`, `list`, `rm`, `pause`, `resume`, `run` | Run prompts at a time or when something happens; see [schedules](background-and-remote.md#schedules) |
| `azazel mcp` | `list`, `add`, `remove`, `login`, `logout`, `approve`, `reject`, `serve`, `import` | Manage MCP servers, or serve azazel as one; see [MCP](mcp.md) |
| `azazel pkg` | `add`, `list`, `update`, `remove` | Install and manage packages; see [packages](packages.md) |
| `azazel skills` | `add`, `list`, `remove` | Install and list skills; see [packages](packages.md#azazel-skills) |
| `azazel browser` | `open` | Show the agents' browser profile in a window to sign in to sites: `azazel browser open [URL]`; see [signing in](web-and-browser.md#signing-in) |
| `azazel lsp` | `list`, `install` | List the language servers and install the ones azazel can; see [installing servers](lsp.md#installing-servers) |
| `azazel rules` | `list`, `test` | Show the rule files of the project and test one; see [rules](rules.md) |
| `azazel scripts` | `list`, `show`, `delete` | List, print and delete the saved Luau scripts that `eval` scripts run with `tools.run`; see [saved scripts](skills-and-commands.md#saved-scripts) |
| `azazel memory` | `list`, `search`, `forget`, `export` | What is remembered about the project; see [memory](memory.md) |
| `azazel index` | | Rebuild the index of the project's code |
| `azazel rag` | `add`, `list`, `remove` | Collections of documents `semantic_search` can look through |
| `azazel ext` | `types` | Write the type declarations of the Luau API; see [extensions](extensions.md) |
| `azazel rpc` | | Serve one session to another program; see [RPC mode](rpc.md) |
| `azazel acp` | | Serve editors that speak the Agent Client Protocol; see [ACP](acp.md) |
