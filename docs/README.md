# azazel documentation

New here? Read [getting started](getting-started.md) first.

## Using azazel

| Page | What it covers |
|---|---|
| [Getting started](getting-started.md) | Install, first run, a first prompt, print mode, updating, uninstalling |
| [The terminal UI](tui.md) | Layout, keys, input prefixes, every slash command, themes |
| [Configuration](configuration.md) | Config files, load order, every setting, trace export, trust, command-line flags, subcommands |
| [Models and logins](models-and-logins.md) | Providers, roles and fallback chains, thinking levels, API keys, subscription and cloud sign-in |
| [Permissions](permissions.md) | Permission modes, rules, the approval prompt, destructive commands, the git push guard, plan mode |
| [Sessions and checkpoints](sessions-and-checkpoints.md) | Resuming and forking sessions, exporting and sharing, bug reports, compaction, `/rewind` |
| [Migrating from another harness](migrating.md) | `azazel migrate` and `/migrate`: models, keys, skills, commands, MCP servers, settings, conversations and memory from pi, omp, jcode, Claude Code and Codex CLI |
| [Background sessions, schedules and remote work](background-and-remote.md) | `azazel bg`, `ps`, `logs`, `attach`, `kill`; `azazel schedule` and triggers; working on another machine with `--ssh` |

## Working as a team

| Page | What it covers |
|---|---|
| [The team](team.md) | Built-in agents, writing your own, parallel tasks, messaging and stopping agents, worktrees |
| [Goals](goals.md) | `/goal` and `azazel goal`, the checker, verify commands, the verifier agent |

## Capabilities

| Page | What it covers |
|---|---|
| [Tools](tools.md) | Every tool an agent can call |
| [Memory](memory.md) | Project memory, recall, the code index, document collections |
| [Web and browser](web-and-browser.md) | Web search backends, web fetch, driving Chromium |
| [Screen and playtesting](screen-and-playtesting.md) | Screen capture and input, the playtester agent |
| [Media and sprites](media-and-sprites.md) | Image, video, 3D and audio generation; sprite sheets for game engines |
| [Language servers](lsp.md) | Diagnostics after edits, definitions, references, renames |
| [Voice](voice.md) | Dictating prompts, spoken replies, speech services and recorder programs |

## Extending azazel

| Page | What it covers |
|---|---|
| [MCP](mcp.md) | Connecting MCP servers; serving azazel to another MCP client |
| [Hooks](hooks.md) | Running commands on tool calls and other events |
| [Rules](rules.md) | Rule files that apply always, by path or on request; stream rules; other tools' rule files |
| [Extensions](extensions.md) | Luau scripts and Rust extensions |
| [Skills and commands](skills-and-commands.md) | Skills, built-in skills, custom slash commands |
| [Packages](packages.md) | Installing bundles of skills, agents, commands and extensions |
| [RPC mode](rpc.md) | Driving azazel from another program over JSON lines; the protocol and client crates |
| [Agent Client Protocol](acp.md) | Using azazel as the agent of an editor that speaks ACP |
