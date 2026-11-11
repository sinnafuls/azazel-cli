# Hooks

A hook is something you configure to run at a fixed point of a session: before a tool call, when you submit a prompt, when the agent is about to stop. A hook can block the event, rewrite a tool's input, settle a permission request or add context for the model. Hooks are shell commands by default; they can also be HTTP calls, a one-off model check or a verifier agent. Use them for policy and automation that must not depend on the model remembering an instruction.

For logic that needs state or new tools, write an [extension](extensions.md) instead.

## Configuration

Hooks live in `config.toml` (see [configuration](configuration.md)) as arrays of tables, one array per event:

```toml
[[hooks.pre_tool_use]]
matcher = "shell"                    # regex on the tool name
if = "shell(git commit:*)"           # optional permission-rule prefilter
command = "\"$AZAZEL_PROJECT_DIR/scripts/check-commit.sh\""
timeout = 30

[[hooks.user_prompt_submit]]
command = "date +'Today is %A, %F.'"
```

Hook arrays concatenate across config layers: hooks from `~/.azazel/config.toml` run first, then the project's, then `config.local.toml`. An unknown event name or an unknown field is a config error.

| Field | Default | Meaning |
|---|---|---|
| `matcher` | every tool | Regular expression that must match the whole tool name: `shell`, `write\|edit`, `mcp__github__.*`. Empty or `*` matches every tool. Tool events only |
| `if` | none | A permission rule (`tool` or `tool(pattern)`) the call must match: the simple commands of a `shell` call, the `path` of `read`/`write`/`edit`, `domain:<host>` of `web_fetch`. Tool events only |
| `type` | `command` | `command`, `http`, `prompt` or `agent` |
| `command` | | Shell command (type `command`) |
| `url`, `headers` | | Endpoint and request headers (type `http`) |
| `prompt` | | Instruction (types `prompt` and `agent`) |
| `model` | `@smol` | Model or role for type `prompt` |
| `timeout` | `60` | Seconds. A command's process tree is killed at the timeout |
| `async` | `false` | `true`: start the hook and do not wait; its result is ignored |

### Hook types

| Type | What runs | Answer |
|---|---|---|
| `command` | The command, in the resolved shell (see [tools](tools.md)), in the agent's working directory, with the event JSON on stdin | Exit code and stdout, below |
| `http` | `POST` of the event JSON to `url` with `headers` | A 2xx reply whose body is empty or a JSON object with the same fields a command prints |
| `prompt` | One model call with your `prompt` and the event JSON | `{"ok": true}` or `{"ok": false, "reason": "..."}` |
| `agent` | A `verifier` agent (see [team](team.md)) given your `prompt` and the event JSON; it can read files and run commands | The same `{"ok": ...}` object at the end of its report |

## Events

`hook_event_name` is the name the hook receives on stdin.

| Config event | `hook_event_name` | Extra stdin fields | When it blocks |
|---|---|---|---|
| `pre_tool_use` | `PreToolUse` | `tool_name`, `tool_input`, `tool_use_id` | The call is denied and the reason goes to the model. May also rewrite the input or decide about the approval prompt |
| `post_tool_use` | `PostToolUse` | the same plus `tool_response` | The reason is appended to the tool result |
| `post_tool_use_failure` | `PostToolUseFailure` | the same plus `tool_response` | Cannot block |
| `user_prompt_submit` | `UserPromptSubmit` | `prompt` | The prompt is rejected and the reason shown to you |
| `session_start` | `SessionStart` | `source`: `startup`, `resume`, `clear` or `compact` | Cannot block |
| `session_end` | `SessionEnd` | none | Cannot block |
| `stop` | `Stop` | `stop_hook_active` | The lead keeps going, with the reason as a user message |
| `subagent_start` | `SubagentStart` | `agent_type` (the definition name) | The spawn is refused |
| `subagent_stop` | `SubagentStop` | `stop_hook_active`, `agent_transcript_path` | The sub-agent keeps going |
| `pre_compact` | `PreCompact` | `trigger`: `manual` or `auto` | A manual compaction is cancelled; an automatic one cannot be blocked |
| `post_compact` | `PostCompact` | none | Cannot block |
| `permission_request` | `PermissionRequest` | `tool_name`, `tool_input` | The call is denied. May also allow it without asking you |
| `notification` | `Notification` | `message` | Cannot block. Fires in the terminal UI when something wants your attention |

