# 0005: Segments, preallocation and rolling

- **Status:** proposed (M1 approval gate)
- **Date:** 2026-10-31

## Context

A journal grows without bound, but a memory map should not be resized under readers. Segments must be easy to copy
and inspect, and creating one must not expose half-written files. Disk-full has to be an error, not a crash.
Spec: [`format.md`](../format.md) → Segment file, Rolling.

## Options considered

1. **Fixed-size, preallocated segments named by first sequence number, created as `.tmp` and renamed into place,
   rolled by size.** Bounds never change while mapped; `seek(seq)` is a directory listing plus a binary search on
   names; a reader finds the next segment from the sequence number in the seal.
2. **One growing file**, remapped as it grows. Readers must coordinate remaps; truncation risks SIGBUS.
3. **Segments named by index** (`0`, `1`, …) with `base_seq` only in the header. Allows creating the next segment
   ahead of time, but `seek` must open headers, and readers need a directory listing to find the next file.
4. **Rolling by size and by UTC day.** Useful for operations, but adds time-based edge cases.

## Decision

Option 1, size-based only (daily rolling is on the "Later" list). Segment size is configurable (multiple of 64 KiB,
64 KiB to 2^40 bytes, default 128 MiB) and recorded in each header; a writer may use a different size for new
segments than older ones have. The next segment is created at roll time, not ahead of time, which keeps the naming
simple at the cost of one segment creation (preallocation + `fdatasync` + rename) on the append that rolls; its
latency is part of the M7 benchmarks. Preallocation: `fallocate` on Linux, `fcntl(F_PREALLOCATE)` + `ftruncate` on
macOS, so disk-full shows up as an error at roll time rather than a SIGBUS on a later write (with the copy-on-write
caveats in the crash contract). Segments are never deleted or truncated by tickertape; only stale `.tape.tmp` files,
which are never segments, are removed by the writer on open.

## Consequences

- A record cannot be larger than one segment's data area. Larger appends fail with `Error::RecordTooLarge`.
- The append that triggers a roll is slower; applications with tight latency budgets should choose large segments.
  Creating the next segment ahead of time is a possible later optimisation behind the same file format.
- Unused space at the end of each sealed segment is wasted (at most one maximum-size record).
- Disk usage only grows. Operators archive or delete old segments themselves, while no reader uses them.
