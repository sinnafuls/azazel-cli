# Tools

Tools are what an agent can do besides writing text: read and change files, run commands, search the web, drive a browser, start other agents. This page lists every built-in tool with its input fields and what it does, and covers the rules they share: read limits, the freshness rule for edits, output truncation, the URL schemes `read` understands, and the `[tools]` settings.

Every call goes through the permission engine first; see [permissions](permissions.md). Each tool declares what a call touches (`read`, `write`, `exec` or `net`), and the permission mode decides from that whether you are asked. When all the calls in one assistant message are read-only they run at the same time; otherwise they run one after another in order.

Fields marked `?` are optional.

## Files

| Tool | Input | Behaviour |
|---|---|---|
| `read` | `path`, `offset?`, `limit?` | Reads a text file, an image, a directory or a `scheme://` address |
| `write` | `path`, `content` | Creates a file or replaces its whole content. Parent directories are created |
| `edit` | `path`, `edits: [{old, new, replace_all?}]` | Exact text replacement in an existing file |

`read`:

- Text comes back as `<line number><tab><text>`. Default 2000 lines per call; `offset` (1-based line) and `limit` page through longer files. Lines are cut at 2000 characters and one call returns at most about 256 KB. A partial read ends with `[lines 1-2000 of 5120; continue with offset=2001]`.
- Files over 64 MB are refused. Other binary files return an error with their size and type.
- PNG, JPEG, WebP and GIF files are returned as images, scaled down when their long side exceeds 1568 px.
- A directory lists up to 500 entries: `name/` for directories, `name<tab>N bytes` for files.
- `~/` is expanded. Relative paths start at the agent's working directory.

Freshness rule for `write` and `edit`: an agent may only overwrite a file it has read (or written) and that has not changed since. Otherwise the call fails with `read the file before overwriting it` or `file changed since you last read it; read it again`. The check compares content, not timestamps. Files you attach with `@path` count as read. When one agent writes a file another agent has read, the other agent is told to read it again before editing.

`edit` applies its edits in order and writes nothing if any of them fails. Each `old` must occur exactly once unless `replace_all` is true; the errors are `old text not found (edit #N)` and `old text matches N times (edit #N); add surrounding lines or set replace_all`. CRLF line endings and a byte-order mark are preserved. Both tools write through a temporary file and a rename. When a language server covers the file, its errors for the new content are appended to the result; see [lsp](lsp.md).

## Search

| Tool | Input | Behaviour |
|---|---|---|
| `glob` | `pattern`, `path?` | Finds files by name, newest first, at most 1000 |
| `grep` | `pattern`, `path?`, `glob?`, `ignore_case?`, `context?`, `mode?`, `limit?`, `multiline?` | Searches file contents with a Rust regular expression |

Both honour `.gitignore` and other ignore files, include hidden files and skip `.git`. In `glob`, `*.rs` matches at any depth, `src/*.rs` only directly in `src`, and `src/**/*.rs` anywhere below it. `grep` modes are `content` (default, `path:line:text`), `files` (paths only) and `count` (`path:N`); `limit` defaults to 200 results, `context` adds lines around each match, `multiline` lets a pattern span lines. Binary files are skipped and matching lines are clipped at 500 characters.

## Shell and jobs

| Tool | Input | Behaviour |
|---|---|---|
| `shell` | `command`, `cwd?`, `timeout_secs?`, `background?`, `description?` | Runs a command in the resolved shell |
| `job` | `action` (`output`, `kill`, `list`), `id?`, `tail?` | Manages background jobs started by `shell` |

`shell` returns stdout and stderr together, then `exit code: N`. Long output keeps its first 100 and last 300 lines; lines over 4000 characters are cut. On timeout (default `shell_timeout_secs`) or abort the whole process tree is killed. With `background: true` the call returns a job id (`job-1`, ...) at once; `job output` returns the last `tail` lines (default 100, a job keeps its last 2000) and the job's state. Background jobs are killed when the session ends. Processes started for an agent can never push to a git remote; see [permissions](permissions.md).

## Todo list, questions, plans

| Tool | Input | Behaviour |
|---|---|---|
| `todo` | `items: [{text, status}]` | Replaces the agent's task list. `status` is `pending`, `in_progress` or `done`; at most one item may be `in_progress` |
| `ask_user` | `question`, `options?`, `multi?` | Asks you a question and waits. Without `options` you type a free answer. Without an interactive user it fails with `no interactive user` |
| `plan_submit` | `path` | In plan mode: submits a plan file under `.azazel/plans/` for your approval. See [permissions](permissions.md) |

