# Memory and search index

Azazel keeps a small long-term memory per project so that facts, decisions and conventions learned in one session are available in the next, and an index of the project's files so agents can search by meaning instead of exact words. Both live in SQLite files under `~/.azazel`; nothing is written into the project. Memory is part of the default build (cargo feature `memory`) and is on unless `[memory] enabled = false`.

## What is remembered

A memory is one self-contained sentence with a kind, optional tags and a scope.

| Kind | Use for |
|---|---|
| `fact` | Something true about the project |
| `decision` | A choice that was made, and why |
| `preference` | How the user wants things done |
| `convention` | A rule of the codebase |
| `gotcha` | A trap that cost time |

| Scope | Stored in | Applies to |
|---|---|---|
| `project` (default) | `~/.azazel/projects/<slug>/memory.db` | This project |
| `global` | `~/.azazel/memory.db` | Every project of this user |

Memories never hold secrets by design: the tool description and the extraction prompt both tell the model to leave out keys, tokens and passwords. Review with `azazel memory list` if in doubt.

## Tools

| Tool | Input | Result |
|---|---|---|
| `memory_save` | `text`, `kind`, `tags?`, `scope?` | `saved as <id> (<scope>)` |
| `memory_search` | `query`, `limit?` (default 8, at most 50), `scope?` (omit for both) | One line per hit: `id [kind, scope] text #tags` |
| `memory_forget` | `id` | Deletes the memory from whichever store holds it |
| `semantic_search` | `query`, `collection?` (default `code`), `path_glob?`, `limit?` (default 10, at most 50) | `path:first-last` and the first 12 lines of each matching chunk |

The `read` tool also takes memory addresses: `memory://` lists every memory, `memory://<id>` shows one, and `memory://search/<query>` returns the 20 best matches.

`memory_search` and `semantic_search` are read-only. `memory_save` and `memory_forget` count as writes, so in `manual` and `accept-edits` mode they prompt unless you add the allow rules `memory_save` and `memory_forget` (see [permissions](permissions.md)).

## Recall

Memories reach the model in two ways, without the model having to ask:

