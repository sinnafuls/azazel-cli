# Permissions

Every tool call an agent makes passes one gate before it runs. The gate combines the session's permission mode, allow/ask/deny rules from config, and five safety guards: deny rules, protected paths, secret files, destructive commands and a guard that stops agents from pushing. When it has to ask, your answer decides. There is no OS-level sandbox: containment is this gate, [checkpoints](sessions-and-checkpoints.md) and [worktree isolation](team.md#worktree-isolation).

## Modes

There are five modes. One mode governs the whole session: the lead, every sub-agent and what the session runs in the background.

| Mode | What it is for | Runs without asking | Asks | Refuses |
|---|---|---|---|---|
| `plan` | Investigating and writing a plan | Read-only tools; `write`/`edit` under `.azazel/plans/`; `plan_submit` | Reads of secret files | Every other write, command and network call |
| `manual` (default) | You approve each action | Read-only tools on paths inside the project | Everything else | |
| `accept-edits` | Edits flow, commands are yours | As `manual`, plus file writes inside the project | Commands, network, writes outside the project | |
| `auto` | No approvals, every guard on | Everything | Only what a guard forces: an `ask` rule, a protected path, a secret file, a destructive command | |
| `bypass` | No approvals, no guards | Everything, including protected paths, secret files, destructive commands and what `ask` rules name | Nothing | Only what a `deny` rule or the push guard refuses |

"Inside the project" means under the project root or one of `permissions.additional_dirs`, plus directories added
with `--add-dir <DIR>` or, for the rest of the run, `/add-dir <path>` (`/dirs` lists them all; Luau extensions
see only the configured ones). A read outside those roots asks in `manual` and `accept-edits`. Commands are `shell`, `browser`, `screen` and MCP tools that are not marked read-only; network calls are `web_search`, `web_fetch`, `web_grep`, `read` of a URL or of a `pr://` or `issue://` address, and the `generate_*` tools. `run_code` itself counts as a read; each tool call its script makes is judged like a call the agent makes directly.

`manual` was called `ask` before 0.5. The old name is refused wherever a mode is given (config, `--mode`, `/mode`, RPC, ACP) with a message that names `manual`. The `ask` list of rules is something else and keeps its name.

### The guards, mode by mode

