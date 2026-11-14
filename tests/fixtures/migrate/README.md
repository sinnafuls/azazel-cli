# Migration fixtures

Small, made-up homes of the harnesses `azazel migrate` reads, one per source: `<source>/home` is
a user's home directory, `<source>/project` a project where the source keeps files of its own.
Nothing here was copied from a real installation: names, paths, keys (`test-key-...`) and
conversations are invented. The SQLite stores of omp (`agent.db`, the mnemopi banks) are
created by the tests that need them.

The shapes follow each harness's own description of its files:

| Source | Shapes taken from |
|---|---|
| pi | `docs/session-format.md`, `docs/message-types.md`, `docs/settings.md`, `docs/models-configuration.md`, `docs/prompt-templates.md`, `docs/skills.md`, `docs/mcp.md` of the pi coding agent (github.com/badlogic/pi-mono, `packages/coding-agent`) |
| omp | `docs/session.md`, `docs/settings.md`, `docs/models.md`, `docs/mcp-config.md`, `docs/slash-command-internals.md`, `docs/task-agent-discovery.md`, `docs/rulebook-matching-pipeline.md`, `docs/blob-artifact-architecture.md` and `src/session/session-entries.ts` of oh-my-pi (github.com/can1357/oh-my-pi) |
| jcode | The serde types of `jcode-session-types`, `jcode-message-types`, `jcode-config-types` and `jcode-memory-types`, and `docs/SYSTEM_PROMPT_CONFIG.md` (github.com/1jehuang/jcode) |
| Claude Code | The settings, memory, sub-agent, slash-command and MCP pages of code.claude.com/docs, and the transcript lines Claude Code writes under `~/.claude/projects` |
| Codex CLI | `docs/config.md` and the `RolloutLine`, `SessionMeta`, `ResponseItem` and `EventMsg` types of `codex-rs/protocol` (github.com/openai/codex), and the custom-prompts and config-reference pages of its documentation |
