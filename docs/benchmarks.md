# Benchmarks

Measured results only, recorded following [`playbooks/run-benchmark.md`](playbooks/run-benchmark.md). Every number
below comes from the raw output of the commands listed with it. There are **no comparisons with other tools**, and
these numbers are not guarantees: they describe two specific machines on one day. What each benchmark measures, and
what it doesn't, is in [`benches/README.md`](../benches/README.md); the methodology is ADR
[0009](decisions/0009-benchmark-methodology.md).

## 2026-10-31

### Environments

| | Owner's Mac | OrbStack VM (Linux on macOS) |
|---|---|---|
| Hardware | MacBook Pro, Apple M4 Pro, 14 cores, 24 GB RAM, internal SSD | Same Mac. Virtualised: **not bare-metal Linux** |
| OS / kernel | macOS 27.0.1 (Darwin 27.0.0) | Ubuntu 26.04.1 LTS, Linux 7.2.12-orbstack, aarch64, 14 vCPUs (`lscpu`), 11 GB RAM |
| Journal filesystem | APFS (`$TMPDIR` on the Data volume) | btrfs (VM disk, `$HOME/tmp`) |
| Commit | `ff24fd1` | `8989c51` (differs from `ff24fd1` only in the run script) |
| Rust | 1.99.1, release profile | 1.99.1, release profile |
| Power, load | On AC power; normal desktop session, no other benchmark running | VM idle apart from the benchmark; host as on the left |

Command (both machines, from the repository root): `benches/run-recorded.sh <outdir>`, which runs `tt-latency`,
`tt-checkpoint` and the Criterion benches with the parameters shown below, twice (Criterion: two separate
invocations). No core pinning.

### Writer-to-reader visible latency (cross-process)

`tt-latency run --rate 100000 --count 1100000 --warmup 100000 --payload <64|1024> --reader <mapped|safe>`, 128 MiB
segments. One writer process sends a record every 10 µs at a scheduled time and stores that **intended** time in
the record; a separate reader process (busy-polling) records `now - intended` for the last 1,000,000 records. A writer
that falls behind keeps sending without skipping, so its backlog counts as latency (coordinated omission handled at
the source). "Late sends" counts records sent more than 1 µs after their intended time, out of 1,100,000. Both
processes use the host's wall clock.

Values in µs. On macOS the wall clock has 1 µs resolution, so "<1" means below the clock's resolution.

**Owner's Mac (APFS)**

| Run | Payload | Reader | p50 | p90 | p99 | p99.9 | p99.99 | max | Late sends |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 64 B | zero-copy | <1 | <1 | 33 | 234 | 276 | 331 | 13,897 |
| 1 | 64 B | safe | 1.0 | 2.0 | 27 | 178 | 905 | 1,215 | 2,012 |
| 1 | 1,024 B | zero-copy | <1 | 64 | 13,689 | 18,629 | 21,430 | 22,282 | 156,603 |
| 1 | 1,024 B | safe | 2.0 | 111 | 28,213 | 42,828 | 45,646 | 46,432 | 79,883 |
| 2 | 64 B | zero-copy | <1 | <1 | 127 | 1,011 | 2,376 | 3,119 | 19,547 |
| 2 | 64 B | safe | 1.0 | 2.0 | 42 | 332 | 1,427 | 1,909 | 1,421 |
| 2 | 1,024 B | zero-copy | <1 | 100 | 15,286 | 20,709 | 22,970 | 23,462 | 170,161 |
| 2 | 1,024 B | safe | 2.0 | 86 | 22,233 | 25,362 | 28,262 | 29,213 | 67,805 |

**OrbStack VM (Linux on macOS, btrfs)**

| Run | Payload | Reader | p50 | p90 | p99 | p99.9 | p99.99 | max | Late sends |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 64 B | zero-copy | 0.15 | 0.20 | 0.45 | 326 | 831 | 1,519 | 4,200 |
| 1 | 64 B | safe | 1.5 | 1.7 | 2.9 | 2,572 | 11,510 | 12,501 | 3,684 |
| 1 | 1,024 B | zero-copy | 0.40 | 0.45 | 380 | 6,881 | 8,356 | 8,765 | 21,072 |
| 1 | 1,024 B | safe | 1.8 | 2.3 | 65,765 | 151,257 | 159,908 | 160,825 | 33,108 |
| 2 | 64 B | zero-copy | 0.15 | 0.20 | 0.29 | 328 | 803 | 1,357 | 3,517 |
| 2 | 64 B | safe | 1.5 | 1.7 | 2.5 | 17 | 507 | 1,013 | 2,134 |
| 2 | 1,024 B | zero-copy | 0.40 | 0.46 | 62,980 | 149,422 | 158,073 | 158,990 | 42,837 |
| 2 | 1,024 B | safe | 1.8 | 2.4 | 301 | 6,476 | 9,159 | 10,084 | 8,257 |

Observations, from the tables only:
- Medians are at or below a few µs in every configuration. Tails vary a lot **between runs of the same
  configuration** (for example, VM 1,024 B safe reader p99: 65,765 µs in run 1, 301 µs in run 2), far beyond the 10 %
  the playbook asks us to flag. Treat the tail columns as "can reach this", not as typical.
- With 1,024 B payloads at 100,000 records/s (about 100 MB/s into fresh pages), the writer itself fell behind its
  schedule in every run (up to 170,161 late sends), and that backlog dominates the tail. The writer appends into
  never-before-touched pages of preallocated segments, so page-fault cost is part of every append.

### `checkpoint()` and roll latency

`tt-checkpoint --payload <64|1024> --batch 1000 --checkpoints 200 --segment-size 16777216`: append 1,000 records,
time one `checkpoint()`, 200 times; then one checkpoint with nothing new. Every append is timed; "roll" appends are
the ones that created a new segment. Values in µs; records/s includes the checkpoints.

