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

**After the check passes, the reader reads again every record the marker newly covers.** The safe (`pread`) reader
validates records in a read buffer of its own, which can hold bytes it read past the cursor before the marker was
written; when the check passes it discards that buffer, so the records from its cursor up to the new marker are read
from the file after the check. The zero-copy reader reads through its mapping, which shows the file's current bytes.
This matters for a repair that keeps the file (see "In-place repairs" below), which no identity check can see.

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
- **F3. Bytes before the marker never change in place.** No tickertape process changes, in a file, a byte of a
  record below the current marker's `next_seq`: the writer only appends, recovery starts at the marker, and a file
  that stopped being current is never written again. Bytes *after* the marker can change in place: recovery zeroes
  everything after an empty commit word at or after the marker in the same file, and when storage damage emptied the
  commit word of a committed record, that removes committed records in place ("In-place repairs" below). The
  argument therefore never relies on bytes after the marker that the reader read before the marker was written.
- **F4. Identities don't come back.** A path that stops naming a file never names it again (recovery moves the old
  file into `quarantine/`, retention moves or deletes it, nothing moves files back), and while the reader holds the
  file open its inode stays allocated, so no new file can have its (device, inode).
- **F5. Records the marker newly covers are read after the check.** When the check passes, the safe reader discards
  its read buffer and the zero-copy reader reads its mapping, so every record from the cursor up to `D` is read from
  `f` after time `c` below.

**Claim.** If the reader reads a marker `D` at time `r` and then, at a time `c` after `r`, finds that its segment's
path still names its open file `f`, then `f` holds the covered record at every sequence number below `D` that it
contains, at `c` and from then on, and the reader returns exactly those. The marker was written at some time `w`
before `r`, so before `c`; at `c`, `f` is a current file; by F2 it holds the covered records; by F3 those bytes
don't change afterwards (a later marker is at or after `D`, since recovery keeps every record below the marker it
starts from); and by F5 the reader reads them after `c`. By F4 the check can't be satisfied by a different file that
happens to have `f`'s identity. The reader's cursor is never past `D` (it returns only records below the marker it
applied), and the end of a record below `D` is the start of the next record or seal in every later history of `f`,
so after an in-place repair the cursor is still on a boundary of the current history.

The interleavings, with X the reader's segment and `f` the file it has open:

