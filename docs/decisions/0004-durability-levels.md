# 0004: Durability levels: visible vs. durable

- **Status:** proposed (M1 approval gate)
- **Date:** 2026-10-31 (revised after the cross-provider review the same day)

## Context

A mapped write becomes visible to other processes long before it reaches stable storage. Users must be able to
choose between speed and durability and be told precisely what each choice survives. Spec:
[`crash-contract.md`](../crash-contract.md).

## Options considered

1. **Two explicit levels: every `append` is visible; `checkpoint()` makes everything so far durable.** Simple to
   explain and test; users checkpoint as often as they can afford.
2. **Per-append durability flag.** More API surface, and invites per-record syncs that cost milliseconds each on macOS.
3. **Background flusher thread.** Adds threads and timing to the library and still guarantees nothing at a given
   moment. Applications can do this themselves with `checkpoint()`.

## Decision

Option 1. `checkpoint()` syncs everything dirty since the last successful checkpoint: data ranges of every segment
touched (the writer keeps rolled-past segments mapped until then), directory entries created or renamed (including the
journal directory's own entry in its parent if the writer created it), then a barrier, then writes the checkpoint
marker (ADR 0007) and syncs it. The exact calls per platform are in the crash contract. On macOS we always use
`F_FULLFSYNC`, because plain `fsync` there doesn't flush the drive's cache; if it's unsupported, `checkpoint()` fails.

**Errors are sticky.** After a failed sync the writer is poisoned: further checkpoints fail with
`Error::DurabilityLost`, because on Linux a failed `fsync` may have dropped the dirty pages ("fsyncgate", 2018), so a
later success proves nothing. A new writer in the same boot can't detect this; the crash contract says so plainly
instead of pretending otherwise.

A roll `fdatasync`s the new segment before linking it into place, so a power loss can't leave a visible segment with
an unsynced header in the common case. `Drop` doesn't checkpoint: data stays visible and in the page cache, which
survives the process. We do **not** claim power-loss safety: process-kill tests prove only the visible level, and C7
stays untested unless a VM-level crash harness is added later.

## Consequences

- A checkpoint costs at least two sync rounds (data, then marker); on macOS, two drive-cache flushes. Measured in M7.
- After a power loss, sequence numbers of lost, never-checkpointed records are reused. A durable-only reader mode is
  on the "Later" list.
- Durability depends on storage assumptions (honest flushes, durable blocks not damaged by later writes to the same
  block), stated in the crash contract.
