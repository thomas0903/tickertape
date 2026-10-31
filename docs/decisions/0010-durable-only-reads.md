# 0010: Durable-only reads

- **Status:** accepted
- **Date:** 2026-10-31

## Context

The crash contract warns that a reader can see, and act on, a record that a later power loss erases; the writer then
reuses that sequence number for a different record. Applications that must not act on such records (risk checks,
order audit trails) need a way to read only what is durable. The checkpoint marker (ADR 0007) already records,
durably, the first sequence number that is *not* covered by the last successful checkpoint.

## Options considered

1. **Leave it to applications** (the writer tells readers out of band how far it has checkpointed). Works, but every
   application re-implements it, usually without the ordering details.
2. **A reader option that reads the checkpoint marker.** No format change: the marker is already written only after
   the data and directory syncs succeed. Readers stop at the marker's `next_seq` and wait for the next checkpoint.
3. **A durable sequence number inside segment headers.** Needs mutable headers (rejected in ADR 0007).

## Decision

Option 2. `ReaderConfig::durable_only(true)` makes a reader return a record with sequence number `s` only if
`s < D`, where `D` is the `next_seq` of the current checkpoint marker (`docs/format.md` → Checkpoint marker); with no
valid marker, `D = 0`. When the next record is not yet covered, the reader behaves as if it had not been written:
`try_next` returns `None`, and `next_timeout` keeps polling, re-reading the marker (with `pread`, no mapping) at most
once per poll. The marker only moves forward within a journal's life, so a reader caches `D` and re-reads only when
it is blocked by it.

The marker is written after everything it covers is durable, and recovery never modifies records before the marker,
so under the crash contract's storage assumptions a durable-only reader never returns a record that a power loss can
remove, and never gets `Error::Discarded` for records it has returned. Records are still validated as usual.

The CLI exposes it as `tickertape tail --durable-only` and `tickertape export --durable-only`.

## Consequences

- Durable-only readers lag the writer by up to one checkpoint interval; the writer controls that interval.
- If the writer never checkpoints, a durable-only reader returns nothing. This is documented, not an error.
- If a checkpoint fails (the writer is poisoned), the marker stops moving and durable-only readers stop advancing,
  which is the safe direction.
- No on-disk format change; older journals work unchanged.
