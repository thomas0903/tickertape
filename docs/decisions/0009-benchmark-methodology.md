# 0009: Benchmark harness methodology

- **Status:** accepted
- **Date:** 2026-10-31

## Context

M7 needs repeatable append, replay, visible-latency and checkpoint measurements without changing the journal API.
Results need to distinguish page-cache state, durability and scheduling backlog.

## Options considered

1. Time from actual sends and reuse append journals: simpler, but hides scheduling backlog and mixes page states.
2. Use intended send timestamps, fresh append batches and explicit warm/cold replay modes.

## Decision

Use Criterion 0.5 for append and warm replay, HdrHistogram with three significant figures for latency, tempfile
for isolated setup and serde_json for machine-readable output. Check all targets on the workspace MSRV.
Criterion reports payload bytes and records in separate groups because each group supports one throughput unit.
Append setup, teardown and initial segment creation are outside timing; each batch first touches fresh segment
pages. Replay includes reader opening and validation, with an untimed replay before each warm-cache sample.

The latency parent starts separate writer and reader processes and releases a readiness barrier with a future
wall-clock origin. The writer sends every scheduled record, including warmup, without skipping or rebasing.
Readers measure from the intended timestamp, so backlog contributes to latency. A backwards clock observation
fails the run rather than wrapping a duration. Wall-clock adjustments otherwise remain a documented limitation.
Child failures and bounded deadlines terminate and reap peers. Existing nonempty journal directories are refused.

Optional cold replay checkpoints and closes the writer before attempting a noninteractive platform cache purge.
An unavailable purge exits successfully without a measurement. This optional mode requires root privileges;
normal benchmarks and CI neither invoke sudo nor require root. Cache eviction is best effort, never guaranteed.

Checkpoint runs use a fresh journal and fixed-size records. Segment capacity follows the public format helpers;
sequence boundaries identify roll appends without adding filesystem scans to each measurement. Appends, dirty
checkpoints and a final checkpoint with nothing new are reported separately. Overall throughput includes append
instrumentation and checkpoint time, excluding opening and the final empty checkpoint.

## Consequences

The harness does not claim core pinning, cold-cache guarantees, clock accuracy or comparable environments.
Unit tests exercise schedules, format-based roll classification and histogram units; process tests exercise the
barrier, both readers, warmup accounting, JSON output and invalid input. CI runs only short functional smoke runs.
The orchestrator records measured results using `docs/playbooks/run-benchmark.md`.
