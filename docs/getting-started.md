# Getting started

Azazel is a coding-agent harness for the terminal: one binary, `azazel`, that runs a lead agent and a team of
specialists against the project in your current directory. This page covers installing it, setting up a model,
sending a first prompt, running it without the interactive UI, and removing it again.

## Install

Linux and macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/sinnafuls/azazel-cli/main/install/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/sinnafuls/azazel-cli/main/install/install.ps1 | iex
```

Both scripts download the latest GitHub release for your platform and verify its SHA-256 before installing.
Releases are built for Windows (x86_64), Linux (x86_64 and ARM64) and macOS (Apple silicon and Intel); on
anything else, install [from source](#from-source).

| Platform | Installed to | PATH |
|---|---|---|
| Linux, macOS | `~/.local/bin` | added through `~/.profile`, `~/.bashrc`, `~/.zshrc` |
| Windows | `%LOCALAPPDATA%\Programs\azazel` | added to the user PATH |

Open a new terminal afterwards so the PATH change takes effect, then check:

```sh
azazel --version
```

### From source

```sh
git clone https://github.com/sinnafuls/azazel-cli
cd azazel-cli
./install/install.sh          # Linux, macOS
.\install\install.ps1         # Windows
```

The scripts build with cargo and install the result the same way as above. To let cargo install it instead:

```sh
cargo install --path crates/azazel-cli
```

### Updating

```sh
azazel update            # install the newest release
azazel update --check    # only report whether one exists
```

The interactive UI also checks GitHub in the background, at most once a day, and shows a notice when a newer
version exists. Turn that off with `[update] check = false` in [configuration](configuration.md) or with the
environment variable `AZAZEL_NO_UPDATE_CHECK=1`.

With `[update] auto = true` the background check also downloads a newer release, verifies it against its
published SHA-256 and keeps it in `~/.azazel/cache/update/<version>/`. When you leave the UI, azazel installs it
the way `azazel update` does and prints one line (`azazel updated 0.2.0 -> 0.3.0; the next start runs it`). A
download that does not match its checksum is thrown away. Print mode, RPC mode and the subcommands never
install anything on their own, and nothing is downloaded when the release cannot be reached. A GitHub token in
`GITHUB_TOKEN` or `GH_TOKEN` is used when set, which lifts GitHub's limit on anonymous requests.

The first time the UI starts after an update it says so, with the number of changes. `azazel changelog` (or
`/changelog`) shows what changed in each release; `azazel changelog --since 0.2.0` shows only what is newer.

## First run

Start it inside a project:

```sh
cd my-project
azazel
```

Azazel needs a model before it can answer. When none is usable, the UI opens `/settings` straight away, on
its Accounts page:

| Row | What it does |
|---|---|
| Sign in with a subscription | Signs in with a Claude or ChatGPT account, or another account a provider signs in with |
| Add an API key | Stores a provider key in `~/.azazel/auth.json` |
| Sign out or forget a key | Removes a stored login or key (shown once something is stored) |

After the first sign-in or key, it goes straight on to choosing a default model, which is saved as
`roles.default` in `~/.azazel/config.toml`; the Models page (`Tab`) changes it later. Come back any time with
`/settings`. Providers, roles and fallback chains are described in [models and logins](models-and-logins.md).

Two commands do the same from the shell:

```sh
azazel init      # pick the default model; in a project without one, write .azazel/config.toml
azazel doctor    # report what was found: config files, credentials, roles, shell, git, checkpoints
```

`azazel init` offers the providers that are ready: those with a key in the environment (for example
`ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or `GEMINI_API_KEY`) or in `auth.json`, and local servers such as Ollama
that answer. `azazel doctor` exits with code 1 when it finds an error.

Coming from pi, omp, jcode, Claude Code or Codex CLI? `azazel migrate --dry-run` shows what `azazel migrate`
would bring over: models and roles, API keys, skills, commands, MCP servers, settings, conversations and
memory. See [migrating](migrating.md).

To try a model without saving anything:

```sh
azazel --model <provider>/<model-id>
```

