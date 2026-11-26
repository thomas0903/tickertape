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

The `append/latency` group repeats `append/records/prefault` with the writer's latency histograms on (ADR 0047), in both
modes that record something.

### What the writer's latency histograms cost (ADR 0047)

`tt-append-cost` gives the instructions of one append and the parts of a timed one:

```sh
cargo build -p tickertape-bench --release --bin tt-append-cost
# Twice, with two counts; the difference of the totals / the difference of the counts. In one segment, no helper threads.
valgrind --tool=callgrind --callgrind-out-file=/dev/null target/release/tt-append-cost append --dir /tmp/a --count 200000 --payload 64
valgrind --tool=callgrind --callgrind-out-file=/dev/null target/release/tt-append-cost append --dir /tmp/b --count 400000 --payload 64
target/release/tt-append-cost parts
```

Gate 1 of ADR 0047 compares the change with the same commit with the latency test compiled out:

```sh
RUSTFLAGS="--cfg tickertape_latency_check_out" CARGO_TARGET_DIR=target/check-out \
    cargo build -p tickertape-bench --release --bin tt-append-cost
```

(a private `cfg` for this measurement: appends are then never timed). For a base commit that does not have the histograms, build the same file there without the lines marked
`[not in the base]`: `grep -v '// \[not in the base\]'`. The time of an append is compared with the `append` benchmark,
as interleaved pairs in alternating order of the two builds, and of the base with a second build of the base: a run says
nothing about a difference that is smaller than base against base. `tt-latency --writer-latency` and `--snapshot-ms` give
the tail of the visible latency with the histograms on.

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
accuracy. Busy polling consumes CPU. Threads are not pinned unless `--writer-cpu`, `--reader-cpu` and
`--helper-cpus` ask for it (Linux); the JSON result's `affinity` is what `/proc` said at the end of the run. Payload creation
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

## Replay and recovery at scale (`tt-scale`)

```sh
cargo run -p tickertape-bench --release --bin tt-scale -- --help
cargo run -p tickertape-bench --release --bin tt-scale -- run --dir /tmp/tt-scale --segments 1000 --segment-size 65536 --repeats 2
```

`tt-scale build` generates a journal of exactly the requested number of segments (all full but the newest, which is
`--fill-last` percent full) with fixed-size records and leaves a checkpoint at its end; `measure` times, each in a
process of its own: `list-directory`, `list-segments`, `reader-open`, the seeks, `replay`, `status` (one sample),
`verify`, `retention-plan` (a plan under a limit no journal reaches), `writer-open-clean`, `writer-open-after-kill`
(after a child writer appended `--tail-records` records without a checkpoint and was killed with `SIGKILL`),
`writer-open-without-marker` (the marker moved aside), and `checkpoint-loop` and `checkpoint-loop-retention` (100
blocking checkpoints after one record each, without and with a retention plan after each). `run` is both. `measure`
changes the journal (writer operations append and checkpoint), so repeated measurements see a few more records. ADR
0042 lists what each operation's cost grows with.

The seeks, each timed with the first record after it: `seek-first`, `seek-middle`, `seek-last`, `seek-segment-end` (the
last record of a full segment in the middle: the longest scan a seek costs), `seek-forward` (a seek to that segment's
first record and 16 equal forward steps through it; `steps_total_s` is the 16 steps) and `seek-to-end`. `--ops seeks`
stands for `reader-open` and all of them, and `--seek-index` lets the reader of every seek use seek indexes (`used_index`
says whether its last seek did). Seek indexes (ADR 0045): `seek-sealed` (the last record of the newest sealed segment),
`index-build` (every index anew; `states`, `index_files`, `index_bytes`) and `seek-sealed-indexed`, the last two after
everything else so that the other operations run without indexes (`--ops unindexed`: all but them), and on request
`index-refresh` (a build with nothing left to do) and `index-check`. Reader operations run once per reader in
`--readers safe,mapped` (default safe). Optional operations: `retention-delete` (deletes the `--count` oldest segments),
`checkpoint-loop-age` and `checkpoint-loop-bytes` (the checkpoint loop with another limit that removes nothing),
`checkpoint-loop-remove-count` and `checkpoint-loop-remove-age` (every checkpoint finds one segment to delete), `cli-verify`, `cli-export` and
`cli-stats` (the program given by `--cli`). `--end <seq>` measures a directory without a checkpoint marker, such as an
archive, up to that sequence number.

