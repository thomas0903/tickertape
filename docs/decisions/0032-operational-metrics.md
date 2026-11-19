# 0032: Operational metrics and reader beacons

- **Status:** proposed
- **Date:** 2026-11-19

## Context

The v1.0 roadmap asks for inexpensive operational metrics for reader lag, checkpoint age, storage headroom and
retention failures, with documented sampling costs and no mandatory telemetry service. An operator running a
recorder wants to know, cheaply and from another process: how far behind each consumer is, how much a power loss
would cost now, how much room is left before the volume fills, and whether retention is failing.

Constraints: format version 1 does not change; `append` gets no new work; the reader hot path stays as it is; readers
never slow the writer down, and nothing a reader does may influence the writer or retention; `unsafe` stays in
`mmap.rs` and `crc.rs`; no background threads, global state, network or mandatory metrics dependency.

Today an outside process can see the segment files, the checkpoint marker and `writer.lock`. Readers leave no trace
on disk, and the writer keeps its retention errors in memory.

## What is measured, and where it comes from

| Value | Source | Accuracy and failure modes |
|---|---|---|
| **Head**: next sequence number, segment, offset | A `pread` scan of the newest segment from the checkpoint position if the marker points into it, otherwise from its first record. A `Sampler` resumes from its previous head | Exact for what was committed when the scan passed. A torn copy of a record being appended is re-read 3 times before the head is reported as `invalid` (as the reader does). If the newest segment is sealed (a crash during a roll), the head is the seal and `end` is `sealed` |
| **Checkpoint**: generation, `next_seq`, position | The marker's two slots, read with two `pread`s (the format's own decoding) | Exact |
| **Checkpoint age** | The marker file's modification time (`fstat`), compared with the observer's wall clock | The marker is written only by `pwrite` of a slot (format.md), so its mtime is the time of the last successful checkpoint, at filesystem timestamp resolution (1 ns on ext4/xfs/btrfs/APFS). It is wrong if something else writes or `touch`es the file, or the journal is copied without preserving times (then it is the copy time; `cp -p`/`rsync -t` keep it). Wall clock: a backward step makes ages too small (reported as 0 with `clock_skew`); a forward step makes them too large until the next checkpoint. If the marker's own sync fails, the slot and mtime are visible although the writer is poisoned: the in-process `Writer::metrics().poisoned` is authoritative there |
| **Exposure** (what a power loss can remove) | Records and bytes between the checkpoint position and the head; its age is the checkpoint age if any record is uncovered, else zero | Exact counts. The age is an upper bound: every uncovered record was appended after the checkpoint. The local timestamp of the oldest uncovered record is reported as well, as data in the caller's clock |
| **Reader lag** (in process) | `Reader::cursor()` against a head: records, and segment-file bytes between the two positions | Records are exact. Bytes count whole segment files between the positions (headers, trailers and unused tails included), so they over-state unread record bytes by less than one record per segment boundary |
| **Reader lag** (other processes) | Opt-in reader beacons (below) | As fresh as the beacon's last update. The unread age is the observer's wall clock minus the `local_ts_ns` of the first unread record, so it is meaningful only when the producer stamps `local_ts_ns` from the same host's wall clock |
| **Storage headroom** | `statvfs` on the journal directory: `f_bavail * f_frsize` available to unprivileged users, and `f_blocks * f_frsize` total | See "Headroom" below |
| **Retention** | In process: `Writer::metrics()` (failed runs, last error), `Writer::retention_report()`. From outside: segment count, total segment bytes, oldest segment's creation time, and how many segments lie before the checkpoint segment | The writer does not persist retention errors. An outside observer can only see their effect: a journal growing past the configured limit |
| **Writer process** | The PID in `writer.lock` and `kill(pid, 0)` | Best effort, never used for health: the PID stays after a clean exit and can be reused. Probing the lock itself would make a writer that opens at that moment fail with `Error::Locked`, so the observer never touches the lock |

