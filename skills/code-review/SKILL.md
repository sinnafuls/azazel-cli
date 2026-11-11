---
name: code-review
description: Use to review a change (a diff, a branch, a pull request, or work another agent just finished) before it is accepted. Gives a rubric, severity levels and a findings format that separates defects from preferences.
---
# Code review

A review answers one question: is this change safe and correct to merge, and if not, what exactly must change. It is not a list of how you would have written it.

## Method

1. **Learn the intent.** Read the task, issue or commit message. If there is none, infer the intent from the diff and state it at the top of the review so a wrong reading is caught early.
2. **See the whole change.** `git status`, `git diff --stat <base>...HEAD`, then the diff itself. Note files you would have expected to change that did not: tests, documentation, migrations, callers.
3. **Read the surrounding code**, not just the changed lines. Open each changed function in full, and at least one caller and one callee. Most review defects are interactions with code outside the diff.
4. **Run it.** Build, run the tests that cover the change, and exercise the new behaviour once by hand. A reviewer who has not run the code is reviewing a text.
5. **Walk the rubric** below. Write each finding when you see it, with its location.
6. **Verify each finding** before reporting it: reproduce the bug, or point to the exact path that fails. Drop what you cannot support, or mark it as a question.

## Rubric

### Correctness
- Does it do what was asked, including the parts that were implied?
- Boundaries: empty, one, many, maximum, negative, zero, duplicate, unsorted, unicode, very long.
- Error paths: what happens when each call that can fail does fail? Are errors propagated with context, or swallowed?
- State: is anything left half-updated on failure? Are resources released on every path?
- Concurrency: shared data without synchronisation, check-then-act sequences, lock ordering, work done while holding a lock, cancellation.
- Arithmetic: overflow, truncation, integer division, floating-point comparison, time zones and units.

### Behaviour changes
- Public interfaces, file formats, configuration keys, command-line flags, database schemas: is any change backward compatible, or migrated?
- Are all call sites updated? Search for the old name and the old signature.
- Defaults that changed silently.

### Security
- Input reaching a query, a shell, a file path, a template or a deserialiser without validation.
- Missing authorisation on a new entry point; authorisation checked on a different object than the one acted on.
- Secrets in code, logs, error messages or test fixtures.
- For anything beyond a glance, apply the `security-review` skill.

### Tests
- Is the new behaviour tested, including a failure case?
- Would the tests fail if the change were reverted? If you cannot tell, revert locally and run them.
- Tests that assert on implementation details, sleep for timing, or depend on each other's order.

### Design and fit
- Does it follow the patterns the codebase already uses for the same job, or introduce a second way?
- Is the change in the right layer? Does a low-level module now know about a high-level one?
- Duplication of logic that already exists nearby.
- Complexity the task does not need: options nobody asked for, abstraction with one implementation.

### Readability
- Names that say what a thing is. Comments that explain why, not what.
- Functions that do one thing. Deep nesting that early returns would flatten.
- Dead code, commented-out code, leftover debugging output.

### Operations
- Logging at the right level, without sensitive data, with enough context to diagnose.
- Performance on realistic sizes: queries in loops, quadratic passes, unbounded memory.
- Feature flags, rollout and rollback when the change is risky.

## Severity

| Level | Meaning | Merge? |
|---|---|---|
| `blocker` | Wrong results, data loss, security hole, crash, broken build or interface | No |
| `major` | A real defect or missing test for important behaviour; a maintenance trap | Not until fixed or explicitly accepted |
| `minor` | Works, but should be better: naming, small duplication, missing edge-case test | Yes, fix soon |
| `nit` | Preference. Say it once, never insist | Yes |
| `question` | You do not understand something or cannot verify it | Answer first |

Be honest about level. A review with ten blockers that are really preferences teaches the author to ignore reviews.

## Output format

```
# Review: <what was reviewed, base..head>

Intent as I understand it: <one or two sentences>
Verdict: approve | approve with minor changes | changes required
Checked by: <build and test commands run, with results>

## Findings

1. [blocker] src/sync/queue.rs:142 — retries reuse the same idempotency key after a partial write
   The second attempt is rejected as a duplicate and the item is dropped.
   Reproduce: <command or steps>
   Suggested fix: derive the key from the attempt's payload hash, or clear it when the write is rolled back.

2. [major] …

## Questions
- …

## Not reviewed
- <files or aspects you skipped, and why>
```

Rules for findings:

- One finding per problem, with `path:line`, what is wrong, why it matters, and a concrete suggestion.
- Quote the code only as much as needed to locate it.
- Describe the problem in the code, not the author.
- Say what is good when it is notable; it tells the author what to keep doing.
- If you fixed nothing and only reviewed, say so. If asked to fix as well, fix blockers and majors, rerun the checks, and list what you changed.
