# Background sessions and remote work

A session does not need a terminal that stays open. `azazel bg` runs a prompt in a background session
that goes on after you close the terminal; you look in, answer it, or take it over later. The same
mechanism lets the UI on your machine show a session that runs on another machine over SSH.

There is no daemon. Each background session is one process (`azazel host`, started for you) that ends
when it is no longer needed.

## Background sessions

```sh
azazel bg "run the test suite and fix what fails"    # prints an id, for example 3fa9c2d1
azazel ps                                            # what is running
azazel logs -f 3fa9                                  # follow what it does
azazel attach 3fa9                                   # open it in the UI
azazel kill 3fa9                                     # stop it
```

| Command | What it does |
|---|---|
| `azazel [flags] bg <prompt>` | Starts a session in the background on the prompt and prints its id. The flags of an ordinary run apply: `--model`, `--mode`, `--name`, `-c`, `--session`, `--trust`, ... |
| `azazel ps [--json]` | Lists the background sessions that are running: id, name, state (`working`, `waiting for you`, `idle`), time since the start, cost, directory |
| `azazel logs <id> [-f] [--json]` | Prints what the session has done so far, as text. `-f` keeps printing until the session ends. `--json` prints the frames as they were written, one JSON object per line |
| `azazel attach <id>` | Opens the session in the full UI. `--plain` shows it as lines instead (see below) |
| `azazel attach --take <id>` | Ends the background session once the lead has finished what it is doing, then opens its session in an ordinary UI in this terminal. `--now` stops the work instead of waiting for it |
| `azazel kill <id>` | Stops the lead, ends the session cleanly, and the process exits |

An `<id>` may be the start of the id, the session's name, or the start of the session's own id.
`azazel bg` answers as soon as the session has started; if it cannot start (no model, a session that is
open elsewhere) it says why and exits with 1. The id is the only thing on standard output, so
`id=$(azazel bg "...")` works in scripts.

In the UI, `/bg <prompt>` does the same from the session you are in: it copies the session and runs the
prompt in the copy, in the background. Your own session stays as it was, so two processes never write
one session file.

### What a background session does when nobody is there

- It works exactly as it would in the UI, in the permission mode it was started in.
- When a tool call needs your approval, or the agent asks a question, the request waits. `azazel ps`
  shows `waiting for you`, and you get a desktop notification (see `[notify]` in
  [configuration](configuration.md)). The request is shown again to whoever attaches next.
- When its work is done and nobody has been attached for ten minutes, it exits. Its session is saved
  like any other: `azazel -c` or `/resume` continues it.

Start with `--mode auto` (or with allow rules) when the work should not wait for you.

### Attaching

`azazel attach <id>` opens the UI on the session: what it said before is summarised as when a session
is resumed, a request that waits is shown, and what it does from then on appears as it happens.
Quitting the UI (`/quit`, `Ctrl+C` twice) leaves the session running; `azazel attach` returns to it.

One client is attached at a time. A second one is refused and told the process id of the first.

Most of the UI works the same on a background session: prompts, steering, queued messages, approvals
and questions, `/model`, `/thinking`, `/mode`, `/new`, `/resume`, `/fork`, `/tree`, `/retry`,
`/rewind`, `/compact`, `/name <text>`, `/export`, `/search`, `/btw`, `!commands`, the transcript
viewer, `/copy`, the agent hub, the reports (`/cost`, `/context`, `/session`, `/tools`, `/todo`,
`/dirs`, `/add-dir`, `/rules`, `/hooks`, `/doctor`, `/trust`, `/forget-approvals`) and every command an
extension added (`/settings`, `/usage`, `/mcp ...`, `/goal`, skills).

What reads or changes files and state of the session's own process is not available while attached,
and says so: `/diff`, `/commit`, `/consult`, `/handoff`, `/name` without a text, `/jobs`, `/reload`, the
list of `/permissions`, the `/mcp` picker, `Ctrl+B`, `# notes` for the memory, and renaming, deleting,
pinning and previewing other sessions in `/resume`. Your prompt history is this project's; prompts
sent to a session elsewhere are not added to it. Themes, key bindings, the clipboard, the external
editor and voice input are the UI's own and work as always.

### As lines

`azazel attach <id> --plain` (and `azazel attach` where there is no terminal) prints what the session
does as lines and reads lines from you:

