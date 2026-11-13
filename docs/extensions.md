# Extensions

Extensions add behaviour to Azazel in code. There are two kinds. A **Luau extension** is a script dropped into an `extensions` directory: it can add tools, react to events, add slash commands and key bindings, show status in the UI, and register model providers and MCP servers, with no compiling. A **Rust extension** is a type implementing the `Extension` trait, compiled into a binary of your own that is otherwise the stock harness.

If a shell script at a fixed event is enough, use a [hook](hooks.md). To share extensions, put them in a [package](packages.md).

## Luau extensions

Luau scripting is part of the default build (cargo feature `lua`).

### Where scripts live

| Location | Loaded |
|---|---|
| Scripts listed by an installed [package](packages.md) | when the package's code is enabled; the id is `<package>-<file name>` |
| `~/.azazel/extensions/<id>.luau` or `~/.azazel/extensions/<id>/init.luau` | always |
| `<project>/.azazel/extensions/<id>.luau` or `.../<id>/init.luau` | only when the project is trusted |

They load in that order, and a later script replaces an earlier one with the same id. Names starting with `.` are skipped.

| Command | Does |
|---|---|
| `/ext` | Lists the loaded extensions and every Luau script with its file, and the error of each disabled script |
| `/reload` | Drops all Luau state and loads the scripts again. Use it after editing a script. In the terminal UI it first reads skills, custom commands, agent definitions, hooks, key bindings and the theme again; see [the terminal UI](tui.md#slash-commands) |
| `azazel ext types` | Writes type declarations for the `azazel` global to `~/.azazel/types/azazel.d.luau`; point your editor's Luau language server at it as a definitions file |

### How a script runs

- Each script has its own sandboxed Luau VM. It has no file, process or network access except through the global table `azazel`, which is read-only.
- The top level runs once when the script loads. **Registering** functions (`tool`, `on`, `command`, `shortcut`, `provider`, `mcp`) work only then. Functions that **wait** (`exec`, `http`, `fs.*`, `agent.spawn`, `memory.*`, `ctx.ui.*`) work only afterwards, inside a tool, handler, command or shortcut.
- One piece of a script's code runs at a time.
- The load, each event handler, each command and each shortcut get 5 seconds of the script's own running time; time spent waiting in an `azazel` function does not count. Tools have no time limit; they are cancelled when the tool call is aborted.
- A script that fails to load, or whose handler raises an error or overruns, is **disabled**: its tools, commands, status segments and widgets go away, you are told once, and the session continues. An error raised inside a tool is returned to the model as an error result and does not disable the script.

### Two small examples

Both ship in `examples/extensions/`. Copy one to `~/.azazel/extensions/` and run `/reload`.

```lua
-- word_count.luau: a tool the agents can call
azazel.tool({
	name = "word_count",
	description = "Count the words in a text file of the project. Returns the number of words.",
	parameters = {
		type = "object",
		properties = { path = { type = "string", description = "The file, relative to the project root" } },
		required = { "path" },
	},
	access = "read",
	execute = function(args, ctx)
		local count = 0
		for _ in string.gmatch(azazel.fs.read(args.path), "%S+") do
			count += 1
		end
		return tostring(count)
	end,
})
```

```lua
-- guard.luau: deny a shell command before it runs, and count how often
azazel.on("tool_call", function(ev, ctx)
	local command = type(ev.input) == "table" and ev.input.command
	if ev.tool ~= "shell" or type(command) ~= "string" then
		return nil
	end
	if string.find(command .. " ", "%f[%a]rm%s+%-%a+%s+/%*?[%s;&|]") then
		azazel.state.set("blocked", (azazel.state.get("blocked") or 0) + 1)
		azazel.notify("guard stopped a command of " .. ctx.agent_id, "warn")
		return { block = true, reason = "guard: refused, it would delete the whole filesystem. Do not retry it." }
	end
end)

azazel.command({
	name = "guard",
	description = "Show how many shell commands the guard has refused",
	run = function()
		return "guard has refused " .. (azazel.state.get("blocked") or 0) .. " command(s)"
	end,
})
```

### The `azazel` API

Registering (load only):

| Function | Contract |
|---|---|
| `azazel.tool{ name, description, parameters?, access?, execute }` | `name`: up to 64 letters, digits, `_`, `-`. `parameters`: a JSON Schema object as a table; omitted means no arguments. `access`: `"read"`, `"write"`, `"exec"` (default) or `"net"`; it decides when you are asked (see [permissions](permissions.md)). `execute(args, ctx)` returns a string or `{ content = string, is_error = boolean }` |
| `azazel.on(event, handler)` | `handler(ev, ctx)` returns `nil`, `{ block = true, reason = string }`, or a table of replacement fields. Events are listed below |
| `azazel.command{ name, description?, argument_hint?, run }` | Adds `/name`. `run(args, ctx)` gets the text after the command; a returned string is shown to you |
| `azazel.prompt(name, render)` | Adds a part to the system prompt of every agent. `render(ctx)` runs each time a prompt is put together (the lead's when a session opens, a sub-agent's when it starts) and returns the part's text, or `nil` to leave it out; `ctx.agent_id` says whose prompt it is. The part comes after the project's context files. `name` is one word of your own; the names of the built-in parts (`identity`, `rules`, `team`, `skills`, `environment`, `context_files`, `memory`, `mode`) are refused. A part that raises an error disables its script |
| `azazel.shortcut(key, handler)` | Binds a key such as `"ctrl+k"` or `"alt+shift+p"` in the terminal UI. `handler(ctx)` |
| `azazel.provider{ id, api, base_url, api_key_env?, headers? }` | Makes an HTTP endpoint's models available as `<id>/<model>`. `api` is `"anthropic-messages"`, `"openai-responses"`, `"openai-chat"` or `"google-generative"`. Without `api_key_env` the endpoint needs no key. A provider stays registered across `/reload`. See [models and logins](models-and-logins.md) |
| `azazel.mcp{ name, ... }` | Adds an MCP server; the other fields are those of an `mcp.json` entry. See [mcp](mcp.md) |

Usable any time:

| Function | Contract |
|---|---|
| `azazel.status(key, text)` | Sets a segment of the status line; `nil` removes it |
| `azazel.widget(key, lines)` | Sets lines shown above the input: a list of strings or one string; `nil` removes them |
| `azazel.notify(text, level?)` | Shows a message. `level` is `"info"` (default), `"warn"` or `"error"` |
| `azazel.json.encode(value, pretty?)`, `azazel.json.decode(text)` | A table with a sequence part encodes as an array; `null` decodes to `nil` |
| `azazel.state.get(key)`, `azazel.state.set(key, value)` | Per-script store kept in `~/.azazel/extensions/.state/<id>.json`; survives restarts and `/reload`. `nil` removes the key |

Only inside a tool, handler, command or shortcut (they wait, or need a running session):

| Function | Contract |
|---|---|
| `azazel.send(text, { deliver? })` | Sends `text` to the lead agent as the user: `"follow_up"` (default, when it would otherwise go idle) or `"steer"` (after its current step) |
| `azazel.session.name()`, `azazel.session.set_name(text)` | The open session's name (`nil` until it has one), and naming it |
| `azazel.session.set_model(specs)` | Changes the lead's model: `"provider/model[:thinking]"`, `"@role"`, or a list of them tried in order. The change is recorded in the session |
| `azazel.session.set_thinking(level)` | Changes the lead's thinking level: `"off"`, `"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"` or `"max"`. Recorded in the session |
| `azazel.session.compact(instructions?)` | Summarises the conversation so far to free context, once the lead is idle |
| `azazel.session.messages()` | The messages from the start of the session to where the conversation is now: `{ id, role, text, content }` each. `role` is `"user"`, `"assistant"` or `"tool"`; `content` is the message's blocks as the session file has them, without the bytes of images |
| `azazel.agent.send(to, message)` | Messages an agent id or `"all"` as the user; returns the ids reached |
| `azazel.agent.stop(id)` | Stops an agent id or `"all"`, with the agents it started; returns the ids stopped |
| `azazel.agent.list()` | `{ id, def, parent, depth, state, elapsed_secs, tokens, cost_usd }` per agent |
| `azazel.exec(program, args?, { cwd?, timeout? })` | Runs a program directly, without a shell. `cwd` is relative to the project root; `timeout` in seconds, default 60. Returns `{ code, stdout, stderr }` |
| `azazel.http{ url, method?, headers?, body?, timeout? }` | Default `GET`, timeout 30 s. Returns `{ status, headers, body }`; an HTTP error status is a response, not an error |
| `azazel.fs.read(path)`, `.write(path, content)`, `.list(path)`, `.exists(path)` | Files inside the project root, the `permissions.additional_dirs` and the script's own data directory `azazel.fs.data_dir` (`~/.azazel/extensions/.data/<id>`). Relative paths start at the project root. A path that leads elsewhere, also through `..` or a symlink, is an error. `list` returns `{ name, is_dir }` entries |
| `azazel.agent.spawn{ agent, prompt, wait? }` | Starts a sub-agent of the lead. With `wait` (default true) returns its report and its id; with `wait = false` returns the id at once |
| `azazel.memory.save(text, kind?, tags?)` | Saves a project memory; `kind` defaults to `"fact"`. Returns the id. Errors with `memory unavailable` when memory is off |
| `azazel.memory.search(query, limit?)` | `{ id, kind, text, tags, scope }` hits, best first; default limit 8 |
| `azazel.tools.call(name, args?)` | Calls a tool of the harness, of another extension or of an MCP server as the lead agent, and returns the text of its result. The call passes the permission rules, the mode and the hooks like one the agent makes itself, so it may ask you; a refused or failed call is an error |

The `azazel.session` functions error with `no session is open` while there is none, as when the script loads.

The second argument of every tool, handler and command (the only argument of a shortcut and of a prompt part) is `ctx`, read-only:

| Field | Value |
|---|---|
| `ctx.cwd`, `ctx.session_id` | Working directory and session id |
| `ctx.agent_id` | The agent the call is about; `"lead"` when it is about none |
| `ctx.mode` | The session's permission mode: `"plan"`, `"manual"`, `"accept-edits"`, `"auto"` or `"bypass"`; `"plan"` for an agent whose definition keeps it there |
| `ctx.ui.confirm(prompt)` | Waits for yes or no; returns a boolean |
| `ctx.ui.select(prompt, options)` | Returns the chosen option and its position. In the terminal UI a short first line of `prompt` is the title of the box and the lines after it its text; an option written `label<TAB>detail` shows the detail in a column beside the label |
| `ctx.ui.input(prompt)` | Returns the text typed. The first line of `prompt` is the title of the box, as for `select` |

The `ctx.ui` functions error with `no interactive user` in headless runs.

### Events

`ev.name` is the event name. `ev.agent` (and `ev.parent`) is `{ id, def, parent, depth, session_id }`.

| Event | Fields of `ev` | Replaceable in the returned table | Can block |
|---|---|---|---|
| `session_start` | `source`: `startup`, `resume`, `clear`, `compact` | | |
| `session_end` | | | |
| `user_prompt` | `text`, `images` (a count) | `text` | yes: the prompt is rejected |
| `turn_start`, `turn_end` | `agent` | | |
| `tool_call` | `agent`, `call_id`, `tool`, `input`, `skip_prompt` | `input`, `skip_prompt` (true runs the call without asking you) | yes: the call is denied and the model sees the reason |
| `tool_result` | `agent`, `call_id`, `tool`, `content`, `is_error` | `content`, `is_error` | |
| `context` | `agent`, `messages` | `messages` (for this request only) | |
| `agent_spawn` | `parent`, `def`, `prompt` | `prompt` | yes: the spawn is refused |
| `agent_end` | `agent`, `status` | | |
| `pre_compact` | `agent`, `manual` | | yes: a manual compaction is cancelled |
| `post_compact` | `agent` | | |
| `stop` | `agent`, `stop_hook_active` | | yes: the agent continues with the reason as a user message |
| `notification` | `level`, `text` | | |
| `file_changed` | `agent`, `path` (absolute), `tool` | | |
| `config_change` | `source`: `settings` or `reload` | | |
| `worktree_create`, `worktree_remove` | `agent_id`, `path` | | |
| `setup` | `trigger`: `init` or `trust` | | |

`notification` fires in the terminal UI when something wants your attention. `file_changed` fires after a `write` or `edit` succeeded, `config_change` after the settings menu or a reload changed the configuration, the worktree events when an isolated agent's worktree is made or removed, and `setup` the first time a project runs trusted (`azazel init` runs no scripts, so a script sees only `trust`). `azazel.on` also accepts `before_agent_start`, but the current version never emits that event.

A `tool_call` handler cannot override deny rules, the git push guard or protected paths; they are checked before it runs and again on a rewritten input.

## Rust extensions

`azazel_cli::Builder` is the embedding point. The stock binary's `main` is `azazel_cli::Builder::default().run()`; yours adds extensions, which are registered after the built-in ones:

```rust
use std::{process::ExitCode, sync::Arc};

use async_trait::async_trait;
use azazel_core::{Event, EventHandler, EventKind, ExtCtx, ExtError, Extension, Outcome, Registrar};
use serde_json::Value;

/// Runs before every tool call of every agent: refuses shell commands that ask for root.
struct NoSudo;

#[async_trait]
impl EventHandler for NoSudo {
    async fn handle(&self, event: &mut Event, _: &ExtCtx) -> Outcome {
        if let Event::ToolCall { tool, input, .. } = event
            && tool == "shell"
            && let Some(command) = input.get("command").and_then(Value::as_str)
            && command.split_whitespace().any(|word| word == "sudo")
        {
            return Outcome::Block { reason: "this harness does not run commands as root".to_string() };
        }
        Outcome::Continue
    }
}

struct MyExt;

impl Extension for MyExt {
    fn id(&self) -> &str { "my-ext" }
    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.on(EventKind::ToolCall, Arc::new(NoSudo));
        Ok(())
    }
}

fn main() -> ExitCode {
    azazel_cli::Builder::default().extension(MyExt).run()
}
```

The complete example, which also adds a tool, is `examples/custom-distro`. Build and run it from the repository:

```bash
cargo run -p custom-distro -- -p "how long is src/main.rs?"
```

Its `Cargo.toml` depends on `azazel-cli`, `azazel-core`, `azazel-agent`, `async-trait`, `serde_json` and `tokio`. Outside the repository, depend on the crates by path or git:

```toml
[dependencies]
azazel-cli = { git = "https://github.com/sinnafuls/azazel-cli" }
azazel-core = { git = "https://github.com/sinnafuls/azazel-cli" }
azazel-agent = { git = "https://github.com/sinnafuls/azazel-cli" }
async-trait = "0.1"
serde_json = "1"
```

### The `Registrar`

Everything an extension contributes goes through the `Registrar` passed to `register`. What it registers is attributed to the extension's `id`.

| Method | Adds |
|---|---|
| `tool(Arc<dyn Tool>)` | A tool. The first tool registered under a name wins, so a built-in tool cannot be replaced |
| `tool_source(Arc<dyn ToolSource>)` | Tools that come and go; asked before every request |
| `command(Command)` | A slash command: `name`, `description`, `argument_hint`, `handler` |
| `command_source(Arc<dyn CommandSource>)` | Commands that come and go |
| `on(EventKind, Arc<dyn EventHandler>)` | An event handler |
| `provider(Arc<dyn Provider>)` | A model provider |
| `scheme(Arc<dyn Scheme>)` | A URL scheme the `read` tool can resolve |
| `skill_root(PathBuf, SkillSource)` | A directory of skills, taking precedence over roots added before it |
| `system_part(name, Arc<dyn SystemPartProvider>)` | A named part of the system prompt, rendered per agent before every request |
| `service(Arc<T>)` / `services()` | Puts a value into, or reads, the shared type map that tools and handlers reach through `ctx.services` |

The traits involved:

| Trait | Crate | Methods |
|---|---|---|
| `Extension` | `azazel-core` | `id()`, `register(&mut Registrar)` |
| `Tool` | `azazel-agent` | `name`, `description`, `schema` (JSON Schema), `access(input)` (`Read`, `Write`, `Exec`, `Net`), `subjects(input)` (what permission rules match), `call(input, &ToolCtx) -> ToolOutput`; optional `concurrency_safe` and `approval_note` |
| `EventHandler` | `azazel-core` | `handle(&mut Event, &ExtCtx) -> Outcome` |
| `CommandHandler` | `azazel-core` | `run(args, &ExtCtx) -> CommandOutcome` (`Done`, `Show(text)`, `Prompt { text, model }`, `Error(text)`) |
| `Scheme` | `azazel-core` | `scheme()`, `read(rest, &ToolCtx)`; optional `access(rest) -> (Access, Option<String>)`, what reading the address touches and the subject permission rules match (default: a plain read of the address) |
| `SystemPartProvider` | `azazel-core` | `render(&AgentInfo) -> Option<String>` |

### Events in Rust

Handlers run in registration order, one at a time, and receive the event mutably. Returning `Outcome::Block { reason }` stops the chain. `EventKind` has every event of the Luau table above (`SessionStart`, `UserPrompt`, `ToolCall`, ...) plus two that Luau scripts do not get:

| Event | Fields | Use |
|---|---|---|
| `BeforeRequest` | `agent`, `request` | Inspect or change the complete model request |
| `PermissionRequest` | `agent`, `tool`, `input`, `subjects`, `decision` | Set `decision` to `Some(true)` or `Some(false)` to settle an approval without asking the user |

The built-in features (core tools, web, MCP, hooks, the Luau host, memory, media, ...) are extensions registered the same way; `/ext` lists their ids.
