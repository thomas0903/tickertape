# 0050: An empty slot that cannot be the head is corruption

- **Status:** accepted (2026-11-26, by the orchestrator on the owner's delegation, after one independent review and a
  re-check)
- **Date:** 2026-11-26
- **Relates to:** [0034](0034-durable-only-reads-check-segment-identity.md) (its facts F1 to F5 are used here),
  [0035](0035-recovery-preserves-a-committed-tail.md) (what recovery does at an empty commit word),
  [0046](0046-seeks-that-need-no-index.md), [0048](0048-publication-frontier.md)

## Context

Issue #79. A commit word of 0 means "no record yet", so a reader waits there. If storage damage zeroes the commit word
of a record in older history, the reader waits for ever: `try_next` returns `Ok(None)`, and a replay ends without an
error although committed and durable records follow. Only `verify` tells. Crash contract C11 says that readers detect
such damage ("length, sequence or chain checks"), and the rustdoc of `try_next` says that `Ok(None)` means "there is no
new record yet". In a journal of 16 segments with one word zeroed in segment 8, `export` wrote 7,980 of 15,267 records and
exited 0.

The difficulty is not to report a slot at which a correct reader waits. A false corruption error under a live writer is
worse than the defect.

## Options considered

1. **Any segment with a base above the reader's means corruption.** Wrong in the window of a roll: a writer links the
   successor, then stores the seal, and may die in between (C4).
2. **The checkpoint marker only.** Sound, but an archive has no marker, and a journal can roll many times past one.
3. **The marker, or a segment that a writer links only after committing the slot** (chosen).
4. **Search the rest of the segment for records, as `verify` does.** Costs up to a segment of reading at each idle
   check, and the search has a false positive that a live writer can cause (a payload holding frames, ADR 0035).
5. **Check at every empty poll.** A marker read or a directory listing for every poll of a spinning reader. Rejected.
6. **Check only after 100 ms at the slot.** Then `while let Some(r) = reader.try_next()?`, `records()` and the CLI's
   `export` still end silently, since they poll an empty slot once. Rejected as the only place.

## Decision

**The rule.** A reader at an empty slot for sequence number `S` in segment `X` reports corruption if its path still names
`X`, the slot is still empty when read after that, and one of these was established before:

- **(A) the marker covers it:** the current marker's `next_seq` is above `S`, or it names a segment after `X` (this
  includes a missing seal when the marker is at the first slot of the next segment);
