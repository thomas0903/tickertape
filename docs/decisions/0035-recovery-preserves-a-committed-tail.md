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

If none is found, `P` is a torn append (or a clean end) and is handled exactly as before.

**False positives** (a torn append judged to be damage). A slot of random bytes passes only if its length is in
range (at most `Z / 2^32`), its flags are zero (`2^-32`), its sequence number falls in a window of at most `Z / 40`
values (`2^-42` for a 128 MiB segment) and its CRC matches (`2^-32`): below `2^-106` per slot, below `2^-82` over the
`2^24` slots of a 128 MiB segment. A final seal needs the marker (`2^-32`) and a CRC match within the window. Stale
frames copied into a payload (an earlier record of this journal, or another journal's) carry sequence numbers outside
the window and are rejected; the sequence check costs nothing and is what makes this so. Without an attacker, the
failure rate is therefore zero in practice, with one exception that no rule can remove: **a payload that contains a
CRC-valid frame of this journal's next records** at an 8-byte-aligned position is byte for byte the same as committed
records behind a damaged commit word. If the writer dies while writing such a payload, recovery preserves the torn
bytes in `quarantine/` and replaces the segment (`Quarantine`; readers at the end get `Error::Discarded`; nothing is
lost) or refuses to open (`Fail`). Only payloads built from this journal's own upcoming frames do this
(`tests/recovery.rs::panic_in_fill_then_shorter_append` builds one on purpose; it panics instead of being killed, so
its bytes are zeroed before recovery sees them).

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
can cause); and for a final seal at most `(Q - P) / 40` CRCs of 12 bytes. It runs once per recovery, at the one
empty slot where the scan stops, and in `verify`.

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

### What cannot be detected

The **newest record's** commit word zeroed, with nothing after it, is byte for byte what a writer killed after
writing the CRC and before the commit store leaves. Recovery handles it as a torn append and zeroes the record;
`verify` cannot report it. A zero-copy reader that held that record's payload sees it change, but only after
something other than a tickertape writer changed the file (the zeroed word), which `open_mapped`'s precondition
already excludes. Damage that zeroes a commit word and everything after it leaves nothing to find or preserve.

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

- New `CorruptReason::EmptyCommitWord(seq)` (the enum is `#[non_exhaustive]`). `inspect::verify` and
  `tickertape verify` report it at the empty word, in the newest segment too.
- `Fail` now refuses states it used to "clean up" by zeroing, because those bytes were committed. After a power loss
  that wrote pages back out of order, records after the checkpoint are preserved in `quarantine/` (or the open is
  refused) instead of zeroed; C8 allowed losing them either way.
- Spec: [`format.md`](../format.md) → Recovery (the table and steps), [`crash-contract.md`](../crash-contract.md)
  (C8, C11, readers and memory safety). Tests: `tests/committed_tail.rs`, `tests/kill_loop.rs::kill_inside_fill`,
  the soak's "zeroed commit word" injection, unit and property tests of `format::find_history_after_empty`, and new
  `recover_dir` fuzz seeds.