If the project contains configuration that can run commands (hooks, extensions, MCP servers and similar), azazel
asks once whether you trust it. See [trust](configuration.md#trust).

## A first prompt

Type a request and press `Enter`:

```text
› what does this project do? @README.md
```

- `@path` attaches a file, `!command` runs a command in your own shell, `/` starts a slash command.
- The reply streams in; finished output scrolls into your terminal's normal scrollback.
- In the default `manual` mode, edits and shell commands need your approval. The prompt offers `Allow once`,
  `Allow this session`, `Always allow <rule>`, `Deny` and `Deny and say why`. `Shift+Tab` switches the mode; see
  [permissions](permissions.md).
- `Enter` while the agent is working steers it; `Esc` interrupts it.
- `/rewind` puts files, conversation or both back to before an earlier prompt; see
  [sessions and checkpoints](sessions-and-checkpoints.md).
- `Ctrl+C` twice, `Ctrl+D` on an empty line, or `/quit` exits.

All keys and commands are listed in [the terminal UI](tui.md). To continue where you left off next time, run
`azazel -c`.

Words after `azazel` are sent as the first message: `azazel fix the failing test` opens the UI and starts on it.
Quote the prompt or put `--` before it when it contains something that looks like a flag or starts with a
subcommand's name (`azazel -- init the repository`).

## Print mode

`-p` runs the prompt without the UI and prints the final answer. The prompt is every word after the flags, quoted
or not. Anything piped on standard input is appended to it; with no prompt, the piped text is the prompt.
`@path` works here too: text files are inlined and images (up to 8) are attached.

```sh
azazel -p summarise the changes in the last commit
azazel -p "what is wrong in @screenshot.png"
git diff | azazel -p "review this diff"
azazel -p "fix the failing test" --mode auto
azazel -p "list the TODO comments" --output json
```

| Flag | Effect |
|---|---|
| `--output text` | Print the final assistant text (default) |
| `--output json` | Print one JSON event per line, each tagged with the `agent` that emitted it |
| `--mode <mode>` | Permission mode for the run: `plan`, `manual`, `accept-edits`, `auto`, `bypass` |
| `--trust` | Trust the project's configuration for this run |
| `--max-cost <USD>` | Stop once the run has spent this much |
| `--json-schema <FILE\|JSON>` | Print only JSON that matches this schema; exit 1 when the run produced none (see [typed results](team.md#typed-results)) |
| `--tools`, `--system-prompt`, `--bare`, ... | See [command-line flags](configuration.md#command-line-flags) |

Nobody is there to approve a tool call in print mode, so a call that would prompt is denied with
`permission required for <tool>: re-run with --mode auto or add an allow rule`. A project that needs trust and
has no recorded decision runs untrusted unless you pass `--trust`.

| Exit code | Meaning |
|---|---|
| 0 | The agent finished |
| 1 | The run failed (for example no model: `no model configured: run "azazel init" or pass --model provider/id`) |
| 2 | Usage error: a missing prompt, an unknown tool, an ambiguous model name |
| 130 | Interrupted with `Ctrl+C` |

To leave a prompt running after you close the terminal, use `azazel bg "<prompt>"`; `azazel attach <id>`
opens it again, and `azazel --ssh <host>` works on another machine. See
[background sessions and remote work](background-and-remote.md).

For driving azazel from another program, see [RPC mode](rpc.md). For runs that only end when the work is
verified, see [goals](goals.md).

## Tab completion

`azazel completions <bash|zsh|fish|powershell|elvish>` prints a completion script. To load it in every new shell:

| Shell | Add to |
|---|---|
| bash | `eval "$(azazel completions bash)"` in `~/.bashrc` |
| zsh | `eval "$(azazel completions zsh)"` in `~/.zshrc` |
| fish | `azazel completions fish \| source` in `~/.config/fish/config.fish` |
| PowerShell | `azazel completions powershell \| Out-String \| Invoke-Expression` in `$PROFILE` |

## Where things are stored

Settings, logins, sessions and logs live in `~/.azazel` (set `AZAZEL_HOME` to move it). Per-project files live in
`<project>/.azazel`. Logs go to `~/.azazel/logs/azazel.log`; add `--debug` for debug-level logs.

## Uninstall

Linux and macOS:

```sh
~/.local/share/azazel/install.sh --uninstall
```

Windows (PowerShell):

```powershell
& "$env:LOCALAPPDATA\Programs\azazel\install.ps1" -Uninstall
```

Add `--purge` (Unix) or `-Purge` (Windows) to also delete `~/.azazel`, which holds your settings, logins and
sessions.
