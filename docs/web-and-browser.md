# Web search, web fetch and the browser

Agents reach the web in three ways: `web_search` asks a search service, `web_fetch` downloads one URL and returns its main content as text, and `browser` drives a real Chromium-based browser for pages that need scripts, clicks or screenshots. `web_search` and `web_fetch` are always present; `browser` is part of the default build (cargo feature `browser`).

## `web_search`

| Input | Meaning |
|---|---|
| `query` | What to search for (required) |
| `limit` | Number of results, default 8, at most 20 |
| `site` | Restrict to one domain, for example `docs.rs` |
| `recency` | `day`, `week`, `month` or `year` |

The result is a numbered list with title, URL and a snippet of at most 500 characters per hit. Snippets are excerpts; agents are told to read a page with `web_fetch` before relying on it.

### Backends

```toml
[web]
search = "auto"        # auto | brave | tavily | exa | searxng | duckduckgo
searxng_url = ""       # address of a SearXNG instance, e.g. "https://search.example.org"
```

| Backend | Needs |
|---|---|
| `brave` | env `BRAVE_API_KEY` |
| `tavily` | env `TAVILY_API_KEY` |
| `exa` | env `EXA_API_KEY` |
| `searxng` | `web.searxng_url`; the instance must have the `json` output format enabled |
| `duckduckgo` | Nothing. Uses the keyless HTML results page, which may answer with a bot challenge under load |

`auto` (the default) picks the first of `brave`, `tavily`, `exa` whose key is set, then `searxng` when `searxng_url` is set, then `duckduckgo`. Naming a backend explicitly without its key or URL makes every `web_search` call fail with a message naming what to set. `azazel doctor` prints the backend in use as `web search: <name>`.

## `web_fetch`

| Input | Meaning |
|---|---|
| `url` | An absolute `http` or `https` URL (required) |
| `format` | `markdown` (default), `text` or `raw` |

Behaviour:

- HTML is reduced to its main article and converted to Markdown (`markdown`) or plain text (`text`). If no article is found, the whole page is converted. `raw` returns the body as sent.
- JSON, XML, plain text and similar text types are returned unchanged.
- Anything else (images, PDFs, archives) is saved under the project's artifacts directory and the path is returned.
- Pages that build their content with scripts come back empty; use `browser` for those.

Limits:

| Limit | Value |
|---|---|
| Schemes | `http` and `https` only |
| Blocked addresses | Link-local and cloud-metadata addresses: `169.254.0.0/16`, `fe80::/10`, their IPv4-mapped forms, and `fd00:ec2::254`. Checked on the URL and again on the addresses the host name resolves to |
| Allowed addresses | Loopback and private ranges, so local development servers work |
| Redirects | At most 5, each target checked like the first URL |
| Timeout | 30 seconds per request |
| Body size | 5 MB; larger responses are refused |
| Cache | Results are kept in memory for 15 minutes per URL and format (the URL fragment is ignored) |

### URLs in `read`

The `read` tool accepts `http://` and `https://` paths and returns the page as Markdown through the same fetcher, with the same limits and cache.

### Permissions

Both tools have network access, which is not granted by `ask` or `accept-edits` mode on its own, so they prompt unless a rule allows them. The subject of a fetch is `domain:<host>`; a rule for a domain also covers its subdomains. Reading a URL with `read` is governed by the `web_fetch` rules.

```toml
[permissions]
allow = [
  "web_search",
  "web_fetch(domain:docs.rs)",
  "web_fetch(domain:localhost)",
]
```

See [permissions](permissions.md) for the rule grammar and modes.

## `browser`

One tool with an `action`. The browser starts on the first `open`, there is one per session, and it is closed when the session ends. If you close its window or it crashes, the next `open` starts a new one.

