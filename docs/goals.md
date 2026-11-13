# Goals and the checker

A goal is an objective the lead agent keeps working on until an independent check agrees it is met. The agent cannot simply declare success: when it claims the goal is done, the **checker** runs the project's build and tests and then has a separate verifier agent inspect the result. A failed check sends the agent back to work. Goals stop on their own when they are verified, when the agent says it is blocked, or when a budget runs out.

## Starting a goal

In the TUI:

```text
/goal make `cargo test` pass without changing the public API
```

| Command | Effect |
|---|---|
| `/goal <objective>` | Starts the goal and sets the lead to work on it. Refused while another goal is open. |
| `/goal` or `/goal status` | Objective, status, attempt number, minutes elapsed, and why it is paused or failed |
| `/goal pause` | Stops pursuing the goal until resumed |
| `/goal resume` | Takes a paused goal up again |
| `/goal drop` | Abandons the goal |

From the command line, without the interactive UI:

```sh
azazel goal "make the test suite pass" --mode auto --max-iterations 5 --max-cost 2.50 --max-minutes 30
```

| Flag | Meaning | Default |
|---|---|---|
| `--max-iterations <N>` | Rejected attempts to finish after which the goal fails | `goal.max_iterations` (8) |
| `--max-cost <USD>` | Pause when the whole team has spent this much | no limit |
| `--max-minutes <N>` | Pause after this long | no limit |

