# Local comparison harness

Implementation of [ADR 0018](../docs/decisions/0018-competitive-benchmarks.md).
This directory is excluded from the product Cargo workspace and its dependency
policy. It contains no measured results. The maintainer records publication runs
in `docs/benchmarks.md` with the benchmark playbook's provenance and both repeats.

## Run

Requires Rust/Cargo, Python 3.9+, Java **21**, and Maven. Initial dependency downloads
need network access; the benchmark processes do not. Chronicle analytics is disabled.
On the owner's Apple Silicon Mac the wrappers select the Homebrew JDK below instead
of `/usr/bin/java`. On other hosts set `JAVA_HOME` to a working JDK 21.

```sh
export JAVA_HOME=/opt/homebrew/opt/openjdk@21  # macOS installation; use your JDK elsewhere
compare/run-all.sh /tmp/tt-compare-smoke --quick
compare/run-all.sh /tmp/tt-compare-recorded
# Select the data filesystem, independent of the result directory:
TMPDIR=/path/to/local-disk compare/run-all.sh /path/to/results
```

OUTDIR must be new or empty. The runner builds release binaries and Java projects
once, then executes every implemented tool × workload × payload × reader × rate
**twice**, sequentially. W1 has one tickertape writer, not one duplicate per reader.
It emits **92 result lines in 15 JSONL files**, `environment.txt`, `commands.txt`,
`build.log`, `children.log`, Java dependency inventories, and a machine-readable
`skipped.json`. Any build, child,
validation, timeout, or missing-tool failure fails the suite; missing measurements
are never substituted with zero. Results already completed remain available.
Each measurement owns a new directory under `$TMPDIR` (Python's normal temp-dir
fallback otherwise), including fixtures and control files; only those owned
scratch directories are removed afterward. No privileged cache purge or affinity
changes are made. Failed child groups are terminated and reaped.

`--quick` uses 2,000 records for W1/W3, and 100 warmup + 1,000 measured for W2.
It covers the full matrix and is a functional smoke check, **not publication data**.
Java W1/W3 hot-path JIT warmup is reduced to 1,000 operations. With dependencies
cached it targets less than three minutes; initial downloads/builds depend on the
network and machine. Full mode uses the counts below. The requested full matrix
cannot finish within 30 minutes without overlapping competing benchmarks or
reducing counts: 24 runs at 10,000/s × 1,100,000 sends require **44 minutes** of
scheduled time alone, plus 4.4 minutes at 100,000/s and all throughput/startup work.
The harness preserves isolation and counts; budget roughly an hour for full mode.

Build/check only (no Java in CI):

```sh
cargo build --locked --release --manifest-path compare/Cargo.toml
cargo fmt --manifest-path compare/Cargo.toml --check
cargo clippy --manifest-path compare/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path compare/Cargo.toml
python3 -m unittest discover -s compare -p 'test_*.py'
# Java builds, if invoking an adapter directly:
JAVA_HOME=/path/to/jdk21 mvn -B -q -f compare/chronicle/pom.xml package dependency:build-classpath -Dmdep.outputFile=target/classpath.txt
JAVA_HOME=/path/to/jdk21 mvn -B -q -f compare/aeron/pom.xml package dependency:build-classpath -Dmdep.outputFile=target/classpath.txt
```

Each tool has a `run.sh` process adapter. The Rust adapters share a package and
**pinned `compare/Cargo.lock`**; Java projects pin release dependencies and build
plugins in their POMs. `run-all.sh` builds the existing `tt-latency` separately with
the product lockfile for tickertape W2. Adapter roles (`writer`/`reader`) output a
single JSON object to stdout; diagnostics go to stderr. The parent adds common
metadata and joins the W2 writer/reader measurements. For example:

```sh
mkdir /tmp/file-fixture
compare/plain-file/run.sh writer --dir /tmp/file-fixture --mode append --count 1000000 --payload 64
compare/plain-file/run.sh reader --dir /tmp/file-fixture --mode warm_replay --count 1000000 --payload 64
# Tickertape replay selects --reader safe or --reader mapped.
# W2 roles require --control, --rate, --warmup; use run-all.sh for the barrier.
```

## Workloads and schema

