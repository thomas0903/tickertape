# Operating a tickertape journal

Practical guidance for running a writer in production-like settings. The guarantees themselves are in
[`crash-contract.md`](crash-contract.md); measured costs are in [`benchmarks.md`](benchmarks.md). This page only
draws conclusions from those two.

## Choosing a segment size

`WriterConfig::segment_size` (default 128 MiB; a multiple of 64 KiB) trades these against each other:

- **Rolls.** The append that fills a segment publishes the next one: write and sync its header, then link it. With
  the default `prefault(true)`, the next segment has been preallocated in the background (see "Writer threads");
  otherwise the roll also preallocates and syncs a whole new file. Either way that append takes milliseconds
  instead of microseconds (see "roll appends" in `benchmarks.md`). Larger segments roll less often.
- **Largest record.** A record must fit in one segment: at most `segment_size - 112` bytes of payload.
- **Disk reservation.** Every segment reserves its full size on disk when it is created, even if it is mostly empty.
- **Open time.** When the writer opens, it reads the part of the newest segment after the last record to clean up
  after a crash, so very large segments make opening slower.
- **Granularity of archiving.** You archive or delete whole segments (below).

For most uses the default is a reasonable start. Pick smaller segments (a few MiB) for low-volume journals you
archive often, larger ones if roll latency matters and disk space doesn't.

## Writer threads

With `WriterConfig::prefault(true)` (the default), the writer moves segment work off its own thread:

- **Page faults (Linux and macOS).** A helper thread, `tickertape-prefault`, keeps the pages just ahead of the write
  position faulted in, so `append` rarely waits for a page fault.
  - **Linux:** the next 256 KiB, with `MADV_POPULATE_WRITE` (ADR 0025; kernels before 5.14 stop the helper on first
    use).
  - **macOS:** the next 8 MiB, with `MADV_WILLNEED` (ADR 0020). On APFS, writing into a preallocated segment otherwise
    faults once per 16 KiB page, slowly.
- **The next segment (macOS only).** Once the active segment is half full, a short-lived thread, `tickertape-spare`,
  creates, preallocates and syncs `next-segment.tmp` (ADR 0022). The roll then only writes and syncs the header and
  links the file into place; preallocating and syncing a whole segment on APFS takes about 50 ms. The spare
  reserves one more segment's worth of disk space from that point on. On Linux a synchronous roll costs about a
  millisecond on ext4, and the spare occasionally stalled the writer, so Linux doesn't use it (ADR 0023).

Neither thread ever writes records. Set `prefault(false)` if you pin threads and want a writer that starts none.

**Sustained high byte rates on Linux.** Under steady write-back (minutes of writing, gigabytes of dirty pages), ext4
itself occasionally stalls the writer for milliseconds, with or without prefaulting (ADR 0025). Plan disk
throughput with headroom: a small cloud VM stalled for hundreds of milliseconds at about 100 MB/s sustained.

## Choosing a checkpoint interval

`Writer::checkpoint()` makes everything appended so far durable. Until then, records are visible and survive the
writer process crashing, but not a power loss (crash contract C1, C7, C8).

- **Cost.** A checkpoint takes milliseconds (see `benchmarks.md`; on macOS it ends with two `F_FULLFSYNC` calls).
  Don't checkpoint after every record unless you need that and can afford it.
- **Exposure.** After a power loss, everything since the last successful checkpoint may be gone, and its sequence
  numbers will be reused for new records.
- **Durable-only readers** (`ReaderConfig::durable_only`, `--durable-only`) see records only once a checkpoint
  covers them, so they lag the writer by up to one checkpoint interval.
- **Recovery time.** When the writer opens, it validates everything written since the last checkpoint; a journal
  that is never checkpointed is validated from the start every time.

A common pattern is a time- or count-based checkpoint (for example every second or every N records, whichever comes
first) plus one before a clean shutdown. Dropping a `Writer` does not checkpoint.

If `checkpoint()` returns an error, the writer refuses all later checkpoints (`Error::DurabilityLost`) while appends
keep working. Treat it as an incident: see "Errors are sticky" in the crash contract.

## Disk space

