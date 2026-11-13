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
| Input | always | The editor between two rules, with its completion list, notices, queued messages, stashed drafts and an attached-image count above it |
| Status line | always | Session facts on the left, the model on the right |

Your own messages appear in the transcript on a band that spans the terminal's width.

### Input

The input sits between two full-width rules, with `❯` in front of the first row. An empty input shows a hint
(`Ask anything, / for commands, @ for files`). The input grows with what you type up to 30 % of the terminal's
height (at least 5 rows) and scrolls beyond that; the rules then say how much is out of view (`↑ 3 more`).

While the agent is working, the rule above the input carries the working indicator: a spinner, `working` /
`thinking` / `running <tool>`, the elapsed time and `esc to interrupt`. With [Vim keys](#vim-keys) on, the same
rule shows `-- NORMAL --` in normal mode.

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
digits `1`-`9` choose a row directly. Some pickers have keys of their own that act on the selected row (`m` in
the agent hub, `Ctrl+F` in the model picker, `d` in `/permissions`); the rule below names them. Where an overlay
asks for text (feedback on a denied call, changes to a plan), `Ctrl+G` opens your editor for it.

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

The file a `read`, `write` or `edit` card names is a link to that file in terminals that follow
[links](#links), and a picture a tool returns is shown under its card where the terminal draws
[images](#images-and-diagrams).

### Status line

| Segment | Shown when | Content |
|---|---|---|
| mode | always | The permission mode: `ask`, `⏸ plan`, `⏵⏵ accept-edits`, `⏵⏵ auto`, in the colour of the input rules |
| `⑂ branch` | the project is a git repository | The checked-out branch |
| directory | always | The working directory, with `~` for your home directory |
| `◫ N%` | the model's context window is known | How full the context window is; warning colour above 70 %, error colour above 90 % |
| `$cost` | the model's prices are known | Money spent in this session |
| `◔ 5h 84%` | a usage window of the subscription in use is at least 80 % full | The fullest window and how full it is; `/usage` shows them all |
| `⇶ N agents` | sub-agents are active | How many are queued, working or waiting for approval |
| `⚑ goal …` | a goal is open | The goal's state and the attempt number; see [goals](goals.md) |
| `⬢ model · level` | always, against the right edge | The model answering and the thinking level |

When the terminal is too narrow for both sides, the model is shortened first, then left out.

## Keys

| Key | Action name | Effect |
|---|---|---|
| `Enter` | `submit` | Send the prompt. While the agent is working: steer it (the message is delivered before its next step) |
| `Alt+Enter` | `follow_up` | While the agent is working: queue the message until it finishes; see [queued messages](#queued-messages-and-stashed-drafts) |
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
| `Ctrl+G` | `external_editor` | Edit the draft in your editor; see [external editor](#external-editor) |
| `Alt+Up`, `Shift+Up` | `dequeue` | Take the last queued message back into the input |
| `Ctrl+S` | `stash` | Park the draft; on an empty input, bring the last parked draft back |
| `Alt+T` | `cycle_thinking` | Next thinking level the current model supports |
| `Alt+M`, `Alt+Shift+M` | `cycle_model`, `cycle_model_back` | Next or previous favourite model |
| `F2` | `transcript` | Open the [transcript viewer](#transcript-viewer) |
| `F5` | `retry` | Same as `/retry` |
| `Ctrl+B` | `background` | While a `shell` command of the agent runs: move it to the background |

`Esc` interrupts the lead only; stop sub-agents from the agent hub or with `/stop`. In the agent hub, `Enter`
follows the selected agent's work in the transcript, `m` messages it, `s` stops it and `S` stops all.

Editing keys in the input: arrows, `Home`/`End`, `Ctrl+A`/`Ctrl+E` (line start/end), `Ctrl+Left`/`Ctrl+Right` or
`Alt+B`/`Alt+F` (word), `Ctrl+W` and `Ctrl+Backspace` (delete word back), `Alt+D` and `Ctrl+Delete` (delete word
forward), `Ctrl+U` (delete to line start), `Ctrl+K` (delete to line end), `Ctrl+Z` (undo), `Ctrl+Shift+Z` (redo).
`Up` on the first line and `Down` on the last walk through earlier prompts.

What `Ctrl+K`, `Ctrl+U`, `Ctrl+W` and `Alt+D` cut goes onto a kill ring of 20 entries. `Ctrl+Y` pastes the newest
one; `Alt+Y` right after that swaps in the one before, and so on around the ring.

`Alt+T` steps through the thinking levels the current model supports, `off` included. `Alt+M` and `Alt+Shift+M`
step through your favourite models: `Ctrl+F` in the model picker (`Ctrl+L`, `/model`) adds the selected model to
the favourites or takes it out, and the list is saved as `tui.favourites` in `~/.azazel/config.toml`. With no
favourites the keys step through `@default`, `@smol` and `@slow`.

`Ctrl+B` works while a `shell` command the agent started is running. The command goes on as a background job
(it is not started again), the agent's call returns at once with the job's id, and `/jobs` lists it.

### Queued messages and stashed drafts

`Alt+Enter` while the agent works does not hand the message to the agent. The UI keeps it, shows it as a muted
`⏵ queued:` line above the input, and sends it when the agent finishes, exactly like a prompt you type then:
`@` files are attached, a checkpoint is taken and prompt hooks run. Several queued messages go one per run, in
order. `Alt+Up` (or `Shift+Up`) takes the last one back into the input with its images. Interrupting with `Esc`
keeps the queue; the next message goes after the next run that finishes on its own.

`Ctrl+S` parks a draft (with its attached images) and clears the input; a line above the input counts what is
parked. `Ctrl+S` on an empty input brings the last parked draft back.

### Pastes and dropped files

A paste of more than 10 lines (`editor.fold_paste_lines`) or 800 characters goes into the input as one token,
`[paste #2 +41 lines]`. The cursor steps over the token as over one character, `Backspace` after it removes it
whole, and the agent receives the full text. The transcript shows the token.

Windows consoles do not mark pasted text, so there azazel treats keys that arrive faster than anyone types as a
paste: line breaks inside it do not send the prompt. One short line that ends with a line break is taken as
typed and sent, as in a shell.

Files dropped on the terminal arrive as pasted paths. When a paste is nothing but absolute paths of existing
files (quoted, backslash-escaped or `file://` forms included), images are attached to the next prompt and other
files are inserted as `@path` mentions; a path with spaces is written `@"my notes/a file.md"`.

### External editor

`Ctrl+G` writes the draft (folded pastes written out) to a file, opens it in your editor and puts what you
saved back into the input. The editor is `[editor] command`, else `$VISUAL`, else `$EDITOR`, else `notepad` on
Windows and `vi` elsewhere; a command may carry arguments (`code --wait`). If the program cannot be started you
get `no editor found: set $VISUAL or $EDITOR, or [editor] command`.

### Vim keys

`/vim` switches Vim keys in the input on or off and saves the choice (`editor.mode = "vim"`). The input starts
in insert mode, where everything works as without Vim keys. `Esc` goes to normal mode, shown as `-- NORMAL --`
in the rule above the input; `Esc` in normal mode does what it always does (interrupt, rewind).

| Normal-mode keys | Effect |
|---|---|
| `h` `j` `k` `l`, `w` `b` `e`, `0` `^` `$`, `gg` `G` | Move; a count in front repeats (`3w`), and `5G` goes to line 5 |
| `i` `a` `I` `A` `o` `O` | Enter insert mode at the cursor, after it, at the line's start or end, on a new line below or above |
| `x` | Delete the character under the cursor |
| `d` + motion, `dd`, `D` | Delete: `dw`, `d$`, a whole line, to the end of the line |
| `c` + motion, `cc`, `C` | Change: delete, then insert mode |
| `yy`, `y` + motion | Copy a line or what the motion covers |
| `p` `P` | Paste after or before the cursor; whole lines go on a line of their own |
| `u`, `Ctrl+R` | Undo, redo |

Deleted and copied text shares the kill ring with the emacs keys. There is no visual mode, no `.` repeat and no
named registers. In normal mode `Ctrl+R` is redo; prompt history search is `Ctrl+R` in insert mode.

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
| `!!` | `!!command` runs the same way, but the output is for you only: nothing is added to the agent's context |
| `# ` | A one-line input such as `# always run clippy before committing` is saved as a project memory of kind `preference`, and its id is shown. Nothing is sent to the agent. See [memory](memory.md) |

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
| `/transcript` | Read the whole conversation full screen; see [transcript viewer](#transcript-viewer) |
| `/copy [code \| n]` | Copy the last reply as Markdown; `/copy code` copies one of its code blocks, `/copy 2` the reply before the last |
| `/vim` | Switch [Vim keys](#vim-keys) in the input on or off |
| `/open [n \| path \| diagram]` | Open the last link of the last reply in your browser; `/open 2` its second link, `/open <path>` a file, `/open diagram` the last Mermaid diagram as an SVG file |
| `/clear` | Clear the screen and scrollback (the conversation is kept) |
| `/cost` | Requests, tokens and money spent in this session |
| `/usage` | What each signed-in subscription has left of its usage windows; see [usage](models-and-logins.md#usage) |
| `/stats [days]` | Sessions, prompts, tokens and cost of the last 30 days (or `days`), over every project: what `azazel stats` prints |
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
| `/permissions` | The permission rules in effect and where they come from. `d` removes a rule added in this session or remembered with "always allow"; `a` adds one (`allow shell(cargo test:*)`), for the session or saved. Rules from config files are shown with the file to edit |
| `/forget-approvals` | Forget every remembered approval and every rule added in this session |
| `/trust [yes\|no]` | Whether this project is trusted and what that covers; `yes` or `no` records the decision, a restart applies it |
| `/add-dir <path>` | Let agents work in another directory too, until azazel exits |
| `/dirs` | List the directories agents may work in |
| `/changelog [version]` | What changed in each release, or only since `version`, in the [viewer](#transcript-viewer) |
| `/settings` | Sign in, add API keys, choose models and change settings; see [settings menu](#settings-menu) |
| `/login [provider] [account]` | Sign in to a provider or store its API key, as another account when a label is given |
| `/logout <provider> [account]` | Forget the stored key or login of a provider |
| `/account [provider]` | The provider's accounts and their usage: use one for this session, make it the default, sign it out, add another |
| `/fast [on\|off\|status]` | Faster replies at a higher price (the priority [service tier](models-and-logins.md#service-tiers)) |
| `/slow [on\|off\|status]` | Cheaper, slower replies (the economy tier) |
| `/preset [name \| save <name>]` | List the saved sets of models, apply one, or save the current one |
| `/tools` | The tools offered to the lead, grouped by where they come from (core, each MCP server, scripts, team), with disabled and deferred ones marked |
| `/jobs` | Background jobs; choose one to read its output or stop it |
| `/todo [add <text> \| done <n> \| clear]` | Show the agent's task list, or change it |
| `/init` | Have the agent explore the repository and write `AGENTS.md` |
| `/doctor` | Check the installation and this project's setup |
| `/rules` | List the rule files, when each applies and whether it has in this session; see [rules](rules.md) |
| `/hooks` | List the hooks, where they come from and how each last ran; see [hooks](hooks.md) |

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
| `/reload` | Read skills, custom commands, agent definitions, hooks, key bindings and the theme again, then drop all Luau extension state and load the scripts again ([extensions](extensions.md)). It reports what changed and every file that failed to load |
| `/mcp` | MCP servers and their state; also `/mcp reconnect\|enable\|disable\|approve\|reject\|login\|logout <server>`; see [MCP](mcp.md) |
| `/mcp:<server>:<prompt>` | Run a prompt offered by an MCP server |
| `/memory [list \| forget <id> \| auto on\|off]` | What is remembered; see [memory](memory.md) |
| `/media` | Media backends, configured models and the output directory; see [media and sprites](media-and-sprites.md) |
| `/lsp` | Language servers and their state; see [LSP](lsp.md) |

Custom commands from `commands/` directories and commands registered by extensions appear in `/help` and in
completion as well. `/memory` is absent when memory is disabled in the configuration.

`/copy` uses the system clipboard. Inside tmux it hands the text to tmux first (`tmux load-buffer -w`), which
keeps it as its newest buffer and passes it on to the terminal's clipboard. Over SSH, or when the system
clipboard fails, it asks the terminal to copy instead (OSC 52, passed through tmux when azazel runs inside it);
the terminal must allow programs to set the clipboard.

## Transcript viewer

`F2` or `/transcript` opens the conversation of the current branch full screen: your prompts, the replies, tool
calls with their results, and thinking when it is shown in the transcript. It uses the terminal's alternate
screen, so the scrollback is as you left it when you return. This is the only place where azazel takes the
mouse: the wheel scrolls here, and text selection with the mouse works everywhere else as usual.

| Key | Effect |
|---|---|
| arrows, `j` `k` | Move the cursor a row |
| `PgUp` `PgDn`, `b` `Space` | Move a screen |
| `g` `G`, `Home` `End` | First and last row |
| mouse wheel | Scroll |
| `[` `]` | Previous and next prompt of yours |
| `/` | Search (case does not matter); `Enter` goes to the first match, `n` and `N` step through the rest |
| `y` | Copy the message under the cursor as Markdown |
| `c` | Copy the code block under the cursor, or the next one of the message |
| `v` | Start selecting lines; move, then `y` copies them |
| `o` | Open the link on the cursor row in your browser |
| `q`, `Esc` | Return |

The agent keeps working while the viewer is open. If it needs you (an approval, a question), the last row says
so and the request is shown when you return.

## Attention, title and progress

azazel tells the terminal what the session is doing, and calls for you when it needs you and you are looking
elsewhere. Everything here is in the `[notify]` table of the [configuration](configuration.md) and in the
Notifications section of the [settings menu](#settings-menu).

| What | When | Setting |
|---|---|---|
| Bell | An approval, a question or a plan waits for you; a run finished or failed after at least `min_secs` seconds | `bell` |
| Desktop notification | The same moments | `desktop` |
| Window title | Always: `azazel · <session name or folder>`, with `* ` in front while agents work and `! ` while one waits for you | `title` |
| Tab progress | Indeterminate while agents work, paused while one waits for you, error after a failed run until you type, cleared when idle | `progress` |
| Keep awake | The machine is kept from sleeping while agents work and for 30 seconds after; the display may still turn off | `keep_awake` |

The bell and the notification only come when the terminal window does not have the focus. The terminal reports
that itself (focus reporting); one that never does counts as not focused, so there the bell rings every time.

`desktop` chooses how the notification is sent:

| Value | Path |
|---|---|
| `auto` (default) | The terminal's own notification where it has one (iTerm2 and WezTerm: OSC 9, kitty: OSC 99, Ghostty: OSC 777); otherwise the operating system's (a Windows toast, the macOS notification centre, the desktop's notification service on Linux) |
| `terminal` | Always the terminal's; one that was not recognised is sent OSC 9 |
| `system` | Always the operating system's |
| `off` | None |

Notification hooks ([hooks](hooks.md), event `notification`) run first, as before. The title that was there
when azazel started is put back when it exits. Progress is shown by Windows Terminal, Ghostty, WezTerm and
iTerm2 (OSC 9;4).

Inside tmux, notifications and progress are wrapped so that tmux passes them to the terminal outside; that needs
`set -g allow-passthrough on` in tmux. The bell is sent as it is, so tmux can flag the window.

Operating-system notifications and keeping the machine awake are the cargo feature `notify`, part of the default
build. A machine without a notification service (a server reached over SSH) shows none.

## Links

In terminals that follow hyperlinks (OSC 8), Markdown links and addresses written out in a reply are clickable,
and so is the file name in the header of a `read`, `write` or `edit` card. A link keeps its address behind it in
brackets, so it can still be read and copied where clicking is not possible. Links are on in kitty, Ghostty,
WezTerm, iTerm2, Windows Terminal and VTE-based terminals (GNOME Terminal and others); they are off in Warp and
inside tmux older than 3.4.

`/open` opens the last link of the last reply in your browser, `/open <n>` its n-th link, `/open <path>` a file
with the program your system uses for it. In the [viewer](#transcript-viewer), `o` opens the link on the cursor
row. These work in every terminal.

## Images and diagrams

Where the terminal draws images, a picture that a tool returns (an image file that was `read`, a screenshot, a
generated image) is shown under the tool's card, at most 60 columns by 20 rows; larger pictures are scaled down.

| Terminal | Protocol |
|---|---|
| kitty, Ghostty | kitty graphics |
| iTerm2, WezTerm | iTerm2 inline images |
| Windows Terminal 1.22 and later, and others that say so | Sixel |

kitty, Ghostty, iTerm2 and WezTerm are recognised from their environment variables. Sixel is used only when the
terminal lists it in its answer to a device query at start; azazel asks for the size of a cell in pixels at the
same time, and assumes 10 by 20 when it is not told. A terminal that does not answer within 80 ms gets no Sixel.
Inside tmux images are off unless a protocol is forced.

```toml
[tui]
images = "auto"   # or off, kitty, iterm, sixel
```

A fenced `mermaid` block in a reply is drawn as an image once the block is complete (flowcharts, sequence, class,
state and ER diagrams, pie charts, Gantt charts, timelines, mind maps and git graphs). While the block is still
arriving, and in terminals without images, its source is shown as code. A diagram that does not parse keeps its
source, with one line saying why. `/open diagram` writes the last diagram of the conversation to the project's
artifacts folder as SVG and opens it, which works in every terminal.

Diagram text is drawn with one font that ships with azazel (Liberation Sans), so a diagram looks the same on
every machine; characters that font lacks (CJK, emoji) are left out. The first diagram of a run takes a moment
longer: the layout measures its text once with a font of the system.

Inline images are the cargo feature `images`, diagrams the feature `diagrams`; both are part of the default
build.

## Settings menu

`/settings` opens with signing in, API keys and models, followed by one section per group of everyday settings.
Each change is saved at once in `~/.azazel/config.toml` (comments and everything else in the file are kept).
Typing nothing for a text setting puts it back on its default. A project's own configuration still wins over
these values inside that project.

| Section | Settings |
|---|---|
| Appearance | theme\*, inline images, show thinking\*, full tool output\* |
| Editor | emacs or Vim keys\*, external editor command\*, paste fold length\* |
| Permissions | the mode sessions start in, what destructive commands need |
| Notifications | bell, desktop notifications, terminal title, tab progress, minimum run length, keep awake |
| Limits | cost cap, cost warning, turn cap |
| Sessions | automatic names, restoring model and thinking on resume, retention |
| Memory | on or off, saving memories from conversations, indexing the project |
| Updates | daily check, automatic install |
| Voice | transcription and speech models, voice, send at once, longest recording |
| Team | coordinator mode, agents at the same time, advisor and its model |

Settings marked \* change the running UI at once. The others are saved and say "applies to the next session".
The keys behind each row are described in [configuration](configuration.md).

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
