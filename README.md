# tickertape

**A Rust library and CLI for a local, append-only event journal on memory-mapped files**: one writer process
records events, other processes on the same machine follow them live, and everything can be replayed later or
exported to Parquet for DuckDB and Python.

> Status: [v1.1.0](https://crates.io/crates/tickertape/1.1.0), stable. Linux and macOS. The on-disk format is version 1. What 1.x keeps compatible: [`docs/compatibility.md`](docs/compatibility.md).

Think of it as a flight recorder for a trading system or any other latency-sensitive program:

- One **writer** appends events: opaque payload bytes, a gap-free sequence number, a schema id and two timestamps
  you supply (for example exchange time and local receive time).
- Any number of **readers**, in other processes, follow the journal live or **replay** it from any sequence number.
  Readers take no lock and write nothing the writer waits for. A reader can spin, poll or block. A blocking reader
  sleeps in the kernel and the writer wakes it (futex on Linux, `os_sync_wait_on_address` on macOS 14.4+; elsewhere it
  polls): the writer makes one wake-up system call when it publishes (an append, or a whole batch) while a reader is
  registered to wait, and several waiters share the call. Measured after 1.0.0 on an x86-64 cloud VM with one blocking
  safe reader at 20,000 records/s of 64 B, the writer's `append` p50 was about 1.5 µs (1.5 and 1.52 in the two builds
  measured) against 0.117 µs beside a spinning reader
  ([`docs/benchmarks.md` → readers that block](docs/benchmarks.md#readers-that-block-the-rule-added-after-review-x86-64)).
  A spinning or polling reader costs the writer no system call.
- Data lives in fixed-size **segment files** that roll over. Segments are never deleted automatically unless
  you configure a retention policy; `tickertape archive` can move or delete old, checkpoint-covered segments.
- The **`tickertape` CLI** tails, inspects, verifies and summarizes journals (`stats`), and exports them to
  JSONL or Parquet. Its `archive` command is the only command that changes a journal.
- **Operational metrics** without a telemetry service: `tickertape status` (text, JSON or Prometheus text format, and
  a health-check exit code) and `tickertape::status` report checkpoint age, what a power loss would remove now,
  storage headroom, and the lag of readers that opt in.

![tickertape architecture: a writer process appends to segment files and a checkpoint marker in one journal directory; safe and zero-copy reader processes and the CLI read them; the CLI exports Parquet or JSONL for DuckDB and Python](docs/figures/architecture.svg)

## Why

There are good building blocks for this in Rust (shared-memory channels, in-memory rings, write-ahead logs), but
few small, permissively licensed libraries that combine a single local writer, independent cross-process live
readers, retained and replayable files, rolling, and easy inspection. tickertape aims to be that, with:

- a **simple, versioned byte format** ([`docs/format.md`](docs/format.md)) that you can decode from any language;
- an **explicit, tested crash contract** ([`docs/crash-contract.md`](docs/crash-contract.md)) that says exactly what
  survives a process crash or a power loss on each platform, and which test checks it;
- a **transparent CLI** and **Parquet export**, so incident analysis happens in DuckDB, pandas or Polars.

## Performance

tickertape was measured against Chronicle Queue, Aeron IPC, yaque, queue-file and a plain file with the comparison suite
in [`compare/`](compare/README.md), in one 24-hour session per machine on two **Google Cloud virtual machines, not bare
metal** (2026-11-25 and 26): x86-64 `c4-standard-16-lssd` (Xeon 6985P-C) and aarch64 `c4a-standard-8-lssd`
(Neoverse-V2), Ubuntu 24.04, ext4 and xfs on local NVMe SSDs. The session commit is `fb2d91b0`, whose library is
identical to the **v1.0.0** release; changes merged since are not in these figures. Every table (with the xfs, unpinned
and tuned rows), the failed cell and the caveats are in [`docs/benchmarks.md` → comparison rerun at
v1.0.0](docs/benchmarks.md#2026-11-25-to-2026-11-26-comparison-rerun-at-v100-google-cloud-vms-linux-x86-64-and-aarch64).
Ranges are min–max of the runs; values are rounded half up to three significant digits (two decimals for million
records/s). The figures describe these machines in that session; they are not guarantees. The older bare-metal figures
(v0.4.0 and v0.3.0, measured with the earlier harness) are in
[`docs/benchmarks.md`](docs/benchmarks.md#2026-11-02-v040-on-bare-metal-linux-aws-z1dmetal), superseded by this rerun.

**The result is mixed.** tickertape led the 64 B append bursts on both machines and file systems; Chronicle Queue led
warm replay in three of four pinned ext4 cells and had the lower reader latency at 10,000 records/s; Aeron IPC had the
lowest observed latency at 100,000 records/s; and the ten-minute ranking changed with the file system.

**Configuration.** Each measured writer and reader thread was pinned to a CPU of its own, and every other thread of the
tools to the remaining CPUs; every reader busy-polls. Chronicle Queue 2026.8 (open source) and Aeron 1.54.0 ran on
OpenJDK 21 with a 256 MiB heap and ParallelGC, asynchronous JVM logging on, and a warm-up through their measured loops
in every process (3 × 1,200,000 and 8 × 20,000 records). Chronicle used a 1 GiB `blockSize` and `microTouch()`, which
touches the next mapped data page while a paced writer waits, so W1 and the unpaced probe get nothing from it (its
pretoucher is Enterprise-only). Aeron IPC used non-sparse, pre-touched term buffers and dedicated busy-spinning driver
threads, a larger CPU budget than the file-based tools'. tickertape ran with its defaults: on Linux, prefaulting 256 KiB
ahead and early write-back (a helper thread starts the kernel's write-back of filled pages with `sync_file_range` as the
writer moves on). It has two readers: the zero-copy `Reader::open_mapped` (`unsafe`) and the safe `Reader::open`
(`pread`). [`compare/README.md`](compare/README.md) says what each setting does and why. The tools promise different
things: Aeron IPC writes no durable journal and this adapter has no Archive replay (its term buffers were memory-mapped
files on the file system under test, so the kernel wrote them back to that disk); yaque is a consuming single-reader
queue; queue-file's upstream API forbids concurrent process access; and the plain-file adapter has no checksum or
crash-recovery protocol.

**The earlier comparison handicapped Chronicle Queue**, as a reader who knows it pointed out: a short warm-up, the
default 64 MiB `blockSize`, no `microTouch()`, no pinning, and, in the ten-minute runs, tools measured in different
sessions. The rerun also ran Chronicle with those old settings (pinned, in the new harness). On x86-64 its reader's
p99.9 at 1,024 B × 10,000 records/s was 34.8–36.8 µs with them, 2.35–2.51 µs with only `microTouch()` added, and
2.36–2.82 µs with the new settings in three of four runs (15.75 ms in the fourth); it appended and replayed faster with
the new settings in every cell. In five reader-latency cells the order against tickertape's zero-copy reader changed:
tickertape was lower than Chronicle's old-settings p99.9 in all five and is higher than the new one in all five (at
x86-64 1,024 B × 10,000 records/s in three of Chronicle's four runs). One of them: x86-64, 64 B × 10,000 records/s,
tickertape 2.17–2.35 µs, Chronicle 2.45–3.51 µs old and 1.68–1.83 µs new.

### Short runs (ext4, pinned)

n = 4 per cell: two rounds of two runs, the rounds 43 to 192 minutes apart depending on the tool.

**Throughput**, million records/s, higher is better. W1 appends 1,000,000 records of 64 B or 100,000 of 1,024 B as fast
as possible: cold from an empty directory, with the open inside the timer; steady into files opened before the timer and
primed with 1,000 records. Both timers stop before the final flush, so W1 is visible, not durable, append throughput,
and a 64 B burst lasts 32–52 ms; on x86-64 the first repeat of every W1-cold round ran at 29.12–30.84 million records/s
(21.99 in the unpinned one) and the second at 17.53–20.07 (cause not traced), which makes tickertape's range
19.11–30.84. W3 replays the records from a warm page cache in a new process.

| | x86-64, 64 B | x86-64, 1,024 B | aarch64, 64 B | aarch64, 1,024 B |
|---|---|---|---|---|
| W1 cold: tickertape | 19.11–30.84 | 2.61–3.08 | 10.46–10.56 | 4.02–4.20 |
| W1 cold: Chronicle Queue | 8.47–8.53 | 2.82–2.87 | 5.49–5.53 | 2.76–2.83 |
| W1 steady: tickertape | 19.44–24.93 | 2.92–3.06 | 10.21–10.55 | 3.77–4.02 |
| W1 steady: Chronicle Queue | 8.54–8.69 | 2.93–3.04 | 5.57–5.60 | 2.97–3.07 |
| W3: tickertape, zero-copy | 20.85–26.41 | 9.11–9.47 | 27.22–31.59 | 10.05–10.36 |
| W3: tickertape, safe | 31.01–32.77 | 6.40–6.57 | 28.12–28.48 | 6.26–6.53 |
| W3: Chronicle Queue | 35.57–36.77 | 10.28–10.59 | 28.17–29.30 | 15.31–17.12 |
| W3: yaque (a scan of its queue) | 28.89–29.61 | 7.38–7.44 | 20.22–22.04 | 3.94–4.41 |

yaque, queue-file and the plain file appended more slowly than both journals in every W1 cell.

**Reader latency, p99.9 in µs**, lower is better: the time from each record's scheduled send to the reader's validated
read, over 1,000,000 records measured after 100,000 unmeasured ones. A send that starts late (more than 1 µs after its
scheduled time) still runs, and its delay counts in the latency.

| | tickertape, zero-copy | tickertape, safe | Chronicle Queue | Aeron IPC | plain file |
|---|---|---|---|---|---|
| x86-64, 64 B, 10,000/s | 2.17–2.35 | 4.30–4.49 | 1.68–1.83 | 2.56–5.65 | 5.62–6.08 |
| x86-64, 64 B, 100,000/s | 2.27–5.96 | 3.83–8.35 | 11.8–15.3 | 0.39–3.47 | 5.10–7.08 |
| x86-64, 1,024 B, 10,000/s | 4.60–6.84 | 6.94–7.64 | 2.36 µs to 15.75 ms | 3.26–3.64 | 5.89–6.02 |
| x86-64, 1,024 B, 100,000/s | 28.6–29.8 | 29.0–30.2 | 23.9–24.6 | 0.77–5.58 | 9.85–11.3 |
| aarch64, 64 B, 10,000/s | 2.87–3.46 | 6.00–6.45 | 0.73–0.78 | 5.56–7.34 | 9.04–9.41 |
| aarch64, 64 B, 100,000/s | 1.88–2.08 | 4.95–6.07 | 7.85–13.3 | 0.62–3.03 | 7.94–8.41 |
| aarch64, 1,024 B, 10,000/s | 8.73–9.10 | 11.6–12.4 | 2.77–3.46 | 4.57–5.96 | 8.39–8.79 |
| aarch64, 1,024 B, 100,000/s | 24.8–28.5 | 24.3–31.1 | 14.5–16.3 | 0.82–2.71 | 14.4–14.7 |

yaque's p99.9 was 5.76 µs to 18.30 ms in these cells; p50, p99, maxima and late sends are in `docs/benchmarks.md`.

### Ten-minute runs (ext4, pinned)

n = 1 per round; each cell shows round 1; round 2. The **probe** is unpaced, with a reader following: it is the only row
in which every tool answers the same question, and at 1,024 B it measures the disk rather than the tool (below). **SV**
(sustained visibility: paced, no syncs) and **SD** (sustained durability: the same with the tool's sync every second, on
the writer's thread) are paced at **half of each tool's own probe in the same round**, so each tool runs at its own
target rate, printed in the table: read them as "at half of its own maximum", not as the same load. tickertape ran its
zero-copy reader and, in SD, the blocking `Writer::checkpoint()`. Aeron IPC has no sync, so no SD row.

| | x86-64, 64 B | x86-64, 1,024 B | aarch64, 64 B | aarch64, 1,024 B |
|---|---|---|---|---|
| Probe, records/s: tickertape | 3,543,632; 3,529,856 | 346,382; 345,534 | 3,458,048; 3,466,622 | 338,179; 337,424 |
| Probe: Chronicle Queue | 3,061,325; 2,952,843 | 350,374; 349,231 | 2,570,572; 2,575,258 | 343,736; 341,297 |
| Probe: Aeron IPC | 7,206,880; 6,489,247 | 4,175,027; 4,036,635 | 3,246,052; 3,820,950 | 2,776,684; 2,796,872 |
| SV and SD target, records/s: tickertape | 1,771,815; 1,764,927 | 173,190; 172,766 | 1,729,024; 1,733,310 | 169,089; 168,712 |
| Target: Chronicle Queue | 1,530,662; 1,476,421 | 175,186; 174,615 | 1,285,285; 1,287,628 | 171,867; 170,648 |
| SV target: Aeron IPC | 3,603,440; 3,244,623 | 2,087,513; 2,018,317 | 1,623,026; 1,910,474 | 1,388,341; 1,398,435 |
| SV reader p99.9 (max): tickertape | 642 µs (1.69 ms); 654 µs (1.56 ms) | 504 µs (1.40 ms); 508 µs (1.38 ms) | 501 µs (1.47 ms); 697 µs (23.81 ms) | 323 µs (1.14 ms); 401 µs (7.83 ms) |
| SV: Chronicle Queue | 102.69 ms (135.53 ms); 100.01 ms (136.05 ms) | 100.27 ms (135.40 ms); 98.44 ms (139.59 ms) | 91.82 ms (134.09 ms); 89.98 ms (133.37 ms) | 91.42 ms (162.92 ms); 99.48 ms (135.53 ms) |
| SV: Aeron IPC | 36.24 ms (78.38 ms); 34.96 ms (80.41 ms) | 107.41 ms (125.96 ms); 99.16 ms (119.14 ms) | 26.02 ms (167.64 ms); 14.72 ms (78.77 ms) | 84.28 ms (166.20 ms); 80.61 ms (159.65 ms) |
| SD, tickertape checkpoint, median (max) | 2.95 ms (4.21 ms); 2.34 ms (3.80 ms) | 2.99 ms (4.11 ms); 2.29 ms (3.81 ms) | 1.38 ms (2.36 ms); 1.49 ms (41.94 ms) | 1.39 ms (2.39 ms); 1.40 ms (76.55 ms) |
| SD, Chronicle Queue synchronous store flush, median (max) | 361.50 ms (404.49 ms); 348.65 ms (413.66 ms) | 516.16 ms (592.97 ms); 514.33 ms (527.96 ms) | 302.78 ms (325.84 ms); 303.82 ms (540.02 ms) | 506.99 ms (845.15 ms); 502.79 ms (514.33 ms) |
| SD reader p99.9: tickertape | 2.39 ms; 1.83 ms | 2.11 ms; 1.55 ms | 989 µs; 1.80 ms | 837 µs; 1.60 ms |
| SD reader p99.9: Chronicle Queue | 369.62 ms; 352.06 ms | 515.64 ms; 513.80 ms | 302.78 ms; 381.68 ms | 621.28 ms; 502.27 ms |

**Durability is not the same operation in every tool's SD row.** A tickertape checkpoint on Linux `msync`s the mapped
ranges it still holds, `fdatasync`s the segment files and `fsync`s new directory entries, then writes the checkpoint
marker and `fdatasync`s it; it gives readers a durable position. Chronicle Queue's is `ExcerptAppender.sync` with
`SyncMode.SYNC`, a synchronous `msync(MS_SYNC)` of its store. Neither was tested for power loss here. The two are not
the same work: tickertape's early write-back starts writing each MiB as soon as the writer has passed it, so when the
checkpoint starts most of the second's data is already on its way to the device, while Chronicle's `msync` appears to
write it then, on the writer's thread (its `bgMicroTouch()` was not used; consistent with one second of its 1,024 B
records, at least 182 MB of payload and envelope, taking about 0.52 s at the disk's 351 MB/s; not traced). With its old
settings, at nearly the same rate (175,124 against 175,186 records/s on x86-64), the flush took 97.98–119.87 ms, which
this does not explain. A tickertape checkpoint without early write-back became durable 0.93 s after its request at about
90 % of the device's rate, in later tickertape-only runs (the disk section cited below). The local SSDs of these machine
types reported a write-through cache in other sessions (this one did not record it); if that held here, no sync paid for
a drive-cache flush.

### Where tickertape is ahead, and where it is behind

**Ahead in these runs:**

- **Appending 64 B records**, on both machines and both file systems (W1 cold: 19.11–30.84 against Chronicle Queue's
  8.47–8.53 million records/s on x86-64, 10.46–10.56 against 5.49–5.53 on aarch64), and 1,024 B records on aarch64 ext4
  (4.02–4.20 against 2.76–2.83).
- **Reader p99.9 at 64 B × 100,000 records/s** (pinned, ext4, zero-copy reader) against Chronicle Queue: 2.27–5.96
  against 11.8–15.3 µs on x86-64, 1.88–2.08 against 7.85–13.3 µs on aarch64. Aeron's range overlaps tickertape's there
  (0.39–3.47 and 0.62–3.03 µs). On x86-64 xfs (n = 2) the ranges overlap Chronicle's (2.21–6.62 against 4.11–6.00), and
  the safe reader is above it (5.26–8.31).
- **Unpaced 64 B on ext4**: 3,529,856–3,543,632 against Chronicle's 2,952,843–3,061,325 records/s on x86-64,
  3,458,048–3,466,622 against 2,570,572–2,575,258 on aarch64.
- **Ten-minute runs on ext4, each tool at half of its own rate** (the targets in the table). tickertape's reader p99.9
  was 323–697 µs in SV, against Chronicle Queue's 89.98–102.69 ms (88.08–98.63 ms with its old settings). The tail is
  not a traced property of Chronicle's reader: the plain file, which also has no early write-back, showed one of the
  same size at 1,024 B (87.43–96.93 ms), though not at 64 B (16.69–18.32 ms, at a lower rate), and on xfs Chronicle's
  tail was 10.6–25.1 µs in three of four cells (below). In SD a tickertape checkpoint took 1.38–2.99 ms (median; maxima
  2.36–76.55 ms) and Chronicle's synchronous store flush 302.78–516.16 ms (median; 97.98–119.87 ms with its old
  settings). These two operations do not do the same work (the paragraph under the table). Aeron's SV figures are in the
  paragraph on settings below.

**Behind or level in these runs:**

- **Replay.** On ext4 Chronicle Queue replayed faster than both of tickertape's readers on x86-64 at both sizes (64 B:
  35.57–36.77 against 20.85–26.41 zero-copy and 31.01–32.77 safe) and on aarch64 at 1,024 B (15.31–17.12 against
  10.05–10.36 and 6.26–6.53); at 64 B on aarch64 the ranges overlap. yaque's scan of its queue was faster than
  tickertape's zero-copy reader at 64 B on x86-64 (28.89–29.61) and than its safe reader at 1,024 B on x86-64
  (7.38–7.44).
- **Appending 1,024 B.** On x86-64 ext4 the ranges overlap Chronicle's (cold 2.61–3.08 against 2.82–2.87, steady
  2.92–3.06 against 2.93–3.04). On xfs (n = 2) Chronicle was ahead in W1 steady on both machines (x86-64 3.73–3.76
  against 2.71–2.90, aarch64 4.08–4.11 against 3.99–4.04) and in W1 cold on x86-64 (3.48–3.53 against 2.61–2.73);
  tickertape was ahead in W1 cold on aarch64 xfs (3.90–3.91 against 3.70–3.72). On the tuned x86-64 boot (ext4, n = 2)
  Chronicle was ahead cold (2.93–2.97 against 2.58–2.64) and steady (3.12–3.14 against 2.65–2.67).
- **Reader p99.9 at 10,000 records/s.** Chronicle Queue was lower in three of the four cells, and in three of four runs
  in the fourth (x86-64, 1,024 B; its fourth run, 15.75 ms, was above both tickertape readers; an `unpinned` run had
  25.74 ms). Aeron was lower than the zero-copy reader at 1,024 B on both machines (3.26–3.64 against 4.60–6.84 µs;
  4.57–5.96 against 8.73–9.10), and the plain file's ranges overlapped it there (5.89–6.02; 8.39–8.79).
- **Reader p99.9 at 100,000 records/s.** Aeron had the lowest observed p99.9 in all four cells; its whole range was
  below tickertape's at 1,024 B and overlapped it at 64 B. At 1,024 B Chronicle was lower than tickertape (23.9–24.6
  against 28.6–29.8 µs on x86-64, 14.5–16.3 against 24.8–28.5 on aarch64), and the plain file too (9.85–11.3 and
  14.4–14.7).
- **Reader latency on xfs** (n = 2), with larger gaps: aarch64 1,024 B × 100,000/s, tickertape 38.8–40.8 µs against
  Chronicle 3.36–5.40 and the plain file 23.2–23.6; aarch64 1,024 B × 10,000/s, 11.6–12.7 against 2.98–4.27; aarch64 64
  B × 10,000/s, 5.07–5.34 against 0.75–0.81; x86-64 1,024 B × 100,000/s, 25.5–26.4 against 15.6–15.7.
- **Maximum latency at 1,024 B** (ext4): tickertape's zero-copy reader had a maximum of 993 µs to 1.32 ms in every run;
  at 100,000/s Chronicle's maxima were 178–550 µs, Aeron's 38.5–129 µs and the plain file's 101–142 µs.
- **Unpaced at 1,024 B**: Chronicle's rate was above tickertape's in all ten pinned pairs of probes (both machines and
  file systems, every part), by 0.6–1.6 %, and the plain file's later single probes too (348,781 and 342,397 records/s).
  At 64 B on x86-64 xfs the two were level (3,519,690 and 3,543,512 against 3,493,704 and 3,517,607); Aeron's 64 B rate
  beat tickertape's on aarch64 in round 2 (3,820,950 against 3,466,622); and in the x86-64 64 B ext4 probes Chronicle's
  reader p99.9 was lower (6.50–6.53 against 8.34–8.63 µs). tickertape's probe reader maximum was above Chronicle's in 18
  of the 20 pinned probe pairs (x86-64 64 B ext4: 312.74–355.47 against 143.65–143.79 ms; x86-64 64 B xfs: 25.82–33.91
  against 4.66–4.75 ms), while its probe p99.9 at 1,024 B was lower in all ten 1,024 B pairs (x86-64 ext4: 28.9–29.1
  against 33.5–33.9 µs). yaque's 1,024 B probes (685,628 and 496,821) stopped at their record cap after 327 and 451 s.
- **Ten-minute runs on xfs** (n = 1): Chronicle Queue's reader p99.9 at half its rate was lower than tickertape's in
  three of four cells (aarch64, 64 B: 10.6 against 382 µs; x86-64, 1,024 B: 25.1 against 284 µs; aarch64, 1,024 B: 11.1
  against 201 µs); in the fourth (x86-64, 64 B, targets 1,759,845 and 1,758,803) tickertape's was 432 µs against 33.31
  ms. tickertape's maxima were lower in all four (1.19–2.47 ms against 4.84–38.04 ms).
- **At 90 % of its own rate with a checkpoint every second** (n = 1 per machine and file system), tickertape stalled at
  64 B on both machines (reader p99.9 49.64 ms on x86-64 ext4, 175.37 ms on x86-64 xfs, 267.12 ms on aarch64 ext4, 1.17
  s on aarch64 xfs) and at 1,024 B on aarch64 (123.40 ms on ext4, 1.39 s on xfs), but not at 1,024 B on x86-64 (2.13 and
  1.66 ms). Reader p99.9 is the delay from scheduled send to read and does not say where the delay arose. Chronicle
  Queue's 64 B run failed (below); at 1,024 B it did not hold its schedule either (x86-64 ext4: 307,301 of 315,324
  records/s, 99.5 % of sends late, 293 syncs and 332 missed sync grid points), and its reader's p99.9 was 3.07–18.71 s.
