# Local comparison harness

Implementation of [ADR 0018](../docs/decisions/0018-competitive-benchmarks.md).
This directory is excluded from the product Cargo workspace and its dependency
policy. It contains no measured results. The maintainer records publication runs
in `docs/benchmarks.md` with the benchmark playbook's provenance and both repeats.

## Run

Requires Rust/Cargo, Python 3.11+ (for `tt-latency`'s process group, see "Signals, failures and leftover
processes"), Java **21**, and Maven. Initial dependency downloads need network access; the benchmark processes do not. Chronicle analytics is disabled.
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
It emits **112 result lines in 20 JSONL files**, `environment.txt`, `commands.txt`,
`build.log`, `children.log`, `retries.jsonl` (latency runs discarded and repeated because the wall clock stepped
backwards, at most three per run), Java dependency inventories, `jvm-logs/` (JVM log and late-send events of every
Java process, see "JVM warm-up, tool settings and JVM logs"), and a machine-readable
`skipped.json`. Any build, child,
validation, timeout, or missing-tool failure fails the suite; missing measurements
are never substituted with zero. Results already completed remain available, in `OUTDIR.incomplete-N` (see
"Signals, failures and leftover processes").
After every measurement the runner calls `sync`, so pending write-back never carries over into the next one.
Each measurement owns a new directory under `$TMPDIR` (Python's normal temp-dir
fallback otherwise), including fixtures and control files; only those owned
scratch directories are removed afterward. No privileged cache purge is made. On Linux
the threads are pinned (below). Failed child groups are terminated and reaped.

Options of the short mode (the sustained mode has its own, below):

| Option | Default | Effect |
|---|---|---|
| `--quick` | off | the smoke counts below; never results |
| `--tools LIST` | all six | comma-separated subset of `tickertape,chronicle,aeron,yaque,queue-file,plain-file`, with the sustained mode's syntax and checks (an unknown or repeated name stops before anything is created) |
| `--writer-cpu`, `--reader-cpu`, `--helper-cpus`, `--no-pin` | automatic on Linux | see "Thread pinning" |
| `--no-long-warmup`, `--no-micro-touch`, `--chronicle-block-size`, `--aeron-sparse-terms`, `--no-aeron-pre-touch`, `--no-jvm-logs`, `--java-experiment` | new settings | see "JVM warm-up, tool settings and JVM logs" |

With `--tools` the run is the full run's cells of those tools and nothing else: the same file names, the same result
fields, and the tools in the full run's order whatever the order given (`--tools aeron,chronicle` runs Chronicle
first within each payload, as a full run does). `skipped.json` lists only the selected tools; only their Java projects
are built (and `tt-latency` only with tickertape), and a JDK is needed only if Chronicle or Aeron is selected. Every
result line has `selected_tools` (all six in a full run) and `environment.txt` a `selected_tools:` line, so a partial
run is never taken for a full one. `benches/rerun.py` runs each short unit as `run.py OUTDIR ... --tools TOOL`.
Against the v1.0.0 rerun's short units (which built both Java projects and wrote the full `skipped.json`) the file set
differs in two ways: a unit without a JVM has no `*-dependencies.txt`, and `skipped.json` lists only the unit's tool
(`{}` for a tool with nothing skipped).

