# Packages

A package is a directory, usually a git repository, that bundles things Azazel can load: skills, agent definitions, slash commands, Luau extensions and MCP servers. `azazel pkg` installs packages from git, from a local directory or by a short registry name, and `azazel skills` is a shortcut for the common case of a package of skills. Packages are per user: they live under `~/.azazel` and apply to every project.

## Commands

```bash
azazel pkg add github:acme/agent-pack          # a GitHub repository
azazel pkg add github:acme/agent-pack@v2       # a branch or tag
azazel pkg add https://example.com/team/pack.git --name team
azazel pkg add ./my-pack                       # a local directory, used in place
azazel pkg add taste                           # a registry name
azazel pkg list
azazel pkg update                              # all packages
azazel pkg update team
azazel pkg remove team
```

| Command | Does |
|---|---|
| `azazel pkg add <source> [--name <name>]` | Installs a package and prints what it contains |
| `azazel pkg list` | Name, source, commit (or `local`), whether code is enabled, install time. With nothing installed it prints the registry names |
| `azazel pkg update [name]` | Fetches the newest commit of the branch or tag the package was installed from; all packages when no name is given |
| `azazel pkg remove <name>` | Forgets the package and deletes its clone |

Changes take effect in the next session. `azazel doctor` lists installed packages.

## Sources

A source is tried as the first of these that fits:

| Form | Meaning |
|---|---|
| `github:<owner>/<repo>[@ref]` | `https://github.com/<owner>/<repo>`, optionally at a branch or tag |
| A git URL (contains `://` or starts with `git@`) | Cloned as given |
| A registry name | The address from the built-in registry, below |
| A directory, relative to the current directory | Used where it is: never copied, and never deleted by `remove` |

Git sources are cloned with `git clone --depth 1` (plus `--branch <ref>`) into `~/.azazel/packages/<name>`, so `git` must be installed and able to clone the address without asking for credentials. If a directory has the same name as a registry entry, write it as `./name`.

The package name defaults to the last part of the address (without `.git`, lower-cased, with anything but letters, digits, `-`, `_` and `.` turned into `-`). Use `--name` to choose another; two packages cannot share a name.

When a cloned repository has no `LICENSE`, `LICENCE` or `COPYING` file at its root, the install prints `note: <source> has no licence file; installed for your local use only`.

## What a package contains

Azazel uses the first description of the package that exists:

| Order | Found | Contributes |
|---|---|---|
| 1 | `azazel.toml` at the root | What the manifest lists |
| 2 | `.claude-plugin/marketplace.json` | The `skills` paths of each plugin whose `source` is a path in the repository |
| 3 | `.claude-plugin/plugin.json` | Its `skills` paths |
| 4 | `.codex-plugin/plugin.json` | Its `skills` paths |
| 5 | Nothing of the above | The directories `skills/`, `agents/` and `commands/`, and a `SKILL.md` at the root (the package is then one skill) |

The plugin manifests (2 to 4) contribute skills only.

### `azazel.toml`

```toml
skills = ["skills"]              # directories of skill folders, or single skill folders
agents = ["agents"]              # directories of agent definitions
commands = ["commands"]          # directories of command files
extensions = ["ext/main.luau"]   # Luau script files
mcp = "mcp.json"                 # an mcp.json with the package's servers
```

Every key is optional. Paths are relative to the package root and must stay inside it; entries that do not exist are skipped.

A package laid out for the manifest above:

```text
my-pack/
  azazel.toml
  skills/release-notes/SKILL.md
  agents/migrator.md
  commands/changes.md
  ext/main.luau
  mcp.json
```

The file formats are those of the user's own directories: [skills and commands](skills-and-commands.md), agent definitions in [team](team.md), [extensions](extensions.md), [mcp](mcp.md). While developing a package, install the working directory itself (`azazel pkg add .`); edits are picked up by the next session.

## Code and `code_enabled`

