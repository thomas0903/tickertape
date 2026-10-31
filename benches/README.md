# tickertape benchmark harness

These tools measure the local journal on the filesystem you select. They do not
compare other products or make latency guarantees. CI runs are functional smoke
checks only. No recorded benchmark results live here.

## Append and warm replay (Criterion)

```sh
cargo bench -p tickertape-bench --no-run
cargo bench -p tickertape-bench
# Select one family:
cargo bench -p tickertape-bench --bench append
cargo bench -p tickertape-bench --bench replay
```

Append measures `Writer::append` for 64 B and 1,024 B payloads, including payload
copying, CRC and publication. Each iteration batch appends 16,384 records to a
fresh journal with default 128 MiB preallocated segments. Opening, initial segment
creation and teardown are outside timing. First-touch **cold pages** are the
steady state of this append-only journal; this is not a warmed overwrite test.
This does not mean a system-wide cold page cache, nor does it measure checkpoint
durability. Fixtures use `tempfile` on the default temporary filesystem (`TMPDIR`
can select another filesystem). Each batch fits in its initial segment; use the
checkpoint tool below to measure roll appends separately.

Replay builds existing journals containing 1,000,000 records of 64 B or 100,000
records of 1,024 B. It measures safe `pread` and mapped zero-copy readers, including
reader opening, segment traversal and record validation, excluding fixture creation
and checkpointing. An untimed complete read precedes every timed sample so the
page cache is warm; this is not a cold-cache measurement. The mapped-reader
contract requires that only the tickertape writer modifies the journal.

Each benchmark has separate `payload_bytes` and `records` groups because Criterion
supports one throughput unit per group. Bytes count payload only, excluding record
headers, alignment, segment metadata and filesystem traffic. The two groups are
separate runs, so their reported rates can vary. Criterion stores reports under
`target/criterion/`. Plotting is disabled to keep dependencies and MSRV checks small.

## Cross-process visible latency

```sh
cargo run -p tickertape-bench --release --bin tt-latency -- --help
cargo run -p tickertape-bench --release --bin tt-latency -- run --dir /tmp/tt-lat --rate 100000 --count 2000000 --payload 64 --reader mapped
cargo run -p tickertape-bench --release --bin tt-latency -- run --dir /tmp/tt-lat-safe --rate 100000 --count 2000000 --payload 64 --reader safe --warmup 100000 --json
```

Use a **new or empty directory** for every run. Tools retain journals and refuse
nonempty directories; they never remove previous data. Segment size defaults to
128 MiB; `--segment-size 65536` can exercise frequent rolls. `--count` is the total
number sent, **including** warmup. The histogram contains `count - warmup` records.

The parent uses `current_exe()` to spawn separate `writer` and `reader` processes.
Both announce readiness before the parent supplies a shared future start time.
The writer schedules record *i* at `t0 + floor(i * 1e9 / rate)` using
`tickertape::clock::realtime_ns()` and busy-waits until that timestamp. It stores
that **intended** time in `local_ts_ns`. When behind, it sends every record without
skipping or rebasing. The reader uses `Backoff::busy_spin()` and records wall-clock
`now - local_ts_ns` after receiving and validating each non-warmup record.
Scheduling backlog therefore contributes to latency and coordinated omission is
handled by the schedule, without histogram post-correction or synthetic samples.

Histograms have three significant figures in nanoseconds, reported in µs as
min/p50/p90/p99/p99.9/p99.99/max. Achieved writer rate covers the first append start
through the last append completion, including scheduled waits. Late sends start
more than 1 µs after intended time. Both writer metrics include warmup. Small
counts can make achieved rate noisy. JSON emits one object with explicit unit
suffixes on statistics and mode `visible_latency`.

This is **visibility**, not checkpoint durability or network latency. Processes
share one host's realtime clock; wall-clock adjustments can distort observations
and negative latencies fail the run. Nanosecond units do not imply nanosecond
accuracy. Busy polling consumes CPU; no core pinning is performed. Payload creation
and process startup are outside measured latency, while page faults, writer rolls,
scheduling delays and reader validation contribute. No checkpoint runs during live
latency. Parent deadlines and child cleanup bound failed runs.

### Optional best-effort cold replay

```sh
cargo run -p tickertape-bench --release --bin tt-latency -- run --dir /tmp/tt-cold --rate 100000 --count 1000000 --payload 64 --reader safe --cold --json
```

This mode **changes the measurement to replay throughput**, not live latency.
The writer pre-writes, checkpoints and exits, then the parent attempts a system-wide
cache purge: Linux `sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches'`, macOS
`sudo -n purge`. This optional operation needs root and can affect other processes;
normal runs and CI require neither root nor network. There is no password prompt.
If unavailable, the tool prints `cold cache unavailable (needs root)` and exits 0
without a result, including when `--json` was requested. No privileged purge is
attempted in ordinary runs. Eviction is best effort, not a cold-cache guarantee.

A fresh reader process measures replay including reader open, traversal and
validation, excluding process startup. Output mode is `cold_replay`, with records/s
and **decimal payload MB/s** (one MB = 1,000,000 bytes). Rate is a required input for
command consistency but does not pace cold pre-writing or replay; warmup is rejected.

## Checkpoint and segment-roll latency

```sh
cargo run -p tickertape-bench --release --bin tt-checkpoint -- --help
cargo run -p tickertape-bench --release --bin tt-checkpoint -- --dir /tmp/tt-ckpt --payload 64 --batch 1000 --checkpoints 20
cargo run -p tickertape-bench --release --bin tt-checkpoint -- --dir /tmp/tt-roll --payload 64 --batch 1000 --checkpoints 20 --segment-size 65536 --json
```

The tool appends a batch, times one `checkpoint()`, and repeats. It also times one
final checkpoint with nothing new. Separate histograms report dirty checkpoints,
the final empty checkpoint, appends that trigger segment creation, and other
appends. Initial segment creation is outside timing. A single empty checkpoint is
one observation, not a statistically meaningful distribution. Empty categories
have count zero and null statistics; choose a small segment size to obtain rolls.

Fixed-size records in a fresh journal make roll sequence boundaries predictable
from public format helpers, checked against actual segment creation in unit tests.
Only `append()` is timed per record, with classification outside timing. Overall
records/s includes appends, per-call instrumentation and dirty checkpoints, excluding
writer opening and the final empty checkpoint. Durations use a monotonic clock.
The filesystem and storage determine what sync costs; this is not a power-loss test.

## Recording a run

Follow [the benchmark playbook](../docs/playbooks/run-benchmark.md). Build release
binaries, commit the harness before recording results, preserve raw output outside
the repository, and run each benchmark at least twice. Record:

- Exact command, journal directory and temporary-filesystem selection; git commit
  and `rustc --version`.
- CPU model/architecture, memory, OS version, kernel and journal filesystem.
- Cache mode, reader kind, payload, segment size, rate, count, warmup and checkpoint
  batch settings; whether coordinated omission is handled by intended timestamps.
- Host load, power state and any external affinity settings. No affinity is applied
  by this harness.
- Virtualization explicitly: **OrbStack VM (Linux on macOS)** plus host Mac model,
  never bare-metal Linux. CI is smoke only. In the OrbStack VM use its own disk
  (`TMPDIR=$HOME/tmp`); `/tmp` is tmpfs and unsuitable for storage measurements.

Maintainers record measured results in `docs/benchmarks.md`, with their
provenance; this implementation adds no measured numbers to documentation.
