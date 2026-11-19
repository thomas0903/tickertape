# 0034: Durable-only reads check the segment's identity when the checkpoint moves

- **Status:** proposed (accepted when the pull request that adds it is merged)
- **Date:** 2026-11-19
- **Amends:** [0010](0010-durable-only-reads.md) (durable-only reads) and, for durable-only readers, the "Readers"
  paragraph of [0016](0016-retention-and-archiving.md) (retention)

## Context

ADR 0010 lets a durable-only reader return the record with sequence number `s` only if `s < D`, where `D` is the
`next_seq` of the current checkpoint marker. The reader reads `D` from the marker file, but it reads records from the
segment file it has open. Its safety argument was that the marker is written after everything it covers is durable
and that recovery never modifies records before the marker. That is true of the journal's **current** files, but the
file a reader has open may have stopped being one of them. A soak test found the result, more than a thousand times
per 30-minute run on macOS and on Linux:

1. A durable-only reader waits at the checkpoint position inside segment X, with X open.
2. The writer dies. Recovery finds a corrupt record after the checkpoint and repairs X by replacement: a copy with the
   tail zeroed is renamed over X, and the tail goes to `quarantine/` (`format.md` → Recovery).
3. The new writer appends different records at those sequence numbers and checkpoints.
4. The reader re-reads the marker, sees a higher `next_seq`, and goes on reading the old X that it still has open. It
   returns the removed records as if they were durable, and only gets `Error::Discarded` at the old file's seal.

This needs the reader's open file to hold valid bytes where the file recovery read was corrupt. The soak produced
that with corrupt copies renamed over segments between restarts; in the field it can come from, for example, a
reader's copy of a page that outlived a storage failure (C11). It breaks crash contract C13.

Visible readers have the same shape (they read on in a replaced file until they notice), but within their promise:
everything they can read from the old file was committed before the crash, and `format.md` → "What readers do about
repairs" already says a reader may return records of a replaced file until it notices (on a validation error, after
waiting 100 ms at an empty slot, or at the next seal), like records lost to a power loss that it had already seen
(C8). The old file holds no records written after the crash, so a visible reader can't be kept in it for long.
Visible readers don't read the marker and are unchanged.

## Options considered

1. **Check the segment's identity before every record.** One `stat` per record on the hot path. Rejected.
2. **Check the segment's identity when a marker moves past the cursor, after reading the marker.** One `stat` per
   marker refresh that unblocks the reader, never per record. Chosen.
3. **The same check before reading the marker.** Unsound: a repair and the new writer's checkpoint can land between
   the check and the read (see the ordering argument below).
4. **A writer epoch in the marker.** A format change, and the reader still couldn't tell which epoch its open file
   belongs to without an epoch in segment headers too. Rejected.
5. **Report only replacement; keep reading a segment that was removed.** That would keep the old behaviour for
   readers overtaken by retention. Unsound: a path that names no file is also a state recovery leaves (it quarantines
   a disconnected segment, the new writer later creates a different segment with the same base, and retention can
   later remove that one too), so a reader can't tell a retention removal from what a repair left behind. Rejected.

## Decision

**The rule.** A checkpoint marker speaks about the journal's current files. A durable-only reader may apply a marker
it has just read only to a segment whose identity it confirmed after reading that marker.

Concretely, when a refreshed marker's `next_seq` is greater than the reader's cursor, the reader checks that its
segment's path still names the file it has open (same device and inode). If it doesn't (recovery replaced the
segment, or retention or recovery removed it), the reader returns `Error::Discarded { seq: <cursor> }` without
applying the marker. As for any `Discarded`, the application calls `seek` to continue on the current chain. A marker
that doesn't move past the cursor lets the reader read nothing, so it needs no check. Files the reader opens after
reading a marker (`Reader::open`, `seek`, the successor at a seal) need no extra check: they were current after the
marker was written, which is what the check establishes for the file already open.

### The ordering argument

Facts the argument uses:

- **F1. Repairs come before checkpoints.** Recovery replaces and quarantines files inside `Writer::open`, persists the
  result, and only then does the new writer append. Only `Writer::checkpoint` writes the marker. So any marker written
  after a repair was written after that repair's files were in place.
- **F2. Covered records stay in the current files.** From the moment a marker with `next_seq = D` is written, every
  file that a segment path names holds, at each sequence number below `D` it contains, exactly the record the marker
  covered. The writer only appends; recovery starts at the marker and never changes a record before it (a repaired
  copy keeps everything before the repair point byte for byte, and the repair point is at or after the marker);
  retention removes whole segments; a segment created after the marker begins at or after `D`.
