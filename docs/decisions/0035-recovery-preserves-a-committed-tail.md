# 0035: Recovery preserves a committed tail behind an empty commit word

- **Status:** accepted (a fix within the existing recovery contract; refines [0007](0007-checkpoint-marker-and-recovery.md))
- **Date:** 2026-11-19

## Context

Writer recovery scans from the checkpoint and stops at the first commit word that reads 0. The recovery table in
[`format.md`](../format.md) treated that position as the end of the journal ("clean end, or a body the previous
writer never committed") and zeroed every non-zero byte after it **in place**. If storage damage zeroes the commit
word of a record after the checkpoint, everything after it is committed history that readers may already have
returned, and recovery destroyed it without preserving it. A review reproduced this: records 0 to 99, checkpoint at
50, the 4-byte commit word of record 60 zeroed; the next writer reported `ZeroedUncommitted` and records 60 to 99
were gone, and a zero-copy reader holding record 99's payload saw it turn to zeros. This contradicts the README
("preserves anything it removes in `quarantine/`"), the roadmap ("does not silently discard committed history") and
the safety argument of `Reader::open_mapped` (recovery never modifies committed bytes). Under `RecoveryPolicy::Fail`
it was worse: `Fail` zeroed the records too, because it only refuses *invalid* bytes.

The format stays version 1; nothing on disk changes.

## What a process crash can leave after the last record

Call the empty slot `P` in segment `X` (size `Z`), expecting sequence number `S`. Before an append starts, every
byte after `P` is zero: appends are contiguous, an abandoned write is zeroed and synced before anything else is
appended ([`format.md`](../format.md) → Abandoned writes), and recovery zeroes or replaces everything after the end
of the chain before the next append. So a writer killed at any instruction leaves, after `P`:

| What was happening | What is left |
|---|---|
| An append (`write_record_fields`, the fill closure, `finish_record_body`, then the commit store) | Non-zero bytes only inside `[P+4, P+len)` of the frame being written: its header fields, part or all of its payload, and its CRC if the fill finished |
| Zeroing an abandoned write (`body.fill(0)`) or a recovering writer zeroing a torn tail | A subset of the same frame |
| Prefaulting (`MADV_POPULATE_WRITE`, `MADV_WILLNEED`) | Nothing: advice never changes contents |
| The macOS prepared spare (ADR 0022) | `next-segment.tmp`, all zeros and not a segment; removed in step 1. Once linked it is the successor named exactly `S` |
| A roll | Before the `link`: nothing new. After it: a segment named exactly `S` and nothing after `P` in `X` (the seal's CRC word at `P+4` at most) |

A process crash therefore never leaves a segment whose base is greater than `S` (rolling to base `B` needs record
`B-1` committed), and never a valid frame after `P` other than one inside the torn frame's own payload.

## Decision

### How recovery tells them apart

At an empty commit word at `P`, before zeroing anything, recovery looks for **committed history** after it. Any of
these counts:

1. **A later record.** An 8-byte-aligned `Q > P` holding a record that validates completely (length in range and
   inside the record area, CRC32C, flags zero) whose sequence number `T` satisfies `S < T <= S + floor((Q - P) / 40)`:
   records `S` to `T - 1` must fit between `P` and `Q`, and every frame is at least 40 bytes.
2. **A final seal.** A valid seal for a `T` in the same range that is the last non-zero item in `X`. (When record `S`
   is the last before a seal, the seal is the only thing after it.)
3. **A later segment.** A segment file whose base is greater than `S`, if `X` holds any non-zero byte after `P`.
   (Without non-zero bytes there is nothing to preserve, and the existing rule for disconnected segments applies.)
   Except when the successor for exactly `S` exists and has not been entered yet, and the non-zero bytes end within
   a seal at `P` (`[P+4, P+8)`): see "Damage around a seal" below.

If none is found, `P` is a torn append (or a clean end) and is handled exactly as before.

**Candidates inside a completely written torn frame are not evidence.** If the frame at `P` has an intact header
apart from its commit word (sequence number `S`, flags zero) and its stored CRC matches for a frame length `L` that
fits and covers every non-zero byte after `P` (`L` from the end of the last non-zero byte to 63 bytes past it), it is
a torn append whose body was completely written, and any candidate lies inside its payload: recovery treats `P` as a
torn append. A writer killed after storing the CRC and before the commit store leaves exactly this. It does not cover
a body torn in the middle (no CRC yet, or a CRC the bytes don't match yet), or a payload ending in more than 63 zero
bytes (its length lies outside the range tried): those stay evidence, the conservative side. Every length tried can
match by chance (`2^-32` each), and a chance match would hide real evidence, so the range is kept to 64 lengths, and
a frame whose CRC also matches for a length that ends where the first candidate starts is not covered: that is a
complete record followed by the next one, which is what a zeroed commit word in front of committed records leaves.
Hiding real evidence therefore needs damage to the frame at `P` beyond its commit word with its sequence number and
flags intact, and then a chance match among 64 lengths (`2^-26`). The check runs once, at the first candidate:
CRC32C over the frame up to the last non-zero byte and up to the first candidate, each length after the first moved
into place with a shift (`crc::x8n`) instead of a new pass.

**False positives** (a torn append judged to be damage). A slot of random bytes passes only if its length is in
range (at most `Z / 2^32`), its flags are zero (`2^-32`), its sequence number falls in a window of at most `Z / 40`
values (`2^-42` for a 128 MiB segment) and its CRC matches (`2^-32`): below `2^-106` per slot, below `2^-82` over the
`2^24` slots of a 128 MiB segment. A final seal needs the marker (`2^-32`) and a CRC match within the window. Frames
copied into a payload are different: the record CRC covers nothing that identifies a journal, so **any payload that
contains an 8-byte-aligned, CRC-valid frame whose sequence number falls in the window** passes, whichever journal the
frame came from. The window rules out frames whose sequence number is at or below `S` or too far ahead for its
position, which removes stale copies of this journal's earlier records, but a frame from another journal with a small
sequence number passes near the start of a journal, and so does any frame when one journal's payloads carry raw
frames read from another (an archiver, a relay, a test that embeds journals). Reproduced (review round 1): journal A
holds records 0 to 19, journal B records 0 to 2, and B's writer is killed while filling record 3's payload with A's
raw frames; A's records 4 onwards are in B's window. If the writer dies while writing such a payload, and its body
was not completely written (above), recovery preserves the torn bytes in `quarantine/` and replaces the segment
(`Quarantine`; readers at the end get `Error::Discarded`; nothing is lost) or **refuses to open after a plain process
crash** (`Fail`). No rule on these bytes can remove this: a torn payload holding such frames is byte for byte the same
as committed records behind a damaged commit word. Nothing is lost either way, so it stays on the conservative side,
and the exception is stated where the contract promises that a crash needs no human (C1, C2, the `RecoveryPolicy`
rustdoc, `operations.md`). An operator whose `Fail` writer refuses with `EmptyCommitWord` after a known process crash
checks the frame at the reported offset: if it has the expected sequence number and the frames after it lie inside
its payload, it is a torn append, and opening once with `Quarantine` preserves those bytes and continues
(`tests/committed_tail.rs::payload_holding_another_journals_frames`; `tests/recovery.rs::panic_in_fill_then_shorter_append`
builds one from this journal's own frames, but panics instead of being killed, so its bytes are zeroed before recovery
sees them).

Requiring **two consecutive frames** was rejected: a single record after the damage (damage at record 98 of 0 to 99)
is a real case, and a payload that embeds one frame can embed two, so it would lose detection without removing the
false positive. Recovering the damaged frame's length from its own CRC and requiring record `S + 1` exactly at
`P + align8(len)` was also rejected as the rule: it does defeat embedded frames, but it only detects a precisely
zeroed 4-byte word. Realistic damage (a lost write, a zeroed sector, pages written back out of order before a power
loss) zeroes whole blocks, headers included, and the first intact record after them carries `S + k`.

**Work bound.** One backward pass over `[P+4, Z)` to find its last non-zero byte (recovery already made this pass to
zero the tail); at each 8-byte position up to that byte, a commit-word load and, if the length is in range, a 16-byte
header read; CRC32C for candidates whose header passes, at most twice the length `Z - P` in total (beyond that the
search stops and reports the tail as committed history, which only bytes made of this journal's own record headers
can cause); for a final seal at most `(Q - P) / 40` CRCs of 12 bytes; and at the first candidate, once, CRC32C over
the frame at `P` up to the last non-zero byte and up to the candidate (at most twice `Z - P` more) plus 72 shifts. It
runs once per recovery, at the one empty slot where the scan stops, and in `verify`.

### Damage around a seal

Segment `X` ends with records up to `S - 1` and a seal for `S`; segments based at `S` and possibly later follow.
Review round 1 reproduced a regression: with the seal's 4-byte marker zeroed (its CRC word intact) and more than one
following segment, evidence (3) fired (a segment based above `S`, a non-zero tail), the tail was quarantined and every
later segment then quarantined as disconnected (861 of 1,200 records removed; `Fail` refused). Before this change the
same bytes took the branch for a crash between publishing a successor and sealing, which rewrites the seal and keeps
everything. They are also exactly what that crash leaves: `write_seal` stores the CRC word, then the marker. So
evidence (3) does not count when the successor for exactly `S` exists and has not been entered, and the non-zero bytes
end within `[P+4, P+8)`. The shapes next to it, each decided explicitly and tested under both policies
(`tests/committed_tail.rs::damage_around_a_seal_is_resealed_or_preserved_never_lost`, with three later segments and
with exactly one):

| Damage at the end of `X` | Evidence | Outcome |
|---|---|---|
| Seal marker zeroed, CRC word intact, several later segments | None (the exception) | Seal written again, every record kept, as for the crash |
| Seal marker zeroed, exactly one later segment | None (no segment above `S`) | The same |
| Seal marker and CRC word zeroed | None (nothing non-zero) | The same |
| Seal intact, record `S - 1`'s commit word zeroed | (2), the final seal for `S` | Corruption at record `S - 1`: `Quarantine` preserves the tail and every later segment (the chain can't continue past a lost record), `Fail` refuses |
| Record `S - 1`'s commit word and the seal marker zeroed | (3): the successor is for `S`, not `S - 1`, so the exception does not apply | The same. A crash never leaves this: rolling needs record `S - 1` committed |
| Record `S - 1`'s commit word and the whole seal zeroed | (3) alone | The same |

The last three lose no byte (everything is in `quarantine/`) but do remove records `S - 1` onwards from the journal;
format v1 cannot express the gap a lost record leaves (see below). A zeroed block from the middle of a sealed segment
to its end, with the first record's body left, is the same case as the last row
(`::zeroed_block_ending_a_sealed_segment_is_preserved`).

### What recovery does with a committed tail

It is corruption, handled under `RecoveryPolicy` exactly like an invalid record at `P`, reported as
`CorruptReason::EmptyCommitWord(T)`:

- `Quarantine` (default): preserve `[P, Z)` byte for byte in `quarantine/`, replace `X` by a copy with `[P, Z)` zeroed
  (`RecoveryAction::RepairedTail`), and move later segments, now disconnected, into `quarantine/`. Readers holding the
  old file keep reading unchanged bytes and get `Error::Discarded` when they notice the replacement.
- `Fail`: refuse with `Error::Corrupt` at `(X, P)` before anything in `X` is zeroed.

**The records after the damaged word are removed (preserved), not kept.** Keeping them was weighed:

- Format version 1 cannot express a gap: sequence numbers increase by exactly 1 and readers require it. When the
  damage covers more than the commit word, records `S` to `T - 1` are gone, and keeping `T` onwards would need a
  format change.
- When only the commit word is lost, the frame at `P` is intact and its length could be found by trying the eight
  lengths that end at `align8` of the next record and checking the CRC. Writing that length back would keep
  everything. It was rejected: it writes a guessed frame boundary into a commit word on the evidence of a CRC three
  bits weaker than a reader's check; it trusts storage that has just lost a write for the bytes around it; it adds a
  second repair mode for one damage shape; and the operator can do the same from the quarantine file with full
  information. One rule for every corrupt position (C8, C11) is easier to trust.
- What removal costs: records `S` onwards leave the journal and their sequence numbers are reused by new appends, as
  after a power loss (C8, which already allows losing records after the checkpoint). Nothing is destroyed: the
  quarantine file starts at `P` and the records validate there once the commit word is put back
  (`tests/committed_tail.rs::zeroed_commit_word_after_checkpoint_is_quarantined`).

### Torn appends are unchanged

Without evidence, recovery zeroes `[P+4, last non-zero byte]` in place and syncs it (`ZeroedUncommitted`), as before.
That stays sound for a zero-copy reader: a reader creates a slice only after loading a non-zero commit word and
validating the record, the commit word at `P` was never non-zero, so no reader holds a slice into `[P, Z)`; records
before `P` are not touched; and the evidence search only reads. The compatibility fixtures killed mid-append, the
kill loops (including kills inside the fill closure) and every simulated torn shape recover exactly as before.

### `verify` on a live journal

`inspect::verify` reads the newest segment with `pread` while a writer may be appending, without the writer's lock.
Review round 1 reproduced false reports: it found the empty commit word at the end, `P`, and while it searched the rest
of the segment (which starts by finding the last non-zero byte from the segment's end, megabytes of reads) the writer
committed the record at `P` and more after it; those validated with sequence numbers in the window and counted as
evidence, so `verify` reported `EmptyCommitWord` on a healthy journal (10 of 10 runs, 44,000 records per second into a
32 MiB segment), and the CLI exits 2 on any problem. The 50 ms re-scan it already did for invalid records hit the same
race at the new end.

So after the search finds evidence in the newest segment, `verify` reads the commit word at `P` again. If it is now
non-zero, the journal grew: `verify` continues the scan from `P` and searches again at the new end. This is sound
because the writer stores `P`'s commit word before it writes anything after `P`'s frame, and evidence there includes
a commit word stored after that one (the later record's, or the seal's marker, each stored with release ordering
after everything written before it): evidence that a live writer appended always comes with a non-zero word at `P`.
Damage at `P` stays zero, because no writer writes a commit word it has already passed. Evidence inside `P`'s own
frame (a payload that holds frames, the accepted false positive) is written before `P`'s commit word; for it `verify`
waits 50 ms once, as it already did for invalid records, and reads the word again before reporting. The loop is
bounded: after 8
rounds (a writer appending faster than `verify` can search) it stops and reports the segment as open at the last
record it reached, which is what it reports for anything appended after it returns; never a problem. Only the newest
segment needs this: the writer appends only there, and the empty word of a segment it has just rolled from only ever
becomes a seal marker, with nothing written after it that could be evidence. No other reader-side function uses the
evidence search (`list_segments`, `status` and the readers never call it), and writer recovery holds the lock.
Tested with a live writer (`::verify_never_reports_a_healthy_journal_under_a_live_writer`, run 100 times in debug and
100 times in release builds, 10 `verify` runs each).

### A crash during the repair

The repair is the existing one for an invalid record (preserve, write the repaired copy as `X.tmp`, rename it over
`X`, then quarantine the disconnected later segments), and recovery is repeatable at every step. Two fault-injection
points (`fault::Point::RepairBeforeRename`, `RepairAfterRename`; compiled out without the `fault-injection` feature)
stand in for a crash there (`::crash_in_the_middle_of_a_repair`):

- After the tail is preserved, before the rename: the next recovery removes `X.tmp`, finds the same damage, and
  preserves the same tail again. `quarantine/` then holds **two files with identical bytes** for the same position
  (the names differ in their timestamp): a duplicate means a repair was interrupted and repeated, nothing more, and
  either copy can be deleted.
- After the rename, before the later segments are handled: `X` already ends at `P` with zeros after it, so the next
  recovery finds no evidence in `X`, ends the chain at `P`, and quarantines the later segments as disconnected.

Either way, every removed byte is in `quarantine/` once the next recovery completes, and the journal keeps records
before `P`.

### What cannot be detected

Recovery and `verify` see only what is after `P`. **Any damage after which no record validates and no final seal or
later segment remains** is byte for byte a torn append, and is zeroed like one: the records it covers are destroyed in
place, nothing reaches `quarantine/`, `verify` reports nothing, and `Fail` does not refuse. The newest record's commit
word zeroed is one case (a writer killed between the CRC store and the commit store leaves the same bytes); so are,
with 120 records and a checkpoint at 100, the commit words of the **two** newest records zeroed (next sequence 118,
records 118 and 119 destroyed) and a zeroed block from the start of the third-newest record into the newest record's
header (next sequence 117), because what remains after the empty word no longer validates
(`::damage_that_leaves_nothing_valid_after_it_cannot_be_detected`). A zero-copy reader holding any of those records
sees its bytes change, but only after something other than a tickertape writer changed the file, which
`open_mapped`'s precondition already excludes. Damage that zeroes a commit word and everything after it leaves
nothing to find or preserve.

## Cost

Recovery of one 128 MiB segment (`tests/committed_tail.rs::recovery_time_128_mib`, median of three opens of fresh
copies, Apple M4 Pro, APFS, `prefault(false)`, other tests idle):

| State | Release before | Release after | Debug before | Debug after |
|---|---|---|---|---|
| Empty segment, clean close (128 MiB of zeros read) | 248 ms | 198 ms | 1,034 ms | 706 ms |
| Nearly full segment (126 MiB of records), clean close | 52 ms | 49 ms | 420 ms | 421 ms |
| Torn append of the largest record (128 MiB of body) | 116 ms | 98 ms | 107 ms | 668 ms |
| Commit word of record 1,000 zeroed, `Fail` | 121 ms, **zeroes 132 MB of records** | 3 ms, refuses | 95 ms, zeroes them | 7 ms, refuses |
| Commit word of record 1,000 zeroed, `Quarantine` | 87 ms, zeroes them | 162 ms, preserves 133 MB | 74 ms, zeroes them | 149 ms, preserves them |

In debug builds the empty-segment case is faster because the zero search now compares 8 bytes at a time and runs
once; in release the difference is within the run-to-run variation of reading never-written pages. The debug cost
of the 128 MiB torn record is the per-position header check over 16 million positions; release builds do it in the
time it takes to read the tail. A repair costs writing the quarantine copy and the repaired segment.

## Consequences

- New `CorruptReason::EmptyCommitWord(seq)` (the enum is `#[non_exhaustive]`); `seq` is the sequence number the
  evidence carries: a later record's, a final seal's next sequence number, or a later segment's base.
  `inspect::verify` and `tickertape verify` report it at the empty word, in the newest segment too.
- `verify` now reads the whole remaining tail of the newest segment, where before it stopped at the end of the
  records. `tickertape verify` on one 128 MiB segment (release build, warm page cache, APFS, Apple M4 Pro, median of 5
  runs of the process): empty segment 9.2 ms before, 21 ms after; half full (64,000 records of 1 KB) 16.1 ms before,
  21.7 ms after; nearly full (127,000 records) 22.7 ms before, 22.8 ms after.
- `Fail` now refuses states it used to "clean up" by zeroing, because those bytes were committed. After a power loss
  that wrote pages back out of order, records after the checkpoint are preserved in `quarantine/` (or the open is
  refused) instead of zeroed; C8 allowed losing them either way.
- `Fail` can refuse after a plain process crash in one case: a torn payload holding frames in the window (above).
  C1, C2, the `RecoveryPolicy` rustdoc and `operations.md` state it, with what an operator does.
- Spec: [`format.md`](../format.md) → Recovery (the table and steps), [`crash-contract.md`](../crash-contract.md)
  (C1, C2, C8, C11, readers and memory safety), [`operations.md`](../operations.md) (checking a journal, after a
  crash), [`cli.md`](../cli.md) (`verify`). Tests: `tests/committed_tail.rs`, `tests/kill_loop.rs::kill_inside_fill`,
  the soak's "zeroed commit word" injection, unit and property tests of `format::find_history_after_empty` and of
  the CRC length search, the fault-injection points `RepairBeforeRename` and `RepairAfterRename`, and new
  `recover_dir` fuzz seeds.
