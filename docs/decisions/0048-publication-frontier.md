# 0048: The zero-copy reader's publication frontier

- **Status:** proposed (design only, nothing is built; issue #62)
- **Date:** 2026-11-25
- **Follows up:** [0027](0027-mapped-reader-validates-in-place.md), "Which loads can race". It would narrow the known
  limitation stated there; 0027's decision stands.

## Context

`unsafe Reader::open_mapped` validates a record from atomic loads of the mapping and hands out a `&[u8]` into it. How
much it loads comes from the record's commit word. ADR 0027 named two kinds of storage damage to a commit word after
which those loads, or a payload the caller holds, can meet a writer's stores, and left a follow-up: bound the loads by
a separate shared word that the writer advances on every append (a publication frontier). The roadmap listed the
missing record as a gap of 1.0.0.

Constraints: format version 1 and C ABI version 1 do not change; 1.x may narrow a known limitation, with a record, and
may weaken nothing ([`compatibility.md`](../compatibility.md) → Crash contract); the zero-copy reader's p99.9 at 64
bytes is 0.5–1.8 µs on bare metal, so tens of nanoseconds count.

### What can go wrong

**Nothing, on an intact chain.** The reader loads bytes only of a frame whose commit word it has acquired, and the
writer never stores to them again (ADR 0027). That holds across writer crashes and recovery, in one process or two.

The cases below need a damaged commit word:

1. **Enlarged length.** An earlier record's length is enlarged, so that its frame reaches past the writer's position
   `Q` *in the segment the writer is appending to* (a longer length in any other segment meets no store of a live
   writer). Validating it loads bytes the writer is storing.
   - *A record the checkpoint marker covers.* Recovery starts at the marker and does not see the damage. Reachable
     end to end: in a scratch program, a covered length set to 20 frames, 10 of them past `Q`, survived
     `Writer::open` (no recovery action), and an `open_mapped` reader then replayed into it under a live writer.
   - *A record after the marker.* Recovery validates it, finds the damage and repairs it (in the same program, one
     action). So this needs damage that appears **while a writer runs**: a clean page evicted and read back damaged,
     or a memory error. The contract says "while no writer runs", which is narrower than what can happen.
2. **Zeroed commit word.** A published record after the marker loses its commit word, and the next recovery cannot
   tell that from a torn append (C11 (3)). It zeroes that record and those after it in place, under open readers.
   - *2a.* A payload the caller still holds changes.
   - *2b.* The reader's later loads meet recovery's and the new writer's stores. Its cursor may be inside a new record.

**What each is:**

| | Same process | Another process | Class |
|---|---|---|---|
| Loads of 1 and 2b | Data race, and a mixed-size one (8-byte load, 4-byte atomic commit store) | The same machine accesses; the language says nothing | Undefined behaviour in Rust's terms in one process; outside the model in two. In practice a torn read, which the CRC rejects |
| The payload slice in 2a | A `&[u8]` whose bytes change | The same | Undefined behaviour either way if the slice is used after the change, and a wrong payload. No later check exists |
| A frame of case 1 that passes the CRC by chance | As 2a, and a wrong record | The same | The copying reader can return such a wrong record too, without the aliasing problem |

Three refinements to what ADR 0027 says:

- *"In the same process"* is the strict reading, and we keep it. The writer and the reader map the file separately, at
  two addresses, and Rust's model does not know that those alias. ADR 0003 treats the file's bytes as the location;
  under that reading the race is undefined behaviour. The machine does the same in both deployments.
- *A seal marker or a shorter length in place of a length* loads nothing out of bounds. Only a longer length can.
- *Odds.* A try reads the slot about 20 times in its 50 ms window. If each read were an independent 2⁻³² chance (they
  are not: a retry can read the same bytes), an application that keeps retrying `Error::Corrupt` would make about 400
  reads a second, or one match in about four months of retrying. The frame's own header is intact, so the sequence
  check adds nothing.

### What has been observed

- **Case 1:** no report of it. Reproduced only by writing the damage. The reader reported a checksum mismatch after
  50 ms, as the safe reader did. Miri reports undefined behaviour for a small model of it (an 8-byte atomic load
  against a 4-byte atomic store; nothing in 32 seeds once the load is bounded), as ADR 0027's reviewer found. Loom
  reports the race for another model.
- **Case 2a:** `tests/reader_retention_stress.rs::durable_readers_under_repairs` injects the damage, and
  accepts and counts changed payloads. The one 60 s run made for this record (macOS) had 156
  in-place repairs and 0 changed payloads: the window is small even with the damage made on purpose.
- **No tool checks the real mappings.** Miri has no file mappings. ThreadSanitizer keeps its state per virtual
  address, so it cannot connect two mappings of one page (reasoned, not run). The loom model in the repository covers
  intact chains only.
- Issue #43 (a mapped view that lagged on aarch64 Linux) is a different effect. No frontier removes it, and it means
  no ordering argument over mapped pages is stronger than the kernel's coherence of them.

So the limitation is theoretical in the sense that matters for planning: it needs silent damage to 4 particular bytes,
and nothing has been seen outside injected faults. It is real in the sense that a caller cannot rule it out.

## Options considered

1. **Document, and do nothing more.** Honest and free. But the caller has no way out except `Reader::open`.
2. **A shared frontier word** (next section). Closes case 1 for readers that attached to it, plus part of 2b. Every
   append pays, and it is missing in many deployments.
3. **Use the checkpoint marker as the frontier** (recommended). It exists already, is synchronised independently of
   the lengths and is CRC-protected. No writer change.
4. **A frontier in the segment header or file.** Changes format version 1, and ADR 0003 (option 2) rejected it.
5. **Validate with `pread` whenever the bound is unknown.** A system call per record for a live reader. Rejected for
   the hot path, used on the cold path below.
6. **Reader registration, so that recovery waits.** The only thing that closes 2a for visible readers. It lets a dead
   or slow reader delay a writer, which ADR 0003 excludes. Rejected.

### Option 2: the best shared word I can find, and why not to build it

- **Where.** Not in the segment (format), and not in a mapped file in the journal directory: truncating that would
  raise `SIGBUS` (ADR 0021's reason). The `notify` region (sealed memfd, POSIX shared memory) has room: a `u64` at 256
  for the position and one at 264 for the segment's base, both zero in a 1.0.0 writer's region ("no frontier").
- **What and when.** The obvious orders are wrong or slow. Storing the frame's *end* before the commit word lets a
  bounded load cover the commit word still to be stored; loom finds that race. Storing it after the commit word is
  sound, but a reader can see the commit word first and must wait for a second line. Better: store the **start** of
  the record being committed, with `Release`, after its body and just before its commit word. The reader accepts a
  frame `[P, E)` if `E <= start` or `P == start`. It loads `start` (`Acquire`) after its fence on the commit word, so
  it always sees at least its own record's start and never refuses an intact record. Loom passes this for intact and
  damaged chains. (The newest record's own length is trusted: it has just been stored.) On open the writer stores its
  recovered end. On a roll it seals, stores 64, then the new base; a reader that sees a higher base knows its segment
  is finished, and one that sees its own base with a refused frame loads the base again.
- **Compatibility and lifetime.** A 1.0.0 writer leaves zeros and a 1.0.0 reader ignores the words. After a crash the
  Linux region is reachable only for readers that already mapped it, with a last value that stays true on an intact
  chain. A reader whose frame is refused must re-read the `notify` record to follow a restart.
- **Why not.**
  - *Coverage.* There is no region with `notify(false)`, for a reader of another user, across pid namespaces or
    containers, with `hidepid`, on a read-only copy, or with no writer. Archives and sealed segments without a writer
    need none, but a reader cannot know that no old writer is about to start. Each gap falls back to today's loads, so
    the contract would depend on facts the caller cannot see; failing instead would break working deployments.
  - *Trust.* ADR 0021 promises that the region's contents affect only latency. A frontier that is too low would stop
    reads.
  - *Cost.* One more store per append. For a tailing reader, one more line moved from the writer's core per record: I
    estimate 50–100 ns or more across cores on servers, against a p99.9 of 500 ns.
  - *Benefit.* It does nothing for 2a, or for 2b while recovery runs. Over option 3 it adds case 1 for uncovered
    records, and 2b once the new writer has opened.

## Decision

**Build no new shared word in 1.x. Bound the zero-copy reader's loads by the checkpoint marker**, in a separate change,
if it meets the cost bar below. If it does not, take option 1 and only correct the texts.

**The bound.** For its open segment the reader keeps `limit`:

| Marker against the segment | `limit` |
|---|---|
| names this segment | the marker's `offset` |
| names a later segment | the segment's size (no tickertape writer stores there, and no recovery from this marker does) |
| names an earlier one, or no valid marker, or it cannot be read, or its offset is not a valid slot | 0: no bound, as today |

A slot at `P < limit` whose frame would end after `limit` is not loaded through the mapping. `limit` is the covered
position, a record or seal boundary, so an intact record never crosses it. Slots at or after `limit` are read as today.

**A crossing frame** (cold path, never inlined) is read with `pread` into the reader's own buffer and judged there, so
the error is the one the safe reader gives; this keeps `open_mapped`'s promise that checks, order and errors are those
of `Reader::open`, which rejecting it with `BadLength` would break (the prototype did that). If it validates there, the
reader reads the marker and checks the identity again. If the marker no longer bounds this slot, the slot is read as
today. If it still does, it is a chance CRC match or the marker describes other bytes, and the reader reports
`BadLength`. It never maps the frame.

**Which marker.** A visible reader reads the two 64-byte slots with `pread` (not the 128 KiB that `read_checkpoint`
reads now) before it opens a segment by path, which proves the file was current after the marker was written (ADR
0034). It reads them again, followed by the identity check, when the cursor reaches `limit` and at the 100 ms check at
an empty slot. Failing to read the marker is never an error for it. A durable-only reader uses the `offset` of the
marker it has applied, kept next to `durable_next_seq`, with ADR 0034's order unchanged.

**What this closes:**

| | Visible reader | Durable-only reader |
|---|---|---|
| Case 1, record below the marker it last read | closed | closed |
| Case 1, record after it | open: needs damage under a running writer, or a marker not read again across a checkpoint, close, damage and reopen | does not arise (never read) |
| 2a and 2b | open | do not arise: it returns nothing at or after the marker, and recovery changes nothing before it |

So **a durable-only zero-copy reader is outside the limitation altogether**, while the marker survives. The exception
is the one C13 already makes: damage that also destroys the marker, or makes the older slot current. For 2a and 2b that
is true of 1.0.0 already and simply not written down; the bound adds case 1.

**Ordering.** On an intact chain nothing changes. For a refused frame there is no load to order. For a damaged frame
that ends inside `limit`, the loads reach committed bytes whose own commit words the reader may not have acquired. They
need the marker: the writer's stores precede its hand-over to the checkpoint thread (a mutex), that thread's `msync`
and `fsync`, and then its `pwrite` of the slot. The reader's `pread` returns that slot, and it must issue
`fence(Acquire)` before loading, as ADR 0003 does after `pread` of a commit word. This is the same class of assumption
as ADR 0003's (system calls on shared pages order as threads would), not a new one. Nothing stores to those bytes
again, so a wide load over several old commit words is not a race.

**Crash, roll, no writer.** A crash leaves the marker where it was, at or before the end. After a roll the old segment
is unbounded until the marker passes it, then fully bounded. Archives have no marker and no writer. A journal that
never checkpoints gains nothing, and recovery validates all of it at every open. A marker that was lost or moved back
(damage, files restored by an operator) leaves a reader with an older, larger `limit`, which can cross a new record
after an in-place repair: that is the crossing path above.

**Versions.** Any writer, 1.0.0 included, works with the new reader in full, and a 1.0.0 reader is unaffected.

## Cost

Prototypes of both, in a scratch copy of the crate. **Busy Apple M4 Pro Mac, load average 17 to 27, 7 alternating
rounds, 64-byte payloads. Informal, not a published result.** Median of the rounds' best batch, with the range:

| | 1.0.0 code | Marker bound | Shared word |
|---|---|---|---|
| append, ns | 13.3 (13.0–13.6) | 13.5 (13.3–14.1) | 13.5 (13.2–13.8) |
| zero-copy replay, ns/record | 17.1 (16.9–19.2) | 17.4 (17.1–18.5) | 17.4 (17.2–18.0) |
| paced tail, p50 visible, ns | 83–125 | 83–125 | 125 |
| paced tail, p99 visible, ns | 291–1,250 | 333–1,250 | 291–792 |

The append code of the first two columns is the same, so 0.2 ns there is the noise. No column can be told from another.
The clock ticks every 41.7 ns, so the tail rows cannot show the shared word's extra cache line. The prototypes were
checked to be active: the bound rejected the damaged length of the scratch program before any load, and a deliberately
wrong frontier made an intact record fail. The shared word lacked the base and the refresh.

**Bar for the change:** `replay` before and after on bare-metal x86-64 and aarch64 (the benchmark `reader.rs` needs
anyway), medians within 1 % at 64 and 1,024 bytes; `tt-latency --reader mapped` at 64 bytes and 100,000 records/s,
pinned cores, at least five alternating pairs, with p50 and p99.9 inside the spread of the unchanged build; the cost of
a segment open, and of a tail whose writer checkpoints every 100 ms, recorded. If option 2 is ever reopened: the same,
plus unpaced append throughput with 0, 1 and 4 tailing readers.

## Contract changes when it is built

None needs a 1.x promise broken: every change narrows the limitation, and no item, error value or on-disk byte changes.

- **`Reader::open_mapped`, `# Safety`:** "Known limitation: a commit word damaged on storage breaks the chain, and then
  a mapped load can touch bytes a writer is storing to without synchronisation." gains "This does not apply to a reader
  with `durable_only(true)`, unless the checkpoint marker is damaged as well." The first bullet ("The length in an
  earlier record's commit word is enlarged while no writer runs, and a writer then appends after that record: …")
  becomes "The length in the commit word of a record not covered by the checkpoint marker the reader last read is
  enlarged, and a writer stores past that record". The second bullet and the last paragraph stay. The precondition
  gains: no process other than a tickertape writer modifies `checkpoint` (a forged marker can only cost the narrowing).
- **`crash-contract.md`**, "Readers, the writer and memory safety": the same two changes. "A separately synchronised
  publication frontier, a planned protocol change, would bound the loads in the first case" becomes a sentence on the
  marker bound with a pointer here. C13 gains "a zero-copy durable-only reader's payloads do not change".
  "Known risks and limits" needs no change: the limitation is in the section before it.
- **`compatibility.md`** → Crash contract: "a zeroed commit word lets recovery rewrite published records in place under
  a live reader. The roadmap's publication frontier is not built; if it is, it narrows that limitation" is wrong today:
  a frontier narrows the *enlarged length*, which the paragraph does not mention. Replace it with both cases and the
  durable-only exception. The correction does not depend on the change.
- **`format.md`** → Checkpoint marker: "Readers ignore this file unless `ReaderConfig::durable_only(true)` is enabled"
  gains "except that a zero-copy reader uses it to bound its loads, which changes no result".
- **`c-api.md`, `cpp-api.md`, `java-api.md`, `tickertape.h`, the Java and C++ reader docs** say only that a writer never
  changes committed bytes, without the limitation. They should gain one sentence pointing to it. Text only, so the ABI
  baseline is untouched. ADR 0027 and the `mmap.rs` comments gain a pointer.

## Work in flight

- **ADR 0045 (sparse seek index, #64).** Its step 2 lets zero-copy and durable-only readers use an index only for
  segments before the marker's. That is the second row of the `limit` table, so the two can share one read of the
  marker's slots. Neither the bound nor a shared word lets that step be relaxed: for a sealed segment the marker does
  not cover, the remaining hazard is recovery changing it in place, which no frontier sees. In covered segments a wrong
  index can meet no store, with or without the bound.
- **ADR 0046 (forward seeks, `seek_to_end`, #66).** A seek that scans from the segment's start is the replay of covered
  records in which case 1 arises, so it benefits most. A seek that continues from the cursor already reads the marker
  and then checks the identity: it installs `limit` from that read. `last_record_in_place` uses `pread` and needs
  nothing. `seek_to_end` starts at the marker's position and reads only uncovered records, so it gains nothing; its
  saved `Place` must include `limit`. Whichever lands second carries the few lines.

## Tests

1. **Loom:** add a damaged frame and the marker to `protocol_model.rs`. Without the bound the model must fail (kept as
   an expected failure); with it, pass, for a visible and a durable-only reader, across a hand-over to the checkpoint
   thread. Scratch results: unbounded and end-before-commit fail; marker, start-before-commit and end-after-commit pass.
2. **Miri:** the same on anonymous memory through `MapRo` with a concurrent writer thread: no report, in many seeds.
3. **No load past `limit`:** a test-only counting view of `MapRo`, for every damaged length of a covered record, at,
   just below and above `limit`; and that no mapped load at all is made for a crossing frame.
4. **Errors:** for every such length, zero-copy and safe reader agree on the error (extends `tests/mapped_reader.rs`,
   whose corruption classes get a marker at each position).
5. **Live:** the scratch program as a test: damage a covered record, reopen, append from a thread and from another
   process, replay with both readers, visible and durable-only.
6. **Intact chains never cross:** the proptest journals with checkpoints at random positions, marker at a seal, at
   offset 64 of a new segment, short marker file, dangling marker, one valid slot, unreadable marker.
7. **Marker lost or moved back** with an in-place repair under an open reader: the reader ends where the unbounded one
   does, after the crossing path. Fault points around each marker read and identity check, as for ADR 0034.
8. **Refresh:** the marker moves while the reader is below `limit`, at it, and waiting at an empty slot.
9. `durable_readers_under_repairs` copies a durable-only zero-copy payload at once; it should also hold one across
   restarts and assert that it never changes. Kill loop, soak and
   the compatibility fixtures run unchanged. Mutations: drop the bound, use `<=` for `<`, skip the identity check.

## Consequences

- A caller who needs the limitation gone has an answer, `durable_only(true)`, that can be checked in code review. Today
  only the safe reader is one. The price is the checkpoint's delay.
- The limitation stays in the contract for visible readers, smaller. Case 2a cannot be closed without registration.
- The roadmap's follow-up is settled by this record once accepted: the shared word is rejected for 1.x.
- `reader.rs` gains a field, one predictable comparison per record and a cold path. `mmap.rs` does not change, but the
  change alters a safety argument and needs the independent review that module's changes get.

## Open decisions

1. **Build the bound, or only document?** Recommended: build it, after #64 and #66 have landed, at low priority. If the
   cost bar fails, document that durable-only readers are outside 2a and 2b.
2. **Reject the shared word for 1.x, or only postpone it?** Recommended: reject. Reopen only with a report of case 1
   for an uncovered record, or if a per-append shared word is wanted for another reason.
3. **Bound visible readers too, or only durable-only ones?** Recommended: both. Visible readers then read the marker
   once per segment; limiting it to durable-only would leave the documented first case as it is for most readers.
4. **Correct `compatibility.md` and "while no writer runs" now?** Recommended: yes, in a small text change on its own,
   since both are inaccurate whatever is decided here.
