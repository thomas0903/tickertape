# 0027: The zero-copy reader validates records straight from atomic loads

- **Status:** accepted (supersedes the zero-copy reader's validation copy in [0003](0003-publication-protocol.md))
- **Date:** 2026-11-19 (revised the same day after three independent reviews: the safety argument for damaged
  lengths, supported targets, tests)

## Context

`unsafe Reader::open_mapped` hands out payloads that borrow from a read-only `MAP_SHARED` mapping. Until now, for
every record it first copied the frame out of the mapping with 8-byte `Relaxed` atomic loads into a reused buffer
(`MapRo::copy_atomic_into`), validated the copy (length, CRC32C, sequence number, flags), and only then created a
`&[u8]` into the mapping for the payload. The copy was added after the M4 review, for two reasons that still hold:

1. **No reference asserting immutability before validation.** Until a record is validated, its commit word is
   untrusted. A length damaged on disk could make a slice cover bytes the writer is appending at that moment. A
   `&[u8]` over bytes that another thread or process writes concurrently is undefined behaviour (a data race through
   non-atomic reads, and a shared reference that asserts its bytes don't change).
2. **The checked bytes must be the used bytes.** A CRC computed over bytes that then change proves nothing about what
   is read afterwards. Validating a snapshot made the header values, the CRC input and the decision come from one
   set of loads.

The copy costs one extra pass over every payload. With the three-lane CRC (ADR 0024) the zero-copy reader spent most
of its time in it, and replayed 1,024-byte records more slowly than the `pread` reader (about 197 against 162 ns per
record on an AWS `c6id.xlarge`, exploratory).

## The publication protocol, as this reader uses it

Spec: [`format.md`](../format.md) → Publication; ADR 0003.

- **Writer.** It writes the body `[P+4, P+len)` with ordinary stores, then stores `len` into the commit word at `P`
  with `Release`. The padding `[P+len, P+align8(len))` is already zero and never written; the next record starts at
  `P+align8(len)`. After the release store the writer never modifies `[P, P+align8(len))` again. Abandoned bodies
  are zeroed (durably) before any later commit. In place, recovery only zeroes bytes after an empty slot
  (`zero_uncommitted_tail`), which on an intact chain were never committed, and writes a missing seal at that slot
  (`write_seal`); every other repair replaces or moves a whole file (ADR 0007, `format.md` → Recovery). So on an
  intact publication chain committed bytes are immutable. A commit word damaged on storage breaks that (below,
  "Which loads can race").
- **Reader, ordering.** It loads the commit word with a 4-byte `Relaxed` atomic load and, if it is non-zero, issues
  `fence(Acquire)`. If that load read the value of the writer's release store, the store synchronises with the fence,
  so every write the writer made before the store happens before every load the reader makes after the fence (fence
  rules of the C++/Rust model, applied across processes under ADR 0003's assumption about `MAP_SHARED` pages).
- **Reader, loads after the fence** (new). Every byte of the frame after the commit word is read with an atomic load,
  each byte exactly once, and nothing else reads it before validation succeeds:
  - the CRC field at `P+4`: one 4-byte `Relaxed` load (`MapRo::load_u32`), 4-aligned;
  - `[P+8, P+align8(len))`: one 8-byte `Relaxed` load per word (`SharedWords`), 8-aligned. The first four words are
    the header fields (`seq`, `schema_id` and `flags`, the two timestamps); the rest are the payload, whose last word
    may end in padding.

  None of these overlaps the commit word, so there is no mixed-size access to it (the old copy loaded it again as
  half of an 8-byte word). The loads are unordered among themselves; they need no order, since one release store
  published all of them. `Relaxed` loads of up to 8 bytes are the only atomic operations `core::sync::atomic`
  documents as allowed on read-only memory on x86-64 and aarch64 ("Atomic accesses to read-only memory", present at
  MSRV 1.85), which is why neither `Acquire` loads nor wider loads are used.

`SharedWords` keeps the `&[AtomicU64]` it wraps private and offers only `Relaxed` loads, so no other operation can
reach the read-only mapping. That slice is a reference, but to `UnsafeCell`s: it asserts nothing about the bytes
staying unchanged. No `&[u8]` or other reference that asserts immutability exists before validation succeeds; the
payload's `&[u8]` is formed afterwards (`MapRo::committed_bytes`). Over ordinary memory (`SharedWords::new`) the same
code runs under Miri and in the fuzzer, and a test runs `MapRo`'s own loads under Miri over an anonymous mapping.

## Which loads can race

The guarantees in this ADR hold **for an intact publication chain**: every commit word the reader follows is one the
writer published. Storage damage to a commit word (its length enlarged, or the word zeroed) breaks the chain, and
then they do not hold; the cases are below.

**Records published by the protocol: no race and no mixed-size access.** For a record whose commit word holds the
length the writer stored, the writer's release store of that word synchronises with the reader's acquire fence, so
every writer store to the frame happens before every reader load of it, and the writer never stores to the frame
again. The reader's loads stay inside `[P+4, P+align8(len))`: they end where the next slot's commit word begins and
never touch another record's commit word, which the writer accesses only as a 4-byte atomic. The commit word at `P`
itself is loaded as a 4-byte atomic too.

**Damaged commit words: known limitations.** Both cases stay inside `open_mapped`'s contract, since only tickertape
writers touch the files.

*An enlarged length.*

1. A writer appends up to offset `Q` in a segment and checkpoints.
2. While nothing runs, storage damage enlarges the length in the commit word of an earlier record at `P`, so that
   the claimed frame `[P, P+align8(len))` reaches past `Q`.
3. A writer reopens. Recovery starts at the checkpoint and does not re-read what precedes it; the writer appends from
   `Q`.
4. A zero-copy reader replays from the segment start. At `P`, its 8-byte loads cover bytes the writer is filling
   with ordinary (non-atomic) stores, and can cover a later record's commit word, which the writer stores as a 4-byte
   atomic.

*A zeroed commit word.*

1. A writer appends records up to sequence number `T`, which readers return; the last checkpoint covers only the
   records before some `S <= T`.
2. Storage damage zeroes the commit word of the published record `S`.
3. A writer reopens. Recovery scans from the checkpoint and stops at the empty slot of record `S`. When it finds
   evidence of the records after it, it usually replaces the segment by a new file (or `Fail` refuses), which leaves
   readers' mappings unchanged (ADR 0035). When it finds none, it cannot tell the damage from a torn append: it takes
   that slot for the end of the journal and zeroes record `S` **and the published records after it** in place, then
   appends from there. That covers damage after which nothing validates, such as the commit word of the newest
   record zeroed; crash contract C11 (3) has the cases.
4. A zero-copy reader that is still open while this happens holds payload slices into those records, and they change
   under it: the bytes it validated are not the bytes it hands back. Its later commit-word loads, and the loads that
   validate the records it then reads, meet the recovering and the new writer's stores to the same bytes.

In both cases a mapped load touches bytes a writer is storing to without synchronisation. When the writer runs in
the same process, that is a data race in Rust's memory model **before any CRC runs**, and overlapping atomic accesses
of different sizes are undefined as well; a reviewer confirmed both with Miri on a small model. When the writer is
another process, which is the normal deployment, the accesses are outside the model altogether (ADR 0003 already
reasons about cross-process `MAP_SHARED` memory by analogy with threads). Validation does not prevent this, since the
loads that race are the ones validation makes.

What happens next is an **assumption about the implementation on the supported machines, not a guarantee**:

- on x86-64 and aarch64 (the only targets that map segments for reading, below), an aligned 4- or 8-byte load is a
  single instruction (`mov`, `ldr`) that cannot fault inside the mapping and returns some mixture of old and new bytes
  of the word;
- the compiler does not split, repeat or invent an atomic load, so each word is read once, and the value validated
  is the value fed to the CRC;
- no `&[u8]` or other reference asserting immutability exists before validation, so nothing licenses the compiler to
  assume the bytes are stable.

Under that assumption, a frame that mixes bytes from different writes fails the CRC, and the reader reports
corruption. How likely a chance match is depends on the corruption and on what the writer is storing; a reader
reads the slot again for at least `VALIDATION_WINDOW` (50 ms) before reporting it, but a retry can read the same
bytes, so retries are not independent attempts. A frame that passes would hand out a slice over bytes that are still being written. In the
zeroed-word case the reader can also return records that validate and then change. **The copying reader before this
decision had exactly the same exposure**, since it copied the same range with the same atomic loads and relied on
the same immutability after validation; this decision does not introduce it, and it removes the old reader's one
mixed-size load (the record's own commit word, loaded again as half of an 8-byte word).

Closing the enlarged-length case needs the reader's loads to be bounded by a publication boundary that is
synchronised independently of the untrusted length: a shared frontier word that the writer advances on every append.
That is a protocol change with its own design and measurements, so it is a separate follow-up (roadmap → "Remove the
mapped reader's validation copy"), not part of this decision. A reader-side frontier alone does not close the
zeroed-word case: there the reader loads nothing out of bounds; the bytes change because recovery rewrites published
records in place. That needs recovery to recognise such damage before it zeroes anything, and recovery cannot tell
every such case from a torn append.

**Can any of the exposure be removed at no cost?** No:

- *A segment that has a successor.* It looks as if no writer store could race with a load in a segment that is no
  longer the active one, but the existence of the successor doesn't establish that: the writer links the successor
  first and only then writes the seal into the old segment (`Writer::roll`), and recovery after a crash between the
  two zeroes the old segment's tail and writes the seal in place. Only an observed seal would prove the segment
  finished, and the reader can't find the seal without walking the very lengths in question (or without a frontier).
  Checking for a successor also costs a directory lookup, which can only be done per segment, not at the load.
- *Clamping the frame.* Any bound on `len` that doesn't come from an independently synchronised word is either the
  segment size, which is already checked, or derived from the same untrusted bytes.
- *Ordering the CRC so that damaged frames fail early.* The CRC needs every word, and the header checks that could
  fail first (sequence number, flags) come from the first words, which are committed bytes anyway; they can't stop
  the loads past `Q`.

Everything the reader can do cheaply already happens: lengths are checked against the segment before any load,
every length step is overflow-checked, and the loads are atomic, aligned and inside the mapping.

## Computing the CRC without a slice

The record CRC covers the commit word's 4 bytes, then `[P+8, P+len)`. `format::decode_record_words`:

1. loads the four header words into registers and lays them out, after the commit word value, in a 36-byte array on
   the stack (`head`);
2. feeds the payload's whole words straight from the mapping to `crc::crc32c_append_parts`;
3. loads the payload's last word, if the payload isn't a multiple of 8 bytes, and feeds its first `len % 8` bytes
   (`tail`); the padding bytes in it are ignored, as the format says and as `decode_record_body` does.

Then it compares the CRC, the sequence number and the flags, in the same order as `decode_record_body`, with the same
errors. The header it returns is the one the CRC covered, because each word was loaded once.

`crc32c_append_parts` (in `crc.rs`):

- **Hardware path** (SSE4.2 on x86-64, the CRC extension on aarch64, detected at run time as before): one function
  compiled with the CPU feature runs `head`, then the words, then `tail`. Each 8-byte atomic load (a plain `ldr` or
  `mov`) feeds a CRC instruction directly. Long payloads use the three-lane blocks of ADR 0024, counted in words
  (32-word and 8-word lanes) with the same shift tables; the rest is serial. Intrinsics still take values only.
- **Software fallback** (no instruction, other architectures, Miri): the words go through a 256-byte stack buffer,
  32 at a time, into the `crc32c` crate. This path still copies, into L1-resident stack memory only, and costs about
  what the old reader cost; it exists for correctness on CPUs that predate the instruction.

The seal check reads the seal CRC with one 4-byte load (`MapRo::load_u32`) instead of an 8-byte copy.

## Time of check and time of use

After validation succeeds, `Reader::materialize` creates `&[u8]` over `[P+40, P+len)` in the mapping, and the caller
reads it later. What makes those bytes equal to the ones the CRC covered is only `open_mapped`'s safety contract: while
the reader exists, no process other than a tickertape writer modifies the segment files, and a tickertape writer
never modifies committed bytes on an intact publication chain (for a damaged commit word, see "Which loads can
race"). **The new reader relies on nothing the old one didn't.** The old reader also validated one set of bytes (its
copy) and handed out a slice of another (the mapping), so it relied on exactly the same immutability.
What the copy additionally gave, that the returned header values are the ones checked even if the contract were
broken, is kept by loading each word once. The validation-before-slice order is unchanged. The safety contract of
`open_mapped`, the crash contract and the on-disk format are therefore unchanged; their wording about "a copy" is
updated.

## Supported targets

The argument rests on `core::sync::atomic`'s guarantee for `Relaxed` loads from read-only memory, which covers
8-byte loads on x86-64 and aarch64 but only 4 bytes on 32-bit targets (on i686 without SSE an 8-byte atomic load is a
locked compare-exchange, which faults on a read-only page). So segments are mapped for reading only on x86-64 and
aarch64 (`mmap::MAPPED_READS`). Elsewhere `Reader::open_mapped` opens the `pread` reader instead: same API, same
results, payloads borrowed from the reader's own buffer, and no read-only mapping at all (`MapRo::new` refuses). The
crate still refuses big-endian and non-Unix targets. CI checks that the gate compiles for `i686-unknown-linux-gnu`.

## Truncation and `SIGBUS`

Unchanged. The reader touches the same pages as before, `[P, P+align8(len))` for each record it validates (the copy
loaded exactly these bytes too), and a truncated mapped file raises `SIGBUS` on the first load past the end, as
before and as any read of a returned payload would. The safe reader still never maps anything.

## Options considered

1. **Keep the copy.** Sound and simple, but one extra pass per payload, and the zero-copy reader stays slower than the
   copying `pread` reader for long records.
2. **`ptr::read_volatile` of each word.** Compiles to the same loads, and in the damaged-length case it would be no
   worse than the atomic loads chosen, since both are formally racy there. It is worse everywhere else: the standard
   library documents which atomic loads work on read-only memory, and says nothing equivalent for volatile reads;
   the compiler may not tear, split or repeat an atomic load, while a volatile read of a `u64` carries no
   single-access guarantee; and atomic loads take part in the happens-before ordering on the valid path (the acquire
   fence orders the writer's stores before them), whereas a non-atomic volatile read of bytes another thread wrote
   is only ordered by that same fence and gains nothing.
3. **Wider loads** (16-byte atomics, SIMD loads, inline assembly). `AtomicU128` is unstable; 16-byte and vector loads
   are not documented as allowed on read-only memory; assembly would add `unsafe` outside the two permitted modules.
   The CRC instruction consumes 8 bytes at a time anyway.
4. **Form `&[u8]` first and use the existing slice CRC.** A reference asserting immutability over bytes a writer may be
   storing, in the damaged-length case, and non-atomic reads; this is what the M4 review removed.
5. **A bounded staging copy**: copy 256 bytes at a time to the stack and run the slice CRC on it. Sound, and it keeps
   the copy in L1, but still a copy of every byte; the software fallback does this. Measured below for comparison.
6. **CRC straight from 8-byte `Relaxed` atomic loads, each word loaded once** (chosen).

## Decision

Option 6, as described above, on x86-64 and aarch64; the `pread` reader on other targets. `MapRo::copy_atomic_into`
and the reader's scratch buffer are removed. Unsafe code: two new blocks in `mmap.rs` (`MapRo::load_u32`, a 4-byte
load; `MapRo::words`, an `&[AtomicU64]` over a bounds-checked, 8-aligned range wrapped in `SharedWords`), and
target-feature functions in `crc.rs` that call the same intrinsics as before on values. The damaged-commit-word
cases above are accepted as known limitations, unchanged from earlier releases.

## Measurements

Exploratory runs on the owner's laptop and in the OrbStack VM are in the pull request; release-quality numbers on
x86-64 and aarch64 servers are recorded later in `docs/benchmarks.md`.

Replay of an existing journal, warm page cache, nanoseconds per record (median of 15 replays of 1,000,000 records
at 64 bytes, or of 30 replays of 100,000 records at 1,024 bytes; three rounds alternating the old and the new build
on the same journal files; a scratch harness, commit `f906721` against `466e8ab`). Exploratory: a laptop and a VM,
not release numbers.

| Machine | Reader | 64 B old | 64 B new | 1,024 B old | 1,024 B new |
|---|---|---|---|---|---|
| Apple M4 Pro, macOS 27.2 (APFS) | zero-copy | 23.5–23.8 | 18.1–18.3 | 137.9–139.7 | 94.0–101.8 |
| Apple M4 Pro, macOS 27.2 (APFS) | `pread` (unchanged) | 20.8–21.5 | 21.1–22.2 | 116.4–126.1 | 115.6–131.8 |
| OrbStack VM, Linux 7.2 aarch64 (btrfs) | zero-copy | 21.9–35.7 | 17.9–23.5 | 117.1–136.8 | 57.9–88.7 |
| OrbStack VM, Linux 7.2 aarch64 (btrfs) | `pread` (unchanged) | 21.6–28.0 | 22.3–24.9 | 94.0–119.5 | 100.0–128.2 |

The zero-copy reader is now faster than the `pread` reader at both sizes on both machines; before, it was slower.
The VM was shared with other workloads and its spread is wide: at 1,024 bytes every new round beat every old round;
at 64 bytes the ranges overlap (the old build's best round, 21.9 ns, beat the new build's worst, 23.5 ns), although
each new round beat the old round run just before it.

Option 5 (stage 768 bytes at a time on the stack, then the slice CRC with the same three lanes), same harness on
the Mac: 27.6–28.8 ns at 64 bytes and 125.6–126.2 ns at 1,024 bytes, slower than the old copy at 64 bytes (the
staging buffer is initialised on every record) and well behind option 6 (17.6–18.4 and 95.4–98.3 ns in the same
session).

Cross-process visible latency (`tt-latency --reader mapped`) could not separate the two builds: p50 was below 1 µs
for both, and the tails varied by orders of magnitude between runs of the same build, following the writer's late
sends on these machines. A difference of 5–40 ns per record is below what that tool resolves here.

## Consequences

- No extra pass over the payload in the zero-copy reader on CPUs with a CRC instruction.
- Results are bit-identical with the `pread` reader, which is now checked directly:
  `tests/mapped_reader.rs` (every payload length from 0 to 4,200 bytes, page and segment boundaries, every corruption
  class on short records and on records at and past the three-lane thresholds, each flag bit and a wrong sequence
  number together with a flag under a valid CRC, a proptest over random damaged journals), unit tests comparing
  `decode_record_words` with `decode_record_body` for every length through two rounds of both lane sizes, with and
  without a recomputed CRC after the damage (so the checks after the CRC are reached), a word-level CRC test for every
  word count on both paths, and a differential check in the `decode_record` fuzz target, which also recomputes the
  CRC and fills the padding with non-zero bytes. Tests also check that both CRC entry points take the hardware path
  where the CPU has it and run the three-lane blocks for long inputs.
- Live validation is tested too: a zero-copy reader polls continuously while writer processes append records of mixed
  sizes (up to 3,199 bytes), roll, and are killed and restarted (`tests/kill_loop.rs::live_mapped_reader_*`); and a
  deterministic test pauses the writer when a record is complete but unpublished and checks that both readers see
  nothing until the commit word is stored, then exactly that record (`fault::before_next_publish`, test builds only).
  The kill loop keeps a live zero-copy reader across every crash, and the `loom` model gains a single-pass validation
  case (the load ordering itself is unchanged).
- The Miri job also runs the CRC tests and `MapRo`'s loads over an anonymous mapping
  (`cargo +nightly miri test -p tickertape --lib -- format crc map_ro`), which execute the word decoder, the software
  fallback, and the pointer arithmetic, bounds and alignment of `MapRo::load_u32` and `MapRo::words`. Miri cannot run
  the hardware path or file mappings. `RUSTFLAGS="--cfg tickertape_force_soft_crc"` compiles out tickertape's own
  hardware path, so the whole suite can run on its fallback (the `crc32c` crate still picks its own hardware
  implementation on capable CPUs).
- `crc.rs` and `mmap.rs` changed, so this needs the independent review those modules require.