The global flags `--mode`, `--model`, `--thinking`, `--output text|json`, `--cwd` and `--trust` apply too. Nobody can answer approval prompts in this mode, so pick a [permission mode](permissions.md#headless-behaviour) or allow rules that let the agent do the work.

| Exit code | Meaning |
|---|---|
| `0` | The goal was verified |
| `2` | The run ended without the goal being verified (failed or paused); the goal's state is printed to stderr |
| `1` | The run itself failed (no model, a provider error, …) |
| `130` | Aborted |

`/goal` uses the default budget: `goal.max_iterations` and no cost or time limit.

## Life of a goal

| Status | Meaning |
|---|---|
| `active` | The lead is working on it |
| `verifying` | The checker is running |
| `done` | The checker verified it |
| `failed` | The checker rejected it `max_iterations` times |
| `paused` | Waiting for you: blocked, over budget, stalled, paused by hand, or the session was reopened |

While a goal is open every request of the lead ends with a note that carries `<goal>…</goal>` with the objective (after the conversation, so setting or closing a goal leaves the [prompt cache](models-and-logins.md#prompt-cache) as it was), and the lead (not sub-agents) has two extra tools:

| Tool | Input | Effect |
|---|---|---|
| `goal_done` | `summary`, `evidence` | Claims the goal is met and runs the checker. Returns `verified`, or an error listing what is still wrong. |
| `goal_blocked` | `reason` | Says the agent cannot continue without you. The goal becomes `paused`. |

**Nudges.** If the lead stops while the goal is still `active`, it is sent:

```text
The goal is not marked complete. Continue working, call goal_done with evidence, or call goal_blocked.
```

After `goal.max_nudges` nudges in a row with no `goal_done` in between, the next stop pauses the goal and you are notified.

**Budgets.** Each rejected `goal_done` counts one iteration; at `max_iterations` the goal is `failed` and the agent is told to stop and report what is still wrong. The cost and time limits are checked whenever the lead stops with the goal still open; exceeding one pauses the goal with the reason. Cost is the spend of the lead and all sub-agents, and is only known for models with prices in the [catalogue](models-and-logins.md#model-catalogue).

Each change of state is recorded in the session transcript. When a session with an unfinished goal is reopened, the goal comes back `paused`; `/goal resume` continues it with the default budget.

## The checker

`goal_done` (and `/verify`) runs these stages:

1. **Commands.** The commands in `verify.commands`, or the auto-detected ones when that list is empty, run one after another in the project root, in the same shell and environment as the agent's `shell` tool (including the [push guard](permissions.md#agents-never-push)). Each may take `verify.timeout_secs`. The run stops at the first command that exits non-zero, times out or cannot be started.
2. **A command failed.** The verdict is a failure naming the command, with the last 200 lines of its output. The verifier agent does not run.
3. **Verifier agent.** When the commands pass and `verify.agent` is true, a [`verifier`](team.md#built-in-agents) sub-agent is started. It receives the objective, the lead's summary and evidence, the command results, and the diffstat and diff of the project since the goal began (up to 60 000 bytes; taken from [checkpoints](sessions-and-checkpoints.md), so absent when checkpoints are off). If no command is known, it is told to find and run the project's build and tests itself. It can read and run commands but not `write` or `edit`.
4. **Verdict.** The verifier's report must end with a fenced JSON block:

   ````text
   ```json
   {"pass": false, "failures": [{"what": "login still returns 500 for empty passwords", "evidence": "curl output …"}]}
   ```
   ````

   `{"pass": true, "failures": []}` passes. A report without a readable block fails with `verifier produced no verdict`; so does a verifier that fails or is stopped.

On a pass, `goal_done` returns the commands with their results followed by `verified`, and the goal is `done`. On a failure it returns an error result such as:

```text
$ cargo build --all-targets (exit code 0)
$ cargo test (exit code 101)
not verified:
- `cargo test` failed (exit code 101)
    test parser::tests::empty_input ... FAILED
    …

Fix this and call goal_done again (attempt 2 of 8).
```

With `verify.agent = false` only the commands decide. If there are also no commands, nothing is checked and every claim passes, so keep at least one of the two.

The verifier runs on the `@verify` role; point it at a different model than the lead with `[roles] verify = [...]` or `[agents.verifier] model = [...]`.

## Auto-detected commands

When `verify.commands` is empty, the commands come from files at the project root. Every matching row contributes, in this order:

| Marker | Commands |
|---|---|
| `Cargo.toml` | `cargo build --all-targets`, `cargo test` |
| `package.json` | `<pm> run <script>` for each of the scripts `typecheck`, `lint`, `build`, `test` that exists, in that order. `<pm>` is `pnpm` with `pnpm-lock.yaml`, `yarn` with `yarn.lock`, `bun` with `bun.lock` or `bun.lockb`, otherwise `npm`. |
| `go.mod` | `go build ./...`, `go vet ./...`, `go test ./...` |
| `pyproject.toml`, `pytest.ini`, or a `tests/test_*.py` file | `python -m pytest -q` |
| a `*.sln` or `*.csproj` file | `dotnet build`, `dotnet test` |
| `Makefile` with a line starting `test:` | `make test` |

`azazel init` writes the detected list into a new `<project>/.azazel/config.toml` so you can edit it.

## Configuration

```toml
[verify]
commands = ["cargo clippy -- -D warnings", "cargo test --workspace"]  # empty: auto-detect
agent = true          # run the verifier agent after the commands pass
timeout_secs = 900    # per command
on_stop = false       # see below

[goal]
max_iterations = 8    # rejected goal_done calls before the goal fails
max_nudges = 3        # stops in a row without finishing before the goal is paused
```

`verify.commands` in a project's config names programs to run, so it is ignored until the project is trusted (or `--trust` is passed); auto-detection is used meanwhile. See [configuration](configuration.md).

## `/verify`

`/verify [what should be true]` runs the checker on the work so far without a goal and prints the same report. Without an argument the objective given to the verifier is "The work done in this session is complete and correct." No diff is supplied, so the verifier inspects the project itself.

## `verify.on_stop`

With `on_stop = true`, ordinary sessions get the command stage too. When the lead finishes a prompt during which it used `write`, `edit` or `shell`, the commands run. If one fails, its output is sent back to the lead with the instruction to fix it before finishing. This happens at most twice per prompt, and the verifier agent is not involved. It does not apply while a goal is active.

## Example: a CI job

```sh
azazel goal "all tests in tests/api pass" \
  --mode auto --trust --max-iterations 4 --max-minutes 20 --output json > run.jsonl
echo "exit: $?"   # 0 verified, 2 not verified
```

Each line of `run.jsonl` is one event tagged with the agent that produced it; the `goal_done` tool results show every check that ran.

See also [team](team.md) for the verifier definition and [hooks](hooks.md) for running your own checks on `stop`.
