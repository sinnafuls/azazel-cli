# Background sessions and remote work

A session does not need a terminal that stays open. `azazel bg` runs a prompt in a background session
that goes on after you close the terminal; you look in, answer it, or take it over later. The same
mechanism lets the UI on your machine show a session that runs on another machine over SSH.

There is no daemon. Each background session is one process (`azazel host`, started for you) that ends
when it is no longer needed. When that process is killed in the middle of its work, the session is
[started again where it stopped](#when-a-background-session-dies) the next time you attach.

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
| `azazel [flags] bg [--request-id <id>] <prompt>` | Starts a session in the background on the prompt and prints its id. The flags of an ordinary run apply: `--model`, `--mode`, `--name`, `-c`, `--session`, `--trust`, ... |
| `azazel ps [--json]` | Lists the background sessions: id, name, state (`working`, `waiting for you`, `idle`, or `interrupted` for one whose process died in the middle of a run), time since the start, cost, directory |
| `azazel logs <id> [-f] [--json]` | Prints what the session has done so far, as text. `-f` keeps printing until the session ends. `--json` prints the frames as they were written, one JSON object per line |
| `azazel attach <id>` | Opens the session in the full UI, beside whoever else is attached. `--plain` shows it as lines instead (see below). An `interrupted` session is started again first |
| `azazel attach --take <id>` | Ends the background session once the lead has finished what it is doing, which disconnects every client, then opens its session in an ordinary UI in this terminal. `--now` stops the work instead of waiting for it |
| `azazel kill <id>` | Stops the lead, ends the session cleanly, and the process exits. An `interrupted` session is taken off the list; its session file stays |

An `<id>` may be the start of the id, the session's name, or the start of the session's own id.
`azazel bg` answers as soon as the session has started; if it cannot start (no model, a session that is
open elsewhere) it says why and exits with 1. The id is the only thing on standard output, so
`id=$(azazel bg "...")` works in scripts.

A script that cannot tell whether its `azazel bg` went through (a timeout, a retry) names the request:
`azazel bg --request-id nightly-2026-11-13 "..."`. The same command sent again starts nothing and prints the id
of the session the first one started, for as long as that session is listed or its log is kept. The name goes
with the prompt into the session, where an RPC client can ask for the run by it (see
[named prompts](rpc.md#named-prompts)).

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
is resumed, the reply being written and the tools that run are shown as they stand, a request that
waits is shown, and what it does from then on appears as it happens.
Quitting the UI (`/quit`, `Ctrl+C` twice) leaves the session running; `azazel attach` returns to it.

Several clients can be attached to one session at the same time: two terminals, a terminal and an
editor plugin, you and a colleague over SSH. Each sees everything the session does. Any of them may
send a prompt, steer, stop the work or answer a request; what one types appears on the others, and a
permission request or question that one answers closes on the others. Nobody has to leave for somebody
else to come in, so `attach --take` is no longer how a session changes hands: it is what it always
was besides, the way to end the background session and continue its session in an ordinary UI (for
`/diff`, `/commit` and the rest of what works only there). It disconnects every client.

Most of the UI works the same on a background session: prompts, steering, queued messages, approvals
and questions, `/model`, `/thinking`, `/mode`, `/new`, `/resume`, `/fork`, `/tree`, `/retry`,
`/rewind`, `/compact`, `/name <text>`, `/export`, `/search`, `/btw`, `!commands`, the transcript
viewer, `/copy`, the agent hub, the [fleet view](tui.md#the-fleet-view) and `@agent` mentions, the
reports (`/cost`, `/context`, `/session`, `/tools`, `/todo`,
`/dirs`, `/add-dir`, `/rules`, `/hooks`, `/doctor`, `/trust`, `/forget-approvals`) and every command an
extension added (`/settings`, `/usage`, `/mcp ...`, `/goal`, skills).

What reads or changes files and state of the session's own process is not available while attached,
and says so: `/diff`, `/commit`, `/consult`, `/handoff`, `/name` without a text, `/jobs`, `/reload`,
`/permissions`, `/open <path>`, the `/mcp` picker, `Ctrl+B`, `# notes` for the memory, and renaming,
deleting, pinning and previewing other sessions in `/resume`. `/rewind` lists the checkpoints without
what changed in the files since each. The popup of an agent shows its work from the moment you
attached, and `@` completes the session's agents but not its definitions (typing `@scout ...` starts
one all the same). Your prompt history is this project's; prompts
sent to a session elsewhere are not added to it. Themes, key bindings, the clipboard, the external
editor and voice input are the UI's own and work as always.

### As lines

`azazel attach <id> --plain` (and `azazel attach` where there is no terminal) prints what the session
does as lines and reads lines from you:

| You type | Effect |
|---|---|
| any text | Sent to the session as a message |
| `/approve`, `/session`, `/always`, `/deny [what to do instead]` | Answers the oldest permission request: once, for the rest of the session, for good, or not at all |
| `/answer <text>` | Answers the oldest question: text, the number of an option (several, separated by commas, where several may be chosen), `yes` or `no`. Where the question lets you type your own answer, anything that is not an option is that answer. Several questions asked together are printed numbered and take one answer each, separated by `;` (`/answer 2; yes; ship it`); an empty one leaves its question open. `/answer` alone dismisses it |
| `/abort` | Stops the current work |
| `/continue` | Goes on with a run that was interrupted (see [below](#when-a-background-session-dies)) |
| `/detach` | Leaves; the session goes on (so does the end of the input, and `Ctrl+C`) |
| `/kill` | Ends the session |

It first prints what you missed: everything since the last client left, or since the start for the
first one. When somebody else is attached already, it prints the run in progress from the prompt that
started it.

### When a background session dies

A background session that ends by itself, or is ended with `azazel kill`, leaves the list. One whose
process is killed in the middle of a run (a crash, a reboot, a kill of the process) stays listed for seven
days, as `interrupted`:

```sh
azazel ps
# ID        NAME         STATE        ELAPSED  COST   DIRECTORY
# 3fa9c2d1  night shift  interrupted  2h 10m   $0.42  /srv/app
azazel attach 3fa9      # starts it again under the same id, on the same session, and goes on
```

`azazel attach <id>` (and `azazel rpc --attach <id>`) starts the process again with the options the first one
was started with and in the permission mode it was last in; `--mode` and `--model` on the `attach` command
line override them. It then goes on with the run: a tool call that only reads is run again, any other call
that had started is reported to the model as interrupted, and a request the model never answered is sent
again. What exactly happens is described under [interrupted runs](sessions-and-checkpoints.md#interrupted-runs).
With `[session] resume_runs = false` the new process waits instead, and `/continue` in the UI goes on.

`azazel kill <id>` forgets an interrupted session without starting it. Its session is an ordinary saved
session either way: `azazel -c`, `azazel --session <id>` or `/resume` opens it in the foreground, where the
UI offers `/continue` (and `azazel -c -p --continue-run` continues without a UI). Once the run has ended
anywhere, the entry leaves the list.

### One writer per session

A background session locks the session it has open. Opening that session anywhere else (`-c`,
`--session`, `/resume`) fails with `this session is running in the background: azazel attach <id>`, and
`/resume` marks such sessions in its list. `azazel attach --take` is how you get it back. The clients
attached to a background session are not writers: they all speak to the one process that is.

### Files

Everything is under `~/.azazel/run/`:

| File | What it is |
|---|---|
| `<id>.json` | What `azazel ps` shows: process id, session, directory, state, cost, the socket, and the mode and options it runs with, which is what starts it again |
| `<id>.log` | Every event frame the session wrote, one JSON object per line. Kept for seven days after the session ends, so `azazel logs <id>` still works |
| `<id>.sock` | The socket clients connect to (Unix; on Windows a named pipe `\\.\pipe\azazel-<id>`) |
| `<id>.lock`, `session-<session id>.lock` | Held while the process lives |
| `req-<hash>.req` | The id of the session a `bg --request-id` started |

The directory is readable by you only. Entries of a process that died without cleaning up are removed
the next time anything lists the sessions, unless it died in the middle of a run (see
[above](#when-a-background-session-dies)). A key given with `--api-key` is never written to an entry; pass
it again to the `attach` that restarts the session.

### For other programs

A background session speaks the [RPC protocol](rpc.md) on its socket. `azazel rpc --attach <id>`
connects standard input and output to it, so any program that can run a command can drive a
background session. The first command must be `attach`:

```json
{"id": 1, "type": "attach", "replay": "missed"}
```

`replay` is `missed` (the default), `all` or `none`. The response says where the session stands (its
`snapshot`: the reply being written, the tools that run, what waits); the frames asked for follow it,
then the requests that still wait for an answer, then everything the session does. Any number of
programs may be attached at once, each answered on its own connection. `{"type": "stop", "now": true}`
ends the session and may be sent on any connection, attached or not.

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
