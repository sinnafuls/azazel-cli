# Remote: follow and steer a session from a browser

Remote opens a session of azazel in a web page, on this machine or on a phone. It shows what the session does as
it happens and, when you allow it, lets you answer approvals and questions, steer the run and stop it.

## The model: master and child

The azazel process is the **master**. It owns the session, the agents, the tools, the credentials and the
permissions, and it keeps running with or without a browser. The browser is a thin **child**: it sends commands and
draws the frames it gets back. Nothing of the session lives in the page; close the tab and the session goes on, open
another and it catches up from the session's own log.

The page speaks the [RPC protocol](rpc.md) unchanged: one JSON object per WebSocket text message, in both
directions. This page describes what is around that: how a browser gets in, which commands each kind of access may
send, and the routes the server answers.

## Using it

```sh
azazel bg "run the test suite and fix what fails"
azazel remote
```

`azazel remote` serves every background session of this machine on `http://127.0.0.1:7420` and prints a pairing
link and a QR code. Open the link in a browser on this machine, and the page lists the sessions. Nothing listens on
the network: the server binds `127.0.0.1` only.

| Flag | Does |
|---|---|
| `--port <N>` | The port to listen on (`[remote] port`, 7420). When it is taken the next nine are tried and the one taken is printed. `0` lets the system choose |
| `--view` | Makes the first link a view link: it can watch but not answer or steer |
| `--json` | Prints `{"url", "pair", "port"}` as the first line of standard output; the rest goes to standard error |
| `--tunnel` | Reaches the server from other devices through a [Cloudflare quick tunnel](#from-a-phone-cloudflare) |
| `--tailscale` | Reaches it through [Tailscale](#from-a-phone-tailscale) |
| `--share` | Reaches it the way `[remote] access` says, `cloudflare` or `tailscale` |
| `--public-url <URL>` | The address of your own tunnel or reverse proxy in front of the server |

While it runs, type `l` and Enter for a new control link, `v` for a new view link, `r` to sign every device out and
void every link, and `q` to stop. Ending the server signs every device out; paired devices are not kept across
restarts.

## Reading a session

The page shows what the lead says as text and what it does as quiet rows: tool calls that follow one another are one
row that says what they did (`Created a file, ran 2 commands`), with a spinner while one runs and the last lines of a
command that is still running beneath it. The row opens in place, on a phone as on a desktop, into one card per call:
its command or input, the change it made with line numbers, and its output in a block that scrolls by itself once it
is longer than about twenty lines, with a button that copies each. A call that failed says `Failed` on its row.

## The model picker

The model chip of a session names the role the lead follows and the model it answers with, `@lead · deepseek/deepseek-chat`,
or the model alone when one was picked by name. Its picker lists the roles that resolve to a model you can use, with that
model and where the role is set, above the models of the providers that are [set up](models-and-logins.md#which-providers-are-listed).
Picking a role puts the lead on it (`set_model` with `["@<role>"]`); picking a model puts it on that model. Either lasts
for the session.

## Starting a session from the browser

A device with control access can start a background session: **New chat** (the `+` beside the list of sessions) asks
for a folder, an optional first message and a permission mode, and opens the session once it runs. In a session, the
pen at the top starts a new chat in the same folder. The session is the same as one `azazel bg` starts: it runs
on the computer, with the options `azazel remote` was started with, and keeps running when the page closes.

Which folders a browser may name:

- The projects sessions have worked in on this computer, and the folders of `[remote] folders` with every folder
  inside them. The sheet lists them and opens a folder to show the ones inside it; hidden folders are left out.
- Any folder of the computer once `[remote] allow_bypass` is set. The folder can then be typed in as well.

A folder whose project is not trusted could run commands through its configuration, so:

- With `[remote] allow_bypass` the session starts, trusting the folder for that session only.
- Otherwise the terminal of `azazel remote` asks: `A browser asks to start a session in ~/work/app, which is not
  trusted…`. `y` and Enter start it, trusting the folder for that session only; anything else, two minutes without an
  answer, or no terminal at all (its input closed) refuse it, and the browser says why. The questions come one at a
  time.

A browser can also continue an earlier session instead of starting a new one: `/resume` in the message box, **Resume…**
in the sidebar, **Or continue an earlier session…** in the New chat sheet, or **Continue** on a session that has ended
open a searchable list of the earlier sessions in the folders the browser may look into (those of `[remote] folders`, or
any folder with `[remote] allow_bypass` or `[remote] full_access`) and of the sessions it started itself (name or first
message, secrets hidden unless `[remote] redact` is off, folder,
when it was last used, number of messages; the project on screen first). Picking one starts a background session on it,
in the folder it worked in, and opens it; when a background session has it open already, that one is opened. It needs
control access, like New chat, and the same rules decide about the folder.

Trusting a folder for good is done on the computer, by running azazel in it once. `/remote`, which shares the session
of a terminal, does not start other sessions.

```toml
[remote]
folders = ["~/work", "~/src/site"]
```

## Sharing the session of your terminal

`/remote` in the terminal UI shares the session you are working in with browsers. The terminal stays the master:
the agents, tools and permissions are this process's, and what a browser sends runs as if you had typed it.

| Command | Does |
|---|---|
| `/remote` | Starts the server on `127.0.0.1` (`[remote] port`) and opens a box with a control link and its QR code. The link is shown in the box, not in the scrollback |
| `/remote view` | A view link: it can watch but not answer or steer |
| `/remote tunnel` | Reaches the server through a [Cloudflare quick tunnel](#from-a-phone-cloudflare); a missing `cloudflared` is offered through the terminal's own popups |
| `/remote tailscale` | Reaches it through [Tailscale](#from-a-phone-tailscale) |
| `/remote share` | Reaches it the way `[remote] access` says |
| `/remote stop` | Stops the server and the tunnel and signs every browser out |

While the session is shared, a line above the input says so and how many browsers are attached. An approval or a
question is shown at the terminal and in the browsers at once: the first answer wins and the other side's request
closes. A secret that an agent asks for is asked at the terminal only. A browser can send prompts, steer, queue a
follow-up, stop the run and change the mode, model and thinking level; every other command answers that it is not
available from a shared terminal session. A browser that reconnects resumes from the last frame it saw.

## From a phone: Cloudflare

```sh
azazel remote --tunnel
```

This runs `cloudflared tunnel --url http://127.0.0.1:<port>`, reads the `https://<name>.trycloudflare.com` address it
prints, and shows the pairing link and QR code for that address. No Cloudflare account or domain is needed. The
tunnel stops when `azazel remote` does, and the address changes on every start, so devices pair again each time.

`cloudflared` is found on `PATH` or in `~/.azazel/remote/`. When it is missing, a run at a terminal asks once:

- **Install it for me** downloads the official release for your platform from
  `github.com/cloudflare/cloudflared/releases/latest` into `~/.azazel/remote/`.
- **Show me how** prints the install command and the download page.
- **Cancel** ends the run.

A run without a terminal, and a download that fails, print the same help: `winget install --id Cloudflare.cloudflared`
on Windows, `brew install cloudflared` on macOS, your distribution's package or the release binary on Linux, and
<https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/>.

The address is public, so the pairing link is what protects it: it works once, for 15 minutes, and the server
accepts the tunnel's host and nothing else besides this machine. Quick tunnels have no uptime guarantee and
allow 200 requests in flight; the page uses one WebSocket and no server-sent events, which they do not carry. To use
a named tunnel or a reverse proxy of your own, set `[remote] public_url` or pass `--public-url`.

## From a phone: Tailscale

```sh
azazel remote --tailscale
```

This asks `tailscale status --json` for the name of this machine on your tailnet, runs `tailscale serve --bg
--https=<port> http://127.0.0.1:<port>` on the first of the HTTPS ports 443, 8443 and 10000 that you do not serve
yet, and shows the link and QR code for `https://<name>.ts.net`. The phone needs the Tailscale app signed in to the
same account. When the server stops, only the entry azazel added is removed (`tailscale serve --https=<port> off`);
your other serve settings stay.

azazel never installs Tailscale. When it is missing or not signed in, the run says what to do: install it from
<https://tailscale.com/download>, sign in with `tailscale up`, and install the app on the phone. Turn on MagicDNS and
HTTPS certificates in the Tailscale admin console if the machine has no name.

## The HTTP and WebSocket API

This section is the contract between the server (`crates/azazel-remote`) and the web page (`web/`). Everything the
page does goes through the routes below.

### Conventions

- The server listens on `127.0.0.1:7420` by default. The port is fixed because an installed page belongs to its
  origin, port included. When it is taken, the server tries 7421 to 7429 and reports the one it took.
- Request and response bodies are JSON (`Content-Type: application/json`) unless a route says otherwise.
- An error is a status code with the body `{"error": "<what happened and what to do>"}`.
- `POST` requests need the header `X-Azazel: 1`. A plain HTML form cannot send it, which closes the door on
  cross-site posts.
- **Authentication.** Every `/api/` route except `POST /api/pair` needs a signed-in device: the cookie
  `azazel_device` (what a browser has after pairing) or the header `Authorization: Bearer <device token>` (scripts
  and tests). Without one the answer is `401`.
- **Host check.** The `Host` header must name the server: `127.0.0.1:<port>`, `localhost:<port>`, `[::1]:<port>`, the
  host of the tunnel, or the host of `remote.public_url`. Anything else gets `421`. This stops DNS
  rebinding.
- **Origin check.** A request that has an `Origin` header must carry one of the allowed origins (the same
  addresses as above, with the scheme the server uses); otherwise `403`. A request
  signed in by cookie must have an `Origin`; a request with a Bearer token may leave it out.
- Answers on `/api/` carry `Cache-Control: no-store`. Every answer carries `X-Content-Type-Options: nosniff`,
  `Referrer-Policy: no-referrer`, and a `Content-Security-Policy` of `default-src 'self'; connect-src 'self';
  img-src 'self' data: blob:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'`. A page must therefore
  have no inline scripts.
- Limits: a request body is at most 64 KiB; a WebSocket message at most 12 MiB.

### Routes

| Route | Needs | What it does |
|---|---|---|
| `GET /` and any other path that is not `/api/…` | nothing | The web page. A path that is not a built file answers with `index.html`, so the page can route by itself. Gzip is used when the browser accepts it. |
| `POST /api/pair` | the pairing secret | Turns a pairing secret into a signed-in device. |
| `GET /api/me` | a device | Who this device is and what it may do. |
| `GET /api/sessions` | a device | The sessions this server can reach. |
| `POST /api/sessions` | a control device | Starts a session in a folder. |
| `POST /api/folders` | a control device | The folders a session may be started in, or those inside one. |
| `POST /api/links` | a device | Makes a new pairing link. |
| `POST /api/logout` | a device | Signs this device out. |
| `GET /api/devices` | a control device | The paired devices, without their tokens. |
| `DELETE /api/devices` | a control device | Signs every device out and voids every link. |
| `GET /api/ws/{id}` | a device | Upgrades to a WebSocket on one session. |

An unknown route under `/api/` is `404`, a wrong method `405`.

#### `POST /api/pair`

Request: `{"secret": "<the part of the link after #pair=>", "bearer": false}`. `bearer` is optional.

Answer `200`: `{"access": "view" | "control", "expires": "<RFC 3339>", "allow_bypass": false, "full_access": false, "can_start": true}`, with a
`Set-Cookie: azazel_device=<token>; HttpOnly; SameSite=Strict; Path=/; Max-Age=<seconds>` (and `Secure` when the
page is served over https). With `"bearer": true` the answer also has `"token"`, and no cookie is set: this is for
programs, which send the token as `Authorization: Bearer`.

The secret works once. The answers: `401` for a secret that is unknown, used or expired, `429` once too many
attempts failed (see [limits](#limits)).

#### `GET /api/me`

Answer `200`:

```json
{
  "access": "control",
  "expires": "2026-11-16T09:30:00Z",
  "allow_bypass": false,
  "full_access": false,
  "can_start": true,
  "protocol": 1,
  "version": "0.11.0"
}
```

`allow_bypass` tells the page whether it may offer the `bypass` mode, and `can_start` whether it may start sessions
(a control device of `azazel remote`; not of `/remote`). The page uses them and `access` to hide what the server would
refuse anyway.

#### `GET /api/sessions`

Answer `200`: `{"sessions": [ … ]}`, newest first. Each session:

| Member | Meaning |
|---|---|
| `id` | What goes into `/api/ws/{id}`. |
| `name` | The session's name, or `null`. |
| `kind` | `background` for a background session (`terminal` is for the session of a terminal UI that shares itself). |
| `state` | `working`, `waiting for you`, `idle` or `interrupted`. |
| `cwd` | The project directory. |
| `cost_usd` | The cost so far, or `null`. |
| `mode` | The permission mode, or `null`. |
| `started` | RFC 3339. |

The page asks again when it needs a fresh list; there is no push.

#### `POST /api/folders`

Request: `{}` for the folders to start in, or `{"under": "<folder>"}` for the folders inside one.

Answer `200`:

| Member | Meaning |
|---|---|
| `at` | The folder looked into, or `null`. |
| `parent` | Its parent, when that may be looked into too; else `null`. |
| `folders` | Each with `path`, `name` (the path with `~` for the home directory), `kind` (`project`, `root` for one of `remote.folders`, `dir` for one inside it or anywhere), `trusted` and `startable`. |
| `anywhere` | Whether any folder may be named (`remote.allow_bypass`). |

A folder that may not be looked into is `403`, one that does not exist `400`. A view device gets `403`.

#### `POST /api/sessions` (start)

Request: `{"cwd": "<folder>", "prompt": "…", "mode": "auto", "request_id": "…"}`. Only `cwd` is needed; `~` is the
home directory. Without `prompt` the session waits for a message. `mode` is the configured one when left out, and
`bypass` is refused unless `remote.allow_bypass` is set. The same `request_id` sent again starts nothing and answers with
the session the first request started.

Answer `200`: `{"id": "<session id>"}`, which goes into `/api/ws/{id}`. The answer waits while the terminal is asked
about a folder that is not trusted (see [starting a session from the browser](#starting-a-session-from-the-browser)).
A folder that does not exist or a `cwd` that is not a whole path is `400`; a folder outside those allowed, a refusal at
the terminal, a view device or a refused mode `403`; more than 10 starts a minute from one device `429`; a session that
could not start `500`. Each says why in `error`.

#### `POST /api/sessions` with `resume` (continue)

Request: `{"resume": "<session id>", "prompt": "…", "mode": "auto", "request_id": "…"}`, with an `id` from
`/api/history` (the start of one, 8 characters or more, is enough when only one listed session has it). `cwd` is not
needed: the session works in its own folder. A session outside the folders a browser may look into, that the server did not start,
is answered as an id that does not exist, whether or not a background session has it open. The mode is the one asked for
or the configured one, never one the earlier session was left in. The answer is `{"id": "<session id>"}` of the new background session, or of
the one that has the earlier session open already; two requests for the same session at once start one. An id that matches
no session, no allowed session or several is `400`. A path is never
accepted. The other answers are those of a start.

#### `POST /api/history`

Request: `{}` for the earlier sessions, most recently used first (at most 100), or `{"query": "<words>"}` for those whose
name, folder or conversation mention the words.

Answer `200`: `{"sessions": [...]}`, each with `id`, `name`, `cwd`, `created`, `updated` (RFC 3339), `first_prompt`,
`messages` and `running` (the id of the background session that has it open, or `null`). Only sessions in folders a browser may look
into, and those this server started, are listed; `name` and `first_prompt` have secrets hidden when `remote.redact` is on. A view device gets `403`.

#### `POST /api/links`

Request: `{"access": "view" | "control"}`. A device with view access can make only view links (`403` otherwise).

Answer `200`: `{"url": "<origin>/#pair=<secret>", "access": "view", "expires": "<RFC 3339>"}`. The link works once
and for `remote.link_minutes`. The page shows it as text and as a QR code.

#### `POST /api/logout`

Signs this device out and clears the cookie. Answer `204`. Its WebSockets close with `4401`.

#### `GET /api/devices`

Lists the devices that are signed in, the one paired first at the top. A view device gets `403`.

Answer `200`: `{"devices": [{"id": "<short label>", "access": "view" | "control", "paired": "<RFC 3339>",
"expires": "<RFC 3339>", "current": true}]}`. `id` is the first characters of a hash of the token, only a name for the
row; no token or pairing secret is ever returned. `current` is true for the device that asked.

#### `DELETE /api/devices`

Signs every device out, this one too, and voids every pairing link that was not used: what `r` does in the terminal
where `azazel remote` runs. It needs the header `X-Azazel: 1` and, as with other changes, an `Origin` when the cookie
signs the request in. A view device gets `403`. Answer `204` and clears the cookie. Every WebSocket closes with `4401`.

### The WebSocket

`GET /api/ws/{id}` with the usual upgrade headers (`Upgrade: websocket`, `Sec-WebSocket-Version: 13`, a key). A
request with a wrong `Host` or `Origin` gets the plain HTTP answer (`421`, `403`) and no upgrade. Everything else is
accepted and, when it fails, closed with a code, because a browser cannot read the status of a refused upgrade: no
device is `4401`, an unknown session `4404`, too many sockets `4409`.

Once upgraded:

1. The server sends the session's `hello` frame as the first message.
2. The client sends `attach` (see [rpc.md](rpc.md#background-sessions)); until then the session
   refuses everything else. The answer, the snapshot and the replay of what the client missed arrive as ordinary
   frames.
3. After that the socket carries commands in one direction and frames (responses, events, requests) in the
   other, as the RPC protocol defines them.

Each message is one JSON object in a text message. A binary message closes the socket with `1003`. A command the
device's access does not allow is not forwarded: the server sends a `response` with the command's `id` and
`"success": false` and an `error`. A message that is not a command gets the same, with `"id": null`. A client that
sends more than 20 commands a second (a burst of 40 is fine) gets the same error for the surplus.

A frame carries `"at"`, the position in the session's log. A client that was cut off attaches again with
`"since": <the last at it saw>` and gets only what it missed.

**Close codes**

| Code | Meaning | What the page does |
|---|---|---|
| `1000` | The session ended. | Shows the session as ended; goes back to the list. |
| `1003` | A binary message was sent. | A bug of the client. |
| `1009` | A message was longer than 12 MiB. | Tells the user to send less. |
| `4401` | Not signed in, or the device was revoked, signed out or has expired. | Shows the pairing screen. Does not retry. |
| `4404` | No session has this id (it may have ended). | Goes back to the list. |
| `4408` | The client fell behind: 4096 frames were waiting to be sent. | Reconnects at once and attaches with `since`. |
| `4409` | Too many connections: 4 per device, 16 in all. | Waits, then retries with backoff. |

### Pairing

1. Whoever runs the server asks for a link (the command prints one at start, and `l` for control or `v` for view
   makes another). It is `<origin>/#pair=<secret>`: 32 random bytes in base64url, valid once and for
   `remote.link_minutes` (15). The secret is in the fragment so that it never reaches a server log, a proxy or a
   `Referer`.
2. The person opens the link. The page reads the secret from `location.hash`, removes it with
   `history.replaceState`, and sends it to `POST /api/pair`.
3. The server answers with the device cookie. The device stays signed in for `remote.device_hours` (24). The
   server keeps only a hash of the token, in memory: ending the server signs every device out.
4. From then on the cookie authorises `GET /api/…` and the WebSocket. A device with control access can make more
   links with `POST /api/links`.

Ten failed pairing attempts in a row void every open link; a new one has to be made.

### Limits

| What | Limit |
|---|---|
| Failed pairings | 5 a minute per address, 20 a minute in all; after 10 in a row every link is void |
| WebSockets | 4 per device, 16 in all |
| Commands | 20 a second per connection, burst 40 |
| Message size | 12 MiB |
| Outbound queue | 4096 frames, then the socket is closed with `4408` |
| Sessions started | 10 a minute per device |

### What each access level may send

The server checks every command before it reaches the session; this is the policy. The commands are those of
[rpc.md](rpc.md).

| Access | Commands it may send |
|---|---|
| **view** | `attach`, `get_state`, `get_messages`, `get_agent_messages`, `get_tree`, `get_usage`, `get_theme`, `get_context`, `get_cost`, `list_models`, `list_roles`, `list_commands`. Frames arrive as for any client; a view client shows requests as "waiting for an answer" and cannot answer them. |
| **control** | Everything, except the commands in the next table. |

A control device may send `handoff` (and the page's `/handoff`), which asks the model for a brief and switches the page to the new session; a view device may not. `get_state` carries the prompt cache's clock, which the status bar shows.

Without `remote.full_access` a control device may not send:

| Command | Why |
|---|---|
| `shell` | Running commands on this machine from a phone is not offered. |
| `export` with a `path` | It would write a file on this machine. |
| `resume_session` with a `path` | It would read a file on this machine. |
| `set_role` with `save` other than `session` | It would change the config files. |
| `save_preset` | It would change the config files. |
| `set_mode` to `bypass` | Refused unless `remote.allow_bypass` is set. |
| `run_command` of a command outside the list a browser may run | The list is the reports (`cost`, `context`, `session`, `tools`, `todo`, `rules`, `doctor`, `usage`, `memory`, `agents`, ...), the commands of the built-in extensions that act on the session itself, `skill:<name>` and `mcp:<server>:<prompt>`. Commands that reach the machine are never run: `login`, `logout`, `settings`, `trust`, `add-dir`, `hooks`, `forget-approvals`, `reload`, `bg` and the like. A project's own custom commands and the commands of Luau scripts run only with `remote.allow_bypass`, whatever their name: the session knows where each command comes from (`source` in `list_commands`: `builtin`, `skill`, `mcp`, `custom` or `script`) and refuses to run a `custom` or `script` command for a browser that may not, so a project command named like `/memory` or `/mcp` does not pass for the built-in one. The prefixes `skill:` and `mcp:` belong to skills and MCP prompts; a custom command or a script cannot take them. |
| `run_command` of `/preset` with anything but a name | `/preset <name>` (letters, digits, `-`, `_`, `.`) is fine; `/preset save ...` would change the config files. |
| `run_command` of `/mcp` with anything but no arguments, `list` or `reconnect <server>` | An allowlist, so `enable`, `disable`, `approve`, `reject`, `login` and `logout` are refused, and so is any argument with a quote or a character other than letters, digits, `-`, `_` and `.` (the command strips quotes, so `"approve" srv` is `approve srv`). |
| `run_command` of `/memory` or `/media` with arguments | `/memory` and `/memory list` and a bare `/media` are fine; `/memory forget` and `/memory auto` change what is stored, and `/media` takes no arguments. |
| `set_setting` with a `scope` other than `session`, or for a trust-gated key or the `remote` table | It would change the config files or what agents may do. `session` scope changes a live setting for this session only. A missing `scope` is `session`. |
| `trust_project`, `add_dir`, `forget_approvals` | They change what the machine trusts and what agents may reach. |

With `remote.full_access` none of the refusals above applies to a control link (a command with no name is still no command), and the menu of `list_commands` is not filtered. A view link stays view-only. See [full access](#full-access).

The server has no route that reads or writes files, so a browser cannot read `auth.json`, credentials or
configuration (it lists the names of folders, only where it may [start a session](#starting-a-session-from-the-browser)), cannot sign in or out of a provider and cannot change what is trusted. The server never approves
anything on its own.

When several clients are attached, the first answer to an approval or a question wins: the others get
`request_closed`, and a second answer is refused.

## Full access

By default a control link cannot reach the machine: it cannot run `shell`, write files through `export` or `resume_session` with a path, save presets, save a role or a setting to a config file, run `/login`, `/logout`, `/settings`, `/trust`, `/add-dir`, `/hooks`, `/forget-approvals`, `/reload`, `/bg`, `/preset save`, `/mcp approve` or any other `/mcp`, `/memory` or `/media` subcommand that changes anything, or send `trust_project`, `add_dir` and `forget_approvals`. A browser may run the other commands (reports, `/usage`, `/memory`, `/agents`, `/preset <name>`, `/mcp reconnect`, skills and MCP prompts), and `set_setting` with `scope: session` for a setting that is not trust-gated (never the `remote` table). `get_settings`, `get_theme`, `get_context` and `get_cost` are read-only; the last two and `get_theme` are open to view links.

With `[remote] full_access = true` a control link may send everything the terminal can, including every command above, the `bypass` mode, project commands, any `set_setting` scope and sessions in any folder. A view link stays view-only. `azazel remote` prints a warning when it starts with it on, and `/api/me` and `/api/pair` report `full_access` (and `allow_bypass` as on, since full access allows `bypass`) so the page shows a banner and offers the mode. `/remote` in the terminal shows the same warning when it starts sharing. Turn it on only on a machine and with links you trust.
