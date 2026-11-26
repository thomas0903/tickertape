# 0056: Recovery refuses a checkpoint marker whose position its segment's records do not reach

- **Status:** proposed
- **Date:** 2026-11-26

## Context

Issue #87. A slot of the checkpoint marker names a segment (`segment_base_seq`), an `offset` in it and `next_seq`, and
asserts: every record with a smaller sequence number was durable when the slot was written, and `next_seq`'s record (or
a seal) starts at `offset` (`format.md` → Checkpoint marker). Recovery checks only that the segment exists and that the
offset is a multiple of 8 in the record area; otherwise it refuses with `DanglingCheckpoint` under every policy
(`format.md` → Recovery, step 2). It then starts scanning at `offset`, expecting `next_seq`, and reads nothing before
it (ADR 0007: "Records before the marker are not re-validated at open").

So when the segment holds fewer records than the marker covers, recovery finds an empty commit word at `offset`, takes
it for the clean end, and the writer appends `next_seq` there. The records between the segment's real end and `offset`
are gone, every reader stops at that place for ever (with ADR 0050: `EmptyCommitWord`, because the marker covers it),
and `verify` reports it, but the writer has made the hole part of the journal's history. The issue's cause is an operator
restoring segments from a backup while keeping a newer `checkpoint` file; the same state follows from storage damage to
the last records the marker covers.