Each run reports `wall_s`, `user_cpu_s`, `system_cpu_s`, `minor_faults`, `major_faults` (faults that waited for
storage), on Linux `storage_read_bytes` and `storage_write_bytes` (`/proc/self/io`, read-ahead included),
`peak_rss_bytes` (the operation's own peak: on Linux `VmHWM` after a reset, which includes mapped journal pages;
on macOS `ru_maxrss`), `open_files_before`, `open_files_limit` and `cache_dropped`; with `--sample-open-files` also
`open_files_most` (counted about every millisecond, so such runs are not for timings). `--max-open-files` lowers the
soft limit for each operation. Seeks add `seq` and `skipped_records` (records read before the target in its segment),
replay `records_per_s` and `payload_bytes_per_s`, writer operations `scanned_from_seq`, `next_seq`, `recovery_actions`
and `open_files_held`. An operation that fails is reported with `error` and its cost, the others still run, and the
exit status is 1 unless `--allow-errors` is given. The top level states `journal` (segments, bytes, records),
`ram_bytes`, `cgroup_memory_max_bytes` (Linux), `cold_requested`, `cold_unavailable`, `errors` and `label` (free text
from `--label`, to tell runs of the same journal apart).

**Archived history.** `tt-scale archive --dir J --to A` moves all but the newest `--keep-segments` segments of a journal
to an archive directory as `tickertape archive --to` does, with a reader of each kind open in the oldest segment, and
reports the cost of the move and what the readers then return, what a seek to an archived record does, reading the
archive and then the rest of the journal, `verify` on both, and the rejoined directory. Measure the archive itself with
`measure --dir A --end <boundary_seq>`.

**Missing and corrupt segments.** `tt-scale damage --dir D --segments N` builds two intact journals (the checkpoint
marker in the newest segment, and two segments before it) and, for every case, a damaged copy per probe. It reports
what both readers do at open, replay, four seeks and `seek_to_end`, what `verify` and `status` say, and what
`Writer::open` does under each recovery policy, with the time of each, and what building and checking seek indexes and
seeks with them do. After each writer probe it compares every record of the intact journal, byte for byte, with the
journal and `quarantine/`: `census` says how many are in a segment, only in quarantine, or in neither, and `gone` counts
those that were in a segment before the writer opened and are in neither afterwards (it must be 0; the unit test checks
every case). The internal probes that change a journal run only inside a directory `damage` made.

**Catch-up.** `tt-scale catch-up` opens a writer on a journal `build` left and appends records at `--rate` with a
background checkpoint every `--checkpoint-every`. After `--warmup` and `--baseline` it starts a reader process
`--backlog` records behind the journal's head. The reader checks every sequence number; it has caught up at the first
`try_next` that finds nothing, and then follows for `--live`. The writer's `append` latency is recorded for each phase
(`baseline`, `catch_up`, `live`), as the duration of the call and from the intended time of the record, which counts a
delayed schedule (coordinated omission). Each phase's percentiles are over that phase's appends only.

**Another core.** The tool builds against the 1.0 core too, for before and after on the same journals: check out the
commit to measure in another worktree, replace its `crates/tickertape` with the other version's
(`git checkout v1.0.0 -- crates/tickertape`), and build with `--no-default-features`, which leaves out what needs the
1.1 core (`seek_to_end`).

`--cold` drops the page cache before every run with `sudo -n` (Linux `drop_caches`, macOS `purge`); without
password-less sudo the runs continue warm and say so. In a VM, dropping the guest's cache leaves the host's cache
warm, so those runs are not storage-cold. On Linux, a journal larger than the memory a run may use can be had on a
smaller machine with a cgroup limit (page cache counts against it), for example
`sudo systemd-run --scope -p MemoryMax=1G -p MemorySwapMax=0 sudo -u "$USER" target/release/tt-scale measure ...`;
a recorded run uses a machine whose RAM is smaller than the journal.

### Cloud run (larger than RAM, local SSD)