Segments are never deleted automatically unless a writer retention policy is configured. Without retention or
`tickertape archive`, disk use only grows. Retention is best effort and depends on checkpoints: a writer that never
checkpoints never frees space, and the checkpoint segment and newer segments always remain.

- Monitor free space on the journal's filesystem. A failed roll returns an error for which
  `Error::is_storage_full()` is true; the journal stays consistent and the append can be retried once space is back.
- **btrfs:** on a full btrfs volume, writes into an already preallocated segment can end the writer process with
  `SIGBUS` (or block it, with copy-on-write disabled), measured in crash contract C10. Keep clear headroom there.
- **APFS:** a `checkpoint()` on a completely full volume fails with `ENOSPC`, which poisons the writer (C9).

## Archiving and deleting old segments

Use `tickertape archive` for an explicit run, or configure a writer retention policy for long-running recorders:

```sh
# Inspect the plan, then move old segments into an existing archive directory.
tickertape archive ./journal --to ./archive --keep-segments 10 --dry-run
tickertape archive ./journal --to ./archive --keep-segments 10
# Or permanently delete a sealed prefix older than 30 days.
tickertape archive ./journal --delete --older-than 30d
```

```rust
use tickertape::{WriterConfig, retention::{Action, Policy}};
let config = WriterConfig::default().retention(
    Policy::default().max_segments(10).max_bytes(1024 * 1024 * 1024),
    Action::Delete,
);
```

The writer runs retention once after recovery on open and after every successful checkpoint. Retention errors
never fail open, append or checkpoint and never poison the writer. Check `Writer::last_retention_error()` for the
most recent error (retained even after later successes), and `Writer::retention_report()` for cumulative counts,
including removals completed before a subsequent failure. Neither is part of the recovery report.

The same rules apply to the library and CLI, without taking the writer lock:

- Remove **from the oldest end only**, stopping at the first ineligible segment or failure. Never skip a file or
  remove a middle segment, since that leaves a gap in the chain.
- Only sealed segments **strictly older than the checkpoint segment** qualify. Without a valid checkpoint marker
  nothing is eligible; a dangling marker is an error. Never remove the marker's segment or anything newer.
- Limits combine with OR: keep at most N total segments, at most B total segment file bytes, or remove segments
  sealed longer ago than D. Seal time is the successor header's creation time, not file modification time.
  Counts and bytes include the active segment. Checkpoint eligibility can prevent reaching the requested limit.
- Moves preserve the segment basename and require an existing destination directory. Same-filesystem moves use
  an atomic no-replace rename. `EXDEV` selects copy, sync the copy, sync the destination directory, remove the
  source, sync the journal directory. Existing files (including symlinks and identical copies) are errors.

An interrupted cross-filesystem move can leave an original, a copy, or both; a partial copy never permits source
removal. On retry, an existing destination remains a conflict. Inspect it and explicitly resolve the conflict
(or choose a fresh destination) before rerunning; tickertape never overwrites it. Completed earlier removals stay
in effect if a later segment fails. A concurrent archiver's missing segment is treated as already removed only
when it was at the oldest end. `retention::apply` revalidates plans against the current journal before removal.
Segments are ordinary files named after their first sequence number (`00000000000000001234.tape`); you can also
copy sealed segments without changing the journal.

Readers that are still inside a segment you delete keep reading it (they hold the file open), but when they reach
its end they return `Error::Discarded`, because the file is gone; seek to their position to continue
(`tickertape tail` does this automatically). Readers positioned before the oldest remaining segment get
`Error::SeqUnavailable` from `seek`. `Reader::open` starts at the oldest segment that exists.

Never truncate or edit a segment in place while anything uses the journal: processes that map it can receive
`SIGBUS`.

## Monitoring

`tickertape status` and the `tickertape::status` module report what an operator needs to know about a running
recorder, cheaply and without a telemetry service (ADR [0032](decisions/0032-operational-metrics.md); the JSON and
Prometheus schemas are in [`cli.md`](cli.md)). Nothing they do slows the writer down, takes its lock or changes the
journal.

### What to alert on

