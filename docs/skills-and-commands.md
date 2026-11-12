# Skills and custom commands

A **skill** is a pack of instructions the model loads only when a task calls for it: how to review code, how to debug, how your team writes release notes. The model sees one line per skill and reads the full text on demand, so many skills cost little context. A **custom command** is a Markdown prompt template you start yourself with `/name`. Both are plain files; neither runs code (except the optional hooks and `` !`cmd` `` lines described below).

## Skills

A skill is a directory holding a `SKILL.md`: YAML frontmatter, then the instructions in Markdown. Supporting files sit beside it and are linked with relative links.

```text
~/.azazel/skills/release-notes/
  SKILL.md
  references/format.md
```

```markdown
---
name: release-notes
description: Use when writing release notes for this repository. Explains the house format and where the data comes from.
---
Write the notes for $ARGUMENTS.

1. Collect the merged changes with `git log --oneline <last tag>..HEAD`.
2. Group them as described in [the format](references/format.md).
```

### Validation

A skill that breaks one of these rules is skipped, never loaded half-way, and reported with the reason by `/skills`, `azazel skills list` and `azazel doctor`.

- The file starts with a `---` line and the frontmatter ends with another.
- The frontmatter is strict YAML: `key: value` pairs with string keys, each key once.
- `name` is lower-case words (letters and digits) joined by single hyphens, at most 64 characters. The frontmatter `name`, not the folder name, is the skill's identity.
- `description` is not empty and at most 1024 characters. The model chooses skills by this line, so say when to use the skill.

### Frontmatter keys

| Key | Default | Meaning |
|---|---|---|
| `name` | required | The skill's name |
| `description` | required | What it is for and when to use it |
| `disable-model-invocation` | `false` | `true` leaves the skill out of the list the model sees; you can still run `/skill:<name>` |
| `user-invocable` | `true` | `false` hides `/skill:<name>` |
| `context` | `inline` | `inline` returns the instructions to the agent that asked. `fork` runs them in a sub-agent and returns its report |
| `agent` | `coder` | The agent definition used for `fork`; see [team](team.md) |
| `model` | none | A model, role or fallback chain (`"@slow"`, `["@slow", "@default"]`). When you start the skill with `/skill:<name>`, that prompt runs on it |
| `hooks` | none | Command hooks active from the skill's first use until the session ends; see [hooks](hooks.md) |
| `allowed-tools` | none | Permission rules, as in [permissions](permissions.md): `read, shell(git diff:*)`. What they do depends on `context`; see below |

`when_to_use` and `argument-hint` are read without error but nothing acts on them in this version. Any other key is ignored.

`allowed-tools` works as it does in Claude Code, and Claude Code's tool names (`Read`, `Edit`, `Bash(git diff:*)`, ...) are understood:

- **`context: fork`:** the list is the sub-agent's tool grant. It gets only the tools the list names, and a rule with a pattern holds its tool to that pattern. The list narrows what the agent definition grants and never adds to it: `allowed-tools: read` on an agent that may read and write gives an agent that can only read.
- **`context: inline`** (the default): each rule is an allow rule from the moment the skill is loaded until the run ends, so the agent is not asked about those calls. The rules end when the agent that loaded the skill stops, when a sub-agent finishes, and at your next prompt. `/permissions` shows them with the source `skill <name>, while <agent> works`. Deny rules, plan mode, protected paths and the push guard still apply.

An entry that is not a rule (`tool` or `tool(pattern)`) is left out with a warning in the log. A list in which nothing is a rule allows nothing, so a mistyped limit on a `fork` skill gives its agent no tools rather than all of them.

An inline skill's rules widen what runs without asking, so they count for built-in, user and package skills always, and for a project's own skills (also those under its `.claude/skills` and `.agents/skills`) only when the project is trusted.

With `context: fork`, an agent that may not start the named agent (for example a sub-agent without `spawns`) follows the instructions itself instead.

### Where skills come from

Roots are read in this order. A skill in a later root replaces one with the same `name` from an earlier root.

| Order | Root | Shown as |
|---|---|---|
| 1 | Built-in skills, shipped in the binary | `built-in` |
| 2 | Installed [packages](packages.md), in install order | `package <name>` |
| 3 | `~/.claude/skills`, `~/.agents/skills` | `compat` |
| 4 | `~/.azazel/skills` | `home` |
| 5 | `<project>/.claude/skills`, `<project>/.agents/skills` | `compat` |
| 6 | `<project>/.azazel/skills` | `project` |

Each skill is `<root>/<directory>/SKILL.md`. The `compat` roots are read only while `[compat] claude = true` (the default; see [configuration](configuration.md)). A project's skills load whether or not the project is trusted; only the hooks they carry need trust.

### How skills are used

- **By the model.** The system prompt lists every model-invocable skill as `- <name>: <description>`, sorted by name. If the list grows past 8000 characters, each description is cut to 200. The model loads a skill with the `skill` tool:

  | Input | Meaning |
  |---|---|
  | `name` | The skill |
  | `args?` | Text that replaces `$ARGUMENTS` in the body. When the body has no `$ARGUMENTS`, it is appended as `Arguments: <args>` |

  The result is `Skill directory: <absolute path>` followed by the body, so relative references resolve.
