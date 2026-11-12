# RPC mode

`azazel rpc` runs one session for another program instead of a person: an editor plugin, a web front end, a test harness. The client writes commands to azazel's standard input and reads responses and events from its standard output, one JSON object per line. Everything the interactive UI can do with a session is available: prompting, steering, aborting, changing model and mode, answering approval prompts and questions, and messaging or stopping sub-agents.

## Starting

```sh
azazel rpc --cwd /path/to/project --mode ask
```

| Flag | Effect |
|---|---|
| `--cwd <dir>` | The directory to work in |
| `--model <spec>` | The lead's model; repeat to add fallbacks |
| `--thinking <level>` | Thinking level |
| `--mode <mode>` | Starting permission mode: `plan`, `ask`, `accept-edits`, `auto` |
| `--trust` | Trust the project's configuration for this run |
| `-c` / `-r <id>` | Continue the latest session or resume one by id; these go before the subcommand: `azazel -c rpc` |

Rules of the transport:

- Each line in either direction is one JSON object, ended by a line feed. Blank input lines are ignored.
- Standard output carries protocol lines only. Logs go to the log file under `~/.azazel/logs`, and startup errors to standard error.
- The session ends when standard input closes. Open requests are then cancelled and the process exits with code 0. If the session cannot be started, the exit code is 1.
- RPC mode has no terminal to ask in, so a project that needs trust is treated as untrusted unless it was trusted before or `--trust` is passed. See [configuration](configuration.md).

## Commands

Every command has a `type` and should carry an `id` of your choosing; the response repeats it.

| `type` | Members | Does | Response `data` |
|---|---|---|---|
| `prompt` | `text`, `images?` | Starts work on the input. While the lead is busy it is queued | `null` |
| `steer` | `text` | Hands the working lead input before its next model request | `null` |
| `follow_up` | `text` | Queues input for when the lead would go idle | `null` |
| `abort` | | Stops the lead's current work | `null` |
| `get_state` | | Reports the session's state | See below |
| `set_model` | `chain` | Changes the lead's model chain, e.g. `["provider/model", "@role"]` | `{"models": [...]}`: the usable models it resolves to |
| `set_mode` | `mode` | Changes the permission mode | `{"mode": "..."}` |
| `agent_send` | `to`, `message` | Messages a sub-agent by id, or `all` | `{"agents": [...]}`: ids reached |
| `agent_stop` | `agent` | Stops a sub-agent and the agents it started, or `all` | `{"agents": [...]}`: ids stopped |
| `approval_response` | `request_id`, `decision`, `feedback?` | Answers an `approval_request` | `null` |
| `ui_response` | `request_id`, `value` | Answers a `ui_request`; `null` dismisses it | `null` |

`images` is a list of `{"mime": "image/png", "data": "<base64>"}`.

`get_state` returns an object with these members:

| Member | Meaning |
|---|---|
| `session_id`, `cwd` | The session and its working directory |
| `mode` | The permission mode |
| `busy` | Whether the lead is working |
| `turn` | The lead's request number within the current prompt; 0 before the first |
| `model` | The model that answered most recently, or `null` |
| `chain` | The lead's model chain as given, e.g. `["@default"]` |
| `thinking` | The thinking level |
| `context_tokens`, `context_window` | Context in use and the model's window |
| `usage` | Token and cost totals of the lead |
| `agents` | The roster, with the same members as the `roster` event below |

## Responses

```json
{"id": 1, "type": "response", "success": true, "data": null}
{"id": 2, "type": "response", "success": false, "error": "unknown command `dance`"}
```

A response means the command was accepted or rejected, not that the work is finished: `prompt` answers at once and the work shows up as events. A line that is not a JSON object is answered with `"id": null`. A `prompt` can be rejected, for example by a `user_prompt_submit` hook; the reason is the `error`.

## Events

Every event of every agent is written as it happens, with the agent's id added as `agent` (`lead` for the main agent, `<definition>-<n>` for sub-agents).

