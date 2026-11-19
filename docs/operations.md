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
its end they return `Error::Discarded`, because the file is gone; seek to their position to continue. Durable-only
readers may return it earlier, when the checkpoint moves past their position (ADR 0034); seeking to that position
then fails with `Error::SeqUnavailable`, because the records there are gone, so continue at the oldest sequence
number it reports. `tickertape tail` does both automatically, and warns on standard error with the range of records
it skipped. Readers positioned before the oldest remaining segment get `Error::SeqUnavailable` from `seek`, also when
the removal happens while `seek` runs. `Reader::open` starts at the oldest segment that exists. A removal never shows
up as a corruption error in readers, `tickertape inspect` or `tickertape verify`, and shows up as an I/O "not found"
error only if the segment a reader chose vanished on each of 8 listings in a row (crash contract C14).
A `stat` that fails other than with "not found" (`EACCES`, `EIO`) is reported as an I/O error, never taken for a
removal.

Never truncate or edit a segment in place while anything uses the journal: processes that map it can receive
`SIGBUS`.

## Monitoring

`tickertape status` and the `tickertape::status` module report what an operator needs to know about a running
recorder, cheaply and without a telemetry service (ADR [0032](decisions/0032-operational-metrics.md); the JSON and
Prometheus schemas are in [`cli.md`](cli.md)). Nothing they do slows the writer down, takes its lock or changes the
journal, and every file is opened non-blocking, so a FIFO or device in the journal can't hang them.

**What is reported is never guessed.** A value a sample can't establish is null (JSON), absent (Prometheus) or
`None` (Rust), usually with a reason. Values that are estimates are named `estimated_*`: the checkpoint's age, the
exposure's age, rolls left and time to full. A sample reads only the newest segment, so when the checkpoint is in an
older segment it can't see a missing or damaged segment in between: `head.verified` and `exposure.verified` are then
false, and the head and exposure are what the newest segment implies. A healthy verdict is then "healthy as far as
the sample can tell" (`health ok (unverified)`), not a promise that the writer would open the journal. A break the
sample does see (the head at invalid bytes, a marker the writer refuses, a segment the listing proves missing) is
reported as corruption. `tickertape verify` checks everything.

### What to alert on

