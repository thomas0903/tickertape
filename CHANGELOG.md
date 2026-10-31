# Changelog

All notable changes to `tickertape` and `tickertape-cli` are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crates follow [Semantic Versioning](https://semver.org/).

## Unreleased

- Python: add a dependency-free, read-only format v1 journal decoder and JSONL CLI in `python/`.
- Docs: an operations guide (`docs/operations.md`): segment size, checkpoint interval, disk space, archiving, recovery.
- CLI: `tail --decode market-demo` (text summary or the JSON `decoded` object, as in `export`).
- Spec: `docs/format.md` now states that reserved header bytes must be zero, which checkpoint slot wins on equal
  generations, and that short marker files are checked slot by slot (all already true of the Rust decoder).

## [0.2.0] - 2026-11-01

Not yet published to crates.io. The on-disk format is unchanged (version 1); v0.1.0 journals work as they are.

- Readers validate records without a per-record allocation: the safe reader in place in its `pread` buffer, the
  zero-copy reader from an atomic copy into a reused buffer (still before any slice into the mapping exists).
- Library: `ReaderConfig::durable_only(true)` waits for checkpoint coverage before returning records (ADR 0010, C13).
- CLI: `tail --durable-only` and `export --durable-only` (JSONL and Parquet); export caps its snapshot at the
  checkpoint observed at startup.
- Examples: add the optional `binance-recorder` example (`--features live`) with exact fixed-point trade parsing,
  periodic/final checkpoints and reconnect backoff; default examples remain synthetic and offline.
- Repository: AI-agent tooling is no longer part of the repository (ADR 0012).

## [0.1.0] - 2026-10-31

First release. Not yet published to crates.io.

### Library (`tickertape`)

- On-disk format version 1 (`docs/format.md`): fixed-size, preallocated segment files with a CRC-protected header,
  40-byte little-endian record headers with CRC32C, seals that link segments, and a two-slot checkpoint marker.
- `Writer`: single writer per journal (exclusive `flock`), `append` and zero-copy `append_with`, size-based rolling,
  `checkpoint()` with platform-specific durability (`msync` + `fdatasync` on Linux, `F_FULLFSYNC` on macOS), sticky
  checkpoint errors, and crash recovery that validates the chain from the last checkpoint and preserves anything it
  removes in `quarantine/`.
- `Reader`: independent cursors with `seek`, non-blocking `try_next`, and `next_timeout` with a configurable
  back-off; the safe `Reader::open` (`pread`) and the zero-copy `unsafe Reader::open_mapped`.
- `inspect::verify`, `inspect::list_segments` and `inspect::read_checkpoint` for read-only inspection.
- Crash contract (`docs/crash-contract.md`) with a test for every guarantee: a 250-kill `SIGKILL` loop, torn and
  corrupt records, stale locks, slow readers, disk full (injected and on real APFS, ext4, btrfs and tmpfs volumes),
  sticky checkpoint errors, a 1,000,000-record cross-process test, property tests, Miri, fuzzing and a `loom` model.
- MSRV 1.85.

### CLI (`tickertape-cli`, binary `tickertape`)

- `tail` (follow live), `inspect`, `verify` (exit code 2 on corruption) and `export --format jsonl|parquet`.
- Snappy Parquet export with raw unsigned metadata, bounded row groups, and opt-in `--decode market-demo` columns
  (also a `decoded` object in JSONL). Default JSONL output is unchanged by `--decode`.

[0.2.0]: https://github.com/thomas0903/tickertape/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/thomas0903/tickertape/releases/tag/v0.1.0