| `type` | Members |
|---|---|
| `agent_start` | |
| `turn_start` | `turn` (counts from 1 within each prompt) |
| `message_start` | |
| `stream` | `event`: one of `start`, `text_delta` (`index`, `delta`), `thinking_delta` (`index`, `delta`), `tool_call_start` (`index`, `id`, `name`), `tool_call_delta` (`index`, `delta`), `block_end` (`index`, `block`) |
| `message_end` | `message`: the complete assistant message |
| `tool_start` | `call_id`, `name`, `input` |
| `tool_update` | `call_id`, `text` (progress output) |
| `tool_end` | `call_id`, `output` (`content`, `is_error`, `details?`), `duration_ms` |
| `turn_end` | |
| `retry` | `attempt`, `model` |
| `fallback` | `from`, `to` |
| `rule` | `name`: a [stream rule](rules.md#stream-rules) matched; the message in progress was discarded and the request is sent again |
| `compaction` | `tokens_before`, `tokens_after` |
| `idle` | `stop`: `stop`, `length`, `tool_use`, `error` or `aborted` |
| `failed` | `error` |

One prompt produces `agent_start`, then per model request `turn_start`, `message_start`, `stream`…, `message_end`, any `tool_start` / `tool_update` / `tool_end`, `turn_end`, and finally `idle` or `failed`. A request whose reply a rule withdrew has `rule` where `message_end` would be. Stream deltas are for display; `message_end` carries the authoritative message.

Three more event types come from the session rather than an agent.

### `roster`

Sent whenever a sub-agent appears or changes state:

```json
{"type": "roster", "members": [
  {"id": "lead", "def": "lead", "parent": null, "depth": 0, "state": "running",
   "elapsed_ms": 5300, "tokens": 1840, "cost_usd": null},
  {"id": "scout-1", "def": "scout", "parent": "lead", "depth": 1, "state": "queued",
   "elapsed_ms": 0, "tokens": 0, "cost_usd": null}
]}
```

`state` is `queued`, `running`, `waiting for approval`, `idle`, `done`, `failed` or `stopped`. See [team](team.md).

### `approval_request`

Sent when a tool call needs the user's permission. The agent waits until the matching `approval_response` arrives.

| Member | Meaning |
|---|---|
| `request_id` | Id to answer with (`r1`, `r2`, …) |
| `agent` | The agent asking |
| `tool`, `input` | The tool and its full input |
| `access` | `read`, `write`, `exec` or `net` |
| `subjects` | What permission rules are matched against: paths, commands, `domain:<host>` |
| `rule` | Optional: the allow rule that `always` would add |
| `note` | Optional: a remark to show, such as a cost warning |

`decision` in the response:

| Decision | Effect |
|---|---|
| `allow` | Allow this call once |
| `always` | Allow it and add `rule` as an allow rule for the session and in `.azazel/config.local.toml` |
| `deny` | Refuse; optional `feedback` is passed to the model |

Each request can be answered once. To avoid approvals altogether, start with `--mode auto` or send `set_mode`. See [permissions](permissions.md).

### `ui_request`

Sent when an agent or extension asks the user something. Answer with `ui_response` and a `value` of the type shown; `null` (or no `value`) dismisses the request.

| `kind` | Members | Expected `value` |
|---|---|---|
| `ask` | `question`, `options`, `multi` | A list of the chosen options, or one string when there are no options |
| `confirm` | `prompt` | `true` or `false` |
| `select` | `prompt`, `options` | The index of the chosen option (the option's text is accepted too) |
| `input` | `prompt` | A string |
| `plan` | `path`, `plan` | `true` to approve the plan, or a string of feedback to revise it |
| `notify` | `level` (`info`, `warn`, `error`), `text` | None; no answer is expected |

## Example

Lines sent by the client are marked `>`, lines from azazel `<`. Long members are shortened with `…` and most `stream` lines are left out.

```
> {"id":1,"type":"prompt","text":"create hello.txt"}
< {"id":1,"type":"response","success":true,"data":null}
< {"type":"agent_start","agent":"lead"}
< {"type":"turn_start","turn":1,"agent":"lead"}
< {"type":"message_start","agent":"lead"}
< {"type":"stream","event":{"type":"tool_call_start","index":0,"id":"c1","name":"write"},"agent":"lead"}
< {"type":"message_end","message":{…},"agent":"lead"}
< {"type":"tool_start","call_id":"c1","name":"write","input":{"path":"hello.txt","content":"hello\n"},"agent":"lead"}
< {"type":"approval_request","request_id":"r1","agent":"lead","tool":"write","access":"write","subjects":["hello.txt"],"input":{"path":"hello.txt","content":"hello\n"}}
> {"id":2,"type":"approval_response","request_id":"r1","decision":"allow"}
< {"id":2,"type":"response","success":true,"data":null}
< {"type":"tool_end","call_id":"c1","output":{"content":[{"type":"text","text":"…"}],"is_error":false},"duration_ms":12,"agent":"lead"}
< {"type":"turn_end","agent":"lead"}
< {"type":"turn_start","turn":2,"agent":"lead"}
< {"type":"message_start","agent":"lead"}
< {"type":"stream","event":{"type":"text_delta","index":0,"delta":"Created hello.txt."},"agent":"lead"}
< {"type":"message_end","message":{…},"agent":"lead"}
< {"type":"turn_end","agent":"lead"}
< {"type":"idle","stop":"stop","agent":"lead"}
```

A client typically reads lines in a loop, routes `response` lines by `id`, renders events by `agent`, and answers every `approval_request` and every `ui_request` other than `notify`.

## Related

For one-shot scripting without a protocol, `azazel -p "<prompt>" --output json` prints the same agent events, one per line, and exits; see [getting started](getting-started.md). The interactive equivalents of these commands are described in [the TUI](tui.md).
