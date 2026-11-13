# Tools

Tools are what an agent can do besides writing text: read and change files, run commands, search the web, drive a browser, start other agents. This page lists every built-in tool with its input fields and what it does, and covers the rules they share: read limits, the read guard for edits, format on write, checks on write, output truncation, the URL schemes `read` understands, and the `[tools]` settings.

Every call goes through the permission engine first; see [permissions](permissions.md). Each tool declares what a call touches (`read`, `write`, `exec` or `net`), and the permission mode decides from that whether you are asked. When all the calls in one assistant message are read-only they run at the same time; otherwise they run one after another in order.

Fields marked `?` are optional.

## Files

| Tool | Input | Behaviour |
|---|---|---|
| `read` | `path`, `offset?`, `limit?`, `pages?` | Reads a text file, an image, a PDF, a notebook, a directory or a `scheme://` address |
| `write` | `path`, `content` | Creates a file or replaces its whole content. Parent directories are created |
| `edit` | `path`, `edits: [{old, new, replace_all?}]` | Exact text replacement in an existing file |

`read`:

- Text comes back as `<line number><tab><text>`. Default 2000 lines per call; `offset` (1-based line) and `limit` page through longer files. Lines are cut at 2000 characters and one call returns at most about 256 KB. A partial read ends with `[lines 1-2000 of 5120; continue with offset=2001]`.
- Files over 64 MB are refused. Other binary files return an error with their size and type.
- PNG, JPEG, WebP and GIF files are returned as images, scaled down when their long side exceeds 1568 px.
- A PDF comes back as its text, each page under a `--- page N ---` line. One call returns at most 20 pages; `pages` picks them (`"3"` or `"2-6"`), and a partial read ends with `[pages 1-20 of 57; continue with pages="21-40"]`. A PDF without a text layer returns `this PDF has no extractable text (it is probably scanned)`. A PDF cannot be edited, so reading one does not count as having read it for `write`.
- A notebook (`.ipynb`) comes back cell by cell: `# cell 4 [code]`, the cell's source, then `--- output ---` and its text outputs, each cut at 2000 characters. An image output is named in a line such as `[image output: cell 4 image 1, image/png, 5120 bytes]`, and the first image of the cells shown is attached to the result (a model without vision gets a note in its place). `offset` and `limit` count cells; a partial read ends with `[cells 1-12 of 40; continue with offset=13]`.
- A directory lists up to 500 entries: `name/` for directories, `name<tab>N bytes` for files.
- `~/` is expanded. Relative paths start at the agent's working directory.

### The read guard

An agent may only change what it has read. For `write` and `edit` that means three things:

- The file must have been read (or written) by this agent and must not have changed since. Otherwise the call fails with `read the file before overwriting it` or `file changed since you last read it; read it again`. The check compares content, not timestamps.
- `edit` must be of lines the agent has read. A `read` with `offset` and `limit` counts for the lines it returned and no others, and several reads add up. An edit whose `old` text lies on other lines fails with `edit targets lines 40-52 of src/a.rs, which you have not read; read them first`, naming exactly the lines still to read, and writes nothing.
- `write` over an existing file takes having read all of it: `you have read only part of src/a.rs; read the rest before overwriting it, or change it with edit`.

What counts as read:

- The lines a `read` returned; a read cut off at 2000 lines or 256 KB covers what it showed. The lines `lsp` `read_symbol` returned, and the lines an `ast_grep` preview showed being replaced.
- Everything the agent wrote itself. After its own edit the lines it wrote are read, and the lines it had read are still read where they now are.
- A whole file you attach with `@path`, and any read of a notebook.
- When a file changes under an agent that has read part of it (another agent, you, a formatter), the lines above and below the change are still read after the next read of any part of the file; the changed lines are not. Files of more than 3000 lines are not followed line by line: after a change, only what is read again counts. When format on write rewrites lines the agent had read, the formatted lines count as read.

Notes and logs (`.md`, `.txt`, `.log`) are not held to lines: any read of one lets it be edited or replaced.

`tools.read_guard` sets how much is asked: `range` (the default, as above), `file` (any read of the file lets any of it be changed, as before this guard knew lines) or `off` (nothing has to have been read, and a file that changed since is overwritten without a word).

`/allow-edit <path>` lets the next `edit` or `write` of one file through whatever the agent has read of it, once. An edit that fails for another reason does not use the allowance up.

When one agent writes a file another agent has read, the other agent is told to read it again before editing.