Skills, agent definitions and commands of a package always load: they are text. Three things can run code on your machine, and they load only when you enabled the package's code:

- Luau extension scripts
- MCP servers from the package's `mcp.json`
- hooks carried by the package's skills

When a package contains any of these, `pkg add` says which and asks once:

```text
It contains 1 extension script(s) and MCP servers.
This package can run code on your machine. Enable its code? [y/N]
```

The answer is stored as `code_enabled` and stays in force across `pkg update`, so an update of a package you enabled can bring new code. To change the answer, remove the package and add it again, or edit `code_enabled` in `~/.azazel/packages.toml`. A package without code is never asked about.

## Precedence

Package content sits above the built-ins and below your own files:

| Kind | Order, lowest first |
|---|---|
| Skills | built-in, packages (in install order), `~/.claude/skills` and `~/.agents/skills`, `~/.azazel/skills`, the project's |
| Agent definitions | built-in, packages, `~/.azazel/agents`, the project's |
| Commands | packages, `~/.azazel/commands`, the project's |
| Luau scripts | packages (id `<package>-<file name>`), `~/.azazel/extensions`, the project's |
| MCP servers | packages, `~/.azazel/mcp.json`, the project's files |

A later entry with the same name replaces an earlier one.

## The registry

The registry is a short list of names built into the binary. It holds names and addresses only; nothing from these repositories ships with Azazel.

| Name | Source | Note |
|---|---|---|
| `taste` | `https://github.com/Leonxlnx/taste-skill` | |
| `components` | `https://github.com/vercel/components.build` | |
| `design-styles` | `https://github.com/bergside/awesome-design-skills` | Installed with `model_invocable = []`: none of its many style skills is offered to the model until you add it by name |

Anything not in the registry installs by URL or `github:` form.

## `azazel skills`

```bash
azazel skills add taste                      # same as: azazel pkg add taste
azazel skills add github:acme/skill-pack
azazel skills add design-styles/<style>      # offer one skill of a package to the model
azazel skills list
azazel skills remove taste
```

| Command | Does |
|---|---|
| `azazel skills add <source>` | Installs a package, exactly like `pkg add` without `--name` |
| `azazel skills add <package>/<skill>` | `<package>` is a registry name or an installed package. Installs the package if needed, then adds `<skill>` to its `model_invocable` list |
| `azazel skills list` | Every skill a session in this project would have, with its source, and every invalid `SKILL.md` with its error |
| `azazel skills remove <name>` | Uninstalls the package of that name (the whole package, not one skill) |

### `model_invocable`

Each installed package may carry a `model_invocable` list:

| Value | Effect |
|---|---|
| absent | Every skill of the package is listed for the model |
| `["a", "b"]` | Only those skills are listed for the model |
| `[]` | None is listed |

Skills that are not listed stay available to you as `/skill:<name>`. This keeps a package with dozens of skills from crowding the list the model chooses from. A skill's own `disable-model-invocation: true` still hides it either way.

## `packages.toml`

`~/.azazel/packages.toml` records what is installed. You rarely need to edit it.

```toml
[[package]]
name = "design-styles"
source = "https://github.com/bergside/awesome-design-skills"
commit = "0123456789abcdef0123456789abcdef01234567"
installed_at = "2026-11-11T09:30:00Z"
code_enabled = false
model_invocable = []

[[package]]
name = "my-pack"
source = "/home/me/src/my-pack"
installed_at = "2026-11-11T09:31:00Z"
code_enabled = true
local = true
```

| Key | Meaning |
|---|---|
| `name` | The package name |
| `source` | The git URL, or the directory of a local package |
| `ref` | The branch or tag given with `@ref`, if any |
| `commit` | The commit installed (git packages) |
| `installed_at` | UTC time of the install |
| `code_enabled` | Whether its extensions, MCP servers and skill hooks load |
| `model_invocable` | See above |
| `local` | `true` when the package is the directory `source` rather than a clone under `~/.azazel/packages/` |
