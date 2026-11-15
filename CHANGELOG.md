# Changelog

Every release of azazel, newest first. `azazel changelog` shows this file, and each release's notes on
GitHub are its section here.

## [Unreleased]

### Added

- Language servers install themselves: when a file's default server (rust-analyzer, typescript-language-server, pyright, gopls) is missing, azazel installs it into `~/.azazel/lsp/` on first use and starts it. `[lsp] install = "ask" | "auto" | "off"` (default `ask`, trust-gated) decides whether it asks first; with nobody to ask, the tool result gives the command. New `azazel lsp list` and `azazel lsp install <server>`, and `azazel doctor` names the missing servers of the project's languages with the command that installs them.
- Ghost text in the input: while you type prose, the rest of the word is shown dim, from your earlier prompts, the run and the project's file names. `Tab` accepts it with a space, `Right` without. Turn it off with `tui.autocomplete = "off"` (Input page of `/settings`).
- `tui.predict_next = true` asks the session's model after each finished turn what you will say next and shows it as ghost text in the empty input; `Tab` or `Right` accepts it. It costs one more request per turn, shown in `/cost` as `prediction`; off by default.
- `azazel schedule add --on pr-comment "<prompt>"` runs an agent when someone writes `@azazel …` (the handle is `[triggers] handle`) on an open pull request of the repository, and `--on slack:<channel id>` does the same for a message that mentions the Slack bot. Each mention runs in a background session and a git worktree of its own (`[triggers] mode`, `accept-edits` by default), never pushes, and is answered on the pull request or in the Slack thread with the agent's last message and the commits it made. Only the user `gh` is signed in as and users with write access (`[triggers] github_authors`), or the Slack user ids in `[triggers] slack_users`, can start a run. See [comments and messages](docs/background-and-remote.md#comments-and-messages-as-triggers).
- The repository map: the `repo_map` tool and `/repomap` give an outline of the project's most referenced definitions (Rust, Python, Go, JavaScript, TypeScript), ranked by how the code refers to them and fitted to a token budget. `[context] repo_map_tokens` puts it in the lead's system prompt (off by default). Built with `--features repomap`; the grammars add 5.3 MB, so it is not in the default build.

### Changed

- The `Resume this session with` / `azazel -r <id>` lines printed on exit are in the theme's muted colour, and the command is no longer indented.
- `/settings` has nine pages: the status line settings moved from a page of their own to a group at the end of Appearance.

## [0.9.1] - 2026-11-15

### Fixed

- `azazel acp` exits with status 0 when the editor closes the connection while azazel is still writing to it; it printed `the ACP connection failed: … Broken pipe` and exited with 1.

## [0.9.0] - 2026-11-15

### Added

- Leaving the terminal UI prints how to come back to the session (`Resume this session with` / `azazel -r <id>`), once the session holds a conversation and is kept on disk.
- `azazel mcp import jcode` copies the MCP servers of jcode (`~/.jcode/mcp.json`, or `$JCODE_HOME`, and `<project>/.jcode/mcp.json`), reading its `servers` key and its `timeout_secs`. Every client's `timeout_secs` now becomes `timeout`.
- `local://<name>` files: a scratch directory of the session, shared by the lead and its sub-agents. `read`, `write`, `edit`, `patch`, `glob` and `grep` take `local://` addresses, `read local://` lists them, and writing there is never asked about. `write`, `edit` and `patch` now refuse other schemes with a clear message instead of writing to a mangled path.
- The `eval` tool runs Python (`python`, `python3` or `py` on `PATH`) or JavaScript (`node`, `bun` or `deno`) the agent writes, in the project directory, approved like a shell command; `AZAZEL_LOCAL` names the `local://` directory to the program. `[tools.eval] python` and `javascript` (both on; also on the Tools page of `/settings`) turn each language off; with both off, `eval` is not offered.
- While sub-agents work, a header above the input counts them (`Agents · 2 running`, with the key of the agent hub), with a row each for the first three (definition, current tool, time, tokens) and `… N more` for the others.
- `Left` twice on an empty input opens the agent hub. The hub lists every sub-agent of the session, finished ones and those of earlier runs included, with parent, cost and task. `Enter` opens the agent's chat: its whole transcript from its file, followed live, with a line to message it; `Esc` or `Left` twice goes back. `x` stops the agent, `f` follows it in the transcript.
- `tui.todo_max` (default 8; Appearance page of `/settings`): a long todo list above the input folds the finished items into one row (`✓ 11 done`), shows the item in progress and the next ones, and counts the rest (`… 3 more`); `0` hides the list.
- `agent_status` (and `agent://<id>/status`, `agent://all/status`) tells at once what an agent is doing, without asking a model: state and time, the tool it runs now and for how long, its last lines, the files it changed, an isolated agent's uncommitted changes and commits, tokens and cost.
- `agent_send` with `ask: true` asks an agent a question on the side, like `/btw`: it is answered in seconds from the agent's conversation on its own model, the agent keeps working and nothing is added to its transcript. The cost counts to the agent that asked, under `btw` in `/cost`.
- `agent_send` says how the message reached each agent: `delivered at its next step`, `woke it` or `queued` (a message to an agent still waiting to start now goes with its prompt instead of failing).
- A tool card appears as soon as the model starts writing the call: its header fills in with the path, command or pattern as the arguments arrive, and a `write` or `edit` shows the last lines of the text being written. The same holds for the sub-agent you follow and in an agent's popup.
- `tui.complete_ignored` (also under `/settings` → Input): `@` offers files that `.gitignore` leaves out, such as `ref/`; `.git/`, `target/` and `node_modules/` are still skipped. Off by default, as before.

### Changed

- The `minimal` status-line preset counts sub-agents at work. In the agent hub, `Enter` opens the agent's chat instead of following it; following is `f`, stopping is `x`.
- `Ctrl+O` now expands or collapses the output of every tool call on screen, not only of calls that finish afterwards: the screen and scrollback are erased and the last 300 things written are drawn again. Inside tmux, screen or zellij it works as before.
- Tool cards that follow one another are set apart by a blank row; compact rows still stand together.
- The lead may push again when you ask it to (`git push`, `gh pr create`, …); the push guard now holds for sub-agents only, whose commands and processes are still refused any push in every mode.

### Fixed

- What you type while the lead waits in `task` or `agent_wait` reaches it at once: the wait ends, the result names the agents still running, and their reports arrive later as messages. Before, the message waited until every agent had finished.
- A message that reaches an agent just as it finishes its turn gets a turn of its own instead of waiting for the next prompt.
- No more band of blank rows between the transcript and the input after the `/` or `@` menu closes (or a tall input, the agents roster or the todo list shrinks): the menu now opens over the last transcript rows and gives them back when it closes.
- Making the window taller or changing its width no longer leaves a gap above the input: once the window has settled, the transcript is drawn again at the new size, as `Ctrl+O` does. Inside tmux, screen and zellij, which keep what is erased, the old behaviour stays.

## [0.8.0] - 2026-11-14

### Added

- The mouse works in popups: move over a row to highlight it, click to select, click the selected row (or
  tick a check box) to pick it, scroll with the wheel; in `/settings` a click on a tab opens its page. The
  terminal reports the mouse only while a popup or the transcript viewer is open, so its own selection and
  scrolling work everywhere else. `tui.mouse = false` (or `/settings` → Appearance) leaves the mouse to the
  terminal.
- RPC: `list_roles`, `set_role`, `save_preset` and `apply_preset` list the model roles with where each value comes from (built-in, home, project, session) and change them for the session or in a config file.
- `/models` (and `/model` alone): one screen for the model of every role. It lists the roles with where each value comes from, their fallbacks, thinking levels and place in the `Ctrl+P` cycle, beside the models of every provider with their context window, output limit, price and what they can do. `Enter` on a model makes it a role's model or fallback, then offers the levels it thinks at; in the Roles view `t` sets the thinking level, `f` adds a fallback, `x` clears, `[` `]` reorder, `c` puts a role in the `Ctrl+P` cycle, `n` makes a new role, `s` and `l` save and load presets, and `w` saves to the project's config instead of the home one. Typing searches every provider at once, providers are asked for their current models in the background (`F5` asks again), and a provider without a credential can be signed in to or given a key from the screen. It works for sessions in another process too, and with the mouse.
- RPC: `list_providers` lists every provider with whether it can be used and how; `refresh_models` asks one for its current models; `list_models` takes `all` for the models of every provider, and its rows carry the provider, the id, the name, the thinking levels, whether the provider can be used and whether it is a subscription.
- `azazel roles` lists every model role with its chain, the model it resolves to, its thinking level and where
  it is set. `azazel roles set <role> <model> [fallback…] [--thinking LEVEL] [--project]`, `azazel roles clear
  <role>` and `azazel roles preset save|load <name>` change them in the home or project config; `--json` on each.
- `[session] resume = "last"` (or `/settings` → Sessions → Open at start) makes the interactive UI open the
  project's most recent session, as `-c` does; `--new` starts a new one anyway. Print mode is not affected.

### Changed

- Model lists write a context window of 1.5 million tokens as `1.5M`, not `1500k`.
- `/settings` → Models: a role's row, and the new row `Models for the lead and each agent`, open `/models`; the provider-then-model prompts are gone. After the first sign-in with no default model, `/models` opens on that provider to choose one. Without the screen (a session in another process), a role's models are typed.
- `/model` alone opens `/models`; `Ctrl+L` and `Alt+P` still open the quick picker for this session's model.
- A thinking level chosen for the session (`/thinking`, `Alt+T`, `--thinking`, RPC `set_thinking`, an agent's
  `thinking`) now wins over the `:level` of the role's model, so `/thinking low` lowers a lead whose role is
  written `:high`; without one, the role's level applies. The status line and `get_state` show the level
  requests go out at.
- `/thinking` offers only `off` and the levels the model takes, starting on the one in use; a typed level the
  model lacks is set to the nearest one it takes, with a note.
- `azazel models` heads the models per provider with where its credential comes from (`no credential` with
  `--all`), and has columns for the price (`subscription`, `free`), the thinking levels, vision and the roles
  that use each model. `--json` adds `name`, `thinking_levels`, `roles`, `login` and `usable`.

### Fixed

