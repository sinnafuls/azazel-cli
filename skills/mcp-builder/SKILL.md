---
name: mcp-builder
description: Use when writing, extending or debugging a Model Context Protocol server so that agents can use a service, a data source or a local program as tools. Covers tool design, the stdio and HTTP transports, and testing the server against this harness.
---
# Building an MCP server

An MCP server gives agents typed tools, readable resources and reusable prompts. The model sees only names, descriptions and schemas, so most of the quality of a server is in those three things.

## 1. Decide what to expose

- List the tasks an agent should be able to complete, not the endpoints of the system behind the server. One tool per task step the model would naturally take.
- Prefer a few capable tools over a mirror of an API with fifty. Every tool costs context and attention.
- Separate reading from changing. Read tools can be called freely; tools that change something should say so and do one thing.
- Decide what must never be exposed: destructive bulk operations, credentials, raw query access.

| Capability | Use for |
|---|---|
| Tools | Actions and queries with arguments. The default choice |
| Resources | Content addressed by URI that an agent may read: files, records, documents |
| Prompts | Parameterised instructions a user starts on purpose; they appear as slash commands |

## 2. Design each tool

**Name**: lower case with underscores, verb first, specific: `search_issues`, `create_invoice`. Group with a prefix when a server covers several areas.

**Description**: written for a model that has never seen the system. Say what the tool does, when to use it and when not to, what it returns, and any limit. Two to five sentences.

**Input schema**: JSON Schema, as tight as the truth allows.
- Every property has a `description` with format and an example value.
- Use `enum` for closed sets, `minimum`/`maximum` for ranges, `required` for what is required. Give defaults.
- Flat objects. Avoid deep nesting and unions the model must guess between.
- Accept the identifiers the model will actually have (a name or URL as well as an internal id), or provide a search tool that returns the id.

**Output**:
- Text the model can use directly: concise, with the identifiers needed for follow-up calls.
- Bound the size. Paginate with a cursor argument and say in the result how to get more. A tool that can return megabytes will fill the context.
- For data, return compact JSON or a short table, not prose. Provide `structuredContent` with an output schema when clients benefit from typed results.
- Images as image content; large binary data as a resource link.

**Errors**: return a tool result with `isError: true` and a message that says what was wrong and how to fix the call ("`status` must be one of open, closed; got `done`"). Protocol-level errors are for malformed requests, not for failed operations.

**Annotations**: set `readOnlyHint: true` on tools that only read. This harness treats such tools as reads (no approval prompt inside the project's permission mode) and everything else as an action that asks first. Set `destructiveHint` and `idempotentHint` truthfully.

## 3. Choose a transport

| Transport | Use when | Notes |
|---|---|---|
| stdio | The server runs on the user's machine as a child process | Simplest. Protocol on stdout; all logging on stderr. Never print anything else to stdout |
| Streamable HTTP | The server is shared or remote | One endpoint handling POST and optional event streams; authentication with OAuth or a bearer header |

Start with stdio. Use an official SDK for the language (TypeScript, Python, Rust, Go, C#, Java) rather than writing the protocol by hand; pin its version and read its current README for the API.

## 4. Implement

1. Create the server with a name and version, and declare only the capabilities you implement.
2. Register tools with name, description, input schema and handler. Validate input again inside the handler; do not trust the schema to have been enforced.
3. Read configuration and secrets from environment variables, never from tool arguments. Fail at start-up with a clear message on stderr when something required is missing.
4. Make handlers safe to cancel and safe to retry. Apply timeouts to calls into the backing system and report progress for long work when the client sent a progress token.
5. Respect roots when the server touches the file system: stay inside the directories the client lists.
6. Keep handlers free of global mutable state unless guarded; calls can arrive concurrently.
7. Add instructions (a short paragraph in the server's initialise result) only for guidance that applies across tools.

## 5. Connect it to this harness

Add the server to an `mcp.json` (`~/.azazel/mcp.json` for every project, `<project>/.azazel/mcp.json` or `<project>/.mcp.json` for one), or with the command line:

```sh
azazel mcp add notes -- node ./dist/server.js
azazel mcp add docs --url https://example.com/mcp --header "Authorization=Bearer ${DOCS_TOKEN}"
azazel mcp list
```

```json
{
  "mcpServers": {
    "notes": { "command": "node", "args": ["./dist/server.js"], "env": { "NOTES_DIR": "${HOME}/notes" } },
    "docs": { "type": "http", "url": "https://example.com/mcp" }
  }
}
```

`${VAR}` and `${VAR:-default}` expand in every string. Project files are used only after the project is trusted.

In a session:

- `/mcp` shows each server's state, tool count, and offers reconnect, enable and disable. `/mcp list` prints the same as text.
- Tools appear to agents as `mcp__<server>__<tool>`. With many tools connected they are found through `tool_search` instead of being listed up front, which makes names and descriptions matter even more.
- Resources are read with the `read` tool at `mcp://<server>/<resource uri>` and listed with `list_mcp_resources`.
- Prompts appear as `/mcp:<server>:<prompt>`.
- A remote server that answers 401 is logged in to with `azazel mcp login <name>`.

## 6. Test

Work from the inside out.

1. **Unit-test handlers** as ordinary functions: valid input, each validation failure, backend errors, empty and large results.
2. **Protocol smoke test** over stdio without any client. Send initialise, the initialised notification, then a call, one JSON object per line:

   ```sh
   printf '%s\n' \
     '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}' \
     '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
     '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
     | node ./dist/server.js
   ```

   The output must be only JSON-RPC lines. Anything else on stdout is a bug.
3. **Against the harness**: add the server, start a session, and check `/mcp` shows it `ready` with the expected tool count. Ask the agent to perform each task from step 1 in plain words, without naming tools. Watch which tools it picks and how it fills arguments.
4. **Read the failures**: a wrong tool choice means descriptions overlap; wrong arguments mean the schema or its descriptions are unclear; a retry loop means an error message does not say how to fix the call. Change the description or schema, not the prompt.
5. **Abuse**: kill the backend, return a huge result, call with missing arguments, cancel mid-call, send two calls at once. The server must answer with errors, not hang or exit.
6. **Restart**: stop the server process during a session; the harness reconnects with backoff, and the next call should succeed.

Logs: the server's stderr and the harness's side of the conversation are in `~/.azazel/logs/azazel.log` (run with `--debug` for more).

## 7. Before sharing

- A README with what the server does, its tools, required environment variables and the `mcp.json` entry to copy.
- No secrets in the repository or in tool output.
- Versions pinned; a lockfile committed.
- The destructive tools are few, clearly named, and annotated.
- Tested on the platforms users run, including Windows paths and line endings for stdio servers.
