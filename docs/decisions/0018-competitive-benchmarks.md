# 0018: Benchmark against other tools

- **Status:** accepted (owner decision, 2026-11-01; supersedes the v0.1 non-goal "any latency claim compared with
  Chronicle, Aeron or other tools")
- **Date:** 2026-11-01; amended 2026-11-25 (what the configuration rule means for the JVM tools, issue #56)

## Context

The owner wants tickertape to be the best tool of its kind and to show it with measurements against popular
alternatives, in Rust and other languages.

## Decision

A comparison suite (`compare/`, outside the Cargo workspace) runs the same workloads against tickertape and the
closest tools that can be run locally without a licence: **Chronicle Queue** (open-source Java edition), **Aeron**
(IPC, and Archive recording where feasible), and Rust crates with overlapping semantics (**yaque**, **queue-file**),
plus a plain-file baseline (`write` + `read`). Workloads: append throughput (64 B and 1 KiB), cross-process
writer-to-reader latency at a fixed rate with intended-time measurement (as `tt-latency`), and replay throughput.

Fairness rules: each tool configured as its own documentation recommends for low latency, the same machine,
payloads, rates and warm-up for all, versions and settings recorded, and every result labelled with what the tool
guarantees (for example, whether data survives a process crash) so that tools doing less work aren't presented as
equivalent. Results go into `docs/benchmarks.md` like all others: measured runs only, both runs reported, no
extrapolation. Where tickertape loses, the docs say so.

## Consequences

- The README may state comparative results, citing `docs/benchmarks.md` and the conditions.
- The suite needs a JDK and Maven locally; it isn't part of CI (too heavy, too noisy), only built in CI if cheap.

## Amendment: configuration and warm-up of the JVM tools (2026-11-25)

A former Chronicle Queue user pointed out that the suite ran Chronicle on its defaults, so it did not follow the
fairness rule above, and that its short JVM warm-up left rarely taken paths to be compiled in the measured window
(issue #56). Review found the same for Aeron: its term buffers were sparse. The rule stands. For the JVM tools it now
means, in `compare/` (details and sources in `compare/README.md`):

- **Chronicle's documented settings, open-source edition.** `blockSize` is 1 GiB. Chronicle's README says about the
  64 MB default "in production you may want this to be high to reduce jitter from creating new chunks", and also "You
  should avoid changing `blockSize` unnecessarily"; 1 GiB is the README's `system.properties` example and what one of
  Chronicle's benchmarks sets (others set 128 MiB and 512 GiB). The documented pretoucher cannot be used: it is part
  of the licensed Enterprise edition only (since 5.24), and this suite compares what runs without a licence. The
  writer calls the open-source appender's `microTouch()` in its busy-wait instead, which takes the write fault of the next 4 KiB page. It is a weaker
  substitute, and every comparison with Chronicle says that its pretoucher did not run. Chronicle's own
  `bench/results.txt` shows it lowering the tail in one group of runs (p99.9 of about 15 to 16 down to 1.5) and not in
  a later one; the rerun publishes it alone as well.
- **Aeron's documented settings.** `termBufferSparseFile(false)`, as Aeron's `LowLatencyMediaDriver` sets it, and
  `preTouchMappedMemory(true)` in the clients: the driver touches the pages of its own mapping of the term buffers only,
  and the writer's faults are in the client's mapping.
- **The same warm-up for all** refers to the workload: the same unmeasured records at the same rate. In addition every
  JVM process first warms up its compiler on scratch data that crosses the rare paths (index arrays, chunks, terms,
  the histogram), deletes that data untimed, and finishes before the writer opens the measured endpoint. The other tools are compiled ahead of time and have no equivalent.
- **Evidence.** Every Java process logs GC, safepoints, handshakes, compilations and deoptimisations and its late sends, so that a
  tail sample can be attributed. Results recorded before this amendment did not have these settings; each can be
  switched off alone and is recorded in the result, so a rerun can publish both rows.

Rejected: calling `pretouch()` anyway (in the open-source 2026.8 it throws `StackOverflowError`); writing our own page
toucher for Chronicle (it would measure our code); larger heaps, Epsilon GC or other JVMs as defaults (available as
opt-in experiments where cheap; Chronicle's documentation does not ask for them).
