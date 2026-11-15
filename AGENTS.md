# Working on azazel

For people and coding agents who change this repository. User documentation is under [docs/](docs/README.md).

## What this is

Azazel is a coding-agent harness for the terminal, written in Rust: one binary, `azazel`, runs a lead agent
that can hand work to sub-agents, with an interactive UI, a print mode (`-p`), a JSON-lines protocol
(`azazel rpc`), the Agent Client Protocol (`azazel acp`) and background sessions. It is a Cargo workspace:
Rust 1.99, edition 2024, tokio. It builds and is tested on Windows, Linux and macOS.

## Layout

| Path | Holds |
|---|---|
| `crates/azazel-ai` | Messages and streams, the `Provider` trait and the wire APIs, the model catalogue, credentials and sign-in, usage windows, the retry and fallback `Router`, and the scripted provider for tests (`testing.rs`) |
| `crates/azazel-agent` | The agent loop, the `Tool` and `Gate` traits, events, compaction |
| `crates/azazel-core` | Paths, configuration (`config.rs`), trust, permissions, sessions, checkpoints, hooks, rules, skills, custom commands, agent definitions, packages, and the `Extension` registrar and event bus (`ext.rs`) |
| `crates/azazel-tools` | The built-in tools: files, search, shell, jobs, terminals, web, LSP, checks on write |
| `crates/azazel-team` | Sub-agents, worktrees, goals and the checker, the coordinator, the advisor, typed results |
| `crates/azazel-tui` | The renderer, components, the editor, key parsing, themes and symbol sets; it knows nothing of agents |
| `crates/azazel-cli` | The binary: the command line (`cli.rs`), building the application (`app.rs`), the terminal UI (`src/tui/`), print mode, RPC, ACP, background sessions, settings, logins |
| `crates/azazel-mcp`, `azazel-memory`, `azazel-browser`, `azazel-media`, `azazel-lua` | Optional features, each an `Extension`: MCP, memory and the code index, the browser and the screen, media generation, Luau scripts and `run_code` |
| `crates/azazel-protocol`, `azazel-client` | The RPC protocol's types, and a client for it |
| `xtask` | Maintenance tasks (`cargo xtask <task>`); `xtask/docs-check` is the docs check |
| `tests/fixtures` | Stand-in programs the tests run (`fake-lsp`, `mcp-echo`, `fake-check`, `fake-sg`, `fake-gh`, `fake-ssh`, …), scripts for the scripted provider (`scripts/*.json`) and sample projects |
| `skills/` | The skills built into the binary |
| `docs/`, `README.md`, `CHANGELOG.md` | User documentation and the release notes |
| `ref/` | Other harnesses, when present: read them for behaviour, never copy from them and never change them |

## Build and test

While working, run the tests of the crates a change touches, not the whole suite:

```sh
cargo test -p azazel-core               # one crate: its unit tests and its integration tests
cargo test -p azazel-core --lib config  # its unit tests whose names contain `config`
cargo test -p azazel-cli --test it rpc  # its integration tests in tests/it/rpc.rs
```