- **F3. Committed bytes never change, and identities don't come back.** No tickertape process modifies committed
  bytes in a file, so what a reader reads from its open file at any time is what that file held when it was
  current. A path that stops naming a file never names it again (recovery moves the old file into `quarantine/`,
  retention moves or deletes it, nothing moves files back), and while the reader holds the file open its inode stays
  allocated, so no new file can have its (device, inode).

**Claim.** If the reader reads a marker `D` at time `r` and then, at a time `c` after `r`, finds that its segment's
path still names its open file `f`, then `f` holds the covered record at every sequence number below `D` that it
contains. The marker was written at some time `w` before `r`, so before `c`; at `c`, `f` is a current file; by F2 it
holds the covered records, and by F3 the reader reads exactly those, then or later. By F3 the check can't be
satisfied by a different file that happens to have `f`'s identity.

The interleavings, with X the reader's segment and `f` the file it has open:

| What happens | Outcome |
|---|---|
| Repair of X and the new writer's checkpoint, then the marker read | The reader reads the new marker; the check finds the repaired copy, not `f`: `Discarded`. This is the reported bug |
| Marker read (the old writer's `D1`), then the repair and the new checkpoint, then the check | The check fails: `Discarded`. Conservative: `D1` described `f` truthfully, but the reader can't tell which writer wrote the marker it read, so it applies neither |
| Marker read, check passes, then the repair | The reader applied `D1`, written while `f` was current, so `f`'s records below `D1` are covered and the repair kept them (F2); the reader returns the right records. At `D1` it reads the new marker, which moves past the cursor, and the check fails: `Discarded` |
| Two or more repairs between the marker read and the check | The path names neither `f` nor the first repair's file: `Discarded`. No repair can bring back `f`'s identity (F3) |
| A reader that already holds a repaired copy, and a second repair after it reads a marker | The same as any repair: the check compares with the file the reader holds now |
| **Rejected order:** check passes, then the repair and the new writer's checkpoint, then the marker read | Nothing shows that `f` was current after the new marker was written; the reader would apply it to `f` and return removed records. The claim needs the check *after* the read |

A reader that got `Discarded` keeps the marker value it last applied. After `seek` it holds a file opened after that
marker was written, which F2 covers. Recovery never writes the marker itself (F1), so there is no marker "of the
repair" to consider.

### Readers overtaken by retention

This changes one behaviour. Before, a durable-only reader whose segment retention removed kept reading the open file
to its end and got `Discarded` at the seal, like any reader (ADR 0016). Now, if it reaches its cached `D` inside that
segment and the refreshed marker moves past its cursor, the check fails and it gets `Discarded` there, at its cursor.
A durable-only reader whose cached `D` already lies beyond the removed segment still reads on to the seal, and visible
readers are unchanged.

This is within what ADR 0016 and crash contract C14 promise. C14 promises that a reader racing with a removal gets
`Error::Discarded` from reads and `Error::SeqUnavailable` from `seek`, never a corruption or I/O error: that still
holds, and the application handles the earlier `Discarded` exactly as the later one (`seek`, which reports the oldest
retained record). The crash contract says history removed by retention "is no longer part of the journal". The
records the reader no longer gets were durable and correct, but removed; whether a reader could still get them from
an open file always depended on timing (a reader slightly further behind gets `SeqUnavailable` for the same records),
so nothing could rely on it. ADR 0016's sentence that readers "keep reading it ... and get `Error::Discarded` at its
end" described what holding the file allows; for durable-only readers it now reads "at its end, or earlier, when the
checkpoint moves past their cursor". A durable-only reader can't keep the old behaviour safely, because it can't tell
a retention removal from a repair (option 5).

### Cost

The check is one `stat` of the segment path, made only when a refreshed marker moves past the cursor, that is at most
once per checkpoint the reader observes, after reading the 128 KiB marker file, which costs more. A durable-only
reader that keeps up with a writer checkpointing after every record makes one per record; otherwise it is amortised
over the records a checkpoint covers. It is never on the per-record path.

## Consequences

- C13 holds when recovery replaces a segment a durable-only reader holds. The rule is stated in the rustdoc of
  `ReaderConfig::durable_only` and `Reader::try_next`, crash contract C13 and C14, and `format.md` (Checkpoint
  marker; What readers do about repairs). ADRs 0010 and 0016 point here.
- New fault-injection hook points `ReaderCheckpoint` (before the marker read) and `ReaderCheckpointIdentity` (between
  the read and the check) let tests put a repair at each point of the interleaving
  (`tests/durable_only.rs`). `tests/reader_retention_stress.rs::durable_readers_under_repairs` restarts the writer
  every few checkpoints with corruption injected before most restarts, and checks every record durable-only readers
  return against the repaired journal.
- The CLI's `export --durable-only` and `stats --durable-only` read the marker before they open their reader, so
  everything they read is in files opened after the marker was written. No change.
- No on-disk format change.