**W1: append.** One process appends 1,000,000 × 64 B or 100,000 × 1,024 B as fast
as possible. Endpoint open, payload allocation and Java JIT warmup are outside the
timer. Record encoding, copies, publication, growth/rollover and first-touch faults
are inside it. No harness checkpoint/fsync occurs inside the loop. Tools can do
internal metadata/growth I/O; tickertape specifically syncs newly created segment
files on rollover. The timer stops before one final sync/flush, whose operation
and separate time are reported (`null` when the API has none). W1 is visible
append throughput, not durable-record throughput. Tool-specific flush operations
are not equivalent durability barriers (see table).

**W2: cross-process visible latency.** Each tool runs one writer process and a
separate reader process. Both announce readiness before the parent atomically
publishes a start time at least 50 ms in the future. Record `i` has intended time
`t0 + floor(i * 1e9 / rate)`, at 100,000/s and 10,000/s. Writers busy-wait until
that wall-clock time and send **every** record when late: no skipping, rebasing,
sleep-based pacing, histogram correction, or synthetic samples. The reader
busy-polls and records wall-clock `now - intended` after sequence and length
validation. A scheduling backlog contributes to latency. 100,000 initial records
are excluded from the histogram; the next 1,000,000 are measured. Java uses
`Instant.now()` epoch seconds/nanos; Rust uses tickertape's `realtime_ns()`.
Wall-clock adjustments and OS clock granularity affect measurements; a negative
observation fails the run. All processes run on the same host. No sync is requested
during W2; ticks that roll can still include internal file creation/sync costs.

HdrHistogram stores ns with **3 significant digits**. W2 JSON follows
`tt-latency --json`: `mode`, `count` (includes warmup), `warmup`, `rate`, `payload`,
`reader`, `segment_size` (null when inapplicable), `latency` with `count`, `min_us`,
`p50_us`, `p90_us`, `p99_us`, `p99_9_us`, `p99_99_us`, `max_us`; `writer` with
`count`, `achieved_records_per_s`, `late_records`; and `coordinated_omission`.
Late means send starts **more than 1 µs** after intended, including warmup.
Achieved writer rate covers first send start to last completion including pacing,
encoding and backpressure; it includes warmup. Short quick runs make this noisy.
Every result additionally has `tool`, `tool_version`, structured `guarantees`,
`workload`, `repeat`, and `quick`. W1 has `append`; W3 has `replay`; each contains
`count`, `seconds`, `records_per_s`, `payload_mb_per_s`. Throughput `rate` is null
because W1/W3 are unpaced. W1 adds `flush_operation` and `flush_seconds`.
The runner rejects truncated counts, invalid histogram ordering, and inconsistent
payload byte rates before writing a result line.

**W3: warm replay.** A writer prewrites the same W1 counts and exits. An untimed
complete traversal warms the page cache, then a **fresh reader process** times
open plus traversal from the start and validation, excluding process startup and
reader teardown.
Java additionally warms the real read path in that reader JVM before reopening the
endpoint for timing; this avoids measuring only interpreter startup. It reads up
to 100,000 records for JIT warmup (1,000 in quick). W1 Java JIT warmup uses a separate
scratch queue. These finite warmups do not prove compilation has converged.

`payload` means **application bytes only** (64 or 1,024). Tickertape uses its native
sequence and local timestamp fields. All other tools carry an additional **16 B
little-endian envelope**: sequence u64 then intended timestamp u64, followed by the
application bytes. W1/W3 timestamps are zero. Plain file adds a 4 B little-endian
length prefix covering envelope + application payload. MB/s is decimal application
bytes/s ÷ 1,000,000, excluding envelopes, tool headers, alignment and filesystem I/O.
This makes application work comparable while retaining each tool's native framing.
Readers validate sequence and length and observe payload views/buffers; they do not
perform an application computation or checksum the full application payload. Native record
validation still applies where the tool performs it.

## Per-tool mapping and guarantees

Pins were selected from Maven Central/crates.io release metadata on 2026-11-01.
These are workload mappings and upstream contracts, **not newly tested kill or
power-loss guarantees**. Sync times alone establish no power-loss guarantee.

