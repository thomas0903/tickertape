# tickertape

**A Rust library and CLI for a local, append-only event journal on memory-mapped files**: one writer process
records events, other processes on the same machine follow them live, and everything can be replayed later or
exported to Parquet for DuckDB and Python.

> Status: [v1.0.0](https://crates.io/crates/tickertape/1.0.0), stable. Linux and macOS. The on-disk format is version 1. What 1.x keeps compatible: [`docs/compatibility.md`](docs/compatibility.md).

Think of it as a flight recorder for a trading system or any other latency-sensitive program:

- One **writer** appends events: opaque payload bytes, a gap-free sequence number, a schema id and two timestamps
  you supply (for example exchange time and local receive time).
- Any number of **readers**, in other processes, follow the journal live or **replay** it from any sequence number.
  Readers take no lock and write nothing the writer waits for. A reader can spin, poll or block. A blocking reader
  sleeps in the kernel and the writer wakes it (futex on Linux, `os_sync_wait_on_address` on macOS 14.4+; elsewhere it
  polls), which costs the writer a wake-up system call for each record the reader blocked for: on `main` after 1.0.0,
  an `append` p50 of 1.5 µs beside a blocking reader against 0.117 µs beside a spinning one (64 B at 20,000 records/s
  on an x86-64 cloud VM; [`docs/benchmarks.md` → readers that block](docs/benchmarks.md#readers-that-block-the-rule-added-after-review-x86-64)).
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
(Neoverse-V2), Ubuntu 24.04, ext4 and xfs on local NVMe SSDs. The library is the **v1.0.0** release; changes merged
since are not in these figures. Every table (with the xfs, unpinned and tuned rows), the failed cell and the caveats are
in [`docs/benchmarks.md` → comparison rerun at
v1.0.0](docs/benchmarks.md#2026-11-25-to-2026-11-26-comparison-rerun-at-v100-google-cloud-vms-linux-x86-64-and-aarch64).
The figures describe these machines in that session; they are not guarantees. The older bare-metal figures (v0.4.0 and
v0.3.0, measured with the earlier harness) are in
[`docs/benchmarks.md`](docs/benchmarks.md#2026-11-02-v040-on-bare-metal-linux-aws-z1dmetal), superseded by this rerun.

**Configuration.** Each measured writer and reader thread was pinned to a CPU of its own, and every other thread of the
tools to the remaining CPUs. Chronicle Queue 2026.8 (open source) and Aeron 1.54.0 ran on OpenJDK 21 with a warm-up
through their measured loops in every process (3 × 1,200,000 and 8 × 20,000 records); Chronicle with a 1 GiB `blockSize`
and `microTouch()` in its writer's busy-wait (its pretoucher is Enterprise-only); Aeron IPC with non-sparse, pre-touched
term buffers and busy-spinning driver threads. tickertape ran with its defaults (on Linux, prefaulting 256 KiB ahead and
early write-back). It has two readers: the zero-copy `Reader::open_mapped` (`unsafe`) and the safe `Reader::open`
(`pread`). [`compare/README.md`](compare/README.md) says what each setting does and why. The tools promise different
things: Aeron IPC keeps nothing on disk and cannot replay, yaque is a consuming single-reader queue, queue-file allows
no concurrent reader, and the plain file has no checksum or recovery.

**The earlier comparison handicapped Chronicle Queue**, as a reader who knows it pointed out: a short warm-up, the
default 64 MiB `blockSize`, no `microTouch()`, no pinning, and, in the ten-minute runs, tools measured in different
sessions. The rerun also ran Chronicle with those old settings (pinned, in the new harness). On x86-64 its reader's
p99.9 at 1,024 B × 10,000 records/s was 34.8–36.8 µs with them, 2.35–2.51 µs with only `microTouch()` added, and
2.36–2.82 µs with the new settings in three of four runs (15.75 ms in the fourth); it appended and replayed faster with
the new settings in every cell. Against the old settings tickertape's zero-copy reader had the lower p99.9 at 1,024 B on
both machines; against the new ones Chronicle's is lower (at x86-64 1,024 B × 10,000 records/s in three of its four
runs).

### Short runs (ext4, pinned)

n = 4 per cell (two rounds of two runs, the rounds 33 to 192 minutes apart depending on the tool); min–max.

**Throughput**, million records/s, higher is better. W1 appends 1,000,000 records of 64 B or 100,000 of 1,024 B, from an
empty directory (cold) or into files opened before the timer (steady); W3 replays them from a warm page cache in a new
process.

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

**Reader latency, p99.9 in µs** from each record's scheduled send to its read, lower is better: 1,000,000 records
measured after 100,000 unmeasured ones, sent on schedule and never skipped.

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

n = 1 per round; each cell shows round 1; round 2. The probe is unpaced, with a reader following: it is the only row in
which every tool answers the same question. SV and SD are paced at **half of each tool's own probe in the same round**
(SV without syncs, SD with the tool's sync every second), so each tool runs at a different rate: read them as "at half
of its own maximum", not as the same load. Aeron IPC has no sync, so no SD row.

| | x86-64, 64 B | x86-64, 1,024 B | aarch64, 64 B | aarch64, 1,024 B |
|---|---|---|---|---|
| Probe, records/s: tickertape | 3,543,632; 3,529,856 | 346,382; 345,534 | 3,458,048; 3,466,622 | 338,179; 337,424 |
| Probe: Chronicle Queue | 3,061,325; 2,952,843 | 350,374; 349,231 | 2,570,572; 2,575,258 | 343,736; 341,297 |
| Probe: Aeron IPC (no journal) | 7,206,880; 6,489,247 | 4,175,027; 4,036,635 | 3,246,052; 3,820,950 | 2,776,684; 2,796,872 |
| SV reader p99.9 (max): tickertape | 642 µs (1.69 ms); 654 µs (1.56 ms) | 504 µs (1.40 ms); 508 µs (1.38 ms) | 501 µs (1.47 ms); 697 µs (23.81 ms) | 323 µs (1.14 ms); 401 µs (7.83 ms) |
| SV: Chronicle Queue | 102.69 ms (135.53 ms); 100.01 ms (136.05 ms) | 100.27 ms (135.40 ms); 98.44 ms (139.59 ms) | 91.82 ms (134.09 ms); 89.98 ms (133.37 ms) | 91.42 ms (162.92 ms); 99.48 ms (135.53 ms) |
| SV: Aeron IPC | 36.24 ms (78.38 ms); 34.96 ms (80.41 ms) | 107.41 ms (125.96 ms); 99.16 ms (119.14 ms) | 26.02 ms (167.64 ms); 14.72 ms (78.77 ms) | 84.28 ms (166.20 ms); 80.61 ms (159.65 ms) |
| SD sync, median (max): tickertape | 2.95 ms (4.21 ms); 2.34 ms (3.80 ms) | 2.99 ms (4.11 ms); 2.29 ms (3.81 ms) | 1.38 ms (2.36 ms); 1.49 ms (41.94 ms) | 1.39 ms (2.39 ms); 1.40 ms (76.55 ms) |
| SD sync: Chronicle Queue | 361.50 ms (404.49 ms); 348.65 ms (413.66 ms) | 516.16 ms (592.97 ms); 514.33 ms (527.96 ms) | 302.78 ms (325.84 ms); 303.82 ms (540.02 ms) | 506.99 ms (845.15 ms); 502.79 ms (514.33 ms) |
| SD reader p99.9: tickertape | 2.39 ms; 1.83 ms | 2.11 ms; 1.55 ms | 989 µs; 1.80 ms | 837 µs; 1.60 ms |
| SD reader p99.9: Chronicle Queue | 369.62 ms; 352.06 ms | 515.64 ms; 513.80 ms | 302.78 ms; 381.68 ms | 621.28 ms; 502.27 ms |

**Durability is not the same operation in every tool's SD row.** tickertape's checkpoint is `msync` and `fdatasync` of
the written data, then the checkpoint marker and its own `fdatasync`, and it gives readers a durable position; Chronicle
Queue's sync is `ExcerptAppender.sync` with `SyncMode.SYNC` (`msync(MS_SYNC)` of its store). Neither was tested for
power loss here.

### Where tickertape is ahead, and where it is behind

**Ahead in these runs:**

- Appending 64 B records, on both machines and both file systems (W1 cold: 19.11–30.84 against Chronicle Queue's
  8.47–8.53 million records/s on x86-64, 10.46–10.56 against 5.49–5.53 on aarch64), and 1,024 B records on aarch64 ext4
  (4.02–4.20 against 2.76–2.83).
- Reader p99.9 at 64 B × 100,000 records/s against Chronicle Queue (2.27–5.96 against 11.8–15.3 µs on x86-64, 1.88–2.08
  against 7.85–13.3 µs on aarch64); Aeron's range overlaps tickertape's there (0.39–3.47 and 0.62–3.03 µs).
- Unpaced 64 B on ext4 (3,529,856–3,543,632 against Chronicle's 2,952,843–3,061,325 records/s on x86-64,
  3,458,048–3,466,622 against 2,570,572–2,575,258 on aarch64).
- At half of each tool's own rate on ext4: reader p99.9 of 323–697 µs against Chronicle Queue's 89.98–102.69 ms and
  Aeron's 14.72–107.41 ms; with a sync every second, a checkpoint median of 1.38–2.99 ms against Chronicle's sync median
  of 302.78–516.16 ms. These are different absolute rates for each tool (the table above).

**Behind or level in these runs:**

- **Replay.** Chronicle Queue replayed faster than both of tickertape's readers on x86-64 at both sizes (64 B:
  35.57–36.77 against 20.85–26.41 zero-copy and 31.01–32.77 safe) and on aarch64 at 1,024 B (15.31–17.12 against
  10.05–10.36 and 6.26–6.53); at 64 B on aarch64 the ranges overlap. yaque's scan of its queue was faster than
  tickertape's zero-copy reader at 64 B on x86-64 (28.89–29.61) and than its safe reader at 1,024 B on x86-64
  (7.38–7.44).
- **Appending 1,024 B on x86-64**: the ranges overlap Chronicle's on ext4 (cold 2.61–3.08 against 2.82–2.87, steady
  2.92–3.06 against 2.93–3.04); on xfs Chronicle was ahead (cold 3.48–3.53 against 2.61–2.73, n = 2).
- **Reader p99.9 at 10,000 records/s**: Chronicle Queue lower in all four cells (at x86-64 1,024 B in three of its four
  runs; aarch64, 64 B: 0.73–0.78 against 2.87–3.46 µs). **At 1,024 B × 100,000 records/s** Chronicle lower (23.9–24.6
  against 28.6–29.8 µs on x86-64, 14.5–16.3 against 24.8–28.5 on aarch64), and the plain file too (9.85–11.3 and
  14.4–14.7). Aeron's ranges are the lowest at 100,000 records/s.
- **xfs, ten-minute runs** (n = 1): Chronicle Queue's reader p99.9 at half its rate was lower than tickertape's in three
  of four cells (aarch64, 64 B: 10.6 against 382 µs; x86-64, 1,024 B: 25.1 against 284 µs; aarch64, 1,024 B: 11.1
  against 201 µs); tickertape's maxima were lower in all four (1.19–2.47 ms against 4.84–38.04 ms).
- **Aeron IPC**, which writes no journal, ran 6,489,247–7,206,880 records/s of 64 B unpaced on x86-64 and
  2,776,684–4,175,027 of 1,024 B on both machines: eight to twelve times tickertape's 1,024 B rate, which is the disk's.
- **At 90 % of its own rate with a checkpoint every second** (n = 1 per machine and file system) tickertape stalls: its
  reader's p99.9 was 49.64 ms to 1.17 s at 64 B and 1.66 ms to 1.39 s at 1,024 B. Chronicle Queue's 64 B run failed
  (below); at 1,024 B its reader's p99.9 was 3.07–18.71 s.
- **On the tuned x86-64 boot** (isolated CPUs, but not interrupt-free: tens of millions of TLB-shootdown interrupts per
  hour reached the writer's CPU), tickertape's unpaced 64 B rate was 2,387,955 records/s against Chronicle's 3,056,715
  (n = 1).

**The new settings did not help the JVM tools in every sustained row.** With the old settings (one run per machine,
hours later, at their own old-settings rates) Chronicle Queue's sync median was 97.98–119.87 ms (302.78–516.16 ms with
the new ones) and Aeron's SV reader p99.9 at 64 B was 4.66–6.32 µs (14.72–36.24 ms). Which setting does it was not
measured. Their unpaced probes, the rows that can be compared, moved less: Aeron's 1,024 B probe on aarch64 was
2,451,881 records/s with the old settings against 2,776,684–2,796,872, Chronicle's within the new-settings rounds or up
to 2.6 % above them.

**The failed cell.** Chronicle Queue's run at 90 % of its probe with 64 B records and a sync every second failed twice
on both machines and both file systems: its reader had not received every record about 120 s after the end of the
schedule (`IllegalStateException: reader timed out`). It is published as failed; there is no latency figure for it.

**Standing caveats.** Cloud VMs with a hypervisor and rate-capped local SSDs (351 MB/s each), not bare metal; one
session per machine; the v1.0.0 library, not `main`; min–max of n = 4, 2 or 1; SV and SD at a different absolute rate
for each tool; the parts with one round ran in a fixed order with tickertape first.

### Near the disk's limit

At 1,024 B the unpaced rates of tickertape and Chronicle Queue (337,424–351,012 records/s, 345.5–359.4 MB/s of payload)
are about the limit of the disk: each of these local SSDs writes at most 351 MB/s by itself (`fio`;
[`docs/benchmarks.md` → how much of the limit is the
disk](docs/benchmarks.md#2026-11-26-how-much-of-the-limit-is-the-disk-fio-tmpfs-and-one-change-at-a-time-google-cloud-vms)).

Close to that limit the writer can stall. In tickertape's own 10-minute runs before 1.0.0 at 1,024 bytes and 90 % of the
unpaced rate, appends of 50–128 ms recurred on aarch64 ext4 ([operating
envelope](docs/benchmarks.md#operating-envelope-and-remaining-bottlenecks)); that rate is 92 % of what the device
writes. In later 60-second runs on `main` after 1.0.0, turning early write-back off
(`WriterConfig::early_writeback(false)`) removed those stalls in 5 of 5 runs on two aarch64 VMs, at the price of
checkpoints becoming durable 0.93 s (median) after the request instead of 0.05–0.39 s (the disk section above). What to
plan for is in [`docs/operations.md`](docs/operations.md).

## Quickstart

Add the library to your project:

```bash
cargo add tickertape
```

[API documentation](https://docs.rs/tickertape/1.0.0/tickertape/) · [CLI reference](docs/cli.md)

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
  `append` returns. It is durable only after a later `checkpoint()` (`msync` and `fdatasync` on Linux; on macOS
  `msync`, `fsync` and two `F_FULLFSYNC`, one for the data and one for the checkpoint marker).
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
