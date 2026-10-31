# Changelog

All notable changes to `tickertape` and `tickertape-cli` are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crates follow [Semantic Versioning](https://semver.org/).

## Unreleased

First release candidate (v0.1.0).

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
