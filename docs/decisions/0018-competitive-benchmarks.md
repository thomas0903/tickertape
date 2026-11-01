# 0018: Benchmark against other tools

- **Status:** accepted (owner decision, 2026-11-01; supersedes the v0.1 non-goal "any latency claim compared with
  Chronicle, Aeron or other tools")
- **Date:** 2026-11-01

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
