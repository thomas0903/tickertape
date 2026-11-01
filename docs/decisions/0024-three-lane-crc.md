# 0024: Three-lane hardware CRC32C for every length

- **Status:** accepted (amends [0019](0019-hardware-crc-for-short-records.md))
- **Date:** 2026-11-02

## Context

ADR 0019 sent records up to 512 bytes through our own serial loop over the CPU's CRC32C instruction, and longer
ones through the `crc32c` crate. The bare-metal comparison then showed tickertape replaying 1,024-byte records more
slowly than Chronicle Queue (`docs/benchmarks.md`: 3.0 against 5.0–5.5 million records/s).

`perf` on an AWS `c6id.xlarge` (x86-64 Ice Lake) attributed about 56% of 1,024-byte replay time to the crate's CRC.
Its x86 code calls a small per-word function for every 8 bytes, and that call is not inlined: it alone took 25% of
the time. This profiling is exploratory, not a recorded benchmark.

## Options considered

1. **Raise the serial loop's cutoff.** A single CRC chain is limited by the instruction's latency of about 3
   cycles, so long inputs stay slow.
2. **Another crate.** `crc-fast` requires Rust 1.89, above our MSRV (ADR 0019).
3. **Our own three-lane loop** (chosen). Inside one function compiled with the CPU feature, run three independent
   CRC chains over three adjacent blocks. The CPU overlaps them, so the latency is hidden. Then combine the three
   chains with precomputed shift tables, the same idea the crate uses but with everything inlined.

## Decision

`crc.rs` handles every length on CPUs with the instruction (SSE4.2 on x86-64, the CRC extension on aarch64):

1. **Long blocks.** While at least 3 × 256 bytes remain, process three 256-byte lanes in parallel. Combine them as
   `shift(shift(c0) ^ c1) ^ c2`, where `shift` advances a CRC register over 256 zero bytes.
2. **Short blocks.** The same with 64-byte lanes.
3. **Tail.** The rest goes 8 bytes at a time, then byte by byte.

`shift` is four lookups in a 4 × 256 table per lane size. The CRC update is linear over GF(2), so advancing any
register over a fixed number of zero bytes is the XOR of the advanced bytes. The tables are built once, at first use,
by running the CRC instruction itself over zero bytes. That takes microseconds and needs no hand-written constants.
The crate stays as the fallback on CPUs without the instruction and under Miri.

Correctness is checked against the crate:
- for every length up to two full rounds of both lane sizes (1,984 bytes), at eight starting offsets;
- by a property test on random data up to 5,000 bytes with random initial values;
- by the RFC 3720 check values.

The x86-64 path also runs in the OrbStack VM through binfmt.

## Consequences

- On the same `c6id.xlarge`, replay of 1,024-byte records went from 273–274 to 161–163 ns per record with the
  `pread` reader, and from about 275 to about 197 ns with the zero-copy reader. 64-byte records gained about 5%. These
  are exploratory numbers; recorded ones go in `docs/benchmarks.md`.
- `crc.rs` grows by a macro that defines the per-architecture loop. It needs the cross-provider review required for
  this module.
- The zero-copy reader now spends most of its time copying records for validation with 8-byte atomic loads. Computing
  the CRC directly from those loads, without the copy, is the next candidate.