Every hook also receives `session_id`, `transcript_path`, `cwd`, `agent_id` (`lead` or a sub-agent id) and `permission_mode`. `tool_response` is `{ "content": "<text>", "is_error": bool, "details"?: ... }`.

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "0195...",
  "transcript_path": "/home/me/.azazel/projects/app-1a2b3c4d/sessions/....jsonl",
  "cwd": "/home/me/app",
  "agent_id": "lead",
  "permission_mode": "ask",
  "tool_name": "shell",
  "tool_input": { "command": "git commit -m wip" },
  "tool_use_id": "call_1"
}
```

Where a hook runs in the life of a tool call: deny rules, the git push guard and protected paths are checked first and cannot be overridden; then `pre_tool_use` hooks; then the permission mode and rules; then, only if you would be asked, `permission_request` hooks. See [permissions](permissions.md).

## What a command hook returns

| Exit | Stdout | Effect |
|---|---|---|
| 0 | empty | The event proceeds |
| 0 | plain text | The event proceeds. On `user_prompt_submit` and `session_start` the text is added to the prompt as context |
| 0 | JSON (starts with `{`) | The fields below are honoured |
| 2 | ignored | Block, with stderr as the reason |
| other | ignored | Not blocking. You get a warning with stderr |

A hook that times out, prints invalid JSON or (for `http`) gets a non-2xx reply also only warns. Up to 1 MB of each output stream is kept.

JSON fields:

| Field | Effect |
|---|---|
| `"decision": "block"`, `"reason"` | Block the event |
| `"decision": "approve"` | Same as `permissionDecision: "allow"` |
| `"continue": false`, `"stopReason"` | Block the event, with `stopReason` (or `reason`) as the reason |
| `hookSpecificOutput.permissionDecision` | `deny`: block (reason from `permissionDecisionReason`). `allow`: run the call without asking you. `ask`: do not waive the approval prompt; the permission mode and rules decide as usual. `allow` and `ask` have an effect on `pre_tool_use` and `permission_request` only |
| `hookSpecificOutput.updatedInput` | `pre_tool_use` only: an object that replaces the tool's whole input |
| `hookSpecificOutput.additionalContext` | Text added to the prompt (`user_prompt_submit`, `session_start`) or to the tool result (`post_tool_use`, `post_tool_use_failure`) |

When one answer says several things the strictest wins: `continue: false`, then `deny`, then `block`, then `ask`, then `allow`. An `allow` never overrides a deny rule, the push guard or a protected path, and a rewritten input is checked against them again.

Hooks of one event run in order. The first one that blocks ends the chain; input rewrites accumulate, each hook seeing what the previous ones left.

## Examples

Block a tool call with exit code 2:

```bash
#!/usr/bin/env bash
# scripts/check-commit.sh: refuse commits while the tests fail
if ! cargo test --quiet >/dev/null 2>&1; then
  echo "the tests fail; fix them before committing" >&2
  exit 2
fi
```

Rewrite a tool's input (needs `jq`):

```toml
[[hooks.pre_tool_use]]
if = "shell(cargo build:*)"
command = "jq '{hookSpecificOutput: {updatedInput: (.tool_input + {command: (.tool_input.command + \" --locked\")})}}'"
```

Post every file change to a local service without waiting, ask a model about risky commands, and have a verifier check the work before the lead stops:

```toml
[[hooks.post_tool_use]]
matcher = "write|edit"
type = "http"
url = "http://localhost:8787/azazel"
async = true

[[hooks.pre_tool_use]]
matcher = "shell"
type = "prompt"
prompt = "Refuse commands that delete files outside the project directory."

[[hooks.stop]]
type = "agent"
prompt = "Run the project's tests. The event may proceed only if they pass."
timeout = 600
```

A `stop` hook that blocks makes the agent take another turn and then fires again, this time with `stop_hook_active: true`. Let the agent stop at some point, or it will loop.

## Environment

Command hooks inherit your environment plus:

| Variable | Value |
|---|---|
| `AZAZEL_PROJECT_DIR` | The project root |
| `AZAZEL_SESSION_ID` | The session id |
| `AZAZEL_HOOK_EVENT` | The config event name, for example `pre_tool_use` |
| `AZAZEL_HOOKS_DISABLED` | `1` |
| `CLAUDE_PROJECT_DIR` | The project root, when `[compat] claude` is true |

Like every process started for an agent, a hook cannot push to a git remote.

`AZAZEL_HOOKS_DISABLED` is the recursion guard: when it is set in the environment Azazel starts with, no hooks are loaded at all. Because hooks run with it set, an `azazel -p ...` started from a hook runs without hooks. You can set it yourself to start one session without hooks:

```bash
AZAZEL_HOOKS_DISABLED=1 azazel
```

An `agent` hook does not fire for events caused by the verifier it started.

## Hooks carried by skills

A skill can bring hooks in its `SKILL.md` frontmatter. They become active the first time the skill is used in a session and stay active until the session ends. See [skills and commands](skills-and-commands.md).

```yaml
hooks:
  Stop:
    - hooks:
        - type: command
          command: "$AZAZEL_SKILL_DIR/check.sh"
          timeout: 30
  PreToolUse:
    - matcher: Bash
      hooks:
        - type: command
          command: "$AZAZEL_SKILL_DIR/guard.sh"
```

Differences from config hooks:

- Events are named as in `hook_event_name` (`PreToolUse`, `Stop`, ...). Only `command` hooks are accepted.
- `matcher` and the `tool_name` on stdin use these names for built-in tools: `Bash` (`shell`), `Read`, `Write`, `Edit`, `Glob`, `Grep`, `WebFetch` (`web_fetch`), `WebSearch` (`web_search`), `Task` (`task`). Other tools keep their own names.
- The environment adds `AZAZEL_SKILL_DIR` and `CLAUDE_SKILL_DIR` (the directory holding `SKILL.md`) and, for skills of a package, `CLAUDE_PLUGIN_ROOT`.
- They run after the config hooks of the same event.

## Trust

Hooks run commands on your machine, so where they come from matters:

| Source | Runs |
|---|---|
| `~/.azazel/config.toml` | Always |
| `<project>/.azazel/config.toml`, `config.local.toml` | Only when the project is trusted. Otherwise the `hooks` table is skipped, and the start-up notice and `azazel doctor` say so |
| Skills in the project (`.azazel/skills`, `.claude/skills`, `.agents/skills`) | Only when the project is trusted |
| Skills of a [package](packages.md) | Only when you enabled the package's code at install |

Headless runs (`-p`, `goal`, `rpc`) use the recorded trust decision; pass `--trust` to trust the project for one run.
