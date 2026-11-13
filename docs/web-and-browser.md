# Web search, web fetch and the browser

Agents reach the web in three ways: `web_search` asks the model provider's own search or a search service, `web_fetch` downloads one URL and returns its main content as text (with `web_grep` to search that text), and `browser` drives a real Chromium-based browser for pages that need scripts, clicks or screenshots. `web_search`, `web_fetch` and `web_grep` are always present; `browser` is part of the default build (cargo feature `browser`).

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
search = "auto"          # auto | provider | brave | tavily | exa | searxng | duckduckgo
searxng_url = ""         # address of a SearXNG instance, e.g. "https://search.example.org"
provider_search = true   # whether `auto` asks the lead's provider first
```

| Backend | Needs |
|---|---|
| `provider` | A lead model whose provider searches the web itself; see [the provider's own search](#the-providers-own-search) |
| `brave` | env `BRAVE_API_KEY` |
| `tavily` | env `TAVILY_API_KEY` |
| `exa` | env `EXA_API_KEY` |
| `searxng` | `web.searxng_url`; the instance must have the `json` output format enabled |
| `duckduckgo` | Nothing. Uses the keyless HTML results page, which may answer with a bot challenge under load |

`auto` (the default) asks the lead's provider first when it has a search tool, and otherwise, or when that search fails, the first of `brave`, `tavily`, `exa` whose key is set, then `searxng` when `searxng_url` is set, then `duckduckgo`. `provider_search = false` takes the provider out of `auto`. Naming a backend explicitly uses that backend alone; without its key or URL every `web_search` call fails with a message naming what to set. `azazel doctor` prints who answers, for example `web search: provider (anthropic/claude-opus-5-5), then duckduckgo` or `web search: brave`.

### The provider's own search

Several providers run a web search on their side when a request asks for it. `web_search` uses that search when the lead's model is at one of them, so a search needs no key of its own and works on a subscription login:

| Provider | What is asked for |
|---|---|
| `anthropic` (API key or Claude subscription) | The Messages API's `web_search` server tool (`web_search_20250305`) |
| `openai`, `openai-codex` (ChatGPT login) | The Responses API's `web_search` tool |
| `google`, `google-gemini-cli` | Grounding with Google Search (`googleSearch`) |
| `xai`, `xai-oauth` | The `web_search` tool of xAI's Responses endpoint, also when the model converses over Chat Completions |

A provider you configure under another name counts when its address is one of these vendors' own (`api.anthropic.com`, `api.openai.com`, `chatgpt.com`, `generativelanguage.googleapis.com`, `cloudcode-pa.googleapis.com`, `api.x.ai`). Gateways and compatible endpoints (OpenRouter, Bedrock, Vertex, Azure, local servers) are not asked.

Each query is one request of its own on the lead's first model, with the lead's credentials, whichever agent called the tool. It carries no part of the conversation: only the query, `site`, `recency` and `limit`, and an instruction to search and cite. The model's thinking level is left off. The results are read from the citations of the answer, not from its wording:

- title and URL are the cited page's;
- the snippet is the passage the provider quotes from the page (Anthropic), or else the sentences of the answer that cite it (OpenAI, xAI, Gemini);
- pages the search found and the answer did not cite follow, without a snippet;
- Gemini names pages by redirect links on `vertexaisearch.cloud.google.com`; each is asked where it leads and replaced by that address, and kept as it is when it does not answer.

The domain of `site` is passed to the provider as a filter where it has one (Anthropic, OpenAI, xAI), and results outside the site, or outside the path it names, are dropped in any case. `recency` is part of the request's wording; no provider takes it as a filter.

The search falls back to the configured backend, per query, when the request fails, takes longer than 90 seconds, or its answer cites no page. The result's details then name the backend that answered and carry the provider's failure as `provider_error`. With `search = "provider"` there is no fallback: the failure is the result, and a lead whose provider has no search tool makes every call fail with a message saying so.

What the requests cost in tokens is counted in the session's spend and shown by `/cost` under `web_search`. Providers also bill each search they run (Anthropic and OpenAI per search, on top of tokens); that fee is not in azazel's figures. Anthropic is told to run at most 2 searches per query.

Permission rules see `web_search` as network access to the configured backend's host (`domain:html.duckduckgo.com` under `auto` with no key). The provider's own search goes to the host the session's model requests already go to and adds no subject.

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

## `http`

One request to a web API: the status line, the response headers and the body come back. It is the tool for JSON APIs and webhooks; `web_fetch` is the one for reading pages.

| Field | Meaning |
|---|---|
| `url` | An absolute `http` or `https` URL (required) |
| `method` | `GET` (default), `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE` or `OPTIONS` |
| `headers` | An object of request headers |
| `body` | The request body as text |
| `json` | The request body as a JSON value, sent with `Content-Type: application/json`; not together with `body` |
| `service` | The stored credential to send when several cover the host; `none` sends the request without one |
| `limit` | The most characters of the body to return, default 30000, at most 200000 |
| `timeout_secs` | How long the request may take, default 30, at most 300 |

```text
HTTP 200 OK · GET https://api.github.com/user · service github
content-type: application/json; charset=utf-8
x-ratelimit-remaining: 4998