| Guard | `plan` | `manual` | `accept-edits` | `auto` | `bypass` |
|---|---|---|---|---|---|
| `deny` rule | refused | refused | refused | refused | refused |
| [Push guard](#agents-never-push) | refused | refused | refused | refused | refused |
| `ask` rule | refused unless it is a read, which asks | asks | asks | asks | runs |
| Write to a [protected path](#protected-paths) | refused | asks | asks | asks | runs |
| [Secret file](#secret-files) | a read asks; a write or a command is refused | asks | asks | asks | runs |
| [Destructive command](#destructive-commands) | refused | asks | asks | asks | runs |

### Bypass

`bypass` is for a machine or a checkout you are willing to lose: a container, a throwaway VM, a scratch clone. Nothing is asked and nothing is guarded except by your own `deny` rules and the push guard. [Hooks](hooks.md) still run and may still block a call; a hook's answer `ask` has nothing left to ask for. `[permissions] destructive = "deny"` is your refusal, like a deny rule, and holds in bypass too.

Entering it always takes a deliberate step:

| How | The confirmation |
|---|---|
| `/mode bypass`, or `bypass` in the `/mode` picker | A warning box whose first answer is `No` |
| `--mode bypass` | Typing the flag is the confirmation |
| `/settings` → Permissions → mode → `bypass` | A question whose first answer is `No`; the setting applies to the sessions you start afterwards |
| `[permissions] mode = "bypass"` in a project's config | Ignored until the project is [trusted](configuration.md#trust); your own `~/.azazel/config.toml` needs no trust |

`Shift+Tab` never reaches it. The status line shows `⚠ bypass` in the colour of errors, and the rules around the input take the same colour, for as long as the session is in it. Over [ACP](acp.md) the mode is offered to the editor only when azazel was started in it; an MCP client of `azazel mcp serve` cannot ask for it; an [RPC](rpc.md) client that sends `set_mode bypass` is expected to have asked its user.

### Switching

| How | Scope |
|---|---|
| `Shift+Tab` | Cycles `plan` → `manual` → `accept-edits` → `auto` in the TUI; from `bypass` it goes to `plan` |
| `/mode` | Picker; `/mode auto` sets it directly |
| `--mode <mode>` | This run; works with `-p`, `azazel goal` and `azazel rpc` |
| `[permissions] mode = "manual"` | Default for new sessions |
| `AZAZEL_PERMISSIONS__MODE=auto` | Environment override |

The mode belongs to the session and each change is recorded in the transcript. A change takes effect at once for everyone in the session: a sub-agent that is in the middle of its work is judged by the new mode from its next tool call on, and so is every agent started later. A background session started with `/bg` begins in the mode the session has at that moment; attached to one (`azazel attach`), `/mode` and `Shift+Tab` change the mode of the background session itself.

No agent has a mode of its own that lets it do more than the session. A definition can only narrow: an agent whose definition says `mode: plan` stays in plan mode whatever the session mode is (see [team](team.md)), `bypass` included.

## Rules

```toml
# <project>/.azazel/config.toml
[permissions]
mode = "manual"
allow = ["shell(cargo build:*)", "shell(cargo test:*)", "edit(src/**)", "web_fetch(domain:docs.rs)"]
ask   = ["shell(rm:*)", "write(*.lock)"]
deny  = ["read(.env)", "read(secrets/**)", "shell(curl:*)", "mcp__github__delete_*"]
additional_dirs = ["../shared-lib"]
```

A rule is `tool` or `tool(pattern)`. The tool part is a glob over tool names, so `mcp__github__*` covers every tool of one MCP server. A rule without a pattern covers every call of the tool. What the pattern matches depends on the tool:

| Tool | Pattern | Examples |
|---|---|---|
| `shell` | Ending in `:*`: a prefix, matching that command alone or followed by a space and anything. Otherwise a glob over the command. | `shell(git diff:*)`, `shell(npm run *)` |
| `read`, `write`, `edit`, `glob`, `grep`, `lsp`, `ast_grep` | Gitignore-style path. No `/`: matches at any depth in the project. With `/`: relative to the project root. Absolute and `~/` paths are allowed. Naming a directory covers everything below it. | `read(.env)`, `edit(src/**)`, `write(~/notes/**)` |
| `web_fetch`, `browser` | `domain:<host>` matches the host and its subdomains. `read` of a URL and `web_grep` are matched as `web_fetch`, and so is `read` of a `pr://` or `issue://` address (host `github.com`). For `browser` actions other than `open` and `navigate`, the pattern is a glob over the action name. | `web_fetch(domain:github.com)`, `browser(snapshot)` |
| Any other tool | A glob over the tool's subject, if it has one (for `screen`, the action name) | `screen`, `screen(capture)` |

The `allow`, `ask` and `deny` arrays concatenate across config layers (home, project, `config.local.toml`, environment). A project's `permissions.allow` is ignored until the project is trusted, and so is `mode = "bypass"`; its `ask` and `deny` always apply. See [configuration](configuration.md).

### Shell commands

A `shell` command line is parsed and split into its simple commands: through pipelines, `&&`/`;` lists, subshells, command substitutions, and scripts passed to `sh -c` or `eval`. Leading variable assignments and the wrappers `sudo`, `env`, `command`, `nohup`, `time` and `xargs` are stripped. Each simple command is matched on its own.

- A `deny` or `ask` rule applies when it matches **any** of the commands.
- An `allow` rule lets the call through only when **every** command is covered by some allow rule.

So `shell(cargo test:*)` allows `cargo test -p core` but not `cargo test; rm -rf target`, and `deny = ["shell(rm:*)"]` catches `ls && sudo rm -rf x`.

A command line that cannot be parsed, and any command when the resolved shell is PowerShell, is treated as one opaque string: deny rules and glob patterns still see it, but allow rules never auto-allow it.

### Programs in a terminal

A [program in a terminal](tools.md#programs-in-a-terminal) is started by a `shell` call (`interactive: true`), which is decided like every other `shell` call: the same rules, the same guards, the same prompt.

The `pty` tool then types into the program and stops it. There are no rules for `pty` to write: those for `shell` decide.

- `pty` `send` and `stop` are allowed where running the program is. They are matched against the command that runs in the terminal, as the `shell` call that started it was. With `allow = ["shell(npm init:*)"]` an agent starts `npm init` and answers its questions without a prompt; in `manual` mode without such a rule you are asked for each `send`, the prompt shows what would be typed, and `Allow this session` and `Always allow` add the rule for the program.
- What is typed is held to the limits no mode lifts, as if it were a command of its own, because the program may be a shell: a `deny` rule for `shell` that matches the text refuses the call, the push guard refuses `git push`, a [destructive command](#destructive-commands) and a command that names a [secret file](#secret-files) are asked about in every mode but `bypass`, and `destructive = "deny"` refuses the first. Keys (`enter`, `ctrl+c`, arrows) are not text and are not checked.
- `pty` `read` and `list` only look: they are reads.
- Text you type into a terminal yourself is yours and is never checked.

These checks read what the agent sends, not what the program does with it. Like the checks on `shell` they catch mistakes, and are no sandbox.

## How a call is decided

In this order; the first step that settles the call wins.

1. A `deny` rule matches: refused, with the rule and the file it came from.
2. The calling agent's definition restricts the tool to patterns (`tools: shell(git diff:*)`) and the call is outside them: refused.
3. The push guard matches: refused.
4. `[permissions] destructive = "deny"` and the call is a destructive command: refused.
5. [Hooks](hooks.md) and [extensions](extensions.md) see the call. They can block it, rewrite its input (the rewritten call goes through steps 1–4 again), or waive the prompt.
6. The agent is in `plan` mode and the call is not a read: refused, unless it writes a plan file or is `plan_submit`.
7. The session is in `bypass` mode: allowed.
8. The call writes to a protected path: it goes to step 13, whatever the mode, allow rules or a waived prompt say.
9. An `ask` rule matches: it goes to step 13, unless a hook waived the prompt, in which case it is allowed.
10. The mode decides, as in the table above.
11. Every subject of the call is covered by `allow` rules: allowed.
12. A hook waived the prompt: allowed.
13. `permission_request` hooks may settle it. Otherwise you are asked.

A [destructive command](#destructive-commands), and a call that names a [secret file](#secret-files), that steps 9 to 12 would allow goes to step 13 instead.

## Protected paths

File-writing tools ask before touching:

- anything inside a `.git` directory;
- `<project>/.azazel/config*.toml`;
- anything under `~/.azazel/`;
- shell profiles: `.bashrc`, `.bash_profile`, `.zshrc`, `.zprofile`, `.profile`, `profile.ps1`, `Microsoft.PowerShell_profile.ps1`.

These prompts have no "always allow" choice, and no allow rule or `auto` mode removes them. Only `bypass` does.

## Secret files

Files that hold credentials are asked about before an agent reads or writes one, in every mode but `bypass`, `auto` included. No allow rule and no hook waives the question, and it has no "always allow" choice: you allow the call once, or the file for the rest of the session.

| What | Paths |
|---|---|
| Environment files | `.env` and `.env.*` anywhere, except `.env.example`, `.env.sample` and `.env.template` |
| Keys | `*.pem`, `*.key`, `id_rsa*`, `id_ed25519*` anywhere |
| Under your home directory | `~/.ssh/**`, `~/.aws/**`, `~/.config/gcloud/**`, `~/.azure/**`, `~/.kube/config`, `~/.docker/config.json`, `~/.netrc`, `~/.npmrc`, `~/.pypirc`, `~/.git-credentials` |
| azazel's own | `~/.azazel/auth.json` (wherever `AZAZEL_HOME` puts it) and `~/.azazel/.env` |
| Yours | Every pattern in `permissions.secret_paths`, written like the path of a rule |

```toml
[permissions]
secret_paths = ["config/prod.yaml", "vault/", "*.kdbx", "~/.config/op/**"]
```

What counts as using one:

- `read`, `write`, `edit`, `lsp` and `ast_grep` on the file, and `grep` or `glob` whose `path` is the file or a directory of secrets (`~/.ssh`).
- A `shell` command with a word that names one: `cat .env`, `type .env`, `less ~/.ssh/id_ed25519`, `cp server.pem /tmp`, `scp deploy.key host:`, `base64 $HOME/.aws/credentials`, `curl -d @.env …`, `--data=@tls.key`, and redirections such as `grep KEY < .env`. Scripts given to `sh -c` are read too. `$HOME`, `${HOME}`, `%USERPROFILE%` and `$env:USERPROFILE` count as `~`; a web address names no file. A command line that cannot be parsed is split at white space and checked word by word.
- Each call a `run_code` script makes, like any other call.

A `grep` over a directory does not ask: it passes the secret files by and ends with `[2 secret files were not searched; name one as `path` to search it]`. Once you allow a file for the session, searches include it.

Tools of MCP servers are not covered: a file server reads what its own configuration lets it read. Use `deny` rules on its tools to bound it.

With nobody to ask (print mode, `azazel goal`) the call is refused:

```text
`.env` is a secret file; using it needs the user's confirmation and nobody is here to give it
```

`/forget-approvals` takes back the files allowed for the session. A command that both destroys work and names a secret file can only be allowed once.

## Destructive commands

Some commands destroy work that no checkpoint or `git` history brings back, or run code nobody has read. A `shell` call that contains one is asked about in every mode but `bypass`, `auto` included, and no allow rule or hook waives the question. The prompt names the risk and has no "always allow" choice.

| Command | Why |
|---|---|
| `rm -r` on `/`, `~`, `..`, or a path outside the project and its extra directories | Deletes a directory tree outside the project |
| `git reset --hard` | Discards uncommitted changes |
| `git clean -f` | Deletes untracked files |
| `git checkout -- .`, `git checkout .`, `git restore .` | Discards uncommitted changes |
| `git branch -D` (or `-d --force`) | Deletes a branch that is not merged |
| `git stash drop`, `git stash clear` | Deletes stashed changes |
| `dd of=/dev/...` | Writes over a device |
| `mkfs`, `mkfs.<type>` | Formats a file system |
| `chmod -R`, `chown -R` on `/` or `~` | Changes permissions or owners of everything |
| `terraform destroy`, `terraform apply -destroy` | Destroys managed infrastructure |
| `kubectl delete namespace` | Deletes a namespace and everything in it |
| `docker system prune -a` | Removes every unused image, container and network |
| `drop database` in a `psql` or `mysql` `-c`/`-e` argument | Drops a database |
| `curl`, `wget`, `iwr` or `irm` piped into `sh`, `bash`, `zsh`, `iex`, `pwsh`, `python` or another interpreter; `bash <(curl …)`, `sh -c "$(curl …)"`, `eval "$(curl …)"`, `iex (irm …)` | Runs what it downloads without anyone having read it |

The check looks at the parsed command words, so it finds these inside pipelines, `&&` lists, subshells and `sh -c` scripts, and it is not fooled by a command that merely mentions one (`echo git reset --hard`). `rm -r` inside the project, for example `rm -r target/tmp`, is ordinary work. A download that is saved (`curl -o install.sh …`) or read as data (`curl … | jq .`, `curl … | python -m json.tool`, `curl … | bash process.sh`) is ordinary too: only an interpreter that takes its program from the pipe counts. A command line that cannot be parsed is split roughly and checked the same way.

```toml
[permissions]
destructive = "ask"     # ask (default), allow or deny
```

| Value | Effect |
|---|---|
| `ask` | You are asked, whatever the mode and the allow rules say; `bypass` mode runs the command |
| `allow` | No extra question: the mode and rules decide as for any other command |
| `deny` | The call is refused and the model is told why, in `bypass` mode too |

With nobody to ask (print mode, `azazel goal`) `ask` refuses the call:

```text
this command discards uncommitted changes (git reset --hard); it needs the user's confirmation and nobody is here to give it (`[permissions] destructive = "allow"` runs such commands unattended)
```

Deny rules and plan mode still come first: a deny rule refuses the command outright, and plan mode refuses every command.

## The approval prompt

The prompt is titled `<agent> wants to use <tool>` and shows the commands, path or host involved.

| Choice | Effect |
|---|---|
| `Allow once` | This call only |
| `Allow this session` | Allows it, and everything the rule shown under `Always allow` covers, until azazel exits. Nothing is written to a file; the rule is listed by `/permissions` as a session rule and `/forget-approvals` takes it back |
| `Always allow <rule>` | Allows it and appends the rule to `permissions.allow` in `<project>/.azazel/config.local.toml` |
| `Deny` | The model is told the user denied the call |
| `Deny and say why` | As `Deny`, with your text passed to the model |

`Esc` denies. The selection starts on `Allow once`, except for a destructive command, where it starts on
`Deny` and neither `Allow this session` nor `Always allow` is offered. For a [secret file](#secret-files) `Allow this session` allows that file until azazel exits, and `Always allow` is not offered. The suggested rule is narrow: `shell(cargo test:*)` for a single command with a subcommand, `edit(src/**)` for a file's directory, `web_fetch(domain:<host>)` for a host, the bare tool name for other tools. When no single rule covers the call (a pipeline of several commands, for example) neither of the two choices that need a rule is offered.

Sub-agents use the same prompt; the roster shows `waiting for your approval` beside the agent until you answer.

`/permissions` lists the current mode and every rule in effect with its kind and source: this session, a config
file path, or `environment or command line`. In the list:

- `d` removes the selected rule when it was added in this session or remembered with "always allow" (it leaves
  `config.local.toml` too). A rule from another config file is shown with the file to edit.
- `a` adds a rule typed as `allow shell(cargo test:*)` (`allow`, `ask` or `deny`). An `allow` rule can be kept
  for this session or saved for the project; `ask` and `deny` rules added here last until azazel exits.
- `/permissions add <kind> <rule>` adds a session rule without opening the list.

`/forget-approvals` drops every remembered approval and every rule added in this session, and says how many.

## Agents never push

Publishing is left to you. Two layers enforce it, in every mode, `bypass` included, and no rule or hook can lift them.

**Command analysis.** A `shell` call is refused when any of its commands is:

- `git push` or `git send-pack` (with or without options such as `-C dir` before the subcommand);
- `jj git push`, `hg push`;
- `gh pr create`, `gh pr merge`, `gh release create`, or `gh repo create --push`.

A command line that could not be parsed is refused when it contains `git … push` or `git … send-pack` within one command. The model receives:

```text
agents never push. Ask the user to push (they can type "!git push").
```

**Environment.** Every process started on an agent's behalf (the `shell` tool, command hooks, verify commands, `azazel.exec` in Luau extensions) runs with `GIT_TERMINAL_PROMPT=0` and extra git configuration, passed through `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_<n>` / `GIT_CONFIG_VALUE_<n>`, that rewrites the push URL of `https://`, `http://`, `ssh://`, `git://`, `git@` and `file://` remotes to `azazel-push-blocked://denied/`. A push started indirectly, by a script or a package manager, therefore fails at the transport with `azazel-push-blocked` in git's error. Fetching and pulling are not affected.

Commands you type yourself with the `!` prefix in the TUI (`!git push`) run in your shell outside the gate and without this environment.

## Plan mode

In `plan` mode the agent is told to investigate with read-only tools, write a plan to `.azazel/plans/<short-name>.md` (what changes, in which files, in what order, how it is verified), and call the `plan_submit` tool with that path. Anything else that is not a read returns:

```text
plan mode: only read-only tools are available. Write the plan to a file under .azazel/plans/ and call plan_submit.
```

`plan_submit` shows the plan in the transcript and asks:

| Choice | Effect |
|---|---|
| `Approve and run` | A session in `plan` mode goes to `accept-edits`; the agent is told `approved; implement the plan`. A session in another mode keeps it: the plan came from an agent whose definition keeps it in plan mode |
| `Revise` | You type feedback; the agent revises the plan and submits again |

Closing the review without choosing leaves the mode unchanged and tells the agent to ask what you want changed.

```sh
azazel --mode plan            # start a session in plan mode
```

## Headless behaviour

In print mode (`-p`) and `azazel goal` nobody can answer a prompt. A call that would ask is refused instead, and the model sees:

```text
permission required for <tool>: re-run with --mode auto or add an allow rule
```

Choose the mode and rules up front:

```sh
azazel -p "fix the failing test" --mode accept-edits
azazel goal "make cargo test pass" --mode auto
```

`auto` still refuses what a guard would ask about, since nobody is there to answer: a write to a protected path, a secret file, a destructive command. `--mode bypass` runs those too.

A project's own `permissions.allow` counts only if the project is trusted; pass `--trust` for a run in a project you have not trusted interactively. `plan_submit` without a user returns the plan's path and tells the agent to stop and summarise the plan.

`azazel rpc` is not headless in this sense: prompts are sent to the client as `approval_request` events and answered with `approval_response` (`allow`, `session`, `always`, `deny`). See [rpc](rpc.md).

## Secrets in tool results

```toml
[tools]
redact_output = true    # default false
```

With this on, the text of every tool result passes the same detector that hides secrets in what the terminal UI shows (`[tui] redact`, see [tui](tui.md)) before the model and the session file get it: API keys, tokens, `KEY=value` assignments and private key blocks are replaced by `[redacted:<kind>]`. The diff kept with an edit is cleaned the same way, and the tool card says `2 secrets redacted`. The file on disk is not changed, and the model works with the placeholders: it cannot copy a value it never saw. Leave it off when the agent has to handle the values themselves.
