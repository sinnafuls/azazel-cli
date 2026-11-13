# Language servers

Azazel talks to language servers (LSP) so that agents get what a compiler knows: errors right after an edit, where a symbol is defined, who uses it, and safe renames. Servers are started on demand for the files agents touch, stopped when none of their files has been touched for a while, and stopped when the session ends. This is part of the default build (cargo feature `lsp`).

## Default servers

These are used without any configuration, each only when its command is found on `PATH`:

| Name | Command | File extensions |
|---|---|---|
| `rust` | `rust-analyzer` | `rs` |
| `typescript` | `typescript-language-server --stdio` | `ts`, `tsx`, `js`, `jsx` |
| `python` | `pyright-langserver --stdio` | `py` |
| `go` | `gopls` | `go` |
| `c` | `clangd` | `c`, `h`, `cc`, `cpp`, `hpp` |

Azazel does not install servers. A default whose command is missing is simply not used.

## Configuring servers

Add or replace servers under `[lsp.servers.<name>]`:

```toml
[lsp.servers.rust]
command = "rust-analyzer"
args = []
extensions = ["rs"]

[lsp.servers.deno]
command = "deno"
args = ["lsp"]
extensions = ["ts", "tsx"]
```

| Key | Required | Meaning |
|---|---|---|
| `command` | yes | A program name on `PATH`, or a path to the executable. On Windows the `PATHEXT` extensions are tried, so `server` finds `server.cmd` |
| `args` | no (default `[]`) | Arguments; the server must speak LSP over standard input and output |
| `extensions` | yes | File extensions it serves, with or without the leading dot, any case |
| `warm_files` | no (default `[]`) | Files opened in the server as soon as it starts, relative to the project root; see [when servers start](#when-servers-start) |

Two keys under `[lsp]` apply to every server:

```toml
[lsp]
idle_secs = 240            # stop a server none of whose files was touched for this long; 0 never stops one
write_severity = "error"   # what follows a write: "error", or "warning" for warnings too
```

How configured servers combine with the defaults:

- A configured server with the same name as a default replaces it.
- A configured server takes its extensions away from the defaults. In the example above, `deno` serves `ts` and `tsx`, and the default `typescript` server keeps only `js` and `jsx`.
- A configured server is always considered available. If its command cannot be found, using it fails with an error and `azazel doctor` warns `lsp server <name>: its command was not found`.

See [configuration](configuration.md) for where config files live and how they merge.

## When servers start

Nothing starts at launch. A server is started in the background the first time an agent

- reads a file it serves with `read`,
- writes or edits such a file with `write` or `edit`, or
- calls the `lsp` tool on such a file.

It is initialised with the project root as its workspace. A file is opened in the server the first time it is touched, and its full text is sent again (followed by a save notification) whenever its content on disk has changed. Files larger than 4 MB are not sent.

`warm_files` are opened as soon as the server has started, before the file that made it start. A language server works out a project from the files it is shown, so naming the entry points (`src/main.rs`, `src/index.ts`) gets that work done while the agent is still reading, and the first question about another file is answered sooner. A warm file that does not exist is passed over.

A server none of whose files has been touched (read, written, or asked about with `lsp`) for `lsp.idle_secs` seconds is shut down, and `/lsp` shows it as `stopped, idle`. It costs memory, not correctness: the next touch of one of its files starts it again, with its warm files, exactly as the first touch did. `idle_secs = 0` keeps servers running until the session ends.

A server that exits on its own is started again on next use, up to three starts; after that calls report why it is not running. Being stopped for idleness does not count towards the three. All servers are shut down when the session ends.

## Diagnostics after edits

After a successful `write` or `edit` of a file whose server is already in use in this session, azazel waits up to 1.5 seconds for the server's diagnostics for the new content and appends the errors to the tool result:

```
edited src/config.rs (1 replacement)
diagnostics:
src/config.rs:42:17 error mismatched types: expected `u32`, found `&str`
src/config.rs:57:9 error cannot find value `timeout` in this scope
```

- Only errors are appended (a diagnostic without a severity counts as an error); warnings and hints are left out. Use the `diagnostics` action to see everything.
- With `lsp.write_severity = "warning"` the warnings follow the errors, as `path:line:col warning <message>`; hints and information are still left out.
- At most 10 lines are listed, errors first, followed by `… <n> more errors` (`… <n> more errors and warnings` when warnings are on).
- Positions are 1-based `line:column`, counted in characters.
- If the server had not been used yet, or was stopped for idleness, the write starts it but does not wait, so the first edit of a session may come back without diagnostics.
- If nothing arrives in time, the result is returned without a `diagnostics:` block.

This gives the agent immediate feedback without running a build. It does not replace the build and tests; see [goals](goals.md) for the checker. Linters run after a write in the same way and add a `checks:` block; see [checks on write](tools.md#checks-on-write).

## The `lsp` tool

| Input | Meaning |
|---|---|
| `action` | `diagnostics`, `definition`, `references`, `hover`, `symbols`, `outline`, `read_symbol`, `rename` or `rename_file` |
| `path` | The file to ask about (required for every action) |
| `line`, `column` | 1-based position of the symbol; `column` is 1 when left out |
| `query` | For `symbols`: search the whole project for this name |
| `new_name` | For `rename`: the new name |
| `symbol` | For `read_symbol`: the symbol's name, or `Outer.name` |
| `occurrence` | For `read_symbol`: which of several symbols of that name, counted from 1 |
| `new_path` | For `rename_file`: where the file goes |

| Action | Needs | Result |
|---|---|---|
| `diagnostics` | `path` | Every diagnostic of the file as `path:line:col <severity> <message>`, with severity `error`, `warning`, `info` or `hint`; or `no diagnostics` |
| `definition` | `path`, `line` | Where the symbol is defined: `path:line:col  <source line>` |
| `references` | `path`, `line` | A count, then one `path:line:col  <source line>` per use, the declaration included |
| `hover` | `path`, `line` | The server's hover text: type, signature, documentation |
| `symbols` | `path` | The file's outline, one `line: kind name` per symbol, nested symbols indented |
| `symbols` | `path`, `query` | Project-wide symbol search: `kind name  path:line:col` |
| `outline` | `path` | Every symbol of the file with its kind, name, first and last line and the line that declares it, nested symbols indented; see [outline and symbol source](#outline-and-symbol-source) |
| `read_symbol` | `path`, `symbol` | The source of one symbol under a `<kind> <name>  <path>:<first>-<last>` line |
| `rename` | `path`, `line`, `new_name` | Renames the symbol in every file that uses it and reports the edits per file |
| `rename_file` | `path`, `new_path` | Moves the file and updates the files that refer to it; see [moving a file](#moving-a-file) |

Lists are cut after 100 lines with a count of the rest. Paths inside the project are shown relative to its root.

```json
{"action": "definition", "path": "src/main.rs", "line": 12, "column": 20}
{"action": "symbols", "path": "src/main.rs", "query": "parse_config"}
{"action": "rename", "path": "src/config.rs", "line": 8, "column": 12, "new_name": "load_config"}
{"action": "outline", "path": "src/config.rs"}
{"action": "read_symbol", "path": "src/config.rs", "symbol": "Config.load"}
{"action": "rename_file", "path": "src/config.rs", "new_path": "src/settings/mod.rs"}
```

If no server serves the file's extension, the tool returns an error saying that no language server is available for that kind of file and that `/lsp` lists the servers.

### Outline and symbol source

`outline` and `read_symbol` let an agent work on a large file without reading all of it. `outline` answers with one line per symbol, from the server's document symbols:

```
struct Config  12-31  pub struct Config {
  field timeout  14-14  pub timeout: Duration,
impl Config  33-120  impl Config {
  method load  34-71  pub fn load(path: &Path) -> Result<Self, ConfigError> {
  method merge  73-120  fn merge(&mut self, other: Config) {
function default_timeout  122-124  fn default_timeout() -> Duration {
```

Each line is `<kind> <name>  <first line>-<last line>  <the line its name is on>`, cut at 160 characters, and a symbol inside another is indented under it. The lines are numbered as `read` numbers them, so `read` with `offset` and `limit` fetches any of them.

`read_symbol` returns the source of the symbol named `symbol`, word for word, under a line that says what it is and where:

```
method load  src/config.rs:34-71
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        …
    }
```

- The lines it returns count as read for the [read guard](tools.md#the-read-guard): the agent can `edit` inside the symbol straight away, and nowhere else in the file.
- `symbol` is a name (`load`) or a name with what contains it (`Config.load`, `Config::load`). When several symbols have the name, the call fails and lists them with their lines; say which with a fuller name or with `occurrence` (1 for the first).
- At most 2000 lines (about 256 KB) come back; a longer symbol ends with `[lines 34-2033 of 34-2400; continue with read offset=2034]`, and only the lines shown count as read.
- No symbol of that name is an error that points to `outline`.

Both need a language server for the file, as every action does; there is no fallback that parses the file without one.

### Rename

`rename` asks the server for a workspace edit and writes it to disk itself, file by file under the same per-file locks `write` and `edit` use. It changes nothing and reports an error when

- the edit would touch a file outside the project, or
- the edit would create, move or delete files.

The agent has then seen the lines the rename changed, in every file, and of the rest of each file what it had seen before.

### Moving a file

`rename_file` moves `path` to `new_path` and keeps the project's references to it right, which a `mv` in the shell does not:

1. The server is asked what must change for the move (`workspace/willRenameFiles`): typically the imports and module declarations that name the file.
2. Those edits are written, under the same rules as a `rename`.
3. The file is moved; missing directories are created. What agents had read of it, they have read of it at its new place.
4. The server is told the move is done (`workspace/didRenameFiles`), the old document is closed in it and the new one opened.

The result is `moved src/config.rs to src/settings/mod.rs; 3 edits in 2 files` and one line per edited file. A server that does not announce these two file operations is not asked or told, and the file is simply moved. The move is refused when `new_path` exists or is outside the project. Only moves made with this action are announced: a file moved with the shell is not.

### Permissions

`rename` is a write on `path` and `rename_file` a write on `path` and `new_path`; every other action is a read. They follow the same mode rules as `edit` and `read`, and rules use the tool name with a path pattern, for example `lsp(src/**)`. See [permissions](permissions.md).

## `/lsp`

Shows one line per server: name, state, command line and extensions.

```
rust  running, 3 files open  `rust-analyzer`  .rs
typescript  not installed  `typescript-language-server --stdio`  .ts .tsx .js .jsx
go  not started  `gopls`  .go
```

| State | Meaning |
|---|---|
| `not started` | Installed, no agent has touched a matching file yet |
| `not installed` | The command was not found |
| `starting` | Being started or busy syncing a file |
| `running, <n> files open` | In use |
| `exited` | The process ended; it is restarted on next use |
| `stopped, idle` | Shut down after `lsp.idle_secs` without a touched file; it is started on next use |
| `failed: <reason>` | Could not be started |

`azazel doctor` lists the servers whose commands were found (`lsp: rust, go`) or prints `lsp: no language server is installed`.

See also: [tools](tools.md) for `read`, `write` and `edit`.
