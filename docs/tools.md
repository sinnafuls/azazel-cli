# Tools

Tools are what an agent can do besides writing text: read and change files, run commands, search the web, drive a browser, start other agents. This page lists every built-in tool with its input fields and what it does, and covers the rules they share: read limits, the read guard for edits, format on write, checks on write, calls that are mended, output truncation, the URL schemes `read` understands, and the `[tools]` settings.

Every call goes through the permission engine first; see [permissions](permissions.md). Each tool declares what a call touches (`read`, `write`, `exec` or `net`), and the permission mode decides from that whether you are asked. When all the calls in one assistant message are read-only they run at the same time; otherwise they run one after another in order.

Fields marked `?` are optional.

## Files

| Tool | Input | Behaviour |
|---|---|---|
| `read` | `path`, `offset?`, `limit?`, `pages?` | Reads a text file, an image, a PDF, a notebook, an Office or mail file, a directory or a `scheme://` address |
| `write` | `path`, `content` | Creates a file or replaces its whole content. Parent directories are created |
| `edit` | `path`, `edits: [{old, new, replace_all?}]` | Exact text replacement in an existing file |
| `patch` | `patch_text` | Adds, changes, moves and deletes files with one patch in the `*** Begin Patch` format; see [patch](#patch). Offered in place of `edit` to OpenAI GPT and Codex models |

`read`:

- Text comes back as `<line number><tab><text>`. Default 2000 lines per call; `offset` (1-based line) and `limit` page through longer files. Lines are cut at 2000 characters and one call returns at most about 256 KB. A partial read ends with `[lines 1-2000 of 5120; continue with offset=2001]`.
- Files over 64 MB are refused. Other binary files return an error with their size and type.
- PNG, JPEG, WebP and GIF files are returned as images, scaled down when their long side exceeds 1568 px.
- A PDF comes back as its text, each page under a `--- page N ---` line. One call returns at most 20 pages; `pages` picks them (`"3"` or `"2-6"`), and a partial read ends with `[pages 1-20 of 57; continue with pages="21-40"]`. A PDF without a text layer returns `this PDF has no extractable text (it is probably scanned)`. A PDF cannot be edited, so reading one does not count as having read it for `write`.
- A notebook (`.ipynb`) comes back cell by cell: `# cell 4 [code]`, the cell's source, then `--- output ---` and its text outputs, each cut at 2000 characters. An image output is named in a line such as `[image output: cell 4 image 1, image/png, 5120 bytes]`, and the first image of the cells shown is attached to the result (a model without vision gets a note in its place). `offset` and `limit` count cells; a partial read ends with `[cells 1-12 of 40; continue with offset=13]`.
- A Word, PowerPoint or Excel file and a mail message are converted; see [Office and mail files](#office-and-mail-files).
- A directory lists up to 500 entries: `name/` for directories, `name<tab>N bytes` for files.
- `~/` is expanded. Relative paths start at the agent's working directory.

### Office and mail files

`read` of a `.docx`, `.pptx`, `.xlsx`, `.msg` or `.eml` file (also `.docm`, `.pptm`, `.xlsm`) converts it into a directory under the project's artifacts directory, `~/.azazel/projects/<slug>/artifacts/<file name>-<hash>/`, and returns where that is, the files in it and an outline of the Markdown:

```text
/work/q3/report.docx is a Word document (36299 bytes). It was converted to Markdown in /home/you/.azazel/projects/q3/artifacts/report.docx-29d79928:
report.md	26 lines
images/image1.png	76 bytes

Outline of report.md; `L` is the line, which `offset` takes when reading this file or report.md:
L1	# Quarterly report
L5	## Revenue
L14	## Risks
L19	### Next steps
```

A Markdown file of at most 8000 characters follows whole. For a longer one, `read` the document again (or the Markdown file) with `offset` set to a line of the outline: the result is those lines of the Markdown, numbered like any text file. The outline holds at most 200 headings and the file list 40 files.

| File | The Markdown | Beside it |
|---|---|---|
| `.docx` | Headings (by the name of their style, so documents in any language work; a document with a title has its headings one level below it), paragraphs with bold, italic and links, bulleted and numbered lists, tables, text boxes | `images/`: the pictures, linked from where they stand |
| `.pptx` | `# Slide N: <title>` per slide in the order of the show, the points of each placeholder as a list, text boxes, tables, and the speaker's notes as `Notes: ...` | `images/`: the pictures |
| `.xlsx` | `# Sheet N: <name>` per sheet with its size and its first 20 rows and 12 columns as a table | `sheets/<N>-<name>.csv`: every sheet whole, a line per row (an empty row is an empty line) |
| `.eml`, `.msg` | The subject as the heading, `From`, `To`, `Cc` and `Date`, then `## Message` and `## Attachments` | `attachments/`: every attachment under its own name |

What is and is not converted:

- A spreadsheet cell is the value Excel last computed: a formula is its result, a date cell is `2026-03-01` or `2026-03-15 14:30:00`, a number is shown with the 15 digits Excel shows. A formula no program ever computed (a file written by a library) is the formula itself, `=SUM(B2:B3)`. Hidden sheets are converted and marked `hidden`; charts are not.
- Headers, footers, footnotes and comments of a document, and charts and diagrams of a presentation, are left out.
- The text of a message is its plain-text part; a message that only has HTML is converted from that, and an Outlook message that only has RTF from that. A message forwarded as an attachment of a `.eml` file becomes `attachments/message-N.eml`, which `read` takes like any other; a message inside a `.msg` file follows the outer one in the same Markdown file, one heading level down.
- The directory is named after the file and its content: the same file is converted once, and a changed one again. These files cannot be edited, so reading one does not count as having read it for `write`.
- A file that only has the extension (a text file called `notes.docx`) is read as the text it is. The old binary formats (`.doc`, `.xls`, `.ppt`) are not read.
- Files other than the Markdown are read with the usual tools: `read` an image or a CSV, `grep` the directory.

A build without the cargo feature `office` (part of the default build) reports these files as binary.

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

### Patch

`patch` takes one patch in the envelope OpenAI's GPT and Codex models are trained to write:

```text
*** Begin Patch
*** Add File: docs/new.md
+# New
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-    print("Hi")
+    print("Hello")
*** Delete File: old.txt
*** End Patch
```

- `*** Add File:` creates a file from the `+` lines that follow. An existing file is replaced, which needs it read whole, as for `write`.
- `*** Update File:` changes a file with `@@` hunks: lines starting with a space are context, `-` lines are removed and `+` lines added. The text after `@@` is a line looked for first, to tell apart places that look alike; a hunk's lines are looked for after it and after the previous hunk, exactly, then ignoring spaces at the ends of lines. `*** End of File` after a hunk ties it to the end of the file. A hunk of only `+` lines adds them after its `@@` line, or at the end of the file. `*** Move to:` right after the header moves the file as well; the new path must not exist.
- `*** Delete File:` deletes a file the agent has read.

Every file is checked before any is written: a hunk whose lines are not found fails the whole patch with the file and the hunk named (`src/app.py, hunk 2: its lines were not found, starting with ...; nothing was changed`), and so does a file the [read guard](#the-read-guard) stops. Each file then goes the way of a `write` or an `edit`: under its lock, through format on write, checks on write and the language server, and the agents that had read it are told. The result names each file with a letter (`A` added, `M` changed, `R` moved, `D` deleted) and carries what the formatter, the checker and the language server said of it. Permission is asked as for `write`, for every file the patch names, move targets included: the rules of `write` and `edit` apply to `patch` too, and the patch is allowed only when each file is.

`[tools] patch` decides who is offered it: with `auto` (the default), a lead or agent whose first model is an OpenAI GPT model (GPT-5 and later; not `gpt-4*` or `gpt-oss`) or a Codex model gets `patch` and no `edit`, every other model `edit` and no `patch`; `on` offers both to every model, `off` never offers `patch`.

### Format on write

A successful `write`, `edit` or `patch` runs the formatter for the file on the file (`[tools] format_on_write = true`, the default). The result then ends with `; formatted with rustfmt`, the diff shown is of the formatted file, and the agent is taken to know the formatted content, so its next edit is not refused as stale. A formatter gets 5 seconds. One that fails, times out or is missing adds a one-line note such as `; rustfmt failed: ...` and never fails the edit.

The formatter for an extension comes from `[tools.format]` when it has an entry:

```toml
[tools]
format_on_write = true

[tools.format]
rs = "rustfmt --edition 2024"
py = "black -q"
sql = "sqlfluff fix {file}"
md = ""                     # no formatter for Markdown
```

The command is run directly, without a shell. The file's path is appended, or put where `{file}` stands. Quote a program path that has spaces. An empty command leaves files of that extension alone.

For an extension with no entry, the first formatter of this table that takes the extension, that the file's project uses and that is installed formats the file. What the project uses is looked for from the file's directory up to the project root. The first group is used whenever the program is on `PATH`:

| Formatter | Extensions | Command |
|---|---|---|
| rustfmt | `rs` | `rustfmt <file>`, with `--edition` from the nearest `Cargo.toml` that states one |
| gofmt | `go` | `gofmt -w <file>` |
| zig | `zig`, `zon` | `zig fmt <file>` |
| terraform | `tf`, `tfvars` | `terraform fmt <file>` |
| shfmt | `sh`, `bash` | `shfmt -w <file>` |
| dart | `dart` | `dart format <file>` |
| mix (trusted projects) | `ex`, `exs`, `eex`, `heex`, `leex`, `neex`, `sface` | `mix format <file>` |
| gleam | `gleam` | `gleam format <file>` |
| nixfmt | `nix` | `nixfmt <file>` |
| ktlint | `kt`, `kts` | `ktlint -F <file>` |
| cljfmt | `clj`, `cljs`, `cljc`, `edn` | `cljfmt fix --quiet <file>` |
| dfmt | `d` | `dfmt -i <file>` |
| latexindent | `tex` | `latexindent -s <file>`; the file is written from what it prints, so it leaves no backup files |
| htmlbeautifier | `erb` | `htmlbeautifier <file>` |
| ormolu | `hs` | `ormolu -i <file>` |

The second group is used only when the project uses it:

| Formatter | Extensions | When | Command |
|---|---|---|---|
| biome | scripts, styles, `html`, `vue`, `svelte`, `json`, `jsonc`, `yaml`, `toml`, `xml`, `md`, `mdx`, `graphql` | a `biome.json` or `biome.jsonc` | `biome format --write <file>` |
| oxfmt | `js`, `jsx`, `mjs`, `cjs`, `ts`, `tsx`, `mts`, `cts` | the nearest `package.json` depends on `oxfmt` | `oxfmt <file>` |
| prettier (trusted projects) | as biome | the nearest `package.json` depends on `prettier` | `prettier --write <file>` |
| ruff | `py`, `pyi` | `ruff.toml`, `.ruff.toml`, ruff in `pyproject.toml`, or ruff in `requirements.txt` or `Pipfile` | `ruff format <file>` |
| uv | `py`, `pyi` | a `uv.lock`, and a uv that has `uv format` | `uv format -- <file>` |
| clang-format | `c`, `cc`, `cpp`, `cxx`, `c++`, `h`, `hh`, `hpp`, `hxx`, `h++`, `ino` | a `.clang-format` or `_clang-format` | `clang-format -i <file>` |
| ocamlformat | `ml`, `mli` | an `.ocamlformat` | `ocamlformat -i <file>` |
| standardrb (trusted projects) | `rb`, `rake`, `gemspec`, `ru` | the `Gemfile` names the gem `standard` | `standardrb --fix <file>` |
| rubocop (trusted projects) | `rb`, `rake`, `gemspec`, `ru` | the `Gemfile` names the gem `rubocop` | `rubocop -a <file>` |
| pint (trusted projects) | `php` | the nearest `composer.json` requires `laravel/pint` | `vendor/bin/pint <file>` |
| air | `r` | the program's `--help` says it is the R formatter, not another program called `air` | `air format <file>` |

biome, oxfmt and prettier are taken from the project's `node_modules/.bin` first, and pint only from its `vendor/bin`; such a copy is the project's code, so it runs only in a trusted project, and an untrusted one falls back to the program on `PATH`. The formatters marked "trusted projects" run code of the project to format a file (a `prettier.config.js`, `.formatter.exs`, a `require` in `.rubocop.yml`) and are left out of an untrusted project. `azazel doctor` lists the formatters it would use for the project, and those it leaves out until the project is trusted.

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

## Guards

Three guards watch the quality of a turn. They are not about permission: they run in every [mode](permissions.md), for the lead and, where it says so, for the agents it starts.

```toml
[guards]
done_check = true   # send the lead back when it stops on unchecked changes
stuck_after = 3     # note on a shell command that failed this often in a row; 0 is off
runaway = true      # stop a reply that repeats itself
```

### Done-check

When the lead is about to stop, and `write` or `edit` changed files since your last prompt (the lead's calls or those of the agents it started), azazel looks for a check that ran after the last change. If there is none, the lead is told once, as a user message, and takes one more round:

```
<done-check>
2 files changed in this turn and nothing has run since that shows the change works: src/config.rs, src/main.rs
</done-check>
This is an automatic note, not a message from the user. Prove the change: run the project's tests, its build or its linter now and say what they showed. If nothing can check it, say plainly in your answer that the change is unverified.
```

The transcript shows `done-check: 2 files changed and nothing run since; the agent was asked to verify`. A change counts as checked when, after it:

| What ran | Examples |
|---|---|
| A `shell` command that is, or contains, one of `[verify] commands` | whatever your project's checks are called |
| A test runner, compiler, linter or type checker | `pytest`, `jest`, `tsc`, `eslint`, `ruff`, `mypy`, `make`, `shellcheck` |
| A build tool told to build, test, check or run | `cargo test`, `cargo clippy`, `npm run lint`, `go vet ./...`, `./gradlew build`, `cmake --build build` |
| The program itself | `python script.py`, `node dist/index.js`, `./run.sh`, `cargo run` |
| [Checks on write](#checks-on-write), on every changed file, without an error | the built-in checkers or `[tools.check]` |
| `goal_done` | the [goal](goals.md)'s own verification |

- The command has to have run, not passed: an agent that saw its tests fail knows where it stands. A check that ran before the last change does not count.
- Prose is not checked: files ending in `.md`, `.mdx`, `.markdown`, `.txt`, `.rst`, `.adoc` or `.org` never cause the note.
- Only changes made with `write` and `edit` are seen. What a `shell` command changes (a formatter, `sed -i`) or `ast_grep` applies is not.
- The note comes once per prompt, after the [report of the checks on write](#the-report-when-the-lead-stops) and before a goal's check and your `stop` hooks. With `[verify] on_stop = true` azazel runs the project's checks itself at every stop, and there is no done-check.

### Stuck

A `shell` command that ends with an exit code other than 0, is killed or times out has failed. When an agent's same command fails `stuck_after` times in a row within one prompt, the result of that call ends with:

```
[stuck] this command has failed 3 times in a row in this turn. Running it again will fail the same way: form a new hypothesis about the cause and test that before trying this again.
```

- Commands are compared without their white space and without what only shapes their output: a trailing `2>&1`, a last `| tail ...` or `| head ...`.
- The note comes again at every further multiple (the 6th failure, the 9th). A run of the command that succeeds starts the count again, and so does your next prompt.
- Each agent has its own counts.

### Runaway

A model can fall into a loop and write the same paragraph over and over until its output limit. While a reply is written, azazel looks at its end: when the same block of 200 characters or more stands there three times in a row, the reply is stopped like a reply that matched a [stream rule](rules.md#stream-rules): the request is cancelled, nothing of the reply is kept or stored, the model is told what it repeated, and the request is sent again. The transcript says `rule runaway stopped the reply; retrying`.

- A short phrase repeated often enough to fill three such blocks counts too. Rulers, blank lines and other runs without a letter or a digit do not, and neither do lines that share a shape but differ, such as the rows of a table.
- The model's thinking is watched the same way.
- At most two replies are withdrawn per prompt, stream rules included; after that the reply stands.

## Search

| Tool | Input | Behaviour |
|---|---|---|
| `glob` | `pattern`, `path?` | Finds files by name, newest first, at most 1000 |
| `grep` | `pattern`, `path?`, `glob?`, `ignore_case?`, `context?`, `mode?`, `limit?`, `multiline?` | Searches file contents with a Rust regular expression |
| `ast_grep` | `action`, `pattern`, `lang?`, `paths?`, `rewrite?`, `max_matches?`, `apply?` | Searches code by its syntax tree, or rewrites what matches. Only when [ast-grep](https://ast-grep.github.io) is installed |

Both honour `.gitignore` and other ignore files, include hidden files and skip `.git`. In `glob`, `*.rs` matches at any depth, `src/*.rs` only directly in `src`, and `src/**/*.rs` anywhere below it. `grep` modes are `content` (default, `path:line:text`), `files` (paths only) and `count` (`path:N`); `limit` defaults to 200 results, `context` adds lines around each match, `multiline` lets a pattern span lines. Binary files are skipped and matching lines are clipped at 500 characters.

A `grep` over a directory passes [secret files](permissions.md#secret-files) by (`.env`, private keys and the like) and ends with `[1 secret file was not searched; name it as `path` to search it]`. Naming the file as `path` asks you, as reading it does; a file you allowed for the session is searched like any other, and so is every file in `bypass` mode.

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
| `shell` | `command`, `cwd?`, `timeout_secs?`, `background?`, `interactive?`, `wake_on_exit?`, `description?` | Runs a command in the resolved shell |
| `job` | `action` (`output`, `kill`, `list`), `id?`, `tail?` | Manages background jobs started by `shell` and background Luau scripts started by `eval` (`lua-1`, ...) |
| `pty` | `action` (`send`, `read`, `stop`, `list`), `id?`, `text?`, `keys?`, `submit?`, `wait?`, `wait_for?`, `timeout_secs?` | Works with a program `shell` started in a terminal; see [programs in a terminal](#programs-in-a-terminal) |

`shell` returns stdout and stderr together, then `exit code: N`. Long output keeps its first 100 and last 300 lines; lines over 4000 characters are cut. On timeout (default `shell_timeout_secs`) or abort the whole process tree is killed. With `background: true` the call returns a job id (`job-1`, ...) at once; `job output` returns the last `tail` lines (default 100, a job keeps its last 2000) and the job's state. Background jobs are killed when the session ends. A background Luau script (`eval` with `background: true`) is a job too: `job output` shows what it printed and its state, `job kill` stops it, and it is stopped when the agent that started it ends; see [`eval`](#eval). Processes started for a sub-agent can never push to a git remote; the lead's can; see [permissions](permissions.md#sub-agents-never-push).

### Programs in a terminal

A program that asks questions, waits for keys or draws a screen (an editor, a REPL, an installer, `git rebase -i`, `ssh`, a login that wants a password) has nothing to talk to in an ordinary `shell` call and hangs until the timeout. `shell` with `interactive: true` runs the command in a real terminal instead: a pseudo-terminal (ConPTY on Windows) whose screen azazel emulates. The call does not wait for the program to end. It returns once the program has drawn something and the screen has been still for a moment:

```
started pty-1 in a terminal; read its screen, type into it and stop it with the pty tool
? package name: (demo)
[pty-1: running for 0s; screen 100x30, cursor at row 1 column 24]
```

The agent then works with it through `pty`:

| Action | Input | What it does |
|---|---|---|
| `read` | `id?`, `wait?`, `wait_for?`, `timeout_secs?` | The screen as text, with the state of the program. Without a wait it answers at once. `wait: "change"` waits until the screen has changed since the agent last saw it, `wait: "exit"` until the program has ended, `wait_for: "text"` until that text is on the screen. A wait gives up after `timeout_secs` (30 by default, at most 1800) and says so; that is not an error |
| `send` | `id?`, `text?`, `keys?`, `submit?` | Types `text` (a line break in it presses Enter), then presses each of `keys` in order, then Enter when `submit` is true. Returns the screen once it has settled |
| `stop` | `id?` | Ends the program. Its last screen stays to be read |
| `list` | | Every terminal of the session with its state and command |

- `id` may be left out while only one terminal is open.
- Keys are `enter`, `tab`, `esc`, `backspace`, `delete`, `insert`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`, `space`, `f1` to `f12` and single characters, with `ctrl+`, `alt+` or `shift+` in front: `ctrl+c`, `alt+x`, `shift+tab`, `ctrl+left`. They are sent as xterm sends them, and the arrow keys follow the mode a full-screen program sets.
- The screen is 100 columns by 30 rows until the [interface shows it](tui.md#terminals), which gives the program the size of its box. The status line of a result has the size and where the cursor is.
- With `wake_on_exit: true` the agent need not watch the program. When it ends, the lead gets a message with the last screen and the address of the log: at once if it is working, as a new prompt if it is idle. Only the lead can be woken; a sub-agent waits with `read` and `wait: "exit"`.
- Everything the program printed is saved: the rows that scrolled off the screen as they do, and the last screen when the program ends. A result names the log once the program has ended (`artifact://pty-<start time>-<n>.log`), and `read` takes that address. What a full-screen program (an editor, a pager) scrolled within its own screen is not output and is not kept.
- You see the terminal live and can type into it yourself; see [terminals](tui.md#terminals). While you have the keyboard an agent's `send` waits (up to two minutes) for you to hand it back, and the next result the agent gets says that you typed.
- At most eight programs run in terminals at once. They are ended when the session ends. `timeout_secs` of `shell` does not apply to an interactive command, and `background` cannot be combined with `interactive`: the program goes on after the call returns anyway.
- An agent whose definition lists its tools needs `pty` beside `shell` to work with what it starts, as it needs `job` for background jobs.
- A terminal exists in the process the session runs in. A session that runs elsewhere (a background session, `--ssh`) still gives its agents terminals, but the interface attached to it does not show them.

Permissions are those of `shell`: starting the program is a `shell` call like any other, typing into it and stopping it are allowed where running it is, and what is typed is checked like a command. See [permissions](permissions.md#programs-in-a-terminal).

## Code execution

| Tool | Input | Behaviour |
|---|---|---|
| `eval` | `language` (`python`, `javascript`, `luau`), `code`, `timeout_secs?` | Runs a Python or JavaScript program and returns what it printed, or a Luau script that calls other tools; see [`eval`](#eval) |

`eval` with `language: "luau"` lets an agent do a job of several steps in one call: read a dozen files and return only the lines that matter, search and then filter, make the same edit in many places. Only the script's result reaches the model, not what the tools returned along the way.

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
- Every call is made as the agent that runs the script and goes through the permission engine, the mode and the hooks like a call the agent makes itself. A Luau `eval` counts as `read` (Python and JavaScript count as `exec`); each call inside the script is judged on its own, so a script cannot do what the agent could not.
- `print(...)` output and the value the script returns are the result. A returned table comes back as JSON. `json.encode` and `json.decode` are available.
- The script runs in a fresh sandboxed Luau VM: no file, process or network access except through `tools`.
- Limits: 120 seconds of the script's own running time (waiting for a tool, an agent or a model does not count), 50 calls of tools, agents and `classify` together, 64 KB of result. A longer result is cut and kept whole as `artifact://<call id>-code`.
- `eval` itself, `task` and the other team tools cannot be called through `tools`; a script starts agents with `agents.run`.
- `tools.run(name, input)` runs one of the user's [saved scripts](skills-and-commands.md#saved-scripts) in a VM of its own, with `input` (any JSON value) as the global `input`, and returns its first returned value, or `nil, message` when the script is missing, fails or is nested too deep. Its tool calls are judged by the permission engine one by one like the caller's and count against the caller's 50 calls and running time. Saved scripts may run others up to 3 levels deep. Only the user saves scripts, with `/script save`; the agent cannot.
- `tools.search(query)` also returns matching saved scripts, as `{ name, description, kind = "script" }`, and `tools.describe(name)` describes one (`kind`, `description`, `input`, `signature`).
- With `background: true` the script runs as a job instead; see [`eval`](#eval) for what changes.

The tool's description lists the names of the callable tools only, within about 2000 characters, because the agent already has every tool's schema in its tool list; a script gets a tool's arguments with `tools.describe(name)`, and every tool the agent has is callable by name. The tool is there when azazel is built with the `lua` feature (the default), also with `--no-extensions`.

Beside `tools`, `print` and `json` a script has these:

| Global | What it does |
|---|---|
| `parallel{ f, g, ... }` | Runs the functions at the same time and returns what each returned (its first value), in the same places: a list gives a list, a table with names keeps its names. At most `team.max_parallel` (4) run at once. A call that only reads runs beside the others; one that changes something (`write`, `edit`, `shell`) waits until it is alone, as it does among an agent's own calls. All of them are waited for; if any raised an error, the first is then raised as `parallel: entry <key>: ...`. Use `pcall` inside a function to keep the results of the others |
| `agents.run(agent, prompt, { model?, isolation?, schema?, name? })` | Starts one agent of the [team](team.md) and waits for it. The options are those of `task`. Returns the agent's report and its id; with a `schema` the first value is the [typed result](team.md#typed-results) as a table. The call is a `task` call for the permission rules and the hooks, and only an agent that has `task` can make it. An agent that fails or is stopped raises an error with what it said |
| `classify(question, { "a", "b", ... }, { model? })` | Has a small model pick the one of 2 to 20 choices that answers the question. Returns `{ choice, index, confidence, reason, model }`: the choice as given, its position from 1, how sure the model is from 0 to 1, a sentence of why, and the model that answered. The [`judge` role](models-and-logins.md#roles) answers unless `model` names a model, a role or a list of them. The request carries the question and the choices and nothing of the conversation, so put what is to be judged into the question. Its cost is in `/cost` under `classify` |
| `store(key, value)`, `load(key)` | Keeps a JSON value (up to 256 KB) for later scripts, and reads it back; `nil` when there is none. Storing `nil` forgets the key. The value is an entry of the session file, so it is there when the session is resumed, and it follows the session's tree: after `/tree`, `/rewind` or a fork a script sees what was stored on the branch it is on. Every agent of a session reads and writes the same values |
| `tools.search(query, limit?)` | Finds tools by what they do: `{ name, description }` of the best matches (8 unless `limit` says otherwise, at most 25), with the first sentence of each description. It also finds the tools of MCP servers that are held back until searched for (see [MCP](mcp.md)); what it finds is callable at once |
| `tools.describe(name)` | `{ name, description, parameters, signature }` of one tool: its whole description, its JSON Schema, and its arguments on one line. `nil` when the agent has no such tool |

An audit over many files keeps the intermediate work out of the conversation:

```lua
local files = load("audit_files") or {}
local jobs = {}
for i, path in ipairs(files) do
    jobs[i] = function()
        return agents.run("scout", "List the unchecked error paths in " .. path, {
            schema = {
                type = "object",
                properties = { findings = { type = "array", items = { type = "string" } } },
                required = { "findings" },
            },
        })
    end
end
local serious = {}
for i, result in ipairs(parallel(jobs)) do
    for _, finding in ipairs(result.findings) do
        local verdict = classify("Can this lose data?\n" .. finding, { "yes", "no" })
        if verdict.choice == "yes" and verdict.confidence > 0.7 then
            table.insert(serious, files[i] .. ": " .. finding)
        end
    end
end
store("audit_serious", serious)
return serious
```

### `eval`

`eval` runs a program the agent writes, for work a script does better than a shell command: computing, parsing and reshaping data, trying out a snippet.

- `language: "luau"` runs a script inside azazel that calls other tools; see [Code execution](#code-execution) for what it can do and its limits. It needs no program on `PATH`, `timeout_secs` does not apply to it, and it counts as `read` for the permission rules (each call inside it is judged on its own), where Python and JavaScript count as `exec`. A rule names it: `allow = ["eval(luau)"]`.
- `language: "luau"` with `background: true` returns a job id (`lua-1`, ...) at once and runs the script as a job. `job output` shows what it printed and whether it is running, `job kill` stops it, and when it ends the agent is woken with a `<job-exit id="lua-1">` message that holds its result and `[lua-1: finished]` (or `failed`, `killed`, `timed out`), as for a program in a terminal that was started with `wake_on_exit`. `Esc` and an abort of the turn leave it running; `job kill`, the end of the agent that started it (a sub-agent finishing) and the end of the session stop it, after the usual 2 second grace. It also stops after `[tools.eval] background_max_secs` of wall-clock time (default 7200, `0` for no limit); the 120 second running-time and 50-call limits apply as before. When the permission engine would ask about a tool call inside a background script, the script pauses (the job shows `waiting for approval of <tool>`) and you are asked, with the job named in the request. Once you allow it the script runs again from the start: the calls it made before (tools, `agents.run`, `classify`, `store`, `load`, `step`) are replayed from a log, not repeated, and it goes on at the call you approved. A script that pauses must therefore make the same calls with the same arguments in the same order; wrap `os.time`, `os.clock`, `math.random` and the like in `step(name, fn)`, which keeps the value of its first run. A script that does not repeat itself fails with `replay diverged at call N`. If you deny the call the script ends as failed, with your feedback in its result. A result over 256 KB, or a log over 8 MB, stops the script from pausing: the call is then refused, with the reason. Time spent waiting for your answer does not count against `background_max_secs`. With `[tools.eval] background_approval = "deny"`, or when nobody can answer (`azazel -p`), the call is refused instead, with a message saying it needed approval; run the script in the foreground or allow the call with a rule. `background` is offered only when Luau is on, and is refused for Python and JavaScript. `azazel -p` waits for running background scripts to wake the lead before it exits, for at most the script's cap, and never more than 2 hours.
- Python runs with the first of `python`, `python3` and `py` on `PATH` (on Windows the `WindowsApps` stand-ins that open the Store are passed over). JavaScript runs with `node`, else `bun`, else `deno`; top-level `await` works. The program is looked for at each call; when none is installed the result says which to install.
- The code is handed to the interpreter on standard input and runs in the agent's working directory with the environment of `shell` commands. Each call is a new process: nothing is kept from one call to the next, so a later call reads what an earlier one left in a file.
- `AZAZEL_LOCAL` names the session's scratch directory, the files `local://<name>` addresses (see [URL schemes](#url-schemes-read-accepts)).
- stdout and stderr come back together, the first 100 and last 300 lines of them, with the exit code; a code other than 0 makes the result an error. The output streams into the running card, `Esc` kills the program, and so does the timeout: `timeout_secs`, else `shell_timeout_secs` (120).
- It can do whatever a program can, so it is approved like `shell`: `manual` and `accept-edits` ask, `auto` runs it, plan mode refuses it. A rule names the language: `allow = ["eval(python)"]`.
- `[tools.eval]` turns each language on or off: `python = false`, `javascript = false` or `luau = false` (all are on by default; also on the Tools page of `/settings`). A language that is off is not offered to the model and is refused if asked for; with all off, `eval` is not offered at all. `luau` has an effect only in a build with the `lua` feature (the default); without it Luau is never offered. A project's config may set them only once the project is trusted. `background_max_secs` is the wall-clock cap of a background Luau script, in seconds (default 7200, `0` for no limit; trusted projects only). `background_approval` is what a background script does at a call that needs approval, `pause` (default) or `deny`; `background_pause_secs` is the longest a paused script waits for your answer (default `0`, until the session ends); both are trusted projects only.

## Todo list, questions, plans

| Tool | Input | Behaviour |
|---|---|---|
| `todo` | `items: [{text, status}]` | Replaces the agent's task list. `status` is `pending`, `in_progress` or `done`; at most one item may be `in_progress` |
| `ask_user` | `question`, `type?`, `context?`, `code?`, `diff?`, `options?`, `multi?`, `recommended?`, `allow_freeform?`; or `questions`; `async?` | Asks you a question, or several in one form, and waits; see below |
| `plan_submit` | `path` | In plan mode: submits a plan file under `.azazel/plans/` for your approval. See [permissions](permissions.md) |

### The task list

The list is shown above the input while work on it remains, and `/todo` shows or changes it. Two things keep an agent on its plan:

- **It is durable.** The lead's list is rebuilt from the session whenever one is opened: after `-c`, `--session`, `--fork`, `/resume`, `/tree`, `/rewind` and `/fork` it is what the last `todo` call that succeeded on that branch set (or what you changed with `/todo` after it). A compacted session keeps its list. A new session starts with none.
- **The agent is reminded of it.** While the list has an item that is not `done`, every request ends with a note such as `[Plan] 1/3 done`, followed by the list as `[x]`, `[~]` and `[ ]` lines. The note is added to what is sent, after the conversation and beside the date and the mode, and is not part of the session, so it costs no context later and leaves the [cached part](models-and-logins.md#prompt-cache) of the request as it is. It stops when every item is done or the list is cleared.

### `ask_user`

| Input | Meaning |
|---|---|
| `question` | What is asked |
| `type` | `select` (one of the options), `multiselect` (any number of them), `text` (a typed answer) or `confirm` (yes or no). Left out, it follows from the rest: `text` without `options`, `multiselect` with `multi: true`, otherwise `select` |
| `context` | What you need to know to answer; shown above the question |
| `options` | The choices of a `select` or `multiselect`: 2 to 8, each different from the others whatever its case and spacing. Not allowed for `text` and `confirm` |
| `code`, `diff` | The code, or the unified diff, the question is about: shown between the context and the question, the code numbered, the diff coloured. One of the two at most |
| `recommended` | The answer the agent would give: one of the `options`, or `yes` or `no` for a `confirm`. It is marked `recommended` and is the row the selection starts on. Not allowed for `text` |
| `allow_freeform` | `select` only: the list ends with `Other…`, which lets you type an answer instead |
| `questions` | Instead of `question`: up to 6 questions, each with the fields above and an optional `header` (a word or two, at most 24 characters, that names it in the form). They are answered together in [one form](tui.md#popups) |
| `async` | The call returns at once; the answers reach the agent later as a message |

A call that breaks one of these rules fails before you are asked anything, with what is wrong (`a `select` question needs 2 to 8 options, not 1`, `the option `red` is offered twice`, `question 2: `recommended` names `green`, which is not one of the options`).

The result of one question is the chosen option, the chosen options one per line, the typed text, or `yes` or `no`. The result of several is each question, numbered, with its answer under it; a question you left open reads `(no answer)`:

```text
1. Which store should the service use?
postgres

2. Anything else I should know?
(no answer)
```

When you dismiss the question or the form, or nobody is there to answer (print mode, a session without a client), the result is `the user declined to answer`. That is an answer, not a failed call: the agent goes on without one. Over [ACP](acp.md) a question that needs typing cannot be asked: alone it is declined the same way, in a form it is left open.

With `async: true` the agent does not wait. The call returns `The question is with the user. The answer will reach you as a message; go on with what does not depend on it.` and the question is shown to you like any other. When you answer, the agent that asked receives the answers between `<user-answer>` tags: as a steer message before its next request while it works, as a new prompt when it has stopped. Dismissing the question sends `the user declined to answer` the same way. Use it for a decision that is needed later, while there is other work to do first.

## Schedules

| Tool | Input | Behaviour |
|---|---|---|
| `schedule` | `action` (`add`, `list`, `pause`, `resume`, `remove`), `when?`, `prompt?`, `session?` (`new`, `this`), `if?`, `until?` (each `{run, is?, matches?, changes?}`), `ends?` (`{after?, at?, times?}`), `tz?`, `id?` | Adds, lists, pauses, resumes and removes the [schedules](background-and-remote.md#schedules) of the project |

`add` takes `when` as `azazel schedule add` does (five cron fields, `every 10m`, `at +2h`, `at 17:30`),
or `on ci-failure`, `on pr-comment`, `on slack:<channel id>` and `on file:<glob>` for a [trigger](background-and-remote.md#triggers), and the
`prompt` a run is given. A run starts a background session that knows nothing of the conversation, so
the prompt has to stand on its own; `session: "this"` sends every run to the session the agent works in
instead. The runs work as the session does at that moment: its model, thinking level, permission mode
and settings.

`if`, `until`, `ends` and `tz` are the [checks and ends](background-and-remote.md#checks-and-ends) of
`azazel schedule add`: `if` lets the prompt run at a due time only when its command holds, `until` ends the
schedule once its command holds, `ends` stops it after a length of time (`after`), at a time (`at`) or
after a number of runs (`times`), and `tz` is the zone of a cron expression or `at` time.

`add`, `pause`, `resume` and `remove` run things later with nobody watching, so they are asked about like a
command (access `exec`): in `manual` and `accept-edits` mode you approve them, plan mode refuses them, and a
run without a user refuses them unless a rule allows `schedule`. The command of each check of an `add` is
asked about besides, as a `shell` call of it would be: the rules for `shell` and the mode decide, a deny
rule for `shell` refuses the call, and a destructive command is asked about in every mode. `list` only
reads. The tool is not in
builds without background sessions.

## Recalling the conversation

| Tool | Input | Behaviour |
|---|---|---|
| `session_recall` | `query?`, `regex?`, `range?`, `expand?`, `touched?`, `limit?` | Reads the agent's own conversation from the session file, including what [compaction](sessions-and-checkpoints.md#compaction) summarised away and tool results that were elided |

A long session keeps only a summary of its beginning and short notes in place of old tool results. The session
file keeps everything, and `session_recall` reads it: the branch the conversation is on, from its first message
to the newest. The lead reads the session; a sub-agent reads its own transcript. A call changes nothing, so it
runs beside other read-only calls and may be repeated freely.

| Input | Meaning |
|---|---|
| `query` | Words to look for, case-insensitive. Entries that hold more of the words come first, then those that hold the rarer words and hold them more often; ties go to the newest |
| `regex` | A regular expression to look for instead; matches come in the order of the conversation |
| `expand` | Entry ids to return in full |
| `range` | `[from, to]`: every entry between two ids, in full. An empty id is the start or the end of the conversation |
| `touched` | `true` lists the files `read`, `write` and `edit` touched. A file that was read comes with the entries that hold what was read; a file that was written, with the entries whose call changed it. Failed calls are left out |
| `limit` | Most entries returned: 10 for a search (at most 50), 20 for a range (at most 100) |

Several inputs may be given in one call; each answers in its own section. A search returns one line per entry,
`<id>  <who or what>: <the first matching line>`, and says how many entries matched when there are more than
`limit`. An entry in full starts with `── <id> · <who or what> · <time> ──`. A range that is cut ends with
`[N more entries; continue with range=["<id>", "<id>"]]`. A search does not find the results of earlier
`session_recall` calls, which would only echo it; `expand` still returns them.

Entry ids are the ones the session file uses. An agent meets them in three places: an elided result
(`[result elided: 8140 bytes; session_recall expand=1a2b3c4d]`), the note that ends a summary, and the ids in
square brackets the summary puts after facts it shortened.

## Web

| Tool | Input | Behaviour |
|---|---|---|
| `web_search` | `query` or `queries` (at most 5), `limit?`, `site?`, `recency?` | Numbered results with title, URL and snippet; several queries run at once and get a section each. `limit` defaults to 8 (at most 20); `recency` is `day`, `week`, `month` or `year`. Answered by the search tool of the lead's provider when it has one, else by the configured backend; see [web and browser](web-and-browser.md#the-providers-own-search) |
| `web_fetch` | `url`, `format?`, `offset?`, `limit?` | Fetches an http or https URL. `format` is `markdown` (default), `text` or `raw`. A long page comes in parts: `offset` is the first line, `limit` the most characters (default 30000). A PDF comes back as its text, a GitHub address as what it names (a repository is cloned), a YouTube video as its captions |
| `web_grep` | `url`, `pattern`, `ignore_case?`, `literal?`, `context?`, `limit?`, `format?` | Searches the page at a URL line by line and returns the matching lines with their numbers, from the same cache as `web_fetch` |
| `http` | `url`, `method?`, `headers?`, `body?` or `json?`, `service?`, `limit?`, `timeout_secs?` | One request to a web API: status, response headers and body. The credential stored for the host with `azazel secrets add` is added to the request and taken out of the result; the agent never sees it |

Details, search backends, stored credentials and limits: [web and browser](web-and-browser.md).

## Language servers

| Tool | Input | Behaviour |
|---|---|---|
| `lsp` | `action`, `path`, `line?`, `column?`, `query?`, `new_name?`, `symbol?`, `occurrence?`, `new_path?` | `action` is `diagnostics`, `definition`, `references`, `hover`, `symbols`, `outline`, `read_symbol`, `rename` or `rename_file`. Positions are 1-based. `rename` and `rename_file` change files; the rest only read |

See [lsp](lsp.md).

## Debugger

| Tool | Input | Behaviour |
|---|---|---|
| `debug` | `action`, `program?`, `args?`, `cwd?`, `language?`, `stop_on_entry?`, `breakpoints?`, `file?`, `line?`, `condition?`, `clear?`, `kind?`, `expression?`, `frame?`, `timeout_secs?` | A debugger over the [Debug Adapter Protocol](https://microsoft.github.io/debug-adapter-protocol/); one session per agent. `action` is `start`, `break`, `continue`, `step`, `pause`, `state`, `eval`, `output` or `stop` |

`start` runs `program` (an executable built with debug info, a test binary or a script) with `args` in `cwd` and waits until it stops, ends or the time limit passes. `breakpoints` (`file`, `line`, `condition?`) are set before it runs; `stop_on_entry` stops it before its first line. `break` adds a breakpoint at `file` and `line` (stopping only when `condition` is true), or with `clear` removes the one at `line`, or all of `file`'s. `continue`, `step` (`kind` is `over`, `in` or `out`) and `pause` wait for the next stop. `state` reports where the program is stopped; with `timeout_secs` it waits for the next stop. `eval` evaluates `expression` in `frame` (0, the top, by default). `output` is what the program printed; `stop` ends the session, which also ends when the agent does.

A stop is reported as its reason, the thread, the stack (up to 12 frames, `#0 inner at src/main.rs:5`) and the locals of the top frame (up to 30, each value cut at 160 characters), followed by what the program printed since the last result. A wait lasts `debug.timeout_secs` (30) unless `timeout_secs` says otherwise, and Esc cancels it; a program still running then is reported as running, and `pause` stops it.

The adapter is found on `PATH` when a session starts: `lldb-dap` (or `lldb-vscode`, or CodeLLDB's `codelldb`) for `rust`, `c` and `cpp`, `python -m debugpy.adapter` for `python`, `dlv dap` for `go`. `language` is guessed from the program (`.py` and `.go`; anything else is native). A missing adapter is an error that names the command that installs it. `[debug.adapters.<language>]` names another command; it is a [gated setting](configuration.md#trust).

Starting a program and evaluating an expression (which may call functions of the program) are `exec`: they are asked about in `manual` mode like `shell` and refused in plan mode. The other actions only read. Without the `debug` Cargo feature the tool is not registered.

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

## Repository map

Part of the default build. A line scanner finds the definitions and their signatures in Rust, Python, Go, JavaScript and TypeScript files; building with `--features repomap-treesitter` reads them with tree-sitter grammars instead, which is more exact and adds 5.3 MB to the binary. `repo_map` reads the Rust, Python, Go, JavaScript and TypeScript files the project's `.gitignore` and `.ignore` files do not leave out (not generated files, not files over 512 KB), ranks the definitions by how the other files refer to them, and writes the best ones that fit a budget. What was read from each file is kept by modification time in `repomap.json` in the project's data directory, so a second run reads only the files that changed. `/repomap` shows the map to you; `/repomap refresh` also rebuilds the copy that `[context] repo_map_tokens` puts in the system prompt.

| Tool | Input | Behaviour |
|---|---|---|
| `repo_map` | `focus?`, `tokens?` | An outline of the project: each file as `path:` followed by indented signature lines, with `⋮` where lines were left out. `focus` lists paths (files or directories, relative to the project root) or names of definitions to favour; `tokens` is the budget and defaults to 1024 |

## Team

See [team](team.md). The lead has all of these. A sub-agent always has `agent_send`; it gets `task`, `agent_stop`, `agent_wait`, `agent_list` and `agent_status` only when its definition lists `spawns` and the depth limit allows; the `worktree_*` tools are the lead's only.

| Tool | Input | Behaviour |
|---|---|---|
| `task` | `agents: [{agent, prompt, name?, model?, isolation?}]`, `wait?` | Starts the listed agents in parallel. With `wait` (default true) returns every report; with `wait: false` returns at once, naming the agents that started and those queued for a free `team.max_parallel` slot, and each report arrives later as a message. A `wait` call with a queued agent returns at once the same way; a queued agent's start also arrives as an `<agent-started>` message. What the user types while it waits ends the wait, and so does `team.wait_secs` (default 300, `0` = no limit); the agents still running report later as messages |
| `agent_send` | `to`, `message`, `ask?` | Messages an agent id, `parent` or `all` and returns at once with how it arrived: `delivered at its next step`, `woke it` (a finished agent resumes with the message) or `queued`. With `ask: true` the agent answers the question on the side, from its conversation, without stopping or recording it; the answer is the result |
| `agent_stop` | `id`, `reason?` | Stops an agent the caller started, and the agents it started |
| `agent_wait` | `ids?`, `timeout_secs?` | Waits (default 600 s) until a started agent has a new result, a message arrives, or the user types |
| `agent_list` | none | Id, definition, state (`queued` while it waits for a slot), activity, time, tokens and cost of every agent |
| `agent_status` | `id` | What an agent (or `all`) is doing, without asking a model: state (`queued` while it waits for a slot), time, the tool it runs now, its last lines, the files it changed, its worktree's changes and commits, tokens and cost |
| `worktree_apply` | `agent` | Brings an isolated agent's changes into the main tree and removes its worktree. On a conflict the worktree is kept |
| `worktree_discard` | `agent` | Removes an isolated agent's worktree and branch |
| `report` | `result` | Hands in the agent's result as a value that matches the schema it was given; a value that does not match is refused with what is wrong. Offered only to an agent that owes a [typed result](team.md#typed-results) |

## Goals

Offered to the lead only, and only while a goal is open. See [goals](goals.md).

| Tool | Input | Behaviour |
|---|---|---|
| `goal_done` | `summary`, `evidence` | Claims the goal is met. The checker runs; the result is `verified` or what is still wrong |
| `goal_blocked` | `reason` | Pauses the goal because the agent needs you |

## Browser and screen

| Tool | Input | Behaviour |
|---|---|---|
| `browser` | `action` plus `url`, `tab`, `ref`, `selector`, `x`, `y`, `text`, `submit`, `keys`, `hold_ms`, `dy`, `for`, `timeout_ms`, `poll_ms`, `expression`, `full_page`, `width`, `height`, `filter`, `ref_user`, `ref_password`, `reason` | Drives a Chromium browser. Actions: `open`, `navigate`, `back`, `reload`, `snapshot`, `click`, `hover`, `type`, `key`, `scroll`, `wait`, `eval`, `screenshot`, `resize`, `console`, `network`, `tabs`, `close`, `fill_login` (types a login stored with `azazel secrets add-site`), `handoff` (shows the browser to you and waits) |
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
| `tool_search` | `query`, `limit?` | Present only when there are too many MCP tools to list and the model cannot search for tools itself: finds tools by description and makes the hits callable. `limit` defaults to 8 (at most 25) |

See [mcp](mcp.md).

### Code-first mode

With `[tools] code_first = true` the model gets full schemas for a small core only: `read`, `edit`, `write`,
`shell`, `eval`, `ask_user`, `todo` and `task`, plus `patch` where it replaces `edit`, `plan_submit`, and the
goal and report tools of runs that must end themselves. Every other tool (search, web, LSP, MCP, memory, the
browser, and so on) is left out of the tool list and called from a Luau script in `eval`:

```lua
local hits = tools.grep{ pattern = "TODO", path = "src" }
return tools.read{ path = "src/main.rs" }
```

`eval`'s description lists those tools by name only, with the tools of each MCP server counted on one line
(`mcp__github (40 tools)`); `tools.search(query)` finds a tool and `tools.describe(name)` gives its arguments.
With `code_first_listing = "signatures"` the description lists one-line signatures instead, as many as fit in
about 6000 characters, and the rest by name; use it to measure what each listing costs. The description is
built once and stays the same for the whole session, so the provider's prompt cache holds. A script can also
call a tool that is in the list. Tools that need a person's decision (`ask_user`, `task`) stay direct calls.

- The list and the description are fixed for the session, so the provider's prompt cache stays valid. An MCP
  server that connects after the description was built is not in it; reach its tools with `tools.search` or
  `tools["mcp__server__tool"]`.
- `defer` and `tool_search` do not apply to hidden tools: they are not in the list at all.
- Permissions, hooks and the read guard apply to a call from a script exactly as to a direct one.
- Sub-agents follow the lead unless their definition has a `tools:` list, which is kept as written. The
  coordinator is never code-first.
- It needs Luau in `eval` (`tools.eval.luau = true`, in a build with the `lua` feature). Without it azazel
  logs a warning and ignores the setting.
- `tools/code-first-compare.py` runs one task several times in each mode with `azazel -p` and compares tokens,
  cache reads, cost and time from the usage log, so you can see whether the mode pays off with your model.

### Deferred tools

On Claude 4.5 and later through Anthropic's own API, the tools `[tools] defer` names are sent with
`defer_loading`: the API keeps them out of the prompt, the model finds them with the API's own tool search,
and the system prompt lists their names so it knows they are there. Finding a tool adds nothing to the tool
list, so the cached prompt stays valid. The default list holds every MCP tool (`mcp__*`) and built-in tools
most sessions never call (`browser`, `screen`, `debug`, `sprite_sheet`, `generate_*`, `schedule`, `pty`,
`http`, `list_mcp_resources`, `worktree_apply`, `worktree_discard`, `memory_forget`). With more MCP tools
than `[mcp] defer_threshold`, every MCP tool is deferred this way and `tool_search` is not offered.

On every other provider and model the tools are sent in full, as before; `defer = []` turns deferral off.

## Repeated reads, retries and time limits

Three settings of `[tools]` shape how the agent loop runs a call.

**Repeated reads.** With `cache_reads` (default on), a `read` of a file or directory, a `grep` or a `glob` that gives exactly the result it gave before, while that earlier result is still in the context, is answered with a one-line note instead of the result again:

```text
(unchanged since your identical `read` call 3 turns ago; its result above is still current)
```

The call still runs, so the read guard counts it, and the result is compared with the earlier one: a file that changed, whether by azazel's tools, `shell` or anything else, comes back in full. A note is used only when the earlier result is still in the context, not elided or summarised away, and only when it is much shorter than the result. Errors, images and `scheme://` addresses are never replaced.

**Retries.** A call that only reads (`web_fetch`, `web_search`, `web_grep`, `http` with GET, HEAD or OPTIONS, the read actions of `lsp`, and MCP tools that declare `readOnlyHint`) is run again, up to `retries` times (default 2, with a wait that doubles from half a second), when it fails with a timeout, a reset or closed connection, HTTP 429 or HTTP 500, 502, 503 or 504. Any other call is never run again. A result that took retries ends with `[succeeded after 1 retry]` or `[still failed after 2 retries]`.

### Keeping the machine responsive

Several agents building at once can take every core. Three settings keep your machine usable; each applies to what agents start, never to your own `!` commands.

- **Priority.** `[tools] priority` (`low` by default, also `idle` or `normal`) sets the operating-system priority of every process started for an agent: `shell` (also in the background), `pty` terminals, `eval`, the checks that run after a write, verify commands and hooks. Programs they start inherit it. On Windows it is `BELOW_NORMAL_PRIORITY_CLASS` (`IDLE_PRIORITY_CLASS` for `idle`); on Linux and macOS the program is started through `nice -n 10` (`19` for `idle`), and nothing changes when `nice` is not installed.
- **Build jobs.** `[tools] build_jobs` (`half` by default) sets `CARGO_BUILD_JOBS`, `MAKEFLAGS=-j<n>` and `CMAKE_BUILD_PARALLEL_LEVEL` in those processes: half of the logical cores, at least 2; or a number; `all` sets nothing. A variable your environment already sets is left as it is.
- **Build cache of worktrees.** `[team] worktree_target`; see [build output of worktrees](team.md#build-output-of-worktrees).

**Time limit.** A call of a tool that has no limit of its own (web, `http`, `lsp`, MCP tools) is stopped after `call_timeout_secs` (default 120; `0` is no limit) with an error that says it timed out, which counts as transient for a call that is retried. `shell`, `eval`, `pty` and sub-agents keep their own limits. `Esc` cancels a call at once, during a retry wait too.

## Output truncation and artifacts

A tool result whose text exceeds `max_result_bytes` (default 50000) is cut at that size. The full text is saved under `~/.azazel/projects/<slug>/artifacts/` and the result ends with:

```text
[output truncated: 181234 bytes in total] full output: artifact://<call id>
```

The agent reads the rest with `read artifact://<call id>`. The same directory holds other files tools produce: screenshots, recordings, downloads that are not text, worktree patches.

## Slimmer and digested results

Two settings keep large results from MCP tools, the web tools and PDFs from filling the context. Both decide once, when the result is produced, and what they leave is what every later request repeats: nothing is rewritten afterwards, so the prompt cache is not disturbed.

**Slimming** (`[tools] slim_results`, on by default, no model involved). A JSON result of an MCP tool, `web_fetch`, `web_search` or `http` of 2000 characters or more loses `node_id`s, `*_html` copies of text that is there as plain text, and null or empty values. When the JSON looks like it comes from GitHub or Linear (an object with `node_id`, `html_url` or `gravatar_id`, or a link to `api.github.com` or `linear.app`) it also loses colours, avatar, image and other media links, and links into an API (`api.…`, `repos/…`, URL templates). Numbers, `false` and `0` stay, and so does everything in JSON from any other source. The result is then compact JSON. Whenever fields were removed, a last line says how many and gives the full original as `artifact://slim-<call id>`, which `read` opens. Text that is not JSON, and JSON with nothing to drop, is left as it is.

**Digests** (`[tools] digest = "smol"`, off by default). A result of an MCP tool or `web_fetch` longer than `digest_min_chars` (after slimming), or a `read` of a local PDF of more than `digest_min_pages` pages, is replaced by notes written by the model of the `smol` [role](models-and-logins.md), who is given your last request. The notes cite the line ranges they come from, are cut at 1500 words, and start with a line that says they are notes. The full text, with one JSON field to a line, is saved as `artifact://digest-<call id>`; the model reads it, with `offset` and `limit` for the cited lines, when the notes are not enough. Notes are cached under `~/.azazel/cache/digests` by a hash of the text and the request, so the same result is not summarised twice.

Never digested: a `read` of anything but a PDF (code, `AGENTS.md`, skills, configuration), shell output (builds, tests, git), a `web_fetch` with an `offset`, results with images, results over 400000 characters, and any result for which the `smol` request fails, takes longer than 90 seconds or does not make it at least half as short. Those arrive as they are.

## Calls that are mended

A model sometimes calls a tool by the name another harness gives it, or sends its input in a slightly wrong shape. Before the permission check, such a call is mended against the tool's input schema:

| The model sends | The call runs with |
|---|---|
| `bash`, `str_replace`, `apply_edit`, `grep_search`, `list_dir`, `ls`, `todowrite` | `shell`, `edit`, `edit`, `grep`, `read`, `read`, `todo` |
| A tool's name in another case (`Read`) | That tool (`read`) |
| An object or array as a JSON string (`"edits": "[{...}]"`) where the schema wants one | The parsed object or array |
| A number or boolean as a string (`"limit": "20"`, `"replace_all": "true"`) where the schema wants one | The number or boolean |
| `null` for a field that may be left out | The field left out |
| A field the tool does not have, where its schema allows no others | The field left out |

A value that already fits its schema is never changed: `"20"` stays a string where a string is wanted. What was mended is added to the result as one line, so the model learns the real shape, and is kept with the call in the session file (the `repairs` of its `tool_started` entry):

```text
note: called as `bash`; the tool is `shell`; `timeout` was a number in a string, read as a number
```

## URL schemes `read` accepts

| Address | Returns |
|---|---|
| `artifact://<id>` | A spilled tool output |
| `local://` | The session's scratch directory, listed |
| `local://<name>` | A file in it; see below |
| `skill://<name>` | The body of a skill |
| `skill://<name>/<relative path>` | A file beside that skill's `SKILL.md` (at most 256 KB) |
| `agent://<id>` | The latest report of an agent |
| `agent://<id>/transcript` | What the agent did: one line per tool call and the full text of what it said |
| `agent://<id>/status` | What the agent is doing now, as `agent_status` gives it; `agent://all/status` for every agent |
| `mcp://<server>/<resource uri>` | A resource of an MCP server |
| `http://...`, `https://...` | The page as Markdown, through the `web_fetch` pipeline, [GitHub and YouTube addresses](web-and-browser.md#github-addresses) included; `offset` and `limit` of `read` page it by line. Permission rules for `web_fetch` apply |
| `session://` | The open session: id, file, name, when it was created, message counts, and up to 20 other sessions of the project |
| `session://<id>` | Another session of the project (a unique start of its id is enough), in the same condensed form as `agent://<id>/transcript` |
| `job://` | The background jobs, one per line |
| `job://<id>` | A background job's output (its last 2000 lines) and state |
| `memory://` | Every memory as `id [kind, scope] text #tags`. With memory on |
| `memory://<id>` | One memory with its kind, scope, date and tags |
| `memory://search/<query>` | The 20 best matches for a query |
| `pr://`, `issue://` | The open pull requests or issues of the repository (`gh pr list`, `gh issue list`, 30 at most) |
| `pr://<n>`, `issue://<n>` | One pull request or issue: title, state, author, labels, description and comments; for a pull request also its branch, a summary of its checks and the changed files. `#<n>` in a prompt attaches the same text, see [GitHub](github.md) |
| `pr://<n>/diff` | A pull request's diff |

`pr://` and `issue://` run the [`gh`](https://cli.github.com) program in the agent's working directory, with the account `gh` is signed in to, and exist only when `gh` is on `PATH` (`azazel doctor` says whether it is). They count as network access to `github.com`: in `manual` mode you are asked, and the rule `web_fetch(domain:github.com)` allows them.

`local://` is a directory of scratch files that belongs to the session, kept in the session's folder (`<sessions>/<id>/local/`) and deleted with it. The lead and its sub-agents share it. Unlike the other schemes it is a place to write: `read`, `write`, `edit`, `patch`, `glob` and `grep` take `local://<name>` where they take a path, and a name may have subdirectories (`local://runs/2/report.md`). A name with `..`, a leading `/` or a drive is refused. Writing there is not a change to the project: it is never asked about, in plan mode too. `write`, `edit` and `patch` take no other scheme (`write does not take agent:// addresses; only local:// and file paths`).

A scheme says what reading one of its addresses touches. Addresses that stay on this machine are plain reads. Extensions can add more schemes; see [extensions](extensions.md).

## `[tools]` configuration

```toml
[tools]
disabled = ["web_search", "screen"]   # tool names no agent is offered
max_result_bytes = 50000              # text kept from one tool result
shell_timeout_secs = 120              # default timeout of the shell tool
shell = ""                            # program name or path; empty means auto-resolve
format_on_write = true                # run the file's formatter after write, edit and patch
patch = "auto"                        # who gets patch: auto (GPT and Codex models, instead of edit), on or off
check_on_write = true                 # run the file's checker after write and edit
check_report = "delta"                # what the lead is told when it stops: delta, all or off
read_guard = "range"                  # what must have been read before an edit: range, file or off
redact_output = false                 # take secrets out of tool results before the model gets them
slim_results = true                   # drop noise fields from large JSON results of MCP tools and web fetches
digest = "off"                        # smol: notes from the smol model replace large MCP, web and PDF results
digest_min_chars = 16000              # MCP and web results longer than this are digested
digest_min_pages = 10                 # PDF reads of more pages than this are digested

[tools.format]                        # file extension to formatter command
rs = "rustfmt --edition 2024"

[tools.check]                         # file extension to checker command
py = "pylint --output-format=parseable"

[tools.scan]
secrets = true                        # scan the project with gitleaks once per session

[tools.eval]
python = true                         # eval may run Python
javascript = true                     # eval may run JavaScript
luau = true                           # eval may run Luau scripts that call other tools
background_max_secs = 7200            # longest a background Luau script may run (0 = no limit)
background_approval = "pause"        # a background script waits for your answer at a call that needs approval (or "deny")
background_pause_secs = 0             # longest a paused script waits for an answer (0 = until the session ends)
```

| Key | Default | Meaning |
|---|---|---|
| `disabled` | `[]` | Tools removed for every agent, by name (also MCP and extension tools) |
| `code_first` | `false` | Full schemas only for a core of tools; the rest are called from Luau `eval` scripts; see [code-first mode](#code-first-mode) |
| `code_first_listing` | `"names"` | How `eval`'s description lists the tools code-first mode hides: `names` (by name, MCP tools counted per server) or `signatures` (one-line signatures); see [code-first mode](#code-first-mode) |
| `defer` | MCP tools and seldom used built-in tools | Tools kept out of the prompt until the model searches for them; see [deferred tools](#deferred-tools) |
| `max_result_bytes` | `50000` | Size above which a result is cut and spilled to an artifact |
| `shell_timeout_secs` | `120` | Timeout of a `shell` call without `timeout_secs` |
| `cache_reads` | `true` | Answers a repeated `read`, `grep` or `glob` with a note; see [repeated reads, retries and time limits](#repeated-reads-retries-and-time-limits) |
| `retries` | `2` | Times a read-only web, LSP or MCP call is run again after a transient failure |
| `call_timeout_secs` | `120` | Time limit of a web, LSP or MCP call; `0` is no limit |
| `priority` | `"low"` | Operating-system priority of the processes started for agents: `low`, `idle` or `normal`; see [keeping the machine responsive](#keeping-the-machine-responsive) |
| `build_jobs` | `"half"` | Build jobs cargo, make and cmake may run at once for agents: `half` of the cores (at least 2), a number, or `all` to set nothing |
| `shell` | `""` | Shell to use instead of the resolved one. In a project config this is honoured only when the project is trusted |
| `format_on_write` | `true` | Runs the file's formatter after `write`, `edit` and `patch`: the configured one, else one the project uses; see [format on write](#format-on-write). Project config: trusted projects only |
| `format` | `{}` | Formatter command per file extension, in place of the built-in one; empty turns formatting off for the extension. Project config: trusted projects only |
| `patch` | `"auto"` | Which models are offered [`patch`](#patch): `auto` offers it in place of `edit` to OpenAI GPT and Codex models, `on` offers both to every model, `off` offers it to none |
| `check_on_write` | `true` | Runs the file's checker after `write` and `edit` when it is installed; see [checks on write](#checks-on-write) |
| `check` | `{}` | Checker command per file extension, replacing the built-in one; empty turns the check off. Project config: trusted projects only |
| `check_report` | `"delta"` | What the lead is told when it stops: findings on the lines the turn changed (`delta`), every finding (`all`), or nothing (`off`) |
| `read_guard` | `"range"` | What an agent must have read of a file before it changes it: the lines an edit changes (`range`), any part of the file (`file`), or nothing (`off`); see [the read guard](#the-read-guard) |
| `redact_output` | `false` | Replaces what looks like a secret in a tool result with `[redacted:<kind>]` before the model and the session file get it; see [permissions](permissions.md#secrets-in-tool-results) |
| `slim_results` | `true` | Drops `node_id`s, HTML copies and empty fields (and colours, avatars and API links of GitHub and Linear results) from large JSON results, and says so; see [slimmer and digested results](#slimmer-and-digested-results) |
| `digest` | `"off"` | `smol` replaces a large MCP, web or PDF result with notes from the `smol` model; `off` keeps results as they are |
| `digest_min_chars` | `16000` | Characters of an MCP or web result above which it is digested |
| `digest_min_pages` | `10` | Pages of a PDF read above which it is digested |
| `scan.secrets` | `true` | Scans the project with `gitleaks` once per session; see [secret scan](#secret-scan). Project config: trusted projects only |
| `eval.python` | `true` | Lets [`eval`](#eval) run Python. Project config: trusted projects only |
| `eval.javascript` | `true` | Lets [`eval`](#eval) run JavaScript. Project config: trusted projects only |
| `eval.background_max_secs` | `7200` | Longest a background Luau script (`eval` with `background: true`) may run, in seconds of wall-clock time; `0` is no limit. Project config: trusted projects only |
| `eval.background_approval` | `"pause"` | What a background Luau script does at a tool call that needs your approval: `pause` asks you and runs the script again once you answer; `deny` refuses the call. Project config: trusted projects only |
| `eval.background_pause_secs` | `0` | Longest a paused background script waits for your answer, in seconds; `0` waits until the session ends. Project config: trusted projects only |
| `eval.luau` | `true` | Lets [`eval`](#eval) run Luau scripts that call other tools (builds with the `lua` feature); with all three off, `eval` is not offered. Project config: trusted projects only |

To take a tool away from one kind of agent only, use `tools` and `disallowed-tools` in its definition; see [team](team.md).

## Shell resolution

With `shell` empty, the first of these that exists is used:

| System | Order |
|---|---|
| Linux, macOS | `bash`, then `sh` |
| Windows | `bash.exe` on `PATH` (the WSL launcher in `System32` is skipped), then the `bash.exe` shipped with Git for Windows, then `pwsh`, then `powershell` |

POSIX shells run the command with `-c`; PowerShell runs it with `-NoProfile -NonInteractive -Command` (without `-NonInteractive` for a [program in a terminal](#programs-in-a-terminal)). The `shell` tool's description tells the model which shell and operating system it is writing for. `azazel doctor` shows the resolved shell.