- **Every request.** Every `preference` and `convention` that is in force, newest first, is listed under `# Memory` in the note that follows the conversation of every request, up to about 1200 tokens (4800 characters). It is not part of the system prompt, so saving or forgetting one leaves the [prompt cache](models-and-logins.md#prompt-cache) as it was.
- **Per prompt.** When you submit a prompt, the five best-matching memories from the project and global stores are appended to that message as a `<memory>` block. A memory is shown once per session, and ones already in that list are skipped.

Everything else is found on demand with `memory_search`.

## Automatic extraction

With `memory.auto_extract = true` (the default), one extra model call turns the conversation into memories:

- when the session ends, and
- after each full compaction of the lead's context.

The call uses the `@memory` role (which falls back to `@smol`, then `@default`; see [models and logins](models-and-logins.md)). It receives the most recent part of the conversation (about 30k tokens) and the 20 most related existing memories, and may return at most 8 items. An item is dropped when its text equals an existing memory (ignoring case and spacing) or, with embeddings configured, when its cosine similarity to an existing memory is 0.92 or higher. An item can replace an older memory; the old one is marked superseded and no longer appears in searches or recall. Extracted memories always go to the project store.

`/memory auto off` turns extraction off for the current session only.

## Search: lexical and hybrid

Without `[embeddings]`, search is full-text only (SQLite FTS5, BM25 ranking): the query is reduced to its alphanumeric terms, joined with OR. With `[embeddings]` configured, the top 50 full-text hits and the top 50 vector hits are fused with reciprocal rank fusion, so a memory or chunk that shares no word with the query can still rank first.

```toml
[embeddings]
provider = "ollama"                 # a built-in provider id or one you defined under [providers.<id>]
model = "<embedding-model-id>"
```

The provider must speak an OpenAI-compatible API (`POST <base_url>/embeddings`) or Google's (`batchEmbedContents`); Anthropic and Bedrock providers have no embeddings endpoint and are rejected at startup. Credentials are the provider's usual ones. Each stored vector remembers the model that produced it; after you change `model`, rows are re-embedded in the background at the next session start (memories) or index refresh (chunks). If an embeddings request fails, that search falls back to full-text.

## Code index and `semantic_search`

The index (`~/.azazel/projects/<slug>/index.db`) holds the project's text files cut into chunks of 60 lines that overlap by 10; a chunk boundary moves to a blank line when one is within 10 lines. The walk honours ignore files and skips hidden files, binary files, files over 1 MiB and lock files (`*.lock`, `package-lock.json`, `pnpm-lock.yaml`, `bun.lockb`).

The index is kept current automatically:

- at session start, new and changed files are re-read in the background (`memory.auto_index`, default `true`);
- two seconds after an agent's `write` or `edit`, that file is re-indexed.

`azazel index` rebuilds the `code` collection from scratch and prints `indexed <n> files in <m> chunks`.

`semantic_search` searches the `code` collection by default. `path_glob` restricts results, for example `src/**/*.rs`.

## Document collections

Other material (specs, API references, notes) can be indexed into named collections that `semantic_search` reaches with `collection: "<name>"`.

```sh
azazel rag add guide docs/ README.md     # index files or directories into docs:guide
azazel rag list                          # collections with file and chunk counts
azazel rag remove guide
```

Names may use letters, digits, `-`, `_` and `.`. Running `rag add` again on the same name refreshes paths already in it. Collections are stored per project, in the same `index.db`.

## Command line

```sh
azazel memory list                       # project memories, then global ones, each newest first
azazel memory search "indentation" --limit 5
azazel memory forget 0a1b2c3d4e5f        # the 12-character id shown by list and search
azazel memory export > memories.jsonl    # every memory, superseded ones included, as JSON lines
```

All of these work on the project found from the current directory (or `--cwd <dir>`), and exit with code 1 on failure.

## Quick add

In the interactive UI, a one-line input that starts with `# ` is remembered instead of sent:

```text
# always run clippy before committing
```

The rest of the line is saved as a `preference` of this project, and its id is shown (`remembered as 0a1b2c3d4e5f: ...`). The agent is not sent anything, but as a preference the note is in front of the agent with every request from then on. A line with more text below it is an ordinary prompt, so a pasted Markdown document is not mistaken for a note. Remove a note with `/memory forget <id>`.

## `/memory`

| Command | Does |
|---|---|
| `/memory` or `/memory list` | Lists project and global memories |
| `/memory forget <id>` | Deletes one |
| `/memory auto` | Shows whether automatic extraction is on for this session |
| `/memory auto on` / `/memory auto off` | Switches it for this session |

## Configuration

```toml
[memory]
enabled = true        # false: no memory tools, no recall, no index upkeep
auto_extract = true   # extract memories at session end and after compaction
auto_index = true     # refresh the code index at session start
```

| Key | Default | Meaning |
|---|---|---|
| `memory.enabled` | `true` | Loads memory for sessions. The `azazel memory`, `index` and `rag` commands work either way |
| `memory.auto_extract` | `true` | Automatic extraction |
| `memory.auto_index` | `true` | Background index refresh at session start |
| `embeddings.provider`, `embeddings.model` | unset | Enables hybrid search; unset means full-text only |

`azazel doctor` reports `memory: off`, `memory: on, word search only (no [embeddings] configured)` or `memory: on, embeddings from <provider>/<model>`. If the stores cannot be opened (for example a bad `[embeddings]` table), the session refuses to start and says to set `[memory] enabled = false` to run without memory.

See also: [configuration](configuration.md), [tools](tools.md), [extensions](extensions.md) for `azazel.memory.save` and `azazel.memory.search` in Luau.