| Signal | Why it matters | Suggested alert |
|---|---|---|
| **Exposure age** (`exposure.age`, `tickertape_exposure_age_seconds`; CLI `--max-checkpoint-age`) | Everything after the last successful checkpoint may be lost in a power loss, and its sequence numbers reused (crash contract C7, C8). The exposure age is an upper bound on how far back that loss reaches: zero when every record is covered, otherwise the checkpoint's age | Warn above 3× your checkpoint interval, page above 10×. Records with no checkpoint at all are always exposed |
| **Checkpoint errors** (`Writer::metrics().poisoned`, or `checkpoint()` returning an error) | After a failed checkpoint, durability can't be guaranteed for this writer, and later checkpoints fail (C12, "Errors are sticky"). The marker then stops moving, so the exposure age grows too | Page on the first error. Only the writer process can see this directly |
| **Storage headroom** (`storage.rolls_left`, `available_bytes`; `--min-headroom-segments`, `--min-headroom`) | Writes into the active segment need no new space, but the next roll needs a whole segment; when it can't get one, `append` fails with a storage-full error (C9). On btrfs a full volume can kill the writer with `SIGBUS` (C10); on APFS a checkpoint on a full volume fails and poisons the writer | Warn below a day's worth of segments at your peak rate (or use `predict_linear` on `tickertape_storage_available_bytes`), page below 2 rolls. Keep a larger margin on btrfs |
| **Reader lag** (`lag.records`/`bytes`, `unread_age`; `--max-reader-lag`) | A slow reader never slows the writer down (C6), so it can fall arbitrarily far behind. With retention, a reader in a segment that is removed gets `Error::Discarded` | Per consumer, from what the application tolerates. Compare `lag.bytes` with the bytes retention keeps to see how close a reader is to losing its segment |
| **Retention failures** (`Writer::metrics().retention_failures`, `Writer::last_retention_error()`) | Retention errors never fail `append` or `checkpoint`; they only show as a journal that keeps growing | Alert when the failure count increases. From outside: `tickertape_segments_bytes` above your retention limit for longer than a checkpoint interval |
| **Head invalid** (`HeadEnd::Invalid`, `tickertape_head_invalid`) | Bytes at the end of the journal fail validation: seen after a power loss or storage corruption, until the next writer repairs them (C8, C11) | Page |
| **Writer process** (`writer.alive`) | Best effort: the PID in `writer.lock` can be reused | Use your process supervisor for this; treat it as a hint |

Checkpoint age comes from the marker file's modification time and the observer's wall clock: a clock stepped back
makes it too small (reported as `clock_skew`), a clock stepped forward too large until the next checkpoint, and
copying a journal without keeping times resets it. Inside the writer, `Writer::metrics().since_checkpoint` uses the
monotonic clock instead.

### Cost of sampling

| Call | Cost |
|---|---|
| `Writer::metrics()`, `Reader::cursor()`, `Status::lag()` | No system calls |
| `ReaderBeacon::update()` | A comparison when the reader hasn't moved; otherwise a monotonic clock read, and one 64-byte `pwrite` at most once per interval (1 s by default) |
| `status()`, `tickertape status` | One `readdir` and one `stat` per segment; two small reads of the checkpoint marker; the header of the oldest and newest segment; a `pread` scan of the newest segment from the checkpoint (at most one segment); `statvfs`; a few small reads per reader beacon |
| `Sampler::sample()`, `tickertape status --watch` | As `status()`, but the scan covers only what was appended since the previous sample |

Nothing is added to `append` or to a reader's read path. Checkpointing regularly keeps one-shot samples cheap,
since they scan only what was appended since the checkpoint.

### Prometheus without a server

A node_exporter textfile collector picks up a file that `tickertape status` keeps up to date:

```sh
# Rewrites the file atomically every 15 seconds; rates and time-to-full appear from the second sample.
tickertape status /data/journal --format prometheus --watch 15s \
    --output /var/lib/node_exporter/textfile/tickertape.prom
# Or from cron, one sample at a time:
* * * * * tickertape status /data/journal --format prometheus --output /var/lib/node_exporter/textfile/tickertape.prom
```

Example alert expressions:

