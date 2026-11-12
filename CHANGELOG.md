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

### Changed

- `-r` is now `--session` and also takes a path to a session file; `--resume` still works.
- A used-up Anthropic or ChatGPT subscription window is reported as a usage limit with its reset time, not as
  an authentication failure.
- `/login` and `/logout` take an optional account label; `azazel auth status` lists each provider's accounts.
- MCP servers a project defines wait for your approval, once per version of their definition. Projects you
  already trust will ask once.
- `--no-context-files` also leaves out rule files.
- MCP servers a project defines wait for your approval, once per version of their definition. Projects you
  already trust will ask once.
- `--no-context-files` also leaves out rule files.

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