`edit` applies its edits in order and writes nothing if any of them fails. Each `old` must occur exactly once unless `replace_all` is true; the errors are `old text not found (edit #N)` and `old text matches N times (edit #N); add surrounding lines or set replace_all`. CRLF line endings and a byte-order mark are preserved. Both tools write through a temporary file and a rename. `write` refuses a file name whose stem is a Windows device name (`con`, `prn`, `aux`, `nul`, `com1` to `com9`, `lpt1` to `lpt9`, so `aux.c` and `NUL` too) with `` `aux.c` is a reserved Windows device name; pick another file name ``, on every system: such a file cannot be opened on Windows, where the project may be cloned. When a language server covers the file, its errors for the new content are appended to the result; see [lsp](lsp.md). When a checker covers it, so are the checker's errors; see [checks on write](#checks-on-write).

### Format on write

With `[tools] format_on_write = true`, a successful `write` or `edit` runs the formatter for the file's extension on the file. The result then ends with `; formatted with rustfmt`, the diff shown is of the formatted file, and the agent is taken to know the formatted content, so its next edit is not refused as stale. A formatter gets 5 seconds. One that fails, times out or is missing adds a one-line note such as `; rustfmt failed: ...` and never fails the edit.

The formatter for an extension comes from `[tools.format]`:

```toml
[tools]
format_on_write = true

[tools.format]
rs = "rustfmt --edition 2024"
py = "black -q"
sql = "sqlfluff fix {file}"
```

The command is run directly, without a shell. The file's path is appended, or put where `{file}` stands. Quote a program path that has spaces. For an extension with no entry, these defaults apply when the program is on `PATH`:

| Extensions | Command |
|---|---|
| `rs` | `rustfmt`, with `--edition` from the nearest `Cargo.toml` that states one |
| `js`, `jsx`, `mjs`, `cjs`, `ts`, `tsx`, `json`, `css`, `scss`, `less`, `html`, `vue` | `prettier --write` |
| `go` | `gofmt -w` |
| `py`, `pyi` | `ruff format` |
| `c`, `h`, `cc`, `cpp`, `cxx`, `hpp`, `hh` | `clang-format -i` |

Both settings name programs to run, so a project's config can set them only when the project is trusted.

### Checks on write

After a successful `write` or `edit`, and after the formatter, the checker (linter) for the file's kind runs on it. This is on by default (`[tools] check_on_write = true`) and does nothing for a kind of file whose checker is not installed. The errors it finds are appended to the tool result, at most 10 of them, in the shape of a language server's diagnostics:

```
edited src/config.rs (1 replacement); formatted with rustfmt
checks:
src/config.rs:42:17 error mismatched types [E0308] [clippy]
… 2 more errors
```

Warnings and notes are not in the result; they are kept for the report the lead gets when it stops (below). What a checker says about other files is dropped. Each line ends with the checker's name, after the code the checker gave the finding when it has one.

These checkers are built in. Each is used only when its program is on `PATH`. Three of them run code of the project in order to check it, so they are used only in a project you trust (below):

| Files | Checker | Command |
|---|---|---|
| `rs` | clippy (trusted projects) | `cargo clippy -p <package> --message-format=short`, run in the directory of the nearest `Cargo.toml` that names a package; `--all-targets` is added for a file under `tests/`, `benches/` or `examples/`. A file that belongs to no package is not checked |
| `py`, `pyi` | ruff | `ruff check --output-format concise --no-fix <file>` |
| `js`, `jsx`, `mjs`, `cjs`, `ts`, `tsx`, `mts`, `cts` | eslint (trusted projects), else oxlint | `eslint -f json <file>` when an eslint config file (`eslint.config.*`, `.eslintrc*`) is beside the file or above it inside the project; otherwise `oxlint -f unix <file>` |
| `sh`, `bash` | shellcheck | `shellcheck -f gcc <file>` |
| `go` | go vet (trusted projects) | `go vet .` in the file's directory |
| `yml`, `yaml` under `.github/workflows` | actionlint | `actionlint -oneline <file>` |
| `Dockerfile`, `Dockerfile.*`, `*.dockerfile` | hadolint | `hadolint --no-color <file>` |

`[tools.check]` names your own checker for an extension. It replaces the built-in one, and an empty command turns the check off for that extension:

```toml
[tools.check]
py = "pylint --output-format=parseable"
sql = "sqlfluff lint {file} --format github-annotation-native"
rs = ""                      # no clippy after edits
dockerfile = "docker-lint"   # the key for Dockerfiles
```

