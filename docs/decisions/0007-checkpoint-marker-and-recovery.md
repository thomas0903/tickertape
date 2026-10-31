# 0007: Checkpoint marker and recovery scope

- **Status:** accepted (owner approval, 2026-10-31)
- **Date:** 2026-10-31

## Context

The first draft recovered only the newest segment. The cross-provider review showed that this is not enough: after
a power loss, an older segment written since the last checkpoint can have a hole while newer ones survive, and
recovery would resume after the newest segment, leaving a gap. Scanning the whole journal at every open is correct
but takes time proportional to all retained data (hundreds of GB for a long-running recorder). The review also showed
that repairing corrupt bytes in place can change memory that a zero-copy reader is borrowing.
Spec: [`format.md`](../format.md) → Checkpoint marker, Recovery.

## Options considered

1. **Scan everything at every open.** Simple and correct, but open time grows with retention.
2. **A checkpoint marker**: after each successful checkpoint, record where it ended. Recovery trusts the journal
   before the marker and validates the full chain after it. Open time is proportional to the data written since the
   last checkpoint.
3. **Marker inside the segment header**, updated in place: avoids a file, but makes headers mutable, which readers
   map and validate.

## Decision

Option 2. A separate `checkpoint` file, created with the journal, with two 64-byte slots 64 KiB apart, written
alternately with a generation counter and a CRC, so a torn write is unlikely to destroy the last good marker (and if
both are lost, recovery falls back to a full scan). It is written only after the data and directory syncs succeed.
Recovery:

- starts at the marker (or the oldest segment if there is none), validates every record and seal through the chain
  of segments, and stops at the first invalid position;
- refuses to open, under every policy, if the marker's segment or a segment in the middle of the chain is missing:
  recreating it could make a stale marker look valid, and silently discarding the rest of the journal is worse than
  asking a human;
- cleans an uncommitted tail in place (no reader ever saw those bytes), and syncs that before appending, so stale
  bytes can't resurface after a later power loss;
- repairs a corrupt committed tail by writing a corrected copy of the segment and renaming it over the original,
  never by editing mapped bytes; readers notice the new inode and return `Error::Discarded`;
- moves every segment in the scanned range that isn't on the valid chain into `quarantine/`, wherever its name falls,
  instead of resuming after it or leaving it for `seek` to find;
- preserves everything it removes in `quarantine/` first, durably: bytes are copied to a new file published with
  `link` (never replacing), whole segments are synced and then moved; the directories are synced and the platform
  barrier applied before any destructive step. `RecoveryPolicy::Fail` refuses instead.

## Consequences

- One more file in the journal directory and one more sync per checkpoint (ADR 0004).
- Journals that are never checkpointed are fully scanned at every open; documented in the API.
- Records before the marker are not re-validated at open; corruption there (bit rot) is found by readers and
  `tickertape verify`, not by recovery.
- Readers must detect a replaced or removed segment and return `Error::Discarded`; applications re-`seek`.
