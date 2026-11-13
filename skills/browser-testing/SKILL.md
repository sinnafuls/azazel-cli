---
name: browser-testing
description: >-
  Use to check a web page or web app in a real browser with the browser tool: confirming a change works, walking a user flow, finding why a page is broken, or capturing evidence. Snapshot, act, assert, with console and network checks.
---
# Browser testing

The `browser` tool drives a real Chromium. Use it to observe what a user would get, not to assume it from source. Every claim in your report should rest on something you saw: a snapshot line, an evaluated value, a console or network entry, or a screenshot.

## The loop

1. **Open** the page: `browser { action: "open", url }`. The result gives the title, the final URL and a snapshot.
2. **Snapshot** is the page as an accessibility tree, one line per named or interactive node:

   ```
   - RootWebArea "Sign in" [ref=e1]
     - heading "Welcome back" [ref=e2]
     - textbox "Email" [ref=e3]
     - textbox "Password" [ref=e4]
     - button "Sign in" [ref=e5]
   ```

   Find the element you need by role and name, and use its `ref`.
3. **Act** on a ref: `click`, `type` (with `submit: true` to press Enter), `hover`, `key`, `scroll`.
4. **Wait** for the consequence, not for time: `wait { for: "text:Dashboard" }`, `wait { for: "role:button \"Undo\"" }` (role and name as a snapshot line shows them), `wait { for: "url:contains:/dashboard" }`, `wait { for: "title:contains:Inbox" }`, `wait { for: "gone:.spinner" }` (the element has left the page), `wait { for: "#results li" }`, `wait { for: "load" }` or `wait { for: "idle" }` (no network activity).
5. **Assert** by reading: take a fresh `snapshot` and look for the expected node, or `eval` an expression that returns the fact (`document.title`, `location.pathname`, `document.querySelectorAll("tr").length`).
6. **Check health**: `console` for errors and uncaught exceptions, `network` for failed or unexpected requests.
7. **Capture**: `screenshot` (add `full_page: true`, or `ref`/`selector` for one element) for anything visual and for evidence.

Refs belong to the snapshot they came from. After any action that changes the page, take a new snapshot before using refs again. A stale ref gives an error that says so.

## Starting what you test

- Start the dev server as a background `shell` job, wait for its "listening" line with the `job` tool, then `open` the local URL. Stop the job when you finish.
- For a static file, serve the directory (`python -m http.server`, or the project's own preview command) rather than opening `file://`, which the tool does not load.
- Local addresses are fine. The first navigation to a host may ask the user for permission, depending on the permission mode.

## Finding elements

Prefer, in order:

1. `ref` from the snapshot: it is what assistive technology and users perceive, and it fails loudly when the page changes.
2. `selector` (CSS) when the element has no accessible name or you need something precise: `selector: "[data-testid=save]"`.
3. `x`, `y` viewport coordinates for canvases and games, taken from a screenshot.

An element missing from the snapshot is itself a finding: it has no role or name, is hidden, or has not rendered. Check with `eval` (`!!document.querySelector(...)`), then check `console`.

## Typing and keys

- `type { ref, text }` focuses the field and inserts the text. Without `ref` or `selector` it types into whatever has focus.
- `key { keys }` presses one key with optional modifiers: `Enter`, `Escape`, `Tab`, `Shift+Tab`, `ArrowDown`, `Control+A`. Use `hold_ms` to keep a key down.
- To replace a field's content: click it, `key Control+A`, then `type`.
- Walk keyboard navigation with repeated `Tab` and a `screenshot` every few stops to confirm the focus ring.

## What to assert

| Question | Evidence |
|---|---|
| Did the action have its effect | A snapshot node that was not there before; an `eval` of state or URL |
| Is the right content shown | Snapshot text; `eval` of `innerText` for long content |
| Did navigation happen | The page line (`title · url`) in the result; `eval location.href` |
| Did the form validate | Error text in the snapshot next to the field; focus position |
| Did the request succeed | `network { filter: "/api/" }`: method, status, type, URL |
| Any script errors | `console`: entries tagged `[error]` or `[exception]` |
| Does it look right | `screenshot`, viewed |
| Is it accessible | Roles and names in the snapshot; contrast via `eval` of computed styles |

Assert the negative too: after a failed login, the dashboard must not be reachable; after deleting, the item must be gone after a `reload`.

## A flow, end to end

```
open      http://localhost:5173/login
type      ref=e3  "ada@example.com"
type      ref=e4  "correct horse"  submit
wait      for "text:Dashboard"
snapshot                       -> heading "Dashboard", link "Sign out"
eval      location.pathname    -> "/dashboard"
network   filter "/api/login"  -> POST 200
console                        -> empty
screenshot
```

Then the failure path: wrong password, expect the error text, expect to remain on `/login`, expect a 401 in `network` and no exception in `console`.

## Responsive and visual checks

- `resize { width, height }` sets the tab's viewport in CSS pixels. Check at least 360×800, 768×1024 and 1280×800, with a `screenshot` at each, and confirm with `eval "window.innerWidth"`.
- Always look at the screenshot you took. Report what is in it, not what the code says should be.
- For a specific component, screenshot by `ref` so the evidence is unambiguous.

## Debugging a broken page

1. `console` first. An exception at load explains most blank pages.
2. `network`: a 404 for a script or stylesheet, a failed API call, a blocked cross-origin request.
3. `eval` to probe state: is the root element empty, did the framework mount, what does the store or `localStorage` hold.
4. `snapshot` to see what did render.
5. Reproduce after a `reload` to separate first-load problems from state problems.
6. Go to the source with what you learned, fix, and run the same checks again.

## Tabs

`open` with `tab: "<name>"` names a tab; later actions take `tab` to address it and default to the tab used last. `tabs` lists them (including pages the site opened itself) and `close` closes one. Use a second tab to compare two states or two users.

## Limits to respect

- Do not enter real credentials or personal data unless the user supplied them for this purpose. Use the project's test accounts.
- Do not drive third-party sites beyond what the task needs, and do not try to get around bot checks.
- Close what you opened and stop the servers you started.

## Report

State the flows exercised and, for each, pass or fail with the evidence: the decisive snapshot lines or evaluated values, console and network findings, and screenshot paths. List what was not tested. If you changed code to fix a failure, show the same check passing afterwards.