| Machine | Run | Payload | Checkpoint p50 | p90 | p99 | max | Empty checkpoint | Roll appends (n, p50, max) | Other appends p50 / p99 / p99.9 / max | Records/s |
|---|---|---|---|---|---|---|---|---|---|---|
| Mac | 1 | 64 B | 7,696 | 8,352 | 8,634 | 10,330 | 4,096 | 1, 4,280, 4,280 | 0.04 / 0.17 / 191 / 2,986 | 120,092 |
| Mac | 1 | 1,024 B | 8,389 | 9,388 | 10,854 | 14,164 | 3,910 | 12, 4,321, 6,033 | 0.17 / 40 / 253 / 3,961 | 89,807 |
| Mac | 2 | 64 B | 7,778 | 8,405 | 8,864 | 9,495 | 4,004 | 1, 3,844, 3,844 | 0.04 / 0.17 / 185 / 2,402 | 119,409 |
| Mac | 2 | 1,024 B | 8,081 | 8,569 | 12,419 | 13,099 | 4,112 | 12, 4,207, 4,624 | 0.17 / 41 / 252 / 3,234 | 92,618 |
| VM | 1 | 64 B | 1,316 | 2,388 | 5,014 | 6,218 | 431 | 1, 657, 657 | 0.04 / 0.92 / 2.1 / 617 | 614,244 |
| VM | 1 | 1,024 B | 4,008 | 5,689 | 9,404 | 10,142 | 1,750 | 12, 1,950, 2,761 | 0.17 / 3.9 / 7.3 / 1,267 | 193,707 |
| VM | 2 | 64 B | 3,965 | 6,844 | 8,757 | 11,141 | 1,017 | 1, 2,092, 2,092 | 0.04 / 1.5 / 3.8 / 1,786 | 229,053 |
| VM | 2 | 1,024 B | 4,448 | 5,911 | 10,666 | 11,256 | 1,730 | 12, 1,827, 2,382 | 0.17 / 5.1 / 10 / 1,623 | 179,370 |

On macOS every checkpoint issues `F_FULLFSYNC` twice (after the data, then after the marker); an empty checkpoint
still took about 4 ms there, which is consistent with that cost, though it was not profiled. Inside the VM, `fdatasync` reaches a virtual disk; what the host does with
it was not measured, so the VM numbers say nothing about durability on real Linux hardware.

### Append and replay throughput (Criterion)

`cargo bench -p tickertape-bench --bench append` and `--bench replay`, two separate invocations per machine. Append
writes batches of 16,384 records into a fresh journal (first-touch pages, the steady state of an append-only
journal), excluding open and teardown. Replay reads a journal of 1,000,000 records (64 B) or 100,000 records (1,024 B)
with a warm page cache, including reader open and full validation. Bytes count payload only. Values are Criterion's
point estimates (middle of its interval), in millions of records per second.

| Benchmark | Mac run 1 | Mac run 2 | VM run 1 | VM run 2 |
|---|---|---|---|---|
| append, 64 B | 3.52 | 5.64 | 9.52 | 10.95 |
| append, 1,024 B | 0.72 | 0.79 | 4.60 | 4.90 |
| replay, 64 B, safe reader | 24.9 | 25.0 | 28.9 | 29.0 |
| replay, 64 B, zero-copy reader | 22.1 | 22.4 | 27.6 | 18.4 |
| replay, 1,024 B, safe reader | 6.19 | 6.74 | 6.34 | 5.94 |
| replay, 1,024 B, zero-copy reader | 5.73 | 5.96 | 5.43 | 2.57 |

In payload bytes, run 1: Mac append 286 MiB/s (64 B) and 659 MiB/s (1,024 B); VM append 647 MiB/s and 4.69 GiB/s;
Mac replay 1.46 GiB/s (64 B, safe) and 6.16 GiB/s (1,024 B, safe); VM replay 1.74 GiB/s and 5.84 GiB/s.

Observations:
- Mac append 64 B (+60 %) and VM zero-copy replay (-33 % and -53 %) differ by more than 10 % between runs; the others
  are within about 10 %.
- The zero-copy reader is not faster than the safe reader in these runs. Since the M4 review, it validates every
  record from an atomic copy before handing out a slice into the mapping, so it currently does more work per record
  than the safe reader's chunked `pread`. Its advantage is not copying large payloads into user buffers, which this
  benchmark doesn't measure.
- Appending on the Mac was slower than in the VM on the same hardware. The cause was not investigated; first-touch
  page faults on APFS versus the VM's btrfs are one candidate.

### Cold page cache replay

`tt-latency run --cold --rate 1000000 --count 1000000 --payload <64|1024> --reader safe`: the writer pre-writes and
checkpoints the journal, the tool drops the page cache, then a fresh reader process replays it.

- **Mac:** not measured. Dropping the page cache (`sudo purge`) needs root, which the benchmark doesn't use.
- **OrbStack VM:** drops the *guest's* page cache (`echo 3 > /proc/sys/vm/drop_caches`). The host may still cache
  the VM's disk, so this is not a cold-storage measurement.

| Run | Payload | Records/s | Payload MB/s |
|---|---|---|---|
| 1 | 64 B | 15,839,219 | 1,014 |
| 1 | 1,024 B | 2,973,966 | 3,045 |
| 2 | 64 B | 16,707,495 | 1,069 |
| 2 | 1,024 B | 3,185,582 | 3,262 |

### Not measured

Bare-metal Linux; Intel/AMD x86-64; ext4 or xfs on real disks; core pinning or isolated CPUs; checkpoint durability
on real Linux hardware; power-loss behaviour. CI runners only run smoke versions of these tools, and their numbers
are never recorded.
