# Permissions

Every tool call an agent makes passes one gate before it runs. The gate combines a session-wide permission mode, allow/ask/deny rules from config, a fixed list of protected paths, a list of destructive commands, a guard that stops agents from pushing, and your answer when it has to ask. There is no OS-level sandbox: containment is this gate, [checkpoints](sessions-and-checkpoints.md) and [worktree isolation](team.md#worktree-isolation).

## Modes

| Mode | Runs without asking | Asks | Refuses |
|---|---|---|---|
| `plan` | Read-only tools; `write`/`edit` under `.azazel/plans/`; `plan_submit` | nothing | Every other write, command and network call |
| `ask` (default) | Read-only tools on paths inside the project | Everything else | |
| `accept-edits` | As `ask`, plus file writes inside the project | Commands, network, writes outside the project | |
| `auto` | Everything | Only what an `ask` rule, a protected path or a destructive command forces | |

"Inside the project" means under the project root or one of `permissions.additional_dirs`, plus directories added
with `--add-dir <DIR>` or, for the rest of the run, `/add-dir <path>` (`/dirs` lists them all; Luau extensions
see only the configured ones). A read outside those roots asks in `ask` and `accept-edits`. Commands are `shell`, `browser`, `screen` and MCP tools that are not marked read-only; network calls are `web_search`, `web_fetch`, `web_grep`, `read` of a URL or of a `pr://` or `issue://` address, and the `generate_*` tools. `run_code` itself counts as a read; each tool call its script makes is judged like a call the agent makes directly.

Deny rules, the push guard, protected paths and the destructive-command check apply in every mode, `auto` included.

### Switching

| How | Scope |
|---|---|
| `Shift+Tab` | Cycles `plan` → `ask` → `accept-edits` → `auto` in the TUI |
| `/mode` | Picker; `/mode auto` sets it directly |
| `--mode <mode>` | This run; works with `-p`, `azazel goal` and `azazel rpc` |
| `[permissions] mode = "ask"` | Default for new sessions |
| `AZAZEL_PERMISSIONS__MODE=auto` | Environment override |

The mode belongs to the session and each change is recorded in the transcript. An agent whose definition says `mode: plan` stays in plan mode whatever the session mode is (see [team](team.md)).

## Rules

```toml
# <project>/.azazel/config.toml
[permissions]
mode = "ask"
allow = ["shell(cargo build:*)", "shell(cargo test:*)", "edit(src/**)", "web_fetch(domain:docs.rs)"]
ask   = ["shell(rm:*)", "write(*.lock)"]
deny  = ["read(.env)", "read(secrets/**)", "shell(curl:*)", "mcp__github__delete_*"]
additional_dirs = ["../shared-lib"]
```

A rule is `tool` or `tool(pattern)`. The tool part is a glob over tool names, so `mcp__github__*` covers every tool of one MCP server. A rule without a pattern covers every call of the tool. What the pattern matches depends on the tool:

| Tool | Pattern | Examples |
|---|---|---|
| `shell` | Ending in `:*`: a prefix, matching that command alone or followed by a space and anything. Otherwise a glob over the command. | `shell(git diff:*)`, `shell(npm run *)` |
| `read`, `write`, `edit`, `glob`, `grep`, `lsp` | Gitignore-style path. No `/`: matches at any depth in the project. With `/`: relative to the project root. Absolute and `~/` paths are allowed. Naming a directory covers everything below it. | `read(.env)`, `edit(src/**)`, `write(~/notes/**)` |
| `web_fetch`, `browser` | `domain:<host>` matches the host and its subdomains. `read` of a URL and `web_grep` are matched as `web_fetch`, and so is `read` of a `pr://` or `issue://` address (host `github.com`). For `browser` actions other than `open` and `navigate`, the pattern is a glob over the action name. | `web_fetch(domain:github.com)`, `browser(snapshot)` |
| Any other tool | A glob over the tool's subject, if it has one (for `screen`, the action name) | `screen`, `screen(capture)` |

The `allow`, `ask` and `deny` arrays concatenate across config layers (home, project, `config.local.toml`, environment). A project's `permissions.allow` is ignored until the project is trusted; its `ask` and `deny` always apply. See [configuration](configuration.md).

### Shell commands

A `shell` command line is parsed and split into its simple commands: through pipelines, `&&`/`;` lists, subshells, command substitutions, and scripts passed to `sh -c` or `eval`. Leading variable assignments and the wrappers `sudo`, `env`, `command`, `nohup`, `time` and `xargs` are stripped. Each simple command is matched on its own.

- A `deny` or `ask` rule applies when it matches **any** of the commands.
- An `allow` rule lets the call through only when **every** command is covered by some allow rule.

So `shell(cargo test:*)` allows `cargo test -p core` but not `cargo test; rm -rf target`, and `deny = ["shell(rm:*)"]` catches `ls && sudo rm -rf x`.

A command line that cannot be parsed, and any command when the resolved shell is PowerShell, is treated as one opaque string: deny rules and glob patterns still see it, but allow rules never auto-allow it.

## How a call is decided

In this order; the first step that settles the call wins.

1. A `deny` rule matches: refused, with the rule and the file it came from.
2. The calling agent's definition restricts the tool to patterns (`tools: shell(git diff:*)`) and the call is outside them: refused.
3. The push guard matches: refused.
4. [Hooks](hooks.md) and [extensions](extensions.md) see the call. They can block it, rewrite its input (the rewritten call goes through steps 1–3 again), or waive the prompt.
5. The call writes to a protected path: it goes to step 10, whatever the mode, allow rules or a waived prompt say.
6. An `ask` rule matches: it goes to step 10, unless a hook waived the prompt, in which case it is allowed.
7. The mode decides, as in the table above. `plan` refuses here.
8. Every subject of the call is covered by `allow` rules: allowed.
9. A hook waived the prompt: allowed.
10. `permission_request` hooks may settle it. Otherwise you are asked.

A [destructive command](#destructive-commands) that steps 6 to 9 would allow goes to step 10 instead.

## Protected paths

File-writing tools always ask before touching:

- anything inside a `.git` directory;
- `<project>/.azazel/config*.toml`;
- anything under `~/.azazel/`;
- shell profiles: `.bashrc`, `.bash_profile`, `.zshrc`, `.zprofile`, `.profile`, `profile.ps1`, `Microsoft.PowerShell_profile.ps1`.

These prompts have no "always allow" choice, and no allow rule or `auto` mode removes them.

## Destructive commands

Some commands destroy work that no checkpoint or `git` history brings back. A `shell` call that contains one is asked about in every mode, `auto` included, and no allow rule or hook waives the question. The prompt names the risk and has no "always allow" choice.

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

The check looks at the parsed command words, so it finds these inside pipelines, `&&` lists, subshells and `sh -c` scripts, and it is not fooled by a command that merely mentions one (`echo git reset --hard`). `rm -r` inside the project, for example `rm -r target/tmp`, is ordinary work. A command line that cannot be parsed is split roughly and checked the same way.

```toml
[permissions]
destructive = "ask"     # ask (default), allow or deny
```

| Value | Effect |
|---|---|
| `ask` | You are asked, whatever the mode and the allow rules say |
| `allow` | No extra question: the mode and rules decide as for any other command |
| `deny` | The call is refused and the model is told why |

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
| `Always allow <rule>` | Allows it and appends the rule to `permissions.allow` in `<project>/.azazel/config.local.toml` |
| `Deny` | The model is told the user denied the call |
| `Deny and say what to do instead` | As `Deny`, with your text passed to the model |

`Esc` denies. The selection starts on `Allow once`, except for a destructive command, where it starts on
`Deny` and `Always allow` is not offered. The suggested rule is narrow: `shell(cargo test:*)` for a single command with a subcommand, `edit(src/**)` for a file's directory, `web_fetch(domain:<host>)` for a host, the bare tool name for other tools. When no single rule covers the call (a pipeline of several commands, for example) the choice is not offered.

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

Publishing is left to you. Two layers enforce it, in every mode, and no rule or hook can lift them.

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
| `Approve and run` | The session mode becomes `accept-edits`; the agent is told `approved; implement the plan` |
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

A project's own `permissions.allow` counts only if the project is trusted; pass `--trust` for a run in a project you have not trusted interactively. `plan_submit` without a user returns the plan's path and tells the agent to stop and summarise the plan.

`azazel rpc` is not headless in this sense: prompts are sent to the client as `approval_request` events and answered with `approval_response` (`allow`, `always`, `deny`). See [rpc](rpc.md).
