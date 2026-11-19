---
name: lean
description: "Use before writing or changing code, to choose the smallest change that fully does the job. A ladder from \"does it need to exist\" down to \"the minimum that works\", what is never cut, and how to mark a deliberate limit."
---
# Lean coding

The best change is the smallest complete one. Before you write code, take the first rung of this ladder that fully solves the problem, and stop there.

## The ladder

1. **Does it need to exist?** Re-read the request. If the need is already met, or the feature answers a question nobody asked, say so and do not build it.
2. **Is it already in the codebase?** Search for the helper, type, config key or test utility before adding one (`grep`, `lsp symbols`, `semantic_search`). Reuse it, even if it needs a small extension.
3. **Does the standard library or the platform do it?** Prefer the language's own function, the shell, the operating system or the framework's built-in over new code.
4. **Does an installed dependency do it?** Look at what the project already depends on before writing it by hand. Do not add a dependency for a few lines.
5. **Can it be one line?** A single expression or call in place of a new function, a new file or a new abstraction.
6. **Otherwise, the minimum that works.** The least code that meets the stated need today: one case, not the family of cases; one caller, not a framework.

Say which rung you took when it is not obvious ("the standard library has `retain`, so no helper").

## What the ladder never cuts

Smaller must not mean unsafe. Keep, whatever the size of the change:

- validation of input at trust boundaries: files, network, user input, other processes, environment;
- handling that prevents data loss: atomic writes, error propagation where a failure would lose or corrupt data, cleanup on failure;
- security: authorisation checks, escaping, path and command construction, secrets;
- accessibility: names, focus order, contrast and keyboard use in anything a person operates.

Inside the program, between functions that trust each other, do not add checks for states that cannot happen.

## Habits that keep a change small

- **Grep the callers before fixing a bug.** Find every use of the function, and every copy of the pattern. Fix the cause where it lives, once, and say how many call sites you checked.
- **One small test for new non-trivial logic.** A branch, a parse, a calculation: one test of the behaviour a caller sees, including the case that would have been wrong. Do not test trivial glue.
- **No options nobody asked for.** No flag, config key, parameter or extension point without a user for it today. Ask before adding an optional extra; do not build it first.
- **No drive-by work.** Leave nearby code alone unless the change needs it. Mention what you noticed instead.
- **Delete when you can.** Code the change makes unused goes in the same change. Report the net lines.

## Marking a deliberate limit

When you choose the minimum knowing it will not scale or cover every case, leave one comment at the spot:

```
// shortcut: <the limit>; upgrade when <the trigger>
```

For example `// shortcut: reads the whole file into memory; upgrade when files pass 100 MB`. Use the comment syntax of the language. The limit says what the code does not handle; the trigger is an observable event, not "later". A shortcut is a decision with a date on it, not a defect. The `debt-ledger` skill collects them.

Do not use the marker for bugs, unfinished work or things you forgot: those are fixed, or reported.

## Check before you finish

- Could any part of the diff be removed and the need still be met?
- Did you add anything no caller uses yet?
- Is every limit you chose either harmless or marked?
- State in the report what you did not run or check, and any risk.
