# The terminal UI

Running `azazel` with no subcommand and no `-p` opens the interactive UI. It is inline, not full-screen: finished
output is written once into your terminal's own scrollback, and only the live region below it is redrawn. This
page lists the layout, keys, input prefixes, slash commands and themes. The UI needs a real terminal; without
one, use [print mode](getting-started.md#print-mode) or [RPC mode](rpc.md).

## Layout

```text
 fix the failing test

I'll run the tests first.

╭─── ✔ shell cargo test ───────────────────────────────────────────────────╮
│ test result: FAILED. 11 passed; 1 failed                                 │
│ … 14 more lines (ctrl+o to expand)                                       │
╰── 1.4s · exit 101 ───────────────────────────────────────────────────────╯
╭─── ⠹ edit src/lib.rs ────────────────────────────────────────────────────╮
╰── 0s ────────────────────────────────────────────────────────────────────╯

── ⠹ running edit · 12s · esc to interrupt ─────────────────────────────────
❯ Steer the agent, alt+enter queues a follow-up
────────────────────────────────────────────────────────────────────────────
ask · ⑂ main · ~/src/app · ◫ 12% · $0.04          ⬢ provider/model · medium
```

The input and the status line sit on the last rows of the terminal from the first frame on. When azazel starts,
what the terminal showed before scrolls up into the scrollback, as after `clear`. The transcript then grows from
the top of the screen down towards the input; once it reaches it, older lines scroll away as in any terminal.
Leaving azazel puts the shell prompt right under the transcript.

The live region, top to bottom:

| Part | Shown when | Content |
|---|---|---|
| Streaming reply | the agent is working | The unfinished tail of the current reply; finished blocks move to scrollback |
| Thinking | thinking is shown (`Ctrl+T`) | The last lines of the model's reasoning, in italics |
| Tool cards | a tool is running | One framed card per running tool: spinner, tool name, a one-line summary of its input, the last 5 output lines, elapsed time |
| Roster | sub-agents are active | One line per queued, working or approval-waiting agent; see [team](team.md) |
| Todo list | the agent has unfinished todos | The lead agent's task list |
| Input | always | The editor between two rules, with its completion list, notices and an attached-image count above it |
| Status line | always | Session facts on the left, the model on the right |

Your own messages appear in the transcript on a band that spans the terminal's width.

### Input

The input sits between two full-width rules, with `❯` in front of the first row. An empty input shows a hint
(`Ask anything, / for commands, @ for files`). The input grows with what you type up to 30 % of the terminal's
height (at least 5 rows) and scrolls beyond that; the rules then say how much is out of view (`↑ 3 more`).

While the agent is working, the rule above the input carries the working indicator: a spinner, `working` /
`thinking` / `running <tool>`, the elapsed time and `esc to interrupt`.

The colour of the rules tells you what `Enter` will do:

| Rules | Meaning |
|---|---|
| border colour | Permission mode `ask` |
| `mode_plan` colour | Permission mode `plan` |
| `mode_edits` colour | Permission mode `accept-edits` |
| `mode_auto` colour | Permission mode `auto` |
| `bash` colour (pink) | The input starts with `!`: it runs as a shell command |

Questions from agents, approval prompts, plan reviews and pickers replace the input until answered, framed the
same way: the title in the rule above, the keys in the rule below. In a picker: `Up`/`Down` move, `Enter` chooses
and `Esc` cancels; the selected row is marked `→` and highlighted, and a list too long to show ends with its
position (`(7/20)`). Searchable pickers (models, sessions, prompt history) filter as you type; in the others,
digits `1`-`9` choose a row directly.

### Tool cards

Every tool call is a framed card. The header names the tool and what it is about; the bottom border carries how
long the call took and, for `shell`, the exit code. The frame and the icon show the state:

| Icon | Frame | State |
|---|---|---|
| spinner | accent | Running |
| `✔` | muted | Finished |
| `⚠` | warning | Interrupted, or refused by you |
| `✘` | error | Failed |

A finished card shows 3 lines of output, or 16 lines of a diff; `… N more lines (ctrl+o to expand)` counts the
rest. `Ctrl+O` expands tool calls that finish from then on. Diffs have a line-number gutter, and added and
removed lines are filled with a background colour.

### Status line

| Segment | Shown when | Content |
|---|---|---|
| mode | always | The permission mode: `ask`, `⏸ plan`, `⏵⏵ accept-edits`, `⏵⏵ auto`, in the colour of the input rules |
| `⑂ branch` | the project is a git repository | The checked-out branch |
| directory | always | The working directory, with `~` for your home directory |
| `◫ N%` | the model's context window is known | How full the context window is; warning colour above 70 %, error colour above 90 % |
| `$cost` | the model's prices are known | Money spent in this session |
| `⇶ N agents` | sub-agents are active | How many are queued, working or waiting for approval |
| `⚑ goal …` | a goal is open | The goal's state and the attempt number; see [goals](goals.md) |
| `⬢ model · level` | always, against the right edge | The model answering and the thinking level |

When the terminal is too narrow for both sides, the model is shortened first, then left out.

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
❯ compare @src/config.rs with @docs/configuration.md
❯ !git status
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
| `/export [path]` | Write the conversation to a file: `.md`, `.html` or `.jsonl` |
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
| `/model [name [fallback …]]` | Pick the model, or set a chain; part of a name or `@role` works too |
| `/thinking [level]` | Pick or set the thinking level: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| `/mode [mode]` | Pick or set the permission mode |
| `/permissions` | List the permission rules in effect and where they come from |
| `/add-dir <path>` | Let agents work in another directory too, until azazel exits |
| `/dirs` | List the directories agents may work in |
| `/changelog [version]` | What changed in each release, or only since `version` |
| `/settings` | Sign in, add API keys and choose the default model |
| `/login [provider] [account]` | Sign in to a provider or store its API key, as another account when a label is given |
| `/logout <provider> [account]` | Forget the stored key or login of a provider |
| `/account [provider]` | The provider's accounts and their usage: use one for this session, make it the default, sign it out, add another |
| `/fast [on\|off\|status]` | Faster replies at a higher price (the priority [service tier](models-and-logins.md#service-tiers)) |
| `/slow [on\|off\|status]` | Cheaper, slower replies (the economy tier) |
| `/preset [name \| save <name>]` | List the saved sets of models, apply one, or save the current one |
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

A custom theme is a file `~/.azazel/themes/<name>.toml` that maps roles to colours. Every role is optional: a role
left out keeps the colour of `dark`, or of `light` when the file sets a light `code_bg`.

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
user_bg = "#2b304a"
diff_add_bg = "#225c2b"
diff_del_bg = "#7a2936"
```

| Role | Used for |
|---|---|
| `text` | Ordinary text |
| `muted` | Notices, hints, details, the status line |
| `accent` | Headings, the `❯` prompt, spinners, the frame of a running tool card |
| `success`, `warning`, `error` | State icons, warnings and errors, the frames of interrupted and failed tool cards |
| `border` | The rules around the input and overlays, quote bars, table lines |
| `border_muted` | The frame of a finished tool card |
| `code_bg` | Background of code blocks |
| `user_bg`, `user_text` | The band behind your own messages and the text on it |
| `selected_bg` | Background of the selected row in pickers and completion lists |
| `diff_add`, `diff_del` | The `+` and `-` signs of a diff |
| `diff_add_bg`, `diff_del_bg` | Background of added and removed diff lines |
| `thinking` | The model's reasoning and the thinking level in the status line |
| `bash` | The input rules while a `!` shell command is typed |
| `mode_plan`, `mode_edits`, `mode_auto` | The input rules and the status-line mode in `plan`, `accept-edits` and `auto` |

A colour is one of:

| Value | Meaning |
|---|---|
| `"#rrggbb"` | A truecolor value |
| integer or `"0"`-`"255"` | An xterm palette index |
| `"black"`, `"red"`, `"green"`, `"yellow"`, `"blue"`, `"magenta"`, `"cyan"`, `"white"` | ANSI colours; prefix `bright_` for the bright variants; `"gray"` also works |
| `"default"` | The terminal's own colour |

An unknown role or an invalid colour is reported; at startup `dark` is used instead. Truecolor is used when
`COLORTERM` is `truecolor` or `24bit`, or in Windows Terminal; otherwise colours are mapped to the nearest of 256.