- **On the tuned x86-64 boot** (n = 1), tickertape's unpaced 64 B rate was 2,387,955 records/s, against Chronicle's
  3,056,715 and its own pinned 3,529,856–3,543,632. The isolated CPUs were not interrupt-free: in the ten minutes
  captured inside tickertape's tuned short unit, the writer's CPU received 1,942,342 TLB-shootdown interrupts; their
  source was not traced and may be the tool's own write-back. The probe itself ran in the tuned sustained unit
  (11:56–12:57), inside capture intervals that also cover other tools' units and brought 26–74 million shootdowns to
  that CPU.
- **Aeron IPC** reached 6,489,247–7,206,880 records/s of 64 B unpaced on x86-64 and 2,776,684–4,175,027 of 1,024 B on
  both machines, eight to twelve times tickertape's 1,024 B rate.

**The JVM tools' sustained figures were worse with the new settings.** Aeron IPC's SV reader p99.9 on ext4 was
14.72–107.41 ms at half its own rate (the targets in the table). Compared with the later single runs with the old
settings: Chronicle Queue's SD flush median 302.78–516.16 ms against 97.98–119.87 ms, its SD reader p99.9 302.78–621.28
ms against 168.95–177.08 ms, Aeron's 64 B SV reader p99.9 14.72–36.24 ms against 4.66–6.32 µs. The two configurations
differ in five settings and in JVM logging, and the old-settings runs were hours later, single, and at their own rates;
which difference does it was not measured. Their unpaced probes moved less: Chronicle's old-settings probes were within
its two new-settings probes or up to 2.6 % above their upper end, Aeron's within them except at 1,024 B on aarch64
(2,451,881 against 2,776,684–2,796,872).