- Models now know their thinking levels: the catalogue takes the efforts models.dev lists for each model (4,338 of 7,429 models), and ChatGPT's own model list says which efforts each model takes. `/thinking`, `alt+t` and `/models` offer only those levels; before, the catalogue named none, so every level was offered. A provider's model list that names no levels no longer hides the catalogue's.
- `/models` kept its selection on a model only until a provider's model list arrived; a list that grew or shrank moved the selection onto another model.
- `--set` takes quoted key parts as TOML does, so `--set 'models."openai/gpt-5.5".context_window=400000'` reaches that model; the quotes were kept in the name before and the setting did nothing. An unclosed quote is an error.
- A reply that Anthropic's safety system stops (`stop_reason: refusal`) ended the turn with nothing on screen. It is now an error that gives the provider's reason and explanation, for example `the model declined to answer (cyber): Cyber safeguards flagged this message…`; it is not retried, and no other model is asked on its own.

## [0.7.0] - 2026-11-14

### Added

- Primary agents: the lead is one of several personas. `build` is the lead as before; `plan` investigates
  read-only, writes its plan under `.azazel/plans/` and hands over to `build` once you approve the plan.
  `tab` in an empty input switches between them, `/agent [name]` picks one, `--agent <name>` starts a run
  as one, and `[agents] default` sets the one sessions start as. A definition of your own with `mode: primary`
  (or `all`) is one more: its body replaces the lead's identity, and its `tools`, limits, `model`,
  `thinking`, `skills` and `color` apply to the lead while it is selected. The status line has an `agent`
  segment in its colour, the input's rules take the colour too, and the switch is kept in the session.
  RPC clients switch with `set_agent`, ACP clients with the session option `agent`.
- About 160 more built-in providers: every provider models.dev lists that speaks a wire API azazel has
  (`anthropic-messages`, `openai-responses`, `openai-chat`, `google-generative`), with its models in the
  catalogue. Set its key variable and its models appear in `/model` and `azazel models`. Each reads a
  variable of its own; the list, with addresses and variables, is in `docs/models-and-logins.md`.
  `cargo xtask catalog` writes these rows; a hand-written provider with the same id replaces one.
- `google` also reads its key from `GOOGLE_GENERATIVE_AI_API_KEY`.
- More sign-ins: `azazel login openai-codex --device` signs in to ChatGPT on a machine without a browser
  (enter a code on any device); `azazel login github-copilot --enterprise <domain>` signs in through a
  GitHub Enterprise and sends requests to its `copilot-api.<domain>`; `azazel login digitalocean`,
  `azazel login poe` and `azazel login snowflake` sign in in the browser (Snowflake beside
  `SNOWFLAKE_PAT`, keeping the account with the login). `/login` takes `--device` and `--enterprise` too.
- Azure through the Azure CLI: `[providers.azure] auth = "azure-cli"` sends a Microsoft Entra token from
  `az account get-access-token` instead of a key, and the model list of `azure` is now the resource's
  deployments (Azure Resource Manager with the Azure CLI, the resource's `/openai/deployments` with a key).
- Tool calls are mended before they run: a tool called by another harness's name (`bash`, `str_replace`,
  `apply_edit`, `grep_search`, `list_dir`, `ls`, `todowrite`) or in another case runs as the real tool, and
  an object or array sent as a JSON string, a number or boolean sent as a string, `null` for an optional
  field and an unknown field of a closed input are put right against the tool's schema. The result gets one
  `note:` line saying what was mended, and the session file keeps it with the call.
- A `patch` tool for OpenAI GPT and Codex models: one patch in the `*** Begin Patch` format adds, changes
  (with `@@` hunks), moves and deletes files. Every file is checked before any is written, permission is
  asked as for `write` for each file, and each file goes through the read guard, format on write, checks on
  write and the language server. `[tools] patch = "auto"` offers it in place of `edit` to those models;
  `"on"` offers both to every model, `"off"` none.
- Format on write knows 26 formatters and uses the one the file's project uses: `gofmt`, `rustfmt`, `zig
  fmt`, `terraform fmt`, `shfmt`, `dart format`, `mix format`, `gleam format`, `nixfmt`, `ktlint`,
  `cljfmt`, `dfmt`, `latexindent`, `htmlbeautifier` and `ormolu` when installed; `prettier` and `oxfmt`
  when `package.json` depends on them (the project's `node_modules/.bin` copy first), `biome` with a
  `biome.json`, `clang-format` with a `.clang-format`, `ocamlformat` with an `.ocamlformat`, `ruff format`
  with a ruff config or dependency, `uv format` with a `uv.lock`, `standardrb` or `rubocop` from the
  `Gemfile`, `pint` from `composer.json`, and `air` for R. Those that run the project's code wait for the
  project to be trusted. `azazel doctor` lists the formatters it would use for the project.
- `/open` (and `azazel open` in the shell) lists the recent sessions of every project, newest first, each with
  its directory; opening one of another project starts azazel again there, in the same terminal, with that
  project's trust, configuration and context files.
- `/cd <dir>` goes on in another directory, with `Tab` completing directory names; a directory of another
  project moves the session into that project's sessions.
- `/worktree [name]` makes a git worktree under `.azazel/worktrees/` on a new branch `azazel/<name>` from HEAD,
  moves the uncommitted changes into it (through `git stash`) and goes on there; `/worktree list` and
  `/worktree remove <name> [--force]` manage them.
- `azazel sessions import <file|URL> [--project <dir>]` makes a session from a session file or a JSON lines
  export, from disk or an `https://` address.
- `azazel export --sanitize` and `/export --sanitize` hide secrets in every format and replace what files held
  (the results of `read`, `grep`, `edit` and the like, and the text given to `write`) with
  `[file content removed]`.
- Print mode takes `-f/--file <path>` (up to 100: text inlined, images attached), `--worktree[=name]` (the run
  happens in a new worktree, whose path is printed at the end) and `--archived` (the session stays out of
  `/resume`, `/open`, `-c` and `azazel sessions list`, which lists it with `--all`).
- `{env:VAR}` and `{file:path}` in any string value of `config.toml`, `models.yml` and `mcp.json`, filled
  once when the file is read: `api_key = "{file:~/.secrets/gateway.key}"`. A `{file:}` path is relative to
  the file that names it. A variable that is not set or a file that cannot be read stops azazel with the
  file and the key named instead of becoming an empty value. A project's files read neither until the
  project is trusted.
- Client certificates per provider for gateways that require mutual TLS: `[providers.<id>.tls]` with
  `cert`, `key` and `ca`, as PEM paths or `{file:}` references. Only that provider's client uses them.
- Per-provider limits: `timeout_secs` (the whole request), `first_event_secs` and `stall_secs` in
  `[providers.<id>]` replace the `[retry]` ones for that provider.
- `{session_id}` in a provider's `headers` becomes the id of the conversation a request belongs to.
- Schedules can check before they run: `azazel schedule add --if "<command>"` runs the prompt only when the
  command holds at its due time (exit 0, or `--if-is <code>`; `--if-matches <regex>` on what it prints;
  `--if-changes` when it prints something new), and `--until "<command>"` ends the schedule once its command
  holds. Checks run before any model is asked. `--ends-after 7d`, `--ends-at <time>` and `--times <n>` end a
  schedule by itself, and `--tz <zone>` reads a cron expression or `at` time in another time zone. The
  `schedule` tool takes the same as `if`, `until`, `ends` and `tz`; the commands of its checks are asked
  about like `shell` commands.
- `azazel schedule pause <id>` and `resume <id>` (and the `schedule` tool's `pause` and `resume`) stop a
  schedule and start it again. `azazel schedule list` shows each schedule's state (active, paused, ended)
  and what its checks said the last time.
- A diff viewer: `/diff` lists the changed files on the left (what happened to each, lines added and removed)
  beside the selected file's diff with its code coloured, in one column or old beside new (`s`, and
  `tui.diff_view = "auto" | "split" | "unified"`; auto is split from 140 columns). `n`/`p` step through the
  files, `[`/`]` through the hunks, `w` wraps long lines, `/` searches every file, and `m` marks a file
  reviewed, which the session keeps until the file's diff changes. `/diff uncommitted` names the default, and
  `/diff <commit, branch, tag or range>` compares with any revision.
- Ten more themes: `dracula`, `nord`, `rose-pine`, `kanagawa`, `one` (`one-dark`, `one-light`),
  `solarized`, `everforest`, `github`, `ayu`, `monokai`, and light palettes for `catppuccin` (Latte),
  `tokyo-night` (Day) and `gruvbox`. A theme with a dark and a light palette follows the terminal's background
  like `auto`; `<name>-dark` and `<name>-light` pin one. `/theme` can be filtered by typing.

### Changed

- GitHub Copilot asks Claude models on its Anthropic Messages API and GPT-5 and later, Grok and MAI Code
  models on its Responses API, where Copilot serves them.
- `tokyo-night` now follows the terminal's background (Night on a dark one, Day on a light one); name
  `tokyo-night-dark` to keep Night everywhere. `catppuccin-mocha` and `gruvbox-dark` are as before.

- In an agent definition `mode` now says what the definition is (`subagent`, `primary` or `all`); keeping an
  agent in plan mode is `permission-mode: plan`. A definition that still says `mode: plan` is listed by
  `/agents` with that message. `azazel migrate` writes `permission-mode`.
- Agents started by an agent kept in plan mode are kept in plan mode too.
- `xai` talks to xAI's Responses API, which xAI names its primary API; Chat Completions is its legacy one.
- `opencode-zen` and `opencode-go` use OpenCode's current addresses (`https://opencode.ai/inference/openai/v1`
  and `https://opencode.ai/inference/go/openai/v1`), and `minimax-code-cn` MiniMax's new mainland-China
  host, `https://api.minimax.cn/anthropic`.
- `[tools] format_on_write` is on by default: a file is formatted only by a formatter its project uses,
  and a missing or failing formatter is a note in the result, never a failed write. An empty
  `[tools.format]` command turns formatting off for that extension.
- `/open` alone opens the session picker; the last link of the last reply is now `/open link` (`/open <n>`,
  `/open <path>` and `/open diagram` are unchanged).
- Releases also have a Linux ARM64 archive (`azazel-linux-aarch64.tar.gz`); the install script picks it on
  ARM64 machines.
- `azazel update` no longer needs a GitHub token: the repository is public. A token in `GITHUB_TOKEN` or
  `GH_TOKEN` is still used when set, which lifts GitHub's limit on anonymous requests.

### Fixed

