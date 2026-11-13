# Changelog

Every release of azazel, newest first. `azazel changelog` shows this file, and each release's notes on
GitHub are its section here.

## [Unreleased]

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
- A feature checklist, kept outside the repository.

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

[Unreleased]: https://github.com/sinnafuls/azazel-cli/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/sinnafuls/azazel-cli/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/sinnafuls/azazel-cli/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/sinnafuls/azazel-cli/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/sinnafuls/azazel-cli/releases/tag/v0.1.0