On a cloud VM with local SSD (the playbook's machine rules apply), with less RAM than the largest journal (64 GiB) and
at least 250 GiB free on the SSD, for example a 4-vCPU, 16 GB machine with one local SSD:

```sh
sudo mkfs.ext4 -F -E lazy_itable_init=0,lazy_journal_init=0 /dev/nvme0n1   # the local SSD's device
sudo mkdir -p /mnt/ssd && sudo mount -o noatime /dev/nvme0n1 /mnt/ssd && sudo chown "$USER" /mnt/ssd
sudo -n true                                   # cold runs need password-less sudo
cargo build --release -p tickertape-bench --bin tt-scale
cargo build --release -p tickertape-cli
benches/scale-session.sh /mnt/ssd/journals ~/tt-scale-out            # every stage: 3.5 hours or so
benches/scale-session.sh /mnt/ssd/journals ~/tt-scale-out seeks      # one stage: the seeks alone
```

`scale-session.sh` runs these stages, and with `TT_SCALE_BASE` set to a second build of the tool ("Another core")
measures the same journals with it too: `counts` (10 to 20,000 segments of 1 MiB, an archive of 5,000 of them, retention,
open files), `many` (100,000 segments of 64 KiB), `sizes` (10 segments of 1 MiB to 1 GiB), `seeks` (the seeks
without and with seek indexes, and the cost of the indexes), `damage` and `big` (64 GiB in 512 segments of 128 MiB:
operations, catch-up, an archive of 32 GiB), and, only when named, `retention` (both cores in turn, 25 minutes:
checkpoints and writer opens with each retention limit, removals). It writes one JSON file per command, `environment.txt` and `progress.txt`.

Check first that `cold_unavailable` is `false`, every cold run has `cache_dropped: true`, and `journal.bytes` of the
big journal exceeds `ram_bytes`. Then read, per operation and segment count or size: `wall_s` and CPU time (how
`reader-open`, the seeks, `writer-open-*`, `list-segments` and `retention-plan` grow with segments, and how
`seek-last`, `replay`, `verify` and `writer-open-without-marker` grow with bytes); `major_faults` (what came from the
SSD); `peak_rss_bytes` (whether memory stays bounded on the big journal); `records_per_s` of cold `replay` (catch-up
rate from storage); and cold `seek-last` against cold `reader-open` (the cost a sparse index would remove, ADR 0042).

## Sustained workloads (`tt-sustained`)

Design and rejected alternatives: [ADR 0026](../docs/decisions/0026-sustained-workloads.md).

```sh
cargo build -p tickertape-bench --release --bins
target/release/tt-sustained --help
# Smoke test: a few seconds, two readers.
target/release/tt-sustained run --dir /tmp/tt-sus --out /tmp/tt-sus-out --rate 20000 --duration 3s \
  --checkpoint-every 500ms --segment-size 1048576 --retain-segments 4 --readers mapped,safe:durable:wait=block
# Ten minutes of 1,024 B records at 100,000/s with checkpoints, rolls, retention and a reader that falls behind.
target/release/tt-sustained run --dir "$J/steady" --out results/steady --rate 100000 --payload 1024 \
  --duration 10m --warmup 10s --checkpoint-every 1s --retain-bytes 68719476736 \
  --readers mapped,safe:wait=default,safe:durable:wait=block,safe:delay=40us
# Bursts at 1,000,000/s for 10 % of every 100 ms (mean 100,000/s); unpaced to find sustainable throughput.
target/release/tt-sustained run ... --burst-rate 1000000 --burst-period 100ms --burst-duty 10
target/release/tt-sustained run ... --unpaced --checkpoint-every 1s --retain-bytes 68719476736
```

One run is one writer process and one process per reader, for `--warmup` plus `--duration` (default 10 minutes; any
duration, down to a few seconds for smoke tests). `--dir` (the journal, on the filesystem under test) and `--out`
(results) must be new or empty and must not contain each other. Run `--help` for every flag.

**Traffic.** `--rate N` schedules record *i* at `t0 + floor(i * 1e9 / N)`. `--burst-rate P` sends bursts at *P*
records/s during the first `--burst-duty` percent (default 10) of every `--burst-period` (default 100 ms), nothing in
the rest: the mean rate is `floor(P * period * duty / 100) / period`. Both store the **intended** time in `local_ts_ns`;
the writer busy-waits for it and sends every record even when late, with no skipping or rebasing, and readers measure
`now - intended`, so a backlog counts as latency (coordinated omission handled as in `tt-latency`). A reader whose wall
clock reads earlier than a record's stamp fails the run (`wall clock moved backwards`); with `--count-clock-backwards`
it records 0 for that sample instead and counts it (`clock_backwards_samples`, `clock_backwards_max_ns` per reader). On
macOS this happened between processes within a minute at 64 B, where readers see records within a microsecond
(`docs/benchmarks.md`, 2026-11-26). `--unpaced` sends as fast as it can until the duration ends; its records carry their
actual send start, so its reader latency is measured from the send, not from a schedule. A paced writer that falls more
than `--overrun-limit` (default 60 s) behind the end of its schedule stops and reports `unsent_records`; nothing is
dropped silently.

**Checkpoints, rolls and retention.** `--checkpoint-every 1s` checkpoints on a fixed wall-clock grid (grid points
missed during a slow checkpoint are counted as `checkpoint_grid_overruns`), `--checkpoint-records N` after every N
records; both run on the writer thread, like an application would, and delay later sends. A final checkpoint after
the last record is timed separately. `--segment-size` controls how often segments roll; roll appends (the appends
that created a segment, classified from the segment capacity as in `tt-checkpoint`) get their own histogram.
`--retain-segments N` / `--retain-bytes B` enable writer retention (delete, after each checkpoint), so a run can write
far more than the disk holds. The tool refuses to start if its estimated peak disk use (bounded by retention when
set) exceeds 90 % of free space (`--no-disk-check` overrides); an unpaced run without retention has no bound and
only gets a warning.

**Payloads.** By default every record's payload is made before the run and `Writer::append` copies it. With
`--encode buffer` a stand-in encoder stores each payload field by field into a reused buffer, which `append` then
copies; with `--encode in-place` it stores the same fields straight into the record (`Writer::append_with`). The
encoding is timed with the append.

**Readers.** `--readers` takes a comma-separated list of `kind[:option]...`: `safe` (`pread`) or `mapped`
(zero-copy), with `durable` (durable-only, needs checkpoints), `wait=spin|default|block|sleep` (as `tt-latency
--wait`; `--wait` sets the default, spin), `delay=<D>` (pause after every record: busy-wait below 1 ms, sleep above),
`stall=<pause>/<every>` and `owned` (copy every record out with `Record::to_owned`). A reader with a delay or stall is
*lagging*: it stops when the writer finishes and reports how far behind it was. When retention removes the segment a
reader is in, the reader gets `Error::Discarded`, seeks to the oldest record still present and counts the records it
never saw (`skipped_by_retention`, `discard_events`). Other readers get `--drain-timeout` (default 60 s) to catch up
after the writer finishes; a reader that doesn't is reported with `drained: false`. For every reader, `records_read +
skipped_by_retention + remaining_records` must equal the records written, or the run fails.

**What it reports.** Text, or with `--json` one object (also always written to `OUT/result.json`) with explicit
unit suffixes: the full configuration (schedule, durability, retention, readers and wait modes, prefault, CPUs), host
facts, the start time, and

- writer: `count`, `planned_records`, `unsent_records`, `stopped_by`, `achieved_records_per_s`,
  `achieved_payload_mb_per_s` (decimal, payload only), `intended_records_per_s`, `late_records` (more than 1 µs after
  the intended time), `max_schedule_delay_us`, `append_latency`, `roll_append_latency`, `checkpoint_latency`,
  `final_checkpoint_us`, `rolls`, `checkpoints`, retention counts and last error, and `at_run_end`: where checkpoints
  and retention stood when the writer stopped sending, before the final blocking checkpoint, which runs retention to
  the end and so hides a backlog (`checkpoints_completed`, `checkpoint_next_seq`, `checkpoint_segment`,
  `retention_removed_segments`, `retention_failures`, `retention_last_error`);
- per reader: `latency` (`latency_kind` is `visible` or `durable_only`), `lag` (`samples`, then
  min/p50/.../max in `_records` and `_bytes`; bytes are records × journal bytes per record), read/skipped/remaining
  counts and `drained`;
- per process: `cpu` (`user_cpu_s`, `system_cpu_s`, their fractions of wall time where 1.0 is one busy core,
  minor and major page faults, context switches, all threads, from `getrusage`), `hot_thread_cpu` (the measuring
  thread alone; Linux only) and, for the writer, `schedule_wait_s` (busy-waiting for intended times, included in its
  CPU time);
- `dataset`: bytes appended, RAM, whether the appended data exceeded RAM, the peak disk estimate and free space.

Latency and lag statistics are min/p50/p90/p99/p99.9/p99.99/max from HdrHistogram (three significant figures).
`--warmup` time is excluded from latency, lag and checkpoint statistics but included in counts, rates and the series.

**Time series.** `OUT/series.jsonl` has one line per wall-clock second (`t_unix_s`, `t_utc`), from the first to the
last second of the run, with the writer's records, late sends, maximum schedule delay, append maximum and p99,
checkpoints and rolls (count and maximum), position and CPU fractions, and per reader the records received, latency
maximum and p99, skipped records, maximum lag in records and bytes, and CPU fractions. The writer's line also has the
`at_run_end` fields as they stood at the end of that second. Operations are attributed to
the second in which they started in their process (a reader: when it received the record), so a writer stall shows
at its start and the readers' latency where they saw it. Timestamps are `CLOCK_REALTIME`, the clock eBPF tools print
with wall-clock timestamps. The raw per-process files (`writer.series.jsonl`, `reader-N.series.jsonl`) stay next to it.

**A failed run** writes no `result.json`. If the writer stopped with an error (for example a full disk), it writes
`OUT/writer-failure.json` with the error, the counts so far and the `at_run_end` fields at the failure, and the
command prints it; the per-process series files are kept.

**Measurement cost.** Per append: two monotonic clock reads, two histogram increments (three on a roll), a
per-second bucket update and one release store of the position. Per received record: one wall-clock read, two
histogram increments, a bucket update and one release store. The measuring threads make no system calls for
measurement except one channel send per second; a sampler thread per process does the rest (the writer's wakes every
millisecond to publish its position with one `pwrite`, readers' every `--lag-interval`, default 10 ms, for one
`pread`, and each reads `getrusage` once a second). To see what this costs on a given machine, compare the unpaced
rate with the Criterion append benchmark on the same filesystem; the difference also includes the readers.

**Lag accuracy.** Lag is the writer's published `next_seq` minus the reader's position, sampled every
`--lag-interval`. The writer's value is up to about a millisecond old, so lag can be understated by that much of the
rate, and it is never negative.

**CPU pinning.** `--cpus 2,3,4,...` pins the writer thread to the first CPU and reader *i*'s thread to CPU *i+1*
(Linux `sched_setaffinity`; needs one CPU per process). Helper threads (tickertape's prefault thread, the samplers)
stay unpinned. On macOS the run continues unpinned and the result says so. A paced writer always busy-waits and a
`wait=spin` reader always spins: each needs a whole core, so give a run at least 1 + readers + 1 CPUs.

**Sizing runs larger than RAM.** Journal bytes per record are the payload plus a 40-byte header rounded up to 8:
104 for 64 B, 1,064 for 1,024 B. A run appends `rate × (warmup + duration) × bytes per record`.

- *Appended data larger than RAM* (steady write-back with the page cache cycling): choose the rate and duration so
  that the appended bytes exceed RAM (the result's `dataset.appended_exceeds_ram`), and keep disk use bounded with
  retention: `--retain-bytes` well below free space. Deleting segments drops their cached pages, so this exercises
  write-back and reclaim, not reads from storage.
- *Readers reading data that is not in RAM*: the retained data must exceed RAM, so the disk must be larger than RAM.
  Retain more than RAM (`--retain-bytes`, e.g. 1.5 × RAM, below free space) and add a lagging reader that falls more
  than RAM behind (for example `safe:delay=<D>` reading at half the rate); its lag in bytes shows when it passed RAM.
  `benches/session.py`'s `beyond-ram` scenario sizes this automatically.

Example: on a 64 GiB machine at 100,000 records/s of 1,024 B (106.4 MB/s of journal data), 1.5 × RAM (103 GB) takes about 16 minutes.

**Where the CPU time goes.** `benches/diagnostics/profile-sustained.sh` runs `tt-sustained`, samples the writer's and
the reader's measuring threads with perf, or counts their system calls, the bytes they read and their heap allocations
(two separate runs: counting distorts a profile); `benches/diagnostics/fold-stacks.py` folds the samples. The results
are profiles, not throughput or latency figures.

**Where a stall comes from.** To find what the writer or a reader waits on during a run (scheduler, page faults,
filesystem and write-back, block I/O), trace it with
[the eBPF diagnostics playbook](../docs/playbooks/diagnose-latency-with-ebpf.md) and the scripts in `diagnostics/`.
Two of them were written for the aarch64 observations of `docs/benchmarks.md` (2026-11-26):
`diagnostics/reader-pauses.bt` tells what a pinned, spinning reader does while it falls behind (off its CPU, in a page
fault or a system call, interrupt work, or its vCPU not running at all), and `diagnostics/frontier-evictions.bt` lists
the page-cache pages the kernel removes or migrates at the write frontier of the active segment, to line up with the
moments a zero-copy reader re-reads a slot.

## Recorded sustained session

`benches/session.py` runs a whole session on the filesystem under test: the tickertape scenarios with `tt-sustained`,
then the sustained comparison matrix (`compare/README.md`). It needs Python 3.9+, Rust, and for the comparison Java 21
and Maven (`JAVA_HOME`), or `--compare-tools` without `chronicle,aeron`.

```sh
# Validate a machine first: a few minutes in total.
python3 benches/session.py --journal-dir /mnt/test/tt --out ~/session-quick --quick --yes
# The full session (prints its estimate, then asks; --yes when not interactive, e.g. under tmux or nohup).
python3 benches/session.py --journal-dir /mnt/test/tt --out ~/session-full
python3 benches/session.py --journal-dir /mnt/test/tt --out ~/session-full --dry-run   # plan and estimate only
```

`--journal-dir` must exist on the filesystem under test; journals and the comparison's data go in a directory the
session owns there (`tt-session-<hash>`), deleted after each run with `sync` before the next. `--out` gets
`manifest.json` (parameters, commit, every run with its exact command, status, times and result path),
`environment.txt` (kernel, distribution, virtualization and DMI product name, CPU, memory, NUMA, filesystem, mount
options, block devices and I/O schedulers, transparent huge pages, write-back sysctls, rustc, cargo, git commit and
status, Java and Maven), `commands.txt`, `tickertape/<run>/` (each run's `result.json` and series), `compare/` (the
comparison's own output), and `logs/`.

The session is safe to re-run: the same command skips finished runs, removes a run's partial output and the leftovers
of a killed session, and refuses an `--out` started with different parameters. SIGTERM or SIGHUP (a closed terminal)
stops the current run and kills its processes. Matrices:

- `--quick`: 64 B only, 10 s per tickertape scenario (probe, steady-50, bursty, lagging, visibility-only), then the
  comparison's quick mode with 3 s runs. Functional validation only, never results.
- `full` (default): 64 B and 1,024 B, 10 minutes per scenario plus 10 s warmup, two repeats (the probe once), all seven
  scenarios, then the full comparison (`compare/README.md`). It prints an estimate of roughly 20 hours with every tool;
  `--only`, `--scenarios`, `--compare-tools`, `--duration` and `--repeats` select less.

Scenario definitions are in `python3 benches/session.py --help`. Rates derive from the tickertape probe of the same
payload, so they adapt to the machine; the probe's rate and every derived argument are in each run's `result.json`.

## Recording a run

Follow [the benchmark playbook](../docs/playbooks/run-benchmark.md). Build release
binaries, commit the harness before recording results, preserve raw output outside
the repository, and run each benchmark at least twice. Record:

- Exact command, journal directory and temporary-filesystem selection; git commit
  and `rustc --version`.
- CPU model/architecture, memory, OS version, kernel and journal filesystem.
- Cache mode, reader kind, payload, segment size, rate, count, warmup and checkpoint
  batch settings; whether coordinated omission is handled by intended timestamps.
- Host load, power state and any external affinity settings. `tt-sustained --cpus` and
  `tt-latency --writer-cpu`/`--reader-cpu` apply affinity, and their results record it.
- Virtualization explicitly: **OrbStack VM (Linux on macOS)** plus host Mac model,
  never bare-metal Linux. CI is smoke only. In the OrbStack VM use its own disk
  (`TMPDIR=$HOME/tmp`); `/tmp` is tmpfs and unsuitable for storage measurements.

Maintainers record measured results in `docs/benchmarks.md`, with their
provenance; this implementation adds no measured numbers to documentation.
