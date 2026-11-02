# 0025: Prefault on Linux again, with a small window

- **Status:** accepted (supersedes the prefault-advice decision of [0023](0023-no-prefault-advice-on-linux.md); the
  spare-segment decision there stands)
- **Date:** 2026-11-02

## Context

ADR 0023 turned off Linux prefaulting (`MADV_POPULATE_WRITE` 8 MiB ahead of the writer). The reason was stalls of
100–200 ms on a 4-vCPU AWS `c6id.xlarge`. Later experiments showed that measurement was confounded:
- The stalling configuration combined prefaulting with the spare segment of ADR 0022.
- It ran on a freshly formatted ext4 volume whose lazy inode-table initialisation was still running.

ADR 0023's own amendment then ruled out both factors.

Three new experiments used experiment-only builds with environment switches (never merged). Setup:
- `tt-latency` with the zero-copy reader at 100,000 records/s;
- every journal deleted and `sync`ed before the next run;
- ext4 formatted without lazy initialisation;
- configurations interleaved;
- values in µs, one value per run.

These numbers support this decision only; published numbers are in `docs/benchmarks.md`.

**Bare metal (`z1d.metal`), 11-second runs, bound to NUMA node 0:**

| Configuration | 64 B p99.9 | 1,024 B p99 | 1,024 B p99.9 |
|---|---|---|---|
| off | 30, 31 | 32, 32, 32 | 85, 125, 56 |
| `POPULATE_WRITE`, 256 KiB window | 2, 2 | 1.8, 1.9, 1.9 | 27, 73, 40 |
| `POPULATE_WRITE`, 8 MiB window | 1, 2 | 2.6, 1.4, 1.4 | 30, 41, 19 |
| `MADV_WILLNEED`, 8 MiB window | 27, 27 | 32, 32, 32 | 1,096, 1,117, 1,179 |

**Bare metal, steady state.** Runs of 5 minutes at 1,024 B and 3 minutes at 64 B, with about 3 GB of dirty pages
under continuous write-back:

| Configuration | 1,024 B p99 | 1,024 B p99.9 | 1,024 B max | 64 B p99.9 | 64 B p99.99 |
|---|---|---|---|---|---|
| off | 37.7, 37.6 | 751, 521 | 16,597, 14,230 | 34 | 321 |
| `POPULATE_WRITE`, 256 KiB | 22.4, 21.9 | 547, 649 | 18,235, 14,049 | 17 | 35 |
| `POPULATE_WRITE`, 8 MiB | 22.1, 21.0 | 536, 691 | 13,607, 15,008 | 19 | 36 |

**Small cloud VM (`c6id.xlarge`, 4 vCPUs, 8 GiB), 1,024 B:**

| Configuration | 11 s p99 | 11 s p99.9 | 5 min p99 | 5 min p99.9 |
|---|---|---|---|---|
| off | 28, 28, 28 | 255, 61, 64 | 86,835; 97,321 | 293,601; 484,704 |
| `POPULATE_WRITE`, 256 KiB | 10, 12, 7 | 17, 19, 16 | 82,969; 94,568 | 169,214; 423,887 |
| `POPULATE_WRITE`, 8 MiB | 8, 6, 8 | 136, 38, 177 | not run | not run |

## What the experiments show

- `POPULATE_WRITE` with a 256 KiB window was never worse than prefaulting off, on either machine, in short or
  steady-state runs. It roughly halved late sends, and it improved p99 (and at 64 B every percentile measured) by a
  large factor.
- The rarest tails in steady state (p99.99 and max, milliseconds) come from ext4 write-back itself and are the same
  with or without prefaulting.
- The small VM cannot sustain about 106 MB/s for minutes: every configuration stalls there for hundreds of
  milliseconds. That is a disk limit, not a prefault effect.
- `MADV_WILLNEED` brings nothing on Linux, and the 8 MiB window has worse 1,024 B tails on the small VM than
  256 KiB.

## Decision

- With `WriterConfig::prefault(true)` (the default), Linux runs the `tickertape-prefault` helper thread. It calls
  `MADV_POPULATE_WRITE` 256 KiB ahead of the writer, waking every 64 KiB of progress and advising at most 64 KiB per
  call.
- macOS keeps `MADV_WILLNEED` 8 MiB ahead (ADR 0020).
- On kernels before Linux 5.14 the advice fails with `EINVAL`, and the helper stops for the rest of the writer's
  life, as before ADR 0023.
- Linux still prepares no spare segment (ADR 0023 amendment).

## Consequences

- The thread is back on Linux by default, and `prefault(false)` turns it off again.
- Linux benchmarks must be re-recorded.
- The cross-provider review required for `mmap.rs` applies.
- The steady-state stalls of ext4 at high byte rates are a separate, open problem. A steady-state workload for the
  comparison suite would show how other tools behave there.
