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
`build.log`, `children.log`, `retries.jsonl` (latency runs discarded and repeated because the wall clock stepped
backwards, at most three per run), Java dependency inventories, `jvm-logs/` (JVM log and late-send events of every
Java process, see "JVM warm-up, Chronicle settings and JVM logs"), and a machine-readable
`skipped.json`. Any build, child,
validation, timeout, or missing-tool failure fails the suite; missing measurements
are never substituted with zero. Results already completed remain available.
After every measurement the runner calls `sync`, so pending write-back never carries over into the next one.
Each measurement owns a new directory under `$TMPDIR` (Python's normal temp-dir
fallback otherwise), including fixtures and control files; only those owned
scratch directories are removed afterward. No privileged cache purge or affinity
changes are made. Failed child groups are terminated and reaped.

`--quick` uses 2,000 records for W1/W3, and 100 warmup + 1,000 measured for W2.
It covers the full matrix and is a functional smoke check, **not publication data**.
The long JVM warm-up is reduced to 4,000 records, which crosses none of the paths it exists for. With
dependencies cached it takes about three minutes; initial downloads/builds depend on the
network and machine. Full mode uses the counts below. The requested full matrix
cannot finish within 30 minutes without overlapping competing benchmarks or
reducing counts: 24 runs at 10,000/s × 1,100,000 sends require **44 minutes** of
scheduled time alone, plus 4.4 minutes at 100,000/s and all throughput/startup work (every Java process also
warms up for a few seconds).
The harness preserves isolation and counts; budget roughly an hour for full mode.

Build/check only (no Java in CI):

