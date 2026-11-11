---
name: debugging
description: Use when something fails, crashes, hangs or gives a wrong result and the cause is not yet known. A disciplined loop of reproduce, isolate, hypothesise, instrument, fix and prove, including bisection across history and inputs.
---
# Debugging

A bug is fixed when you can say why it happened, show the failure disappearing for that reason, and show it will be noticed if it returns. Anything less is a guess that happened to coincide with the symptom going away.

## 1. Reproduce

Nothing else starts until the failure happens on demand.

- Get the exact report: the command, the input, the environment, the full error text and stack trace. Run it yourself with `shell`; do not reason from a paraphrase.
- Reduce the reproduction to one command. Write it down. If it is slow, make it fast (a smaller input, a single test, a narrower filter); you will run it dozens of times.
- If the failure is intermittent, measure its rate first: run it 20 or 50 times in a loop and count. A fix cannot be judged without the baseline. Then look for what varies between runs: ordering, timing, time of day, leftover state, parallelism, random seeds.
- If you cannot reproduce it, compare environments (versions, configuration, data, platform) and collect more evidence from where it does fail: logs, core dumps, a trace. Say plainly that it is not reproduced.

## 2. Isolate

Shrink the distance between a known-good state and the failure. Bisect, do not browse.

- **Across history**: if it worked before, `git bisect` finds the change. `git bisect start`, mark a bad and a good commit, then `git bisect run <the one-command reproduction>` when the command's exit code tells pass from fail.
- **Across input**: halve the input until the failure stops; keep the half that fails. Stop when removing anything makes it pass.
- **Across code**: find the last point where the state is right and the first where it is wrong, and move them toward each other. Check intermediate values at the midpoint of the pipeline rather than reading the pipeline from the top.
- **Across configuration**: defaults against the failing configuration, one difference at a time. The same for dependency versions.

Read the stack trace from the innermost frame in your own code outward. Read the error message twice, literally; it often names the cause.

## 3. Hypothesise

State one cause that would produce exactly the observed symptom, and one observation that would prove it wrong. "The cache returns a stale entry because the key omits the locale; if so, the same request with caching disabled will succeed." Then make that observation.

- Prefer the hypothesis that explains all the evidence, including what did not fail.
- If the test contradicts the hypothesis, discard it fully. Do not patch it with a second assumption.
- Keep a short log of hypotheses tried and their results. It prevents circles and is most of the final explanation.

## 4. Instrument

Look instead of thinking harder.

- Add temporary logging at the boundary you are bisecting: inputs, outputs, the branch taken. Print types and lengths as well as values; print identity (addresses, ids) when aliasing is possible.
- Use the debugger when state is complex: break at the first wrong value, inspect, step back up the stack. A debugger reaches agents through an MCP server when one is configured.
- Use the tools built for the class of bug: sanitizers for memory and threads, `strace`/`dtrace`/Process Monitor for system interaction, a packet capture for network, `EXPLAIN` for queries, the language server's diagnostics (`lsp`) for type errors.
- For timing bugs, logging can hide the failure. Record events into a buffer and dump it afterwards, or make the race more likely with a deliberate delay at the suspected point.
- Remove the instrumentation when you finish, or turn the useful part into permanent logging on purpose.

## 5. Fix

- Fix the cause, at the place where the wrong state is created, not where it is noticed. A null check at the crash site hides a missing initialisation elsewhere.
- Make the smallest change that removes the cause. Refactoring rides in a separate change.
- Ask where else the same mistake exists: `grep` for the pattern and fix the siblings, or list them in the report.
- If the real fix is large or risky, say so, and offer the contained fix and the proper one as separate steps.

## 6. Prove

1. Write a test that fails for the reason you identified. Run it before the fix and see it fail; a regression test that never failed proves nothing.
2. Apply the fix. The test passes.
3. Run the original one-command reproduction. For intermittent failures, run it enough times that a pass means something given the measured rate.
4. Run the surrounding test suite for collateral damage.

## Report

- **Symptom**: what failed and how to reproduce it.
- **Cause**: the mechanism, in two or three sentences, with file and line.
- **Fix**: what changed and why that removes the cause.
- **Proof**: the test added and the commands run with their results.
- **Also**: related instances found, and anything not verified.

## Traps

- Changing several things at once, then not knowing which mattered.
- Fixing the first anomaly you see. Confirm it causes the reported symptom.
- Trusting "it cannot be that". Check the things that cannot be: the build is stale, the wrong file is being loaded, the test is not running the code you edited, the environment variable is not set in this shell.
- Stopping at "it works now" without a cause. If the failure vanished and you do not know why, it will be back.
