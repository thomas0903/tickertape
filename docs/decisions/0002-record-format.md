# 0002: Record format

- **Status:** proposed (M1 approval gate)
- **Date:** 2026-10-31

## Context

The byte format is the project's main promise: simple, versioned, easy to decode from any language, and robust
against torn writes. Payloads are opaque bytes plus a schema id (typed zero-copy schemas are a non-goal).
Spec: [`format.md`](../format.md).

## Options considered

1. **Fixed 40-byte little-endian header + opaque payload, 8-byte aligned, CRC32C per record.** Trivial to decode
   with `struct.unpack` or `from_le_bytes`; aligned `u64` fields; one checksum covers header and payload.
2. **Varint-encoded header** (protobuf style): smaller records, but harder to decode by hand, unaligned, and the
   length can't double as an atomic commit word.
3. **Self-describing records** (e.g. CBOR/MessagePack envelope): flexible, but slower, bigger, and pushes schema
   questions into the journal.
4. **Checksum choice:** CRC32C (hardware instructions on x86-64 SSE4.2 and aarch64 CRC, ubiquitous in storage) vs.
   xxHash/XXH3 (faster in software, but a hash, not designed around burst-error detection) vs. no checksum
   (can't distinguish torn writes from data).

## Decision

Option 1 with CRC32C. Fields: `len` (also the commit word), `crc`, `seq` (u64), `schema_id` (u32), `flags` (u32,
reserved, must be 0), `venue_ts_ns` and `local_ts_ns` (u64 nanoseconds since the Unix epoch, 0 = unset), payload.
The CRC covers `len` and every byte after the CRC field up to `len`, so a commit word that tears or is corrupted
fails validation. `len` is at most `0xFFFFFFFE` (`0xFFFFFFFF` marks a seal), which caps payloads just under 4 GiB.
The sequence number is stored in every record (redundant with position) so that gaps, replays and
misplaced bytes are detected. Little-endian only, enforced at compile time; all supported targets are little-endian.
Timestamps are unsigned because the epoch-nanosecond range of a `u64` reaches year 2554 and "unset" is clearer as 0
than as a negative sentinel.

## Consequences

- 40 bytes of overhead per record plus up to 7 bytes of padding. For a 64-byte payload that is about 40 %. Accepted
  for simplicity; a later version could add a compact header behind a new `format_version`.
- Any change to field meaning bumps `format_version`. Reserved fields give room for compatible additions.
- The Python/DuckDB path decodes nothing: it reads exported Parquet. A read-only Python decoder is possible later from
  `format.md` alone.
