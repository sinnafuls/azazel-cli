---
name: performance
description: Use when something is too slow or uses too much memory, or before optimising anything. Measure first, find the bottleneck with the right profiler for the ecosystem, change one thing, and prove the gain with a benchmark.
---
# Performance

Optimising without measuring is guessing, and the guess is usually wrong: the time goes somewhere unexpected. Every step here produces a number.

## 1. Define the target

- What is slow, for whom, and how slow: "the report page takes 9 s for accounts with 50k rows", not "the app is slow".
- What would be good enough. Stop when you reach it.
- Which measure matters: latency (median and tail), throughput, memory peak, start-up time, binary size, energy.

## 2. Build a reproducible measurement

- One command that runs the slow scenario with realistic data and prints the number. Realistic size matters: a problem that is quadratic is invisible on ten rows.
- Measure an optimised build. Debug builds mislead.
- Run it several times; report the median and the spread. For short operations use a benchmark harness that iterates and handles warm-up. For command-line programs, `hyperfine` gives statistics over runs.
- Control noise: same machine, nothing heavy running, same inputs, warm caches unless cold start is the subject.
- Record the baseline before changing anything.

## 3. Find where the time goes

Profile; do not read code looking for something slow.

| Ecosystem | CPU | Memory | Notes |
|---|---|---|---|
| Rust, C, C++ | `perf record` + `perf report` or a flamegraph (`cargo flamegraph`), Instruments on macOS, Windows Performance Recorder or VTune on Windows | heaptrack, DHAT (Valgrind), `dhat` crate, Instruments Allocations | Build with optimisation and debug symbols |
| Go | `pprof` CPU profile (`go test -cpuprofile`, `net/http/pprof`) | `pprof` heap and allocs profiles | `go tool trace` for scheduling and blocking |
| JavaScript and TypeScript | Node `--cpu-prof`, the browser Performance panel | Heap snapshots, allocation timeline | In the browser also check layout, paint and long tasks |
| Python | `cProfile` + `snakeviz`, `py-spy` for running processes | `tracemalloc`, `memray` | `py-spy dump` shows where a hung process is |
| Java and Kotlin | async-profiler, Java Flight Recorder | JFR allocation profiling, heap dumps | Watch garbage-collection pauses |
| .NET | `dotnet-trace`, PerfView | `dotnet-counters`, `dotnet-gcdump` | |
| SQL | `EXPLAIN ANALYZE`, the slow query log | | Count queries per request first |

Read a profile top down: which top-level operation owns the time, then which call under it. Look at inclusive time to find the region and self time to find the hot function. A wide flat profile with no dominant frame means the cost is spread (allocation, cache misses, too much work overall) rather than one bad function.

When a profiler is not available, bracket regions with a monotonic clock and print durations, narrowing by halves.

Check the kind of slowness before the details:

- **CPU-bound**: a core is busy. Profile CPU.
- **Waiting**: the process is idle while time passes. Look at I/O, locks, network round trips, sleeps.
- **Memory-bound**: swapping, garbage collection, allocator time. Profile allocations.

## 4. Common causes, most frequent first

1. **Doing the work too many times**: a query, a file read or a network call inside a loop; recomputing the same value; re-rendering what did not change. Fix by batching, hoisting, caching for the scope where the value is valid.
2. **The wrong algorithm or data structure**: linear search inside a loop, repeated sorting, list membership tests, string concatenation in a loop, nested loops over the same collection. Use a map or set, sort once, build with a buffer.
3. **Too much I/O**: unbuffered reads and writes, many small requests, synchronous calls in sequence that could run concurrently, missing connection reuse, missing database indexes, fetching columns and rows that are never used.
4. **Allocation and copying**: cloning large values, growing collections without reserving capacity, temporary objects in hot loops, converting between representations repeatedly.
5. **Contention**: a lock held across slow work, a single-threaded bottleneck in a parallel pipeline, false sharing, oversubscribed thread pools.
6. **Serialisation**: parsing and printing dominate more often than expected; reflection-based encoders, pretty printing, repeated encoding of the same data.
7. **Start-up work**: eager initialisation, loading everything before the first request, synchronous network calls at launch.
8. **Cache behaviour**: pointer chasing through scattered objects, arrays of large structures when only one field is read.

## 5. Change one thing

- Start with the largest item in the profile. A 5 % function made twice as fast is a 2.5 % gain.
- Prefer removing work to doing it faster.
- Make one change, rerun the measurement, keep or revert. Record each result.
- Keep the code as simple as it can be for the gain. If a change buys little and costs clarity, revert it.
- Check correctness after each change with the tests; fast and wrong is not an improvement.

## 6. Benchmarks worth keeping

When the scenario matters going forward, leave a benchmark in the project's harness (`criterion`, `go test -bench`, `pytest-benchmark`, JMH, `benchmark.js`):

- Realistic input sizes, at least two sizes to show how cost grows.
- Setup outside the timed region.
- Prevent the compiler from removing the work (use the harness's black-box helper).
- Note in the benchmark what it protects and the numbers at the time of writing.

## 7. Report

```
Target: <scenario and goal>
Baseline: <median, spread, how measured>
Finding: <where the time went, with the profile evidence>
Change: <what was done>
Result: <new median, spread; ratio to baseline>
Trade-offs: <memory, complexity, behaviour differences>
Left: <next largest cost, if the goal is not met>
```

Include the exact commands so the numbers can be reproduced.

## Traps

- Benchmarking a debug build, or a tiny input.
- Trusting a single run.
- Optimising the benchmark instead of the product: caching the one input the benchmark uses.
- Adding concurrency to hide an inefficient algorithm.
- Caches without a bound or an invalidation rule; they become memory leaks and stale-data bugs.
- Micro-optimising code that the profile shows is not hot.
