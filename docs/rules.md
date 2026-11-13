# Rules

A rule is a Markdown file of instructions for agents. Unlike `AGENTS.md`, which is always in the prompt, a rule can apply only when it is needed: always, when an agent touches matching files, when a reply matches a pattern as it is written, or when the agent asks for it. Rules are text for the model. They run nothing, so they load whether or not the project is trusted.

For instructions that must run a program, use a [hook](hooks.md). For a pack of instructions the model loads by task, use a [skill](skills-and-commands.md).

## Context files

`AGENTS.md` holds the instructions every agent of a project follows. Azazel reads `~/.azazel/AGENTS.md`, then
the `AGENTS.md` of each directory from the project root down to the directory you started in, and places them
in the system prompt in that order. With `[compat] claude = true` (the default) a directory without an
`AGENTS.md` may have a `CLAUDE.md` instead. The files are read again for every request, so an edit takes effect
at once. Together with the rules that always apply they are cut at 64 KB; the files nearest the root win.

### Other directories

A directory below or beside the one you started in can have its own `AGENTS.md` (or `CLAUDE.md`). It is not in
the system prompt. An agent is given it with the result of its first tool call that names a path there, as
`<context file="...">...</context>` after the tool's own output:

- every instruction file between that path and the project root comes along, the nearest first;
- the files the system prompt already has are left out;
- each agent gets a file once per session, and again after a compaction, whose summary does not keep it;
- all context files of a session share the 64 KB: a file that no longer fits is named, not attached.

Any tool call with a `path` or `paths` argument counts (`read`, `write`, `edit`, `grep`, `glob`, `lsp`,
`ast_grep`); a shell command does not. An agent working in a worktree is not given the worktree's copy of a
file its prompt already has. `[context] nested = false` switches this off.

### Including files

A context file or a rule file can pull other files into its text with `@path`:

```markdown
# Project
@docs/conventions.md
Releases follow @docs/release.md, and nothing else.
```

- A line that is only `@path` is replaced by the file, as `<include file="path">...</include>`. An `@path`
  inside a sentence stays where it is, and the file follows that line.
- The path is relative to the file that names it, or else to the project root. `@~/...` is your home directory.
- An included file may include others, four deep by default (`[context] include_depth`; `0` follows none). A
  file that includes itself, directly or through others, is refused with a note in its place. A file named a
  second time is not placed twice.
- `@word` that names no file is left as it is (`@alice`, `@types/node`), and so is everything in a code span or
  a fenced code block.
