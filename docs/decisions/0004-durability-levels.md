# 0004: Durability levels: visible vs. durable

- **Status:** proposed (M1 approval gate)
- **Date:** 2026-10-31

## Context

A mapped write becomes visible to other processes long before it reaches stable storage. Users must be able to
choose between speed and durability, and must be told precisely what each choice survives. Spec:
[`crash-contract.md`](../crash-contract.md).

## Options considered

1. **Two explicit levels: every `append` is visible; `checkpoint()` makes everything so far durable.** Simple to
   explain and test; users batch checkpoints at the rate they can afford.
2. **Per-append durability flag.** Same mechanism, more API surface, and invites per-record `fsync` that costs
   milliseconds per record on macOS.
3. **Background flusher thread** (periodic `msync(MS_ASYNC)`). Adds a thread and timing to the library and still
   gives no guarantee at any particular moment. Applications can do this themselves with `checkpoint()`.

## Decision

Option 1. `checkpoint()` syncs the dirty range of every segment written since the last checkpoint and the directory
if a segment was created: on Linux `msync(MS_SYNC)` + `fdatasync` (+ directory `fsync`); on macOS `msync(MS_SYNC)` +
`fcntl(F_FULLFSYNC)` (+ directory `F_FULLFSYNC`, falling back to `fsync`). On macOS we always use `F_FULLFSYNC`,
because plain `fsync` there does not flush the drive's cache and would make "durable" mean less than on Linux. A
roll additionally `fdatasync`s the new segment before renaming it into place, so that a power loss can't leave a
visible segment with a garbage header in the common case.

`Drop` does not checkpoint: dropping the writer leaves everything visible and in the page cache, which survives the
process. Applications call `checkpoint()` before exit if they need durability.

We do **not** claim power-loss safety. Process-kill tests (C1–C4) prove only the visible level. C7 stays marked
untested unless a VM- or filesystem-level crash harness is added later.

## Consequences

- A failed sync is returned to the caller. On Linux, a failed `fsync` may already have dropped the dirty pages
  ("fsyncgate", PostgreSQL 2018), so after an error durability since the last good checkpoint is unknown. We report
  it; we don't retry silently.
- After a power loss, sequence numbers of lost, never-checkpointed records are reused. Documented in the crash
  contract; a durable-only reader mode is a "Later" item.
- `checkpoint()` latency is a required benchmark (M7), on both platforms.
