# Changelog

All notable changes to `tickertape` and `tickertape-cli` are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crates follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

- Library: dropping a `Writer` now releases the journal lock explicitly (`flock(LOCK_UN)`), after its helper threads
  have stopped and readers have been told it has gone, and so does a `Writer::open` that fails after taking the lock.
  Until now the lock was released only when the last copy of its descriptor closed, and a child that another thread
  had forked (for example to spawn a program) holds a copy until it calls `exec` or exits, so reopening the journal
  right after dropping the writer or after a failed open, or handing it over to another process, could fail with
  `Error::Locked` naming this process. Only the process that opened the writer unlocks: a forked child dropping its
  copy of a writer (which it must not do) leaves the lock with the parent. A writer process that exits or dies
  without dropping its writer still leaves the lock with such a child until it execs or exits, for its whole life if
  it never execs (crash contract C3).
- Python: installable bindings (`bindings/python/`, PyPI name `tickertape`, not yet published; ADR 0029). `Writer` and
  `Reader` classes over the Rust library with batch appends and reads that cross the language boundary once, explicit
  partial-failure reporting (`BatchAppendError`), columnar `RecordBatch` output for NumPy, pyarrow and pandas (all
  optional), durable-only reads, interruptible waits that release the GIL, an exception for every error variant, and
  type stubs. Records are copied into `bytes`; the zero-copy mapped reader is not exposed. One abi3 wheel per platform
  for CPython 3.11+; a wheel workflow builds manylinux, musllinux and macOS wheels and an sdist, with a Trusted
  Publishing step that stays off until PyPI is configured. The pure-Python decoder in `python/` is unchanged. Handles
  refuse use in a forked child (`UsedAfterForkError`, also after process-id reuse), and closing them there releases
  the child's copies of the descriptors and mappings, so the journal can be reopened while the child runs (a handle
  that another thread was using at the fork is never released there); a waiting read holds its reader only for 50 ms
  slices, so signal handlers and other threads can use or close it; a read delivers its records only once its Python
  result is complete and no signal is pending, so a record staged when Ctrl-C arrives (also while a large result is
  built, or while the read waits for another thread to deliver) is returned by the next read; a read that a signal
  handler interrupted to fork raises `UsedAfterForkError` in the child instead of waiting for the reader's lock or
  reporting a timeout, and so does any call during which Python code forked, whatever it would have returned or
  raised (the owner check is the last thing a method does, after its exception is built); once all staged records are delivered, a reader keeps at most 6 MiB of staging buffer (enough
  for `read_batch(10_000)` to reuse it), and a larger one is freed with the GIL released before the read that
  delivers them returns (records staged by a read that raised stay in their buffer until a later read returns them); the test-only methods that can wedge a handle exist only in
  builds with the `test-hooks` feature, not in release wheels; the writer's reports are named tuples; every exception
  survives `pickle` with its current attribute values, also after an attribute was deleted.
- Core: `Reader::next_timeout` no longer panics for a timeout too large to add to `Instant::now()` (it waits
  forever), and the reader's back-off no longer panics when doubling a sleep near `Duration::MAX`.
- Core: new `Writer::release_after_fork`, only for a writer inherited across `fork` and only in the child: closes the
  child's descriptors and unmaps its mappings without unlocking the shared lock, syncing, touching the helper threads
  or changing any file. It frees heap memory, so it is not async-signal-safe; no call on the writer may be in
  progress in another thread at the fork. Used by the Python bindings.
- Release operations: `release.py prepare` also checks the Python bindings' version and lockfile against the tag, and
  resolves only `refs/tags/<tag>`, so a branch named like a release tag is refused; the release workflows check out
  the fully qualified tag. The release checklist adds a `v*` tag ruleset and required reviewers for `pypi`.