```sh
cargo build --locked --release --manifest-path compare/Cargo.toml
cargo fmt --manifest-path compare/Cargo.toml --check
cargo clippy --manifest-path compare/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path compare/Cargo.toml
python3 -m unittest discover -s compare -p 'test_*.py'   # short and sustained mode, JVM settings
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

## Sustained mode

Implementation of [ADR 0026](../docs/decisions/0026-sustained-workloads.md). Runs of at least 10 minutes per
configuration for every compatible tool, both payloads, at rates below and near each tool's sustainable throughput,
in a matrix that survives interrupted sessions. Like the short mode it contains no results; CI never runs it.

```sh
export JAVA_HOME=/path/to/jdk21
# Plan and estimated duration only (writes nothing):
python3 compare/run.py ~/compare-sustained --sustained --dry-run
# Validate a machine: 5 s runs, 64 B, one repeat (a few minutes; never results).
TMPDIR=/mnt/test/tmp python3 compare/run.py ~/compare-quick --sustained --quick --keep-going
# The full matrix: 10-minute runs, 64 B and 1,024 B, two repeats (about 16 hours with every tool).
TMPDIR=/mnt/test/tmp python3 compare/run.py ~/compare-sustained --sustained --keep-going
# After an interruption, the same command plus --resume continues where it stopped.
TMPDIR=/mnt/test/tmp python3 compare/run.py ~/compare-sustained --sustained --keep-going --resume
```

`TMPDIR` selects the filesystem under test, as in the short mode. `benches/session.py` runs this matrix as part of a
recorded session. Options: `--duration` (seconds per run, default 600), `--warmup` (paced seconds before the measured
part, default 10), `--repeats` (default 2), `--tools`, `--workloads`, `--payloads`, `--rates 10000,100000` (explicit
rates for every tool) or `--rate-fractions 0.5,0.9` (default), `--sync-every-ms` (default 1000), `--tt-readers
safe,mapped` (default mapped; `tt-sustained` covers both readers in depth), `--keep-going` (record a failed run in
`failures.jsonl` and continue; the default stops) and `--dry-run`.

**Workloads.** For every tool and payload:

- **probe**: unpaced sends for the run duration, with a concurrent reader where the tool supports one, so the rate is
  what the whole pipeline sustains (writer, storage write-back and reader). Its records carry the actual send time,
  so its reader latency is measured from the send. The probe's `count` is only a disk cap.
- **SV, sustained visibility**: paced at a fraction of the probe's achieved rate (default 50 % and 90 %) with the
  short W2 protocol: intended times, busy-waiting, every record sent when late (no skipping or rebasing), one
  concurrent reader measuring `now - intended` after validation. No harness sync. The first `--warmup` seconds are
  excluded from the histogram, not from the writer's counts.
- **SD, sustained periodic durability**: SV plus the tool's closest equivalent of a checkpoint every
  `--sync-every-ms`, on the writer thread, on a fixed grid (grid points missed during a slow sync are counted as
  `grid_overruns`). Its latency is reported separately (`writer.periodic_sync`).

Visibility-only (SV) and durability (SD) results are separate runs and must be reported separately. Paced rates come
from each tool's own probe, so 90 % is "near this tool's sustainable throughput on this machine", not the same rate
for every tool; use `--rates` for identical absolute rates.

**Supported cases and unequal guarantees.**

| Tool | probe | SV | SD operation | Notes |
|---|---|---|---|---|
| tickertape | writer + reader | yes | `Writer::checkpoint` (data, then the durable marker) | the only tool whose sync also publishes a durable position readers can use |
| Chronicle Queue | writer + reader | yes | `ExcerptAppender.sync` with `SyncMode.SYNC` (`msync(MS_SYNC)` of the store) | not tested for power loss here; which index files it covers is Chronicle's choice |
| Aeron IPC | writer + reader | yes | **unsupported** | IPC has no persistence; Archive is not implemented |
| yaque | writer + reader | yes | **unsupported** | no sync operation; `try_send` only flushes its buffer to the OS |
| queue-file | **writer only** | **writer only** | `QueueFile::sync_all` (`fsync`) | upstream forbids concurrent process access: achieved rate, late sends and sync latency only, no reader latency |
| plain file | writer + reader | yes | `File::sync_data` (`fdatasync`) | no CRC, no recovery protocol |

None of these sync operations is verified for power-loss durability by this suite, and their costs cover different
amounts of data and metadata. `matrix.json` lists the unsupported cases of the selected matrix. Short-mode
guarantee labels (`guarantees` in every result) still apply.

**Outputs.** OUTDIR gets `matrix.json` (the parameters; `--resume` refuses different ones), `environment.txt` (and
`environment-resume-<time>.txt` for every resumed session), `build.log`, `commands.txt` (every child command, labelled
by run), `children.log` (child stderr and raw child JSON), `runs/<label>.json` (one result per run, written
atomically when the run finishes), `skipped/<label>.json` (runs that could not fit on the disk, with the reason),
`failures.jsonl`, `retries.jsonl` (runs discarded because the wall clock stepped backwards, at most three per run)
and `results.jsonl` (all `runs/` results concatenated at the end; `runs/` is the source of truth). Labels are
`<workload>-<tool>-<reader>-<payload>-<rate>-r<repeat>`, with rate `unpaced`, `f50` (50 % of the probe) or
`r10000` (explicit).

Each result has `writer` (`count`, `planned_count`, `achieved_records_per_s`, `late_records`, `max_late_us`,
`stopped_by`, `periodic_sync`), `java` (as in the short mode) and `reader_result` (the latency histogram as in W2; for the probe, `count` and
`latency`; `null` for queue-file), plus `tool`, `tool_version`, `guarantees`, `workload`, `payload`, `reader`,
`concurrent_reader`, `rate_tag`, `rate`, `count`, `warmup`, `duration_s`, `warmup_s`, `sync_every_ms`,
`periodic_sync_operation`, `repeat`, `quick`, start and finish times, the data directory and free space before the run.

**No hidden drops.** The runner rejects a run unless the paced writer sent every scheduled record, the reader received
exactly the records written (minus the warmup in its histogram), the late count is in range, every percentile is
finite and ordered, and an SD run synced at least once. A tool that cannot keep up shows it as late sends, latency
and, for the probe, a lower rate.

**Resume and interruptions.** Runs execute sequentially in a fixed order (probes first, then repeats with tools
interleaved). A finished or skipped run is never repeated; a failed run is retried by the next `--resume`. SIGTERM
and SIGHUP stop the current run and kill its process groups. Each measurement's data lives under a scratch directory
owned by this OUTDIR (`$TMPDIR/tt-compare-sustained-<hash>`), which the next session removes, so a killed session
leaves nothing behind that fills the disk. The runner calls `sync` after every run.

**Disk use.** Long runs at high rates write a lot: 10 minutes at 1,000,000 records/s of 64 B is about 62 GB for
tickertape. Before every run the runner predicts its data (records × a conservative per-record size) and skips it with
the reason in `skipped/` if it exceeds 80 % of free space minus 1 GiB; the probe stops at that size
(`stopped_by: count`). Use a volume larger than the expected data, or a shorter duration.

**Estimated duration.** The runner prints `estimated_seconds` before starting and the remaining estimate before every
run (schedule time plus a fixed allowance per run for process start, drain and `sync`). With all six tools, the
default matrix has 92 runs: 12 probes and, per repeat and payload, 12 SV and 8 SD runs.

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
`workload`, `repeat`, `quick`, and `java` (`null` for tools without a JVM: the requested settings and what each Java
process ran with). W1 has `append`; W3 has `replay`; each contains
`count`, `seconds`, `records_per_s`, `payload_mb_per_s`. Throughput `rate` is null
because W1/W3 are unpaced. W1 adds `flush_operation` and `flush_seconds`.
The runner rejects truncated counts, invalid histogram ordering, and inconsistent
payload byte rates before writing a result line.

**W3: warm replay.** A writer prewrites the same W1 counts and exits. An untimed
complete traversal warms the page cache, then a **fresh reader process** times
open plus traversal from the start and validation, excluding process startup and
reader teardown.
Java additionally warms the read path in that reader JVM before it opens the fixture for timing (next section);
this avoids measuring only interpreter startup. Finite warm-ups do not prove that compilation has converged.

## JVM warm-up, Chronicle settings and JVM logs

Issue #56. Each of these is on by default and has its own switch, and every result of a JVM tool records under `java`
what was requested and what each process reports. `run.py` refuses a result whose process ran with other settings.
Results recorded before this section existed (in `docs/benchmarks.md`) used the old behaviour; all four switches
together give it again, apart from the measured loops being methods of their own now and Aeron's reader using one
fragment handler instead of one lambda per poll.

| Setting | Default | Old behaviour |
|---|---|---|
| JVM warm-up (Chronicle, Aeron) | long: 3 × 1,200,000 records and 8 × 20,000 | `--no-long-warmup`: W1/W3 100,000 records, W2 and sustained runs none |
| Chronicle `microTouch()` in the busy-wait | on | `--no-micro-touch` |
| Chronicle `blockSize` | 1 GiB | `--chronicle-block-size default`: 64 MiB, Chronicle's default |
| JVM logs and late-send events | on | `--no-jvm-logs` |

**Warm-up.** HotSpot compiles a path it has not seen into an uncommon trap; the first time the path is taken, the
compiled code is discarded in the middle of the run. The old warm-up (and the unmeasured start of W2, which is only
100,000 records too) never reached these paths, so each was first taken in the measured window:

- Chronicle's first 64 MiB chunk boundary, at about record 799,000 with 64 B records. With 1,024 B records the first
  100,000 records do cross one, so this gap is specific to 64 B.
- Chronicle's first index array boundary, at record 1,048,576 (4096 × 256).
- The reader's histogram branch, at the first record after the warm-up part.
- Aeron's first 16 MiB term boundary, at record 131,072 with 64 B records. Aeron therefore gets the same warm-up.

The long warm-up runs in every Java process before it announces readiness or starts its timer, in every mode (W1, W2,
W3, probe, SV, SD). It runs the measured loops themselves, in the run's mode and at its rate, on scratch endpoints:
paced sends with the busy-wait, a backlog where every send is late, then paced again; the reader tails a feeder thread
in the same JVM, or replays a scratch queue for W3. Three full rounds cross the index boundary and at least one chunk
boundary each; eight short rounds repeat an endpoint's first records, because some traps only count on their fourth
hit. Chronicle's scratch queues keep 64 MiB chunks in the full rounds (256 KiB in the short ones), whatever the
measured queue uses.

The warm-up must not help the measured run otherwise: every scratch directory is separate from the data directory and
deleted before the measurement, nothing of it is timed, and the process then collects its garbage and runs `sync`, so
that neither its objects nor its write-back reach into the measurement. The W3 warm-up does not read the fixture. A
scratch queue takes up to about 1.3 GB (1,024 B records) while it exists. It is not a proof: `attribute.py` (below)
lists what still deoptimises. In checks of this harness the W2 window had no deoptimisation apart from the loop's own
exit, or one of a small helper method (`MappedBytesStore.inside`) at an index boundary after a chunk change; 15 s
sustained runs had up to two of those in the writer.

**No Chronicle pretoucher.** Chronicle's documentation recommends its pretoucher against page-fault outliers. It is
part of Chronicle Queue Enterprise only, since 5.24 (5.23 still had it in the open-source edition). In the
open-source 2026.8 used here `appender.pretouch()` logs "Pretoucher is only supported in Chronicle Queue Enterprise"
and then throws `StackOverflowError`, because the open-source fallback in `PretouchUtil` calls
`queue.createPretoucher()`, which calls `PretouchUtil` again. The suite uses open-source editions without a licence
(ADR 0018), so it calls what the open-source appender has instead: `microTouch()`, declared by `MicroTouched` (not by
`ExcerptAppender`, hence a cast), on every iteration of the writer's busy-wait between records.

In 2026.8 `microTouch()` (class `MicroToucher`) takes the position of the last written record's header, rounds it up
to the next 4 KiB, and does one compare-and-swap of 0 to 0 on the long there, once per page, if the page lies in the
chunk that is mapped now (with its overlap). That takes the write fault of the **next** data page on the waiting
writer thread instead of inside the next record. It does not read ahead further than one page, does not map the next
chunk, create the next cycle file or touch index pages, and it can only run while the writer waits: not in W1 and the
probe, which never wait, and not while a paced writer is behind. On a system with 16 KiB pages three of four calls
touch a page that is already there. The result records the number of touches (`micro_touched_pages`). Its
companion `bgMicroTouch()` (a background `msync` of written pages) is not used. tickertape and the other tools have no
such hook in this suite; tickertape prefaults on a background thread by itself.

**`blockSize`.** Chronicle's README says about the default of 64 MB: "in production you may want this to be high to
reduce jitter from creating new chunks", and gives `512G` and `1G` as examples. Chronicle's own latency benchmark
(`InternalBenchmarkMain`) sets `blockSize(1 << 30)`. The suite uses that 1 GiB for the writer and the reader. 512 GiB
would need a sparse file of that size and a matching address space on every test system. With 1 GiB, 64 B records
cross a chunk boundary every 12.8 million records, and a W2 run of 1,100,000 × 1,024 B crosses one.

**JVM logs.** Every Java process writes `jvm-logs/<label>-<role>.log` in OUTDIR with
`-Xlog:gc*,safepoint,jit+compilation=debug,deoptimization=debug` (`jit+compilation` logs nothing below debug;
`deoptimization` adds the reason of each trap), decorated with UTC and `System.nanoTime()`. The file beside it,
`<label>-<role>.events.json`, has the measured window and the **episodes**: runs of consecutive late sends (more than
1 µs, the definition of `late_records`) on the writer, or of receives slower than 100 µs on the reader, each with its
first record, start, number of records, maximum and end. Episodes rather than records, because one stall makes every
record of the backlog late. There is room for 65,536 of them. A run near saturation has more, so a filling log keeps
only longer ones: over 10 µs from half full, 100 µs from three quarters, 1 ms from seven eighths. `episodes_dropped`
counts those left out; when it is not zero, the counts below describe the kept episodes only.

```sh
python3 compare/attribute.py OUTDIR/jvm-logs/chronicle-W2-64-default-100000-1-*.events.json
```

prints the safepoints and C2 deoptimisations in the measured window, and how many episodes a GC, another safepoint or
a deoptimisation can explain (its docstring has the rule), or neither. Episodes are kept in preallocated arrays and written after the run. The JVM
logs use `-Xlog:async`: the threads that log hand the line to a writer thread and never wait for the file. After the
long warm-up there is next to nothing to log: checks on an OrbStack VM (Linux on macOS) had 1 to 6 lines per process in the 10 s W2 window.

**Opt-in experiments**, off by default: `--java-experiment always-pre-touch` (`-XX:+AlwaysPreTouch`), `epsilon`
(Epsilon GC with a 4 GiB heap limit; proves that no collection ran, and fails the run if the heap fills) and
`no-guaranteed-safepoint` (`-XX:GuaranteedSafepointInterval=0`). Repeat the option to combine them.

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
| tickertape, current checkout (0.3.0) | `Writer::append`, 128 MiB segments | existing `tt-latency`, `safe` and `mapped` | `Reader::try_next`, `safe` and `open_mapped` | `Writer::checkpoint`, data + durable marker |
| Chronicle Queue 2026.8, Apache-2.0 OSS, `blockSize` 1 GiB | appender `writingDocument`, raw bytes | two JVMs, `microTouch()` in the busy-wait; tailer `readingDocument`, read primitives directly from mapped bytes | new tailer `toStart`, same raw document API | `ExcerptAppender.sync`, default `MS_ASYNC` **submission**, not fsync completion |
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

The runner adds the logging flags and the opt-in experiments per process; `commands.txt` has them as `--jvm=...`.

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

- No core affinity, isolation, power control, harness prefaulting (other than Chronicle's `microTouch()`), privileged cache eviction,
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
  its writer metrics still include warmup. The JVM warm-up before it is documented above.
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
- [Chronicle Queue 2026.8 `MicroToucher`](https://github.com/OpenHFT/Chronicle-Queue/blob/chronicle-queue-2026.8/src/main/java/net/openhft/chronicle/queue/impl/single/MicroToucher.java), [`PretouchUtil`](https://github.com/OpenHFT/Chronicle-Queue/blob/chronicle-queue-2026.8/src/main/java/net/openhft/chronicle/queue/util/PretouchUtil.java) and [`InternalBenchmarkMain`](https://github.com/OpenHFT/Chronicle-Queue/blob/chronicle-queue-2026.8/src/main/java/net/openhft/chronicle/queue/internal/main/InternalBenchmarkMain.java)
- [Chronicle Queue 2026.8 README: block size, pretoucher](https://github.com/OpenHFT/Chronicle-Queue/blob/chronicle-queue-2026.8/README.adoc)
- [Chronicle Queue Maven release metadata](https://repo.maven.apache.org/maven2/net/openhft/chronicle-queue/maven-metadata.xml)
- [Aeron best practices: threading, polling, idle strategies and filesystem](https://github.com/aeron-io/aeron/wiki/Best-Practices-Guide)
- [Aeron 1.54.0](https://github.com/aeron-io/aeron/tree/1.54.0)
- [yaque 0.6.6 API and crash recovery](https://docs.rs/yaque/0.6.6/yaque/)
- [queue-file 2.0.0, including unsupported concurrent process access](https://docs.rs/queue-file/2.0.0/queue_file/)
