# Screen capture, input and playtesting

The `screen` tool lets an agent see and operate the desktop: it lists windows, captures a window or display as an image, and sends mouse and keyboard input. On top of it, the `playtester` agent, the `playtest` skill and the `/playtest` command run a build, play it the way a user would and report defects with the frames that show them. `screen` is part of the default build (cargo feature `screen`); web builds are driven with the `browser` tool instead (see [web and browser](web-and-browser.md)).

## The `screen` tool

One tool with an `action`.

| Action | Parameters | Result |
|---|---|---|
| `windows` | | One line per titled window: id, title, app, position, size, and `minimised` when it is |
| `capture` | `window?`, `display?`, `region?` | An image of the window, else of the display (the primary one by default) |
| `focus` | `window` | Brings the window to the front and gives it the keyboard |
| `click` | `x`, `y`, `button?` (`left`, `right`, `middle`), `double?` | Moves the pointer there and clicks |
| `move` | `x`, `y` | Moves the pointer |
| `drag` | `from` (`[x, y]`), `to` (`[x, y]`), `duration_ms?` (default 300, at most 5000) | Presses the left button at `from`, moves in steps, releases at `to` |
| `scroll` | `x`, `y`, `dy` | `dy` wheel notches at that point; positive scrolls down |
| `key` | `keys`, `hold_ms?` (at most 10000) | One key with optional modifiers; `hold_ms` keeps it down (movement in games) |
| `type` | `text` | Types the text into the focused window |
| `wait` | `ms` (at most 10000) | Pauses |
| `record` | `seconds`, `fps?`, `window?`, `display?`, `region?` | Saves a sequence of frames and returns a contact sheet |

`window` is a window id from `windows`, or part of a title (case is ignored; a visible window is preferred to a minimised one). `display` is an index starting at 0. `region` is `[x, y, width, height]` inside the window or display, in the units `windows` reports.

`keys` is one key, optionally with modifiers joined by `+`: `Enter`, `Escape`, `Tab`, `Space`, `Backspace`, `Delete`, `ArrowLeft`, `Home`, `PageDown`, `F1` to `F12`, a single character such as `w`, or combinations such as `Control+S`, `Alt+F4`, `Shift+Tab`. Modifier names are `Control` (`Ctrl`), `Alt` (`Option`), `Shift` and `Meta` (`Cmd`, `Super`, `Win`).

### Captures and coordinates

`capture` saves the full-resolution PNG as `screen-<n>.png` in the project's artifacts directory (`~/.azazel/projects/<slug>/artifacts/`) and returns the image to the model scaled so its long side is at most 1568 pixels.

`x` and `y` of `click`, `move`, `drag` and `scroll` are pixels of the image the most recent `capture` returned, not screen coordinates. The tool remembers that capture's origin on screen and its scale, and maps each point back:

```
screen_x = origin_x + x * captured_width  / image_width
screen_y = origin_y + y * captured_height / image_height
```

Consequences:

- Capture before pointing. Without a capture the tool answers `capture first: coordinates refer to the last capture`.
- A point outside the last image is refused.
- After the window moves or resizes, or after capturing something else, capture again: the mapping always belongs to the latest capture, including one taken with `region`.

Input is queued for the application, which handles it in its own time. The tool waits 150 ms after each input action; capture again afterwards to see the effect.

A short sequence, as tool inputs:

```json
{"action": "windows"}
{"action": "focus", "window": "Notepad"}
{"action": "capture", "window": "Notepad"}
{"action": "click", "x": 240, "y": 180}
{"action": "type", "text": "hello"}
{"action": "capture", "window": "Notepad"}
```

### Recording

`record` captures the window, display or region for `seconds` (above 0, at most 60) at `fps` frames per second (default 5, at most 10).

- Frames go to `artifacts/recordings/<UTC timestamp>/0001.png`, `0002.png`, and so on.
- When `ffmpeg` is on `PATH`, the frames are also joined into `out.mp4` in the same directory.
- The result names the directory and includes one image: up to nine evenly spaced frames, three to a row, in reading order.

Use it for defects that only show in motion: animation glitches, flicker, physics.

### Permissions

`screen` counts as execution. It prompts in `ask` and `accept-edits` mode, runs without asking in `auto`, and is refused in `plan`. The subject of a call is its action name, so rules can allow all of it or parts:

```toml
[permissions]
allow = ["screen"]                                   # every action
# allow = ["screen(windows)", "screen(capture)"]     # looking only; input still prompts
```

Input goes to whatever is under the pointer or has the focus, so an agent with `screen` can operate any application on the desktop. Keep it on `ask` unless you are watching, and see [permissions](permissions.md).

### Platform notes

- **Windows.** The process declares itself DPI-aware so that window positions, capture pixels and input coordinates agree on scaled displays.
- **macOS.** `focus` uses `osascript` (System Events).
- **Linux.** `focus` needs `xdotool` or `wmctrl` on `PATH`.
- Errors from the capture and input libraries are returned as they are, for example when the system denies capture or input, or the compositor is not supported.
- A build made without the `screen` feature has no `screen` tool; `browser` still works.

## The `playtester` agent

A built-in agent definition for running and playing a build.

| Setting | Value |
|---|---|
| Tools | `shell`, `job`, `screen`, `browser`, `read` |
| Model | `@vision` (falls back to `@default` until you set `[roles] vision`) |
| Turn limit | 80 |
| Spawns | none |

It starts the build as a background `shell` job (or opens a web build with `browser`), finds and focuses the window, then loops: capture, one action, capture again and compare. It checks the job's output for crashes, kills the job when done, and does not edit the project. Its report has these sections: **Session**, **Defects** (most severe first, each with severity `crash`, `major` or `minor`, steps, expected, observed, whether it repeated, and the path of the frame), **Crash output**, **Observations**, **Not tested**.

Point the role at a model that can see images:

```toml
[roles]
vision = ["<provider>/<vision-model-id>"]
```

or override only this agent with `[agents.playtester] model = ["<provider>/<model-id>"]`. The lead can also delegate to it with the `task` tool; see [team](team.md).

## `/playtest`

```
/playtest <launch command> [-- <what to test>]
```

Starts a `playtester` in the background with the launch command, the working directory and what to test; its report is delivered to the lead when it finishes. What to test is the text after the last ` -- `, unless that text starts with `-`, in which case it is taken as part of the command. Without it, the playtester covers "the main flows a new player or user would try first".

```
/playtest ./build/game.exe
/playtest cargo run --release -- --windowed -- jumping and the pause menu
/playtest cargo run -- --level 3
```

In the third line everything is the launch command. The playtester's `shell` and `screen` calls go through the normal approval prompts, labelled with its agent id, unless the mode or your rules allow them.

## The `playtest` skill

The built-in skill `playtest` holds the same method in more detail: the launch steps, the capture/act/observe loop, a coverage checklist (front end, core loop, input, boundaries, interruptions, persistence, text and layout, performance by eye), what to do when something looks wrong, severity levels and the report template. Load it into the current conversation with `/skill:playtest`, or let the model pick it from the skills list when a task calls for it. See [skills and commands](skills-and-commands.md).
