# Web search, web fetch and the browser

Agents reach the web in three ways: `web_search` asks a search service, `web_fetch` downloads one URL and returns its main content as text (with `web_grep` to search that text), and `browser` drives a real Chromium-based browser for pages that need scripts, clicks or screenshots. `web_search`, `web_fetch` and `web_grep` are always present; `browser` is part of the default build (cargo feature `browser`).

## `web_search`

| Input | Meaning |
|---|---|
| `query` | What to search for |
| `queries` | Several searches in one call, at most 5, in place of or beside `query` |
| `limit` | Number of results per query, default 8, at most 20 |
| `site` | Restrict to one domain, for example `docs.rs` |
| `recency` | `day`, `week`, `month` or `year` |

The result is a numbered list with title, URL and a snippet of at most 500 characters per hit. Snippets are excerpts; agents are told to read a page with `web_fetch` before relying on it.

One of `query` and `queries` is required. A call with one query answers with the list alone. A call with several sends three at a time and answers in the order asked, one section per query; blank and repeated queries are dropped, and `site`, `recency` and `limit` apply to each:

```text
## tokio select macro
1. select in tokio - Rust
   https://docs.rs/tokio/latest/tokio/macro.select.html
   Waits on multiple concurrent branches, ...

## tokio join macro
brave search failed with HTTP 429 Too Many Requests: rate limit exceeded
```

A query that fails says so in its section and the others keep their results; the call is an error only when every query failed.

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
| `offset` | The first line to return, 1-based (default 1) |
| `limit` | The most characters to return, default 30000, at most 200000 |

Behaviour:

- HTML is reduced to its main article and converted to Markdown (`markdown`) or plain text (`text`). If no article is found, the whole page is converted. `raw` returns the body as sent.
- JSON, XML, plain text and similar text types are returned unchanged.
- A PDF (by its content type, or by its first bytes when the server names no useful type) comes back as its text, every page under a `--- page N ---` line. The file is also saved under the project's artifacts directory, and the first line of the result names it: `<url> is a PDF (<n> bytes), saved to <path>`. A PDF without a text layer is saved and the result says why there is no text. A build without the `pdf` feature saves the file and says it cannot read it.
- Anything else (images, archives) is saved under the project's artifacts directory and the path is returned.
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

### Long pages

A page that fits `limit` is returned whole. A longer one comes in parts that end on a whole line, and the last line of each part says which lines it holds and where to go on:

```text
[lines 1-212 of 1830; continue with offset=213, or web_grep to search the page]
```

Calling again with that `offset` returns the next part from the cache, without another request; the last part ends with `[lines 1650-1830 of 1830]`. A single line longer than `limit` (a minified file) is cut, and the trailer gives its full length. An `offset` past the last line is an error that names the line count.

### URLs in `read`

The `read` tool accepts `http://` and `https://` paths and returns the page as Markdown through the same fetcher, with the same limits and cache. Its `offset` (first line) and `limit` (most lines) page a long page the same way, 30000 characters at a time, with the same trailer.

### Permissions

These tools have network access, which is not granted by `manual` or `accept-edits` mode on its own, so they prompt unless a rule allows them. The subject of a fetch is `domain:<host>`; a rule for a domain also covers its subdomains. Reading a URL with `read` and searching one with `web_grep` are governed by the `web_fetch` rules, so a host you allow for `web_fetch` needs no second rule; `tools.disabled` is the way to switch `web_grep` off.

```toml
[permissions]
allow = [
  "web_search",
  "web_fetch(domain:docs.rs)",
  "web_fetch(domain:localhost)",
]
```

See [permissions](permissions.md) for the rule grammar and modes.

## `web_grep`

Searches the text of a page line by line, so an agent can find the passage it needs in a long page without reading it. The page comes from the same cache as `web_fetch`: grepping a page that was just fetched, or fetching around a match, costs no request.

| Input | Meaning |
|---|---|
| `url` | An absolute `http` or `https` URL (required) |
| `pattern` | A regular expression (Rust `regex` syntax), matched within one line (required) |
| `ignore_case` | Match without regard to case |
| `literal` | Treat `pattern` as plain text |
| `context` | Lines shown before and after each match, 0 to 10 (default 0) |
| `limit` | Number of matches, 1 to 100 (default 25) |
| `format` | The form of the page to search: `markdown` (default), `text` or `raw` |

With `pattern` `[Ww]ork stealing`, `context` 1 and `limit` 2:

```text
211- The scheduler keeps one queue per worker.
212: Work stealing moves tasks from a busy worker to an idle one.
213- A stolen task runs on the thief's thread.
--
479- Each worker owns a queue.
480: The work stealing queue is bounded.
481- A full queue spills into the shared one.
[stopped at 2 matches; the next is on line 733: narrow the pattern or raise limit (at most 100)]
```

- `N: line` is a match and `N- line` a context line; `--` separates stretches that are apart. `N` is the line number `web_fetch` takes as `offset`, as long as both use the same `format`.
- Lines are cut at 250 characters. A long line is shown around its match, with `…` where text was left out.
- Without a match the result is `no matches for "<pattern>" (<n> lines scanned)`.
- A pattern that is not a valid regular expression is an error, reported before anything is fetched.

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
| `wait` | `for`, `timeout_ms?` (default 10000), `poll_ms?` (default 100) | Waits until `for` holds; see [waiting](#waiting) |
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
{"action": "wait", "for": "role:button \"Undo\""}
{"action": "screenshot"}
{"action": "console"}
```

### Waiting

`wait` returns as soon as its condition holds, with the time it took, and fails with `` `<for>` did not happen within <n> ms `` at `timeout_ms`. Waiting for what an action should bring about, rather than acting again at once, is what keeps a run from tripping over a page that has not settled.

| `for` | Holds when |
|---|---|
| `load` | The document has finished loading |
| `idle` | There has been no network activity for 500 ms |
| `text:<string>` | The page's visible text contains the string |
| `role:<role>` or `role:<role> "<name>"` | The accessibility tree has a node with that role, and that name when one is given. Both are written as a snapshot line shows them: `role:button "Save changes"` |
| `url:contains:<string>` | The tab's address contains the string |
| `title:contains:<string>` | The page title contains the string |
| `gone:<selector>` | No element matches the CSS selector any more (a spinner, a dialog) |
| anything else | A CSS selector: an element matches it |

The page is looked at every `poll_ms` milliseconds (default 100, at least 10), and once more at the limit.

### Permissions

`browser` counts as execution: it prompts in `manual` and `accept-edits` mode and runs freely in `auto`. The subject is `domain:<host>` for `open` and `navigate`, and the action name for everything else.

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
