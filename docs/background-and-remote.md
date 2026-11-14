# Background sessions, schedules and remote work

A session does not need a terminal that stays open. `azazel bg` runs a prompt in a background session
that goes on after you close the terminal; you look in, answer it, or take it over later. The same
mechanism lets the UI on your machine show a session that runs on another machine over SSH, and
lets a [schedule](#schedules) run a prompt at a time or when something happens.

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
`/rewind`, `/compact`, `/name <text>`, `/export`, `/share`, `/bug`, `/search`, `/btw`, `!commands`, the transcript
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
| `schedule-<id>.lock`, `trigger-<id>.lock` | Held by the run of a schedule while it starts, and by the session that watches a trigger |

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

## Schedules

A schedule runs a prompt at a time, with nobody there: every morning, every ten minutes, once in two
hours. Each run starts a background session like `azazel bg` does, so `azazel ps`, `logs` and `attach`
work on it as on any other.

```sh
azazel schedule add "0 9 * * 1-5" "check for outdated dependencies and open a branch that updates them"
azazel schedule add every 10m "look at the deploy and say what changed"
azazel schedule add at +2h "run the test suite again and summarise what still fails"
azazel schedule list
azazel schedule run 1a2b        # now, whatever the time is
azazel schedule pause 1a2b
azazel schedule resume 1a2b
azazel schedule rm 1a2b
```

| Command | What it does |
|---|---|
| `azazel [flags] schedule add <when> "<prompt>" [--session <id> \| --new]` | Adds a schedule and prints its id. The flags of an ordinary run that say how to work (`--model`, `--thinking`, `--mode`, `--set`, `--add-dir`, `--tools`, ...) are kept with it: every run works that way |
| `azazel schedule list [--json]` | The schedules of this project: id, when, state (`active`, `paused`, `ended`), the next run, what the last run came to, what its [checks](#checks-and-ends) said the last time, how often it ran, where the prompt goes, the prompt |
| `azazel schedule pause <id>` | Stops a schedule from running, and takes it out of the system's scheduler, until it is resumed |
| `azazel schedule resume <id>` | Lets a paused schedule run again and hands it back to the system's scheduler. An ended schedule is not resumed: add it again |
| `azazel schedule rm <id>` | Removes a schedule, from the system's scheduler too |
| `azazel schedule run <id>` | Runs it now, its checks first. `--due` runs it only when it is due; `azazel schedule run --due` runs every schedule of the project that is due at this moment. A paused or ended schedule does not run |

`<when>` is one of:

| Form | Meaning |
|---|---|
| `"<minute> <hour> <day> <month> <weekday>"` | A cron expression, in local time: lists (`1,15`), ranges (`9-17`), steps (`*/15`, `5/20`), month and weekday names (`jan`, `mon-fri`), `7` or `0` for Sunday; when both days are given, either one counts. `@hourly`, `@daily`, `@weekly`, `@monthly` and `@yearly` work too. Quote it: a shell expands `*` |
| `every <length>` | Again and again, counted from when the schedule was added: `every 10m`, `every 2h`, `every 1d`, `every 1h30m`. Whole minutes, from one minute to 31 days |
| `at <time>` | Once: `at +2h`, `at 17:30` (the next time the clock shows it), `at 2026-12-01 09:00`, or a full timestamp. The schedule is removed after its run |

A cron expression and an `at` time are in local time unless `--tz <zone>` names another zone
(`azazel schedule add --tz America/New_York "0 9 * * 1-5" "<prompt>"`), as the IANA database names it.
The system's scheduler keeps local time: it wakes a run of a cron expression of another zone at its times
as they are here and an hour either side (for summer time), and azazel decides in the expression's zone.

Where the prompt goes:

- By default, and with `--new`, every run starts a background session of its own, named `schedule <id>`.
  It ends two minutes after it has finished when nobody is attached; what it did stays in `azazel logs`
  and in its session.
- With `--session <id>` every run goes to that session. When a background session has it open, the
  prompt is sent to it: it starts on it at once when it has nothing to do, and after its current work
  otherwise. When none has, a background session is started on it. A session you have open in the
  interactive UI at that moment is not held by a background session, and a run would write to it
  beside you: give `--session` a session that only background runs use.

Schedules are kept per project, in `~/.azazel/projects/<project>/schedules.json`, not in the repository:
a checkout never brings schedules with it.

### Checks and ends

A schedule can run a shell command at each due time, before any model is asked, and go by its result:

```sh
azazel schedule add every 10m --if "git fetch -q && git status -sb | grep -q behind" "pull, rebuild and run the tests"
azazel schedule add every 5m --until "gh pr view 42 --json state -q .state | grep -q MERGED" "look at the review comments on PR 42"
azazel schedule add every 30m --if "curl -s https://example.com/version" --if-changes "a new version is out: read its notes"
azazel schedule add "0 9 * * *" --times 5 --ends-after 7d "summarise yesterday's issues"
```

| Flag | Effect |
|---|---|
| `azazel schedule add --if <command>` | The prompt runs only when the command holds; otherwise the run is skipped, and the list says why |
| `azazel schedule add --until <command>` | Runs first; once it holds, the schedule ends and its prompt does not run |
| `azazel schedule add --if-is <code>`, `--until-is <code>` | The exit code the command holds with: 0 unless given |
| `azazel schedule add --if-matches <regex>`, `--until-matches <regex>` | It holds only when what it prints matches; `^` and `$` are the ends of a line |
| `azazel schedule add --if-changes`, `--until-changes` | It holds only when it prints something else than the time before; never on its first run, which is what the next is compared with |
| `azazel schedule add --ends-after <length>` | The schedule ends this long after it is added: `2h`, `7d` |
| `azazel schedule add --ends-at <time>` | The schedule ends at this time, written as for `at`; with `--ends-after` too, the earlier counts |
| `azazel schedule add --times <n>` | The schedule ends once its prompt has run this many times |
| `azazel schedule add --tz <zone>` | The zone of a cron expression or `at` time |

A check holds when every condition given holds. It runs in the schedule's directory with the shell the
agents' commands run in (`[tools] shell`), for at most 60 seconds; one that cannot run or takes longer
does not hold. A trigger's checks run when it happens, in the session that saw it.

An ended schedule (its `until` held, its end time passed, or it ran its number of times) leaves the
system's scheduler and stays in the list, marked `ended` with the reason, until you remove it. An `at`
whose `if` does not hold at its time ends the same way.

A check runs later with nobody watching, as you: `azazel schedule add` is you typing it. When an agent adds
a schedule with the [`schedule` tool](tools.md#schedules), each check's command is asked about as a
`shell` command would be: the rules for `shell` and the mode decide, deny rules refuse it, and a
destructive command is asked about in every mode. A project that is not trusted cannot allow it with
rules of its own: its `permissions.allow` counts only once the project is trusted.

### Who starts a run

There is no daemon. A schedule is handed to the scheduler of the operating system, which starts
`azazel schedule run <id> --scheduled` at the times it may be due:

| System | Where the entry is |
|---|---|
| Windows | A task `azazel\<id>` of Task Scheduler (`schtasks`), which runs while you are signed in, without a console window |
| Linux | A line of your crontab that ends in `# azazel-schedule:<id>` |
| macOS | A launch agent `~/Library/LaunchAgents/dev.azazel.schedule.<id>.plist` |

azazel itself decides whether the run goes ahead. Where the scheduler cannot say a schedule exactly (a
cron expression in Task Scheduler, `every 45m` in cron), the entry wakes azazel more often and azazel
does nothing until the schedule is due.

- **Missed runs are not caught up.** A run whose time passed while the machine was off or asleep is
  gone: the scheduler is told not to make up for it, and a cron time that azazel is woken for more
  than two minutes late (ten for an `at`) does nothing. An `every` counts from its last run, so after
  a pause it runs once, not once for each interval that went by. An `at` that was missed stays in the
  list, marked `missed`, until you remove it.
- **One run at a time.** While the background session the last run started is still at work, the next
  run is skipped and the list says so. A prompt for a session that still has the last one waiting is
  not sent a second time. Two runs of one schedule woken at once are told apart by a lock file; one
  of them goes ahead.
- **Permissions.** A run has nobody to ask. In `manual` and `accept-edits` mode an approval makes the
  session wait (`azazel ps` shows `waiting for you`, and you are notified); add the schedule with
  `--mode auto`, or with allow rules, for work that should go through. See [permissions](permissions.md).

When `schtasks`, `crontab` or `launchctl` is not there, `schedule add` fails and says so. With
`[schedule] register = false` schedules are kept without the system's scheduler; call
`azazel schedule run --due` from whatever runs things for you (a systemd timer, a CI job).
`azazel doctor` says which scheduler is in use.

### Triggers

A trigger runs its prompt when something happens instead of at a time:

```sh
azazel schedule add --on ci-failure "pull the log of the failed run, find the cause and fix it on a branch"
azazel schedule add --on "file:src/**/*.rs" "run the tests of what changed and fix what fails"
```

| Trigger | When it runs |
|---|---|
| `--on ci-failure` | A CI run of the project ends in failure. The runs are read with `gh run list` (the GitHub CLI, signed in) every `[schedule] poll_secs` seconds (60). Runs that had failed before anybody watched do not count; the prompt gets the run's name, branch, address and the command that prints what failed |
| `--on file:<glob>` | A file of the project that matches the glob is added, changed or deleted. Files git ignores are not looked at. The prompt gets the list of changed files, once they have stopped changing for two seconds |

A trigger is watched by a session that is open in the project: the interactive UI, or a background
session (`azazel bg`, or one a schedule started). Nothing is watched while no session is open, and
what happened meanwhile does not run later. When several sessions are open, one of them owns each
trigger, so the prompt runs once.

The prompt goes to the session that saw it happen, as a message of yours: at once when the lead has
nothing to do, after its current work otherwise. `--session <id>` lets only that session take it;
`--new` starts a background session for it instead. Files that change while the lead works do not run
a `file:` trigger: they are the lead's own doing.

### The `schedule` tool and `/loop`

An agent can add, list, pause, resume and remove schedules with the [`schedule` tool](tools.md#schedules);
all but listing ask for permission like a command, and the commands of the checks like `shell` commands. Inside a session, [`/loop`](tui.md#loops) sends a prompt
again and again without a schedule.

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