Before a change is handed over, run these from the repository root; they must pass:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p <each crate the change touches>
cargo build --no-default-features -p azazel-cli
cargo xtask docs-check
```

The whole suite (`cargo test --workspace --no-fail-fast`) runs in CI (`.github/workflows/ci.yml`) on
every push and pull request, and before a release.

- `cargo build --no-default-features -p azazel-cli` builds without any optional feature; code behind a
  feature (`mcp`, `lua`, `memory`, `browser`, `media`, `voice`, `detach`, …) must not be needed without it.
- Each crate has one integration-test program, `tests/it/main.rs`, with a module per area
  (`tests/it/rpc.rs`) and the helpers they share in `tests/it/common.rs`. Add a test to the module of
  its area, or a unit test beside the code; do not add files directly under `tests/`, as each would be
  another program linked against the whole crate.
- One test: `cargo test -p azazel-cli --test it print_mode::`, or `cargo test -p azazel-core config_keys`.
- A test that fails only when the whole suite runs: run it alone as well, and say so with both results.

Other tasks:

| Command | Does |
|---|---|
| `cargo xtask docs-check` | Compares the docs with the code; see [Docs](#docs) |
| `cargo xtask bench-startup` | Builds the release binary and fails when it is larger than 56 MB, or `azazel --version` takes more than 30 ms, or a scripted `azazel -p hi` more than 150 ms. Prints the size and what it gained since the last run |
| `cargo xtask rpc-schema` | Writes `docs/rpc.schema.json` from `azazel-protocol`; run it after changing the protocol |
| `cargo xtask catalog` | Regenerates from models.dev (needs the network) the model catalogue `crates/azazel-ai/catalog/models.json`, the generated provider rows `crates/azazel-ai/src/providers/generated.rs` and their table in `docs/models-and-logins.md` |
| `cargo xtask dist [--target <triple>]` | Builds the release archive and its checksum into `dist/`; the release workflow runs it |
| `cargo xtask prune [--dry-run]` | Deletes the build output of earlier builds from `target/`: for every crate it keeps the version built last and any built in the last hour |

### Keeping `target/` small

Cargo keeps every version of a crate it ever built, so `target/` grows by gigabytes a day while you change
code, features and flags (each test file is its own binary with its own debug info). After a session of
building and testing, and before you hand a change over, run `cargo xtask prune`. It only deletes what a
newer build replaced, so the next build stays incremental. A worktree made for one change has its own
`target/`: remove the worktree (`git worktree remove --force <dir>`) once the change is merged, and delete
the directory by hand if a running program kept a file open. `cargo clean` frees everything, at the cost
of a full build. Agent worktrees share the main checkout's `target/` by default (`[team] worktree_target`), so
`cargo xtask prune` and `cargo clean` there act on the directory they all use; with `own`, each has its own.

## Trying a change by hand

Never test against your own `~/.azazel`. Give the run a scratch home and a scratch project, both under
`tmp/` (which git ignores), and delete them afterwards. The scratch project needs a `.azazel/` directory
of its own: without one its project root is this repository, and the UI asks whether to trust it.

```sh
cargo build -p azazel-cli --features test-support
mkdir -p tmp/live/project/.azazel
AZAZEL_HOME=tmp/live/home AZAZEL_MOCK_SCRIPT=tests/fixtures/scripts/write_read.json \
  target/debug/azazel.exe -p --mode auto --cwd tmp/live/project "write the file"
