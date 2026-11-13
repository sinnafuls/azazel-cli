# Configuration

Azazel is configured with TOML files, environment variables and command-line flags. This page lists where the
files live, how they are merged, every table and key with its default, which project settings are held back until
you trust the project, and the command-line flags.

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
binary), `AZAZEL_HOOKS_DISABLED` (load no hooks), `AZAZEL_NO_UPDATE_CHECK` (skip the update check).

## Models

```toml
[roles]
default = ["<provider>/<model-id>", "ollama/<model-id>"]   # an ordered fallback chain
smol = ["ollama/<model-id>", "@default"]

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
| `[roles]` | `default` | unset | The lead agent's chain. Entries are `provider/model[:thinking]` or `@role` |
| | `smol`, `slow`, `verify`, `vision` | `["@default"]` | Chains for cheap work, hard work, the checker and image input |
| | `plan` | `["@slow"]` | Chain for planning |
| | `memory` | `["@smol"]` | Chain for memory extraction |
| | `lead`, and one per built-in agent | see [roles](models-and-logins.md#roles) | The lead's chain and each agent's, so each can have its own model |
| `[providers.<id>]` | `name` | preset, else the id | What the provider is called in menus |
| | `api` | preset | A [wire API](models-and-logins.md#wire-apis): `anthropic-messages`, `openai-responses`, `openai-chat`, `openrouter`, `google-generative`, `google-gemini-cli`, `ollama-chat`, `azure-openai-responses`, `azure-openai-chat` |
| | `base_url` | preset | Endpoint; may contain `${VAR}` and `${VAR:-default}`. A new provider id needs both `api` and `base_url` |
| | `api_key_env` | preset | Environment variable holding the key |
| | `api_key_command` | unset | Shell command whose output is the key |
| | `api_key` | unset | The key itself |
| | `headers` | preset | Extra headers on every request; values may contain `${VAR}` |
| | `auth` | `auto` | `auto` (what the wire API prescribes), `bearer`, or `api-key` for an `api-key:` header (Azure resource keys) |
| | `discovery` | preset, else `auto` | How models are listed: `auto`, `ollama`, `lm-studio` or `off` |
| | `extra_body` | `{}` | Members written into every request body (routing preferences, vendor switches) |
| | `region`, `profile` | unset | AWS settings, for `bedrock` |
| | `project`, `location` | unset | Google Cloud settings, for `vertex` |
| | `oauth`, `oauth_headers` | preset | A sign-in instead of a key; see [models and logins](models-and-logins.md) |
| | `account` | the default account | Which stored account of the provider to use; see [several accounts](models-and-logins.md#several-accounts) |
| `[presets.<name>.roles]` | `<role>` | none | A saved set of role chains that `/preset <name>` and `--preset <name>` apply; see [presets](models-and-logins.md#presets) |
| `[models."<provider>/<id>"]` | `name` | catalogue | Display name |
| | `context_window`, `max_output` | catalogue | Token limits; unknown models get 128000 and 8192 |
| | `vision`, `reasoning` | catalogue | Capabilities; unknown models get `false` |
| | `thinking_levels` | every level | The thinking levels the model accepts, for example `["low", "medium", "high"]` |
| | `cost` | catalogue | `{ input, output, cache_read, cache_write }` in USD per million tokens |
| `[agents.<name>]` | `model` | definition | Chain (a list) replacing the agent definition's; see [team](team.md) |
| | `thinking` | definition | Thinking level replacing the definition's |
| `[retry]` | `max_attempts` | `3` | Tries per model before moving down the chain |
| | `cooldown_secs` | `300` | How long a failed model is skipped |

## Behaviour

| Table | Key | Default | Meaning |
|---|---|---|---|
| `[permissions]` | `mode` | `"ask"` | `plan`, `ask`, `accept-edits` or `auto`; see [permissions](permissions.md) |
| | `allow`, `ask`, `deny` | `[]` | Rules such as `"shell(cargo build:*)"` |
| | `additional_dirs` | `[]` | Directories outside the project agents may work in (relative to the project root) |
| | `destructive` | `"ask"` | What a command that destroys work (`git reset --hard`, `rm -r ~`) needs in every mode: `ask`, `allow` or `deny`; see [permissions](permissions.md#destructive-commands) |
| `[checkpoints]` | `enabled` | `true` | Snapshot the project before each prompt; see [sessions and checkpoints](sessions-and-checkpoints.md) |
| | `max_files` | `100000` | Checkpoints turn off for projects with more files than this |
| `[compaction]` | `enabled` | `true` | Automatic context compaction |
| | `reserve_tokens` | `16384` | Summarise when the context is within this many tokens of the window |
| | `keep_recent_tokens` | `20000` | Roughly how much recent context is kept verbatim |
| | `keep_tool_results` | `8` | Newest tool results that are never elided |
| | `micro_threshold` | `0.6` | Fraction of the window at which old tool results are elided |
| `[team]` | `max_parallel` | `4` | Sub-agents running at once; see [team](team.md) |
| | `max_depth` | `2` | How deep agents may spawn agents |
| `[verify]` | `commands` | `[]` | Check commands; empty means auto-detect; see [goals](goals.md) |
| | `agent` | `true` | Run the verifier agent after the commands pass |
| | `timeout_secs` | `900` | Time limit per command |
| | `on_stop` | `false` | Also run the commands when an ordinary prompt changed something |
| `[goal]` | `max_iterations` | `8` | Failed verifications after which a goal fails |
| | `max_nudges` | `3` | Idle reminders before a goal is paused |
| `[tools]` | `disabled` | `[]` | Tool names withheld from every agent; see [tools](tools.md) |
| | `max_result_bytes` | `50000` | Tool output beyond this is cut and saved as an artifact |
| | `shell_timeout_secs` | `120` | Default time limit of a `shell` call |
| | `shell` | `""` | Shell program; empty means auto-detect (`bash`, else `sh`; on Windows Git's bash, `pwsh`, `powershell`) |
| | `format_on_write` | `false` | Run the file's formatter after `write` and `edit`; see [tools](tools.md#format-on-write) |
| | `format` | `{}` | `[tools.format]`: file extension to formatter command, such as `rs = "rustfmt --edition 2024"` |
| `[[hooks.<event>]]` | `matcher`, `if`, `type`, `command`, `url`, `headers`, `prompt`, `model` | unset | See [hooks](hooks.md). `type` defaults to `command` |
| | `timeout` | `60` | Seconds |
| | `async` | `false` | Do not wait for the hook |
| | `once` | `false` | Run at most one time per session |
| | `wake` | `false` | With `async`: give the hook's output to the lead agent when it finishes |
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
| | `auto_extract` | `true` | Extract memories at session end and after compaction |
| | `auto_index` | `true` | Refresh the code index in the background |
| `[embeddings]` | `provider`, `model` | absent | Embedding endpoint; both keys are required. Absent means word search only |
| `[web]` | `search` | `"auto"` | `auto`, `brave`, `tavily`, `exa`, `searxng` or `duckduckgo`; see [web and browser](web-and-browser.md) |
| | `searxng_url` | `""` | Address of a SearXNG instance |
| `[browser]` | `path` | `""` | Browser binary; empty means look for Chrome, Chromium, Edge or Brave |
| | `headless` | `true` | Run the browser without a window |
| `[mcp]` | `startup_wait_ms` | `250` | How long the first request waits for MCP servers; see [MCP](mcp.md) |
| | `defer_threshold` | `40` | Above this many MCP tools, they are found through `tool_search` instead |
| `[media]` | `output_dir` | `"assets/generated"` | Where generated files go; see [media and sprites](media-and-sprites.md) |
| `[media.models]` | `image`, `video`, `model3d`, `audio` | `""` | Model per kind, as `<backend>:<model id>` |
| `[lsp.servers.<name>]` | `command`, `extensions` | required | Language server program and the file extensions it handles; see [LSP](lsp.md) |
| | `args` | `[]` | Arguments |
| `[tui]` | `theme` | `"dark"` | `dark`, `light` or a file in `~/.azazel/themes/`; see [the terminal UI](tui.md#themes) |
| | `favourites` | `[]` | Models `Alt+M` steps through, such as `["@default", "anthropic/claude-x"]`; `Ctrl+F` in the model picker edits the list |
| | `images` | `"auto"` | Inline images: `auto` (what the terminal supports), `off`, or a protocol to force: `kitty`, `iterm`, `sixel`; see [images](tui.md#images) |
| | `show_thinking` | `false` | Start with the model's thinking shown (`Ctrl+T` toggles it) |
| | `expand_tools` | `false` | Start with tool output shown in full (`Ctrl+O` toggles it) |
| `[editor]` | `mode` | `"emacs"` | `emacs` or `vim` keys in the input; `/vim` switches; see [Vim keys](tui.md#vim-keys) |
| | `command` | `""` | Program `Ctrl+G` opens the draft in; empty uses `$VISUAL`, then `$EDITOR`, then `notepad` or `vi` |
| | `fold_paste_lines` | `10` | Pastes longer than this many lines fold into one token; `0` never folds |
| `[compat]` | `claude` | `true` | Also read `CLAUDE.md` and `.claude/` skills, agents and commands |
| `[notify]` | `bell` | `true` | Ring the terminal bell when an agent needs you or a long run ends, while the terminal is not focused; see [attention](tui.md#attention-title-and-progress) |
| | `desktop` | `"auto"` | Desktop notification at the same moments: `auto`, `terminal`, `system` or `off` |
| | `title` | `true` | Show the session and its state in the terminal's title |
| | `progress` | `true` | Show working, waiting and failed in the terminal's tab (OSC 9;4) |
| | `min_secs` | `20` | A finished or failed run notifies only when it took at least this many seconds |
| | `keep_awake` | `true` | Keep the machine from sleeping while agents work |
| `[update]` | `check` | `true` | Look for a newer release at most once a day |
| | `auto` | `false` | Also download a newer release in the background and install it when the UI exits; see [updating](getting-started.md#updating) |

## Trust

A project's files can make azazel run programs. Until you trust a project, the parts that could do so are held
back. A project needs a trust decision when it has any of:

- files in `.azazel/extensions/`
- `.azazel/mcp.json` or `.mcp.json`
- one of the gated settings in `.azazel/config.toml` or `.azazel/config.local.toml`
- `.azazel/models.yml`
- a project skill with `hooks` in its frontmatter

Gated settings: `hooks`, `providers`, `permissions.allow`, `permissions.destructive`, `lsp`, `verify.commands`,
`tools.shell`, `browser.path`, `editor.command`.

The interactive UI asks once per project, before it starts: `Trust this project? Its config can run commands.
[y/N]`. The answer is stored in `~/.azazel/trust.json`. In the UI, `/trust` shows the decision and what it
covers, and `/trust yes` or `/trust no` changes it; the change applies from the next start.
Print mode, RPC mode and the subcommands never ask: they treat an undecided project as untrusted unless you pass
`--trust`, which trusts it for that run only.

In an untrusted project:

- gated settings in the two project config files are dropped; everything else in them still applies
- the project's `models.yml` is not read
- project extensions, project MCP servers and skill hooks are not loaded
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
| `--thinking <LEVEL>` | `off` (default), `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| `--mode <MODE>` | Permission mode: `plan`, `ask`, `accept-edits`, `auto` |
| `--output <text\|json>` | Output of print mode; default `text` |
| `-c`, `--continue` | Continue the most recent session of this project |
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
| `--no-extensions`, `--no-skills`, `--no-hooks`, `--no-context-files`, `--no-mcp`, `--no-memory` | Leave that kind of addition out of this run. `--no-context-files` also leaves out [rule files](rules.md) |
| `--bare` | All six `--no-*` switches |
| `--max-turns <N>` | Requests the lead may make per prompt (`[limits] max_turns`) |
| `--max-cost <USD>` | Stop every agent once the run has spent this much (`[limits] max_cost_usd`) |
| `--tier <TIER>` | [Service tier](models-and-logins.md#service-tiers) of the requests: `priority`, `standard`, `economy` |
| `--preset <NAME>` | Use the models of a saved [preset](models-and-logins.md#presets) for this run |
| `--cwd <DIR>` | Work in this directory instead of the current one |
| `--trust` | Trust this project's configuration for this run |
| `--api-key <KEY>` | Key for the provider of the first `--model`, for this run only |
| `--debug` | Write debug-level logs to `~/.azazel/logs/azazel.log` |
| `--version`, `--help` | Print the version or the usage |

`--model`, `--thinking`, `--mode`, `--output`, `--cwd`, `--trust`, `--session-dir`, `--add-dir`, `--set`,
`--tier`, `--preset` and `--max-turns` may also follow a subcommand, as in `azazel goal "make the tests pass" --mode auto`.

A wrong flag value (an unknown tool, a model name that matches nothing or several models, a bad `--set`, an
unknown tier or preset) exits with status 2.

Subcommands: `init`, `doctor`, `login`, `logout`, `update`, `completions` ([getting started](getting-started.md)),
`changelog` ([getting started](getting-started.md)), `models`, `auth status` ([models and logins](models-and-logins.md)),
`sessions`, `export`, `stats` ([sessions](sessions-and-checkpoints.md)), `goal`
([goals](goals.md)), `mcp` ([MCP](mcp.md)), `pkg`, `skills` ([packages](packages.md)), `memory`, `index`, `rag`
([memory](memory.md)), `ext types` ([extensions](extensions.md)) and `rpc` ([RPC mode](rpc.md)).