| You type | Effect |
|---|---|
| any text | Sent to the session as a message |
| `/approve`, `/always`, `/deny [what to do instead]` | Answers the oldest permission request |
| `/answer <text>` | Answers the oldest question: text, the number of an option, `yes` or `no`. `/answer` alone dismisses it |
| `/abort` | Stops the current work |
| `/detach` | Leaves; the session goes on (so does the end of the input, and `Ctrl+C`) |
| `/kill` | Ends the session |

It first prints what you missed: everything since the last client left, or since the start for the
first one.

### One writer per session

A background session locks the session it has open. Opening that session anywhere else (`-c`,
`--session`, `/resume`) fails with `this session is running in the background: azazel attach <id>`, and
`/resume` marks such sessions in its list. `azazel attach --take` is how you get it back.

### Files

Everything is under `~/.azazel/run/`:

| File | What it is |
|---|---|
| `<id>.json` | What `azazel ps` shows: process id, session, directory, state, cost, the socket |
| `<id>.log` | Every event frame the session wrote, one JSON object per line. Kept for seven days after the session ends, so `azazel logs <id>` still works |
| `<id>.sock` | The socket clients connect to (Unix; on Windows a named pipe `\\.\pipe\azazel-<id>`) |
| `<id>.lock`, `session-<session id>.lock` | Held while the process lives |

The directory is readable by you only. Entries of a process that died without cleaning up are removed
the next time anything lists the sessions.

### For other programs

A background session speaks the [RPC protocol](rpc.md) on its socket. `azazel rpc --attach <id>`
connects standard input and output to it, so any program that can run a command can drive a
background session. The first command must be `attach`:

```json
{"id": 1, "type": "attach", "replay": "missed"}
```

`replay` is `missed` (the default), `all` or `none`. The frames asked for follow the response, then
the requests that still wait for an answer. `{"type": "stop", "now": true}` ends the session and may
be sent on any connection, attached or not.

## Remote work over SSH

```sh
azazel --ssh build-box --remote-dir /srv/app "why does the nightly build fail?"
```

azazel runs on the other machine and the UI runs here. Its agents, tools, credentials, permissions
and sessions are the other machine's: every tool works on files there, `!commands` run there, `@`
completes its files, and what you paste as an image travels with the prompt. Nothing of the directory
you start from is used.

| Flag | Effect |
|---|---|
| `--ssh <host>` | The machine, named as you would for `ssh` (a host of your `~/.ssh/config`, `user@host`) |
| `--remote-dir <dir>` | The directory to work in there; the login directory when left out |
| `--remote-azazel <command>` | How azazel is started there, when it is not `azazel` on the path |
| `--detach` | Run the prompt in a background session there, which survives the connection, and open the UI on it. Without a terminal, and with `-p` or `--plain`, only its id is printed |
| `--attach <id>` | Open a background session of that machine; with `--plain`, as lines |
| `-p` | Print the reply instead of opening the UI |

`--model`, `--thinking`, `--mode`, `--name`, `-c`, `--session`, `--set`, `--trust` and the `--no-*`
switches are passed on to the other azazel. `azazel --ssh <host> ps`, `logs <id>` and `kill <id>` list,
show and stop its background sessions.

```sh
azazel --ssh build-box --detach "bisect the failing test"   # a background session there, in the UI here
azazel --ssh build-box ps
azazel --ssh build-box --attach 3fa9                        # back to it after the connection dropped
```

A connection that drops ends the UI, not a background session: attach again and go on. A session
started without `--detach` ends with the connection and is saved there; `azazel --ssh <host> -c`
continues it.

What it needs:

- azazel installed on the other machine, of the same protocol version. When it is missing or too old
  you are told, with the one line that installs it there.
- A login that needs no questions: `ssh` is run with `BatchMode=yes`, so an unknown host key, a
  password or a passphrase prompt fails instead of appearing under the UI. Log in once with `ssh`
  yourself to accept the host key, and use a key held by an agent.
- A POSIX shell as the login shell on the other machine.

The connection is `ssh -T <host> azazel rpc ...`, so everything SSH does (jump hosts, ports, keys, the
config file) applies. The same list of what the UI can and cannot do as under [attaching](#attaching)
applies here.

## Related

- [RPC mode](rpc.md): the protocol a background session and the remote UI speak.
- [Sessions and checkpoints](sessions-and-checkpoints.md): where sessions are kept, `/resume`, forks.
- [Permissions](permissions.md): modes and rules for work that should not wait for approvals.
