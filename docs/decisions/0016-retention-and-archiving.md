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

(Amended 2026-11-22 by ADR [0044](0044-retention-without-a-backlog-wait.md): "sealed" is checked from the end of the
file for a deletion, which no longer reads the records it discards; a move still reads and validates the whole
segment.)

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
  `Writer::last_retention_error()`. (Amended 2026-11-22 by ADR [0044](0044-retention-without-a-backlog-wait.md): a
  run on the background checkpoint thread stops early when a checkpoint or `close` waits for it, and the next run
  continues.)
- **CLI:** `tickertape archive <DIR> (--to <DEST> | --delete) [--keep-segments N] [--max-bytes B] [--older-than DUR]
  [--dry-run]`. It is the only CLI command that changes a journal; it takes no writer lock (it doesn't need one).

**Readers** positioned inside a segment that is removed keep reading it (they hold it open) and get
`Error::Discarded` at its end; `seek` to continue. Readers before the oldest remaining segment get
`Error::SeqUnavailable`. (Amended 2026-11-19 by ADR [0034](0034-durable-only-reads-check-segment-identity.md): a
durable-only reader may get `Error::Discarded` earlier, when the checkpoint moves past its cursor.)

*Note, 2026-11-27.* `Reader::open` starts at the oldest segment that remains. When the oldest segment of its listing is
gone before it opens it, it opens the next one of the same listing (it keeps the 8 oldest bases of each listing), and
lists again only once those are gone too; after 8 listings it returns the last I/O "not found" error, as before. Until
then it listed again after each lost try. A retention run removes segments one after another, oldest first, and makes
each removal durable; where that sync does nothing (tmpfs), a removal took about as long as the reader's listing and
open, the run removed each segment a new listing named, and a reader opening while one run caught up dozens of segments
lost all 8 tries (`tests/empty_slot.rs::live_readers_of_a_killed_writer_see_no_false_error` on tmpfs; deterministic in
`tests/retention.rs::reader_open_keeps_up_with_a_retention_run_as_fast_as_its_listings`). Nothing older than a listed
segment appears later (crash contract C14), except a segment an archiver puts back (ADR
[0055](0055-an-archiver-takes-a-segment-out-before-it-lets-it-go.md)); C14 already lets a reader that opens while it is
out start after it. So the next listed segment that opens is the oldest that remains. The bound is up to 64 failed opens
(8 listings, each trying up to its 8 oldest segments) before the I/O "not found" error. `seek` keeps listing again: when its segment was the oldest one, the answer is
`Error::SeqUnavailable` from a new listing.

## Consequences

- "Segments are never deleted automatically" becomes "never deleted unless a retention policy is configured". The
  crash contract gets an entry for retention (never removes the checkpoint segment or anything after it; an
  interrupted move never loses a segment), with tests.
- Retention depends on checkpoints: a writer that never checkpoints never frees space.
