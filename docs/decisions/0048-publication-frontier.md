# 0048: The zero-copy reader's publication frontier

- **Status:** accepted on 2026-11-25 (design only, nothing is built; issue #62)
- **Date:** 2026-11-25
- **Follows up:** [0027](0027-mapped-reader-validates-in-place.md), "Which loads can race". It narrows the known
  limitation stated there; 0027's decision stands.

**In short.** No shared frontier word in 1.x: deferred. The checkpoint marker, which exists already, will bound the
zero-copy reader's loads. **The gain is for replay and catch-up. A caught-up reader tailing the head, the common
low-latency case, gets nothing from the bound.** A durable-only zero-copy reader is outside the limitation for damaged
commit words, as long as the journal's markers are intact.

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

The cases below need a commit word whose value no writer stored there:

1. **A non-zero value**, whose claimed frame reaches bytes that a writer stores: the active segment past the writer's
   position `Q`, or, while a writer opens, the bytes after an uncovered end in an older segment, which recovery zeroes
   or seals in place. Validating the slot loads them.
   - *A covered record's length, enlarged.* Recovery starts at the checkpoint marker and does not see it. A covered
     length set to 20 frames, 10 of them past `Q`, survived `Writer::open` (no recovery action), and an `open_mapped`
     reader then replayed into it under a live writer.
   - *An uncovered record's length, enlarged.* Recovery validates the record and repairs it (one action). So this needs
     damage that appears **while a writer runs**: a clean page evicted and read back damaged, or a memory error. The
     contract says "while no writer runs", which is narrower than what can happen.
   - *The empty slot at the head,* damaged into a length or a seal marker under a live writer. With 1,920 written
     there, visible readers of both kinds report a checksum mismatch (with a seal marker, an invalid seal), and the
     next append fills those bytes. Durable-only readers do not touch the slot.
2. **Zero** in place of a published, uncovered record's commit word, which the next recovery cannot tell from a torn
   append (C11 (3)). It zeroes that record and those after it in place, under open readers.
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
- *In place of a record's length,* a seal marker or a shorter length loads nothing outside that record. At the empty
  head, any non-zero value makes the reader load bytes the next append stores.
- *Odds.* A try reads the slot about 20 times in its 50 ms window. If each read were an independent 2⁻³² chance (they
  are not: a retry can read the same bytes), an application that keeps retrying `Error::Corrupt` would make about 400
  reads a second, or one match in about four months of retrying. An enlarged record's own header is intact, so the
  sequence check adds nothing there.

### What has been observed

Entries marked † come from scratch programs that were deleted; they cannot be rerun from the repository. Test plan
items 1, 2 and 5 recreate the marker-bound parts.

- **Case 1:** no report of it; reproduced only by writing the damage (the three outcomes above †; the review of this
  record reran the covered and uncovered cases on 1.0.0 code with the same result). For a covered length both readers
  reported a checksum mismatch after 50 ms †. Miri reports undefined behaviour for a small model of it (an 8-byte
  atomic load against a 4-byte atomic store) and nothing in 32 seeds once the load is bounded †, as ADR 0027's reviewer
  found. Loom reports the race for another model †.
- **Case 2a:** `tests/reader_retention_stress.rs::durable_readers_under_repairs_long` injects the damage, and accepts
  and counts changed payloads. The one run made for this record (macOS, 60 s) had 156 in-place repairs and 0 changed
  payloads.
- **No tool checks the real mappings.** Miri has no file mappings. ThreadSanitizer keeps its state per virtual
  address, so it cannot connect two mappings of one page (reasoned, not run). The loom model in the repository covers
  intact chains only.
- Issue #43 (a mapped view that lagged on aarch64 Linux) is a different effect. No frontier removes it, and it means
  no ordering argument over mapped pages is stronger than the kernel's coherence of them.

So the limitation needs silent damage to 4 particular bytes, and nothing has been seen outside injected faults. It is
real in the sense that a caller cannot rule it out.

## Options considered

1. **Document, and do nothing more.** Free. But the caller has no way out except `Reader::open`.
2. **A shared frontier word** (next section). Closes case 1 for readers that attached to it, plus part of 2b, at the
   head too. Deferred.
3. **Use the checkpoint marker as the frontier** (chosen). It exists already, is synchronised independently of the
   lengths and is CRC-protected. No writer change.
4. **A frontier in the segment header or file.** Changes format version 1, and ADR 0003 (option 2) rejected it.
5. **Validate with `pread` whenever the bound is unknown.** A system call per record for a live reader. Rejected for
   the hot path, used on the cold path below.
6. **Reader registration, so that recovery waits.** The only thing that closes 2a for visible readers. It lets a dead
   or slow reader delay a writer, which ADR 0003 excludes. Rejected.

### Option 2: the shared word, deferred in 1.x

- **Where.** Not in the segment (format), and not in a mapped file in the journal directory: truncating that would
  raise `SIGBUS` (ADR 0021's reason). The `notify` region (sealed memfd, POSIX shared memory) has room for a `u64`
  position and a `u64` segment base, both zero in a 1.0.0 writer's region ("no frontier").
- **What and when.** Storing the frame's *end* before the commit word lets a bounded load cover the commit word still
  to be stored; loom finds that race †. Storing it after the commit word is sound †, but a reader can see the commit
  word first and must wait for a second line. Better: store the **start** of the record being committed, with
  `Release`, after its body and just before its commit word. The reader accepts a frame `[P, E)` if `E <= start` or
  `P == start` (the newest record's own length is trusted: it has just been stored). It loads `start` (`Acquire`) after
  its fence on the commit word, so it always sees at least its own record's start and never refuses an intact record;
  loom passes this for intact and damaged chains †. On open the writer stores its recovered end. On a roll it seals,
  stores 64, then the new base; a reader that sees a higher base knows its segment is finished, and one that sees its
  own base with a refused frame loads the base again. A refused frame takes the crossing path of the decision below.
- **Compatibility and lifetime.** A 1.0.0 writer leaves zeros and a 1.0.0 reader ignores the words. After a crash the
  Linux region is reachable only for readers that already mapped it, with a last value that stays true on an intact
  chain. A reader whose frame is refused must re-read the `notify` record to follow a restart.
- **Why not now.**
  - *Coverage.* There is no region with `notify(false)`, for a reader of another user, across pid namespaces or
    containers, with `hidepid`, or with no writer. Each gap falls back to today's loads (a reader cannot know that no
    old writer is about to start), so the contract would depend on facts the caller cannot see.
  - *Small extra benefit.* Over option 3 it adds case 1 for uncovered slots, and 2b once the new writer has opened. It
    does nothing for 2a, or for 2b while recovery runs.
- **Cost, not known.** One more store per record, on a region where `Notify::publish` already does a sequentially
  consistent `fetch_add` per append (per batch in `append_batch`), so it could share that line; and for a tailing
  reader one more line moved from the writer's core per record, which I would guess at 50–100 ns across server cores.
  The prototype below measured neither properly. Cost is therefore not a reason here.
- **Reopening** needs a report of case 1 for an uncovered slot, or another need for a per-append word; and a
  measurement of the complete design (base and refresh included) against the bar below, with unpaced append throughput
  with 0, 1 and 4 tailing readers.

## Decision

**Build no shared word in 1.x. Bound the zero-copy reader's loads by the checkpoint marker**, visible and durable-only
readers alike, in a separate change, at low priority, after pull requests #64 and #66, if it meets the cost bar below.
If it does not, take option 1.

**The bound.** For its open segment the reader keeps `limit`:

| Marker against the segment | `limit` |
|---|---|
| names this segment | the marker's `offset` |
| names a later segment | the segment's size (no tickertape writer stores there, and no recovery from this marker does) |
| names an earlier one, or no valid marker, or it cannot be read, or its offset is not a valid slot | 0: no bound, as today |

A slot at `P < limit` whose frame would end after `limit` is not loaded through the mapping. `limit` is the covered
position, a record or seal boundary, so with the writer's own markers an intact record never crosses it. Slots at or
after `limit` are read as today.

**A crossing frame** (cold path, never inlined): the reader reads the slot again as the safe reader reads a fresh slot,
commit word included, with `pread` into its own buffer, and does what the safe reader would: the same error, or, if it
validates, **the record, with its payload borrowed from that buffer** (`materialize` picks buffer or mapping per
record). The frame is never loaded through the mapping. This is memory-safe whatever the marker says, and returns what
`Reader::open` returns, so `open_mapped`'s promise holds (checks, order and errors are those of `Reader::open`). A
marker that does not fit the files (a mismatched restore, or one that was lost or moved back before an in-place repair)
can put `limit` inside an intact record: that record then costs a copy. The path does not read the marker. Each
re-read in the 50 ms retry loop is the same `pread`s again.

**Which marker.**

- *Visible reader.* It reads the two 64-byte slots with `pread` (not the 128 KiB `read_checkpoint` reads now) before
  it opens a segment by path, which proves that the file was current after the marker was written (ADR 0034). It reads
  them again, and then checks the identity, once when the cursor reaches `limit` and at the 100 ms check at an empty
  slot. **If the marker cannot be read, or that identity check finds another file, none, or fails, the old `limit`
  stays and nothing is reported on that account.** A reader overtaken by retention thus still reads its segment to the
  end (C14); the identity checks that exist today keep their results.
- *Durable-only reader.* It uses the `offset` of the marker it has applied, kept next to `durable_next_seq`, with ADR
  0034's order and errors unchanged.
- The marker file is opened by path each time, not cached, as durable-only readers do now, so a replaced file is
  followed: at most once per segment, once per `limit` reached, and ten times a second when idle.

**What this closes,** as long as no marker was lost and no earlier one became current (next paragraph):

| | Visible reader | Durable-only reader |
|---|---|---|
| Case 1 | closed for slots below the `limit` it holds | closed |
| 2a, 2b | open | do not arise: it returns nothing at or after the marker, and recovery changes nothing before it |

**What stays open for a visible reader:** a commit word at or after the bound it holds has a value that no writer
stored there. The bound it holds is

- *0* when the marker could not be read at the open. Damage to a covered record then suffices, with no writer running;
- *stale* when the marker has moved on since. The reader reads it again only at `limit`, after 100 ms at an empty
  slot, or at the next segment. One that enters a segment at the roll (`limit` 0) and then lags without waiting that
  long at an empty slot stays unbounded there although the marker covers it, and so does one whose re-read or identity
  check failed. Damage to a record covered since then suffices, with no writer running;
- *current*, with the uncovered tail and the empty slot after it (the whole segment, if there is no valid marker or it
  names an earlier segment). Case 1 then needs damage under a running writer, since a reopen validates the rest.

**The condition on the marker.** The narrowing holds while the journal's `checkpoint` file holds the markers the writer
wrote and no earlier one becomes current. That is a condition of the narrowing, **not a new obligation on the caller of
`open_mapped`**: without it the reader has today's limitation and, as above, no wrong result. Damage to both slots, or
to the newer one, breaks it. So does a marker sync that fails: the slot can be readable for a while and later revert.
C13's exception names only damage that also destroys the marker, so this is wider than C13's text.

**Scope.** A durable-only zero-copy reader is outside the limitation *for damaged commit words*; for case 2 that is
true of 1.0.0 already. No marker helps against storage that returns different bytes for a page read again (evicted and
read back): that changes a payload slice the caller holds, covered or not, for every zero-copy reader.

**Ordering.** On an intact chain nothing changes. A crossing frame loads nothing through the mapping. For a damaged
frame that ends inside `limit`, the loads reach committed bytes whose own commit words the reader may not have
acquired, so they need the marker: the writer's stores precede its hand-over to the checkpoint thread (a mutex), that
thread's `msync` and `fsync`, and its `pwrite` of the slot; the reader's `pread` returns the slot, and the reader issues
`fence(Acquire)`. ADR 0003 assumes the reader's half (a fence after `pread`). This **also assumes that the writer's
`pwrite` acts as a release for its earlier stores**, with no fence in 1.0.0 code. That is reasonable after `msync` and
`fsync`, but it is a new assumption, and loom and Miri models will assume it, not test it. Nothing stores to those bytes
again, so a wide load over several old commit words is not a race.

**Crash, roll, no writer.** A crash leaves the marker where it was, at or before the end. After a roll the old segment
keeps its bound until a reader takes a marker that passes it. Archives have no marker and no writer. A journal that
never checkpoints gains nothing, and recovery validates all of it at every open.

**Versions.** Any writer, 1.0.0 included, works with the new reader in full, and a 1.0.0 reader is unaffected.

## Cost

Prototypes of both, in a scratch copy of the crate †. **Busy Apple M4 Pro Mac, load average 17 to 27, 7 alternating
rounds, 64-byte payloads. Informal, not a published result.** Median of the rounds' best batch, with the range:

| | 1.0.0 code | Marker bound | Shared word |
|---|---|---|---|
| append, ns | 13.3 (13.0–13.6) | 13.5 (13.3–14.1) | 13.5 (13.2–13.8) |
| zero-copy replay, ns/record | 17.1 (16.9–19.2) | 17.4 (17.1–18.5) | 17.4 (17.2–18.0) |
| paced tail, p50 visible, ns | 83–125 | 83–125 | 125 |
| paced tail, p99 visible, ns | 291–1,250 | 333–1,250 | 291–792 |

The append code of the first two columns is the same, so 0.2 ns there is noise. The replay ranges overlap. The clock
ticks every 41.7 ns, so the tail rows cannot show an extra cache line. The prototypes were active: the bound rejected a
damaged length before any load, and a deliberately wrong frontier made an intact record fail. The shared word lacked
the base and the refresh, and the bound lacked the refresh and the buffer path.

**Bar for the change** (bare-metal x86-64 and aarch64, pinned cores, figures in its pull request). Each comparison uses
three builds interleaved: the old one twice (A, A′) and the new one (B). A machine counts only if A and A′ meet, between
themselves, the limit set for B. Medians over the runs:

| Measurement | Runs of each build | Limit for B against A |
|---|---|---|
| `replay`, mapped, 64 B | 20 | + 0.5 ns per record |
| `replay`, mapped, 1,024 B | 20 | + 1 % |
| `tt-latency --reader mapped`, 64 B, 100,000/s | 20 | p50 + 10 ns, p99.9 + 100 ns |
| the same, the writer checkpointing every 100 ms | 20 | p50 + 10 ns, p99.9 + 100 ns |
| opening a segment, mapped, warm | 1,000 opens, 20 times | + 10 µs |

The 64-byte replay limit is absolute because a percentage of 17 ns is below what one branch can cost (the informal
difference above is 0.3 ns); against a p99.9 of 500 ns it is nothing. If no machine counts, the change is not built.

## Contract changes

### Corrections, independent of the bound (a separate small text change)

The existing texts state the case too narrowly. Stating it correctly names more damage than they do, so it needs the
rule in `compatibility.md` ("weakening a guarantee, or turning an untested claim into a weaker one, is a breaking
change"): **this is a correction, not a weakening.** The guarantee is already conditioned on an intact chain, "where
every commit word the reader follows is one a writer published", and every damage added below breaks that condition.
No state in which the guarantee held loses it; the list of examples was incomplete.

- **`Reader::open_mapped`, `# Safety`,** first bullet, from "The length in an earlier record's commit word is enlarged
  while no writer runs" to "the writer is storing.", becomes: "A commit word holds a non-zero value that no writer
  stored there, and the frame it claims reaches bytes that a writer stores: an earlier record's length, enlarged, or a
  length or a seal marker in the empty slot at the head. The damage can appear whether or not a writer runs. Validating
  that slot loads bytes the writer is storing."
- **`crash-contract.md`,** the bullet "*An enlarged length.*" becomes: "*A non-zero value that no writer stored.* If
  storage damage enlarges the length in an earlier record's commit word, or puts a length or a seal marker into the
  empty slot at the head, so that the claimed frame reaches bytes a writer stores, validating that slot makes the reader
  load bytes the writer is storing at that moment. The damage can appear whether or not a writer runs." And the sentence
  from "A separately synchronised publication frontier, a planned protocol change," to the end of its paragraph becomes:
  "ADR 0048 (accepted, not built) bounds the loads in the first case by the checkpoint marker, for the records it
  covers. The second case stays open for visible readers, where the bytes change because recovery rewrites them (ADR
  0027 → "Which loads can race")."
- **`compatibility.md`** → Crash contract, from "a zeroed commit word lets recovery rewrite" to "with its own ADR."
  becomes: "a commit word with a value that no writer stored there can make the reader load bytes a writer is storing
  or, if it is zero, let recovery rewrite published records in place under a live reader. ADR 0048 (accepted, not built)
  narrows the first by the checkpoint marker in a 1.x release."

### With the bound

These narrow the limitation. No item, error value, precondition or on-disk byte changes.

- **`Reader::open_mapped`, `# Safety`,** after the two bullets, and **`crash-contract.md`** after its two: "As long as
  the journal's `checkpoint` file holds the markers the writer wrote and no earlier one becomes current, neither case
  concerns a reader with `durable_only(true)`, and the first does not concern a slot below the checkpoint position the
  reader last took (at a segment's open, on reaching that position, and while it waits). Without that, the limitation is
  as stated." The paragraph on ADR 0048 above drops "not built". `open_mapped`'s first paragraph gains: "A record whose
  frame crosses that position is copied into the reader's buffer instead."
- **`crash-contract.md`,** C13: "No tickertape process changes a payload a durable-only zero-copy reader returned,
  unless the marker was lost or an earlier one became current. Storage that returns different bytes for a page read
  again can." "Known risks and limits" needs no change: the limitation is in the section before it.
- **`compatibility.md`:** "ADR 0048 (accepted, not built) narrows" becomes "ADR 0048 narrows"; "in a 1.x release" goes.
- **`format.md`** → Checkpoint marker: "Readers ignore this file unless `ReaderConfig::durable_only(true)` is enabled"
  gains "(a visible zero-copy reader uses it only to bound its loads; it returns the same records and errors whatever
  the file holds)".
- **`c-api.md`, `cpp-api.md`, `java-api.md`, `tickertape.h`, the Java and C++ reader docs** say only that a writer never
  changes committed bytes, without the limitation. They gain one sentence pointing to it. Text only, so the ABI
  baseline is untouched. ADR 0027 and the `mmap.rs` comments gain a pointer.

## Work in flight

- **ADR 0045 (sparse seek index, #64).** Its step 2 lets zero-copy and durable-only readers use an index only for
  segments before the marker's. That is the second row of the `limit` table, so the two can share one read of the
  marker's slots. Neither the bound nor a shared word lets that step be relaxed: for a sealed segment the marker does
  not cover, the remaining hazard is recovery changing it in place, which no frontier sees. In covered segments a wrong
  index can meet no store, with or without the bound.
- **ADR 0046 (forward seeks, `seek_to_end`, #66).** A seek that scans from the segment's start is the replay of covered
  records in which case 1 arises, so it benefits most. `seek` opens the candidate segment before
  `continues_from_cursor` reads the marker. The candidate takes its `limit` from the read that its own open makes first
  (above); to take it from `continues_from_cursor`'s later read instead, the candidate's identity would have to be
  checked again after that read. A seek that continues keeps the segment it has open: a visible reader may take `limit`
  from that marker, since the identity check follows it; a durable-only reader does not apply that marker, so it keeps
  the `limit` of the one it has applied. `last_record_in_place` uses `pread` and needs nothing. `seek_to_end` starts at
  the marker's position: a visible reader then reads only uncovered records and gains nothing, while a durable-only
  reader can refresh to a newer marker on the way and reads records that one covers, inside its bound. The saved
  `Place` must include `limit`.

## Tests

1. **Loom:** add a damaged frame and the marker to `protocol_model.rs`. Without the bound the model must fail (kept as
   an expected failure); with it, pass, for a visible and a durable-only reader, across a hand-over to the checkpoint
   thread. (The marker is an atomic with release and acquire: see Ordering. The shared word's variants are not
   recreated.)
2. **Miri:** the same on anonymous memory through `MapRo` with a concurrent writer thread: a report without the bound,
   none with it, in at least 32 seeds.
3. **No load past `limit`:** a test-only counting view of `MapRo`, for every damaged length of a covered record, at,
   just below and above `limit`; no mapped load after the commit word for a crossing frame.
4. **Same results:** for every such length, zero-copy and safe reader agree on the error (extends
   `tests/mapped_reader.rs`, whose corruption classes get a marker at each position). With a CRC-valid marker at every
   aligned offset of an intact journal, both visible readers return every record and the payload of a crossing one is right
   while it is borrowed.
5. **Live:** a covered length damaged and the writer reopened, with no recovery action; an uncovered one, which the
   reopen repairs; and a length and a seal marker in the empty head slot under a live writer, which the next append
   overwrites. Appends from a thread and from another process, all four kinds of reader.
6. **Intact chains never cross:** the proptest journals with checkpoints at random positions, marker at a seal, at
   offset 64 of a new segment, short marker file, dangling marker, one valid slot, unreadable marker.
7. **Marker lost or moved back** with an in-place repair under an open reader: the same results as the safe reader.
8. **C14:** retention removes the segment while a visible reader's cursor is below `limit`: all its records are still
   returned, and `Error::Discarded` comes at its end. The same with the marker unreadable, and with the `stat` failing
   other than "not found", at the re-read at `limit`. Fault points around each marker read and identity check.
9. **Refresh:** the marker moves while the reader is below `limit`, at it, and waiting at an empty slot.
10. `durable_readers_under_repairs` copies a durable-only zero-copy payload at once; it should also hold one across
    restarts and assert that it never changes. Kill loop, soak and the compatibility fixtures run unchanged. Mutations:
    drop the bound, use `<=` for `<`, skip the identity check, propagate its error, return the crossing record from the
    mapping.

## Consequences

- For damaged commit words, a caller who needs the limitation gone has `durable_only(true)`, at the price of the
  checkpoint's delay. Until now only the safe reader was an answer.
- The limitation stays in the contract for visible readers, smaller. Case 2a cannot be closed without registration.
- The roadmap's follow-up is settled: the shared word is deferred in 1.x, and building the bound is later work.
- `reader.rs` gains a field, one predictable comparison per record, a choice in `materialize` and a cold path, which
  the bar covers. `mmap.rs` does not change, but the change alters a safety argument and needs the independent review
  that module's changes get.
- No decision is left open in this record.
