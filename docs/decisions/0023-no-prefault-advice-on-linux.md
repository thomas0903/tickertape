# 0023: No prefault advice on Linux

- **Status:** accepted (amends [0020](0020-background-prefault.md))
- **Date:** 2026-11-01

## Context

ADR 0020 added a helper thread that keeps the pages ahead of the writer faulted in: `MADV_WILLNEED` on macOS,
`MADV_POPULATE_WRITE` on Linux. It was measured on APFS and in the OrbStack VM (btrfs). The first recorded runs on a
real Linux machine showed very large tails at 1,024-byte records and 100,000 records/s:
- the machine was an AWS `c6id.xlarge` (x86-64 Ice Lake, 2 cores / 4 threads, a cloud VM, not bare metal);
- the disk was ext4 on local NVMe.

An experiment on a fresh instance of the same type isolated the cause. Setup:
- a build with temporary switches, never merged;
- `tt-latency` at 100,000 records/s with the zero-copy reader;
- each journal deleted before the next run;
- two runs per cell, values in µs.

| Configuration | 1,024 B p99 | 1,024 B p99.9 | 64 B p99.9 |
|---|---|---|---|
| prefault off | 29, 29 | 77, 68 | 29, 27 |
| `POPULATE_WRITE` helper + spare segment (ADR 0022) | 115,999; 101,057 | 200,671; 184,812 | 12, 9 |
| `POPULATE_WRITE` helper only | 5; 42,041 | 159; 126,288 | 13, 11 |
| `WILLNEED` helper + spare segment | 7,188; 16,327 | 93,061; 80,347 | 24, 25 |
| `WILLNEED` helper only | 24, 25 | 643, 560 | 27, 24 |
| spare segment only, no advice | 29, 29 | 64, 65 | 34, 37 |

These numbers support decisions in this ADR only. Published numbers are in `docs/benchmarks.md`.

The likely mechanism is page write-back:
- `POPULATE_WRITE` marks pages dirty before the writer reaches them.
- At about 100 MB/s, the kernel's background write-back is running continuously and writes those pages out early.
- A writer store into a page that is under write-back then waits for the I/O.
- On ext4, each early write-back of a preallocated (unwritten) extent also needs a journal commit.

`WILLNEED` dirties nothing, but combined with the spare's own prefault and preallocation it still caused tens of
milliseconds of stalls. The spare segment on its own is harmless and slightly improves the worst cases at 1,024 B
(p99.99 854 and 871 µs, against 1,129 and 982 µs with everything off).

## Decision

- **Linux and other non-Apple systems:** no `madvise` prefaulting at all. `WriterConfig::prefault(true)` starts no
  `tickertape-prefault` thread there, and the spare segment is prepared without advice.
- **macOS:** unchanged. A `MADV_WILLNEED` helper plus the spare segment, as measured in ADRs 0020 and 0022.
- `benches/run-recorded.sh` deletes each run's journal and calls `sync` before the next run. In the first recorded
  Linux run, the write-back of gigabytes written by earlier runs distorted the later ones: the prefault-off
  configuration showed 8–62 ms p99 there, against 29 µs in the experiment.

## Consequences

- On Linux, 64-byte records lose part of the gain `POPULATE_WRITE` gave: p99.9 is about 27–37 µs instead of 9–12 µs
  in this experiment. In exchange there are no 100-millisecond stalls at higher byte rates. A safer way to prefault
  on Linux would be very small windows, or prefaulting only below some byte rate. That needs measurement on more
  hardware (bare metal, xfs) first.
- The Linux `MADV_POPULATE_WRITE` code is removed. `mmap::Advice::prefault` is a no-op off macOS.
- Linux benchmarks must be re-recorded with these settings.
