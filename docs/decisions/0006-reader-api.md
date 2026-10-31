# 0006: Reader API: borrowed records with copy-out

- **Status:** proposed (M1 approval gate)
- **Date:** 2026-10-31

## Context

Readers want to process records without copying, but a borrowed slice into a shared mapping is only sound if the
bytes cannot change and the mapping cannot go away while it is borrowed. Readers also need replay from any sequence
number and live tailing without wake-up primitives.

## Options considered

1. **Lending API:** `Reader::try_next(&mut self) -> Result<Option<Record<'_>>>`, where `Record` borrows the payload
   from the reader's mapping until the next call. Zero-copy, and the borrow checker enforces the bounded lifetime.
   It can't implement `Iterator` (lending iterators aren't expressible there), so loops use `while let`.
2. **Copy-out only:** return owned `Vec<u8>` payloads. Always sound even against misbehaving processes, but one
   allocation and copy per record.
3. **Long-lived borrowed records** (`Record<'journal>` valid while the journal is open): requires keeping every
   visited segment mapped, and makes it easy to hold references across segment changes.

## Decision

Option 1 as the main API, with `Record::to_owned() -> OwnedRecord` (and `Reader::next_owned`) for copy-out. The
reader maps segments read-only; a `Record<'_>` borrows `&mut Reader`, so the reader cannot move to the next segment
(and unmap the old one) while a record is borrowed. Soundness rests on the writer contract: committed bytes are never
modified (ADR 0003). Payload slices never cover a commit word. The crash contract documents the cooperating-processes
assumption and SIGBUS risk; readers that can't trust the other processes should copy out.

Positioning and waiting:
- `Reader::open` positions at the first record of the oldest segment. `seek(seq)` positions so that the next record
  returned is `seq`; if `seq` is not written yet, the reader waits for it.
- `try_next` never blocks: `Ok(None)` means no new record yet. `next_timeout` polls with the configured `Backoff`
  (spin, then yield, then sleep with exponential growth up to a cap) until a record arrives or the timeout passes.
- Every record is validated (length, CRC, sequence) before it is returned. An invalid record is returned as
  `Error::Corrupt` and the reader does not advance, so a retry after a writer recovery can succeed.

## Consequences

- No `Iterator` impl; a `for_each`-style helper or a copy-out iterator may be added if users ask.
- Python and other languages don't get live zero-copy access (a non-goal); they use the exported Parquet.
- Readers never take locks and never write, so any number of them can run, including on read-only copies of a journal.
