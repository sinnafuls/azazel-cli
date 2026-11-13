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
- The first line azazel writes is `{"type":"hello","protocol":1,"version":"0.2.0"}`. `protocol` grows when a change would break a client written for the version before.
- Standard output carries protocol lines only. Logs go to the log file under `~/.azazel/logs`, and startup errors to standard error.
- The session ends when standard input closes. Open requests are then cancelled and the process exits with code 0. If the session cannot be started, the exit code is 1.
- RPC mode has no terminal to ask in, so a project that needs trust is treated as untrusted unless it was trusted before or `--trust` is passed. See [configuration](configuration.md).

## Commands

Every command has a `type` and should carry an `id` of your choosing; the response repeats it.

| `type` | Members | Does | Response `data` |
|---|---|---|---|
| `prompt` | `text`, `images?` | Starts work on the input. While the lead is busy it is queued | `null` |
| `steer` | `text`, `images?` | Hands the working lead input before its next model request | `null` |
| `follow_up` | `text` | Queues input for when the lead would go idle | `null` |
| `abort` | | Stops the lead's current work | `null` |
| `get_state` | | Reports the session's state | See below |
| `set_model` | `chain` | Changes the lead's model chain, e.g. `["provider/model", "@role"]` | `{"models": [...]}`: the usable models it resolves to |
| `set_mode` | `mode` | Changes the permission mode | `{"mode": "..."}` |
| `agent_send` | `to`, `message` | Messages a sub-agent by id, or `all` | `{"agents": [...]}`: ids reached |
| `agent_stop` | `agent` | Stops a sub-agent and the agents it started, or `all` | `{"agents": [...]}`: ids stopped |
| `approval_response` | `request_id`, `decision`, `feedback?` | Answers an `approval_request` | `null` |
| `ui_response` | `request_id`, `value` | Answers a `ui_request`; `null` dismisses it | `null` |
| `set_name` | `name` | Names the session | `null` |
| `set_thinking` | `level` | Changes the thinking level: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` | `null` |
| `set_tier` | `tier` | Changes the service tier: `priority`, `standard`, `economy` | `null` |
| `compact` | `instructions?` | Summarises the conversation so far to free context; a `compaction` event follows | `null` |
| `shell` | `command`, `share?` | Runs a command in your shell, outside the permission rules. With `share` (the default) the lead is told what was run and what it printed | `{"output": "...", "status": "exit code: 0"}` |
| `btw` | `question` | Asks a question on the side, without interrupting the lead. The answer streams as `btw_delta` frames | `{"answer": "...", "model": "...", "dropped_tools?": [...]}` |
| `export` | `format?`, `path?` | Writes the session to a file: `markdown`, `html` or `jsonl`. The format comes from `format`, else from the file name, else it is Markdown; the default file is `azazel-session-<id>.<ext>` in the working directory | `{"path": "..."}` |
| `run_command` | `name`, `args?` | Runs a slash command an extension registered (`fast`, `preset`, a custom command), one of the session's reports (`cost`, `context`, `session`, `tools`, `todo`, `dirs`, `add-dir`, `rules`, `hooks`, `doctor`, `trust`, `forget-approvals`), or a skill as `skill:<name>` | `{"outcome": "done"}`, `{"outcome": "show", "text": "..."}`, or `{"outcome": "prompt", "text": "..."}` when it sent a prompt to the lead |
| `list_commands` | | What `run_command` takes | A list of `{"name", "description"}` |
| `list_files` | | The files of the working directory, relative to it, for completing `@` mentions; ignored files are left out and the list is cut at 20,000 | A list of strings |
| `list_models` | | The models of the providers you have a credential for | A list of `{"model", "context_window", "max_output", "vision", "reasoning", "cost"}` |
| `get_usage` | | What is known of each provider account's usage windows | A list of `{"provider", "account", "windows": [{"name", "used", "resets_at?", "status"}], "blocked_until?"}` |

The session commands report the saved sessions and the open one, change which session is open, or change where in it the conversation continues. Those that change something are refused while the lead works; send `abort` first.

| `type` | Members | Does | Response `data` |
|---|---|---|---|
| `new_session` | `name?` | Leaves the open session and starts a new one | The state |
| `resume_session` | `session_id` or `path` | Opens a saved session by its id (a unique prefix is enough) or by its file | The state |
| `fork_session` | `entry_id?` | Continues in a copy of the open session, cut at `entry_id` when it is given | The state |
| `list_sessions` | `query?` | The saved sessions of this project, most recently used first; with `query`, those whose messages contain it | A list of `{"id", "path", "name", "created", "updated", "first_prompt", "messages", "prompts", "pinned", "usage", "matches?"}` |
| `get_messages` | `leaf?` | The messages from the start of the session to `leaf` (the current leaf when left out) | A list of `{"entry_id", "timestamp", "message"}` |
| `get_tree` | | Every entry of the session file | A list of `{"id", "parent_id", "timestamp", "kind", "role?", "text?", "target_id?", "current"}`; `current` marks the branch that ends at the current leaf |
| `set_leaf` | `entry_id`, `before?` | Continues the conversation from another entry of the tree; with `before`, from the entry before it, which is how a prompt is gone back to | The state |
| `set_label` | `entry_id`, `label` | Labels an entry of the tree; an empty label takes the label away | `null` |
| `get_session` | | The open session as its file holds it | `{"path", "header", "entries", "leaf"}`: the file's first line, its entries as stored, and the entry the conversation continues from |
| `retry` | | Goes back to before the last prompt and sends it again; the earlier attempt stays in the file as a branch | `null` |
| `rewind` | `entry_id`, `what?` | Goes back to a checkpoint. `entry_id` names a `checkpoint` entry of the tree, or the prompt that followed it. `what` is `code` (the files), `conversation` or `both` (the default) | `null` |

`resume_session` takes `session_id` rather than `id` because `id` is the command's own id. After a `rewind` of the files only, the lead is told that files changed under it. See [sessions and checkpoints](sessions-and-checkpoints.md).

`images` is a list of `{"mime": "image/png", "data": "<base64>"}`.

`get_state` returns an object with these members:

| Member | Meaning |
|---|---|
| `session_id`, `session_file`, `name`, `cwd` | The session, its file, its name (`null` until it has one) and its working directory |
| `mode` | The permission mode |
| `busy` | Whether the lead is working |
| `turn` | The lead's request number within the current prompt; 0 before the first |
| `model` | The model that answered most recently, or `null` |
| `chain` | The lead's model chain as given, e.g. `["@default"]` |
| `thinking` | The thinking level |
| `tier` | The service tier, or `null` at the standard tier |
| `context_tokens`, `context_window` | Context in use and the model's window |
| `usage`, `requests` | Token and cost totals of the lead, and how many model requests it has made |
| `agents` | The roster, with the same members as the `roster` event below |
| `branch` | The checked-out git branch of the project, or `null` |
| `coordinator`, `goal` | Whether coordinator mode is on; the open goal in a few words, or `null` |
| `todo` | The lead's task list: `{"text", "status"}` with `pending`, `in_progress` or `done` |
| `roles` | The model each role (`default`, `smol`, ...) resolves to first |

## Responses

```json
{"id": 1, "type": "response", "success": true, "data": null}
{"id": 2, "type": "response", "success": false, "error": "unknown command `dance`"}
```

A response means the command was accepted or rejected, not that the work is finished: `prompt` answers at once and the work shows up as events. `shell`, `btw` and `run_command` answer when they are done, and other commands are answered meanwhile. A line that is not a JSON object is answered with `"id": null`. A `prompt` can be rejected, for example by a `user_prompt_submit` hook; the reason is the `error`.

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
| `account` | `provider`, `from`, `to`, `until` (Unix seconds), `limit`: the provider's account `from` ran out of allowance and requests now use `to` |
| `tier_dropped` | `model`, `tier`: the model refused the service tier and is asked without one from now on |
| `rule` | `name`: a [stream rule](rules.md#stream-rules) matched; the message in progress was discarded and the request is sent again |
| `compaction` | `tokens_before`, `tokens_after` |
| `idle` | `stop`: `stop`, `length`, `tool_use`, `error` or `aborted` |
| `failed` | `error` |

One prompt produces `agent_start`, then per model request `turn_start`, `message_start`, `stream`…, `message_end`, any `tool_start` / `tool_update` / `tool_end`, `turn_end`, and finally `idle` or `failed`. A request whose reply a rule withdrew has `rule` where `message_end` would be. Stream deltas are for display; `message_end` carries the authoritative message.

Four more frame types come from the session rather than an agent.

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

`state` is `queued`, `running`, `waiting for approval`, `idle`, `done`, `failed` or `stopped`. A running member
also carries `activity`: what it is doing, in a few words. See [team](team.md).

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
| `destructive` | Optional, `true` when the call would destroy work: approval should not be the answer a stray key gives |

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
| `input` | `prompt`, `secret?` | A string. A `secret` one (a key, a password) should not be shown as it is typed |
| `plan` | `path`, `plan` | `true` to approve the plan, or a string of feedback to revise it |
| `notify` | `level` (`info`, `warn`, `error`), `text` | None; no answer is expected |

### `user_input`

Written by a [background session](background-and-remote.md) only: what a client sent the lead,
`{"type": "user_input", "kind": "prompt", "text": "..."}` with `kind` `prompt`, `steer` or `follow_up`. It is in
the session's log and in what a client that attaches later is replayed, so both show what was asked.

### `btw_delta`

A piece of the answer to a `btw` command: `{"type": "btw_delta", "id": <the command's id>, "delta": "..."}`. The command's response carries the whole answer.

## Background sessions

A background session (`azazel bg`, see [background sessions](background-and-remote.md)) serves this protocol on
a local socket instead of standard input and output; `azazel rpc --attach <id>` connects your standard input and
output to it. Two commands exist only there:

| `type` | Members | Does | Response `data` |
|---|---|---|---|
| `attach` | `pid?`, `replay?` | The first command on a connection: takes the place of the session's one client. `replay` is `missed` (what was written since the last client left; the default), `all` or `none`; those frames follow the response, then the requests that still wait for an answer. A second client is refused, and told the `pid` the first one gave | `{"host", "pid", "session_id", "session_file"}` |
| `stop` | `now?` | Ends the session once the lead has finished what it is doing; with `now` the lead is stopped first. Any connection may send it, attached or not. The connection closes when the session has ended | `null` |

Until `attach` has been answered, every other command is refused. Responses and `btw_delta` frames go to the
client that is attached and are not logged; every other frame is appended to `~/.azazel/run/<id>.log`.

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

## A client in Python

This uses only the standard library. It sends one prompt, prints the reply as it is written, allows every tool call, and stops when the lead is idle.

```python
import json
import subprocess
import sys

azazel = subprocess.Popen(
    ["azazel", "rpc", "--cwd", "."],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, encoding="utf-8",
)

def send(command):
    azazel.stdin.write(json.dumps(command) + "\n")
    azazel.stdin.flush()

hello = json.loads(azazel.stdout.readline())
assert hello["type"] == "hello" and hello["protocol"] == 1, hello
send({"id": 1, "type": "prompt", "text": " ".join(sys.argv[1:]) or "say hello"})

for line in azazel.stdout:
    frame = json.loads(line)
    kind = frame["type"]
    if kind == "response" and not frame["success"]:
        sys.exit(frame["error"])
    elif kind == "stream" and frame["agent"] == "lead":
        if frame["event"]["type"] == "text_delta":
            print(frame["event"]["delta"], end="", flush=True)
    elif kind == "tool_start":
        print(f"\n[{frame['agent']}] {frame['name']}", file=sys.stderr)
    elif kind == "approval_request":
        send({"type": "approval_response", "request_id": frame["request_id"], "decision": "allow"})
    elif kind == "ui_request" and frame["kind"] != "notify":
        send({"type": "ui_response", "request_id": frame["request_id"], "value": None})
    elif kind == "failed" and frame["agent"] == "lead":
        sys.exit(frame["error"])
    elif kind == "idle" and frame["agent"] == "lead":
        break

print()
azazel.stdin.close()
azazel.wait()
```

## A client in Rust

Two crates of the workspace are for clients. Neither depends on the rest of azazel.

- `azazel-protocol` has the types of every line: `Command`, `Frame`, and the `data` of the responses (`State`, `SessionRow`, `TreeRow`, ...). A `Frame` it does not know reads as `Frame::Unknown`, so a client keeps working with a newer azazel.
- `azazel-client` starts `azazel rpc` (`Client::spawn`) or connects to one over any pair of byte streams (`Client::connect`), sends commands (`send`), hands out frames (`frames`), and runs one prompt to its end (`prompt_and_wait`).

```rust
use azazel_client::protocol::{Command, Decision, Frame};
use azazel_client::{Client, Options};

let client = Client::spawn(Options::new("azazel")).await?;
let run = client
    .prompt_and_wait("fix the failing test", |frame| match frame {
        Frame::ApprovalRequest { request_id, .. } => Some(Command::ApprovalResponse {
            request_id: request_id.clone(),
            decision: Decision::Allow,
            feedback: None,
        }),
        _ => None,
    })
    .await?;
println!("{}", run.text);
client.shutdown().await?;
```

`examples/client-demo` is a complete program: `cargo run -p client-demo -- --allow "create hello.txt"`.

## Schema

[`rpc.schema.json`](rpc.schema.json) is the JSON Schema of every line and of the responses' `data`, generated from `azazel-protocol` with `cargo xtask rpc-schema`. A test fails when the file is out of date.

## Related

For one-shot scripting without a protocol, `azazel -p "<prompt>" --output json` prints the same agent events, one per line, and exits; see [getting started](getting-started.md). The interactive equivalents of these commands are described in [the TUI](tui.md).
