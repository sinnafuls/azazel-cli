# Migrating from another harness

`azazel migrate` brings over what pi, omp (oh-my-pi), jcode, Claude Code and Codex CLI hold on this machine:
providers, models and roles, API keys, skills, custom commands, agent definitions, MCP servers, rule files,
settings, conversations and memories. The other program's files are only read. Nothing azazel already has is
replaced unless you ask for it, and a report says what became of every single thing.

```sh
azazel migrate --dry-run             # what would be brought over; writes nothing
azazel migrate                       # everything, from every harness that is installed
azazel migrate --from omp --only models,keys,chats
azazel migrate --from claude --project ~/code/site
azazel migrate --overwrite           # replace what is already there
```

In the terminal UI, `/migrate` does the same in a popup: it lists the harnesses it found with what each
holds (`omp: 21 models and roles, 13 keys and sign-ins, 239 chats, ...`), lets you untick sources and kinds,
shows the plan of a dry run (`ctrl+o` reads all of it), and brings it over when you confirm. The report opens
full screen when it is done. `/migrate` never replaces what is there; for that, run `azazel migrate
--overwrite`.

| Flag | Meaning |
|---|---|
| `--from <SOURCE>` | `pi`, `omp`, `jcode`, `claude`, `codex` or `all` (the default); several separated by commas |
| `--only <KINDS>` | Only these kinds, separated by commas: `models`, `keys`, `skills`, `commands`, `agents`, `mcp`, `rules`, `settings`, `chats`, `memory`. Default: all of them |
| `--dry-run` | Print the report of what would be done and write nothing. Configuration changes are checked against a copy of your files, so the report is what a real run would say. Conversations are listed, not read |
| `--overwrite` | Replace what is already there (a role, a setting, a provider, a key, a file, an MCP server) instead of leaving it. A stored sign-in is never replaced by a key, and an imported conversation is never imported twice |
| `--project <DIR>` | The project whose own files (`.pi/`, `.omp/`, `.jcode/`, `.claude/`, `.codex/`) are read and whose `.azazel/` they go to. Default: the project you are in |

The exit code is 1 when something failed, and when a source named with `--from` is not installed.

## Where each harness is looked for

