# 0046: Seeks that need no index

- **Status:** accepted (under the owner's delegation, 2026-11-25)
- **Date:** 2026-11-25
- **Relates to:** [0034](0034-durable-only-reads-check-segment-identity.md) (its facts F1 to F5 are used here),
  [0042](0042-replay-and-recovery-at-scale.md) (the sparse index, which this does not decide)

## Context

Issue #59. `Reader::seek` reads and validates a segment from its first record to the target, also when the target is
ahead of the reader in the segment it already has open. There is no way to start at the end of a journal, so
`tickertape tail --from-end` read every retained segment. The pure-Python decoder read every segment whatever
`start_seq` was. None of this needs an index or a format change.

The difficulty is the first point. `seek` is how an application gets a reader back onto the current chain after a
repair, and recovery can change the file a reader has open without changing its identity: it zeroes records in place
after an empty commit word, and the next writer appends a different history there (`format.md` → What readers do
about repairs). A visible reader can't notice, so its cursor may not be a record boundary any more, and `seek` to its
position is the documented way out. A seek that simply went on from the cursor would break that.

## Options considered

1. **Always continue from the cursor if the file's identity is unchanged.** Breaks the recovery described above.
2. **Decide at the seek, from the current marker, the identity and the record before the cursor.** No state. Was the
   first version of this change; review found that it is not enough: a marker read *after* the reader passed its
   records says nothing about their offsets, and the record before the cursor can be imitated by bytes in a payload of
   the new history. The reader then stays off the chain however often it seeks to its position. It also reads the
   marker at every seek.
3. **Continue only where the records passed were covered by a marker read before them** (chosen). One number in the
   reader, written only by seeks; nothing on the per-record path. A live reader ahead of the checkpoint gains nothing
   until its next scan.

## Decision

**Forward seeks.** `seek` lists the directory, chooses the segment and opens it as before, with the same errors. It
starts at the cursor instead of the segment's first record if all of these hold, and otherwise at the first record:

- the target is at or after the cursor, and the segment opened is the file the reader has open (device and inode);
- the cursor is at or below `seek_covered`;
- the record before the cursor, read from the file now, is valid, has the sequence number before the cursor's and
  ends at the cursor.

`seek_covered` is the `next_seq` of the marker, read after the segment was opened, and kept only if the path still
names that file after the marker was read (0 otherwise). A seek that starts at the segment's first record sets it, as
does one that continues to a target at or past it (to extend it). `seek_to_end` sets it to the marker it starts from.
A failed seek leaves it alone.

*Why that is enough.* This is ADR 0034's claim. Where the number was set, the cursor was a record boundary of the
current chain (a segment's first record, the marker's position, or a cursor trusted by this rule) in a file that was
current after the marker was written. So that file, and any file entered through a seal since, holds the covered
record at every sequence number below the number, and no recovery changes it (F2, F3); seeks drop the read buffer, so
the reader read those records from the file after that (F5). While the cursor is at or below the number, it has passed
only such records, and their end is a boundary of every later history of the file. The file is still current at the
seek, since its path was just opened (F4). The last-record check adds nothing to this.

*What differs from a scan from the segment's start.* Both differences need storage damage.

1. Records the reader has passed are not validated a second time. If a covered record behind the cursor is damaged
   after the reader passed it (and it is not the last one), a scan stops there and the continued seek does not. What it
   returns is still covered, validated and part of the journal: recovery starts at the marker and does not remove it.
2. Damage to the marker is outside ADR 0034's argument. If it destroys the marker, or sets it back to an older one
   (the newest slot lost, so that the other one counts), and destroys covered records, recovery can remove those in
   place, under a cursor that was trusted. The last-record check is there for this case: the seek
   scans, unless the new history has, right before the cursor, bytes that form a record with a valid CRC32C and
   exactly the expected sequence number. If they are its own record, the cursor is a boundary. If they are inside
   another record's payload, the seek goes on from a wrong position, again on every seek to it or forward. This holds for
   every kind of reader. A seek to the last record the reader passed (returned or skipped), or an earlier one, or a new
   reader, gets out; a target before `position()` is not enough after a seek the reader has not caught up with. `crash-contract.md` (Readers after a repair) and `format.md` (What
   readers do about repairs) say so.

**The end.** `Reader::seek_to_end` opens the newest listed segment (a durable-only reader: the segment holding the
marker's position), starts at the marker's position if the marker names that segment, otherwise at its first record,
and reads on, following seals, until there is nothing more to read. It then checks that the segment is still in
place, and starts over if it is not. A durable-only reader forgets the marker it had applied, since the end is where
the marker is now; without a valid marker it is placed at the first record of the oldest segment, without reading, and
a marker that can't be read is an error. "The end" is an observation: every record committed before the call is behind
the reader, every one committed after it returns is ahead. The rejected alternative was a fixed target (the head of a
`status` sample, then a seek): it reads the newest segment twice and leaves the reader behind the writer. A failure
puts the reader back exactly as it was, read buffer and applied marker included (C13 needs the latter: the marker may
have been applied to a newer file), so that the bindings can keep their rule that a failed seek changes nothing. The C
ABI, C++, Python and Java get the call as an addition, which ABI version 1 allows (ADR 0043).

**The Python decoder** starts at the newest segment whose base is at or below `start_seq`.

**Choosing the segment** still lists the directory on every seek. A cache of bases can be stale after retention,
archiving or recovery, and the listing costs little against the scan; not changed.

## Consequences

- The cost of a forward seek in a checkpointed segment, after the first one, is the records between the cursor and the
  target plus the listing, the open and one record read. A seek that scans, or continues to a target past the known
  coverage, also reads the marker (now its two slots, not the whole file) and makes a `stat`. A journal that never
  checkpoints gains nothing. The first forward seek after `seek_to_end` scans when no record has been read since,
  because the reader does not know the record before its cursor.
- An index entry (ADR 0042 and the work after it) is another way to place the cursor. A seek that lands through one must
  leave `seek_covered` true for that cursor: set it only if the entry is a boundary of the current chain by an argument
  of its own, after the marker was read, otherwise to 0.
- `tests/seek.rs` checks the results against a fresh reader's, which always scans, over random appends, rolls,
  checkpoints, repairs of both kinds and removals, for the four kinds of reader, and each condition on its own.

## Amendment (2026-11-27): no cached listing (issue #80)

Decided by the orchestrator on the owner's delegation. Measured since (`benchmarks.md`, 2026-11-25, one x86-64 cloud
machine, ext4 on local SSD): the listing is about 0.29 µs per segment with a warm cache, so a seek costs about 3 ms at
10,000 segments, 6 ms at 20,000 and 29 ms at 100,000 before it reads anything, and 16 forward seeks cost 16 listings;
the seek index does not change that. With nothing cached, reading the directory took 18, 35 and 215 ms there.

**There is still no cached listing.** A cache of bases would have to be invalidated by retention (the writer's and
`retention::apply` in any process), by archivers in other processes, which take segments out of the journal at any
time (ADR 0055), and by recovery, which replaces segments. A process can't learn of the others' changes without asking
the file system, and a stale entry would send a reader to a segment that was removed or replaced, which the reader then
has to detect and report as a removal (`Error::Discarded`) or an unavailable sequence number where a fresh listing finds
the record. The supported answer is a moderate number of segments: `docs/operations.md` → How many segments to keep.
The rustdoc of `Reader::seek` says what a seek costs.