| Signal | Why it matters | Suggested alert |
|---|---|---|
| **Corruption** (`tickertape_corrupt`; exit code 2) | The head is at bytes that fail validation (seen after a power loss or storage corruption, until the next writer repairs them; C8, C11), the checkpoint marker points to a missing segment or an impossible offset, or the segment listing proves a segment missing between the checkpoint and the head, so the writer refuses to open the journal | Page |
| **Exposure age** (`exposure.estimated_age`, `tickertape_exposure_estimated_age_seconds`; CLI `--max-checkpoint-age`) | Everything after the last successful checkpoint may be lost in a power loss, and its sequence numbers reused (crash contract C7, C8). The exposure age is an upper bound on how far back that loss reaches: zero when every record is covered, otherwise the checkpoint's estimated age. It is unknown (and `--max-checkpoint-age` is breached) when records are uncovered and there is no checkpoint or its time is unreliable | Warn above 3× your checkpoint interval, page above 10×; alert on `absent(tickertape_exposure_estimated_age_seconds)` too |
| **Checkpoint errors** (`Writer::metrics().poisoned`, or `checkpoint()` returning an error) | After a failed checkpoint, durability can't be guaranteed for this writer, and later checkpoints fail (C12, "Errors are sticky"). The marker then stops moving, so the exposure age grows too | Page on the first error. Only the writer process can see this directly |
| **Storage headroom** (`storage.available_bytes`, `estimated_rolls_left`; `--min-headroom`, `--min-headroom-segments`) | Writes into the active segment need no new space, but the next roll needs a whole segment; when it can't get one, `append` fails with a storage-full error (C9). **On btrfs** a full volume can kill the writer with `SIGBUS` even inside the active segment (C10), so the rolls estimate is optimistic there; on APFS a checkpoint on a full volume fails and poisons the writer. A prepared spare segment is reported (`spare_segment`) but not counted, since publishing it can still fail | Warn below a day's worth of segments at your peak rate (or use `predict_linear` on `tickertape_storage_available_bytes`), page below 2 rolls. Keep a larger margin on btrfs |
| **Reader lag** (`lag.records`/`bytes`, `unread_age`; `--max-reader-lag`) | A slow reader never slows the writer down (C6), so it can fall arbitrarily far behind. With retention, a reader in a segment that is removed gets `Error::Discarded`, and its beacon shows `lag_unknown_reason` `segment_gone`. A durable-only reader's lag is measured against the checkpoint, which it can't pass | Per consumer, from what the application tolerates. Compare `lag.bytes` (a segment-file footprint) with the bytes retention keeps to see how close a reader is to losing its segment |
| **Retention failures** (`Writer::metrics().retention_failures`, `Writer::last_retention_error()`) | Retention errors never fail `append` or `checkpoint`; they only show as a journal that keeps growing | Alert when the failure count increases. From outside: `tickertape_segments_bytes` above your retention limit for longer than a checkpoint interval |
| **Staleness of the metrics** (`tickertape_status_timestamp_seconds`, `tickertape_up`) | A sample that fails writes `tickertape_up 0`; a `tickertape status --watch` that stops (killed, hung storage) stops updating the file, which a textfile collector keeps serving | `tickertape_up == 0`, and `time() - tickertape_status_timestamp_seconds > 3 * <interval>` |
| **Writer process** (`writer.alive`) | Best effort: the PID in `writer.lock` can be reused | Use your process supervisor for this; treat it as a hint |

The checkpoint's age is estimated from the marker file's modification time and the observer's wall clock (the writer
changes the marker only at a checkpoint). It is reported as unknown when the time is detectably wrong: in the future
(a clock stepped back), earlier than the creation of the marker's segment (a copy that didn't keep times, a clock
step), or after the marker was cut short. A clock stepped forward, a later `touch`, or a copy made after the segment
was created can't be detected and make the estimate too large or too small. Inside the writer,
`Writer::metrics().since_checkpoint` uses the monotonic clock instead.

### Cost of sampling

| Call | Cost |
|---|---|
| `Writer::metrics()`, `Reader::cursor()`, `Status::lag()` | No system calls |
| `ReaderBeacon::update()` | A comparison when the reader hasn't moved; otherwise a monotonic clock read, and at most once per interval (1 s by default) one 128-byte `pwrite`, plus a 64-byte header read when the reader moved to another segment |
| `status()`, `tickertape status` | One `readdir` and one `stat` per segment; two small reads of the checkpoint marker between two `fstat`s; the header of the newest segment, and of the checkpoint's and the oldest segment if different; a `pread` scan of the newest segment from the checkpoint (at most one segment; again if the writer rolled during the sample); one record read for the oldest uncovered record when the scan didn't pass it; `statvfs`; per reader beacon an `open`, an `fstat`, one or two small reads, two `flock`s and an `lstat`, plus a segment header read and one record read for a lagging beacon |
| `Sampler::sample()`, `tickertape status --watch` | As `status()`, but the scan covers only what was appended since the previous sample |

A record read is 4 KiB, or the whole record if it is larger (it is validated before its timestamp is used). The
exact counts are in the rustdoc of `tickertape::status::status` and in ADR 0032. Nothing is added to `append` or to
a reader's read path. Checkpointing regularly keeps one-shot samples cheap, since they scan only what was appended
since the checkpoint, and lets them verify the chain.

### Prometheus without a server

A node_exporter textfile collector picks up a file that `tickertape status` keeps up to date:

