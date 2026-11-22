# 0044: Retention that holds up neither checkpoints nor `close`

- **Status:** Accepted by the maintainers under the owner's delegation, 2026-11-22
- **Date:** 2026-11-22
- **Amends:** [0016](0016-retention-and-archiving.md) (what is validated before a removal) and
  [0038](0038-background-checkpoints.md) (retention on the checkpoint thread)

## Context

Issue #45. Before retention deleted or moved a segment, it read the whole segment and validated every record and the
seal (`retention.rs`: `validate_sealed` → `journal::scan_file`), once in `plan` and again in `apply`, so every
removal read the segment twice. With background checkpoints (ADR 0038) the writer's retention runs on the checkpoint
thread after each checkpoint, and `Writer::checkpoint`, `Writer::close` and dropping the writer waited for the whole
run. Behind an unpaced 1,024-byte writer that outruns the disk on Google Cloud VMs with local SSD and 64 GiB of
retention, the old segments were no longer in the page cache, so each removal read whole segments back from disk:
retention fell tens of GB behind, the final checkpoint and `close` waited for it, and the benchmark timed out
(`docs/benchmarks.md` → Operating envelope). Nothing was lost and nothing wrong was reported; the limit is on the
operating envelope.

Two questions: what a removal has to read, and how to keep a checkpoint and `close` from waiting for a backlog of
removals. Constraints: format version 1 unchanged; the crash contract's retention guarantees (C14) kept; `unsafe` only
where it already is; no new option in the C ABI.

## What the full scan protected

From the code and the contract:

1. **The journal stays a chain the writer can open.** Retention removes only from the oldest end, stops at the first
   ineligible segment, removes nothing at or after the checkpoint segment, and checks before each removal that the
   segment is the oldest listed and that its successor is the next one listed (`apply`'s listing check). Recovery
   with a valid marker starts at the marker's segment and trusts everything before it (`docs/format.md` → Recovery,
   step 2), so it never reads a segment retention may remove. Without a valid marker it starts at the oldest segment
   that remains, which after a removal is the successor, a segment whose header retention checked. None of this
   depends on the records inside the segment removed.
2. **Never removing a segment the checkpoint still needs.** That is the position check against the marker (read again
   before each removal, and made durable first by an archiver), not the scan.
3. **Never removing past a broken chain.** The scan checked that the segment ends in a valid seal naming the successor
   listed after it, so that retention does not remove a segment whose seal is damaged or missing, or which names a
   segment that is not there (a missing middle). This is what "sealed" means in C14.
4. **Noticing corrupt records.** A damaged record anywhere in the segment stopped the removal with
   `Error::Corrupt`. For a deletion this protects nothing the journal keeps: the bytes are discarded either way, and
   C14 promises nothing about them. For a move it matters: the archive receives the segment as the copy of record,
   and the soak test checks that every segment retention moves is a sealed prefix of valid records.

So a deletion needs (1)–(3), and only (3) read the segment's data.

## Options considered

1. **Establish the seal from the end of the file (chosen).** The format makes every byte after a seal zero
   (`docs/format.md` → Segment file), and a seal is eight bytes at an 8-byte-aligned offset. So the 8-byte word
   holding the last non-zero byte of the record area is the seal, if the segment is sealed: reading backwards from the
   end of the file over the zeros finds it in one small read in the usual case (what was left free when the segment
   rolled is less than the frame that did not fit, plus the 8-byte trailer). This works for any segment, whoever
   sealed it, and for an archiver in another process.
2. **Remember the seals this writer wrote.** No read at all for segments this writer sealed, but segments inherited
   from an earlier writer, and every segment an archiver removes, would still need the full scan; and the writer would
   carry state about segments it no longer holds. Option 1 is cheap enough for both.
3. **Keep the full scan for deletions and only move retention off the checkpoint path.** `close` and the next
   checkpoint would still wait for the segment being scanned, and the disk would still be read back in full for data
   being discarded; under overload that read competes with the writer's own writes.
4. **A fixed number of removals per checkpoint.** Bounds the wait, but a writer that fills more segments per
   checkpoint interval than the limit could never catch up, even with nothing else waiting.

## Decision

**What is checked before a removal.** For both actions, as before: the plan belongs to this journal directory; the
segment is the oldest listed and is still the file planned (same device and inode); its header and its successor's
header are valid; the successor is the next segment listed; the checkpoint marker is valid (made durable first by an
archiver, crash contract C14) and the segment is strictly older than its segment. Then:

- **Delete** checks the seal from the end of the file: the last non-zero 8-byte word of the record area is a valid
  seal (commit word `0xFFFFFFFF`, CRC matching) naming the successor, not at the first slot. It reads backwards from
  the end, 4 KiB first and twice as much each time up to 1 MiB. Its records are not read. If the end is not such a
  seal, the whole segment is scanned as before, so the error (`BadSeal`, `BrokenChain`, or what the scan found first)
  is the one the full scan gives, and a segment the full scan accepted is still accepted.
