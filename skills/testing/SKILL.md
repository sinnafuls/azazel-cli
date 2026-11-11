---
name: testing
description: Use when writing or extending tests, deciding what level a test belongs at, or dealing with flaky or slow tests. Covers choosing behaviours and boundaries, fixtures and test data, doubles, and making tests deterministic.
---
# Testing

A test earns its place by failing when the behaviour a user depends on breaks, and by staying quiet otherwise. Tests that restate the implementation fail on every refactor and catch nothing.

## 1. Find out how this project tests

Before writing anything, read: the test command in the README, manifest or CI configuration; two existing test files near the code you are changing; any shared fixtures or helpers. Use the same framework, layout, naming and helpers. Run the existing suite once so you know its baseline (how long, any failures that were already there).

## 2. Choose the level

| Level | Exercises | Use when | Cost |
|---|---|---|---|
| Unit | One function or type, no I/O | Logic with many cases: parsing, calculation, state machines | Milliseconds |
| Integration | Several units with real collaborators: database, file system, HTTP against a local fake | Behaviour lives in the interaction: queries, serialisation, wiring | Tens of milliseconds to seconds |
| End to end | The built program through its real interface | A few critical paths, to prove the pieces are connected | Seconds and up |

Put each behaviour at the lowest level that can observe it. Many cases at unit level, a handful of paths at integration level, one or two journeys end to end. If a unit test needs five mocks to run, the behaviour belongs at integration level.

## 3. Choose what to assert

Write down the behaviours before the code of the test. For each public operation:

- **The main case**: typical input, expected result.
- **Boundaries**: empty, one element, the maximum, just past the maximum, zero, negative, duplicate, already sorted and reversed, the first and last valid values of every range.
- **Classes of input**: one representative of each kind that is handled differently. Do not test five values that take the same path.
- **Errors**: each way the operation can be refused, with the error the caller will see.
- **State transitions**: allowed and refused transitions; doing the same thing twice; order dependence.
- **Invariants**: properties that hold for any input (round trip, idempotence, ordering, conservation). These suit property-based tests when the project has a library for them.
- **Precedence**: when two rules apply, which wins.

Assert on results a caller can observe: return values, persisted state, emitted messages, exit codes. Not on private fields, call counts or log text, unless that is the contract.

## 4. Shape of a test

- One behaviour per test. The name states it as a fact: `rejects_orders_over_the_credit_limit`, not `test_order_3`.
- Arrange, act, assert, in that order, with the act being one line.
- The expected value is written out literally. A test that computes its expectation with the same logic as the code under test proves nothing.
- No branches or loops with logic in a test. Table-driven cases are fine: a list of inputs and expected outputs, with the case shown in the failure message.
- A failure message should identify the case without a debugger.

## 5. Fixtures and data

- Build the smallest data that shows the behaviour. Three records, not three hundred.
- Use builder helpers with sensible defaults so each test states only what matters to it.
- Keep fixture files small, readable and beside the tests. Generate large or binary fixtures in code.
- Each test creates what it needs and cleans up through the framework's temporary directory or transaction. No test depends on what another left behind, and the suite passes in any order and in parallel.
- Snapshot tests suit large structured output (rendered text, serialised documents). Review every snapshot when it is created and when it changes; an unread snapshot is not a test.

## 6. Doubles

Replace only what you must: the network, the clock, randomness, third-party services, things that are slow or cost money.

- Prefer a fake (a small working implementation: an in-memory store, a local HTTP server with canned replies) over a mock that scripts call sequences.
- Inject the clock and the random source; never read the real ones in code you want to test.
- Do not mock the thing under test or types you own that are cheap to construct.
- For external HTTP APIs, record a real response once as a fixture and keep a note of where it came from.

## 7. Determinism

A flaky test is a bug: either in the test or in the product. Find which.

| Cause | Fix |
|---|---|
| Real time, sleeps, timeouts | Injected clock; wait for a condition, not a duration |
| Unordered collections compared in order | Sort, or compare as sets |
| Shared state: globals, environment variables, fixed ports, a common directory | Per-test instances; port 0; temporary directories |
| Test order dependence | Remove the dependence; run the suite shuffled to find it |
| Concurrency in the code under test | Make the race deterministic with explicit synchronisation points; run the test many times in a loop to confirm |
| Network access | Local fakes only; the suite passes offline |
| Locale, time zone, line endings, path separators | Set them explicitly or compare normalised values |

To investigate: run the one test in a loop (50 to 500 times), alone and then with the rest of its file, and compare failure rates. Do not add retries to hide flakiness.

## 8. When fixing a bug

Write the failing test first, from the reproduction. See it fail for the right reason. Fix. See it pass. The test stays.

## 9. Before you finish

- Run the new tests, then the whole suite.
- Break the code on purpose (revert the change, flip a condition) and confirm the new tests fail. Restore it.
- Check the tests pass on the platforms the project supports when paths, shells or line endings are involved.
- Report what is covered, what is deliberately not covered and why, and the commands run with their results.

## Not worth testing

Trivial accessors, framework behaviour, generated code, exact wording of messages that are not a contract, and the fact that a function was called. Spend the effort on boundaries and error paths instead.
