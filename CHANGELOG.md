# Changelog

Every release of azazel, newest first. `azazel changelog` shows this file, and each release's notes on
GitHub are its section here.

## [Unreleased]

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
- `azazel changelog` and `/changelog`; the first start after an update says how many changes it brought.
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
- `[voice]` settings: `stt`, `tts`, `voice`, `language`, `record_command`, `stt_command`, `play_command`,
  `auto_send`, `max_secs`. Speech uses the `audio/transcriptions` and `audio/speech` endpoints of any
  configured provider, or local programs; `azazel doctor` reports what is set up and what is missing.

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
- A reopened session comes back on the model and thinking level it last used (`[session] restore_model`);
  `--model` and `--thinking` win.
- Naming, pinning or labelling a session no longer counts as using it.
- `Shift+Enter` adds a line in text prompts such as the commit message and feedback inputs.
- `tui.show_thinking` and `tui.expand_tools` take effect at startup.
- A skill's `allowed-tools` now takes effect: it limits the sub-agent of a `context: fork` skill, and for other
  skills allows the listed calls without a prompt until the run ends.

### Fixed

- On Windows, a multi-line paste no longer sends the prompt at its first line break.
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

[Unreleased]: https://github.com/sinnafuls/azazel-cli/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/sinnafuls/azazel-cli/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/sinnafuls/azazel-cli/releases/tag/v0.1.0