## Web

| Tool | Input | Behaviour |
|---|---|---|
| `web_search` | `query`, `limit?`, `site?`, `recency?` | Numbered results with title, URL and snippet. `limit` defaults to 8 (at most 20); `recency` is `day`, `week`, `month` or `year` |
| `web_fetch` | `url`, `format?` | Fetches an http or https URL. `format` is `markdown` (default), `text` or `raw` |

Details, search backends and limits: [web and browser](web-and-browser.md).

## Language servers

| Tool | Input | Behaviour |
|---|---|---|
| `lsp` | `action`, `path`, `line?`, `column?`, `query?`, `new_name?` | `action` is `diagnostics`, `definition`, `references`, `hover`, `symbols` or `rename`. Positions are 1-based. `rename` changes files; the rest only read |

See [lsp](lsp.md).

## Skills

| Tool | Input | Behaviour |
|---|---|---|
| `skill` | `name`, `args?` | Loads a skill: returns its directory and its instructions with `$ARGUMENTS` filled in |

See [skills and commands](skills-and-commands.md).

## Memory

Registered when `[memory] enabled` is true (the default). See [memory](memory.md).

| Tool | Input | Behaviour |
|---|---|---|
| `memory_save` | `text`, `kind`, `tags?`, `scope?` | Stores a memory. `kind` is `fact`, `decision`, `preference`, `convention` or `gotcha`; `scope` is `project` (default) or `global` |
| `memory_search` | `query`, `limit?`, `scope?` | Best matches as `id [kind, scope] text #tags`. `limit` defaults to 8 |
| `memory_forget` | `id` | Deletes a memory |
| `semantic_search` | `query`, `collection?`, `path_glob?`, `limit?` | Searches the indexed project files (`code`, default) or a document collection. Returns `path:first-last` and the first 12 lines of each chunk; `limit` defaults to 10 |

## Team

See [team](team.md). The lead has all of these. A sub-agent always has `agent_send`; it gets `task`, `agent_stop`, `agent_wait` and `agent_list` only when its definition lists `spawns` and the depth limit allows; the `worktree_*` tools are the lead's only.

| Tool | Input | Behaviour |
|---|---|---|
| `task` | `agents: [{agent, prompt, name?, model?, isolation?}]`, `wait?` | Starts the listed agents in parallel. With `wait` (default true) returns every report; with `wait: false` returns the ids and each report arrives later as a message |
| `agent_send` | `to`, `message` | Messages an agent id, `parent` or `all`. A finished agent resumes with the message |
| `agent_stop` | `id`, `reason?` | Stops an agent the caller started, and the agents it started |
| `agent_wait` | `ids?`, `timeout_secs?` | Waits (default 600 s) until a started agent has a new result or a message arrives |
| `agent_list` | none | Id, definition, state, activity, time, tokens and cost of every agent |
| `worktree_apply` | `agent` | Brings an isolated agent's changes into the main tree and removes its worktree. On a conflict the worktree is kept |
| `worktree_discard` | `agent` | Removes an isolated agent's worktree and branch |

## Goals

Offered to the lead only, and only while a goal is open. See [goals](goals.md).

| Tool | Input | Behaviour |
|---|---|---|
| `goal_done` | `summary`, `evidence` | Claims the goal is met. The checker runs; the result is `verified` or what is still wrong |
| `goal_blocked` | `reason` | Pauses the goal because the agent needs you |

## Browser and screen

| Tool | Input | Behaviour |
|---|---|---|
| `browser` | `action` plus `url`, `tab`, `ref`, `selector`, `x`, `y`, `text`, `submit`, `keys`, `hold_ms`, `dy`, `for`, `timeout_ms`, `expression`, `full_page`, `width`, `height`, `filter` | Drives a Chromium browser. Actions: `open`, `navigate`, `back`, `reload`, `snapshot`, `click`, `hover`, `type`, `key`, `scroll`, `wait`, `eval`, `screenshot`, `resize`, `console`, `network`, `tabs`, `close` |
| `screen` | `action` plus `window`, `display`, `region`, `x`, `y`, `button`, `double`, `from`, `to`, `duration_ms`, `dy`, `keys`, `hold_ms`, `text`, `ms`, `seconds`, `fps` | Captures and operates the desktop. Actions: `windows`, `capture`, `focus`, `click`, `move`, `drag`, `scroll`, `key`, `type`, `wait`, `record` |