- Operational metrics (ADR 0032), with no format change and nothing added to `append` or the reader's read path.
  Values a sample can't establish are reported as unknown, never guessed, and estimates are named as such:
  - `tickertape::status::status(dir)` and `status::Sampler` sample a journal from any process: the writer's head,
    the checkpoint marker (validated as the writer validates it) and an estimate of its age from the marker's
    modification time, what a power loss would remove now (exposure), storage headroom in bytes and estimated segment
    rolls, and the writer's PID. Whether the sample verified the chain from the checkpoint to the head is reported; a
    break it confirms is corruption, and a healthy but unverified sample says so. A `Sampler` resumes its scan where it
    stopped only with proof that the segment is the same, unchanged file, and measures append rates and an estimated
    time until the volume is full.
  - `Writer::metrics()` (head, checkpoint age by the monotonic clock, uncheckpointed records and bytes, poisoned
    flag, retention failure count), `Reader::cursor()` and `Status::lag()` for in-process reader lag.
  - `status::ReaderBeacon`: an opt-in 128-byte file in `<dir>/readers/` that publishes a reader's position, the
    identity of its segment and whether it is durable-only, for observers in other processes. Beacons are published
    atomically under a directory lock, never replace anything but a stale beacon (`Error::BeaconInUse` for a live
    one), and a held `flock` tells live readers from dead ones; the writer and retention never read beacons.
  - CLI: `tickertape status <DIR>` with text, `--json` (`schema_version` 1) and `--format prometheus` output,
    `--watch` (failed samples are reported and the watch continues; `tickertape_up` and
    `tickertape_status_timestamp_seconds` show staleness), `--output` (atomic replace through an exclusively created
    temporary file, for a node_exporter textfile collector; it replaces only an earlier status output or an empty
    file, never a symlink or anything else), and health thresholds (`--max-checkpoint-age`,
    `--min-headroom`, `--min-headroom-segments`, `--max-reader-lag`) that exit with code 3; a journal the sample
    finds corrupt (or the writer would refuse) exits with 2. A journal missing at start-up is reported as a failed
    sample (`tickertape_up 0`).
  - `export --output` no longer fails when a reader beacon disappears during its alias check or `readers/` can't be
    listed (another user's). v0.4.0 failed transiently in those cases. Everywhere else in the journal the check still
    fails closed.
  - `rustix` now also enables its `process` feature (for `kill(pid, 0)`); no new crates.
  - A writer that finds the checkpoint marker cut short no longer extends it at open: it extends it after its next
    checkpoint has succeeded, so that observers can't take the time of the cut or of the extension for a
    checkpoint's (a short marker's age is reported unknown). Every reader already accepts a short marker.
  - C ABI: the new `Error::BeaconInUse` has its own status code, `TICKERTAPE_E_BEACON_IN_USE` (-18); no C function
    creates beacons yet.
