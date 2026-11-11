# The terminal UI

Running `azazel` with no subcommand and no `-p` opens the interactive UI. It is inline, not full-screen: finished
output is written once into your terminal's own scrollback, and only a small live region at the bottom is redrawn.
This page lists the layout, keys, input prefixes, slash commands and themes. The UI needs a real terminal; without
one, use [print mode](getting-started.md#print-mode) or [RPC mode](rpc.md).

## Layout

The live region, top to bottom:

| Part | Shown when | Content |
|---|---|---|
| Streaming reply | the agent is working | The unfinished tail of the current reply; finished blocks move to scrollback |
| Thinking | thinking is shown (`Ctrl+T`) | The last lines of the model's reasoning |
| Tool cards | a tool is running | Tool name, a one-line summary of its input, the last 5 output lines, elapsed time |
| Activity line | the agent is working | Spinner, `working` / `thinking` / `running <tool>`, elapsed time, `esc to interrupt` |
| Roster | sub-agents are active | One line per queued, working or approval-waiting agent; see [team](team.md) |
| Todo list | the agent has unfinished todos | The lead agent's task list |
| Input | always | The editor, with its completion list, notices and an attached-image count above it |
| Status line | always | `mode · model · ctx N% · $cost · goal state · directory` (parts appear when known) |

Questions from agents, approval prompts, plan reviews and pickers replace the input until answered. In a picker:
`Up`/`Down` move, `Enter` chooses and `Esc` cancels. Searchable pickers (models, sessions, prompt history) filter
as you type; in the others, digits `1`-`9` choose a row directly.

A finished tool call shows 3 lines of output (16 lines of a diff). `Ctrl+O` expands tool calls that finish from
then on.

## Keys

| Key | Action name | Effect |
|---|---|---|
| `Enter` | `submit` | Send the prompt. While the agent is working: steer it (the message is delivered before its next step) |
| `Alt+Enter` | `follow_up` | While the agent is working: queue the message until it finishes |
| `Shift+Enter`, `Ctrl+J` | `newline` | Insert a line break |
| `Esc` | `abort` | Interrupt the lead agent. Twice within a second on an empty, idle input: open `/rewind` |
| `Ctrl+C` | `clear_or_exit` | Clear the input; on an empty input, twice within a second exits |
| `Ctrl+D` | `exit` | Exit when the input is empty |
| `Shift+Tab` | `cycle_mode` | Next permission mode: `plan`, `ask`, `accept-edits`, `auto` |
| `Alt+A` | `agent_hub` | Open the agent hub |
| `Ctrl+O` | `toggle_tool_output` | Expand or collapse tool output |
| `Ctrl+T` | `toggle_thinking` | Show or hide thinking |
| `Ctrl+L` | `model_picker` | Pick the model |
| `Ctrl+R` | `history_search` | Search earlier prompts and put one back in the input |
| `Ctrl+V` | `paste_image` | Attach the clipboard image to the next prompt |
| `Tab` | `complete` | Open or accept completion |

`Esc` interrupts the lead only; stop sub-agents from the agent hub or with `/stop`. In the agent hub, `Enter`
follows the selected agent's work in the transcript, `m` messages it, `s` stops it and `S` stops all.

Editing keys in the input: arrows, `Home`/`End`, `Ctrl+A`/`Ctrl+E` (line start/end), `Ctrl+Left`/`Ctrl+Right` or
`Alt+B`/`Alt+F` (word), `Ctrl+W` and `Ctrl+Backspace` (delete word back), `Alt+D` and `Ctrl+Delete` (delete word
forward), `Ctrl+U` (delete to line start), `Ctrl+K` (delete to line end), `Ctrl+Z` (undo), `Ctrl+Y` or
`Ctrl+Shift+Z` (redo). `Up` on the first line and `Down` on the last walk through earlier prompts.

### Changing keys

`~/.azazel/keybindings.toml` maps an action name from the table above to a list of keys:

```toml
model_picker = ["f2"]
agent_hub = ["f3", "alt+a"]
exit = []                      # an empty list unbinds the action
```

- A listed action gets exactly the listed keys, and those keys stop triggering any action the file does not mention.
- Modifiers: `ctrl`, `alt`, `shift`, `super` (also `cmd`, `win`), `meta`, `hyper`. Keys: a single character,
  `enter`, `esc`, `tab`, `space`, `backspace`, `delete`, `insert`, `home`, `end`, `pageup`, `pagedown`, `up`,
  `down`, `left`, `right`, `f1` to `f24`.
- An unknown action or an invalid key makes azazel ignore the whole file and show a warning at startup.
- `/keys` lists the bindings in effect.

When the terminal supports the kitty keyboard protocol, azazel enables it so chords such as `Shift+Enter` are
reported reliably. Where `Shift+Enter` does not arrive, use `Ctrl+J`.

## Input prefixes

| Prefix | Meaning |
|---|---|
| `/` | A slash command. A completion list appears as you type; `Tab` accepts, and `Enter` on a half-typed name completes it |
| `@` | Fuzzy file completion over the working directory (gitignored files and `.git` are left out). On submit, each `@path` that names a UTF-8 text file of at most 256 KB is attached to the prompt and counts as read by the agent |
| `!` | `!command` runs in your shell, outside the permission engine and the push guard. You see the output, and the agent gets it as a note without being asked to reply |

Example:

```text
› compare @src/config.rs with @docs/configuration.md
› !git status
```

## Slash commands

Session and display:

| Command | Description |
|---|---|
| `/help` | List every command |
| `/new` | Start a new session |
| `/resume` | Reopen an earlier session of this project |
| `/tree` | Go back to an earlier point of this session |
| `/fork` | Continue from here in a copy of this session |
| `/rewind` | Go back to before an earlier prompt: files, conversation or both |
| `/compact [what to keep]` | Summarise the conversation so far |
| `/export [path]` | Write the conversation to a Markdown file |
| `/clear` | Clear the screen and scrollback (the conversation is kept) |
| `/cost` | Requests, tokens and money spent in this session |
| `/context` | How full the context window is, message count, tools offered |
| `/theme [name]` | Pick or set the colour theme |
| `/keys` | List key bindings |
| `/quit`, `/exit` | Exit |

`/resume`, `/tree`, `/fork`, `/rewind`, `/compact` and `/export` are covered in
[sessions and checkpoints](sessions-and-checkpoints.md).

Models, permissions and setup:

| Command | Description |
|---|---|
| `/model [provider/id [fallback …]]` | Pick the model, or set a chain; `@role` names work too |
| `/thinking [level]` | Pick or set the thinking level: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| `/mode [mode]` | Pick or set the permission mode |
| `/permissions` | List the permission rules in effect and where they come from |
| `/settings` | Sign in, add API keys and choose the default model |
| `/login [provider]` | Sign in to a provider or store its API key |
| `/logout <provider>` | Forget the stored key or login of a provider |
| `/init` | Have the agent explore the repository and write `AGENTS.md` |
| `/doctor` | Check the installation and this project's setup |

Team and goals ([team](team.md), [goals](goals.md), [screen and playtesting](screen-and-playtesting.md)):

| Command | Description |
|---|---|
| `/agents` | The agents of this session and the definitions they can be started from |
| `/msg <agent id\|all> <message>` | Send a message to an agent |
| `/stop <agent id\|all>` | Stop an agent and the agents it started |
| `/goal <objective>` | Set an objective the agent works on until it is verified; also `/goal status\|pause\|resume\|drop` |
| `/verify [what should be true]` | Run the project's checks and a verifier agent on the work so far |
| `/playtest <launch command> [-- <what to test>]` | Have a playtester run a build, play it and report defects |

Skills, extensions and services:

| Command | Description |
|---|---|
| `/skills` | List skills and where they come from |
| `/skill:<name> [args]` | Run a skill; see [skills and commands](skills-and-commands.md) |
| `/ext` | List loaded extensions and Luau scripts, with the errors of disabled scripts |
| `/reload` | Drop all Luau extension state and load the scripts again; see [extensions](extensions.md) |
| `/mcp` | MCP servers and their state; also `/mcp reconnect\|enable\|disable\|login\|logout <server>`; see [MCP](mcp.md) |
| `/mcp:<server>:<prompt>` | Run a prompt offered by an MCP server |
| `/memory [list \| forget <id> \| auto on\|off]` | What is remembered; see [memory](memory.md) |
| `/media` | Media backends, configured models and the output directory; see [media and sprites](media-and-sprites.md) |
| `/lsp` | Language servers and their state; see [LSP](lsp.md) |

Custom commands from `commands/` directories and commands registered by extensions appear in `/help` and in
completion as well. `/memory` is absent when memory is disabled in the configuration.

## Themes

Two themes are built in: `dark` (default) and `light`. Set one for good in [configuration](configuration.md):

```toml
[tui]
theme = "light"
```

`/theme` opens a picker and `/theme <name>` switches for the running session.

A custom theme is a file `~/.azazel/themes/<name>.toml` that maps roles to colours. Roles left out keep the
`dark` colour.

```toml
# ~/.azazel/themes/dusk.toml
text = "default"
muted = "#7f848e"
accent = "#c678dd"
success = "green"
warning = "bright_yellow"
error = 203
border = "#586070"
code_bg = "#1e222a"
diff_add = "#7ec699"
diff_del = "#f07178"
```

| Value | Meaning |
|---|---|
| `"#rrggbb"` | A truecolor value |
| integer or `"0"`-`"255"` | An xterm palette index |
| `"black"`, `"red"`, `"green"`, `"yellow"`, `"blue"`, `"magenta"`, `"cyan"`, `"white"` | ANSI colours; prefix `bright_` for the bright variants; `"gray"` also works |
| `"default"` | The terminal's own colour |

An unknown role or an invalid colour is reported; at startup `dark` is used instead. Truecolor is used when
`COLORTERM` is `truecolor` or `24bit`, or in Windows Terminal; otherwise colours are mapped to the nearest of 256.
