# Operating a tickertape journal

Practical guidance for running a writer in production-like settings. The guarantees themselves are in
[`crash-contract.md`](crash-contract.md); measured costs are in [`benchmarks.md`](benchmarks.md). This page only
draws conclusions from those two.

## Choosing a segment size

`WriterConfig::segment_size` (default 128 MiB; a multiple of 64 KiB) trades these against each other:

- **Rolls.** The append that fills a segment creates the next one: preallocate, write the header, sync, link. That
  append takes milliseconds instead of microseconds (see "roll appends" in `benchmarks.md`). Larger segments roll
  less often.
- **Largest record.** A record must fit in one segment: at most `segment_size - 112` bytes of payload.
- **Disk reservation.** Every segment reserves its full size on disk when it is created, even if it is mostly empty.
- **Open time.** When the writer opens, it reads the part of the newest segment after the last record to clean up
  after a crash, so very large segments make opening slower.
- **Granularity of archiving.** You archive or delete whole segments (below).

For most uses the default is a reasonable start. Pick smaller segments (a few MiB) for low-volume journals you
archive often, larger ones if roll latency matters and disk space doesn't.

## Writer threads

By default the writer starts one helper thread, `tickertape-prefault`, that keeps the next 8 MiB of the active segment
faulted in (`madvise`), so `append` rarely waits for a page fault (ADR 0020). It never writes to the journal. On APFS
this matters most: writing into a preallocated segment otherwise faults once per 16 KiB page, slowly. Set
`WriterConfig::prefault(false)` if you pin threads and want the writer to start none; it then works exactly as
before, faulting pages on first write. On Linux it uses `MADV_POPULATE_WRITE` (5.14 and later; older kernels stop
the thread on first use).

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

tickertape never deletes segments, so disk use only grows until you archive.

- Monitor free space on the journal's filesystem. A failed roll returns an error for which
  `Error::is_storage_full()` is true; the journal stays consistent and the append can be retried once space is back.
- **btrfs:** on a full btrfs volume, writes into an already preallocated segment can end the writer process with
  `SIGBUS` (or block it, with copy-on-write disabled), measured in crash contract C10. Keep clear headroom there.
- **APFS:** a `checkpoint()` on a completely full volume fails with `ENOSPC`, which poisons the writer (C9).

## Archiving and deleting old segments

Segments are ordinary files named after the first sequence number they hold (`00000000000000001234.tape`). You can
copy them anywhere at any time; a copy of a sealed segment is complete and immutable.

To free space, delete or move segments **from the oldest end only**, and only:

- segments older than the one named in the checkpoint marker (`tickertape inspect` shows it), because the writer
  refuses to open if the marker's segment is missing; and
- never a segment in the middle of the chain: a writer opening without a checkpoint refuses a journal with a gap.

Readers that are still inside a segment you delete keep reading it (they hold the file open), but when they reach
its end they return `Error::Discarded`, because the file is gone; seek to their position to continue
(`tickertape tail` does this automatically). Readers positioned before the oldest remaining segment get
`Error::SeqUnavailable` from `seek`. `Reader::open` starts at the oldest segment that exists.

Never truncate or edit a segment in place while anything uses the journal: processes that map it can receive
`SIGBUS`.

## Checking a journal

- `tickertape verify <dir>` checks every header, record and seal and exits with code 2 if anything is wrong; it is
  read-only and safe to run while the writer is active.
- `tickertape inspect <dir>` lists segments, their sequence ranges and fill level, and the checkpoint marker.

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