The command is run directly, without a shell, in the agent's working directory. The file's path is appended, or put where `{file}` stands. Its output is read line by line, from both streams, for `file:line:column: severity: message` (the column and the severity may be missing; a line without a severity is a warning; `error`, `warning` and `note`, `info`, `style`, `help` are understood). An exit code other than 0 is expected when something was found.

Limits, none of which ever fails the write:

- A write waits 5 seconds for its checker. One that takes longer goes on in the background, and the result says so: `; clippy is still running; what it finds joins the report when you stop`. When the lead stops, the report waits for it, and for the files that waited behind it, for two minutes at most. A checker may run for 60 seconds in all; then it is ended with everything it started, and the file is named in the report as not checked.
- One run of a checker for a kind of file goes on in the background per directory it runs in (per package for clippy). A file written there meanwhile waits behind it, without making its write wait: `; clippy is still checking an earlier write; what it finds here joins the report when you stop`. When the run is done, the files that waited are checked, with one run for all that share a command line, so one clippy run serves every file of a package. A file written several times while it waited is checked once.
- Your next prompt ends the checks that are still running. With `check_report = "off"` nothing would show what a background run finds, so a checker is ended after the 5 seconds and the result says `; clippy did not finish within 5 s`.
- A checker that exits with an error and reports nothing adds `; ruff failed: <its first line>`. A checker named in `[tools.check]` that is not installed adds ``; checker `pylint` was not found``.
- At most 3 checkers run at the same time over all agents. Files larger than 1 MB are not checked.

`cargo clippy` builds the package, so on a large crate, and the first time on any, it takes longer than a write waits. Its findings then come with the report at the end of the turn instead of with the write.

#### Checkers that run the project's code

To check a file, `cargo clippy` builds and runs the package's build scripts and procedural macros, `eslint` loads the project's configuration file, which is a program, and `go vet` builds the package (with cgo, a C compiler too). That is the project's code running without a tool call you could have refused, so these three are off in a project that is not trusted; for scripts, oxlint is used in the place of eslint. ruff, oxlint, shellcheck, actionlint and hadolint only read the file and run everywhere. A checker you name in your own `[tools.check]` is yours to vouch for and runs everywhere too.

