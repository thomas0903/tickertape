# 0007: Checkpoint marker and recovery scope

- **Status:** proposed (M1 approval gate)
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

Option 2. A separate `checkpoint` file with two 64-byte slots in separate 4 KiB blocks, written alternately with a
generation counter and a CRC, so a torn write can't destroy the last good marker. It is written only after the data
and directory syncs succeed. Recovery:

- starts at the marker (or the oldest segment if there is none), validates every record and seal through the chain
  of segments, and stops at the first invalid position;
- cleans an uncommitted tail in place (no reader ever saw those bytes), and syncs that before appending, so stale
  bytes can't resurface after a later power loss;
- repairs a corrupt committed tail by writing a corrected copy of the segment and renaming it over the original,
  never by editing mapped bytes; readers notice the new inode and remap;
- moves segments disconnected from the valid chain into `quarantine/` instead of resuming after them;
- copies everything it removes into `quarantine/` first, durably (unique names, exclusive create, file and directory
  synced), so nothing is ever silently destroyed. `RecoveryPolicy::Fail` refuses instead.

## Consequences

- One more file in the journal directory and one more sync per checkpoint (ADR 0004).
- Journals that are never checkpointed are fully scanned at every open; documented in the API.
- Records before the marker are not re-validated at open; corruption there (bit rot) is found by readers and
  `tickertape verify`, not by recovery.
- Reader code must handle a replaced or removed segment (remap, or report that recovery discarded records).