```sh
# Rewrites the file atomically every 15 seconds; rates and time-to-full appear from the second sample.
tickertape status /data/journal --format prometheus --watch 15s \
    --output /var/lib/node_exporter/textfile/tickertape.prom
# Or from cron, one sample at a time:
* * * * * tickertape status /data/journal --format prometheus --output /var/lib/node_exporter/textfile/tickertape.prom
```

With `--watch`, a sample that fails replaces the file with `tickertape_up 0` and the watch goes on; a one-shot run
does the same before it exits with an error. The file must not lie inside the journal, its `quarantine/` or the beacon
directory, and `status` replaces an existing file only if it is an earlier output of `status` (in any format) or an
empty regular file; anything else, including a symlink, is refused on standard error and stops a watch with exit
code 1 (remove the file, or choose another path), so keep the timestamp alert below. `--watch` with Prometheus output needs `--output` (several expositions
in one stream are not valid). The `journal` label is the canonical path of the journal directory.

Example alert expressions:

```text
tickertape_corrupt == 1
tickertape_up == 0
time() - tickertape_status_timestamp_seconds > 60
tickertape_exposure_estimated_age_seconds > 30 or (tickertape_exposure_records > 0 unless tickertape_exposure_estimated_age_seconds)
tickertape_storage_estimated_rolls_left < 2
predict_linear(tickertape_storage_available_bytes[6h], 24 * 3600) < 0
tickertape_reader_lag_records{reader="risk-engine"} > 100000 unless on(instance, job, journal, reader) tickertape_reader_live == 0
```

The last rule counts every reader beacon except a stale one (`tickertape_reader_live` is 0 only for a beacon whose
owner died, and absent when its lock state is unknown), as `--max-reader-lag` does: an `uncertain` beacon still
counts. `tickertape_reader_live == 1` instead would silently skip `uncertain` beacons. The match keeps the scrape
identity: `instance` and `job` are the labels Prometheus attaches to every scraped series by default (for a
textfile collector, the node_exporter's), so a stale beacon on one host never suppresses a live reader's alert on
another host that uses the same journal path and reader name. If your relabelling adds or replaces identifying
labels (a `host` label, say), list those in `on(...)` too, or leave out `on(...)` to match on every label.

As a health check (systemd, Kubernetes, a load balancer script), the exit code is enough:

```sh
tickertape status /data/journal --max-checkpoint-age 30s --min-headroom-segments 4 --max-reader-lag 100000
# 0 healthy; 3 a threshold breached; 2 the journal is corrupt or the writer would refuse it; 1 I/O or usage error.
```

When a one-shot run also has an `--output` it can't write, the health verdict still decides the exit code (2 or 3, or 1
for a failed sample) and the output's failure goes to standard error; a healthy sample then exits 1.

### Exporting from your own process

The library has no metrics dependency; a few lines connect it to whatever the application uses. With the
`prometheus` crate, for example:

