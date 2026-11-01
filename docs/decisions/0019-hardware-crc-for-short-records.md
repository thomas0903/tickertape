# 0019: Hardware CRC32C for short records, in a second `unsafe` module

- **Status:** accepted
- **Date:** 2026-11-01

## Context

Every record carries a CRC32C over its commit word, header fields and payload (`docs/format.md`). The writer computes it
on every `append` and both readers check it on every record. We used the `crc32c` crate (0.6.8) for it, in two calls
per record: one for the 4-byte length and one for the rest.

When we profiled replay of 64-byte payloads on an Apple M-series Mac (scratch harness, not a recorded benchmark),
nearly all of the roughly 22 ns per record went to these two calls. The crate is fast on long inputs: it interleaves
three hardware CRC streams and combines them with tables. On short inputs, though, it pays a fixed setup cost of about
8 ns per call. A plain loop over the CPU's CRC32C instruction (`crc32cd` on aarch64, SSE4.2 `crc32` on x86-64) took
5.9 ns for 96 bytes, where the crate took 11.1 ns. Most market-data records are short (tens to a few hundred bytes).

Constraints:
- MSRV 1.85. On that version the CPU intrinsics are `unsafe fn`s.
- Runtime feature detection is needed on x86-64 (SSE4.2 is not in the baseline) and on aarch64 Linux (CRC is not in
  the Armv8.0 baseline).
- The project rule is that only `mmap.rs` contains `unsafe`.

## Options considered

1. **Keep the `crc32c` crate.** No new `unsafe`, but the CRC stays the largest per-record cost on short records for
   the writer and both readers.
2. **Switch to `crc-fast`.** It is fast and MIT OR Apache-2.0, but it requires Rust 1.89, which is above our MSRV.
   It also brings a larger dependency surface (C FFI and a cache).
3. **A software table CRC for the 4-byte length only.** This is safe code and removes one of the two calls, but the
   remaining call keeps its fixed cost.
4. **Our own short-input path over the CPU instruction, falling back to the crate.** It needs a few lines of `unsafe`:
   the calls to intrinsics behind a runtime feature check.

## Decision

Option 4. A new module, `crates/tickertape/src/crc.rs`, provides `crc32c(data)` and `crc32c_append(crc, data)`:

- For inputs up to `SHORT_MAX` (512 bytes; the serial loop was faster than the crate at every length measured up
  to 512 bytes and slower at 1056), when the CPU supports the instruction (checked at run time with
  `is_x86_feature_detected!("sse4.2")` or `is_aarch64_feature_detected!("crc")`, which std caches), it uses a serial
  loop: 8 bytes per instruction, then single bytes for the tail.
- Otherwise it calls the `crc32c` crate, which stays a dependency for long inputs and as the software fallback.
- Under Miri it always uses the crate, so the format code stays Miri-checkable.

The results are bit-identical to the crate's. A property test compares the two on random inputs and initial values,
and the RFC 3720 check value is tested.

The `unsafe` rule changes from "only `mmap.rs`" to:

> **`mmap.rs`** for mappings and system calls, and **`crc.rs`** for CPU intrinsics only. Intrinsics take values, not
> pointers.

Each `unsafe` block in `crc.rs` has a `// SAFETY:` comment naming the feature check that precedes it. Changes to
`crc.rs` need the same cross-provider review as `mmap.rs`.

## Consequences

- Faster `append`, replay and live reads for short records. Measured gains go in `docs/benchmarks.md` from recorded
  runs only.
- Two modules to review instead of one. `CONTRIBUTING.md` and the crate's agent instructions state the new rule.
- The on-disk format does not change: the CRC values are identical.
- The fuzz target `recover_dir` keeps using the `crc32c` crate directly, as an independent oracle.
- The x86-64 path is tested on CI (`ubuntu-latest`) and locally under Rosetta 2. The aarch64 path is tested on macOS
  and on the OrbStack Linux VM.