What recovery can check cheaply against a slot: that the record `next_seq - 1` is in the marker's segment and ends,
with its padding, exactly at `offset` (or, at offset 64, that `next_seq` is the segment's `base_seq`). It cannot check
older covered records without reading what the marker exists to let it skip.

**Which states can contradict the marker, and which are legitimate.** On storage that honours the sync calls, no crash
leaves a durable slot ahead of its data: a checkpoint makes every dirty segment durable (steps 1–3) before it writes the
slot (step 4), recovery never changes a byte before the marker's position (ADR 0034, F3), retention never removes the
marker's segment (C14), and a slot torn in its write fails its CRC, so the other slot, whose data was durable before it
was written, is current. The states that look similar and must keep opening, each tested (below): a torn or zeroed tail
at or after the marker; an in-place zeroing or a repair by replacement after the marker, which keeps every byte before
it; a roll in progress (the marker at the old segment's end, before or after its seal was written, or at offset 64 of
the new segment); the newest segment after the marker's missing and created again (step 4); the newer slot torn, with
the older current; a short marker file; a journal after retention. A contradiction then needs something outside the
contract: a restore of segments older than the marker, storage that lost a flushed write or wrote the marker before
the data (storage assumption (a)), damage to the covered records (assumption (b), or bit rot), a write-back error that
went unreported (`crash-contract.md` → Segments the writer no longer holds, the limit; and Errors are sticky), or
outside modification.

## Options considered

1. **Keep opening (1.0.0).** Cheap, but it turns a detectable contradiction into permanent history: the writer
   continues at `next_seq`, so records the marker vouched for are missing for ever, and the open reports nothing.
   Recovery already refuses the same contradiction when it is visible from the directory listing (a missing segment),
   "because the history the marker vouches for is missing".
2. **Refuse under every policy, checking only the record right before the marker** (chosen).
3. **Fall back to the other slot when it is consistent, or under `Quarantine` continue after the segment's real end.**
   Rejected: the newer slot's existence shows that its records were once durable, and durable-only readers may have
   returned them (C13). Continuing from an older position reuses their sequence numbers for different records, which is
   a decision about history, not a repair: `Quarantine` preserves bytes, and here there are none to preserve (the
   missing records are not in the directory). The older slot is also as stale as the newer one after a restore.
4. **Validate the whole segment up to the marker, or every covered segment.** Detects more damage, but open time would
   grow with the segment size (and with the journal), against ADR 0007's bound, for states `verify` already reports.

## Decision

**Rule** (`format.md` → Recovery, step 2): with a current checkpoint, after the existing checks, the marker's segment
must hold, right before `offset`, a valid record (the record rules of `format.md`) with sequence number `next_seq - 1`
that ends, with its padding, exactly at `offset`; at `offset` 64, `next_seq` must equal the segment's `base_seq`.
Otherwise the open is refused under every policy with `Error::Corrupt`, reason `DanglingCheckpoint(segment_base_seq)`,
reported at the segment's path and the marker's offset (a missing segment or an invalid offset stays reported at the
`checkpoint` file, offset 0). The check runs before step 2a and before anything is changed, so a refusal leaves the
journal exactly as a dangling marker does today (C11: lock file, removed temporary files; every seek index kept). The
writer appends nothing and never rewrites the marker.

**Why `DanglingCheckpoint`, not a new variant.** It is the same contradiction (the marker vouches for history the
journal lacks) with the same remedy, and the variant's code exists in the C ABI (13), Python (`dangling_checkpoint`),
C++ and Java. A new variant would be allowed (`#[non_exhaustive]`) but would add a status code, enumerators and
mappings in four bindings for a state an operator handles identically. The report's path tells the two apart: the
segment and the marker's offset for this case, the `checkpoint` file for the others. The variant's documentation and
message change; messages are not covered (ADR 0043).

**How recovery finds the record.** Records do not record their predecessor's offset, so it searches backward from
`offset - 40` in steps of 8 for a commit word `len` with `align8(len)` reaching exactly `offset`, whose `seq` field is
`next_seq - 1` and whose `flags` are zero, and validates the first such candidate fully (CRC32C). Candidates inside a
long record's payload are possible only if the payload holds a frame header for exactly that sequence number and
length; a payload holding a whole valid frame of that kind is accepted, as a reader would accept it. The CRC32C work is
bounded by twice the bytes between offset 64 and `offset`; past that (only payloads that hold many such headers) it
walks the segment forward from offset 64 instead and accepts if that walk lands exactly at `offset` with `next_seq`.

**Cost on a healthy open.** The backward probe reads the commit words from the marker's offset back to the start of
the last record, and the CRC32C covers that record once: work proportional to that record's length, independent of
the journal's and the segment's size. For records up to a few KiB the bytes are in the page recovery reads at `offset`
anyway, or the one before it, so a cold open reads a page or two more at most. Measured on an Apple M4 Pro (APFS, warm page
cache, 128 MiB segments, median of 21 opens, two runs each): with the marker at the end of a segment of 1,040-byte
records, 0.6 and 0.9 ms with the check against 1.3 and 1.6 ms without (run-to-run noise exceeds the effect); with a
64 MiB last record, 25 and 26 ms against 14 and 17 ms (the CRC32C of that record). A cold open with such a record reads
it from disk once. The published figures (`benchmarks.md`, writer open at scale) have small records and are not
re-measured. The refusal path probes up to the start of the segment.

**What changes for a journal with damaged covered records.** 1.0.0 opened a journal whose last record before the
marker's offset, in the marker's segment, was zeroed or failed validation, and appended after it; `verify` and readers
reported the damage at that record then and later. Now the open is refused under both policies. Damage to any earlier
covered record, in that segment or an older one, is unchanged: the open succeeds and `verify` reports it. When the
marker is at offset 64 nothing before it is read (the previous segment is not opened), so that case is unchanged too.

**What an operator does after a refusal** (`operations.md`): run `verify` (it names the place where the records end
or the damage is) and `status` or `inspect` (the marker's segment, offset and `next_seq`). Then either restore the
`checkpoint` file that belongs to the restored segments (the backup's), or restore the damaged segment from a copy; or,
knowingly, move `checkpoint` out of the directory: the writer then opens without a marker and validates from the oldest
segment, continuing where restored records simply end and treating a damaged record with records after it like any
corrupt position (`Quarantine` preserves the bytes from it on and the later segments in `quarantine/`, `Fail`
refuses); the sequence numbers the old marker covered beyond the last valid record go to new records, durable-only
readers see nothing and retention removes nothing until the next checkpoint. No command sets the marker back, and
this change adds none.

## Consequences

- A behaviour change in a 1.x release, argued as a bug fix (ADR 0043): `format.md` says the marker's history is
  missing when it names a segment that is missing and refuses; it never allowed recovery to append into a gap that the
  marker covers, and the result contradicts the sequence rule (every sequence number in the journal exactly once, in
  order) that readers check. No journal a correctly working tickertape writer leaves on honest storage is refused.
- Tests: the issue's restore; the damaged last covered record (zeroed, bad CRC); `next_seq` disagreeing with the
  record before `offset`; a marker at offset 64 with the wrong `next_seq`; both policies; both slots; and every
  legitimate state above (`tests/marker_position.rs`). Two sweeps, sampled in the normal run and complete with
  `--ignored` in release: a marker forged at every position a writer can give one (755 record boundaries, seals and
  first slots of a five-segment journal, 1,510 opens, all succeed with no recovery action), and 1,161 crash and damage
  states after the marker (cut at a slot, cut with the later segments kept, a commit word zeroed; at every slot of the
  four segments after the older of two markers, with the newer slot current and torn): `Quarantine` opens all of them,
  and `Fail` refuses 678 opens, each for the damage it refused before, none as a dangling marker. The fuzz target
  `recover_dir` keeps its invariants (a refusal is an allowed outcome).
- Docs: `format.md` (Recovery, step 2; work), `crash-contract.md` (C11, storage assumptions, Missing segments),
  `operations.md` (Moving a tape → Restoring from a backup: restore the marker with the segments; Limits; Checking a
  journal; After a crash: a refused open), `cli.md` (what `status` does not check), `CorruptReason::DanglingCheckpoint` and
  the bindings' descriptions of code 13, the CHANGELOG.
- Left open: `status` does not predict this refusal (its `checkpoint.problem` set is not declared extensible in
  `cli.md`); retention and `tickertape archive` still accept such a marker (they remove only segments before its
  segment, which this contradiction does not make unsafe); a marker at offset 64 whose previous segment lacks records is
  not detected; and there is no tool that sets the marker back.