See [web and browser](web-and-browser.md) and [screen and playtesting](screen-and-playtesting.md).

## Media

The four `generate_*` tools are registered only when a media backend has credentials. `sprite_sheet` is always available. See [media and sprites](media-and-sprites.md).

| Tool | Input |
|---|---|
| `generate_image` | `prompt`, `size?`, `aspect_ratio?`, `count?`, `transparent?`, `reference_images?`, `model?`, `out?` |
| `generate_video` | `prompt`, `image?`, `duration_secs?`, `aspect_ratio?`, `model?`, `out?` |
| `generate_3d` | `prompt?`, `image?`, `format?` (`glb`, `obj`, `fbx`), `model?`, `out?` |
| `generate_audio` | `prompt`, `kind` (`sfx`, `music`, `speech`), `duration_secs?`, `voice?`, `model?`, `out?` |
| `sprite_sheet` | `source`, `name`, `target`, `animations?`, `key_color?`, `tolerance?`, `trim?`, `padding?`, `max_size?`, `out_dir?` |

## MCP

| Tool | Input | Behaviour |
|---|---|---|
| `mcp__<server>__<tool>` | the server's own schema | A tool of a connected MCP server |
| `list_mcp_resources` | `server?` | Lists resources as `mcp://<server>/<uri>  name` |
| `tool_search` | `query`, `limit?` | Present only when there are too many MCP tools to list: finds tools by description and makes the hits callable. `limit` defaults to 8 (at most 25) |

See [mcp](mcp.md).

## Output truncation and artifacts

A tool result whose text exceeds `max_result_bytes` (default 50000) is cut at that size. The full text is saved under `~/.azazel/projects/<slug>/artifacts/` and the result ends with:

```text
[output truncated: 181234 bytes in total] full output: artifact://<call id>
```

The agent reads the rest with `read artifact://<call id>`. The same directory holds other files tools produce: screenshots, recordings, downloads that are not text, worktree patches.

## URL schemes `read` accepts

| Address | Returns |
|---|---|
| `artifact://<id>` | A spilled tool output |
| `skill://<name>` | The body of a skill |
| `skill://<name>/<relative path>` | A file beside that skill's `SKILL.md` (at most 256 KB) |
| `agent://<id>` | The latest report of an agent |
| `agent://<id>/transcript` | What the agent did: one line per tool call and the full text of what it said |
| `mcp://<server>/<resource uri>` | A resource of an MCP server |
| `http://...`, `https://...` | The page as Markdown, through the `web_fetch` pipeline. Permission rules for `web_fetch` apply |

Extensions can add more schemes; see [extensions](extensions.md).

## `[tools]` configuration

```toml
[tools]
disabled = ["web_search", "screen"]   # tool names no agent is offered
max_result_bytes = 50000              # text kept from one tool result
shell_timeout_secs = 120              # default timeout of the shell tool
shell = ""                            # program name or path; empty means auto-resolve
```

| Key | Default | Meaning |
|---|---|---|
| `disabled` | `[]` | Tools removed for every agent, by name (also MCP and extension tools) |
| `max_result_bytes` | `50000` | Size above which a result is cut and spilled to an artifact |
| `shell_timeout_secs` | `120` | Timeout of a `shell` call without `timeout_secs` |
| `shell` | `""` | Shell to use instead of the resolved one. In a project config this is honoured only when the project is trusted |

To take a tool away from one kind of agent only, use `tools` and `disallowed-tools` in its definition; see [team](team.md).

## Shell resolution

With `shell` empty, the first of these that exists is used:

| System | Order |
|---|---|
| Linux, macOS | `bash`, then `sh` |
| Windows | `bash.exe` on `PATH` (the WSL launcher in `System32` is skipped), then the `bash.exe` shipped with Git for Windows, then `pwsh`, then `powershell` |

POSIX shells run the command with `-c`; PowerShell runs it with `-NoProfile -NonInteractive -Command`. The `shell` tool's description tells the model which shell and operating system it is writing for. `azazel doctor` shows the resolved shell.
