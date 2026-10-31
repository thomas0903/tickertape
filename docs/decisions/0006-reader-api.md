# 0006: Reader API: safe copying reader, opt-in zero-copy

- **Status:** proposed (M1 approval gate)
- **Date:** 2026-10-31 (revised twice after cross-provider reviews the same day)

## Context

Readers want to process records cheaply, but a `&[u8]` into a shared mapping is only sound if no one modifies the
bytes while it is borrowed, and tickertape cannot enforce what other processes do to files. Readers also need replay
from any sequence number and live tailing without wake-up primitives.

## Options considered

1. **Zero-copy only**, safe signature: a `Record<'_>` borrowing the mapping. Fast, but memory safety would silently
   depend on every other process with write access to the files.
2. **Copy-out only**: always sound, one copy per record.
3. **Both, with the zero-copy constructor `unsafe`**: the default is sound unconditionally; callers who want zero-copy
   state the precondition explicitly.

## Decision

Option 3. Both constructors return the same lending API: `try_next(&mut self) -> Result<Option<Record<'_>>>`, where
the record borrows the reader until the next call (so it can't outlive a segment change).

- `Reader::open(dir, config)`: safe. Reads the files with `pread` (in chunks) into a buffer the reader owns and
  validates the copy (ADR 0003). Memory-safe whatever other processes do, and immune to `SIGBUS`.
- `unsafe Reader::open_mapped(dir, config)`: zero-copy. The payload slice points into the read-only mapping. Safety
  precondition: while the reader exists, no process other than a tickertape writer modifies the journal's segment
  files. A tickertape writer never modifies committed bytes (ADR 0007 repairs by replacing files).
- `Record::to_owned()` copies a record out for longer use.

Positioning and waiting:
- `open` positions at the first record of the oldest segment. `seek(seq)` positions so that the next record returned
  is `seq`; if `seq` isn't written yet, the reader waits for it.
- `try_next` never blocks: `Ok(None)` means no new record yet. `next_timeout` polls with the configured `Backoff`
  (spin, yield, then sleep with exponential growth up to a cap) until a record arrives or the timeout passes.
- Every record is validated (length, CRC, sequence) before it is returned. An invalid record is returned as
  `Error::Corrupt` and the reader doesn't advance. A reader that detects its segment file was replaced or removed by
  recovery returns `Error::Discarded` and never continues at its old offset; the application calls `seek`, which only
  lands on positions validated along the current chain (`format.md` → What readers do about repairs).

## Consequences

- No `Iterator` impl (lending iterators aren't expressible there); loops use `while let`.
- The default path costs a copy per record; the benchmarks (M7) measure both readers.
- Python and other languages get no live access (a non-goal); they use the exported Parquet.
- Readers never take locks or write, so any number can run, including on read-only copies of a journal.