**Headroom.** Segments are preallocated, so on APFS, ext4 and xfs writes into the active segment need no new space
(crash contract C10; btrfs is the exception). The failure a full volume causes is the next roll, which needs a whole
new segment (C9). Headroom is therefore reported as bytes available and as **rolls left**: `available / segment size`
(the newest segment's size; a writer reopened with another size uses that), plus one if a prepared spare
(`next-segment.tmp`, ADR 0022) already holds the next segment. It predicts how many more rolls can succeed if
nothing else uses the volume. Given an append rate, the estimated time until full is the time until the writer
would need roll `rolls_left + 1`: the bytes left in the active segment plus `rolls_left` segments, divided by the
rate. A one-shot status has no rate; a `Sampler` measures one between its samples. Prometheus users get a better
estimate from `predict_linear` over the available bytes, which also counts other users of the volume.

## Reader positions from other processes

Options:

1. **In-process API only.** Smallest, but the CLI cannot answer "how far behind is each consumer".
2. **Leave it to the application** (each consumer exports its own lag). Works with any metrics system, but every
   application re-implements it and the operator needs one dashboard per consumer.
3. **Automatic position reporting from every reader.** Readers would write to the journal directory (they may not
   have permission, and today they never write), and the hot path would need a clock read or a counter.
4. **An opt-in beacon the application drives** (chosen).

**Decision: `status::ReaderBeacon`.** An application that wants its reader to be visible creates a beacon with a
name and calls `beacon.update(&reader)` whenever convenient (for example after each batch). Beacons live in
`<dir>/readers/<name>` by default, or in any directory the application chooses.

- **Format** (outside format version 1, own magic and version): 64 bytes, little-endian: magic `TTREADER`, `u32`
  version 1, `u32` pid, `u64` position, segment base and offset of the cursor, `u64` wall-clock update time, 12
  reserved zero bytes, CRC32C of bytes 0–59. Written with one `pwrite` at offset 0, never synced: it only has to
  survive the reader process, not a power loss. An observer that sees a torn copy (CRC mismatch) re-reads once,
  then reports the beacon as `invalid`.
- **Cost.** Readers without a beacon do no work: nothing changes in `Reader`. `update` reads the monotonic clock and
  writes only if the position moved and at least the beacon's interval (default 1 s) has passed since its last
  write, so it costs one clock read per call and at most one `pwrite` per interval. `flush` writes unconditionally.
- **Liveness.** The beacon holds an exclusive `flock` on its file while it exists. An observer probes with a
  non-blocking shared `flock`: if that fails the beacon is `live`; if it succeeds the owner has died (the kernel
  releases `flock`s of dead processes, as for `writer.lock`, crash contract C3), the beacon is `stale`, and the
  observer releases the probe at once. Staleness does not depend on PIDs (no reuse problem, works across PID
  namespaces) or on clocks. Dropping a beacon removes its file; a stale file stays until someone deletes it or a new
  beacon with the same name takes it over. Lag thresholds apply to live beacons only, so a dead reader never looks
  like a lagging one.
- **Permissions.** Creating `<dir>/readers/` needs write access to the journal directory; beacon files are created
  with mode `0644`, so an observer needs only read access. A reader without write access passes another directory,
  and the observer is pointed at it (`--readers-dir`).
- **Never influences the writer.** The writer, readers, recovery and retention never read the beacon directory.
  Retention that waits for slow readers would be a separate, explicitly designed opt-in; it is out of scope.

## API

- `status::status(dir) -> Result<Status>`: one read-only sample, from any process. Plain `#[non_exhaustive]` structs.
- `status::Sampler`: the same, but keeps the previous head (resumes the scan there, so steady-state cost is
  proportional to the bytes appended since the last sample) and reports append rates and time until full.
- `Writer::metrics() -> WriterMetrics`: head, last checkpoint (with a monotonic age, immune to clock steps),
  uncheckpointed records and bytes, poisoned flag, retention failure count. No system calls.
- `Reader::cursor() -> Position` and `Status::lag(cursor) -> Lag`: in-process reader lag. No system calls.
- `status::ReaderBeacon`: as above.

No metrics crate is a dependency. The docs show how to export a `Status` to Prometheus or OpenTelemetry in a few
lines of application code. An optional `metrics`-facade feature is possible later; it is left out because the
facade would fix metric names and types in the core crate before 1.0 for little gain over those few lines.

## Sampling cost

| Call | Files opened | System calls | Scans records? |
|---|---|---|---|
| `Writer::metrics`, `Reader::cursor`, `Status::lag` | 0 | 0 | No |
| `ReaderBeacon::update` | 0 | A comparison if the reader has not moved; else one monotonic clock read, and 1 `pwrite` at most once per interval | No |
| `status()` / `Sampler::sample()` | marker, newest segment, `writer.lock`, the beacon directory and each beacon; plus one segment per lagging beacon and one for the oldest uncovered record if the checkpoint is in an older segment | 1 `readdir` of the journal plus one `stat` per segment; marker `fstat` + 2 `pread`; `statvfs`; per beacon `flock` + `pread` | The newest segment from the checkpoint (or its start); a `Sampler` only from its previous head. Bounded by one segment |

## CLI

`tickertape status <DIR>` rather than an `inspect` flag: `inspect` is a full checksum scan whose cost grows with the
journal, while `status` is meant to run every few seconds. Read-only, no writer lock. Output `text` (default),
`json` (one object; JSONL with `--watch`), or `prometheus` (text exposition format 0.0.4, for a node_exporter
textfile collector or a cron job; `--output` replaces the file atomically). The JSON carries `"schema_version":1`;
fields may be added without a version change, while a removal or change of meaning bumps it. Health thresholds
`--max-checkpoint-age`, `--min-headroom`, `--min-headroom-segments` and `--max-reader-lag` make the command a health
check: exit 3 if one is breached (or the head is invalid), 0 if healthy, 1 for usage, I/O or a missing journal.
`--watch <INTERVAL>` repeats until interrupted and reports health in each sample instead of exiting.

## Consequences

- No format change. `docs/format.md` lists `readers/` as an advisory name outside the format, like `notify`.
- The CLI's "only `archive` changes a journal" statement still holds: `status` writes nothing into the journal, and
  `--output` must be outside it.
- Left out on purpose: automatic reader reporting; reader-aware retention; persisting the writer's retention errors
  for outside observers; one-shot append rates; a `metrics`/OpenTelemetry integration in the core crate; any
  server. Each can be added later without breaking this API.
- Tests: unit tests for every derived value and edge case in `status.rs`; cross-process tests
  (`tests/status.rs`); CLI tests for the JSON schema, Prometheus output and exit codes.
