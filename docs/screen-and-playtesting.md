# Screen capture, input and playtesting

The `screen` tool lets an agent see and operate the desktop: it lists windows, captures a window or display as an image, and sends mouse and keyboard input. On top of it, the `playtester` agent, the `playtest` skill and the `/playtest` command run a build, play it the way a user would and report defects with the frames that show them. `screen` is part of the default build (cargo feature `screen`); web builds are driven with the `browser` tool instead (see [web and browser](web-and-browser.md)).

## The `screen` tool

One tool with an `action`.

| Action | Parameters | Result |
|---|---|---|
| `windows` | | One line per titled window: id, title, app, position, size, and `minimised` when it is |
| `capture` | `window?`, `display?`, `region?` | An image of the window, else of the display (the primary one by default). The text names the capture id |
| `zoom` | `region`, `window?`, `display?`, `capture_id?` | The `region` (`[x, y, width, height]` in pixels of the capture) of that capture's target as it is now, at the screen's own resolution, for reading small text. It does not change what `x` and `y` mean |
| `cursor` | `window?`, `display?`, `capture_id?` | Where the pointer is, in pixels of the capture, or that it is off the capture, or its screen position when there is no capture |
| `focus` | `window` | Brings the window to the front and gives it the keyboard |
| `open` | `app`, `args?` | Starts an application by name or path, or opens a file or address with its application. Windows: `ShellExecute`; macOS: `open -a` (or `open` for a path); Linux: the program when `app` is on `PATH`, else `xdg-open`. Then list the windows to find it |
| `click` | `x`, `y`, `button?` (`left`, `right`, `middle`), `double?`, `count?` (1 to 3), `modifiers?` | Moves the pointer there, waits 50 ms and clicks. `count: 3` is a triple click; `modifiers` (`shift`, `ctrl`, `alt`, `meta`) are held during the click |
| `move` | `x`, `y` | Moves the pointer |
| `drag` | `to` (`[x, y]`), `from?` (`[x, y]`), `duration_ms?` (default 300, at most 5000) | Presses the left button at `from` (the pointer's position when omitted), moves in steps, releases at `to` |
| `scroll` | `dy?`, `dx?`, `x?`, `y?` | `dy` wheel notches (positive scrolls down) and `dx` notches (positive scrolls right) at that point, or at the pointer when `x` and `y` are omitted |
| `key` | `keys`, `repeat?` (1 to 100), `hold_ms?` (at most 10000) | One key with optional modifiers, `repeat` times 8 ms apart; `hold_ms` keeps each press down (movement in games). `hold_ms` times `repeat` must stay under 30 s |
| `type` | `text` | Types the text into the focused window. Text longer than 200 characters, or with a non-ASCII character, is pasted through the clipboard, which is put back afterwards |
| `wait` | `ms` (at most 10000) | Pauses |
| `record` | `seconds`, `fps?`, `window?`, `display?`, `region?` | Saves a sequence of frames and returns a contact sheet |

`window` is a window id from `windows`, or part of a title (case is ignored; a visible window is preferred to a minimised one). `display` is an index starting at 0. `region` is `[x, y, width, height]` inside the window or display, in the units `windows` reports.

`keys` is one key, optionally with modifiers joined by `+`: `Enter`, `Escape`, `Tab`, `Space`, `Backspace`, `Delete`, `ArrowLeft`, `Home`, `PageDown`, `F1` to `F12`, a single character such as `w`, or combinations such as `Control+S`, `Alt+F4`, `Shift+Tab`. Modifier names are `Control` (`Ctrl`), `Alt` (`Option`), `Shift` and `Meta` (`Cmd`, `Super`, `Win`).

### Captures and coordinates

`capture` returns the image to the model as a JPEG (quality 75) scaled so its long side is at most 1568 pixels, and saves that same JPEG as `screen-<id>.jpg` in the project's artifacts directory (`~/.azazel/projects/<slug>/artifacts/`). A full-resolution PNG is not written. For a 1920×1080 display of a game-like frame the old way, a saved PNG plus a resized PNG sent, cost 4.4 MB and 1.6 MB; the JPEG is about 49 KB. `zoom` is for what the downscaling makes unreadable. Frames of `record` stay PNG.

`x`, `y`, `from` and `to` of `click`, `move`, `drag` and `scroll` are pixels of an image `capture` returned, not screen coordinates. The tool remembers the last 16 captures with their origin on screen and their scale. Which one an action uses:

- `capture_id`, the number the capture reported, when given;
- else the latest capture of the `window` or `display` named in the same call;
- else the latest capture.

So a capture of the display does not make the window's coordinates unusable. The point maps back as:

```
screen_x = origin_x + x * captured_width  / image_width
screen_y = origin_y + y * captured_height / image_height
```

Consequences:

- Capture before pointing. Without a capture the tool answers `capture first: coordinates refer to the last capture`.
- A point outside the image is refused.
- When the window or display has moved or been resized since the capture, the action is refused with `… moved or was resized since capture <id>; capture again`. This covers captures taken with `region` too: the whole target is compared.
- `zoom` captures a part of the same target again but changes nothing in the mapping: keep pointing in pixels of the capture you chose.

The tool waits 50 ms between moving the pointer and pressing a button or turning the wheel, and 150 ms after every input action, because input is queued for the application, which handles it in its own time. Capture again afterwards to see the effect.

What is on screen is untrusted: text in a window, a web page or a game is data, and the tool's description tells the model never to follow instructions shown there, to verify each action with a new capture, and to ask the user before anything irreversible.

A short sequence, as tool inputs:

```json
{"action": "windows"}
{"action": "focus", "window": "Notepad"}
{"action": "capture", "window": "Notepad"}
{"action": "zoom", "window": "Notepad", "region": [0, 0, 300, 80]}
{"action": "click", "window": "Notepad", "x": 240, "y": 180, "count": 3}
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

`screen` counts as execution. It prompts in `manual` and `accept-edits` mode, runs without asking in `auto`, and is refused in `plan`. The subject of a call is its action name, so rules can allow all of it or parts:

```toml
[permissions]
allow = ["screen"]                                   # every action
# allow = ["screen(windows)", "screen(capture)"]     # looking only; input still prompts
```

Input goes to whatever is under the pointer or has the focus, so an agent with `screen` can operate any application on the desktop. Keep the mode on `manual` unless you are watching, and see [permissions](permissions.md).

### Safety

Input actions (`click`, `move`, `drag`, `scroll`, `key`, `type`) are guarded; `windows`, `capture`, `focus`, `wait` and `record` are not.

- **Esc stops it.** While the agent drives the desktop, and for the rest of the turn after it started, a press of Esc anywhere on the desktop cancels the turn. The key still reaches the application you pressed it in. Later `screen` input calls in that turn fail with a message that says so; your next request lifts it. An Esc the tool sends itself with `key` does not count. Held keys and buttons are released when an action stops. Windows only so far: on macOS and Linux there is no Esc watch yet, and input is not refused for lack of one. If the watch cannot start on Windows, input is refused.
- **One session at a time.** The first input action takes `~/.azazel/screen.lock`, which names the process and the session. Another azazel session that tries input meanwhile gets an error naming the holder. The lock is released when the agent stops, at the end of the session, and on Esc; a lock whose process is gone is taken over.
- **Cancelling.** An aborted turn stops before the next input call, between the steps of a `drag` and during `hold_ms` of `key`.
- **Which applications.** `[screen] allow_apps` lists applications that may receive input, by program file or app name, case ignored (`allow_apps = ["notepad.exe", "MyGame"]`). When it is not empty, each input action first checks the application that has the focus and is refused if it is not listed, with the name it found. Empty, the default, permits any application. Use `focus` to bring a listed window forward.

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
