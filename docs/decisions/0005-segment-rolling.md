# 0005: Segments, preallocation and rolling

- **Status:** proposed (M1 approval gate)
- **Date:** 2026-10-31 (revised after the cross-provider review the same day)

## Context

A journal grows without bound, but a memory map should not be resized under readers. Segments must be easy to copy
and inspect, and creating one must not expose half-written files. Disk-full must be an error, not a crash.
Spec: [`format.md`](../format.md) → Segment file, Rolling.

## Options considered

1. **Fixed-size, preallocated segments named by first sequence number, created as `.tmp` and linked into place,
   rolled by size.** Bounds never change while mapped; `seek(seq)` is a directory listing plus a binary search on
   names; a reader finds the next segment from the sequence number in the seal.
2. **One growing file**, remapped as it grows. Readers must coordinate remaps; truncation risks SIGBUS.
3. **Segments named by index** with `base_seq` only in the header. Allows creating the next segment ahead of time,
   but `seek` must open headers and readers need a directory listing to find the next file.
4. **Rolling by size and by UTC day.** Useful operationally, but adds time-based edge cases.

## Decision

Option 1, size-based only (daily rolling is on the "Later" list). Segment size is configurable (multiple of 64 KiB,
64 KiB to 2^40 bytes, default 128 MiB) and recorded in each header; a writer may use a different size for new
segments than older ones have. Preallocation: `fallocate` on Linux, `fcntl(F_PREALLOCATE)` + `ftruncate` on macOS, so
disk-full shows up as an error at roll time rather than a SIGBUS later (with the copy-on-write caveats in the crash
contract).

A new segment is published with `link(tmp, final)` + `unlink(tmp)`, which fails instead of replacing an existing
file, so a roll can never swap out a segment a reader has mapped. An empty segment is never rolled (the new file would
need the same name): if the configured size was raised and the first record of an empty segment doesn't fit, the
append fails with `RecordTooLarge`. A failed roll removes its temporary file and is retried by the next append.
Segments are never deleted or truncated by tickertape; only stale `.tape.tmp` files, which are never segments, are
removed, and only by the lock holder.

The next segment is created at roll time, not ahead of time, which keeps naming simple at the cost of one segment
creation (preallocation, `fdatasync`, link) on the append that rolls. That latency is part of the M7 benchmarks.

## Consequences

- A record cannot be larger than one segment's record area. Larger appends fail with `Error::RecordTooLarge`.
- The append that triggers a roll is slower; latency-sensitive applications should choose large segments. Creating
  the next segment ahead of time is a possible later optimisation within the same file format.
- Unused space at the end of each sealed segment is wasted (less than one maximum-size record).
- Disk usage only grows. Operators archive or delete old segments themselves, while no reader uses them.
