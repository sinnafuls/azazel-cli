---
name: playtest
description: >-
  Use to run a game or application build and try it the way a player or user would: launching it, operating it through screen capture and input (or the browser for web builds), and reporting defects with the frames that show them.
---
# Playtesting

You are the first user of this build. Your job is to operate it, notice what is wrong, and prove each problem with a frame or a log line. You do not fix anything while playtesting.

## Tools

| Target | Tools |
|---|---|
| Native window (game, desktop app) | `shell` (background) and `job` to launch and watch the process; `screen` to see and operate it |
| Web build | `browser`: `screenshot` to see, `click` with `x`,`y`, `key` with `hold_ms`, `eval`, `console` |

`screen` actions: `windows` (list), `focus`, `capture`, `click`, `move`, `drag`, `scroll`, `key`, `type`, `wait`, `record`. Coordinates for `click`, `move`, `drag` and `scroll` are pixels of the image the most recent `capture` returned, so always capture before pointing.

The slash command `/playtest <launch command> [-- <what to test>]` starts a `playtester` agent with this method.

## Before you start

1. Read the task: the launch command, what to test, and anything about controls. If controls are unknown, look for a README, an in-game help screen or a controls menu before guessing.
2. Decide the session plan in three to six lines: the flows to cover and in what order. The named flows come first; exploration comes after.

## Launch

1. Start the build as a background job: `shell { command, background: true }`. Keep the job id.
2. Wait for the window: `screen windows` until the title appears (use `screen wait` between attempts; give up after about 30 seconds and read the job output).
3. `screen focus { window }`, then `screen capture { window }`.
4. Confirm it rendered: not a black or white frame, not an error dialog. If it failed to start, the job output is your report.

## The loop

Repeat until the plan is covered:

1. **Capture** the window.
2. **Describe** to yourself what the frame shows: the screen you are on, what can be interacted with, the state (health, score, selection, text).
3. **Decide** one action and what you expect it to do.
4. **Act**: one `key`, `click`, `type` or `drag`. For movement, hold: `key { keys: "d", hold_ms: 600 }`.
5. **Wait** briefly if the result takes time (`screen wait { ms: 300 }`).
6. **Capture** again and **compare** with the expectation.
7. **Note** the step in a running list: action, expected, observed, frame path when relevant.

One action per capture when you are investigating; a short sequence between captures when you are just travelling through known screens. Check the job's output every few minutes and at once if the window freezes, closes or stops responding.

Use `record { window, seconds, fps }` for anything that only shows in motion: animation glitches, physics, flicker, input lag. It returns a contact sheet of evenly spaced frames and saves all frames.

## What to cover

First, the flows the task names. Then:

- **Front end**: every menu item, back navigation from each screen, settings that apply and persist, quitting.
- **Core loop**: start, play for a while, win or complete, lose or fail, restart.
- **Input**: every documented control; two inputs at once; rapid repeated input; input during transitions and loading; holding a key through a state change.
- **Boundaries**: edges of the play area, minimum and maximum values, empty and full inventories, zero and many of anything countable.
- **Interruptions**: pause and resume, losing and regaining focus, resizing or toggling fullscreen where supported.
- **Persistence**: save, quit, relaunch, load.
- **Text and layout**: clipped or overlapping text, placeholder strings, wrong language, unreadable contrast.
- **Performance by eye**: stutter, long loads, input that lags behind.

## When something looks wrong

1. Capture immediately. The frame is your evidence.
2. Write down the last few steps that led here.
3. Try once more from a known state to see whether it repeats. Note "repeats", "intermittent (1 of 3)" or "seen once".
4. Reduce if cheap: does it happen without the previous step?
5. Check the job output for errors or a crash trace at the same moment.
6. Move on. Do not investigate the code.

## Severity

| Level | Meaning |
|---|---|
| `crash` | The build exits, freezes or cannot continue |
| `major` | A feature does not work, progress is blocked, or data is lost |
| `minor` | Works, with a visible flaw: cosmetic glitch, wrong text, awkward behaviour |
| `note` | Observation about feel, pacing or clarity; not a defect |

## Rules

- Send input only to the build under test. Focus its window first; if focus is lost, re-focus before continuing.
- Do not edit the project or its save data unless the task says so.
- Do not report a defect without a frame or log output that shows it.
- Do not report what you inferred from reading code as if you had observed it.
- Kill the job when finished: `job { action: "kill", id }`.

## Report

```
# Playtest: <build or command>

Session: <launch command>; <duration>; flows covered: <list>

## Defects

### 1. <title> (major)
Steps:
1. …
2. …
Expected: …
Observed: …
Repeats: yes | intermittent (n of m) | seen once
Evidence: <frame path>; <log lines if any>

## Crash output
<relevant lines of the job's output, quoted>

## Observations
- <feel, pacing, clarity>

## Not tested
- <flow>: <why>
```

Order defects by severity. Keep steps exact enough that someone else can follow them without having watched.