| Tool / version | W1 | W2 | W3 | Final W1 operation |
|---|---|---|---|---|
| tickertape 0.2.0, current checkout | `Writer::append`, 128 MiB segments | existing `tt-latency`, `safe` and `mapped` | `Reader::try_next`, `safe` and `open_mapped` | `Writer::checkpoint`, data + durable marker |
| Chronicle Queue 2026.8, Apache-2.0 OSS | appender `writingDocument`, raw bytes | two JVMs; tailer `readingDocument`, read primitives directly from mapped bytes | new tailer `toStart`, same raw document API | `ExcerptAppender.sync`, default `MS_ASYNC` **submission**, not fsync completion |
| Aeron 1.54.0, Apache-2.0 | skipped | IPC publication `offer` / subscription `poll(256)`, two JVMs | skipped | none |
| yaque 0.6.6 | unbounded `try_send`, 128 MiB segments | `Receiver::try_recv` + guard `commit`, no periodic state saves | non-consuming `QueueIter`, from initial saved queue head | none exposed; `try_send` flushes its user buffer to file per message, **not fsync** |
| queue-file 2.0.0 | `add`, `set_sync_writes(false)`; default header publication retained | unsupported: upstream explicitly forbids concurrent process access | `iter().borrowed_next()`, non-consuming, reuses read buffer | `sync_all` |
| plain file, suite v1 | unbuffered `File::write_all`, normally one `write(2)` per complete frame, retries short writes | unbuffered `File::read`, retains partial frames and polls at EOF | same parser from offset 0 | `File::sync_all` |

| Tool / selected mode | Survives writer SIGKILL? | Persistent / arbitrary-position replay | Multiple independent readers | Zero-copy application read? |
|---|---|---|---|---|
| tickertape | published validated prefix under product crash contract; checkpoint needed for power-loss durability | file journal / yes, native sequence seek | yes | mapped: yes; safe: no |
| Chronicle OSS | completed documents in OS-backed mapped files per upstream; crash-during-document not tested here | file journal / yes, index seek | yes, independent tailers | yes, primitives read directly in document scope |
| Aeron IPC | no durable recovery; a subscriber may drain retained buffers while driver lives | **IPC transport, no persistence** / no archive replay | yes, active subscribers; flow control differs | yes, callback view into term buffer; send copies |
| yaque | upstream offers recovery after unhandled kills; stale locks/replay possible; not tested here | persistent consuming SPSC queue / no arbitrary seek; W3 only scans unconsumed data | no, exclusive receiver lock | no, allocates vectors |
| queue-file, sync disabled | upstream atomic V2 headers; weaker durability without per-operation sync; not tested here | FIFO file / iteration of retained items, no journal-position API | no supported concurrent access | no, borrowed buffer is a reused **copy** |
| plain file | completed writes remain in OS cache; partial last frame possible, no recovery protocol | file / sequential scan only in this adapter | yes, no shared mutable reader metadata | no |

Aeron's media driver is embedded in the writer JVM, with **DEDICATED** threading,
`BusySpinIdleStrategy` for conductor and `NoOpIdleStrategy` for sender/receiver,
following Aeron's `LowLatencyMediaDriver` recommendation. Both clients have their
own conductor threads; the reader busy-polls `Subscription.poll`. Messages here
fit the IPC MTU; no fragmentation/reassembly path is needed. IPC term size is
16 MiB. Publication backpressure is retried without dropping or changing intended
time. The writer keeps its driver alive until the reader drains. Busy threads use
several cores: this is a higher resource budget than the simple file adapter.
All Aeron buffers use the selected temporary filesystem. In particular macOS APFS
is **not** the RAM disk Aeron's docs suggest for ideal IPC performance. No RAM disk
is silently created for one tool.

**Archive is not implemented.** Its recording service, recording-position drain,
stop-recording protocol and replay publication require a separate adapter and
completion definition to distinguish publication from archive visibility/durability.
The present suite provides the requested IPC W2, and explicitly skips Aeron W1/W3
in `skipped.json`; it never treats IPC term files as replayable archive files.

Yaque's W2 commits consume records and may remove old segments. Receiver periodic
state saves are disabled (`save_every_nth(None)`, `save_every(None)`) to avoid mixing
receiver checkpoint policy into visibility latency. W3 intentionally uses the
upstream-recommended `QueueIter` instead: it leaves unconsumed fixtures intact,
so an untimed warm traversal and a fresh timed reader see the same records.
This is a **queue scan**, not proof of journal replay semantics. queue-file W3
similarly uses non-destructive iteration, not remove-per-record dequeue timing.
Its `borrowed_next` name denotes a reusable heap buffer, not mmap zero-copy.

## JVM configuration and remaining fairness limits

Both Java adapters use the exact array in [`java-flags.sh`](java-flags.sh):

