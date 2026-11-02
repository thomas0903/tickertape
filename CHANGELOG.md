# Changelog

All notable changes to `tickertape` and `tickertape-cli` are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crates follow [Semantic Versioning](https://semver.org/).

## [0.4.0] - 2026-11-02

First public release on crates.io (both `tickertape` and `tickertape-cli`). Earlier versions were repository tags only.
The on-disk format is unchanged (version 1). Results on bare-metal Linux, against
v0.3.0 on the same machine type, are in `docs/benchmarks.md`: append is 57–69% faster, 1,024-byte replay is 60%
faster, and latency tails are far lower.

- Performance (Linux): the writer prefaults again, with `MADV_POPULATE_WRITE` only 256 KiB ahead of the write position
  (ADR 0025; `WriterConfig::prefault`, on by default). The stalls that turned it off in v0.3.0 came from other causes.
  In experiments on bare metal and on a small cloud VM, the small window was never worse than no prefaulting, and it
  cut p99 latency by a large factor. The cost: `checkpoint()` also writes out the dirty pages prefaulted ahead (on bare
  metal, p50 0.38 → 0.84 ms at 64 B).
- Performance: CRC32C now runs three hardware CRC chains in parallel for long records (ADR 0024). This replaces the
  `crc32c` crate's x86 path, which wasn't inlined. Replay of 1,024-byte records is markedly faster (exploratory
  profiling on an AWS x86 VM: from about 274 to about 162 ns per record with the `pread` reader). Results are
  bit-identical.

## [0.3.0] - 2026-11-02

Repository tag only; not published to crates.io. The on-disk format is unchanged (version 1); v0.1 and v0.2 journals work as they
are. Two new files may appear in a journal directory:
- `notify`, an 80-byte wake-up record;
- on macOS, `next-segment.tmp`, the next segment prepared in the background.

Format v1 readers ignore both.

- Benchmarks: a comparison suite against Chronicle Queue, Aeron, yaque, queue-file and plain files (`compare/`,
  ADR 0018), recorded on Linux (an AWS cloud VM) and macOS; results and caveats in `docs/benchmarks.md`.
  `run-recorded.sh` and the comparison runner now sync between runs, so earlier runs' write-back can't distort
  later ones.
- Performance (macOS): the writer prefaults the active segment from a background thread (ADR 0020;
  `WriterConfig::prefault`, on by default). On APFS, first writes into preallocated pages were the writer's
  bottleneck; appends no longer stall on them in steady state. Not used on Linux, where the equivalent advice made
  latency worse (ADR 0023).
- Performance (macOS): the next segment is prepared (preallocated, synced, mapped and prefaulted) in the
  background once the active one is half full (ADR 0022). A roll then only writes, syncs and links the header: on
  APFS the writer's roll stall dropped from about 50 ms to about 5 ms. Part of `WriterConfig::prefault`. Not used on
  Linux, where a synchronous roll costs about 1 ms on ext4 and the spare occasionally stalled the writer (ADR 0023).
- Performance: CRC32C uses the CPU instruction directly for records up to 512 bytes (ADR 0019), which speeds up
  `append` and both readers for short records. Results are bit-identical; the format is unchanged.
- Performance: the zero-copy reader copies records for validation with a plain word loop (faster replay).
- Benchmarks: `tt-latency --wait spin|default|block|sleep` selects how the reader waits.
- Library: reader wake-ups (ADRs 0017, 0021). Blocked readers wait in a shared-memory region that the writer
  creates and names in the journal's `notify` record (futex on a sealed memfd on Linux, `os_sync_wait_on_address`
  on a POSIX shared-memory object on macOS 14.4+) instead of sleeping; a new record wakes them within
  microseconds. The region can't shrink, so the safe reader still can't receive `SIGBUS`. On by default
  (`ReaderConfig::notify`, `WriterConfig::notify`); readers poll where wake-ups are unavailable, including between
  different users.
- Library: add `retention::{Policy, Action, plan, apply}` for checkpoint-covered sealed prefixes, opt-in
  `WriterConfig::retention`, `Writer::last_retention_error` and cumulative `Writer::retention_report`.
  Retention errors never fail open, append or checkpoint. No on-disk format change.
- CLI: add `archive` with move/delete, count/bytes/age limits, read-only `--dry-run` and a documented `--json`
  schema. It is the only command that changes a journal. Existing archive files are never replaced.
- CLI: add read-only `stats` with streaming range/checkpoint statistics, bounded payload percentiles, exact
  UTC-second counts, and a documented `--json` schema.
- Library: `Reader::records()`, an iterator of owned records committed so far (for `take`, `filter`, `collect`).
- Fix: with `RecoveryPolicy::Fail`, a sealed segment whose successor has an invalid header is now refused instead
  of quarantined (found by the new `recover_dir` fuzz target, which runs writer recovery on generated directories).
- Python: add a dependency-free, read-only format v1 journal decoder and JSONL CLI in `python/`.
- Docs: an operations guide (`docs/operations.md`): segment size, checkpoint interval, disk space, archiving, recovery.
- CLI: `tail --decode market-demo` (text summary or the JSON `decoded` object, as in `export`).
- Spec: `docs/format.md` now states that reserved header bytes must be zero, which checkpoint slot wins on equal
  generations, and that short marker files are checked slot by slot (all already true of the Rust decoder).

## [0.2.0] - 2026-11-01

Repository tag only; not published to crates.io. The on-disk format is unchanged (version 1); v0.1.0 journals work as they are.

- Readers validate records without a per-record allocation: the safe reader in place in its `pread` buffer, the
  zero-copy reader from an atomic copy into a reused buffer (still before any slice into the mapping exists).
- Library: `ReaderConfig::durable_only(true)` waits for checkpoint coverage before returning records (ADR 0010, C13).
- CLI: `tail --durable-only` and `export --durable-only` (JSONL and Parquet); export caps its snapshot at the
  checkpoint observed at startup.
- Examples: add the optional `binance-recorder` example (`--features live`) with exact fixed-point trade parsing,
  periodic/final checkpoints and reconnect backoff; default examples remain synthetic and offline.
- Repository: AI-agent tooling is no longer part of the repository (ADR 0012).

## [0.1.0] - 2026-10-31

First release. Repository tag only; not published to crates.io.

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

[0.4.0]: https://github.com/thomas0903/tickertape/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/thomas0903/tickertape/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/thomas0903/tickertape/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/thomas0903/tickertape/releases/tag/v0.1.0
