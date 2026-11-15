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
- **Host check.** The `Host` header must name the server: `127.0.0.1:<port>`, `localhost:<port>`, `[::1]:<port>`, a
  LAN address of this machine, or the host of `remote.public_url`. Anything else gets `421`. This stops DNS
  rebinding.
- **Origin check.** A request that has an `Origin` header must carry one of the allowed origins (the same
  addresses as above, with the scheme the server uses, and `remote.public_url`); otherwise `403`. A request
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
| `POST /api/links` | a device | Makes a new pairing link. |
| `POST /api/logout` | a device | Signs this device out. |
| `GET /api/ws/{id}` | a device | Upgrades to a WebSocket on one session. |

An unknown route under `/api/` is `404`, a wrong method `405`.

#### `POST /api/pair`

Request: `{"secret": "<the part of the link after #pair=>", "bearer": false}`. `bearer` is optional.

Answer `200`: `{"access": "view" | "control", "expires": "<RFC 3339>", "allow_bypass": false}`, with a
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
  "protocol": 1,
  "version": "0.11.0"
}
```

`allow_bypass` tells the page whether it may offer the `bypass` mode. The page uses `access` to hide what the
server would refuse anyway.

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

#### `POST /api/links`

Request: `{"access": "view" | "control"}`. A device with view access can make only view links (`403` otherwise).

Answer `200`: `{"url": "<origin>/#pair=<secret>", "access": "view", "expires": "<RFC 3339>"}`. The link works once
and for `remote.link_minutes`. The page shows it as text and as a QR code.

#### `POST /api/logout`

Signs this device out and clears the cookie. Answer `204`. Its WebSockets close with `4401`.

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

### What each access level may send

The server checks every command before it reaches the session; this is the policy. The commands are those of
[rpc.md](rpc.md).

| Access | Commands it may send |
|---|---|
| **view** | `attach`, `get_state`, `get_messages`, `get_agent_messages`, `get_tree`, `get_usage`, `list_models`, `list_roles`, `list_commands`. Frames arrive as for any client; a view client shows requests as "waiting for an answer" and cannot answer them. |
| **control** | Everything, except the commands in the next table. |

A control device may not send:

| Command | Why |
|---|---|
| `shell` | Running commands on this machine from a phone is not offered. |
| `export` with a `path` | It would write a file on this machine. |
| `resume_session` with a `path` | It would read a file on this machine. |
| `set_role` with `save` other than `session` | It would change the config files. |
| `save_preset` | It would change the config files. |
| `set_mode` to `bypass` | Refused unless `remote.allow_bypass` is set. |
| `run_command` of anything outside `cost`, `context`, `session`, `tools`, `todo`, `rules`, `doctor`, `skill:<name>` and the commands extensions mark as safe | `trust`, `add-dir`, `hooks` and `forget-approvals` change what agents may do. |

The server has no route that reads or writes files, so a browser cannot read `auth.json`, credentials or
configuration, cannot sign in or out of a provider and cannot change what is trusted. The server never approves
anything on its own.

When several clients are attached, the first answer to an approval or a question wins: the others get
`request_closed`, and a second answer is refused.
