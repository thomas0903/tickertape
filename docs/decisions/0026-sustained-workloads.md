# 0026: Sustained workloads

- **Status:** accepted
- **Date:** 2026-11-19

## Context

The next release's main objective is sustained performance (roadmap, "Next release"). The existing tools only make
short runs: `tt-latency` and `tt-checkpoint` take seconds, and the comparison suite's latency runs last about 11 s.
ADR 0025 showed that short runs miss what matters at a sustained byte rate: under minutes of write-back, ext4 stalls
the writer for milliseconds, and a small VM for hundreds of milliseconds. The roadmap asks for reproducible workloads
with bursty and steady traffic, periodic checkpoints, segment rolls, several readers, a reader that falls behind and
datasets larger than RAM, for tickertape and, in comparable form, for the other tools. Results are recorded later on
cloud Linux machines, so the tooling must be resumable and must say up front how long a session takes.

## Options considered

1. **Extend `tt-latency`** with more flags: one tool, but its count-based model, single reader and stdout-only output
   don't fit duration-based runs with several readers and a time series. Rejected.
2. **One in-process benchmark** with writer and reader threads: simpler synchronization, but readers would share an
   allocator, page tables and CPU accounting with the writer, unlike real deployments. Rejected; ADR 0009 already
   chose separate processes.
3. **A new `tt-sustained` binary** (separate writer and reader processes, as `tt-latency`), a sustained mode in
   `compare/run.py`, and a session runner. Chosen.

Also rejected:

- **Histogram post-correction for coordinated omission** (HdrHistogram's expected-interval correction): it
  synthesizes samples. The schedule is the correction, as in ADR 0009.
- **Skipping or rebasing late sends** so the writer "catches up": it hides overload. A paced writer that falls more
  than `--overrun-limit` behind its schedule end stops and reports the unsent records instead.
- **Measuring lag by scanning the journal** from the reader: costs I/O on the measured path. A new public API to
  publish the writer's position: unnecessary (below). `Writer::next_seq`, `Reader::position`, `Error::Discarded`,
  `Error::SeqUnavailable` and `inspect::list_segments` were enough; the library is unchanged.
- **A time series sampled from the hot thread only**: idle readers and stalled writers would leave gaps exactly when
  something interesting happens. A sampler thread per process writes its own line every second.
- **`core_affinity`** for pinning: `nix` (already in the lockfile through `ctrlc`, MIT) also provides `getrusage` and
  `statvfs`, so one small dependency covers all three without `unsafe` in the bench crate.
- **Forcing every comparison tool through every workload**: tools without a sync operation (yaque) or without
  persistence (Aeron IPC) are listed as unsupported for periodic durability, and queue-file, which forbids concurrent
  processes, runs writer-only. Nothing is emulated.

## Decision

**`tt-sustained`** runs one writer process and N reader processes for a fixed duration (default 10 minutes):

- Traffic: steady (`--rate`), bursty on/off (`--burst-rate`, `--burst-period`, `--burst-duty`; bursts at the peak
  rate for the duty share of every period) or unpaced (`--unpaced`). Paced records carry their intended time in
  `local_ts_ns`; the writer busy-waits for it, never skips or rebases, and readers measure `now - intended`. Schedule
  arithmetic is integer and drift-free (`floor(i * 1e9 / rate)`, per burst for bursty).
- Writer options: payload, segment size, periodic checkpoints on a fixed wall-clock grid or every N records (on the
  writer thread, so they delay later sends as they would in an application), writer retention by segments or bytes,
  prefault on or off.
- Readers: any list of `safe`/`mapped`, visible or `durable`, with `tt-latency`'s wait modes, and lagging readers with
  a per-record delay or periodic stalls. When retention removes a segment under a reader, it gets `Error::Discarded`,
  seeks, and counts the records it lost; `read + skipped + remaining = written` is checked for every reader.
- Measured: achieved and intended rates, late sends (more than 1 µs), append latency, roll appends (classified by
  sequence number from the segment capacity, as `tt-checkpoint`), checkpoint latency, per-reader latency, reader lag in
  records and journal bytes, CPU time per process (`getrusage`, all threads; on Linux also the measuring thread),
  page faults and context switches, and the full configuration. Statistics are min/p50/p90/p99/p99.9/p99.99/max from
  HdrHistogram with three significant figures.
- Lag: the writer's sampler thread publishes `next_seq` about every millisecond with one 24-byte `pwrite` to a
  control file (position, done flag, check word; torn reads are retried). Each reader's sampler compares it with the
  reader's position every `--lag-interval` (10 ms). Lag is never negative and can be understated by up to a
  millisecond of appends.
- Time series: each process writes per-second lines (operations from the measuring thread through a channel, and its
  own CPU, fault and lag samples); the parent merges them into `series.jsonl`, one line per wall-clock second (UTC),
  so stalls can be lined up with kernel traces. Operations are keyed by when they started in their process.
- Hot-path cost per append: two monotonic clock reads, two histogram increments (three on a roll), a per-second
  bucket update and one release store; per received record: one wall-clock read, two histogram increments, a bucket
  update and one release store. System calls for measurement happen on the sampler threads, never on the measured
  threads (except one channel send per second).

**`compare/run.py --sustained`** adds three workloads per tool and payload: an unpaced *probe* of the run duration
(with a concurrent reader where supported) whose achieved rate is the tool's sustained throughput, *SV* (sustained
visibility at fractions of the probe, default 50 % and 90 %, or explicit rates) and *SD* (SV plus the tool's closest
checkpoint equivalent every second). Each result goes to its own file as soon as the run finishes; `--resume`
skips finished runs; a killed session's scratch data is removed on restart; disk use is checked before every run.

**`benches/session.py`** runs a named matrix (tickertape scenarios derived from a tickertape probe, then the
comparison) on the filesystem under test, captures the environment, keeps a manifest, prints its estimated duration
and needs confirmation (`--yes` when not interactive). `--quick` validates a machine in a few minutes.

## Consequences

- No library or format change. The bench crate gains `nix` (MIT; `fs`, `resource`, `sched`). Calling
  `Reader::open_mapped` still needs an `unsafe` block at the call site, as in `tt-latency`.
- Results depend on the machine; CI only smoke-tests the tools. Measured numbers go to `docs/benchmarks.md` through the
  playbook, never into this ADR. The first recorded session (Google Cloud VMs, x86-64 and aarch64, ext4 and xfs) is in
  `docs/benchmarks.md` → "2026-11-19 to 2026-11-22, sustained workloads on Google Cloud VMs", with the operating
  envelope it supports; its 1,024 B scenarios did not run (their probe failed) and its comparison rows for tickertape
  were measured again in a second session, as that section says.
- Busy-waiting writers and spinning readers burn whole cores: CPU numbers include `schedule_wait_s` for the writer,
  and readers that should show realistic CPU use `wait=block` or `wait=default`.
- Wall-clock steps distort latency and the series; a backwards step fails a run (the comparison runner retries it up
  to three times and records the retry).
- Lagging readers stop when the writer finishes and report what was left; non-lagging readers get `--drain-timeout`
  to catch up, and an undrained reader is reported, not hidden.
- Comparison guarantees still differ per tool (`compare/README.md`); the eBPF diagnostics playbook (next roadmap
  item) builds on the wall-clock series.