| What happens | Outcome |
|---|---|
| Repair of X and the new writer's checkpoint, then the marker read | The reader reads the new marker; the check finds the repaired copy, not `f`: `Discarded`. This is the reported bug |
| Marker read (the old writer's `D1`), then the repair and the new checkpoint, then the check | The check fails: `Discarded`. Conservative: `D1` described `f` truthfully, but the reader can't tell which writer wrote the marker it read, so it applies neither |
| Marker read, check passes, then the repair | The reader applied `D1`, written while `f` was current, so `f`'s records below `D1` are covered and the repair kept them (F2); the reader returns the right records. At `D1` it reads the new marker, which moves past the cursor, and the check fails: `Discarded` |
| Two or more repairs between the marker read and the check | The path names neither `f` nor the first repair's file: `Discarded`. No repair can bring back `f`'s identity (F4) |
| A reader that already holds a repaired copy, and a second repair after it reads a marker | The same as any repair: the check compares with the file the reader holds now |
| In-place repair after the marker the reader applied (`D1`), the new writer's history appended into `f` and checkpointed (`D2`), then the marker read | The check passes: `f` is still the current file. The reader discards its buffer and reads the records from its cursor up to `D2` from `f` again: the records recovery kept, then the new history. It returned nothing past `D1`, and recovery kept everything below `D1`, so nothing it returned was removed and there is no `Discarded`. Before the buffer was discarded, the safe reader returned the removed records it had buffered (found by review) |
| **Rejected order:** check passes, then the repair and the new writer's checkpoint, then the marker read | Nothing shows that `f` was current after the new marker was written; the reader would apply it to `f` and return removed records. The claim needs the check *after* the read |

A reader that got `Discarded` keeps the marker value it last applied. After `seek` it holds a file opened after that
marker was written, which F2 covers. Recovery never writes the marker itself (F1), so there is no marker "of the
repair" to consider.

### In-place repairs

Recovery doesn't always replace a file. At an empty commit word it can zero the rest of the segment in place
(`format.md` → Recovery, step 4), and the new writer appends into the same file. After a writer *process* crash the
bytes it zeroes were never committed. But storage damage (C11) that empties the commit word of a committed record
after the checkpoint makes that record look like the end, so recovery removes it and every record after it in
place, on the same inode. Recovery could replace the file instead whenever it can tell that committed records follow
the empty word; it can't tell when only the newest record's commit word is lost, so this shape remains. (ADR
[0035](0035-recovery-preserves-a-committed-tail.md) has since made recovery usually replace the file when it finds
committed history after the empty word (crash contract C11); the in-place repair remains for damage it can't tell from a torn append,
such as the newest record's commit word zeroed, and that is the case below.) The file keeps its identity, so no
identity check can see it. What it means:

- **Durable-only readers** are unaffected, by F3 and F5: the records they return are below a marker they applied,
  which recovery keeps byte for byte, and the records after that are read again from the current bytes once a new
  marker is trusted. The cursor stays on a record boundary of the new history (see the claim), so the reader goes on
  reading it, with no `Error::Discarded` and no corruption report. A durable-only reader therefore doesn't need to
  treat "the bytes at my cursor changed while the identity didn't" as discarded history: nothing it returned was
  removed. `tests/durable_only.rs::durable_only_reader_rereads_the_tail_after_an_in_place_repair` damages the
  uncheckpointed records from the middle one on, and the newest alone (zeroing their commit words, which since ADR
  0035 is the shape that is still repaired in place), for the safe and the zero-copy reader, with the reader waiting at the
  checkpoint or stopped inside the coverage of a marker it had applied; `durable_readers_under_repairs` mixes
  in-place repairs with replacements.
- **Visible readers** can't see an in-place repair, and nothing in this decision changes that. A visible reader may
  return the removed records it had already read (or, for the safe reader, buffered), as for a replaced file (they
  were committed). It then continues at its cursor in the new history without `Error::Discarded`. If the new
  history's records line up with the old ones there, it reads them, so it moves from the removed history into the
  new one without `Discarded`. Otherwise its cursor falls inside a record of the new history, and it waits at what
  looks like an empty slot (`Ok(None)`; the identity check passes, so it never gets `Discarded`), or, once the bytes
  there are non-zero, reports `Error::Corrupt` for the new history, which is not corrupt. It would return a record
  that is not in the journal only if the new history held, at that offset, bytes forming a frame with a valid
  CRC32C and the expected sequence number. `seek` to its position puts it back on the current chain. Detecting this
  needs recovery to replace the file (or a writer generation in the format), so it is left to the work on recovery;
  `crash-contract.md` C11 and "Readers after a repair" describe it.

Storage damage to the marker itself is outside this argument: without a valid marker, recovery starts at the oldest
segment and may zero records in place that an earlier marker covered (C11).

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

**At a segment's end, with an empty successor** (found by review). The check above runs only when a marker moves
past the cursor. A reader that has read every record of segment A waits at A's seal; the writer rolls to B and commits
nothing there (a caught panic in `append_with`'s `fill` after the roll leaves exactly this), checkpoints (the marker
now names B, but its `next_seq` equals the cursor), and retention removes A, which the marker no longer protects.
The marker can't move past the cursor until the writer commits a record in B, so the reader returned `Ok(None)` on
every call for as long as the writer stayed idle, while a visible reader at the same position returns `Discarded`.
That breaks C14 (a reader racing with a removal gets `Discarded`).

So a durable-only reader waiting at the checkpoint whose marker names a later segment than the one it holds does what
a visible reader does at that slot. Every record of the held segment is then covered, so if the file is current the
cursor is at its seal: the reader checks the segment's identity and follows the seal to the successor, exactly as
`next_slot` does at a seal, but returns nothing (the records in the successor wait for a marker as before; the
successor is a file opened after the marker was read, which needs no further check). At any other slot the held file
isn't the current one, and the reader checks its identity only.

In the exact scenario the result is `Error::Discarded { seq: <cursor> }`, not a move to B:

- C14 promises `Discarded` to a reader whose segment retention removed, and a visible reader at the same position gets
  it at the same call. A durable-only reader that stalls where a visible reader is told its file is gone breaks that.
- C13 and option 5: the reader can't tell that A was removed by retention rather than moved to `quarantine/` by
  recovery, and the seal that would lead it to B was read from a file that is no longer one of the journal's current
  files. The rule above lets a marker, and so the chain it implies, apply only to a segment confirmed current after
  reading it. Following the seal of a removed file would be the unconfirmed case, even though here the marker names B
  at the cursor.
- Nothing is lost: everything the reader returned was below a marker it applied, and `seek` to the cursor lands on B's
  first record, which is where a move to B would have put it.

When A is still in place (the writer rolled and retention hasn't run), the reader moves to B at the first call and
no longer holds A; a later removal of A doesn't concern it.

The check is throttled to once per 100 ms (`IDENTITY_CHECK_INTERVAL`), the interval at which a visible reader waiting
at an empty slot checks its segment, so a durable-only reader notices the removal as quickly as a visible one. At a
seal the first check moves the reader on, so a reader keeping up with the writer makes it once per roll, never per
poll; it repeats, at most ten `stat`s a second, only while the held file shows no seal at the cursor, and a reader
whose marker names its own segment (the caught-up case) never makes it.
`tests/durable_only.rs::durable_only_reader_at_a_removed_segment_end_with_an_empty_successor_is_discarded` covers the
safe and the zero-copy reader, with A in place and removed.

The probe of the slot at the cursor reads the file, never the safe reader's buffer (found by review). The buffer can
hold a record past the checkpoint that recovery later zeroed in place (storage damage emptied its commit word), where
the new writer's roll then wrote A's seal; the marker names B at the cursor, so it never moves past it and the buffer
is never discarded. Probed from the buffer, that record hid the seal on every call: the reader never moved to B and
reported `Discarded` once retention removed A, although it had polled while A was current. Nothing else reads the
buffer while the reader waits at the checkpoint: records at the cursor are read only after a marker moves past it,
which discards the buffer first. The zero-copy reader reads the mapping, which shows the current bytes.
`tests/durable_only.rs::durable_only_reader_sees_a_seal_written_over_a_buffered_record` covers both readers.

### Cost

The check is one `stat` of the segment path, made only when a refreshed marker moves past the cursor, that is at most
once per checkpoint the reader observes, after reading the 128 KiB marker file, which costs more. A durable-only
reader that keeps up with a writer checkpointing after every record makes one per record; otherwise it is amortised
over the records a checkpoint covers. It is never on the per-record path. When the check passes, the safe reader also
discards its read buffer, so its next read is one commit-word `pread` and a buffer refill, at the same frequency.
A reader waiting at the checkpoint with a marker that names a later segment makes at most one more `stat` (and one
slot read) per 100 ms, in practice once per roll (above).

## Consequences

- C13 holds when recovery replaces a segment a durable-only reader holds, and when it removes records after the
  checkpoint in place. The rule is stated in the rustdoc of
  `ReaderConfig::durable_only` and `Reader::try_next`, crash contract C13 and C14, and `format.md` (Checkpoint
  marker; What readers do about repairs). ADRs 0010 and 0016 point here.
- New fault-injection hook points `ReaderCheckpoint` (before the marker read) and `ReaderCheckpointIdentity` (between
  the read and the check) let tests put a repair at each point of the interleaving
  (`tests/durable_only.rs`). `tests/reader_retention_stress.rs::durable_readers_under_repairs` restarts the writer
  every few checkpoints with damage injected before most restarts (a corrupt copy renamed over a segment, or
  commit words zeroed in place from a record in the newest segment on), and checks every record durable-only readers return against the repaired journal.
- The CLI's `export --durable-only` and `stats --durable-only` read the marker before they open their reader, so
  everything they read is in files opened after the marker was written. No change.
- No on-disk format change.
