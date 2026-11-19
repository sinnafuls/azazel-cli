---
name: refactoring
description: >-
  Use when restructuring code without changing what it does: extracting, renaming, moving, splitting modules, replacing a pattern across a codebase, or cleaning up before a feature. Characterisation tests first, then small mechanical steps that each keep the build green.
---
# Refactoring

Refactoring changes structure and preserves behaviour. The discipline is in proving the second half. If behaviour must change too, that is a separate change, before or after, never mixed in.

## 1. Decide what and why

- Name the problem the structure causes: a change that had to be made in four places, a function nobody can test, a module that knows too much. Refactor toward the next real change, not toward an ideal.
- Define done: which code moves where, which interface results. Write it in the plan.
- Agree the limits: public interfaces that must not change, files that are out of scope.
- Leave `shortcut: <limit>; upgrade when <trigger>` comments alone: each records a decision to do less. Remove one only when its trigger has happened and the refactoring is the upgrade; the `debt-ledger` skill lists them.

## 2. Pin the behaviour

Before the first edit, make sure a wrong step will be caught.

1. Run the existing tests. Note the baseline, including failures that already exist.
2. Check that they exercise the code you will touch: break it on purpose in one place and see a test fail. If none does, coverage is missing where it matters.
3. Add **characterisation tests** for the uncovered behaviour. These record what the code does now, right or wrong:
   - call the code with representative inputs, including the odd ones, and assert on the actual outputs;
   - for code with side effects, capture them at the boundary (files written, queries issued, messages sent);
   - for large outputs, snapshot them.
   If you find a bug while doing this, record it and leave it. Fixing it changes behaviour.
4. Note which behaviour cannot be pinned (timing, external systems) and take extra care there.

## 3. Take small steps

Each step is one mechanical transformation, leaves the code compiling and the tests passing, and can be reverted alone.

| Step | How to keep it safe |
|---|---|
| Rename | Use the language server (`lsp rename`) when available; otherwise `grep` for every occurrence, including strings, configuration, documentation and reflection |
| Extract function | Move the block unchanged; pass what it reads, return what it writes; no logic edits in the same step |
| Inline | The reverse; check each call site |
| Move to another module | Move first with a re-export or forwarding function left behind; update callers; then remove the forwarder |
| Change a signature | Add the new form beside the old; migrate callers in groups; delete the old |
| Replace conditional with dispatch | Introduce the new structure for one case, verify, then move the rest one at a time |
| Split a type or module | Extract one cohesive group of fields and the functions that use only them |
| Remove duplication | Make the copies identical first, then replace them with one |
| Delete dead code | Prove it is unreferenced: search, including dynamic uses, and build |

After each step: build, run the relevant tests, and commit locally if the user's workflow uses commits (push only when the user asks). When a step breaks something and the cause is not obvious in a minute, revert the step and take a smaller one.

## 4. Moving code across a codebase

For a change that touches many files (an API replaced everywhere, a pattern rewritten):

1. Find every site: `grep` for the name and for the shapes that do not use the name directly. Keep the list.
2. Make the new form available alongside the old.
3. Convert in batches that each build, grouped by module. Use the same transformation each time; if a site does not fit, stop and understand why rather than forcing it.
4. When no caller remains, remove the old form and anything that existed only to support it.
5. Search once more for leftovers: comments, documentation, examples, configuration, test names.

For wide mechanical edits, sub-agents can each take a disjoint set of files; give every one the exact transformation and the rule that files belong to one agent only.

## 5. Keep behaviour and structure apart

- Do not fix bugs, change messages, reorder side effects or "improve" logic during a refactoring step. Note them for a later change.
- Do not reformat files you are moving code through; formatting noise hides the real diff. If the project's formatter must run, give formatting its own step.
- Beware of changes that look structural but are not: reordering evaluation, changing when something is initialised, altering visibility that reflection depends on, changing iteration order, turning eager into lazy.

## 6. Finish

- The full test suite passes, with the same baseline as before plus the tests you added.
- Build with warnings on: unused imports, functions and parameters point at leftovers.
- The diff contains no behaviour change. Read it once with that single question.
- Remove scaffolding: forwarders, compatibility aliases, temporary flags.
- Decide what to do with characterisation tests: keep those that describe intended behaviour, rewrite those that pinned accidents, delete those that only pinned structure.

## Report

What was restructured and why; the steps taken; how behaviour was pinned and verified (commands and results); anything found on the way that should be changed separately (bugs, further clean-up); anything not covered by tests.

## When not to

- No tests and no way to add them: say so, and propose the smallest seam that makes testing possible first.
- The code is about to be deleted or replaced.
- The refactoring is larger than the change it is meant to enable, and nothing else will benefit.