- `Ctrl+V` attaches a screenshot in Windows Terminal: with an image and no text on the clipboard the terminal
  pastes nothing and lets only the key's release through, which azazel now takes for the key. An empty
  paste from any terminal reads the clipboard for an image too. `Alt+V` works as before.
- `Ctrl+C` twice exits at once in a long session: the end of a session no longer waits for memory
  extraction, which asked a model about the whole conversation (38 s on a 5 MB imported session, even when
  nothing was prompted). A session with something prompted in it is extracted by the next session of the
  project in the background; one resumed and left asks nothing. A further `Ctrl+C` while the session closes
  ends azazel at once.
- Conversations brought over by `azazel migrate` call azazel's tools: their tool calls are renamed (`bash`
  is `shell`, Claude Code's `Read` is `read`, Codex's `apply_patch` is `patch`, ...), so a resumed session
  no longer calls `bash` and gets `unknown tool`. `azazel migrate --fix-chats` renames them in conversations
  imported earlier, keeping a copy of each file it changes. Skills, agents and commands with the name of
  one built into azazel are left out and reported as `skipped`.
- `opencode-go` and `opencode-zen` send the `x-opencode-session` header OpenCode's gateways now require; they
  refused every request with `400 MissingSessionID` before.
- In Windows Terminal the input, its rules and the status line follow the window again when it is resized,
  maximised or zoomed (`ctrl+-`, `ctrl+=`): they span the new width on the last rows instead of keeping the
  width and the row they had, and no rule or status line drawn at the old width is left above them. A
  window narrowed while a popup or the viewer is open no longer has the end of the transcript written over
  when it closes.
- A path an agent names through a symbolic link or a Windows short name (`/var/…` on macOS, which is
  `/private/var/…`; `C:\Users\RUNNER~1\…`) is checked as the file it is: a secret file, a protected file or
  a path outside the project is asked about as it is when named the usual way, and `grep` leaves secret
  files out of a directory named that way too.
- Two agents started at the same moment (from `parallel` in a script, or two `task` calls) no longer take
  the same name, which failed one of them with `cannot write …: File exists`.
- `azazel rpc` writes its frames in the order things happened: a notice given during a run (such as a
  cache miss) comes before the run's `idle`, and what a sub-agent did comes before the run of the lead its
  report starts.

## [0.6.0] - 2026-11-14

### Added

- When a subscription runs out in the middle of a run and no other account or model of the chain can take
  the request, azazel asks instead of failing: "Use the reset" (when the account holds a banked reset that
  gives back what is full), "Wait until reset" (the run waits with a countdown and goes on at the reset
  time; `esc` stops it) or "Pick another model" (the agent goes on with the model you pick). An RPC or ACP
  client gets the same choice as a question. With nobody to ask the run fails as before, unless
  `[usage] auto_reset = true` lets a banked reset be spent.
- Banked resets: `/usage` and `azazel usage` show the resets each login holds, with how many, when the
  first lapses and whether one can be spent now: the reset credits of a ChatGPT plan, and the session
  reset and the weekly resets of a Claude subscription. `/reset-usage [provider]` spends one after showing
  what it gives back. A redemption is written to `~/.azazel/resets/` before it is sent, is sent once and
  never again by itself, and an answer that does not come is reported as "not known", not as "not spent";
  the next usage report settles it. See `docs/models-and-logins.md`.