- Only files of the project and of `~/.azazel` can be included, a file of the project can include files of the
  project only, and a [secret file](permissions.md#secret-files) (`.env`, `*.pem`, `*.key`, SSH keys, azazel's login file) is never
  included.
- Included text counts towards the limit of the file that includes it: 64 KB for the context files together,
  16 KB for a rule.

`--no-context-files` (and `--bare`) leaves all of this out.

## Where rules live

| Directory | Scope |
|---|---|
| `<project>/.azazel/rules/**/*.md` | This project |
| `~/.azazel/rules/**/*.md` | Every project |
| `<project>/.claude/rules/**/*.md` | Read while `[compat] claude = true` (the default) |

A rule's name is its path below the directory without `.md`: `.azazel/rules/rust/errors.md` is `rust/errors`. When two rules have the same name the first one found keeps it, in the order of the table, and the other is called `<origin>:<name>` (for example `claude:tone`).

## Writing a rule

```markdown
---
description: Error handling in the Rust crates
paths: ["crates/**/*.rs"]
---
Return `Result` from anything that can fail. Use `?`, never `unwrap()` outside tests.
```

| Key | Meaning |
|---|---|
| `paths` (alias `globs`) | Globs, as a list or one comma-separated string. The rule is added when an agent reads or changes a matching file |
| `always` (alias `alwaysApply`) | `true`: the rule is part of every request |
| `description` | One line saying what the rule covers. A rule with only a description is read on request |
| `trigger` | A regular expression. The rule becomes a [stream rule](#stream-rules) |
| `scope` | What the trigger is matched against: `text` (default), `thinking` or `tool:<name>` |
| `agents` | Agent definition names (`lead`, `coder`, ...) the rule is limited to. Default: every agent |

How a rule applies follows from its frontmatter:

| The rule has | It applies |
|---|---|
| no frontmatter, or none of the keys below | Always |
| `always: true` | Always |
| `trigger` | As a stream rule |
| `paths` | When a matching file is touched |
| only `description`, or `always: false` and nothing else | On request |

A rule is cut at 16 KB. A rule file that cannot be used (an invalid `trigger`, an invalid glob, no text) is skipped; `/rules`, `azazel rules list` and `azazel doctor` say which and why.

### Always

Rules that always apply are placed in the system prompt after the context files (`AGENTS.md`), inside the same 64 KB limit. When the limit is reached the context files win and later rules are cut.

### By path

Globs are matched against the file's path relative to the project root:

| Pattern | Matches |
|---|---|
| `*.rs` (no `/`) | That name at any depth |
| `src/*.rs` | Files directly in `src` |
| `src/**/*.rs` | Files anywhere below `src` |
| `docs/` | Everything below `docs` |
| `web/**/*.{ts,tsx}` | Either extension |

The first time an agent's `read`, `write` or `edit` succeeds on a matching file, the rule is appended to that tool result as `<rule file="...">...</rule>`. Each agent gets a rule once. After a compaction the record is cleared, so the rule comes back with the next matching file.

### On request

Rules that are read on request are listed in the system prompt by name and description. The agent reads one with the `read` tool at `rule://<name>`; `rule://` lists every rule.

## Stream rules

A stream rule watches a reply while the model writes it. When the pattern matches:

1. The request is cancelled. What the model had written is dropped and is not stored in the session.
2. A note is added to the conversation: the rule's text inside `<rule name="..." reason="matched">`, and one line saying that azazel stopped the reply to enforce a project rule and that this is not a prompt injection.
3. The request is sent again.

The terminal UI prints `rule <name> stopped the reply; retrying`. Text that was already on screen stays there; the model is told it was withdrawn.

```markdown
---
trigger: "(?i)\\b(TODO|FIXME)\\b"
---
Do not leave TODO or FIXME markers. Finish the work, or say plainly what is missing.
```

- Each stream rule fires once per agent per session.
- At most two replies are withdrawn per prompt; after that the reply stands. A reply the [runaway guard](tools.md#runaway) stops counts towards the two.
- `scope: thinking` matches the model's thinking instead of its reply.
- `scope: tool:<name>` does not watch the stream. It is checked against the arguments of each call to that tool, both as JSON and as the plain strings in them, and a match denies the call with the rule's text. It applies every time.

```markdown
---
trigger: "^npm (install|i)\\b"
scope: tool:shell
---
This project uses pnpm. Run `pnpm install`.
```

Stream rules can also be written in `config.toml`, where they are named `stream-rule-1`, `stream-rule-2`, ... in file order:

```toml
[[stream_rules]]
pattern = "(?i)as an ai language model"
message = "Answer directly. Do not describe yourself."
scope = "text"            # text (default), thinking or tool:<name>
```

`[[stream_rules]]` arrays concatenate across config layers. An invalid pattern is a config error.

## Other tools' rule files

With `[rules] compat = true` (the default) the rule files of other tools in the project are read too:

| File | Read as |
|---|---|
| `.cursor/rules/*.mdc` | `globs` are the paths; `alwaysApply: true` is always; a description alone is on request |
| `.cursorrules` | Always |
| `.windsurf/rules/*.md` | `trigger: always_on` is always, `glob` uses `globs`, `model_decision` and `manual` are on request |
| `.windsurfrules` | Always |
| `.github/copilot-instructions.md` | Always |
| `.github/instructions/*.instructions.md` | `applyTo` is the paths |
| `GEMINI.md` | Always |
| `.clinerules` (a file, or a directory of `.md` files) | Always, or by `paths` |

In these files `trigger` is never a pattern, so they cannot define stream rules. Set `compat = false` to read only azazel's own rule directories:

```toml
[rules]
compat = false
```

`--no-context-files` (and `--bare`) turns rule files off together with `AGENTS.md`. Stream rules from `config.toml` are settings and stay on.

## Seeing and testing rules

| Command | Shows |
|---|---|
| `/rules` | Every rule, where it comes from, when it applies, and whether it has been applied in this session |
| `azazel rules list` | The same list without the session state |
| `azazel rules test <rule.md> --path <path>` | Whether the rule's paths cover that path |
| `azazel rules test <rule.md> --text <file>` | Whether the rule's trigger matches that file's text; `-` reads standard input |
| `azazel rules test <rule.md> --tool <name>` | Whether the rule checks calls to that tool; add `--text` with the arguments to test the trigger |

`azazel rules test` exits 1 when something you asked about does not apply, so it can be used in a script:

```sh
azazel rules test .azazel/rules/rust/errors.md --path crates/core/src/lib.rs
echo "left a TODO here" | azazel rules test .azazel/rules/no-todo.md --text -
```