`--quick` uses 2,000 records for W1/W3, and 100 warmup + 1,000 measured for W2.
It covers the full matrix and is a functional smoke check, **not publication data**.
The long JVM warm-up is reduced to 4,000 records, which crosses none of the paths it exists for. With
dependencies cached it takes a few minutes; initial downloads/builds depend on the
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
python3 -m unittest discover -s compare -p 'test_*.py'   # short and sustained mode, JVM settings, signals, --tools
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
# Both roles take --cpu N (pin the measured thread) and --helper-cpus LIST (Rust adapters).
```

## Thread pinning

Both modes pin the threads on Linux, the same way for every tool:

| Thread | Where it runs |
|---|---|
| measured writer thread | `--writer-cpu`, alone |
| measured reader thread | `--reader-cpu`, alone |
| everything else | `--helper-cpus` |

"Everything else" is tickertape's prefault, write-back and checkpoint helpers, the JVM's GC, JIT and service threads,
Chronicle's background threads, Aeron's media driver (conductor, sender, receiver) and client conductors, yaque's file
watchers, and the runner itself.

- **How.** After the build the runner moves itself to the helper CPUs, so every process and thread it starts inherits
  them; tickertape also gets them as `WriterConfig::helper_cpus`, since its default would use every CPU of the cpuset.
  The measured thread then pins itself (`sched_setaffinity`; from Java with `taskset` on its thread id, so no tool needs
  an extra native library): after opening its endpoint, except in `W1-cold` and in W3's reader, where opening is inside
  the timer. There it pins itself first, so threads that the tool starts in the timed open begin on the measured CPU.
- **Threads that start on a measured CPU.** A thread begins on the CPUs of the thread that starts it. Every 2 ms the
  runner moves such threads to the helper CPUs and counts them in `moved_threads`, until the measured thread has read
  back the affinity. Known ones are yaque's reader (one file watcher per segment) and, in `W1-cold`, tickertape's
  helpers, which also place themselves. Until it is moved, such a thread competes with the measured one: in functional
  VM runs yaque's reader waited for a CPU for up to a few milliseconds per run, at 1 ms as at 10 ms, because the kernel
  runs the new thread first. That can be in yaque's tail latency; no other tool's
  thread was moved outside a timed open in those runs. `moved_threads` is `null`
  where nothing was watched: in unpinned runs, and in tickertape's W2, where `tt-latency` starts its own processes (its
  read-back is checked like the others).
- **Which CPUs.** Without options, two physical cores in one L3 cache, not CPU 0's core if others are free, not isolated
  CPUs; the helpers get every other allowed, non-isolated CPU outside those two cores, so no hyperthread sibling of a
  measured core. The topology comes from `/sys/devices/system/cpu`. A machine without a third physical core needs
  explicit options or `--no-pin`.
- **Named CPUs** are used exactly, for example isolated ones (`isolcpus`):
  `--writer-cpu 2 --reader-cpu 3 --helper-cpus 0-1,4-7`. Helper CPUs should not be isolated (the kernel does not balance
  threads between isolated CPUs) and, for Aeron, at least four: its driver alone busy-spins on three, as its
  documentation recommends with a core for each. With fewer, Aeron's latency is that of threads sharing CPUs: the runner
  prints a warning and records it in `affinity.warnings`. **Do not publish Aeron figures from such a machine**; the
  automatic choice on four cores without hyperthreads is one.
  `measured_cpus_sharing_a_physical_core` lists measured CPUs whose hyperthread sibling a named choice uses too.
- **What is recorded.** `affinity` in every result has the plan and, under `writer` and `reader`, what `/proc` said after
  the last measured record, with the tool still open: `cpus_allowed` and `last_cpu` of the measured thread and
  `cpus_allowed` of every other thread of its process. The runner fails a pinned run if a measured thread was not on
  exactly its CPU, if another thread may run on a measured CPU, or if the part of the runner that moves threads did not
  work. `environment.txt` has the plan, and the sustained mode resumes only with the same one.
- **`--no-pin`** leaves scheduling to the kernel, as in all results recorded before this option existed. On macOS, which
  has no hard affinity, nothing is pinned and results say `affinity: unsupported on macOS`.
- The JVM sizes its GC and JIT thread pools from the CPUs it may use, so in pinned runs from the helper CPUs. Pinning
  does not isolate a CPU: the kernel and other processes can still run there unless the machine is set up for it.

`benches/session.py` passes `--compare-affinity="OPTIONS"` on to this runner.

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
interleaved; the tools in the order `--tools` gives). A finished or skipped run is never repeated; a failed run is
retried by the next `--resume`. SIGTERM, SIGINT and SIGHUP stop the current run and end its process groups (see
"Signals, failures and leftover processes"); the OUTDIR keeps its name, since it resumes. Each measurement's data lives under a scratch directory
owned by this OUTDIR (`$TMPDIR/tt-compare-sustained-<hash>`), which the next session removes, so a killed session
leaves nothing behind that fills the disk. The runner calls `sync` after every run.

**Disk use.** Long runs at high rates write a lot: 10 minutes at 1,000,000 records/s of 64 B is about 62 GB for
tickertape. Before every run the runner predicts its data (records × a conservative per-record size) and skips it with
the reason in `skipped/` if it exceeds 80 % of free space minus 1 GiB; the probe stops at that size
(`stopped_by: count`). Use a volume larger than the expected data, or a shorter duration.

**Estimated duration.** The runner prints `estimated_seconds` before starting and the remaining estimate before every
run (schedule time plus a fixed allowance per run for process start, drain and `sync`). With all six tools, the
default matrix has 92 runs: 12 probes and, per repeat and payload, 12 SV and 8 SD runs.

## Signals, failures and leftover processes

What each process runs in is what it ran in at v1.0.0, with one addition. The adapters (the Chronicle Queue and Aeron
JVMs, whose writer embeds Aeron's media driver, and the Rust adapters) each lead a session and process group of their
own, as before, so a terminal's Ctrl-C or hang-up reaches only the runner. `tt-latency` (tickertape's W2), with the
writer and reader processes it starts, stays in the runner's session, with its output read from a pipe in a blocking
wait, as before; it now leads a process group of its own within that session, so that it can be ended as a group
(Python 3.11's `process_group`). On Linux `environment.txt` records what decides the scheduling groups:
`kernel.sched_autogroup_enabled` (autogroup gives each session a group), the runner's cgroup and its controllers.

However a measurement ends — finished, a tool's failure, the runner's time limit (its records at the intended rate
plus 120 s, plus the JVM warm-up allowance), SIGTERM, SIGINT, or SIGHUP when an ssh session drops — the runner ends
each child's group that is still there: SIGTERM to the whole group, up to 3 s, then SIGKILL to the whole group, and it
reaps the leader, so no zombie and no late writer stays behind. A signal that arrives while a child is being started
or while the children are being stopped takes effect right after that. Further SIGTERM, SIGINT or SIGHUP are then
ignored for the rest of the runner's life, so nothing cuts the cleanup short; to stop the runner at once, `kill -QUIT`
or `kill -KILL` it, and the guard (below) still ends the children. The runner exits with 128 + the signal (143 for
SIGTERM), also when its terminal or output pipe is gone. A signal that was ignored when the runner started (`nohup`)
stays ignored. In both modes nothing is added to a measured loop: the handling happens in the runner, outside the
tools.

An unfinished short-mode run (interrupted or failed) is renamed `OUTDIR.incomplete-N`, the convention of
`benches/rerun.py`, so it never sits where a finished run would; a finished run has `skipped.json`, written last. A
sustained OUTDIR is not renamed: it resumes.

SIGKILL of the runner cannot be handled in the runner. For it the runner starts one guard process per session
(`reaper.py`, in a session of its own, on the runner's helper CPUs when pinned). It blocks on a pipe from the runner
and wakes only when a child starts or ends (to record its group) and when the pipe closes. If the pipe closes
without the runner having released it, the runner is gone: the guard ends every group it still had (SIGTERM, 3 s,
SIGKILL) and renames a short-mode OUTDIR `.incomplete-N`. It renames only the directory it was started for (same
device and inode), not a new one the driver made at the same path to run the unit again. On Linux it signals a group
only while the PID is free or still belongs to the leader it recorded (start time in `/proc/<pid>/stat`); on macOS a
leader reaped just before the runner was killed, whose number another process got within those seconds, would be
signalled. It adds no thread to any tool. Only SIGKILL of the runner
and of the guard together (a `kill -9` of both, or of a whole cgroup) leaves children running; then end their
process groups by PID. `PR_SET_PDEATHSIG` was not used: it is Linux-only, needs code in the child before `exec`, and
would not reach `tt-latency`'s children. Tools exiting when their stdin closes would need a reader thread in each
tool. `benches/rerun.py` runs each unit the same way, with a 20 s grace period and a guard of its own, so SIGKILL of
the driver ends the unit, whose runner then cleans up as above.

`test_signals.py` covers every way out with stand-in children (`standin.py`: no build, no JDK) and, where a JDK 21
and the built Java adapters exist, with real Chronicle Queue and Aeron JVMs (`JvmTests`; skipped otherwise, and in
CI, unless `TT_COMPARE_REQUIRE_JVM=1`). `test_tools.py` compares `--tools` runs with a full run.

## Workloads and schema

**W1: append.** One process appends 1,000,000 × 64 B or 100,000 × 1,024 B as fast
as possible. There are **two figures per tool**, from separate runs (`W1-cold`,
`W1-steady`), because the tools prepare their files at different moments:

| Before the first timed append | tickertape | Chronicle | yaque | queue-file | plain file |
|---|---|---|---|---|---|
| by open | 128 MiB segment preallocated, synced and mapped; helper prefaults the first 256 KiB (Linux) | `metadata.cq4t` only | empty segment file | 8 KiB file, not synced | empty file |
| by the first append | nothing | cycle file, index and first mapping created | nothing | file doubled and synced | nothing |
| by later appends | first-touch faults beyond the prefault window; a new segment at each roll | first-touch faults; next mapped chunk | file growth | file doubled **and synced** each time it is full | file growth |

- **`W1-cold`, cold start:** the timer starts before open, in an empty directory, and stops after the last append.
  Everything above is inside it, for every tool. Process start is outside, and so is the Java JIT warmup, on a scratch
  queue in the same JVM: files are cold, code is not. The record buffer and an empty histogram are created inside it.
- **`W1-steady`, steady burst:** the tool is opened and 1,000 priming records are appended before the timer starts, for
  every tool, so files, first mappings and first pages exist. queue-file is opened with 128 MiB capacity (as large as a
  tickertape or yaque segment); as `QueueFile::open` creates it, it would double and sync its file inside the timer. Beyond
  that the tools keep their settings (Chronicle's mapped chunk is `blockSize`, 1 GiB here unless switched back), so growth, rolls,
  chunk mappings and first-touch page faults beyond the priming records stay inside the timer, except where the tool
  itself prefaults. This is a short burst into ready files, not a long-run rate (see the sustained mode).

W1 is short. At the rates recorded with the old W1 (`docs/benchmarks.md`, v0.4.0 on bare metal) tickertape's 100,000 ×
1,024 B take 31–32 ms, and a faster machine takes less, so one scheduler delay, page-fault stall or helper thread that
starts late is a visible share of the figure. W1 shows what a burst costs, with that noise in it; it cannot show a
steady rate, and differences of a few percent between tools or versions mean nothing here.

Results recorded before this split (`W1` without a suffix) opened the endpoint before the timer and primed nothing:
tickertape's preallocation was outside the timer and Chronicle's cycle file inside. They are comparable with neither
figure. W3's fixture writer still works that way (`--mode append`); it is not timed in any result.

In both, record encoding, copies, publication and growth are inside the timer. No harness checkpoint/fsync occurs
inside the loop. Tools can do internal metadata/growth I/O.
The timer stops before one final sync/flush, whose operation
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
`workload`, `repeat`, `quick`, `affinity`, `java` (`null` for tools without a JVM: the requested settings and what
each Java process ran with), and `selected_tools` (the run's `--tools`, all six in a full run; results written before
the option existed lack it and were full runs, except the v1.0.0 rerun's short units, one tool each: the
`command:` line of their `environment.txt` names the tool). W1 has `append` (with `timer_starts` and `priming_records`); W3 has `replay`; each contains
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

## JVM warm-up, tool settings and JVM logs

Issue #56. Each of these is on by default and has its own switch. Every result of a JVM tool records under `java` what
was requested and what each process reports (the endpoint's settings, the warm-up it ran, its JVM arguments), and the
runner fails a run whose process reports something else.

| Setting | Default | Switch back to |
|---|---|---|
| JVM warm-up (Chronicle, Aeron) | long: 3 × 1,200,000 records and 8 × 20,000 | `--no-long-warmup`: W1/W3 100,000 records, W2 and sustained runs none |
| Chronicle `microTouch()` in the busy-wait | on | `--no-micro-touch` |
| Chronicle `blockSize` | 1 GiB | `--chronicle-block-size default`: 64 MiB, Chronicle's default |
| Aeron term buffers | not sparse | `--aeron-sparse-terms`: sparse, Aeron's default |
| Aeron clients' `preTouchMappedMemory` | on | `--no-aeron-pre-touch`: off, Aeron's default |
| JVM logs and late-send events | on | `--no-jvm-logs` |

The old settings, with which the comparison results in `docs/benchmarks.md` before the rerun at v1.0.0 were recorded,
are all six switches together. The rerun (`docs/benchmarks.md` → "comparison rerun at v1.0.0") ran with these
defaults, with all six switches (its `jvm-old` rows) and with `microTouch()` alone (`microtouch-only`). One of them alone isolates its effect; for `microTouch()` alone, between all off and all on, leave out
`--no-micro-touch` and pass the others:

```sh
# all off, then microTouch() alone, then all on (the default)
python3 compare/run.py OUT ... --no-long-warmup --no-micro-touch --chronicle-block-size default --aeron-sparse-terms --no-aeron-pre-touch --no-jvm-logs
python3 compare/run.py OUT ... --no-long-warmup --chronicle-block-size default --aeron-sparse-terms --no-aeron-pre-touch --no-jvm-logs
python3 compare/run.py OUT ...
```

The old settings are not the old code. What still differs from the harness that recorded the old results: the measured
loops are methods of their own that send or poll in batches of 256; a paced writer spins to the start time before its
first record's busy-wait; the busy-wait calls `Endpoint.idle()`, which does nothing when `microTouch()` is off; Aeron's
reader uses one fragment handler instead of a lambda per poll; the runner pins threads. `--no-jvm-logs` also switches
off the episode log described below.

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

The warm-up must not help or disturb the measured run otherwise:

- Every scratch directory is separate from the data directory and deleted before the measurement, and nothing of it is
  timed. A scratch queue takes up to about 1.3 GB (1,024 B records) while it exists.
- The process then collects its garbage and runs `sync`, so that neither its objects nor its write-back reach into the
  measurement. The W3 reader does not run `sync`, and does not read the fixture: that would change what is replayed.
- In live runs both processes have warmed up before the writer opens the measured endpoint. Write-back while it is open
  would write-protect its dirty pages again, and the pre-touched term buffers of Aeron would fault once more.
- The runner allows the JVM tools ten minutes more per run, because a slow disk can take minutes for the warm-up's
  scratch data (about 4 GB per Java process, 12 GB for a W3 run, at 1,024 B).
- With pinned threads (below) the warm-up and its helper threads run on the helper CPUs, before the measured thread
  pins itself, so they cannot be confined to its core.

It is not a proof: `attribute.py` (below) lists what still deoptimises. In checks of this harness the W2 window had no
deoptimisation apart from the loop's own exit, or one of a small helper method (`MappedBytesStore.inside`) at an index
boundary after a chunk change; 15 s sustained runs had up to two of those in the writer.

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
touch a page that is already there. What it costs the writer: each call that touches nothing does a volatile write
(`lastPageToSync`), so Chronicle's busy-wait is slightly coarser than the other tools'; and the waiting thread pays more
page faults (on aarch64 the compare-and-swap on an unmapped page takes a read fault and then a write fault of 4 KiB),
where without it a fault on the record path covers more. `micro_touch_attempts` in the result counts the calls that
reached a new page, whether or not the touch was made. Its companion `bgMicroTouch()` (a background `msync` of written
pages) is not used.

Chronicle's own evidence is `bench/results.txt` in its tests, whose sections have few labels and no machine or unit. In
its first group the 99.9th percentile of ten runs is 15.2–16.5 without anything, 1.3–1.5 under "microtouch",
15.2–16.3 under "pretouch" and 15.2–16.4 under "bgMicroTouch" (so no gain from the last two). A later group of five
runs does not repeat it: 16.4–19.7 without anything, and 11.6–424,149 under "micro", with two runs above 20,000. The
rerun therefore also publishes `microTouch()` alone, on both architectures (`docs/benchmarks.md` → "comparison rerun
at v1.0.0" → "Chronicle Queue and Aeron under the old and the new settings").

**`blockSize`.** Chronicle's README says about the default of 64 MB: "in production you may want this to be high to
reduce jitter from creating new chunks". It also says "You should avoid changing `blockSize` unnecessarily". Its
examples are `512G` on the command line and `1G` in `system.properties`, and the suite uses that 1 GiB for the writer
and the reader. Chronicle's benchmarks differ: `InternalBenchmarkMain` also sets `1 << 30`,
`ChronicleQueueLatencyDistribution` 128 MiB and `ThroughputPerfMain` 512 GiB on systems with sparse files, which needs
a sparse file of that size and a matching address space on every test system. With 1 GiB, 64 B records cross a chunk
boundary every 12.8 million records, and a W2 run of 1,100,000 × 1,024 B crosses one.

**JVM logs.** Every Java process writes `jvm-logs/<label>-<role>.log` in OUTDIR with
`-Xlog:gc*,safepoint,handshake*,jit+compilation=debug,deoptimization=debug` (`jit+compilation` logs nothing below debug;
`deoptimization` adds the reason of each trap), decorated with UTC and `System.nanoTime()`. The file beside it,
`<label>-<role>.events.json`, has the measured window, the wall clock and `System.nanoTime()` together before and after
the run, and for live runs the **episodes**: runs of consecutive late sends (more than 1 µs, the definition of
`late_records`) on the writer, or of receives slower than 100 µs on the reader, each with its first record, start,
number of records, worst delay and the time of it, and end. Episodes rather than records, because one stall makes every
record of the backlog late. There is room for 65,536 of them. A run near saturation has more, so a filling log keeps
only longer ones: over 10 µs from half full, 100 µs from three quarters, 1 ms from seven eighths. `episodes_dropped`
counts those left out; when it is not zero, the counts below describe the kept episodes only.

```sh
python3 compare/attribute.py OUTDIR/jvm-logs/chronicle-W2-64-default-100000-1-*.events.json
```

prints the pauses and C2 deoptimisations in the measured window, and how many episodes a GC, another safepoint, a
handshake or a deoptimisation can explain (its docstring has the rule), or neither. Only the time in which an episode's
worst delay built up counts, so an event elsewhere in a long episode explains nothing. A handshake's duration includes
waiting for the other threads. Episodes are kept in preallocated arrays and written after the run. The JVM logs use
`-Xlog:async`: the threads that log hand the line to a writer thread and never wait for the file. After the long
warm-up there is next to nothing to log: checks on an OrbStack VM (Linux on macOS) had a handful of lines per process
in the 10 s W2 window.

**Opt-in experiments**, off by default: `--java-experiment always-pre-touch` (`-XX:+AlwaysPreTouch`), `epsilon`
(Epsilon GC with a 4 GiB heap limit; proves that no collection ran, and fails the run if the heap fills) and
`no-guaranteed-safepoint` (`-XX:GuaranteedSafepointInterval=0`). Repeat the option to combine them.

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
`BusySpinIdleStrategy` for conductor and `NoOpIdleStrategy` for sender/receiver and
`termBufferSparseFile(false)`, as Aeron's `LowLatencyMediaDriver` sets them (the idle strategies
are separate instances). Aeron's default is sparse term buffers, which Aeron says "can save space at
the expense of latency"; the driver touches their pages only when they are not sparse. The clients also
set `preTouchMappedMemory(true)`, which is not Aeron's default and not part of that class, because the
client maps the same buffers a second time and takes its page faults in that mapping. Results up to
v1.0.0 used Aeron's defaults for both (`--aeron-sparse-terms`, `--no-aeron-pre-touch`). Both clients have their
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
command, data filesystem selection and the pinning plan. `commands.txt` records each exact child
command including temporary paths. Record host load, power and thermal conditions
manually before publication. Keep both repeats and flag >10%
variation under the existing playbook. Do not publish quick-run rankings.

Other limits to retain with any comparison:

- Threads are pinned on Linux only, and to CPUs that other work may share. No isolation, power control, harness
  prefaulting (other than Chronicle's `microTouch()` and Aeron's own pre-touch), privileged cache eviction,
  replication, compression, or network transport. Busy polling and driver threads
  consume different CPU budgets. OS scheduling/thermal/load/clock effects remain.
- W1 includes tool-specific record copies, CRC, syscall counts, indexing, metadata,
  sparse/preallocated growth, and allocation. The plain baseline adds no CRC or
  crash recovery. These differing guarantees are part of the results, not erased.
- What happens before and inside W1's timer differs per tool (table above); the two figures bracket it. W3 includes
  reader opening/traversal and each tool's native validation, which is not the same amount of work. In W2 and the
  sustained mode Chronicle creates its first cycle file at the first scheduled send, in the warmup.
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