```text
tickertape_exposure_age_seconds > 30
tickertape_storage_rolls_left < 2
predict_linear(tickertape_storage_available_bytes[6h], 24 * 3600) < 0
tickertape_reader_lag_records{reader="risk-engine"} > 100000 and tickertape_reader_live == 1
```

As a health check (systemd, Kubernetes, a load balancer script), the exit code is enough:

```sh
tickertape status /data/journal --max-checkpoint-age 30s --min-headroom-segments 4 --max-reader-lag 100000
# 0 healthy, 3 a threshold breached, 1 I/O or usage error, 2 a damaged newest segment header.
```

### Exporting from your own process

The library has no metrics dependency; a few lines connect it to whatever the application uses. With the
`prometheus` crate, for example:

```rust
use prometheus::{IntGauge, register_int_gauge};

let exposure = register_int_gauge!("tickertape_exposure_records", "Records not yet durable")?;
let rolls_left = register_int_gauge!("tickertape_storage_rolls_left", "Segment rolls that still fit")?;
let poisoned = register_int_gauge!("tickertape_writer_poisoned", "1 after a failed checkpoint")?;
let mut sampler = tickertape::status::Sampler::new("/data/journal");
// On each scrape or timer tick:
let status = sampler.sample()?;
exposure.set(status.exposure.records as i64);
rolls_left.set(status.storage.rolls_left as i64);
poisoned.set(i64::from(writer.metrics().poisoned)); // in the writer's process
```

With OpenTelemetry, register observable gauges whose callbacks read a `Status` the same way (the builder names vary
between `opentelemetry` versions):

```rust
let meter = opentelemetry::global::meter("tickertape");
let dir = std::path::PathBuf::from("/data/journal");
let _exposure = meter
    .u64_observable_gauge("tickertape.exposure.records")
    .with_callback(move |gauge| {
        if let Ok(status) = tickertape::status::status(&dir) {
            gauge.observe(status.exposure.records, &[]);
        }
    })
    .build();
```

### Making readers in other processes visible

Readers leave no trace on disk by default, so an observer can't see their position. A reader that should be
monitored opts in with a beacon:

```rust
use tickertape::status::ReaderBeacon;

let mut beacon = ReaderBeacon::create("/data/journal", "risk-engine", &reader)?;
loop {
    if let Some(record) = reader.next_timeout(Some(std::time::Duration::from_secs(1)))? {
        // ... handle the record ...
    }
    beacon.update(&reader)?; // writes at most once a second, only when the position moved
}
```

The beacon is `<dir>/readers/risk-engine`; it disappears when dropped. A reader that crashes leaves its file behind,
but `status` reports it as `stale` (the beacon's `flock` died with the process) and never counts it as lag; delete
the file when you no longer want to see it. Readers that can't write to the journal directory use
`ReaderBeacon::create_in` with another directory, and the observer reads it with `--readers-dir`. Beacons never
influence the writer or retention.

## Checking a journal

- `tickertape verify <dir>` checks every header, record and seal and exits with code 2 if anything is wrong; it is
  read-only and safe to run while the writer is active.
- `tickertape inspect <dir>` lists segments, their sequence ranges and fill level, and the checkpoint marker.
- `tickertape status <dir>` is the cheap, frequent check: head, checkpoint age, exposure, headroom and reader lag
  (see "Monitoring"). Unlike `inspect` and `verify`, it does not scan the whole journal.

## After a crash

- **Writer process crash:** just start the writer again. It cleans up a partly written record and continues with the
  next sequence number (C1–C4). Its `recovery_report()` lists what it did; after a plain process crash it contains at
  most a zeroed uncommitted tail, a completed seal or roll, and removed temporary files.
- **Power loss or storage errors:** the writer validates the journal from the last checkpoint. Corrupt or
  disconnected data is moved to `quarantine/` (never deleted), or, with `RecoveryPolicy::Fail`, the writer refuses to
  open so that a human can look first. Readers that had seen discarded records get `Error::Discarded`.
- **Refused to open** (missing checkpoint segment or a gap in the chain): restore the missing segments from a copy,
  or move the affected segments aside knowingly. tickertape won't discard history on its own.

The `quarantine/` directory is never cleaned up by tickertape; inspect it and delete it yourself when you're done.
