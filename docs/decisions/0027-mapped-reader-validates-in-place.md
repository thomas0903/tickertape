# 0027: The zero-copy reader validates records straight from atomic loads

- **Status:** accepted (supersedes the zero-copy reader's validation copy in [0003](0003-publication-protocol.md))
- **Date:** 2026-11-19

## Context

`unsafe Reader::open_mapped` hands out payloads that borrow from a read-only `MAP_SHARED` mapping. Until now, for
every record it first copied the frame out of the mapping with 8-byte `Relaxed` atomic loads into a reused buffer
(`MapRo::copy_atomic_into`), validated the copy (length, CRC32C, sequence number, flags), and only then created a
`&[u8]` into the mapping for the payload. The copy was added after the M4 review, for two reasons that still hold:

1. **No reference before validation.** Until a record is validated, its commit word is untrusted. A length corrupted
   on disk could make a slice cover bytes the writer is appending at that moment. A `&[u8]` over bytes that another
   thread or process writes concurrently is undefined behaviour (a data race through non-atomic reads, and a shared
   reference that asserts its bytes don't change).
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
  `P+align8(len)`. After the release store the writer never modifies `[P, P+align8(len))` again: abandoned bodies are
  zeroed (durably) before any later commit, and recovery repairs by writing a new file, never by editing a mapped one
  (ADR 0007). This is the "committed bytes are immutable" guarantee.
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
  published all of them. `Relaxed` 4- and 8-byte loads are the only atomic operations `std::sync::atomic` documents
  as allowed on read-only memory on x86-64 and aarch64 ("Atomic accesses to read-only memory", present at MSRV 1.85),
  which is why neither `Acquire` loads nor wider loads are used.
- **The bad case.** If the commit word is corrupted so that the frame reaches bytes the writer is writing now, every
  read is still an atomic load: no data race and no reference exist, and at worst a load returns a stale or torn
  value. The CRC then fails, except for a collision (about 2^-32 per corrupted record, as before), and no slice is
  created.

`SharedWords` keeps the `&[AtomicU64]` it wraps private and offers only `Relaxed` loads, so no other operation can
reach the read-only mapping. Over ordinary memory (`SharedWords::new`) the same code runs under Miri and in the
fuzzer.

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
the reader exists, no process other than a tickertape writer modifies the segment files, and a tickertape writer never
modifies committed bytes. **The new reader relies on nothing the old one didn't.** The old reader also validated one
set of bytes (its copy) and handed out a slice of another (the mapping), so it relied on exactly the same immutability.
What the copy additionally gave, that the returned header values are the ones checked even if the contract were
broken, is kept by loading each word once. The validation-before-slice order is unchanged. The safety contract of
`open_mapped`, the crash contract and the on-disk format are therefore unchanged; their wording about "a copy" is
updated.

## Truncation and `SIGBUS`

Unchanged. The reader touches the same pages as before, `[P, P+align8(len))` for each record it validates (the copy
loaded exactly these bytes too), and a truncated mapped file raises `SIGBUS` on the first load past the end, as
before and as any read of a returned payload would. The safe reader still never maps anything.

## Options considered

1. **Keep the copy.** Sound and simple, but one extra pass per payload, and the zero-copy reader stays slower than the
   copying `pread` reader for long records.
2. **`ptr::read_volatile` of each word.** Compiles to the same loads, but in Rust's memory model a volatile read that
   races with a write is still a data race; it is only defined for memory outside the abstract machine (MMIO). The
   corrupted-length case is exactly where the race could happen, so this would weaken the argument for no gain.
3. **Wider loads** (16-byte atomics, SIMD loads, inline assembly). `AtomicU128` is unstable; 16-byte and vector loads
   are not documented as allowed on read-only memory; assembly would add `unsafe` outside the two permitted modules.
   The CRC instruction consumes 8 bytes at a time anyway.
4. **Form `&[u8]` first and use the existing slice CRC.** Undefined behaviour in the corrupted-length case; this is
   what the M4 review removed.
5. **A bounded staging copy**: copy 256 bytes at a time to the stack and run the slice CRC on it. Sound, and it keeps
   the copy in L1, but still a copy of every byte; the software fallback does this. Measured below for comparison.
6. **CRC straight from 8-byte `Relaxed` atomic loads, each word loaded once** (chosen).

## Decision

Option 6, as described above. `MapRo::copy_atomic_into` and the reader's scratch buffer are removed. Unsafe code:
two new blocks in `mmap.rs` (`MapRo::load_u32`, a 4-byte load; `MapRo::words`, an `&[AtomicU64]` over a bounds-checked,
8-aligned range wrapped in `SharedWords`), and target-feature functions in `crc.rs` that call the same intrinsics as
before on values.

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
  class, a proptest over random damaged journals), unit tests comparing `decode_record_words` with
  `decode_record_body` for every length through two rounds of both lane sizes, a word-level CRC test for every word
  count on both paths, and a differential check in the `decode_record` fuzz target. The kill loop keeps a live
  zero-copy reader across every crash, and the `loom` model gains a single-pass validation case (the load ordering
  itself is unchanged).
- The Miri job also runs the CRC tests (`cargo +nightly miri test -p tickertape --lib -- format crc`), which execute
  the word decoder and the software fallback over `SharedWords` on ordinary memory. Miri cannot run the hardware path
  or real mappings.
- `crc.rs` and `mmap.rs` changed, so this needs the independent review those modules require.