{"login":"octocat","id":583231}
```

- Any status is a result: a `404` or a `500` comes back with its body. Only a request that could not be sent or answered is an error.
- A redirect is followed when the request is a `GET` or `HEAD` and the new address is on the same host (same scheme and port), at most 5 times. Otherwise the redirect itself is returned, with a line saying where it leads.
- A body longer than `limit` is cut, and the whole body is saved under artifacts; the last line names the file. Content that is not text (an image, an archive, a PDF) is saved to a file and the result names it. Bodies over 5 MB are refused.
- Link-local and cloud-metadata addresses are refused, as for `web_fetch`.

### Credentials for services

An agent should be able to call an API you have a key for without ever holding the key. `azazel secrets` stores the key with the host it belongs to, and `http` adds it to the requests that go there.

```sh
azazel secrets add github --host api.github.com --header "Authorization: Bearer ghp_..."
azazel secrets add maps --host "*.maps.example" --query "key=..."
azazel secrets add shop --host localhost:8080 --basic "me:password"
azazel secrets list            # names, hosts and kinds; never a secret (--json for one object per line)
azazel secrets rm github
```

- `--header "Name: value"` adds a request header, `--query "name=value"` a query parameter, `--basic "user:password"` HTTP basic authentication. Give one of them. Written as `-`, the value is asked for at the terminal without being shown (or read from a pipe), which keeps it out of the shell history.
- `--host` is a host name (`api.example.com`), its subdomains (`*.example.com`), or either with a port. Without a port the credential goes to the scheme's own port only.
- The credential is sent over `https` only, or to this machine (`localhost`, `127.0.0.1`, `[::1]`). A plain `http` request to a host with a stored credential is refused; `service: "none"` sends it without.
- Of several credentials for a host, the one stored for the host itself wins over a `*.` pattern, then the first name; `service` picks another. A credential is never sent to a host it was not stored for, whatever the agent asks.
- A header or query parameter of the same name in the agent's request is replaced by the stored one.
- The agent never sees the credential. The address shown is the one the agent wrote. In the response, every occurrence of the credential (the header value, the token after `Bearer`, the parameter as sent, the password and the encoded `user:password`) becomes `[redacted:credential]`, in the headers, the body, the saved file and error messages; the value of a `Set-Cookie` header is hidden too, and whatever else looks like a secret is redacted as with [`tools.redact_output`](tools.md). A `401` adds a line saying the stored credential was sent and refused.
- The tool description names the services stored when the session starts (name and host), so the agent knows what it can reach.

The credentials are kept in `~/.azazel/secrets.json` (wherever `AZAZEL_HOME` puts it), a file of their own beside the provider logins in `auth.json` and written the same way: mode `0600` on Unix, by temp file and rename under a lock file. It is a [secret file](permissions.md#secret-files): an agent that tries to read it is asked about in every mode but `bypass`. `azazel doctor` names what is stored.

### Permissions

`http` is network access, like `web_fetch`: it prompts in `manual` and `accept-edits` unless a rule allows it, and runs freely in `auto`. Its subjects are `domain:<host>` and, when the request would carry a stored credential, `auth:<service>`. An `allow` rule has to cover both, so allowing a host does not hand out the credential stored for it; the approval prompt says which credential the request carries, and offers no `Always allow` for such a request.

```toml
[permissions]
allow = ["http(domain:api.github.com)", "http(auth:github)"]
ask = ["http(auth:*)"]        # ask before any stored credential is used, in `auto` too
deny = ["http(auth:prod)"]
```

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
| `fill_login` | `ref_user?`, `ref_password?`, `submit?`, `tab?` | Types the login stored for this page into the fields; see [signing in](#signing-in) |
| `handoff` | `reason`, `url?`, `tab?` | Shows the browser to you, waits until you have finished there, and returns the page; see [signing in](#signing-in) |

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

### Signing in

An agent can sign in to a site without knowing the password, and can ask you to do the part only you can do.

```sh
azazel secrets add-site "https://github.com/login*" --username octocat
```

`azazel secrets add-site <url-pattern> --username <name> [--password <password>] [--name <name>]` stores a login. The password is asked for at the terminal when `--password` is left out. The pattern is the address of the sign-in page: scheme, host (or `*.host`), and a path in which `*` stands for any run of characters; without a path it covers every page of the site. The query and fragment of the page's address play no part. Only `https` pages, and pages served by this machine, can have a login. The login is named after the host unless `--name` says otherwise; `azazel secrets list` and `azazel secrets rm <name>` show and forget it.

`fill_login` types that login into the page:

```json
{"action": "fill_login", "ref_user": "e4", "ref_password": "e5", "submit": true}
```

- It works only when the address of the tab matches a stored pattern; on any other page nothing is typed and the result says no login is stored for it. Of several matching logins the one with the longest pattern is used.
- `ref_user` must be a text field (`text`, `email`, `tel`, `search`, `url`, `number`) and `ref_password` a password field; otherwise nothing is typed. Give one of them for a form that asks for the name and the password on separate pages. What was in a field is replaced. `submit` presses Enter afterwards.
- The result says which stored login was typed, never its values. From then on the password is taken out of everything the tool returns in this session (`[redacted:credential]`): snapshots, `eval` results, console and network lines, errors.

`handoff` is for what cannot be stored: a login you would rather type yourself, a second factor, a CAPTCHA, a consent screen.

```json
{"action": "handoff", "reason": "Sign in to the staging dashboard and approve the 2FA prompt", "url": "https://staging.example.com/login"}
```

- A browser running without a window is closed and started again with one, on the same profile (cookies and storage are kept) and with the same tabs at the same addresses; a browser that already has a window stays as it is. `url` is opened first when given.
- You are shown the `reason` and asked to finish in the browser window and press Enter. What you type before Enter goes to the agent as a note. Esc declines, and the agent is told so.
- The agent is paused meanwhile. It then gets your note, the list of tabs as you left them, and a snapshot of the current page, and carries on in the same window: the browser stays visible for the rest of the session.
- It needs someone to ask: in print mode and other runs without a user the action fails.

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

`browser` counts as execution: it prompts in `manual` and `accept-edits` mode and runs freely in `auto`. The subject is `domain:<host>` for `open` and `navigate`, and the action name for everything else (`fill_login` and `handoff` among them, so `ask = ["browser(fill_login)"]` asks before a stored login is typed in any mode).

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