| Action | Parameters | Result |
|---|---|---|
| `open` | `url`, `tab?` (name for the new tab) | Opens a tab, waits for load (30 s), returns title, URL and a snapshot |
| `navigate` | `url`, `tab?` | Title and URL |
| `back`, `reload` | `tab?` | Title and URL |
| `snapshot` | `tab?` | The accessibility tree, one line per named or interactive node: `- <role> "<name>" [ref=e<n>]`, at most 400 lines |
| `click`, `hover` | `ref`, or `selector`, or `x` and `y` | Moves to the element's centre (scrolling it into view) and clicks or hovers |
| `type` | `text`, `ref?` or `selector?`, `submit?` | Focuses the element (or uses the current focus), inserts the text, optionally presses Enter |
| `key` | `keys`, `hold_ms?` | One key with optional modifiers: `Enter`, `Escape`, `ArrowDown`, `a`, `Control+A`, `Shift+Tab`, `F5` |
| `scroll` | `dy`, `ref?` / `selector?` / `x`,`y` | Scrolls by `dy` pixels (positive is down) at the target, or at the viewport centre |
| `wait` | `for`, `timeout_ms?` (default 10000) | `for` is `load`, `idle` (no network activity for 500 ms), a CSS selector, or `text:<string>` |
| `eval` | `expression` | Runs JavaScript, awaits promises, returns the value as JSON (cut at 16000 characters) |
| `screenshot` | `tab?`, `full_page?`, `ref?` / `selector?` | PNG of the viewport, the whole page, or one element; saved under artifacts and returned as an image |
| `resize` | `width`, `height` (200 to 8000) | Sets the tab's viewport in CSS pixels, for responsive checks |
| `console` | `tab?` | The last 100 console messages and uncaught exceptions |
| `network` | `tab?`, `filter?` (URL substring) | The last 100 requests: method, status, type, URL |
| `tabs` | | Lists tabs; `*` marks the current one. Pop-ups and links opened in new tabs are adopted |
| `close` | `tab?` | Closes a tab |

Notes:

- `tab` defaults to the tab used last. Unnamed tabs are called `tab-1`, `tab-2`, and so on.
- Refs (`e12`) come from the latest snapshot of that tab and are replaced by the next snapshot. `open` returns a snapshot, so its refs can be used at once.
- `open` and `navigate` take `http` and `https` URLs (and `about:blank`) only.
- Screenshots are scaled so the long side is at most 1568 pixels before they are sent to the model; the saved file keeps full resolution. A full-page capture is limited to 16000 pixels in height. A model without vision receives the text `[image omitted: model has no vision]` in place of the image, and the saved path.

A typical exchange, as tool inputs:

```json
{"action": "open", "url": "http://localhost:5173/"}
{"action": "click", "ref": "e7"}
{"action": "wait", "for": "text:Saved"}
{"action": "screenshot"}
{"action": "console"}
```

### Permissions

`browser` counts as execution: it prompts in `ask` and `accept-edits` mode and runs freely in `auto`. The subject is `domain:<host>` for `open` and `navigate`, and the action name for everything else.

```toml
[permissions]
allow = ["browser(domain:localhost)", "browser(snapshot)", "browser(screenshot)", "browser(console)"]
```

The rule `browser` without a pattern allows every action.

### Finding the browser

The binary is looked up in this order:

1. `[browser] path`: a program name on `PATH` or a path to a file;
2. the environment variable `AZAZEL_BROWSER`;
3. Chrome, Chromium, Edge and Brave, in that order: standard install locations on Windows and macOS, then their usual names on `PATH` (the only lookup on Linux).

If none is found, the tool returns `no Chromium-based browser found; set [browser] path`. `azazel doctor` prints the binary it would use.

```toml
[browser]
path = ""          # empty: search as described above
headless = true    # false shows the browser window
```

The browser runs on its own profile directory, `~/.azazel/browser-profile`, so logins made there persist between sessions and your everyday profile is not touched. Only one process can use that profile at a time; a second session that tries to start a browser gets an error saying so.

See also: [tools](tools.md), [screen and playtesting](screen-and-playtesting.md) for native windows, and the built-in `browser-testing` skill ([skills and commands](skills-and-commands.md)).