**The failed cell.** Chronicle Queue's SD run at 90 % of its probe with 64 B records: in each of the four machine and
file-system combinations both attempts failed (eight attempts). The harness's reader reached its deadline, about 120 s
after the end of the schedule, before it had seen every scheduled record (`IllegalStateException: reader timed out`);
`children.log` has no other error (the writer's result is empty because the run was stopped), and the cause was not
established. It is published as failed; there is no latency figure for it.

**Standing caveats.** Cloud VMs with a hypervisor and rate-capped local SSDs (351 MB/s each), not bare metal; one
session per machine; the v1.0.0 library, not `main`; min–max of n = 4, 2 or 1; SV and SD at a different absolute rate
for each tool; the single-round parts used a fixed tool order, and on xfs tickertape ran first on a freshly made file
system and Chronicle followed on the same one.

### Near the disk's limit

At 1,024 B the unpaced rates of tickertape and Chronicle Queue in the table (337,424–350,374 records/s, 345.5–358.8 MB/s
of payload) are about the limit of the disk. Payload accepted and bytes written to the device are different things: each
of these local SSDs writes at most 351 MB/s by itself (the average of separate raw-device `fio` jobs), and an unpaced
run holds gigabytes of written but not yet written-back pages in the page cache (up to 5.1–5.4 GiB on aarch64 and
10.1–10.2 GiB on x86-64 during 70 s unpaced runs; [`docs/benchmarks.md` → how much of the limit is the
disk](docs/benchmarks.md#2026-11-26-how-much-of-the-limit-is-the-disk-fio-tmpfs-and-one-change-at-a-time-google-cloud-vms)).

Close to that limit the writer can stall. In tickertape's own 10-minute runs before 1.0.0, at 1,024 bytes and 90 % of
the unpaced rate with a background checkpoint (`Writer::request_checkpoint`, which syncs on its own thread) every
second, appends of 50–128 ms recurred on aarch64 ext4 ([operating
envelope](docs/benchmarks.md#operating-envelope-and-remaining-bottlenecks)); that rate is about 324 MB/s of journal
bytes, 92 % of the separately measured device rate. In later 60-second runs on `main` after 1.0.0 (the disk section),
the default's longest appends there were 83–144 ms on the first aarch64 VM and 10–16 ms on a second one; turning early
write-back off (`WriterConfig::early_writeback(false)`) brought them to 1.0–2.3 ms in 5 of 5 runs, at the price of a
background checkpoint becoming durable 0.93 s (median) after its request instead of 0.05–0.39 s. On x86-64 there were no
such stalls either way (longest appends 0.25–0.64 ms), and turning it off only made checkpoints that late (0.93 s
against 1.7–2.3 ms). What to plan for is in [`docs/operations.md`](docs/operations.md).

## Quickstart

Add the library to your project:

```bash
cargo add tickertape
```

[API documentation](https://docs.rs/tickertape/1.1.0/tickertape/) · [CLI reference](docs/cli.md)

Install the `tickertape` CLI (Linux x86-64 and ARM64, macOS Intel and Apple Silicon) in one of these ways.
[Installation](docs/install.md) has the details, the static Linux builds and how to verify a download.

From source with Cargo:

```bash
cargo install tickertape-cli --locked
```

A prebuilt binary with [cargo-binstall](https://github.com/cargo-bins/cargo-binstall) (from v0.5.1):

```bash
cargo binstall tickertape-cli
```

With Homebrew on macOS or Linux (from v0.5.1):

```bash
brew install thomas0903/tap/tickertape
```

With mise:

```bash
mise use github:thomas0903/tickertape
```

Or download an archive and `SHA256SUMS` from the
[GitHub releases](https://github.com/thomas0903/tickertape/releases/latest).

As a library:

```rust
use tickertape::{NewRecord, Reader, ReaderConfig, Writer, WriterConfig};

fn main() -> tickertape::Result<()> {
    // Writer process: append, and checkpoint when you need durability, not just visibility.
    let mut writer = Writer::open("/tmp/journal", WriterConfig::default())?;
    let seq = writer.append(&NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: tickertape::clock::realtime_ns(),
        payload: b"hello",
    })?;
    writer.checkpoint()?;

    // Reader process (or the same one): replay from any sequence number, then follow live.
    let mut reader = Reader::open("/tmp/journal", ReaderConfig::default())?;
    reader.seek(seq)?;
    while let Some(record) = reader.next_timeout(Some(std::time::Duration::from_secs(1)))? {
        println!("{} {:?}", record.seq, record.payload);
    }
    Ok(())
}
```

The whole flow with synthetic market data (producer, live consumer in another process, verify, Parquet, DuckDB) in six
commands is in [`examples/`](examples/README.md):

```bash
cargo build --release -p tickertape-examples -p tickertape-cli
./target/release/demo-producer --dir /tmp/tt-demo --count 200000 --rate 50000 &
./target/release/demo-consumer --dir /tmp/tt-demo --count 200000
./target/release/tickertape verify /tmp/tt-demo
./target/release/tickertape export /tmp/tt-demo --format parquet --decode market-demo --output /tmp/tt-demo.parquet
python3 -c "import duckdb; print(duckdb.sql(\"SELECT kind, count(*), min(seq), max(seq) FROM read_parquet('/tmp/tt-demo.parquet') GROUP BY kind\"))"
```

CLI reference, exit codes and the JSONL and Parquet schemas: [`docs/cli.md`](docs/cli.md).
Choosing segment sizes and checkpoint intervals, archiving old segments, monitoring and what to do after a crash:
[`docs/operations.md`](docs/operations.md).
From other languages:

| Language | What | Status | Docs |
|---|---|---|---|
| C (and others through FFI) | C ABI: `libtickertape` and `tickertape.h` | stable from 1.0 (ABI version 1) | [`docs/c-api.md`](docs/c-api.md) |
| C++ | Header-only C++17 binding over the C ABI: `tickertape.hpp` (RAII handles, borrowed views, works without exceptions) | stable from 1.0 | [`docs/cpp-api.md`](docs/cpp-api.md) |
| Python | Installable bindings that write, follow and replay journals | on PyPI as `tickertape` (from v0.5.1) | [`bindings/python/`](bindings/python/README.md) |
| Java (JDK 25+) | Binding over the C ABI with the Foreign Function and Memory API, in `bindings/java` | on Maven Central as `io.github.thomas0903:tickertape` (from v0.5.1); stable from 1.0 | [`docs/java-api.md`](docs/java-api.md) |
| Python | Read-only decoder without dependencies | | [`python/`](python/README.md) |

[`docs/python.md`](docs/python.md) compares the two Python options.
What each of these, the Rust API, the CLI's JSON and the on-disk format promise from 1.0:
[`docs/compatibility.md`](docs/compatibility.md).

## iceoryx2 integration

[`integrations/iceoryx2`](integrations/iceoryx2/README.md) records one or several
[iceoryx2](https://github.com/eclipse-iceoryx/iceoryx2) publish/subscribe services into a journal. Any tickertape
reader follows the recording live without iceoryx2, `tickertape-iox2 replay` publishes it back onto iceoryx2 (renamed
by default, at the recorded pace or as fast as possible), `tickertape export` writes it to Parquet, and
`tickertape-iox2 export`/`import` convert to and from iceoryx2's own recording files. It is a separate crate with its
own version (0.1.0, not yet published) and lockfile; its quick start and the byte layout of its records are in its
README. Why iceoryx2: [ADR 0030](docs/decisions/0030-iceoryx2-integration.md).

## Guarantees, in short

The full statement, per platform and with the test for each claim, is [`docs/crash-contract.md`](docs/crash-contract.md).

- **Visible is not durable.** A record is visible to readers, and survives the writer process dying, as soon as
  `append` returns. It is durable only after a later `checkpoint()`. On Linux a checkpoint `msync`s the mapped ranges
  it still holds, `fdatasync`s the segment files and `fsync`s new directory entries, then writes the checkpoint marker
  and `fdatasync`s it. On macOS it syncs the mapped data with `msync(MS_SYNC)`, the segment files and new directory
  entries with `fsync`, then issues `F_FULLFSYNC` on the newest segment before writing the checkpoint marker and again
  on the marker file.
- **Process crashes.** After the writer is killed at any point, every committed record is intact and the next
  writer continues with no gap and no duplicate. Tested with 250 `SIGKILL`s at random points per run, on Linux
  and macOS, with live readers (safe and zero-copy) across all of them.
- **Power loss is not claimed.** Durability after a checkpoint depends on the filesystem and drive honouring
  flushes; it is stated, not tested. Records after the last checkpoint may be lost, including ones readers saw.
  Readers that must not act on such records can use `ReaderConfig::durable_only(true)` (CLI: `--durable-only`),
  which only returns records covered by a checkpoint.
- **Corruption is detected, never silently returned.** Every record is checked (length, CRC32C, sequence). Recovery
  repairs a corrupt tail by replacing its segment and moves disconnected segments aside, preserving both in
  `quarantine/`. Some bytes after the last record are zeroed in place without a copy: usually a torn append that no
  reader saw, but also damage that recovery cannot tell from one. See the crash contract, C8 and C11.
- **Readers.** `Reader::open` reads with `pread` and is memory-safe whatever other processes do. Zero-copy
  `Reader::open_mapped` is `unsafe`: it requires that only a tickertape writer modifies the files.
- **Known limits.** A full btrfs volume can crash the writer with `SIGBUS` even inside preallocated segments; a
  checkpoint on a full APFS volume fails. See the crash contract's "Known risks".

## Non-goals

Replication or any networking; multiple writers to one journal; Windows and network filesystems; live zero-copy
readers in Python (use the Parquet export); typed zero-copy schemas (payloads are bytes plus a schema id). The full
list, and what may come later, is in [`docs/roadmap.md`](docs/roadmap.md).

## Alternatives

A factual overview; the measured comparison is in [`docs/benchmarks.md`](docs/benchmarks.md). Check each project's
own documentation for the current state.

| Project | What it is | Main difference from tickertape |
|---|---|---|
| [Chronicle Queue](https://github.com/OpenHFT/Chronicle-Queue) | Mature Java persisted queue (open-source core) | Its Rust library is part of the commercial Enterprise edition and [requires a licence key](https://rustdoc.chronicle.software/chronicle/queue/index.html) |
| [Aeron](https://aeron.io/) and [Aeron Archive](https://aeron.io/docs/aeron-archive/overview/) | Messaging transport with recording, replay and replication; Rust clients exist (e.g. [rusteron](https://crates.io/crates/rusteron-archive)) | Runs a separate media driver and covers networking and replication; much larger scope |
| [iceoryx2](https://github.com/eclipse-iceoryx/iceoryx2) | Zero-copy shared-memory IPC in Rust | Live publish/subscribe; its own recorder writes one service per file, with no live readers of the recording. [`tickertape-iceoryx2`](integrations/iceoryx2/README.md) records iceoryx2 services into a tickertape journal and replays them |
| [yaque](https://crates.io/crates/yaque), [queue-file](https://crates.io/crates/queue-file) | Disk-backed queues / FIFO files in Rust | Queue semantics (consume once) rather than independent readers replaying from any position |
| [commitlog](https://crates.io/crates/commitlog), [okaywal](https://crates.io/crates/okaywal) | Rust append-only log and write-ahead log | Storage building blocks without a cross-process live-reader API |
| [Databento DBN](https://github.com/databento/dbn) | Market-data encoding format with Rust and Python tooling | A data format, not a multi-reader journal; DBN records could be tickertape payloads |
| [NautilusTrader](https://github.com/nautechsystems/nautilus_trader) | Trading platform with Parquet data catalog and event store | A full application framework rather than a standalone journal library |

Background research: [`docs/research/2026-10-31-market-review.md`](docs/research/2026-10-31-market-review.md).

## Project layout

`crates/tickertape` (library) · `crates/tickertape-cli` (the `tickertape` binary) · `crates/tickertape-ffi` (C ABI,
C header and C++ binding) · `bindings/java` (Java binding) · `tests/` (cross-process and crash tests) · `fuzz/` ·
`benches/` · `examples/` · `integrations/iceoryx2` (separate crate) · `docs/` (format, crash contract, decisions,
playbooks).
Contributions: [`CONTRIBUTING.md`](CONTRIBUTING.md). Security issues: [`SECURITY.md`](SECURITY.md).

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you,
as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
