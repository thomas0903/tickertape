# 0035: Recovery preserves a committed tail behind an empty commit word

- **Status:** accepted (a fix within the existing recovery contract; refines [0007](0007-checkpoint-marker-and-recovery.md))
- **Date:** 2026-11-19; amended 2026-11-20 (the tail in front of a successor that does not open is preserved, or
  refused under `Fail`, instead of zeroed in place, and an I/O error opening that successor fails the open: "Damage
  around a seal")

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
2. **A final seal.** A valid seal for a `T` in the same range, and at most `SEAL_SEARCH_RECORDS` (128) above `S`,
   that is the last non-zero item in `X`. (When record `S` is the last before a seal, the seal is the only thing
   after it.)
3. **A later segment**, meaning one with a **higher** base than `S` (the successor for exactly `S` is not one): a
   segment file whose base is greater than `S`, if `X` holds any non-zero byte after `P`. (Without non-zero bytes
   there is nothing to preserve, and the existing rule for disconnected segments applies.) It does not count when
   a segment for exactly `S` exists, is not `X` and has not been entered, but does not open, and every non-zero byte
   after `P`'s commit word lies in `[P+4, P+8)`, the seal's CRC word (see "Damage around a seal").

The order is fixed. First, when the successor for exactly `S` exists, is not `X` itself (at `X`'s first record slot
`S` is `X`'s own base), has not been entered yet, and opens (the writer opens and maps it successfully; a valid header
alone is not enough; an I/O error opening it fails the open, see "Damage around a seal"), `P` is where `X`'s seal belongs and the chain continues in that successor; that takes precedence, and none of the above is
consulted (see "Damage around a seal" below). Otherwise, if `X` holds any non-zero byte after `P`'s commit word, the search
runs, in `X` first: the first record or final seal it finds (1, 2), or a stop at its work bound ("Work bound" below),
decides, unless the torn-frame check below dismisses it. Only when the search of `X` yields nothing is (3) consulted.
Evidence, or a stop at the work bound that is not dismissed, means a committed tail (preserved, or refused under
`Fail`).

If the search completes without finding any, `P` is treated as a torn append (or a clean end) and handled exactly as
before; that decision follows from the evidence, and damage that left none is zeroed the same way ("What cannot be
detected"). The exception is a successor for exactly `S` that exists, is not `X`, has not been entered and does not
open: the chain is then broken at `P`, and non-zero bytes after its commit word are preserved, not zeroed ("Damage
around a seal").

**Candidates inside a completely written torn frame are not evidence.** If the frame at `P` has an intact header
apart from its commit word (sequence number `S`, flags zero) and its stored CRC matches for a frame length `L` that
fits and covers every non-zero byte after `P` (`L` from the end of the last non-zero byte to 63 bytes past it), and
no matching length ends at the first candidate's aligned boundary, recovery takes it for a torn append whose body
was completely written, with any candidate inside its payload, and does not count the candidates in the segment (a
segment with a higher base is still consulted afterwards, and counts under evidence (3)). This is a heuristic
on the CRC, not a proof: a writer killed after storing the CRC and before the commit store leaves such bytes, and
damaged bytes can match by chance. It does not cover
a body torn in the middle (no CRC yet, or a CRC the bytes don't match yet), or a payload ending in more than 63 zero
bytes (its length lies outside the range tried): those stay evidence, the conservative side, and with a candidate
inside them they are the process-crash exception below. Every length tried can
match by chance (`2^-32` each), and a chance match would hide real evidence, so the range is kept to 64 lengths, and
a frame whose CRC also matches for a length that ends where the first candidate starts is not covered: that is a
complete record followed by the next one, which is what a zeroed commit word in front of committed records leaves.
That rule protects when the first candidate is the record right after `P`. Hiding real evidence otherwise needs a
chance match among 64 lengths (`2^-26`), in one of these shapes: the frame at `P` damaged beyond its commit word with
its sequence number and flags intact; two consecutive commit words emptied, so that the first candidate is the
record two after `P`; or a committed record at `P` whose own payload holds frames in the window, so that the first
candidate lies inside it. The check runs once, at the first candidate:
CRC32C over the frame up to the last non-zero byte and up to the first candidate, each length after the first moved
into place with a shift (`crc::x8n`) instead of a new pass.

**False positives** (a torn append judged to be damage). A slot of random bytes passes only if its length is in
range (at most `Z / 2^32`), its flags are zero (`2^-32`), its sequence number falls in a window of at most `Z / 40`
values (`2^-42` for a 128 MiB segment) and its CRC matches (`2^-32`): below `2^-106` per slot, below `2^-82` over the
`2^24` slots of a 128 MiB segment. A final seal needs the marker at an 8-byte-aligned position, only its CRC word and zeros after it, and a CRC
match for one of the next sequence numbers tried. Payloads make the marker likely (it is `ff ff ff ff`, an all-ones
or `-1` field), so the CRC is the bound that matters: at most `SEAL_SEARCH_RECORDS` (128) numbers are tried, each a
`2^-32` chance, so **at most `2^-25` (about 1 in 34 million) per torn append whose payload, as far as it was written,
ends in a seal marker at an aligned position**, and `2^-57` for a payload of random bytes. Review round 2 measured the
uncapped window (every number the space allows, `(Q - P) / 40`): about 1 in 1,280 such crashes for a 128 MiB tail
and 9.5% at 16 GiB, each a refusal under `Fail` after a plain process crash. The cap gives up only a final seal more
than 128 records after the empty word with the successor segment lost too: with the successor present, evidence (3)
covers a damaged run of any length before a seal. 128 covers a zeroed 4 KiB block of minimal 40-byte records (102).
Frames
copied into a payload are different: the record CRC covers nothing that identifies a journal, so **any payload that
contains an 8-byte-aligned, CRC-valid frame whose sequence number falls in the window** passes, whichever journal the
frame came from, and so does a payload that ends with the eight bytes of a seal for a `T` in the window (a seal copied
from a journal, or bytes that happen to form one), as evidence (2). The window rules out frames whose sequence number is at or below `S` or too far ahead for its
position, which removes stale copies of this journal's earlier records, but a frame from another journal with a small
sequence number passes near the start of a journal, and so does any frame when one journal's payloads carry raw
frames read from another (an archiver, a relay, a test that embeds journals). Reproduced (review round 1): journal A
holds records 0 to 19, journal B records 0 to 2, and B's writer is killed while filling record 3's payload with A's
raw frames; A's records 4 onwards are in B's window. If the writer dies while writing such a payload, and the frame
does not pass the torn-frame check above (for example because its CRC was not yet stored, or because its payload ends
in more than 63 zero bytes), recovery preserves the torn bytes in `quarantine/` and replaces the segment
(`Quarantine`; readers at the end get `Error::Discarded`; nothing is lost) or **refuses to open after a plain process
crash** (`Fail`). No rule on these bytes can remove this: a torn payload holding such frames is byte for byte the same
as committed records behind a damaged commit word. Nothing is lost either way, so it stays on the conservative side,
and the exception is stated where the contract promises that a crash needs no human (C1; C2, the `RecoveryPolicy`
rustdoc and `operations.md` point to it). The same holds for a torn payload of header-shaped bytes that
exhausts the work budget (`UncheckedTail`; "Work bound" below). An operator whose `Fail` writer refuses with
`EmptyCommitWord` or `UncheckedTail` after a known process crash can check the frame at the reported offset: one
with the expected sequence number whose payload holds the frames after it is consistent with a torn append, but
cannot prove one, since the bytes can be identical to damage. What decides is the application's own knowledge of
what it was writing when the process died; if it was such a payload, opening once with `Quarantine` preserves those
bytes and continues
(`tests/committed_tail.rs::payload_holding_another_journals_frames`, `::payload_ending_in_a_seal_for_the_next_record`; `tests/recovery.rs::panic_in_fill_then_shorter_append`
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
search stops and reports the tail as committed history, which only header-shaped bytes can cause: lengths in range,
a sequence number in the window and zero flags at several positions, as damage or a torn payload can leave them),
unless the torn-frame check above dismisses the slot where it stopped; for a final seal at most 128 CRCs of 12 bytes, counted in the same bound (which is raised by those
1,536 bytes); and at the first candidate, once, CRC32C over
the frame at `P` up to the last non-zero byte and up to the candidate (at most twice `Z - P` more) plus 72 shifts. It
runs once per recovery, at the one empty slot where the scan stops, and in `verify`. When the bound stops the search
before a candidate is decided, nothing was proven (the bound is checked before the candidate that would cross it is
validated, so that candidate's CRC is unknown: it may or may not be a record): the tail is still treated as committed history, but it is reported as
`CorruptReason::UncheckedTail(seq)`, not `EmptyCommitWord`, with the sequence number in the header of the candidate
record where the search stopped, or `S + 1` when it stopped at a final seal. A plain process crash can cause it: a writer killed while filling a payload that holds several such
header-shaped byte sequences (copies of this journal's record headers, for example) leaves overlapping length claims
that exhaust the budget (every candidate checked before the budget runs out fails its CRC, and the one that would
exceed it is not checked) while the torn record's own CRC was never written. That is
part of the process-crash exception above: `Quarantine` preserves the bytes, `Fail` refuses with `UncheckedTail`
(`tests/committed_tail.rs::torn_payload_of_header_shaped_bytes_is_unchecked`). Memory is bounded too: `verify` reads with `pread` into a 1 MiB window, and every read, including the CRC32C
of a candidate or a scanned record with a large length, goes in pieces of at most that size, so a damaged length never
allocates more (review round 2); the writer reads its own mapping and is unchanged.

### Damage around a seal

Segment `X` ends with records up to `S - 1` and a seal for `S`; segments based at `S` and possibly later follow.
Review round 1 reproduced a regression: with the seal's 4-byte marker zeroed (its CRC word intact) and more than one
following segment, evidence (3) fired (a segment based above `S`, a non-zero tail), the tail was quarantined and every
later segment then quarantined as disconnected (861 of 1,200 records removed; `Fail` refused). Before this change the
same bytes took the branch for a crash between publishing a successor and sealing, which rewrites the seal and keeps
everything. They are also exactly what that crash leaves: `write_seal` stores the CRC word, then the marker.

Review round 2 found the same failure one byte away, and found that the outcome depended on when recovery was
interrupted: 50 records in segments based 0, 18 and 36, segment 0's seal marker zeroed and one stray non-zero byte
just past the seal. The tail then ended outside the seal, the segment based 36 counted as evidence, and recovery
replaced segment 0 and quarantined both successors (18 records kept); interrupted after the rename, the next open
found an empty tail, wrote the seal and kept all 50. So the successor comes first: **when the segment for exactly
`S` exists, is not `X`, has not been entered and opens, `P` is where `X`'s seal belongs**, whatever follows `P`, and no evidence
is consulted. If nothing but the seal's CRC word follows `P`, it is the crash (or a zeroed marker): the word is
zeroed and the seal written, as before. If anything else follows, no crash left it (a roll writes nothing after the
seal, and records `S` onwards are in the successor), and it is not records of `X` either (they would repeat the
successor's sequence numbers): it is damage around the seal, not missing history. `Quarantine` preserves the bytes from `P` to the last non-zero
byte and replaces `X` exactly as any repair does (`RepairedTail`, reason `BadSeal`), then writes the seal and continues into the
successor in the same open; `Fail` refuses with `BadSeal` at `P` without changing a byte of `X`. The repair leaves `X` ending
at `P`, which is where an interrupted repair leaves the next open, and that open takes the same branch and writes the
same seal, so every path gives the same journal (`::crash_in_the_middle_of_a_seal_repair`, uninterrupted and
interrupted at both fault points). This cannot lose records the previous code kept: everything in `X` before `P` and
every segment are kept, and the bytes after `P` are preserved. Nor can it refuse or quarantine what a process crash
leaves, which is at most the CRC word.

A successor for exactly `S` that exists but does not open does not take the resealing branch: the search runs. "Does
not open" means that the successor itself is invalid: the writer's open reports it as corrupt (a file shorter than a
header, an invalid header, a size that does not match the header, a base that does not match the name) or as an
unsupported format version. Any other error opening, reading or mapping it is an I/O error (`EACCES` on files restored
read-only, `EMFILE`, `ENOMEM` or a mapping limit), which says nothing about the journal: the open returns it under
either policy without changing anything at `P` or at the successor, as the seal branch does for a successor that a
valid seal names (what the scan did before it reached `P`, a seal written in front of an earlier segment for
example, stays). Taken as a
broken chain, such an error after a plain crash between publishing the successor and sealing `X` would preserve the
seal's CRC word, replace `X` and quarantine the healthy successor and every later segment, or make `Fail` report
`BrokenChain` and hide the cause (`::successor_that_fails_to_open_for_an_io_error_fails_the_open`: once the error is
gone, the next open completes the roll). When it finds evidence, the tail is a committed tail ("What recovery does with a
committed tail", below). When it finds none, the chain is broken at `P`: `X` ends there, and the successor, which
recovery cannot enter, and every later segment are disconnected (step 5). What happens to `X` depends on what follows
`P`'s commit word:

- **Non-zero bytes.** The chain cannot continue past them whatever they are, and they can be the only bytes left for
  diagnosis or salvage, so they are handled as a corrupt tail, reported as `CorruptReason::BrokenChain(S)` at `P`.
  `Quarantine` preserves the bytes from `P` to the last non-zero byte of `X` in `quarantine/` and replaces `X` by a
  copy with `[P, Z)` zeroed (`RepairedTail`), exactly as for any corrupt tail, then moves the successor and every
  later segment into `quarantine/` as disconnected. `Fail` refuses with `BrokenChain(S)` at `(X, P)` before it
  changes a byte of `X` or moves a file. Two shapes reach this branch: only bytes of the seal's CRC word left (every
  non-zero byte in `[P+4, P+8)`), whether or not segments with higher bases exist (evidence (3) does not count for those bytes: they are what a crash between
  publishing the successor and sealing leaves, with the successor's header damaged since); and other bytes after
  `P` with no segment of a higher base (with one, evidence (3) holds: the tail is preserved as `EmptyCommitWord`, or
  `UncheckedTail` from the search of `X`, `Fail` refusing without changing a byte of `X`, and the successor and later
  segments are disconnected in the same way).
- **Nothing non-zero** (the whole seal zeroed, for example): there is nothing to preserve or zero, and recovery
  keeps the behaviour it had. `X` is left as it is, ending at `P`, and the disconnected segments are quarantined, or
  `Fail` refuses at the first of them, normally the successor (`BrokenChain(S)` at its offset 0), without changing a
  byte of `X`.

The reason is `BrokenChain(S)` at `P` because it says what is wrong, and it is what `verify` already reports for
this state: `X`'s records end at `P` without a seal, and the segment that would continue the chain cannot be
entered. `BadSeal` would say that the bytes at `P` are a damaged seal in front of a successor that continues the
chain, which recovery then reseals; here nothing continues, and with bytes past the seal's place they need not be a
seal at all. The successor's own error (`BadMagic`, `BadSegmentHeader`, `SizeMismatch`, for example) describes another file at
offset 0 (offset 8 for an unsupported version), and `verify` reports it there when the successor fails its
read-only checks; the writer's report is about `X`, where the chain ends. So in the two cases this change adds
(only the seal's CRC word left, or other bytes with no higher-base segment) the writer and `verify` agree on the
reason and the position. They do not in the two it leaves alone: with other bytes and a higher-base segment the
writer reports `EmptyCommitWord` for that segment's base at `P` where `verify` reports `BrokenChain`, and with
nothing non-zero after `P` a refusing writer reports `BrokenChain` at the successor's offset 0 where `verify` reports
it at `P`.

`verify` reports `BrokenChain` at `P` in every case of this paragraph where the search of `X` finds nothing, and a
problem at the successor only when it fails `verify`'s own read-only checks (a segment the writer cannot open
read-write or map can pass them; the writer's open then fails with that I/O error, above). Every case under both policies:
`tests/committed_tail.rs::successor_that_does_not_open_ends_the_chain_at_the_seals_place`; the repair interrupted at
both fault points: `::crash_in_the_middle_of_a_broken_chain_repair` ("A crash during the repair", below).

Before this amendment (2026-11-20) the non-zero bytes of the first branch were zeroed in place without a copy,
under both policies, and `Fail` refused at the successor only after that zeroing.

The shapes with a successor that opens, each decided explicitly and tested under both policies
(`tests/committed_tail.rs::damage_around_a_seal_is_resealed_or_preserved_never_lost`, with three later segments and
with exactly one):

"Every record kept" in this table is the seal repair itself, the records of `X` before `P`; the successor is then
scanned like any segment.

| Damage at the end of `X` | Evidence | Outcome |
|---|---|---|
| Seal marker zeroed, CRC word intact, several later segments | Not consulted | Seal written again, every record kept, as for the crash |
| Seal marker zeroed, exactly one later segment | Not consulted | The same |
| Seal marker and CRC word zeroed | Not consulted | The same |
| Seal marker zeroed and other bytes after it, successor opens (any number of later segments) | Not consulted | `Quarantine`: the bytes after `P` preserved (`BadSeal`), seal written again, every record kept; `Fail` refuses. Before round 2: with later segments, segment 0's tail and every later segment were quarantined; with one, the bytes were zeroed in place without being preserved, under both policies |
| Seal intact, record `S - 1`'s commit word zeroed | (2), the final seal for `S` | Corruption at record `S - 1`: `Quarantine` preserves the tail and every later segment (the chain can't continue past a lost record), `Fail` refuses |
| Record `S - 1`'s commit word and the seal marker zeroed | (3): the successor is for `S`, not `S - 1`, so the exception does not apply | The same. A crash never leaves this: rolling needs record `S - 1` committed |
| Record `S - 1`'s commit word and the whole seal zeroed | (3) alone | The same |

The last three lose no byte (everything is in `quarantine/`) but do remove records `S - 1` onwards from the journal;
format v1 cannot express the gap a lost record leaves (see below). A zeroed block from the middle of a sealed segment
to its end, with the first record's body left, is the same case as the last row
(`::zeroed_block_ending_a_sealed_segment_is_preserved`).

### What recovery does with a committed tail

It is corruption, handled under `RecoveryPolicy` exactly like an invalid record at `P`, reported as
`CorruptReason::EmptyCommitWord(T)` when a record, a seal or a segment with a higher base is the evidence, and as
`CorruptReason::UncheckedTail` when the search stopped at its work bound outside a completely written torn frame
(the same repair or refusal either way):

- `Quarantine` (default): preserve the bytes from `P` to the last non-zero byte of `X` in `quarantine/` (the zeros
  after it are left out), replace `X` by a copy with `[P, Z)` zeroed
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

When the search completes without evidence, recovery zeroes `[P+4, last non-zero byte]` in place and syncs it
(`ZeroedUncommitted`), as before, except in front of a successor for exactly `S` that does not open ("Damage around
a seal"). For a torn append that stays sound for a zero-copy reader (for damage that left no
evidence it does not: "What cannot be detected"): a reader creates a slice only after loading a non-zero commit word and
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
record it reached, which is what it reports for anything appended after it returns; never a problem. A scan that
continues from `P` can catch the writer mid-append at the new end as the first scan can, so an invalid record there
gets the same retry; the first scan and the continued scans share that retry per segment (review round 2;
`inspect::tests::follow_loop_retries_a_record_caught_mid_append`). Since this was merged with the reader fixes for
retention races (crash contract C14), the retry is no longer one 50 ms rescan but rescans after pauses of 10 ms
doubling up to 320 ms (630 ms in all), still one budget per segment: the continued scans take the pauses the first
scan left and never start them again, so `verify` sleeps at most 630 ms per segment for rescans however many rounds
the loop makes. The 50 ms wait at a still-empty word is separate, once per segment, so rescans cannot use it up
(`inspect::tests::follow_loop_rescans_share_the_first_scans_budget`). Only the newest
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
- For damage around a seal (above), after the rename the next recovery finds `X` ending at `P` with the successor
  waiting, writes the seal and continues, which is what the uninterrupted repair does next.
- For a tail in front of a successor that does not open (above), after the rename the next recovery finds `X` ending
  at `P` with nothing non-zero after it, ends the chain there without zeroing anything, and quarantines the successor
  and the later segments as disconnected, which is what the uninterrupted repair does next
  (`::crash_in_the_middle_of_a_broken_chain_repair`: every path, uninterrupted or interrupted at either point, ends
  with the same segment files and checkpoint).

Either way, every removed byte is in `quarantine/` once the next recovery completes, and the journal keeps records
before `P`.

### What cannot be detected

Recovery and `verify` see only what is after `P`. **Any damage after which the search counts no evidence** (after
the torn-frame check no record and no final seal in the segment counts: one that validates but lies inside a
completely written torn frame is dismissed; no segment with a higher base counts under the rules of evidence (3), so
not one when only the seal's CRC word is left in front of a successor for exactly `S` that does not open; and the
search did not stop at its work bound outside such a frame) is classified as a torn append: recovery cannot
tell it from one, whether or not a real torn append could leave those bytes. Unless a distinct, unentered segment
for exactly `S` exists (if it opens, the seal branch decides; if it does not, the chain is broken at `P`: both
below), it is zeroed like one: the records it covers are destroyed in place, and `ZeroedUncommitted` reports the action
taken, though a reader may already have returned them. What else happens depends on the rest of the directory. In
the newest segment, with no segment for `S` or above and nothing else wrong, the zeroing is all: nothing reaches
`quarantine/`, `verify` reports nothing, and `Fail` does not refuse. A distinct, unentered segment for exactly `S`
that opens takes recovery into the seal branch ("Damage around a seal"), not the search: the seal's CRC word is
zeroed and the seal written, or other bytes after `P` are preserved (or refused with `BadSeal`); `verify`, which does not weigh other segments,
reports `BrokenChain` at `P` unless it finds evidence in `X`. When that segment exists but does not open, nothing
is zeroed: a non-zero tail is preserved as `BrokenChain` (or refused under `Fail` without changing a byte of `X`),
and that segment and later ones are disconnected ("Damage around a seal"); `verify` reports `BrokenChain` at `P`. The newest record's commit
word zeroed is one case (a writer killed between the CRC store and the commit store leaves the same bytes); so are,
with 120 records and a checkpoint at 100, the commit words of the **two** newest records zeroed (next sequence 118,
records 118 and 119 destroyed) and a zeroed block from the start of the third-newest record into the newest record's
header (next sequence 117), because what remains after the empty word no longer validates
(`::damage_that_leaves_nothing_valid_after_it_cannot_be_detected`). A zero-copy reader holding any of those records
sees its bytes change, but only after something other than a tickertape writer changed the file, which
`open_mapped`'s precondition already excludes. Damage that zeroes a commit word and everything after it in `X`
leaves nothing in `X` to find or preserve. A final seal counts only for one of the next 128 sequence numbers, so damage that
leaves a valid final seal further away is zeroed the same way, but only when the search finds no other evidence (no
record in its window, and no segment with a higher base: the seal's successor lost too) and completes within its
work bound.

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
  evidence carries: a later record's, a final seal's next sequence number, or the base of a segment above `S`.
  `inspect::verify` and `tickertape verify` report it at the empty word, in the newest segment too, from evidence in
  the segment itself. Where only a later segment proves that history exists (the commit word and the seal both
  zeroed, nothing valid left in the segment), the writer reports `EmptyCommitWord` but `verify` reports the
  segment's end as `BrokenChain` (it does not weigh later segments against an empty word); either way the problem is
  reported at the same position.
- New `CorruptReason::UncheckedTail(seq)` for a search stopped by its work bound (above). The C ABI maps both
  (`TICKERTAPE_CORRUPT_EMPTY_COMMIT_WORD` = 14, `TICKERTAPE_CORRUPT_UNCHECKED_TAIL` = 15), with the sequence number in
  the error message.
- Where a segment's records end at an empty commit word, `verify` now searches the rest of that segment, where
  before it stopped at the end of the records: it reads backward from the segment's end to find the last non-zero
  byte, then checks the candidates up to it, and can stop early at the first evidence or at its work bound (`cli.md`
  → `verify`). On a live journal it can also pause: rescans that add up to 630 ms per segment, plus the time of the
  scans, and a separate 50 ms wait in the newest segment ("`verify` on a live journal", above). `tickertape verify` on one 128 MiB segment (release build, warm page cache, APFS, Apple M4 Pro, median of 5
  runs of the process): empty segment 9.2 ms before, 21 ms after; half full (64,000 records of 1 KB) 16.1 ms before,
  21.7 ms after; nearly full (127,000 records) 22.7 ms before, 22.8 ms after.
- `Fail` now refuses states with evidence of committed history that it used to "clean up" by zeroing. After a power loss
  that wrote pages back out of order, records after the checkpoint that recovery finds evidence of are preserved in
  `quarantine/` (or the open is refused) instead of zeroed; C8 allowed losing them either way.
- `Fail` can refuse after a plain process crash in one case: a torn payload holding frames, or ending with a seal,
  in the window (`EmptyCommitWord`), or holding enough header-shaped bytes to exhaust the work budget
  (`UncheckedTail`), when the torn frame does not pass the torn-frame check (above). C1 states it; C2, the
  `RecoveryPolicy` rustdoc and `operations.md` point to it, with what an operator does.
- What `Fail` does not promise: it does not change the bytes of the corruption it reports, nor anything after them in
  that segment, and moves no file, but the open is not read-only. Before scanning it creates the lock file if it is
  missing and writes its PID into it (best effort), removes stale temporary names (`*.tape.tmp`, the prepared spare),
  and creates the checkpoint file if it is missing (a marker shorter than its full length is left as it is: only a
  successful checkpoint extends it), so a refused open can still have changed those files. When it refuses, the changes recovery made earlier in the scan stay in
  place: the clean-ups a process crash needs (a zeroed torn tail, a seal written for a crash between publishing a
  successor and sealing, for example in segment `X` when the corruption is in its successor, a created missing
  successor), and the zeroing at an empty commit word where the search completed without evidence, which after
  damage removes published records ("What cannot be detected"). In front of a segment for exactly `S` that does not
  open nothing is zeroed: `Fail` refuses at `P` before changing `X` when bytes follow it ("Damage around a seal").
  Crash contract C11 keeps this list.
- Spec: [`format.md`](../format.md) → Recovery (the table and steps), [`crash-contract.md`](../crash-contract.md)
  (C1, C2, C8, C11, readers and memory safety), [`operations.md`](../operations.md) (checking a journal, after a
  crash), [`cli.md`](../cli.md) (`verify`). Tests: `tests/committed_tail.rs`, `tests/kill_loop.rs::kill_inside_fill`,
  the soak's "zeroed commit word" injection, unit and property tests of `format::find_history_after_empty` and of
  the CRC length search, the fault-injection points `RepairBeforeRename` and `RepairAfterRename`, and new
  `recover_dir` fuzz seeds.
