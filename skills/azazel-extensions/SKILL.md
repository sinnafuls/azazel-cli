---
name: azazel-extensions
description: >-
  Use when customising this harness: writing a Luau extension (tools, event handlers, commands), configuring hooks, defining a custom agent, adding a slash command, or writing a skill. Contains the file locations, formats and API tables for each.
---
# Extending azazel

Pick the lightest mechanism that does the job.

| You want | Use | Where |
|---|---|---|
| Reusable instructions the model loads when relevant | Skill | `<root>/skills/<name>/SKILL.md` |
| A prompt template the user starts with `/name` | Command | `<root>/commands/<name>.md` |
| A specialist with its own tools, model and prompt | Agent definition | `<root>/agents/<name>.md` |
| A shell script run at fixed points (block, rewrite, add context) | Hook | `[[hooks.<event>]]` in `config.toml` |
| New tools, event logic, commands, UI segments, in a script | Luau extension | `<root>/extensions/<name>.luau` |
| Tools from another program or service | MCP server | `mcp.json` (see the `mcp-builder` skill) |
| Anything compiled in | Rust `Extension` | `examples/custom-distro` in the source tree |
| To share any of the above | Package | a directory or git repository (see Packages) |

`<root>` is `~/.azazel` (all projects) or `<project>/.azazel` (one project; the project's copy wins on a name clash). Project hooks, extensions and MCP servers run only after the user has trusted the project. `azazel doctor` and `/doctor` show what was loaded and what was skipped.

## Skills

A directory with a `SKILL.md`:

```markdown
---
name: release-notes
description: Use when writing release notes for this repository. Explains the house format and where the data comes from.
---
Body: instructions in Markdown. Supporting files sit beside it (a `references/` folder) and are linked with relative Markdown links.
```

| Frontmatter key | Meaning |
|---|---|
| `name` | Required. Lower-case words joined by hyphens, at most 64 characters. This, not the folder, is the skill's identity |
| `description` | Required, at most 1024 characters. Say when to use it; the model chooses skills by this line |
| `disable-model-invocation` | `true` hides it from the model; the user can still run `/skill:<name>` |
| `user-invocable` | `false` hides the slash command |
| `context` | `inline` (default) returns the body to the calling agent; `fork` runs it in a sub-agent and returns the report |
| `agent` | Definition used for `fork` (default `coder`) |
| `model` | Model chain used for the prompt `/skill:<name>` sends |
| `hooks` | Hooks active from the skill's first use until the session ends |

`$ARGUMENTS` in the body is replaced by the arguments. Files beside `SKILL.md` are read as `skill://<name>/<relative path>`. `/skills` lists valid skills and the parse error of each invalid one. Keep bodies under 500 lines and move detail into `references/`.

## Commands

A Markdown file; its path below `commands/` without `.md`, with `/` written as `:`, is the command name (`commands/git/sync.md` is `/git:sync`).

```markdown
---
description: Summarise what changed since a tag
argument-hint: <tag>
model: "@smol"
---
Summarise the changes since $1 for a changelog.

!`git log --oneline $1..HEAD`

Follow the format in @docs/CHANGELOG.md.
```

| Form | Replaced by |
|---|---|
| `$ARGUMENTS` | Everything typed after the command |
| `$1` … `$9` | Positional arguments (quotes group words) |
| `${2:-default}` | Argument 2, or `default` when absent |
| `@path` | The file's content, attached when the prompt is sent |
| A line `` !`command` `` | That command's output (trusted projects only) |

## Agent definitions

A Markdown file whose frontmatter configures the specialist and whose body is its system prompt.

```markdown
---
name: migrator
description: Use to write and check database migrations for this project.
tools: read, glob, grep, edit, write, shell(make migrate:*), shell(psql:*)
disallowed-tools: web_fetch
model: ["@slow", "@default"]
thinking: high
mode: inherit
max-turns: 40
skills: [testing]
spawns: []
isolation: none
---
You write migrations for …
```

| Key | Meaning |
|---|---|
| `name`, `description` | Required. The lead decides when to delegate from the description |
| `tools` | Tools granted. Omitted: every tool except the orchestration tools. `tool(pattern)` grants the tool and limits this agent's use of it to the pattern |
| `disallowed-tools` | Removed from the grant |
| `model` | A model or fallback chain: `provider/model[:thinking]` or a role such as `@smol` |
| `thinking` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| `mode` | `inherit` (the session's permission mode) or `plan` (read-only) |
| `max-turns` | Requests per task before it must report |
| `skills` | Skills whose bodies are appended to the system prompt |
| `spawns` | Definitions this agent may start itself |
| `isolation` | `none`, or `worktree` to work in its own git worktree and hand back a patch |

`[agents.<name>]` in `config.toml` overrides `model` and `thinking` without editing the file. `/agents` lists definitions and running agents. The built-in definitions are `scout`, `planner`, `coder`, `tester`, `reviewer`, `verifier`, `researcher`, `designer`, `artist` and `playtester`; a file with the same `name` replaces one.

## Hooks

Shell commands (or HTTP calls, or model checks) run at fixed events. In `config.toml`:

```toml
[[hooks.pre_tool_use]]
matcher = "shell"                 # regex on the tool name
if = "shell(git commit:*)"        # optional permission-rule prefilter
command = "./scripts/check-commit.sh"
timeout = 30                      # seconds; default 60

[[hooks.user_prompt_submit]]
command = "date +'Today is %A, %F.'"
```

| Event | Fires | Blocking means |
|---|---|---|
| `pre_tool_use` | Before a tool runs | Call denied; the reason goes to the model. May rewrite the input or waive the approval prompt |
| `post_tool_use` | After a tool succeeded | Reason appended to the result |
| `post_tool_use_failure` | After a tool failed | (cannot block) |
| `user_prompt_submit` | When the user sends a prompt | Prompt rejected. Plain output is added as context |
| `session_start` | Start, resume, clear, compact | Plain output is added as context |
| `session_end` | Session closes | (cannot block) |
| `stop` | The lead is about to finish a turn | The agent continues with the reason as a message |
| `subagent_start` / `subagent_stop` | A sub-agent starts or is about to finish | Spawn refused / the sub-agent continues |
| `pre_compact` / `post_compact` | Around context compaction | Manual compaction cancelled |
| `permission_request` | Before the user would be asked | The hook's decision is used |
| `notification` | Something wants the user's attention | (cannot block) |

Fields: `matcher`, `if`, `type` (`command` default, `http`, `prompt`, `agent`), `command`, `url`, `headers`, `prompt`, `model`, `timeout`, `async` (`true`: do not wait).

The hook receives JSON on stdin: `hook_event_name` (`PreToolUse`, `Stop`, …), `session_id`, `transcript_path`, `cwd`, `agent_id`, `permission_mode`, plus per event `tool_name`, `tool_input`, `tool_use_id`, `tool_response`, `prompt`, `source`, `stop_hook_active`, `agent_type`, `trigger`, `message`.

Results:

- Exit 0 with plain output: allowed; the output is context where the table says so.
- Exit 0 with JSON on stdout: `{"decision": "block", "reason": "…"}`, `{"continue": false, "stopReason": "…"}`, or `{"hookSpecificOutput": {"permissionDecision": "allow" | "deny" | "ask", "permissionDecisionReason": "…", "updatedInput": {…}, "additionalContext": "…"}}`.
- Exit 2: block, with stderr as the reason.
- Any other exit code: not blocking; stderr is shown to the user as a warning.

Environment: `AZAZEL_PROJECT_DIR`, `AZAZEL_SESSION_ID`, `AZAZEL_HOOK_EVENT`. A `stop` hook must let the agent stop when `stop_hook_active` is true, or it will loop.

## Luau extensions

A script `extensions/<id>.luau`, or `extensions/<id>/init.luau`. It runs once at load in a sandbox and registers what it provides through the global `azazel`. `/ext` lists scripts and their errors; `/reload` loads them again; `azazel ext types` writes editor type declarations to `~/.azazel/types/azazel.d.luau`.

```lua
azazel.tool({
  name = "word_count",
  description = "Count the words in a text file of the project.",
  parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
  access = "read",
  execute = function(args, ctx)
    local n = 0
    for _ in string.gmatch(azazel.fs.read(args.path), "%S+") do n += 1 end
    return tostring(n)
  end,
})

azazel.on("tool_call", function(ev, ctx)
  if ev.tool == "shell" and string.find(ev.input.command, "rm %-rf /") then
    return { block = true, reason = "refused by policy" }
  end
end)
```

| Function | Contract |
|---|---|
| `azazel.tool{ name, description, parameters, access?, execute }` | `parameters` is JSON Schema as a table. `access`: `"read"`, `"write"`, `"exec"` (default), `"net"`. `execute(args, ctx)` returns a string or `{ content, is_error }` |
| `azazel.on(event, handler)` | `handler(ev, ctx)` returns `nil`, `{ block = true, reason }`, or replacement fields |
| `azazel.command{ name, description, run }` | Slash command; `run(args, ctx)` may return text to show |
| `azazel.shortcut(key, fn)` | Key binding in the terminal UI |
| `azazel.status(key, text)`, `azazel.widget(key, lines)` | Status-line segment; lines above the editor (`nil` removes) |
| `azazel.notify(text, level?)` | `"info"`, `"warn"`, `"error"` |
| `azazel.send(text, { deliver })` | Message the lead as the user: `"follow_up"` (default) or `"steer"` |
| `azazel.exec(cmd, args, { cwd?, timeout? })` | Runs a program; returns `{ code, stdout, stderr }` |
| `azazel.http{ url, method?, headers?, body?, timeout? }` | Returns `{ status, headers, body }` |
| `azazel.fs.read / write / list / exists` | Confined to the project, the configured additional directories and `azazel.fs.data_dir` |
| `azazel.json.encode / decode` | |
| `azazel.state.get / set` | Per-script store that survives restarts |
| `azazel.agent.spawn{ agent, prompt, wait? }`, `.send`, `.stop`, `.list` | Sub-agents |
| `azazel.memory.save / search` | Project memory |
| `azazel.provider{ id, api, base_url, api_key_env?, headers? }` | Adds a model provider |
| `azazel.mcp{ name, … }` | Adds an MCP server (fields as in `mcp.json`) |
| `ctx.ui.confirm / select / input` | Ask the user |
| `ctx.cwd`, `ctx.session_id`, `ctx.agent_id`, `ctx.mode` | Read-only |

Events and the fields a handler may replace:

| Event | Replaceable | Can block |
|---|---|---|
| `session_start`, `session_end`, `turn_start`, `turn_end`, `agent_end`, `post_compact`, `notification` | | |
| `user_prompt` | `text` | yes |
| `before_agent_start` | `system_append` | |
| `tool_call` (`ev.tool`, `ev.input`, `ev.agent`) | `input`, `skip_prompt` | yes |
| `tool_result` | `content`, `is_error` | |
| `context` | `messages` | |
| `agent_spawn` | `prompt` | yes |
| `pre_compact`, `stop` | | yes |

Rules of the sandbox: registering functions (`tool`, `on`, `command`, `shortcut`, `provider`, `mcp`) work only while the script loads. A handler has 5 seconds of its own running time; a handler that errors or overruns disables its script, and the session continues. Functions that wait (`exec`, `http`, `fs`, `agent`, `memory`, `ctx.ui`) are available inside tools, handlers and commands, not at load.

## Packages

A package is a directory, usually a git repository, that bundles skills, agent definitions, commands, Luau extensions and MCP servers. Describe it with an `azazel.toml` at its root; every path is relative to the root and must stay inside it:

```toml
name = "my-pack"
skills = ["skills"]              # directories of skill folders, or single skill folders
agents = ["agents"]
commands = ["commands"]
extensions = ["ext/main.luau"]   # script files
mcp = "mcp.json"
```

Without an `azazel.toml`, a Claude plugin manifest (`.claude-plugin/marketplace.json` or `plugin.json`) or a Codex one (`.codex-plugin/plugin.json`) is read for its `skills` paths; without those, the directories `skills/`, `agents/`, `commands/` and a `SKILL.md` at the root are used.

| Command | Does |
|---|---|
| `azazel pkg add <source> [--name <name>]` | Installs from a git URL, `github:<owner>/<repo>[@ref]`, a directory (used in place, never copied) or a registry name |
| `azazel pkg list`, `pkg update [name]`, `pkg remove <name>` | Shows, refreshes, uninstalls |
| `azazel skills add <source>` | The same install, for packages of skills |
| `azazel skills add <package>/<skill>` | Offers one skill of a package to the model, installing the package first if needed |
| `azazel skills list` | Every skill a session here would have, with where each comes from |

Skills, agents and commands of a package always load. Its extensions, MCP servers and the hooks its skills carry load only when the user answered yes to the question asked at install, so write a package to be useful without them where you can. Precedence, lowest first: built-in, packages (in install order), the user's `~/.azazel`, the project's `.azazel`. A packaged script's id is `<package>-<file name>`. While developing, install the working directory itself (`azazel pkg add .`) and edits take effect on the next session.

## Checking your work

1. Put the file in place, then `/reload` (extensions) or start a new session (hooks, agents, commands, skills).
2. `/ext`, `/skills`, `/agents`, `/doctor`: confirm it loaded and read any error.
3. Exercise it: ask for something that should trigger the tool, hook or agent, and read the result.
4. Hooks: test the script alone by piping a sample event to it, and check its exit code.
