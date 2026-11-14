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
| Widgets | an extension shows some | The lines [extensions](extensions.md#in-the-terminal-ui) set with `azazel.widget`: one row a line, at most 10 rows |
| Input | always | The editor between two rules, with its completion list, notices, queued messages, stashed drafts and an attached-image count above it |
| Status line | always | Session facts on the left, the model on the right, or the [segments you choose](#status-line) |

Your own messages appear in the transcript on a band that spans the terminal's width.

### Input

The input sits between two full-width rules, with `❯` in front of the first row. An empty input shows a hint
(`Ask anything, / for commands, @ for files`). The input grows with what you type up to 30 % of the terminal's
height (at least 5 rows) and scrolls beyond that; the rules then say how much is out of view (`↑ 3 more`).

While the agent is working, the rule above the input carries the [working indicator](#working-indicator): a
spinner, `working` / `thinking` / `running <tool>`, the elapsed time and `esc to interrupt`. A run that
waits for a usage window to reset says what it waits for and how long is left (`waiting for the 5-hour
limit of anthropic · 2 h 10 min left`); see
[when every account is out](models-and-logins.md#when-every-account-is-out). With
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

### Window size

When the window changes size, or the font is zoomed (which changes how many rows and columns fit), the input
and the status line stay on the last rows and everything azazel is still drawing is laid out again for the new
width.

- A drag reports dozens of sizes. azazel waits until the size has been still for 60 ms and then draws once, at
  the size the window has by then. Nothing is drawn in between. It also asks for the size before every
  frame, so that the spinner does not draw at the old size between the change and the terminal's report of
  it.
- Laid out again at the new width: the reply that is being written, tool cards that are still running (and in
  the [compact style](#compact-style) the group that is still growing), the agents at work, the todo list, the
  input and the status line. A [popup](#popups) is centred again, and the
  [transcript viewer](#transcript-viewer) wraps its text again.
- Left as they are: transcript lines that were already written. They belong to the terminal's scrollback; the
  terminal re-wraps or cuts them when the window gets narrower, and azazel does not write them again. Lines
  said while a popup is open are written when it closes, cut to the width the window has then.
- A window that gets taller: the live region takes the rows the window gained, so the input is on the last row
  again. In a terminal that fills new rows from its scrollback instead of adding them below, blank rows stand
  between the transcript and the input until the transcript grows into them.
- A window that gets shorter: the terminal keeps its last rows, where the input is, and moves the rows at the
  top into its scrollback. Nothing is written again, so after a large change the rows above the input can be
  blank until the transcript grows into them; what was said is in the scrollback.
- A window that gets narrower: most terminals re-wrap what is on screen, which puts the rules and the status
  line on two rows each. azazel works out how many rows the old live region takes now and erases all of it.
  Where it cannot be sure it counts too few rows rather than too many: a row of the old input box can then
  stay behind in the transcript, and no transcript line is erased. When the re-wrapped rows no longer fit on
  the screen, the terminal moves the first rows of the live region (what was being written) into its
  scrollback, where azazel cannot reach them; the screen itself is drawn anew. xterm, rxvt, st, GNU screen
  and the Linux console cut rows instead, and are treated that way.
- A window that changes width while a popup or the viewer is open: the terminal re-wraps the transcript
  when the popup closes, and some terminals (Windows Terminal) then put the cursor back where it was, among
  the rows the re-wrap added. azazel works those rows out from what it wrote and starts the live region
  below them, so nothing of the transcript is written over; at worst a blank row is left above the input.
- On Windows azazel asks the console to report changes of the window's size (Windows Terminal reports none
  to a program that does not ask), and reads where the console has its cursor after the size settled and
  after a popup or the viewer closes, so the live region gets exactly the rows down to the bottom of the
  window whatever the terminal did with the transcript.
- A popup that is open when the window gets smaller than 44 columns by 12 rows becomes a box in place of the
  input, and a popup again when the window is large enough.

### Popups

Questions from agents, approval prompts, plan reviews, pickers and the `/settings` screen open as a box in
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
- A question from the agent (`ask_user`) shows what the agent says you need to know, the code or the diff it
  is about (code numbered, a diff coloured), then the question, then the choices: a list to pick from, check
  boxes with a `Done` row where several may be chosen (`Enter` or `Space` ticks), `Yes` and `No`, or a line to
  type on. The option the agent would take is marked `recommended` and is where the selection starts. A list
  that ends with `Other…` lets you type your own answer under it; `Esc` there returns to the options. `Esc`
  declines; the agent is told and goes on without an answer.
- Several questions asked together are one box, a **form**, with a row of tabs on top: one per question, by
  number and name, and `Send`. `Tab` and `Shift+Tab` turn the tabs, and so do `Left` and `Right` where nothing
  is being typed. Answering a question puts a tick on its tab and brings the next open one into view; when
  none is open the `Send` tab shows every answer, and `Enter` there sends them. You can go to `Send` at any
  time: questions left open are sent without an answer, and the tab says how many are. `Esc` declines the
  whole form.
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

### Terminals

When an agent starts a program in a terminal (`shell` with `interactive`; see
[programs in a terminal](tools.md#programs-in-a-terminal)), its screen opens in a popup and follows the program
live, colours included:

```
  ╭─ pty-1 · the agent is typing · npm init ─────────────────────────╮
  │ This utility will walk you through creating a package.json file. │
  │                                                                  │
  │ package name: (demo)                                             │
  │                                                                  │
  ╰─ enter type into it · esc hide · ctrl+x stop it ─────────────────╯
```

- The box only shows the program at first: the agent drives it. `Enter` (or `Ctrl+G`) gives you the keyboard.
  The border takes the accent colour, the title says `you are typing`, the cursor shows where the program has
  it, and every key and paste goes to the program, `Esc` and `Ctrl+C` included. `Ctrl+G` hands the keyboard
  back to the agent.
- While you have the keyboard, what the agent wants to type waits. The agent is told that you typed with the
  next screen it gets.
- `Esc` puts the box away; the program goes on, and a row above the input says which program runs
  (`pty-1 runs npm init · f3 shows it`). `F3` shows the newest terminal again, `/terminals` lists them all.
  `Ctrl+X` stops the program.
- The program is given a screen the size of the inside of the box, and a new size when the window changes.
- When the program ends, the transcript says how (`pty-1 exited with code 0 · npm init`). A box you were only
  watching closes with it. One you were typing into stays with its last screen until `Esc`.
- An approval or a question from an agent opens over the terminal and gets the keys until it is answered.
- The box follows the same rules as every popup: in a window too small for one, or with `popups = false`, it
  takes the place of the input.

Terminals show for a session that runs in this process. An interface attached to a session elsewhere does not
show them.

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

The status line is made of named segments, some against the left edge and some against the right. Which ones,
in which order and with what between them is yours to choose: [`/statusline`](#setting-it-up) sets it up, and
`[statusline]` in the [configuration](configuration.md#features) holds it. A segment with nothing to say (no
branch outside a repository, no cost on a subscription) is left out.

| Segment | Shown when | Content |
|---|---|---|
| `mode` | always | The permission mode: `manual`, `⏸ plan`, `⏵ accept-edits`, `⏵⏵ auto`, `⚠ bypass`, in the colour of the input rules (`bypass` in the colour of errors). `coordinator` follows it while [coordinator mode](team.md#coordinator-mode) is on |
| `agent` | always | The [primary agent](team.md#primary-agents) the lead is (`build`, `plan`, …), in the colour its definition gives it, else the accent colour. The rules around the input take that colour too |
| `branch` | the project is a git repository | `⑂ main`: the checked-out branch |
| `git_dirty` | files differ from the last commit | `± 3`: how many files are changed or untracked. Counted again after every turn and every `!command`, and otherwise at most every five seconds |
| `path` | always | The working directory, with `~` for your home directory. A long one keeps its end (`…/crates/azazel-tui`) and at most a third of the line |
| `context` | the model's context window is known | `◫ 42%`: how full the context window is; warning colour above 70 %, error colour above 90 % |
| `bar` | the context window is known and the terminal is at least 100 columns wide | `[━━━╸────]`: the context window as a bar. Right behind `context` it stands with it as one piece |
| `rate` | the lead has streamed a reply since your last prompt | `↯ 78 tok/s`: how fast the lead writes, in output tokens per second; see below |
| `cost` | the lead answers with an API key and the model's prices are known | Money spent in this session: `$0.42`, `$12.4`. In the warning colour from `limits.warn_cost_usd` on |
| `usage` | the lead answers on a subscription you signed in to | `◔ 61% 5h`: the fullest usage window of the account in use that counts the lead's model (`7d-fable` for a window of one model family) and how full it is, in the warning colour from 80 % on; `/usage` shows them all |
| `cache` | the provider has counted prompt tokens | `≋ 87%`: the share of this session's prompt tokens that were read from the provider's cache |
| `agents` | sub-agents are active | `⇶ 2 agents`: how many are queued, working or waiting for approval |
| `goal` | a goal is open | `⚑ goal …`: the goal's state and the attempt number; see [goals](goals.md) |
| `todo` | the lead has a todo list | `☑ 2/5`: items done and items in all; in the colour of success once all are done |
| `queue` | messages wait for the agent to finish | `⏵ 1 queued`; see [queued messages](#queued-messages-and-stashed-drafts) |
| `session` | the session has a name | `❖ fix the build`, cut to 24 columns |
| `account` | the lead's provider is used with an account of `auth.json` | `♙ work`: the label of the sign-in or stored key that requests go out with. It follows the account when work [moves to another one](models-and-logins.md#several-accounts) |
| `extensions` | a [Luau extension](extensions.md#in-the-terminal-ui) has set a text with `azazel.status` | The texts of all scripts in the order they were first set, each a piece of its own with the separator of the line between them; one line of each, cut to 32 columns |
| `model` | always | `⬢ provider/model`: the model answering. With a [virtual model](models-and-logins.md#virtual-models) it is `virtual/auto -> provider/model`: what you chose and the model the last request went to |
| `tier` | a [service tier](models-and-logins.md#service-tiers) is on | `fast` or `slow` |
| `thinking` | always | The thinking level |
| `time` | always | `◷ 14:05`: the time of day |

The symbols follow [`tui.glyphs`](#symbols): at the `ascii` level the line is plain ASCII
(`git: main | ctx 42% [===>----] | 78 tok/s`).

**Presets.** A preset is a list of segments for each side:

| Preset | Left | Right |
|---|---|---|
| `default` | `mode`, `agent`, `branch`, `path`, `context`, `bar`, `rate`, `cost`, `usage`, `agents`, `goal`, `extensions` | `model`, `tier`, `thinking` |
| `minimal` | `mode`, `path` | `model` |
| `powerline` | `mode`, `branch`, `git_dirty`, `path`, `context`, `agents`, `extensions` | `cost`, `usage`, `thinking`, `model` |
| `full` | `mode`, `agent`, `branch`, `git_dirty`, `path`, `context`, `bar`, `rate`, `cost`, `usage`, `cache`, `agents`, `goal`, `todo`, `queue`, `extensions` | `session`, `account`, `model`, `tier`, `thinking`, `time` |

`statusline.left` and `statusline.right` name the segments of a side yourself; an empty list takes the
preset's, less what the other list took, and `["none"]` is a side with nothing on it. A segment stands once: named
on both sides, it stays where it is named first and a warning says so, as it does for a name that is no segment.

**Separators.** `statusline.separator` is what stands between two segments: `dot` (` · `), `bar` (` │ `),
`space` (two spaces) or `powerline`. With `powerline` every segment is a block of colour: the mode in the colour
of the mode, the model in the accent colour, the others on two quiet backgrounds in turn. With
`tui.glyphs = "nerd"` an arrow (``, ``) ends each block; without a Nerd Font the blocks stand side by side.
The `powerline` preset is drawn in blocks unless you choose `bar` or `space` for it.

**Segment options.** `[statusline.segments.<name>]` sets how one segment is written:

| Option | Segments | Values |
|---|---|---|
| `priority` | all | `0` to `100`: how long the segment stays on a narrow line; see below |
| `icon` | all | `false` leaves out the symbol before the text |
| `max` | `path`, `model`, `agent`, `branch`, `goal`, `session`, `account`, `extensions` | The most columns of its text (for `extensions`, of each text), `4` to `200` |
| `style` | `path` | `short` (the default: `~` and the end of a long path), `full`, `basename` (the directory's own name) |
| `style` | `model` | `full` (the default), `short` (without the provider) |
| `style` | `context` | `percent` (the default), `tokens` (`◫ 84.0k/200.0k`) |
| `style` | `account` | `label` (the default), `email` (the address the account signed in with, hidden as `j***@example.com` unless `usage.redact_email` is off; the label for a stored key) |
| `format`, `seconds` | `time` | `24h` (the default) or `12h`; `seconds = true` adds them |
| `cells`, `min_width` | `bar` | Cells of the bar (`4` to `40`, default `8`); the narrowest terminal it is drawn on (default `100`, `0` for any) |

```toml
[statusline]
preset = "minimal"
right = ["context", "model", "time"]
separator = "bar"

[statusline.segments.path]
style = "basename"

[statusline.segments.model]
style = "short"
```

An option a segment does not take, or a value it cannot be, is named in a warning when azazel starts and left
at its default.

**A narrow terminal.** When the line does not fit, whole segments leave it, the one with the lowest priority
first: `bar` (5), `time` (15), `account` (20), `session` (25), `cache` (30), `extensions` (32), `todo` (35),
`queue` (38), `tier` (40), `goal` (45), `agents` (50), `cost` and `usage` (55), `rate` (60), `thinking` (70),
`context` (75), `path` (80), `git_dirty` (84), `branch` (85), `agent` (90), `model` (95), `mode` (100). The segment with the
highest priority of each side stays; only then is the right side shortened, and after that left out. The texts
of `extensions` leave one by one, the last first.

#### Setting it up

`/statusline` opens a popup with the line it would make on its first row, for a session that has something to
show in every segment. The status line under the popup follows what you choose as well. The rows are the
preset, the separator, the segments of the left side and of the right side in their order, then the segments
that are off (the sample shows some of them).

```text
╭─ Status line ────────────────────────────────────────────────────────────────────────────────────╮
│ ⏵ accept-edits · ⑂ main · ~/src/app · ◫ 42% · ↯ 78 tok/s      ⬢ anthropic/claude-opus-5-5 · high │
│                                                                                                  │
│ ▌ Preset      default                                                                            │
│   Separator   dot                                                                                │
│   ▣ mode        left   the permission mode                                                       │
│   ▣ branch      left   the git branch                                                            │
│   ▣ path        left   the working directory                                                     │
│   ▣ extensions  left   what extensions show with azazel.status                                   │
│   ▣ model       right  the model that answers                                                    │
│   ▣ thinking    right  the thinking level                                                        │
│   □ time               the time of day                                                           │
│   □ git_dirty          files changed since the last commit                                       │
╰─ ↑↓ move · space on or off · left right side or value · [ ] reorder · enter save · esc cancel ───╯
```

| Key | On | Effect |
|---|---|---|
| `Left`, `Right`, `Space` | `Preset`, `Separator` | The next or previous value. Another preset puts its own segments on the line |
| `Space` | a segment | Puts it on the line (at the end of the left side) or takes it off |
| `Left`, `Right` | a segment | Moves it to the end of the left or the right side |
| `[`, `]`, `Shift+Up`, `Shift+Down` | a segment on the line | Moves it one place towards the start or the end of the line; past the end of the left side is the start of the right side |
| `Enter` | | Saves `preset`, `separator`, `left` and `right` under `[statusline]` in `~/.azazel/config.toml`. A side that is the preset's is not written, so it follows the preset |
| `Esc` | | Puts the line back as it was |

`/statusline minimal` (or `default`, `powerline`, `full`) takes a preset at once and saves it. The Status line
page of [`/settings`](#settings) edits the same keys, the segment options and the command included.

#### Your own command

`statusline.command` names a program whose output stands in place of the left side. It is run in your shell,
in the working directory, with the session as JSON on its standard input, and the first line it prints on its
standard output is shown. Colours (SGR sequences) are kept; every other escape sequence and control character
is removed. The right side stays, and gives way to a long line down to its last segment.

```toml
[statusline]
command = "jq -r '.model.display_name + \" in \" + .workspace.current_dir'"
```

```json
{
  "hook_event_name": "Status",
  "session_id": "01JD…",
  "session": { "id": "01JD…", "name": "fix the build" },
  "cwd": "/home/ann/src/app",
  "workspace": { "current_dir": "/home/ann/src/app", "project_dir": "/home/ann/src/app" },
  "model": { "id": "anthropic/claude-opus-5-5", "display_name": "claude-opus-5-5" },
  "mode": "accept-edits",
  "thinking": "high",
  "branch": "main",
  "context": { "used_tokens": 84000, "window": 200000, "used_percentage": 42 },
  "context_window": { "context_window_size": 200000, "used_percentage": 42, "remaining_percentage": 58 },
  "cost": { "total_cost_usd": 0.42 },
  "version": "0.5.0"
}
```

`session_id`, `cwd`, `workspace`, `model`, `context_window` and `cost.total_cost_usd` are named as Claude Code
names them, so a program written for its status line reads the same fields here. A fact that is not known is
`null`.

The program runs when something it is given has changed, after every turn and every `!command`, and no more
often than once a second. It is ended after two seconds. When it fails, takes too long or prints nothing, the
segments are shown instead, and a failure is said once. The key is honoured in the home config and in a
[trusted](configuration.md#trust) project only.

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
none; `git_dirty`, `account` and `extensions` are not shown for it.

## Keys

| Key | Action name | Effect |
|---|---|---|
| `Enter` | `submit` | Send the prompt. While the agent is working: steer it (the message is delivered before its next step) |
| `Alt+Enter` | `follow_up` | While the agent is working: queue the message until it finishes; see [queued messages](#queued-messages-and-stashed-drafts) |
| `Shift+Enter`, `Ctrl+J`, `Ctrl+Enter` | `newline` | Insert a line break. Where the terminal sends `Shift+Enter` and `Ctrl+Enter` as plain `Enter`, `Ctrl+J` does it; see [Shift+Enter](#shiftenter-and-terminal-setup) |
| `Esc` | `abort` | Interrupt the lead agent. Twice within a second on an empty, idle input: open `/rewind` |
| `Ctrl+C` | `clear_or_exit` | Clear the input; on an empty input, twice within a second exits: the terminal is yours again at once, and nothing at the end of the session waits for a model (memories are extracted by the next session; see [memory](memory.md#automatic-extraction)). Another `Ctrl+C` while the session is still closing ends azazel there and then |
| `Ctrl+D` | `exit` | Exit when the input is empty |
| `Shift+Tab` | `cycle_mode` | Next permission mode: `plan`, `manual`, `accept-edits`, `auto`. `bypass` is not a step: it is entered with `/mode bypass` and confirmed |
| `Alt+A` | `agent_hub` | Open the agent hub |
| `Ctrl+O` | `toggle_tool_output` | Expand or collapse tool output; in the [compact style](#compact-style), also write the last group of calls as full cards |
| `Ctrl+T` | `toggle_thinking` | Show or hide thinking |
| `Ctrl+L`, `Alt+P` | `model_picker` | Open the [model picker](#models-and-roles-at-a-key): roles, favourites, recent models, then every model |
| `Ctrl+R` | `history_search` | Search what you sent in this project, in any session, and put one prompt back in the input |
| `Ctrl+V`, `Alt+V` | `paste_image` | Take what is on the clipboard: an image or image files are attached, other files mentioned, text pasted; see [pastes, images and dropped files](#pastes-images-and-dropped-files). In Windows Terminal `Ctrl+V` pastes text itself, and attaches an image when the clipboard holds one and no text. VS Code on Windows pastes with `Ctrl+V` itself, so there the key for an image is `Alt+V` |
| `Ctrl+Shift+V`, `Alt+Shift+V` | `paste_text` | Paste the clipboard's text as text, even when it is the path of an image |
| `Tab` | `complete` | Open or accept completion. In an empty input: switch the lead to the next [primary agent](team.md#primary-agents) (`build`, `plan`, …) |
| `Ctrl+G` | `external_editor` | Edit the draft in your editor; see [external editor](#external-editor) |
| `Alt+Up`, `Shift+Up` | `dequeue` | Take the last queued message back into the input |
| `Ctrl+S` | `stash` | Park the draft; on an empty input, bring the last parked draft back |
| `Alt+T` | `cycle_thinking` | Next thinking level the current model supports |
| `Ctrl+P`, `Alt+M` | `cycle_model` | Put the lead on the next favourite model or role; see [models and roles at a key](#models-and-roles-at-a-key) |
| `Ctrl+Shift+P`, `Alt+Shift+P`, `Alt+Shift+M` | `cycle_model_back` | The previous one |
| `F2` | `transcript` | Open the [transcript viewer](#transcript-viewer) |
| `F5` | `retry` | Same as `/retry` |
| `Ctrl+B` | `background` | While a `shell` command of the agent runs: move it to the background |
| `F3` | `terminal` | Show the newest [terminal](#terminals) of a program an agent runs |
| `Ctrl+Space` | `voice` | Start recording; again: stop and put the transcript in the input. `Esc` cancels; see [voice](voice.md) |

`Esc` interrupts the lead only; stop sub-agents from the roster, the agent hub or with `/stop`. In the agent
hub, `Enter` follows the selected agent's work in the transcript, `o` opens it in a popup, `m` messages it,
`s` stops it and `S` stops all.

Editing keys in the input: arrows, `Home`/`End`, `Ctrl+A`/`Ctrl+E` (line start/end), `Ctrl+Left`/`Ctrl+Right` or
`Alt+B`/`Alt+F` (word), `Ctrl+W` and `Ctrl+Backspace` (delete word back), `Alt+D` and `Ctrl+Delete` (delete word
forward), `Ctrl+U` (delete to line start), `Ctrl+K` (delete to line end), `Ctrl+Z` (undo), `Ctrl+Shift+Z` (redo).
`Up` on the first line and `Down` on the last walk through earlier prompts, those of earlier sessions of the
project included; see [prompt history](sessions-and-checkpoints.md#prompt-history). `Down` in an empty input
goes to the roster while sub-agents are at work; see [the fleet view](#the-fleet-view).

What `Ctrl+K`, `Ctrl+U`, `Ctrl+W` and `Alt+D` cut goes onto a kill ring of 20 entries. `Ctrl+Y` pastes the newest
one; `Alt+Y` right after that swaps in the one before, and so on around the ring.

A [Luau extension](extensions.md#in-the-terminal-ui) can bind keys of its own with `azazel.shortcut`. A key of
the table above keeps its action, and a key that types a character still types it; any other key runs the
script's handler, the editing keys of the input included. `/keys` lists these keys with the script of each.

`Alt+T` steps through the thinking levels the current model supports, `off` included.

### Models and roles at a key

`Ctrl+P` puts the lead on the next entry of a cycle, `Ctrl+Shift+P` or `Alt+Shift+P` on the previous one. For a
moment (two seconds, or until the next key) the cycle stands above the input with the entry the lead is on
now in brackets:

```text
@default [@smol] @slow  ctrl+p next · ctrl+l all
```

- The cycle is `tui.favourites`: models (`anthropic/claude-x`) and roles (`@smol`), in the order of the list.
  `Ctrl+F` in the model picker adds the selected row to the favourites or takes it out, and the list is saved
  in `~/.azazel/config.toml`; the Models page of `/settings` edits it too.
- With no favourites the cycle is the roles that were given models: `@default`, `@smol` and `@slow` first, the
  others by name. A role that answers with the same models as one before it is left out.
- `--models a,b,c` names the cycle for one run, in the words `--model` takes (part of a model's name is
  enough). `Ctrl+F` still edits the saved list.
- An entry that resolves to no model you can use is skipped.
- A step onto a role makes that role's chain the lead's: the lead answers with the role's models and its
  fallbacks, and follows the role when you change it. The choice is recorded in the session like one made with
  `/model`, so `/resume` comes back on it. Sub-agents keep their own models.
- The lead is on the entry it was put on, or else on the first entry that answers with the model the lead
  answers with: a lead on `@default` counts as being on the favourite that names the model `@default` answers
  with, and the first step goes to the entry after it.
- A terminal that does not report keys the enhanced way sends `Ctrl+Shift+P` as `Ctrl+P`, and Windows Terminal
  and VS Code keep it for their command palette: `Alt+Shift+P` always arrives. VS Code also keeps `Ctrl+P`;
  `Alt+M` and `Alt+Shift+M` do the same there. `/keys` says which keys your terminal keeps.

`Alt+P` or `Ctrl+L` open the model picker, the quick way to put this session on another model, with the row of
what the lead is on selected (`/model` alone opens [`/models`](#models) instead). Typing
filters it. The rows come in this order: the roles, each with the model it answers with first; your favourites
(marked `★`), in the order of the list; the models chosen lately (marked `recent`), the latest first; then
[virtual models](models-and-logins.md#virtual-models) and every other model of the providers you have a
credential for. The last eight models you chose, with the picker, `/model <name>` or the cycling keys, are kept
in `~/.azazel/cache/recent-models`.

`Ctrl+B` works while a `shell` command the agent started is running. The command goes on as a background job
(it is not started again), the agent's call returns at once with the job's id, and `/jobs` lists it.

### The fleet view

While sub-agents are at work the roster above the input is a list you can move in. `Down` in an empty input
puts the keyboard on its first row; a line under the rows names the keys:

| Key | Effect |
|---|---|
| `Up`, `Down` | Move between the agents. `Up` from the first row returns to the input |
| `Enter` | Opens the selected agent in a popup |
| `x` | Stops the selected agent and the agents it started |
| `Esc` | Returns to the input. Any other key does too, and is typed there |

The popup shows the agent at work: the task it was given and every message sent to it since, what it says as
it is written, its reasoning when thinking is shown, and its tool calls as the cards the transcript shows,
running ones with their time and last output. The title says how the agent stands (`scout-1 · scout ·
running: grep TODO · 42s · 3.4k tokens`). It follows the newest rows; `PgUp` and `PgDn` scroll, and with
nothing typed so do `Up`, `Down`, `Home` and `End`. What the popup shows is kept from the moment the UI
opened on the session, the newest 400 items per agent; the agent's transcript file has everything.

The line at the bottom is for the agent: what you type and send with `Enter` reaches it as a steer message
before its next request, or starts it again when it has finished (`Shift+Enter` adds a line, the
`external_editor` key opens your editor). `Ctrl+X` stops the agent, `Esc` closes the popup; the agent goes on
either way. `@scout-1` alone at the prompt, and `o` in the agent hub, open the same popup for any agent of
the session, finished ones included.

### Mentions

A prompt that starts with `@` and the id of a sub-agent, or the name of an [agent definition](team.md), is
for that agent and not for the lead:

```text
❯ @scout-1 also check the Windows path
❯ @scout where is the retry policy configured?
```

The first sends the text to `scout-1` as `/msg scout-1 ...` does. The second starts a new `scout` with the
text as its task, as `/task scout ...` does: it appears in the roster and its report goes to the lead when it
is done. An id is looked for before a definition. Typing `@` at the start of a prompt offers the agents and
the definitions before the files; a name that is neither is an ordinary [file mention](#input-prefixes)
and the prompt goes to the lead.

### Queued messages and stashed drafts

`Alt+Enter` while the agent works does not hand the message to the agent. The UI keeps it, shows it as a muted
`⏵ queued:` line above the input, and sends it when the agent finishes, exactly like a prompt you type then:
`@` files are attached, a checkpoint is taken and prompt hooks run. Several queued messages go one per run, in
order. With `[tui] queue_mode = "all"` they go together instead: when the agent finishes, everything that is
queued is sent as one prompt, the messages in order with a blank line between them, and the images of the later
ones numbered on from those of the earlier ones. `Alt+Up` (or `Shift+Up`) takes the last one back into the input
with its images. Interrupting with `Esc` keeps the queue; the next message goes after the next run that
finishes on its own.

`Ctrl+S` parks a draft (with its attached images) and clears the input; a line above the input counts what is
parked. `Ctrl+S` on an empty input brings the last parked draft back.

### Pastes, images and dropped files

`Ctrl+V` (or `Alt+V`) reads the clipboard:

| The clipboard holds | What happens |
|---|---|
| An image (a screenshot, a copied picture) | It is attached to the draft |
| Files (copied in Explorer, Finder or a file manager) | Images among them are attached; the others become `@path` mentions |
| Text that is nothing but paths of existing image files | Those images are attached |
| Other text | It is pasted, as the terminal's own paste would |
| Nothing of these | `the clipboard holds no image, file or text` |

An attached image shows in the draft as a chip, `[Image #1]`, where the cursor was. The chip is one character
to the keys: the cursor steps over it, and `Backspace` or `Delete` removes it and with it the image. The
numbers always run from 1 without gaps in the order the chips stand in the draft, so deleting `[Image #1]`
turns `[Image #2]` into `[Image #1]`, and `Ctrl+Z` brings a deleted image back. Text can go before, between and
after the chips ("compare [Image #1] with [Image #2]"). When the prompt is sent, the agent gets the text with
the labels in it and the images in the same order; the transcript shows the labels. A chip that is in the draft
when you run a slash command goes along if the command turns into a prompt, and is dropped otherwise.

Images are sent as PNG, JPEG, WebP or GIF. Anything else that can be decoded (BMP, which is what Windows hands
to WSL) is converted to PNG, and an image whose longer side is over 1568 pixels is scaled down to that.

The clipboard is read through the system's own interface. Where that finds nothing it understands, or there
is none (WSL has no way to an image on the Windows clipboard), azazel runs the platform's program instead:

| System | Program | Notes |
|---|---|---|
| Windows, WSL | `powershell.exe` | Reads the image as PNG, else the copied files, else the text; allowed 8 seconds. Under WSL, `C:\...` paths are read as `/mnt/c/...` |
| macOS | `osascript`, `pbpaste` | The image as PNG, else the files copied in Finder, else the text |
| Linux | `wl-paste` under Wayland, else `xclip` | The image in the best type offered, else the copied files (`text/uri-list`), else the text |

These programs run only when you press the key, with fixed arguments: nothing from the clipboard becomes part
of a command. When the program is missing the answer is that the clipboard holds nothing.

Most terminals keep `Ctrl+V` (or `Cmd+V`) for their own paste. With an image and no text on the clipboard
they paste nothing: an empty paste, or in Windows Terminal only the release of the key. azazel takes either
for `Ctrl+V` and reads the clipboard, so a screenshot is attached however the terminal pastes.

`Ctrl+Shift+V` (or `Alt+Shift+V`) pastes the clipboard's text and nothing else: a path stays a path. Most
terminals keep `Ctrl+Shift+V` for their own paste, which arrives as pasted text and is treated as described
next.

A paste of more than 10 lines (`editor.fold_paste_lines`) or 800 characters goes into the input as one token,
`[paste #2 +41 lines]`. The cursor steps over the token as over one character, `Backspace` after it removes it
whole, and the agent receives the full text. The transcript shows the token.

Windows consoles do not mark pasted text, so there azazel treats keys that arrive faster than anyone types as a
paste: line breaks inside it do not send the prompt. One short line that ends with a line break is taken as
typed and sent, as in a shell.

Files dropped on the terminal arrive as pasted paths, and so does a path you paste yourself. When a paste is
nothing but absolute paths of existing files, images are attached as chips and other files are inserted as
`@path` mentions; a path with spaces is written `@"my notes/a file.md"`. The paths may be one on a line or
separated by spaces, bare, in single or double quotes, backslash-escaped (outside Windows), `~/...`,
`file://` addresses, or Windows paths (`C:\Users\me\shot.png`; under WSL they are read as `/mnt/c/...`).
At most 32 files are taken from one paste and an image file may be 20 MB; a paste with anything else in it is
text.

### External editor

`Ctrl+G` writes the draft (folded pastes written out, attached images as their labels) to a file, opens it in
your editor and puts what you saved back into the input. An image whose label is still in the text stays
attached; deleting the label takes the image off. The editor is `[editor] command`, else `$VISUAL`, else `$EDITOR`, else `notepad` on
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
- `/keys` lists the bindings in effect, then the keys extensions bound with `azazel.shortcut` (and which of
  them never run, because the key is bound to an action here or types a character), then the key conflicts
  it finds.

`/keys` reports three kinds of conflict, each with the key, the action and, where the action has another key
that is fine, that key:

| Conflict | Example line |
|---|---|
| A key the file gives to two actions. The action that comes first in the table above gets it | `ctrl+o is bound to toggle_thinking and stash: toggle_thinking gets it` |
| A key the terminal azazel runs in keeps for itself in its default setup. Known for Windows Terminal, VS Code, kitty, WezTerm, Alacritty, Ghostty, iTerm2, Terminal and, inside tmux, its prefix key | `alt+enter (follow_up) does not reach azazel: Windows Terminal toggles full screen` |
| A key the terminal cannot tell from another because it does not report keys the enhanced way: `Shift+Enter` and `Ctrl+Enter` (plain `Enter`), `Ctrl+Shift+letter` (`Ctrl+letter`), `Ctrl+I` (`Tab`), `Ctrl+M` (`Enter`), `Ctrl+[` (`Esc`), `Ctrl+Tab` (`Tab`). Not reported on Windows, where the console tells every key apart | `shift+enter (newline) arrives as plain enter in this terminal; /terminal-setup can change that (ctrl+j works)` |

The terminal is recognised from its environment variables (`TERM_PROGRAM`, `WT_SESSION`, `KITTY_WINDOW_ID`,
`WEZTERM_EXECUTABLE`, `ALACRITTY_WINDOW_ID`, `GHOSTTY_RESOURCES_DIR`, `TERM`); what it keeps for itself is its
default key bindings, so keys you have changed in the terminal are not known.

### Shift+Enter and `/terminal-setup`

When the terminal supports the kitty keyboard protocol, azazel enables it so chords such as `Shift+Enter` are
reported reliably; Windows Terminal and WezTerm report them to a Windows program without it. Where `Shift+Enter` arrives
as plain `Enter`, `Ctrl+J` adds a line, and `/terminal-setup` makes `Shift+Enter` do it: it adds a key binding
to the terminal that types a line feed for `Shift+Enter`. A line feed is `Ctrl+J`; a Windows console hands it
to azazel as `Ctrl+Enter`. `/terminal-setup` therefore needs `ctrl+j` (on Windows `ctrl+enter`) to be a key of
`newline`, as both are unless you changed them.

| Terminal | What `/terminal-setup` does |
|---|---|
| Any terminal that reports keys the enhanced way; on Windows also Windows Terminal and WezTerm | Says that `Shift+Enter` works; changes nothing |
| VS Code, Cursor, Windsurf, VSCodium | Adds the binding to the editor's `keybindings.json` (`workbench.action.terminal.sendSequence` with `\n`, when the terminal has the focus). When the window is on another machine (SSH, WSL, a container) the binding is shown instead, because the file is there |
| Windows Terminal from WSL | Adds a `sendInput` action for `shift+enter` to `settings.json`, when exactly one is found under `/mnt/c/Users`; else shows it |
| Alacritty | Adds a `[[keyboard.bindings]]` entry to `alacritty.toml` (or to the list, when the bindings are written as one); a new file when there is none, unless Alacritty is still set up with `alacritty.yml` |
| kitty, WezTerm, Ghostty, iTerm2 (when they do not report keys the enhanced way, as inside tmux) | Shows the line or the steps to put in: `kitty.conf`, `wezterm.lua`, Ghostty's config file, iTerm2's key mappings |
| Others | Says that there is no fix and that `Ctrl+J` adds a line |

Before a file is changed you are shown the binding and the file and asked: `Add the key binding` or
`Show it and change nothing`. The file as it was is copied to `<name>.azazel-backup` beside it first. Comments
and everything else in the file stay as they are, a file that already has a `Shift+Enter` binding for the
terminal is left alone, and a file azazel cannot make sense of is not touched: the binding is shown instead.
The terminals named above read their file again by themselves.

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
| `/open` | Open a recent session of any project: in place for one of this project, else by starting again in its directory; see [projects and directories](sessions-and-checkpoints.md#projects-and-directories) |
| `/cd <dir>` | Go on in another directory (`Tab` completes it); a directory of another project moves the session there |
| `/worktree [name \| list \| remove <name>]` | Go on in a new git worktree on a branch of its own, with the uncommitted changes; list or remove the worktrees. See [worktrees](sessions-and-checkpoints.md#worktrees) |
| `/name [text]` | Name this session; without text, have a name suggested |
| `/session` | What this session is: file, id, name, messages, branch points, tokens and cost, model, checkpoints, agents |
| `/search <text>` | Find sessions of this project by what was said in them; choosing a line resumes its session |
| `/tree` | Go back to an earlier point of this session; `Alt+L` labels an entry, `Ctrl+L` and `Ctrl+U` filter. The agent is told in a few lines what happened on the branch you leave |
| `/fork` | Continue from here in a copy of this session |
| `/retry` | Send your last prompt again from where it was sent; the first attempt stays as a branch |
| `/continue` | Go on with the run that was cut short when azazel was last killed in the middle of it |
| `/fix` | Repair a conversation the provider refuses: tool calls left without a result get one, empty and invalid blocks go, and what changed is listed |
| `/handoff [what matters]` | Continue in a new session that starts from a brief of this one |
| `/consult [question]` | Ask a saved session a question without resuming it |
| `/rewind` | Go back to before an earlier prompt: files, conversation or both |
| `/compact [what to keep]` | Summarise the conversation so far |
| `/export [path] [--sanitize]` | Write the conversation to a file: `.md`, `.html` or `.jsonl`; `--sanitize` hides secrets and leaves out what files held, [for sharing](sessions-and-checkpoints.md#sanitized-exports) |
| `/share` | Upload the conversation, secrets hidden, as a secret gist and print its link; asks first |
| `/bug [what went wrong]` | Write a bug report to a zip and offer to open an issue |
| `/transcript` | Read the whole conversation full screen; see [transcript viewer](#transcript-viewer) |
| `/copy [code \| n \| last n]` | Copy the last reply as Markdown; `/copy code` copies one of its code blocks, `/copy 2` the reply before the last, `/copy last 4` the last four messages, yours and the agent's, under `## You` and `## Assistant` headings |
| `/vim` | Switch [Vim keys](#vim-keys) in the input on or off |
| `/open link \| n \| path \| diagram` | `/open link` opens the last link of the last reply in your browser, `/open 2` its second link, `/open <path>` a file, `/open diagram` the last [Mermaid diagram](#mermaid-diagrams) as an SVG file (in a build that has diagrams). `/open` alone opens a session |
| `/clear` | Clear the screen and scrollback (the conversation is kept) |
| `/cost` | Requests, tokens and money spent in this session, with what side questions, commit drafts and the advisor cost; how much of the input was read from the [prompt cache](models-and-logins.md#prompt-cache), per model, and why a request missed it |
| `/btw <question>` | Ask about the session on the side while the agent works; see [side questions](#side-questions) |
| `/bg <prompt>` | Run a prompt in the background, in a copy of this session; see [background sessions](background-and-remote.md) |
| `/loop <count \| interval \| until "<condition>"> <prompt>` | Send a prompt again and again; see [loops](#loops) |
| `/diff [turn \| session \| uncommitted \| <ref>]` | What changed, in the [diff viewer](#diffs-and-commits): the uncommitted changes, the changes since your last prompt or since the session began, or the working tree against a commit, branch or tag (or a range such as `main..topic`) |
| `/commit [what to say]` | Commit with a drafted message you edit first; see [diffs and commits](#diffs-and-commits) |
| `/usage` | A box with what each signed-in subscription has left: per account every usage window with a bar, the share used, its reset time and what it counts, the plan, and the account in use; `r` asks the providers again. See [usage](models-and-logins.md#usage) |
| `/reset-usage [provider]` | Spend a [banked reset](models-and-logins.md#banked-resets) of a Claude or ChatGPT subscription: shows what the account holds and what the reset gives back, and asks before it is spent |
| `/stats [days]` | Sessions, prompts, tokens and cost of the last 30 days (or `days`), over every project: what `azazel stats` prints |
| `/context` | What the lead's last request was made of, tokens per category; `enter` reads a category's exact text. See [the context inspector](#the-context-inspector) |
| `/theme [name]` | Pick or set the colour theme; each row of the picker shows the theme's accent, success, warning and error colours |
| `/glyphs [name]` | Pick or set the symbols the UI draws with: `unicode`, `nerd`, `ascii` or `auto`. Each row of the picker is drawn in its own symbols; the choice is saved as `tui.glyphs` and applies at once |
| `/statusline [preset]` | Set up the [status line](#status-line): pick a preset and a separator, put segments on the line, take them off and order them, with the line they make in view. `/statusline minimal` takes a preset at once |
| `/keys` | List key bindings, the keys [extensions](extensions.md#in-the-terminal-ui) bound, and the [key conflicts](#changing-keys) found: keys given to two actions, keys the terminal keeps for itself, keys it cannot tell apart |
| `/terminal-setup` | Make `Shift+Enter` add a line in this terminal: says whether it already does, else adds the key binding to the terminal's file (after asking, with a backup) or shows it; see [Shift+Enter](#shiftenter-and-terminal-setup) |
| `/debug` | Where azazel keeps its files, the state of the UI and the session, the terminal's variables and the last 200 lines of the log, in the [viewer](#transcript-viewer) |
| `/restart` | Start azazel again in this terminal on the session that is open, with the options it was started with: what picks up a new version after `azazel update` |
| `/quit`, `/exit` | Exit |

`/resume`, `/open`, `/cd`, `/worktree`, `/name`, `/session`, `/search`, `/tree`, `/fork`, `/retry`, `/continue`,
`/fix`, `/handoff`, `/consult`, `/rewind`, `/compact`, `/export`, `/share`, `/bug`, `/debug` and `/restart` are
covered in [sessions and checkpoints](sessions-and-checkpoints.md).

Models, permissions and setup:

| Command | Description |
|---|---|
| `/model [name [fallback …]]` | Set the lead's chain for this session; part of a name or `@role` works too. Alone it opens `/models` |
| `/models` | [Choose the model of every role](#models), with fallbacks, thinking, the `Ctrl+P` cycle and presets |
| `/thinking [level]` | Pick or set the thinking level: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| `/mode [mode]` | Pick or set the permission mode. `bypass` opens a warning box that starts on `No`; see [permissions](permissions.md#bypass) |
| `/agent [name]` | Pick the [primary agent](team.md#primary-agents) the lead is, or switch to one: `/agent plan`. `Tab` in an empty input steps through them |
| `/permissions` | The permission rules in effect and where they come from. `d` removes a rule added in this session or remembered with "always allow"; `a` adds one (`allow shell(cargo test:*)`), for the session or saved. Rules from config files are shown with the file to edit |
| `/forget-approvals` | Forget every remembered approval and every rule added in this session |
| `/trust [yes\|no]` | Whether this project is trusted and what that covers; `yes` or `no` records the decision, a restart applies it |
| `/add-dir <path>` | Let agents work in another directory too, until azazel exits |
| `/dirs` | List the directories agents may work in |
| `/changelog [version]` | What changed in each release, or only since `version`, in the [viewer](#transcript-viewer) |
| `/settings` | Sign in, add API keys, choose models and change every setting; see [settings](#settings) |
| `/login [provider] [account]` | Sign in to a provider or store its API key, as another account when a label is given |
| `/logout <provider> [account]` | Forget the stored key or login of a provider |
| `/account [provider]` | The provider's accounts, their usage and until when one is out of allowance: use one for this session, make it the default, sign it out, add another |
| `/fast [on\|off\|status]` | Faster replies at a higher price (the priority [service tier](models-and-logins.md#service-tiers)) |
| `/slow [on\|off\|status]` | Cheaper, slower replies (the economy tier) |
| `/preset [name \| save <name>]` | List the saved sets of models, apply one, or save the current one |
| `/tools` | The tools offered to the lead, grouped by where they come from (core, each MCP server, scripts, team), with disabled and deferred ones marked, and who [replaced or wraps](extensions.md#replacing-and-wrapping-tools) a tool |
| `/jobs` | Background jobs; choose one to read its output or stop it |
| `/terminals` | Programs agents run in [terminals](#terminals); choose one to watch it, type into it or stop it |
| `/todo [add <text> \| done <n> \| clear]` | Show the agent's task list, or change it |
| `/init` | Have the agent explore the repository and write `AGENTS.md` |
| `/migrate` | Bring over what pi, omp, jcode, Claude Code and Codex CLI hold: tick the harnesses and the kinds (models, keys, skills, commands, agents, MCP servers, rules, settings, chats, memory), read the plan, confirm; see [migrating](migrating.md) |
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

## The context inspector

`/context` takes the lead's last request apart. The request is the one that was sent, kept as it was built:
nothing is rendered again to count it.

```text
╭─ Context ──────────────────────────────────────────────────────────────────╮
│ request 12 of lead · anthropic/claude-opus-5-5                             │
│ 84.2k of 200.0k tokens (42%), as the provider counted them; each category  │
│ is its estimate scaled to that                                             │
│                                                                            │
│ ▌ system prompt: identity     112   0%  ━───────────────────  1 part       │
│   AGENTS and context files   3.1k   4%  ━───────────────────  1 part       │
│   skill index                1.4k   2%  ━───────────────────  1 part       │
│   memory                      620   1%  ━───────────────────  1 part       │
│   tool schemas               9.8k  12%  ━━──────────────────  27 tools     │
│   tool schemas: MCP github  14.6k  17%  ━━━─────────────────  31 tools     │
│   user messages              1.2k   1%  ━───────────────────  6 blocks     │
│   assistant messages         4.9k   6%  ━───────────────────  18 blocks    │
│   tool calls                 2.3k   3%  ━───────────────────  22 calls     │
│   tool results              43.1k  51%  ━━━━━━━━━━──────────  22 results   │
│   images                     3.0k   4%  ━───────────────────  2 images     │
╰─ ↑↓ move · enter read · esc cancel ────────────────────────────────────────╯
```

| Category | Holds |
|---|---|
| `system prompt: <name>` | One part of the system prompt under the name it is built with: `identity`, `rules`, `team`, `environment`, `mcp`, `coordinator`, or a part an extension added. `system prompt: other` is text without a name, added by a hook |
| `date and branch`, `mode and goal`, `task list reminder` | The notes sent after the conversation, which are true for one request only and stay out of the [prompt cache](models-and-logins.md#prompt-cache) |
| `AGENTS and context files` | `AGENTS.md` and the other context files |
| `rules on request` | The list of rule files the agent may read |
| `skill index` | The names and descriptions of the skills |
| `memory` | What memory keeps in front of the agent; it is sent with the notes after the conversation |
| `tool schemas` | The name, description and input schema of every built-in tool; `tool schemas: MCP <server>` is the same for one MCP server |
| `user messages`, `assistant messages`, `thinking`, `tool calls` | The conversation as it is sent, by what each block is |
| `tool results` | The text of tool results, elided ones as their notes |
| `images` | Every image, in a prompt or in a tool result, at the flat 1500 tokens compaction counts for one |

Each bar is the category's share of the request; a category that holds anything shows at least one cell. A
narrow box drops the counts, then the bars. `enter` opens the category's exact text in the full-screen viewer:
a part of the system prompt word for word, each tool with its schema, each message under the number it has in
the request. `q` returns to the box.

The numbers are estimates made with the counter compaction uses (a quarter of the bytes of text). Once the
provider has answered, the total is the input tokens it counted, cached ones included, and every category is
scaled by the same factor so that they add up to it. Until then, and for a provider that counts nothing, the
line above the list says the numbers are estimated.

In a session that was just reopened and has sent nothing yet, `/context` shows the numbers of its last request
from the session's `context.json`; the text of a category is there once a request has been sent. While the agent
works, a box that is open describes the request it was opened on; `enter` after the agent has sent another
opens the box on that one.

`azazel context [PATH|ID] [--json]` prints the same numbers for a saved session, the latest of this project by
default. `--json` is one object:

```json
{"session": "0198…", "agent": "lead", "request": 12, "model": "anthropic/claude-opus-5-5",
 "window": 200000, "estimate": 79310, "reported": 84200, "tokens": 84200,
 "categories": [{"kind": "system", "name": "identity", "label": "system prompt: identity",
                 "items": 1, "estimate": 105, "tokens": 112}]}
```

`kind` is `system`, `tools`, `messages`, `tool_results` or `images`; `estimate` is the counter's number and
`tokens` the scaled one. Over [RPC](rpc.md), `run_command` with the name `context` returns the text table,
with `args: "json"` this object, and with `args: "text <request> <category index>"` the text of one category.

## Loops

`/loop` sends one prompt again and again, each round once the lead has finished the one before:

| Command | What it does |
|---|---|
| `/loop 5 fix one failing test` | Five rounds, then it ends. `5x` works too; at most 1000 |
| `/loop 10m look at the deploy and say what changed` | A round every ten minutes, counted from the start of the round before (`30s`, `10m`, `1h`; five seconds at least). A round that takes longer is followed at once |
| `/loop until "cargo test passes" fix the next failing test` | After every round the `judge` role (by default the `smol` model) is asked whether the condition holds, and the loop ends when it says yes. The condition goes in quotes |
| `/loop` | Where the loop stands |
| `/loop stop` | Ends it; the round in progress goes on |

A loop ends by itself when it has had its rounds or its condition holds. It is stopped by:

- `Esc`, which also stops the round in progress;
- a round that fails: the provider gives up, the run hits a limit, or it is interrupted;
- a check that fails: the judge cannot be asked, or answers neither yes nor no. A loop nothing could
  end is not left running;
- `[schedule] loop_max` rounds (50) for an interval or a condition; `0` takes the limit away.

The judge is a request of its own, outside the session: it is shown the condition, the prompt and the
end of the conversation (what was said, the tool calls and what they printed), and told to go by what
was done, not by what the agent says it will do. Its `YES` or `NO` and its reason are shown; `/cost`
lists what it cost under `loop`. A condition needs a session of this process: it does not work on a
background session you are attached to, while a counted or timed loop does.

What you type while a loop runs works as always: a message steers the round in progress, `Alt+Enter`
queues one, which goes out before the next round. To run a prompt at a time whether or not the UI is
open, use a [schedule](background-and-remote.md#schedules).

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

`/diff` opens what changed full screen, on the terminal's alternate screen like the
[transcript viewer](#transcript-viewer):

| Command | Shows |
|---|---|
| `/diff`, `/diff uncommitted` | The working tree against `HEAD`: staged and unstaged changes, and untracked files as additions |
| `/diff turn` | What changed since the checkpoint taken before your last prompt |
| `/diff session` | What changed since the session's first prompt |
| `/diff <ref>` | The working tree against a commit, branch or tag (`/diff main`, `/diff HEAD~3`), untracked files included; a range (`/diff main..topic`) shows what changed between its two ends |

`/diff turn` and `/diff session` compare [checkpoints](sessions-and-checkpoints.md), so they also work outside a
git repository and include what shell commands changed. The others need a git repository.

```text
Diff · working tree against HEAD                    file 1/3 · 0/3 reviewed · unified
▌ M src/lib.rs  +3 -1 │   M src/lib.rs  +3 -1
  A notes.md      +12 │ ▌ @@ -10,6 +10,8 @@ fn main()
✓ ? draft.txt      +2 │    10 10  fn main() {
                      │    11    -    let x = 1;
                      │       11 +    let x = 10;
j/k ↑↓ move · n/p file · [ ] hunk · m reviewed · s split · w wrap · / search · y copy · q back
```

The files are listed on the left: what happened to each (`M` changed, `A` added, `D` deleted, `R` renamed, `?` not
tracked by git), the lines added and removed, and `✓` once you marked it reviewed. The list gives its room to the
diff in a window narrower than 60 columns. On the right is the selected file's diff, its code coloured as its
language like a file the agent [reads](#tool-cards), in one of two layouts:

- **unified**: one column with the old and the new line number of each line;
- **split**: the old file on the left and the new one on the right, each removed line beside the added line
  that replaced it.

`tui.diff_view` chooses: `auto` (the default) is split in a window of 140 columns or more and unified in a
narrower one; `split` and `unified` always take that layout. `s` switches for as long as the viewer is open.

| Key | Effect |
|---|---|
| arrows, `j` `k` | Move the cursor a row of the diff |
| `PgUp` `PgDn`, `b` `Space` | Move a screen |
| `g` `G`, `Home` `End` | First and last row of the file |
| `n` `p`, `Tab` `Shift+Tab` | Next and previous file |
| `[` `]` | Previous and next hunk |
| `m` | Mark the file reviewed and go on to the next file that is not; on a reviewed file, take the mark away |
| `s` | Split or unified |
| `w` | Wrap long lines instead of cutting them |
| `/` | Search (case does not matter): `Enter` goes to the next match, in this file and then in the files after it; `/` and `Enter` with nothing typed goes on to the next one |
| `y` | Copy the file's diff |
| `q`, `Esc` | Return |

A reviewed mark is kept in the session: `/diff` shows it again later, and after `/resume`, as long as the
file's diff is the same. A file that changed again since you reviewed it is unmarked. The agent keeps working
while the viewer is open; if it needs you, the last row says so and the request is shown when you return.

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
Appearance page of the [settings](#settings).

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

`/open link` opens the last link of the last reply in your browser, `/open <n>` its n-th link, `/open <path>` a
file with the program your system uses for it. In the [viewer](#transcript-viewer), `o` opens the link on the cursor
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
same time, and assumes 10 by 20 when it is not told. A terminal that does not answer within 80 ms gets no Sixel; an
answer that has begun to arrive by then is read to its end, for up to half a second.
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

The feature is not part of the default build: the diagram renderer and what draws its output add 4.9 MB to
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

## Settings

`/settings` opens one screen with every setting azazel has, on ten pages behind a row of tabs:

```
╭─ Settings ───────────────────────────────────────────────────────────────────────────────────────╮
│ … Models  Appearance 2  Status line  Input  Permissions  Tools  Sessions  Usage  Advanced        │
│ Colours, symbols, the working indicator and how azazel gets your atte…  saves to the home config │
│                                                                                                  │
│ Interface                                                                                       ┃│
│ ▌ Theme                            tokyo-night                                          ●  home ┃│
│   Symbols                          auto                                                         ┃│
│   Working indicator                shimmer                                                      ┃│
│   Indicator phrases                (none)                                                       ┃│
│   Session colour in the indicator  off                                                          ┃│
│   Tool calls                       card                                                         ┃│
│   Hide secrets in what is shown    on                                                       env ┃│
│   Menus and questions as popups    off                                                  ●  flag ┃│
│   Inline images                    auto                                                         ┃│
│   Show the model's thinking        off                                                          ┃│
│   Show tool output in full         off                                                          ┃│
│ Notifications                                                                                   ┃│
│   Ring the terminal bell           on                                                           ││
│   Desktop notifications            auto                                                         ││
│   Session in the terminal title    on                                                           ││
│                                                                                                  │
│ The colours: a built-in theme, a file in ~/.azazel/themes, or auto (follows the terminal).       │
│                                                                                                  │
│ tui.theme · default dark · set in the home config                                                │
╰─ tab page · ↑↓ move · enter edit · / search · r reset · p to project · esc close ────────────────╯
```

| Page | What is on it |
|---|---|
| Accounts | Sign in with a subscription, add an API key, sign out or forget a key, the credentials in use, the order accounts are tried in |
| Models | The default model and the model of every role, favourite models\*, the advisor, per-agent overrides, retries and stalls, prompt cache warming |
| Appearance | Theme\*, symbols\*, working indicator\* and its phrases, session colour for the indicator\*, tool call style\*, hiding secrets\*, popups\*, inline images, show thinking\*, full tool output\*; bell, desktop notifications, terminal title, tab progress, shortest run that notifies, keep awake |
| Status line | Preset\*, left and right segments\*, separator\*, segment options\*, command\*; [`/statusline`](#setting-it-up) sets the same line up with the segments in a list |
| Input | Emacs or Vim keys\*, external editor command\*, paste fold length\*, queued messages; transcription and speech models, voice, language, send at once, longest recording, the commands that record, transcribe and play |
| Permissions | The mode sessions start in, what destructive commands need, allow, ask and deny rules, additional directories, secret files; the guards |
| Tools | Disabled tools, result size, shell and its time limit, redaction for the model; formatters and checkers after a write, the findings report, the read guard, the secret scan; language servers; MCP; web search and the browser; media models; verification and goals |
| Sessions | Automatic names, restoring model and thinking on resume, retention, interrupted runs; checkpoints; compaction; memory; nested context files and the rule files of other tools |
| Usage | Subscription usage (spending resets unattended, how often usage is fetched, e-mail addresses) and the cost and turn limits |
| Advanced | Coordinator mode and team size, schedules and loops, updates, trace export, and the tables only the config file edits |

Settings marked \* change the running UI at once. The others are saved and apply to the next session; the last
row under the list says so. The keys behind each row are described in [configuration](configuration.md).

A row shows the name of a setting, the value in effect, `●` when that is not the default, and the layer the
value comes from: `home` (`~/.azazel/config.toml`), `project`, `local` (`config.local.toml`), `env` (an
`AZAZEL_<TABLE>__<KEY>` variable) or `flag` (`--set` and the other flags); a value no layer sets has none. The
number beside a tab is how many settings of that page are not on their default. Under the list stand what the
selected setting does, then its key, its default and who sets it.

| Key | What it does |
|---|---|
| `Tab`, `Right` | The next page. Each page keeps the row it was left on |
| `Shift+Tab`, `Left` | The page before |
| `Up`, `Down`, `PgUp`, `PgDn`, `Home`, `End` | Move over the settings of the page |
| `Enter`, `Space` | Edit the selected setting (below) |
| `/` | Search all pages: what you type is looked for in the names, keys, values and descriptions of every setting, every word of it. The matches stand under the name of their page and the tabs count them. `Enter` edits a match; `Esc` ends the search on the page of the selected match, with it selected |
| `r` | Reset: take the setting out of the file changes go to, which puts it back on its default unless another layer sets it |
| `p` | Save changes to `<project>/.azazel/config.toml` instead of `~/.azazel/config.toml`, and back. Only in a [trusted](configuration.md#trust) project |
| `Esc` | Close. What changed is listed in the transcript |

What `Enter` does depends on the setting:

| Setting | Editor |
|---|---|
| On or off | Flips at once |
| One of several values (theme, symbols, modes) | A list of the values, starting on the one in use. For the settings marked \* each value is shown in the running UI as you move over it; `Enter` keeps it, `Esc` puts back what applied |
| A number or a text | An input holding the value in use. `Enter` saves; a value the configuration refuses stays in the input with the reason. Nothing typed puts a text back on its default |
| A list (favourites, rules, phrases) | Its entries. `Enter` edits an entry, or adds one on the last row; `a` adds, `d` removes, `[` and `]` move the entry up and down. Every change is saved as it is made |
| A table (formatters, checkers, language servers, account order) | Its entries as `name = value`, edited like a list. A value is text, or TOML where the key takes more: `rust = { command = "rust-analyzer", extensions = ["rs"] }` |
| A role on the Models page | [`/models`](#models) on the Roles view with that role selected; `Esc` there comes back to the settings. The row `Models for the lead and each agent` opens it too |

Each change is saved at once (comments and everything else in the file are kept). A value that is the default
is taken out of the file instead of written, unless a file below says something else. Where a layer above the
file sets the same key (the project's config, an `AZAZEL_` variable, a flag), the row keeps showing what applies
and the line that confirms the change says who sets it. The permission rules of all layers are joined, so their
editors hold only the rules of the file changes go to. Making `bypass` the mode every session starts in is asked
about, with `No` first.

Signing in, adding a key and signing out ask their questions in boxes of their own and come back to the screen
where it was. After the first sign-in or key, with no default model yet, [`/models`](#models) opens on that
provider's models to choose the default one. Credentials are kept in `~/.azazel/auth.json`.

`[providers]`, `[models]`, `[presets]`, `[hooks]`, `[[stream_rules]]` and `[embeddings]` are tables of tables
or lists of tables: the Advanced page lists them with what sets them and why they are edited in the file.

In a session that runs in another process (`azazel attach`, SSH) and for RPC clients, `/settings` shows the same
pages as a chain of menus: the pages, then the settings of one with their values, then the value. There a list
is typed with commas between its entries and a table one `name = value` at a time (`name =` removes an entry).
A role's models are typed, the first one then its fallbacks with spaces between them (`openai/gpt-5:high
@default`), and saved in `~/.azazel/config.toml`; nothing typed puts the role back on its built-in value.

## Models

`/models` (or `/model` alone) opens one screen for choosing models: the model of every role, its fallbacks
and its thinking level, the `Ctrl+P` cycle and presets, beside the models of every provider. It is a large
popup, as wide as the window less four columns (at most 150) and as tall as it less four rows.

```
╭─ Models · saving to home ─────────────────────────────────────────────────────────────────────╮
│ Model roles: where each one's model comes from, its fallbacks and its thinking                │
│ ⌕                                                                                       20/20 │
│ ▌Roles          3/20 │ ▌● default  anthropic/claude-opus-5-5            ◉ high  ⟳ 1      home │
│  Recent            4 │  ● smol     openai/gpt-5.6-luna                          ⟳ 2      home │
│  All models      342 │                ↳ anthropic/claude-haiku-4-5                            │
│  ──────────────────  │  ○ slow     auto → anthropic/claude-opus-5-5                   built-in │
│  Anthropic        48 │  ○ plan     auto → anthropic/claude-opus-5-5                   built-in │
│  OpenAI          120 │  + New role…                                                           │
│  ──────────────────  │                                                                        │
│  Groq                │  ctrl+p cycle: @default › @smol                                        │
│                                                                                               │
╰─ enter pick · t thinking · f fallback · x clear · [ ] reorder · c cycle · n new · esc close ──╯
```

The sidebar on the left lists `Roles` (how many have a value of their own, of all), `Recent`, `All models`,
then the providers you have a credential for with their number of models, then, dimmed, those you have none
for. Below 80 columns the sidebar is left out and `Tab` steps through these instead.

The body shows the scope selected in the sidebar:

- **A list of models**: `provider/id`, a `●` on the model the session is on, the roles whose model it is, then
  the context window, the output limit, the price per million tokens in and out (`subscription` through a
  sign-in, `free`), and `◐` for a model that thinks and `◫` for one that takes images. In a narrow box the price
  goes first, then the output limit, then the context window. The models roles use and those chosen lately
  come first, above a line. Under the list stand what the selected model is and costs, its thinking levels,
  and the roles that use it (`used by: default ◉ high · smol`).
- **The Roles view**: each role with a solid dot when it has a value of its own and a hollow one when it
  falls back (`auto → model`), its first model, then on the right its thinking level, its place in the
  `Ctrl+P` cycle and where its value comes from (`home`, `project`, `local`, `session`, `built-in`). Its
  fallbacks stand under it as `↳ model` rows. The last row makes a new role, and the foot of the view shows the
  `Ctrl+P` cycle.
- **A provider without a credential**: which variable to set, `Sign in` for a provider with a sign-in and
  `Add API key` for one that takes a key, then the models its catalogue entry lists. `Enter` runs the sign-in or
  asks for the key, and the screen comes back on that provider with its models.

Typing searches every scope at once: `provider/id`, the name, and `free` for a model that costs nothing. A
model whose id or name is what you typed comes first, then those that contain it, then looser matches. The
counts in the sidebar become the number of matches, and the arrows step over what has none.

The screen opens on the catalogue at once and asks each provider you have a credential for for the models it
serves now, in the background and once per run; a spinner stands beside a provider while it is asked. A
provider you rest on in the sidebar that was not asked yet is asked after a moment, and `F5` asks again. What
the providers answer is kept in `~/.azazel/cache/models.json` for the next run.

| Where | Key | What it does |
|---|---|---|
| Sidebar | `Up`, `Down`, `Home`, `End` | Another scope |
| | `Right`, `Enter` | Into the body |
| Models | `Up`, `Down`, `PgUp`, `PgDn`, `Home`, `End` | Move |
| | `Enter` | The roles the model can take, in a strip at the foot: pick one and the model is that role's first model, or `+ fallback for…` and a role to add it to as a fallback. Then the thinking strip offers `inherit` and the levels the model has; `inherit` writes no level, so the session's applies |
| | `F5` | Ask the provider for its models again |
| Roles view | `Enter` | Choose the role's model: the list of every model opens with its model selected, and `Enter` picks one (on a fallback row it replaces that fallback) |
| | `t` | The thinking level of the role's model, or of the fallback |
| | `f` | Add a fallback |
| | `x`, `Delete` | Clear the role, which falls back to the next config file or its built-in value; on a fallback row, remove the fallback |
| | `[`, `]` | Move the role earlier or later in the `Ctrl+P` cycle; on a fallback row, move the fallback among the others |
| | `c` | Put the role in the `Ctrl+P` cycle (`@role` in `tui.favourites`, saved in `~/.azazel/config.toml`) or take it out |
| | `n` | A new role: type its name, then choose its model |
| | `s`, `l` | Save every role as a preset (type its name), or load one for this session |
| | `w` | Save changes to the project's `.azazel/config.toml` instead of `~/.azazel/config.toml` (the title says which), and back. Only in a [trusted](configuration.md#trust) project |
| Anywhere | `Alt+S`, `Alt+L`, `Alt+W` | What `s`, `l` and `w` do in the Roles view, where letters search |
| | `Tab` | Between the sidebar and the body; below 80 columns, the next scope |
| | `Esc` | Back a step: the strip, then choosing a model for a role, then the search, then the screen closes |

Every change is made at once, through the session (so a session in another process changes the same way), and
the first row says what it came to: `smol → openai/gpt-5.6-luna (home config)`. When the project's config
sets the same role, which wins over the home config, the row says so. Choosing the model of `default` (or of
`lead`) also puts the running session back on it. The mouse works too: the wheel moves in the part under it,
a click selects, a click on the selected row does what `Enter` does, and a click on a chip of the strip picks
it.

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

These themes are built in:

| Theme | Dark | Light | Colours from |
|---|---|---|---|
| `auto` | `dark` (default) | `light` | azazel; the text keeps the terminal's own colour |
| `dark-daltonized` | `dark-daltonized` | | azazel: success and error in blue and orange instead of green and red |
| `catppuccin` | `catppuccin-dark` (Mocha, also `catppuccin-mocha`) | `catppuccin-light` (Latte, also `catppuccin-latte`) | [catppuccin.com](https://catppuccin.com/palette) |
| `tokyo-night` | `tokyo-night-dark` (Night) | `tokyo-night-light` (Day, also `tokyo-night-day`) | [tokyonight.nvim](https://github.com/folke/tokyonight.nvim) |
| `gruvbox` | `gruvbox-dark` | `gruvbox-light` | [gruvbox](https://github.com/morhetz/gruvbox) |
| `dracula` | `dracula-dark` (Dracula) | `dracula-light` (Alucard, also `alucard`) | [draculatheme.com](https://draculatheme.com/spec) |
| `nord` | `nord` | | [nordtheme.com](https://www.nordtheme.com/docs/colors-and-palettes) |
| `rose-pine` | `rose-pine-dark` (main, also `rose-pine-main`) | `rose-pine-light` (Dawn, also `rose-pine-dawn`) | [rosepinetheme.com](https://rosepinetheme.com/palette) |
| `kanagawa` | `kanagawa-dark` (Wave, also `kanagawa-wave`) | `kanagawa-light` (Lotus, also `kanagawa-lotus`) | [kanagawa.nvim](https://github.com/rebelot/kanagawa.nvim) |
| `one` | `one-dark` | `one-light` | Atom's One Dark and One Light |
| `solarized` | `solarized-dark` | `solarized-light` | [Solarized](https://ethanschoonover.com/solarized/) |
| `everforest` | `everforest-dark` | `everforest-light` | [Everforest](https://github.com/sainnhe/everforest) (medium) |
| `github` | `github-dark` | `github-light` | GitHub's [Primer](https://primer.style/foundations/primitives/color) colours |
| `ayu` | `ayu-dark` | `ayu-light` | [ayu-colors](https://github.com/ayu-theme/ayu-colors) |
| `monokai` | `monokai` | | Monokai's original colours |

Set one for good in [configuration](configuration.md):

```toml
[tui]
theme = "catppuccin"
```

A theme with a dark and a light palette, named without `-dark` or `-light`, follows the terminal: the light
palette on a light background, the dark one on a dark background. `auto` is that pair for `dark` and `light`.
`catppuccin-dark` or `catppuccin-light` pins one palette whatever the background; a theme with one palette is
that palette on any background. In every built-in theme, text on the background of a popup keeps a contrast of at
least 4.5:1. When the UI starts
it asks the terminal for its background colour (OSC 11) and waits up to 200 ms for the answer; a terminal
that answers costs none of that time, and an answer that has begun to arrive by then is read to its end. A
terminal that does not answer, and output that is not a terminal, get `dark`. An answer that comes later
still is read as typed text and shows in the input; on a line that slow, name a theme instead. The terminal is asked once, at
the start: after changing the terminal's own colours, `/restart` asks again. `/theme` says which palette a theme
that follows the terminal shows in this one; switched to in a running session that did not start on such a
theme, it is dark until azazel starts again. Commands that print in colour outside the UI (`azazel usage`) do
not ask the terminal and draw such a theme dark.

`/theme` opens a picker (type to filter it), in which each theme shows its accent, success, warning and error colours, and
`/theme <name>` switches for the running session. `--theme <name>` on the command line sets the theme of one
run, and `--theme <file>` takes a theme file from anywhere; a name or file that cannot be used ends the run
with status 2.

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
