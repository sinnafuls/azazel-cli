# RPC mode

`azazel rpc` runs one session for another program instead of a person: an editor plugin, a web front end, a test harness. The client writes commands to azazel's standard input and reads responses and events from its standard output, one JSON object per line. Everything the interactive UI can do with a session is available: prompting, steering, aborting, changing model and mode, answering approval prompts and questions, and messaging or stopping sub-agents.

## Starting

```sh
azazel rpc --cwd /path/to/project --mode manual
```

| Flag | Effect |
|---|---|
| `--cwd <dir>` | The directory to work in |
| `--model <spec>` | The lead's model; repeat to add fallbacks |
| `--thinking <level>` | Thinking level |
| `--mode <mode>` | Starting permission mode: `plan`, `manual`, `accept-edits`, `auto`, `bypass` |
| `--trust` | Trust the project's configuration for this run |
| `-c` / `-r <id>` | Continue the latest session or resume one by id; these go before the subcommand: `azazel -c rpc` |

A session whose last process was killed in the middle of a run says so in `get_state` (`interrupted`); `continue_run` goes on with it. See [interrupted runs](sessions-and-checkpoints.md#interrupted-runs).

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
| `prompt` | `text`, `images?`, `request_id?` | Starts work on the input. While the lead is busy it is queued. With a `request_id` the prompt can be sent again safely: see [named prompts](#named-prompts) | `null`; with a `request_id`, the run's state |
| `continue_run` | | Goes on with the run that the end of the session's process cut short: tool calls without a result get one, and the request the model never answered is sent again. Refused when no run was interrupted or the lead works | The run's state: `{"run_id", "state": "running", "request_id?"}` |
| `steer` | `text`, `images?` | Hands the working lead input before its next model request | `null` |
| `follow_up` | `text` | Queues input for when the lead would go idle | `null` |
| `abort` | | Stops the lead's current work | `null` |
| `get_state` | | Reports the session's state | See below |
| `set_model` | `chain` | Changes the lead's model chain, e.g. `["provider/model", "@role"]` | `{"models": [...]}`: the usable models it resolves to |
| `set_mode` | `mode` | Changes the permission mode of the session, for the lead and every sub-agent, running ones included: `plan`, `manual`, `accept-edits`, `auto` or `bypass`. `ask`, the old name of `manual`, is an error that names `manual`. `bypass` lifts every guard but deny rules and the push guard: a client that offers it should have its user confirm first | `{"mode": "..."}` |
| `set_agent` | `agent` | Makes the lead another [primary agent](team.md#primary-agents) (`build`, `plan`, or one of your definitions with `mode: primary`): its identity, tools, limits, model and thinking level apply from the next request, and the switch is written to the session. A name that is no primary agent is an error that lists them | `{"agent": "..."}` |
| `agent_send` | `to`, `message` | Messages a sub-agent by id, or `all` | `{"agents": [...]}`: ids reached |
| `agent_stop` | `agent` | Stops a sub-agent and the agents it started, or `all` | `{"agents": [...]}`: ids stopped |
| `agent_mention` | `name`, `text` | `@name text` as the user types it at a prompt: sends `text` to the sub-agent with the id `name`, or starts an agent from the definition of that name with `text` as its task (its report goes to the lead) | `{"agent": "scout-1", "started": true}`, or `null` when `name` is neither an agent nor a definition |
| `approval_response` | `request_id`, `decision`, `feedback?` | Answers an `approval_request` | `null` |
| `ui_response` | `request_id`, `value` | Answers a `ui_request`; `null` dismisses it | `null` |
| `set_name` | `name` | Names the session | `null` |
| `set_thinking` | `level` | Changes the lead's thinking level for the session: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. It wins over the `:level` of the lead's role | `null` |
| `set_tier` | `tier` | Changes the service tier: `priority`, `standard`, `economy` | `null` |
| `compact` | `instructions?` | Summarises the conversation so far to free context; a `compaction` event follows | `null` |
| `shell` | `command`, `share?` | Runs a command in your shell, outside the permission rules. With `share` (the default) the lead is told what was run and what it printed | `{"output": "...", "status": "exit code: 0"}` |
| `handoff` | `focus?` | Hands the work to a new session: keeps a draft brief made without a model (`handoff-<session id>.md` in the session's folder), asks the lead's model once for a structured brief, then switches to a new session whose first message is that brief. Refused while the lead works, while another hand-off is being written, or when there is nothing to hand off. While the brief is being written (`busy` is true) `prompt`, `steer`, `follow_up` and `continue_run` are refused too, and `abort` cancels the hand-off (its response is an error). The draft is deleted once the new session is open and stays when the request fails. See [handoff](sessions-and-checkpoints.md#handoff) | `{"session_id": "<the new session>", "from": "<the old one>"}` |
| `btw` | `question` | Asks a question on the side, without interrupting the lead. The answer streams as `btw_delta` frames | `{"answer": "...", "model": "...", "dropped_tools?": [...]}` |
| `export` | `format?`, `path?` | Writes the session to a file: `markdown`, `html` or `jsonl`. The format comes from `format`, else from the file name, else it is Markdown; the default file is `azazel-session-<id>.<ext>` in the working directory | `{"path": "..."}` |
| `run_command` | `name`, `args?`, `only?` (a list of `source`s: the command runs only if it comes from one of them, else it is refused) | Runs a slash command an extension registered (`fast`, `preset`, a custom command), one of the session's reports (`cost`, `context` (also `context json` and `context text <request> <category>`; see [the context inspector](tui.md#the-context-inspector)), `session`, `tools`, `todo`, `dirs`, `add-dir`, `rules`, `hooks`, `doctor`, `trust`, `forget-approvals`), or a skill as `skill:<name>` | `{"outcome": "done"}`, `{"outcome": "show", "text": "..."}`, or `{"outcome": "prompt", "text": "..."}` when it sent a prompt to the lead |
| `list_commands` | | What `run_command` takes | A list of `{"name", "description", "source": "builtin\|skill\|mcp\|custom\|script"}` |
| `list_files` | | The files of the working directory, relative to it, for completing `@` mentions; ignored files are left out and the list is cut at 20,000 | A list of strings |
| `list_models` | `all?` | The models of the providers you have a credential for; with `all: true`, of every provider. A provider `refresh_models` asked has its rows from that answer | A list of [models](#models-and-providers) |
| `list_providers` | | Every provider, whether it can be used, and how | A list of [providers](#models-and-providers) |
| `refresh_models` | `provider` | Asks the provider for the models it serves now; the answer stands for the rest of the session and is kept in `~/.azazel/cache/models.json` for later ones | A list of [models](#models-and-providers) of that provider |
| `list_roles` | | Every [model role](models-and-logins.md#roles): its chain, where that comes from, what it resolves to, and the saved presets | [Roles](#roles) |
| `set_role` | `role`, `chain?`, `save?` | Points `role` at `chain` (`["provider/model:high", "@slow"]`), or without `chain` takes away the value `save` names so the role falls back to the next config file or its built-in value. `save` is `session` (the default: this session only), `home` (also writes `~/.azazel/config.toml`) or `project` (the project's `.azazel/config.toml`, refused when the project is not trusted). Setting `default` or `lead` puts the lead back on its role | [Roles](#roles) |
| `save_preset` | `name` | Writes every role as it is now under `[presets.<name>.roles]` in the home config | [Roles](#roles) |
| `apply_preset` | `name` | The old name of `set_profile`: points every role of a profile at its models for this session. Sends `profile_changed` too | [Roles](#roles) |
| `set_profile` | `name` | Moves the session to a [profile](models-and-logins.md#profiles), built-in or from the configuration: each role it names is pointed at its models and the lead follows its role again. A name that is no profile, or a profile whose providers have no login or do not exist, is refused with the reason. Sends a `profile_changed` frame | The new state (see `get_state`) |
| `save_profile` | `name` | Writes every role as it is now under `[profiles.<name>.roles]` in the home config | The new state (see `get_state`) |
| `get_usage` | | What is known of each provider account's usage windows | A list of `{"provider", "account", "windows": [{"name", "used", "resets_at?", "status", "scope?"}], "blocked_until?", "blocks?": [{"until", "since", "window?", "scope?"}]}`. A window or block with a `scope` is about one model family only; `blocked_until` is about every model |
| `get_settings` | | Every setting of the terminal's settings screen | `{"settings": [{"path", "page", "group", "label", "kind": "switch\|choice\|theme\|number\|text\|list\|table\|role", "choices?", "live", "description", "value", "default", "source?": "session\|flag\|env\|local\|project\|home", "gated"}], "pages", "project", "home_config", "project_config"}` |
| `set_setting` | `path`, `value`, `scope?` (`session`, `home`, `project`; default `session`) | Changes one setting; `value: null` takes it out of the file `scope` names. `session` changes nothing on disk. A session with a terminal UI takes every `live` setting; any other session (`azazel rpc`, a background session) takes only `tui.theme`, by name (`auto`, a built-in theme or one in the home's `themes` directory, never a path), and refuses the rest with a message. `project` needs a trusted project. A change of `tui.theme` sends a `theme` frame before the response (the frame is left out, and the response still succeeds, when the theme cannot be read) | `{"setting": <row of get_settings>, "message"}` |
| `get_theme` | `light?` | The colours of the configured theme | `{"name", "light", "roles": {"text": "#rrggbb", ...}}` |
| `get_context` | | What the lead's last request was made of | `null` before a request, else `{"agent", "request", "model?", "window", "estimate", "reported?", "tokens", "categories": [{"kind", "name", "label", "items", "estimate", "tokens"}]}` |
| `get_cost` | | Tokens and money of the session | `{"requests", "usage", "note?", "cache_hit_rate?", "models": [{"model", "requests", "input", "cache_read", "cache_write", "hit_rate?"}], "side": [{"purpose", "usage"}], "warming?"}` |
| `trust_project` | `trusted?` (default true) | As `/trust yes` or `/trust no` | `{"root", "trusted", "saved", "message"}` |
| `add_dir` | `path` | As `/add-dir` | `{"dirs", "message"}` |
| `forget_approvals` | | As `/forget-approvals` | `{"count", "message"}` |
| `browser_watch` | `on` | Starts or stops receiving `browser_frame` frames, a live picture of the tab the agents' browser shows; frames are produced only while someone watches. See [`browser_frame`](#browser_frame) | A [browser state](#browser_state) |
| `browser_take` | `on` | Takes the agents' browser over, or hands it back. While it is taken over the agents' next browser action waits, and only this client's `browser_input` is accepted; when it is handed back the agent is told what you did. Refused while another client holds it | A [browser state](#browser_state) |
| `browser_input` | `input` | Clicks, types, scrolls or navigates in the agents' browser. `input` has a `kind`: `click` (`x`, `y`, `button?`, `count?`), `scroll` (`x`, `y`, `dy`), `key` (`keys`, as the browser tool's `press`), `text` (`text`), `navigate` (`url`), `back` or `reload`. `x` and `y` are fractions (0 to 1) of the frame from its top left corner. Refused unless this client has taken the browser over | A [browser state](#browser_state) |
| `browser_sign_in` | `url?` | Shows the agents' own browser profile in a visible window on the machine that runs azazel, to sign in to sites, opened on `url` (http or https) when given | A [browser state](#browser_state) |

The session commands report the saved sessions and the open one, change which session is open, or change where in it the conversation continues. Those that change something are refused while the lead works; send `abort` first.

| `type` | Members | Does | Response `data` |
|---|---|---|---|
| `new_session` | `name?` | Leaves the open session and starts a new one | The state |
| `resume_session` | `session_id` or `path` | Opens a saved session by its id (a unique prefix is enough) or by its file | The state |
| `fork_session` | `entry_id?` | Continues in a copy of the open session, cut at `entry_id` when it is given | The state |
| `list_sessions` | `query?` | The saved sessions of this project, most recently used first; with `query`, those whose messages contain it | A list of `{"id", "path", "name", "created", "updated", "first_prompt", "messages", "prompts", "pinned", "usage", "matches?"}` |
| `get_messages` | `leaf?`, `before?`, `limit?` | The messages from the start of the session to `leaf` (the current leaf when left out). With `limit`, only the last `limit` of them; with `before` (the `entry_id` of a message on the branch), the last `limit` messages before that one, so a client can load the history in pages. An unknown `before` is an error | A list of `{"entry_id", "timestamp", "message"}`, oldest first |
| `get_agent_messages` | `agent`, `before?`, `limit?` | The messages of the transcript of sub-agent `agent`, paged like `get_messages`. An agent that has written nothing yet has none | A list of `{"entry_id", "timestamp", "message"}`, oldest first |
| `get_tree` | | Every entry of the session file | A list of `{"id", "parent_id", "timestamp", "kind", "role?", "text?", "target_id?", "current"}`; `current` marks the branch that ends at the current leaf, and the journal entries of its runs (`run_started`, ...), which hang from it |
| `set_leaf` | `entry_id`, `before?` | Continues the conversation from another entry of the tree; with `before`, from the entry before it, which is how a prompt is gone back to. When that leaves a branch the agent worked on, a [summary of it](sessions-and-checkpoints.md#branch-summaries) is added to the conversation a moment later and announced with a `notify` request | The state |
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
| `via` | The [virtual model](models-and-logins.md#virtual-models) (`virtual/<name>`) that picked `model` for that request; absent when the chain named the model itself |
| `virtual_models` | The virtual models extensions define, as `virtual/<name>`; `set_model` takes them. Absent when there are none |
| `chain` | The lead's model chain as given, e.g. `["@default"]` |
| `thinking` | The thinking level the lead's requests go out at: the one chosen for the session (`set_thinking`, `--thinking`, the primary agent's), else the `:level` of the model its chain asks first, else `off` |
| `tier` | The service tier, or `null` at the standard tier |
| `profile` | The [profile](models-and-logins.md#profiles) the session last moved to, or the one `--profile` named; absent when there is none |
| `profiles` | The profiles `set_profile` takes, built-in ones first: `{"name", "source" (`builtin`, `user` or `project`), "usable", "missing_logins"?, "unknown_providers"?, "roles"}`. `usable` is false when a provider the profile names has no login or is not a provider of this session (a typo, an extension that is not installed); `missing_logins` and `unknown_providers` list those providers; `roles` maps each role the profile sets to its first model |
| `context_tokens`, `context_window` | Context in use and the model's window |
| `usage`, `requests` | Token and cost totals of the lead, and how many model requests it has made |
| `agents` | The roster, with the same members as the `roster` event below |
| `branch` | The checked-out git branch of the project, or `null` |
| `coordinator`, `goal` | Whether coordinator mode is on; the open goal in a few words, or `null` |
| `agent`, `primary_agents` | The primary agent the lead is; the ones `set_agent` takes, as `{"name", "description", "color"}` (`color` is `#rrggbb`, absent when the definition gives none), in the order a client steps through them |
| `todo` | The lead's task list: `{"text", "status"}` with `pending`, `in_progress` or `done` |
| `roles` | The model each role (`default`, `smol`, ...) resolves to first |
| `interrupted` | Present when the session's last run was cut short by the end of a process and `continue_run` can go on with it: where it stopped, in a few words (`stopped while shell ran`) |
| `show_thinking` | Whether the terminal shows the model's thinking right now: the `tui.show_thinking` setting as the terminal's thinking key (`Ctrl+T`) left it. A process without a terminal reports the setting. The web page follows it unless its own setting says otherwise |
| `checked` | Whether the code was checked since it was last changed: absent until a file other than prose is changed, `false` after a change, `true` once a build, test or lint command ran after it. Changes and checks of every agent of the session count |
| `cache_left_secs`, `cache_ttl_secs` | Seconds until the provider drops the lead's prompt cache unless a request reads it first (`0`: it has), and how long it keeps it. Absent while the lead works and when the cache is too small to matter. A client that shows a clock counts down from when the answer arrived |

### Roles

`list_roles`, `set_role`, `save_preset` and `apply_preset` answer with the roles as they are after the command:

| Member | Meaning |
|---|---|
| `roles` | One `{"role", "chain", "source", "builtin?", "resolved", "thinking?"}` per role, the built-in roles first |
| `presets` | The names of the saved presets |
| `project` | Whether `set_role` may write the project's config (the project is trusted) |

Where `set_role` keeps a change is its `save` member, one of three words (`SaveTo` in the schema): `session`
changes the running session only, `home` writes `~/.azazel/config.toml` too, and `project` writes the project's
`.azazel/config.toml`, which a project that is not trusted refuses. Taking a value away (no `chain`) removes it from
the file `save` names. The other three commands do not take it: `save_preset` always writes the home config, and
`apply_preset` and `set_profile` change the session only.

In a role, `chain` is the value as configured, `resolved` the usable models it comes to in order, `builtin` the value it falls back to when nothing sets it, and `thinking` the level written with its first model (`provider/model:high`). `source` says where the chain comes from: `builtin`, `home`, `project`, `project_local`, `session` (changed in this session and not saved) or `unset` (nothing sets it, so it resolves to no model).

### Models and providers

A model of `list_models` and `refresh_models`:

| Member | Meaning |
|---|---|
| `model`, `provider`, `id` | `provider/id`, and its two parts |
| `name?` | What the model is called, when that is more than its id |
| `context_window`, `max_output` | Tokens |
| `vision`, `reasoning` | Whether it takes images, and whether it thinks |
| `thinking_levels?` | The thinking levels it takes; absent for every level when `reasoning` is set |
| `cost` | `{"input", "output"}` in USD per million tokens, or `null` when not known |
| `usable` | Its provider has a credential, or needs none |
| `login` | Its provider is used through a subscription sign-in, so it is not paid by the token |

A provider of `list_providers` is `{"id", "name", "usable", "login", "source?", "env?", "oauth", "models"}`:
`source` says where its credential comes from (`OPENAI_API_KEY`, `auth.json (login)`), `env` the variables a
key is read from, `oauth` whether it can be signed in to, and `models` how many models it has.

### Named prompts

A client that cannot tell whether its `prompt` arrived (a connection that dropped, a retry after a timeout) names it: `request_id` is any string of the client's choosing. The first `prompt` with a given `request_id` starts a run and answers `{"request_id": "...", "state": "queued"}`. Any later `prompt` with the same `request_id` starts nothing, whatever its text, and answers with where that run stands:

| Member | Meaning |
|---|---|
| `request_id` | The name |
| `run_id` | The run as the session file records it; absent while the lead has not taken the prompt up |
| `state` | `queued` (the lead has not taken it up yet), `running`, `ended`, or `interrupted` (the process that ran it ended first; `continue_run` goes on with it) |
| `duplicate` | `true`: this is not the first prompt of that name |
| `stop` | For an ended run: the `stop` of its `idle` event, or `failed` |
| `text` | The last thing the lead said in the run |

The events of a run that is still going on arrive on the connection as they would have; a run that has ended is read from the response. The names are kept in the session file, so they hold for as long as the session does, across processes. `azazel bg --request-id <id>` and the `_meta.requestId` of an ACP prompt do the same for those two ways in (see [background sessions](background-and-remote.md) and [ACP](acp.md)).

## Responses

```json
{"id": 1, "type": "response", "success": true, "data": null}
{"id": 2, "type": "response", "success": false, "error": "unknown command `dance`"}
```

A response means the command was accepted or rejected, not that the work is finished: `prompt` answers at once and the work shows up as events. `shell`, `btw` and `run_command` answer when they are done, and other commands are answered meanwhile. A line that is not a JSON object is answered with `"id": null`. A failed response may carry a `code`: `refused` means the server does not allow this device the command (`azazel remote` for a browser). A `prompt` can be rejected, for example by a `user_prompt_submit` hook; the reason is the `error`.

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
| `retry` | `attempt`, `model`, `stalled_secs?`: the message in progress was discarded and the same model is asked again; `stalled_secs` is there when the model had sent nothing for that long ([stalls](models-and-logins.md#stalls)) |
| `fallback` | `from`, `to` |
| `account` | `provider`, `from`, `to`, `until` (Unix seconds), `limit`: the provider's account `from` ran out of allowance and requests now use `to` |
| `profile_changed` | `from?`, `to`, `reason` (`user`, `limit` or `reset`), `until?`, `limit?`: the session moved to profile `to`. `until` (Unix seconds, when the window resets) and `limit` (what ran out, such as `5-hour limit`) are there for `limit`; `reset` is the move back after a window has reset (`[profiles] on_reset = "auto"`). A switch on a usage limit or a reset is told by this frame alone, with no `notify` request besides. Not tied to an agent |
| `tier_dropped` | `model`, `tier`: the model refused the service tier and is asked without one from now on |
| `rule` | `name`: a [stream rule](rules.md#stream-rules) matched, or the reply looped (`runaway`, see [guards](tools.md#guards)); the message in progress was discarded and the request is sent again |
| `compaction` | `tokens_before`, `tokens_after` |
| `idle` | `stop`: `stop`, `length`, `tool_use`, `error` or `aborted` |
| `failed` | `error` |

One prompt produces `agent_start`, then per model request `turn_start`, `message_start`, `stream`…, `message_end`, any `tool_start` / `tool_update` / `tool_end`, `turn_end`, and finally `idle` or `failed`. A request whose reply a rule withdrew has `rule` where `message_end` would be. Stream deltas are for display; `message_end` carries the authoritative message.

Eight more frame types come from the session rather than an agent.

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
| `rule` | Optional: the allow rule that `session` and `always` would add. Without it both act as `allow` |
| `note` | Optional: a remark to show, such as a cost warning |
| `destructive` | Optional, `true` when the call would destroy work: approval should not be the answer a stray key gives |
| `secret` | Optional, `true` when the call reads or writes a [secret file](permissions.md#secret-files): `session` allows that file until the session ends, and `always` saves nothing |

`decision` in the response:

| Decision | Effect |
|---|---|
| `allow` | Allow this call once |
| `session` | Allow it and add `rule` as an allow rule until the process ends; nothing is written to a file |
| `always` | Allow it and add `rule` as an allow rule for the session and in `.azazel/config.local.toml` |
| `deny` | Refuse; optional `feedback` is passed to the model |

Each request can be answered once. To avoid approvals altogether, start with `--mode auto` or send `set_mode`. See [permissions](permissions.md).

### `ui_request`

Sent when an agent or extension asks the user something. Answer with `ui_response` and a `value` of the type shown; `null` (or no `value`) dismisses the request.

| `kind` | Members | Expected `value` |
|---|---|---|
| `ask` | `question`, `context?`, `code?`, `diff?`, `expects`, `options`, `multi`, `recommended?`, `allow_freeform?` | What `expects` names: a list of the chosen options for `select` and `multiselect`, a string for `text`, `true` or `false` for `confirm`. See below |
| `form` | `questions`: a list of questions, each with the members of an `ask` and an optional `header` | A list with one entry per question, in their order: what an `ask` of that question expects, or `null` for a question the user left open |
| `confirm` | `prompt` | `true` or `false` |
| `select` | `prompt`, `options` | The index of the chosen option (the option's text is accepted too) |
| `input` | `prompt`, `secret?` | A string. A `secret` one (a key, a password) should not be shown as it is typed |
| `plan` | `path`, `plan` | `true` to approve the plan, or a string of feedback to revise it |
| `notify` | `level` (`info`, `warn`, `error`), `text` | None; no answer is expected |

An `ask` request is a question of the `ask_user` tool (or of an extension). `context`, when present, is what the user needs to know and belongs above the question. `options` is empty for `text` and `confirm`; `multi` is `true` exactly when `expects` is `multiselect`. With `allow_freeform` the user may type an answer instead of choosing: send it as a string, or as a list with that one string. A single chosen option may be sent as a string too, and `yes` and `no` are accepted for a `confirm`. `code` or `diff` (never both) is source text the question is about, to be shown with it: code as it is written, a unified diff as a diff. `recommended` is the answer the agent would give, one of the `options` or `yes` or `no`: mark it, and start the selection on it.

A `select` request is also how azazel asks what to do when a subscription has run out in the middle of a run: its `prompt` starts with the line `Usage limit reached`, and its options are `Use the reset` (when the account holds one), `Wait until reset` and `Pick another model`. Dismissing it lets the run fail with the provider's refusal; see [when every account is out](models-and-logins.md#when-every-account-is-out).

A `form` request is an `ask_user` call with several `questions`, to be answered together:

```json
{"type": "ui_request", "request_id": "r4", "kind": "form", "questions": [
  {"question": "Which store?", "header": "Store", "expects": "select", "options": ["sqlite", "postgres"], "multi": false, "recommended": "postgres"},
  {"question": "Anything else?", "expects": "text", "options": [], "multi": false}
]}
{"id": 9, "type": "ui_response", "request_id": "r4", "value": [["postgres"], null]}
```

`header` is a short name for the question (a tab, a label). A list shorter than the questions leaves the rest open, and so does an entry that is no answer to its question; `null` in place of the list dismisses the whole form.

### `user_input`

Without `agent` it is written by a [background session](background-and-remote.md) only: what a client sent the
lead, `{"type": "user_input", "kind": "prompt", "text": "..."}` with `kind` `prompt`, `steer` or `follow_up`. It
is in the session's log, in what a client that attaches later is replayed, and in what the session's other
clients read, so all of them show what was asked. The client that sent the input is not sent the frame.

With `agent` it is what a sub-agent was handed, in every session:
`{"type": "user_input", "kind": "prompt", "text": "...", "agent": "scout-1"}` for the task the agent started
with and for each message sent to it since (by the user, by `agent_send`, or by another agent, whose message
is wrapped in `<agent-message from="...">`). A client that shows an agent's work shows it there.

### `request_closed`

Written by a background session only: `{"type": "request_closed", "request_id": "r3"}` says that an
`approval_request` or `ui_request` no longer waits, because another client answered it or whoever asked gave up.
A client that shows the request takes it away. The client that answered is not sent the frame.

### `browser_frame`

`{"type": "browser_frame", "tab": "t1", "data": "<base64 JPEG>", "width": 1280, "height": 720, "seq": 41}`: one picture of the tab the agents' browser shows, sent only to the clients that asked with `browser_watch`. Frames are ephemeral: they carry no `at`, are never written to the log of a background session and are never replayed. A client that reads slowly is sent fewer of them, never closed for it. The size and rate are set by `[browser] live_fps`, `live_quality` and `live_max_width`.

### `browser_state`

`{"type": "browser_state", "running": true, "visible": false, "tab": "t1", "url": "https://example.com/", "title": "Example", "held": false, "held_by_you": false, "watchers": 1}`: what the agents' browser is doing, sent to every attached client when a browser starts or ends, the tab or its address changes, or a client takes it over or hands it back. `held` is true while a client has taken it over and the agents wait; `held_by_you` is whether that client is the one the frame is sent to. `tab`, `url` and `title` are left out when no browser runs. Ephemeral like `browser_frame`; the responses of the four `browser_*` commands carry the same members. A session without a browser answers `{"running": false, ...}`.

### `btw_delta`

A piece of the answer to a `btw` command: `{"type": "btw_delta", "id": <the command's id>, "delta": "..."}`. The command's response carries the whole answer.

### `theme`

Sent when the terminal theme changed because `set_setting` changed `tui.theme`: `{"type": "theme", "name": "nord", "light": false, "roles": {"text": "#d8dee9", "accent": "#88c0d0", ...}}`. It has the members of the `get_theme` response, made for a dark background. It comes before the response to the `set_setting` that caused it. A client that follows the terminal's background should ask `get_theme` again with its own `light`.

## Background sessions

A background session (`azazel bg`, see [background sessions](background-and-remote.md)) serves this protocol on
a local socket instead of standard input and output; `azazel rpc --attach <id>` connects your standard input and
output to it. Two commands exist only there:

| `type` | Members | Does | Response `data` |
|---|---|---|---|
| `attach` | `pid?`, `replay?`, `since?` | The first command on a connection: makes it a client of the session, beside any that are attached already. `replay` is `missed` (the default), `all` or `none`; those frames follow the response, then the requests that still wait for an answer, then everything the session does from then on. With `since`, the `at` of the last frame the client saw, the frames logged after that one follow instead, and `replay` is not used (see [resuming](#resuming-after-a-dropped-connection)) | `{"host", "pid", "session_id", "session_file", "clients", "snapshot", "cursor", "gap?"}` |
| `stop` | `now?` | Ends the session once the lead has finished what it is doing; with `now` the lead is stopped first. Any connection may send it, attached or not. The connection closes when the session has ended | `null` |

Until `attach` has been answered, every other command is refused.

Any number of clients may be attached. Each is sent every event; any of them may prompt, steer, abort or answer a
request. A response and the `btw_delta` frames of a `btw` go to the client that sent the command, under the `id`
that client gave it, so two clients need not agree on ids. What one client sends the lead reaches the others as
`user_input`, and a request one of them answered closes on the others with `request_closed`. A second answer to
the same request is refused. Responses, `btw_delta`, `request_closed`, `browser_frame` and `browser_state` are not logged
or replayed; every other frame is appended to `~/.azazel/run/<id>.log`.

`replay: "missed"` replays what was written since the last client left (everything, for the first client). When
another client is attached already, nobody was away: the frames of the run in progress are replayed, from the
input that started it.

### Resuming after a dropped connection

Every frame a background session logs carries `"at"`: the byte offset of its line in the log, which grows with
each frame. A client keeps the `at` of the last frame it read. When its connection drops, it connects again and
sends `attach` with `since` set to that value; it is replayed the frames logged after that one, exactly once each,
and the requests that still wait for an answer (those it saw before are not sent again). The response's `cursor`
is the `at` the next frame will have, where live frames begin. When the log has no frame with that `at` (a
different session, or an `at` that was never one), `gap` is `true`: the client cannot trust what it shows, so it
resynchronises with `get_messages`, and the frames of the run in progress are replayed as with `missed`. Frames
that are not logged (responses, `btw_delta`, `request_closed`) have no `at`.

`snapshot` in the response to `attach` says where the session stands at that moment, for a client that draws the
session's state rather than replaying frames:

| Member | Meaning |
|---|---|
| `busy` | Whether the lead is working |
| `tail` | The last messages of the conversation (at most 20), oldest first, as `get_messages` gives them |
| `last_error?` | The lead's most recent run failure, cleared when a new run starts; lets a late client show why the run stopped |
| `streaming` | The replies being written: `{"agent", "text", "thinking?"}` with the text so far |
| `tools` | The tool calls that run: `{"agent", "call_id", "name", "input", "elapsed_ms"}` |
| `queue` | What clients sent the lead that it has not taken up yet: `{"kind", "text"}` |
| `pending` | The `request_id`s of the requests that wait for an answer; the requests themselves follow as frames |

`clients` counts the clients attached, the new one included.

| Member | Meaning |
|---|---|
| `cursor` | The `at` of the next frame: where live frames begin |
| `gap` | Present and `true` when `since` named no frame of the log |

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
