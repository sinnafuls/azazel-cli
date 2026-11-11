# MCP servers

Azazel is a Model Context Protocol client. An MCP server gives agents extra tools, readable resources and prompt templates: a database, an issue tracker, a disassembler, anything that speaks the protocol. This page covers where servers are configured, the transports, how their tools appear to agents, logins, and what happens when a server calls back into the client.

MCP is part of the default build (cargo feature `mcp`).

## Configuration files

Servers are read from these files, later ones winning when two define the same name:

| Order | File | Loaded |
|---|---|---|
| 1 | the `mcp.json` of installed [packages](packages.md) | when the package's code is enabled |
| 2 | `~/.azazel/mcp.json` | always |
| 3 | `<project>/.mcp.json` | when the project is trusted |
| 4 | `<project>/.azazel/mcp.json` | when the project is trusted |

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
    "legacy": { "type": "sse", "url": "https://example.com/sse", "enabled": false }
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

Keys Azazel does not know are ignored, so a file shared with other MCP clients loads.

### Variable expansion

`${VAR}` and `${VAR:-default}` are expanded from the environment in `command`, `args`, `env` values, `cwd`, `url`, `headers` values and the `oauth` `clientId` and `scope`. A variable that is unset and has no default disables that server; `azazel mcp list`, `/mcp` and `azazel doctor` report it as `${VAR} is not set`. Keep secrets in the environment, not in the file.

## Transports

| Type | How it connects |
|---|---|
| `stdio` | Starts the command and speaks MCP over its stdin and stdout. Its stderr goes to the log file at debug level (`azazel --debug`) |
| `http` | Streamable HTTP to `url` |
| `sse` | Tries streamable HTTP first; when the server refuses it, falls back to the legacy SSE transport |

Servers start connecting in the background when a session starts. The first request of the session waits at most `startup_wait_ms` for them; servers that come up later add their tools to a later request. A server that fails or disconnects is retried after 1, 2, 4, 8 and 16 seconds and then marked `failed` (a connection that lasted 30 seconds starts the count over). States shown by the tools below: `ready`, `connecting`, `not started`, `disabled`, `needs login`, `failed: <why>`.

## Managing servers

```bash
azazel mcp list                                   # every server and whether it connects
azazel mcp add fs -- npx -y @modelcontextprotocol/server-filesystem .
azazel mcp add docs --url https://example.com/mcp --header 'Authorization=Bearer ${DOCS_TOKEN}'
azazel mcp add fs --project -- npx -y some-server # write to <project>/.azazel/mcp.json
azazel mcp remove fs
azazel mcp login docs
azazel mcp logout docs
```

| Command | Does |
|---|---|
| `azazel mcp list` | Gives servers up to 10 seconds to connect, then prints each one's state, tool and prompt counts, command line or URL, and the file it comes from |
| `azazel mcp add <name> [--project] -- <command> [args...]` | Adds a stdio server to `~/.azazel/mcp.json`, or with `--project` to `<project>/.azazel/mcp.json` |
| `azazel mcp add <name> [--project] --url <url> [--header NAME=VALUE]...` | Adds an HTTP server |
| `azazel mcp remove <name>` | Removes the entry from the first of `<project>/.azazel/mcp.json`, `<project>/.mcp.json`, `~/.azazel/mcp.json` that has it |
| `azazel mcp login <name>` / `logout <name>` | OAuth login for an HTTP server; see below |

In the terminal UI, `/mcp` opens a list of servers; picking one offers `reconnect`, `disable`, `login` and `logout` (or `enable` for a disabled one). The same actions can be typed: `/mcp list`, `/mcp reconnect <server>`, `/mcp enable <server>`, `/mcp disable <server>`, `/mcp login <server>`, `/mcp logout <server>`. Enabling or disabling writes `enabled` into the file that defines the server.

## Tools

Each server tool becomes a tool named `mcp__<server>__<tool>`; characters outside `A-Z a-z 0-9 _ -` in either part become `_`. The server's input schema is passed through unchanged.

- A tool whose `readOnlyHint` annotation is true counts as a read; every other MCP tool counts as `exec`, so in `ask` and `accept-edits` mode you are asked before it runs. To allow tools without a prompt, add rules by name, for example `allow = ["mcp__github__*"]`; see [permissions](permissions.md).
- Results: text stays text and images stay images. Audio and other binary content is saved under the artifacts directory and the path returned. `structuredContent` is appended as JSON when the result has no text. `isError` marks the result as an error.
- Progress notifications show up as live output of the tool call. Aborting the call sends the server a cancellation.
- When a server announces that its tool or prompt list changed, the lists are fetched again.
- A server's `instructions` are added to the system prompt, up to 2000 characters per server.

### Many tools: `tool_search`

When connected servers offer more tools than `defer_threshold` (default 40), MCP tools are no longer listed in every request. Agents get `tool_search { query, limit? }` instead: it ranks all MCP tools against the query (BM25 over names and descriptions), returns each hit with its description and input schema, and makes the hits callable from the agent's next step for the rest of the session.

## Resources and prompts

- `list_mcp_resources { server? }` lists resources and resource templates as `mcp://<server>/<uri>  name`.
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
startup_wait_ms = 250   # how long the first request waits for servers to connect
defer_threshold = 40    # above this many MCP tools, use tool_search
```

## Trust

A server entry is a command to run, so project files (`.mcp.json`, `.azazel/mcp.json`) load only in a trusted project. The terminal UI asks once per project; `-p`, `goal` and `rpc` runs use the recorded answer, or `--trust` for that run. `azazel mcp add --project` in an untrusted project writes the entry and reminds you that it will not start until the project is trusted. `~/.azazel/mcp.json` is yours and always loads.

Luau extensions can add servers with `azazel.mcp{...}`; see [extensions](extensions.md). To write a server of your own, load the built-in `mcp-builder` skill; see [skills and commands](skills-and-commands.md).
