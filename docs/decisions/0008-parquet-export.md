# ADR 0008: Parquet export and opt-in market demo decoding

- Status: accepted
- Date: 2026-10-31

## Context

M6 needs a bounded-memory, DuckDB-readable export while preserving opaque payloads and unsigned journal metadata.

## Decision

Use parquet 59 with only the `snap` feature, low-level typed column writers, Snappy compression and row groups
of at most 100,000 records. Reuse JSONL's two safe-reader passes to capture a committed prefix and export it.
Always finalize the Parquet footer after a reader error, including corruption discovered on the second pass.
Output path validation is shared with JSONL and Parquet requires an explicit file destination.

Keep timestamps as unsigned raw nanoseconds; signed Parquet timestamp types cannot cover u64. See
[the CLI contract](../cli.md) for the schema and DuckDB conversion. An opt-in decoder enum selects market-demo;
its separate codec validates schema id, length, reserved bytes and side/action tags. Invalid demo records get
null decoded values without failing opaque export. No library API or journal format changes are needed. The CLI exposes the codec in its own small Rust library
so integration tests and future producer code can share the exact encoding. `tiny-keccak` 2.0.2, pulled in by
parquet → ahash → const-random, uses CC0-1.0; allow that license only for this exact crate/version in cargo-deny.

## Consequences

Memory is bounded by one row group and its column buffers, rather than total journal size. Very large individual
payloads can still consume significant memory. Decoded fixed-point values are approximate doubles; raw payloads
and unsigned identifiers remain lossless. Parquet 60 is excluded because this project targets Rust 1.85.

The DuckDB integration test skips absent Python/DuckDB by default and requires it with
`TICKERTAPE_REQUIRE_DUCKDB=1`; a dedicated Linux/macOS CI matrix installs it and enforces the check.