- Fixed: writer recovery no longer zeroes committed records in place when it finds evidence of them after an empty
  commit word (storage damage that zeroed a commit word after the checkpoint, for example). It used to zero
  everything after an empty commit word (and then, where the expected successor opened, rewrite the seal and go on),
  under `RecoveryPolicy::Fail` too, changing bytes that zero-copy readers had borrowed. It now searches the bytes after an empty commit word for
  committed history and, when it finds some, treats the tail as corruption: `Quarantine` preserves it in
  `quarantine/` and replaces the segment, `Fail` refuses (what an open under `Fail` may still change: crash contract
  C11). New `CorruptReason::EmptyCommitWord`, and `CorruptReason::UncheckedTail` when the search stops at its work
  bound before deciding, both also reported by `verify` and mapped in the C ABI
  (`TICKERTAPE_CORRUPT_EMPTY_COMMIT_WORD`, `TICKERTAPE_CORRUPT_UNCHECKED_TAIL`) (ADR 0035). `verify` and `inspect`
  now search the rest of a segment whose records end at an empty commit word, in bounded memory whatever length a
  damaged slot claims. A seal whose marker is empty, in front of a successor segment that opens, is rewritten, keeping the records
  of its segment (non-zero bytes beyond the seal's CRC word are preserved first, or `Fail` refuses with `BadSeal`);
  a seal with its marker present and an invalid CRC is a corrupt tail like any other (crash contract C11 (1)). Some damage still cannot be told
  from a torn append and is zeroed in place as before, and a known gap remains in front of a successor that does not
  open (crash contract C8, C11). A torn record whose payload holds bytes the search counts as committed history (raw
  frames copied from another journal, for example) can be preserved, or refused under `Fail`, even after a plain
  process crash (C1).
- Fix: readers racing with retention (the writer's policy or `tickertape archive`) no longer report a removed segment
  as an error of the wrong kind. `Reader::seek` returned an I/O "not found" error when the segment it chose was
  removed before it opened it; it now lists the segments again and returns `Error::SeqUnavailable` with the new
  oldest sequence number. `Reader::open` and `Reader::open_mapped` likewise start at the oldest segment that remains.
  A reader at a seal whose segment and successor were both removed returned `CorruptReason::BrokenChain`; it now
  returns `Error::Discarded` (a missing successor while the reader's segment is still in place is still a broken
  chain). `inspect::list_segments` and `inspect::verify` (CLI `inspect`, `verify`) skip segments removed while they
  run instead of failing, and `verify` no longer reports a problem when the writer rolls during the scan. Readers and
  inspection also list again when a directory listing of a live journal comes back empty (on btrfs a listing omits
  segments created after it started, so concurrent retention could make it miss all of them). New
  fault-injection hook points (`fault::run_at`) and a stress test with `max_segments(2)` retention cover this.
- Fix: a durable-only reader (`ReaderConfig::durable_only`) could return records that were never checkpointed and
  that recovery had moved to `quarantine/` (crash contract C13). When recovery replaced the segment the reader held
  and the new writer checkpointed, the reader applied the new checkpoint marker to the old file it still had open.
  A durable-only reader now confirms, after reading a marker that moves past its position, that its segment's path
  still names the file it has open (one `stat`, never per record), and returns `Error::Discarded` otherwise
  (ADR 0034). As a consequence, a durable-only reader overtaken by retention may get `Error::Discarded` when the
  checkpoint moves past its position instead of at the end of the removed segment. New hook points
  `ReaderCheckpoint` and `ReaderCheckpointIdentity`, deterministic tests and a stress test with corruption injected
  between writer restarts cover this, and the soak test now fails on such a record instead of counting it.
  A safe durable-only reader could also return such records from its read buffer when recovery removed them in
  place, in a file that kept its identity (storage damage that emptied a commit word after the checkpoint, where
  recovery finds no committed history after it and so can't tell it from a torn append, such as the newest record's
  commit word zeroed; see the recovery fix above); it now discards that buffer when it applies a new marker, so it
  reads every newly covered record from the file again.
  A durable-only reader waiting at the end of a segment that retention removed, with the checkpoint naming the next
  segment but not yet past its position (the writer rolled and committed nothing there), returned `Ok(None)` for as
  long as the writer stayed idle; it now returns `Error::Discarded` there, as a visible reader does.
- Fix: `tickertape tail` exited with an error when the records at its position had been removed by retention, which
  a `--durable-only` tail now learns of when the checkpoint moves past it. It continues at the oldest record still in
  the journal and warns on standard error with the range it skipped.
- Fix: a `stat` that failed with an error other than "not found" (`EACCES`, `EIO`) counted as the segment being
  gone, so a reader could report `Error::Discarded` and `verify` could miss a broken chain. It is now an I/O error.
- Fix: `verify` (CLI `verify`, `inspect`) scanned a state a running writer passes through again only once, after
  50 ms, so a writer descheduled between linking a new segment and sealing the old one for longer made it report a
  broken chain. It now scans again with growing pauses, up to 630 ms in all (new hook point `InspectRescan` between
  those scans). The scans with which `verify` follows a live writer past an empty commit word at the end of the
  newest segment (ADR 0035, above) take their rescans of an invalid record from the same 630 ms per segment instead
  of a 50 ms retry of their own; the 50 ms `verify` waits once before reporting a still-empty commit word stays.
- C ABI (ADR 0028, `docs/c-api.md`): a new workspace crate, `crates/tickertape-ffi`, builds `libtickertape` as a
  shared and a static library with a committed, cbindgen-generated C99/C++11 header (`include/tickertape.h`, checked
  for drift in CI) and cargo-c metadata for installation with a pkg-config file. It covers the writer (append,
  zero-copy append through a callback, batch append, checkpoint), the safe and mapped readers (seek, try-next and
  timed next, copy-out and batch reads, durable-only), options, `verify`, segment listing, the checkpoint marker and
  retention, with stable error codes, per-thread error details and no panics across the boundary. Not published to
  crates.io and not stable before 1.0 (`TICKERTAPE_ABI_VERSION` 0, so programs require the exact library version;
  the header has version macros and `tickertape_version_number()` for the check). Extensible report structs use a
  `uint64_t struct_size` that the library fills in both directions; struct layouts are asserted at compile time in
  Rust and C, and CI compares the header with a frozen baseline from 1.0. Examples in `examples/c/`, including a
  C++11 RAII wrapper.
- Library: `Writer::abandoned_write_error()` reports a failed clean-up after an abandoned write (a panicking
  `append_with` fill), which until now only showed as refused appends and checkpoints; those errors now carry the OS
  error as the inner error of an `io::Error` of kind `Other`, so `Error::is_storage_full()` is false for them even
  after `ENOSPC` (freeing space does not make the writer accept appends again), and the `Error::DurabilityLost`
  message no longer blames a failed checkpoint in that case. A failed clean-up is a storage incident like a failed
  checkpoint: reopening gives a usable writer but no durability guarantee for the records since the last successful
  checkpoint. The test-only `fault-injection` feature gains `fault::Point::AbandonedWriteSync`.
- Repository: `unsafe` code is now also allowed in `crates/tickertape-ffi`, as a scoped extension of the rule (ADR
  0028). The release tooling refuses workspace members that it does not release unless they have `publish = false`.
- Performance: the zero-copy reader (`Reader::open_mapped`) no longer copies each record into a scratch buffer to
  validate it. It computes the CRC32C straight from 8-byte atomic loads of the mapping, feeding each load to the CPU's
  CRC instruction, and still validates every record before creating any slice into the mapping (ADR 0027). The same
  records are accepted and rejected, with the same errors; the on-disk format and `open_mapped`'s safety contract are
  unchanged.
- Tests: the mapped and safe readers are checked against each other over every payload length up to 4,200 bytes, page
  and segment boundaries, every corruption class and random damaged journals; the kill loop keeps a live zero-copy
  reader across every crash; Miri also runs the CRC tests.
- Platforms: `Reader::open_mapped` maps segments only on x86-64 and aarch64, where 8-byte atomic loads from read-only
  memory are documented to work. On other targets it now reads with `pread`, like `Reader::open` (same API and
  results; a frame too long for the address space is reported as an `Error::Io` with `ErrorKind::OutOfMemory`
  instead of panicking). CI checks that the crate builds for `i686-unknown-linux-gnu`.
- Docs: `open_mapped`'s rustdoc, ADR 0027 and the crash contract now state that the zero-copy reader's guarantees
  hold for an intact publication chain, and when its loads can race with a writer: a commit word damaged on storage,
  either a length enlarged (validation then loads bytes a live writer is storing) or a published record's word
  zeroed (recovery can then zero that record and the published records after it in place, under a live reader), as
  in every earlier release. A publication frontier, a separate follow-up, would close only the first.
- Tests: a zero-copy reader polls while writer processes append records of mixed sizes, roll and are killed; a
  deterministic test checks that a complete but unpublished record stays invisible; differential tests recompute the
  CRC after damage so the sequence and flag checks are reached; the corruption matrix covers records long enough for
  the three-lane CRC; tests check that the word-fed CRC takes the hardware path and the three-lane blocks; Miri runs
  `MapRo`'s loads over an anonymous mapping; `--cfg tickertape_force_soft_crc` runs the suite on tickertape's CRC
  fallback (the `crc32c` crate still uses the CPU instruction where it has one).
- Benchmarks: the test of a lagging reader under retention no longer depends on machine speed (it failed on
  macOS CI runners). `tt-sustained` readers no longer need to retry a seek when retention removes the segment they
  were about to open: `Reader::seek` now lists again by itself.
- Benchmarks: sustained workloads (ADR 0026). `tt-sustained` runs one writer and any mix of safe/mapped,
  visible/durable-only and lagging reader processes for a fixed duration (default 10 minutes) with steady, bursty or
  unpaced traffic, periodic checkpoints, segment rolls and writer retention, and reports latency percentiles, late
  sends, reader lag, CPU time and a per-second wall-clock time series. `compare/run.py --sustained` adds a resumable
  matrix of long probe, sustained-visibility and periodic-durability runs for every comparison tool, and
  `benches/session.py` runs a whole recorded session with environment capture, a manifest and a time estimate. No
  library or format change; the bench crate now depends on `nix`.
- Tests: format-v1 compatibility fixtures (ADR 0033). Journals written by the v0.1.0 to v0.4.0 writers themselves
  (clean and killed mid-append, on Linux aarch64, Linux x86-64 and macOS) are committed with a manifest; the current
  readers, `verify`, writer recovery, the CLI and the Python decoder are checked against them, and every released
  version's readers are checked against journals from the current writer (`tests/compat/generate.sh`).
- Tests: a soak tier (`tests/soak.rs`) combining writer kills, rolls, checkpoints, retention, injected corruption,
  storage-full and checkpoint failures with six reader processes, for a configurable time, reproducible from a
  seed. CI runs a 20 s smoke in the long tests and 90 minutes per platform weekly (`.github/workflows/soak.yml`).
- CI: tests, long tests and the fuzz smoke on aarch64 Linux (`ubuntu-24.04-arm`).
- Docs: define the path to v1.0 around sustained performance, optional eBPF diagnostics, reader/checkpoint
  optimization, ARM measurements, interoperability, replay/recovery at scale and explicit stability/release gates.
- Docs: v1.0 scope decisions of 2026-11-19. The external integration is an iceoryx2 recorder and replayer
  (ADR 0030); v1.0 adds a C++ header and a Java binding, and more install channels (ADR 0031); cloud measurements
  move to Google Cloud VMs.
- Dependencies: `cargo deny` ignores RUSTSEC-2026-0543 (chrono unmaintained). chrono reaches the CLI only through
  parquet, which has no release without it; the library does not depend on it.
- Release operations: add tag-driven Trusted Publishing, native Linux/macOS CLI archives with checksums,
  registry/docs.rs verification, and a documented retry/backfill process.
- Repository: add bug/feature issue forms, binary installation instructions and CI checks for release tooling.

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