So that a plain Rust, JavaScript or Go project gets its checker, the project counts as having something to trust when checks on write are on, the checker is installed, `[tools.check]` does not replace it, and the project root has a `Cargo.toml` (clippy), an eslint config file (eslint) or a `go.mod` (go vet). The interactive UI then asks once, as it does for hooks and MCP servers; see [trust](configuration.md#trust). Print mode, RPC mode and the subcommands do not ask: there an undecided project gets these checkers only with `--trust`.

#### The report when the lead stops

When the lead is about to stop, azazel first waits for the checks still running in the background (the transcript says `checks: waiting for a checker that is still running`). If the checkers found something in the files written since your last prompt (by the lead or by the agents it started), the lead is then told once, as a user message, and takes one more round:

```
<checks>
The checkers found 1 error, 3 warnings in 2 files written in this turn:
src/config.rs: 1 error, 1 warning
tools/release.py: 2 warnings
On the lines changed in this turn:
src/config.rs:42:17 error mismatched types [E0308] [clippy]
tools/release.py:9:5 warning F841 Local variable `tag` is assigned to but never used [ruff]
1 file was not checked, so nothing found there does not mean it is clean: web/app.ts (oxlint did not finish within 60 s)
</checks>
This is an automatic report, not a message from the user. Fix what your changes caused, say in a line what you leave and why, then finish.
```

The transcript shows one muted line, such as `checks: 1 error, 3 warnings in 2 files, 2 on changed lines; the agent was told`. `[tools] check_report` chooses what is listed:

| Value | Listed |
|---|---|
| `"delta"` (default) | Findings on lines the turn changed, and on the two lines that meet where it removed some. Lines are followed through later edits of the same file, and a new file is changed everywhere. The counts are of everything found in the written files |
| `"all"` | Every finding in the written files |
| `"off"` | No report; errors still join the result of each write |

There is a report only when there is something to list: a turn whose files have findings only on lines it left alone, or only files that were not checked, ends without one. Each file's findings are those of its last write, so what the agent fixed during the turn is not reported. At most 30 findings are listed. The report comes after the [advisor](team.md#advisor)'s review and before a [goal](goals.md)'s own check and your `stop` [hooks](hooks.md).

#### Secret scan

With `[tools.scan] secrets = true` (the default), `gitleaks` on `PATH` and a trusted project (`--trust`, or a project you answered the trust question for), azazel runs `gitleaks detect --no-banner --report-format json` over the project once per session, in the background, starting at your first prompt. What it finds joins the next report the lead gets when it stops, or makes one:

```
gitleaks found 2 secrets in the project (file, line and the rule that matched):
config/dev.env:3 generic-api-key
deploy/keys.txt:12 aws-access-token
```

The secret itself is never read from the scan's output, shown or sent to a model. A project that is not a git repository is scanned as files (`--no-git`). At most 20 places are listed; the scan gets 2 minutes and ends with the session. `check_report = "off"` turns the scan off too, since the report is the only place its findings go.

`azazel doctor` lists the built-in checkers that are installed (`checks on write: clippy (rs), shellcheck (sh, bash)`), names each one that is off for lack of trust (`checks on write: clippy (rs): off, the project is not trusted`), warns about a `[tools.check]` program that is missing, and says whether secrets are scanned for and, if not, why. `[tools.check]` and `[tools.scan]` name or start programs, so a project's config can set them only when the project is trusted.

## Search

| Tool | Input | Behaviour |
|---|---|---|
| `glob` | `pattern`, `path?` | Finds files by name, newest first, at most 1000 |
| `grep` | `pattern`, `path?`, `glob?`, `ignore_case?`, `context?`, `mode?`, `limit?`, `multiline?` | Searches file contents with a Rust regular expression |
| `ast_grep` | `action`, `pattern`, `lang?`, `paths?`, `rewrite?`, `max_matches?`, `apply?` | Searches code by its syntax tree, or rewrites what matches. Only when [ast-grep](https://ast-grep.github.io) is installed |

Both honour `.gitignore` and other ignore files, include hidden files and skip `.git`. In `glob`, `*.rs` matches at any depth, `src/*.rs` only directly in `src`, and `src/**/*.rs` anywhere below it. `grep` modes are `content` (default, `path:line:text`), `files` (paths only) and `count` (`path:N`); `limit` defaults to 200 results, `context` adds lines around each match, `multiline` lets a pattern span lines. Binary files are skipped and matching lines are clipped at 500 characters.

### `ast_grep`

`ast_grep` runs the external [ast-grep](https://ast-grep.github.io) program, found on `PATH` as `ast-grep` or `sg`. Azazel does not install it. Without it the tool does not exist and `azazel doctor` says `ast_grep: off, ast-grep is not installed`. (On Linux `sg` is usually another program, which runs a command under a group; it is used only when it answers `--version` as ast-grep does.)

A pattern is code with metavariables: `$NAME` stands for any one node of the syntax tree and `$$$NAME` for any number of them, so `console.log($$$ARGS)` matches every call whatever its arguments and `if ($COND) { return $X }` every such statement however it is spaced. `lang` names the language when the pattern could be more than one.

| Input | Meaning |
|---|---|
| `action` | `search` or `replace` |
| `pattern` | The code to match |
| `lang` | The pattern's language, such as `rust`, `typescript`, `python` |
| `paths` | Files and directories to look in; the working directory when left out. With `apply`, the files to write |
| `rewrite` | `replace`: the code that takes the place of each match; it may use the pattern's metavariables |
| `max_matches` | Matches looked at, in the order of the files and of their places in them: 50 by default, at most 200 |
| `apply` | `replace`: write the files. Left out or `false`, the call previews |

`search` returns each match as `path:line: <source line>`, with `(+N lines)` when it runs over several, then one indented line per metavariable:

```
src/api.ts:14: console.log("loaded", user.id)
  $$$ARGS = "loaded" , user.id
src/api.ts:31: console.log(err)
  $$$ARGS = err
[50 of 312 matches shown; narrow the pattern or the paths, or raise max_matches (at most 200)]
```

`replace` without `apply` writes nothing. It returns the unified diff of what would change and, in its first line, the call that writes it:

```
preview: 3 replacements in 2 files, nothing written. To write them, run this again with apply: true and paths: ["src/api.ts","src/app.ts"]
--- a/src/api.ts
+++ b/src/api.ts
@@ -12,5 +12,5 @@
…
```

`replace` with `apply: true` writes the files named in `paths`, which must be files, not directories: they are what permission is asked for. The replacements are made by azazel from what ast-grep found, not by ast-grep, so each file goes the way of an `edit`: under the file's lock, through the [read guard](#the-read-guard), format on write, checks on write and the language server, and the agents that had read it are told. The result names each file with its number of replacements and carries the same `diagnostics:` and `checks:` blocks an edit would.

- A file is written only when the agent has seen the lines being replaced, in the preview or by reading them, and the file has not changed since. A file that changed after the preview is refused with `file changed since you last read it; read it again`: preview it again.
- All the files are written or none: one file refused leaves the others untouched, and the error lists every refusal.
- A match inside another match is left for a second run, and the result says how many were.
- A search is a read, and so is a preview. Only `apply` is a write. Rules name the tool with a path pattern, such as `ast_grep(src/**)`; see [permissions](permissions.md).

## Shell and jobs

| Tool | Input | Behaviour |
|---|---|---|
| `shell` | `command`, `cwd?`, `timeout_secs?`, `background?`, `description?` | Runs a command in the resolved shell |
| `job` | `action` (`output`, `kill`, `list`), `id?`, `tail?` | Manages background jobs started by `shell` |

`shell` returns stdout and stderr together, then `exit code: N`. Long output keeps its first 100 and last 300 lines; lines over 4000 characters are cut. On timeout (default `shell_timeout_secs`) or abort the whole process tree is killed. With `background: true` the call returns a job id (`job-1`, ...) at once; `job output` returns the last `tail` lines (default 100, a job keeps its last 2000) and the job's state. Background jobs are killed when the session ends. Processes started for an agent can never push to a git remote; see [permissions](permissions.md).

## Code execution

| Tool | Input | Behaviour |
|---|---|---|
| `run_code` | `code` | Runs a Luau script that calls other tools, and returns what it printed and returned |

`run_code` lets an agent do a job of several steps in one call: read a dozen files and return only the lines that matter, search and then filter, make the same edit in many places. Only the script's result reaches the model, not what the tools returned along the way.

```lua
local counts = {}
for _, path in ipairs({ "src/lib.rs", "src/main.rs" }) do
    local text = tools.read({ path = path })
    local lines = 0
    for _ in string.gmatch(text, "[^\n]+") do lines += 1 end
    table.insert(counts, path .. ": " .. lines)
end
return table.concat(counts, "\n")
```

- `tools.<name>{ ... }` calls a tool with the arguments the agent would give it and returns the text of its result. A name that is not an identifier is written `tools["mcp__server__tool"]{ ... }`.
- A call that fails or is refused raises an error whose message starts with the tool's name; `pcall` catches it.
- Every call is made as the agent that runs the script and goes through the permission engine, the mode and the hooks like a call the agent makes itself. `run_code` counts as `read`; each call inside it is judged on its own, so a script cannot do what the agent could not.
- `print(...)` output and the value the script returns are the result. A returned table comes back as JSON. `json.encode` and `json.decode` are available.
- The script runs in a fresh sandboxed Luau VM: no file, process or network access except through `tools`.
- Limits: 120 seconds of the script's own running time (waiting for a tool does not count), 50 tool calls, 64 KB of result. A longer result is cut and kept whole as `artifact://<call id>-code`.
- `run_code` itself, `task` and the other team tools cannot be called from a script.

The tool's description lists the callable tools with their arguments, within about 3000 tokens; tools that do not fit are named, and every tool the agent has is callable by name. The tool is there when azazel is built with the `lua` feature (the default), also with `--no-extensions`.

## Todo list, questions, plans

| Tool | Input | Behaviour |
|---|---|---|
| `todo` | `items: [{text, status}]` | Replaces the agent's task list. `status` is `pending`, `in_progress` or `done`; at most one item may be `in_progress` |
| `ask_user` | `question`, `type?`, `context?`, `options?`, `multi?`, `allow_freeform?` | Asks you a question and waits; see below |
| `plan_submit` | `path` | In plan mode: submits a plan file under `.azazel/plans/` for your approval. See [permissions](permissions.md) |

### The task list

The list is shown above the input while work on it remains, and `/todo` shows or changes it. Two things keep an agent on its plan:

- **It is durable.** The lead's list is rebuilt from the session whenever one is opened: after `-c`, `--session`, `--fork`, `/resume`, `/tree`, `/rewind` and `/fork` it is what the last `todo` call that succeeded on that branch set (or what you changed with `/todo` after it). A compacted session keeps its list. A new session starts with none.
- **The agent is reminded of it.** While the list has an item that is not `done`, every request ends with a note such as `[Plan] 1/3 done`, followed by the list as `[x]`, `[~]` and `[ ]` lines. The note is added to what is sent and is not part of the session, so it costs no context later and leaves the cached part of the conversation as it is. It stops when every item is done or the list is cleared.

### `ask_user`

| Input | Meaning |
|---|---|
| `question` | What is asked |
| `type` | `select` (one of the options), `multiselect` (any number of them), `text` (a typed answer) or `confirm` (yes or no). Left out, it follows from the rest: `text` without `options`, `multiselect` with `multi: true`, otherwise `select` |
| `context` | What you need to know to answer; shown above the question |
| `options` | The choices of a `select` or `multiselect`: 2 to 8, each different from the others whatever its case and spacing. Not allowed for `text` and `confirm` |
| `allow_freeform` | `select` only: the list ends with `Other…`, which lets you type an answer instead |

A call that breaks one of these rules fails before you are asked anything, with what is wrong (`a `select` question needs 2 to 8 options, not 1`, `the option `red` is offered twice`).

The result is the chosen option, the chosen options one per line, the typed text, or `yes` or `no`. When you dismiss the question, or nobody is there to answer (print mode, a session without a client), the result is `the user declined to answer`. That is an answer, not a failed call: the agent goes on without one. Over [ACP](acp.md) a question that needs typing cannot be asked and is declined the same way.

## Web

| Tool | Input | Behaviour |
|---|---|---|
| `web_search` | `query` or `queries` (at most 5), `limit?`, `site?`, `recency?` | Numbered results with title, URL and snippet; several queries run at once and get a section each. `limit` defaults to 8 (at most 20); `recency` is `day`, `week`, `month` or `year` |
| `web_fetch` | `url`, `format?`, `offset?`, `limit?` | Fetches an http or https URL. `format` is `markdown` (default), `text` or `raw`. A long page comes in parts: `offset` is the first line, `limit` the most characters (default 30000). A PDF comes back as its text |
| `web_grep` | `url`, `pattern`, `ignore_case?`, `literal?`, `context?`, `limit?`, `format?` | Searches the page at a URL line by line and returns the matching lines with their numbers, from the same cache as `web_fetch` |

Details, search backends and limits: [web and browser](web-and-browser.md).

## Language servers

| Tool | Input | Behaviour |
|---|---|---|
| `lsp` | `action`, `path`, `line?`, `column?`, `query?`, `new_name?`, `symbol?`, `occurrence?`, `new_path?` | `action` is `diagnostics`, `definition`, `references`, `hover`, `symbols`, `outline`, `read_symbol`, `rename` or `rename_file`. Positions are 1-based. `rename` and `rename_file` change files; the rest only read |

See [lsp](lsp.md).

## Skills

| Tool | Input | Behaviour |
|---|---|---|
| `skill` | `name`, `args?` | Loads a skill: returns its directory and its instructions with `$ARGUMENTS` filled in |

A skill's `allowed-tools` limits the sub-agent of a `context: fork` skill to those tools, and for any other skill allows them without a prompt until the run ends. See [skills and commands](skills-and-commands.md).

## Memory

Registered when `[memory] enabled` is true (the default). See [memory](memory.md).

| Tool | Input | Behaviour |
|---|---|---|
| `memory_save` | `text`, `kind`, `tags?`, `scope?` | Stores a memory. `kind` is `fact`, `decision`, `preference`, `convention` or `gotcha`; `scope` is `project` (default) or `global` |
| `memory_search` | `query`, `limit?`, `scope?` | Best matches as `id [kind, scope] text #tags`. `limit` defaults to 8 |
| `memory_forget` | `id` | Deletes a memory |
| `semantic_search` | `query`, `collection?`, `path_glob?`, `limit?` | Searches the indexed project files (`code`, default) or a document collection. Returns `path:first-last` and the first 12 lines of each chunk; `limit` defaults to 10 |

## Team

See [team](team.md). The lead has all of these. A sub-agent always has `agent_send`; it gets `task`, `agent_stop`, `agent_wait` and `agent_list` only when its definition lists `spawns` and the depth limit allows; the `worktree_*` tools are the lead's only.

| Tool | Input | Behaviour |
|---|---|---|
| `task` | `agents: [{agent, prompt, name?, model?, isolation?}]`, `wait?` | Starts the listed agents in parallel. With `wait` (default true) returns every report; with `wait: false` returns the ids and each report arrives later as a message |
| `agent_send` | `to`, `message` | Messages an agent id, `parent` or `all`. A finished agent resumes with the message |
| `agent_stop` | `id`, `reason?` | Stops an agent the caller started, and the agents it started |
| `agent_wait` | `ids?`, `timeout_secs?` | Waits (default 600 s) until a started agent has a new result or a message arrives |
| `agent_list` | none | Id, definition, state, activity, time, tokens and cost of every agent |
| `worktree_apply` | `agent` | Brings an isolated agent's changes into the main tree and removes its worktree. On a conflict the worktree is kept |
| `worktree_discard` | `agent` | Removes an isolated agent's worktree and branch |

## Goals

Offered to the lead only, and only while a goal is open. See [goals](goals.md).

| Tool | Input | Behaviour |
|---|---|---|
| `goal_done` | `summary`, `evidence` | Claims the goal is met. The checker runs; the result is `verified` or what is still wrong |
| `goal_blocked` | `reason` | Pauses the goal because the agent needs you |

## Browser and screen

| Tool | Input | Behaviour |
|---|---|---|
| `browser` | `action` plus `url`, `tab`, `ref`, `selector`, `x`, `y`, `text`, `submit`, `keys`, `hold_ms`, `dy`, `for`, `timeout_ms`, `poll_ms`, `expression`, `full_page`, `width`, `height`, `filter` | Drives a Chromium browser. Actions: `open`, `navigate`, `back`, `reload`, `snapshot`, `click`, `hover`, `type`, `key`, `scroll`, `wait`, `eval`, `screenshot`, `resize`, `console`, `network`, `tabs`, `close` |
| `screen` | `action` plus `window`, `display`, `region`, `x`, `y`, `button`, `double`, `from`, `to`, `duration_ms`, `dy`, `keys`, `hold_ms`, `text`, `ms`, `seconds`, `fps` | Captures and operates the desktop. Actions: `windows`, `capture`, `focus`, `click`, `move`, `drag`, `scroll`, `key`, `type`, `wait`, `record` |

See [web and browser](web-and-browser.md) and [screen and playtesting](screen-and-playtesting.md).

## Media

The four `generate_*` tools are registered only when a media backend has credentials. `sprite_sheet` is always available. See [media and sprites](media-and-sprites.md).

| Tool | Input |
|---|---|
| `generate_image` | `prompt`, `size?`, `aspect_ratio?`, `count?`, `transparent?`, `reference_images?`, `model?`, `out?` |
| `generate_video` | `prompt`, `image?`, `duration_secs?`, `aspect_ratio?`, `model?`, `out?` |
| `generate_3d` | `prompt?`, `image?`, `format?` (`glb`, `obj`, `fbx`), `model?`, `out?` |
| `generate_audio` | `prompt`, `kind` (`sfx`, `music`, `speech`), `duration_secs?`, `voice?`, `model?`, `out?` |
| `sprite_sheet` | `source`, `name`, `target`, `animations?`, `key_color?`, `tolerance?`, `trim?`, `padding?`, `max_size?`, `out_dir?` |

## MCP

| Tool | Input | Behaviour |
|---|---|---|
| `mcp__<server>__<tool>` | the server's own schema | A tool of a connected MCP server |
| `list_mcp_resources` | `server?` | Lists resources as `mcp://<server>/<uri>  name` |
| `tool_search` | `query`, `limit?` | Present only when there are too many MCP tools to list: finds tools by description and makes the hits callable. `limit` defaults to 8 (at most 25) |

See [mcp](mcp.md).

## Output truncation and artifacts

A tool result whose text exceeds `max_result_bytes` (default 50000) is cut at that size. The full text is saved under `~/.azazel/projects/<slug>/artifacts/` and the result ends with:

```text
[output truncated: 181234 bytes in total] full output: artifact://<call id>
```

The agent reads the rest with `read artifact://<call id>`. The same directory holds other files tools produce: screenshots, recordings, downloads that are not text, worktree patches.

## URL schemes `read` accepts

| Address | Returns |
|---|---|
| `artifact://<id>` | A spilled tool output |
| `skill://<name>` | The body of a skill |
| `skill://<name>/<relative path>` | A file beside that skill's `SKILL.md` (at most 256 KB) |
| `agent://<id>` | The latest report of an agent |
| `agent://<id>/transcript` | What the agent did: one line per tool call and the full text of what it said |
| `mcp://<server>/<resource uri>` | A resource of an MCP server |
| `http://...`, `https://...` | The page as Markdown, through the `web_fetch` pipeline; `offset` and `limit` of `read` page it by line. Permission rules for `web_fetch` apply |
| `session://` | The open session: id, file, name, when it was created, message counts, and up to 20 other sessions of the project |
| `session://<id>` | Another session of the project (a unique start of its id is enough), in the same condensed form as `agent://<id>/transcript` |
| `job://` | The background jobs, one per line |
| `job://<id>` | A background job's output (its last 2000 lines) and state |
| `memory://` | Every memory as `id [kind, scope] text #tags`. With memory on |
| `memory://<id>` | One memory with its kind, scope, date and tags |
| `memory://search/<query>` | The 20 best matches for a query |
| `pr://`, `issue://` | The open pull requests or issues of the repository (`gh pr list`, `gh issue list`, 30 at most) |
| `pr://<n>`, `issue://<n>` | One pull request or issue with its comments |
| `pr://<n>/diff` | A pull request's diff |

`pr://` and `issue://` run the [`gh`](https://cli.github.com) program in the agent's working directory, with the account `gh` is signed in to, and exist only when `gh` is on `PATH` (`azazel doctor` says whether it is). They count as network access to `github.com`: in `ask` mode you are asked, and the rule `web_fetch(domain:github.com)` allows them.

A scheme says what reading one of its addresses touches. Addresses that stay on this machine are plain reads. Extensions can add more schemes; see [extensions](extensions.md).

## `[tools]` configuration

```toml
[tools]
disabled = ["web_search", "screen"]   # tool names no agent is offered
max_result_bytes = 50000              # text kept from one tool result
shell_timeout_secs = 120              # default timeout of the shell tool
shell = ""                            # program name or path; empty means auto-resolve
format_on_write = false               # run the file's formatter after write and edit
check_on_write = true                 # run the file's checker after write and edit
check_report = "delta"                # what the lead is told when it stops: delta, all or off
read_guard = "range"                  # what must have been read before an edit: range, file or off

[tools.format]                        # file extension to formatter command
rs = "rustfmt --edition 2024"

[tools.check]                         # file extension to checker command
py = "pylint --output-format=parseable"

[tools.scan]
secrets = true                        # scan the project with gitleaks once per session
```

| Key | Default | Meaning |
|---|---|---|
| `disabled` | `[]` | Tools removed for every agent, by name (also MCP and extension tools) |
| `max_result_bytes` | `50000` | Size above which a result is cut and spilled to an artifact |
| `shell_timeout_secs` | `120` | Timeout of a `shell` call without `timeout_secs` |
| `shell` | `""` | Shell to use instead of the resolved one. In a project config this is honoured only when the project is trusted |
| `format_on_write` | `false` | Runs the file's formatter after `write` and `edit`; see [format on write](#format-on-write). Project config: trusted projects only |
| `format` | `{}` | Formatter command per file extension. Project config: trusted projects only |
| `check_on_write` | `true` | Runs the file's checker after `write` and `edit` when it is installed; see [checks on write](#checks-on-write) |
| `check` | `{}` | Checker command per file extension, replacing the built-in one; empty turns the check off. Project config: trusted projects only |
| `check_report` | `"delta"` | What the lead is told when it stops: findings on the lines the turn changed (`delta`), every finding (`all`), or nothing (`off`) |
| `read_guard` | `"range"` | What an agent must have read of a file before it changes it: the lines an edit changes (`range`), any part of the file (`file`), or nothing (`off`); see [the read guard](#the-read-guard) |
| `scan.secrets` | `true` | Scans the project with `gitleaks` once per session; see [secret scan](#secret-scan). Project config: trusted projects only |

To take a tool away from one kind of agent only, use `tools` and `disallowed-tools` in its definition; see [team](team.md).

## Shell resolution

With `shell` empty, the first of these that exists is used:

| System | Order |
|---|---|
| Linux, macOS | `bash`, then `sh` |
| Windows | `bash.exe` on `PATH` (the WSL launcher in `System32` is skipped), then the `bash.exe` shipped with Git for Windows, then `pwsh`, then `powershell` |

POSIX shells run the command with `-c`; PowerShell runs it with `-NoProfile -NonInteractive -Command`. The `shell` tool's description tells the model which shell and operating system it is writing for. `azazel doctor` shows the resolved shell.
