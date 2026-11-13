# MCP servers

Azazel is a Model Context Protocol client. An MCP server gives agents extra tools, readable resources and prompt templates: a database, an issue tracker, a disassembler, anything that speaks the protocol. This page covers where servers are configured, the transports, how their tools appear to agents, logins, and what happens when a server calls back into the client.

MCP is part of the default build (cargo feature `mcp`).

## Configuration files

Servers are read from these files, later ones winning when two define the same name:

| Order | File | Loaded |
|---|---|---|
| 1 | the `mcp.json` of installed [packages](packages.md) | when the package's code is enabled |
| 2 | `~/.azazel/mcp.json` | always |
| 3 | `<project>/.mcp.json` | when the project is trusted; each server also needs [your approval](#approving-a-projects-servers) |
| 4 | `<project>/.azazel/mcp.json` | when the project is trusted; each server also needs your approval |

An entry in a later file replaces the whole entry of the same name. Each file holds an `mcpServers` object:

```json
{
  "mcpServers": {
    "fs": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "."],
      "env": { "LOG_LEVEL": "warn" },
      "cwd": "."
    },
    "docs": {
      "type": "http",
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" },
      "timeout": 60,
      "oauth": { "clientId": "", "scope": "", "callbackPort": 0 }
    },
    "legacy": { "type": "sse", "url": "https://example.com/sse", "enabled": false },
    "github": {
      "command": "github-mcp-server",
      "lifecycle": "lazy-keep-alive",
      "includeTools": ["get_*", "list_*", "search_*"],
      "excludeTools": ["get_secret*"]
    }
  }
}
```

| Key | Applies to | Meaning |
|---|---|---|
| `type` | all | `stdio`, `http` or `sse`. Defaults to `stdio` when `command` is present, otherwise `http` when `url` is present. `streamable-http` and `streamableHttp` are accepted for `http` |
| `command`, `args` | stdio | Program and arguments. The program is looked up on `PATH` |
| `env` | stdio | Variables added to the server's environment |
| `cwd` | stdio | Working directory, relative to the project root. Default: the project root |
| `url` | http, sse | The server's address |
| `headers` | http, sse | Headers sent with every request |
| `timeout` | all | Seconds one request may take. Default 60 |
| `oauth` | http, sse | `clientId` (empty: register a client dynamically), `scope` (space separated), `callbackPort` (0: any free port) |
| `enabled` | all | `false` keeps the entry but does not start it. Default true |
| `lifecycle` | all | When the server starts: `eager`, `lazy` or `lazy-keep-alive`; see [when servers start](#when-servers-start). Default `lazy` for `stdio`, `eager` for `http` and `sse` |
| `includeTools` | all | Globs on the server's own tool names (`get_*`). With any, only matching tools are offered |
| `excludeTools` | all | Globs on the server's own tool names. Matching tools are not offered, whatever `includeTools` says |

Keys Azazel does not know are ignored, so a file shared with other MCP clients loads.

### Variable expansion

`${VAR}` and `${VAR:-default}` are expanded from the environment in `command`, `args`, `env` values, `cwd`, `url`, `headers` values and the `oauth` `clientId` and `scope`. A variable that is unset and has no default disables that server; `azazel mcp list`, `/mcp` and `azazel doctor` report it as `${VAR} is not set`. Keep secrets in the environment, not in the file.

## Transports

| Type | How it connects |
|---|---|
| `stdio` | Starts the command and speaks MCP over its stdin and stdout. Its stderr goes to the log file at debug level (`azazel --debug`) |
| `http` | Streamable HTTP to `url` |
| `sse` | Tries streamable HTTP first; when the server refuses it, falls back to the legacy SSE transport |

### When servers start

| `lifecycle` | Starts | Stops |
|---|---|---|
| `eager` | When the session starts, in the background | When the session ends |
| `lazy` | With the first call into it: a tool call, a prompt, a resource read | After `mcp.idle_timeout_secs` (default 600) without a call; the next call starts it again. `0` never stops it |
| `lazy-keep-alive` | With the first call into it | When the session ends |

A local program (`stdio`) is `lazy` unless its entry says otherwise, so a session with ten configured servers starts none of them until an agent uses one. A remote server (`http`, `sse`) is `eager`: a connection costs nothing to keep.

A server that is not running can still be offered because azazel keeps what each `lazy` server said it has (tools with their schemas, prompts, instructions) in `~/.azazel/cache/mcp/<server>.json`. From that file the tools are listed and found by `tool_search`, the prompts are slash commands and the instructions are in the system prompt, all without starting anything. The file holds for one version of the entry: a server whose entry changed, or that has no file yet, is started once when the session starts, asked, and from then on treated as its lifecycle says. Every connection refreshes the file.

Eager servers start connecting in the background when a session starts. The first request of the session waits at most `startup_wait_ms` for them; servers that come up later add their tools to a later request. A server that fails or disconnects is retried after 1, 2, 4, 8 and 16 seconds and then marked `failed` (a connection that lasted 30 seconds starts the count over). A `lazy` server whose connection ends is not retried; the next call starts it. States shown by the tools below: `ready`, `connecting`, `not started`, `not started (cached N tools)` for a server that waits for its first call, `disabled`, `waiting for approval`, `rejected`, `needs login`, `failed: <why>`.

## Managing servers

```bash
azazel mcp list                                   # every server and its state
azazel mcp add fs -- npx -y @modelcontextprotocol/server-filesystem .
azazel mcp add docs --url https://example.com/mcp --header 'Authorization=Bearer ${DOCS_TOKEN}'
azazel mcp add fs --project -- npx -y some-server # write to <project>/.azazel/mcp.json
azazel mcp remove fs
azazel mcp login docs
azazel mcp logout docs
azazel mcp approve fs                             # let a server this project defines start
azazel mcp import cursor                          # copy the servers another client has
azazel mcp serve                                  # be an MCP server for another client
```

| Command | Does |
|---|---|
| `azazel mcp list` | Gives the servers it starts up to 10 seconds to connect, then prints each one's state, tool and prompt counts, command line or URL, and the file it comes from. It starts what a session would start: a `lazy` server whose tools are known is listed as `not started (cached N tools)`; `"lifecycle": "eager"` in its entry, or `/mcp reconnect <server>` in a session, connects it to see that it works |
| `azazel mcp add <name> [--project] -- <command> [args...]` | Adds a stdio server to `~/.azazel/mcp.json`, or with `--project` to `<project>/.azazel/mcp.json` |
| `azazel mcp add <name> [--project] --url <url> [--header NAME=VALUE]...` | Adds an HTTP server |
| `azazel mcp remove <name>` | Removes the entry from the first of `<project>/.azazel/mcp.json`, `<project>/.mcp.json`, `~/.azazel/mcp.json` that has it |
| `azazel mcp login <name>` / `logout <name>` | OAuth login for an HTTP server; see below |
| `azazel mcp approve <name>...` / `approve --all` / `reject <name>` | Decides whether a server the project defines may start; see [approving a project's servers](#approving-a-projects-servers) |
| `azazel mcp import [client] [--yes]` | Copies the servers another client is configured with into `~/.azazel/mcp.json`; see [importing](#importing-from-other-clients) |
| `azazel mcp serve` | Serves azazel to another MCP client on standard input and output; see [serving azazel over MCP](#serving-azazel-over-mcp) |

In the terminal UI, `/mcp` opens a list of servers; picking one offers `reconnect`, `disable`, `login` and `logout` (or `enable` for a disabled one). The same actions can be typed: `/mcp list`, `/mcp reconnect <server>`, `/mcp enable <server>`, `/mcp disable <server>`, `/mcp approve <server>`, `/mcp reject <server>`, `/mcp login <server>`, `/mcp logout <server>`. Enabling or disabling writes `enabled` into the file that defines the server.

### Importing from other clients

`azazel mcp import` reads the MCP configuration of other clients, lists the servers it finds, and adds the ones you choose to `~/.azazel/mcp.json`.

| Client | Files read |
|---|---|
| `claude-desktop` | `claude_desktop_config.json` in `%APPDATA%\Claude`, `~/Library/Application Support/Claude` or `~/.config/Claude` |
| `claude-code` | `~/.claude.json`: its own servers and those it keeps for this project |
| `cursor` | `~/.cursor/mcp.json` and `<project>/.cursor/mcp.json` |
| `vscode` | `mcp.json` in the user folder of VS Code (`%APPDATA%\Code\User`, `~/Library/Application Support/Code/User` or `~/.config/Code/User`) and `<project>/.vscode/mcp.json` |
| `windsurf` | `~/.codeium/windsurf/mcp_config.json` |

Without a client name all five are read. In a terminal you are asked which servers to import (`all`, numbers such as `1,3`, or Enter for none); `--yes` imports every one. A name that `~/.azazel/mcp.json` already has is never replaced without asking, and with `--yes` it is left as it is. The listing shows the command line or address only; environment values and headers are copied but not printed.

`${env:NAME}` becomes `${NAME}`. Variables azazel does not fill in, such as `${workspaceFolder}` and `${input:...}`, are copied as written and named in the listing so you can edit the entry afterwards. A project's `.mcp.json` needs no import: azazel reads it where it is.

## Tools

Each server tool becomes a tool named `mcp__<server>__<tool>`; characters outside `A-Z a-z 0-9 _ -` in either part become `_`. The server's input schema is passed through unchanged.

- A tool whose `readOnlyHint` annotation is true counts as a read; every other MCP tool counts as `exec`, so in `manual` and `accept-edits` mode you are asked before it runs. To allow tools without a prompt, add rules by name, for example `allow = ["mcp__github__*"]`; see [permissions](permissions.md).
- Results: text stays text and images stay images. Audio and other binary content is saved under the artifacts directory and the path returned. `structuredContent` is appended as JSON when the result has no text. `isError` marks the result as an error.
- Progress notifications show up as live output of the tool call. Aborting the call sends the server a cancellation.
- When a server announces that its tool or prompt list changed, the lists are fetched again.
- A server's `instructions` are added to the system prompt, up to 2000 characters per server.
- `includeTools` and `excludeTools` in the server's entry choose which of its tools exist for agents at all. A tool that is left out is not listed, not found by `tool_search` and cannot be called. The globs match the server's own tool name (`get_issue`), not `mcp__<server>__get_issue`.

### Many tools: `tool_search`

When the servers offer more tools than `defer_threshold` (default 40), counting those of servers that wait for their first call, MCP tools are no longer listed in every request. Agents get `tool_search { query, limit? }` instead: it ranks all MCP tools against the query (BM25 over names and descriptions), returns each hit with its description and input schema, and makes the hits callable from the agent's next step for the rest of the session. In a `run_code` script, `tools.search(query)` does the same search and the script can call what it found at once (see [code execution](tools.md#code-execution)).

## Resources and prompts

- `list_mcp_resources { server? }` lists resources and resource templates as `mcp://<server>/<uri>  name`. Without `server` it asks the servers that are connected; naming a `lazy` server that is not running starts it, as reading one of its resources does.
- The `read` tool accepts those addresses: `read mcp://docs/file:///guide.md`. Text and images are returned; other binary content is saved as an artifact.
- Each server prompt is a slash command `/mcp:<server>:<prompt>`. Arguments are filled by position, or by name as `key=value`; quotes group words. The prompt's text is sent to the lead as your message.

```text
/mcp:docs:summarise "release notes" tone=dry
```

## OAuth login

When an HTTP server answers 401, its state becomes `needs login` and you are told to run `/mcp login <server>`. `azazel mcp login <name>` and `/mcp login <name>` run the authorization-code flow with PKCE:

1. Azazel listens on `http://127.0.0.1:<port>/callback` (`oauth.callbackPort`, or any free port).
2. It discovers the server's authorization endpoints and registers a client dynamically, unless `oauth.clientId` names a pre-registered one.
3. It shows the address to open in your browser and waits up to 5 minutes for the redirect.
4. The tokens are stored in `~/.azazel/auth.json` under `mcp:<server url>` and refreshed automatically. The server then reconnects.

`logout` deletes the stored tokens. Stdio servers have nothing to log in to.

## Sampling, elicitation and roots

Servers can ask the client for things. Azazel answers these requests:

| Request | Behaviour |
|---|---|
| Roots | The project root and the `permissions.additional_dirs` |
| Sampling (the server asks for a model completion) | The first time a server asks, you are asked `MCP server ... asks to use your model to generate text. Allow it for this session?`. Allowed requests run on the `@smol` role, without tools; text and images in the request are kept. Without an interactive user the request is refused |
| Elicitation (the server asks you for input) | Form requests are asked field by field in the UI; URL requests show the address and ask you to confirm when done. Without an interactive user the request is declined |
| Logging | Written to `~/.azazel/logs/azazel.log` |

## Settings

```toml
[mcp]
startup_wait_ms = 250     # how long the first request waits for servers to connect
defer_threshold = 40      # above this many MCP tools, use tool_search
idle_timeout_secs = 600   # a lazy server stops after this long without a call; 0 never stops it
```

## Trust

A server entry is a command to run, so project files (`.mcp.json`, `.azazel/mcp.json`) load only in a trusted project. The terminal UI asks once per project; `-p`, `goal` and `rpc` runs use the recorded answer, or `--trust` for that run. `azazel mcp add --project` in an untrusted project writes the entry and reminds you that it will not start until the project is trusted. `~/.azazel/mcp.json` is yours and always loads.

### Approving a project's servers

Trusting a project does not start whatever servers it defines, now or later. Each server in a project file needs your approval, and the approval holds for the definition as it was when you gave it: `~/.azazel/trust.json` keeps the server's name, a hash of its entry and your decision. A server that is new, or whose entry changed in any way, is `waiting for approval` and does not start.

- The terminal UI asks at the start of a session, with one list of the waiting servers: tick the ones that may start. The others are recorded as rejected and you are not asked about them again until they change.
- Headless runs (`-p`, `goal`) leave waiting servers off. `azazel mcp list` and `azazel doctor` name them.
- `azazel mcp approve <name>...`, `azazel mcp approve --all` and `azazel mcp reject <name>` decide from the shell; `/mcp approve <server>` and `/mcp reject <server>` decide in a session, where an approved server starts at once.
- `--trust` approves every project server for that run and records nothing.

Servers in `~/.azazel/mcp.json`, in installed packages and from extensions are not subject to this.

## Serving azazel over MCP

`azazel mcp serve` turns this around: azazel is the server, on standard input and output, and another MCP client (an editor, a chat application, another agent) uses it. In the other client's configuration:

```json
{ "mcpServers": { "azazel": { "command": "azazel", "args": ["mcp", "serve"] } } }
```

It serves these tools:

| Tool | Does |
|---|---|
| `azazel_prompt` | Hands azazel a task and returns its final reply when it has finished. Arguments: `prompt`; optional `cwd` (the directory to work in, the server's own by default), `mode` (the permission mode), `model`, and `session` |
| `read`, `glob`, `grep`, `web_search`, `web_fetch` | azazel's own read-only tools, under their own names, in the directory the server was started in. See [tools](tools.md) |

Every `azazel_prompt` call runs a session of its own to the end of the prompt. The reply ends with `session: <id>`, and the structured result carries `reply` and `session_id`. Pass the id back as `session`, with the same `cwd`, to continue that conversation; `azazel sessions list` shows these sessions like any other.

The permission rules apply as they do everywhere. When a call needs your word, azazel asks through MCP elicitation: the client shows the question ("azazel wants to use `write` on notes.txt. Allow it?") with one yes-or-no field. A client without elicitation cannot be asked, so the call is refused and the agent is told; start the task with `"mode": "auto"`, or add allow rules, to let it work unasked. See [permissions](permissions.md).

Flags before the subcommand apply to every session: `azazel --model <spec> --trust mcp serve`. The read-only tools need no model; `azazel_prompt` answers with an error while none is configured.

Luau extensions can add servers with `azazel.mcp{...}`; see [extensions](extensions.md). To write a server of your own, load the built-in `mcp-builder` skill; see [skills and commands](skills-and-commands.md).