```rust
use prometheus::{IntGauge, register_int_gauge};

let exposure = register_int_gauge!("tickertape_exposure_records", "Records not yet durable")?;
let rolls_left = register_int_gauge!("tickertape_storage_estimated_rolls_left", "Segment rolls that still fit")?;
let poisoned = register_int_gauge!("tickertape_writer_poisoned", "1 after a failed checkpoint")?;
let mut sampler = tickertape::status::Sampler::new("/data/journal");
// On each scrape or timer tick:
let status = sampler.sample()?;
if let Some(records) = status.exposure.records {
    exposure.set(records as i64);
}
rolls_left.set(status.storage.estimated_rolls_left as i64);
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
        if let Some(records) = tickertape::status::status(&dir).ok().and_then(|s| s.exposure.records) {
            gauge.observe(records, &[]);
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

The beacon is `<dir>/readers/risk-engine` (128 bytes); it disappears when dropped. A reader that crashes leaves its
file behind, but `status` reports it as `stale` (the beacon's `flock` died with the process) and never counts it as
lag; delete the file when you no longer want to see it, or create a beacon with the same name, which takes it over.
Creating a beacon never replaces anything but a stale beacon (a valid one, or one whose last update was cut short
when its owner died): a name held by a live reader fails with `Error::BeaconInUse`, and any other file under the
name is left alone. Names that are journal files' names, in any letter case, are refused. Beacons never influence
the writer or retention.

- **Permissions.** Beacon files are created with the reader's umask (mode `0666` before the umask). An observer
  needs read access to the beacon directory and the files; a beacon it can't read is reported as `unreadable`, and a
  directory it can't list as unusable, without failing the sample. A reader of **another user's journal**, which
  can't write to its directory, creates its beacon with `ReaderBeacon::create_in` in a directory of its own, and the
  observer is pointed at it with `--readers-dir` (or `Sampler::readers_dir`).
- **Identity.** A beacon records the identity of the segment file its cursor is in, and an observer computes lag only
  against that same file. Beacons of different journals can share a directory: a beacon of another journal shows a
  `lag_unknown_reason` of `other_history`, and so does a reader still in a segment that recovery repaired or a
  journal replaced at the same path.
- **`fork`.** The lock belongs to the open file: a child created with `fork` that keeps the beacon's descriptor
  keeps the beacon `live` after its creator dies, until the child exits. The descriptor is close-on-exec, so `exec`
  drops it, but every child holds it from `fork` to `exec`: while another thread spawns a process, a beacon dropped
  at that moment can still look live (and a creator of the same name can get `BeaconInUse`) for that instant. A
  child's copy of a `ReaderBeacon` never writes or removes the file: `update` fails in the child, and dropping the
  copy leaves the parent's beacon in place.
- **Filesystems.** Beacons, like journals, are for local filesystems. On network filesystems `flock` may be
  unsupported (observers then report `uncertain`, and lag thresholds apply as for live beacons) or emulated (on Linux
  NFS, an observer in the owner's own process can release the owner's lock).

## Checking a journal

- `tickertape verify <dir>` checks every header, record and seal and exits with code 2 if anything is wrong; it is
  read-only and safe to run while the writer is active. A segment that ends in a state a running writer passes
  through (a record being written, or a segment not yet sealed whose successor exists) is scanned again with growing
  pauses, for up to 630 ms, before it is reported. If the writer is stalled in such a state for longer, `verify` can
  report a problem that isn't there; run it again to tell. With no writer running, every problem it reports is real.
- `tickertape inspect <dir>` lists segments, their sequence ranges and fill level, and the checkpoint marker.
- `tickertape status <dir>` is the cheap, frequent check: head, checkpoint age, exposure, headroom and reader lag
  (see "Monitoring"). Unlike `inspect` and `verify`, it does not scan the whole journal.

## After a crash

- **Writer process crash:** just start the writer again. It cleans up a partly written record and continues with the
  next sequence number (C1–C4). Its `recovery_report()` lists what it did; after a plain process crash it contains at
  most a zeroed uncommitted tail, a completed seal or roll, and removed temporary files.
- **Power loss or storage errors:** the writer validates the journal from the last checkpoint. Corrupt or
  disconnected data is moved to `quarantine/` (never deleted), or, with `RecoveryPolicy::Fail`, the writer refuses to
  open so that a human can look first. Readers that had seen discarded records get `Error::Discarded`, except after a
repair in place (damage that emptied a commit word, so recovery zeroes the records after it in the same file), which
visible readers can't notice: they may continue into the new history, wait, or report corruption, until seeked to
their position (crash contract C11). Durable-only readers never return records that recovery discarded (C13).
- **Refused to open** (missing checkpoint segment or a gap in the chain): restore the missing segments from a copy,
  or move the affected segments aside knowingly. tickertape won't discard history on its own.

The `quarantine/` directory is never cleaned up by tickertape; inspect it and delete it yourself when you're done.