| Source | Directory | Changed by |
|---|---|---|
| pi | `~/.pi/agent` | `PI_CODING_AGENT_DIR` (when the directory it names is not omp's), `PI_CODING_AGENT_SESSION_DIR` for the sessions |
| omp | `~/.omp/agent` | `PI_CODING_AGENT_DIR` (when the directory holds `config.yml` or `agent.db`), `PI_CONFIG_DIR` (the name of `.omp`), `OMP_PROFILE` or `PI_PROFILE` (`~/.omp/profiles/<name>/agent`) |
| jcode | `~/.jcode`, and its key files in the application config directory (`%APPDATA%\jcode`, `~/Library/Application Support/jcode`, `~/.config/jcode`) | `JCODE_HOME` (keys then in `$JCODE_HOME/config/jcode`) |
| Claude Code | `~/.claude` and `~/.claude.json` | `CLAUDE_CONFIG_DIR` (the state file is then `.claude.json` inside it) |
| Codex CLI | `~/.codex` | `CODEX_HOME` |

A harness that is not installed is left out without a word; when none is found the command says where it
looked.

## What is brought over

| Kind | From | To |
|---|---|---|
| `models` | pi `models.json` and `settings.json` (`defaultProvider`, `defaultModel`, `defaultThinkingLevel`); omp `models.yml` and `config.yml` (`modelRoles`, `retry.fallbackChains`); jcode `config.toml` (`[provider]`, `[providers.<name>]`); Claude Code `settings.json` (`model`); Codex `config.toml` (`model`, `model_provider`, `model_reasoning_effort`, `[model_providers.<id>]`) | Providers with their models in `~/.azazel/models.yml`; roles in `[roles]` of `~/.azazel/config.toml` |
| `keys` | pi `auth.json`; omp `agent.db`, `.env` and the keys written into `models.yml`; jcode's `*.env` key files and `api_key` in `config.toml`; Claude Code `primaryApiKey` in `.claude.json`; Codex `OPENAI_API_KEY` in `auth.json` | `~/.azazel/auth.json` |
| `skills` | `skills/<name>/` of pi, omp, jcode and Codex, and of the project's `.pi`, `.omp`, `.jcode`, `.codex` | `~/.azazel/skills/`, `<project>/.azazel/skills/`, each directory whole |
| `commands` | pi `prompts/*.md`; omp `commands/**/*.md`; Claude Code `~/.claude/commands/**/*.md`; Codex `prompts/*.md`; and the project's `.pi/prompts`, `.omp/commands` | `~/.azazel/commands/`, `<project>/.azazel/commands/`, under the same names |
| `agents` | omp `agents/*.md` (user and `.omp/agents`); Claude Code `~/.claude/agents/*.md` | `~/.azazel/agents/`, `<project>/.azazel/agents/` |
| `mcp` | pi, omp and jcode `mcp.json` (user and project); Claude Code `mcpServers` and this project's servers in `.claude.json`; Codex `[mcp_servers.<name>]` | `~/.azazel/mcp.json`, `<project>/.azazel/mcp.json` |
| `rules` | The files below | `~/.azazel/rules/`, `<project>/.azazel/rules/` |
| `settings` | The settings below | `~/.azazel/config.toml` |
| `chats` | pi and omp session files; jcode session snapshots with their journals; Claude Code transcripts; Codex rollouts. Of every project, not only this one | Sessions of the project each was held in (`~/.azazel/projects/<project>/sessions/`) |
| `memory` | omp's mnemopi banks; jcode `memory/*.json`; Claude Code `projects/<project>/memory/*.md` | The [memory](memory.md) of the project each belongs to, or the memory of every project |

### Models and roles

- A provider the source defines goes to `models.yml` with its address, wire format, headers and models
  (name, context window, output limit, vision, reasoning, thinking levels, prices). Its key is not written
  there: it is a key (below). A key that is the name of an environment variable becomes `api_key_env`, and
  one that is a command (`!pass show x` in pi and omp) becomes `api_key_command`.
- An azazel provider speaks one wire format. When the models of one pi or omp provider speak two (some
  `openai-completions`, some `openai-responses`), the ones that differ become a provider of their own,
  `<id>-responses`, `<id>-chat`, `<id>-anthropic` or `<id>-google`, with the same address and key, and the
  roles that name those models follow them.
- Names are translated: `lm-studio` is `lmstudio`, `llama.cpp` is `llamacpp`, jcode's `claude` is
  `anthropic`, `gemini` is `google`, `copilot` is `github-copilot`. `openai-completions` is `openai-chat`,
  `google-generative-ai` is `google-generative`. A provider whose wire format azazel does not speak is left
  out and named in the report.
- A role is its model with its thinking level (`provider/model:high`), followed by its fallbacks (omp
  `retry.fallbackChains.<role>`). A model of a provider azazel does not know is left out of the chain and
  named; a role with nothing left is not written. omp's `advisor` role also becomes `[advisor] model`, and
  its `cycleOrder` becomes `tui.favourites` (`@default`, `@slow`, ...). pi's `enabledModels` become
  `tui.favourites`.
- Codex's model is taken from the ChatGPT subscription (`openai-codex/<model>`) when Codex is signed in with
  ChatGPT, and from the API (`openai/<model>`) when it has an API key. jcode's `openai` is read the same way.
- A model alias of Claude Code (`opus`, `sonnet`) names no model azazel could ask for and is reported.

### Keys and sign-ins

API keys are copied into `auth.json`. The first key of a provider becomes its default account; a further key
of the same provider from the same source becomes an account beside it (`openrouter#omp-2`; see
[several accounts](models-and-logins.md#several-accounts)). A key that is already stored is skipped, and a
key of a provider azazel does not have is left out.

A key is also left out when the source signs in to the same provider: the source used the subscription, and
a stored key would become the account azazel asks with and be billed for what the subscription covered. The
report names the key and the command that stores it anyway (`azazel login <provider> --key`).

**Sign-ins are never copied.** A copied refresh token ends the login of the program it was taken from the
first time either side refreshes it. Every sign-in found (a Claude or ChatGPT subscription, Copilot, ...) is
listed with the command to run, its e-mail address cut to its first letter:

```text
sign in       anthropic (d***@example.com): sign-ins are not copied; run `azazel login anthropic`
```

No key is ever printed, in the report or anywhere else. Credentials omp marked as removed or failed are not
read. Claude Code's subscription is recognised by its `.credentials.json`; on macOS it keeps that login in the
system's keychain, where it is not looked for. An account named in `.claude.json` beside an API key is the
Console account the key came from, not a sign-in.

### Skills, commands and agents

- A skill is copied as it is, with the files beside its `SKILL.md`. One azazel would refuse to load (a name
  that is not lower-case words joined by hyphens, no description) is copied and named with the reason.
- A command keeps `description`, `argument-hint` and `model` of its frontmatter; other keys
  (`allowed-tools`, `side`, ...) are dropped and named. In its body `$@` becomes `$ARGUMENTS`, Claude Code's
  `$ARGUMENTS[0]` becomes `$1`, and Codex's `$$` becomes `$`. Forms azazel has nothing for stay as written
  and are named: argument slices (`${@:2}`, `$@[2:1]`) and Codex's named arguments (`$FILES`).
- An agent definition keeps `name`, `description`, `tools`, `disallowed-tools`, `model`, `thinking`,
  `permission-mode` (`plan`), `max-turns`, `skills`, `spawns`, `isolation` and an `output-schema` written as a
  mapping. Claude Code's tool names and model aliases are translated (`Bash` is `shell`, `opus` is `@slow`),
  omp's `bash` is `shell`, `thinkingLevel` and `effort` become `thinking`. Every other key (`color`,
  `hooks`, `mcpServers`, `blocking`, ...) is dropped and named. A definition azazel could not load after
  that is left out with the reason.
- What azazel reads where it is stays there and is reported as `in place`: `~/.claude/skills`,
  `~/.agents/skills`, and a project's `.claude/skills`, `.claude/commands`, `.claude/agents`,
  `.claude/rules` and `.mcp.json` (see [skills and commands](skills-and-commands.md), [team](team.md),
  [rules](rules.md) and [MCP](mcp.md)).

### MCP servers

Entries are converted as [`azazel mcp import`](mcp.md#importing-from-other-clients) converts them:
`${env:NAME}` becomes `${NAME}`, a disabled server stays disabled. Timeouts are carried over in seconds
(omp writes milliseconds). Codex's `bearer_token_env_var` becomes an `Authorization: Bearer ${NAME}` header,
`env_http_headers` become headers filled from the variables, `env_vars` are passed on, and `enabled_tools`
and `disabled_tools` become `includeTools` and `excludeTools`. OAuth settings of a server are not carried
over: sign in with `azazel mcp login <name>`. A project's servers go to `<project>/.azazel/mcp.json`, where
they wait for your approval like any project server.

### Rule and context files

| File | Becomes |
|---|---|
| `~/.claude/CLAUDE.md` | `~/.azazel/rules/claude.md`, a rule that always applies |
| `~/.claude/rules/**/*.md` | `~/.azazel/rules/claude/` (their `paths:` is read as it is) |
| `<project>/CLAUDE.md` beside an `AGENTS.md` (which is the one azazel reads), `<project>/.claude/CLAUDE.md`, `<project>/CLAUDE.local.md` | `<project>/.azazel/rules/claude.md`, `claude-project.md`, `claude-local.md` |
| omp `RULES.md` and `AGENTS.md` of the user and of `<project>/.omp` | `rules/omp-rules.md`, `rules/omp-agents.md` |
| omp `rules/*.md` and `*.mdc` | `rules/omp/`; `condition` (or `ttsr_trigger`) becomes `trigger`, `globs` and `alwaysApply` are read as they are, other keys are dropped and named |
| pi `~/.pi/agent/AGENTS.md` | `~/.azazel/rules/pi-agents.md` |
| jcode `~/AGENTS.md`, `~/.jcode/prompt-overlay.md`, `<project>/.jcode/prompt-overlay.md` | `rules/jcode-agents.md`, `rules/jcode-prompt-overlay.md` |
| Codex `~/.codex/AGENTS.md` | `~/.azazel/rules/codex-agents.md` |

A context file gets a frontmatter that makes it a rule that always applies. Files it includes with a line
that is only `@path` are written into it, because the copy no longer sits beside them; includes that cannot
be followed are counted in the report. A project's `AGENTS.md`, and its `CLAUDE.md` when there is no
`AGENTS.md`, are read where they are and stay there.

### Settings

| Source | Setting | Becomes |
|---|---|---|
| pi, omp | `compaction.enabled`, `reserveTokens`, `keepRecentTokens` | `[compaction] enabled`, `reserve_tokens`, `keep_recent_tokens` |
| pi, omp | `steeringMode` (`one-at-a-time`, `all`) | `tui.queue_mode` (`one`, `all`) |
| pi, omp | `hideThinkingBlock: false` | `tui.show_thinking = true` |
| pi, omp | `shellPath` | `tools.shell` |
| pi | `theme`, `retry.maxRetries` | `tui.theme` (when azazel has a theme of that name), `retry.max_attempts` |
| omp | `symbolPreset` | `tui.glyphs` |
| omp | `theme.dark` | `tui.theme`, when azazel has a theme of that name |
| omp | `statusLine.preset` (`default`, `minimal`, `full`), `statusLine.separator`, `statusLine.sessionAccent` | `statusline.preset`, `statusline.separator` (`powerline`, `bar`, `space`), `tui.session_accent` |
| omp | `tools.approvalMode` | `permissions.mode`: `yolo` is `auto`, the others are `manual` |
| omp | `task.maxConcurrency` | `team.max_parallel` |
| jcode | `[display] theme`, `[features] memory` | `tui.theme`, `memory.enabled` |
| Claude Code | `permissions.allow`, `ask`, `deny` | `[permissions] allow`, `ask`, `deny`, added to the rules that are there |
| Claude Code | `permissions.defaultMode`, `additionalDirectories` | `permissions.mode`, `permissions.additional_dirs` |
| Claude Code | `cleanupPeriodDays`, `statusLine.command`, `apiKeyHelper` | `session.retention_days`, `statusline.command`, `[providers.anthropic] api_key_command` |
| Claude Code | `theme` in `.claude.json` | `tui.theme`, when azazel has a theme of that name |
| Codex | `approval_policy` with `sandbox_mode` | `permissions.mode`: a read-only sandbox is `plan`, `never` is `auto`, the others are `manual` |

- Claude Code's permission rules are rewritten: `Bash(npm test:*)` is `shell(npm test:*)`, `Read(./.env)`
  is `read(.env)`, `Edit(src/**)` is `edit(src/**)` and `write(src/**)` (a rule for editing covers writing
  a file there), `WebFetch(domain:x)` is `web_fetch(domain:x)`, and `mcp__server` is `mcp__server__*`. A
  rule for a tool azazel does not have is named and left out.
- Modes that ask nothing (`yolo`, `bypassPermissions`, Codex's `never` with `danger-full-access`) become
  `auto`, never `bypass`: `auto` still asks before a
  [destructive command](permissions.md#destructive-commands). The report says so, and how to lift that.
- Every value is written through azazel's own configuration check: one azazel would refuse is reported as
  failed and the file stays as it was. A value that is azazel's default anyway is not written.
- Settings without an equivalent are named in the report: hooks (their commands read the other harness's
  tool names and inputs), key bindings, extensions, omp's disabled providers, Codex's profiles and sandbox
  details, and whatever else the source's file holds that azazel has no key for. A project's own
  `.claude/settings.json` and `.codex/config.toml` are not converted.

### Conversations

Every conversation of every project becomes a session of the project it was held in, so `/resume` and
`azazel sessions list` in that project show it. It is named `imported from <source>: <title>`, keeps the
times of the original (it sorts by when it was last used there), and the original stays where it is.

- The branch the conversation was left on is imported, message by message: your prompts with their images,
  replies with their thinking, tool calls and results, the model and token counts of each reply.
  Compactions are kept as compactions, so a resumed session starts from the summary as the original did.
  Messages on branches that were left are counted in the report and not imported.
- Thinking is kept as text. The signature that lets a provider take its own thinking back is kept for
  Anthropic's and dropped for the others.
- Tool calls keep the names and inputs the other harness gave them. A call without a recorded result gets
  one that says so; a result without a call is left out; both are counted in the report.
- Sub-agents' conversations (omp's `<session>/<Agent>.jsonl`, Claude Code's `subagents/agent-*.jsonl`)
  become the session's agent transcripts; a sub-agent's own sub-agent is named after both (`Scout.Helper`).
- A command you ran with `!` in pi or omp becomes the note azazel writes for its own `!` commands. What a
  harness wrote into the conversation for itself (reminders, context it injected, the record of a slash
  command) is kept beside the conversation or left out, never shown as something you said.
- Images omp moved out of its session files are read back from its blob store.
- A session records where it came from. Running the migration again skips every conversation that is
  already there, so a later run brings only what is new.
- A conversation whose directory the source does not record is left out and named, and so is a pi or omp
  session file that does not begin with a session header.
- Sessions keep the times of the conversations they were, so `session.retention_days` applies to them as
  to any session: when it is set, the report says how many of the imported conversations are older than
  that and would be deleted when azazel next starts.
- The first listing of a project's sessions reads every one of them; the migration does that once at the
  end, so `/resume` opens at once afterwards.

### Memory

- omp: what was retained on purpose in the mnemopi banks, each for the project it was learned in. The
  transcript episodes the banks also hold are not memories in azazel's sense and are counted in the report;
  the conversations they were cut from are imported as chats. Other memory backends of omp are not read.
- jcode: the entries of `memory/global.json` and `memory/projects/*.json` that are in force. jcode names a
  project's file by a hash of its path; when that matches no directory known from its sessions, the
  memories become memories of every project and the report says so.
- Claude Code: each file of a project's `memory/` directory is one memory (`type: user` a preference,
  `feedback` a convention, the others facts); a lone `MEMORY.md` is read item by item.
- Codex does not document the format of its memory files; they are not read.

Imported memories carry the tags `imported` and the source's name. One whose text is already remembered is
skipped.

## The report

```text
omp (/home/you/.omp/agent)
  models
    imported  provider gate with 2 models -> models.yml
    imported  role default = anthropic/claude-x:high
    skipped   role smol = gate/small:low: roles.smol is already ["ollama/qwen"] (--overwrite replaces it)
    lossy     role odd: `nowhere/model-y` is left out: azazel has no provider `nowhere`
  keys
    imported  openrouter: API key from agent.db -> auth.json
    sign in   anthropic (d***@example.com): sign-ins are not copied; run `azazel login anthropic`
  chats
    imported  Fix the cart total (/work/shop): 14 messages, 1 sub-agent
    skipped   Old session (/work/shop): already imported

5 imported, 2 skipped, 1 lossy, 1 to sign in to
```

| Word | Meaning |
|---|---|
| `imported` (`would import` in a dry run) | Written |
| `skipped` | Left out because it is there already, or because something else is and `--overwrite` was not given |
| `lossy` | Carried over in part, or not at all, because azazel has nothing to hold it |
| `in place` | Not copied: azazel reads the file where it is |
| `sign in` | A sign-in to repeat with `azazel login <provider>` |
| `failed` | Could not be read or written; the exit code is 1 |

Settings, skills, commands and agent definitions written while the UI is open take effect after `/reload`
or a restart.