```

That prints `Wrote and verified hello.txt.` and leaves `tmp/live/project/hello.txt`. The binary is
`target/debug/azazel` on Linux and macOS.

- **The scripted provider.** A build with `--features test-support` has a provider `mock`. With
  `AZAZEL_MOCK_SCRIPT=<file>` the lead's model is `mock/lead` and each request is answered by the next step
  of the script: `{ "lead": [ { "reply": { "text": "…", "tool_calls": [ { "id": "c1", "name": "read",
  "input": { "path": "a.txt" } } ] } } ] }`. Every queue of the script is a model: `"scout": [ … ]` is
  `mock/scout`, which a role or an agent uses once it is pointed there
  (`--set 'roles.smol=["mock/scout"]'`). A queue that runs out fails the request with `mock script
  exhausted`. `AZAZEL_MOCK_REQUESTS=<file>` records every request, and `AZAZEL_MOCK_TOOLS=1` adds the
  tool `test_wait`. The formats are in
  `crates/azazel-ai/src/testing.rs`; examples are in `tests/fixtures/scripts/`.
- **Settings for one run.** `--set table.key=value` (`--set tui.popups=false`,
  `--set 'roles.judge=["mock/judge"]'`), or `AZAZEL_<TABLE>__<KEY>`. The tests of scripted runs pass
  `--no-memory` and `--set session.auto_name=false`, so that extracting memories and naming the session
  ask the script for nothing; a test that moves in the session tree also passes
  `--set session.branch_summary=false`, or the summary of the branch it left takes a scripted reply.
- **No real models and no real logins.** Do not copy `~/.azazel/auth.json` anywhere: a sign-in that is
  refreshed from a copy stops working in the original. A scripted run needs none of your provider keys;
  unset them (`ANTHROPIC_API_KEY`, …) so that no run can reach a real provider by mistake.
- **The interactive UI** needs a terminal. To drive it from a script, run the binary in a pseudo-terminal
  and read its output through a terminal emulator: Python's `pywinpty` (Windows) or `pty`, with `pyte`.
  - Read the terminal in a background thread; a read blocks.
  - Send keys as the bytes a terminal sends: `\r` for Enter, `\x1b` for Esc. Type the text, wait until it
    shows in the input, then send Enter as a write of its own.
  - Typing a slash command opens the completion menu, where Enter completes the highlighted row when it
    differs from what was typed (`/mode` becomes `/model`). End the command with a space (`/mode `, then
    Enter): that closes the menu and Enter runs it.
  - Popups are drawn on the alternate screen, which `pyte` does not have: split the output at
    `ESC [ ? 1049 h` and `ESC [ ? 1049 l` and feed what is between them to a second screen.
  - Wait for the text you expect to appear instead of sleeping. The first screen is ready when the status
    line shows the model (`mock/lead`).

## Tests

- Test behaviour a user or a caller can see. No test that a message has a certain wording for its own
  sake, that a function was called, or that a struct holds what was put into it.
- Deterministic. Never order events with a fixed sleep: wait for the condition (a file, a line of output,
  a frame). Time comes from a clock the test controls where the code allows it.
- Nothing outside the test's own temporary directory, no network, no real credentials, no secrets in
  fixtures. Stand-in programs live in `tests/fixtures/` and are found through `PATH` by the test.
- Compare canonical paths: a temporary directory on macOS is behind a symlink (`/var` is `/private/var`).
  `azazel_core::paths::canonical` gives the form the program uses, and never a `\\?\` path on Windows.
- Windows paths can be long and contain spaces; build them with `Path::join`, never by joining strings
  with `/`, and do not assume a path fits in 260 characters (git is run with `core.longpaths`).
- A test must pass alone and in the full suite, in any order, beside other tests: no shared files, no
  fixed ports (bind port 0), no global state without a lock.
- Integration tests of the binary are in `crates/azazel-cli/tests/`: each makes a sandbox (a scratch
  `AZAZEL_HOME` and project), runs `azazel` with a script for the scripted provider and reads what it
  printed or wrote. Terminal rendering is tested against a `vt100` screen in `azazel-tui`.
- `unwrap` is denied outside tests (`clippy::unwrap_used`); `unsafe` is forbidden.

## Conventions

- **Configuration.** A table is a `config_table!` struct in `crates/azazel-core/src/config.rs` with its
  defaults, a field of `Config`, and a name in `TABLES`. Unknown keys are load errors. A key that names a
  program to run, or widens what agents may do, goes into `TRUST_GATED`: a project's config may set it only
  once the project is trusted. A table whose entries the user names (`[providers.<id>]`) also goes into
  `named_entries` in `config_keys.rs`.
- **Features are extensions.** A feature registers its tools, slash commands, event handlers, system-prompt
  parts, URL schemes and services through the `Registrar` it is handed in `Extension::register`; `App::build`
  (`crates/azazel-cli/src/app.rs`) loads the built-in ones in order. Shared state is a service: put it in
  with `r.service(Arc<T>)` and read it through `ctx.services.get::<T>()` in a tool or handler. Do not add
  globals, and do not reach from one feature into another's internals.
- **The UI.** Popups and pickers are `Overlay`s that return an `Action`; colours come from `Theme` roles and
  symbols from `theme.glyph(..)`, never from literals, so every theme and the `ascii` symbol set work.
- **One way to do a thing.** Before adding a helper, look for the existing one (`FileTracker`, `Shell`,
  `Paths`, `frontmatter`, `text::redact`). Replace a convention everywhere or not at all.
- **Text for users** (messages, docs, help) is plain English: say what happens and what to do. Comments say
  why, not what.
- **Dependencies.** A new crate needs a reason that outweighs its size; check with
  `cargo xtask bench-startup`. Optional subsystems are Cargo features of `azazel-cli`.
- **HTTPS.** The binary has one TLS implementation, rustls on `ring`, and `reqwest` comes without a
  provider of its own: a client built before the provider is installed panics. Build every client with
  `azazel_ai::tls::client()` or `azazel_ai::tls::client_builder()`; clippy turns `reqwest::Client::new`
  and `reqwest::Client::builder` away. Do not enable a crate's default TLS feature without checking
  `cargo tree -i aws-lc-rs` stays empty.
- **Release profile.** Everything is built for size (`opt-level = "s"`) except the packages listed under
  `[profile.release.package]` in `Cargo.toml`, which keep `opt-level = 3` because time is spent there. A
  package joins the list with a measurement of what it gains and what it costs in
  `cargo xtask bench-startup`. `LIBSQLITE3_FLAGS` in `.cargo/config.toml` leaves unused parts out of the
  bundled SQLite; FTS5 must stay.

## Docs

The docs must say exactly what the code does. `cargo xtask docs-check` holds them to it and fails with one
line per difference, naming the page and the name:

| What the code has | Where the docs must list it |
|---|---|
| Every configuration table and key, with its default | The settings tables of `docs/configuration.md`; a page with a ``## `[table]` configuration`` section lists that table's keys too |
| Trust-gated settings | The `Gated settings:` list in `docs/configuration.md` |
| Hook events and the fields of a hook | The tables of `docs/hooks.md`, the `Hook events:` list in `docs/configuration.md`, and the built-in skill `skills/azazel-extensions/SKILL.md` |
| Keymap actions, with the keys bound to each by default | The key table of `docs/tui.md` |
| Flags of `azazel` | The flags table of `docs/configuration.md` |
| Subcommands and their actions | The subcommands table of `docs/configuration.md` |
| Flags of subcommands | A line of a page under `docs/` that names the command and the flag (`azazel sessions prune [--older-than 30d]`) |
| RPC commands, frames and UI request kinds | The tables and frame sections of `docs/rpc.md`; `docs/rpc.schema.json` is generated |
| The Luau API (`crates/azazel-lua/src/azazel.d.luau`) | The `Function` and event tables of `docs/extensions.md` and of the built-in skill |
| Slash commands | The tables under "Slash commands" in `docs/tui.md` |
| Tools | The `Tool` tables of `docs/tools.md` |

It also checks, the other way round, that every name those tables list exists; that every `azazel …`
command line shown in a page uses subcommands and flags that exist; that every page under `docs/` is
linked from `docs/README.md`; that every relative link leads to a file and a heading that exist; that
and that `CHANGELOG.md` has a section for the version in `Cargo.toml`. It needs no network. The check asks the program for its registries
(`azazel_cli::inventory`, `azazel_core::config_keys`), so a name that is registered is seen without
further work; a tool that is registered only under a condition is listed in
`inventory::conditional_tools`.

## Adding a feature

1. The code, behind a Cargo feature when it is an optional subsystem.
2. Tests of what it does (see [Tests](#tests)).
3. The page under `docs/` that owns the behaviour, in the table or section docs-check reads.
4. A line under `## [Unreleased]` in `CHANGELOG.md` (`### Added`, `### Changed` or `### Fixed`), written
   for a user: what they can do now, and with which command, key or setting.
5. The gates of [Build and test](#build-and-test), `cargo xtask docs-check` among them.
6. When the change adds a dependency or much code: `cargo xtask bench-startup`, and the size it reports
   in the description of the change.

## Releasing

1. Every gate passes on `main`, and `cargo xtask bench-startup` is within its limits.
2. In `CHANGELOG.md`, rename `## [Unreleased]` to `## [x.y.z] - <date>` and put a new, empty
   `## [Unreleased]` above it.
3. Set `version` under `[workspace.package]` in `Cargo.toml` to `x.y.z` and build once, so that
   `Cargo.lock` follows.
4. Commit, tag `vx.y.z` and push the tag. `.github/workflows/release.yml` checks that the tag matches the
   version, takes the release notes from the version's section of `CHANGELOG.md`, builds every platform
   with `cargo xtask dist` and publishes the archives with their checksums. The installers and
   `azazel update` download the latest release by those names.