- **(B) a later segment proves it:** a segment with a base above `S` exists **and has a valid header** (for the base in its
  name and its own size), or the newest segment has base `S`, is not `X` and holds a valid record in its first slot. A
  writer links a segment's name only after it wrote the header (`create_segment`, the spare's publication), so a file with
  such a name and no valid header (a stray file, one whose header never reached the disk) is no evidence. Every later name
  is tried, the newest first. A segment with a valid header copied in from **another journal** counts: nothing in a header
  ties it to a journal (its base matches the name, its size its file, and the sizes of a journal's segments may differ),
  and recovery and `verify` treat it the same way (a segment with a higher base is evidence of history, or disconnected).
  The files then no longer form one chain, and the report is what the operator needs: `verify` names the foreign file.

The error is `Error::Corrupt` with the segment, the slot's offset and `CorruptReason::EmptyCommitWord(n)`, `n` being the
marker's `next_seq` (A) or the segment's base (B). No new error, variant or code in any binding. `verify` reports a problem
at the same position, as `EmptyCommitWord`, `UncheckedTail` or, if its search finds nothing in that segment,
`BrokenChain`. So that it always does, `verify` now weighs the marker as well: a newest segment that ends before what the
marker covers is a problem.

**Why it is sound.** The order of the writer's stores: a record's commit word, then everything after it. The seal of `X`,
then the first record of its successor (`Writer::roll`), then the link of the one after that. All of them before a marker
that covers them. Recovery keeps the order (it seals before it enters the successor, and a new writer's first checkpoint
comes after it) and changes nothing before the marker it starts from (ADR 0034, F2 and F3). So if the evidence was read
first and the file that was then in place still shows 0, the word was emptied. A reader therefore reads the slot **again,
after the evidence**: the 0 it saw first may be older than the record. It reads it from the file with `pread`, also if it
is the zero-copy reader, whose mapping can lag (`crash-contract.md` → Known risks and limits).

**The 50 ms window.** The argument does not need it. Before reporting, the reader reads the slot again for 50 ms, as it does
for every slot it reports, and then establishes the evidence, the identity and the empty slot a second time. That is
defence in depth, kept at a cost of 50 ms per report and nothing otherwise: it covers a violation of the argument's one
assumption about the platform, that a store a writer made before the evidence is visible to a `pread` made after the
reader saw the evidence. Issue #43 showed that a mapped view of a page can lag a writer's stores for microseconds; a `pread`
has not been seen to, but if one did, the window would turn that lag into a delay rather than a false report. The only test
that notices its removal is the one of the delay.

**The cases where an empty slot is legitimate:**

| Case | Why it is not reported |
|---|---|
| The head | No marker covers it and no segment is later |
| A record being written; a torn append, before, during and after recovery zeroes it | Its commit word is stored before a marker covers it or a later segment is linked |
| A roll: the successor linked, the seal not stored yet (also for good, after a kill) | The successor has base `S` and holds no record; a marker in it is written after the seal |
| A recovery that writes that seal or creates a missing successor | As the roll |
| The newest segment created empty by recovery | `S` is its base: it is the reader's own segment |
| The slot committed, rolled past and checkpointed between two of the reader's reads | The slot is read again after the evidence |
| The segment replaced by recovery, or removed by retention (C14) | The identity is checked after the evidence: `Error::Discarded`, as before |
| A marker missing, unreadable or set back | It covers less: (A) does not apply. Nothing is reported on account of an unreadable one |
| A visible reader whose records a repair in place removed | Below |
| A listing that omits segments (btrfs), a header that cannot be read | Shows nothing; a later check sees it |
| A 1.0.0 writer | The order of its stores is the same |

*A cursor off the current chain.* After a repair in place (C11 (3)) a visible reader may stand at an offset that starts no
record of the new history, and wait there. Nothing there is damaged, but a marker passes it in time. Reporting it as
corruption would be a false report (one the contract allows, "reports corruption at a position that no longer starts a
record"). The reader can tell, by ADR 0046's means: it is at its segment's first slot, at or below `seek_covered`, or
the record it passed last is still right before the cursor. If none holds, the records it passed are not in the file any
more, and it returns **`Error::Discarded`**, whose documented answer, `seek`, is right in both cases that lead here: it
puts the reader on the current chain, or, if the record before the cursor was damaged too, makes it report that damage.
Until now it waited until the application sought on its own.

**States that are reported although no byte was damaged.** In each of them the files do not hold the history they claim,
and `verify` reports the same position.

- *After a power loss*, files can hold a later segment and not the records before it (C8); or the successor's first page
  reached the disk and the old segment's seal did not. Until a writer opens the journal and repairs it (it seals, or
  quarantines the disconnected segments), (B) reports that; a reader started at boot before the writer fails until then. A
  marker (A) is never in that state under the storage assumptions.
- *A restore that keeps a newer marker*: segments restored from a backup, with the marker of the running journal kept. (A)
  reports the end of the restored records, which the marker covers. `Writer::open` accepts such a marker today and carries
  on, which makes the gap below it permanent (issue #87, not changed here).
- *A copy of a journal* whose marker or later segment is newer than its earlier segments, and a segment with a valid header
  copied in from another journal (above).

**Where the check runs.** In `at_empty_slot`, never on the record path:

- at the first empty slot after the reader was opened or moved by `seek` or `seek_to_end`, at once;
- then with the identity check that a reader makes after each 100 ms at an empty slot. The marker is read each time (an
  `open` and two 64-byte reads). The segments are listed at most once a second, only if the marker shows nothing, and only
  if the directory's modification time is not the one it had when a listing showed nothing (counted only if it was at
  least 2 s old then, so that another change must show; not while a successor without a record exists). So an idle
  reader of a journal with many segments lists them once or twice after each roll, and then makes one `stat` a second;
- for a durable-only reader that waits at the marker while it names a later segment, with the check that exists there.

A visible reader that keeps up with a writer and never waits 100 ms makes no check after its first. One comparison
(`Option::is_none`) is added to an empty poll. Measured on a busy Apple M4 Pro (informal): an empty poll of the zero-copy
reader 21.1 to 21.3 ns before, 21.3 to 21.6 ns after; of the safe reader about 285 ns either way; the first empty poll after opening
0.4 µs before, 21 µs after with 2 segments and 2 ms with 5,000 (the listing). With 10,000 segments a listing takes about
5 ms on that machine, and each later name whose header is read one `open`, `fstat` and 64-byte read (about 14 µs); a reader
at the head of a journal has no later names, and lists only after the directory changed, at most once a second. The record
path compiles to the same instructions on x86-64 (on aarch64 `try_next` is one instruction shorter, an offset that now fits
an instruction), but the state is boxed: as a 40-byte field of `Reader` it made the zero-copy reader replay 64-byte
records in twice the time on that machine, and 40 or 48 bytes of padding did the same to the code before this change (8 to
32 or 64 did not). The slowdown follows the size of the struct; its mechanism is not established (issue #86).

**The delay.** The first empty slot after opening or seeking is reported in the call that meets it, which takes 50 ms or
a little more. Later ones: by (A) at the first call at least 100 ms after the reader arrived there, by (B) at the first
such call 1 s or more after the segments were last listed (so within 1.1 s plus the spacing of the calls), plus the
50 ms. The evidence has to exist by then: damage above the marker in the newest segment is reported when a checkpoint
covers it or the writer rolls.

**Afterwards** every call returns the error again (each makes the whole check, as for any slot that fails validation),
until the slot or the evidence changes. The cursor does not move. No reader gets past the slot: the length that led to the
next record is gone, so the records from there to the end of the segment cannot be found. Later segments stay readable
(`seek` to a segment's base). `tickertape verify` names the position and what follows. Recovery repairs only what is after
the marker (`Quarantine` preserves the tail, `Fail` refuses); covered history is restored from a copy of the segment.

**What is still not detected.** An emptied word that nothing above covers: in the newest segment at or above the marker
(the head and a torn append look the same); in the last segment of a directory without a marker; the last seal in front of a
segment without a record, with no marker in it. Records before a seek's landing place (C15). A file that the safe reader had already buffered
is read from the buffer. A wait that a marker's loss makes look legitimate.

## Compatibility

A bug fix under ADR 0043: the contract promised the report. But callers see an error where there was a wait, and
`Discarded` in one more case. `Error::Corrupt`, `EmptyCommitWord` and `Discarded` are existing values in every binding; the
C ABI is unchanged. The on-disk format is unchanged and any writer works. 1.0.0 readers still wait.

## Consequences

- `export`, `stats`, `tail` and `age` exit 2 where they ended with 0 or waited; `export --durable-only` says what is wrong
  instead of "journal changed". Their code is unchanged.
- The pure-Python decoder already refused an empty slot before the newest segment. With `durable_only` it now also
  refuses one in the newest segment below the marker.
- `tests/empty_slot.rs` holds the reproduction, each row of the table, and a randomised test with live readers of four
  kinds that must see no false error, and must see every injected one; `protocol_model.rs` (loom) has the order of the
  stores, with the variant that does not read the slot again as an expected failure.
