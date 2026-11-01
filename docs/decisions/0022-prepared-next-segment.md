# 0022: Prepare the next segment in the background

- **Status:** accepted
- **Date:** 2026-11-01

## Context

The recorded comparison on the owner's Mac (`docs/benchmarks.md`, 2026-11-01) showed tickertape's writer-to-reader
latency for 1,024-byte records far behind Chronicle Queue's:

| Rate | tickertape p99.9 | Chronicle Queue p99.9 |
|---|---|---|
| 10,000 records/s | 3.5–4.4 ms | 14 µs |
| 100,000 records/s | 14–16 ms | 3.1–3.6 ms |

The late sends point at segment rolls. A roll creates, preallocates and syncs a 128 MiB file on the writer's thread.
A scratch timing of the roll's steps on APFS (not a recorded benchmark) gave:

| Step | Time |
|---|---|
| `fsync` after `F_PREALLOCATE` | 45–50 ms |
| header `pwrite` + `fsync` | about 4 ms |
| `mmap` | 30 µs |
| link + unlink | 0.6 ms |

The new segment's first pages then also fault on the writer's thread, because the prefault helper (ADR 0020) only
starts on them after the roll.

## Options considered

1. **Prepare a spare segment ahead of time on the prefault helper thread** (chosen): create it, preallocate it,
   sync it, map it and prefault its first pages. At roll the writer only writes the header, syncs it and links the
   file into place, so the header is still durable before the file is published.
2. **Skip the header sync at roll.** This is cheaper, but a power loss could then leave a published successor with a
   zero header. With `RecoveryPolicy::Fail` the next open would refuse it.
3. **Sparse segments.** They give up crash-contract C10 on APFS (ADR 0020).

## Decision

**Spare.** With `WriterConfig::prefault` on, once the active segment is half full, the writer asks the helper for a
spare: `<dir>/next-segment.tmp`. The helper:
1. removes any stale file of that name;
2. creates the file with `O_EXCL`;
3. preallocates `segment_size` bytes and syncs the file;
4. maps it and prefaults the first 8 MiB;
5. hands the file and the mapping to the writer.

The spare stays all zeros, including its header, until it is used.

**Roll.** If a spare of the right size is ready, the writer does three things, in this order:
1. writes the header with `pwrite` and syncs it;
2. links the spare to `{base_seq:020}.tape`, then removes the `.tmp` name;
3. seals the old segment and switches to the new one.

These are the same publication steps as before, through the same `Link` failure point. If no spare is ready, or
any step fails before the link, the writer drops the spare (removing the file) and creates the segment
synchronously, exactly as before. So disk-full handling at roll (C9) is unchanged: a failed spare only means the
slow path runs.

**Recovery.** `next-segment.tmp` is never published and holds no records. Like `*.tape.tmp`, recovery removes it when
a writer opens the journal, and lists the removal in the recovery report.

## Consequences

- The roll's 45–50 ms preallocation sync moves off the writer's thread. The remaining cost is the header sync (about
  4 ms on the Mac). Benchmarks must be re-recorded.
- A spare reserves one more segment's worth of disk space from the time the active segment is half full. A full
  volume is therefore noticed earlier, by the helper, which then leaves the roll to the synchronous path. The real
  small-filesystem tests (C9, C10) cover this.
- With `prefault(false)` nothing changes. The tests that inject failures into synchronous segment creation run with
  that setting, and new tests cover the spare path: use, fallback, and a leftover spare after a crash.
- The kill loop runs with prefaulting on, so it kills writers while a spare is being prepared and while it is being
  published.