- **By you.** `/skill:<name> [args]` sends the same text to the lead as your prompt.
- **Reference files.** `read skill://<name>` returns the body; `read skill://<name>/<relative path>` returns a file beside `SKILL.md` (at most 256 KB, never outside the skill's directory).

| Command | Does |
|---|---|
| `/skills` | Lists every skill with its source and description, marks those that are `manual` (hidden from the model), `model only` or `disabled`, and shows each invalid `SKILL.md` with its error |
| `azazel skills list` | The same list, from the command line |
| `azazel skills add <source>` | Installs a package of skills; see [packages](packages.md) |

### Built-in skills

The built-in skills are extracted to `~/.azazel/cache/skills-<hash>/` the first time a build runs. To change one, put a skill with the same `name` in `~/.azazel/skills` or the project.

| Skill | Use it for |
|---|---|
| `api-design` | Designing or reviewing an HTTP or RPC API, a library surface or a wire format |
| `azazel-extensions` | Customising Azazel: Luau extensions, hooks, agent definitions, commands, skills, packages |
| `browser-testing` | Checking a web page or app in a real browser with the `browser` tool |
| `code-review` | Reviewing a diff, branch or pull request: rubric, severities, findings format |
| `debugging` | Finding the cause of a failure: reproduce, isolate, hypothesise, instrument, fix, prove |
| `design-tokens` | Deriving colour, type, spacing, radius and shadow tokens and emitting them as CSS and a Tailwind theme |
| `docs-writing` | READMEs, tutorials, how-to guides, reference pages, changelogs |
| `frontend-design` | Building or restyling a user interface so that it looks designed |
| `game-dev` | Working in Godot, Unity, Unreal Engine, Roblox or Bevy projects, including the asset pipeline |
| `git-workflow` | Commits, branches, conflicts, bisecting, recovering lost work |
| `mcp-builder` | Writing, extending or debugging an MCP server |
| `media-generation` | Prompts for images, video, 3D models, audio and sprite sheets |
| `performance` | Measuring, profiling and proving a speed or memory improvement |
| `playtest` | Running a game or application build and trying it as a user would |
| `refactoring` | Restructuring code without changing what it does |
| `release-engineering` | Versioning, changelogs, CI matrices, cross-compiling, packaging |
| `reverse-engineering` | Understanding software or data without its source: binaries, file formats, protocols |
| `security-review` | Looking for security defects in code you are allowed to assess |
| `testing` | Writing tests, choosing their level, fixing flaky or slow tests |
| `ui-review` | Auditing an interface for accessibility and usability problems |

## Custom commands

A custom command is a Markdown file. Its path below a `commands` directory, without `.md` and with `/` written as `:`, is its name: `commands/review.md` is `/review`, `commands/git/sync.md` is `/git:sync`.

| Order | Directory |
|---|---|
| 1 | `commands` directories of installed [packages](packages.md) |
| 2 | `~/.azazel/commands/` |
| 3 | `<project>/.claude/commands/` (while `[compat] claude = true`) |
| 4 | `<project>/.azazel/commands/` |

A command in a later directory replaces one of the same name from an earlier directory. Choose names that are not already in `/help`.

For example, `<project>/.azazel/commands/changes.md`:

```markdown
---
description: Summarise what changed since a tag
argument-hint: <tag> [audience]
model: "@smol"
---
Summarise the changes since $1 for ${2:-users}.

!`git log --oneline $1..HEAD`

Follow the format in @docs/CHANGELOG.md.
```

```text
/changes v1.4.0 "plugin authors"
```

Frontmatter is optional:

| Key | Meaning |
|---|---|
| `description` | Shown in `/help` and in completion. Default: the first line of the body |
| `argument-hint` | Shown after the description |
| `model` | A model, role or fallback chain this one prompt runs on |

In the body:

| Form | Replaced by |
|---|---|
| `$ARGUMENTS` | Everything typed after the command |
| `$1` ... `$9` | Positional arguments; quotes group words. A missing argument is empty |
| `${N:-default}` | Argument `N`, or `default` when it is absent |
| A line that is only `` !`command` `` | The command's output. It runs in the resolved shell (see [tools](tools.md)) in the working directory, with a 30 second limit, and only when the project is trusted; otherwise the line stays as text |
| `@path` | Left in place; the file's content is attached in a `<file path="...">` block when the prompt is sent |

The expanded text is sent to the lead as your prompt. `@path` works in any prompt you type, not only in commands: the file must be UTF-8 text of at most 256 KB, paths are relative to the working directory, and an attached file counts as read for the [freshness rule](tools.md) of `write` and `edit`.

## `/init`

`/init` sends the lead a fixed prompt: explore the repository and write `AGENTS.md` at the project root with the sections Build, Test, Layout and Conventions, giving exact commands, in under 200 lines. An existing `AGENTS.md` is corrected and extended rather than replaced. Azazel loads `AGENTS.md` into the system prompt of every later session in the project.

This is not the same as the `azazel init` subcommand, which chooses your default model and creates the project's config file; see [getting started](getting-started.md).

## Extending further

- Slash commands that run code are added by [extensions](extensions.md) (`azazel.command`) and by MCP servers (`/mcp:<server>:<prompt>`, see [mcp](mcp.md)).
- The built-in `azazel-extensions` skill holds these formats in a form the model can use: ask the agent to write a skill, a command or an agent definition for you.