- **Move** (`MoveTo`) reads the whole segment, from the file it checked the identity of, and validates every record
  and the seal naming the successor, as before; only then is it renamed or copied.
- **`plan`** checks every candidate as a deletion does, so it reads one header per segment and the end of each
  candidate. A damaged record inside a sealed candidate no longer makes the plan fail; a move finds it when it reaches
  that segment and stops there, with the earlier segments moved (as for any error of `apply`).

What a deletion no longer establishes: that the records in the segment are valid, and that the seal it finds is the
one the chain of records reaches. A segment whose seal was lost and whose last record happens to end, at an 8-byte
boundary, with exactly the eight bytes of a valid seal naming its successor is taken as sealed and deleted (a move refuses it). Such a segment is
before the checkpoint segment and its successor is in place, so the journal is still a chain; the test
`tests/retention.rs::deletion_takes_the_last_non_zero_bytes_for_the_seal` pins this behaviour. A caller who wants
damaged records found before they are deleted runs `inspect::verify` (CLI `tickertape verify`) first, or moves instead
of deleting; no option is added for it.

**Retention on the checkpoint thread yields.** The writer's retention run after a background checkpoint stops before
its next removal, once it has handled one planned segment, as soon as a checkpoint is queued, or `Writer::checkpoint`
or `Writer::close` waits for the thread, or the writer is dropped. The run after the next checkpoint plans again and
continues. Planning stops early in the same way once it has one candidate. So the time from a request to a durable
marker, and the time `close` takes, include at most the run's planning (one directory listing and one header read per
segment, plus the end of one candidate) and one removal for each job still to finish, not the backlog: when one job
is running and another is queued, each is followed by such a bounded run, so the wait covers at most two. A candidate
whose end is not a valid seal costs that run a full read of the segment, and every later run again until the segment
is repaired or removed by hand (`Writer::last_retention_error` reports it). A run that was not interrupted removes
everything the policy asks for, as before. Each run handles at least one planned segment, so under a constant queue of
checkpoints retention still makes progress, one segment per checkpoint at least.

Unchanged: the writer's retention at open and after a blocking `Writer::checkpoint` runs on the writer's thread until
it has removed what the policy asks for or fails (nothing else waits for the writer's thread there); an archiver's
`retention::apply` applies its whole plan.

**Visibility.** Retention behind is not an error. It shows in what exists: `status` → `segments.before_checkpoint`
(segments older than the marker's segment, the ones retention could remove) and `segments.total_bytes` with the
storage headroom thresholds, `Writer::retention_report`, and `retention_failures` for runs that fail. No counter or
field is added.

## Consequences

- Crash contract C14 states exactly what is validated before a deletion and before a move. `docs/operations.md` says
  the same in brief.
- Callers can see: `plan` (and `tickertape archive --dry-run`) lists a sealed segment with damaged records instead of
  failing; `apply` with `Delete` removes it, and with `MoveTo` stops at it with `Error::Corrupt` after moving the
  segments before it; with background checkpoints, `Writer::checkpoint`, `Writer::close` and drop no longer wait for
  the whole retention run of the checkpoint thread (a blocking `Writer::checkpoint` then runs retention to the end on
  the calling thread, as before), and segments that were eligible can remain after `close` until a later run. No API item
  changes; the `fault-injection` feature gains `Point::RetentionRemove` and `Point::CheckpointWait`.
- Tests: `tests/background_checkpoint.rs::retention_backlog_holds_up_neither_a_checkpoint_nor_close` (28 eligible
  segments, each with a damaged record; the queued checkpoint reaches its marker after one removal, `close` returns
  after one more, and a later writer's open removes the rest without an error); `tests/retention.rs::`
  `damaged_record_in_a_sealed_segment_is_deleted_but_never_moved`, `::broken_seal_is_never_removed` (a damaged seal, no
  seal, and a seal naming another segment, before and after the plan, under each action),
  `::deletion_takes_the_last_non_zero_bytes_for_the_seal`.
- Still to measure: the 1,024-byte sustained scenarios with background checkpoints and 64 GiB of retention on the
  machines of issue #45, and the 64-byte runs at 90 % of the probe rate (`docs/benchmarks.md` → Operating envelope).
  Under sustained overload retention can now fall behind instead of holding up the checkpoints; whether it keeps up
  there, and the storage it needs meanwhile, is what those runs show. They are recorded in `docs/benchmarks.md` →
  Second sustained session and → Long verified runs (`3fea1c0`): every session run finished and retention held the
  10-minute probes at their limit; in the long runs under forced compaction it removed almost nothing, for a reason
  not yet known (issue #45).
- Format version 1 is unchanged; no `unsafe` was added; nothing changes in the C ABI or the bindings, which use the
  Rust retention code.
