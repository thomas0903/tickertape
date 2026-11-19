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

## Checking a journal

- `tickertape verify <dir>` checks every header, record and seal and exits with code 2 if anything is wrong; it is
  read-only and safe to run while the writer is active. A segment that ends in a state a running writer passes
  through (a record being written, or a segment not yet sealed whose successor exists) is scanned again with growing
  pauses, for up to 630 ms, before it is reported. Records the writer commits while `verify` looks at the end of the
  newest segment are followed, not reported (ADR 0035); a record being written at the end `verify` follows them to is
  scanned again with what is left of those 630 ms for that segment. If the writer is stalled in such a state for
  longer, `verify` can report a problem that isn't there; run it again to tell. Likewise, if the writer takes longer
  than about 50 ms to fill a single record whose payload itself contains valid tickertape frames with nearby sequence
  numbers (a journal that stores another journal's raw frames), `verify` can report that record as an empty commit
  word in front of committed records while it is being written; a second run after the record is committed reports
  nothing. With no writer running, every problem it reports is real. `verify` reads the whole newest segment,
  including the unwritten part after the last record.
- `tickertape inspect <dir>` lists segments, their sequence ranges and fill level, and the checkpoint marker.

## After a crash

- **Writer process crash:** just start the writer again. It cleans up a partly written record and continues with the
  next sequence number (C1–C4). Its `recovery_report()` lists what it did; after a plain process crash it contains at
  most a zeroed uncommitted tail, a completed seal or roll, and removed temporary files, with one exception: if the
  record being written had a payload holding a valid record frame whose sequence number falls in recovery's window
  (for example raw frames copied from another journal), or ending with the eight bytes of a valid seal for one, and
  the crash interrupted it before the body was complete (or the payload ends in more than 63 zero bytes), recovery
  cannot tell those bytes from damage (`EmptyCommitWord`, ADR 0035). The same holds for a payload holding several
  header-shaped byte sequences (copies of this journal's record headers, for example) whose length claims exhaust
  recovery's work budget although none validates (`UncheckedTail`). With the default `Quarantine` it then
  reports a `RepairedTail`, the torn record's bytes are in `quarantine/`, and nothing committed is lost. With
  `RecoveryPolicy::Fail` the writer refuses to open: look at the frame at the reported file and offset (it should
  carry the expected sequence number in its header, with the frames or headers that count as evidence inside its
  payload); if
  so, it was the record being written, so open the writer once with `RecoveryPolicy::Quarantine`, which preserves it
  and continues. If not, treat it as the storage damage below.
- **Power loss or storage errors:** the writer validates the journal from the last checkpoint. Corrupt or
  disconnected data is moved to `quarantine/` (never deleted), or, with `RecoveryPolicy::Fail`, the writer refuses to
  open so that a human can look first. That includes committed records hidden behind a zeroed commit word
  (`EmptyCommitWord`, or `UncheckedTail` when the bytes were too costly to check completely): the quarantine file
  starts at the empty word and holds them byte for byte (ADR 0035). A quarantine file of a tail stops at the last
  non-zero byte: the zeros after it are left out. To restore records from it, copy it back to the offset in its name
  and keep the rest of the segment zero (or, outside a segment, append zeros): a last record whose payload ends in
  zero bytes declares a length that reaches past the end of the copy, and it only validates with those zeros put
  back. Damage
  after which no record validates and no final seal (for one of the next 128 sequence numbers) or segment with a
  higher base than the expected sequence number remains (for example the commit words of the newest records zeroed)
  can't be told from a crash and is zeroed like a torn append, under either policy; so is a valid final seal further
  away, when nothing else is found and the search completes within its work budget. When the segment for exactly
  the expected sequence number exists and opens, that comes first: a damaged seal in front of it is rewritten (in
  place when only its checksum word is left, otherwise after the bytes after it are preserved and the segment
  replaced), and every record is kept. If
  `quarantine/` holds two files with identical bytes for the same position, a repair was interrupted and repeated;
  either copy can be deleted.
  Readers that had seen discarded records get `Error::Discarded`, except after a repair in place (damage that can't
  be told from a crash, as above, so recovery zeroes the records after the empty commit word in the same file), which
  visible readers can't notice: they may continue into the new history, wait, or report corruption, until seeked to
  their position (crash contract C11). Durable-only readers never return records that recovery discarded (C13).
- **What `Fail` still does when it refuses:** it never changes the corrupt bytes or anything after them, but the
  open is not read-only. It records its PID in the lock file, removes stale temporary files (`*.tape.tmp`,
  `next-segment.tmp`) before scanning, and keeps the clean-ups a crash needs that come before the corruption: a
  zeroed torn tail, a seal written after a crash between publishing the next segment and sealing (for example the
  seal of the segment in front of a corrupt one), a created missing successor. When the next segment exists but has
  an invalid header, recovery first zeroes in place what follows the last record of the segment in front of it (the
  seal's checksum word, or other bytes in which it finds no committed record when no segment follows that next
  one), then refuses at that next segment
  (`BrokenChain`); the zeroing stays.
- **Refused to open** (missing checkpoint segment or a gap in the chain): restore the missing segments from a copy,
  or move the affected segments aside knowingly. tickertape won't discard history on its own.

The `quarantine/` directory is never cleaned up by tickertape; inspect it and delete it yourself when you're done.
