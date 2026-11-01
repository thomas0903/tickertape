# 0016: Retention and archiving of old segments

- **Status:** accepted (owner decision: build the "Later" items, 2026-11-01)
- **Date:** 2026-11-01

## Context

A journal only grows: tickertape never deletes segments (ADR 0005), and `docs/operations.md` tells operators how to
archive old segments by hand without breaking recovery. Long-running recorders need this automated, with the same
safety rules, and as a CLI command.

## Decision

**Eligibility (one rule for everything).** A segment may be archived or deleted only if it is sealed, it is strictly
older than the segment named by the current checkpoint marker, and every older segment is also being removed (removal
proceeds from the oldest end, one segment at a time, and stops at the first segment that isn't eligible or the first
failure). Without a valid checkpoint marker nothing is eligible. This keeps the journal a contiguous chain that the
writer can always open (`docs/format.md` → Recovery refuses a missing checkpoint segment or a gap).

**Policy** (`tickertape::retention::Policy`): any combination of `max_segments` (keep at most N segments in total),
`max_bytes` (keep total segment bytes at or below B), and `max_age` (remove segments sealed longer ago than D, where a
segment's seal time is its successor's `created_unix_ns`). A segment is removed if any configured limit asks for it
and it is eligible. With no limit configured, nothing is removed.

**Action:** `Delete` or `MoveTo(dir)`. A move is a `rename` when the destination is on the same filesystem; otherwise
copy, sync the copy, sync the destination directory, then remove the original and sync the journal directory. An
interrupted move leaves the original, the copy, or both, never neither. Moves never replace an existing file at the
destination (they fail instead).

**Where it runs:**
- **Library:** `retention::plan(dir, &policy) -> Plan` (read-only; what would be removed and why) and
  `retention::apply(dir, &plan, &action) -> Report`. Safe to call while a writer is active: the writer never touches
  eligible segments again (they are older than its last checkpoint).
- **Writer:** opt-in `WriterConfig::retention(policy, action)`; applied after each successful checkpoint (when the
  marker moves) and on open. Failures never fail an append or a checkpoint; the last error is available from
  `Writer::last_retention_error()`.
- **CLI:** `tickertape archive <DIR> (--to <DEST> | --delete) [--keep-segments N] [--max-bytes B] [--older-than DUR]
  [--dry-run]`. It is the only CLI command that changes a journal; it takes no writer lock (it doesn't need one).

**Readers** positioned inside a segment that is removed keep reading it (they hold it open) and get
`Error::Discarded` at its end; `seek` to continue. Readers before the oldest remaining segment get
`Error::SeqUnavailable`.

## Consequences

- "Segments are never deleted automatically" becomes "never deleted unless a retention policy is configured". The
  crash contract gets an entry for retention (never removes the checkpoint segment or anything after it; an
  interrupted move never loses a segment), with tests.
- Retention depends on checkpoints: a writer that never checkpoints never frees space.
