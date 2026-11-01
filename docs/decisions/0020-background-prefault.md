# 0020: Prefault the active segment from a background thread

- **Status:** accepted (supersedes the rejection of option 4 in [0014](0014-prefaulting-segment-pages-rejected.md))
- **Date:** 2026-11-01

## Context

ADR 0014 suggested profiling where the writer stalls before trying another remedy. We did that, on the owner's Mac
(APFS) and in the OrbStack Linux VM (btrfs), with scratch programs that copy 104-byte and 1,060-byte records into a
freshly created, preallocated 64–128 MiB segment mapping. These are exploratory numbers, not recorded benchmarks:

- **APFS, preallocated segment** (what the writer does today): about 0.85 GB/s. Single writes stall for tens of
  microseconds once per 16 KiB page (p99.9 18–36 µs at 104-byte records). This is the writer's throughput ceiling:
  the CPU work per `append` is about 10 ns.
- **APFS, sparse file** (no preallocation): about 13 GB/s. Preallocated-but-unwritten blocks make every first write
  to a page slow. Sparse files would give up crash-contract C10 (writes into an existing segment are unaffected by
  a full volume), so dropping preallocation is not an option.
- **APFS, preallocated, after `madvise(MADV_WILLNEED)` on the range**: the later writes take no page faults at all
  (64 MiB copied in 0.6 ms). The `madvise` call itself takes about 23 ms per 64 MiB (about 2.8 GB/s).
- **APFS, a helper thread that keeps `MADV_WILLNEED` 8 MiB ahead of the writer**: 104-byte records went from
  0.7 GB/s to 2.55 GB/s, the p99.9 write went from 18–36 µs to 42 ns, and writes over 10 µs went from about 1,500
  to 3 per 128 MiB. At 1,060-byte records flat out, the writer catches up with the helper (about 2.7 GB/s) and some
  stalls remain. At realistic rates the helper stays ahead.
- **btrfs, `fallocate`d segment**: about 9–18 GB/s with no prefault. `MADV_POPULATE_WRITE` (Linux 5.14+) on the range
  takes about 3 ms per 64 MiB and leaves no page faults for the writer.

ADR 0014 measured two *synchronous* variants: `MADV_WILLNEED` over a whole new segment, and touching every page at
roll time. Both stalled the writer. It rejected a background thread because that thread would write into the
shared mapping, which is new `unsafe` concurrency. A thread that only calls `madvise` never reads or writes the
mapped bytes, so that objection doesn't apply.

## Options considered

1. **Background thread calling `madvise` ahead of the write position** (chosen). The writer's hot path gains one
   comparison per `append` and, once per MiB, a mutex and a condition-variable notify.
2. **Windowed `madvise` in the writer thread.** Same throughput gain, but each 4 MiB call blocks the writer for
   about 1.7 ms on APFS: a worse tail than today.
3. **Sparse segments on macOS.** Fastest, but it loses C10 on APFS.
4. **Pre-zeroing segments with `pwrite` at roll.** 128 MiB of extra I/O per roll, on the writer's path.

## Decision

- `WriterConfig::prefault(bool)`, default **true**. When it is on, `Writer::open` starts one thread,
  `tickertape-prefault`. If the thread can't be started, the writer runs without it.
- The thread keeps the active segment advised up to `PREFAULT_WINDOW` (8 MiB) past the write position. It advises in
  1 MiB calls, so it notices a roll or shutdown quickly. The writer wakes it when the write position has advanced
  `PREFAULT_STEP` (1 MiB) since the last wake-up, and on every roll (for the new segment).
- The call is `MADV_WILLNEED` on macOS and `MADV_POPULATE_WRITE` on Linux. On Linux, if the kernel rejects it
  (`EINVAL`, before 5.14), the thread stops for the rest of the writer's life. Errors are otherwise ignored: the
  call is only a hint, and the writer's own page fault is the fallback, exactly as without prefaulting.
- **Lifetime.** A segment's mapping becomes a reference-counted region. The helper holds a counted handle whose only
  operation is `advise(offset, len)`, so the region is unmapped only after both the writer and the helper are done
  with it. Dropping the `Writer` stops and joins the thread.
- `madvise` neither reads nor writes the mapped bytes from the program's point of view, so it doesn't conflict with
  the writer's `&mut` access to the same pages. The call is in `mmap.rs` with a `// SAFETY:` comment, and the module
  goes through cross-provider review.

## Consequences

- One more thread per writer, with its own on/off switch. Users who pin threads, or who need a writer with no
  helper threads, set `prefault(false)`.
- On Linux, `MADV_POPULATE_WRITE` marks up to 8 MiB of not-yet-written pages ahead of the writer dirty. The kernel
  writes those zero pages back, which is a little extra I/O. Their contents don't change (zeros, as the format
  requires), and a checkpoint syncs only the written range, as before.
- On a full btrfs volume, the helper's call fails (`EFAULT`) and the writer's own fault behaves as described in C10.
  This change doesn't make that case better or worse.
- Crash safety is unchanged: a killed process loses its threads, and the helper never writes to the mapping. The
  kill loop and every writer test run with prefaulting on, because it is the default.
- Benchmarks must be re-recorded (`append`, `tt-latency` at 1 KiB and 100,000 records/s), with `prefault(false)` as
  the comparison.
