# The terminal UI

Running `azazel` with no subcommand and no `-p` opens the interactive UI. It is inline, not full-screen: finished
output is written once into your terminal's own scrollback, and only the live region below it is redrawn. This
page lists the layout, keys, input prefixes, slash commands and themes. The UI needs a real terminal; without
one, use [print mode](getting-started.md#print-mode) or [RPC mode](rpc.md).

The same UI opens on a session that runs elsewhere: `azazel attach <id>` on a background session, and
`azazel --ssh <host>` on another machine. A few commands need the session to be in the UI's own process;
[background sessions and remote work](background-and-remote.md#attaching) lists them.

## Layout

```text
 fix the failing test

I'll run the tests first.

╭─── ✔ shell cargo test ───────────────────────────────────────────────────╮
│ test result: FAILED. 11 passed; 1 failed                                 │
│ … 14 more lines (ctrl+o to expand)                                       │
╰── 1.4s · exit 101 · 1.9k chars ──────────────────────────────────────────╯
╭─── ⠹ edit src/lib.rs ────────────────────────────────────────────────────╮
╰── 0s ────────────────────────────────────────────────────────────────────╯

── ⠹ running edit · 12s · esc to interrupt ─────────────────────────────────
❯ Steer the agent, alt+enter queues a follow-up
────────────────────────────────────────────────────────────────────────────
manual · ⑂ main · ~/src/app · ◫ 12% · ↯ 78 tok/s · $0.04    ⬢ provider/model · medium
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
| Tool cards | a tool is running | One framed card per running tool: spinner, tool name, a one-line summary of its input, the last 5 output lines, elapsed time. In the [compact style](#compact-style), the group of calls that is still growing instead |
| Roster | sub-agents are active | One line per queued, working or approval-waiting agent; see [team](team.md) |
| Todo list | the agent has unfinished todos | The lead agent's task list |
| Input | always | The editor between two rules, with its completion list, notices, queued messages, stashed drafts and an attached-image count above it |
| Status line | always | Session facts on the left, the model on the right |

Your own messages appear in the transcript on a band that spans the terminal's width.

### Input

The input sits between two full-width rules, with `❯` in front of the first row. An empty input shows a hint
(`Ask anything, / for commands, @ for files`). The input grows with what you type up to 30 % of the terminal's
height (at least 5 rows) and scrolls beyond that; the rules then say how much is out of view (`↑ 3 more`).

While the agent is working, the rule above the input carries the [working indicator](#working-indicator): a
spinner, `working` / `thinking` / `running <tool>`, the elapsed time and `esc to interrupt`. With
[Vim keys](#vim-keys) on, the same rule shows `-- NORMAL --` in normal mode. While you dictate it shows
`● rec`, the time and a level meter, then `transcribing`; see [voice](voice.md).

The colour of the rules tells you what `Enter` will do:

| Rules | Meaning |
|---|---|
| border colour | Permission mode `manual` |
| `mode_plan` colour | Permission mode `plan` |
| `mode_edits` colour | Permission mode `accept-edits` |
| `mode_auto` colour | Permission mode `auto` |
| `error` colour | Permission mode `bypass`: nothing is asked and no guard applies |
| `bash` colour (pink) | The input starts with `!`: it runs as a shell command |

### Popups

Questions from agents, approval prompts, plan reviews, pickers and every step of `/settings` open as a box in
the middle of the window, over the transcript, which is dimmed behind it:

```
  azazel 0.4.0 · anthropic/claude-opus-5-5 · ~/src/app
  ╭─ Model ──────────────────────────────────────────────────────────╮
  │ current: anthropic/claude-opus-5-5                               │
  │                                                                  │
  │ ⌕ opus                                                     3/41  │
  │ ▌ anthropic/claude-opus-5-5   ctx 200k · subscription            │
  │   anthropic/claude-opus-5     ctx 200k · subscription            │
  │   openrouter/anthropic/opus   ctx 200k · $15/$75                 │
  ╰─ type to filter · ↑↓ move · enter pick · ctrl+f favourite ───────╯
──────────────────────────────────────────────────────────────────────
manual · ⑂ main · ~/src/app                        ⬢ claude-opus-5-5
```

- The title is in the top edge and the keys are in the bottom edge. The box is nine tenths of the window wide
  (at least 44 and at most 100 columns) and as tall as its content, up to four rows less than the window.
- The selected row has a bar at its left, a background across the row and its label in bold. Details stand in a
  column of their own and are cut, never wrapped. A list longer than the box scrolls, with a scrollbar inside
  the right edge.
- `Up`/`Down` (or `Tab`) move, `PgUp`/`PgDn` move a page, `Enter` picks and `Esc` cancels. Searchable lists
  (models, sessions, the session tree, prompt history, any list of more than twelve options) have a filter row
  that shows what you typed and how many rows match (`3/41`); in the others, digits `1`-`9` pick a row directly.
- Some pickers have keys of their own that act on the selected row (`m` in the agent hub, `Ctrl+F` in the model
  picker, `d` in `/permissions`, `Ctrl+R` `Ctrl+D` `Ctrl+P` `Tab` in the session picker, `Alt+L` in `/tree`);
  the bottom edge names them, or a line above the list where they are many.
- An approval lists what the call touches above the choices: `Allow once`, `Allow this session` and
  `Always allow <rule>` when a rule covers the call, `Deny`, and `Deny and say why`; see
  [permissions](permissions.md#the-approval-prompt). A note about the call (it may cost money, it deletes
  files) is in the warning colour, and a call that would destroy work has the whole box in that colour, starts
  on `Deny` and can only be allowed once.
- A question from the agent (`ask_user`) shows what the agent says you need to know, then the question, then
  the choices: a list to pick from, check boxes with a `Done` row where several may be chosen, `Yes` and `No`,
  or a line to type on. A list that ends with `Other…` lets you type your own answer instead. `Esc` declines;
  the agent is told and goes on without an answer.
- A plan review shows the plan in the box. Text that does not fit (a long plan, a long command) ends with
  `… N more lines`; `Ctrl+O` opens all of it in the [full-screen viewer](#transcript-viewer) and `q` returns to
  the box.
- Where a box asks for text (feedback on a denied call, changes to a plan), `Shift+Enter` adds a line and
  `Ctrl+G` opens your editor for it. A secret (an API key) shows as dots.

The agent keeps working behind a popup: its reply and its running tools show in the dimmed transcript, and the
last two rows of the window stay what they are, the working row and the [status line](#status-line). Nothing is
written to the real transcript while a popup is open. When it closes, the transcript is exactly as it was, and
what was said meanwhile is added below it.

A window smaller than 44 columns by 12 rows has no room for a popup: there the same box takes the place of the
input until it is answered. `popups = false` under `[tui]` in [configuration](configuration.md) makes that the
rule in every window.

### Working indicator

A band of light passes over the words of the working indicator, so a glance tells a run that is alive from a
terminal that hangs. `tui.indicator` (or `/settings`, Appearance) chooses how it moves:

| `tui.indicator` | Movement |
|---|---|
| `shimmer` (default) | A soft band enters from the left, crosses the words one cell per frame and leaves, over and over |
| `kitt` | One bright cell runs to the end of the words and back, with a fading trail |
| `static` | Nothing moves but the spinner |

The words are muted, the edge of the band is in the text colour and its centre is bold in the accent colour.
The band is redrawn about 30 times a second, and only while the agent works; `static` keeps the spinner's 12.

| Key in `[tui]` | Effect |
|---|---|
| `indicator_text = ["Working", "Still at it"]` | Phrases shown in turn, one pass of the band each, in place of `working` / `thinking`. A running tool is still named (`running shell`) |
| `session_accent = true` | The centre of the band takes a colour derived from the session's name instead of the theme's accent: the same name always gives the same colour, so two terminals with two sessions are told apart at a glance. Renaming the session changes it. Every such colour has the same lightness and saturation, and hues that read badly on a dark background (yellow-green, cyan) are left out |

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
rest, and the bottom border says how long the whole output is (`4.1k chars`). `Ctrl+O` expands tool calls that
finish from then on (up to 40 lines of output, all of a diff). Diffs have a line-number gutter, and added and
removed lines are filled with a background colour.

What reading and searching return is drawn for what it is. None of this changes what the model receives:

| Call | Inside the card |
|---|---|
| `read` of a file | A listing: the file's own line numbers in a column, a bar, then the code coloured by its language (taken from the file's extension). Long lines are cut, not wrapped. A file of an unknown kind, and a listing of more than 128 KB, is numbered and left plain. The line that says which part of the file was read (`[lines 40-120 of 900; …]`) closes the listing |
| `read` of a directory | One level of a tree: `├─ src/` for a directory (bold, accent), `├─ Cargo.toml  1.2 KB` for a file, sizes in a column |
| `grep` | The name of each file once (bold, accent), under it `NNN │ line` for every line found in it. Where the line holds the text you searched for as it was written, that text is bold in the warning colour; a regular expression is never run again to find it, so a pattern that is more than plain text marks nothing. Lines of context are muted, and `…` stands where lines were skipped. `mode: "files"` is grouped like `glob` |
| `glob` | Each directory once (bold, accent, `./` for the directory searched), its files indented under it. Directories keep the order of their newest file |
| `web_grep` | `NNN │ line` rows like `grep`, for the one page |

```text
╭─── ✔ read src/main.rs ───────────────────╮   ╭─── ✔ grep run ───────────────────────────╮
│   1 │ fn main() {                        │   │ src/main.rs                              │
│   2 │     run();                         │   │   2 │     run();                         │
│   3 │ }                                  │   │ src/lib.rs                               │
╰── 2ms ───────────────────────────────────╯   │  14 │ pub fn run() {                     │
                                               ╰── 6ms ───────────────────────────────────╯
```

With `tui.glyphs = "nerd"`, files and directories in trees and search results have an icon: forty kinds of file
by extension or name (`rs`, `py`, `ts`, `md`, `Dockerfile`, `.gitignore`, pictures, archives…), a folder, and a
plain file for everything else. Their colours are roles of the [theme](#themes), so a theme recolours them.

#### Compact style

`tui.tool_style = "compact"` (or `/settings`, Appearance, tool call style) shows a finished call on one row
instead of a card: the state icon, the tool, what it was about in brackets, then how many lines it returned, how
long it took and, for `shell`, the exit code. A call that failed or was refused ends with the first line of
what went wrong. Two or more calls that finish one after another stand as a tree under a row that counts them
and shows the worst of their states. An edit keeps its diff under its row; nothing else has a body.

```text
✘ 3 tool calls · 1 failed
├─ ✔ read (src/main.rs) · 42 lines · 3ms
├─ ✔ edit (src/main.rs) · 12ms
│  12 -    old();
│  12 +    new();
└─ ✘ shell (cargo test) · 1.4s · exit 101 · error[E0425]: cannot find function `new`
```

A group grows above the input while its calls come in and is written to the transcript when the agent says
something else or the run ends. A call that is still running stands in the same tree with a spinner, the time
it has been running and, under its row, the last 5 lines it printed. `Ctrl+O` writes the calls of the group on screen again as full cards and shows the calls that follow as
cards until it is pressed again; the [transcript viewer](#transcript-viewer) always shows full cards.

#### Hiding secrets

With `tui.redact` on (the default), what looks like a secret is replaced by `[redacted:<kind>]` in tool cards
(the summary, the body, a diff and the live output of a running call), in the transcript viewer and what is
copied from it, and in Markdown and HTML [exports](sessions-and-checkpoints.md). It is a matter of display only:
the model and the session file keep the text, and a `.jsonl` export is the session as stored, so that it can be
opened again.

| `<kind>` | Recognised by |
|---|---|
| `anthropic-key`, `openai-key` | `sk-ant-…`, `sk-…` (32 characters or more) |
| `github-token` | `ghp_…`, `gho_…`, `ghu_…`, `ghs_…`, `ghr_…`, `github_pat_…` |
| `google-key`, `aws-key`, `slack-token` | `AIza…`, `AKIA…` / `ASIA…`, `xoxb-…` (and `xoxa`, `xoxp`, `xoxr`, `xoxs`) |
| `jwt` | Three base64 parts that start `eyJ….eyJ….` |
| `bearer` | The token after `Bearer`; the word stays |
| `secret` | The value of an assignment to a name in capitals with `KEY`, `TOKEN`, `SECRET` or `PASSWORD` in it (`DB_PASSWORD=…`); the name stays. Numbers, `true`/`false`, and references such as `$OTHER` are left alone |
| `private-key` | A PEM block from `-----BEGIN … PRIVATE KEY-----` to its end |

This catches the common shapes, not every secret: a password in prose or a key of an unknown shape is shown.

The file a `read`, `write` or `edit` card names is a link to that file in terminals that follow
[links](#links), and a picture a tool returns is shown under its card where the terminal draws
[images](#images).

### Status line

| Segment | Shown when | Content |
|---|---|---|
| mode | always | The permission mode: `manual`, `⏸ plan`, `⏵ accept-edits`, `⏵⏵ auto`, `⚠ bypass`, in the colour of the input rules (`bypass` in the colour of errors) |
| `coordinator` | [coordinator mode](team.md#coordinator-mode) is on | The lead plans and delegates only |
| `⑂ branch` | the project is a git repository | The checked-out branch |
| directory | always | The working directory, with `~` for your home directory. A long one keeps its end (`…/crates/azazel-tui`) and at most a third of the line |
| `◫ N%` | the model's context window is known | How full the context window is; warning colour above 70 %, error colour above 90 %. On a terminal at least 100 columns wide a bar follows it: `◫ 42% [━━━╸────]` |
| `↯ N tok/s` | the lead has streamed a reply since your last prompt | How fast the lead writes, in output tokens per second; see below |
| `$cost` | the lead answers with an API key and the model's prices are known | Money spent in this session: `$0.42`, `$12.4`. In the warning colour from `limits.warn_cost_usd` on |
| `◔ 61% 5h` | the lead answers on a subscription you signed in to | The fullest usage window of the account and how full it is, in the warning colour from 80 % on; `/usage` shows them all |
| `⇶ N agents` | sub-agents are active | How many are queued, working or waiting for approval |
| `⚑ goal …` | a goal is open | The goal's state and the attempt number; see [goals](goals.md) |
| `⬢ model · level` | always, against the right edge | The model answering and the thinking level |

**Speed.** Each reply of the lead is timed from its first text or thinking to its end, so the wait for the
model to start is not part of the speed. While a reply streams, the number is an estimate (characters received,
four to a token); when the reply ends, the tokens the provider counted (reasoning included) replace the
estimate. The segment is in the accent colour while the lead works and stays, muted, after the turn until your
next prompt. Speeds under 1 tok/s and replies that arrive in one piece are not shown. Sub-agents are not
measured.

**Cost or usage.** What a session costs is only money when tokens are paid for one by one. When the lead's
provider is used through a sign-in (`azazel login`, a Claude or ChatGPT subscription), the cost is hidden and
the usage window is shown instead, as soon as the provider has reported one. With an API key (from
`--api-key`, the environment, `api_key_command`, the configuration or `auth.json`) the cost is shown. A key in
the environment wins over a stored sign-in, as it does for the requests themselves. A session that runs in
another process (`azazel attach`, `--ssh`) shows the cost that process reports, and nothing when it reports
none.

When the terminal is too narrow for both sides, whole segments leave the left side from its end: the goal
first, then the agents, the cost or usage, the speed, the context, the directory and the branch. The mode
always stays. Only then is the model shortened, and after that left out.

## Keys

| Key | Action name | Effect |
|---|---|---|
| `Enter` | `submit` | Send the prompt. While the agent is working: steer it (the message is delivered before its next step) |
| `Alt+Enter` | `follow_up` | While the agent is working: queue the message until it finishes; see [queued messages](#queued-messages-and-stashed-drafts) |
| `Shift+Enter`, `Ctrl+J` | `newline` | Insert a line break |
| `Esc` | `abort` | Interrupt the lead agent. Twice within a second on an empty, idle input: open `/rewind` |
| `Ctrl+C` | `clear_or_exit` | Clear the input; on an empty input, twice within a second exits |
| `Ctrl+D` | `exit` | Exit when the input is empty |
| `Shift+Tab` | `cycle_mode` | Next permission mode: `plan`, `manual`, `accept-edits`, `auto`. `bypass` is not a step: it is entered with `/mode bypass` and confirmed |
| `Alt+A` | `agent_hub` | Open the agent hub |
| `Ctrl+O` | `toggle_tool_output` | Expand or collapse tool output; in the [compact style](#compact-style), also write the last group of calls as full cards |
| `Ctrl+T` | `toggle_thinking` | Show or hide thinking |
| `Ctrl+L` | `model_picker` | Pick the model |
| `Ctrl+R` | `history_search` | Search what you sent in this project, in any session, and put one prompt back in the input |
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
| `Ctrl+Space` | `voice` | Start recording; again: stop and put the transcript in the input. `Esc` cancels; see [voice](voice.md) |

`Esc` interrupts the lead only; stop sub-agents from the agent hub or with `/stop`. In the agent hub, `Enter`
follows the selected agent's work in the transcript, `m` messages it, `s` stops it and `S` stops all.

Editing keys in the input: arrows, `Home`/`End`, `Ctrl+A`/`Ctrl+E` (line start/end), `Ctrl+Left`/`Ctrl+Right` or
`Alt+B`/`Alt+F` (word), `Ctrl+W` and `Ctrl+Backspace` (delete word back), `Alt+D` and `Ctrl+Delete` (delete word
forward), `Ctrl+U` (delete to line start), `Ctrl+K` (delete to line end), `Ctrl+Z` (undo), `Ctrl+Shift+Z` (redo).
`Up` on the first line and `Down` on the last walk through earlier prompts, those of earlier sessions of the
project included; see [prompt history](sessions-and-checkpoints.md#prompt-history).

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
| `/resume` | Reopen an earlier session of this project, on the model and thinking level it last used; its keys rename, delete, pin and preview sessions |
| `/name [text]` | Name this session; without text, have a name suggested |
| `/session` | What this session is: file, id, name, messages, branch points, tokens and cost, model, checkpoints, agents |
| `/search <text>` | Find sessions of this project by what was said in them; choosing a line resumes its session |
| `/tree` | Go back to an earlier point of this session; `Alt+L` labels an entry, `Ctrl+L` and `Ctrl+U` filter |
| `/fork` | Continue from here in a copy of this session |
| `/retry` | Send your last prompt again from where it was sent; the first attempt stays as a branch |
| `/continue` | Go on with the run that was cut short when azazel was last killed in the middle of it |
| `/handoff [what matters]` | Continue in a new session that starts from a brief of this one |
| `/consult [question]` | Ask a saved session a question without resuming it |
| `/rewind` | Go back to before an earlier prompt: files, conversation or both |
| `/compact [what to keep]` | Summarise the conversation so far |
| `/export [path]` | Write the conversation to a file: `.md`, `.html` or `.jsonl` |
| `/transcript` | Read the whole conversation full screen; see [transcript viewer](#transcript-viewer) |
| `/copy [code \| n]` | Copy the last reply as Markdown; `/copy code` copies one of its code blocks, `/copy 2` the reply before the last |
| `/vim` | Switch [Vim keys](#vim-keys) in the input on or off |
| `/open [n \| path \| diagram]` | Open the last link of the last reply in your browser; `/open 2` its second link, `/open <path>` a file, `/open diagram` the last [Mermaid diagram](#mermaid-diagrams) as an SVG file (in a build that has diagrams) |
| `/clear` | Clear the screen and scrollback (the conversation is kept) |
| `/cost` | Requests, tokens and money spent in this session, with what side questions, commit drafts and the advisor cost |
| `/btw <question>` | Ask about the session on the side while the agent works; see [side questions](#side-questions) |
| `/bg <prompt>` | Run a prompt in the background, in a copy of this session; see [background sessions](background-and-remote.md) |
| `/diff [turn \| session]` | The uncommitted changes, or the changes since your last prompt or since the session began; see [diffs and commits](#diffs-and-commits) |
| `/commit [what to say]` | Commit with a drafted message you edit first; see [diffs and commits](#diffs-and-commits) |
| `/usage` | What each signed-in subscription has left of its usage windows; see [usage](models-and-logins.md#usage) |
| `/stats [days]` | Sessions, prompts, tokens and cost of the last 30 days (or `days`), over every project: what `azazel stats` prints |
| `/context` | How full the context window is, message count, tools offered |
| `/theme [name]` | Pick or set the colour theme; each row of the picker shows the theme's accent, success, warning and error colours |
| `/glyphs [name]` | Pick or set the symbols the UI draws with: `unicode`, `nerd`, `ascii` or `auto`. Each row of the picker is drawn in its own symbols; the choice is saved as `tui.glyphs` and applies at once |
| `/keys` | List key bindings |
| `/quit`, `/exit` | Exit |

`/resume`, `/name`, `/session`, `/search`, `/tree`, `/fork`, `/retry`, `/continue`, `/handoff`, `/consult`,
`/rewind`, `/compact` and `/export` are covered in [sessions and checkpoints](sessions-and-checkpoints.md).

Models, permissions and setup:

| Command | Description |
|---|---|
| `/model [name [fallback …]]` | Pick the model, or set a chain; part of a name or `@role` works too |
| `/thinking [level]` | Pick or set the thinking level: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| `/mode [mode]` | Pick or set the permission mode. `bypass` opens a warning box that starts on `No`; see [permissions](permissions.md#bypass) |
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
| `/review [ref]` | Have a reviewer read the uncommitted changes, or the commits since `ref`, and report findings to the lead |
| `/security-review [ref]` | The same review, limited to security and guided by the `security-review` skill |
| `/task <agent> [name=<id>] <prompt>` | Start any agent yourself; its report goes to the lead. `/task` alone lists the agents |
| `/coordinator [on\|off]` | Have the lead only plan and delegate; alone, shows the mode ([coordinator mode](team.md#coordinator-mode)) |
| `/advise [question]` | Ask the [advisor](team.md#advisor), a second model, about the work so far; only you see the answer |

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
| `/allow-edit <path>` | Let the next edit of one file through, whatever the agent has read of it; see [the read guard](tools.md#the-read-guard) |
| `/voice` | Start recording, or stop and put the transcript in the input; see [voice](voice.md) |
| `/speak on\|off\|stop` | Read replies aloud, stop doing so, or stop the one being read |

Custom commands from `commands/` directories and commands registered by extensions appear in `/help` and in
completion as well. `/memory` is absent when memory is disabled in the configuration.

`/copy` uses the system clipboard. Inside tmux it hands the text to tmux first (`tmux load-buffer -w`), which
keeps it as its newest buffer and passes it on to the terminal's clipboard. Over SSH, or when the system
clipboard fails, it asks the terminal to copy instead (OSC 52, passed through tmux when azazel runs inside it);
the terminal must allow programs to set the clipboard.

## Side questions

`/btw <question>` asks the model something about the session without interrupting it: "which file did you
change first?", "why two parsers?". It works while the agent is busy. The question goes out as a request of its
own, with the same system prompt and tools as the lead's (so the provider's prompt cache is reused) and the
conversation as it stands. The model is told to answer once, from what it already knows, without tools. Nothing
is written to the session, and the agent never sees the exchange unless you send it. The cost is counted in the
session's spend and listed by `/cost` under `btw`.

The answer streams into a panel that takes the place of the input until you close it:

| Key | Effect |
|---|---|
| `Esc` | Close the panel; a request still running is cancelled |
| `c` | Copy the answer as Markdown |
| `s` | Send the question and its answer to the agent: as a steering message while it works, as a prompt when it is idle |
| `f` | Ask a follow-up. The exchange so far goes along with it, and still stays out of the session |
| `↑` `↓`, `PgUp` `PgDn`, `Home` `End` | Scroll a long answer |

If the model answers with a tool call anyway, the call is dropped and the panel says
`the model tried to use <tool>; ask in the main conversation`. An approval or a question from the agent is shown
over the panel and the panel returns afterwards.

## Diffs and commits

`/diff` shows what changed in the [transcript viewer](#transcript-viewer), one item per file, so `[` and `]`
jump between files and `y` copies the diff of the file under the cursor:

| Command | Shows |
|---|---|
| `/diff` | The working tree against `HEAD`: staged and unstaged changes, and untracked files as additions |
| `/diff turn` | What changed since the checkpoint taken before your last prompt |
| `/diff session` | What changed since the session's first prompt |

`/diff turn` and `/diff session` compare [checkpoints](sessions-and-checkpoints.md), so they also work outside a
git repository and include what shell commands changed. Plain `/diff` needs a git repository.

`/commit [what to say]` makes a commit as you:

1. It reads `git status`, the staged diff and the last ten commit subjects. When nothing is staged it asks
   whether to stage everything (`git add -A`); stage a part yourself with `!git add <path>` first to commit only
   that.
2. The `smol` role drafts a message in the style of the recent subjects. Anything you write after `/commit` is
   passed to it as a hint. The draft costs a small request, listed by `/cost` under `commit`.
3. The message opens in an input. Edit it; `Shift+Enter` adds a line and `Ctrl+G` opens it in your editor.
   `Enter` commits, `Esc` or an empty message cancels.
4. `git commit` runs with your identity, hooks and signing. Its output is shown, and the agent is told about the
   commit the way it is told about a `!command`.

`/commit` never amends and never pushes. When there is nothing to commit it says so.

## Transcript viewer

`F2` or `/transcript` opens the conversation of the current branch full screen: your prompts, the replies, tool
calls with their results as full cards, and thinking when it is shown in the transcript. What looks like a
secret is [hidden](#hiding-secrets) here as in the cards. It uses the terminal's alternate
screen, so the scrollback is as you left it when you return. This is the only place where azazel takes the
mouse: the wheel scrolls here, and text selection with the mouse works everywhere else as usual. The same
viewer shows the text of a [popup](#popups) that is too long for its box (`Ctrl+O`).

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

## Images

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

Inline images are the cargo feature `images`, part of the default build.

### Mermaid diagrams

A build with the cargo feature `diagrams` draws a fenced `mermaid` block of a reply as an image, in place, once
the block is complete and where the terminal draws images: flowcharts, sequence, class, state and ER diagrams,
pie charts, Gantt charts, timelines, mind maps and git graphs.

```sh
cargo install --path crates/azazel-cli --features diagrams
```

The feature is not part of the default build: the diagram renderer and what draws its output add 5.8 MB to
the binary. Without it a `mermaid` block is shown as its source, like any other code block.

With the feature:

- A block that is still arriving, and any block in a terminal without images, is shown as source.
- A diagram that does not parse keeps its source, with one line saying why.
- `/open diagram` writes the last diagram of the conversation to the project's artifacts folder as SVG and
  opens it, which works in every terminal.
- Diagram text is drawn with one font that ships with azazel (Liberation Sans), so a diagram looks the same on
  every machine; characters that font lacks (CJK, emoji) are left out.
- The first diagram of a run takes a moment longer than the next: the layout measures its text once with a
  font of the system.

## Settings menu

`/settings` opens with signing in, API keys and models, followed by one section per group of everyday settings.
Each change is saved at once in `~/.azazel/config.toml` (comments and everything else in the file are kept).
Typing nothing for a text setting puts it back on its default. A project's own configuration still wins over
these values inside that project.

| Section | Settings |
|---|---|
| Appearance | theme\*, symbols\*, working indicator\*, session colour for the indicator\*, tool call style\*, hiding secrets\*, popups\*, inline images, show thinking\*, full tool output\* |
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

## Symbols

Every icon, frame and tree line the UI draws comes from one of three sets. `tui.glyphs` (or `/settings`,
Appearance, which changes the running UI at once) chooses the set; `AZAZEL_GLYPHS` overrides it for one run.

| `tui.glyphs` | Draws with |
|---|---|
| `unicode` | Symbols every modern terminal font has: `❯ ✔ ✘ ⚠ ⑂ ◫ ↯ ⬢`, rounded frames, braille spinner |
| `nerd` | The same frames and spinner, with icons from a [Nerd Font](https://www.nerdfonts.com) in place of the symbols, and an icon before every file and directory in [tool cards](#tool-cards). The terminal must use such a font; azazel cannot tell whether it does |
| `ascii` | Plain ASCII only: `>` for the prompt, `-` rules, tables and tool cards drawn with `+`, `-` and a vertical bar, `[+] [!] [-]` for the state of a tool call, `[x] [~] [ ]` todo marks, `* - +` bullets, a spinner that turns through `-`, `\`, a vertical bar and `/`, a vertical bar between status segments, `...` where text is cut, words for icons (`git: main`, `ctx 42%`), and the opening banner in thin letters |
| `auto` (default) | `unicode`, or `ascii` when `LC_ALL`, `LC_CTYPE` or `LANG` names an encoding that is not UTF-8 |

An icon that would only repeat the word beside it is left out at the `ascii` level (`2 agents`, not
`agents: 2 agents`).

## Themes

Six themes are built in: `dark` (default), `light`, `catppuccin-mocha`, `tokyo-night`, `gruvbox-dark` and
`dark-daltonized` (success and error in blue and orange instead of green and red). Set one for good in
[configuration](configuration.md):

```toml
[tui]
theme = "light"
```

`/theme` opens a picker, in which each theme shows its accent, success, warning and error colours, and
`/theme <name>` switches for the running session.

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
| `border` | The rules around the input, the frame of a popup, quote bars, table lines |
| `border_muted` | The frame of a finished tool card |
| `code_bg` | Background of code blocks |
| `user_bg`, `user_text` | The band behind your own messages and the text on it |
| `selected_bg` | Background of the selected row in pickers and completion lists |
| `surface` | Background inside a [popup](#popups) |
| `backdrop` | Text of the transcript behind a popup |
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