- `azazel migrate [--from pi|omp|jcode|claude|codex|all] [--only <kinds>] [--dry-run] [--overwrite]
  [--project <dir>]` and `/migrate` bring over what another harness holds on this machine: providers,
  models, roles and fallbacks (to `models.yml` and `[roles]`), API keys (to `auth.json`), skills, custom
  commands (placeholders converted), agent definitions (frontmatter mapped), MCP servers of the user and the
  project, rule and context files azazel does not read where they are, settings that have an equivalent,
  the conversations of every project (as sessions named `imported from <source>: <title>`, with thinking,
  images, tool calls and results, the model of each reply, compactions and sub-agents' transcripts) and
  memories. The source is only read; nothing that is there is replaced without `--overwrite`; sign-ins are
  never copied but listed with the `azazel login <provider>` to run, and a key of a provider the source
  signs in to is left out, so that nothing is billed to a key by surprise; a second run skips what is
  already there. A report lists everything imported, skipped and lost. `/migrate` shows the harnesses found with
  their counts, lets you untick sources and kinds, and shows the plan of a dry run before it writes. See
  `docs/migrating.md`.
- `/settings` is one screen of ten pages behind a row of tabs (Accounts, Models, Appearance, Status line,
  Input, Permissions, Tools, Sessions, Usage, Advanced) and reaches every configuration key. `Tab` and the
  arrows change pages, `/` searches all of them, `r` puts a setting back on its default, and `p` saves to the
  project's config instead of the home config in a trusted project.
- Each settings row shows the value in effect, a mark when it is not the default, and the layer it comes from
  (`home`, `project`, `local`, `env`, `flag`); the tabs count what is changed on each page, and the selected
  setting is described under the list with its key and default.
- Settings are edited in place: switches flip, choices open a list that shows each value in the running UI
  while it is being chosen (theme, symbols, indicator, tool call style), numbers and texts are typed, and
  lists and tables (favourite models, permission rules, formatters, checkers, language servers, account order,
  agent overrides) have an editor that adds, changes, removes and reorders entries.
- Images in the draft: `Ctrl+V` and `Alt+V` take what is on the clipboard. An image is attached and shows as
  a chip, `[Image #1]`, where the cursor was; copied files and text that is nothing but paths of image files
  attach those images (other files become `@path` mentions); other text is pasted. The chip is one character
  to the cursor and the delete keys, the numbers always run from 1 without gaps, undo brings a deleted image
  back, and the prompt is sent with the labels in the text and the images in that order. Where the system
  clipboard cannot be read directly, PowerShell (Windows, WSL), `osascript` (macOS) or `wl-paste`/`xclip`
  (Linux) is asked. BMP images are converted to PNG. `Ctrl+Shift+V` and `Alt+Shift+V` (`paste_text`) paste
  the clipboard's text as text.
- Pasted paths: one path a line, quoted paths and Windows paths (`C:\...`, read as `/mnt/c/...` under WSL)
  are recognised as dropped files.
- `/terminal-setup`: says whether `Shift+Enter` adds a line in the terminal azazel runs in, and where it does
  not, adds the key binding that makes it to VS Code's `keybindings.json`, Windows Terminal's `settings.json`
  (from WSL) or `alacritty.toml`, after asking and with a copy of the file kept beside it; for kitty, WezTerm,
  Ghostty and iTerm2 it shows what to put in.
- `/keys` reports key conflicts under the bindings: keys the key bindings file gives to two actions, keys the
  terminal keeps for itself (`alt+enter` in Windows Terminal, `ctrl+j` in VS Code, the tmux prefix), and keys
  a terminal without enhanced key reporting cannot tell apart.
- `cargo xtask docs-check` compares the docs with the code and fails with one line per difference, naming
  the page and the name: configuration tables and keys with their defaults, trust-gated settings, hook
  events, keymap actions and their default keys, flags, subcommands and their actions, RPC commands and
  frames, the Luau API, slash commands and tools, in both directions. It also checks every `azazel …`
  command line the docs show, that every page is linked from `docs/README.md`, that relative links lead to
  a file and a heading that exist, and that `docs/rpc.schema.json` is current. CI runs it after the tests.
- `AGENTS.md` at the root of the repository, the guide for people and coding agents who change azazel: the
  layout of the workspace, the commands that must pass, how to try a change with a scratch home and the
  scripted provider, the rules for tests and docs, the checklist for adding a feature and the steps of a
  release. `CLAUDE.md` points to it.
- `/restart` starts azazel again in the same terminal on the session that is open, with the options it was
  started with and the model, thinking level and permission mode it is on now: a running session gets onto a
  new version after `azazel update` without losing the conversation. Background sessions and sessions over SSH
  are attached to again.
- `/fix` repairs a conversation the provider refuses: tool calls left without a result are answered as
  interrupted, tool results that answer no call and empty or invalid blocks and messages are dropped, and
  what changed is listed. The repaired conversation is a new branch; the old one stays in the session.
- `[tui] queue_mode = "all"` sends every message queued with `Alt+Enter` as one prompt when the agent
  finishes, instead of one per run (`"one"`, the default). It is on the Input page of `/settings` and applies
  at once.
- Branch summaries: going back with `/tree` has the `smol` role write up in a few lines what happened on the
  branch you left, and adds that to the conversation where it goes on, so the agent knows what was tried
  there. `[session] branch_summary = false` turns it off.
- A muted line says when the lead's prompt cache ran out while the session was idle, with what the next
  request costs because of it (`about $1.58 instead of $0.13` where the model is paid for by the token).
- `[tui] theme = "auto"` (also `/theme auto` and `--theme auto`) follows the terminal's background: `light`
  on a light one, `dark` on a dark one or when the terminal does not say. The terminal is asked once, at the
  start (OSC 11).
- `/copy last <n>` copies the last `n` messages of the conversation, yours and the agent's, as Markdown.
- `/debug` shows where azazel keeps its files, the state of the UI and the session, the terminal's variables
  and the last 200 lines of the log, full screen.
- `docs/configuration.md` documents `[limits]` (`max_cost_usd`, `warn_cost_usd`, `max_turns`), the role of
  each built-in agent with its default, and `--continue-run`; every subcommand is in a table with its
  actions.
- The status line is made of segments you choose: `mode`, `model`, `thinking`, `tier`, `path`, `branch`,
  `context`, `bar`, `rate`, `cost`, `usage`, `agents`, `goal`, `todo`, `queue`, `session`, `time`,
  `git_dirty`, `account` and `cache`. `[statusline]` takes a `preset` (`default`, `minimal`, `powerline`,
  `full`), your own `left` and `right` lists, a `separator` (`dot`, `bar`, `space`, `powerline`) and options
  per segment under `[statusline.segments.<name>]` (`priority`, `icon`, `max`, `style`, the clock's `format`
  and `seconds`, the bar's `cells` and `min_width`). `powerline` draws every segment as a block of colour,
  ended by an arrow with Nerd Font symbols. On a narrow terminal the segment with the lowest priority leaves
  first, on either side.
- `/statusline` sets the status line up in a popup: a preset, a separator, and each segment switched on or
  off, moved to the other side and reordered, with the resulting line shown above the list and on the status
  line itself; `Enter` saves it to `~/.azazel/config.toml`. `/statusline <preset>` takes a preset at once. The
  Status line page of `/settings` applies its changes to the running UI.
- `statusline.command` (home config or a trusted project): a program that is given the session as JSON on its
  standard input (the model, the directory, the session, the context window and the cost, under the names
  Claude Code uses, plus the mode, the thinking level and the branch) and whose first line of output, colours
  kept, replaces the left side of the status line. It runs when the session changed, at most once a second,
  and is ended after two seconds; when it fails the segments come back.
- `Ctrl+P` puts the lead on the next favourite model or role and `Ctrl+Shift+P` or `Alt+Shift+P` on the
  previous one, and the cycle shows above the input for two seconds with the current entry marked. Favourites
  may be roles (`@smol`): a step onto a role makes that role's chain the lead's. With no favourites the keys
  step through the roles that were given models. `Alt+M` and `Alt+Shift+M` still work.
- `Alt+P` opens the model picker (`Ctrl+L` and `/model` still do). It lists the roles first, then the
  favourites, then the models chosen lately, then every other model, and opens on the row of what the lead is
  on.
- `--models a,b,c` names what the cycling keys step through for one run.
- `/usage` is a box in the middle of the window: per provider and account every usage window with a bar, the
  share used, the reset time and what it counts, the plan, extra usage and what it cost, banked resets, the
  account in use, and until when an account is left alone. `r` asks the providers again. `azazel usage`
  prints the same box in a terminal.
- Usage windows of single model families: Anthropic's `limits` rows (`weekly_scoped`, such as the weekly
  allowance of the Fable models) and its `7d_oi` headers, and ChatGPT's `additional_rate_limits`, are read
  and shown (`7d Fable`). Every other window the usage endpoints return is shown too.
- A request no longer starts on an account that is known to be out of allowance for its model when another
  stored account has room: the usage of every stored subscription login is fetched in the background while a
  session is open (`[usage] poll_secs`), and accounts that ran out stay marked across runs in
  `~/.azazel/cache/limits.json` until their reset, a successful request or a usage report with room.
- `[accounts] order` sets the order in which work moves through the accounts of a provider.
- `[usage] redact_email` (on by default) shows e-mail addresses as `j***@example.com` in `/usage`,
  `azazel usage` and `/account`.

### Changed

- An account that ran out of the allowance of one model family (Fable) is left alone for those models only; it
  keeps answering for the others. The line about moving to another account names that window
  (`7-day Fable limit`), and the status line shows the fullest window that counts the lead's model.
- `azazel usage --json` has `in_use`, `spend`, `resets`, `blocks` and, per window, `label` and `scope`; its
  `email` is hidden like everywhere else unless `usage.redact_email = false`. The plain output is laid out
  like the box.
- `get_usage` (RPC): windows carry `scope`, accounts carry `blocks`.
- A full usage window of an account with extra usage on and money left is no longer shown as refusing
  requests.
- The binary is smaller with every feature kept (the Windows build by 8.8 MB, 16%). What changed
  underneath: one TLS library instead of two (rustls on `ring` for every HTTPS connection, Bedrock
  included; aws-lc is gone), dependencies built for size except where the time goes (drawing, Markdown and
  highlighting, regular expressions, JSON, the async runtime, the allocator), the model catalogue, the
  built-in skills and the changelog stored compressed and unpacked when first needed, and SQLite built
  without the modules nothing uses. Start-up is unchanged.
- HTTPS no longer offers the post-quantum hybrid key exchange (X25519MLKEM768), which only aws-lc
  provided. TLS 1.2 and 1.3, the system's certificate store and proxy settings work as before.
- `/settings`: signing in, adding an API key and signing out are on the Accounts page, the default model and
  the model of every role on the Models page. A value set back to its default is taken out of `config.toml`
  instead of being written. Where there is no screen (a session in another process, RPC clients) the same pages
  are a chain of menus.
- Choosing a theme in `/settings` no longer writes a `theme:` line into the transcript for each theme tried.
- `Ctrl+Enter` adds a line, like `Shift+Enter` and `Ctrl+J` (`newline`).
- A change of the window's size (dragging, or zooming the font) is taken in once the size has been still for
  60 ms and drawn once. The live region grows with a taller window, so the input stays on the last rows; on
  Windows it is placed from where the console has its cursor, which is exact. The image row above the input
  (`[N image(s) attached to the next prompt]`) is gone: the chips in the draft say it.

- The built-in skill `azazel-extensions` describes the current version: the hook events `file_changed`,
  `config_change`, `worktree_create`, `worktree_remove` and `setup`, the hook options `once` and `wake`,
  `output-schema` in agent definitions, `allowed-tools` in skills, npm packages, and the whole Luau API
  (`azazel.wrap`, `azazel.prompt`, `azazel.router`, `azazel.classify`, `azazel.session`, `azazel.tools.call`,
  `replaces`).
- `docs/tools.md` lists the `report` tool and `tools.redact_output`; `docs/extensions.md` lists
  `azazel.fs.data_dir` and `before_agent_start` in their tables.

### Fixed

- Typing a whole command such as `/mode` and pressing Enter runs it, instead of completing to a longer
  command that starts with it (`/model`).
- After the window got taller (or the font smaller) the input and the status line no longer stay above the
  bottom of the window.
- On Windows the size taken after a resize was one column and one row too large, so lines were cut one column
  late and wrapped.
- After the window got narrower in a terminal that re-wraps its content, part of the old input box (a rule,
  a blank row) no longer stays behind in the transcript.
- The transcript viewer lays its text out once after a drag instead of at every size on the way.
- An answer of the terminal to what azazel asks it at the start (the size of a cell, what it can draw) that
  was still arriving when the 80 ms were over no longer ends up in the input as typed text
  (`6;7;21;22;23;24;28;32;42c`): an answer that has begun is read to its end.
- On Windows `azazel update` no longer fails with `cannot move ... aside` when the binary an earlier update
  retired is still in use by an azazel that keeps running (as after `/restart`): the binary is retired under
  another name.
- After requests moved to another account, the status line shows that account's usage window at once, not
  the window of the account that ran out.
- What Luau extensions add to the terminal UI is shown and runs; until now it was stored and never used.
  `azazel.status` texts are the new status-line segment `extensions` (in the presets `default`, `powerline`
  and `full`; `/statusline`, `statusline.left` and `statusline.right` place it like any segment),
  `azazel.widget` lines stand above the input, and a key bound with `azazel.shortcut` runs its handler when
  azazel has no action for the key. `/keys` lists these keys with their scripts. See "In the terminal UI"
  in `docs/extensions.md`.

## [0.5.0] - 2026-11-13

### Added

- `run_code` scripts orchestrate: `agents.run(agent, prompt, { model, isolation, schema })` starts an agent
  as `task` does and returns its report, or with a `schema` the value it reported; `parallel{ f, g, ... }`
  runs functions at the same time (at most `team.max_parallel`) and returns their results in the same
  places, so tool calls and agents run side by side; `store(key, value)` and `load(key)` keep JSON values in
  the session, where they survive a resume and follow the session's tree; `tools.search(query)` and
  `tools.describe(name)` find and describe tools the description does not list, MCP tools that are held
  back included.
- `classify(question, choices, { model })` in scripts, and `azazel.classify` in extensions: a small model
  picks one of the choices and says how sure it is (`{ choice, index, confidence, reason, model }`). It is
  asked on the new `judge` role, which falls back to `smol`; its cost is in `/cost` under `classify`.
- Virtual models: an extension's `azazel.router(name, function(request) ... end)` registers `virtual/<name>`,
  which `/model`, `--model`, roles and `set_model` take like any model. The function picks the model (or
  chain, with an optional thinking level) of each request from why it is made (`user`, `continuation`,
  `retry`, `direct`), the model that answered last, and the conversation. The session records the choice
  and, per request, the model it went to with the virtual model behind it (`request_started` has `via`);
  the status line shows `virtual/auto -> provider/model`. Rust extensions use `Registrar::virtual_model`.
  `examples/extensions/auto_model.luau` is a small router. A virtual model is refused while `--provider`
  keeps a run to one provider's models.
- RPC: `State` has `via` and `virtual_models`.
- Schedules: `azazel schedule add "0 9 * * 1-5" "<prompt>"`, `add every 10m "<prompt>"` and
  `add at +2h "<prompt>"` run a prompt at a time, in a background session of its own or, with `--session`,
  in one session every time; `schedule list`, `rm` and `run` manage them. They are kept per project and
  handed to the system's scheduler (Task Scheduler, cron, launchd), so nothing of azazel has to be running.
  A run that was missed is not caught up, and a run is skipped while the one before it is still at work.
  `[schedule] register = false` keeps them without the system's scheduler, for `azazel schedule run --due`.
- Triggers: `azazel schedule add --on ci-failure "<prompt>"` runs the prompt when a CI run fails (read
  with `gh run list` every `schedule.poll_secs` seconds), `--on file:<glob>` when a matching file changes.
  A session that is open in the project (the UI or a background session) watches for them and gets the
  prompt as a message; with several open, one of them owns each trigger.
- `/loop`: `/loop 5 <prompt>` sends a prompt five times, `/loop 10m <prompt>` every ten minutes, and
  `/loop until "<condition>" <prompt>` until the `judge` role, shown the end of the conversation, says the
  condition holds. `Esc`, a round that fails and a check that fails stop it; `schedule.loop_max` (50) is
  the most rounds an interval or a condition takes.
- The `schedule` tool: an agent adds, lists and removes schedules and triggers; adding and removing ask
  for permission like a command.
- `azazel doctor` says which scheduler starts the schedules and whether `gh` is there for a CI trigger.
- Interactive programs: `shell` with `interactive: true` runs a command in a real terminal (a pseudo-terminal;
  ConPTY on Windows) for programs that ask questions, wait for keys or draw a screen: editors, REPLs,
  installers, `git rebase -i`, logins. The call returns the first screen and a terminal id, and the new `pty`
  tool reads the screen (`read`, which can wait for a change, for a text or for the end), types text and keys
  (`send` with `text`, `keys` such as `ctrl+c` or `down`, and `submit`), and ends the program (`stop`). With
  `wake_on_exit` the lead is told when the program ends, with its last screen and the saved log
  (an `artifact://` address).
- The terminal of such a program opens as a popup and follows it live. `Enter` gives you the keyboard, after
  which every key goes to the program and the agent's input waits; `Ctrl+G` hands it back, `Esc` hides the
  box, `Ctrl+X` stops the program. `F3` (`terminal`) and `/terminals` bring a hidden terminal back.
- Typing into a terminal is decided by the rules for `shell`: it is allowed where the program is, and the text
  is held to `deny` rules, the push guard and the destructive-command and secret-file checks like a command.
- Trace export: `[telemetry] exporter = "otlp"` or `"langsmith"` sends each prompt as a trace of its own
  (prompt, model request, tool call; a sub-agent under the call that started it) with models, tokens, cost,
  errors and durations, to an OpenTelemetry collector over HTTP or to LangSmith's runs API. Every span names
  its session (`session.id`; LangSmith threads), and a prompt arrives when it ends. Off by default.
  Prompt and reply text, tool input and output go only with `content = true`, secrets hidden. Spans are sent in
  batches beside the session: no turn waits for the backend, and its first failure is logged once.
- `/share` uploads the conversation, secrets hidden, as a secret gist through `gh` and prints the link. It says
  what is uploaded and asks first.
- `/bug [what went wrong]` and `azazel bug` write a bug report to a zip under `~/.azazel/reports/`: version,
  system, `azazel doctor`, the settings in effect with secrets removed, the end of the log and, when asked for,
  the conversation with secrets hidden. They offer to open an issue with the summary through `gh`; the zip stays
  on the machine.
- `azazel doctor` says where traces go and whether `gh` is there for `/share` and `/bug`.
- Permission mode `bypass`: nothing is asked and no guard applies except your `deny` rules and the push guard.
  It is entered with `/mode bypass` (a warning box that starts on `No`), `--mode bypass` or `/settings`, never
  with `Shift+Tab`; the status line shows it in the colour of errors; a project's config may set it only once
  the project is trusted.
- Secret files (`.env`, `*.pem`, `*.key`, SSH keys, cloud and package credentials under the home directory,
  azazel's own login file, and `permissions.secret_paths`) are asked about before an agent reads or writes one,
  in every mode but `bypass`. A shell command that names one counts (`cat .env`, `curl -d @.env`). The prompt
  offers `Allow once` and `Allow this session`, never `Always allow`; `grep` over a directory passes them by.
- A download piped into an interpreter (`curl … | sh`, `iwr … | iex`, `bash <(curl …)`) is a destructive
  command: asked about in every mode but `bypass`.
- `tools.redact_output = true` takes secrets out of tool results before the model and the session file get
  them; the tool card says how many were redacted.
- RPC: `approval_request` carries `secret`; ACP offers `Allow this file for this session` for a secret file.
- `session_recall`: an agent reads its own conversation back from the session file, including what a summary
  replaced and tool results that were elided. It searches by words (best match first) or by regular
  expression, returns entries in full by id (`expand`) or between two ids (`range`), and lists the files the
  conversation read and wrote with the entries that did it (`touched`). It only reads, and coordinator mode
  keeps it.
- `/context` is an inspector: the lead's last request by category (each part of the system prompt by name,
  AGENTS and context files, the skill index, memory, tool schemas with each MCP server on its own, messages,
  tool calls, tool results, images), each with its tokens, its share and a bar. `enter` opens a category's
  exact text in the full-screen viewer. The numbers use the counter compaction uses and are scaled to the
  input tokens the provider counted.
- `azazel context [PATH|ID] [--json]` prints the same breakdown for a saved session from the `context.json`
  the session keeps of its last request; over RPC, `run_command context` takes `json` and
  `text <request> <category>`.
- Stall watchdog: a provider that sends nothing for `retry.first_event_secs` (20) after a request, or for
  `retry.stall_secs` (120) in the middle of a reply, is given up and the same model asked again after a
  pause. The half of a reply that had arrived is dropped and never reaches the model. A model that stalls
  `retry.max_stalls` (3) times within a request is left for the next one in the chain. The transcript and
  print mode say `no answer from <model> for 20 s; retrying`; the RPC `retry` event carries `stalled_secs`.
- Done-check (`guards.done_check`): when the lead stops after `write` or `edit` changed code and no build,
  test, linter, `[verify]` command or clean check on write ran since, it is told once per prompt to prove the
  change or say plainly that it is unverified.
- Stuck guard (`guards.stuck_after`): the result of a `shell` command that has failed three times in a row in
  a turn tells the agent to form a new hypothesis before trying it again.
- Runaway guard (`guards.runaway`): a reply that repeats a block of 200 characters or more three times in a
  row is stopped as it is written and asked for again, like a reply that matched a stream rule.
- Durable runs: the session file now records where each run stands (`run_started`, `request_started`,
  `tool_started`, `run_ended`), as notes beside the conversation that an older azazel skips. A session whose
  process was killed in the middle of a run says so when it is opened again, and `/continue`,
  `azazel -c --continue-run` and the RPC command `continue_run` go on with it: calls that only read are run
  again, other calls that had started are reported to the model as interrupted, and a request that was never
  answered is sent again.
- `azazel ps` lists a background session whose process died in the middle of a run as `interrupted`, and
  `azazel attach <id>` starts it again on the same session, with the options and mode it had, and goes on
  (`[session] resume_runs = false` makes it wait for `/continue`). `azazel kill <id>` forgets it.
- Several clients can be attached to one background session at once. Each is told where the session stands
  when it attaches (`snapshot` in the `attach` response: the reply being written, running tools, queued input,
  waiting requests) and then sent everything; any of them may prompt, steer or answer, what one types shows
  on the others (`user_input`), and a request one answers closes on the others (`request_closed`).
- Named prompts: `request_id` on the RPC `prompt`, `azazel bg --request-id <id>` and `_meta.requestId` on an
  ACP `session/prompt`. A prompt sent again under the same name starts nothing and answers with the state of
  the run the first one started, for as long as the session exists.
- `web_search` asks the lead's own model provider to search when it has a search tool: Anthropic's
  `web_search` server tool, the `web_search` tool of the Responses API (OpenAI, the ChatGPT login, xAI) and
  Gemini's Google Search grounding. It needs no search key and works on a subscription login. Each query is
  one request on the lead's model; the numbered results are read from the pages its answer cites. A search
  that fails, or cites nothing, is answered by the configured backend instead. `[web] search = "provider"`
  uses the provider alone, `provider_search = false` leaves it out, and `/cost` counts the requests under
  `web_search`.

- `/cost` reports the prompt cache: the share of the session's input that was read from it, the same per
  model, and the last requests that missed it with the cause (the model, the tool list, a named part of the
  system prompt, the thinking level, a rewritten message, or the five minutes a provider keeps an unused
  cache). `cache.report_misses = true` also says so in the transcript when it happens.
- `cache.warming = "idle"` keeps Anthropic's prompt cache alive while a session waits for you: shortly before
  it would expire the lead's last request is sent again for one token, for up to `cache.warm_minutes` after the
  lead's last request and only when at least 4096 tokens are cached. `/cost` shows the warming requests and
  when the next one is due. Off by default.
- The `AGENTS.md` (or `CLAUDE.md`) of a directory other than the one you started in reaches an agent with its
  first tool call that names a path there, together with those of the directories above it, once per agent
  and session and within the 64 KB of the context files (`context.nested`).
- `@path` in `AGENTS.md`, `CLAUDE.md` and rule files places that file in the text, relative to the including
  file or the project root, up to `context.include_depth` files deep. Cycles are refused, and only files of
  the project and of `~/.azazel` that hold no credentials can be included.
- `ask_user` asks several things at once: `questions` (up to 6, each with a short `header`) are answered in
  one form with a tab per question (`Tab`, `Shift+Tab`) and a `Send` tab that shows the answers first.
  A question can carry the `code` or the `diff` it is about and name the option the agent would take
  (`recommended`), which is marked and is where the selection starts.
- `ask_user` with `async: true` returns at once; the answers reach the agent that asked as a message, a steer
  while it works and a new prompt when it has stopped.
- The fleet view: `Down` in an empty input puts the keyboard in the roster. `Enter` opens the selected
  agent's work in a popup (its task, its replies as they are written, its tool cards), typing there steers
  it, `Ctrl+X` stops it; `x` in the roster stops the selected agent. `o` in the agent hub opens the same
  popup for any agent, finished ones included.
- `@<agent id> text` at the prompt sends the text to that agent and `@<definition> text` starts an agent from
  that definition; `@` at the start of a prompt completes both before the files.
- RPC: the `form` request, `recommended`, `code` and `diff` on `ask`, the `agent_mention` command, and
  `user_input` frames with `agent` for what a sub-agent is handed. `azazel attach --plain` answers a form
  with `/answer a; b; c`.
- GitHub addresses in `web_fetch`, `web_grep` and `read`: a repository is cloned one commit deep under the
  artifacts directory and comes back as its file tree, its README and the path of the clone; a directory, a
  file (with `#L10-L20`, those lines), a pull request (`/files` or `.diff` for the changes), an issue and an
  Actions run (its jobs and the logs of the failed steps) come from GitHub's API. Everything goes through `gh`
  when it is signed in, so private repositories work; otherwise GitHub is asked without a login.
- YouTube videos in `web_fetch` and `read`: the captions as a transcript with the time of each part.
- `read` converts Word, PowerPoint, Excel and mail files (`.docx`, `.pptx`, `.xlsx`, `.msg`, `.eml`) to
  Markdown in a directory under the artifacts directory and returns an outline by line; pictures, every sheet
  as CSV and attachments are files beside it. `offset` and `limit` then read lines of the Markdown. New cargo
  feature `office`, part of the default build.
- `azazel secrets add <name> --host <host> --header "Name: value"` (or `--query "name=value"`, or `--basic
  "user:password"`; `-` asks for the value at the terminal) stores a credential for the requests agents make
  to one host, in `~/.azazel/secrets.json` (a file of its own beside `auth.json`, and a secret file for the
  permission rules). `azazel secrets list` shows names, hosts and kinds, never a secret; `azazel secrets rm
  <name>` forgets one; `azazel doctor` names them.
- `http` tool: one request to a web API (`method`, `url`, `headers`, `body` or `json`), returning the status,
  the response headers and the body. The credential stored for the host is added to the request and taken out
  of everything that comes back, so the agent uses a service without holding its key. It is sent over https
  (or to this machine) only and never to another host; redirects are followed for a `GET` or `HEAD` on the
  same host only. Permission subjects are `domain:<host>` and, with a stored credential, `auth:<service>`:
  an `allow` has to cover both.
- `azazel secrets add-site <url-pattern> --username <name>` stores the login of a site, and the `browser`
  action `fill_login` types it into the fields `ref_user` and `ref_password` when the address of the page
  matches the pattern. The password goes into a password field or nowhere, and is taken out of whatever the
  browser tool returns afterwards.
- `browser` action `handoff`: the browser is shown in a window on the same profile with the same tabs, you
  are asked to finish there (`reason`) and press Enter, and the agent carries on with the page as you left
  it. For logins you type yourself, second factors and CAPTCHAs.
- Resources for one run: `-e/--extension <path>` loads a Luau script, or a directory laid out like a package
  (skills, agent definitions, commands, scripts, MCP servers) with its code enabled; `--skill <path>` adds a
  skill or a directory of skills; `--mcp-config <file>` adds the servers of an `mcp.json`; `--theme <name|file>`
  sets the theme. Nothing is installed or remembered. They are trusted like your own files, win over what is
  found elsewhere under the same name, and still load with the `--no-*` switches and `--bare`.
- `--provider <id>` takes a run's models from one provider: names given to `--model` and `/model` are looked
  up among its models, the picker lists only them, and without `--model` the lead keeps the models of its
  chain that are that provider's.
- `azazel pkg add npm:<name>[@<version or tag>]` installs a package from the npm registry without npm or Node:
  the tarball is checked against the SHA-512 the registry lists and unpacked like a clone; no install script
  runs and no dependency is fetched. `azazel pkg update` moves it to what `latest` or its tag names now.
  `NPM_CONFIG_REGISTRY` names another registry.
- Extensions can replace and wrap tools. Luau: `azazel.tool{ ..., replaces = true }` puts a tool in the place
  of the built-in (or MCP, or other extension's) tool of that name, and `azazel.wrap(name, { before, after })`
  runs functions around every call of a tool: `before` may change the arguments, refuse the call or answer
  it, `after` may change the result. Rust: `Registrar::replace_tool`, `replace_tool_source`, `wrap_tool` and
  the `ToolWrap` trait. A disabled script replaces and wraps nothing. `/tools` and `azazel doctor` say which
  tools are replaced or wrapped, and by which script.

### Changed

- `[web] search = "auto"` now tries the lead's provider first, then `brave`, `tavily`, `exa`, `searxng` and
  `duckduckgo` as before. `azazel doctor` names who answers: `web search: provider (<model>), then <backend>`.
- The details of a `web_search` result name the backend of each query, and `provider_error` when the
  provider's search was passed over.
- The permission mode `ask` is now called `manual`, everywhere a mode is named: `permissions.mode`, `--mode`,
  `/mode`, RPC `set_mode` and `get_state`, ACP session modes, the `permission_mode` of hooks, `ctx.mode` in
  Luau. The old name is refused with a message that names the new one. The `ask` list of rules is unchanged.
- One mode governs the whole session. A change of mode applies at once to the lead and to every sub-agent,
  running ones included; an agent definition can only narrow it (`mode: plan`).
- `auto` mode keeps every safety guard: it asks about protected paths, secret files and destructive commands.
- Plan mode refuses a write or a command before a protected path or an `ask` rule can turn it into a question.
- Approving a plan leaves plan mode only when the session is in it: a plan from an agent whose definition keeps
  it in plan mode no longer changes the session's mode.
- An elided tool result now names the entry that holds it,
  `[result elided: <n> bytes; session_recall expand=<id>]`, and a compaction summary ends with a note naming
  the newest entry it covers. The summariser sees entry ids and puts them after facts whose detail it leaves
  out. An agent without `session_recall` gets the notes as before.
- `/context` without a request to show says so instead of estimating how full the window is; the status line
  still shows that.
- A provider's model list that does not answer no longer holds the first request up for minutes: it is given
  up after `retry.first_event_secs`.
- `azazel attach` no longer refuses a second client, and `attach --take` disconnects every client.
- Responses of a background session go to the connection that sent the command, under the id that client
  chose, so two clients need not agree on ids.
- A prompt sent after a reply whose tool calls never got a result (a killed process, a conversation taken up
  in the middle of a run) now answers those calls first, so the provider no longer rejects the request.
- What changes while a session runs is no longer part of the system prompt: the date and git branch, plan
  mode, the goal, the memories kept in the prompt and the task list reminder are sent after the conversation
  as a `<system-note>`, outside what a provider caches. A mode switch, a new goal or a saved memory used to
  make the next request pay for the whole conversation again; now the cached prefix is read as before.
- The task list reminder is part of that note instead of a message of its own.
- A select question that allows your own answer takes it in the same box, under the options, instead of in
  a second one; `Esc` there returns to the options.

## [0.4.0] - 2026-11-13

### Added

- Built-in themes `catppuccin-mocha`, `tokyo-night`, `gruvbox-dark` and `dark-daltonized`, and two new theme
  colours, `surface` and `backdrop`.
- `tui.glyphs` (and `AZAZEL_GLYPHS`) chooses the symbols the UI draws with: `unicode` (default), `nerd` for
  Nerd Font icons, or `ascii`.
- New configuration keys: `tui.popups`, `tui.indicator`, `tui.indicator_text`, `tui.session_accent`,
  `tui.tool_style`, `tui.redact`, `tools.check_on_write`, `[tools.check]`, `tools.check_report`,
  `tools.read_guard`, `tools.scan.secrets`, `lsp.idle_secs`, `lsp.write_severity`,
  `lsp.servers.<name>.warm_files`, `checkpoints.timeout_ms`, `checkpoints.exclude` and
  `mcp.idle_timeout_secs`.
- Checks on write: after `write` and `edit` the linter for the file's kind runs on it when it is installed
  (clippy, ruff, eslint or oxlint, shellcheck, go vet, actionlint, hadolint, or your own from `[tools.check]`).
  Errors join the tool result; when the lead stops it is told once what was found on the lines the turn
  changed (`tools.check_report`), with the files that could not be checked. A write waits 5 seconds for its
  checker; a slower one finishes in the background (60 seconds at most) and its findings come with that report.
- clippy, eslint and go vet run the project's own code to check a file (build scripts, macros, the eslint
  config), so they run only in a trusted project. A project with a `Cargo.toml`, an eslint config or a `go.mod`
  at its root now has something to trust when one of them is installed, and the UI asks once.
- Secret scan: with `gitleaks` installed and a trusted project, the project is scanned once per session in the
  background and the places found join that report, never the secrets.
- `azazel doctor` lists the installed checkers and says whether secrets are scanned for.
- `web_fetch` returns a long page in parts: `offset` (first line) and `limit` (characters, default 30000),
  with a last line that names the lines shown and the offset to continue from. `read` of a URL pages the
  same way with its own `offset` and `limit`.
- `web_grep` searches a fetched page by regular expression or plain text and returns the matching lines
  with their numbers, from the same cache as `web_fetch`. It is governed by the `web_fetch` permission
  rules, and the `scout`, `planner` and `researcher` agents and coordinator mode have it.
- `web_search` takes up to five `queries` in one call, searched three at a time, each with its own
  section; a query that fails says so without losing the others.
- `web_fetch` returns the text of a PDF, under a first line naming the file it saved.
- `browser` `wait` can wait for `role:<role> "<name>"`, `url:contains:<s>`, `title:contains:<s>` and
  `gone:<selector>`, and `poll_ms` sets how often it looks.
- The status line shows how fast the lead writes (`↯ 78 tok/s`): an estimate while a reply streams, the
  provider's own token count once it ends. It stays, muted, until the next prompt.
- A working indicator that moves: a band of light passes over `thinking` / `running <tool>` above the input.
  `tui.indicator` chooses `shimmer`, `kitt` or `static`, `tui.indicator_text` sets phrases to show in turn, and
  `tui.session_accent` tints the band with a colour of the session's own.
- On terminals at least 100 columns wide the status line draws the context window as a bar
  (`◫ 42% [━━━╸────]`).
- `/settings`, Appearance: theme (a list of the built-in themes and the files in `~/.azazel/themes`), symbols,
  working indicator, session colour, tool call style, hiding secrets and popups. All of them change the running
  UI at once.
- Menus, pickers, questions, approvals, plan reviews and every step of `/settings` open as a box in the middle
  of the window over the dimmed transcript, which is back exactly as it was when the box closes
  (`tui.popups = false`, or a window under 44x12, keeps them in place of the input).
- `/glyphs` picks the symbol set from a list in which every row is drawn in its own symbols, and saves it.
- `Ctrl+O` in a box whose text does not fit (a long plan, a long command) opens all of it full screen.
- The card of a `read` shows the file as a listing: its line numbers in a column and the code coloured by
  its language. A directory is a tree, with sizes in a column.
- The cards of `grep` and `web_grep` put the lines found under the name of their file, behind their line
  numbers, with the text searched for picked out; `glob` puts files under their directory.
- `tui.glyphs = "nerd"` adds an icon before files and directories in tool cards.
- `tui.tool_style = "compact"` shows a finished tool call on one row and calls that follow one another as a
  tree under a row that counts them; an edit keeps its diff. `Ctrl+O` writes the last group as full cards.
- `tui.redact` (on by default) hides API keys, tokens, `Bearer` tokens, `…KEY=`/`…TOKEN=`/`…SECRET=`/
  `…PASSWORD=` values and PEM private keys in tool cards, the transcript viewer and Markdown and HTML
  exports (`[redacted:<kind>]`). The model and the session file keep the text.
- A tool card whose body was cut says how long the whole output is (`4.1k chars`).
- The task list survives: it is rebuilt from the session on `-c`, `--session`, `--fork`, `/resume`, `/tree`,
  `/rewind` and `/fork`, and while it has open items the agent is reminded of it with every request.
- `ask_user` takes a `type` (`select`, `multiselect`, `text`, `confirm`), a `context` shown above the
  question, and `allow_freeform`, which adds an `Other…` row to type an answer.
- `Allow this session` in the permission prompt allows a rule until azazel exits without saving it (RPC:
  decision `session`; ACP: option `allow_session`; plain attach: `/session`).
- MCP servers have a `lifecycle` (`eager`, `lazy`, `lazy-keep-alive`). A `lazy` server is started by the
  first call into it and stopped after `mcp.idle_timeout_secs` without one; its tools, prompts and
  instructions are offered meanwhile from `~/.azazel/cache/mcp/`.
- `includeTools` and `excludeTools` in an MCP server's entry choose which of its tools agents get.
- Checkpoints: a fork continues the history of its parent, the shadow repository is packed at the end of a
  session, and `checkpoints.exclude` is applied.
- The read guard knows lines. `edit` must be of lines the agent has read: a `read` with `offset` and `limit`
  counts for the lines it returned, several reads add up, and an edit elsewhere is refused with
  `edit targets lines 40-52 of src/a.rs, which you have not read; read them first`. What an agent read stays
  read through its own edits and, line by line, through changes made by others.
  `tools.read_guard = "file"` brings the old rule back and `"off"` removes the guard.
- `/allow-edit <path>` lets the next edit of one file through, whatever the agent has read of it.
- `lsp` has three new actions. `outline` lists every symbol of a file with its kind, lines and declaration;
  `read_symbol` returns the source of one symbol (`Type.method`, or `occurrence` among several of a name),
  which then counts as read, so an agent can edit a function of a large file without reading the file;
  `rename_file` moves a file and has the language server update the files that refer to it.
- `warm_files` of a language server are opened as soon as it starts, and `lsp.write_severity = "warning"`
  adds the server's warnings to what follows a write.
- `ast_grep`, when [ast-grep](https://ast-grep.github.io) is installed (`ast-grep` or `sg`): search code by
  its syntax tree with `$NAME` and `$$$NAME` metavariables, or rewrite what matches. A replacement is
  previewed as a diff and written with `apply: true` to the files named; each file is written like an edit
  (read guard, format, checks, language server), and all of them or none. `azazel doctor` says whether it is on.

### Changed

- `write` over an existing file takes having read all of it; a read of a part is no longer enough
  (`you have read only part of src/a.rs; read the rest before overwriting it, or change it with edit`). Notes
  and logs (`.md`, `.txt`, `.log`) still need only a read of any part.
- Language servers stop after `lsp.idle_secs` (240) without a touched file and start again on the next touch;
  `/lsp` shows `stopped, idle`.
- `web_fetch` no longer returns a page of any size in one piece: more than `limit` characters come in parts.
- Every symbol outside tool cards (the prompt, rules, spinner, status line, todo list, roster, queued
  messages, Markdown bullets, quotes and tables, the transcript viewer, side questions, pickers and popups)
  follows `tui.glyphs`. At `ascii` the UI draws with plain ASCII only, and the opening banner uses thin letters.
- The status line shows the cost only when the lead answers with an API key. On a subscription you signed in
  to, it shows the fullest usage window instead (`◔ 61% 5h`), from the first response on rather than only once
  the window is nearly full.
- A status line that does not fit drops whole segments from the end of its left side (goal, agents, cost,
  speed, context, directory, branch) before it shortens the model. A long working directory keeps its end.
- `accept-edits` is marked `⏵` in the status line; `⏵⏵` is `auto` alone.
- Tool cards follow `tui.glyphs` like the rest of the UI: at `ascii` their frame is `+`, `-` and a vertical
  bar and their state is `[+]`, `[!]` or `[-]`.
- Every list has a bar beside the selected row, details in a column of their own, a scrollbar when it is longer
  than its box and, where it filters as you type, a count of the rows that match.
- A plan under review is shown in the review box, and a note on an approval stands out in the warning colour;
  an approval for a call that would destroy work has a warning-coloured frame.
- The model pickers show each model's context window and price (`ctx 200k · $3/$15`), or `subscription` for a
  provider used through a login; the theme picker shows each theme's colours; `/settings` rows show their
  current value in a column of its own.
- MCP servers that run as a local command are `lazy` by default: they are no longer started with the
  session. `"lifecycle": "eager"` in an entry keeps the old behaviour.
- A question the user dismisses, or that nobody can answer, is no longer a failed tool call: `ask_user`
  returns `the user declined to answer`. Its options are checked (2 to 8, no duplicates).
- The last choice of the permission prompt is `Deny and say why`.
- A prompt that changed no file takes no snapshot of its own.
- RPC: an `ask` request carries `expects`, `context` and `allow_freeform`.

### Fixed

- The card of a followed sub-agent's tool call shows what the call was about (the file, the command) instead
  of its raw arguments.
- `write` refuses file names Windows reserves for devices (`aux.c`, `NUL`, `com1.txt`), on every system.
- The task list of the previous session no longer stays on screen after `/new` or `/resume`.
- A `git` that hangs can no longer hold up a prompt: a snapshot is stopped after `checkpoints.timeout_ms`
  and checkpoints turn off for the session.
- A rewind that fails part of the way puts the files back as they were.

## [0.3.0] - 2026-11-13

### Added

- New configuration tables: `[session]`, `[limits]`, `[notify]`, `[editor]`, `[voice]`, `[advisor]`,
  `[rules]`, `[presets]` and `[[stream_rules]]`, plus new keys in `[tui]`, `[team]`, `[permissions]`,
  `[tools]` and `[update]`. Settings that name a program to run are ignored in an untrusted project.
- Session lists come from an index kept beside the session files, so pickers open without reading every
  session again; sessions are listed most recently used first.
- The prompt can follow the flags without `-p`: `azazel fix the tests` opens the UI and starts on it, and
  `azazel -p fix the tests` no longer needs quotes.
- `@image.png` in a prompt attaches the image (up to 8 per prompt).
- Model names may be partial everywhere you type one (`--model fable`, `/model haiku:high`); `azazel models`
  lists the models you can use.
- Launch flags: `--session <path|id>`, `--fork`, `--name`, `--session-dir`, `--no-session`, `--add-dir`,
  `--set table.key=value`, `--tools`, `--no-tools`, `--disallowed-tools`, `--system-prompt`,
  `--append-system-prompt`, `--no-extensions`, `--no-skills`, `--no-hooks`, `--no-context-files`, `--no-mcp`,
  `--no-memory`, `--bare`, `--max-turns` and `--max-cost`.
- A cost cap: past `[limits] max_cost_usd` (or `--max-cost`) no agent makes another request, and a warning is
  shown once at `warn_cost_usd`.
- `azazel auth status`, `azazel completions <shell>`, and `/add-dir` and `/dirs` in the terminal UI.
- Sessions name themselves after the first reply (a short title from the `smol` role); `--name` sets one.
- `azazel sessions list|search|delete|prune|export` and `azazel export`; exports can be Markdown, HTML (one
  file, no scripts) or JSON lines, and `/export` picks the format from the file name.
- `[session] retention_days` deletes old unpinned sessions in the background.
- `/name` names the session, or has a name suggested; the name shows in the welcome line.
- `/session` shows what the open session holds: messages, branches, tokens and cost, model, checkpoints,
  agents.
- `/resume` keys: `Ctrl+R` rename, `Ctrl+D` delete, `Ctrl+P` pin, `Ctrl+N` named only, `Tab` preview. Pinned
  sessions are listed first.
- `/tree`: `Alt+L` labels an entry, `Ctrl+L` lists labelled entries, `Ctrl+U` lists your prompts.
- `/retry` (`F5`) sends your last prompt again; the first attempt stays as a branch.
- `/handoff [focus]` continues in a new session that starts from a brief of this one.
- `/search <text>` finds sessions by what was said in them and resumes the one you choose.
- `/consult [question]` asks a saved session a question without resuming it.
- Prompt history is kept per project across sessions: Up/Down and `Ctrl+R` reach earlier sessions' prompts.
- `azazel stats`: sessions, prompts, tokens and cost per day, model, project and tool.
- `azazel changelog` and `/changelog` (in the viewer); the first start after an update says how many changes
  it brought.
- Several accounts per provider: `azazel login <provider> --as <label>`, `azazel logout <provider> --account
  <label>`, and `[providers.<id>] account` to choose the default one.
- `/account` lists a provider's accounts with their usage windows and lets you use one for the session, make it
  the default, sign it out or add another.
- When an account runs out of its usage allowance, azazel moves to the provider's next account and asks the
  same model again, and says so.
- Service tiers: `/fast`, `/slow` and `--tier` ask OpenAI models for priority or flex processing and Claude
  models for fast mode; the status line shows the tier.
- Model presets: `/preset`, `/preset <name>`, `/preset save <name>` and `--preset <name>`.
- Rule files in `.azazel/rules/` and `~/.azazel/rules/` that apply always, when an agent touches matching files,
  or on request through `rule://<name>`; the rule files of Cursor, Windsurf, Copilot, Gemini, Cline and
  `.claude/rules/` are read too (`[rules] compat`).
- Stream rules: a `trigger` in a rule file or `[[stream_rules]]` in the config stops a reply as it is written and
  asks again with the rule.
- `/rules`, `azazel rules list` and `azazel rules test`.
- Hook options `once` and `wake`, and hook events `file_changed`, `config_change`, `worktree_create`,
  `worktree_remove` and `setup`; `/hooks` lists every hook, where it comes from and how it last ran.
- Commands that destroy work (`git reset --hard`, `rm -r` outside the project and others) are asked about in
  every mode; `[permissions] destructive` sets `ask`, `allow` or `deny`.
- API keys are read from `~/.azazel/.env` and, in a trusted project, from the project's `.env`.
- `azazel mcp import` copies servers from Claude Desktop, Claude Code, Cursor, VS Code and Windsurf;
  `azazel mcp approve` and `azazel mcp reject` decide about a project's servers.
- Edit the draft in your own editor with `Ctrl+G` (`[editor] command`, `$VISUAL`, `$EDITOR`).
- Vim keys in the input (`/vim`, `editor.mode = "vim"`).
- A kill ring: `Ctrl+Y` pastes what `Ctrl+K`, `Ctrl+U`, `Ctrl+W` and `Alt+D` cut; `Alt+Y` cycles.
- Long pastes fold into one token; dropped files become image attachments or `@` mentions;
  `@"path with spaces"` works.
- `!!command` runs a shell command without telling the agent.
- `Ctrl+S` stashes a draft; `Alt+Up` takes a queued message back.
- `Alt+T` steps the thinking level; `Alt+M` and `Alt+Shift+M` step through favourite models (`Ctrl+F` in the
  model picker).
- A full-screen transcript viewer (`F2`, `/transcript`) with search, jumps, copy and the mouse wheel.
- `/copy`, `/tools`, `/jobs`, `/todo`, `/trust`, `/forget-approvals`; `/permissions` can remove and add rules.
- `/settings` has sections for appearance, editor, permissions, notifications, limits, sessions, memory,
  updates, voice and team.
- `Ctrl+B` moves a running shell command to the background.
- `read` reads PDFs as text, page by page (`pages: "2-6"`, 20 pages per call), and says so when a PDF is
  scanned; it shows Jupyter notebooks cell by cell with their outputs.
- New `read` addresses: `session://`, `session://<id>`, `job://<id>`, `memory://`, `memory://<id>`,
  `memory://search/<query>`, and with the `gh` program installed `pr://`, `pr://<n>`, `pr://<n>/diff`,
  `issue://`, `issue://<n>`.
- `run_code` tool: the agent runs a Luau script that calls its other tools and gets back only what the script
  prints and returns; every call inside passes the permission rules. Luau extensions can call tools with
  `azazel.tools.call(name, args)`.
- Format on write: `[tools] format_on_write = true` runs the file's formatter after `write` and `edit`;
  `[tools.format]` sets commands per extension.
- Quick-add memory: a one-line input starting with `# ` is saved as a project preference.
- Voice input: `Ctrl+Space` or `/voice` records from the microphone, a second press transcribes and puts the
  words in the input, `Esc` cancels; the input rule shows the recording time and a level meter.
- Spoken replies: `/speak on|off|stop` reads the final reply of each run aloud; `Esc` or typing stops it.
- `/btw <question>` asks about the session on the side while the agent works. The answer appears in a panel
  where you can copy it, send it to the agent or ask a follow-up; nothing is added to the session.
- `/review [ref]` and `/security-review [ref]` start a reviewer on your uncommitted changes, or on the commits
  since a branch, tag or commit.
- `/task <agent> [name=<id>] <prompt>` starts any agent yourself; its report goes to the lead.
- `/diff` shows uncommitted changes file by file in the viewer; `/diff turn` and `/diff session` show what
  changed since your last prompt or since the session began.
- `/commit [hint]` drafts a commit message, lets you edit it and commits as you. It never amends or pushes.
- Typed results: give an agent a `schema` in a `task` call, or `output-schema` in its definition, and it hands
  back JSON that matches. The value is checked, a mismatch is sent back for correction, and `agent://<id>`
  returns the JSON.
- `azazel -p --json-schema <FILE|JSON>` prints only JSON that matches the schema and exits 1 when the run
  produced none.
- Coordinator mode (`--coordinator`, `/coordinator on|off`, `[team] coordinator`): the lead plans and delegates
  and has no tool that edits files or runs commands. A session remembers the mode.
- Advisor (`[advisor] model`, `when = "stop"`): a second model reviews the lead's work before it stops and can
  send it back for one more round. `/advise [question]` asks it on demand.
- `azazel doctor` reports the advisor and coordinator settings.
- `azazel acp`: use azazel as the agent of an editor that speaks the Agent Client Protocol. Prompts, tool calls
  with diffs, permission requests, the task list as the plan, modes, cancel, and loading a stored session.
- `azazel mcp serve`: serve azazel to another MCP client. `azazel_prompt` runs a task and returns the reply and
  a session id to continue with; `read`, `glob`, `grep`, `web_search` and `web_fetch` are served under their
  own names.
- RPC commands: `new_session`, `resume_session`, `fork_session`, `list_sessions`, `get_messages`, `get_tree`,
  `set_leaf`, `set_name`, `set_thinking`, `set_tier`, `compact`, `retry`, `rewind`, `shell`, `export`, `btw`,
  `list_models`, `get_usage`, `run_command`. The first line is `{"type":"hello","protocol":1,"version":…}`.
- Crates `azazel-protocol` (typed commands and frames) and `azazel-client` (a Rust client),
  `examples/client-demo`, a Python example, and `docs/rpc.schema.json`.
- Luau: `azazel.prompt(name, render)` adds a system prompt part; `azazel.session` reads and changes the open
  session.
- The terminal bell and a desktop notification when an agent needs you or a long run ends or fails while the
  terminal is not focused (`[notify]`).
- The terminal title shows the session and its state; the tab shows progress in Windows Terminal, Ghostty,
  WezTerm and iTerm2.
- The machine is kept awake while agents work (`[notify] keep_awake`).
- Clickable links in replies and on the file names of `read`, `write` and `edit` cards; `/open` opens the last
  link of the last reply, a numbered link or a file.
- Pictures returned by tools are shown inline in kitty, Ghostty, iTerm2, WezTerm and Sixel terminals
  (`[tui] images`).
- Mermaid diagrams drawn inline and `/open diagram`, in builds with the cargo feature `diagrams`.
- `/usage` and `azazel usage [--json]`: what Claude and ChatGPT subscriptions have left; the status line shows
  a window from 80 %.
- `/stats [days]`.
- `[update] auto = true` downloads a new release in the background and installs it when the UI exits.
- Inside tmux, notifications and progress are passed through and `/copy` goes through `tmux load-buffer -w`.
- `[voice]` settings: `stt`, `tts`, `voice`, `language`, `record_command`, `stt_command`, `play_command`,
  `auto_send`, `max_secs`. Speech uses the `audio/transcriptions` and `audio/speech` endpoints of any
  configured provider, or local programs; `azazel doctor` reports what is set up and what is missing.
- Background sessions: `azazel bg <prompt>` runs a prompt in a session that goes on after the terminal is
  closed and prints its id; `azazel ps` lists them, `azazel logs <id> [-f]` shows what one did, `azazel attach
  <id>` opens it in the full UI (`--plain` as lines you answer), `azazel attach --take <id>` brings it back
  into this terminal, and `azazel kill <id>` stops it. `/bg <prompt>` starts one from the UI on a copy of the
  open session. Approvals and questions raised while nobody is attached wait, with a desktop notification.
- `azazel --ssh <host> [--remote-dir DIR] [prompt]` works on another machine: azazel runs there and the UI
  (or with `-p` the reply) is shown here. `--detach` runs the prompt in a background session there,
  `--attach <id>` opens one, and `azazel --ssh <host> ps|logs|kill` manage them.
- A session a background session has open is locked: opening it elsewhere says where it runs, and `/resume`
  marks it.
- RPC: `attach` and `stop` for background sessions, `azazel rpc --attach <id>` to reach one from any
  program, `get_session`, `set_label`, `list_commands`, `list_files`, `set_leaf` with `before`, images with
  `steer`, and the session's reports (`cost`, `session`, `tools`, ...) through `run_command`. `get_state` also
  reports the session file, the git branch, coordinator mode, the goal, the task list, the roles and the
  request count.

### Changed

- `-r` is now `--session` and also takes a path to a session file; `--resume` still works.
- A used-up Anthropic or ChatGPT subscription window is reported as a usage limit with its reset time, not as
  an authentication failure.
- `/login` and `/logout` take an optional account label; `azazel auth status` lists each provider's accounts.
- MCP servers a project defines wait for your approval, once per version of their definition. Projects you
  already trust will ask once.
- `--no-context-files` also leaves out rule files.
- `Alt+Enter` messages are held by the UI and sent as real prompts (files attached, checkpoint, hooks).
- `/reload` also re-reads skills, commands, agent definitions, rule files, hooks, key bindings and the theme.
- `Ctrl+Y` pastes from the kill ring; redo is `Ctrl+Shift+Z`.
- `/cost` also lists what side questions, commit drafts and the advisor cost.
- `/tools` marks tools withheld in coordinator mode.
- A refusal for "extra usage" names the full window and when it resets.
- RPC `get_state` also reports `name` and `tier`; RPC `set_model` is recorded in the session; RPC errors for a
  wrong member name its kind, for example "`chain` must be a list".
- A reopened session comes back on the model and thinking level it last used (`[session] restore_model`);
  `--model` and `--thinking` win.
- Naming, pinning or labelling a session no longer counts as using it.
- `Shift+Enter` adds a line in text prompts such as the commit message and feedback inputs.
- `tui.show_thinking` and `tui.expand_tools` take effect at startup.
- A skill's `allowed-tools` now takes effect: it limits the sub-agent of a `context: fork` skill, and for other
  skills allows the listed calls without a prompt until the run ends.

- The terminal UI reaches its session through one interface, so it runs the same on a session in its own
  process, on a background session and over SSH.
- RPC `get_state` reports `busy` from the moment a prompt is accepted, not only once the lead has picked
  it up.
- The release binary gains `interprocess` and, on Windows, `winsafe` (feature `detach`, on by default).

### Fixed

- On Windows, a multi-line paste no longer sends the prompt at its first line break.
- `/retry` no longer sends an image that an `@mention` attached twice.
- In coordinator mode, a side question, hand-off or advisor review is sent with the tools the lead is offered,
  so it reuses the cached prompt.
- An MCP server that fails the moment it is started (a missing program) is reported as failed instead of
  staying `connecting`.
- The approval prompt for a destructive command starts on "Deny", so a stray Enter cannot approve it.

## [0.2.0] - 2026-11-12

### Added

- A restyled terminal UI: the input and status line sit at the bottom between rules, tool calls are framed
  cards, your messages have a background band and diffs have line numbers and coloured rows.
- About 80 built-in providers that speak a standard chat API, new wire formats (native Ollama, OpenRouter,
  Azure resources, Gemini CLI) and `~/.azazel/models.yml` for your own providers and models.
- A model role for the lead and for each built-in agent, set in `/settings` or under `[roles]`.

### Fixed

- Claude subscription requests were refused when a tool name started with `mcp_`; the MCP resource tool is
  now `list_mcp_resources`, and that refusal is no longer retried as a rate limit.
- A cold browser gets 30 seconds to start.

## [0.1.0] - 2026-11-11

### Added

- First release: a lead agent with a team of specialists, the model layer with fallbacks, permission modes
  and checkpoints, goals with a checker, hooks and Luau extensions, MCP, memory and search, web and
  browser tools, screen capture and playtesting, media generation and sprite sheets, language servers,
  subscription sign-in, packages, built-in skills, RPC mode, installers and `azazel update`.

[Unreleased]: https://github.com/sinnafuls/azazel-cli/compare/v0.9.1...HEAD
[0.9.1]: https://github.com/sinnafuls/azazel-cli/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/sinnafuls/azazel-cli/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/sinnafuls/azazel-cli/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/sinnafuls/azazel-cli/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/sinnafuls/azazel-cli/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/sinnafuls/azazel-cli/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/sinnafuls/azazel-cli/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/sinnafuls/azazel-cli/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/sinnafuls/azazel-cli/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/sinnafuls/azazel-cli/releases/tag/v0.1.0
