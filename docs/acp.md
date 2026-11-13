# Agent Client Protocol

`azazel acp` makes azazel an agent of the [Agent Client Protocol](https://agentclientprotocol.com/) (ACP), the protocol editors such as Zed speak to coding agents. The editor starts azazel, sends it prompts, shows what it does, and asks you when a tool call needs your permission. azazel speaks protocol version 1: JSON-RPC on standard input and output.

azazel works the way it does in the terminal: it reads files and runs commands itself, with the same tools, permission rules, skills, hooks and MCP servers. Only the interface is the editor's.

## Setting up an editor

Point the editor at the `azazel` binary with the argument `acp`. In Zed's `settings.json`:

```json
{
  "agent_servers": {
    "azazel": { "command": "azazel", "args": ["acp"] }
  }
}
```

Flags that go before a subcommand apply to every session the editor opens:

```sh
azazel --model anthropic/claude-fable-5 --mode accept-edits acp
```

There is nothing to sign in to from the editor. azazel uses the logins and keys it already has (`azazel login`, `azazel init`, environment variables); see [models and logins](models-and-logins.md). A session cannot start without a model: the editor then shows azazel's error, which names `azazel init`.

## Sessions

Each editor session is an azazel session in the directory the editor names, stored like any other: `azazel sessions list` shows it, and `azazel --session <id>` opens it in the terminal. Several sessions in different projects can be open at once; each has its own configuration, tools and servers.

| The editor sends | azazel does |
|---|---|
| `session/new` with `cwd` and `mcpServers` | Starts a session in `cwd`. The MCP servers the editor brings (stdio, HTTP and SSE) are added to that session only |
| `session/load` with a session id | Opens the stored session and replays its conversation to the editor before it answers |
| `session/prompt` | Runs the prompt to its end. Text, images and embedded files become one message; an embedded text file is attached the way an `@path` mention attaches one |
| `session/cancel` | Stops the work; the prompt ends as `cancelled` |
| `session/set_mode` | Changes the permission mode |

A prompt that arrives while another runs cancels that one first.

A project's own configuration counts only when the project is trusted: nobody is asked in this mode, so trust it once from a terminal (`azazel` there asks), or pass `--trust`. See [configuration](configuration.md).

## What the editor shows

While a prompt runs azazel sends `session/update` notifications:

| Update | When |
|---|---|
| `agent_message_chunk`, `agent_thought_chunk` | The lead writes or thinks |
| `tool_call` | The lead starts a tool. `kind` is `read`, `edit`, `search`, `execute`, `fetch` or `other`; `locations` names the file |
| `tool_call_update` | The tool ended: `completed` or `failed`, with what it returned. After `write` and `edit` the content includes a diff of the whole file (`oldText`, `newText`) |
| `plan` | The lead changed its task list (the `todo` tool) |
| `current_mode_update` | The permission mode changed, also when azazel changes it itself: an approved plan leaves plan mode |
| `session_info_update` | The session got its name |

The work of sub-agents shows as the lead's `task` call and its result, not step by step.

The prompt's answer carries why the lead stopped: `end_turn`, `max_tokens`, `max_turn_requests` (the turn cap was reached while tools were still wanted), `cancelled`, or `refusal` when the model's reply ended in an error. A run that fails outright is a JSON-RPC error with the reason.

## Permissions and questions

The permission modes are offered as the session's modes: `plan`, `ask`, `accept-edits` and `auto`. See [permissions](permissions.md).

When a call needs your word, azazel sends `session/request_permission` for it with these options:

| Option | Effect |
|---|---|
| `allow_once` | Allow this call |
| `allow_always` | Allow it and remember the rule, as "always" does in the terminal. Offered when a rule can express the call |
| `reject_once` | Refuse it |

Dismissing the request refuses the call.

Questions use the same request, with one option per choice: the `ask_user` tool's choices, a yes-or-no confirmation, and the review of a plan (approve it, or keep planning). A question that needs a typed answer cannot be asked this way; the agent is told nobody can answer it.

## Not in this version

- azazel does not use the editor's `fs/*` and `terminal/*` methods: it reads, writes and runs commands on its own, so unsaved editor buffers are not seen.
- Audio in prompts is ignored.
- Slash commands are not offered to the editor.

## Building without it

ACP is the Cargo feature `acp` of `azazel-cli`, on by default.