```text
-Xms256m -Xmx256m -XX:+UseParallelGC
--add-exports=java.base/jdk.internal.misc=ALL-UNNAMED
--add-exports=java.base/jdk.internal.util=ALL-UNNAMED
--add-exports=java.base/jdk.internal.ref=ALL-UNNAMED
--add-exports=java.base/sun.nio.ch=ALL-UNNAMED
--add-exports=jdk.unsupported/sun.misc=ALL-UNNAMED
--add-exports=jdk.compiler/com.sun.tools.javac.file=ALL-UNNAMED
--add-opens=jdk.compiler/com.sun.tools.javac=ALL-UNNAMED
--add-opens=java.base/java.lang=ALL-UNNAMED
--add-opens=java.base/java.lang.reflect=ALL-UNNAMED
--add-opens=java.base/java.io=ALL-UNNAMED
--add-opens=java.base/java.util=ALL-UNNAMED
-Dchronicle.analytics.disable=true
-Dorg.slf4j.simpleLogger.defaultLogLevel=warn
```

The nine classpath module flags follow OpenHFT's Java-version support document;
`jdk.internal.misc` is additionally required by current Agrona, and the newer
Chronicle article includes it and `jdk.internal.util`. Parallel GC and equal fixed
heaps are explicit suite choices; no claim is made that these optimize all hosts.
Environment capture includes JVM version, this exact flags file, CPU, RAM, OS,
filesystem of the temporary data path, Rust/Maven versions, git SHA/dirty state,
command, and data filesystem selection. `commands.txt` records each exact child
command including temporary paths. Record host load, power, thermal conditions and
external affinity manually before publication. Keep both repeats and flag >10%
variation under the existing playbook. Do not publish quick-run rankings.

Other limits to retain with any comparison:

- No core affinity, isolation, power control, prefaulting, privileged cache eviction,
  replication, compression, or network transport. Busy polling and driver threads
  consume different CPU budgets. OS scheduling/thermal/load/clock effects remain.
- W1 includes tool-specific record copies, CRC, syscall counts, indexing, metadata,
  sparse/preallocated growth, and allocation. The plain baseline adds no CRC or
  crash recovery. These differing guarantees are part of the results, not erased.
- Tickertape creates its initial segment during writer open (outside W1), while
  Chronicle can create its first cycle/data mapping in the first timed append.
  Subsequent rolls/growth are timed in both. W3 includes reader opening/traversal
  and each tool's native validation, which is not the same amount of work.
- Fixed payload patterns avoid generation in the timer. Envelope encoding remains
  in the timer for tools without native fields. MB/s counts application data, not
  total bytes written. Native field/index layouts have different overhead.
- Java JIT, allocation, compiler threads and GC remain possible during measurements.
  W2 warmup follows the identical schedule and is excluded only from histogram;
  its writer metrics still include warmup. W1/W3 same-JVM warmup is documented above.
  A short smoke run cannot characterize tail latency or steady-state Java behavior.
- The plain file parser is deliberately unbuffered, since this baseline is `write(2)`
  + `read(2)`; tickertape's safe reader has its own chunked buffer. Polling EOF can
  add syscalls. No direct I/O, batching or readahead policy is forced across tools.
- Chronicle's final async msync submission, tickertape checkpoint and file fsyncs
  must be reported as different operations. No harness sync is requested in W2.
  Internal creation/roll synchronization and ordinary OS background writeback remain.
- Fresh data directories prevent accidental reuse. Page-cache warming is best effort
  under memory pressure. Filesystem choice, including Linux tmpfs or VM storage,
  must accompany any reported result; label OrbStack as a VM, never bare-metal.

Upstream references:

- [OpenHFT Java support and required flags](https://github.com/OpenHFT/OpenHFT/blob/ea/docs/Java-Version-Support.adoc)
- [Chronicle's current JVM flags explanation](https://chronicle.software/insights/blogs/running-chronicle-on-java-17-and-later-the-required-jvm-arguments-explained)
- [Chronicle Queue 2026.8 API examples and contracts](https://github.com/OpenHFT/Chronicle-Queue/blob/chronicle-queue-2026.8/README.adoc)
- [Chronicle Queue Maven release metadata](https://repo.maven.apache.org/maven2/net/openhft/chronicle-queue/maven-metadata.xml)
- [Aeron best practices: threading, polling, idle strategies and filesystem](https://github.com/aeron-io/aeron/wiki/Best-Practices-Guide)
- [Aeron 1.54.0](https://github.com/aeron-io/aeron/tree/1.54.0)
- [yaque 0.6.6 API and crash recovery](https://docs.rs/yaque/0.6.6/yaque/)
- [queue-file 2.0.0, including unsupported concurrent process access](https://docs.rs/queue-file/2.0.0/queue_file/)
