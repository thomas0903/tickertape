# 0046: Seeks that need no index

- **Status:** proposed
- **Date:** 2026-11-25
- **Relates to:** [0034](0034-durable-only-reads-check-segment-identity.md) (its facts F1 to F4 are used here),
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
2. **Remember, per reader, up to where a marker covered the records when they were read.** Sound, but a live visible
   reader reads ahead of the checkpoint all the time, and every record it read ahead would stay unusable as a start.
3. **Decide at the seek, from the current marker and the file** (chosen). No state in the reader and nothing on the
   per-record path.

## Decision

**Forward seeks.** `seek` lists the directory, chooses the segment and opens it as before, with the same errors. It
then starts at the cursor instead of the segment's first record if all of these hold, and otherwise does what it did:

- the target is at or after the cursor, and the segment opened is the file the reader has open (device and inode);
- the checkpoint marker's `next_seq` is at or above the cursor's sequence number, and after the marker was read the
  segment's path still names that file;
- the record before the cursor, read from the file now, is valid, has the sequence number before the cursor's and
  ends at the cursor.

*Why that is enough.* The second condition makes the open file current after the marker was written, so by ADR 0034
(F2, F3) it holds the covered record at every sequence number below the marker that it contains, and no recovery
changes those. The walk from the segment's start would therefore pass the covered record with the sequence number
before the cursor's, and continue right after it. The third condition finds a valid record with that sequence number
ending at the cursor. Without an in-place repair since the reader passed it, it is the record the reader passed. After
one, everything from the repair point on was zeroed, so it is either the covered record, or bytes inside a later
record's payload that form a frame with a valid CRC32C and exactly that sequence number; ADR 0034 and C11 already
accept that coincidence for a reader that reads on. Records from the cursor on are read from the file after the
checks (the read buffer is dropped) and validated as before.

*What differs from a scan from the segment's start.* Records the reader has passed are not validated a second time. If
storage damages a covered record behind the cursor after the reader passed it (and the record before the cursor is
intact), a scan would stop there and the continued seek does not. What it returns is still covered, validated and part
of the journal: recovery starts at the marker and does not remove it. C11 ("never return a record that fails
validation") and the rest of the crash contract are unchanged. Damage that destroys the marker makes the second
condition fail, as long as no later checkpoint has been written; after one, the third condition still holds the line.

**The end.** `Reader::seek_to_end` opens the newest listed segment (a durable-only reader: the segment holding the
marker's position), starts at the marker's position if the marker names that segment, otherwise at its first record,
and reads on, following seals, until there is nothing more to read. It then checks that the segment is still in
place, and starts over if it is not. "The end" is an observation: every record committed before the call is behind
the reader, every one committed after it returns is ahead. The rejected alternative was a fixed target (the head of a
`status` sample, then a seek): it reads the newest segment twice and leaves the reader behind the writer. A failure
puts the reader back where it was, so that the bindings can keep their rule that a failed seek changes nothing. The C
ABI, C++, Python and Java get the call as an addition, which ABI version 1 allows (ADR 0043).

**The Python decoder** starts at the newest segment whose base is at or below `start_seq`.

**Choosing the segment** still lists the directory on every seek. A cache of bases can be stale after retention,
archiving or recovery, and the listing costs little against the scan; not changed.

## Consequences

- The cost of a forward seek in a checkpointed segment is the records between the cursor and the target, plus a
  marker read, a `stat` and one record read. Seeks in uncovered records, backward and into other segments cost what
  they did. A journal that never checkpoints gains nothing.
- The place where `seek` picks its start (`continues_from_cursor`) is where an index entry would be a second candidate
  (ADR 0042); it needs its own argument for why the entry is a boundary of the current chain.
- `tests/seek.rs` checks the results against a fresh reader's, which always scans, over random appends, rolls,
  checkpoints, repairs of both kinds and removals, for the four kinds of reader, and each condition on its own.
