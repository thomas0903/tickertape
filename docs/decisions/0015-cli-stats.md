# 0015: Streaming CLI statistics

- **Status:** accepted
- **Date:** 2026-11-01

## Decision

`stats` aggregates selected records during one nonblocking safe-reader startup scan, ending at its first
uncommitted slot or inclusive upper sequence bound. As with export's capture scan, this is an observation
during scanning, not an atomic snapshot of a live writer. No journal records are reread to produce output.
Freeze the optional checkpoint boundary before scanning; emit nothing on corruption or input I/O failure.

Keep payload length counts in a fixed histogram: exact integer lengths 0–4096, then power-of-two upper
bounds. Use nearest-rank percentiles. Actual extrema and per-schema means remain exact.

Both timestamp fields use fixed UTC one-second buckets, even for ordered input; this makes their meaning
independent of timestamp ordering. Ties choose the earliest second. Zero is unset: exclude it from time
summaries, and count decreases only between adjacent selected records with both fields set. Largest local
gap likewise requires two adjacent records with nonzero local timestamps and a nonnegative difference;
ties choose the first pair in sequence order. A zero span has a null mean rate.

Exact busiest-second counts for arbitrarily unordered input cannot use a fixed-size in-memory map.
Sort seconds in bounded chunks and, when necessary, merge runs using two anonymous temporary files outside
the journal. Temporary disk usage is linear in timestamp count; memory is independent of record count
apart from the public reader's segment listing and per-schema aggregates (bounded by the u32 schema space).
No new format or public library API is needed. Reuse inspect's hand-written UTC conversion.

## Consequences

Stats never takes the writer lock or writes journal entries. Large inputs need temporary disk space;
temporary-file failures return exit 1 without stats output. JSON is a documented contract in `docs/cli.md`
and `stats --help`. Empty or unavailable summaries use null, never NaN or infinity.
