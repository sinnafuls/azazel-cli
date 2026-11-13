# Language servers

Azazel talks to language servers (LSP) so that agents get what a compiler knows: errors right after an edit, where a symbol is defined, who uses it, and safe renames. Servers are started on demand for the files agents touch and stopped when the session ends. This is part of the default build (cargo feature `lsp`).

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

A server that exits is started again on next use, up to three starts per session; after that calls report why it is not running. All servers are shut down when the session ends.

## Diagnostics after edits

After a successful `write` or `edit` of a file whose server is already in use in this session, azazel waits up to 1.5 seconds for the server's diagnostics for the new content and appends the errors to the tool result:

```
edited src/config.rs (1 replacement)
diagnostics:
src/config.rs:42:17 error mismatched types: expected `u32`, found `&str`
src/config.rs:57:9 error cannot find value `timeout` in this scope
```

- Only errors are appended (a diagnostic without a severity counts as an error); warnings and hints are left out. Use the `diagnostics` action to see everything.
- At most 10 errors are listed, followed by `… <n> more errors`.
- Positions are 1-based `line:column`, counted in characters.
- If the server had not been used yet, the write starts it but does not wait, so the first edit of a session may come back without diagnostics.
- If nothing arrives in time, the result is returned without a `diagnostics:` block.

This gives the agent immediate feedback without running a build. It does not replace the build and tests; see [goals](goals.md) for the checker. Linters run after a write in the same way and add a `checks:` block; see [checks on write](tools.md#checks-on-write).

## The `lsp` tool

| Input | Meaning |
|---|---|
| `action` | `diagnostics`, `definition`, `references`, `hover`, `symbols` or `rename` |
| `path` | The file to ask about (required for every action) |
| `line`, `column` | 1-based position of the symbol; `column` is 1 when left out |
| `query` | For `symbols`: search the whole project for this name |
| `new_name` | For `rename`: the new name |

| Action | Needs | Result |
|---|---|---|
| `diagnostics` | `path` | Every diagnostic of the file as `path:line:col <severity> <message>`, with severity `error`, `warning`, `info` or `hint`; or `no diagnostics` |
| `definition` | `path`, `line` | Where the symbol is defined: `path:line:col  <source line>` |
| `references` | `path`, `line` | A count, then one `path:line:col  <source line>` per use, the declaration included |
| `hover` | `path`, `line` | The server's hover text: type, signature, documentation |
| `symbols` | `path` | The file's outline, one `line: kind name` per symbol, nested symbols indented |
| `symbols` | `path`, `query` | Project-wide symbol search: `kind name  path:line:col` |
| `rename` | `path`, `line`, `new_name` | Renames the symbol in every file that uses it and reports the edits per file |

Lists are cut after 100 lines with a count of the rest. Paths inside the project are shown relative to its root.

```json
{"action": "definition", "path": "src/main.rs", "line": 12, "column": 20}
{"action": "symbols", "path": "src/main.rs", "query": "parse_config"}
{"action": "rename", "path": "src/config.rs", "line": 8, "column": 12, "new_name": "load_config"}
```

If no server serves the file's extension, the tool returns an error saying that no language server is available for that kind of file and that `/lsp` lists the servers.

### Rename

`rename` asks the server for a workspace edit and writes it to disk itself, file by file under the same per-file locks `write` and `edit` use. It changes nothing and reports an error when

- the edit would touch a file outside the project, or
- the edit would create, move or delete files.

### Permissions

`rename` is a write on `path`; every other action is a read. They follow the same mode rules as `edit` and `read`, and rules use the tool name with a path pattern, for example `lsp(src/**)`. See [permissions](permissions.md).

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
| `failed: <reason>` | Could not be started |

`azazel doctor` lists the servers whose commands were found (`lsp: rust, go`) or prints `lsp: no language server is installed`.

See also: [tools](tools.md) for `read`, `write` and `edit`.
